//! move2sp1: compile Move to RISC-V ELF programs for the SP1 zkVM.
//!
//! Pipeline: Move source --(frontend: Sui's move-compiler)--> bytecode
//! --(translate)--> LLVM IR --(target: LLVM)--> riscv64 object --(link + runtime)--> ELF.
//!
//! Library API: [`load`] the Move code, then [`build`] programs from it. The `move2sp1`
//! binary is a thin CLI over these.

pub mod frontend;
pub mod link;
pub mod package;
pub mod target;
pub mod translate;

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use inkwell::context::Context;
use move_compiler::editions::Edition;

pub use frontend::{Compiled, Expected, SourcePackage, UnitTest};
pub use translate::{EntryAbi, TypeOrigin};

/// Where the Move code comes from.
#[derive(Clone, Debug, Default)]
pub struct Sources {
    /// A package directory (with `Move.toml`), or `.move` files.
    pub paths: Vec<PathBuf>,
    /// Named address assignments; they override `Move.toml`.
    pub addresses: Vec<(String, String)>,
}

/// Build options.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Use the fast variant of every fidelity point (see `translate::FIDELITY_POINTS`).
    pub fast: bool,
    /// A runtime library to link instead of the embedded one.
    pub runtime: Option<Vec<u8>>,
    /// Also write the generated LLVM IR (before optimization) here.
    pub emit_llvm: Option<PathBuf>,
}

/// What a program runs.
#[derive(Clone, Copy, Debug)]
pub enum Entry<'a> {
    /// A Move function: `module::function` or `function`.
    Function(&'a str),
    /// Every module's `init` (package publish).
    Publish,
    /// A program image: these functions callable through the dispatch table, each given as
    /// `address::module::function` with `<type, args>` if generic. The transaction (a PTB)
    /// is then input data.
    Image(&'a [String]),
}

/// A compiled SP1 program.
pub struct Program {
    pub elf: Vec<u8>,
    pub abi: EntryAbi,
}

/// Front end: compile the sources (a package resolved through `Move.toml`, or loose `.move`
/// files) together with the embedded frameworks. `test_mode` includes `#[test_only]` code.
pub fn load(sources: &Sources, test_mode: bool) -> Result<Compiled> {
    let (packages, mut addresses) = match sources.paths.as_slice() {
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
    addresses.retain(|(name, _)| !sources.addresses.iter().any(|(n, _)| n == name));
    addresses.extend(sources.addresses.iter().cloned());
    frontend::compile(&packages, &addresses, test_mode)
}

/// A package given as bytecode (e.g. fetched from chain).
#[derive(Clone, Debug, Default)]
pub struct BytecodePackage {
    /// Serialized modules (`.mv`).
    pub modules: Vec<Vec<u8>>,
    /// Where its types were defined (upgraded packages; see `translate::TypeOrigin`).
    pub type_origins: Vec<translate::TypeOrigin>,
}

/// Load packages from bytecode, without the source front end or the embedded frameworks
/// (provide `0x1` / `0x2` too). Modules of `roots` are the program's own (entry candidates).
pub fn load_bytecode(roots: &[BytecodePackage], deps: &[BytecodePackage]) -> Result<Compiled> {
    use anyhow::Context as _;
    let modules = |pkgs: &[BytecodePackage]| -> Result<Vec<move_binary_format::CompiledModule>> {
        pkgs.iter()
            .flat_map(|p| &p.modules)
            .map(|b| {
                move_binary_format::CompiledModule::deserialize_with_defaults(b)
                    .context("bad module bytecode")
            })
            .collect()
    };
    let type_origins = roots
        .iter()
        .chain(deps)
        .flat_map(|p| p.type_origins.clone())
        .collect();
    Ok(Compiled {
        targets: modules(roots)?,
        deps: modules(deps)?,
        tests: vec![],
        type_origins,
    })
}

/// Translate, optimize and link one entry point into an SP1 ELF.
pub fn build(compiled: &Compiled, entry: Entry, options: &Options) -> Result<Program> {
    let modules: Vec<_> = compiled.targets.iter().chain(&compiled.deps).collect();
    let prog = translate::Program::new(modules).with_type_origins(&compiled.type_origins);
    let candidates: Vec<usize> = (0..compiled.targets.len()).collect();

    // bytecode -> LLVM IR
    let ctx = Context::create();
    let tm = target::machine()?;
    let mut translator = translate::Translator::new(
        &ctx,
        &prog,
        tm.get_target_data(),
        translate::Options { fast: options.fast },
    );
    let abi = match entry {
        Entry::Function(name) => translator.compile(prog.lookup_entry(name, &candidates)?)?,
        Entry::Publish => translator.compile_publish(&prog.init_functions())?,
        Entry::Image(fns) => {
            let insts = fns
                .iter()
                .map(|f| parse_function(&prog, f))
                .collect::<Result<Vec<_>>>()?;
            translator.compile_image(&insts)?
        }
    };
    if let Some(p) = &options.emit_llvm {
        target::prepare(translator.module(), &tm);
        translator
            .module()
            .print_to_file(p)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }

    // LLVM IR -> riscv64 object
    let obj = target::emit_object(translator.module(), &tm, "O2")?;

    // link with the runtime into an SP1 ELF
    let rt = options.runtime.as_deref().unwrap_or(target::RUNTIME_LIB);
    let mut objects = vec![link::InputObject {
        name: "program.o".into(),
        data: &obj,
    }];
    objects.extend(link::select_archive_members(
        &objects,
        &[("libmove_rt_sui.a", rt)],
        &["_start"],
    )?);
    let notes = [link::NoteSection {
        name: ".move2sp1.abi".into(),
        data: abi.to_json().into_bytes(),
    }];
    Ok(Program {
        elf: link::link(&objects, &notes, "_start")?,
        abi,
    })
}

/// `address::module::function<T1, T2>` -> a function instance of the loaded program.
fn parse_function(prog: &translate::Program, spec: &str) -> Result<translate::FnInst> {
    use anyhow::Context as _;
    let (path, args) = match spec.find('<') {
        Some(i) => (&spec[..i], Some(&spec[i..])),
        None => (spec, None),
    };
    let parts: Vec<&str> = path.split("::").collect();
    let [addr, module, function] = parts.as_slice() else {
        bail!("expected address::module::function, got {spec}")
    };
    let addr = move_core_types::account_address::AccountAddress::from_hex_literal(&format!(
        "0x{}",
        addr.trim_start_matches("0x")
    ))
    .with_context(|| format!("bad address in {spec}"))?;
    let id = prog
        .function_by_name(&addr, module, function)
        .with_context(|| format!("function {spec} not loaded"))?;
    let type_args = match args {
        Some(a) => {
            let inner = a
                .strip_prefix('<')
                .and_then(|a| a.strip_suffix('>'))
                .with_context(|| format!("bad type arguments in {spec}"))?;
            translate::split_type_args(inner)
                .iter()
                .map(|t| translate::Type::parse(prog, t))
                .collect::<Result<_>>()?
        }
        None => vec![],
    };
    Ok(translate::FnInst { id, type_args })
}

/// Build every unit test of the root package (loaded in test mode) whose name contains
/// `filter`: one program per `#[test]`, or the reason it could not be compiled.
pub fn build_tests<'a>(
    compiled: &'a Compiled,
    options: &Options,
    filter: Option<&str>,
) -> Vec<(&'a UnitTest, Result<Program>)> {
    compiled
        .tests
        .iter()
        .filter(|t| filter.is_none_or(|f| t.name.contains(f)))
        .map(|t| (t, build(compiled, Entry::Function(&t.name), options)))
        .collect()
}

/// Read a runtime library override from disk.
pub fn read_runtime(path: &Path) -> Result<Vec<u8>> {
    use anyhow::Context as _;
    std::fs::read(path).with_context(|| format!("reading {}", path.display()))
}
