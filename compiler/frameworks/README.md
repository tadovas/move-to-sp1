Move packages deployed on the Sui network, copied verbatim from Sui
(`crates/sui-framework/packages/{move-stdlib,sui-framework}/sources` at commit
1c2b97ab17ccd5fb43c445e44351bc269ddaf76f, Apache-2.0):

* `move-stdlib`   — `std` at `0x1`
* `sui-framework` — `sui` at `0x2`

They are embedded into `move2sp1` at build time (see `build.rs`) and compiled together with
user code: Move 2024 macros and `use fun`/`#[syntax]` declarations exist only in source,
so dependencies cannot be consumed as bytecode alone. Only functions reachable from the
entry point are translated.
