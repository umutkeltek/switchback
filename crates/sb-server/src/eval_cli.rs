use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use clap::Subcommand;
use sb_core::ExecutionTaskType;
use sb_store::SqliteStore;
use serde::Serialize;

#[derive(Debug, Clone, Subcommand)]
pub(crate) enum EvalCmd {
    /// Validate or import evaluation case manifests.
    Case {
        #[command(subcommand)]
        action: EvalCaseCmd,
    },
    /// Convert a harness-specific JSON result into a sanitized eval run manifest.
    Convert {
        #[arg(value_parser = parse_harness_kind)]
        kind: sb_eval::HarnessKind,
        /// Harness result JSON/YAML.
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        case_id: String,
        #[arg(long)]
        case_revision: String,
        #[arg(long)]
        strategy_id: Option<String>,
        #[arg(long, value_parser = parse_verdict)]
        verdict: Option<sb_eval::Verdict>,
        #[arg(long, value_parser = parse_run_status)]
        status: Option<sb_eval::RunStatus>,
        /// Optional output file. Defaults to stdout.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Ingest one externally-produced harness run manifest.
    Ingest {
        /// Eval run manifest JSON/YAML.
        #[arg(long)]
        result: PathBuf,
        /// Optional case manifest to import before ingesting the run.
        #[arg(long)]
        case: Option<PathBuf>,
        /// Validate input without opening or writing the store.
        #[arg(long)]
        dry_run: bool,
    },
    /// Report eval evidence grouped by harness.
    Report {
        /// Report grouping. MVP supports `harness`.
        #[arg(long, default_value = "harness")]
        by: String,
        /// Optional task type filter: chat, coding, extraction, judge, tool_agent, embeddings.
        #[arg(long)]
        task_type: Option<String>,
        /// Optional case tag filter.
        #[arg(long)]
        tag: Option<String>,
        /// Optional harness id filter.
        #[arg(long)]
        harness: Option<String>,
        /// Optional harness version filter.
        #[arg(long)]
        harness_version: Option<String>,
        /// Optional strategy id filter.
        #[arg(long)]
        strategy_id: Option<String>,
        /// Exclude cache-hit runs from the report.
        #[arg(long)]
        exclude_cache_hits: bool,
        /// Include only runs starting/finishing at or after this epoch-ms.
        #[arg(long)]
        since_ms: Option<u64>,
        /// Include only runs starting/finishing at or before this epoch-ms.
        #[arg(long)]
        until_ms: Option<u64>,
        /// Mark rows with fewer runs as insufficient sample.
        #[arg(long, default_value_t = 1)]
        min_runs: u64,
    },
    /// Build and publish precomputed eval evidence snapshots.
    Snapshot {
        #[command(subcommand)]
        action: EvalSnapshotCmd,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub(crate) enum EvalCaseCmd {
    /// Validate a case manifest file without writing it.
    Validate { path: PathBuf },
    /// Import a case manifest into the eval evidence store.
    Import { path: PathBuf },
}

#[derive(Debug, Clone, Subcommand)]
pub(crate) enum EvalSnapshotCmd {
    /// Build snapshot JSON from stored runs without activating it.
    Build {
        /// Snapshot grouping. Supports `harness`, `strategy`, `harness_version`.
        #[arg(long, default_value = "harness")]
        by: String,
        /// Optional task type filter: chat, coding, extraction, judge, tool_agent, embeddings.
        #[arg(long)]
        task_type: Option<String>,
        /// Optional tag filter.
        #[arg(long)]
        tag: Option<String>,
        /// Optional harness id filter.
        #[arg(long)]
        harness: Option<String>,
        /// Optional harness version filter.
        #[arg(long)]
        harness_version: Option<String>,
        /// Optional strategy id filter.
        #[arg(long)]
        strategy_id: Option<String>,
        /// Exclude cache-hit runs from snapshot.
        #[arg(long)]
        exclude_cache_hits: bool,
        /// Include only runs starting/finishing after this epoch-ms.
        #[arg(long)]
        since_ms: Option<u64>,
        /// Include only runs starting/finishing before this epoch-ms.
        #[arg(long)]
        until_ms: Option<u64>,
        /// Mark rows with fewer runs as insufficient sample.
        #[arg(long, default_value_t = 1)]
        min_runs: u64,
        /// Override snapshot generation time for deterministic fixture builds.
        #[arg(long)]
        generated_at_ms: Option<u64>,
        /// Optional output file. Snapshot JSON is also printed to stdout.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Publish a built snapshot under a stable name.
    Publish {
        /// Built snapshot JSON/YAML.
        #[arg(long)]
        snapshot: PathBuf,
        /// Published snapshot name.
        #[arg(long, default_value = "current")]
        name: String,
    },
    /// Print the currently published snapshot for a name.
    Current {
        /// Published snapshot name.
        #[arg(long, default_value = "current")]
        name: String,
    },
}

#[derive(Debug, Serialize)]
struct EvalCaseOutput {
    ok: bool,
    action: &'static str,
    case_id: String,
    case_revision: String,
    tags: Vec<String>,
}

#[derive(Debug, Serialize)]
struct EvalIngestOutput {
    ok: bool,
    dry_run: bool,
    run_id: String,
    inserted: bool,
    case_id: String,
    case_revision: String,
    harness: String,
    verdict: sb_eval::Verdict,
}

#[derive(Debug, Serialize)]
struct EvalReportOutput {
    schema: &'static str,
    by: String,
    rows: Vec<sb_eval::EvalReportRow>,
}

#[derive(Debug, Serialize)]
struct EvalSnapshotPublishOutput {
    ok: bool,
    name: String,
    snapshot_id: String,
    generated_at_ms: u64,
    published_at_ms: i64,
    snapshot_sha256: String,
}

#[derive(Debug, Serialize)]
struct EvalSnapshotCurrentOutput {
    ok: bool,
    name: String,
    snapshot_id: String,
    generated_at_ms: u64,
    published_at_ms: i64,
    snapshot_sha256: String,
    rows: usize,
    snapshot: sb_eval::EvalEvidenceSnapshot,
}

struct EvalReportOptions {
    by: String,
    task_type: Option<String>,
    tag: Option<String>,
    harness: Option<String>,
    harness_version: Option<String>,
    strategy_id: Option<String>,
    exclude_cache_hits: bool,
    since_ms: Option<u64>,
    until_ms: Option<u64>,
    min_runs: u64,
}

struct EvalSnapshotOptions {
    report: EvalReportOptions,
    generated_at_ms: Option<u64>,
    output: Option<PathBuf>,
}

struct EvalConvertOptions {
    kind: sb_eval::HarnessKind,
    input: PathBuf,
    case_id: String,
    case_revision: String,
    strategy_id: Option<String>,
    verdict: Option<sb_eval::Verdict>,
    status: Option<sb_eval::RunStatus>,
    output: Option<PathBuf>,
}

pub(crate) fn run_eval_cmd(action: EvalCmd, store_path: &Path, json: bool) -> anyhow::Result<()> {
    match action {
        EvalCmd::Case { action } => run_eval_case_cmd(action, store_path, json),
        EvalCmd::Convert {
            kind,
            input,
            case_id,
            case_revision,
            strategy_id,
            verdict,
            status,
            output,
        } => run_eval_convert_cmd(EvalConvertOptions {
            kind,
            input,
            case_id,
            case_revision,
            strategy_id,
            verdict,
            status,
            output,
        }),
        EvalCmd::Ingest {
            result,
            case,
            dry_run,
        } => run_eval_ingest_cmd(result, case, dry_run, store_path, json),
        EvalCmd::Report {
            by,
            task_type,
            tag,
            harness,
            harness_version,
            strategy_id,
            exclude_cache_hits,
            since_ms,
            until_ms,
            min_runs,
        } => run_eval_report_cmd(
            EvalReportOptions {
                by,
                task_type,
                tag,
                harness,
                harness_version,
                strategy_id,
                exclude_cache_hits,
                since_ms,
                until_ms,
                min_runs,
            },
            store_path,
            json,
        ),
        EvalCmd::Snapshot { action } => run_eval_snapshot_cmd(action, store_path, json),
    }
}

fn run_eval_case_cmd(action: EvalCaseCmd, store_path: &Path, json: bool) -> anyhow::Result<()> {
    match action {
        EvalCaseCmd::Validate { path } => {
            let case = load_eval_case(&path)?;
            case.validate().map_err(|err| anyhow!(err.0))?;
            print_case_output("validate", case, json)
        }
        EvalCaseCmd::Import { path } => {
            let case = load_eval_case(&path)?;
            case.validate().map_err(|err| anyhow!(err.0))?;
            let store = open_eval_store(store_path)?;
            store.put_eval_case(&case)?;
            print_case_output("import", case, json)
        }
    }
}

fn run_eval_convert_cmd(options: EvalConvertOptions) -> anyhow::Result<()> {
    let EvalConvertOptions {
        kind,
        input,
        case_id,
        case_revision,
        strategy_id,
        verdict,
        status,
        output,
    } = options;
    let input_value: serde_json::Value = load_manifest(&input)
        .with_context(|| format!("load harness result {}", input.display()))?;
    let run = sb_eval::HarnessConversion {
        kind,
        case_id,
        case_revision,
        strategy_id,
        verdict,
        status,
        input: input_value,
    }
    .convert()
    .map_err(|err| anyhow!(err.0))?;
    let rendered = serde_json::to_string_pretty(&run)?;
    if let Some(path) = output {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, rendered)?;
    } else {
        println!("{rendered}");
    }
    Ok(())
}

fn run_eval_ingest_cmd(
    result: PathBuf,
    case_path: Option<PathBuf>,
    dry_run: bool,
    store_path: &Path,
    json: bool,
) -> anyhow::Result<()> {
    let case = case_path
        .as_ref()
        .map(|path| load_eval_case(path))
        .transpose()?;
    if let Some(case) = &case {
        case.validate().map_err(|err| anyhow!(err.0))?;
    }

    let run = load_eval_run(&result)?;
    run.validate().map_err(|err| anyhow!(err.0))?;

    let receipt = if dry_run {
        sb_eval::EvalIngestReceipt {
            run_id: run.stable_run_id(),
            inserted: false,
        }
    } else {
        let store = open_eval_store(store_path)?;
        if let Some(case) = &case {
            store.put_eval_case(case)?;
        }
        store.ingest_eval_run(&run)?
    };

    let output = EvalIngestOutput {
        ok: true,
        dry_run,
        run_id: receipt.run_id,
        inserted: receipt.inserted,
        case_id: run.case_id,
        case_revision: run.case_revision,
        harness: run.harness,
        verdict: run.outcome.verdict,
    };
    if json {
        print_json(&output)
    } else {
        println!(
            "eval ingest: run={} harness={} verdict={:?} inserted={}",
            output.run_id, output.harness, output.verdict, output.inserted
        );
        Ok(())
    }
}

fn run_eval_report_cmd(
    options: EvalReportOptions,
    store_path: &Path,
    json: bool,
) -> anyhow::Result<()> {
    let by = options.by.clone();
    let query = eval_report_query(&options)?;
    let store = open_eval_store(store_path)?;
    let report = store.eval_report(query)?;
    if json {
        print_json(&EvalReportOutput {
            schema: "switchback.eval.report/v1",
            by,
            rows: report.rows,
        })
    } else {
        print_report_table(&report);
        Ok(())
    }
}

fn run_eval_snapshot_cmd(
    action: EvalSnapshotCmd,
    store_path: &Path,
    json: bool,
) -> anyhow::Result<()> {
    match action {
        EvalSnapshotCmd::Build {
            by,
            task_type,
            tag,
            harness,
            harness_version,
            strategy_id,
            exclude_cache_hits,
            since_ms,
            until_ms,
            min_runs,
            generated_at_ms,
            output,
        } => run_eval_snapshot_build_cmd(
            EvalSnapshotOptions {
                report: EvalReportOptions {
                    by,
                    task_type,
                    tag,
                    harness,
                    harness_version,
                    strategy_id,
                    exclude_cache_hits,
                    since_ms,
                    until_ms,
                    min_runs,
                },
                generated_at_ms,
                output,
            },
            store_path,
        ),
        EvalSnapshotCmd::Publish { snapshot, name } => {
            run_eval_snapshot_publish_cmd(snapshot, name, store_path, json)
        }
        EvalSnapshotCmd::Current { name } => run_eval_snapshot_current_cmd(name, store_path, json),
    }
}

fn run_eval_snapshot_build_cmd(
    options: EvalSnapshotOptions,
    store_path: &Path,
) -> anyhow::Result<()> {
    let EvalSnapshotOptions {
        report,
        generated_at_ms,
        output,
    } = options;
    let query = eval_report_query(&report)?;
    let store = open_eval_store(store_path)?;
    let eval_report = store.eval_report(query.clone())?;
    let generated_at_ms = generated_at_ms.unwrap_or_else(|| sb_store::now_millis().max(0) as u64);
    let snapshot = sb_eval::EvalEvidenceSnapshot::from_report(&query, eval_report, generated_at_ms);
    let rendered = serde_json::to_string_pretty(&snapshot)?;
    if let Some(path) = output {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, &rendered)?;
    }
    println!("{rendered}");
    Ok(())
}

fn run_eval_snapshot_publish_cmd(
    snapshot_path: PathBuf,
    name: String,
    store_path: &Path,
    json: bool,
) -> anyhow::Result<()> {
    let snapshot: sb_eval::EvalEvidenceSnapshot = load_manifest(&snapshot_path)
        .with_context(|| format!("load eval evidence snapshot {}", snapshot_path.display()))?;
    snapshot.validate().map_err(|err| anyhow!(err.0))?;
    let store = open_eval_store(store_path)?;
    let record = store.publish_eval_evidence_snapshot(&name, &snapshot)?;
    let output = EvalSnapshotPublishOutput {
        ok: true,
        name: record.name,
        snapshot_id: record.snapshot_id,
        generated_at_ms: record.generated_at_ms,
        published_at_ms: record.published_at_ms,
        snapshot_sha256: record.snapshot_sha256,
    };
    if json {
        print_json(&output)
    } else {
        println!(
            "eval snapshot publish: name={} snapshot={} published_at_ms={}",
            output.name, output.snapshot_id, output.published_at_ms
        );
        Ok(())
    }
}

fn run_eval_snapshot_current_cmd(
    name: String,
    store_path: &Path,
    json: bool,
) -> anyhow::Result<()> {
    let store = open_eval_store(store_path)?;
    let record = store
        .get_eval_evidence_snapshot_record(&name)?
        .ok_or_else(|| anyhow!("no published eval evidence snapshot named `{name}`"))?;
    let snapshot = store
        .get_eval_evidence_snapshot(&name)?
        .ok_or_else(|| anyhow!("no published eval evidence snapshot named `{name}`"))?;
    let output = EvalSnapshotCurrentOutput {
        ok: true,
        name: record.name,
        snapshot_id: record.snapshot_id,
        generated_at_ms: record.generated_at_ms,
        published_at_ms: record.published_at_ms,
        snapshot_sha256: record.snapshot_sha256,
        rows: snapshot.rows.len(),
        snapshot,
    };
    if json {
        print_json(&output)
    } else {
        println!(
            "eval snapshot current: name={} snapshot={} rows={} published_at_ms={}",
            output.name, output.snapshot_id, output.rows, output.published_at_ms
        );
        Ok(())
    }
}

fn eval_report_query(options: &EvalReportOptions) -> anyhow::Result<sb_eval::EvalReportQuery> {
    let (group_by_strategy, group_by_harness_version) = parse_report_grouping(&options.by)?;
    Ok(sb_eval::EvalReportQuery {
        task_type: options
            .task_type
            .as_deref()
            .map(parse_task_type)
            .transpose()
            .with_context(|| "invalid --task-type")?,
        tag: options.tag.clone(),
        harness: options.harness.clone(),
        harness_version: options.harness_version.clone(),
        strategy_id: options.strategy_id.clone(),
        exclude_cache_hits: options.exclude_cache_hits,
        since_ms: options.since_ms,
        until_ms: options.until_ms,
        min_runs: options.min_runs,
        group_by_strategy,
        group_by_harness_version,
    })
}

fn load_eval_case(path: &Path) -> anyhow::Result<sb_eval::EvalCaseManifest> {
    load_manifest(path).with_context(|| format!("load eval case {}", path.display()))
}

fn load_eval_run(path: &Path) -> anyhow::Result<sb_eval::EvalRunIngest> {
    load_manifest(path).with_context(|| format!("load eval run {}", path.display()))
}

fn load_manifest<T: for<'de> serde::Deserialize<'de>>(path: &Path) -> anyhow::Result<T> {
    let raw = fs::read_to_string(path)?;
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("yaml" | "yml") => Ok(serde_yaml::from_str(&raw)?),
        _ => Ok(serde_json::from_str(&raw)?),
    }
}

fn open_eval_store(path: &Path) -> anyhow::Result<SqliteStore> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    SqliteStore::open(path.to_string_lossy().as_ref()).map_err(Into::into)
}

fn parse_task_type(value: &str) -> anyhow::Result<ExecutionTaskType> {
    let task_type = ExecutionTaskType::parse(value);
    if task_type == ExecutionTaskType::Unknown && value != "unknown" {
        Err(anyhow!("unknown task type `{value}`"))
    } else {
        Ok(task_type)
    }
}

fn parse_harness_kind(value: &str) -> Result<sb_eval::HarnessKind, String> {
    sb_eval::HarnessKind::parse(value).ok_or_else(|| {
        "unknown harness kind; expected codex-cli, claude-code, or aider".to_string()
    })
}

fn parse_verdict(value: &str) -> Result<sb_eval::Verdict, String> {
    match value {
        "pass" => Ok(sb_eval::Verdict::Pass),
        "fail" => Ok(sb_eval::Verdict::Fail),
        "partial" => Ok(sb_eval::Verdict::Partial),
        "inconclusive" => Ok(sb_eval::Verdict::Inconclusive),
        "not_evaluated" | "not-evaluated" => Ok(sb_eval::Verdict::NotEvaluated),
        _ => Err(
            "unknown verdict; expected pass, fail, partial, inconclusive, or not_evaluated"
                .to_string(),
        ),
    }
}

fn parse_run_status(value: &str) -> Result<sb_eval::RunStatus, String> {
    match value {
        "succeeded" => Ok(sb_eval::RunStatus::Succeeded),
        "failed" => Ok(sb_eval::RunStatus::Failed),
        "cancelled" | "canceled" => Ok(sb_eval::RunStatus::Cancelled),
        "timed_out" | "timeout" => Ok(sb_eval::RunStatus::TimedOut),
        "inconclusive" => Ok(sb_eval::RunStatus::Inconclusive),
        _ => Err(
            "unknown run status; expected succeeded, failed, cancelled, timed_out, or inconclusive"
                .to_string(),
        ),
    }
}

fn parse_report_grouping(value: &str) -> anyhow::Result<(bool, bool)> {
    let mut saw_harness = false;
    let mut group_by_strategy = false;
    let mut group_by_harness_version = false;
    for part in value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        match part {
            "harness" => saw_harness = true,
            "strategy" | "strategy_id" => group_by_strategy = true,
            "harness_version" | "version" => group_by_harness_version = true,
            _ => return Err(anyhow!("unsupported eval report grouping `{part}`")),
        }
    }
    if !saw_harness {
        return Err(anyhow!("eval report grouping must include harness"));
    }
    Ok((group_by_strategy, group_by_harness_version))
}

fn print_case_output(
    action: &'static str,
    case: sb_eval::EvalCaseManifest,
    json: bool,
) -> anyhow::Result<()> {
    let output = EvalCaseOutput {
        ok: true,
        action,
        case_id: case.case_id,
        case_revision: case.case_revision,
        tags: case.tags,
    };
    if json {
        print_json(&output)
    } else {
        println!(
            "eval case {action}: {}@{}",
            output.case_id, output.case_revision
        );
        Ok(())
    }
}

fn print_report_table(report: &sb_eval::EvalReport) {
    println!(
        "{:<20} {:<14} {:<16} {:>5} {:>8} {:>8} {:>8} {:>12} {:>12}",
        "harness",
        "version",
        "strategy",
        "runs",
        "pass",
        "fail",
        "unknown",
        "median_ms",
        "median_cost"
    );
    for row in &report.rows {
        let unknown = row.inconclusive_count + row.not_evaluated_count;
        println!(
            "{:<20} {:<14} {:<16} {:>5} {:>8} {:>8} {:>8} {:>12} {:>12}",
            row.harness,
            row.harness_version.as_deref().unwrap_or("-"),
            row.strategy_id.as_deref().unwrap_or("-"),
            row.runs,
            row.pass_count,
            row.fail_count,
            unknown,
            row.median_latency_ms
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_string()),
            row.median_cost_micros
                .map(format_micros_usd)
                .unwrap_or_else(|| "-".to_string())
        );
    }
}

fn format_micros_usd(micros: u64) -> String {
    format!("${:.4}", micros as f64 / 1_000_000.0)
}

fn print_json<T: Serialize>(value: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
