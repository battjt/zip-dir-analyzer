use std::{
    fs::{self, File},
    io::{self, BufRead, Read, Seek, Write},
    path::Path,
    sync::{Arc, Mutex},
};

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

#[derive(Clone)]
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
        .run_path(root_dir)
    }
}
trait Reek: Read + Seek {}
impl<W: Write> Logga<W> {
    /*
    1. dir
    2. zip file
         file within zip
         zip within zip
    3. file
    */
    fn run_path<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let path = path.as_ref();
        let path_str = path.to_str().unwrap();
        if path.is_dir() {
            self.run_dir(path)
        } else if path_str.ends_with(".zip") {
            let file = fs::File::open(path)?;
            self.run_zip(path_str, Box::new(&file))
        } else {
            self.grep(path_str, &File::open(path)?)
        }
    }

    fn run_zip(&self, path: &str, zip_file: Box<dyn Reek>) -> Result<(), anyhow::Error> {
        let mut archive = zip::ZipArchive::new(zip_file)?;
        for i in 0..archive.len() {
            let (file_name, is_dir) = {
                let zf = archive.by_index(i)?;
                (zf.name().to_string(), zf.is_dir())
            };
            if is_dir {
                // just a directory placeholder.
            } else {
                let file_name = path.to_string() + "!" + &file_name;
                let zip_seek = archive.by_index_seek(i)?;
                if file_name.ends_with(".zip") {
                    self.report(&file_name, "zip in zip not supported")?;
                    self.run_zip(&file_name, Box::new(&zip_seek))?;
                } else {
                    self.grep(&file_name, zip_seek)?;
                }
            }
        }
        Ok(())
    }

    fn run_dir(&self, path: &Path) -> Result<(), anyhow::Error> {
        for d in std::fs::read_dir(path)? {
            self.run_path(&d?.path())?;
        }
        Ok(())
    }

    fn grep<T: Read>(&self, path: &str, data: T) -> Result<(), anyhow::Error> {
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
