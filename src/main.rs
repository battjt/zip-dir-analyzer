use anyhow::{Ok, Result};
use clap::Parser;
use executors::{threadpool_executor::ThreadPoolExecutor, Executor};
use indicatif::{ProgressBar, ProgressStyle};
use jaq_interpret::{Ctx, Filter, FilterT, ParseCtx, RcIter, Val};
use regex::Regex;
use serde_json::Value;
use std::{
    fs::{self, File},
    io::{self, stdout, BufRead, Read, Write},
    path::Path,
    sync::{atomic::AtomicU64, Arc, Mutex},
    thread,
    time::Duration,
};

fn main() -> Result<()> {
    let args = Args::parse();
    if args.jq {
        let mut ctx = ParseCtx::new(Vec::new());
        ctx.insert_natives(jaq_core::core());
        ctx.insert_defs(jaq_std::std());

        let (f, errs) = jaq_parse::parse(&args.pattern, jaq_parse::main());
        if !errs.is_empty() {
            let error_message = errs
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(anyhow::anyhow!(error_message));
        }

        let filter = ctx.compile(f.unwrap());

        ZipDirAnalyzer::run(args, JqProcessor { filter })
    } else {
        let regex = Regex::new(&args.pattern)?;
        ZipDirAnalyzer::run(args, RegexProcessor { regex })
    }
}

trait TextProcessor: Send + Clone {
    fn grep_file<T: Read>(&self, path: &str, data: T) -> Result<bool>;
}
#[derive(Clone)]
struct JqProcessor {
    filter: Filter,
}
#[derive(Clone)]
struct RegexProcessor {
    regex: Regex,
}
impl TextProcessor for ZipDirAnalyzer<JqProcessor> {
    fn grep_file<T: Read>(&self, path: &str, data: T) -> Result<bool> {
        let value: Result<Value, _> = serde_json::from_reader(data);
        match value {
            Result::Ok(value) => {
                let inputs = RcIter::new(core::iter::empty());
                let out = self
                    .processor
                    .filter
                    .run((Ctx::new([], &inputs), Val::from(value)));
                for o in out {
                    match o {
                        Result::Ok(json_val) => {
                            if self.report(path, json_val.as_str().unwrap())? {
                                return Ok(true);
                            }
                        }
                        Err(err) => {
                            println!("Error: {}", err);
                        }
                    };
                }
            }
            Err(je) => {
                if !self.args.quiet {
                    println!("JSON Error: {}", je)
                }
            }
        };
        Ok(false)
    }
}
impl TextProcessor for ZipDirAnalyzer<RegexProcessor> {
    /// base file searching routine
    fn grep_file<T: Read>(&self, path: &str, data: T) -> Result<bool> {
        let status = format!("processing: {path}");
        self.progress.set_message(status);

        let mut consecutive_error_count = 0;
        for r in io::BufReader::new(data).lines() {
            match r {
                Err(err) => {
                    if consecutive_error_count > self.args.max_errors {
                        if !self.args.quiet {
                            eprintln!(
                                "WARN: {path} skipping file ({} consecutive errors) {err}",
                                self.args.max_errors
                            );
                        }
                        // After too many consecutive errors, skip file. This allows some corrupt lines to be skipped and when there is a terminal error, the whole file will be skipped.
                        break;
                    }
                    if !self.args.quiet {
                        eprintln!("WARN: {path} skipped line due to {err}");
                    }
                    consecutive_error_count += 1;
                }
                Result::Ok(line) => {
                    if self.processor.regex.is_match(&line) && self.report(path, &line)? {
                        return Ok(true);
                    }
                    consecutive_error_count = 0;
                }
            }
        }
        Ok(false)
    }
}

/// Search directory for files matching the file_pat that include the line_pat. The contents of zip files are also searched.
///
/// The progress is reported as files processed from the filesystem, not files within the zips. X zips each of Y files will report X operations, not X*Y operations.
#[derive(Parser, Debug, Default, Clone)]
#[command(version, about)]
pub struct Args {
    /// Directory to search. Use '-' to indicate that the list of directories will be on stdin.
    #[arg()]
    directory: String,

    /// Only analyze files with names matching this regex.
    #[arg()]
    file_pat: String,

    /// Report lines that match this regex.
    #[arg()]
    pattern: String,

    /// Report skipped files.
    #[arg(long, short = 'v')]
    verbose: bool,

    /// Do not report non-text file errors.
    #[arg(long, short = 'q')]
    quiet: bool,

    /// Delimiter between file name and matching line.
    #[arg(long, short = 'd', default_value = ": ")]
    delimiter: String,

    /// Delimiter between zip file name and the file name.
    #[arg(long, short = 'z', default_value = "!")]
    zip_delimiter: String,

    /// How many directories to process in parallel.
    #[arg(long, default_value_t=num_cpus::get())]
    parallel: usize,

    /// Do not report file name in results.
    #[arg(long)]
    no_file: bool,

    /// Only report the file name once in the results.
    #[arg(long)]
    file_only: bool,

    /// Only report the zip name once in the results.
    #[arg(long)]
    zip_only: bool,

    /// Max consecutive errors to allow before skipping file.
    #[arg(long, default_value_t = 3)]
    max_errors: usize,

    /// Use jaq (similar to jq) to query JSON files instead of regex.
    #[arg(long, default_value_t = false)]
    jq: bool,
}

#[derive(Clone)]
struct ZipDirAnalyzer<TP: Send + Clone> {
    pool: Arc<Mutex<ThreadPoolExecutor>>,
    ops_scheduled: Arc<AtomicU64>,
    ops_complete: Arc<AtomicU64>,
    processor: TP,
    file_regex: Regex,
    args: Args,
    progress: ProgressBar,
}

impl<TP> ZipDirAnalyzer<TP>
where
    ZipDirAnalyzer<TP>: TextProcessor,
    TP: Send + Clone + 'static,
{
    /// Main entry point.
    pub fn run(args: Args, processor: TP) -> Result<()> {
        let binding = args.directory.clone();
        let zip_dir_analyzer = ZipDirAnalyzer {
            pool: Arc::new(Mutex::new(ThreadPoolExecutor::new(args.parallel))),
            ops_scheduled: Default::default(),
            ops_complete: Default::default(),
            processor,
            file_regex: regex::Regex::new(&args.file_pat)?,
            args,
            progress: ProgressBar::new(100),
        };

        zip_dir_analyzer
            .progress
            .set_style(ProgressStyle::with_template(
                "{bar} {pos}/{len} {wide_msg}",
            )?);

        if binding == "-" {
            for line in io::stdin().lines() {
                zip_dir_analyzer.walk_path(Path::new(line?.as_str()))?
            }
        } else {
            zip_dir_analyzer.walk_path(Path::new(binding.as_str()))?;
        }
        let mut scheduled = 1;
        let mut complete = 0;
        // wait for all processing to complete
        while scheduled > complete {
            scheduled = zip_dir_analyzer
                .ops_scheduled
                .load(std::sync::atomic::Ordering::Relaxed);
            complete = zip_dir_analyzer
                .ops_complete
                .load(std::sync::atomic::Ordering::Relaxed);
            zip_dir_analyzer.progress.set_length(scheduled);
            zip_dir_analyzer.progress.set_position(complete);
            thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    }

    /// evaluate how to process the path
    fn walk_path(&self, path: &Path) -> Result<()> {
        let path_str = path.to_str().unwrap();
        if path.is_dir() {
            self.walk_dir(path)
        } else if path_str.ends_with(".zip") {
            let mut file = fs::File::open(path)?;
            self.walk_zip(path_str, &mut file)
        } else if path.is_file() {
            self.grep_file(path_str, &File::open(path)?)?;
            Ok(())
        } else {
            // skipping links and devices and such
            if self.args.verbose {
                eprintln!("INFO: skipping non-file {}", path_str);
            }
            Ok(())
        }
    }

    /// path is a directory.  Process each entry in a separate thread.
    fn walk_dir(&self, path: &Path) -> Result<()> {
        for entry in std::fs::read_dir(path)? {
            self.ops_scheduled
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            let quiet = self.args.quiet;
            let self_clone = self.clone();
            let path_buf = entry?.path();
            self.pool.lock().unwrap().execute(move || {
                let result = self_clone.walk_path(path_buf.as_path());
                if result.is_err() && !quiet {
                    eprintln!(
                        "WARN: {} skipped due to {}",
                        path_buf.to_str().unwrap(),
                        result.unwrap_err()
                    );
                }
                self_clone
                    .ops_complete
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            });
        }
        Ok(())
    }

    fn walk_zip(&self, path: &str, zip_file: &mut File) -> Result<()> {
        let mut archive = zip::ZipArchive::new(zip_file)?;
        for i in 0..archive.len() {
            let zip_file = archive.by_index(i)?;
            if zip_file.is_dir() {
                // just a directory placeholder.
            } else {
                let file_name = path.to_string() + "!" + zip_file.name();
                if file_name.ends_with(".zip") {
                    eprintln!("No support for a zip of a zip yet {file_name}");
                } else {
                    self.search_file(&file_name, zip_file)?;
                }
            }
        }
        Ok(())
    }

    fn search_file<T: Read>(&self, path: &str, data: T) -> Result<()> {
        if self.file_regex.is_match(path) {
            self.grep_file(path, data)?;
        } else if self.args.verbose {
            eprintln!("INFO: skipping {}", path);
        }
        Ok(())
    }

    /// all reporting
    fn report(&self, file: &str, line: &str) -> Result<bool> {
        if self.args.no_file {
            stdout().write_fmt(format_args!("{line}\n"))?;
        } else if self.args.file_only {
            let file = if self.args.zip_only {
                file.split(&self.args.zip_delimiter).next().unwrap_or(file)
            } else {
                file
            };
            stdout().write_fmt(format_args!("{}\n", file))?;
        } else {
            stdout().write_fmt(format_args!("{}{}{}\n", file, self.args.delimiter, line))?;
        }
        Ok(self.args.file_only || self.args.zip_only)
    }
}
