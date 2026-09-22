//! Value mapping: the relation `left value -> target value`.
//!
//! A mapping can be:
//! * extracted from the data itself (`auto`), or
//! * loaded from one or more reference files.
//!
//! Because a left value may be associated with several targets, we keep the
//! full set of observed targets (for the ambiguity report) and designate a
//! canonical target (most frequent, ties broken lexicographically) used for
//! validation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use simd_csv::ByteRecord;

use crate::pattern::Separator;
use crate::rules::ColumnResolver;

/// left value -> (target value -> number of observations)
pub type MapCounts = HashMap<Box<str>, HashMap<Box<str>, u64>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappingOrigin {
    Auto,
    File,
}

#[derive(Debug, Clone)]
pub struct Mapping {
    pub origin: MappingOrigin,
    /// Full relation: left value -> sorted (target, count) pairs.
    pub targets: HashMap<Box<str>, Vec<(Box<str>, u64)>>,
    /// Canonical target for each left value.
    pub canonical: HashMap<Box<str>, Box<str>>,
}

impl Mapping {
    /// Build a mapping from raw observation counts.
    pub fn from_counts(counts: MapCounts, origin: MappingOrigin) -> Self {
        let mut targets: HashMap<Box<str>, Vec<(Box<str>, u64)>> =
            HashMap::with_capacity(counts.len());
        let mut canonical: HashMap<Box<str>, Box<str>> = HashMap::with_capacity(counts.len());

        for (key, hits) in counts {
            let mut entries: Vec<(Box<str>, u64)> = hits.into_iter().collect();
            // Deterministic order: most frequent first, then lexicographic.
            entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            canonical.insert(key.clone(), entries[0].0.clone());
            targets.insert(key, entries);
        }

        Mapping {
            origin,
            targets,
            canonical,
        }
    }

    pub fn from_counts_auto(counts: MapCounts) -> Self {
        Self::from_counts(counts, MappingOrigin::Auto)
    }

    pub fn expected(&self, key: &str) -> Option<&str> {
        self.canonical.get(key).map(|s| s.as_ref())
    }

    pub fn is_ambiguous(&self, key: &str) -> bool {
        self.targets.get(key).is_some_and(|t| t.len() > 1)
    }

    pub fn len(&self) -> usize {
        self.targets.len()
    }

    pub fn ambiguous_count(&self) -> usize {
        self.targets.values().filter(|t| t.len() > 1).count()
    }
}

/// Merge one map's counts into another.
pub fn merge_counts(into: &mut MapCounts, from: MapCounts) {
    for (key, hits) in from {
        let entry = into.entry(key).or_default();
        for (target, count) in hits {
            *entry.entry(target).or_insert(0) += count;
        }
    }
}

/// Split a cell into tokens. When `multi` is false the whole value is a single
/// token.
/// Split a cell into tokens. When `multi` is false the whole value is a single
/// token. `separator` may be a literal string or a regex.
pub fn split_tokens<'a>(value: &'a str, multi: bool, separator: &Separator) -> Vec<&'a str> {
    if !multi {
        return vec![value];
    }
    separator
        .split(value)
        .into_iter()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .collect()
}

/// Positional pairing between left and right tokens.
pub fn pair_tokens<'a>(
    left: &'a [&'a str],
    right: &'a [&'a str],
) -> impl Iterator<Item = (&'a str, &'a str)> {
    left.iter().copied().zip(right.iter().copied())
}

/// Load a mapping from one or more reference files.
///
/// Each file is read with its own header row. `left_columns` and
/// `right_columns` are resolved against it; when more than one column is given
/// they are combined into a composite key with `join_separator`. The rule's
/// transforms are applied to the reference values too, so that keys and targets
/// are normalized exactly like the data being validated. When `multi` is true,
/// values are split on `value_separator` and paired positionally.
pub struct FileMappingSpec<'a> {
    pub left_columns: &'a [String],
    pub right_columns: &'a [String],
    pub left_transforms: &'a [crate::transform::Transform],
    pub right_transforms: &'a [crate::transform::Transform],
    pub multi: bool,
    pub value_separator: &'a Separator,
    pub join_separator: &'a str,
    pub trim: bool,
    pub delimiter: u8,
}

fn resolve_columns(
    path: &Path,
    columns: &[String],
    headers: &[String],
    side: &str,
) -> Result<Vec<usize>, String> {
    columns
        .iter()
        .map(|column| {
            ColumnResolver::resolve(column, headers).ok_or_else(|| {
                format!(
                    "mapping file {}: {} column '{}' not found (available: {})",
                    path.display(),
                    side,
                    column,
                    headers.join(", ")
                )
            })
        })
        .collect()
}

pub fn load_from_files(files: &[PathBuf], spec: &FileMappingSpec) -> Result<Mapping, String> {
    if files.is_empty() {
        return Err("mapping_files is set but no files were provided".to_string());
    }

    let mut counts: MapCounts = HashMap::new();
    let mut left_scratch = String::new();
    let mut right_scratch = String::new();
    let mut left_key = String::new();
    let mut right_key = String::new();

    for path in files {
        let file = std::fs::File::open(path)
            .map_err(|e| format!("cannot open mapping file {}: {e}", path.display()))?;
        let mut builder = simd_csv::ReaderBuilder::with_capacity(64 * 1024);
        builder
            .delimiter(spec.delimiter)
            .has_headers(true)
            .flexible(true);
        let mut reader = builder.from_reader(file);

        let headers: Vec<String> = {
            let record = reader
                .byte_headers()
                .map_err(|e| format!("cannot read headers of {}: {e}", path.display()))?;
            record
                .iter()
                .map(|c| String::from_utf8_lossy(c).into_owned())
                .collect()
        };

        let left_idx = resolve_columns(path, spec.left_columns, &headers, "left")?;
        let right_idx = resolve_columns(path, spec.right_columns, &headers, "right")?;

        let mut record = ByteRecord::new();
        loop {
            match reader.read_byte_record(&mut record) {
                Ok(false) => break,
                Ok(true) => {}
                Err(e) => {
                    return Err(format!("error reading {}: {e}", path.display()));
                }
            }

            crate::transform::compose(
                left_idx
                    .iter()
                    .map(|&i| String::from_utf8_lossy(record.get(i).unwrap_or(b""))),
                spec.left_transforms,
                spec.join_separator,
                spec.trim,
                &mut left_scratch,
                &mut left_key,
            );
            crate::transform::compose(
                right_idx
                    .iter()
                    .map(|&i| String::from_utf8_lossy(record.get(i).unwrap_or(b""))),
                spec.right_transforms,
                spec.join_separator,
                spec.trim,
                &mut right_scratch,
                &mut right_key,
            );

            let left_tokens = split_tokens(&left_key, spec.multi, spec.value_separator);
            let right_tokens = split_tokens(&right_key, spec.multi, spec.value_separator);

            for (l, r) in pair_tokens(&left_tokens, &right_tokens) {
                *counts
                    .entry(l.into())
                    .or_default()
                    .entry(r.into())
                    .or_insert(0) += 1;
            }
        }
    }

    Ok(Mapping::from_counts(counts, MappingOrigin::File))
}
