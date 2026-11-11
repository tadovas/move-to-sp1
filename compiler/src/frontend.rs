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
    command_line::compiler::PASS_CFGIR,
    diagnostics::report_diagnostics_to_buffer,
    editions::{Edition, Flavor},
    shared::{NumericalAddress, PackageConfig, PackagePaths},
    unit_test::{
        ExpectedFailure, ExpectedMoveError, MoveErrorType, plan_builder::construct_test_plan,
    },
};
use move_core_types::vm_status::StatusCode;
use move_symbol_pool::Symbol;

pub use crate::package::SourcePackage;

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

pub struct Compiled {
    /// Modules of the root package (the first of `packages`).
    pub targets: Vec<CompiledModule>,
    /// Modules of its dependencies and the frameworks.
    pub deps: Vec<CompiledModule>,
    /// Unit tests of the root package (test mode only).
    pub tests: Vec<UnitTest>,
}

/// A `#[test]` function and its expected outcome.
pub struct UnitTest {
    /// `module::function`
    pub name: String,
    pub expected: Expected,
}

/// Expected outcome of a unit test (from `#[expected_failure(...)]`).
#[derive(Clone, Copy, Debug)]
pub enum Expected {
    Success,
    /// A Move `abort`, optionally with a specific code.
    Abort(Option<u64>),
    /// A VM error with this status code (e.g. `arithmetic_error` = 4017).
    VmError(u64),
}

/// Compile `packages` (root first) together with the embedded frameworks. `test_mode`
/// includes `#[test_only]` code (test helpers, `sui::test_scenario`) and `#[test]` functions.
pub fn compile(
    packages: &[SourcePackage],
    addresses: &[(String, String)],
    test_mode: bool,
) -> Result<Compiled> {
    let root = packages
        .first()
        .context("no package to compile")?
        .name
        .as_str();
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
    let mut package_paths: Vec<PackagePaths> = packages
        .iter()
        .enumerate()
        .map(|(i, p)| PackagePaths {
            // dependencies: `is_dependency` silences lints in code the user doesn't own
            name: Some((
                p.name.as_str().into(),
                PackageConfig {
                    edition: p.edition,
                    is_dependency: i > 0,
                    ..config.clone()
                },
            )),
            paths: to_symbols(&p.sources),
            named_address_map: named.clone(),
        })
        .collect();

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
        package_paths.push(PackagePaths {
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

    let (files, res) = Compiler::from_package_paths(None, package_paths, vec![])?
        .set_flags(if test_mode {
            Flags::testing()
        } else {
            Flags::empty()
        })
        .run::<PASS_CFGIR>()
        .context("running Move compiler")?;
    // Same steps as the Move compiler's own test flow: test plan from the CFGIR, then bytecode.
    let mut test_plan = vec![];
    let res = res.and_then(|cfgir_compiler| {
        let (compiler, cfgir) = cfgir_compiler.into_ast();
        if test_mode {
            let env = compiler.compilation_env();
            test_plan =
                construct_test_plan(env, Some(Symbol::from(root)), &cfgir).unwrap_or_default();
        }
        compiler.at_cfgir(cfgir).build()
    });
    let res = res.map_err(|(_pass, diags)| diags);
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
        let user = u.named_module.package_name == Some(Symbol::from(root));
        if user { &mut targets } else { &mut deps }.push(u.named_module.module);
    }
    let tests = test_plan
        .iter()
        .flat_map(|m| {
            m.tests
                .iter()
                .filter(|(_, t)| t.arguments.is_empty())
                .map(|(name, t)| UnitTest {
                    name: format!("{}::{name}", m.module_id.name()),
                    expected: expected_outcome(t.expected_failure.as_ref()),
                })
        })
        .collect();
    Ok(Compiled {
        targets,
        deps,
        tests,
    })
}

fn expected_outcome(f: Option<&ExpectedFailure>) -> Expected {
    let code = |e: &MoveErrorType| match e {
        MoveErrorType::Code(c) => Expected::Abort(Some(*c)),
        // named error constants (`abort_code = module::EName`): any abort is accepted
        MoveErrorType::ConstantName(_) => Expected::Abort(None),
    };
    match f {
        None => Expected::Success,
        Some(ExpectedFailure::Expected) => Expected::Abort(None),
        Some(ExpectedFailure::ExpectedWithCodeDEPRECATED(e)) => code(e),
        Some(ExpectedFailure::ExpectedWithError(ExpectedMoveError(status, sub, _))) => {
            match (status, sub) {
                (StatusCode::ABORTED, Some(e)) => code(e),
                (StatusCode::ABORTED, None) => Expected::Abort(None),
                (status, _) => Expected::VmError(*status as u64),
            }
        }
    }
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
