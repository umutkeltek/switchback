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
        /// Mark rows with fewer runs as insufficient sample.
        #[arg(long, default_value_t = 1)]
        min_runs: u64,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub(crate) enum EvalCaseCmd {
    /// Validate a case manifest file without writing it.
    Validate { path: PathBuf },
    /// Import a case manifest into the eval evidence store.
    Import { path: PathBuf },
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

pub(crate) fn run_eval_cmd(action: EvalCmd, store_path: &Path, json: bool) -> anyhow::Result<()> {
    match action {
        EvalCmd::Case { action } => run_eval_case_cmd(action, store_path, json),
        EvalCmd::Ingest {
            result,
            case,
            dry_run,
        } => run_eval_ingest_cmd(result, case, dry_run, store_path, json),
        EvalCmd::Report {
            by,
            task_type,
            tag,
            min_runs,
        } => run_eval_report_cmd(by, task_type, tag, min_runs, store_path, json),
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
    by: String,
    task_type: Option<String>,
    tag: Option<String>,
    min_runs: u64,
    store_path: &Path,
    json: bool,
) -> anyhow::Result<()> {
    if by != "harness" {
        return Err(anyhow!("eval report MVP only supports --by harness"));
    }
    let store = open_eval_store(store_path)?;
    let report = store.eval_report(sb_eval::EvalReportQuery {
        task_type: task_type
            .as_deref()
            .map(parse_task_type)
            .transpose()
            .with_context(|| "invalid --task-type")?,
        tag,
        min_runs,
    })?;
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
        "{:<20} {:>5} {:>8} {:>8} {:>8} {:>12} {:>12}",
        "harness", "runs", "pass", "fail", "unknown", "median_ms", "median_cost"
    );
    for row in &report.rows {
        let unknown = row.inconclusive_count + row.not_evaluated_count;
        println!(
            "{:<20} {:>5} {:>8} {:>8} {:>8} {:>12} {:>12}",
            row.harness,
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
