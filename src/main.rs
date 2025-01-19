use acc_reader::AccReader;
use anyhow::Result;
use clap::Parser;
use executors::{threadpool_executor::ThreadPoolExecutor, Executor};
use regex::Regex;
use std::{
    fs::{self, File},
    io::{self, stderr, BufRead, Read, Write},
    path::Path,
    sync::{atomic::AtomicU32, Arc, Mutex},
    thread,
    time::Duration,
};

fn main() -> Result<()> {
    ZipDirAnalyzer::run(Cli::parse())
}

#[derive(Parser, Debug, Default, Clone)]
pub struct Cli {
    #[arg()]
    root_dir: String,
    #[arg()]
    file_pat: String,
    #[arg()]
    line_pat: String,
}
#[derive(Clone)]
struct ZipDirAnalyzer {
    pool: Arc<Mutex<ThreadPoolExecutor>>,
    concurrent_ops: Arc<AtomicU32>,
    regex: Regex,
}

impl ZipDirAnalyzer {
    fn run(args: Cli) -> Result<()> {
        let zip_dir_analyzer = ZipDirAnalyzer {
            pool: Default::default(),
            concurrent_ops: Default::default(),
            regex: regex::Regex::new(&args.line_pat)?,
        };
        zip_dir_analyzer.walk_path(Path::new(args.root_dir.as_str()))?;

        // wait for all processing to complete
        while zip_dir_analyzer.concurrent_ops.load(std::sync::atomic::Ordering::Relaxed) > 0 {
            thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    }
    fn walk_dir(&self, path: &Path) -> Result<(), anyhow::Error> {
        for entry in std::fs::read_dir(path)? {
            let path_buf = entry?.path();
            let s = self.clone();
            s.concurrent_ops.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.pool.lock().unwrap().execute(move || {
                s.walk_path(path_buf.as_path()).unwrap();
                s.concurrent_ops.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            });
        }
        Ok(())
    }
    fn walk_path(&self, path: &Path) -> Result<(), anyhow::Error> {
        let path_str = path.to_str().unwrap();
        if path.is_dir() {
            self.walk_dir(&path)
        } else if path_str.ends_with(".zip") {
            let mut file = fs::File::open(path)?;
            self.walk_zip(path_str, &mut file)
        } else {
            self.grep_file(path_str, &File::open(path)?)
        }
    }
    fn walk_zip(&self, path: &str, zip_file: &mut dyn Read) -> Result<(), anyhow::Error> {
        let mut archive = zip::ZipArchive::new(AccReader::new(zip_file))?;
        for i in 0..archive.len() {
            let mut zip_file = archive.by_index(i)?;
            if zip_file.is_dir() {
                // just a directory placeholder.
            } else {
                let file_name = path.to_string() + "!" + &zip_file.name().to_string();
                if file_name.ends_with(".zip") {
                    self.walk_zip(&file_name, &mut zip_file)?;
                } else {
                    self.grep_file(&file_name, zip_file)?;
                }
            }
        }
        Ok(())
    }

    fn grep_file<T: Read>(&self, path: &str, data: T) -> Result<(), anyhow::Error> {
        let lines = io::BufReader::new(data).lines();
        for line in lines {
            let line = line?;
            if self.regex.is_match(&line) {
                self.report(path, &line)?;
            }
        }
        Ok(())
    }

    fn report(&self, file: &str, line: &str) -> Result<(), anyhow::Error> {
        Ok(stderr().write_fmt(format_args!("{}: {}\n", file, line))?)
    }
}
