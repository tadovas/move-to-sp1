//! Link at SP1's STACK_TOP (0x7800_0000), as `cargo prove build` does: SP1 v6 rejects programs
//! with segments below it.
fn main() {
    println!("cargo:rustc-link-arg-bins=--image-base={}", 0x7800_0000u64);
}
