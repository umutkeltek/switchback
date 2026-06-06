use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Args;
use rusqlite::{Connection, OpenFlags};
use sb_core::{ClientProfileKind, Config};
use sb_store::{NativeHistoryImportBatch, NativeHistoryImportRecord, NativeHistorySourceRecord};
use sb_store::{SqliteStore, StateStore};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::setup_cli::NativeClientTarget;

#[derive(Args, Clone)]
pub(crate) struct NativeImportHistoryArgs {
    /// Limit reporting to one native client.
    #[arg(long, value_enum, default_value_t = NativeClientTarget::All)]
    pub(crate) client: NativeClientTarget,
    /// Preview importable metadata without writing the state store.
    #[arg(long, conflicts_with = "apply")]
    pub(crate) dry_run: bool,
    /// Persist metadata-only source summaries into the Switchback state store.
    #[arg(long, conflicts_with = "dry_run")]
    pub(crate) apply: bool,
    /// SQLite state-store path. Defaults to server.state_store from --config.
    #[arg(long)]
    pub(crate) state_store: Option<PathBuf>,
    /// Include exact local paths. Defaults to stable redacted path ids.
    #[arg(long)]
    pub(crate) show_local_paths: bool,
    /// Number of sample files to list for glob-backed sources.
    #[arg(long, default_value_t = 10)]
    pub(crate) sample_files: usize,
    /// Maximum files scanned per glob-backed source.
    #[arg(long, default_value_t = 250)]
    pub(crate) max_files: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct NativeHistoryImportReport {
    schema: &'static str,
    ok: bool,
    dry_run: bool,
    applied: bool,
    read_only: bool,
    content_policy: ContentPolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    storage: Option<ImportStorageReport>,
    clients: Vec<ClientHistoryReport>,
    totals: HistoryTotals,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    next_actions: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ContentPolicy {
    metadata_only: bool,
    stores_prompts: bool,
    stores_responses: bool,
    stores_local_paths: bool,
    path_redaction_default: bool,
    transport: &'static str,
}

#[derive(Debug, Serialize)]
struct ImportStorageReport {
    kind: &'static str,
    state_store: String,
    state_store_path_redacted: bool,
    import_id: String,
    source_rows_written: u64,
    metadata_only: bool,
    stores_prompts: bool,
    stores_responses: bool,
    stores_local_paths: bool,
}

#[derive(Debug, Serialize)]
struct ClientHistoryReport {
    id: &'static str,
    sources: Vec<HistorySourceReport>,
    totals: HistoryTotals,
}

#[derive(Debug, Clone, Default, Serialize)]
struct HistoryTotals {
    source_count: usize,
    existing_source_count: usize,
    file_count: usize,
    record_count: u64,
    parse_error_count: u64,
    byte_count: u64,
}

#[derive(Debug, Serialize)]
struct HistorySourceReport {
    source_id: &'static str,
    client: &'static str,
    kind: &'static str,
    parser: &'static str,
    path_pattern: &'static str,
    path_id: String,
    path: String,
    path_redacted: bool,
    exists: bool,
    truncated: bool,
    skipped_file_count: usize,
    file_count: usize,
    record_count: u64,
    parse_error_count: u64,
    byte_count: u64,
    modified_at_ms_min: Option<i64>,
    modified_at_ms_max: Option<i64>,
    observed_at_min: Option<String>,
    observed_at_max: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    sample_files: Vec<PathSample>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tables: Vec<SqliteTableSummary>,
    preview: ImportPreview,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    errors: Vec<String>,
}

#[derive(Debug, Serialize)]
struct PathSample {
    path: String,
    path_redacted: bool,
}

#[derive(Debug, Serialize)]
struct SqliteTableSummary {
    table: &'static str,
    exists: bool,
    record_count: u64,
    observed_at_min: Option<String>,
    observed_at_max: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    errors: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ImportPreview {
    transport: &'static str,
    client: &'static str,
    source_id: &'static str,
    record_count: u64,
    observed_at_min: Option<String>,
    observed_at_max: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct JsonlSourceSpec {
    source_id: &'static str,
    client: ClientProfileKind,
    path_pattern: &'static str,
    glob: bool,
}

#[derive(Debug, Clone, Copy)]
struct SqliteSourceSpec {
    source_id: &'static str,
    client: ClientProfileKind,
    path_pattern: &'static str,
    tables: &'static [SqliteTableSpec],
}

#[derive(Debug, Clone, Copy)]
struct SqliteTableSpec {
    table: &'static str,
    time_columns: &'static [&'static str],
}

const CODEX_SQLITE_SOURCES: &[SqliteSourceSpec] = &[
    SqliteSourceSpec {
        source_id: "codex_state_sqlite",
        client: ClientProfileKind::Codex,
        path_pattern: "${HOME}/.codex/state_5.sqlite",
        tables: &[
            SqliteTableSpec {
                table: "threads",
                time_columns: &["updated_at_ms", "created_at_ms", "updated_at", "created_at"],
            },
            SqliteTableSpec {
                table: "agent_jobs",
                time_columns: &["updated_at", "created_at", "completed_at", "started_at"],
            },
        ],
    },
    SqliteSourceSpec {
        source_id: "codex_logs_sqlite",
        client: ClientProfileKind::Codex,
        path_pattern: "${HOME}/.codex/logs_2.sqlite",
        tables: &[SqliteTableSpec {
            table: "logs",
            time_columns: &["ts"],
        }],
    },
    SqliteSourceSpec {
        source_id: "codex_goals_sqlite",
        client: ClientProfileKind::Codex,
        path_pattern: "${HOME}/.codex/goals_1.sqlite",
        tables: &[SqliteTableSpec {
            table: "thread_goals",
            time_columns: &["updated_at_ms", "created_at_ms"],
        }],
    },
];

const JSONL_SOURCES: &[JsonlSourceSpec] = &[
    JsonlSourceSpec {
        source_id: "codex_history_jsonl",
        client: ClientProfileKind::Codex,
        path_pattern: "${HOME}/.codex/history.jsonl",
        glob: false,
    },
    JsonlSourceSpec {
        source_id: "codex_session_index_jsonl",
        client: ClientProfileKind::Codex,
        path_pattern: "${HOME}/.codex/session_index.jsonl",
        glob: false,
    },
    JsonlSourceSpec {
        source_id: "claude_history_jsonl",
        client: ClientProfileKind::ClaudeCode,
        path_pattern: "${HOME}/.claude/history.jsonl",
        glob: false,
    },
    JsonlSourceSpec {
        source_id: "claude_projects_jsonl",
        client: ClientProfileKind::ClaudeCode,
        path_pattern: "${HOME}/.claude/projects/**/*.jsonl",
        glob: true,
    },
];

pub(crate) fn native_history_import(
    args: NativeImportHistoryArgs,
    config_path: &Path,
) -> anyhow::Result<NativeHistoryImportReport> {
    if args.dry_run == args.apply {
        anyhow::bail!("native import-history requires exactly one of --dry-run or --apply");
    }

    let clients = native_client_kinds(args.client)
        .into_iter()
        .map(|kind| client_history_report(kind, &args))
        .collect::<Vec<_>>();
    let totals = clients
        .iter()
        .fold(HistoryTotals::default(), |mut acc, client| {
            acc.add(&client.totals);
            acc
        });
    let mut warnings = Vec::new();
    if totals.existing_source_count == 0 {
        warnings.push("no native history sources were found".to_string());
    }
    if totals.parse_error_count > 0 {
        warnings.push("one or more source records could not be parsed as metadata".to_string());
    }
    if clients
        .iter()
        .flat_map(|client| client.sources.iter())
        .any(|source| source.truncated)
    {
        warnings.push("one or more glob-backed sources were truncated by --max-files".to_string());
    }

    let mut report = NativeHistoryImportReport {
        schema: "switchback/native-history-import@1",
        ok: true,
        dry_run: args.dry_run,
        applied: false,
        read_only: args.dry_run,
        content_policy: ContentPolicy {
            metadata_only: true,
            stores_prompts: false,
            stores_responses: false,
            stores_local_paths: false,
            path_redaction_default: !args.show_local_paths,
            transport: "client_native_import",
        },
        storage: None,
        clients,
        totals,
        warnings,
        next_actions: if args.dry_run {
            vec![
                "review dry-run counts and parse errors".to_string(),
                "run native import-history --apply with a configured state store to persist metadata-only source summaries".to_string(),
            ]
        } else {
            Vec::new()
        },
    };

    if args.apply {
        let store_path = resolve_state_store_path(&args, config_path)?;
        let import_id = new_import_id(&report);
        let batch = report.to_store_batch(&import_id)?;
        let store = SqliteStore::open(&store_path.to_string_lossy())?;
        let write = store.record_native_history_import(&batch)?;
        report.applied = true;
        report.read_only = false;
        report.storage = Some(ImportStorageReport {
            kind: "sqlite_state_store",
            state_store: display_path(&store_path, args.show_local_paths),
            state_store_path_redacted: !args.show_local_paths,
            import_id,
            source_rows_written: write.source_rows_written,
            metadata_only: true,
            stores_prompts: false,
            stores_responses: false,
            stores_local_paths: false,
        });
        report
            .next_actions
            .push("use the state store native_history_imports/native_history_sources tables for local-client history observability".to_string());
    }

    Ok(report)
}

pub(crate) fn print_native_import_history_text(report: &NativeHistoryImportReport) {
    println!(
        "native import-history {}",
        if report.ok { "ok" } else { "not-ok" }
    );
    println!("dry_run {}", report.dry_run);
    println!("applied {}", report.applied);
    println!("read_only {}", report.read_only);
    println!("sources {}", report.totals.source_count);
    println!("existing_sources {}", report.totals.existing_source_count);
    println!("files {}", report.totals.file_count);
    println!("records {}", report.totals.record_count);
    println!("parse_errors {}", report.totals.parse_error_count);
    if let Some(storage) = &report.storage {
        println!("state_store {}", storage.state_store);
        println!("import_id {}", storage.import_id);
        println!("source_rows_written {}", storage.source_rows_written);
    }
    for warning in &report.warnings {
        println!("warning: {warning}");
    }
    for action in &report.next_actions {
        println!("next: {action}");
    }
}

fn client_history_report(
    kind: ClientProfileKind,
    args: &NativeImportHistoryArgs,
) -> ClientHistoryReport {
    let mut sources = Vec::new();
    for spec in JSONL_SOURCES
        .iter()
        .filter(|spec| spec.client == kind)
        .copied()
    {
        sources.push(jsonl_source_report(spec, args));
    }
    for spec in CODEX_SQLITE_SOURCES
        .iter()
        .filter(|spec| spec.client == kind)
        .copied()
    {
        sources.push(sqlite_source_report(spec, args.show_local_paths));
    }
    let totals = sources
        .iter()
        .fold(HistoryTotals::default(), |mut acc, source| {
            acc.add_source(source);
            acc
        });
    ClientHistoryReport {
        id: kind.default_id(),
        sources,
        totals,
    }
}

fn jsonl_source_report(
    spec: JsonlSourceSpec,
    args: &NativeImportHistoryArgs,
) -> HistorySourceReport {
    let mut files = if spec.glob {
        collect_claude_project_files()
    } else {
        vec![expand_home(spec.path_pattern)]
    };
    let discovered_file_count = files.len();
    let truncated = spec.glob && discovered_file_count > args.max_files;
    if truncated {
        files.truncate(args.max_files);
    }
    let skipped_file_count = discovered_file_count.saturating_sub(files.len());
    let mut summary = JsonlSummary::default();
    let mut sample_files = Vec::new();
    let mut errors = Vec::new();
    let mut existing_files = 0usize;

    for path in files {
        if path.exists() {
            existing_files += 1;
            if sample_files.len() < args.sample_files {
                sample_files.push(PathSample {
                    path: display_path(&path, args.show_local_paths),
                    path_redacted: !args.show_local_paths,
                });
            }
            match summarize_jsonl_file(&path) {
                Ok(file_summary) => summary.add(file_summary),
                Err(e) => errors.push(format!("{}: {e}", display_path(&path, false))),
            }
        }
    }

    let exists = existing_files > 0;
    let path = if spec.glob {
        spec.path_pattern.to_string()
    } else {
        display_path(&expand_home(spec.path_pattern), args.show_local_paths)
    };
    let path_id = source_path_id(spec.path_pattern, spec.glob);
    let path_redacted = !args.show_local_paths && !spec.glob;

    source_report(
        spec.source_id,
        spec.client,
        "jsonl",
        "jsonl_metadata",
        spec.path_pattern,
        path_id,
        path,
        path_redacted,
        exists,
        truncated,
        skipped_file_count,
        existing_files,
        summary.record_count,
        summary.parse_error_count,
        summary.byte_count,
        summary.modified_at_ms_min,
        summary.modified_at_ms_max,
        summary.observed_at_min,
        summary.observed_at_max,
        sample_files,
        Vec::new(),
        errors,
    )
}

fn sqlite_source_report(spec: SqliteSourceSpec, show_local_paths: bool) -> HistorySourceReport {
    let path_buf = expand_home(spec.path_pattern);
    let exists = path_buf.exists();
    let mut tables = Vec::new();
    let mut errors = Vec::new();
    let mut record_count = 0u64;
    let mut observed_at_min = None;
    let mut observed_at_max = None;
    let (modified_at_ms_min, modified_at_ms_max, byte_count) = if exists {
        file_metadata_summary(&path_buf)
    } else {
        (None, None, 0)
    };

    if exists {
        match Connection::open_with_flags(
            &path_buf,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            Ok(conn) => {
                for table in spec.tables {
                    let table_summary = summarize_sqlite_table(&conn, table);
                    record_count += table_summary.record_count;
                    merge_range(
                        &mut observed_at_min,
                        &mut observed_at_max,
                        table_summary.observed_at_min.clone(),
                        table_summary.observed_at_max.clone(),
                    );
                    tables.push(table_summary);
                }
            }
            Err(e) => errors.push(format!("open sqlite metadata source: {e}")),
        }
    }

    source_report(
        spec.source_id,
        spec.client,
        "sqlite",
        "sqlite_table_metadata",
        spec.path_pattern,
        source_path_id(spec.path_pattern, false),
        display_path(&path_buf, show_local_paths),
        !show_local_paths,
        exists,
        false,
        0,
        usize::from(exists),
        record_count,
        0,
        byte_count,
        modified_at_ms_min,
        modified_at_ms_max,
        observed_at_min,
        observed_at_max,
        Vec::new(),
        tables,
        errors,
    )
}

#[allow(clippy::too_many_arguments)]
fn source_report(
    source_id: &'static str,
    client: ClientProfileKind,
    kind: &'static str,
    parser: &'static str,
    path_pattern: &'static str,
    path_id: String,
    path: String,
    path_redacted: bool,
    exists: bool,
    truncated: bool,
    skipped_file_count: usize,
    file_count: usize,
    record_count: u64,
    parse_error_count: u64,
    byte_count: u64,
    modified_at_ms_min: Option<i64>,
    modified_at_ms_max: Option<i64>,
    observed_at_min: Option<String>,
    observed_at_max: Option<String>,
    sample_files: Vec<PathSample>,
    tables: Vec<SqliteTableSummary>,
    errors: Vec<String>,
) -> HistorySourceReport {
    HistorySourceReport {
        source_id,
        client: client.default_id(),
        kind,
        parser,
        path_pattern,
        path_id,
        path,
        path_redacted,
        exists,
        truncated,
        skipped_file_count,
        file_count,
        record_count,
        parse_error_count,
        byte_count,
        modified_at_ms_min,
        modified_at_ms_max,
        observed_at_min: observed_at_min.clone(),
        observed_at_max: observed_at_max.clone(),
        sample_files,
        tables,
        preview: ImportPreview {
            transport: "client_native_import",
            client: client.default_id(),
            source_id,
            record_count,
            observed_at_min,
            observed_at_max,
        },
        errors,
    }
}

#[derive(Default)]
struct JsonlSummary {
    record_count: u64,
    parse_error_count: u64,
    byte_count: u64,
    modified_at_ms_min: Option<i64>,
    modified_at_ms_max: Option<i64>,
    observed_at_min: Option<String>,
    observed_at_max: Option<String>,
}

impl JsonlSummary {
    fn add(&mut self, other: Self) {
        self.record_count += other.record_count;
        self.parse_error_count += other.parse_error_count;
        self.byte_count += other.byte_count;
        merge_i64_range(
            &mut self.modified_at_ms_min,
            &mut self.modified_at_ms_max,
            other.modified_at_ms_min,
            other.modified_at_ms_max,
        );
        merge_range(
            &mut self.observed_at_min,
            &mut self.observed_at_max,
            other.observed_at_min,
            other.observed_at_max,
        );
    }
}

impl HistoryTotals {
    fn add(&mut self, other: &Self) {
        self.source_count += other.source_count;
        self.existing_source_count += other.existing_source_count;
        self.file_count += other.file_count;
        self.record_count += other.record_count;
        self.parse_error_count += other.parse_error_count;
        self.byte_count += other.byte_count;
    }

    fn add_source(&mut self, source: &HistorySourceReport) {
        self.source_count += 1;
        if source.exists {
            self.existing_source_count += 1;
        }
        self.file_count += source.file_count;
        self.record_count += source.record_count;
        self.parse_error_count += source.parse_error_count;
        self.byte_count += source.byte_count;
    }
}

fn summarize_jsonl_file(path: &Path) -> anyhow::Result<JsonlSummary> {
    let mut summary = JsonlSummary::default();
    let (modified_min, modified_max, byte_count) = file_metadata_summary(path);
    summary.modified_at_ms_min = modified_min;
    summary.modified_at_ms_max = modified_max;
    summary.byte_count = byte_count;

    let file = File::open(path)?;
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        summary.record_count += 1;
        match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(value) => {
                if let Some(timestamp) = extract_timestamp(&value) {
                    merge_one(
                        &mut summary.observed_at_min,
                        &mut summary.observed_at_max,
                        timestamp,
                    );
                }
            }
            Err(_) => summary.parse_error_count += 1,
        }
    }
    Ok(summary)
}

fn summarize_sqlite_table(conn: &Connection, table: &SqliteTableSpec) -> SqliteTableSummary {
    let mut errors = Vec::new();
    if !sqlite_table_exists(conn, table.table) {
        return SqliteTableSummary {
            table: table.table,
            exists: false,
            record_count: 0,
            observed_at_min: None,
            observed_at_max: None,
            errors,
        };
    }

    let record_count = sqlite_count(conn, table.table).unwrap_or_else(|e| {
        errors.push(format!("count: {e}"));
        0
    });
    let (observed_at_min, observed_at_max) =
        sqlite_observed_range(conn, table).unwrap_or_else(|e| {
            errors.push(format!("timestamp_range: {e}"));
            (None, None)
        });

    SqliteTableSummary {
        table: table.table,
        exists: true,
        record_count,
        observed_at_min,
        observed_at_max,
        errors,
    }
}

fn sqlite_table_exists(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get::<_, i64>(0),
    )
    .map(|exists| exists != 0)
    .unwrap_or(false)
}

fn sqlite_count(conn: &Connection, table: &str) -> rusqlite::Result<u64> {
    let sql = format!("SELECT COUNT(*) FROM {}", quote_identifier(table));
    conn.query_row(&sql, [], |row| row.get::<_, i64>(0))
        .map(|count| count.max(0) as u64)
}

fn sqlite_observed_range(
    conn: &Connection,
    table: &SqliteTableSpec,
) -> rusqlite::Result<(Option<String>, Option<String>)> {
    let columns = sqlite_columns(conn, table.table)?;
    let Some(time_column) = table
        .time_columns
        .iter()
        .find(|column| columns.iter().any(|found| found == **column))
    else {
        return Ok((None, None));
    };
    let sql = format!(
        "SELECT MIN({col}), MAX({col}) FROM {table}",
        col = quote_identifier(time_column),
        table = quote_identifier(table.table)
    );
    conn.query_row(&sql, [], |row| {
        let min = sqlite_value_to_string(row.get_ref(0)?);
        let max = sqlite_value_to_string(row.get_ref(1)?);
        Ok((min, max))
    })
}

fn sqlite_columns(conn: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", quote_identifier(table)))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    rows.collect()
}

fn sqlite_value_to_string(value: rusqlite::types::ValueRef<'_>) -> Option<String> {
    use rusqlite::types::ValueRef;
    match value {
        ValueRef::Null => None,
        ValueRef::Integer(value) => Some(value.to_string()),
        ValueRef::Real(value) => Some(value.to_string()),
        ValueRef::Text(value) => Some(String::from_utf8_lossy(value).to_string()),
        ValueRef::Blob(_) => None,
    }
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn collect_claude_project_files() -> Vec<PathBuf> {
    let root = expand_home("${HOME}/.claude/projects");
    let mut files = Vec::new();
    collect_jsonl_files(&root, &mut files);
    files.sort();
    files
}

fn collect_jsonl_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_files(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

fn extract_timestamp(value: &serde_json::Value) -> Option<String> {
    const KEYS: &[&str] = &[
        "timestamp",
        "created_at",
        "updated_at",
        "createdAt",
        "updatedAt",
        "time",
        "ts",
        "date",
        "start_time",
        "startTime",
        "lastActivityTime",
    ];
    for key in KEYS {
        if let Some(found) = value.get(*key).and_then(timestamp_value_to_string) {
            return Some(found);
        }
    }
    None
}

fn timestamp_value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(value) if !value.trim().is_empty() => Some(value.clone()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn merge_range(
    min: &mut Option<String>,
    max: &mut Option<String>,
    other_min: Option<String>,
    other_max: Option<String>,
) {
    if let Some(value) = other_min {
        merge_one(min, max, value);
    }
    if let Some(value) = other_max {
        merge_one(min, max, value);
    }
}

fn merge_one(min: &mut Option<String>, max: &mut Option<String>, value: String) {
    if min.as_ref().map_or(true, |current| value < *current) {
        *min = Some(value.clone());
    }
    if max.as_ref().map_or(true, |current| value > *current) {
        *max = Some(value);
    }
}

fn merge_i64_range(
    min: &mut Option<i64>,
    max: &mut Option<i64>,
    other_min: Option<i64>,
    other_max: Option<i64>,
) {
    if let Some(value) = other_min {
        if min.map_or(true, |current| value < current) {
            *min = Some(value);
        }
    }
    if let Some(value) = other_max {
        if max.map_or(true, |current| value > current) {
            *max = Some(value);
        }
    }
}

fn file_metadata_summary(path: &Path) -> (Option<i64>, Option<i64>, u64) {
    let Ok(metadata) = fs::metadata(path) else {
        return (None, None, 0);
    };
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64);
    (modified, modified, metadata.len())
}

fn display_path(path: &Path, show_local_paths: bool) -> String {
    if show_local_paths {
        return path.display().to_string();
    }
    redacted_path_id(path)
}

fn redacted_path_id(path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let short = digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("path-{short}")
}

fn expand_home(path: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if path == "${HOME}" || path == "~" {
        return home.unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("${HOME}/") {
        if let Some(home) = home {
            return home.join(rest);
        }
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

impl NativeHistoryImportReport {
    fn to_store_batch(&self, import_id: &str) -> anyhow::Result<NativeHistoryImportBatch> {
        let warnings_json = serde_json::to_string(&self.warnings)?;
        let import = NativeHistoryImportRecord {
            import_id: import_id.to_string(),
            client_filter: client_filter_id(&self.clients),
            metadata_only: self.content_policy.metadata_only,
            stores_prompts: self.content_policy.stores_prompts,
            stores_responses: self.content_policy.stores_responses,
            stores_local_paths: self.content_policy.stores_local_paths,
            source_count: self.totals.source_count as u64,
            existing_source_count: self.totals.existing_source_count as u64,
            file_count: self.totals.file_count as u64,
            record_count: self.totals.record_count,
            parse_error_count: self.totals.parse_error_count,
            byte_count: self.totals.byte_count,
            warnings_json,
            created_at_ms: now_millis(),
        };
        let mut sources = Vec::new();
        for client in &self.clients {
            for source in &client.sources {
                sources.push(source.to_store_record(import_id)?);
            }
        }
        Ok(NativeHistoryImportBatch { import, sources })
    }
}

impl HistorySourceReport {
    fn to_store_record(&self, import_id: &str) -> anyhow::Result<NativeHistorySourceRecord> {
        Ok(NativeHistorySourceRecord {
            import_id: import_id.to_string(),
            source_id: self.source_id.to_string(),
            client: self.client.to_string(),
            kind: self.kind.to_string(),
            parser: self.parser.to_string(),
            path_pattern: self.path_pattern.to_string(),
            path_id: self.path_id.clone(),
            exists: self.exists,
            truncated: self.truncated,
            skipped_file_count: self.skipped_file_count as u64,
            file_count: self.file_count as u64,
            record_count: self.record_count,
            parse_error_count: self.parse_error_count,
            byte_count: self.byte_count,
            modified_at_ms_min: self.modified_at_ms_min,
            modified_at_ms_max: self.modified_at_ms_max,
            observed_at_min: self.observed_at_min.clone(),
            observed_at_max: self.observed_at_max.clone(),
            tables_json: serde_json::to_string(&self.tables)?,
            errors_json: serde_json::to_string(&self.errors)?,
        })
    }
}

fn resolve_state_store_path(
    args: &NativeImportHistoryArgs,
    config_path: &Path,
) -> anyhow::Result<PathBuf> {
    if let Some(path) = &args.state_store {
        return Ok(expand_home_path(path));
    }
    let cfg = Config::from_path(config_path)?;
    let Some(state_store) = cfg.server.state_store.as_ref() else {
        anyhow::bail!(
            "native import-history --apply requires --state-store or server.state_store in --config"
        );
    };
    Ok(expand_home(state_store.path()))
}

fn expand_home_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    expand_home(&text)
}

fn client_filter_id(clients: &[ClientHistoryReport]) -> String {
    if clients.len() == 2
        && clients.iter().any(|client| client.id == "codex")
        && clients.iter().any(|client| client.id == "claude-code")
    {
        "all".to_string()
    } else {
        clients
            .iter()
            .map(|client| client.id)
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn new_import_id(report: &NativeHistoryImportReport) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let seed = format!(
        "{}:{}:{}:{}:{}",
        nanos,
        std::process::id(),
        report.totals.source_count,
        report.totals.file_count,
        report.totals.record_count
    );
    let mut hasher = Sha256::new();
    hasher.update(seed.as_bytes());
    let digest = hasher.finalize();
    let short = digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("native-import-{nanos}-{short}")
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn source_path_id(path_pattern: &str, glob: bool) -> String {
    if glob {
        redacted_path_id(Path::new(path_pattern))
    } else {
        redacted_path_id(&expand_home(path_pattern))
    }
}

fn native_client_kinds(target: NativeClientTarget) -> Vec<ClientProfileKind> {
    match target {
        NativeClientTarget::All => vec![ClientProfileKind::Codex, ClientProfileKind::ClaudeCode],
        NativeClientTarget::Codex => vec![ClientProfileKind::Codex],
        NativeClientTarget::ClaudeCode => vec![ClientProfileKind::ClaudeCode],
    }
}
