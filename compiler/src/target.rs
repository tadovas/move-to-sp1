//! LLVM target setup: riscv64im, bare metal, no compressed instructions (SP1 decodes
//! 32-bit instructions only), no linker relaxation (our linker does not relax).

use anyhow::{Result, anyhow};
use inkwell::{
    OptimizationLevel,
    module::Module,
    passes::PassBuilderOptions,
    targets::{
        CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine, TargetTriple,
    },
};

pub const TRIPLE: &str = "riscv64-unknown-none-elf";

pub fn machine() -> Result<TargetMachine> {
    Target::initialize_riscv(&InitializationConfig::default());
    let triple = TargetTriple::create(TRIPLE);
    let target = Target::from_triple(&triple).map_err(|e| anyhow!("{e}"))?;
    target
        .create_target_machine(
            &triple,
            "generic-rv64",
            "+m,-a,-c,-f,-d,-relax",
            OptimizationLevel::Aggressive,
            RelocMode::Static,
            CodeModel::Medium,
        )
        .ok_or_else(|| anyhow!("cannot create riscv64 target machine"))
}

pub fn prepare(module: &Module, tm: &TargetMachine) {
    module.set_triple(&tm.get_triple());
    module.set_data_layout(&tm.get_target_data().get_data_layout());
}

/// Optimize (`opt_level` = "O0".."O3") and emit an ELF relocatable object.
pub fn emit_object(module: &Module, tm: &TargetMachine, opt_level: &str) -> Result<Vec<u8>> {
    prepare(module, tm);
    module
        .verify()
        .map_err(|e| anyhow!("invalid LLVM IR: {}", e.to_string()))?;
    module
        .run_passes(
            &format!("default<{opt_level}>"),
            tm,
            PassBuilderOptions::create(),
        )
        .map_err(|e| anyhow!("LLVM passes: {e}"))?;
    let buf = tm
        .write_to_memory_buffer(module, FileType::Object)
        .map_err(|e| anyhow!("LLVM codegen: {e}"))?;
    Ok(buf.as_slice().to_vec())
}

/// The runtime static library embedded at build time (see `build.rs`).
pub const RUNTIME_LIB: &[u8] = include_bytes!(env!("MOVE2SP1_RUNTIME_LIB"));
