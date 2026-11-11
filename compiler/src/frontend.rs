//! Move source -> verified Move bytecode, using Sui's `move-compiler` crate in-process.
//!
//! User sources are compiled together with the embedded framework packages (`std` at 0x1,
//! `sui` at 0x2, see `frameworks/README.md`), in Sui mode. All packages are compiler
//! *targets*: the compiler only emits bytecode for targets, and the translator needs the
//! framework bytecode too (only reachable functions end up in the ELF).

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

// `MOVE_STDLIB`, `SUI_FRAMEWORK`: `&[(relative path, source)]`, embedded by `build.rs`.
include!(concat!(env!("OUT_DIR"), "/frameworks.rs"));

/// An embedded framework package.
struct Framework {
    package: &'static str,
    named_address: &'static str,
    address: &'static str,
    sources: &'static [(&'static str, &'static str)],
}

const FRAMEWORKS: &[Framework] = &[
    Framework {
        package: "MoveStdlib",
        named_address: "std",
        address: "0x1",
        sources: MOVE_STDLIB,
    },
    Framework {
        package: "Sui",
        named_address: "sui",
        address: "0x2",
        sources: SUI_FRAMEWORK,
    },
];

const USER_PACKAGE: &str = "main";

pub struct Compiled {
    /// Modules from the user's sources.
    pub targets: Vec<CompiledModule>,
    /// Framework modules that targets may call into.
    pub deps: Vec<CompiledModule>,
}

pub fn compile(sources: &[PathBuf], addresses: &[(String, String)]) -> Result<Compiled> {
    let mut named: BTreeMap<Symbol, NumericalAddress> = FRAMEWORKS
        .iter()
        .map(|f| {
            (
                f.named_address.into(),
                NumericalAddress::parse_str(f.address).unwrap(),
            )
        })
        .collect();
    for (name, addr) in addresses {
        let a = NumericalAddress::parse_str(addr)
            .map_err(|e| anyhow::anyhow!("bad address {addr}: {e}"))?;
        named.insert(name.as_str().into(), a);
    }

    let config = PackageConfig {
        edition: Edition::E2024,
        flavor: Flavor::Sui,
        ..PackageConfig::default()
    };
    let to_symbols = |paths: &[PathBuf]| {
        paths
            .iter()
            .map(|p| Symbol::from(p.to_string_lossy().as_ref()))
            .collect()
    };
    let mut packages = vec![PackagePaths {
        name: Some((USER_PACKAGE.into(), config.clone())),
        paths: to_symbols(sources),
        named_address_map: named.clone(),
    }];

    // The compiler reads files from disk: materialize the embedded sources in a scratch dir.
    let scratch = ScratchDir::new()?;
    for fw in FRAMEWORKS {
        let mut paths = vec![];
        for (rel, src) in fw.sources {
            let p = scratch.0.join(fw.package).join(rel);
            std::fs::create_dir_all(p.parent().unwrap())?;
            std::fs::write(&p, src)?;
            paths.push(p);
        }
        packages.push(PackagePaths {
            // `is_dependency` silences lints in code the user doesn't own
            name: Some((
                fw.package.into(),
                PackageConfig {
                    is_dependency: true,
                    ..config.clone()
                },
            )),
            paths: to_symbols(&paths),
            named_address_map: named.clone(),
        });
    }

    let (files, res) = Compiler::from_package_paths(None, packages, vec![])?
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

    let (mut targets, mut deps) = (vec![], vec![]);
    for u in units {
        let user = u.named_module.package_name == Some(Symbol::from(USER_PACKAGE));
        if user { &mut targets } else { &mut deps }.push(u.named_module.module);
    }
    Ok(Compiled { targets, deps })
}

/// A temporary directory, removed on drop.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Result<Self> {
        let p = std::env::temp_dir().join(format!("move2sp1-{}", std::process::id()));
        std::fs::create_dir_all(&p)?;
        Ok(ScratchDir(p))
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
