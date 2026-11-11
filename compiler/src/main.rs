//! move2sp1: compile Move to RISC-V ELF programs for the SP1 zkVM.
//!
//! Pipeline: Move source --(frontend: Sui's move-compiler)--> bytecode
//! --(translate)--> LLVM IR --(target: LLVM)--> riscv64 object --(link + runtime)--> ELF.

mod frontend;
mod link;
mod package;
mod target;
mod translate;

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use clap::{Parser, Subcommand};
use inkwell::context::Context;
use move_compiler::editions::Edition;
use serde_json::json;

use frontend::{Compiled, Expected, SourcePackage};

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
            for m in &load(&input, false)?.targets {
                println!("{m:#?}");
            }
        }
        Cmd::Build {
            input,
            entry,
            output,
            test_mode,
            emit_llvm,
        } => {
            let compiled = load(&input, test_mode)?;
            let (elf, abi) = build_elf(&compiled, &entry, &input, emit_llvm.as_deref())?;
            std::fs::write(&output, &elf)?;
            eprintln!(
                "{} -> {} ({} bytes, entry {}({}) -> ({}))",
                describe(&input.sources),
                output.display(),
                elf.len(),
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

/// Front end: a package directory (resolved through `Move.toml`) or loose `.move` files.
fn load(input: &Input, test_mode: bool) -> Result<Compiled> {
    let (packages, mut addresses) = match input.sources.as_slice() {
        [dir] if dir.join("Move.toml").is_file() => {
            let g = package::load(dir, test_mode)?;
            (g.packages, g.addresses)
        }
        [dir] if dir.is_dir() => bail!("{} has no Move.toml", dir.display()),
        files => {
            let pkg = SourcePackage {
                name: "main".into(),
                edition: Edition::E2024,
                sources: files.to_vec(),
            };
            (vec![pkg], vec![])
        }
    };
    // command-line assignments win
    addresses.retain(|(name, _)| !input.addresses.iter().any(|(n, _)| n == name));
    addresses.extend(input.addresses.iter().cloned());
    frontend::compile(&packages, &addresses, test_mode)
}

/// Translate, optimize and link one entry point into an SP1 ELF.
fn build_elf(
    compiled: &Compiled,
    entry: &str,
    input: &Input,
    emit_llvm: Option<&Path>,
) -> Result<(Vec<u8>, translate::EntryAbi)> {
    let modules: Vec<_> = compiled.targets.iter().chain(&compiled.deps).collect();
    let prog = translate::Program::new(modules);
    let candidates: Vec<usize> = (0..compiled.targets.len()).collect();
    let entry = prog.lookup_entry(entry, &candidates)?;

    // bytecode -> LLVM IR
    let ctx = Context::create();
    let tm = target::machine()?;
    let mut translator = translate::Translator::new(&ctx, &prog, tm.get_target_data());
    let abi = translator.compile(entry)?;
    if let Some(p) = emit_llvm {
        target::prepare(translator.module(), &tm);
        translator
            .module()
            .print_to_file(p)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }

    // LLVM IR -> riscv64 object
    let obj = target::emit_object(translator.module(), &tm, "O2")?;

    // link with the runtime into an SP1 ELF
    let rt_owned;
    let rt: &[u8] = match &input.runtime {
        Some(p) => {
            rt_owned = std::fs::read(p).with_context(|| format!("reading {}", p.display()))?;
            &rt_owned
        }
        None => target::RUNTIME_LIB,
    };
    let mut objects = vec![link::InputObject {
        name: "program.o".into(),
        data: &obj,
    }];
    let members = link::select_archive_members(&objects, &[("libmove_rt_sui.a", rt)], &["_start"])?;
    objects.extend(members);
    let notes = [link::NoteSection {
        name: ".move2sp1.abi".into(),
        data: abi.to_json().into_bytes(),
    }];
    Ok((link::link(&objects, &notes, "_start")?, abi))
}

/// `move2sp1 test`: one ELF per `#[test]` and a manifest of expected outcomes.
fn test(input: &Input, out: &Path, filter: Option<&str>) -> Result<()> {
    let compiled = load(input, true)?;
    std::fs::create_dir_all(out)?;
    let mut manifest = vec![];
    let (mut built, mut failed) = (0, 0);
    for t in compiled
        .tests
        .iter()
        .filter(|t| filter.is_none_or(|f| t.name.contains(f)))
    {
        let expected = match t.expected {
            Expected::Success => json!("success"),
            Expected::Abort(code) => json!({ "abort": code }),
            Expected::VmError(status) => json!({ "vm_error": status }),
        };
        let elf_name = format!("{}.elf", t.name.replace("::", "__"));
        match build_elf(&compiled, &t.name, input, None) {
            Ok((elf, _)) => {
                std::fs::write(out.join(&elf_name), elf)?;
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
