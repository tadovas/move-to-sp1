//! Move source -> verified Move bytecode, using Sui's `move-compiler` crate in-process.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use move_binary_format::CompiledModule;
use move_compiler::{
    Compiler, Flags,
    diagnostics::report_diagnostics_to_buffer,
    editions::{Edition, Flavor},
    shared::{NumericalAddress, PackageConfig, PackagePaths},
};
use move_symbol_pool::Symbol;

/// Move stdlib sources (copied from Sui's `move-stdlib`, Apache-2.0), embedded so the
/// binary is self-contained.
const STDLIB: &[(&str, &str)] = &[];

pub struct Compiled {
    /// Modules from the user's sources.
    pub targets: Vec<CompiledModule>,
    /// Dependency modules (stdlib) that targets may call into.
    pub deps: Vec<CompiledModule>,
}

pub fn compile(sources: &[PathBuf], addresses: &[(String, String)]) -> Result<Compiled> {
    let mut named: BTreeMap<Symbol, NumericalAddress> = BTreeMap::new();
    named.insert("std".into(), NumericalAddress::parse_str("0x1").unwrap());
    for (name, addr) in addresses {
        let a = NumericalAddress::parse_str(addr)
            .map_err(|e| anyhow::anyhow!("bad address {addr}: {e}"))?;
        named.insert(name.as_str().into(), a);
    }

    // Stdlib sources are written to a scratch dir so the compiler can read them as files.
    let std_dir = tempdir()?;
    let mut std_paths = vec![];
    for (name, src) in STDLIB {
        let p = std_dir.join(name);
        std::fs::write(&p, src)?;
        std_paths.push(Symbol::from(p.to_string_lossy().as_ref()));
    }

    let config = PackageConfig {
        edition: Edition::E2024,
        flavor: Flavor::Core,
        ..PackageConfig::default()
    };
    // Stdlib goes in as a second *target* package: the compiler only emits bytecode for
    // targets, and we need the stdlib bytecode too (it gets compiled into the ELF).
    let mut targets = vec![PackagePaths {
        name: Some(("main".into(), config.clone())),
        paths: sources
            .iter()
            .map(|p| Symbol::from(p.to_string_lossy().as_ref()))
            .collect(),
        named_address_map: named.clone(),
    }];
    if !std_paths.is_empty() {
        targets.push(PackagePaths {
            name: Some(("std".into(), config)),
            paths: std_paths,
            named_address_map: named,
        });
    }

    let (files, res) = Compiler::from_package_paths(None, targets, vec![])?
        .set_flags(Flags::empty())
        .build()
        .context("running Move compiler")?;
    let (units, warnings) = match res {
        Ok(ok) => ok,
        Err(diags) => {
            let buf = report_diagnostics_to_buffer(&files, diags, true);
            bail!(
                "Move compilation failed:\n{}",
                String::from_utf8_lossy(&buf)
            );
        }
    };
    if !warnings.is_empty() {
        let buf = report_diagnostics_to_buffer(&files, warnings, true);
        eprint!("{}", String::from_utf8_lossy(&buf));
    }

    let mut out = Compiled {
        targets: vec![],
        deps: vec![],
    };
    for u in units {
        let is_std = u.named_module.package_name == Some(Symbol::from("std"));
        let m = u.named_module.module;
        if is_std {
            out.deps.push(m)
        } else {
            out.targets.push(m)
        }
    }
    let _ = std::fs::remove_dir_all(&std_dir);
    Ok(out)
}

fn tempdir() -> Result<PathBuf> {
    let p = std::env::temp_dir().join(format!("move2sp1-std-{}", std::process::id()));
    std::fs::create_dir_all(&p)?;
    Ok(p)
}
