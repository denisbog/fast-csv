//! The validation rule DSL.
//!
//! Grammar (line oriented, `#` starts a comment outside quotes):
//!
//! ```text
//! defaults {
//!   separator  = ","
//!   multi      = false
//!   compare    = eq
//!   report_limit = 10
//! }
//!
//! rule "start and end dates agree" {
//!   left            = start_date
//!   right           = end_date
//!   transform_left  = date(["%Y-%m-%d", "%d/%m/%Y"], "%Y-%m-%d")
//!   transform_right = date("%Y-%m-%d", "%Y-%m-%d")
//!   compare         = eq
//!   mapping         = none
//! }
//!
//! rule "country name -> code" {
//!   left         = country_name
//!   right        = country_code
//!   transform_left = trim | lower
//!   mapping      = auto           # extract the mapping from the data
//! }
//!
//! rule "labels agree with reference" {
//!   left          = labels
//!   right         = ref_labels
//!   multi         = true
//!   separator     = ";"
//!   mapping_files = ["ref1.csv", "ref2.csv"]
//!   mapping_left  = label
//!   mapping_right = id
//! }
//! ```
//!
//! Values are either bare tokens, quoted strings, lists `[...]`, calls
//! `name(args...)` or pipelines `a | b | c`.

use std::path::{Path, PathBuf};

use crate::compare::CompareOp;
use crate::transform::Transform;

/// A parsed DSL value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    List(Vec<Value>),
    Call(String, Vec<Value>),
    Pipeline(Vec<Value>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        let s = self.as_str()?;
        match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" | "on" => Some(true),
            "false" | "no" | "0" | "off" => Some(false),
            _ => None,
        }
    }

    pub fn as_usize(&self) -> Option<usize> {
        self.as_str()?.trim().parse().ok()
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(items) => Some(items),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum MappingSourceDef {
    /// No mapping: left is compared as-is against right.
    None,
    /// Extract the left -> right mapping from the data itself.
    Auto,
    /// Load one or more reference files.
    Files {
        files: Vec<PathBuf>,
        /// One or more columns forming the lookup key.
        left: Vec<String>,
        /// One or more columns forming the target value.
        right: Vec<String>,
        multi: bool,
        separator: String,
    },
}

#[derive(Debug, Clone)]
pub struct RuleDefaults {
    pub separator: String,
    pub multi: bool,
    pub compare: CompareOp,
    pub report_limit: usize,
    pub mapping_separator: String,
    /// Joins several columns into a single composite key.
    pub join_separator: String,
}

impl Default for RuleDefaults {
    fn default() -> Self {
        RuleDefaults {
            separator: ",".to_string(),
            multi: false,
            compare: CompareOp::Eq,
            report_limit: 10,
            mapping_separator: ",".to_string(),
            join_separator: "|".to_string(),
        }
    }
}

/// A rule as written in the DSL, before column indices are resolved.
#[derive(Debug, Clone)]
pub struct RuleDef {
    pub name: String,
    /// One column, or several columns combined into a composite key.
    pub left: Vec<String>,
    pub right: Vec<String>,
    pub transform_left: Vec<Transform>,
    pub transform_right: Vec<Transform>,
    pub compare: Option<CompareOp>,
    pub multi: Option<bool>,
    pub separator: Option<String>,
    pub join_separator: Option<String>,
    pub mapping: MappingSourceDef,
    pub report_limit: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct Program {
    pub defaults: RuleDefaults,
    pub rules: Vec<RuleDef>,
}

/// Parse a DSL file.
pub fn load_file(path: &Path) -> Result<Program, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read rules file {}: {e}", path.display()))?;
    parse(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Parse DSL source text.
pub fn parse(text: &str) -> Result<Program, String> {
    let mut defaults = RuleDefaults::default();
    let mut rules: Vec<RuleDef> = Vec::new();

    #[derive(Clone, Copy, PartialEq)]
    enum Module {
        Defaults,
        Rule,
    }

    let mut module: Option<Module> = None;
    let mut current_rule: Option<RuleDef> = None;
    let mut accumulator = String::new();
    let mut line_no = 0usize;

    for raw in text.lines() {
        line_no += 1;
        let line = strip_comment(raw);
        if accumulator.is_empty() && line.trim().is_empty() {
            continue;
        }

        // Join continuation lines until quotes/brackets balance.
        if !accumulator.is_empty() {
            accumulator.push('\n');
        }
        accumulator.push_str(&line);

        if !is_balanced(&accumulator) {
            continue;
        }

        let statement = accumulator.trim().to_string();
        accumulator.clear();
        if statement.is_empty() {
            continue;
        }

        if let Some(header) = statement.strip_suffix('{') {
            let header = header.trim();
            if header == "defaults" {
                module = Some(Module::Defaults);
            } else if let Some(name) = header.strip_prefix("rule") {
                let name = parse_rule_name(name.trim())
                    .ok_or_else(|| format!("line {line_no}: invalid rule name"))?;
                module = Some(Module::Rule);
                current_rule = Some(RuleDef {
                    name,
                    left: Vec::new(),
                    right: Vec::new(),
                    transform_left: Vec::new(),
                    transform_right: Vec::new(),
                    compare: None,
                    multi: None,
                    separator: None,
                    join_separator: None,
                    mapping: MappingSourceDef::None,
                    report_limit: None,
                });
            } else {
                return Err(format!("line {line_no}: unexpected block '{header}'"));
            }
            continue;
        }

        if statement == "}" {
            if let Some(rule) = current_rule.take() {
                rules.push(validate_rule(rule, line_no)?);
            }
            module = None;
            continue;
        }

        let (key, value) = statement
            .split_once('=')
            .ok_or_else(|| format!("line {line_no}: expected `key = value`"))?;
        let key = key.trim();
        let value = parse_value(value.trim())
            .map_err(|e| format!("line {line_no}: {e}"))?;

        match module {
            Some(Module::Defaults) => apply_default(&mut defaults, key, &value, line_no)?,
            Some(Module::Rule) => {
                let rule = current_rule
                    .as_mut()
                    .ok_or_else(|| format!("line {line_no}: assignment outside a rule"))?;
                apply_rule_key(rule, key, &value, line_no)?;
            }
            None => {
                return Err(format!(
                    "line {line_no}: assignment `{key}` outside of a block"
                ))
            }
        }
    }

    if !accumulator.trim().is_empty() {
        return Err("unterminated statement at end of file".to_string());
    }
    if let Some(rule) = current_rule.take() {
        rules.push(validate_rule(rule, line_no)?);
    }
    if rules.is_empty() {
        return Err("no rules were defined".to_string());
    }

    Ok(Program { defaults, rules })
}

fn validate_rule(rule: RuleDef, line_no: usize) -> Result<RuleDef, String> {
    if rule.left.is_empty() || rule.right.is_empty() {
        return Err(format!(
            "rule '{}': both `left` and `right` columns are required",
            rule.name
        ));
    }
    let _ = line_no;
    Ok(rule)
}

fn apply_default(
    defaults: &mut RuleDefaults,
    key: &str,
    value: &Value,
    line_no: usize,
) -> Result<(), String> {
    match key {
        "separator" => defaults.separator = expect_string(value, key, line_no)?,
        "multi" => defaults.multi = expect_bool(value, key, line_no)?,
        "compare" => {
            defaults.compare = CompareOp::parse(expect_string_ref(value, key, line_no)?)
                .ok_or_else(|| format!("line {line_no}: unknown comparison operator"))?
        }
        "report_limit" => defaults.report_limit = expect_usize(value, key, line_no)?,
        "mapping_separator" => defaults.mapping_separator = expect_string(value, key, line_no)?,
        "join_separator" => defaults.join_separator = expect_string(value, key, line_no)?,
        other => return Err(format!("line {line_no}: unknown defaults key `{other}`")),
    }
    Ok(())
}

fn apply_rule_key(
    rule: &mut RuleDef,
    key: &str,
    value: &Value,
    line_no: usize,
) -> Result<(), String> {
    match key {
        "left" => rule.left = expect_string_or_list(value, key, line_no)?,
        "right" => rule.right = expect_string_or_list(value, key, line_no)?,
        "transform_left" => rule.transform_left = parse_transforms(value, line_no)?,
        "transform_right" => rule.transform_right = parse_transforms(value, line_no)?,
        "compare" => {
            rule.compare = Some(
                CompareOp::parse(expect_string_ref(value, key, line_no)?)
                    .ok_or_else(|| format!("line {line_no}: unknown comparison operator"))?,
            )
        }
        "multi" => rule.multi = Some(expect_bool(value, key, line_no)?),
        "separator" => rule.separator = Some(expect_string(value, key, line_no)?),
        "join_separator" => rule.join_separator = Some(expect_string(value, key, line_no)?),
        "report_limit" => rule.report_limit = Some(expect_usize(value, key, line_no)?),
        "mapping" => {
            let raw = expect_string_ref(value, key, line_no)?;
            rule.mapping = match raw.to_ascii_lowercase().as_str() {
                "none" | "off" | "" => MappingSourceDef::None,
                "auto" | "extract" => MappingSourceDef::Auto,
                other => {
                    return Err(format!(
                        "line {line_no}: `mapping` must be `auto` or `none`, got `{other}`"
                    ))
                }
            };
        }
        "mapping_files" => {
            let files = expect_string_list(value, key, line_no)?
                .into_iter()
                .map(PathBuf::from)
                .collect();
            rule.mapping = MappingSourceDef::Files {
                files,
                left: match &rule.mapping {
                    MappingSourceDef::Files { left, .. } => left.clone(),
                    _ => Vec::new(),
                },
                right: match &rule.mapping {
                    MappingSourceDef::Files { right, .. } => right.clone(),
                    _ => Vec::new(),
                },
                multi: match &rule.mapping {
                    MappingSourceDef::Files { multi, .. } => *multi,
                    _ => false,
                },
                separator: match &rule.mapping {
                    MappingSourceDef::Files { separator, .. } => separator.clone(),
                    _ => ",".to_string(),
                },
            };
        }
        "mapping_left" | "mapping_right" | "mapping_multi" | "mapping_separator" => {
            if !matches!(rule.mapping, MappingSourceDef::Files { .. }) {
                rule.mapping = MappingSourceDef::Files {
                    files: Vec::new(),
                    left: Vec::new(),
                    right: Vec::new(),
                    multi: false,
                    separator: ",".to_string(),
                };
            }
            if let MappingSourceDef::Files {
                left,
                right,
                multi,
                separator,
                ..
            } = &mut rule.mapping
            {
                match key {
                    "mapping_left" => *left = expect_string_or_list(value, key, line_no)?,
                    "mapping_right" => *right = expect_string_or_list(value, key, line_no)?,
                    "mapping_multi" => *multi = expect_bool(value, key, line_no)?,
                    "mapping_separator" => *separator = expect_string(value, key, line_no)?,
                    _ => unreachable!(),
                }
            }
        }
        other => return Err(format!("line {line_no}: unknown rule key `{other}`")),
    }
    Ok(())
}

fn expect_string(value: &Value, key: &str, line_no: usize) -> Result<String, String> {
    expect_string_ref(value, key, line_no).map(str::to_string)
}

/// Accept either a single string or a list of strings. Used for `left`,
/// `right`, `mapping_left` and `mapping_right`, which can name one or several
/// columns (composite keys).
fn expect_string_or_list(
    value: &Value,
    key: &str,
    line_no: usize,
) -> Result<Vec<String>, String> {
    match value {
        Value::List(items) => {
            if items.is_empty() {
                return Err(format!("line {line_no}: `{key}` list must not be empty"));
            }
            items
                .iter()
                .map(|item| {
                    item.as_str().map(str::to_string).ok_or_else(|| {
                        format!("line {line_no}: `{key}` list must contain column names")
                    })
                })
                .collect()
        }
        other => other
            .as_str()
            .map(|s| vec![s.to_string()])
            .ok_or_else(|| format!("line {line_no}: `{key}` expects a column name or a list")),
    }
}

fn expect_string_ref<'a>(value: &'a Value, key: &str, line_no: usize) -> Result<&'a str, String> {
    value
        .as_str()
        .ok_or_else(|| format!("line {line_no}: `{key}` expects a string value"))
}

fn expect_bool(value: &Value, key: &str, line_no: usize) -> Result<bool, String> {
    value
        .as_bool()
        .ok_or_else(|| format!("line {line_no}: `{key}` expects true/false"))
}

fn expect_usize(value: &Value, key: &str, line_no: usize) -> Result<usize, String> {
    value
        .as_usize()
        .ok_or_else(|| format!("line {line_no}: `{key}` expects an integer"))
}

fn expect_string_list(value: &Value, key: &str, line_no: usize) -> Result<Vec<String>, String> {
    let items = value
        .as_list()
        .ok_or_else(|| format!("line {line_no}: `{key}` expects a list [\"a\", \"b\"]"))?;
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("line {line_no}: `{key}` list must contain strings"))
        })
        .collect()
}

/// Parse a transform pipeline value into a list of transforms.
fn parse_transforms(value: &Value, line_no: usize) -> Result<Vec<Transform>, String> {
    let steps: Vec<&Value> = match value {
        Value::Pipeline(parts) => parts.iter().collect(),
        other => vec![other],
    };
    let mut out = Vec::new();
    for step in steps {
        if let Some(transform) = parse_transform(step, line_no)? {
            out.push(transform);
        }
    }
    Ok(out)
}

fn parse_transform(value: &Value, line_no: usize) -> Result<Option<Transform>, String> {
    let transform = match value {
        Value::Str(name) => match name.trim().to_ascii_lowercase().as_str() {
            "none" | "identity" | "" => return Ok(None),
            "lower" | "lowercase" => Ok(Transform::Lower),
            "upper" | "uppercase" => Ok(Transform::Upper),
            "trim" => Ok(Transform::Trim),
            "collapse" | "squash" => Ok(Transform::Collapse),
            "int" | "integer" => Ok(Transform::Int),
            "float" | "number" => Ok(Transform::Float),
            "bool" | "boolean" => Ok(Transform::Bool),
            other => Err(format!("line {line_no}: unknown transform `{other}`")),
        },
        Value::Call(name, args) => match name.trim().to_ascii_lowercase().as_str() {
            "date" | "datetime" | "time" => {
                let (inputs, output) = parse_date_args(args, line_no)?;
                Ok(Transform::Date { inputs, output })
            }
            "replace" => {
                if args.len() != 2 {
                    return Err(format!("line {line_no}: replace(from, to) expects 2 arguments"));
                }
                Ok(Transform::Replace {
                    from: expect_string(&args[0], "replace", line_no)?,
                    to: expect_string(&args[1], "replace", line_no)?,
                })
            }
            "prefix" => Ok(Transform::Prefix(expect_string(
                args.first().ok_or_else(|| format!("line {line_no}: prefix() needs 1 argument"))?,
                "prefix",
                line_no,
            )?)),
            "suffix" => Ok(Transform::Suffix(expect_string(
                args.first().ok_or_else(|| format!("line {line_no}: suffix() needs 1 argument"))?,
                "suffix",
                line_no,
            )?)),
            other => Err(format!("line {line_no}: unknown transform `{other}`")),
        },
        _ => Err(format!("line {line_no}: invalid transform value")),
    };
    transform.map(Some)
}

fn parse_date_args(args: &[Value], line_no: usize) -> Result<(Vec<String>, String), String> {
    match args.len() {
        1 => Ok((
            vec![expect_string(&args[0], "date", line_no)?],
            "%Y-%m-%d".to_string(),
        )),
        2 => {
            let inputs = match &args[0] {
                Value::List(items) => items
                    .iter()
                    .map(|v| expect_string(v, "date", line_no))
                    .collect::<Result<Vec<_>, _>>()?,
                other => vec![expect_string(other, "date", line_no)?],
            };
            Ok((inputs, expect_string(&args[1], "date", line_no)?))
        }
        _ => Err(format!(
            "line {line_no}: date(format) or date([formats], output) expected"
        )),
    }
}

// ---------------------------------------------------------------------------
// Lexing helpers
// ---------------------------------------------------------------------------

fn parse_rule_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
    {
        parse_value(trimmed).ok()?.as_str().map(str::to_string)
    } else {
        Some(trimmed.to_string())
    }
}

/// Strip a `#` comment that is not inside quotes.
fn strip_comment(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for c in line.chars() {
        if escaped {
            out.push(c);
            escaped = false;
            continue;
        }
        match quote {
            Some(q) => {
                if c == '\\' {
                    escaped = true;
                    out.push(c);
                } else if c == q {
                    quote = None;
                    out.push(c);
                } else {
                    out.push(c);
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                    out.push(c);
                } else if c == '#' {
                    break;
                } else {
                    out.push(c);
                }
            }
        }
    }
    out
}

/// Whether quotes and value brackets are balanced (used for line continuation).
/// Block braces are intentionally ignored: `rule "x" {` is a complete header.
fn is_balanced(s: &str) -> bool {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in s.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some(q) => {
                if c == '\\' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                '[' | '(' => depth += 1,
                ']' | ')' => depth -= 1,
                _ => {}
            },
        }
    }
    depth <= 0 && quote.is_none()
}

/// Split `s` on top-level occurrences of `sep`, respecting quotes and brackets.
fn split_top_level(s: &str, sep: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for c in s.chars() {
        if escaped {
            current.push(c);
            escaped = false;
            continue;
        }
        match quote {
            Some(q) => {
                current.push(c);
                if c == '\\' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                    current.push(c);
                } else if c == '[' || c == '(' || c == '{' {
                    depth += 1;
                    current.push(c);
                } else if c == ']' || c == ')' || c == '}' {
                    depth -= 1;
                    current.push(c);
                } else if c == sep && depth == 0 {
                    parts.push(current.trim().to_string());
                    current.clear();
                } else {
                    current.push(c);
                }
            }
        }
    }
    parts.push(current.trim().to_string());
    parts
}

/// Parse a DSL value.
pub fn parse_value(input: &str) -> Result<Value, String> {
    let parts = split_top_level(input, '|');
    if parts.len() > 1 {
        if parts.iter().any(|p| p.is_empty()) {
            return Err("invalid pipeline: empty step".to_string());
        }
        let steps = parts
            .iter()
            .map(|p| parse_atom(p))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(Value::Pipeline(steps));
    }
    parse_atom(input)
}

fn parse_atom(input: &str) -> Result<Value, String> {
    let s = input.trim();
    if s.is_empty() {
        return Ok(Value::Str(String::new()));
    }

    // Quoted string.
    let first = s.chars().next().unwrap();
    if first == '"' || first == '\'' {
        if s.len() < 2 || !s.ends_with(first) {
            return Err(format!("unterminated string: {s}"));
        }
        let inner = &s[first.len_utf8()..s.len() - first.len_utf8()];
        return Ok(Value::Str(unescape(inner)));
    }

    // List.
    if let Some(inner) = s.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        if inner.trim().is_empty() {
            return Ok(Value::List(Vec::new()));
        }
        let items = split_top_level(inner, ',')
            .iter()
            .map(|p| parse_value(p))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(Value::List(items));
    }

    // Call: identifier(...)
    if let Some(open) = s.find('(') {
        if s.ends_with(')') {
            let name = s[..open].trim();
            if !name.is_empty() && is_identifier(name) {
                let inner = &s[open + 1..s.len() - 1];
                let args = if inner.trim().is_empty() {
                    Vec::new()
                } else {
                    split_top_level(inner, ',')
                        .iter()
                        .map(|p| parse_value(p))
                        .collect::<Result<Vec<_>, _>>()?
                };
                return Ok(Value::Call(name.to_string(), args));
            }
        }
    }

    Ok(Value::Str(s.to_string()))
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
}

#[allow(clippy::while_let_on_iterator)]
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some('\'') => out.push('\''),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_program() {
        let src = r#"
            defaults {
              separator = ";"
              report_limit = 3
            }
            rule "r1" {
              left = a
              right = b
              transform_left = trim | lower
              mapping = auto
            }
            rule "r2" {
              left = c
              right = d
              mapping_files = ["m1.csv", "m2.csv"]
              mapping_left = name
              mapping_right = code
            }
        "#;
        let program = parse(src).unwrap();
        assert_eq!(program.rules.len(), 2);
        assert_eq!(program.defaults.separator, ";");
        assert_eq!(program.rules[0].transform_left.len(), 2);
        assert!(matches!(program.rules[0].mapping, MappingSourceDef::Auto));
        match &program.rules[1].mapping {
            MappingSourceDef::Files { files, left, right, .. } => {
                assert_eq!(files.len(), 2);
                assert_eq!(left, &["name".to_string()]);
                assert_eq!(right, &["code".to_string()]);
            }
            _ => panic!("expected files mapping"),
        }
    }

    #[test]
    fn parses_values() {
        assert_eq!(parse_value("\"a,b\"").unwrap(), Value::Str("a,b".into()));
        assert!(matches!(parse_value("[a, b]").unwrap(), Value::List(_)));
        assert!(matches!(parse_value("date(\"%Y\", \"%Y\")").unwrap(), Value::Call(..)));
        assert!(matches!(parse_value("a | b").unwrap(), Value::Pipeline(_)));
    }

    #[test]
    fn parses_composite_columns() {
        let src = r#"
            rule "r" {
              left = [a, b]
              right = c
              join_separator = "|"
              mapping_files = ["m.csv"]
              mapping_left = [x, y]
              mapping_right = z
            }
        "#;
        let program = parse(src).unwrap();
        let rule = &program.rules[0];
        assert_eq!(rule.left, vec!["a", "b"]);
        assert_eq!(rule.right, vec!["c"]);
        assert_eq!(rule.join_separator.as_deref(), Some("|"));
        match &rule.mapping {
            MappingSourceDef::Files { left, right, .. } => {
                assert_eq!(left, &["x".to_string(), "y".to_string()]);
                assert_eq!(right, &["z".to_string()]);
            }
            _ => panic!("expected files mapping"),
        }
    }

    #[test]
    fn comments_and_quotes() {
        assert_eq!(strip_comment(r#"a = "x # y" # real comment"#).trim(), r#"a = "x # y""#);
    }
}
