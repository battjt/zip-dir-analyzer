use std::{
    fs::{self, File},
    io::{self, BufRead, Read, Write},
    path::Path,
    sync::{Arc, Mutex},
};

use acc_reader::AccReader;
use anyhow::Result;
use clap::Parser;
use executors::threadpool_executor::ThreadPoolExecutor;
use regex::Regex;

fn main() -> Result<()> {
    Logga::run(Cli::parse())
}

#[derive(Parser, Debug, Default, Clone)]
pub struct Cli {
    #[arg()]
    root_dir: String,
    #[arg()]
    file_pat: String,
    #[arg()]
    line_pat: String,
    #[arg()]
    report_file: Option<String>,
}

struct Logga<W: Write> {
    args: Cli,
    pool: ThreadPoolExecutor,
    out: Arc<Mutex<W>>,
    rx: Regex,
}

impl Logga<File> {
    fn run(args: Cli) -> Result<()> {
        let root_dir = args.root_dir.clone();
        let report_file: Option<String> = args.report_file.clone();
        let t: Box<dyn Write> = report_file.map_or(Box::new(io::stderr()) as Box<dyn Write>, |f| {
            Box::new(File::create(f).unwrap()) as Box<dyn Write>
        });
        let out = Arc::new(Mutex::new(t));
        let rx = regex::Regex::new(&args.line_pat)?;
        Logga {
            args,
            pool: ThreadPoolExecutor::default(),
            out,
            rx,
        }
        .walk_path(Path::new(root_dir.as_str()))
    }
}
impl<W: Write> Logga<W> {
    fn walk_path(&self, path: &Path) -> Result<()> {
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

    fn walk_dir(&self, path: &Path) -> Result<(), anyhow::Error> {
        for d in std::fs::read_dir(path)? {
            let path_buf = d?.path();
            self.walk_path(path_buf.as_path())?;
        }
        Ok(())
    }

    fn grep_file<T: Read>(&self, path: &str, data: T) -> Result<(), anyhow::Error> {
        let lines = io::BufReader::new(data).lines();
        for line in lines {
            let line = line?;
            if self.rx.is_match(&line) {
                self.report(path, &line)?;
            }
        }
        Ok(())
    }

    fn report(&self, file: &str, line: &str) -> Result<(), anyhow::Error> {
        Ok(self
            .out
            .lock()
            .unwrap()
            .write_fmt(format_args!("{}: {}\n", file, line))?)
    }
}
