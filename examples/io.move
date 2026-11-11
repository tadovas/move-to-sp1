/// Structured entry I/O (BCS <-> JSON in the runner) and runtime-backed natives:
/// std::bcs, std::hash, std::string, std::debug.
module demo::io;

use std::string::String;

public struct Point has copy, drop { x: u64, y: u64 }

public enum Shape has copy, drop {
    Circle { r: u64 },
    Rect { w: u64, h: u64 },
    Empty,
}

public fun scale(p: Point, k: u64): Point {
    Point { x: p.x * k, y: p.y * k }
}

public fun sum_points(ps: vector<Point>): Point {
    let mut acc = Point { x: 0, y: 0 };
    ps.do!(|p| { acc.x = acc.x + p.x; acc.y = acc.y + p.y; });
    acc
}

public fun grow(s: Shape): Shape {
    match (s) {
        Shape::Circle { r } => Shape::Circle { r: r + 1 },
        Shape::Rect { w, h } => Shape::Rect { w: w + 1, h: h + 1 },
        Shape::Empty => Shape::Empty,
    }
}

public fun greet(name: String): String {
    let mut s = b"hello, ".to_string();
    s.append(name);
    s
}

public fun digests(data: vector<u8>): (vector<u8>, vector<u8>) {
    (std::hash::sha2_256(data), std::hash::sha3_256(data))
}

public fun encode(p: Point): vector<u8> {
    std::bcs::to_bytes(&p)
}

public fun show(p: Point, s: Shape, owner: address): u64 {
    std::debug::print(&p);
    std::debug::print(&s);
    std::debug::print(&vector[1u16, 2, 3]);
    std::debug::print(&owner);
    std::debug::print(&b"bytes".to_string());
    0
}
