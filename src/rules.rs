//! Compiled rules: DSL definitions resolved against the input headers.

use std::path::PathBuf;

use regex::Regex;

use crate::compare::CompareOp;
use crate::dsl::{ColumnSpec, MappingSourceDef, Predicate, Program};
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

/// A resolved side of a rule: one or more column indices, or a fallback list
/// where the first non-empty column wins.
#[derive(Debug, Clone)]
pub enum ColumnRef {
    Columns(Vec<usize>),
    Or(Vec<usize>),
}

impl ColumnRef {
    pub fn indices(&self) -> &[usize] {
        match self {
            ColumnRef::Columns(indices) | ColumnRef::Or(indices) => indices,
        }
    }
}

/// A compiled row predicate. All column references are resolved to indices.
#[derive(Debug, Clone)]
pub enum CompiledPredicate {
    In { column: ColumnRef, values: Vec<String> },
    AnyIn { columns: Vec<ColumnRef>, values: Vec<String> },
    AllIn { columns: Vec<ColumnRef>, values: Vec<String> },
    Eq { column: ColumnRef, value: String },
    Ne { column: ColumnRef, value: String },
    Empty { column: ColumnRef },
    NotEmpty { column: ColumnRef },
    And(Vec<CompiledPredicate>),
    Or(Vec<CompiledPredicate>),
    Not(Box<CompiledPredicate>),
    Const(bool),
}

impl CompiledPredicate {
    /// Append every referenced column index (used to build the row extractor).
    pub fn collect_indices(&self, out: &mut Vec<usize>) {
        match self {
            CompiledPredicate::In { column, .. }
            | CompiledPredicate::Eq { column, .. }
            | CompiledPredicate::Ne { column, .. }
            | CompiledPredicate::Empty { column }
            | CompiledPredicate::NotEmpty { column } => out.extend_from_slice(column.indices()),
            CompiledPredicate::AnyIn { columns, .. } | CompiledPredicate::AllIn { columns, .. } => {
                for column in columns {
                    out.extend_from_slice(column.indices());
                }
            }
            CompiledPredicate::And(parts) | CompiledPredicate::Or(parts) => {
                for part in parts {
                    part.collect_indices(out);
                }
            }
            CompiledPredicate::Not(inner) => inner.collect_indices(out),
            CompiledPredicate::Const(_) => {}
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompiledRule {
    pub name: String,
    /// Human-readable, e.g. `product + region` for composite keys.
    pub left_name: String,
    pub right_name: String,
    /// One or more columns per side; `Or` picks the first non-empty one.
    pub left: ColumnRef,
    pub right: ColumnRef,
    pub transform_left: Vec<Transform>,
    pub transform_right: Vec<Transform>,
    pub compare: CompareOp,
    pub multi: bool,
    pub separator: Separator,
    pub join_separator: String,
    /// Compiled regex for `compare = matches | not_matches`.
    pub pattern: Option<Regex>,
    pub trim: bool,
    pub allow_empty: bool,
    /// When this holds, the row is skipped (see `validation_skipped`).
    pub skip: Option<CompiledPredicate>,
    /// When this does not hold, the row is excluded from mapping extraction
    /// and skipped during validation.
    pub mapping_filter: Option<CompiledPredicate>,
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

fn compile_columns(
    rule: &str,
    side: &str,
    spec: &ColumnSpec,
    headers: &[String],
) -> Result<ColumnRef, String> {
    let indices = resolve_all(rule, side, spec.names(), headers)?;
    Ok(match spec {
        ColumnSpec::Columns(_) => ColumnRef::Columns(indices),
        ColumnSpec::Or(_) => ColumnRef::Or(indices),
    })
}

/// Predicates operate on a single value, so a composite list is not allowed.
fn compile_condition_column(
    rule: &str,
    spec: &ColumnSpec,
    headers: &[String],
) -> Result<ColumnRef, String> {
    if let ColumnSpec::Columns(names) = spec {
        if names.len() > 1 {
            return Err(format!(
                "rule '{rule}': a predicate column must be a single column or or(...), got [{}]",
                names.join(", ")
            ));
        }
    }
    compile_columns(rule, "condition", spec, headers)
}

fn compile_predicate(
    rule: &str,
    predicate: &Predicate,
    headers: &[String],
) -> Result<CompiledPredicate, String> {
    Ok(match predicate {
        Predicate::In { column, values } => CompiledPredicate::In {
            column: compile_condition_column(rule, column, headers)?,
            values: values.clone(),
        },
        Predicate::AnyIn { columns, values } => CompiledPredicate::AnyIn {
            columns: columns
                .iter()
                .map(|column| compile_condition_column(rule, column, headers))
                .collect::<Result<Vec<_>, _>>()?,
            values: values.clone(),
        },
        Predicate::AllIn { columns, values } => CompiledPredicate::AllIn {
            columns: columns
                .iter()
                .map(|column| compile_condition_column(rule, column, headers))
                .collect::<Result<Vec<_>, _>>()?,
            values: values.clone(),
        },
        Predicate::Eq { column, value } => CompiledPredicate::Eq {
            column: compile_condition_column(rule, column, headers)?,
            value: value.clone(),
        },
        Predicate::Ne { column, value } => CompiledPredicate::Ne {
            column: compile_condition_column(rule, column, headers)?,
            value: value.clone(),
        },
        Predicate::Empty { column } => CompiledPredicate::Empty {
            column: compile_condition_column(rule, column, headers)?,
        },
        Predicate::NotEmpty { column } => CompiledPredicate::NotEmpty {
            column: compile_condition_column(rule, column, headers)?,
        },
        Predicate::And(parts) => CompiledPredicate::And(
            parts
                .iter()
                .map(|part| compile_predicate(rule, part, headers))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Predicate::Or(parts) => CompiledPredicate::Or(
            parts
                .iter()
                .map(|part| compile_predicate(rule, part, headers))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Predicate::Not(inner) => {
            CompiledPredicate::Not(Box::new(compile_predicate(rule, inner, headers)?))
        }
        Predicate::Const(value) => CompiledPredicate::Const(*value),
    })
}

pub fn compile(program: Program, headers: &[String]) -> Result<Plan, String> {
    let mut rules = Vec::with_capacity(program.rules.len());

    for def in program.rules {
        let left = compile_columns(&def.name, "left", &def.left, headers)?;
        let right = compile_columns(&def.name, "right", &def.right, headers)?;
        let skip = def
            .skip
            .as_ref()
            .map(|predicate| compile_predicate(&def.name, predicate, headers))
            .transpose()?;
        let mapping_filter = def
            .mapping_filter
            .as_ref()
            .map(|predicate| compile_predicate(&def.name, predicate, headers))
            .transpose()?;

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
        let trim = def.trim.unwrap_or(program.defaults.trim);
        let allow_empty = def.allow_empty.unwrap_or(program.defaults.allow_empty);

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

        let left_name = def.left.display();
        let right_name = if def.right.is_empty() {
            "(pattern)".to_string()
        } else {
            def.right.display()
        };

        rules.push(CompiledRule {
            name: def.name,
            left_name,
            right_name,
            left,
            right,
            transform_left: def.transform_left,
            transform_right: def.transform_right,
            compare,
            multi,
            separator,
            join_separator,
            pattern: def.pattern,
            trim,
            allow_empty,
            skip,
            mapping_filter,
            mapping,
            report_limit,
        });
    }

    Ok(Plan { rules })
}
