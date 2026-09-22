//! Regex-aware value splitting.
//!
//! Mirrors xan's `split(string, pattern)`: a separator is either a literal
//! string or a compiled regex, depending on whether the DSL wrapped it in
//! `regex("...")`.

use regex::Regex;

#[derive(Debug, Clone)]
pub enum Separator {
    Literal(String),
    Regex(Regex),
}

impl Separator {
    pub fn literal(value: impl Into<String>) -> Self {
        Separator::Literal(value.into())
    }

    /// Split a value into raw pieces (not yet trimmed or filtered).
    pub fn split<'a>(&self, value: &'a str) -> Vec<&'a str> {
        match self {
            Separator::Literal(literal) => {
                if literal.is_empty() {
                    vec![value]
                } else {
                    value.split(literal.as_str()).collect()
                }
            }
            Separator::Regex(pattern) => pattern.split(value).collect(),
        }
    }
}

impl Default for Separator {
    fn default() -> Self {
        Separator::Literal(",".to_string())
    }
}

impl PartialEq for Separator {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Separator::Literal(a), Separator::Literal(b)) => a == b,
            (Separator::Regex(a), Separator::Regex(b)) => a.as_str() == b.as_str(),
            _ => false,
        }
    }
}
