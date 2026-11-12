//! Fidelity points: places where the runtime implements Sui semantics exactly at a cost, and
//! a faster, deviating variant exists behind the compiler's `--fast` flag (e.g. `ids`: Sui's
//! Blake2b object IDs vs SHA-256 on the SP1 precompile).
//!
//! Both variants run inside [`tracked!`], which wraps them in SP1 cycle-tracker markers, so
//! the execution report states exactly how many cycles each point cost and how often it ran.

/// `tracked!("point", expr)`: evaluate `expr` as an invocation of fidelity point `point`
/// (cycles attributed in the SP1 report under `fidelity:<point>`). The markers are constant
/// strings, so tracking costs two `write` syscalls per invocation.
#[macro_export]
macro_rules! tracked {
    ($point:literal, $body:expr) => {{
        $crate::fidelity::marker(concat!(
            "cycle-tracker-report-start: fidelity:",
            $point,
            "\n"
        ));
        let result = $body;
        $crate::fidelity::marker(concat!("cycle-tracker-report-end: fidelity:", $point, "\n"));
        result
    }};
}

#[doc(hidden)]
pub fn marker(line: &'static str) {
    sp1_zkvm::syscalls::syscall_write(1, line.as_ptr(), line.len());
}
