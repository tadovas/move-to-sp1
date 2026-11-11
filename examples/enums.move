/// Exercises enums: match (by value, by ref, by mut ref), generic enums, enums with
/// heap payloads (copy/equality), enums inside vectors.
module demo::enums;

public enum Shape has copy, drop {
    Circle { r: u64 },
    Rect { w: u64, h: u64 },
    Empty,
}

public enum Maybe<T> has copy, drop {
    Nothing,
    Just(T),
}

fun area(s: &Shape): u64 {
    match (s) {
        Shape::Circle { r } => 3 * *r * *r,
        Shape::Rect { w, h } => *w * *h,
        Shape::Empty => 0,
    }
}

fun widen(s: &mut Shape) {
    match (s) {
        Shape::Rect { w, h: _ } => *w = *w * 2,
        _ => {},
    }
}

fun unwrap_or<T: drop>(m: Maybe<T>, default: T): T {
    match (m) {
        Maybe::Just(x) => x,
        Maybe::Nothing => default,
    }
}

/// areas of [Circle(2), Rect(3x4), Empty] = 12 + 12 + 0 = 24
public fun test_shapes(): u64 {
    let shapes = vector[Shape::Circle { r: 2 }, Shape::Rect { w: 3, h: 4 }, Shape::Empty];
    let mut total = 0;
    shapes.do_ref!(|s| total = total + area(s));
    total
}

/// mutate through &mut match: Rect(3x4) -> Rect(6x4) = 24; Circle unchanged = 12 -> 2412
public fun test_widen(): u64 {
    let mut a = Shape::Rect { w: 3, h: 4 };
    let mut b = Shape::Circle { r: 2 };
    widen(&mut a);
    widen(&mut b);
    area(&a) * 100 + area(&b)
}

/// generic enum at u64 and at vector<u8>: 7 + 5 + len("hello")*100 = 512
public fun test_generic(): u64 {
    let x = unwrap_or(Maybe::Just(7u64), 0);
    let y = unwrap_or(Maybe::Nothing, 5u64);
    let s = unwrap_or(Maybe::Just(b"hello"), b"");
    x + y + s.length() * 100
}

/// copies of enums with vector payloads are independent; equality is structural
/// equal copies -> 1, after mutating the copy they differ -> +10, tags differ -> +100
public fun test_eq(): u64 {
    let a = Maybe::Just(vector[1u64, 2, 3]);
    let mut b = a;
    let mut r = if (a == b) 1 else 0;
    match (&mut b) {
        Maybe::Just(v) => v.push_back(4),
        Maybe::Nothing => {},
    };
    if (a != b) r = r + 10;
    if (a != Maybe::Nothing) r = r + 100;
    r
}
