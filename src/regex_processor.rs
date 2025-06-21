use anyhow::{Ok, Result};
use regex::Regex;
use std::io::{BufRead, BufReader, Read};

use crate::{shared_iterator::SharedIterator, TextProcessor, ZipDirAnalyzer};
#[derive(Clone)]
pub struct RegexProcessor {
    regex: Regex,
}

impl RegexProcessor {
    pub fn new(regex: &str) -> Result<Self> {
        Ok(Self {
            regex: Regex::new(regex)?,
        })
    }
}
impl TextProcessor for ZipDirAnalyzer<RegexProcessor> {
    /// base file searching routine
    fn grep_file<T: Read>(&self, path: &str, data: T) -> Result<bool> {
        self.progress.set_message(format!("processing: {path}"));

        let mut consecutive_error_count = 0;
        let mut lines = BufReader::new(data).lines();
        let lines = SharedIterator::new(&mut lines);

        for line_result in lines.clone() {
            match line_result {
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
                    if self.processor.regex.is_match(&line) {
                        // line matched, so now report
                        let more_lines = &mut lines.clone().map(|r| r.unwrap());
                        let this_line = core::iter::once(line);
                        let mut all_lines = this_line.chain(more_lines);
                        if self.report(path, &mut all_lines)? {
                            // only needed to match once, so exit early
                            return Ok(true);
                        }
                    }
                    consecutive_error_count = 0;
                }
            }
        }
        Ok(false)
    }
}
