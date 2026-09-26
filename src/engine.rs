//! Validation engine.
//!
//! Design (mirroring `xan`):
//!
//! * parsing is delegated to the SIMD-accelerated `simd-csv` reader using a
//!   single reusable `ByteRecord` (no per-row allocation);
//! * when the file is seekable it is split into record-aligned byte segments
//!   with `simd_csv::Seeker`, then each segment is read independently by a
//!   rayon worker — the same strategy xan uses for parallel commands;
//! * rules are compiled to integer column indices once, and the hot loop only
//!   does O(1) indexing and cheap `Cow` transforms;
//! * example rows are sampled with a bounded, distinct, deterministic reservoir
//!   so a gigabyte file never grows the memory footprint.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rayon::prelude::*;
use simd_csv::ByteRecord;

use crate::compare::CompareOp;
use crate::mapping::{self, MapCounts, Mapping, MappingOrigin};
use crate::pattern::Separator;
use crate::progress::Progress;
use crate::report::{
    AmbiguityReport, Example, MappingEntry, MappingReport, Report, RuleReport, TargetExample,
};
use crate::rules::{ColumnRef, CompiledPredicate, MappingPlan, Plan};
use crate::sampler::Sampler;
use crate::transform::{compose, Transform};

const BUFFER_CAPACITY: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub path: PathBuf,
    pub delimiter: u8,
    pub threads: usize,
    pub id_idx: Option<usize>,
    pub progress: Option<Arc<Progress>>,
}

/// Read the header row of the main input.
pub fn read_headers(path: &Path, delimiter: u8) -> Result<Vec<String>, String> {
    let file = File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let mut builder = simd_csv::ReaderBuilder::with_capacity(BUFFER_CAPACITY);
    builder.delimiter(delimiter).has_headers(true);
    let mut reader = builder.from_reader(file);

    let headers = reader
        .byte_headers()
        .map_err(|e| format!("cannot read headers of {}: {e}", path.display()))?;

    Ok(headers
        .iter()
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect())
}

/// Compute record-aligned byte segments for parallel processing.
pub fn segments_for(
    path: &Path,
    delimiter: u8,
    count: usize,
) -> Result<Vec<(u64, u64)>, String> {
    let file = File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;

    let mut builder = simd_csv::SeekerBuilder::new();
    builder.delimiter(delimiter).has_headers(true);

    let seeker = builder
        .from_reader(file)
        .map_err(|e| format!("cannot seek {}: {e}", path.display()))?;

    let Some(mut seeker) = seeker else {
        return Ok(Vec::new());
    };

    let ranges = seeker
        .segments(count.max(1))
        .map_err(|e| format!("cannot split {}: {e}", path.display()))?;

    Ok(ranges.into_iter().filter(|(from, to)| to > from).collect())
}

/// A `Read` adapter that reports how many bytes were consumed.
struct CountingReader<R> {
    inner: R,
    progress: Option<Arc<Progress>>,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buf)?;
        if let Some(progress) = &self.progress {
            progress.tick(read as u64);
        }
        Ok(read)
    }
}

fn open_segment(
    path: &Path,
    delimiter: u8,
    from: u64,
    to: u64,
    progress: Option<&Arc<Progress>>,
) -> Result<simd_csv::Reader<CountingReader<std::io::Take<File>>>, String> {
    let mut file = File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    file.seek(SeekFrom::Start(from))
        .map_err(|e| format!("cannot seek {}: {e}", path.display()))?;

    let limited = file.take(to.saturating_sub(from));
    let counted = CountingReader {
        inner: limited,
        progress: progress.cloned(),
    };
    let mut builder = simd_csv::ReaderBuilder::with_capacity(BUFFER_CAPACITY);
    builder
        .delimiter(delimiter)
        .has_headers(false)
        .flexible(true);

    Ok(builder.from_reader(counted))
}

// ---------------------------------------------------------------------------
// Accumulators
// ---------------------------------------------------------------------------

struct RuleAccum {
    checked: u64,
    passed: u64,
    failed: u64,
    skipped: u64,
    transform_errors: u64,
    unmapped: u64,
    pass_samples: Sampler<Example>,
    fail_samples: Sampler<Example>,
    /// Keyed by the ambiguous input value, so up to `limit` distinct inputs are
    /// reported.
    ambiguous_samples: Sampler<Example>,
}

impl RuleAccum {
    fn new(limit: usize) -> Self {
        RuleAccum {
            checked: 0,
            passed: 0,
            failed: 0,
            skipped: 0,
            transform_errors: 0,
            unmapped: 0,
            pass_samples: Sampler::new(limit),
            fail_samples: Sampler::new(limit),
            ambiguous_samples: Sampler::new(limit),
        }
    }

    fn merge(&mut self, other: RuleAccum) {
        self.checked += other.checked;
        self.passed += other.passed;
        self.failed += other.failed;
        self.skipped += other.skipped;
        self.transform_errors += other.transform_errors;
        self.unmapped += other.unmapped;
        self.pass_samples.merge(other.pass_samples);
        self.fail_samples.merge(other.fail_samples);
        self.ambiguous_samples.merge(other.ambiguous_samples);
    }
}

/// Column slots so the hot loop indexes a small per-row vector instead of
/// repeatedly hashing header names.
struct Slots {
    needed: Vec<usize>,
    /// Maps a CSV column index to its position in the extracted cell vector.
    slot_of: Vec<usize>,
    id_slot: Option<usize>,
}

impl Slots {
    fn build(plan: &Plan, id_idx: Option<usize>) -> Self {
        let mut needed: Vec<usize> = Vec::with_capacity(plan.rules.len() * 2 + 1);
        for rule in &plan.rules {
            needed.extend_from_slice(rule.left.indices());
            needed.extend_from_slice(rule.right.indices());
            if let Some(predicate) = &rule.skip {
                predicate.collect_indices(&mut needed);
            }
            if let Some(predicate) = &rule.auto_mapping_filter {
                predicate.collect_indices(&mut needed);
            }
        }
        if let Some(id) = id_idx {
            needed.push(id);
        }
        needed.sort_unstable();
        needed.dedup();

        let width = needed.last().map_or(0, |&column| column + 1);
        let mut slot_of = vec![usize::MAX; width];
        for (slot, &column) in needed.iter().enumerate() {
            slot_of[column] = slot;
        }
        let id_slot = id_idx.map(|id| slot_of[id]);

        Slots {
            needed,
            slot_of,
            id_slot,
        }
    }

    /// The extracted cell for a CSV column (empty when the row is short).
    #[inline]
    fn cell<'a>(&self, cells: &'a [String], column: usize) -> &'a str {
        cells[self.slot_of[column]].as_str()
    }

    #[inline]
    fn extract(&self, record: &ByteRecord, cells: &mut Vec<String>) {
        if cells.len() < self.needed.len() {
            cells.resize_with(self.needed.len(), String::new);
        }
        for (slot, &column) in self.needed.iter().enumerate() {
            let raw = record.get(column).unwrap_or(b"");
            let cell = &mut cells[slot];
            cell.clear();
            cell.push_str(&String::from_utf8_lossy(raw));
        }
    }
}

/// Compose one side of a rule into `out`. `Or` selects the first non-empty
/// candidate column before applying the transform pipeline.
#[allow(clippy::too_many_arguments)]
fn compose_side(
    side: &ColumnRef,
    cells: &[String],
    slots: &Slots,
    transforms: &[Transform],
    join: &str,
    trim: bool,
    scratch: &mut String,
    out: &mut String,
) -> bool {
    match side {
        ColumnRef::Columns(columns) => compose(
            columns.iter().map(|&column| slots.cell(cells, column)),
            transforms,
            join,
            trim,
            scratch,
            out,
        ),
        ColumnRef::Or(columns) => {
            let chosen = columns
                .iter()
                .map(|&column| slots.cell(cells, column))
                .find(|value| !(if trim { value.trim() } else { *value }).is_empty());
            match chosen {
                Some(value) => compose(
                    std::iter::once(value),
                    transforms,
                    join,
                    trim,
                    scratch,
                    out,
                ),
                None => {
                    out.clear();
                    true
                }
            }
        }
    }
}

/// Evaluate a compiled predicate against the extracted row cells.
#[inline]
fn predicate_holds(
    predicate: &CompiledPredicate,
    cells: &[String],
    slots: &Slots,
    trim: bool,
) -> bool {
    predicate.evaluate(&|column| slots.cell(cells, column), trim)
}

// ---------------------------------------------------------------------------
// Pass 1: build auto mappings
// ---------------------------------------------------------------------------

fn fill_tokens(buffer: &str, multi: bool, separator: &Separator, out: &mut Vec<String>) {
    out.clear();
    out.extend(
        mapping::split_tokens(buffer, multi, separator)
            .into_iter()
            .map(str::to_string),
    );
}

#[allow(clippy::too_many_arguments)]
fn build_counts_segment(
    path: &Path,
    delimiter: u8,
    rules: &[crate::rules::CompiledRule],
    auto_indices: &[usize],
    slots: &Slots,
    from: u64,
    to: u64,
    progress: Option<&Arc<Progress>>,
) -> Result<Vec<MapCounts>, String> {
    let mut result: Vec<MapCounts> = auto_indices.iter().map(|_| MapCounts::new()).collect();
    let mut reader = open_segment(path, delimiter, from, to, progress)?;
    let mut record = ByteRecord::new();
    let mut cells: Vec<String> = Vec::with_capacity(slots.needed.len());
    let mut left_buf = String::new();
    let mut right_buf = String::new();
    let mut component_buf = String::new();
    let mut left_tokens: Vec<String> = Vec::new();
    let mut right_tokens: Vec<String> = Vec::new();

    loop {
        match reader.read_byte_record(&mut record) {
            Ok(false) => break,
            Ok(true) => {}
            Err(e) => return Err(format!("error reading {}: {e}", path.display())),
        }

        slots.extract(&record, &mut cells);

        for (position, &rule_index) in auto_indices.iter().enumerate() {
            let rule = &rules[rule_index];

            // Rows skipped for validation, or excluded by the auto-mapping
            // filter, do not contribute to the extracted mapping.
            if let Some(predicate) = &rule.skip {
                if predicate_holds(predicate, &cells, slots, rule.trim) {
                    continue;
                }
            }
            if let Some(predicate) = &rule.auto_mapping_filter {
                if !predicate_holds(predicate, &cells, slots, rule.trim) {
                    continue;
                }
            }

            compose_side(
                &rule.left,
                &cells,
                slots,
                &rule.transform_left,
                &rule.join_separator,
                rule.trim,
                &mut component_buf,
                &mut left_buf,
            );
            compose_side(
                &rule.right,
                &cells,
                slots,
                &rule.transform_right,
                &rule.join_separator,
                rule.trim,
                &mut component_buf,
                &mut right_buf,
            );
            fill_tokens(&left_buf, rule.multi, &rule.separator, &mut left_tokens);
            fill_tokens(&right_buf, rule.multi, &rule.separator, &mut right_tokens);

            let counts = &mut result[position];
            for (left, right) in left_tokens.iter().zip(right_tokens.iter()) {
                *counts
                    .entry(left.as_str().into())
                    .or_default()
                    .entry(right.as_str().into())
                    .or_insert(0) += 1;
            }
        }
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Pass 2: validate
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn validate_segment(
    path: &Path,
    delimiter: u8,
    rules: &[crate::rules::CompiledRule],
    mappings: &[Option<Arc<Mapping>>],
    slots: &Slots,
    from: u64,
    to: u64,
    row_base: Option<u64>,
    progress: Option<&Arc<Progress>>,
) -> Result<Vec<RuleAccum>, String> {
    let mut accums: Vec<RuleAccum> = rules
        .iter()
        .map(|rule| RuleAccum::new(rule.report_limit))
        .collect();
    let mut reader = open_segment(path, delimiter, from, to, progress)?;
    let mut record = ByteRecord::new();
    let mut cells: Vec<String> = Vec::with_capacity(slots.needed.len());

    let mut expected_buf: Vec<String> = Vec::new();
    let mut actual_buf: Vec<String> = Vec::new();
    let mut left_buf = String::new();
    let mut right_buf = String::new();
    let mut component_buf = String::new();
    let mut left_tokens: Vec<String> = Vec::new();
    let mut right_tokens: Vec<String> = Vec::new();

    let mut local_row: u64 = 0;

    loop {
        match reader.read_byte_record(&mut record) {
            Ok(false) => break,
            Ok(true) => {}
            Err(e) => return Err(format!("error reading {}: {e}", path.display())),
        }

        local_row += 1;
        let row_number = row_base.map(|base| base + local_row - 1);
        slots.extract(&record, &mut cells);

        for (rule_index, rule) in rules.iter().enumerate() {
            let accum = &mut accums[rule_index];

            // An explicit `validation_skipped` predicate marks the row as
            // skipped. `mapping_filter` no longer skips validation; it only
            // selects which rows define the mapping.
            if let Some(predicate) = &rule.skip {
                if predicate_holds(predicate, &cells, slots, rule.trim) {
                    accum.checked += 1;
                    accum.skipped += 1;
                    continue;
                }
            }

            let left_ok = compose_side(
                &rule.left,
                &cells,
                slots,
                &rule.transform_left,
                &rule.join_separator,
                rule.trim,
                &mut component_buf,
                &mut left_buf,
            );
            let right_ok = compose_side(
                &rule.right,
                &cells,
                slots,
                &rule.transform_right,
                &rule.join_separator,
                rule.trim,
                &mut component_buf,
                &mut right_buf,
            );
            // Optional relation: when both the source and the target are empty
            // the row is skipped instead of being reported as a failure. For
            // pattern rules there is no target, so an empty value is skipped.
            if rule.allow_empty
                && left_buf.is_empty()
                && (rule.pattern.is_some() || right_buf.is_empty())
            {
                accum.checked += 1;
                accum.skipped += 1;
                continue;
            }

            // Regex rules only need the left value: the rule-level pattern
            // decides the outcome, mirroring xan's `match(value, regex(...))`.
            let id = match slots.id_slot {
                Some(slot) => {
                    let value = cells[slot].trim();
                    if value.is_empty() {
                        format!("row:{}", row_number.unwrap_or(local_row))
                    } else {
                        value.to_string()
                    }
                }
                None => format!("row:{}", row_number.unwrap_or(local_row)),
            };

            let expected_repr: Option<String>;
            let matched;

            if let Some(pattern) = &rule.pattern {
                let is_match = pattern.is_match(&left_buf);
                matched = match rule.compare {
                    CompareOp::NotMatches => left_ok && !is_match,
                    _ => left_ok && is_match,
                };
                expected_repr = Some(pattern.as_str().to_string());
            } else {
                fill_tokens(&left_buf, rule.multi, &rule.separator, &mut left_tokens);
                fill_tokens(&right_buf, rule.multi, &rule.separator, &mut right_tokens);

                expected_buf.clear();
                if let Some(mapping) = &mappings[rule_index] {
                    for token in &left_tokens {
                        match mapping.expected(token) {
                            Some(target) => expected_buf.push(target.to_string()),
                            None => {
                                accum.unmapped += 1;
                                // `\u{0}` cannot appear in a real target, so
                                // this sentinel can never accidentally match.
                                expected_buf.push(format!("\u{0}{token}"));
                            }
                        }

                        if mapping.is_ambiguous(token) && !accum.ambiguous_samples.is_disabled() {
                            accum.ambiguous_samples.offer(
                                token,
                                Example {
                                    id: id.clone(),
                                    row: row_number,
                                    left: token.clone(),
                                    right: right_buf.clone(),
                                    expected: mapping.expected(token).map(str::to_string),
                                },
                            );
                        }
                    }
                } else {
                    expected_buf.extend(left_tokens.iter().cloned());
                }

                actual_buf.clear();
                actual_buf.extend(right_tokens.iter().cloned());

                expected_buf.sort_unstable();
                expected_buf.dedup();
                actual_buf.sort_unstable();
                actual_buf.dedup();

                matched = left_ok && right_ok && rule.compare.evaluate(&expected_buf, &actual_buf);
                expected_repr = mapping_expected(mappings[rule_index].as_deref(), &expected_buf);
            }

            accum.checked += 1;
            if !left_ok || !right_ok {
                accum.transform_errors += 1;
            }

            if matched {
                accum.passed += 1;
                if !accum.pass_samples.is_disabled() {
                    accum.pass_samples.offer(
                        &id,
                        Example {
                            id: id.clone(),
                            row: row_number,
                            left: left_buf.clone(),
                            right: right_buf.clone(),
                            expected: expected_repr.clone(),
                        },
                    );
                }
            } else {
                accum.failed += 1;
                if !accum.fail_samples.is_disabled() {
                    accum.fail_samples.offer(
                        &id,
                        Example {
                            id: id.clone(),
                            row: row_number,
                            left: left_buf.clone(),
                            right: right_buf.clone(),
                            expected: expected_repr.clone(),
                        },
                    );
                }
            }
        }
    }

    Ok(accums)
}

fn mapping_expected(mapping: Option<&Mapping>, expected: &[String]) -> Option<String> {
    mapping.map(|_| {
        expected
            .iter()
            .map(|value| match value.strip_prefix('\u{0}') {
                Some(unmapped) => format!("<unmapped:{unmapped}>"),
                None => value.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ")
    })
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

pub fn run(plan: &Plan, config: &EngineConfig) -> Result<Report, String> {
    let id_idx = config.id_idx;
    // Without a unique id column we cannot number rows across parallel
    // segments, so we fall back to a single sequential segment.
    let thread_count = if id_idx.is_some() {
        config.threads.max(1)
    } else {
        1
    };

    let segments = segments_for(&config.path, config.delimiter, thread_count)?;
    let row_base = if segments.len() <= 1 { Some(1) } else { None };

    let slots = Slots::build(plan, id_idx);

    // Resolve file-based mappings up-front, reusing parsed reference tables
    // and equivalent mappings across rules.
    let mut cache = mapping::MappingCache::new();
    let mut mappings: Vec<Option<Arc<Mapping>>> = Vec::with_capacity(plan.rules.len());
    for rule in &plan.rules {
        let mapping = match &rule.mapping {
            MappingPlan::Files {
                files,
                left,
                right,
                multi,
                separator,
                filter,
            } => cache
                .load(
                    files,
                    &mapping::FileMappingSpec {
                        left_columns: left,
                        right_columns: right,
                        left_transforms: &rule.transform_left,
                        right_transforms: &rule.transform_right,
                        multi: *multi,
                        value_separator: separator,
                        join_separator: &rule.join_separator,
                        trim: rule.trim,
                        delimiter: config.delimiter,
                        filter: filter.as_ref(),
                    },
                )
                .map(Some)?,
            _ => None,
        };
        mappings.push(mapping);
    }

    let auto_indices: Vec<usize> = plan
        .rules
        .iter()
        .enumerate()
        .filter(|(_, rule)| matches!(rule.mapping, MappingPlan::Auto))
        .map(|(index, _)| index)
        .collect();

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(thread_count)
        .build()
        .map_err(|e| format!("cannot create thread pool: {e}"))?;

    // Pass 1 (auto mapping) and pass 2 (validation) each read the whole file.
    let progress = config.progress.as_ref();
    if let Some(progress) = progress {
        if progress.enabled() {
            let passes: u64 = if auto_indices.is_empty() { 1 } else { 2 };
            let file_size = std::fs::metadata(&config.path).map(|m| m.len()).unwrap_or(0);
            progress.set_total(file_size * passes);
        }
    }

    // Pass 1: extract auto mappings from the data.
    if !auto_indices.is_empty() {
        let per_segment: Vec<Vec<MapCounts>> = pool.install(|| {
            segments
                .par_iter()
                .map(|&(from, to)| {
                    build_counts_segment(
                        &config.path,
                        config.delimiter,
                        &plan.rules,
                        &auto_indices,
                        &slots,
                        from,
                        to,
                        progress,
                    )
                })
                .collect::<Result<Vec<_>, String>>()
        })?;

        let mut totals: Vec<MapCounts> = (0..auto_indices.len()).map(|_| MapCounts::new()).collect();
        for segment in per_segment {
            for (position, counts) in segment.into_iter().enumerate() {
                mapping::merge_counts(&mut totals[position], counts);
            }
        }
        for (position, &rule_index) in auto_indices.iter().enumerate() {
            mappings[rule_index] = Some(Arc::new(Mapping::from_counts_auto(
                std::mem::take(&mut totals[position]),
            )));
        }
    }

    // Pass 2: validate every rule.
    let per_segment: Vec<Vec<RuleAccum>> = pool.install(|| {
        segments
            .par_iter()
            .map(|&(from, to)| {
                validate_segment(
                    &config.path,
                    config.delimiter,
                    &plan.rules,
                    &mappings,
                    &slots,
                    from,
                    to,
                    row_base,
                    progress,
                )
            })
            .collect::<Result<Vec<_>, String>>()
    })?;

    let mut accums: Vec<RuleAccum> = plan
        .rules
        .iter()
        .map(|rule| RuleAccum::new(rule.report_limit))
        .collect();
    for segment in per_segment {
        for (index, accum) in segment.into_iter().enumerate() {
            accums[index].merge(accum);
        }
    }

    if let Some(progress) = progress {
        progress.finish();
    }

    Ok(build_report(plan, accums, mappings))
}

fn build_report(
    plan: &Plan,
    accums: Vec<RuleAccum>,
    mappings: Vec<Option<Arc<Mapping>>>,
) -> Report {
    let mut rules = Vec::with_capacity(plan.rules.len());
    let mut rows_checked = 0u64;

    for (index, rule) in plan.rules.iter().enumerate() {
        let accum = &accums[index];
        rows_checked = rows_checked.max(accum.checked);

        let mapping_report = mappings[index].as_ref().map(|mapping| {
            build_mapping_report(mapping, &accum.ambiguous_samples, rule.report_limit)
        });

        let ambiguous_inputs = mapping_report
            .as_ref()
            .map_or(0, |report| report.ambiguous_inputs);
        let status = if accum.failed == 0 && ambiguous_inputs == 0 {
            "passed"
        } else {
            "failed"
        };

        rules.push(RuleReport {
            name: rule.name.clone(),
            left: rule.left_name.clone(),
            right: rule.right_name.clone(),
            status: status.to_string(),
            rows_checked: accum.checked,
            rows_passed: accum.passed,
            rows_failed: accum.failed,
            rows_skipped: accum.skipped,
            transform_errors: accum.transform_errors,
            unmapped_values: accum.unmapped,
            pass_examples: accum.pass_samples.payloads(),
            fail_examples: accum.fail_samples.payloads(),
            mapping: mapping_report,
        });
    }

    let rules_passed = rules.iter().filter(|r| r.passed()).count();

    Report {
        rules_total: rules.len(),
        rules_passed,
        rules_failed: rules.len() - rules_passed,
        rows_checked,
        rules,
    }
}

fn build_mapping_report(
    mapping: &Mapping,
    ambiguous_samples: &Sampler<Example>,
    limit: usize,
) -> MappingReport {
    let mut entries: Vec<MappingEntry> = mapping
        .targets
        .iter()
        .map(|(input, targets)| MappingEntry {
            input: input.to_string(),
            canonical: mapping.expected(input).unwrap_or("").to_string(),
            ambiguous: targets.len() > 1,
            targets: targets
                .iter()
                .map(|(value, count)| TargetExample {
                    value: value.to_string(),
                    count: *count,
                })
                .collect(),
        })
        .collect();
    entries.sort_by(|a, b| a.input.cmp(&b.input));

    // Collect one example row per ambiguous input.
    let examples_by_input: HashMap<String, Example> = ambiguous_samples
        .payloads()
        .into_iter()
        .map(|example| (example.left.clone(), example))
        .collect();

    let mut ambiguous_inputs: Vec<&MappingEntry> =
        entries.iter().filter(|entry| entry.ambiguous).collect();
    // Report the worst offenders first (most targets, then most observations).
    ambiguous_inputs.sort_by(|a, b| {
        b.targets
            .len()
            .cmp(&a.targets.len())
            .then_with(|| total_count(b).cmp(&total_count(a)))
            .then_with(|| a.input.cmp(&b.input))
    });

    let ambiguities: Vec<AmbiguityReport> = ambiguous_inputs
        .into_iter()
        .take(limit)
        .map(|entry| AmbiguityReport {
            input: entry.input.clone(),
            targets: entry.targets.iter().take(limit).cloned().collect(),
            examples: examples_by_input
                .get(&entry.input)
                .cloned()
                .into_iter()
                .collect(),
        })
        .collect();

    MappingReport {
        source: match mapping.origin {
            MappingOrigin::Auto => "auto".to_string(),
            MappingOrigin::File => "file".to_string(),
        },
        distinct_inputs: mapping.len(),
        ambiguous_inputs: mapping.ambiguous_count(),
        entries,
        ambiguities,
    }
}

fn total_count(entry: &MappingEntry) -> u64 {
    entry.targets.iter().map(|t| t.count).sum()
}
