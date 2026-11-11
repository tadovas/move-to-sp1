//! Decoded program outputs, as JSON that keeps Move's field order.
//!
//! `serde_json::Map` sorts keys (its `preserve_order` feature would change ordering for
//! every crate in the build), so outputs use this small tree and printer instead.

use std::fmt::Write;

pub enum Out {
    Null,
    Bool(bool),
    /// A JSON number, already formatted.
    Num(String),
    Str(String),
    Array(Vec<Out>),
    /// Fields in declaration order.
    Object(Vec<(String, Out)>),
}

impl Out {
    pub fn is_compound(&self) -> bool {
        matches!(self, Out::Array(_) | Out::Object(_))
    }

    /// Single-line JSON.
    pub fn compact(&self) -> String {
        let mut s = String::new();
        self.write(&mut s, None);
        s
    }

    /// Indented JSON.
    pub fn pretty(&self) -> String {
        let mut s = String::new();
        self.write(&mut s, Some(0));
        s
    }

    /// `indent`: `None` for compact output, `Some(depth)` for pretty output.
    fn write(&self, out: &mut String, indent: Option<usize>) {
        let (open_sep, sep, close_sep) = match indent {
            None => (String::new(), ",".to_string(), String::new()),
            Some(d) => (
                format!("\n{}", "  ".repeat(d + 1)),
                format!(",\n{}", "  ".repeat(d + 1)),
                format!("\n{}", "  ".repeat(d)),
            ),
        };
        let inner = indent.map(|d| d + 1);
        match self {
            Out::Null => out.push_str("null"),
            Out::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Out::Num(n) => out.push_str(n),
            Out::Str(s) => out.push_str(&serde_json::to_string(s).unwrap()),
            Out::Array(items) if items.is_empty() => out.push_str("[]"),
            Out::Object(fields) if fields.is_empty() => out.push_str("{}"),
            Out::Array(items) => {
                out.push('[');
                out.push_str(&open_sep);
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(&sep);
                    }
                    v.write(out, inner);
                }
                out.push_str(&close_sep);
                out.push(']');
            }
            Out::Object(fields) => {
                out.push('{');
                out.push_str(&open_sep);
                for (i, (k, v)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push_str(&sep);
                    }
                    let _ = write!(
                        out,
                        "{}:{}",
                        serde_json::to_string(k).unwrap(),
                        if indent.is_some() { " " } else { "" }
                    );
                    v.write(out, inner);
                }
                out.push_str(&close_sep);
                out.push('}');
            }
        }
    }
}
