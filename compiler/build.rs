//! Locates the runtime static library (`libmove_rt.a`) and embeds it into the compiler.
//!
//! * `MOVE2SP1_RUNTIME=/path/libmove_rt.a` — use a prebuilt runtime (e.g. a release artifact).
//! * otherwise (dev) — build `../runtime` with the `succinct` toolchain for
//!   `riscv64im-succinct-zkvm-elf`. This is the only place an external tool is invoked,
//!   and only at *build* time of move2sp1; the resulting binary is self-contained.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    embed_stdlib();
    println!("cargo:rerun-if-env-changed=MOVE2SP1_RUNTIME");
    let lib = match std::env::var("MOVE2SP1_RUNTIME") {
        Ok(p) => PathBuf::from(p),
        Err(_) => build_runtime(),
    };
    assert!(
        lib.exists(),
        "runtime library not found at {}",
        lib.display()
    );
    println!("cargo:rerun-if-changed={}", lib.display());
    println!("cargo:rustc-env=MOVE2SP1_RUNTIME_LIB={}", lib.display());
}

fn build_runtime() -> PathBuf {
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("../runtime")
        .canonicalize()
        .expect("../runtime not found (set MOVE2SP1_RUNTIME to a prebuilt libmove_rt.a)");
    for f in [
        "src",
        "Cargo.toml",
        "Cargo.lock",
        ".cargo/config.toml",
        "rust-toolchain.toml",
    ] {
        println!("cargo:rerun-if-changed={}", dir.join(f).display());
    }

    let mut cmd = Command::new("cargo");
    cmd.current_dir(&dir).args(["build", "--release"]);
    // Don't leak the outer build's toolchain/flags into the nested riscv build.
    for (k, _) in std::env::vars() {
        if k.starts_with("CARGO")
            || k.starts_with("RUSTC")
            || k == "RUSTUP_TOOLCHAIN"
            || k == "RUSTFLAGS"
        {
            cmd.env_remove(k);
        }
    }
    let status = cmd.status().expect("failed to run cargo for the runtime");
    assert!(status.success(), "building the move2sp1 runtime failed");
    dir.join("target/riscv64im-succinct-zkvm-elf/release/libmove_rt.a")
}

/// Generate `$OUT_DIR/stdlib.rs`: `STDLIB: &[(file name, source)]` for every `stdlib/*.move`.
fn embed_stdlib() {
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("stdlib");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("stdlib/ directory")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "move"))
        .collect();
    files.sort();
    let entries: String = files
        .iter()
        .map(|p| {
            let name = p.file_name().unwrap().to_string_lossy();
            format!(
                "    ({name:?}, include_str!({:?})),\n",
                p.display().to_string()
            )
        })
        .collect();
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("stdlib.rs");
    std::fs::write(
        out,
        format!("pub const STDLIB: &[(&str, &str)] = &[\n{entries}];\n"),
    )
    .unwrap();
}
