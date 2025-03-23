use anyhow::{Ok, Result};
use clap::Parser;
use executors::{threadpool_executor::ThreadPoolExecutor, Executor};
use indicatif::{ProgressBar, ProgressStyle};
use regex::Regex;
use std::{
    fs::{self, File},
    io::{self, stdout, BufRead, Read, Write},
    path::Path,
    sync::{atomic::AtomicU64, Arc, Mutex},
    thread,
    time::Duration,
};
use zip::read::read_zipfile_from_stream;

fn main() -> Result<()> {
    let args = Args::parse();
    if args.jq {
        ZipDirAnalyzer::<JqProcessor>::run(args, JqProcessor {})
    } else {
        let processor = RegexProcessor {
            regex: Regex::new(&args.pattern)?,
        };
        ZipDirAnalyzer::<RegexProcessor>::run(args, processor)
    }
}

trait TextProcessor: Send + Clone {
    fn grep_file<T: Read>(&self, path: &str, data: T) -> Result<()>;
}
#[derive(Clone)]
struct JqProcessor {}
#[derive(Clone)]
struct RegexProcessor {
    regex: Regex,
}
impl TextProcessor for ZipDirAnalyzer<JqProcessor> {
    fn grep_file<T: Read>(&self, path: &str, data: T) -> Result<()> {
        todo!()
    }
}
impl TextProcessor for ZipDirAnalyzer<RegexProcessor> {
    /// base file searching routine
    fn grep_file<T: Read>(&self, path: &str, data: T) -> Result<()> {
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
                    consecutive_error_count = consecutive_error_count + 1;
                }
                Result::Ok(line) => {
                    if self.processor.regex.is_match(&line) {
                        if self.args.file_only {
                            stdout().write_fmt(format_args!("{path}\n"))?;
                            break;
                        } else {
                            self.report(path, &line)?;
                        }
                    }
                    consecutive_error_count = 0;
                }
            }
        }
        Ok(())
    }
}

/// Search directory for files matching the file_pat that include the line_pat. The contents of zip files are also searched.
///
/// The progress is reported as files processed from the filesystem, not files within the zips. X zips each of Y files will report X operations, not X*Y operations.
#[derive(Parser, Debug, Default, Clone)]
#[command(version, about)]
pub struct Args {
    /// directory to search
    #[arg()]
    directory: String,

    /// only analyze files with names matching this regex
    #[arg()]
    file_pat: String,

    /// report lines that match this regex
    #[arg()]
    pattern: String,

    /// report skipped files
    #[arg(long, short = 'v')]
    verbose: bool,

    /// do not report non-text file errors
    #[arg(long, short = 'q')]
    quiet: bool,

    /// delimiter between file name and matching line
    #[arg(short = 'd', default_value = ": ")]
    delimiter: String,

    /// how many directories to process in parallel
    #[arg(long, default_value_t=num_cpus::get())]
    parallel: usize,

    /// do not report file name in results
    #[arg(long)]
    no_file: bool,

    /// only report the file name once in the results
    #[arg(long)]
    file_only: bool,

    /// max consecutive errors to allow before skipping file.
    #[arg(long, default_value_t = 3)]
    max_errors: usize,

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

        zip_dir_analyzer.walk_path(Path::new(binding.as_str()))?;

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
            self.walk_dir(&path)
        } else if path_str.ends_with(".zip") {
            let mut file = fs::File::open(path)?;
            self.walk_zip(path_str, &mut file)
        } else if path.is_file() {
            self.grep_file(path_str, &File::open(path)?)
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

    /// path is a zip, so unzip and process each entry
    fn walk_zip<R: Read>(&self, path: &str, read: &mut R) -> Result<()> {
        loop {
            match read_zipfile_from_stream(read)? {
                Some(mut file) => {
                    if !file.is_dir() {
                        let path = path.to_string() + "!" + file.name();
                        if file.name().ends_with(".zip") {
                            eprintln!("Zip in zip not supported.");
                            // self.walk_zip(&path, &mut file)?;
                        } else {
                            self.search_file(&path, &mut file)?;
                        }
                    }
                }
                None => return Ok(()),
            }
        }
    }

    fn search_file<T: Read>(&self, path: &str, data: T) -> Result<()> {
        if self.file_regex.is_match(path) {
            self.grep_file(path, data);
        } else if self.args.verbose {
            eprintln!("INFO: skipping {}", path);
        }
        Ok(())
    }

    /// all reporting
    fn report(&self, file: &str, line: &str) -> Result<()> {
        if self.args.no_file {
            stdout().write_fmt(format_args!("{line}\n"))?;
        } else {
            stdout().write_fmt(format_args!("{}{}{}\n", file, self.args.delimiter, line))?;
        }
        Ok(())
    }
}
