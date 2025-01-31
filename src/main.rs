use anyhow::Result;
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

fn main() -> Result<()> {
    ZipDirAnalyzer::run(Args::parse())
}

/// Search directory for files matching the file_pat that include the line_pat. Zip files are also searched.
///
/// The progress is reported as files processed from the filesystem, not files within the zips. A X zips of Y files will report X operations, not X*Y operations.
#[derive(Parser, Debug, Default, Clone)]
pub struct Args {
    /// directory to search
    #[arg()]
    directory: String,

    /// only analyze files with names matching this regex
    #[arg()]
    file_pat: String,

    /// report lines that match this regex
    #[arg()]
    line_pat: String,

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
}

#[derive(Clone)]
struct ZipDirAnalyzer {
    pool: Arc<Mutex<ThreadPoolExecutor>>,
    ops_scheduled: Arc<AtomicU64>,
    ops_complete: Arc<AtomicU64>,
    regex: Regex,
    file_regex: Regex,
    args: Args,
    progress: ProgressBar,
}

impl ZipDirAnalyzer {
    /// Main entry point.
    pub fn run(args: Args) -> Result<()> {
        let binding = args.directory.clone();
        let zip_dir_analyzer = ZipDirAnalyzer {
            pool: Arc::new(Mutex::new(ThreadPoolExecutor::new(
                args.parallel,
            ))),
            ops_scheduled: Default::default(),
            ops_complete: Default::default(),
            regex: regex::Regex::new(&args.line_pat)?,
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
    fn walk_zip(&self, path: &str, zip_file: &mut File) -> Result<()> {
        let mut archive = zip::ZipArchive::new(zip_file)?;
        for i in 0..archive.len() {
            let  zip_file = archive.by_index(i)?;
            if zip_file.is_dir() {
                // just a directory placeholder.
            } else {
                let file_name = path.to_string() + "!" + &zip_file.name().to_string();
                if file_name.ends_with(".zip") {
                    eprintln!("No support for a zip of a zip yet {file_name}");
                } else {
                    self.grep_file(&file_name, zip_file)?;
                }
            }
        }
        Ok(())
    }

    /// base file searching routine
    fn grep_file<T: Read>(&self, path: &str, data: T) -> Result<()> {
        if self.file_regex.is_match(path) {
            let status = format!("processing: {}", path);
            self.progress.set_message(status);
            let lines = io::BufReader::new(data).lines();
            let mut consecutive_error_count = 0;
            for line in lines {
                if line.is_err() {
                    let err = line.unwrap_err();
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
                    // report errors, but continue processing file
                    if !self.args.quiet {
                        eprintln!("WARN: {path} skipped line due to {err}");
                    }
                    consecutive_error_count = consecutive_error_count + 1;
                } else {
                    let line = line?;
                    if self.regex.is_match(&line) {
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
        } else {
            if self.args.verbose {
                eprintln!("INFO: skipping {}", path);
            }
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
