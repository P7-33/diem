// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

use diem_framework::{diem_core_modules_full_path, diem_framework_named_addresses};
use log::LevelFilter;
use move_stdlib::move_stdlib_modules_full_path;
use std::{
    fs::{create_dir_all, remove_dir_all},
    path::Path,
};

pub fn generate_script_abis(output_path: &Path) {
    recreate_dir(output_path);

    let options = move_prover::cli::Options {
        move_sources: crate::move_files(),
        move_deps: vec![
            move_stdlib_modules_full_path(),
            diem_core_modules_full_path(),
            crate::custom_move_modules_full_path(),
        ],
        move_named_address_values: move_prover::cli::named_addresses_for_options(
            &diem_framework_named_addresses(),
        ),
        verbosity_level: LevelFilter::Warn,
        run_abigen: true,
        abigen: abigen::AbigenOptions {
            output_directory: output_path.to_string_lossy().to_string(),
            // we only have script funs and no scripts so this should be fine
            compiled_script_directory: "".to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    options.setup_logging_for_test();
    move_prover::run_move_prover_errors_to_stderr(options).unwrap();
}

pub fn generate_script_builder(
    output_path: &Path,
    abi_paths: impl IntoIterator<Item = impl AsRef<Path>>,
) {
    let abis: Vec<_> = abi_paths
        .into_iter()
        .flat_map(|path| {
            transaction_builder_generator::read_abis(&[path.as_ref()]).unwrap_or_else(|_| {
                panic!("Failed to read ABIs at {}", path.as_ref().to_string_lossy())
            })
        })
        .collect();

    {
        let mut file = std::fs::File::create(&output_path).unwrap_or_else(|_| {
            panic!(
                "Failed to open file {:?} for Rust script build generation",
                output_path
            )
        });
        transaction_builder_generator::rust::output(&mut file, &abis, /* local types */ true)
            .expect("Failed to generate Rust transaction builders");
    }

    std::process::Command::new("rustfmt")
        .arg(output_path)
        .status()
        .expect("Failed to run rustfmt on generated code");
}

fn recreate_dir(dir_path: &Path) {
    remove_dir_all(&dir_path).unwrap_or(());
    create_dir_all(&dir_path).unwrap();
}
