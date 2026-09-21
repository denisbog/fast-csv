//! Value transformations applied to cells before comparison/mapping.
//!
//! Transforms write into a caller-provided `String` buffer so that a whole
//! pipeline can run with zero allocations once the buffer is warm (the engine
//! reuses one buffer per side across every row).

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};

/// A single transformation step.
#[derive(Debug, Clone)]
pub enum Transform {
    Lower,
    Upper,
    Trim,
    /// Trim then collapse all internal whitespace runs to a single space.
    Collapse,
    /// Parse a date/datetime using one of `inputs` formats and re-emit using
    /// `output`. Both sides are normalized to the same textual representation.
    Date { inputs: Vec<String>, output: String },
    /// Parse an integer and re-emit its canonical form (drops leading zeros,
    /// plus signs, etc.).
    Int,
    /// Parse a float and re-emit its canonical form.
    Float,
    /// Normalize common boolean spellings to `true` / `false`.
    Bool,
    Replace { from: String, to: String },
    Prefix(String),
    Suffix(String),
}

impl Transform {
    /// Apply the transform, appending the result to `out`.
    ///
    /// Returns `false` when a parsing transform failed; in that case `out` is
    /// left with the (unmodified) input.
    pub fn write_into(&self, input: &str, out: &mut String) -> bool {
        match self {
            Transform::Lower => {
                out.extend(input.chars().flat_map(char::to_lowercase));
                true
            }
            Transform::Upper => {
                out.extend(input.chars().flat_map(char::to_uppercase));
                true
            }
            Transform::Trim => {
                out.push_str(input.trim());
                true
            }
            Transform::Collapse => {
                let mut first = true;
                for word in input.split_whitespace() {
                    if !first {
                        out.push(' ');
                    }
                    out.push_str(word);
                    first = false;
                }
                true
            }
            Transform::Date { inputs, output } => match reparse_date(input, inputs, output) {
                Some(value) => {
                    out.push_str(&value);
                    true
                }
                None => {
                    out.push_str(input);
                    false
                }
            },
            Transform::Int => {
                let trimmed = input.trim();
                match trimmed.parse::<i64>() {
                    Ok(value) => {
                        push_i64(out, value);
                        true
                    }
                    Err(_) => {
                        out.push_str(input);
                        false
                    }
                }
            }
            Transform::Float => {
                let trimmed = input.trim();
                match trimmed.parse::<f64>() {
                    Ok(value) if value.is_finite() => {
                        push_float(out, value);
                        true
                    }
                    _ => {
                        out.push_str(input);
                        false
                    }
                }
            }
            Transform::Bool => match parse_bool(input) {
                Some(true) => {
                    out.push_str("true");
                    true
                }
                Some(false) => {
                    out.push_str("false");
                    true
                }
                None => {
                    out.push_str(input);
                    false
                }
            },
            Transform::Replace { from, to } => {
                if input.contains(from.as_str()) {
                    out.push_str(&input.replace(from.as_str(), to));
                } else {
                    out.push_str(input);
                }
                true
            }
            Transform::Prefix(prefix) => {
                out.push_str(prefix);
                out.push_str(input);
                true
            }
            Transform::Suffix(suffix) => {
                out.push_str(input);
                out.push_str(suffix);
                true
            }
        }
    }
}

/// Apply a full pipeline into `out`. `out` is overwritten. Returns whether all
/// steps succeeded.
pub fn apply_pipeline(transforms: &[Transform], input: &str, out: &mut String) -> bool {
    out.clear();
    let Some((first, rest)) = transforms.split_first() else {
        out.push_str(input);
        return true;
    };

    let mut ok = first.write_into(input, out);

    // Two buffers are swapped explicitly (rather than reassigning a `&str`)
    // so the borrow checker is happy and no temporary allocation is needed.
    let mut scratch = String::new();
    let mut current_is_out = true;

    for transform in rest {
        if current_is_out {
            scratch.clear();
            ok &= transform.write_into(out, &mut scratch);
            current_is_out = false;
        } else {
            out.clear();
            ok &= transform.write_into(&scratch, out);
            current_is_out = true;
        }
    }

    if !current_is_out {
        out.clear();
        out.push_str(&scratch);
    }

    ok
}

/// Combine several already-extracted cell values into one composite key.
///
/// Each part runs through `transforms` independently, then the results are
/// joined with `join`. `scratch` is reused between parts, so no allocation is
/// needed once it is warm, and the final key is written to `out`.
pub fn compose<S, I>(
    parts: I,
    transforms: &[Transform],
    join: &str,
    scratch: &mut String,
    out: &mut String,
) -> bool
where
    S: AsRef<str>,
    I: IntoIterator<Item = S>,
{
    out.clear();
    let mut ok = true;
    for (index, part) in parts.into_iter().enumerate() {
        if index > 0 {
            out.push_str(join);
        }
        ok &= apply_pipeline(transforms, part.as_ref(), scratch);
        out.push_str(scratch);
    }
    ok
}

fn push_i64(out: &mut String, value: i64) {
    use std::fmt::Write as _;
    let _ = write!(out, "{value}");
}

fn push_float(out: &mut String, value: f64) {
    use std::fmt::Write as _;
    if value == value.trunc() && value.abs() < 1e15 {
        let _ = write!(out, "{}", value as i64);
    } else {
        let _ = write!(out, "{value}");
    }
}

fn parse_bool(input: &str) -> Option<bool> {
    match input.trim().to_ascii_lowercase().as_str() {
        "true" | "t" | "yes" | "y" | "1" | "vrai" | "oui" => Some(true),
        "false" | "f" | "no" | "n" | "0" | "faux" | "non" => Some(false),
        _ => None,
    }
}

fn reparse_date(input: &str, inputs: &[String], output: &str) -> Option<String> {
    let value = input.trim();
    if value.is_empty() {
        return None;
    }

    for fmt in inputs {
        if let Ok(dt) = NaiveDateTime::parse_from_str(value, fmt) {
            return Some(dt.format(output).to_string());
        }
        if let Ok(date) = NaiveDate::parse_from_str(value, fmt) {
            let dt = date.and_time(NaiveTime::MIN);
            return Some(dt.format(output).to_string());
        }
    }

    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(value) {
        return Some(dt.naive_utc().format(output).to_string());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_normalized() {
        let transform = Transform::Date {
            inputs: vec!["%d/%m/%Y".into(), "%Y-%m-%d".into()],
            output: "%Y-%m-%d".into(),
        };
        let mut out = String::new();
        assert!(transform.write_into("31/12/2020", &mut out));
        assert_eq!(out, "2020-12-31");
        out.clear();
        assert!(transform.write_into("2020-01-05", &mut out));
        assert_eq!(out, "2020-01-05");
        out.clear();
        assert!(!transform.write_into("not a date", &mut out));
    }

    #[test]
    fn pipeline_chains() {
        let pipeline = vec![Transform::Trim, Transform::Lower];
        let mut out = String::new();
        assert!(apply_pipeline(&pipeline, "  HeLLo ", &mut out));
        assert_eq!(out, "hello");
    }

    #[test]
    fn pipeline_of_three() {
        let pipeline = vec![Transform::Trim, Transform::Upper, Transform::Suffix("!".into())];
        let mut out = String::new();
        apply_pipeline(&pipeline, " ab ", &mut out);
        assert_eq!(out, "AB!");
    }

    #[test]
    fn bools() {
        let mut out = String::new();
        assert!(Transform::Bool.write_into("YES", &mut out));
        assert_eq!(out, "true");
        out.clear();
        assert!(Transform::Bool.write_into("0", &mut out));
        assert_eq!(out, "false");
    }

    #[test]
    fn composite_keys() {
        let transforms = vec![Transform::Trim, Transform::Lower];
        let mut scratch = String::new();
        let mut out = String::new();
        let ok = compose(
            [" Widget ", "EU"],
            &transforms,
            "|",
            &mut scratch,
            &mut out,
        );
        assert!(ok);
        assert_eq!(out, "widget|eu");
    }
}
