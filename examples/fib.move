module demo::fib;

/// Iterative Fibonacci. Overflow aborts (Move semantics): the loop also computes
/// fib(n+1), so n <= 92.
public fun fib(n: u64): u64 {
    let mut a = 0;
    let mut b = 1;
    let mut i = 0;
    while (i < n) {
        let c = a + b;
        a = b;
        b = c;
        i = i + 1;
    };
    a
}

public fun main(n: u64): u64 {
    fib(n)
}
