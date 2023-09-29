#![feature(try_trait_v2)]
#![feature(try_trait_v2_residual)]

#![feature(rustc_private)]
extern crate rustc_ast;
extern crate rustc_ast_pretty;
extern crate rustc_data_structures;
extern crate rustc_driver;
extern crate rustc_errors;
extern crate rustc_feature;
extern crate rustc_hash;
extern crate rustc_interface;
extern crate rustc_lint_defs;
extern crate rustc_middle;
extern crate rustc_session;
extern crate rustc_span;

pub mod cli;
pub mod config;
pub mod passes;

use std::time::Instant;

use rustc_interface::interface::Result as CompilerResult;

use crate::config::Config;

pub fn run(config: Config) -> CompilerResult<()> {
    let t_start = Instant::now();

    let Some(analysis_pass) = passes::analysis::run(&config)? else { return Ok(()) };

    if let config::Mode::PrintCode = config.opts.mode {
        println!("{}", analysis_pass.generated_crate_code);
        return Ok(());
    }

    let compilation_pass = passes::compilation::run(&config, &analysis_pass)?;

    if config.opts.report_timings {
        println!("finished in {total:.2?}",
            total = t_start.elapsed(),
        );
        println!("analysis took {analysis:.2?} (targets {targets:.2?}; mutations {mutations:.2?}; batching {batching:.2?}; codegen {codegen:.2?})",
            analysis = analysis_pass.duration,
            targets = analysis_pass.target_analysis_duration,
            mutations = analysis_pass.mutation_analysis_duration,
            batching = analysis_pass.mutation_batching_duration,
            codegen = analysis_pass.codegen_duration,
        );
        println!("compilation took {compilation:.2?}",
            compilation = compilation_pass.duration,
        );
    }

    Ok(())
}
