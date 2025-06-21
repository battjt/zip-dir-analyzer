use std::io::Read;

use anyhow::Result;
use jaq_interpret::{Ctx, Filter, FilterT, ParseCtx, RcIter, Val};
use serde_json::Value;

use crate::{TextProcessor, ZipDirAnalyzer};

/// Under development
#[derive(Clone)]
pub struct JqProcessor {
    filter: Filter,
}

impl JqProcessor {
    pub fn new(filter: &str) -> Result<Self> {
        let mut ctx = ParseCtx::new(Vec::new());
        // ctx.insert_natives(jaq_core::core());
        // ctx.insert_defs(jaq_std::std());
        let (f, errs) = jaq_parse::parse(filter, jaq_parse::main());
        if !errs.is_empty() {
            let error_message = errs
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(anyhow::anyhow!(error_message));
        }

        let filter = ctx.compile(f.expect("Failed to parse JQ"));
        Ok(Self { filter })
    }
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
                            // optimize?
                            let str = (**json_val.as_str().unwrap()).clone();
                            if self.report(path, &mut core::iter::once(str))? {
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
