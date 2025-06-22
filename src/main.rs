use anyhow::{Ok, Result};
use clap::*;
use executors::{threadpool_executor::ThreadPoolExecutor, Executor};
use indicatif::{ProgressBar, ProgressStyle};
use regex::Regex;
use std::{
    fs::{self, File},
    io::{self, Read},
    path::Path,
    sync::{atomic::AtomicU64, Arc, Mutex},
    thread,
    time::Duration,
};

use crate::{jq_processor::JqProcessor, regex_processor::RegexProcessor};

mod jq_processor;
mod regex_processor;
mod shared_iterator;

fn main() -> Result<()> {
    let args = Args::parse();
    let pattern = &args.pattern.clone();
    if args.jq {
        ZipDirAnalyzer::run(args, JqProcessor::new(pattern)?)
    } else {
        ZipDirAnalyzer::run(args, RegexProcessor::new(pattern)?)
    }
}

trait TextProcessor: Send + Clone {
    fn grep_file<T: Read>(&self, path: &str, data: T) -> Result<bool>;
}

#[derive(Debug, Default, Clone, ValueEnum)]
enum Output {
    #[default]
    /// File name followed by the pattern match.
    All,
    /// File name. May be the name of a zip.
    File,
    /// File name, including zip file entries.
    Entry,
    /// The pattern match result.
    Pattern,
}

/// Search directory for files matching the file_pat that include the pattern. The contents of zip files are also searched.
///
/// The progress is reported as files processed from the filesystem, not files within the zips. X zips each of Y files will report X operations, not X*Y operations.
#[derive(Parser, Debug, Default, Clone)]
#[command(version, about)]
pub struct Args {
    /// What output is desired.
    #[arg(value_enum)]
    output: Output,

    /// Directory to search. Use '-' to indicate that the list of directories will be on stdin.
    #[arg()]
    directory: String,

    /// Only analyze files with names matching this regex.
    #[arg()]
    file_pat: String,

    /// Report lines that match this regex (or jq expression).
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

    /// Max consecutive errors to allow before skipping file.
    #[arg(long, default_value_t = 5)]
    max_errors: usize,

    /// Use jaq (similar to jq) to query JSON files instead of regex.
    #[arg(long, default_value_t = false)]
    jq: bool,

    /// How many lines after matching line should be reported.
    #[arg(long, short = 'A', default_value_t = 0)]
    after: u32,
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
        let directory = args.directory.clone();
        let zip_dir_analyzer = ZipDirAnalyzer {
            pool: Arc::new(Mutex::new(ThreadPoolExecutor::new(args.parallel))),
            ops_scheduled: Default::default(),
            ops_complete: Default::default(),
            processor,
            file_regex: Regex::new(&args.file_pat)?,
            args,
            progress: ProgressBar::new(100),
        };

        zip_dir_analyzer
            .progress
            .set_style(ProgressStyle::with_template(
                "{bar} {pos}/{len} {wide_msg}",
            )?);

        if directory == "-" {
            for line in io::stdin().lines() {
                zip_dir_analyzer.schedule_walk_path(Path::new(line?.as_str()));
            }
        } else {
            zip_dir_analyzer.walk_path(Path::new(directory.as_str()))?;
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
            self.schedule_walk_path(entry?.path().as_path());
        }
        Ok(())
    }

    fn schedule_walk_path(&self, path: &std::path::Path) {
        // increment scheduled ops
        self.ops_scheduled
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let self_clone = self.clone();
        let path = path.to_path_buf();
        self.pool.lock().unwrap().execute(move || {
            let result = self_clone.walk_path(&path);
            // log any failure
            if result.is_err() && !self_clone.args.quiet {
                eprintln!(
                    "WARN: {} skipped due to {}",
                    path.to_str().unwrap(),
                    result.unwrap_err()
                );
            }
            // decrement scheduled ops
            self_clone
                .ops_complete
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
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
    fn report(&self, file: &str, lines: &mut dyn Iterator<Item = String>) -> Result<bool> {
        match self.args.output {
            Output::All => {
                let delimiter = &self.args.delimiter;
                for _ in 0..self.args.after + 1 {
                    if let Some(line) = lines.next() {
                        println!("{file}{delimiter}{line}");
                    }
                }
                Ok(false)
            }
            Output::File => {
                let file = file.split(&self.args.zip_delimiter).next().unwrap_or(file);
                println!("{file}");
                Ok(true)
            }
            Output::Entry => {
                println!("{file}");
                Ok(true)
            }
            Output::Pattern => {
                for _ in 0..self.args.after + 1 {
                    if let Some(line) = lines.next() {
                        println!("{line}");
                    }
                }
                Ok(false)
            }
        }
    }
}
