# fast-csv / `fvalidate`

A fast, dependency-light Rust command line tool that validates relationships
between columns of a **large CSV file**, driven by a small rule DSL.

It answers questions like:

* *Do `start_date` and `end_date` hold the same instant, even though one is
  `2020-02-01` and the other `01/02/2020`?*
* *Does every `country_name` map to the expected `country_code`?*
* *Do the comma-separated `tags` match `ref_tags` regardless of order?*
* *Does the reference table (`city → code`) agree with my data, and where is
  the reference itself ambiguous?*

## Why is `xan` so fast, and what did we borrow from it?

[`xan`](https://github.com/medialab/xan) is a CSV toolkit built by médialab on
top of their [`simd-csv`](https://github.com/medialab/simd-csv) crate. Its speed
comes from a handful of deliberate choices, all of which are applied here:

| xan technique | How it is used in `fvalidate` |
| --- | --- |
| **SIMD-accelerated CSV parsing** — `simd-csv` mixes a state machine with `memchr`-style vectorised string searching, with runtime AVX2 detection. | All parsing goes through `simd_csv::Reader`, so the same parser speed applies. |
| **Zero-copy / reused buffers** — records are read into a single pre-allocated `ByteRecord` that is cleared and refilled, avoiding per-row allocation. | The hot loops keep one `ByteRecord` plus reusable `String`/`Vec` scratch buffers for the whole file. |
| **Column indexing instead of name lookup** — headers are resolved once, columns accessed by integer index. | Rules are compiled to `left_idx`/`right_idx`, and a `Slots` table maps them to a per-row cell vector. |
| **Record-aligned parallel segments** — `Seeker` finds safe byte offsets between records, then rayon workers read each segment independently. | `engine::segments_for` uses `Seeker::segments`; pass 1 (mapping extraction) and pass 2 (validation) run `rayon` over those ranges. |
| **Streaming, bounded memory** — nothing keeps the whole file in RAM. | Example rows use a bounded min-hash reservoir; only the extracted mapping (distinct inputs) is retained. |
| **Tuned release profile** — LTO, single codegen unit, `opt-level = 3`. | See `[profile.release]` in `Cargo.toml`; build with `RUSTFLAGS='-C target-cpu=native'` for AVX2. |

Cold results on a 300k-row / 17 MB file with 4 rules:

```
sequential (-j 1): 0.48 s
parallel   (-j 4): 0.14 s
```

Both produce byte-identical reports (except for row numbers, which are only
meaningful in sequential mode).

## Build

```bash
cargo build --release
# optional: make SIMD use every feature of your CPU
RUSTFLAGS='-C target-cpu=native' cargo build --release
```

The binary is `target/release/fvalidate`.

## Usage

```bash
fvalidate <input.csv> -r <rules.vl> [options]
cat data.csv | fvalidate - -r rules.vl --id-column id
```

| Option | Description |
| --- | --- |
| `-r, --rules <FILE>` | Rule DSL file (required). |
| `-d, --delimiter <BYTE>` | Field delimiter; `\t`/`tab` accepted (default `,`). |
| `--id-column <NAME>` | Column holding a unique value used to identify rows. Without it, processing falls back to a single sequential pass so row numbers stay exact. |
| `-n, --examples <N>` | Number of example rows per rule (default `10`; overridable per rule). |
| `-j, --threads <N>` | Worker threads (`0` = all cores). |
| `--format <text\|json\|html>` | Report format (default `text`). |
| `--title <TITLE>` | Title used by the HTML report. |
| `-o, --output <FILE>` | Write the report to a file. |
| `--no-fail` | Always exit `0`, even if rules fail. |

Exit code is `1` when at least one rule fails, `0` otherwise.

## The rule DSL

A rule set is a `defaults` block plus any number of `rule` blocks. `#` starts a
comment (outside quotes).

```text
defaults {
  separator       = ";"
  multi           = false
  compare         = eq
  report_limit    = 10
  mapping_separator = ","
}

rule "start and end dates agree" {
  left            = start_date
  right           = end_date
  transform_left  = date(["%Y-%m-%d", "%d/%m/%Y"], "%Y-%m-%d")
  transform_right = date(["%Y-%m-%d", "%d/%m/%Y"], "%Y-%m-%d")
  compare         = eq
}

rule "country name maps to code" {
  left           = country_name
  right          = country_code
  transform_left = trim | lower      # pipeline
  mapping        = auto              # extracted from the data
}

rule "tags agree as sets" {
  left      = tags
  right     = ref_tags
  multi     = true
  separator = ";"
  compare   = eq
}

rule "city maps to code via reference files" {
  left          = city
  right         = city_code
  mapping_files = ["examples/citymap1.csv", "examples/citymap2.csv"]
  mapping_left  = city
  mapping_right = code
}
```

### Values

A value is a bare token, a quoted string (`"..."` or `'...'`), a list
`[...]`, a call `name(args...)`, or a pipeline `a | b | c`. Values may span
several lines as long as brackets/quotes balance.

### Rule keys

| Key | Meaning |
| --- | --- |
| `left`, `right` | Columns to compare: a header name, `#index`, or a list of columns `[a, b]` forming a composite key. |
| `transform_left`, `transform_right` | Normalization pipeline (see below). With several columns it is applied to **each** component before joining. |
| `compare` | `eq` (default), `ne`, `subset`, `superset`, `intersect`, or regex `matches` / `not_matches`. Operates on token sets. |
| `multi` | Split cells into multiple values before comparing (default `false`). |
| `separator` | Token separator when `multi = true`; a plain string or `regex("...")`. |
| `join_separator` | Glue used to combine several columns of one side into a single key (default `|`). |
| `pattern` | Rule-level regex used by `compare = matches` / `not_matches`; `right` may then be omitted. |
| `mapping` | `none` (default) or `auto` (extract from the data). |
| `mapping_files` | List of reference CSVs; enables file-based mapping. |
| `mapping_left`, `mapping_right` | Column(s) inside the reference files; lists are allowed. |
| `mapping_multi`, `mapping_separator` | Multi-value handling inside reference files. |
| `report_limit` | Overrides `-n` for this rule. |

Defaults can be set for `separator`, `multi`, `compare`, `report_limit`,
`mapping_separator` and `join_separator`.

### Validating a target that depends on two input values

When the expected value is only determined by a **combination** of columns,
give `left` (or `right`) a list. The components are transformed individually,
then joined with `join_separator` into a single lookup key. This works for both
auto-extracted and reference-file mappings.

```text
# The warehouse can only be known from the pair (product, region).
rule "warehouse derived from product and region" {
  left            = [product, region]
  right           = warehouse
  transform_left  = trim | lower          # applied to product and to region
  transform_right = trim | upper
  mapping_files   = ["examples/warehouse_map.csv"]
  mapping_left    = [product, region]     # two key columns in the reference
  mapping_right   = warehouse
}
```

Run the bundled example:

```bash
fvalidate examples/orders.csv -r examples/rules_composite.vl --id-column order_id
```

It reports `product + region` as the key, flags the rows whose `warehouse`
does not match the pair, and — in the `auto` variant — reports `widget|eu` and
`gizmo|apac` as ambiguous composite keys.

### Transforms

Applied left-to-right, zero allocation after warm-up:

* `trim`, `collapse` (trim + squeeze whitespace)
* `lower`, `upper`
* `date(fmt)`, `date(fmt, out)`, `date([fmt1, fmt2], out)` — parses with the
  first matching format (`RFC 3339` as a last resort) and re-emits with `out`.
* `int`, `float`, `bool` — canonical numeric/boolean forms.
* `replace(from, to)`, `prefix(s)`, `suffix(s)`
* **Regex** (see [Regex expressions](#regex-expressions)):
  `replace(regex("p"), "r")`, `regex_replace("p", "r")`, `match(regex("p")[, group])`,
  `regex_keep(regex("p"))`.

### Regex expressions

Regex support mirrors xan: `regex("...")` compiles a pattern once (at rule
compile time) and is used by other expressions.

| Expression | xan equivalent | Behaviour |
| --- | --- | --- |
| `regex("p")` | `regex("p")` | A compiled pattern value. Bare as a transform it extracts the whole match. |
| `replace(regex("p"), "r")` | `replace(s, regex("p"), "r")` | Regex replacement with capture groups (`$1`, `${name}`). A plain string stays a literal replace. |
| `regex_replace("p", "r")` | — | Regex replacement where the pattern may be a plain string. |
| `match(regex("p")[, n])` | `match(s, regex("p"), n)` | Extracts capture group `n` (default `0`, the whole match). A missing match is a transform error (the value is left unchanged). |
| `regex_keep(regex("p"))` | — | Keeps only the concatenation of all matches (e.g. strip non-digits). |
| `separator = regex("p")` | `split(s, regex("p"))` | Splits multi-value cells on a regex. |
| `pattern` + `compare = matches` | `match(s, regex("p"))` as a filter | Validates the `left` value against a rule-level regex; `right` is optional. `not_matches` inverts it. |

```text
rule "phone digits" {
  left           = phone_raw
  right          = phone_digits
  transform_left = replace(regex("[^0-9]"), "")     # xan-style regex replace
}

rule "email shape" {
  left    = email
  pattern = "^[^@[:space:]]+@[^@[:space:]]+\\.[A-Za-z]{2,}$"
  compare = matches                                   # single-column validation
}

rule "tags (regex separator)" {
  left      = tags
  right     = ref_tags
  multi     = true
  separator = regex("\\s*[;,|]\\s*")                  # split on ; , or |
}
```

Run the bundled example:

```bash
fvalidate examples/contacts.csv -r examples/rules_regex.vl --id-column contact_id
```

### Comparison semantics

Every cell is normalized to a **set of tokens** (one token unless `multi`):

1. the `left` cell is transformed, then optionally mapped to expected target
   token(s);
2. the `right` cell is transformed into the actual token set;
3. `compare` is evaluated on the two sorted, de-duplicated sets.

Because sets are used, `multi = true` makes comparison order-independent
(`a;b == b;a`). When a side is a list of columns, the individual values are
normalized first and then joined with `join_separator` before this pipeline.

## Mapping

A mapping is the relation `left value → target value(s)`.

* **`mapping = auto`** extracts the relation from the data itself by observing
  `(left, right)` pairs. When a left value is associated with several targets it
  is **ambiguous**; the most frequent target becomes the canonical one used for
  validation, so minority rows fail and the ambiguity is reported. When `multi`
  is enabled, tokens are paired positionally (`left[i] ↔ right[i]`).
* **`mapping_files = [...]`** loads one or more reference tables (each with its
  own header row) and unions their relations. `mapping_left`/`mapping_right`
  may be lists, in which case the reference values form a composite key. The
  rule's transforms are applied to the reference values as well, so keys and
  targets are normalized exactly like the data. `Paris → PAR` in one file and
  `Paris → PAR2` in another is reported as an ambiguity. Any known target is
  accepted; unmatched left values fail and are counted as `unmapped_values`.

Pass 1 builds the auto mapping in parallel (per-segment local counters merged at
the end); pass 2 validates against it.

## Report

The report is available as human-readable text (`--format text`, default),
machine-readable JSON (`--format json`) or as a **self-contained HTML page**
(`--format html`, no external assets, light/dark aware, all values
HTML-escaped). For every rule
it contains:

* the number of rows checked, passed and failed;
* transform errors and unmapped values;
* up to `N` **distinct** matching rows and `N` **distinct** failed rows, keyed
  by the unique id column (`-n` controls `N`; distinctness and a deterministic,
  unbiased sample are provided by a bounded min-hash reservoir);
* the **complete** extracted/loaded mapping;
* for every ambiguous input (up to `N`), the distinct target values with counts,
  and example rows.

Example (truncated):

```
====================================================================
 CSV validation report
====================================================================
Rules processed : 4
Rules passed    : 0
Rules failed    : 4
Rows checked    : 8

[2] "country name maps to code"  FAILED
    left  : country_name
    right : country_code
    checked=8 passed=7 failed=1 transform_errors=0 unmapped_values=0
    matching rows (7):
      row=4 id="4" left="united states" right="US" expected="US"
      ...
    failed rows (1):
      row=5 id="5" left="france" right="US" expected="FR"
    mapping (auto): 4 distinct inputs, 1 ambiguous
      ambiguous input "france" -> "FR" (4), "US" (1)
        example: row=1 id="1" left="france" right="FR" expected="FR"
    full mapping:
      "france" -> "FR" [ambiguous]  {"FR" (4), "US" (1)}
      "germany" -> "DE"  {"DE" (1)}
      "united states" -> "US"  {"US" (1)}
      "usa" -> "US"  {"US" (1)}
```

A rule is marked `failed` when it has failing rows **or** an ambiguous mapping.

## Project layout

```
src/
  main.rs       CLI, stdin spooling, output
  dsl.rs        DSL tokenizer/parser -> Program
  rules.rs      compilation of rules against headers (column indices)
  transform.rs  value transforms
  compare.rs    set comparison operators
  mapping.rs    mapping storage, reference-file loading, ambiguity
  sampler.rs    bounded distinct min-hash reservoir
  engine.rs     segment discovery + parallel passes + orchestration
  report.rs     report model and text/JSON rendering
tests/
  integration.rs
examples/
  people.csv    small fixture
  rules.vl      example rule set (single-column keys, transforms, multi, mappings)
  citymap1.csv, citymap2.csv
  orders.csv    composite-key fixture
  warehouse_map.csv
  rules_composite.vl   two-input-value example
  contacts.csv  regex fixture
  rules_regex.vl       regex example
```

## Tests

```bash
cargo test
```

Unit tests cover the DSL, transforms and sampler; integration tests run the
binary end-to-end (including stdin and parallel-equals-sequential).
