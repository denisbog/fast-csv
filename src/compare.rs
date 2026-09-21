//! Comparison semantics between the "expected" and "actual" value sets.
//!
//! Every cell is normalized into a set of tokens (a single token when the cell
//! is not a comma-separated multi-value). Comparisons therefore always operate
//! on sets, which makes `multi` handling orthogonal to the operator.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    /// The expected and actual sets are equal.
    Eq,
    /// The expected and actual sets differ.
    Ne,
    /// Every expected token is present in the actual set.
    Subset,
    /// Every actual token is present in the expected set.
    Superset,
    /// The sets share at least one token.
    Intersect,
}

impl CompareOp {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "eq" | "equal" | "equals" | "=" | "==" => Some(CompareOp::Eq),
            "ne" | "neq" | "!=" | "<>" => Some(CompareOp::Ne),
            "subset" | "in" | "contains_all_reverse" => Some(CompareOp::Subset),
            "superset" | "contains" => Some(CompareOp::Superset),
            "intersect" | "overlap" | "any" => Some(CompareOp::Intersect),
            _ => None,
        }
    }

    /// `expected` and `actual` must be sorted + deduplicated.
    pub fn evaluate(&self, expected: &[String], actual: &[String]) -> bool {
        match self {
            CompareOp::Eq => expected == actual,
            CompareOp::Ne => expected != actual,
            CompareOp::Subset => is_subset(expected, actual),
            CompareOp::Superset => is_subset(actual, expected),
            CompareOp::Intersect => intersect(expected, actual),
        }
    }
}

fn is_subset(a: &[String], b: &[String]) -> bool {
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => return false,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                i += 1;
                j += 1;
            }
        }
    }
    i == a.len()
}

fn intersect(a: &[String], b: &[String]) -> bool {
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => return true,
        }
    }
    false
}
