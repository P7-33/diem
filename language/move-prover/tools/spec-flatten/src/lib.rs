// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

use anyhow::{anyhow, Result};
use std::{collections::BTreeMap, str::FromStr};
use structopt::StructOpt;

use bytecode::function_target_pipeline::{FunctionVariant, VerificationFlavor};
use move_model::ast::SpecBlockTarget;

mod ast_print;
mod workflow;

// spec simplification pass
mod exp_inlining;
mod exp_trimming;

use ast_print::SpecPrinter;
use workflow::WorkflowOptions;

/// List of simplification passes available
#[derive(Clone, Debug)]
pub enum SimplificationPass {
    Inline,
    TrimAbortsIf,
}

impl FromStr for SimplificationPass {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let r = match s {
            "inline" => SimplificationPass::Inline,
            "trim_aborts_if" => SimplificationPass::TrimAbortsIf,
            _ => return Err(s.to_string()),
        };
        Ok(r)
    }
}

/// Options passed into the specification flattening tool.
#[derive(StructOpt, Clone)]
pub struct FlattenOptions {
    /// Options common and shared by the proving workflow and all passes
    #[structopt(flatten)]
    pub workflow: WorkflowOptions,

    /// Simplification processing pipeline
    #[structopt(short, long)]
    pub pipeline: Vec<SimplificationPass>,

    /// Dump stepwise result
    #[structopt(long = "dump-stepwise")]
    pub dump_stepwise: bool,

    /// Dump stepwise result in raw exp printing format
    #[structopt(long = "dump-stepwise-raw", requires = "dump-stepwise")]
    pub dump_stepwise_raw: bool,
}

//**************************************************************************************************
// Entrypoint
//**************************************************************************************************

pub fn run(options: &FlattenOptions) -> Result<()> {
    let workflow_options = &options.workflow;
    let (env, targets) = workflow::prepare(workflow_options)?;

    // make sure the original verification works
    let proved = workflow::prove(workflow_options, &env, &targets)?;
    if !proved {
        return Err(anyhow!("Original proof is not successful"));
    }

    // flatten spec in target modules
    let mut simplified_specs = BTreeMap::new();
    for (fun_id, variant) in targets.get_funs_and_variants() {
        if !matches!(
            variant,
            FunctionVariant::Verification(VerificationFlavor::Regular)
        ) {
            // only care for functions that have the regular verification variant
            continue;
        }

        let fun_env = env.get_function(fun_id);
        if !fun_env.module_env.is_target() {
            // only run on specs in target module
            continue;
        }
        match &workflow_options.target {
            None => {
                if !fun_env.has_unknown_callers() {
                    // only run on specs for external-facing functions
                    continue;
                }
            }
            Some(target) => {
                if fun_env.get_simple_name_string().as_ref() != target {
                    // only run on matched function name
                    continue;
                }
            }
        }

        // get a copy of the original spec
        let fun_target = targets.get_target(&fun_env, &variant);
        let mut fun_spec = Some(fun_target.get_spec().clone());

        // prepare for stepwise result printing
        let fun_scope = SpecBlockTarget::Function(fun_id.module_id, fun_id.id);
        let printer = SpecPrinter::new(&env, &fun_scope);
        if options.dump_stepwise {
            println!(
                "================ fun {} ================",
                fun_env.get_full_name_str()
            );
        }

        // pass the spec through the simplification pipeline
        for (i, pass) in options.pipeline.iter().enumerate() {
            let target = fun_target.clone();
            let old_spec = fun_spec.take().unwrap();
            let new_spec = match pass {
                SimplificationPass::Inline => {
                    exp_inlining::inline_all_exp_in_spec(workflow_options, target, old_spec)
                }
                SimplificationPass::TrimAbortsIf => {
                    exp_trimming::trim_aborts_ifs(workflow_options, target, old_spec)
                }
            }?;

            // dump stepwise results if requested
            if options.dump_stepwise {
                println!("step {} - {:?} {{", i, pass);
                for cond in &new_spec.conditions {
                    if options.dump_stepwise_raw {
                        println!("\t{:?} {}", cond.kind, cond.exp.display(&env));
                    } else {
                        println!("\t{}", SpecPrinter::convert(printer.print_condition(cond)));
                    }
                }
                println!("}}");
            }

            fun_spec = Some(new_spec);
        }

        simplified_specs.insert(fun_id, fun_spec.unwrap());
    }

    // dump the final result
    for (fun_id, spec) in simplified_specs {
        let fun_env = env.get_function(fun_id);
        let fun_scope = SpecBlockTarget::Function(fun_id.module_id, fun_id.id);
        let printer = SpecPrinter::new(&env, &fun_scope);
        if !spec.conditions.is_empty() {
            println!("fun {}{{", fun_env.get_full_name_str());
            for cond in &spec.conditions {
                println!("\t{}", SpecPrinter::convert(printer.print_condition(cond)));
            }
            println!("}}");
        }
    }

    // everything is OK
    Ok(())
}
