//! Compiled rules: DSL definitions resolved against the input headers.

use std::path::PathBuf;

use regex::Regex;

use crate::compare::CompareOp;
use crate::dsl::{MappingSourceDef, Program};
use crate::pattern::Separator;
use crate::transform::Transform;

/// Resolve a column reference (a header name or `#index`) against headers.
pub struct ColumnResolver;

impl ColumnResolver {
    pub fn resolve(reference: &str, headers: &[String]) -> Option<usize> {
        let reference = reference.trim();

        if let Some(rest) = reference.strip_prefix('#') {
            if let Ok(index) = rest.trim().parse::<usize>() {
                return (index < headers.len()).then_some(index);
            }
        }

        if let Some(index) = headers.iter().position(|h| h == reference) {
            return Some(index);
        }

        // Case-insensitive fallback.
        headers
            .iter()
            .position(|h| h.eq_ignore_ascii_case(reference))
    }
}

#[derive(Debug, Clone)]
pub enum MappingPlan {
    None,
    /// Extract the mapping from the data itself.
    Auto,
    /// Load from reference files.
    Files {
        files: Vec<PathBuf>,
        left: Vec<String>,
        right: Vec<String>,
        multi: bool,
        separator: Separator,
    },
}

#[derive(Debug, Clone)]
pub struct CompiledRule {
    pub name: String,
    /// Human-readable, e.g. `product + region` for composite keys.
    pub left_name: String,
    pub right_name: String,
    /// One or more columns per side; joined with `join_separator`.
    pub left_idx: Vec<usize>,
    pub right_idx: Vec<usize>,
    pub transform_left: Vec<Transform>,
    pub transform_right: Vec<Transform>,
    pub compare: CompareOp,
    pub multi: bool,
    pub separator: Separator,
    pub join_separator: String,
    /// Compiled regex for `compare = matches | not_matches`.
    pub pattern: Option<Regex>,
    pub mapping: MappingPlan,
    pub report_limit: usize,
}

pub struct Plan {
    pub rules: Vec<CompiledRule>,
}

fn resolve_all(
    rule: &str,
    side: &str,
    columns: &[String],
    headers: &[String],
) -> Result<Vec<usize>, String> {
    columns
        .iter()
        .map(|column| {
            ColumnResolver::resolve(column, headers).ok_or_else(|| {
                format!(
                    "rule '{rule}': {side} column '{column}' not found (available: {})",
                    headers.join(", ")
                )
            })
        })
        .collect()
}

pub fn compile(program: Program, headers: &[String]) -> Result<Plan, String> {
    let mut rules = Vec::with_capacity(program.rules.len());

    for def in program.rules {
        let left_idx = resolve_all(&def.name, "left", &def.left, headers)?;
        let right_idx = resolve_all(&def.name, "right", &def.right, headers)?;

        let multi = def.multi.unwrap_or(program.defaults.multi);
        let separator = def
            .separator
            .clone()
            .unwrap_or_else(|| program.defaults.separator.clone());
        let report_limit = def.report_limit.unwrap_or(program.defaults.report_limit);
        let join_separator = def
            .join_separator
            .clone()
            .unwrap_or_else(|| program.defaults.join_separator.clone());

        // `pattern` implies a regex comparison unless stated otherwise.
        let compare = match def.compare {
            Some(compare) => compare,
            None if def.pattern.is_some() => CompareOp::Matches,
            None => program.defaults.compare,
        };
        if compare.is_regex() && def.pattern.is_none() {
            return Err(format!(
                "rule '{}': compare = {} requires a `pattern`",
                def.name,
                compare.as_str()
            ));
        }
        if def.pattern.is_some() && def.right.is_empty() && !compare.is_regex() {
            return Err(format!(
                "rule '{}': `pattern` with a non-regex comparison needs a `right` column",
                def.name
            ));
        }

        let mapping = match &def.mapping {
            MappingSourceDef::None => MappingPlan::None,
            MappingSourceDef::Auto => MappingPlan::Auto,
            MappingSourceDef::Files {
                files,
                left,
                right,
                multi,
                separator,
            } => MappingPlan::Files {
                files: files.clone(),
                left: left.clone(),
                right: right.clone(),
                multi: *multi,
                separator: separator
                    .clone()
                    .unwrap_or_else(|| program.defaults.mapping_separator.clone()),
            },
        };

        let right_name = if def.right.is_empty() {
            "(pattern)".to_string()
        } else {
            def.right.join(" + ")
        };

        rules.push(CompiledRule {
            name: def.name,
            left_name: def.left.join(" + "),
            right_name,
            left_idx,
            right_idx,
            transform_left: def.transform_left,
            transform_right: def.transform_right,
            compare,
            multi,
            separator,
            join_separator,
            pattern: def.pattern,
            mapping,
            report_limit,
        });
    }

    Ok(Plan { rules })
}
