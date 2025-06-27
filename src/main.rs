use anyhow::{Ok, Result};
use clap::*;
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
use threadpool::ThreadPool;

use crate::{jq_processor::JqProcessor, regex_processor::RegexProcessor};

mod jq_processor;
mod regex_processor;
mod shared_iterator;

fn main() -> Result<()> {
    let args = Args::parse();
    let pattern = &args.pattern.clone();
    if args.jq {
        ZipDirAnalyzer::new(args, JqProcessor::new(pattern)?)?.run()
    } else {
        ZipDirAnalyzer::new(args, RegexProcessor::new(pattern)?)?.run()
    }
}

trait TextProcessor: Send {
    fn process_file<T: Read>(&self, path: &str, data: T) -> Result<bool>;
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

    /// Delimiter between lines when `after` is used.
    #[arg(long, default_value = "\n")]
    line_delimiter: String,

    /// How many files to process in parallel.
    #[arg(long, default_value_t=2 * num_cpus::get())]
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

struct ZipDirAnalyzer<TP> {
    pool: ThreadPool,
    stdout_lock: Mutex<()>,
    ops_complete: AtomicU64,
    processor: TP,
    file_regex: Regex,
    args: Args,
    progress: ProgressBar,
}

impl<TP: Send + Sync + 'static> ZipDirAnalyzer<TP>
where
    ZipDirAnalyzer<TP>: TextProcessor,
{
    /// Main entry point.
    pub fn run(self) -> Result<()> {
        self.progress.set_style(ProgressStyle::with_template(
            "{bar} {pos}/{len} {wide_msg}",
        )?);

        let this = Arc::new(self);
        let c = this.clone();
        this.pool.execute(move || {
            if c.args.directory == "-" {
                for line in io::stdin().lines() {
                    c.schedule_walk_path(Path::new(line.unwrap().as_str()));
                }
            } else {
                c.schedule_walk_path(Path::new(c.args.directory.as_str()));
            }
        });

        let mut scheduled = 1;
        let mut complete = 0;
        // wait for all processing to complete
        while scheduled > complete {
            thread::sleep(Duration::from_millis(50));
            complete = this.ops_complete.load(std::sync::atomic::Ordering::Relaxed);
            scheduled = (this.pool.active_count() + this.pool.queued_count()) as u64 + complete;
            this.progress.set_length(scheduled);
            this.progress.set_position(complete);
        }
        this.progress
            .println(format!("Complete {complete} of {scheduled}"));

        Ok(())
    }

    fn search_file<T: Read>(&self, path: &str, data: T) -> Result<()> {
        if self.file_regex.is_match(path) {
            self.progress.set_message(format!("processing: {path}"));
            self.process_file(path, data)?;
        } else if self.args.verbose {
            self.progress.println(format!("INFO: skipping {path}"));
        }
        Ok(())
    }

    /// all reporting
    fn report(&self, file: &str, lines: &mut dyn Iterator<Item = String>) -> Result<bool> {
        let _io = self.stdout_lock.lock();
        match self.args.output {
            Output::File => {
                let file = file.split(&self.args.zip_delimiter).next().unwrap_or(file);
                println!("{file}");
                Ok(true)
            }
            Output::Entry => {
                println!("{file}");
                Ok(true)
            }
            Output::All => {
                let delimiter = &self.args.delimiter;
                let line_delimiter = &self.args.line_delimiter;
                let s = lines
                    .take(1 + self.args.after as usize)
                    .map(|line| format!("{file}{delimiter}{line}"))
                    .fold(String::new(), |a, b| a + line_delimiter + &b);
                println!("{s}");
                Ok(false)
            }
            Output::Pattern => {
                let line_delimiter = &self.args.line_delimiter;
                let s = lines
                    .take(1 + self.args.after as usize)
                    .map(|line| line.to_string())
                    .fold(String::new(), |a, b| a + line_delimiter + &b);
                println!("{s}");
                Ok(false)
            }
        }
    }

    pub fn new(args: Args, processor: TP) -> Result<ZipDirAnalyzer<TP>>
    where
        TP: Send + 'static,
    {
        Ok(ZipDirAnalyzer {
            pool: ThreadPool::with_name("worker".to_string(), args.parallel),
            stdout_lock: Mutex::new(()),
            ops_complete: Default::default(),
            processor,
            file_regex: Regex::new(&args.file_pat)?,
            args,
            progress: ProgressBar::new(100),
        })
    }

    /// path is a directory.  Process each entry in a separate thread.
    fn walk_dir(self: &Arc<Self>, path: &Path) -> Result<()> {
        for entry in std::fs::read_dir(path)? {
            self.schedule_walk_path(entry?.path().as_path());
        }
        Ok(())
    }

    fn schedule_walk_path(self: &Arc<Self>, path: &std::path::Path) {
        let path = path.to_path_buf();
        let c = self.clone();
        self.pool.execute(move || {
            c.walk_path(&path)
                .unwrap_or_else(|_| panic!("Failed to walk path {path:?}"));
        });
    }

    fn walk_path(self: &Arc<Self>, path: &Path) -> Result<()> {
        // increment ops complete, before the work, so that a failure will not
        self.ops_complete
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let result = {
            let this = &self;
            let path_str = path.to_str().unwrap();
            if path.is_dir() {
                this.walk_dir(path)
            } else if path_str.ends_with(".zip") {
                let mut file = fs::File::open(path)?;
                this.walk_zip(path_str, &mut file)
            } else if path.is_file() {
                this.progress.set_message(format!("processing: {path_str}"));
                this.process_file(path_str, &File::open(path)?)?;
                Ok(())
            } else {
                // skipping links and devices and such
                if this.args.verbose {
                    this.progress
                        .println(format!("INFO: skipping non-file {path_str}"));
                }
                Ok(())
            }
        };
        // log any failure
        if result.is_err() && !self.args.quiet {
            self.progress.println(format!(
                "WARN: {} skipped due to {}",
                path.to_str().unwrap(),
                result.unwrap_err()
            ));
        }
        Ok(())
    }

    fn walk_zip(self: &Arc<Self>, path: &str, zip_file: &mut File) -> Result<()> {
        let mut archive = zip::ZipArchive::new(zip_file)?;
        for i in 0..archive.len() {
            let zip_file = archive.by_index(i)?;
            if zip_file.is_dir() {
                // just a directory placeholder.
            } else {
                let file_name = path.to_string() + "!" + zip_file.name();
                if file_name.ends_with(".zip") {
                    self.progress
                        .println(format!("No support for a zip of a zip yet {file_name}"));
                } else {
                    self.search_file(&file_name, zip_file)?;
                }
            }
        }
        Ok(())
    }
}
