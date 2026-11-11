mod codegen;
mod frontend;
mod link;
mod target;

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use inkwell::context::Context;

#[derive(Parser)]
#[command(
    name = "move2sp1",
    about = "Compile Move to RISC-V ELF programs for the SP1 zkVM"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Compile Move sources into an SP1-compatible RISC-V ELF.
    Build {
        sources: Vec<PathBuf>,
        /// Named address assignment, e.g. `-a demo=0x2` (`std=0x1` is predefined).
        #[arg(short = 'a', long = "address", value_parser = parse_addr)]
        addresses: Vec<(String, String)>,
        /// Entry function: `module::function` or `function` (default: `main`).
        #[arg(short, long, default_value = "main")]
        entry: String,
        /// Output ELF path.
        #[arg(short, long, default_value = "out.elf")]
        output: PathBuf,
        /// Also write the generated LLVM IR (before optimization) to this path.
        #[arg(long)]
        emit_llvm: Option<PathBuf>,
        /// Use this runtime library instead of the embedded one.
        #[arg(long)]
        runtime: Option<PathBuf>,
    },
    /// Print the Move bytecode of the compiled sources.
    Dump {
        sources: Vec<PathBuf>,
        #[arg(short = 'a', long = "address", value_parser = parse_addr)]
        addresses: Vec<(String, String)>,
    },
}

fn parse_addr(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .ok_or_else(|| format!("expected NAME=ADDRESS, got {s}"))
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Dump { sources, addresses } => {
            let c = frontend::compile(&sources, &addresses)?;
            for m in &c.targets {
                println!("{m:#?}");
            }
        }
        Cmd::Build {
            sources,
            addresses,
            entry,
            output,
            emit_llvm,
            runtime,
        } => {
            // 1. Move source -> bytecode
            let compiled = frontend::compile(&sources, &addresses)?;
            let modules: Vec<_> = compiled.targets.iter().chain(&compiled.deps).collect();
            let prog = codegen::Program { modules };
            let candidates: Vec<usize> = (0..compiled.targets.len()).collect();
            let entry = prog.lookup_entry(&entry, &candidates)?;

            // 2. bytecode -> LLVM IR
            let ctx = Context::create();
            let mut cg = codegen::Codegen::new(&ctx, &prog);
            let abi = cg.compile(entry)?;
            let tm = target::machine()?;
            if let Some(p) = emit_llvm {
                target::prepare(&cg.module, &tm);
                cg.module
                    .print_to_file(&p)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            }

            // 3. LLVM IR -> riscv64 object
            let obj = target::emit_object(&cg.module, &tm, "O2")?;

            // 4. link with the runtime into an SP1 ELF
            let rt_owned;
            let rt: &[u8] = match &runtime {
                Some(p) => {
                    rt_owned =
                        std::fs::read(p).with_context(|| format!("reading {}", p.display()))?;
                    &rt_owned
                }
                None => target::RUNTIME_LIB,
            };
            let mut objects = vec![link::InputObject {
                name: "program.o".into(),
                data: &obj,
            }];
            let members =
                link::select_archive_members(&objects, &[("libmove_rt.a", rt)], &["_start"])?;
            objects.extend(members);
            let notes = [link::NoteSection {
                name: ".move2sp1.abi".into(),
                data: abi.to_text().into_bytes(),
            }];
            let elf = link::link(&objects, &notes, "_start")?;
            std::fs::write(&output, &elf)?;
            eprintln!(
                "{} -> {} ({} bytes, entry {}({}) -> ({}))",
                sources
                    .iter()
                    .map(|s| s.display().to_string())
                    .collect::<Vec<_>>()
                    .join(" "),
                output.display(),
                elf.len(),
                abi.name,
                abi.params
                    .iter()
                    .map(|s| s.name())
                    .collect::<Vec<_>>()
                    .join(", "),
                abi.returns
                    .iter()
                    .map(|s| s.name())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
    }
    Ok(())
}
