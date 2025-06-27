use crate::{TextProcessor, ZipDirAnalyzer};
use anyhow::Result;
use jaq_core::{
    load::{Arena, File, Loader},
    Ctx, Filter, Native, RcIter,
};
use jaq_json::Val;
use serde_json::Value;
use std::io::Read;

type JqFileType = ();

#[derive(Clone)]
pub struct JqProcessor {
    filter: Filter<Native<Val>>,
}

impl JqProcessor {
    pub fn new(filter_expr: &str) -> Result<Self> {
        let loader = Loader::new(
            // ToDo: Allow custom preludes?
            jaq_std::defs().chain(jaq_json::defs()), //.chain(semconv_prelude()), // [],
        );
        let arena = Arena::default();
        let program: File<&str, JqFileType> = File {
            code: filter_expr,
            path: (), // ToDo - give this the weaver-config location.
        };

        // parse the filter
        let modules = loader
            .load(&arena, program)
            .expect("Unable to load JAQ program");

        let funs = jaq_std::funs().chain(jaq_json::funs());
        #[allow(clippy::map_identity)]
        let filter = jaq_core::Compiler::<_, Native<_>>::default()
            // To trick compiler, we re-borrow `&'static str` with shorter lifetime.
            // This is *NOT* a simple identity function, but a lifetime inference workaround.
            .with_funs(funs.map(|x| x))
            .compile(modules)
            .expect("Unable to compile JAQ modules");
        Ok(Self { filter })
    }
}

impl TextProcessor for ZipDirAnalyzer<JqProcessor> {
    fn process_file<T: Read>(&self, path: &str, data: T) -> Result<bool> {
        let value: Result<Value, _> = serde_json::from_reader(data);
        match value {
            Result::Ok(value) => {
                let inputs = RcIter::new(core::iter::empty());
                let results = self
                    .processor
                    .filter
                    .run((Ctx::new([], &inputs), Val::from(value.clone())));
                for result in results {
                    match result {
                        Result::Ok(json_val) => {
                            if self.report(path, &mut core::iter::once(json_val.to_string()))? {
                                return Ok(true);
                            }
                        }
                        Err(err) => {
                            println!("Error: {err}");
                        }
                    };
                }
            }
            Err(je) => {
                if !self.args.quiet {
                    println!("JSON Error: {je}")
                }
            }
        };
        Ok(false)
    }
}
