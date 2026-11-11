/// Exercises structs, references, vectors, generics, copy semantics and equality.
/// Each `test_*` function is an entry point returning a value that is easy to check.
module demo::types;

public struct Point has copy, drop { x: u64, y: u64 }

public struct Pair<T: copy + drop> has copy, drop { a: T, b: T }

public struct Bag has copy, drop { items: vector<u64>, owner: address }

fun manhattan(p: &Point): u64 { p.x + p.y }

fun scale(p: &mut Point, k: u64) {
    p.x = p.x * k;
    p.y = p.y * k;
}

fun swap<T: copy + drop>(p: Pair<T>): Pair<T> {
    let Pair { a, b } = p;
    Pair { a: b, b: a }
}

fun sum(v: &vector<u64>): u64 {
    let mut s = 0;
    v.do_ref!(|x| s = s + *x);
    s
}

/// (3,4) scaled by 10 -> 30 + 40 = 70
public fun test_point(): u64 {
    let mut p = Point { x: 3, y: 4 };
    scale(&mut p, 10);
    manhattan(&p)
}

/// sum of 0..n
public fun test_vec(n: u64): u64 {
    let mut v = vector[];
    let mut i = 0;
    while (i < n) {
        v.push_back(i);
        i = i + 1;
    };
    sum(&v)
}

/// generic struct instantiated at u64 and at vector<u8> -> 2*100 + 1 = 201
public fun test_generic(): u64 {
    let p = swap(Pair { a: 1u64, b: 2u64 });
    let q = swap(Pair { a: b"ab", b: b"cd" });
    let ok = q.a == b"cd" && q.b == b"ab";
    p.a * 100 + (if (ok) 1 else 0)
}

/// vectors are values: mutating a copy leaves the original alone -> 3*10 + 4 = 34
public fun test_copy(): u64 {
    let v = vector[1u64, 2, 3];
    let mut w = v;
    w.push_back(4);
    v.length() * 10 + w.length()
}

/// structural equality of structs containing vectors -> 1 (equal) + 10 (differ) = 11
public fun test_eq(): u64 {
    let a = Bag { items: vector[1u64, 2, 3], owner: @0xCAFE };
    let mut b = a;
    let same = if (a == b) 1 else 0;
    *&mut b.items[2] = 7;
    let differ = if (a != b) 10 else 0;
    same + differ
}

/// bubble sort of a scrambled 0..n; returns 1 if sorted correctly
public fun test_sort(n: u64): u64 {
    let mut v = vector[];
    let mut i = 0;
    while (i < n) {
        v.push_back((i * 7919) % n);
        i = i + 1;
    };
    let mut i = 0;
    while (i < n) {
        let mut j = 0;
        while (j + 1 < n - i) {
            if (v[j] > v[j + 1]) v.swap(j, j + 1);
            j = j + 1;
        };
        i = i + 1;
    };
    let mut k = 0;
    while (k < n) {
        if (v[k] != k) return 0;
        k = k + 1;
    };
    1
}

/// vector of structs, pop_back, unpack: popped (3,3) -> 6, two left -> 6 + 200 = 206
public fun test_pop(): u64 {
    let mut ps = vector[Point { x: 1, y: 1 }, Point { x: 2, y: 2 }, Point { x: 3, y: 3 }];
    let last = ps.pop_back();
    let Point { x, y } = last;
    x + y + ps.length() * 100
}

/// out-of-bounds index aborts with VECTOR_OPERATION_ERROR
public fun test_oob(): u64 {
    let v = vector[1u64, 2, 3];
    v[3]
}

/// std vector API built on range natives: append, insert, remove, reverse, option
/// [1,2,3] ++ [4,5] -> insert 9 at 0 -> remove idx 1 -> reverse -> [5,4,3,2,9]
/// digits as number -> 54329, plus option::some(7).destroy_some() * 100000 = 754329
public fun test_std(): u64 {
    let mut v = vector[1u64, 2, 3];
    v.append(vector[4, 5]);
    v.insert(9, 0);
    v.remove(1);
    v.reverse();
    let mut acc = 0;
    v.do!(|d| acc = acc * 10 + d);
    let o = std::option::some(7u64);
    acc + o.destroy_some() * 100000
}
