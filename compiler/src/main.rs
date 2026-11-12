//! `move2sp1`: CLI over the `move2sp1` library (see `lib.rs`).

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Parser, Subcommand};
use move2sp1::{Entry, Expected, Options, Sources};
use serde_json::json;

#[derive(Parser)]
#[command(
    name = "move2sp1",
    about = "Compile Move to RISC-V ELF programs for the SP1 zkVM"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// Where the Move code comes from.
#[derive(clap::Args)]
struct Input {
    /// A package directory (with `Move.toml`), or `.move` files.
    sources: Vec<PathBuf>,
    /// Named address assignment, e.g. `-a demo=0x100` (overrides `Move.toml`).
    #[arg(short = 'a', long = "address", value_parser = parse_addr)]
    addresses: Vec<(String, String)>,
    /// Use this runtime library instead of the embedded one.
    #[arg(long)]
    runtime: Option<PathBuf>,
    /// Enable all optimisations that deviate from exact Sui behaviour ("fidelity points"):
    /// `ids` — SHA-256 object/dynamic-field IDs (SP1 precompile) instead of Sui's Blake2b
    /// ones (unique and deterministic, but not the IDs Sui would assign).
    #[arg(long)]
    fast: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Compile Move code into an SP1-compatible RISC-V ELF.
    Build {
        #[command(flatten)]
        input: Input,
        /// Entry function: `module::function` or `function` (default: `main`).
        #[arg(short, long, default_value = "main")]
        entry: String,
        /// Output ELF path.
        #[arg(short, long, default_value = "out.elf")]
        output: PathBuf,
        /// Include `#[test_only]` code (test helpers, `sui::test_scenario`).
        #[arg(long)]
        test_mode: bool,
        /// Build a *publish* program instead of an entry function: it runs every module's
        /// `init` (dependencies first) and outputs the created objects as state.
        #[arg(long, conflicts_with = "entry")]
        publish: bool,
        /// Also write the generated LLVM IR (before optimization) to this path.
        #[arg(long)]
        emit_llvm: Option<PathBuf>,
    },
    /// Compile every `#[test]` of a package into its own ELF, plus a `tests.json` manifest
    /// (run them with `movevm test <dir>`).
    Test {
        #[command(flatten)]
        input: Input,
        /// Output directory.
        #[arg(short, long, default_value = "out/tests")]
        output: PathBuf,
        /// Only tests whose name contains this.
        #[arg(long)]
        filter: Option<String>,
    },
    /// Print the Move bytecode of the compiled sources.
    Dump {
        #[command(flatten)]
        input: Input,
    },
}

fn parse_addr(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .ok_or_else(|| format!("expected NAME=ADDRESS, got {s}"))
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Dump { input } => {
            for m in &move2sp1::load(&input.sources(), false)?.targets {
                println!("{m:#?}");
            }
        }
        Cmd::Build {
            input,
            entry,
            output,
            test_mode,
            publish,
            emit_llvm,
        } => {
            let compiled = move2sp1::load(&input.sources(), test_mode)?;
            let entry = if publish {
                Entry::Publish
            } else {
                Entry::Function(&entry)
            };
            let options = Options {
                emit_llvm,
                ..input.options()?
            };
            let program = move2sp1::build(&compiled, entry, &options)?;
            std::fs::write(&output, &program.elf)?;
            let abi = &program.abi;
            eprintln!(
                "{} -> {} ({} bytes, entry {}({}) -> ({}))",
                describe(&input.sources),
                output.display(),
                program.elf.len(),
                abi.name,
                abi.params.join(", "),
                abi.returns.join(", "),
            );
        }
        Cmd::Test {
            input,
            output,
            filter,
        } => test(&input, &output, filter.as_deref())?,
    }
    Ok(())
}

impl Input {
    fn sources(&self) -> Sources {
        Sources {
            paths: self.sources.clone(),
            addresses: self.addresses.clone(),
        }
    }

    fn options(&self) -> Result<Options> {
        let runtime = self
            .runtime
            .as_deref()
            .map(move2sp1::read_runtime)
            .transpose()?;
        Ok(Options {
            fast: self.fast,
            runtime,
            emit_llvm: None,
        })
    }
}

/// `move2sp1 test`: one ELF per `#[test]` and a manifest of expected outcomes.
fn test(input: &Input, out: &Path, filter: Option<&str>) -> Result<()> {
    let compiled = move2sp1::load(&input.sources(), true)?;
    std::fs::create_dir_all(out)?;
    let mut manifest = vec![];
    let (mut built, mut failed) = (0, 0);
    for (t, program) in move2sp1::build_tests(&compiled, &input.options()?, filter) {
        let expected = match &t.expected {
            Expected::Success => json!("success"),
            Expected::Abort(code, location) => json!({ "abort": code, "location": location }),
            Expected::VmError(status, location) => {
                json!({ "vm_error": status, "location": location })
            }
        };
        let elf_name = format!("{}.elf", t.name.replace("::", "__"));
        match program {
            Ok(program) => {
                std::fs::write(out.join(&elf_name), program.elf)?;
                manifest.push(json!({ "name": t.name, "elf": elf_name, "expected": expected }));
                built += 1;
            }
            Err(e) => {
                eprintln!("{}: not compiled: {e:#}", t.name);
                manifest.push(json!({ "name": t.name, "expected": expected, "compile_error": format!("{e:#}") }));
                failed += 1;
            }
        }
    }
    std::fs::write(
        out.join("tests.json"),
        serde_json::to_string_pretty(&manifest)?,
    )?;
    eprintln!(
        "{built} test(s) compiled, {failed} not compiled -> {}/tests.json",
        out.display()
    );
    Ok(())
}

fn describe(sources: &[PathBuf]) -> String {
    sources
        .iter()
        .map(|s| s.display().to_string())
        .collect::<Vec<_>>()
        .join(" ")
}
