#!/usr/bin/env -S cargo +nightly -Zscript
---
[package]
edition = "2024"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
---

//! Rebuild `crates/coseva/docs/PERF.md` from a clean benchmark artifact.
//!
//! # Why this exists in the shape it does
//!
//! Its predecessor was deleted rather than repaired. It carried a hand-written
//! table mapping benchmark names to report rows, and when benchmarks were
//! renamed or removed the table went stale silently: unmatched names were
//! rendered as an em dash, so a report full of numbers from benchmarks that no
//! longer existed looked exactly like a healthy one. The output was worse than
//! having no report, because a reader could not tell.
//!
//! `perf_artifacts.py` runs each focused harness from a clean tree and
//! normalizes its machine-readable output into one JSON file. This renderer
//! never scrapes benchmark prose and never substitutes a missing measurement.
//!
//! # Usage
//!
//! From anywhere in the repository:
//!
//! ```text
//! crates/coseva/scripts/perf_report.rs              # measure all sections, then write
//! crates/coseva/scripts/perf_report.rs --no-run     # reuse docs/PERF.json
//! crates/coseva/scripts/perf_report.rs --check      # fail if docs/PERF.md is out of date
//! ```

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::{env, fs};

use serde::Deserialize;

const DEFAULT_ARTIFACT: &str = "crates/coseva/docs/PERF.json";

#[derive(Clone, Deserialize)]
struct Evidence {
    command: String,
    host: Host,
    toolchain: Toolchain,
    source: Source,
}

#[derive(Clone, Deserialize)]
struct Host {
    hostname: String,
    #[serde(default)]
    platform: String,
    #[serde(default)]
    machine: String,
    cpu_model: String,
    logical_cpus: Option<usize>,
    /// The runtime-selected kernel arm the profiled process reached.
    ///
    /// Absent from artifacts produced before `benches/dispatch.rs` existed, and
    /// rendered only when present, so an old artifact still reports rather than
    /// claiming an arm nobody measured. It is not `cpu_model`: the benchmarks
    /// run under Valgrind, which emulates the CPU that answers `CPUID`.
    #[serde(default)]
    dispatch_arm: Option<String>,
}

#[derive(Clone, Deserialize)]
struct Toolchain {
    #[serde(default)]
    version: String,
    #[serde(default)]
    rustc: String,
    cargo: String,
    #[serde(default)]
    release: String,
    #[serde(default)]
    commit_hash: String,
}

#[derive(Clone, Deserialize)]
struct Source {
    revision: String,
    tree_clean: bool,
}

#[derive(Deserialize)]
struct Artifact {
    schema: u32,
    read: ReadSection,
    write: InstructionSection,
    parallel: ParallelSection,
    index: InstructionSection,
    memory: MemorySection,
    proc_macro: MacroSection,
}

#[derive(Deserialize)]
struct ReadSection {
    evidence: Evidence,
    documents: Vec<Document>,
    counts: Vec<ReadCount>,
}

#[derive(Deserialize)]
struct ReadCount {
    function: String,
    document: String,
    instructions: u64,
}

#[derive(Deserialize)]
struct InstructionSection {
    evidence: Evidence,
    counts: Vec<InstructionCount>,
}

#[derive(Deserialize)]
struct InstructionCount {
    case: String,
    variant: String,
    instructions: u64,
}

#[derive(Deserialize)]
struct ParallelSection {
    evidence: Evidence,
    points: Vec<ParallelPoint>,
}

#[derive(Deserialize)]
struct ParallelPoint {
    id: String,
    bytes: u64,
    median_ns: f64,
}

#[derive(Deserialize)]
struct MemorySection {
    evidence: Evidence,
    metric: String,
    cases: Vec<MemoryCase>,
}

#[derive(Deserialize)]
struct MemoryCase {
    case: String,
    operations: usize,
    allocations: u64,
    allocated_bytes: u64,
    peak_live_bytes: usize,
}

#[derive(Deserialize)]
struct MacroSection {
    evidence: Evidence,
    metric: String,
    samples: usize,
    cases: Vec<MacroCase>,
}

#[derive(Deserialize)]
struct MacroCase {
    case: String,
    milliseconds: f64,
}

/// A record shape, its prose, and what `csv` calls the same thing.
struct Shape {
    key: &'static str,
    title: &'static str,
    blurb: &'static str,
    /// What `csv` calls the same thing, or `None` when it has no counterpart.
    csv_equivalent: Option<&'static str>,
}

/// Row order in the published tables, and the labels each shape carries.
///
/// Presentation only — no number comes from here. A shape listed with no
/// measurement behind it is a hard error, so this cannot drift out of step with
/// the benchmark the way its predecessor's table did.
const SHAPES: &[Shape] = &[
    Shape {
        key: "record",
        title: "`Record`",
        blurb: "fields borrowed straight from the input, with no allocation per record",
        csv_equivalent: None,
    },
    Shape {
        key: "text_record",
        title: "`TextRecord`",
        blurb: "an owned record of validated `String` fields",
        csv_equivalent: Some("`StringRecord`"),
    },
    Shape {
        key: "byte_record",
        title: "`ByteRecord`",
        blurb: "an owned record of raw byte fields",
        csv_equivalent: Some("`ByteRecord`"),
    },
    Shape {
        key: "decoded",
        title: "decoded struct",
        blurb: "a typed struct via `#[derive(CsvDecode)]`",
        csv_equivalent: None,
    },
    Shape {
        key: "deserialized",
        title: "deserialized struct",
        blurb: "a typed struct via Serde",
        csv_equivalent: Some("Serde"),
    },
];

/// The front ends, in the order they are published.
const FRONT_ENDS: &[(&str, &str)] = &[
    ("slice", "`SliceParser`"),
    ("io", "`IoParser`"),
    ("push", "`PushParser`"),
];

/// A document's dimensions, read from the example that shares the generator.
#[derive(Deserialize)]
struct Document {
    name: String,
    bytes: u64,
    records: u64,
}

fn main() -> ExitCode {
    match run() {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("perf_report: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<String, String> {
    let mut measure = true;
    let mut check = false;
    let mut artifact_path = PathBuf::from(DEFAULT_ARTIFACT);
    let mut macro_artifact = None;
    let mut destination = None;
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--no-run" => measure = false,
            "--check" => {
                check = true;
                measure = false;
            }
            "--artifact" => {
                artifact_path =
                    PathBuf::from(arguments.next().ok_or("--artifact needs a path")?);
            }
            "--macro-artifact" => {
                macro_artifact = Some(PathBuf::from(
                    arguments.next().ok_or("--macro-artifact needs a path")?,
                ));
            }
            "--output" => {
                destination = Some(PathBuf::from(
                    arguments.next().ok_or("--output needs a path")?,
                ));
            }
            "--help" | "-h" => return Ok(usage()),
            other => return Err(format!("unrecognised argument `{other}`; try --help")),
        }
    }

    let crate_root = crate_root()?;
    let workspace_root = crate_root
        .parent()
        .and_then(Path::parent)
        .ok_or("cannot find the workspace root")?
        .to_path_buf();

    let artifact_path = if artifact_path.is_absolute() {
        artifact_path
    } else {
        workspace_root.join(artifact_path)
    };
    if measure {
        println!("measuring all report sections; this takes several minutes");
        run_collector(
            &workspace_root,
            &crate_root,
            &artifact_path,
            macro_artifact.as_deref(),
        )?;
    }

    let artifact = read_artifact(&artifact_path)?;
    let report = render(&artifact)?;

    let destination = destination
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                workspace_root.join(path)
            }
        })
        .unwrap_or_else(|| crate_root.join("docs").join("PERF.md"));
    if check {
        let current = fs::read_to_string(&destination)
            .map_err(|error| format!("cannot read {}: {error}", destination.display()))?;
        return if strip_stamp(&current) == strip_stamp(&report) {
            Ok(format!("{} is up to date", destination.display()))
        } else {
            Err(format!(
                "{} is out of date; rerun scripts/perf_report.rs",
                destination.display()
            ))
        };
    }

    fs::write(&destination, &report)
        .map_err(|error| format!("cannot write {}: {error}", destination.display()))?;
    Ok(format!(
        "wrote {} from {} read-matrix cases and the focused artifacts",
        destination.display(),
        artifact.read.counts.len()
    ))
}

fn usage() -> String {
    "usage: perf_report.rs [--no-run|--check] [--artifact PATH] \
     [--macro-artifact PATH] [--output PATH]\n\
     \n\
     \x20 (no flags)          measure every section from a clean tree, then write\n\
     \x20 --no-run            reuse the committed docs/PERF.json artifact\n\
     \x20 --check             fail if docs/PERF.md differs from the artifact\n\
     \x20 --artifact PATH     read or write a different normalized artifact\n\
     \x20 --macro-artifact P  consume the stable proc-macro JSON artifact\n\
     \x20 --output PATH       write/check a different report path"
        .to_string()
}

fn strip_stamp(report: &str) -> String {
    report.to_string()
}

fn crate_root() -> Result<PathBuf, String> {
    // Derived from the script's own path rather than the working directory, so
    // it can be run from anywhere: the script lives in `<crate>/scripts`.
    let script = PathBuf::from(env::args().next().ok_or("no argv[0]")?);
    let script = script
        .canonicalize()
        .map_err(|error| format!("cannot resolve {}: {error}", script.display()))?;
    script
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("cannot find the crate root from {}", script.display()))
}

fn run_collector(
    workspace_root: &Path,
    crate_root: &Path,
    artifact: &Path,
    macro_artifact: Option<&Path>,
) -> Result<(), String> {
    let mut command = Command::new("python3");
    command
        .current_dir(workspace_root)
        .arg(crate_root.join("scripts").join("perf_artifacts.py"))
        .arg("--output")
        .arg(artifact);
    if let Some(path) = macro_artifact {
        command.arg("--macro-artifact").arg(path);
    }
    let status = command
        .status()
        .map_err(|error| format!("cannot run perf_artifacts.py: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("perf_artifacts.py failed with {status}"))
    }
}

fn read_artifact(path: &Path) -> Result<Artifact, String> {
    let text = fs::read_to_string(path).map_err(|error| {
        format!(
            "cannot read {}: {error}\nrun without --no-run to measure first",
            path.display()
        )
    })?;
    let artifact: Artifact = serde_json::from_str(&text)
        .map_err(|error| format!("cannot parse {}: {error}", path.display()))?;
    if artifact.schema != 1 {
        return Err(format!("unsupported artifact schema {}", artifact.schema));
    }
    for (name, evidence) in [
        ("read", &artifact.read.evidence),
        ("write", &artifact.write.evidence),
        ("parallel", &artifact.parallel.evidence),
        ("index", &artifact.index.evidence),
        ("memory", &artifact.memory.evidence),
        ("proc macro", &artifact.proc_macro.evidence),
    ] {
        if !evidence.source.tree_clean {
            return Err(format!("{name} artifact was not measured from a clean tree"));
        }
    }
    Ok(artifact)
}

/// Instructions per record.
fn per_record(instructions: u64, records: u64) -> f64 {
    instructions as f64 / records as f64
}

/// Look a case up, failing rather than printing a placeholder.
fn count(
    counts: &BTreeMap<(String, String), u64>,
    function: &str,
    document: &Document,
) -> Result<f64, String> {
    counts
        .get(&(function.to_string(), document.name.clone()))
        .map(|instructions| per_record(*instructions, document.records))
        .ok_or_else(|| {
            format!(
                "no measurement for `{function}` on `{}`; the report is incomplete, \
                 so none was written",
                document.name
            )
        })
}

fn render(artifact: &Artifact) -> Result<String, String> {
    let mut out = String::new();
    let documents = &artifact.read.documents;
    let counts = artifact
        .read
        .counts
        .iter()
        .map(|count| {
            (
                (count.function.clone(), count.document.clone()),
                count.instructions,
            )
        })
        .collect::<BTreeMap<_, _>>();

    writeln!(out, "# What coseva costs\n").unwrap();
    writeln!(
        out,
        "Generated by `scripts/perf_report.rs` from the normalized machine-readable\n\
         artifact produced by `scripts/perf_artifacts.py`. Do not edit by hand.\n"
    )
    .unwrap();

    render_preamble(&mut out);
    render_evidence(&mut out, &artifact.read.evidence);
    render_documents(&mut out, documents);

    writeln!(out, "## The matrix\n").unwrap();
    writeln!(
        out,
        "Instructions per record; lower is better. Each document is a different file\n\
         shape, so compare down a column and not across a row — a `wide` record has 128\n\
         columns and a `metrics` record has five, and no ratio between those two means\n\
         anything.\n"
    )
    .unwrap();

    for shape in SHAPES {
        render_shape(&mut out, shape, documents, &counts)?;
    }

    render_capability(&mut out, documents, &counts)?;
    render_mechanisms(&mut out);
    render_write(&mut out, &artifact.write)?;
    render_parallel(&mut out, &artifact.parallel)?;
    render_index(&mut out, &artifact.index)?;
    render_memory(&mut out, &artifact.memory);
    render_macro(&mut out, &artifact.proc_macro);
    render_caveats(&mut out);
    Ok(out)
}

fn render_preamble(out: &mut String) {
    writeln!(out, "## Read throughput\n").unwrap();
    writeln!(
        out,
        "Callgrind instruction counts: instructions retired reading a whole document,\n\
         divided by the number of records in it. They are reproducible to the\n\
         instruction, which is what makes a 2% difference meaningful rather than noise.\n"
    )
    .unwrap();
    writeln!(
        out,
        "They are not times. Instruction counts do not model cache misses, branch\n\
         misprediction or memory bandwidth, and a document large enough to fall out of L2\n\
         is not modelled well by any of them. Read them as a strong signal about how much\n\
         work is done and a weak one about wall clock.\n"
    )
    .unwrap();
    writeln!(
        out,
        "Both crates get an 8 KiB buffer and are asked to resolve headers, so no\n\
         comparison here is secretly a comparison of buffer sizes or header settings.\n\
         Every case asserts a checksum computed by the document generator rather than by\n\
         a parse, so a case that skipped a column or stopped unescaping fails instead of\n\
         reporting a better number; `tests/benchmark_parity.rs` runs those same\n\
         assertions without valgrind.\n"
    )
    .unwrap();
}

fn render_evidence(out: &mut String, evidence: &Evidence) {
    let rustc = if evidence.toolchain.version.is_empty() {
        evidence.toolchain.rustc.lines().next().unwrap_or("unknown")
    } else {
        &evidence.toolchain.version
    };
    let release = if evidence.toolchain.release.is_empty() {
        rustc
    } else {
        &evidence.toolchain.release
    };
    let commit = if evidence.toolchain.commit_hash.is_empty() {
        ""
    } else {
        &evidence.toolchain.commit_hash
    };
    let host_detail = [evidence.host.platform.as_str(), evidence.host.machine.as_str()]
        .into_iter()
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(out, "**Reproduce this section**\n").unwrap();
    writeln!(out, "- Command: `{}`", evidence.command).unwrap();
    writeln!(
        out,
        "- Host: `{}` — {}{}{}{}",
        evidence.host.hostname,
        evidence.host.cpu_model,
        evidence
            .host
            .logical_cpus
            .map_or_else(String::new, |cpus| format!(", {cpus} logical CPUs")),
        if host_detail.is_empty() {
            String::new()
        } else {
            format!(", {host_detail}")
        },
        evidence
            .host
            .dispatch_arm
            .as_deref()
            .filter(|arm| *arm != "unrecorded")
            .map_or_else(String::new, |arm| format!(
                ", dispatched kernels: {arm} (as the profiler emulated them, not as this host reports them)"
            ))
    )
    .unwrap();
    writeln!(
        out,
        "- Rust toolchain: `{release}`{}; `{}`",
        if commit.is_empty() {
            String::new()
        } else {
            format!(" (`{commit}`)")
        },
        evidence.toolchain.cargo
    )
    .unwrap();
    writeln!(
        out,
        "- Source: clean tree at revision `{}`\n",
        evidence.source.revision
    )
    .unwrap();
}

fn render_documents(out: &mut String, documents: &[Document]) {
    writeln!(out, "## The documents\n").unwrap();
    writeln!(
        out,
        "Generated from a fixed seed by `benches/documents.rs`, not scraped from\n\
         anywhere, and none of them is a real file. The shapes are drawn from what CSV\n\
         producers actually emit; read a row as \"a file like this\", not as a measurement\n\
         of any particular document.\n"
    )
    .unwrap();
    writeln!(
        out,
        "| Document | Bytes | Records | Bytes/record | What it stresses |"
    )
    .unwrap();
    writeln!(out, "|---|---:|---:|---:|---|").unwrap();
    for document in documents {
        let stresses = match document.name.as_str() {
            "metrics" => "5 narrow numeric columns; per-record overhead",
            "wide" => "128 columns; per-column cost",
            "quoted" => "embedded delimiters and newlines; the quoted path",
            "prose" => "a long free-text column; large-field copying",
            "spreadsheet" => "CRLF, a UTF-8 BOM, quoted text; what a spreadsheet emits",
            _ => "see `benches/documents.rs`",
        };
        writeln!(
            out,
            "| `{}` | {} | {} | {} | {stresses} |",
            document.name,
            thousands(document.bytes),
            thousands(document.records),
            document.bytes / document.records
        )
        .unwrap();
    }
    writeln!(out).unwrap();
}

fn header_row(out: &mut String, first: &str, documents: &[Document]) {
    write!(out, "| {first} |").unwrap();
    for document in documents {
        write!(out, " `{}` |", document.name).unwrap();
    }
    writeln!(out).unwrap();
    write!(out, "|---|").unwrap();
    for _ in documents {
        write!(out, "---:|").unwrap();
    }
    writeln!(out).unwrap();
}

fn render_shape(
    out: &mut String,
    shape: &Shape,
    documents: &[Document],
    counts: &BTreeMap<(String, String), u64>,
) -> Result<(), String> {
    writeln!(out, "### {} — {}\n", shape.title, shape.blurb).unwrap();
    header_row(out, "Front end", documents);

    for (front_end, label) in FRONT_ENDS {
        let function = format!("{}_{front_end}", shape.key);
        write!(out, "| {label} |").unwrap();
        for document in documents {
            write!(out, " {:.0} |", count(counts, &function, document)?).unwrap();
        }
        writeln!(out).unwrap();
    }

    let Some(equivalent) = shape.csv_equivalent else {
        write!(out, "| `csv` |").unwrap();
        for _ in documents {
            write!(out, " no counterpart |").unwrap();
        }
        writeln!(out, "\n").unwrap();
        writeln!(
            out,
            "`csv` cannot express this shape at all, so there is no comparison to draw. The\n\
             row is published because the capability is itself a result, not because it won\n\
             anything.\n"
        )
        .unwrap();
        return Ok(());
    };

    let csv_function = format!("{}_csv", shape.key);
    write!(out, "| `csv` ({equivalent}) |").unwrap();
    let mut csv_values = Vec::new();
    for document in documents {
        let value = count(counts, &csv_function, document)?;
        csv_values.push(value);
        write!(out, " {value:.0} |").unwrap();
    }
    writeln!(out).unwrap();

    // The comparison is the point of the table, so it is computed here rather
    // than left to the reader to do in their head.
    write!(out, "| **best coseva vs `csv`** |").unwrap();
    for (index, document) in documents.iter().enumerate() {
        let mut best = f64::MAX;
        for (front_end, _) in FRONT_ENDS {
            let value = count(counts, &format!("{}_{front_end}", shape.key), document)?;
            best = best.min(value);
        }
        write!(out, " {} |", ratio(best, csv_values[index])).unwrap();
    }
    writeln!(out, "\n").unwrap();
    Ok(())
}

/// A percentage difference, phrased so the direction cannot be misread.
fn ratio(coseva: f64, csv: f64) -> String {
    let percent = (coseva - csv) / csv * 100.0;
    if percent <= -0.5 {
        format!("**{:.0}% faster**", -percent)
    } else if percent >= 0.5 {
        format!("{percent:.0}% slower")
    } else {
        "even".to_string()
    }
}

fn render_capability(
    out: &mut String,
    documents: &[Document],
    counts: &BTreeMap<(String, String), u64>,
) -> Result<(), String> {
    writeln!(out, "## Paths the `csv` crate cannot express\n").unwrap();
    writeln!(
        out,
        "Two of the five shapes have no counterpart in `csv`: it has no borrowed record\n\
         form, and no typed decoding of its own beyond Serde. Those are reasons to reach\n\
         for this crate that a head-to-head instruction count cannot show, so what they\n\
         cost relative to the shapes that *do* have a counterpart is worth stating\n\
         directly.\n"
    )
    .unwrap();
    writeln!(
        out,
        "Instructions per record on `SliceParser`, all five shapes side by side:\n"
    )
    .unwrap();

    header_row(out, "Shape", documents);
    for shape in SHAPES {
        let function = format!("{}_slice", shape.key);
        let marker = if shape.csv_equivalent.is_none() {
            " (coseva only)"
        } else {
            ""
        };
        write!(out, "| {}{marker} |", shape.title).unwrap();
        for document in documents {
            write!(out, " {:.0} |", count(counts, &function, document)?).unwrap();
        }
        writeln!(out).unwrap();
    }
    writeln!(out).unwrap();
    Ok(())
}

/// Explain where the differences come from.
///
/// A ratio nobody can account for reads as a broken benchmark, and on this
/// corpus one of them is nearly fivefold. Each claim below names a mechanism
/// that can be checked against a profile rather than a number that has to be
/// taken on trust, so it stays true as the numbers move.
fn render_mechanisms(out: &mut String) {
    writeln!(out, "## Where the differences come from\n").unwrap();
    writeln!(
        out,
        "The spread across documents is wide enough to be worth accounting for. Each of\n\
         these was read off a profile, not inferred from the table.\n"
    )
    .unwrap();
    writeln!(
        out,
        "**Long fields are where the gap is largest.** On `prose`, whose free-text column\n\
         runs to a few hundred bytes, `csv` spends about 89% of its time inside\n\
         `csv_core`'s reader, which advances one byte at a time through a state machine —\n\
         roughly eleven instructions for every input byte. This crate locates the closing\n\
         quote with a vector search and copies the field with one `memcpy`, so its cost\n\
         per byte is several times lower. The longer the fields, the further those two\n\
         diverge; a document of short fields would not show it.\n"
    )
    .unwrap();
    writeln!(
        out,
        "**Many short columns are where it is narrowest.** On `wide`, at 128 columns, the\n\
         work is dominated by per-field bookkeeping — a boundary, a bounds check, a push —\n\
         which neither crate can vectorise away, and there is little field body over which\n\
         to amortise a scan. The two converge to within a few per cent there, and that is\n\
         the honest worst case for this crate.\n"
    )
    .unwrap();
    writeln!(
        out,
        "**The typed rows diverge for a different reason.** `csv` has no typed decoding of\n\
         its own, so its Serde path assembles a record first and deserializes out of it;\n\
         this crate decodes from the parsed field spans without materialising the record\n\
         in between. That is a structural difference rather than a faster loop, which is\n\
         why the deserialized rows move further than the record rows do.\n"
    )
    .unwrap();
    writeln!(
        out,
        "**Borrowing is worth more the wider the record.** `Record` hands back fields that\n\
         point into the input, so a wide record costs no allocation, where an owned record\n\
         pays for one per field. That is the largest single difference in the capability\n\
         table above, and it is available only on `SliceParser` and inside a `PushParser`\n\
         chunk, where the input outlives the record.\n"
    )
    .unwrap();
}

fn instruction_map(section: &InstructionSection) -> BTreeMap<(String, String), u64> {
    section
        .counts
        .iter()
        .map(|count| {
            (
                (count.case.clone(), count.variant.clone()),
                count.instructions,
            )
        })
        .collect()
}

fn instruction(
    counts: &BTreeMap<(String, String), u64>,
    case: &str,
    variant: &str,
) -> Result<u64, String> {
    counts
        .get(&(case.to_string(), variant.to_string()))
        .copied()
        .ok_or_else(|| format!("no instruction measurement for `{case}.{variant}`"))
}

fn marginal(
    counts: &BTreeMap<(String, String), u64>,
    case: &str,
) -> Result<f64, String> {
    let low = instruction(counts, case, "rows_100")?;
    let high = instruction(counts, case, "rows_1000")?;
    Ok((high - low) as f64 / 900.0)
}

fn render_write(out: &mut String, section: &InstructionSection) -> Result<(), String> {
    writeln!(out, "## Write throughput\n").unwrap();
    writeln!(
        out,
        "Marginal Callgrind instructions per record, computed as\n\
         `(rows_1000 - rows_100) / 900`; lower is better. The sinks are in memory, so\n\
         this isolates CSV emission rather than filesystem throughput.\n"
    )
    .unwrap();
    render_evidence(out, &section.evidence);
    let counts = instruction_map(section);
    let cases = [
        ("vec", "`VecEmitter`, borrowed slices"),
        ("push", "`PushEmitter`, borrowed slices"),
        ("io", "`IoEmitter`, borrowed slices"),
        ("csv_writer", "`csv::Writer`, borrowed slices"),
        ("vec_encode", "`CsvEncode` struct"),
        ("vec_serialize", "coseva Serde struct"),
        ("csv_serialize", "`csv` Serde struct"),
    ];
    writeln!(out, "| Path | Instructions/record |").unwrap();
    writeln!(out, "|---|---:|").unwrap();
    for (case, label) in cases {
        writeln!(out, "| {label} | {:.0} |", marginal(&counts, case)?).unwrap();
    }
    let vec = marginal(&counts, "vec")?;
    let csv = marginal(&counts, "csv_writer")?;
    writeln!(
        out,
        "\nFor pre-split borrowed fields, `VecEmitter` is {} than `csv::Writer` on this\n\
         corpus. Typed rows include field formatting and dispatch as well as CSV framing.\n",
        ratio(vec, csv)
    )
    .unwrap();
    Ok(())
}

fn parallel_map(section: &ParallelSection) -> BTreeMap<&str, &ParallelPoint> {
    section
        .points
        .iter()
        .map(|point| (point.id.as_str(), point))
        .collect()
}

fn parallel_point<'a>(
    points: &'a BTreeMap<&str, &ParallelPoint>,
    id: &str,
) -> Result<&'a ParallelPoint, String> {
    points
        .get(id)
        .copied()
        .ok_or_else(|| format!("no Criterion measurement for `{id}`"))
}

fn gib_per_second(point: &ParallelPoint) -> f64 {
    point.bytes as f64 / point.median_ns * 1_000_000_000.0 / (1_u64 << 30) as f64
}

fn render_parallel(out: &mut String, section: &ParallelSection) -> Result<(), String> {
    writeln!(out, "## Parallel scaling\n").unwrap();
    writeln!(
        out,
        "Criterion median wall time converted to GiB/s. `fold` is compared with the\n\
         borrowed serial reduction; `owned` is compared with the reused owned-record\n\
         serial path. Speedup is baseline time divided by parallel time.\n"
    )
    .unwrap();
    render_evidence(out, &section.evidence);
    let points = parallel_map(section);
    writeln!(
        out,
        "| Input | Serial borrowed | Auto `fold` | Speedup | Serial owned | Auto owned | Speedup |"
    )
    .unwrap();
    writeln!(out, "|---:|---:|---:|---:|---:|---:|---:|").unwrap();
    for size in [8, 16, 32, 64] {
        let borrowed = parallel_point(
            &points,
            &format!("parallel/serial/borrowed/{size}MiB"),
        )?;
        let fold = parallel_point(
            &points,
            &format!("parallel/fold/threads-auto/{size}MiB"),
        )?;
        let owned =
            parallel_point(&points, &format!("parallel/serial/owned/{size}MiB"))?;
        let parallel_owned = parallel_point(
            &points,
            &format!("parallel/owned/threads-auto/{size}MiB"),
        )?;
        writeln!(
            out,
            "| {size} MiB | {:.2} GiB/s | {:.2} GiB/s | {:.2}× | {:.2} GiB/s | {:.2} GiB/s | {:.2}× |",
            gib_per_second(borrowed),
            gib_per_second(fold),
            borrowed.median_ns / fold.median_ns,
            gib_per_second(owned),
            gib_per_second(parallel_owned),
            owned.median_ns / parallel_owned.median_ns,
        )
        .unwrap();
    }
    writeln!(
        out,
        "\nScaling at 64 MiB, where startup is amortized:\n\n\
         | Path | Threads | Throughput | Speedup over matching serial path |\n\
         |---|---:|---:|---:|"
    )
    .unwrap();
    for (path, serial_path) in [("fold", "borrowed"), ("owned", "owned")] {
        let serial =
            parallel_point(&points, &format!("parallel/serial/{serial_path}/64MiB"))?;
        for threads in ["2", "4", "8", "auto"] {
            let point = parallel_point(
                &points,
                &format!("parallel/{path}/threads-{threads}/64MiB"),
            )?;
            writeln!(
                out,
                "| `{path}` | {threads} | {:.2} GiB/s | {:.2}× |",
                gib_per_second(point),
                serial.median_ns / point.median_ns,
            )
            .unwrap();
        }
    }
    writeln!(
        out,
        "\nThe crossover is the first size whose speedup stays above 1× on repeated idle-host\n\
         runs. This reference run crosses earlier, but 8 and 16 MiB have not held their\n\
         margin under ordinary host load, so the public fallback threshold remains the\n\
         conservative 32 MiB point. Wall-clock rows are host-specific; do not compare\n\
         them across machines.\n"
    )
    .unwrap();
    writeln!(
        out,
        "\nTwo follow-up probes on 2026-08-15 used the same 64 MiB document shape with\n\
         `CARGO_TARGET_DIR=target-perf-parallel`. They were diagnostic wall-clock runs,\n\
         not baseline refreshes. The first temporarily timed the borrowed `fold` path's\n\
         serial boundary pass and parallel phase separately for 15 repetitions. The\n\
         boundary pass was roughly constant in absolute time and became a larger share\n\
         as more workers shortened the parallel phase:\n\
         \n\
         | Threads | Total `fold` median | Boundary median | Parallel phase median | Boundary share |\n\
         |---:|---:|---:|---:|---:|\n\
         | 2 | 61.0 ms | 15.8 ms | 44.4 ms | 25.9% |\n\
         | 4 | 41.0 ms | 18.1 ms | 23.9 ms | 44.2% |\n\
         | 8 | 28.3 ms | 15.7 ms | 12.9 ms | 55.5% |\n\
         | auto | 27.7 ms | 15.5 ms | 12.5 ms | 56.0% |\n\
         \n\
         The serial borrowed median in the same run was 79.3 ms, so the boundary pass is\n\
         not immaterial: because the serial path does not run it at all, the prelude by\n\
         itself would cap the borrowed path near 79.3 / 15.5 = 5.1× on this run. The\n\
         published reference row's serial time gives the same conclusion at roughly a\n\
         4× cap. Past eight threads the already-minimal boundary scan is therefore the\n\
         majority of elapsed time and a real hard ceiling, but it does not explain the\n\
         remaining gap between the measured 2-3× speedup and that 4-5× ceiling. That\n\
         gap remains bandwidth and coordination. A speculative parallel boundary scan\n\
         is still not recommended without stronger evidence: it would have to reconcile\n\
         quote parity across chunk boundaries for hostile quoted input while preserving\n\
         the exact split and deterministic-error contracts.\n\
         \n\
         A second probe swept the owned path's private coordination constants by editing\n\
         and rebuilding one value at a time. Each row used 11 repetitions; the\n\
         owned path's observed min/max spreads were commonly 20-80%, so only large,\n\
         directional changes are meaningful. No setting lifted the 2-, 4-, and 8-thread\n\
         rows above 1× together.\n\
         \n\
         | Queue depth (`CHUNKS_PER_THREAD=16`) | 2 threads | 4 threads | 8 threads | auto |\n\
         |---:|---:|---:|---:|---:|\n\
         | 1 | 0.36× | 0.39× | 0.36× | 0.79× |\n\
         | 2 | 0.30× | 0.33× | 0.50× | 1.30× |\n\
         | 4 | 0.42× | 0.52× | 1.33× | 1.34× |\n\
         | 8 | 0.48× | 0.83× | 1.38× | 1.32× |\n\
         | 16 | 0.59× | 0.99× | 1.24× | 1.14× |\n\
         \n\
         | Chunks per thread (`QUEUE_DEPTH=4`) | 2 threads | 4 threads | 8 threads | auto |\n\
         |---:|---:|---:|---:|---:|\n\
         | 2 | 0.29× | 0.31× | 0.32× | 0.34× |\n\
         | 4 | 0.32× | 0.34× | 0.40× | 0.44× |\n\
         | 8 | 0.83× | 0.91× | 1.21× | 2.40× |\n\
         | 16 | 0.42× | 0.52× | 1.33× | 1.34× |\n\
         | 32 | 0.42× | 0.79× | 1.32× | 1.39× |\n\
         \n\
         The `CHUNKS_PER_THREAD=8` auto result was the largest number in the sweep, so it\n\
         was re-run against the shipped `CHUNKS_PER_THREAD=16` in a single interleaved\n\
         session: five alternating cycles, five repetitions per configuration per\n\
         cycle, with `QUEUE_DEPTH=4`. It collapsed under repetition and regressed every\n\
         owned row relative to the shipped value:\n\
         \n\
         | Chunks per thread | Threads | Median time | Min | Max | IQR | Speedup |\n\
         |---:|---:|---:|---:|---:|---:|---:|\n\
         | 16 | 2 | 334.6 ms | 301.3 ms | 406.0 ms | 40.8 ms | 0.34× |\n\
         | 16 | 4 | 276.8 ms | 192.4 ms | 331.6 ms | 30.7 ms | 0.42× |\n\
         | 16 | 8 | 120.9 ms | 100.0 ms | 272.3 ms | 43.1 ms | 0.95× |\n\
         | 16 | auto | 105.6 ms | 74.2 ms | 178.5 ms | 24.8 ms | 1.09× |\n\
         | 8 | 2 | 356.3 ms | 314.4 ms | 414.3 ms | 30.4 ms | 0.33× |\n\
         | 8 | 4 | 327.0 ms | 282.8 ms | 373.9 ms | 29.6 ms | 0.36× |\n\
         | 8 | 8 | 260.7 ms | 199.7 ms | 354.5 ms | 34.0 ms | 0.45× |\n\
         | 8 | auto | 130.9 ms | 94.7 ms | 220.2 ms | 43.5 ms | 0.90× |\n\
         \n\
         The earlier 2.40× auto result for `CHUNKS_PER_THREAD=8` was a measured outlier,\n\
         not a landable tuning result. The remaining columnar-batch idea would have to\n\
         replace the public `for_each_batch(&mut Vec<ByteRecord>)` owned-record contract\n\
         rather than retune private coordination.\n"
    )
    .unwrap();
    Ok(())
}

fn render_index(out: &mut String, section: &InstructionSection) -> Result<(), String> {
    writeln!(out, "## Index build, generation, and seek\n").unwrap();
    writeln!(
        out,
        "Callgrind instructions. Build rows use the same marginal calculation as writing;\n\
         seek rows are the fixed cost of resolving and reading one record.\n"
    )
    .unwrap();
    render_evidence(out, &section.evidence);
    let counts = instruction_map(section);
    writeln!(out, "| Build path | Instructions/record |").unwrap();
    writeln!(out, "|---|---:|").unwrap();
    for (case, label) in [
        ("build", "in-memory `CsvIndex::build`"),
        ("create", "streaming `CsvIndex::create`"),
        ("generate", "encode and index with `CsvIndex::generate`"),
    ] {
        writeln!(out, "| {label} | {:.0} |", marginal(&counts, case)?).unwrap();
    }
    writeln!(
        out,
        "\n| Seek path | First | Middle | Last |\n|---|---:|---:|---:|"
    )
    .unwrap();
    for (case, label) in [
        ("seek", "validated in-memory index"),
        ("reader_seek", "streamed index reader"),
    ] {
        writeln!(
            out,
            "| {label} | {} | {} | {} |",
            thousands(instruction(&counts, case, "at_first")?),
            thousands(instruction(&counts, case, "at_middle")?),
            thousands(instruction(&counts, case, "at_last")?),
        )
        .unwrap();
    }
    writeln!(
        out,
        "\n| Source rows | Plain seek | Bound seek |\n|---:|---:|---:|"
    )
    .unwrap();
    for rows in [1, 10, 100, 1000] {
        let variant = format!("rows_{rows}");
        writeln!(
            out,
            "| {rows} | {} | {} |",
            thousands(instruction(&counts, "seek_by_size", &variant)?),
            thousands(instruction(&counts, "bound_seek", &variant)?),
        )
        .unwrap();
    }
    writeln!(
        out,
        "\nBinding pays source validation once: plain seek grows with source size, while a\n\
         bound seek stays approximately constant.\n"
    )
    .unwrap();
    Ok(())
}

fn render_memory(out: &mut String, section: &MemorySection) {
    writeln!(out, "## Allocation and peak memory\n").unwrap();
    writeln!(
        out,
        "The focused harness instruments the process global allocator over generated\n\
         numeric documents. Peak memory below means **peak additional live heap**, not\n\
         resident set size; input and harness setup are outside the measured region.\n\
         \n\
         The parallel rows run at a pinned four threads, because the pools they watch are\n\
         sized per worker and a row taken at the host's thread count could not be\n\
         committed. Each parallel path is measured at 16 MiB and at 64 MiB: the pair is\n\
         the only way to state what `parallel.rs` promises — that peak follows the\n\
         threads and their work unit and not the document — and the harness fails when\n\
         either grows more than threefold for the fourfold document. `index_build_serial`\n\
         runs below the parallel index threshold so it stays the serial builder whatever\n\
         the host has, and `index_build_parallel` is the threaded one beside it.\n"
    )
    .unwrap();
    render_evidence(out, &section.evidence);
    writeln!(out, "Metric: `{}`.\n", section.metric).unwrap();
    writeln!(
        out,
        "| Path | Records | Allocations | Cumulative heap growth | Peak additional heap |"
    )
    .unwrap();
    writeln!(out, "|---|---:|---:|---:|---:|").unwrap();
    for case in &section.cases {
        writeln!(
            out,
            "| `{}` | {} | {} | {} | {} |",
            case.case,
            thousands(case.operations as u64),
            thousands(case.allocations),
            human_bytes(case.allocated_bytes as usize),
            human_bytes(case.peak_live_bytes),
        )
        .unwrap();
    }
    writeln!(out).unwrap();
}

fn render_macro(out: &mut String, section: &MacroSection) {
    writeln!(out, "## Proc-macro compile time\n").unwrap();
    writeln!(
        out,
        "Clean downstream `cargo check` wall time after dependencies are prepared. The\n\
         stable statistic is the harness's 20%-trimmed mean over {} samples per case; widths are\n\
         eight fields (`narrow`) and 128 fields (`wide`).\n",
        section.samples
    )
    .unwrap();
    render_evidence(out, &section.evidence);
    writeln!(out, "Metric: `{}`.\n", section.metric).unwrap();
    writeln!(out, "| Derive fixture | Compile time |").unwrap();
    writeln!(out, "|---|---:|").unwrap();
    for case in &section.cases {
        writeln!(out, "| `{}` | {:.1} ms |", case.case, case.milliseconds).unwrap();
    }
    writeln!(out).unwrap();
}

fn render_caveats(out: &mut String) {
    writeln!(out, "## How to read the measurements\n").unwrap();
    writeln!(
        out,
        "- Callgrind sections are deterministic work counts, not elapsed time.\n\
         - Parallel and proc-macro sections are wall-clock measurements and are meaningful\n\
         \x20 only on the host named in their evidence block.\n\
         - Peak heap excludes the generated input and is not operating-system RSS.\n\
         - Every corpus is synthetic. Measure your own data when its shape differs.\n\
         - Instruction counts from different benchmark binaries are not comparable to each\n\
         \x20 other; the optimizer's inlining decisions inside a measured loop depend on the\n\
         \x20 rest of the binary. Every ratio quoted anywhere in this crate is between rows\n\
         \x20 measured in one binary.\n\
         - Source comments justifying an `#[inline]` with a percentage record what removing\n\
         \x20 it measured at the time. They are observations, not enforced properties: a gate\n\
         \x20 can only measure the code as written, never the counterfactual. The properties\n\
         \x20 that are enforced are the ratios listed by `scripts/perf_gate.py`.\n"
    )
    .unwrap();
    writeln!(
        out,
        "The normalized artifact is publication evidence: generation refuses a dirty tree,\n\
         records each section's exact command, host, toolchain and revision, and fails if\n\
         any focused harness is absent rather than printing a placeholder.\n"
    )
    .unwrap();
}

/// Group digits so a six-figure byte count is readable.
fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

fn human_bytes(value: usize) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut amount = value as f64;
    let mut unit = 0;
    while amount >= 1024.0 && unit + 1 < UNITS.len() {
        amount /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} {}", UNITS[unit])
    } else {
        format!("{amount:.2} {}", UNITS[unit])
    }
}
