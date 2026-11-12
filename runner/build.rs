//! Builds the batch-aggregation SP1 program (`../runtime/aggregator`) with the `succinct`
//! toolchain and embeds its ELF (`MOVEVM_AGGREGATOR_ELF`), or uses a prebuilt one given by
//! `MOVEVM_AGGREGATOR=/path/to/elf`. Build time only.

use std::{path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-env-changed=MOVEVM_AGGREGATOR");
    let elf = match std::env::var("MOVEVM_AGGREGATOR") {
        Ok(p) => PathBuf::from(p),
        Err(_) => build(),
    };
    assert!(
        elf.exists(),
        "aggregator ELF not found at {}",
        elf.display()
    );
    println!("cargo:rerun-if-changed={}", elf.display());
    println!("cargo:rustc-env=MOVEVM_AGGREGATOR_ELF={}", elf.display());
}

fn build() -> PathBuf {
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("../runtime")
        .canonicalize()
        .expect("../runtime not found (set MOVEVM_AGGREGATOR to a prebuilt ELF)");
    for f in [
        "aggregator/src",
        "aggregator/build.rs",
        "aggregator/Cargo.toml",
        "Cargo.toml",
    ] {
        println!("cargo:rerun-if-changed={}", dir.join(f).display());
    }
    let mut cmd = Command::new("cargo");
    cmd.current_dir(&dir)
        .args(["build", "--release", "-p", "move-aggregator"]);
    for (k, _) in std::env::vars() {
        if k.starts_with("CARGO")
            || k.starts_with("RUSTC")
            || k == "RUSTUP_TOOLCHAIN"
            || k == "RUSTFLAGS"
        {
            cmd.env_remove(k);
        }
    }
    let status = cmd
        .status()
        .expect("failed to run cargo for the aggregator");
    assert!(status.success(), "building the aggregator failed");
    dir.join("target/riscv64im-succinct-zkvm-elf/release/move-aggregator")
}
