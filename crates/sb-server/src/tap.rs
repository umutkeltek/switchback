//! Transparent tap (Mode B). A passthrough listener that forwards a client's
//! request VERBATIM to a fixed upstream — its own `Authorization`, its own
//! headers, its raw body — streams the response back unchanged, and only
//! observes (records a metadata trace; optionally the full bodies to a separate
//! local file). No canonical-IR round-trip, no credential lease: the vendor sees
//! the native client's request, so there is nothing re-shaped to flag.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc::{sync_channel, RecvTimeoutError, SyncSender},
    Arc, Mutex, RwLock,
};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::{to_bytes, Body, Bytes};
use axum::extract::ws::{
    rejection::WebSocketUpgradeRejection, CloseFrame as AxumCloseFrame, Message as AxumWsMessage,
    WebSocket, WebSocketUpgrade,
};
use axum::extract::DefaultBodyLimit;
use axum::extract::State;
use axum::http::{
    header::{CONTENT_LENGTH, CONTENT_TYPE},
    HeaderMap, Method, StatusCode, Uri,
};
use axum::response::{IntoResponse, Response};
use axum::Router;
use base64::Engine as _;
use futures::{SinkExt, Stream, StreamExt};
use sb_bodylog::{
    BodyCaptureGap, BodyEventInput, BodyLogger, CaptureMode, CaptureStage, PressureStatus,
};
use sb_core::{RouteDecision, TapConfig};
use sb_paths::RuntimePaths;
use sb_trace::{Attempt, NativeExecutionObservation, RequestTrace, TraceLog};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest,
    protocol::{frame::coding::CloseCode, CloseFrame as TungsteniteCloseFrame},
    Message as TungsteniteMessage,
};

/// Hop-by-hop headers the proxy must not copy; the HTTP client manages framing.
const HOP_BY_HOP: &[&str] = &[
    "host",
    "content-length",
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
];

const TAP_METADATA_BODY_BYTES: usize = 1024 * 1024;
const TAP_SSE_TERMINAL_WINDOW_BYTES: usize = 8192;
const TAP_CAPTURE_QUEUE_CAPACITY: usize = 256;
const TAP_CAPTURE_QUEUE_MAX_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const TAP_CAPTURE_BODY_MAX_BYTES: usize = 16 * 1024 * 1024;
const TAP_WEBSOCKET_CAPTURE_CHUNK_BYTES: usize = 4 * 1024 * 1024;
const TAP_CAPTURE_RETRY_WARNING_INTERVAL: Duration = Duration::from_secs(30);
const TAP_CAPTURE_PERSISTENCE_FAILURES_TO_DEGRADE: u64 = 3;
const TAP_CAPTURE_PRESSURE_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const TAP_CAPTURE_PRESSURE_FAILURES_TO_DEGRADE: u32 = 3;
const TAP_CAPTURE_IDLE_SEAL_INTERVAL: Duration = Duration::from_secs(30);
const TAP_CAPTURE_MAINTENANCE_TICK: Duration = Duration::from_secs(1);
const LANE_ID_HEADER: &str = "x-switchback-lane-id";
const LANE_REVISION_HEADER: &str = "x-switchback-lane-revision";
const REQUESTED_EFFORT_HEADER: &str = "x-switchback-requested-effort";
const LAUNCH_PROFILE_HEADER: &str = "x-switchback-launch-profile";
const CONFORMANCE_REVISION_HEADER: &str = "x-switchback-conformance-revision";
const HARNESS_HEADER: &str = "x-switchback-harness";
const CAPTURE_POLICY_HEADER: &str = "x-switchback-capture-policy";
const PROFILE_CONFORMANCE_SCHEMA: &str = "switchback/profile-conformance@1";
const PROFILE_AUTHORITY_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const PROFILE_PROJECTION_MAX_BYTES: u64 = 1024 * 1024;

fn is_hop_by_hop(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    HOP_BY_HOP.contains(&lower.as_str())
}

fn is_auth_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "x-api-key" | "api-key" | "x-goog-api-key"
    )
}

pub(crate) fn is_execution_observation_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        LANE_ID_HEADER
            | LANE_REVISION_HEADER
            | REQUESTED_EFFORT_HEADER
            | LAUNCH_PROFILE_HEADER
            | CONFORMANCE_REVISION_HEADER
            | HARNESS_HEADER
            | CAPTURE_POLICY_HEADER
    )
}

fn bounded_token(value: &str, max_len: usize) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > max_len
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/'))
    {
        return None;
    }
    Some(value.to_string())
}

fn lane_revision(value: &str) -> Option<String> {
    let digest = value.trim().strip_prefix("sha256:")?;
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("sha256:{}", digest.to_ascii_lowercase()))
}

fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name)?.to_str().ok().map(str::to_string)
}

#[derive(Debug, Clone)]
pub(crate) struct TapCaptureContext {
    launch_profile: Option<String>,
    launch_capture_policy: Option<String>,
    claimed_capture_policy: Option<String>,
    capture_authority_available: bool,
}

impl Default for TapCaptureContext {
    fn default() -> Self {
        Self::from_values(None, None)
    }
}

impl TapCaptureContext {
    fn from_headers_with_authority(
        headers: &HeaderMap,
        authority: &CaptureProfileAuthority,
    ) -> Self {
        let launch_profile = header_string(headers, LAUNCH_PROFILE_HEADER);
        let claimed_capture_policy = header_string(headers, CAPTURE_POLICY_HEADER);
        let mut context =
            Self::from_values(launch_profile.as_deref(), claimed_capture_policy.as_deref());
        if !authority.is_available() {
            context.launch_capture_policy = Some("metadata_only".to_string());
            context.capture_authority_available = false;
            return context;
        }
        let revision = header_string(headers, CONFORMANCE_REVISION_HEADER)
            .and_then(|value| lane_revision(&value));
        context.launch_capture_policy = context
            .launch_profile
            .as_deref()
            .zip(revision.as_deref())
            .and_then(|(profile, revision)| authority.resolve(profile, revision));
        context
    }

    pub(crate) fn from_header_pairs_with_authority(
        headers: &[(String, String)],
        authority: &CaptureProfileAuthority,
    ) -> Self {
        let value = |wanted: &str| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(wanted))
                .map(|(_, value)| value.as_str())
        };
        let mut context =
            Self::from_values(value(LAUNCH_PROFILE_HEADER), value(CAPTURE_POLICY_HEADER));
        if !authority.is_available() {
            context.launch_capture_policy = Some("metadata_only".to_string());
            context.capture_authority_available = false;
            return context;
        }
        let revision = value(CONFORMANCE_REVISION_HEADER).and_then(lane_revision);
        context.launch_capture_policy = context
            .launch_profile
            .as_deref()
            .zip(revision.as_deref())
            .and_then(|(profile, revision)| authority.resolve(profile, revision));
        context
    }

    fn from_values(launch_profile: Option<&str>, claimed_capture_policy: Option<&str>) -> Self {
        let launch_profile = launch_profile.and_then(|value| bounded_token(value, 128));
        let claimed_capture_policy = claimed_capture_policy.and_then(valid_capture_policy);
        Self {
            launch_profile,
            launch_capture_policy: None,
            claimed_capture_policy,
            capture_authority_available: true,
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.launch_capture_policy.as_deref() != Some("off")
    }

    pub(crate) fn captures_payload(&self) -> bool {
        !matches!(
            self.launch_capture_policy.as_deref(),
            Some("metadata_only" | "off")
        )
    }

    pub(crate) fn effective_capture_policy(&self) -> Option<&str> {
        self.launch_capture_policy.as_deref()
    }

    pub(crate) fn merge_metadata(&self, metadata: serde_json::Value) -> serde_json::Value {
        let mut object = match metadata {
            serde_json::Value::Object(object) => object,
            other => {
                let mut object = serde_json::Map::new();
                object.insert("metadata".to_string(), other);
                object
            }
        };
        if let Some(profile) = self.launch_profile.as_deref() {
            object.insert(
                "launch_profile".to_string(),
                serde_json::Value::String(profile.to_string()),
            );
        }
        if let Some(policy) = self.launch_capture_policy.as_deref() {
            object.insert(
                "launch_capture_policy".to_string(),
                serde_json::Value::String(policy.to_string()),
            );
        }
        if let Some(policy) = self.claimed_capture_policy.as_deref() {
            object.insert(
                "launch_capture_policy_claimed".to_string(),
                serde_json::Value::String(policy.to_string()),
            );
        }
        if !self.capture_authority_available {
            object.insert(
                "capture_authority".to_string(),
                serde_json::Value::String("unavailable".to_string()),
            );
        }
        serde_json::Value::Object(object)
    }
}

fn valid_capture_policy(value: &str) -> Option<String> {
    match value.trim() {
        "segmented_full_wire" => Some("segmented_full_wire".to_string()),
        "metadata_only" => Some("metadata_only".to_string()),
        "off" => Some("off".to_string()),
        _ => None,
    }
}

#[derive(Debug, Clone)]
struct AuthorizedCaptureProfile {
    revision: String,
    capture_policy: String,
}

#[derive(Debug, Clone)]
pub(crate) struct CaptureProfileAuthority {
    profiles: Arc<RwLock<HashMap<String, AuthorizedCaptureProfile>>>,
    available: Arc<std::sync::atomic::AtomicBool>,
}

impl CaptureProfileAuthority {
    pub(crate) fn load_live_default() -> Self {
        let root = std::env::var_os("SB_PROFILE_PROJECTION_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| RuntimePaths::from_env().profile_projection_root());
        let (profiles, available) = match load_capture_profiles(&root) {
            Ok(profiles) => (profiles, true),
            Err(err) => {
                tracing::warn!(
                    path = %root.display(),
                    error = %err,
                    "Switchback profile conformance authority unavailable; body capture degrades to metadata-only"
                );
                (HashMap::new(), false)
            }
        };
        let authority = Self {
            profiles: Arc::new(RwLock::new(profiles)),
            available: Arc::new(std::sync::atomic::AtomicBool::new(available)),
        };
        let weak = Arc::downgrade(&authority.profiles);
        let availability = Arc::downgrade(&authority.available);
        std::thread::Builder::new()
            .name("switchback-profile-authority".to_string())
            .spawn(move || loop {
                std::thread::sleep(PROFILE_AUTHORITY_REFRESH_INTERVAL);
                let Some(profiles) = weak.upgrade() else {
                    break;
                };
                let Some(available) = availability.upgrade() else {
                    break;
                };
                match load_capture_profiles(&root) {
                    Ok(refreshed) => {
                        if let Ok(mut current) = profiles.write() {
                            *current = refreshed;
                            available.store(true, Ordering::Release);
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            path = %root.display(),
                            error = %err,
                            "Switchback profile conformance refresh failed; retaining last known authority"
                        );
                    }
                }
            })
            .ok();
        authority
    }

    #[cfg(test)]
    pub(crate) fn from_entries<'a>(
        entries: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>,
    ) -> Result<Self, String> {
        let mut profiles = HashMap::new();
        for (id, revision, capture_policy) in entries {
            let id =
                bounded_token(id, 128).ok_or_else(|| format!("invalid launch profile `{id}`"))?;
            let revision =
                lane_revision(revision).ok_or_else(|| format!("invalid revision `{revision}`"))?;
            let capture_policy = valid_capture_policy(capture_policy)
                .ok_or_else(|| format!("invalid capture policy `{capture_policy}`"))?;
            profiles.insert(
                id,
                AuthorizedCaptureProfile {
                    revision,
                    capture_policy,
                },
            );
        }
        Ok(Self {
            profiles: Arc::new(RwLock::new(profiles)),
            available: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        })
    }

    fn is_available(&self) -> bool {
        self.available.load(Ordering::Acquire)
    }

    fn resolve(&self, profile: &str, revision: &str) -> Option<String> {
        let profiles = self.profiles.read().ok()?;
        let configured = profiles.get(profile)?;
        (configured.revision == revision).then(|| configured.capture_policy.clone())
    }
}

#[derive(Debug, Deserialize)]
struct CaptureProfileProjection {
    schema: String,
    authority: CaptureProfileProjectionAuthority,
    launch_profile_ref: String,
    conformance_revision: String,
    profile: CaptureProfileProjectionBody,
}

#[derive(Debug, Deserialize)]
struct CaptureProfileProjectionAuthority {
    owner: String,
    compound_role: String,
    projection: String,
}

#[derive(Debug, Deserialize)]
struct CaptureProfileProjectionBody {
    id: String,
    capture_policy: CaptureProfileProjectionPolicy,
}

#[derive(Debug, Deserialize)]
struct CaptureProfileProjectionPolicy {
    mode: String,
}

fn load_capture_profiles(
    root: &std::path::Path,
) -> Result<HashMap<String, AuthorizedCaptureProfile>, String> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(err) => return Err(err.to_string()),
    };
    let mut profiles = HashMap::new();
    for entry in entries {
        let entry = entry.map_err(|err| err.to_string())?;
        let file_type = entry.file_type().map_err(|err| err.to_string())?;
        if !file_type.is_file()
            || entry.path().extension().and_then(|value| value.to_str()) != Some("json")
        {
            continue;
        }
        let metadata = entry.metadata().map_err(|err| err.to_string())?;
        if metadata.len() > PROFILE_PROJECTION_MAX_BYTES {
            return Err(format!(
                "profile projection exceeds {} bytes: {}",
                PROFILE_PROJECTION_MAX_BYTES,
                entry.path().display()
            ));
        }
        let projection: CaptureProfileProjection =
            serde_json::from_slice(&fs::read(entry.path()).map_err(|err| err.to_string())?)
                .map_err(|err| format!("parse {}: {err}", entry.path().display()))?;
        if projection.schema != PROFILE_CONFORMANCE_SCHEMA
            || projection.authority.owner != "switchback"
            || projection.authority.compound_role != "consumer"
            || projection.authority.projection != "non_secret_profile_conformance"
        {
            return Err(format!(
                "invalid Switchback authority projection: {}",
                entry.path().display()
            ));
        }
        let id = bounded_token(&projection.profile.id, 128)
            .ok_or_else(|| format!("invalid profile id in {}", entry.path().display()))?;
        let expected_ref = format!("switchback://launch-profiles/{id}");
        if projection.launch_profile_ref != expected_ref
            || entry.path().file_stem().and_then(|value| value.to_str()) != Some(id.as_str())
        {
            return Err(format!(
                "profile identity mismatch in {}",
                entry.path().display()
            ));
        }
        let revision = lane_revision(&projection.conformance_revision)
            .ok_or_else(|| format!("invalid profile revision in {}", entry.path().display()))?;
        let capture_policy = valid_capture_policy(&projection.profile.capture_policy.mode)
            .ok_or_else(|| format!("invalid capture policy in {}", entry.path().display()))?;
        profiles.insert(
            id,
            AuthorizedCaptureProfile {
                revision,
                capture_policy,
            },
        );
    }
    Ok(profiles)
}

fn observed_effort(value: &serde_json::Value) -> Option<(String, String)> {
    const POINTERS: &[&str] = &[
        "/reasoning/effort",
        "/output_config/effort",
        "/reasoning_effort",
        "/effort",
        "/response/reasoning/effort",
        "/response/output_config/effort",
        "/response/reasoning_effort",
        "/response/effort",
    ];
    POINTERS.iter().find_map(|path| {
        let effort = value.pointer(path)?.as_str()?;
        bounded_token(effort, 32).map(|effort| (effort, (*path).to_string()))
    })
}

fn observed_model(value: &serde_json::Value) -> Option<String> {
    const POINTERS: &[&str] = &["/model", "/response/model", "/session/model"];
    POINTERS.iter().find_map(|path| {
        value
            .pointer(path)?
            .as_str()
            .and_then(|model| bounded_token(model, 256))
    })
}

fn native_execution_observation(
    headers: &HeaderMap,
    body: Option<&serde_json::Value>,
) -> Option<NativeExecutionObservation> {
    let observed = body.and_then(observed_effort);
    let observation = NativeExecutionObservation {
        lane_id: header_string(headers, LANE_ID_HEADER)
            .and_then(|value| bounded_token(&value, 128)),
        lane_revision: header_string(headers, LANE_REVISION_HEADER)
            .and_then(|value| lane_revision(&value)),
        launch_profile: header_string(headers, LAUNCH_PROFILE_HEADER)
            .and_then(|value| bounded_token(&value, 128)),
        conformance_revision: header_string(headers, CONFORMANCE_REVISION_HEADER)
            .and_then(|value| lane_revision(&value)),
        harness: header_string(headers, HARNESS_HEADER).and_then(|value| bounded_token(&value, 64)),
        requested_effort: header_string(headers, REQUESTED_EFFORT_HEADER)
            .and_then(|value| bounded_token(&value, 32)),
        observed_effort: observed.as_ref().map(|(effort, _)| effort.clone()),
        observed_effort_path: observed.map(|(_, path)| path),
    };
    (!observation.is_empty()).then_some(observation)
}

#[derive(Debug, Clone, Default)]
struct TapWebSocketObservation {
    native_execution: NativeExecutionObservation,
    inbound_model: Option<String>,
}

fn observe_websocket_request_frame(observation: &mut TapWebSocketObservation, text: &str) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    if observation.native_execution.observed_effort.is_none() {
        if let Some((effort, path)) = observed_effort(&value) {
            observation.native_execution.observed_effort = Some(effort);
            observation.native_execution.observed_effort_path = Some(path);
        }
    }
    if observation.inbound_model.is_none() {
        observation.inbound_model = observed_model(&value);
    }
}

/// One bounded blocking worker per tap keeps body persistence off Tokio's
/// executor without creating an unbounded task/thread per captured event.
///
/// The queue is deliberately lossless: once a body is accepted, transient
/// persistence failures are retried and a full queue backpressures producers
/// instead of discarding evidence. WebSocket frames are chunked before they
/// reach this queue, so normal streaming does not pay one job/SQLite row/file
/// transaction per wire frame.
#[derive(Clone)]
pub(crate) struct CaptureWorker {
    sender: SyncSender<CaptureJob>,
    budget: CaptureBudget,
    fallback_logger: BodyLogger,
    pressure_checks: bool,
}

enum CaptureJob {
    FullWire {
        input: BodyEventInput,
        _budget: CaptureBudgetPermit,
        _queue: CaptureQueuePermit,
    },
    MetadataOnly {
        input: BodyEventInput,
        pressure: Box<PressureStatus>,
        gap: Option<BodyCaptureGap>,
        _queue: CaptureQueuePermit,
    },
}

#[derive(Clone)]
struct CaptureBudget {
    queued_bytes: Arc<AtomicUsize>,
    max_bytes: usize,
}

impl CaptureBudget {
    fn new(max_bytes: usize) -> Self {
        Self {
            queued_bytes: Arc::new(AtomicUsize::new(0)),
            max_bytes,
        }
    }

    fn try_reserve(&self, bytes: usize) -> Option<CaptureBudgetPermit> {
        if bytes > self.max_bytes {
            return None;
        }
        let mut queued = self.queued_bytes.load(Ordering::Relaxed);
        loop {
            let next = queued.checked_add(bytes)?;
            if next > self.max_bytes {
                return None;
            }
            match self.queued_bytes.compare_exchange_weak(
                queued,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Some(CaptureBudgetPermit {
                        queued_bytes: Arc::clone(&self.queued_bytes),
                        bytes,
                    });
                }
                Err(actual) => queued = actual,
            }
        }
    }

    fn reserve(&self, bytes: usize) -> CaptureBudgetPermit {
        loop {
            if let Some(permit) = self.try_reserve(bytes) {
                return permit;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[cfg(test)]
    fn queued_bytes(&self) -> usize {
        self.queued_bytes.load(Ordering::Acquire)
    }
}

struct CaptureBudgetPermit {
    queued_bytes: Arc<AtomicUsize>,
    bytes: usize,
}

impl Drop for CaptureBudgetPermit {
    fn drop(&mut self) {
        self.queued_bytes.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

struct CaptureQueuePermit {
    logger: BodyLogger,
}

impl CaptureQueuePermit {
    fn new(logger: &BodyLogger) -> Self {
        if let Err(err) = logger.note_capture_queue_enqueued() {
            tracing::warn!(error = %err, "capture queue metrics enqueue failed");
        }
        Self {
            logger: logger.clone(),
        }
    }
}

impl Drop for CaptureQueuePermit {
    fn drop(&mut self) {
        if let Err(err) = self.logger.note_capture_queue_dequeued() {
            tracing::warn!(error = %err, "capture queue metrics dequeue failed");
        }
    }
}

impl CaptureWorker {
    pub(crate) fn new(logger: BodyLogger) -> std::io::Result<Self> {
        Self::new_inner(logger, !cfg!(test))
    }

    #[cfg(test)]
    fn new_pressure_checked(logger: BodyLogger) -> std::io::Result<Self> {
        Self::new_inner(logger, true)
    }

    fn new_inner(logger: BodyLogger, pressure_checks: bool) -> std::io::Result<Self> {
        let (sender, receiver) = sync_channel::<CaptureJob>(TAP_CAPTURE_QUEUE_CAPACITY);
        let fallback_logger = logger.clone();
        if pressure_checks && !cfg!(test) {
            let mut failures = 0;
            refresh_capture_pressure(&logger, &mut failures);
        }
        std::thread::Builder::new()
            .name("switchback-tap-capture".to_string())
            .spawn(move || {
                let mut last_pressure_refresh = Instant::now();
                let mut pressure_refresh_failures = 0;
                loop {
                    match receiver.recv_timeout(TAP_CAPTURE_MAINTENANCE_TICK) {
                        Ok(CaptureJob::FullWire { input, .. }) => {
                            persist_capture_job(&logger, input)
                        }
                        Ok(CaptureJob::MetadataOnly {
                            input,
                            pressure,
                            gap,
                            ..
                        }) => persist_metadata_capture_job(&logger, input, &pressure, gap),
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => {
                            if let Err(err) = logger.seal_active() {
                                tracing::warn!(error = %err, "capture segment shutdown seal failed");
                            }
                            break;
                        }
                    }
                    if pressure_checks
                        && last_pressure_refresh.elapsed()
                            >= TAP_CAPTURE_PRESSURE_REFRESH_INTERVAL
                    {
                        refresh_capture_pressure(&logger, &mut pressure_refresh_failures);
                        last_pressure_refresh = Instant::now();
                    }
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as i64;
                    if let Err(err) = logger.seal_idle_at(
                        now,
                        TAP_CAPTURE_IDLE_SEAL_INTERVAL.as_millis() as i64,
                    ) {
                        tracing::warn!(error = %err, "capture segment idle seal failed");
                    }
                }
            })?;
        Ok(Self {
            sender,
            budget: CaptureBudget::new(TAP_CAPTURE_QUEUE_MAX_BYTES),
            fallback_logger,
            pressure_checks,
        })
    }

    pub(crate) fn submit(&self, input: BodyEventInput) {
        let pressure = if self.pressure_checks {
            Some(match self.fallback_logger.pressure_status() {
                Ok(status) => status,
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "capture pressure cache unavailable; degrading to metadata-only"
                    );
                    if let Err(mark_err) = self
                        .fallback_logger
                        .mark_capture_writer_failed("pressure_observation")
                    {
                        tracing::warn!(
                            error = %mark_err,
                            "capture pressure state persistence failed"
                        );
                    }
                    match self.fallback_logger.pressure_status() {
                        Ok(status) => status,
                        Err(status_err) => {
                            tracing::warn!(
                                error = %status_err,
                                "capture pressure status unavailable; capture skipped"
                            );
                            return;
                        }
                    }
                }
            })
        } else {
            None
        };

        if let Some(pressure) = pressure.filter(|status| status.mode == CaptureMode::MetadataOnly) {
            self.submit_metadata_only(input, pressure);
            return;
        }

        if input.body.len() > TAP_CAPTURE_BODY_MAX_BYTES {
            let gap = BodyCaptureGap {
                reason: "body_limit_exceeded".to_string(),
                body_sha256: format!("{:x}", Sha256::digest(&input.body)),
                body_bytes: input.body.len() as u64,
            };
            let mut input = input;
            input.body = Vec::new();
            self.submit_gap(input, gap);
            return;
        }
        let body_bytes = input.body.len();
        let budget = self.budget.reserve(body_bytes);
        let job = CaptureJob::FullWire {
            input,
            _budget: budget,
            _queue: CaptureQueuePermit::new(&self.fallback_logger),
        };
        if let Err(disconnected) = self.sender.send(job) {
            // A worker disconnect is exceptional, but it must not turn into an
            // evidence hole. Persist synchronously on the caller as the
            // fail-closed fallback; this can slow the request but cannot drop
            // the already-accepted body.
            if let CaptureJob::FullWire { input, .. } = disconnected.0 {
                persist_capture_job(&self.fallback_logger, input);
            }
        }
    }

    fn submit_metadata_only(&self, input: BodyEventInput, pressure: PressureStatus) {
        let job = CaptureJob::MetadataOnly {
            input,
            pressure: Box::new(pressure),
            gap: None,
            _queue: CaptureQueuePermit::new(&self.fallback_logger),
        };
        // Fail-closed, never drop: metadata-only events are the evidence of a
        // degraded window — skipping them loses exactly what healing needs.
        // Blocking briefly under a burst is the same trade the FullWire path
        // already makes. Observed live 2026-08-07: 12k+ events dropped here
        // while the heal deadlocked in metadata-only.
        if let Err(disconnected) = self.sender.send(job) {
            if let CaptureJob::MetadataOnly { input, .. } = disconnected.0 {
                persist_capture_job(&self.fallback_logger, input);
            }
        }
    }

    fn submit_gap(&self, input: BodyEventInput, gap: BodyCaptureGap) {
        let mut pressure = match self.fallback_logger.pressure_status() {
            Ok(status) => status,
            Err(err) => {
                tracing::warn!(error = %err, "capture pressure cache unavailable; gap skipped");
                return;
            }
        };
        pressure.mode = CaptureMode::MetadataOnly;
        pressure.reasons.push(format!("capture_gap:{}", gap.reason));
        let job = CaptureJob::MetadataOnly {
            input,
            pressure: Box::new(pressure),
            gap: Some(gap),
            _queue: CaptureQueuePermit::new(&self.fallback_logger),
        };
        // Fail-closed: a gap is evidence too; never skip it (same rationale as
        // submit_metadata_only — degraded windows are exactly when evidence is
        // scarcest, and drops there compounded a 12k-event hole on 2026-08-07).
        if let Err(disconnected) = self.sender.send(job) {
            if let CaptureJob::MetadataOnly { input, .. } = disconnected.0 {
                persist_capture_job(&self.fallback_logger, input);
            }
        }
    }

    pub(crate) fn submit_payload(&self, mut input: BodyEventInput, payload: CapturePayload) {
        match payload {
            CapturePayload::Full(body) => {
                input.body = body;
                self.submit(input);
            }
            CapturePayload::Gap(gap) => {
                input.body.clear();
                self.submit_gap(input, gap);
            }
        }
    }

    pub(crate) fn submit_authorized_payload(
        &self,
        mut input: BodyEventInput,
        payload: CapturePayload,
        effective_policy: Option<&str>,
    ) {
        match effective_policy {
            Some("off") => {}
            Some("metadata_only") => {
                let gap = match payload {
                    CapturePayload::Full(body) => BodyCaptureGap {
                        reason: "launch_profile_policy".to_string(),
                        body_sha256: format!("{:x}", Sha256::digest(&body)),
                        body_bytes: body.len() as u64,
                    },
                    CapturePayload::Gap(gap) => gap,
                };
                input.body.clear();
                self.submit_gap(input, gap);
            }
            Some("segmented_full_wire") | None => self.submit_payload(input, payload),
            Some(other) => {
                tracing::warn!(
                    policy = other,
                    "invalid trusted capture policy; defaulting to full-wire"
                );
                self.submit_payload(input, payload);
            }
        }
    }
}

fn refresh_capture_pressure(logger: &BodyLogger, consecutive_failures: &mut u32) {
    match logger.evaluate_pressure() {
        Ok(_) => *consecutive_failures = 0,
        Err(err) => {
            *consecutive_failures = consecutive_failures.saturating_add(1);
            // Pressure observation is a read/projection refresh, not a body-writer
            // mutation. A transient ENOENT can occur while the backup adapter seals,
            // projects, or reclaims segments. Treating the first read miss as a
            // writer failure permanently moved every tap to MetadataOnly, reset
            // healing to zero, and deadlocked recovery behind unbacked bytes.
            //
            // Retain the last successful admission state for two refreshes (at the
            // 30s cadence), but fail closed after 3 consecutive failures: a genuine
            // inability to observe pressure must not permit unbounded writes. Actual
            // full-wire persistence errors still fail closed immediately below.
            if *consecutive_failures < TAP_CAPTURE_PRESSURE_FAILURES_TO_DEGRADE {
                tracing::warn!(
                    error = %err,
                    consecutive_failures = *consecutive_failures,
                    "background capture pressure observation failed; retaining last known capture mode and retrying"
                );
                return;
            }
            tracing::warn!(
                error = %err,
                consecutive_failures = *consecutive_failures,
                "capture pressure observation repeatedly failed; degrading to metadata-only"
            );
            if let Err(mark_err) = logger.mark_capture_writer_failed("pressure_observation") {
                tracing::warn!(
                    error = %mark_err,
                    "capture pressure state persistence failed"
                );
            }
        }
    }
}

fn persist_metadata_capture_job(
    logger: &BodyLogger,
    input: BodyEventInput,
    pressure: &PressureStatus,
    gap: Option<BodyCaptureGap>,
) {
    let result = match gap {
        Some(gap) => logger.record_capture_gap(input, pressure, gap),
        None => logger.record_metadata_only(input, pressure),
    };
    if let Err(err) = result {
        tracing::warn!(
            error = %err,
            "metadata-only capture-gap persistence failed; not retrying"
        );
    }
}

#[derive(Debug)]
pub(crate) enum CapturePayload {
    Full(Vec<u8>),
    Gap(BodyCaptureGap),
}

pub(crate) struct CaptureAccumulator {
    limit: usize,
    body: Option<Vec<u8>>,
    hasher: Sha256,
    body_bytes: u64,
}

impl CaptureAccumulator {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit,
            body: Some(Vec::new()),
            hasher: Sha256::new(),
            body_bytes: 0,
        }
    }

    pub(crate) fn observe(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
        self.body_bytes = self.body_bytes.saturating_add(bytes.len() as u64);
        let exceeds_limit = self
            .body
            .as_ref()
            .is_some_and(|body| body.len().saturating_add(bytes.len()) > self.limit);
        if exceeds_limit {
            self.body = None;
        } else if let Some(body) = self.body.as_mut() {
            body.extend_from_slice(bytes);
        }
    }

    pub(crate) fn finish(self) -> CapturePayload {
        match self.body {
            Some(body) => CapturePayload::Full(body),
            None => CapturePayload::Gap(BodyCaptureGap {
                reason: "body_limit_exceeded".to_string(),
                body_sha256: format!("{:x}", self.hasher.finalize()),
                body_bytes: self.body_bytes,
            }),
        }
    }
}

struct TapRequestCaptureStream<S> {
    inner: S,
    capture: SharedCaptureAccumulator,
}

impl<S, E> Stream for TapRequestCaptureStream<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    type Item = Result<Bytes, E>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let item = Pin::new(&mut self.inner).poll_next(cx);
        if let Poll::Ready(Some(Ok(bytes))) = &item {
            if let Ok(mut capture) = self.capture.lock() {
                if let Some(capture) = capture.as_mut() {
                    capture.observe(bytes);
                }
            }
        }
        item
    }
}

fn finish_shared_capture(capture: &SharedCaptureAccumulator) -> Option<CapturePayload> {
    capture
        .lock()
        .ok()
        .and_then(|mut capture| capture.take())
        .map(CaptureAccumulator::finish)
}

fn persist_capture_job(logger: &BodyLogger, input: BodyEventInput) {
    let request_id = input.request_id.clone();
    let stage = input.capture_stage;
    let mut attempts = 0u64;
    let mut last_warning: Option<Instant> = None;
    loop {
        attempts += 1;
        match logger.record(input.clone()) {
            Ok(_) => return,
            Err(err) => {
                // SQLite lock contention is expected when backup/projection work
                // briefly overlaps capture. The accepted body remains in this
                // retry loop, so the first failed attempt is not evidence that
                // full-wire persistence is broken. Preserve admission through
                // two retries; fail closed on the third consecutive failure.
                if attempts == TAP_CAPTURE_PERSISTENCE_FAILURES_TO_DEGRADE {
                    if let Err(mark_err) =
                        logger.mark_capture_writer_failed("full_wire_persistence")
                    {
                        tracing::warn!(
                            error = %mark_err,
                            "capture writer failure could not persist degraded state"
                        );
                    }
                }
                let should_warn = attempts == 1
                    || last_warning.map_or(true, |last| {
                        last.elapsed() >= TAP_CAPTURE_RETRY_WARNING_INTERVAL
                    });
                if should_warn {
                    tracing::warn!(
                        %request_id,
                        ?stage,
                        attempts,
                        error = %err,
                        "tap body capture persistence blocked; retrying without dropping the body"
                    );
                    last_warning = Some(Instant::now());
                }
                let delay_ms = (attempts.saturating_mul(25)).min(1_000);
                std::thread::sleep(Duration::from_millis(delay_ms));
            }
        }
    }
}

#[derive(Clone)]
struct TapState {
    id: String,
    upstream: String,
    upstream_host: String,
    headers: Vec<(String, String)>,
    capture_authority: CaptureProfileAuthority,
    capture_worker: Option<CaptureWorker>,
    traces: Arc<TraceLog>,
    client: reqwest::Client,
}

/// Build the axum app for one tap listener. Every request, any method/path, is
/// forwarded to `tap.upstream`. `capture_sink` is the compatibility event log;
/// body bytes go through `sb-bodylog` when `capture_bodies` is enabled.
pub(crate) fn build_tap_app(
    tap: &TapConfig,
    traces: Arc<TraceLog>,
    capture_sink: Option<PathBuf>,
) -> Router {
    build_tap_app_with_capture_authority(
        tap,
        traces,
        capture_sink,
        CaptureProfileAuthority::load_live_default(),
    )
}

fn build_tap_app_with_capture_authority(
    tap: &TapConfig,
    traces: Arc<TraceLog>,
    capture_sink: Option<PathBuf>,
    capture_authority: CaptureProfileAuthority,
) -> Router {
    build_tap_app_with_capture_authority_and_logger(
        tap,
        traces,
        capture_sink,
        capture_authority,
        None,
    )
}

fn build_tap_app_with_capture_authority_and_logger(
    tap: &TapConfig,
    traces: Arc<TraceLog>,
    capture_sink: Option<PathBuf>,
    capture_authority: CaptureProfileAuthority,
    capture_logger: Option<BodyLogger>,
) -> Router {
    // A plain client: no per-egress identity injection (that path refuses auth
    // headers); the tap forwards the client's own credentials untouched. No
    // total timeout so long streamed responses aren't cut off.
    let client = reqwest::Client::builder()
        .build()
        .expect("tap reqwest client builds");
    let upstream = tap.upstream.trim_end_matches('/').to_string();
    let upstream_host = upstream
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or(&upstream)
        .to_string();
    let headers = tap
        .headers
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let state = TapState {
        id: tap.id.clone(),
        upstream,
        upstream_host,
        headers,
        capture_authority,
        capture_worker: if tap.capture_bodies {
            capture_logger
                .or_else(|| {
                    capture_sink.and_then(|sink| match BodyLogger::from_legacy_sink(sink) {
                        Ok(logger) => Some(logger),
                        Err(err) => {
                            tracing::warn!(tap = %tap.id, error = %err, "tap body logger disabled");
                            None
                        }
                    })
                })
                .and_then(|logger| match CaptureWorker::new(logger) {
                    Ok(worker) => Some(worker),
                    Err(err) => {
                        tracing::warn!(tap = %tap.id, error = %err, "tap body capture worker disabled");
                        None
                    }
                })
        } else {
            None
        },
        traces,
        client,
    };
    Router::new()
        .fallback(forward)
        .layer(DefaultBodyLimit::disable())
        .with_state(state)
}

type SharedCaptureAccumulator = Arc<Mutex<Option<CaptureAccumulator>>>;

enum TapRequestBody {
    Buffered(Bytes),
    Streaming {
        body: reqwest::Body,
        capture: Option<SharedCaptureAccumulator>,
    },
}

impl TapRequestBody {
    fn bytes(&self) -> Option<&Bytes> {
        match self {
            TapRequestBody::Buffered(bytes) => Some(bytes),
            TapRequestBody::Streaming { .. } => None,
        }
    }
}

async fn forward(
    State(st): State<TapState>,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if let Ok(ws) = ws {
        return forward_websocket(st, ws, uri, headers).await;
    }

    let started = Instant::now();
    let request_id = sb_core::new_id("tap");
    let capture_context =
        TapCaptureContext::from_headers_with_authority(&headers, &st.capture_authority);
    let path_and_query = uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or_else(|| uri.path());
    let url = format!("{}{}", st.upstream, path_and_query);

    let (body, capture_request_body) =
        match prepare_request_body(&st, &request_id, body, &headers, &capture_context).await {
            Ok(prepared) => prepared,
            Err(response) => return response,
        };

    // Best-effort metadata from the request body (never logged beyond this).
    let parsed: Option<serde_json::Value> = body
        .bytes()
        .and_then(|bytes| serde_json::from_slice(bytes).ok());
    let inbound_model = parsed
        .as_ref()
        .and_then(|v| v.get("model"))
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let streamed = parsed
        .as_ref()
        .and_then(|v| v.get("stream"))
        .and_then(|s| s.as_bool())
        .unwrap_or(false);
    let native_execution = native_execution_observation(&headers, parsed.as_ref());
    if let (Some(worker), Some(request_body)) =
        (st.capture_worker.clone(), capture_request_body.clone())
    {
        write_request_capture(
            RequestCapture {
                worker,
                tap_id: st.id.clone(),
                request_id: request_id.clone(),
                upstream: st.upstream.clone(),
                model: inbound_model.clone(),
                content_type: header_content_type(&headers),
                metadata: serde_json::json!({
                    "method": method.as_str(),
                    "path": path_and_query,
                    "timing_source": "switchback_edge",
                    "token_source": "provider_usage",
                }),
                capture_context: capture_context.clone(),
            },
            CapturePayload::Full(request_body.to_vec()),
        );
    }

    // Forward the request verbatim: client headers minus hop-by-hop, raw body.
    // Authorization and every vendor header (anthropic-beta, user-agent, …) pass
    // through untouched — this is what makes it indistinguishable from native.
    let mut rb = st.client.request(method.clone(), &url);
    let (request_body, stream_capture) = match body {
        TapRequestBody::Buffered(body) => (reqwest::Body::from(body), None),
        TapRequestBody::Streaming { body, capture } => (body, capture),
    };
    rb = rb.body(request_body);
    for (name, value) in headers.iter() {
        if is_hop_by_hop(name.as_str()) || is_execution_observation_header(name.as_str()) {
            continue;
        }
        rb = rb.header(name, value);
    }
    for (name, value) in &st.headers {
        if is_hop_by_hop(name) || is_auth_header(name) || is_execution_observation_header(name) {
            continue;
        }
        rb = rb.header(name, value);
    }

    let upstream_result = rb.send().await;
    if let (Some(capture), Some(stream_capture)) = (st.capture_worker.clone(), stream_capture) {
        if let Some(payload) = finish_shared_capture(&stream_capture) {
            write_request_capture(
                RequestCapture {
                    worker: capture,
                    tap_id: st.id.clone(),
                    request_id: request_id.clone(),
                    upstream: st.upstream.clone(),
                    model: inbound_model.clone(),
                    content_type: header_content_type(&headers),
                    metadata: serde_json::json!({
                        "method": method.as_str(),
                        "path": path_and_query,
                        "timing_source": "switchback_edge",
                        "token_source": "provider_usage",
                    }),
                    capture_context: capture_context.clone(),
                },
                payload,
            );
        }
    }
    let upstream_resp = match upstream_result {
        Ok(resp) => resp,
        Err(err) => {
            record_trace(
                &st,
                TapTraceInput {
                    request_id: &request_id,
                    inbound_model: &inbound_model,
                    streamed,
                    status: 502,
                    started,
                    ok: false,
                    warning: None,
                    native_execution: native_execution.clone(),
                },
            );
            tracing::warn!(tap = %st.id, host = %st.upstream_host, error = %err, "tap upstream request failed");
            return (StatusCode::BAD_GATEWAY, "tap upstream request failed").into_response();
        }
    };

    let status = upstream_resp.status();
    let observe_sse_terminal =
        status.is_success() && (streamed || is_sse_response(upstream_resp.headers()));

    // Copy the upstream status + response headers (minus hop-by-hop) and stream
    // the body back unchanged. Capture tees the body to the sink without buffering.
    let mut builder = Response::builder().status(status);
    for (name, value) in upstream_resp.headers().iter() {
        if is_hop_by_hop(name.as_str()) {
            continue;
        }
        builder = builder.header(name, value);
    }

    let capture_finalize = st
        .capture_worker
        .as_ref()
        .filter(|_| capture_context.enabled())
        .map(|worker| CaptureFinalize {
            worker: worker.clone(),
            tap_id: st.id.clone(),
            request_id: request_id.clone(),
            upstream: st.upstream.clone(),
            model: inbound_model.clone(),
            status: status.as_u16(),
            content_type: header_content_type(upstream_resp.headers()),
            capture_context,
        });
    let trace_finalize = TapTraceFinalize {
        st,
        request_id,
        inbound_model,
        streamed,
        status: status.as_u16(),
        started,
        upstream_ok: status.is_success(),
        native_execution,
    };
    let capture_accumulator = capture_finalize
        .as_ref()
        .filter(|capture| capture.capture_context.captures_payload())
        .map(|_| CaptureAccumulator::new(TAP_CAPTURE_BODY_MAX_BYTES));
    let body = Body::from_stream(TapResponseStream {
        inner: upstream_resp.bytes_stream(),
        capture_accumulator,
        capture_finalize,
        trace_finalize: Some(trace_finalize),
        observe_sse_terminal,
        saw_terminal: false,
        sse_window: Vec::new(),
    });

    builder.body(body).unwrap_or_else(|_| {
        (StatusCode::BAD_GATEWAY, "tap could not build response").into_response()
    })
}

async fn prepare_request_body(
    st: &TapState,
    request_id: &str,
    body: Body,
    headers: &HeaderMap,
    capture_context: &TapCaptureContext,
) -> Result<(TapRequestBody, Option<Bytes>), Response> {
    if st.capture_worker.is_some() && capture_context.captures_payload() {
        if request_body_len(headers).is_some_and(|len| len <= TAP_CAPTURE_BODY_MAX_BYTES) {
            let body = to_bytes(body, TAP_CAPTURE_BODY_MAX_BYTES)
                .await
                .map_err(|err| {
                    tracing::warn!(tap = %st.id, error = %err, "tap request body capture failed");
                    (StatusCode::BAD_GATEWAY, "tap request body capture failed").into_response()
                })?;
            return Ok((TapRequestBody::Buffered(body.clone()), Some(body)));
        }
        let capture = Arc::new(Mutex::new(Some(CaptureAccumulator::new(
            TAP_CAPTURE_BODY_MAX_BYTES,
        ))));
        let stream = TapRequestCaptureStream {
            inner: body.into_data_stream(),
            capture: Arc::clone(&capture),
        };
        return Ok((
            TapRequestBody::Streaming {
                body: reqwest::Body::wrap_stream(stream),
                capture: Some(capture),
            },
            None,
        ));
    }

    let capture_marker =
        (st.capture_worker.is_some() && capture_context.enabled()).then(Bytes::new);
    if request_body_len(headers).is_some_and(|len| len <= TAP_METADATA_BODY_BYTES) {
        let body = to_bytes(body, TAP_METADATA_BODY_BYTES).await.map_err(|err| {
            tracing::warn!(tap = %st.id, request_id = %request_id, error = %err, "tap request body metadata read failed");
            (
                StatusCode::BAD_GATEWAY,
                "tap request body metadata read failed",
            )
                .into_response()
        })?;
        return Ok((TapRequestBody::Buffered(body), capture_marker));
    }

    Ok((
        TapRequestBody::Streaming {
            body: reqwest::Body::wrap_stream(body.into_data_stream()),
            capture: None,
        },
        capture_marker,
    ))
}

fn request_body_len(headers: &HeaderMap) -> Option<usize> {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}

async fn forward_websocket(
    st: TapState,
    ws: WebSocketUpgrade,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let started = Instant::now();
    let request_id = sb_core::new_id("tap");
    let native_execution = native_execution_observation(&headers, None);
    let capture_context =
        TapCaptureContext::from_headers_with_authority(&headers, &st.capture_authority);
    let path_and_query = uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or_else(|| uri.path());
    let url = match websocket_upstream_url(&st.upstream, path_and_query) {
        Some(url) => url,
        None => {
            record_trace(
                &st,
                TapTraceInput {
                    request_id: &request_id,
                    inbound_model: "",
                    streamed: true,
                    status: 502,
                    started,
                    ok: false,
                    warning: None,
                    native_execution: native_execution.clone(),
                },
            );
            return (
                StatusCode::BAD_GATEWAY,
                "tap upstream is not websocket-compatible",
            )
                .into_response();
        }
    };

    let mut upstream_request = match url.into_client_request() {
        Ok(request) => request,
        Err(err) => {
            record_trace(
                &st,
                TapTraceInput {
                    request_id: &request_id,
                    inbound_model: "",
                    streamed: true,
                    status: 502,
                    started,
                    ok: false,
                    warning: None,
                    native_execution: native_execution.clone(),
                },
            );
            tracing::warn!(tap = %st.id, host = %st.upstream_host, error = %err, "tap websocket request build failed");
            return (
                StatusCode::BAD_GATEWAY,
                "tap websocket request build failed",
            )
                .into_response();
        }
    };

    for (name, value) in headers.iter() {
        if should_forward_websocket_header(name.as_str()) {
            upstream_request
                .headers_mut()
                .append(name.clone(), value.clone());
        }
    }
    for (name, value) in &st.headers {
        if is_hop_by_hop(name) || is_auth_header(name) || is_execution_observation_header(name) {
            continue;
        }
        let Ok(name) =
            tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(name.as_bytes())
        else {
            continue;
        };
        let Ok(value) = tokio_tungstenite::tungstenite::http::HeaderValue::from_str(value) else {
            continue;
        };
        upstream_request.headers_mut().append(name, value);
    }

    let requested_protocols: Vec<String> = ws
        .requested_protocols()
        .filter_map(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
        .collect();

    let (upstream_socket, _) = match connect_async(upstream_request).await {
        Ok(upstream) => upstream,
        Err(err) => {
            record_trace(
                &st,
                TapTraceInput {
                    request_id: &request_id,
                    inbound_model: "",
                    streamed: true,
                    status: 502,
                    started,
                    ok: false,
                    warning: None,
                    native_execution: native_execution.clone(),
                },
            );
            tracing::warn!(tap = %st.id, host = %st.upstream_host, error = %err, "tap websocket upstream connect failed");
            return (
                StatusCode::BAD_GATEWAY,
                "tap websocket upstream connect failed",
            )
                .into_response();
        }
    };

    let capture_finalize = st
        .capture_worker
        .as_ref()
        .filter(|_| capture_context.enabled())
        .map(|worker| {
            Arc::new(Mutex::new(WebSocketCapture::new(
                worker.clone(),
                st.id.clone(),
                request_id.clone(),
                st.upstream.clone(),
                capture_context,
            )))
        });

    let observation = Arc::new(Mutex::new(TapWebSocketObservation {
        native_execution: native_execution.unwrap_or_default(),
        inbound_model: None,
    }));
    let trace_finalize = TapWebSocketTraceFinalize {
        st,
        request_id,
        started,
        observation,
    };
    ws.protocols(requested_protocols)
        .on_upgrade(move |client_socket| {
            bridge_websockets(
                client_socket,
                upstream_socket,
                trace_finalize,
                capture_finalize,
            )
        })
}

fn websocket_upstream_url(upstream: &str, path_and_query: &str) -> Option<String> {
    upstream
        .strip_prefix("http://")
        .map(|rest| format!("ws://{rest}{path_and_query}"))
        .or_else(|| {
            upstream
                .strip_prefix("https://")
                .map(|rest| format!("wss://{rest}{path_and_query}"))
        })
        .or_else(|| {
            upstream
                .strip_prefix("ws://")
                .map(|rest| format!("ws://{rest}{path_and_query}"))
        })
        .or_else(|| {
            upstream
                .strip_prefix("wss://")
                .map(|rest| format!("wss://{rest}{path_and_query}"))
        })
}

fn should_forward_websocket_header(name: &str) -> bool {
    if is_hop_by_hop(name) || is_execution_observation_header(name) {
        return false;
    }

    let lower = name.to_ascii_lowercase();
    !matches!(
        lower.as_str(),
        "sec-websocket-key"
            | "sec-websocket-version"
            | "sec-websocket-accept"
            | "sec-websocket-extensions"
    )
}

async fn bridge_websockets(
    client_socket: WebSocket,
    upstream_socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    trace_finalize: TapWebSocketTraceFinalize,
    capture_finalize: Option<Arc<Mutex<WebSocketCapture>>>,
) {
    let (mut client_tx, mut client_rx) = client_socket.split();
    let (mut upstream_tx, mut upstream_rx) = upstream_socket.split();
    let client_capture = capture_finalize.clone();
    let upstream_capture = capture_finalize.clone();
    let client_observation = trace_finalize.observation.clone();

    let client_to_upstream = async {
        loop {
            match client_rx.next().await {
                Some(Ok(message)) => {
                    let warning = axum_close_warning("websocket_client_closed", &message);
                    if let AxumWsMessage::Text(text) = &message {
                        if let Ok(mut observation) = client_observation.lock() {
                            observe_websocket_request_frame(&mut observation, text);
                        }
                    }
                    if let Some(capture) = client_capture.as_ref() {
                        if let Ok(mut capture) = capture.lock() {
                            capture.record_client(&message);
                        }
                    }
                    let Some(message) = axum_to_tungstenite(message) else {
                        continue;
                    };
                    let closing = message.is_close();
                    if upstream_tx.send(message).await.is_err() {
                        return Some("websocket_upstream_send_failed".to_string());
                    }
                    if closing {
                        return warning;
                    }
                }
                Some(Err(err)) => {
                    return Some(format!(
                        "websocket_client_read_error:{}",
                        warning_token(&err.to_string())
                    ));
                }
                None => return Some("websocket_client_ended_without_close_frame".to_string()),
            }
        }
    };

    let upstream_to_client = async {
        loop {
            match upstream_rx.next().await {
                Some(Ok(message)) => {
                    let warning = tungstenite_close_warning("websocket_upstream_closed", &message);
                    if let Some(capture) = upstream_capture.as_ref() {
                        if let Ok(mut capture) = capture.lock() {
                            capture.record_upstream(&message);
                        }
                    }
                    let Some(message) = tungstenite_to_axum(message) else {
                        continue;
                    };
                    let closing = matches!(message, AxumWsMessage::Close(_));
                    if client_tx.send(message).await.is_err() {
                        return Some("websocket_client_send_failed".to_string());
                    }
                    if closing {
                        return warning;
                    }
                }
                Some(Err(err)) => {
                    return Some(format!(
                        "websocket_upstream_read_error:{}",
                        warning_token(&err.to_string())
                    ));
                }
                None => return Some("websocket_upstream_ended_without_close_frame".to_string()),
            }
        }
    };

    let warning = tokio::select! {
        warning = client_to_upstream => warning,
        warning = upstream_to_client => warning,
    };
    if let Some(capture) = capture_finalize {
        if let Ok(capture) = capture.lock() {
            write_websocket_capture(capture.clone(), 101);
        }
    }
    trace_finalize.record(warning);
}

fn axum_to_tungstenite(message: AxumWsMessage) -> Option<TungsteniteMessage> {
    match message {
        AxumWsMessage::Text(text) => Some(TungsteniteMessage::Text(text.to_string().into())),
        AxumWsMessage::Binary(binary) => Some(TungsteniteMessage::Binary(binary)),
        AxumWsMessage::Ping(ping) => Some(TungsteniteMessage::Ping(ping)),
        AxumWsMessage::Pong(pong) => Some(TungsteniteMessage::Pong(pong)),
        AxumWsMessage::Close(close) => Some(TungsteniteMessage::Close(
            close.map(axum_close_to_tungstenite),
        )),
    }
}

fn tungstenite_to_axum(message: TungsteniteMessage) -> Option<AxumWsMessage> {
    match message {
        TungsteniteMessage::Text(text) => Some(AxumWsMessage::Text(text.to_string().into())),
        TungsteniteMessage::Binary(binary) => Some(AxumWsMessage::Binary(binary)),
        TungsteniteMessage::Ping(ping) => Some(AxumWsMessage::Ping(ping)),
        TungsteniteMessage::Pong(pong) => Some(AxumWsMessage::Pong(pong)),
        TungsteniteMessage::Close(close) => {
            Some(AxumWsMessage::Close(close.map(tungstenite_close_to_axum)))
        }
        TungsteniteMessage::Frame(_) => None,
    }
}

fn axum_close_to_tungstenite(close: AxumCloseFrame) -> TungsteniteCloseFrame {
    TungsteniteCloseFrame {
        code: CloseCode::from(close.code),
        reason: close.reason.to_string().into(),
    }
}

fn tungstenite_close_to_axum(close: TungsteniteCloseFrame) -> AxumCloseFrame {
    AxumCloseFrame {
        code: u16::from(close.code),
        reason: close.reason.to_string().into(),
    }
}

fn axum_close_warning(prefix: &str, message: &AxumWsMessage) -> Option<String> {
    match message {
        AxumWsMessage::Close(Some(frame)) => close_warning(prefix, frame.code, &frame.reason),
        AxumWsMessage::Close(None) => None,
        _ => None,
    }
}

fn tungstenite_close_warning(prefix: &str, message: &TungsteniteMessage) -> Option<String> {
    match message {
        TungsteniteMessage::Close(Some(frame)) => {
            close_warning(prefix, u16::from(frame.code), &frame.reason)
        }
        TungsteniteMessage::Close(None) => None,
        _ => None,
    }
}

fn close_warning(prefix: &str, code: u16, reason: &str) -> Option<String> {
    let reason = warning_token(reason);
    if code == 1000 && reason.is_empty() {
        return None;
    }
    if reason.is_empty() {
        Some(format!("{prefix}:{code}"))
    } else {
        Some(format!("{prefix}:{code}:{reason}"))
    }
}

fn warning_token(value: &str) -> String {
    value
        .chars()
        .filter_map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '=') {
                Some(ch)
            } else if ch.is_whitespace() || matches!(ch, ':' | '/' | '\\' | '"' | '\'') {
                Some('_')
            } else {
                None
            }
        })
        .take(96)
        .collect()
}

struct TapTraceInput<'a> {
    request_id: &'a str,
    inbound_model: &'a str,
    streamed: bool,
    status: u16,
    started: Instant,
    ok: bool,
    warning: Option<String>,
    native_execution: Option<NativeExecutionObservation>,
}

fn record_trace(st: &TapState, input: TapTraceInput<'_>) {
    let mut decision = RouteDecision::new(input.request_id, "transparent_tap");
    decision.add_reason(format!("tap={}", st.id));
    decision.add_reason(format!("upstream={}", st.upstream_host));
    let latency = input.started.elapsed().as_millis() as u64;
    let mut trace = RequestTrace::start(input.request_id, 0, input.inbound_model, "tap", decision)
        .with_client_metadata(Some(st.id.clone()), Some("passthrough".to_string()))
        .with_native_execution(input.native_execution);
    if let Some(warning) = input.warning.as_deref() {
        trace.warning(warning);
    }
    let class = if input.ok {
        None
    } else {
        Some(input.warning.as_deref().unwrap_or("upstream_error"))
    };
    trace.attempt(match class {
        None => Attempt::success(
            st.upstream_host.clone(),
            "tap",
            input.inbound_model,
            "client-native",
            "direct",
            latency,
        ),
        Some(c) => Attempt::failed(
            st.upstream_host.clone(),
            "tap",
            input.inbound_model,
            "client-native",
            "direct",
            latency,
            c,
            false,
        ),
    });
    st.traces
        .record(trace.finish(input.status, latency, input.streamed));
}

/// Holds what to persist once the response stream completes.
struct CaptureFinalize {
    worker: CaptureWorker,
    tap_id: String,
    request_id: String,
    upstream: String,
    model: String,
    status: u16,
    content_type: Option<String>,
    capture_context: TapCaptureContext,
}

struct RequestCapture {
    worker: CaptureWorker,
    tap_id: String,
    request_id: String,
    upstream: String,
    model: String,
    content_type: Option<String>,
    metadata: serde_json::Value,
    capture_context: TapCaptureContext,
}

#[derive(Clone)]
struct CapturedWsFrame {
    kind: &'static str,
    text: Option<String>,
    body: Vec<u8>,
    close_code: Option<u16>,
}

#[derive(Clone, Serialize)]
struct StoredWsFrame {
    sequence: usize,
    frame_kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    close_code: Option<u16>,
    body_bytes: usize,
    body_encoding: &'static str,
    body: String,
}

impl StoredWsFrame {
    fn from_captured(sequence: usize, frame: CapturedWsFrame) -> Self {
        let body_bytes = frame.body.len();
        let (body_encoding, body) = match frame.text {
            Some(text) => ("utf8", text),
            None => (
                "base64",
                base64::engine::general_purpose::STANDARD.encode(&frame.body),
            ),
        };
        Self {
            sequence,
            frame_kind: frame.kind,
            close_code: frame.close_code,
            body_bytes,
            body_encoding,
            body,
        }
    }
}

#[derive(Clone, Default)]
struct WebSocketFrameBuffer {
    frames: Vec<StoredWsFrame>,
    raw_body_bytes: usize,
    chunk_count: usize,
}

impl WebSocketFrameBuffer {
    fn push(&mut self, frame: StoredWsFrame) {
        self.raw_body_bytes = self.raw_body_bytes.saturating_add(frame.body_bytes);
        self.frames.push(frame);
    }

    fn should_flush(&self) -> bool {
        self.raw_body_bytes >= TAP_WEBSOCKET_CAPTURE_CHUNK_BYTES
    }
}

#[derive(Clone)]
struct WebSocketCapture {
    worker: CaptureWorker,
    tap_id: String,
    request_id: String,
    upstream: String,
    model: String,
    capture_context: TapCaptureContext,
    client_frame_count: usize,
    upstream_frame_count: usize,
    client_buffer: WebSocketFrameBuffer,
    upstream_buffer: WebSocketFrameBuffer,
}

impl WebSocketCapture {
    fn new(
        worker: CaptureWorker,
        tap_id: String,
        request_id: String,
        upstream: String,
        capture_context: TapCaptureContext,
    ) -> Self {
        Self {
            worker,
            tap_id,
            request_id,
            upstream,
            model: String::new(),
            capture_context,
            client_frame_count: 0,
            upstream_frame_count: 0,
            client_buffer: WebSocketFrameBuffer::default(),
            upstream_buffer: WebSocketFrameBuffer::default(),
        }
    }

    fn record_client(&mut self, message: &AxumWsMessage) {
        self.client_frame_count += 1;
        let Some(frame) = axum_frame_body(message) else {
            return;
        };
        if let Some(text) = frame.text.as_deref() {
            self.capture_model(text);
        }
        if !self.capture_context.captures_payload() {
            return;
        }
        self.client_buffer
            .push(StoredWsFrame::from_captured(self.client_frame_count, frame));
        if self.client_buffer.should_flush() {
            self.flush_client_frames();
        }
    }

    fn record_upstream(&mut self, message: &TungsteniteMessage) {
        self.upstream_frame_count += 1;
        let Some(frame) = tungstenite_frame_body(message) else {
            return;
        };
        if !self.capture_context.captures_payload() {
            return;
        }
        self.upstream_buffer.push(StoredWsFrame::from_captured(
            self.upstream_frame_count,
            frame,
        ));
        if self.upstream_buffer.should_flush() {
            self.flush_upstream_frames();
        }
    }

    fn capture_model(&mut self, text: &str) {
        if !self.model.is_empty() {
            return;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            return;
        };
        if let Some(model) = value.get("model").and_then(|v| v.as_str()) {
            self.model = model.to_string();
            return;
        }
        if let Some(model) = value
            .get("response")
            .and_then(|v| v.get("model"))
            .and_then(|v| v.as_str())
        {
            self.model = model.to_string();
        }
    }

    fn flush_client_frames(&mut self) {
        let frames = std::mem::take(&mut self.client_buffer.frames);
        let raw_body_bytes = std::mem::take(&mut self.client_buffer.raw_body_bytes);
        if frames.is_empty() {
            return;
        }
        self.client_buffer.chunk_count += 1;
        write_ws_frame_chunk(
            self,
            CaptureStage::ClientInbound,
            "client",
            self.client_buffer.chunk_count,
            raw_body_bytes,
            frames,
        );
    }

    fn flush_upstream_frames(&mut self) {
        let frames = std::mem::take(&mut self.upstream_buffer.frames);
        let raw_body_bytes = std::mem::take(&mut self.upstream_buffer.raw_body_bytes);
        if frames.is_empty() {
            return;
        }
        self.upstream_buffer.chunk_count += 1;
        write_ws_frame_chunk(
            self,
            CaptureStage::ClientResponse,
            "upstream",
            self.upstream_buffer.chunk_count,
            raw_body_bytes,
            frames,
        );
    }

    fn flush_pending_frames(&mut self) {
        self.flush_client_frames();
        self.flush_upstream_frames();
    }
}

struct TapTraceFinalize {
    st: TapState,
    request_id: String,
    inbound_model: String,
    streamed: bool,
    status: u16,
    started: Instant,
    upstream_ok: bool,
    native_execution: Option<NativeExecutionObservation>,
}

impl TapTraceFinalize {
    fn record(self, status_override: Option<u16>, warning: Option<&'static str>) {
        let status = status_override.unwrap_or(self.status);
        record_trace(
            &self.st,
            TapTraceInput {
                request_id: &self.request_id,
                inbound_model: &self.inbound_model,
                streamed: self.streamed,
                status,
                started: self.started,
                ok: self.upstream_ok && warning.is_none(),
                warning: warning.map(ToOwned::to_owned),
                native_execution: self.native_execution,
            },
        );
    }
}

struct TapWebSocketTraceFinalize {
    st: TapState,
    request_id: String,
    started: Instant,
    observation: Arc<Mutex<TapWebSocketObservation>>,
}

impl TapWebSocketTraceFinalize {
    fn record(self, warning: Option<String>) {
        let observation = self
            .observation
            .lock()
            .ok()
            .map(|observation| observation.clone())
            .unwrap_or_default();
        let inbound_model = observation.inbound_model.unwrap_or_default();
        let native_execution =
            (!observation.native_execution.is_empty()).then_some(observation.native_execution);
        record_trace(
            &self.st,
            TapTraceInput {
                request_id: &self.request_id,
                inbound_model: &inbound_model,
                streamed: true,
                status: 101,
                started: self.started,
                ok: true,
                warning,
                native_execution,
            },
        );
    }
}

/// Tees the upstream response to the client unchanged while finalizing metadata
/// after the stream ends. Body capture remains explicit; SSE terminal detection
/// keeps only a tiny rolling window and never writes content to traces.
struct TapResponseStream<S> {
    inner: S,
    capture_accumulator: Option<CaptureAccumulator>,
    capture_finalize: Option<CaptureFinalize>,
    trace_finalize: Option<TapTraceFinalize>,
    observe_sse_terminal: bool,
    saw_terminal: bool,
    sse_window: Vec<u8>,
}

impl<S> TapResponseStream<S> {
    fn observe_chunk(&mut self, chunk: &Bytes) {
        if !self.observe_sse_terminal || self.saw_terminal {
            return;
        }
        self.sse_window.extend_from_slice(chunk);
        if self.sse_window.len() > TAP_SSE_TERMINAL_WINDOW_BYTES {
            let excess = self.sse_window.len() - TAP_SSE_TERMINAL_WINDOW_BYTES;
            self.sse_window.drain(..excess);
        }
        if sse_window_has_terminal_event(&self.sse_window) {
            self.saw_terminal = true;
            self.sse_window.clear();
        }
    }

    fn finalize(&mut self, status_override: Option<u16>, warning: Option<&'static str>) {
        if let Some(fin) = self.capture_finalize.take() {
            let payload = self
                .capture_accumulator
                .take()
                .map(CaptureAccumulator::finish)
                .unwrap_or_else(|| CapturePayload::Full(Vec::new()));
            write_capture(fin, payload);
        }
        if let Some(fin) = self.trace_finalize.take() {
            fin.record(status_override, warning);
        }
    }
}

impl<S> Drop for TapResponseStream<S> {
    fn drop(&mut self) {
        if self.trace_finalize.is_some() {
            self.finalize(Some(499), Some("client_aborted"));
        }
    }
}

impl<S> Stream for TapResponseStream<S>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                if let Some(capture) = self.capture_accumulator.as_mut() {
                    capture.observe(&chunk);
                }
                self.observe_chunk(&chunk);
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(err))) => {
                self.finalize(None, Some("upstream_stream_error"));
                Poll::Ready(Some(Err(std::io::Error::other(err))))
            }
            Poll::Ready(None) => {
                let warning = if self.observe_sse_terminal && !self.saw_terminal {
                    Some("upstream_closed_before_terminal")
                } else {
                    None
                };
                self.finalize(None, warning);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

fn is_sse_response(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("text/event-stream"))
}

fn sse_window_has_terminal_event(window: &[u8]) -> bool {
    let text = String::from_utf8_lossy(window);
    [
        "response.completed",
        "response.failed",
        "response.cancelled",
        "response.incomplete",
        "[DONE]",
        "message_stop",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

fn write_capture(fin: CaptureFinalize, response: CapturePayload) {
    let effective_policy = fin
        .capture_context
        .effective_capture_policy()
        .map(str::to_string);
    let metadata = fin.capture_context.merge_metadata(serde_json::json!({
        "tap": fin.tap_id,
        "timing_source": "switchback_edge",
        "token_source": "provider_usage",
    }));
    fin.worker.submit_authorized_payload(
        BodyEventInput {
            request_id: fin.request_id,
            capture_stage: CaptureStage::ClientResponse,
            protocol: "http".to_string(),
            upstream: Some(fin.upstream),
            model: Some(fin.model),
            status: Some(fin.status),
            content_type: fin.content_type,
            metadata,
            body: Vec::new(),
        },
        response,
        effective_policy.as_deref(),
    );
}

fn write_websocket_capture(mut fin: WebSocketCapture, status: u16) {
    fin.flush_pending_frames();
    let effective_policy = fin
        .capture_context
        .effective_capture_policy()
        .map(str::to_string);
    let summary = serde_json::json!({
        "schema_version": "switchback.websocket_session.v1",
        "protocol": "websocket",
        "client_frame_count": fin.client_frame_count,
        "upstream_frame_count": fin.upstream_frame_count,
        "client_chunk_count": fin.client_buffer.chunk_count,
        "upstream_chunk_count": fin.upstream_buffer.chunk_count,
    });
    let metadata = fin.capture_context.merge_metadata(serde_json::json!({
        "tap": fin.tap_id,
        "capture_format": "websocket_session_summary_v1",
        "timing_source": "switchback_edge",
        "token_source": "provider_usage",
    }));
    fin.worker.submit_authorized_payload(
        BodyEventInput {
            request_id: fin.request_id,
            capture_stage: CaptureStage::ClientSession,
            protocol: "websocket".to_string(),
            upstream: Some(fin.upstream),
            model: Some(fin.model),
            status: Some(status),
            content_type: Some("application/json".to_string()),
            metadata,
            body: Vec::new(),
        },
        CapturePayload::Full(serde_json::to_vec(&summary).unwrap_or_default()),
        effective_policy.as_deref(),
    );
}

fn write_request_capture(capture: RequestCapture, payload: CapturePayload) {
    let effective_policy = capture
        .capture_context
        .effective_capture_policy()
        .map(str::to_string);
    let metadata = capture
        .capture_context
        .merge_metadata(merge_tap_metadata(capture.tap_id, capture.metadata));
    capture.worker.submit_authorized_payload(
        BodyEventInput {
            request_id: capture.request_id,
            capture_stage: CaptureStage::ClientInbound,
            protocol: "http".to_string(),
            upstream: Some(capture.upstream),
            model: Some(capture.model),
            status: None,
            content_type: capture.content_type,
            metadata,
            body: Vec::new(),
        },
        payload,
        effective_policy.as_deref(),
    );
}

fn write_ws_frame_chunk(
    capture: &WebSocketCapture,
    stage: CaptureStage,
    direction: &'static str,
    chunk_sequence: usize,
    raw_body_bytes: usize,
    frames: Vec<StoredWsFrame>,
) {
    let first_sequence = frames
        .first()
        .map(|frame| frame.sequence)
        .unwrap_or_default();
    let last_sequence = frames
        .last()
        .map(|frame| frame.sequence)
        .unwrap_or_default();
    let frame_count = frames.len();
    let chunk = serde_json::json!({
        "schema_version": "switchback.websocket_frames.v1",
        "direction": direction,
        "chunk_sequence": chunk_sequence,
        "frames": frames,
    });
    let body = match serde_json::to_vec(&chunk) {
        Ok(body) => body,
        Err(err) => {
            panic!("serializing the typed WebSocket capture chunk must succeed: {err}")
        }
    };
    let metadata = capture.capture_context.merge_metadata(serde_json::json!({
        "tap": capture.tap_id.clone(),
        "direction": direction,
        "capture_format": "websocket_frames_v1",
        "chunk_sequence": chunk_sequence,
        "frame_count": frame_count,
        "first_sequence": first_sequence,
        "last_sequence": last_sequence,
        "raw_body_bytes": raw_body_bytes,
        "timing_source": "switchback_edge",
        "token_source": "provider_usage",
    }));
    capture.worker.submit_authorized_payload(
        BodyEventInput {
            request_id: capture.request_id.clone(),
            capture_stage: stage,
            protocol: "websocket".to_string(),
            upstream: Some(capture.upstream.clone()),
            model: Some(capture.model.clone()),
            status: Some(101),
            content_type: Some("application/vnd.switchback.websocket-frames+json".to_string()),
            metadata,
            body: Vec::new(),
        },
        CapturePayload::Full(body),
        capture.capture_context.effective_capture_policy(),
    );
}

fn merge_tap_metadata(tap_id: String, metadata: serde_json::Value) -> serde_json::Value {
    let mut object = match metadata {
        serde_json::Value::Object(object) => object,
        other => {
            let mut object = serde_json::Map::new();
            object.insert("metadata".to_string(), other);
            object
        }
    };
    object.insert("tap".to_string(), serde_json::Value::String(tap_id));
    serde_json::Value::Object(object)
}

fn header_content_type(headers: &HeaderMap) -> Option<String> {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

fn axum_frame_body(message: &AxumWsMessage) -> Option<CapturedWsFrame> {
    captured_axum_frame(message)
}

fn tungstenite_frame_body(message: &TungsteniteMessage) -> Option<CapturedWsFrame> {
    captured_tungstenite_frame(message)
}

fn captured_axum_frame(message: &AxumWsMessage) -> Option<CapturedWsFrame> {
    match message {
        AxumWsMessage::Text(text) => Some(captured_text_frame("text", &text.to_string())),
        AxumWsMessage::Binary(binary) => Some(captured_binary_frame("binary", binary.as_ref())),
        AxumWsMessage::Ping(ping) => Some(captured_binary_frame("ping", ping.as_ref())),
        AxumWsMessage::Pong(pong) => Some(captured_binary_frame("pong", pong.as_ref())),
        AxumWsMessage::Close(close) => Some(CapturedWsFrame {
            kind: "close",
            text: close.as_ref().map(|frame| frame.reason.to_string()),
            body: close
                .as_ref()
                .map(|frame| frame.reason.as_bytes().to_vec())
                .unwrap_or_default(),
            close_code: close.as_ref().map(|frame| frame.code),
        }),
    }
}

fn captured_tungstenite_frame(message: &TungsteniteMessage) -> Option<CapturedWsFrame> {
    match message {
        TungsteniteMessage::Text(text) => Some(captured_text_frame("text", text.as_ref())),
        TungsteniteMessage::Binary(binary) => {
            Some(captured_binary_frame("binary", binary.as_ref()))
        }
        TungsteniteMessage::Ping(ping) => Some(captured_binary_frame("ping", ping.as_ref())),
        TungsteniteMessage::Pong(pong) => Some(captured_binary_frame("pong", pong.as_ref())),
        TungsteniteMessage::Close(close) => Some(CapturedWsFrame {
            kind: "close",
            text: close.as_ref().map(|frame| frame.reason.to_string()),
            body: close
                .as_ref()
                .map(|frame| frame.reason.as_bytes().to_vec())
                .unwrap_or_default(),
            close_code: close.as_ref().map(|frame| u16::from(frame.code)),
        }),
        TungsteniteMessage::Frame(_) => None,
    }
}

fn captured_text_frame(kind: &'static str, text: &str) -> CapturedWsFrame {
    CapturedWsFrame {
        kind,
        text: Some(text.to_string()),
        body: text.as_bytes().to_vec(),
        close_code: None,
    }
}

fn captured_binary_frame(kind: &'static str, bytes: &[u8]) -> CapturedWsFrame {
    CapturedWsFrame {
        kind,
        text: None,
        body: bytes.to_vec(),
        close_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use axum::extract::ws::{close_code, CloseFrame, Message as AxumWsMessage, WebSocketUpgrade};
    use axum::routing::{any, post};
    use axum::Json;
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderValue;
    use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;

    fn temp_capture_root(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "switchback-tap-bodylog-{tag}-{}-{nanos}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }
    fn isolated_body_logger(
        legacy_jsonl: &std::path::Path,
        archive_root: &std::path::Path,
    ) -> (BodyLogger, sb_bodylog::BodyLoggerConfig) {
        let config = sb_bodylog::BodyLoggerConfig {
            state_dir: legacy_jsonl.parent().unwrap().to_path_buf(),
            archive_root: archive_root.to_path_buf(),
            legacy_jsonl: Some(legacy_jsonl.to_path_buf()),
            inline_threshold_bytes: 1,
        };
        if let Some(inherited) = std::env::var_os("SWITCHBACK_BODY_ARCHIVE_ROOT") {
            assert_ne!(
                config.archive_root,
                PathBuf::from(inherited),
                "body-capture tests must ignore the inherited archive root"
            );
        }
        fs::create_dir_all(archive_root).unwrap();
        let logger = BodyLogger::new(config.clone()).unwrap();
        (logger, config)
    }

    fn build_isolated_capture_tap_app(
        tap: &TapConfig,
        traces: Arc<TraceLog>,
        legacy_jsonl: &std::path::Path,
        archive_root: &std::path::Path,
    ) -> (Router, sb_bodylog::BodyLoggerConfig) {
        build_isolated_capture_tap_app_with_authority(
            tap,
            traces,
            legacy_jsonl,
            archive_root,
            CaptureProfileAuthority::load_live_default(),
        )
    }

    fn build_isolated_capture_tap_app_with_authority(
        tap: &TapConfig,
        traces: Arc<TraceLog>,
        legacy_jsonl: &std::path::Path,
        archive_root: &std::path::Path,
        authority: CaptureProfileAuthority,
    ) -> (Router, sb_bodylog::BodyLoggerConfig) {
        let (logger, config) = isolated_body_logger(legacy_jsonl, archive_root);
        let app = build_tap_app_with_capture_authority_and_logger(
            tap,
            traces,
            Some(legacy_jsonl.to_path_buf()),
            authority,
            Some(logger),
        );
        (app, config)
    }

    async fn wait_for_traces(traces: &TraceLog, expected: usize) -> Vec<sb_trace::TraceRecord> {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let recent = traces.recent(8);
                if recent.len() >= expected {
                    return recent;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the tap persisted its trace before the bounded test deadline")
    }

    fn wait_for_body_events(
        logger: &BodyLogger,
        request_id: &str,
        expected: usize,
        timeout: std::time::Duration,
    ) -> Vec<sb_bodylog::BodyRecord> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let events = logger.events_for_request(request_id);
            let timed_out = std::time::Instant::now() >= deadline;
            match events {
                Ok(events) if events.len() >= expected || timed_out => {
                    return events;
                }
                Ok(_) => {}
                Err(error) if timed_out => {
                    panic!("capture index stayed locked until polling deadline: {error}");
                }
                Err(_) => {}
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[tokio::test]
    async fn tap_forwards_request_verbatim_and_records_a_trace() {
        // Fake upstream: echoes back the auth header + body length it received.
        let upstream = Router::new().route(
            "/v1/messages",
            post(|headers: HeaderMap, body: Bytes| async move {
                Json(serde_json::json!({
                    "seen_auth": headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("<none>"),
                    "seen_beta": headers
                        .get("anthropic-beta")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("<none>"),
                    "seen_execution_lane": headers
                        .get(LANE_ID_HEADER)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("<none>"),
                    "seen_launch_profile": headers
                        .get("x-switchback-launch-profile")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("<none>"),
                    "seen_harness": headers
                        .get("x-switchback-harness")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("<none>"),
                    "body_len": body.len(),
                }))
            }),
        );
        let up_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(up_listener, upstream).await.unwrap() });

        // Tap pointed at the fake upstream.
        let traces = Arc::new(TraceLog::in_memory(16));
        let cfg = TapConfig {
            id: "test-tap".to_string(),
            bind: "127.0.0.1:0".to_string(),
            upstream: format!("http://{up_addr}"),
            capture_bodies: false,
            headers: Default::default(),
        };
        let tap_app = build_tap_app(&cfg, traces.clone(), None);
        let tap_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tap_addr = tap_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(tap_listener, tap_app).await.unwrap() });

        // Client sends its own auth; the tap must forward it untouched.
        let resp: serde_json::Value = reqwest::Client::new()
            .post(format!("http://{tap_addr}/v1/messages"))
            .header("authorization", "Bearer CLIENT-OWN-TOKEN")
            .header("anthropic-beta", "oauth-2025-04-20")
            .header(LANE_ID_HEADER, "gpt56-sol-ultra")
            .header(
                LANE_REVISION_HEADER,
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )
            .header(REQUESTED_EFFORT_HEADER, "ultra")
            .header("x-switchback-launch-profile", "claude-zai-full")
            .header(
                "x-switchback-conformance-revision",
                format!("sha256:{}", "b".repeat(64)),
            )
            .header("x-switchback-harness", "claude-code")
            .json(&serde_json::json!({
                "model": "claude-x",
                "messages": [],
                "output_config": {"effort": "ultra"}
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        assert_eq!(
            resp["seen_auth"], "Bearer CLIENT-OWN-TOKEN",
            "auth forwarded verbatim"
        );
        assert_eq!(
            resp["seen_beta"], "oauth-2025-04-20",
            "vendor headers forwarded"
        );
        assert_eq!(
            resp["seen_execution_lane"], "<none>",
            "internal execution headers must not leak upstream"
        );
        assert_eq!(
            resp["seen_launch_profile"], "<none>",
            "launch-profile identity must not leak upstream"
        );
        assert_eq!(
            resp["seen_harness"], "<none>",
            "harness identity must not leak upstream"
        );
        assert!(resp["body_len"].as_u64().unwrap() > 0, "body forwarded");

        let recent = traces.recent(8);
        assert_eq!(recent.len(), 1, "the tap recorded one trace");
        assert_eq!(recent[0].inbound_model, "claude-x");
        assert_eq!(recent[0].route, "tap");
        assert_eq!(recent[0].final_status, 200);
        assert_eq!(
            recent[0].native_execution,
            Some(NativeExecutionObservation {
                lane_id: Some("gpt56-sol-ultra".to_string()),
                lane_revision: Some(format!("sha256:{}", "a".repeat(64))),
                launch_profile: Some("claude-zai-full".to_string()),
                conformance_revision: Some(format!("sha256:{}", "b".repeat(64))),
                harness: Some("claude-code".to_string()),
                requested_effort: Some("ultra".to_string()),
                observed_effort: Some("ultra".to_string()),
                observed_effort_path: Some("/output_config/effort".to_string()),
            })
        );
        let native_json = serde_json::to_value(&recent[0].native_execution).unwrap();
        assert_eq!(
            native_json["launch_profile"], "claude-zai-full",
            "the trace must retain the resolved Switchback profile identity"
        );
        assert_eq!(native_json["harness"], "claude-code");
        assert_eq!(
            native_json["conformance_revision"],
            format!("sha256:{}", "b".repeat(64))
        );
    }

    #[tokio::test]
    async fn tap_applies_configured_non_auth_headers() {
        let upstream = Router::new().route(
            "/v1/messages",
            post(|headers: HeaderMap| async move {
                Json(serde_json::json!({
                    "seen_auth": headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("<none>"),
                    "seen_forwarded_marker": headers
                        .get("x-tap-forwarded-marker")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("<none>"),
                }))
            }),
        );
        let up_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(up_listener, upstream).await.unwrap() });

        // Deliberately NOT `x-headroom-base-url` with a provider URL. Taps do
        // forward that header, but Headroom honors it only on OpenAI-shaped
        // paths — never on `/v1/messages` — so using it here as the example read
        // as "this is how a lane selects its provider" and cost a real lane
        // weeks of 401s. Provider selection is the Headroom process's own
        // pinned target; a tap picks a provider by which instance it forwards
        // to. `sb lane doctor`'s tap.no_openai_base_url_override enforces that.
        let mut tap_headers = std::collections::BTreeMap::new();
        tap_headers.insert("x-tap-forwarded-marker".to_string(), "kept".to_string());
        tap_headers.insert("authorization".to_string(), "Bearer wrong".to_string());

        let traces = Arc::new(TraceLog::in_memory(16));
        let cfg = TapConfig {
            id: "zai-headroom-tap".to_string(),
            bind: "127.0.0.1:0".to_string(),
            upstream: format!("http://{up_addr}"),
            capture_bodies: false,
            headers: tap_headers,
        };
        let tap_app = build_tap_app(&cfg, traces, None);
        let tap_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tap_addr = tap_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(tap_listener, tap_app).await.unwrap() });

        let resp: serde_json::Value = reqwest::Client::new()
            .post(format!("http://{tap_addr}/v1/messages"))
            .header("authorization", "Bearer client")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        assert_eq!(resp["seen_auth"], "Bearer client");
        assert_eq!(resp["seen_forwarded_marker"], "kept");
    }

    #[tokio::test]
    async fn tap_body_capture_writes_protected_segment_and_index() {
        let upstream = Router::new().route(
            "/v1/responses",
            post(|body: Bytes| async move {
                assert!(
                    String::from_utf8_lossy(&body).contains("capture-request-secret"),
                    "upstream receives the original request body"
                );
                Json(serde_json::json!({
                    "id": "resp_test",
                    "output_text": "capture-response-secret",
                }))
            }),
        );
        let up_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(up_listener, upstream).await.unwrap() });

        let root = temp_capture_root("http");
        let state_dir = root.join("state");
        let archive_root = state_dir.join("body").join("archive");
        let legacy_jsonl = state_dir.join("tap-bodies.jsonl");

        let traces = Arc::new(TraceLog::in_memory(16));
        let cfg = TapConfig {
            id: "codex-tap".to_string(),
            bind: "127.0.0.1:0".to_string(),
            upstream: format!("http://{up_addr}"),
            capture_bodies: true,
            headers: Default::default(),
        };
        let (tap_app, bodylog_config) =
            build_isolated_capture_tap_app(&cfg, traces.clone(), &legacy_jsonl, &archive_root);
        let tap_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tap_addr = tap_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(tap_listener, tap_app).await.unwrap() });

        let body: serde_json::Value = reqwest::Client::new()
            .post(format!("http://{tap_addr}/v1/responses"))
            .json(&serde_json::json!({
                "model": "gpt-test",
                "input": "capture-request-secret",
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["output_text"], "capture-response-secret");

        let logger = sb_bodylog::BodyLogger::open_existing(bodylog_config)
            .unwrap()
            .expect("tap body logger created the index");
        let mut status = logger.status().unwrap();
        for _ in 0..50 {
            if status.events >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            status = logger.status().unwrap();
        }
        assert_eq!(status.events, 2);
        assert_eq!(status.blobs, 2);
        assert_eq!(status.spool_backlog, 0);
        assert!(status.archive_available);

        let events = logger.latest_events(10).unwrap();
        assert_eq!(events.len(), 2);
        assert!(events
            .iter()
            .all(|event| event.storage == "archive_segment"
                && event.archive_path.ends_with(".sbcap")));
        let captured: Vec<Vec<u8>> = events
            .iter()
            .map(|event| logger.read_blob(&event.body_sha256).unwrap())
            .collect();
        assert!(captured
            .iter()
            .any(|body| String::from_utf8_lossy(body).contains("capture-request-secret")));
        assert!(captured
            .iter()
            .any(|body| String::from_utf8_lossy(body).contains("capture-response-secret")));
        assert!(
            !legacy_jsonl.exists(),
            "the retired per-event compatibility sink stays frozen"
        );
    }

    #[tokio::test]
    async fn launch_capture_policy_is_applied_end_to_end_and_never_forwarded_upstream() {
        let upstream = Router::new().route(
            "/v1/messages",
            post(|headers: HeaderMap| async move {
                Json(serde_json::json!({
                    "seen_capture_policy": headers
                        .get("x-switchback-capture-policy")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("<none>"),
                    "ok": true,
                }))
            }),
        );
        let up_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(up_listener, upstream).await.unwrap() });

        let root = temp_capture_root("profile-policy-http");
        let state_dir = root.join("state");
        let archive_root = state_dir.join("body").join("archive");
        let legacy_jsonl = state_dir.join("tap-bodies.jsonl");
        let traces = Arc::new(TraceLog::in_memory(16));
        let cfg = TapConfig {
            id: "claude-tap".to_string(),
            bind: "127.0.0.1:0".to_string(),
            upstream: format!("http://{up_addr}"),
            capture_bodies: true,
            headers: Default::default(),
        };
        let metadata_revision = format!("sha256:{}", "c".repeat(64));
        let off_revision = format!("sha256:{}", "d".repeat(64));
        let authority = CaptureProfileAuthority::from_entries([
            (
                "profile-metadata",
                metadata_revision.as_str(),
                "metadata_only",
            ),
            ("profile-off", off_revision.as_str(), "off"),
        ])
        .unwrap();
        let (tap_app, bodylog_config) = build_isolated_capture_tap_app_with_authority(
            &cfg,
            traces,
            &legacy_jsonl,
            &archive_root,
            authority,
        );
        let tap_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tap_addr = tap_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(tap_listener, tap_app).await.unwrap() });

        let client = reqwest::Client::new();
        let metadata_only: serde_json::Value = client
            .post(format!("http://{tap_addr}/v1/messages"))
            .header("x-switchback-launch-profile", "profile-metadata")
            .header("x-switchback-conformance-revision", &metadata_revision)
            .header("x-switchback-capture-policy", "segmented_full_wire")
            .json(&serde_json::json!({
                "model": "qwen3.8-max-preview",
                "messages": [{"role": "user", "content": "private prompt"}],
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            metadata_only["seen_capture_policy"], "<none>",
            "the Switchback-owned capture policy must stop at the local observation edge"
        );

        let logger = sb_bodylog::BodyLogger::open_existing(bodylog_config)
            .unwrap()
            .expect("tap body logger created the index");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut events = logger.latest_events(10).unwrap();
        while events.len() < 2 && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            events = logger.latest_events(10).unwrap();
        }
        assert_eq!(
            events.len(),
            2,
            "request and response gaps are both retained"
        );
        assert!(events.iter().all(|event| event.storage == "metadata_only"));
        assert!(events.iter().all(|event| {
            event.metadata["capture_metadata"]["launch_profile"] == "profile-metadata"
                && event.metadata["capture_metadata"]["launch_capture_policy"] == "metadata_only"
                && event.metadata["capture_metadata"]["launch_capture_policy_claimed"]
                    == "segmented_full_wire"
        }));
        assert_eq!(logger.status().unwrap().blobs, 0);

        let capture_off: serde_json::Value = client
            .post(format!("http://{tap_addr}/v1/messages"))
            .header("x-switchback-launch-profile", "profile-off")
            .header("x-switchback-conformance-revision", &off_revision)
            .header("x-switchback-capture-policy", "segmented_full_wire")
            .json(&serde_json::json!({
                "model": "private-model",
                "messages": [{"role": "user", "content": "must not persist"}],
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(capture_off["seen_capture_policy"], "<none>");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            logger.latest_events(10).unwrap().len(),
            2,
            "capture policy off must persist neither wire bodies nor capture-gap rows"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn tap_body_capture_never_blocks_forwarding_on_a_busy_index() {
        let upstream = Router::new().route(
            "/v1/messages",
            post(|| async { Json(serde_json::json!({"ok": true})) }),
        );
        let up_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(up_listener, upstream).await.unwrap() });

        let root = temp_capture_root("busy-index");
        let state_dir = root.join("state");
        let archive_root = state_dir.join("body").join("archive");
        let legacy_jsonl = state_dir.join("tap-bodies.jsonl");
        let traces = Arc::new(TraceLog::in_memory(16));
        let cfg = TapConfig {
            id: "busy-capture-tap".to_string(),
            bind: "127.0.0.1:0".to_string(),
            upstream: format!("http://{up_addr}"),
            capture_bodies: true,
            headers: Default::default(),
        };
        let (tap_app, _bodylog_config) =
            build_isolated_capture_tap_app(&cfg, traces, &legacy_jsonl, &archive_root);

        // Hold the capture index so BodyLogger::record must wait for its SQLite
        // busy timeout. Observability is allowed to lag or fail; it must never
        // delay the transparent forwarding path.
        let index_path = state_dir.join("body/index-v2.sqlite");
        let capture_lock = rusqlite::Connection::open(index_path).unwrap();
        capture_lock.execute_batch("BEGIN EXCLUSIVE").unwrap();

        let tap_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tap_addr = tap_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(tap_listener, tap_app).await.unwrap() });

        let response = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            reqwest::Client::new()
                .post(format!("http://{tap_addr}/v1/messages"))
                .header("content-type", "application/json")
                .body(r#"{"model":"claude-x","messages":[]}"#)
                .send(),
        )
        .await;

        capture_lock.execute_batch("ROLLBACK").unwrap();
        let _ = fs::remove_dir_all(root);
        let response = response
            .expect("a busy evidence index must not delay the caller")
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn tap_capture_budget_bounds_total_bytes_and_releases_reservations() {
        let budget = CaptureBudget::new(1024);
        assert!(
            budget.try_reserve(1025).is_none(),
            "one oversized body must become a typed capture gap instead of widening the memory bound"
        );
        let first = budget.try_reserve(768).unwrap();
        assert_eq!(budget.queued_bytes(), 768);
        assert!(budget.try_reserve(257).is_none());
        assert_eq!(budget.queued_bytes(), 768);

        drop(first);
        assert_eq!(budget.queued_bytes(), 0);
        let full = budget.try_reserve(1024).unwrap();
        assert_eq!(budget.queued_bytes(), 1024);
        assert!(budget.try_reserve(1).is_none());
        drop(full);
        assert_eq!(budget.queued_bytes(), 0);
    }

    #[test]
    fn capture_accumulator_discards_payload_after_limit_but_keeps_identity() {
        let mut capture = CaptureAccumulator::new(8);
        capture.observe(b"12345");
        capture.observe(b"6789");

        let CapturePayload::Gap(gap) = capture.finish() else {
            panic!("oversized capture must finish as a typed gap");
        };
        assert_eq!(gap.reason, "body_limit_exceeded");
        assert_eq!(gap.body_bytes, 9);
        assert_eq!(
            gap.body_sha256,
            "15e2b0d3c33891ebb0f1ef609ec419420c20e320ce94c65fbc8c3312448eb225"
        );
    }

    #[test]
    fn direct_tap_resolves_capture_policy_from_switchback_profile_revision() {
        let revision = format!("sha256:{}", "a".repeat(64));
        let authority = CaptureProfileAuthority::from_entries([(
            "configured-profile",
            revision.as_str(),
            "metadata_only",
        )])
        .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            LAUNCH_PROFILE_HEADER,
            HeaderValue::from_static("configured-profile"),
        );
        headers.insert(
            CONFORMANCE_REVISION_HEADER,
            HeaderValue::from_str(&revision).unwrap(),
        );
        headers.insert(
            CAPTURE_POLICY_HEADER,
            HeaderValue::from_static("segmented_full_wire"),
        );

        let resolved = TapCaptureContext::from_headers_with_authority(&headers, &authority);
        assert!(!resolved.captures_payload());
        assert!(resolved.enabled());

        headers.insert(
            CONFORMANCE_REVISION_HEADER,
            HeaderValue::from_static(
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            ),
        );
        headers.insert(CAPTURE_POLICY_HEADER, HeaderValue::from_static("off"));
        let stale = TapCaptureContext::from_headers_with_authority(&headers, &authority);
        assert!(
            stale.captures_payload() && stale.enabled(),
            "stale revision and caller policy must not reduce configured full-wire capture"
        );
    }

    #[test]
    fn unavailable_profile_authority_degrades_body_capture_to_metadata_only() {
        let authority = CaptureProfileAuthority {
            profiles: Arc::new(RwLock::new(HashMap::new())),
            available: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let context = TapCaptureContext::from_headers_with_authority(&HeaderMap::new(), &authority);
        assert!(context.enabled());
        assert!(
            !context.captures_payload(),
            "unreadable Switchback authority must not default to full-wire capture"
        );
        assert_eq!(
            context.merge_metadata(serde_json::json!({}))["capture_authority"],
            "unavailable"
        );
    }

    #[test]
    fn failed_pressure_refresh_retains_last_known_capture_mode() {
        let root = temp_capture_root("pressure-refresh-race");
        let state_dir = root.join("state");
        let archive_root = state_dir.join("body").join("archive");
        fs::create_dir_all(&archive_root).unwrap();
        let logger = BodyLogger::new(sb_bodylog::BodyLoggerConfig {
            state_dir: state_dir.clone(),
            archive_root,
            legacy_jsonl: Some(state_dir.join("tap-bodies.jsonl")),
            inline_threshold_bytes: 1,
        })
        .unwrap();
        assert_eq!(
            logger.pressure_status().unwrap().mode,
            CaptureMode::SegmentedFullWire
        );

        // Reproduce the live race: a backup operation temporarily made the
        // index path unobservable while the background pressure refresh ran.
        // The refresh must not promote this read failure into a writer failure
        // and permanently discard subsequent request/response bodies.
        fs::remove_file(state_dir.join("body/index-v2.sqlite")).unwrap();
        let mut failures = 0;
        refresh_capture_pressure(&logger, &mut failures);

        let status = logger.pressure_status().unwrap();
        assert_eq!(status.mode, CaptureMode::SegmentedFullWire);
        assert_eq!(status.writer_failures, 0);
        assert_eq!(failures, 1);

        // The retry budget is bounded: a persistent inability to inspect the
        // capture store must still fail closed rather than write indefinitely.
        refresh_capture_pressure(&logger, &mut failures);
        assert_eq!(
            logger.pressure_status().unwrap().mode,
            CaptureMode::SegmentedFullWire
        );
        refresh_capture_pressure(&logger, &mut failures);
        let status = logger.pressure_status().unwrap();
        assert_eq!(status.mode, CaptureMode::MetadataOnly);
        assert_eq!(status.writer_failures, 1);
        assert_eq!(failures, TAP_CAPTURE_PRESSURE_FAILURES_TO_DEGRADE);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn tap_capture_retries_a_body_after_the_index_recovers() {
        let root = temp_capture_root("capture-retry");
        let state_dir = root.join("state");
        let archive_root = state_dir.join("body").join("archive");
        fs::create_dir_all(&archive_root).unwrap();
        let logger = BodyLogger::new(sb_bodylog::BodyLoggerConfig {
            state_dir: state_dir.clone(),
            archive_root,
            legacy_jsonl: Some(state_dir.join("tap-bodies.jsonl")),
            inline_threshold_bytes: 1,
        })
        .unwrap();
        let worker = CaptureWorker::new(logger.clone()).unwrap();

        let capture_lock =
            rusqlite::Connection::open(state_dir.join("body/index-v2.sqlite")).unwrap();
        capture_lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
        worker.submit(BodyEventInput {
            request_id: "req-retry".to_string(),
            capture_stage: CaptureStage::ClientInbound,
            protocol: "http".to_string(),
            upstream: Some("https://upstream.invalid".to_string()),
            model: Some("model-test".to_string()),
            status: None,
            content_type: Some("application/json".to_string()),
            metadata: serde_json::json!({"test": "transient-index-lock"}),
            body: br#"{"body":"must-survive"}"#.to_vec(),
        });

        // BodyLogger's busy timeout is 250 ms. Keep the lock beyond it so the
        // first persistence attempt fails, then prove the accepted job is
        // retried instead of being discarded.
        std::thread::sleep(std::time::Duration::from_millis(350));
        capture_lock.execute_batch("ROLLBACK").unwrap();

        let timeout = std::time::Duration::from_secs(3);
        let events = wait_for_body_events(&logger, "req-retry", 1, timeout);
        assert_eq!(events.len(), 1, "an accepted capture job must not be lost");
        assert_eq!(
            logger.read_blob(&events[0].body_sha256).unwrap(),
            br#"{"body":"must-survive"}"#
        );
        let pressure = logger.pressure_status().unwrap();
        assert_eq!(
            pressure.mode,
            CaptureMode::SegmentedFullWire,
            "transient index contention recovered; it must not degrade future captures"
        );
        assert_eq!(pressure.writer_failures, 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn degraded_capture_records_a_gap_without_reserving_payload_queue_bytes() {
        let root = temp_capture_root("capture-pressure-gap");
        let state_dir = root.join("state");
        let archive_root = state_dir.join("body").join("archive");
        fs::create_dir_all(&archive_root).unwrap();
        let logger = BodyLogger::new(sb_bodylog::BodyLoggerConfig {
            state_dir: state_dir.clone(),
            archive_root,
            legacy_jsonl: Some(state_dir.join("tap-bodies.jsonl")),
            inline_threshold_bytes: 1,
        })
        .unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        logger
            .evaluate_pressure_at(
                sb_bodylog::PressureObservation {
                    free_bytes: 40_000_000_000,
                    capacity_bytes: 1_000_000_000_000,
                    last_backup_success_at_unix_ms: Some(now),
                    backup_generation: 1,
                    unbacked_bytes: 0,
                },
                now,
            )
            .unwrap();
        let worker = CaptureWorker::new_pressure_checked(logger.clone()).unwrap();
        worker.submit(BodyEventInput {
            request_id: "req-pressure-gap".to_string(),
            capture_stage: CaptureStage::ClientInbound,
            protocol: "openai".to_string(),
            upstream: Some("https://upstream.invalid".to_string()),
            model: Some("model-test".to_string()),
            status: Some(200),
            content_type: Some("application/json".to_string()),
            metadata: serde_json::json!({"test": "pressure-gap"}),
            body: vec![b'x'; 4 * 1024 * 1024],
        });
        assert_eq!(
            worker.budget.queued_bytes(),
            0,
            "metadata-only admission must happen before reserving payload bytes"
        );

        let events = wait_for_body_events(
            &logger,
            "req-pressure-gap",
            1,
            std::time::Duration::from_secs(3),
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].storage, "metadata_only");
        assert!(logger.read_blob(&events[0].body_sha256).is_err());
        assert_eq!(logger.status().unwrap().blobs, 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn capture_submission_reads_cached_pressure_without_sampling_sqlite() {
        let root = temp_capture_root("capture-pressure-cache");
        let state_dir = root.join("state");
        let archive_root = state_dir.join("body").join("archive");
        fs::create_dir_all(&archive_root).unwrap();
        let logger = BodyLogger::new(sb_bodylog::BodyLoggerConfig {
            state_dir: state_dir.clone(),
            archive_root,
            legacy_jsonl: Some(state_dir.join("tap-bodies.jsonl")),
            inline_threshold_bytes: 1,
        })
        .unwrap();
        let worker = CaptureWorker::new_pressure_checked(logger.clone()).unwrap();
        assert!(
            logger.pressure_status().unwrap().free_bytes.is_none(),
            "the cached controller begins without a filesystem sample"
        );
        let capture_lock =
            rusqlite::Connection::open(state_dir.join("body/index-v2.sqlite")).unwrap();
        capture_lock.execute_batch("BEGIN EXCLUSIVE").unwrap();

        let started = std::time::Instant::now();
        worker.submit(BodyEventInput {
            request_id: "req-pressure-cache".to_string(),
            capture_stage: CaptureStage::ClientInbound,
            protocol: "openai".to_string(),
            upstream: Some("https://upstream.invalid".to_string()),
            model: Some("model-test".to_string()),
            status: None,
            content_type: Some("application/json".to_string()),
            metadata: serde_json::json!({"test": "cached-pressure"}),
            body: b"cached-pressure-admission".to_vec(),
        });
        let admission_elapsed = started.elapsed();

        capture_lock.execute_batch("ROLLBACK").unwrap();
        assert!(
            admission_elapsed < std::time::Duration::from_millis(100),
            "capture admission sampled the locked SQLite projection inline: {admission_elapsed:?}"
        );
        assert!(
            logger.pressure_status().unwrap().free_bytes.is_none(),
            "request admission refreshed pressure instead of reading the background cache"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn capture_worker_does_not_treat_event_metadata_as_policy_authority() {
        let root = temp_capture_root("capture-profile-policy");
        let state_dir = root.join("state");
        let archive_root = state_dir.join("body").join("archive");
        fs::create_dir_all(&archive_root).unwrap();
        let logger = BodyLogger::new(sb_bodylog::BodyLoggerConfig {
            state_dir: state_dir.clone(),
            archive_root,
            legacy_jsonl: Some(state_dir.join("tap-bodies.jsonl")),
            inline_threshold_bytes: 1,
        })
        .unwrap();
        let worker = CaptureWorker::new(logger.clone()).unwrap();

        worker.submit(BodyEventInput {
            request_id: "req-profile-metadata".to_string(),
            capture_stage: CaptureStage::ClientInbound,
            protocol: "openai".to_string(),
            upstream: Some("https://upstream.invalid".to_string()),
            model: Some("model-test".to_string()),
            status: Some(200),
            content_type: Some("application/json".to_string()),
            metadata: serde_json::json!({
                "launch_capture_policy": "metadata_only",
                "launch_profile": "claude-qwen",
            }),
            body: vec![b'x'; 4 * 1024 * 1024],
        });
        worker.submit(BodyEventInput {
            request_id: "req-profile-off".to_string(),
            capture_stage: CaptureStage::ClientInbound,
            protocol: "openai".to_string(),
            upstream: Some("https://upstream.invalid".to_string()),
            model: Some("model-test".to_string()),
            status: Some(200),
            content_type: Some("application/json".to_string()),
            metadata: serde_json::json!({
                "launch_capture_policy": "off",
                "launch_profile": "private-profile",
            }),
            body: vec![b'y'; 4 * 1024 * 1024],
        });
        let metadata_events = wait_for_body_events(
            &logger,
            "req-profile-metadata",
            1,
            std::time::Duration::from_secs(3),
        );
        let off_events = wait_for_body_events(
            &logger,
            "req-profile-off",
            1,
            std::time::Duration::from_secs(3),
        );
        assert_eq!(metadata_events.len(), 1);
        assert_eq!(metadata_events[0].storage, "archive_segment");
        assert_eq!(
            off_events.len(),
            1,
            "arbitrary event metadata must not suppress capture"
        );
        assert_eq!(off_events[0].storage, "archive_segment");
        assert_eq!(logger.status().unwrap().blobs, 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn tap_capture_backpressures_instead_of_dropping_when_the_queue_is_full() {
        let root = temp_capture_root("capture-full-queue");
        let state_dir = root.join("state");
        let archive_root = state_dir.join("body").join("archive");
        fs::create_dir_all(&archive_root).unwrap();
        let logger = BodyLogger::new(sb_bodylog::BodyLoggerConfig {
            state_dir: state_dir.clone(),
            archive_root,
            legacy_jsonl: Some(state_dir.join("tap-bodies.jsonl")),
            inline_threshold_bytes: 1,
        })
        .unwrap();
        let worker = CaptureWorker::new(logger.clone()).unwrap();

        let capture_lock =
            rusqlite::Connection::open(state_dir.join("body/index-v2.sqlite")).unwrap();
        capture_lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let producer = std::thread::spawn(move || {
            for sequence in 0..300 {
                worker.submit(BodyEventInput {
                    request_id: "req-full-queue".to_string(),
                    capture_stage: CaptureStage::ClientInbound,
                    protocol: "websocket".to_string(),
                    upstream: Some("https://upstream.invalid".to_string()),
                    model: Some("model-test".to_string()),
                    status: Some(101),
                    content_type: Some("application/json".to_string()),
                    metadata: serde_json::json!({"sequence": sequence}),
                    body: format!(r#"{{"sequence":{sequence}}}"#).into_bytes(),
                });
            }
        });

        // Let the writer hit its busy timeout and the bounded channel fill.
        // The producer is allowed to block here; returning while discarding
        // jobs is the data-loss bug this test falsifies.
        std::thread::sleep(std::time::Duration::from_millis(350));
        capture_lock.execute_batch("ROLLBACK").unwrap();
        producer.join().unwrap();

        // This test runs alongside other SQLite-heavy tap tests. The single
        // blocking writer deliberately favors bounded, lossless persistence
        // over throughput, so allow suite contention without weakening the
        // 300/300 completion assertion.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let query = || {
            logger
                .query_events(sb_bodylog::BodyEventQuery {
                    request_id: Some("req-full-queue".to_string()),
                    limit: 1000,
                    ..sb_bodylog::BodyEventQuery::default()
                })
                .unwrap()
        };
        let mut events = query();
        while events.len() < 300 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
            events = query();
        }
        assert_eq!(
            events.len(),
            300,
            "bounded capture must slow producers, never discard accepted jobs"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn websocket_frames_are_stored_losslessly_without_one_row_per_frame() {
        let root = temp_capture_root("websocket-chunk");
        let state_dir = root.join("state");
        let archive_root = state_dir.join("body").join("archive");
        fs::create_dir_all(&archive_root).unwrap();
        let logger = BodyLogger::new(sb_bodylog::BodyLoggerConfig {
            state_dir: state_dir.clone(),
            archive_root,
            legacy_jsonl: Some(state_dir.join("tap-bodies.jsonl")),
            inline_threshold_bytes: 1,
        })
        .unwrap();
        let worker = CaptureWorker::new(logger.clone()).unwrap();
        let mut capture = WebSocketCapture::new(
            worker,
            "codex-tap".to_string(),
            "req-ws-chunk".to_string(),
            "https://chatgpt.invalid".to_string(),
            TapCaptureContext::default(),
        );

        for sequence in 1..=8 {
            capture.record_client(&AxumWsMessage::Text(
                serde_json::json!({
                    "type": "response.create",
                    "sequence": sequence,
                    "payload": format!("exact-frame-{sequence}"),
                })
                .to_string()
                .into(),
            ));
        }
        write_websocket_capture(capture, 101);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let events = loop {
            match logger.events_for_request("req-ws-chunk") {
                Ok(events) if events.len() >= 2 || std::time::Instant::now() >= deadline => {
                    break events;
                }
                Ok(_) => {}
                Err(error) if std::time::Instant::now() >= deadline => {
                    panic!("capture index stayed locked until the polling deadline: {error}");
                }
                Err(_) => {}
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };

        assert_eq!(
            events.len(),
            2,
            "one small single-direction session should use one frame chunk plus one summary"
        );
        let chunk = events
            .iter()
            .find(|event| event.metadata["capture_format"] == "websocket_frames_v1")
            .expect("a reconstructable WebSocket frame chunk");
        assert_eq!(chunk.metadata["frame_count"], 8);
        assert_eq!(chunk.metadata["first_sequence"], 1);
        assert_eq!(chunk.metadata["last_sequence"], 8);

        let body = logger.read_blob(&chunk.body_sha256).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let frames = body["frames"].as_array().unwrap();
        assert_eq!(frames.len(), 8);
        assert_eq!(frames[0]["sequence"], 1);
        assert_eq!(frames[0]["body_encoding"], "utf8");
        assert!(frames[0]["body"]
            .as_str()
            .unwrap()
            .contains("exact-frame-1"));
        assert_eq!(frames[7]["sequence"], 8);
        assert!(frames[7]["body"]
            .as_str()
            .unwrap()
            .contains("exact-frame-8"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn tap_body_capture_does_not_delay_runtime_shutdown_when_index_is_busy() {
        let root = temp_capture_root("busy-index-shutdown");
        let state_dir = root.join("state");
        let archive_root = state_dir.join("body").join("archive");
        let legacy_jsonl = state_dir.join("tap-bodies.jsonl");
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (start_tx, start_rx) = std::sync::mpsc::sync_channel(1);
        let (forwarded_tx, forwarded_rx) = std::sync::mpsc::sync_channel(1);
        let (shutdown_tx, shutdown_rx) = std::sync::mpsc::sync_channel(1);

        let worker = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let upstream = Router::new().route(
                    "/v1/messages",
                    post(|| async { Json(serde_json::json!({"ok": true})) }),
                );
                let up_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let up_addr = up_listener.local_addr().unwrap();
                tokio::spawn(async move { axum::serve(up_listener, upstream).await.unwrap() });

                let traces = Arc::new(TraceLog::in_memory(16));
                let cfg = TapConfig {
                    id: "busy-shutdown-tap".to_string(),
                    bind: "127.0.0.1:0".to_string(),
                    upstream: format!("http://{up_addr}"),
                    capture_bodies: true,
                    headers: Default::default(),
                };
                let (tap_app, _bodylog_config) =
                    build_isolated_capture_tap_app(&cfg, traces, &legacy_jsonl, &archive_root);
                let tap_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let tap_addr = tap_listener.local_addr().unwrap();
                tokio::spawn(async move { axum::serve(tap_listener, tap_app).await.unwrap() });
                ready_tx.send(tap_addr).unwrap();

                tokio::task::spawn_blocking(move || start_rx.recv().unwrap())
                    .await
                    .unwrap();
                let response: serde_json::Value = reqwest::Client::new()
                    .post(format!("http://{tap_addr}/v1/messages"))
                    .header("content-type", "application/json")
                    .body(r#"{"model":"claude-x","messages":[]}"#)
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                forwarded_tx.send(response["ok"] == true).unwrap();
            });
            drop(runtime);
            shutdown_tx.send(()).unwrap();
        });

        ready_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let index_path = state_dir.join("body/index-v2.sqlite");
        let capture_lock = rusqlite::Connection::open(index_path).unwrap();
        capture_lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
        start_tx.send(()).unwrap();
        let forwarded = forwarded_rx.recv_timeout(std::time::Duration::from_secs(1));

        let shutdown_was_delayed = shutdown_rx
            .recv_timeout(std::time::Duration::from_millis(250))
            .is_err();
        capture_lock.execute_batch("ROLLBACK").unwrap();
        if shutdown_was_delayed {
            shutdown_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
        }
        worker.join().unwrap();
        let _ = fs::remove_dir_all(root);

        assert!(
            forwarded.unwrap(),
            "the caller must receive the upstream response while capture is backpressured"
        );
        assert!(
            !shutdown_was_delayed,
            "best-effort capture must not keep the Tokio runtime alive under backpressure"
        );
    }

    #[tokio::test]
    async fn tap_does_not_generate_413_for_large_native_request_bodies() {
        let upstream = Router::new()
            .route(
                "/responses",
                post(|body: Bytes| async move {
                    Json(serde_json::json!({
                        "body_len": body.len(),
                    }))
                }),
            )
            .layer(DefaultBodyLimit::disable());
        let up_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(up_listener, upstream).await.unwrap() });

        let traces = Arc::new(TraceLog::in_memory(16));
        let cfg = TapConfig {
            id: "codex-tap".to_string(),
            bind: "127.0.0.1:0".to_string(),
            upstream: format!("http://{up_addr}"),
            capture_bodies: false,
            headers: Default::default(),
        };
        let tap_app = build_tap_app(&cfg, traces.clone(), None);
        let tap_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tap_addr = tap_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(tap_listener, tap_app).await.unwrap() });

        let previous_tap_limit = 64 * 1024 * 1024;
        let payload = Bytes::from(vec![b'a'; previous_tap_limit + 1]);
        let resp = reqwest::Client::new()
            .post(format!("http://{tap_addr}/responses"))
            .body(payload.clone())
            .send()
            .await
            .unwrap();

        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "large native payloads must reach upstream instead of being rejected by the tap"
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["body_len"], payload.len());
    }

    #[tokio::test]
    async fn oversized_direct_bodies_forward_unchanged_and_persist_typed_gaps() {
        let body_bytes = TAP_CAPTURE_BODY_MAX_BYTES + 1;
        let upstream = Router::new()
            .route(
                "/responses",
                post(move |body: Bytes| async move {
                    assert_eq!(body.len(), body_bytes);
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(Body::from(vec![b'r'; body_bytes]))
                        .unwrap()
                }),
            )
            .layer(DefaultBodyLimit::disable());
        let up_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(up_listener, upstream).await.unwrap() });

        let root = temp_capture_root("bounded-direct-bodies");
        let state_dir = root.join("state");
        let archive_root = state_dir.join("body").join("archive");
        let legacy_jsonl = state_dir.join("tap-bodies.jsonl");
        let traces = Arc::new(TraceLog::in_memory(16));
        let cfg = TapConfig {
            id: "bounded-direct-tap".to_string(),
            bind: "127.0.0.1:0".to_string(),
            upstream: format!("http://{up_addr}"),
            capture_bodies: true,
            headers: Default::default(),
        };
        let (tap_app, bodylog_config) =
            build_isolated_capture_tap_app(&cfg, traces, &legacy_jsonl, &archive_root);
        let tap_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tap_addr = tap_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(tap_listener, tap_app).await.unwrap() });

        let response = reqwest::Client::new()
            .post(format!("http://{tap_addr}/responses"))
            .body(vec![b'q'; body_bytes])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.bytes().await.unwrap().len(), body_bytes);

        let logger = sb_bodylog::BodyLogger::open_existing(bodylog_config)
            .unwrap()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let events = loop {
            let events = logger.latest_events(10).unwrap();
            if events.len() >= 2 || std::time::Instant::now() >= deadline {
                break events;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|event| {
            event.storage == "metadata_only"
                && event.body_bytes == body_bytes as u64
                && event.metadata["gap"]["reason"] == "body_limit_exceeded"
        }));
        assert_eq!(logger.status().unwrap().blobs, 0);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn tap_warns_when_sse_stream_closes_before_terminal_event() {
        let upstream = Router::new().route(
            "/responses",
            post(|| async move {
                let chunks = futures::stream::iter(vec![
                    Ok::<_, std::io::Error>(Bytes::from_static(
                        b"event: response.created\ndata: {}\n\n",
                    )),
                    Ok::<_, std::io::Error>(Bytes::from_static(
                        b"event: response.output_text.delta\ndata: {\"delta\":\"hi\"}\n\n",
                    )),
                ]);
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(chunks))
                    .unwrap()
            }),
        );
        let up_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(up_listener, upstream).await.unwrap() });

        let traces = Arc::new(TraceLog::in_memory(16));
        let cfg = TapConfig {
            id: "codex-tap".to_string(),
            bind: "127.0.0.1:0".to_string(),
            upstream: format!("http://{up_addr}"),
            capture_bodies: false,
            headers: Default::default(),
        };
        let tap_app = build_tap_app(&cfg, traces.clone(), None);
        let tap_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tap_addr = tap_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(tap_listener, tap_app).await.unwrap() });

        let text = reqwest::Client::new()
            .post(format!("http://{tap_addr}/responses"))
            .json(&serde_json::json!({"model": "gpt-5", "stream": true}))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        assert!(text.contains("response.created"));
        assert!(text.contains("response.output_text.delta"));

        let recent = traces.recent(8);
        assert_eq!(recent.len(), 1, "the tap recorded one trace");
        assert_eq!(recent[0].final_status, 200);
        assert!(
            recent[0]
                .warnings
                .iter()
                .any(|warning| warning == "upstream_closed_before_terminal"),
            "truncated SSE streams should be visible in the trace"
        );
    }

    #[tokio::test]
    async fn tap_tunnels_websocket_upgrade_and_frames() {
        async fn upstream_ws(headers: HeaderMap, ws: WebSocketUpgrade) -> impl IntoResponse {
            let seen_auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<none>")
                .to_string();
            let seen_beta = headers
                .get("openai-beta")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<none>")
                .to_string();
            let seen_lane = headers
                .get(LANE_ID_HEADER)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<none>")
                .to_string();
            let seen_capture_policy = headers
                .get("x-switchback-capture-policy")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<none>")
                .to_string();

            ws.on_upgrade(move |mut socket| async move {
                if let Some(Ok(AxumWsMessage::Text(text))) = socket.recv().await {
                    let reply = format!(
                        "upstream:{seen_auth}:{seen_beta}:{seen_lane}:{seen_capture_policy}:{text}"
                    );
                    let _ = socket.send(AxumWsMessage::Text(reply.into())).await;
                }
            })
        }

        let upstream = Router::new().route("/backend-api/codex/realtime", any(upstream_ws));
        let up_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(up_listener, upstream).await.unwrap() });

        let root = temp_capture_root("profile-policy-websocket");
        let state_dir = root.join("state");
        let archive_root = state_dir.join("body").join("archive");
        let legacy_jsonl = state_dir.join("tap-bodies.jsonl");
        let traces = Arc::new(TraceLog::in_memory(16));
        let cfg = TapConfig {
            id: "codex-tap".to_string(),
            bind: "127.0.0.1:0".to_string(),
            upstream: format!("http://{up_addr}"),
            capture_bodies: true,
            headers: Default::default(),
        };
        let (tap_app, bodylog_config) =
            build_isolated_capture_tap_app(&cfg, traces.clone(), &legacy_jsonl, &archive_root);
        let tap_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tap_addr = tap_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(tap_listener, tap_app).await.unwrap() });

        let mut request = format!("ws://{tap_addr}/backend-api/codex/realtime?session=abc")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "authorization",
            HeaderValue::from_static("Bearer CLIENT-WS-TOKEN"),
        );
        request
            .headers_mut()
            .insert("openai-beta", HeaderValue::from_static("realtime=v1"));
        request
            .headers_mut()
            .insert(LANE_ID_HEADER, HeaderValue::from_static("gpt56-sol-ultra"));
        request.headers_mut().insert(
            LANE_REVISION_HEADER,
            HeaderValue::from_static(
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ),
        );
        request
            .headers_mut()
            .insert(REQUESTED_EFFORT_HEADER, HeaderValue::from_static("ultra"));
        request.headers_mut().insert(
            "x-switchback-launch-profile",
            HeaderValue::from_static("codex-private"),
        );
        request.headers_mut().insert(
            "x-switchback-capture-policy",
            HeaderValue::from_static("metadata_only"),
        );
        request
            .headers_mut()
            .insert("x-switchback-harness", HeaderValue::from_static("codex"));

        let (mut socket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);

        socket
            .send(TungsteniteMessage::Text(
                serde_json::json!({
                    "type": "response.create",
                    "response": {
                        "model": "gpt-5.6-sol",
                        "reasoning": {"effort": "ultra"}
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let echoed = socket.next().await.unwrap().unwrap();
        assert_eq!(
            echoed.into_text().unwrap(),
            concat!(
                "upstream:Bearer CLIENT-WS-TOKEN:realtime=v1:<none>:<none>:",
                "{\"response\":{\"model\":\"gpt-5.6-sol\",\"reasoning\":{\"effort\":\"ultra\"}},\"type\":\"response.create\"}"
            )
        );
        socket.close(None).await.unwrap();

        // Sending a WebSocket close frame does not wait for the server-side
        // relay task to observe it and finalize its trace.
        let recent = wait_for_traces(&traces, 1).await;
        assert_eq!(recent.len(), 1, "the tap recorded one WebSocket trace");
        assert_eq!(recent[0].route, "tap");
        assert_eq!(recent[0].final_status, 101);
        assert!(recent[0].streamed);
        assert_eq!(recent[0].inbound_model, "gpt-5.6-sol");
        assert_eq!(
            recent[0].native_execution,
            Some(NativeExecutionObservation {
                lane_id: Some("gpt56-sol-ultra".to_string()),
                lane_revision: Some(format!("sha256:{}", "a".repeat(64))),
                launch_profile: Some("codex-private".to_string()),
                conformance_revision: None,
                harness: Some("codex".to_string()),
                requested_effort: Some("ultra".to_string()),
                observed_effort: Some("ultra".to_string()),
                observed_effort_path: Some("/response/reasoning/effort".to_string()),
            })
        );

        let logger = sb_bodylog::BodyLogger::open_existing(bodylog_config)
            .unwrap()
            .expect("tap body logger created the WebSocket index");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut events = logger.latest_events(10).unwrap();
        while events.len() < 3 && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            events = logger.latest_events(10).unwrap();
        }
        assert_eq!(
            events.len(),
            3,
            "an untrusted metadata-only claim cannot suppress either frame direction or the session summary"
        );
        assert!(events
            .iter()
            .all(|event| event.storage == "archive_segment"));
        assert!(events.iter().all(|event| {
            event.metadata["launch_profile"] == "codex-private"
                && event.metadata["launch_capture_policy"].is_null()
                && event.metadata["launch_capture_policy_claimed"] == "metadata_only"
        }));
        assert_eq!(logger.status().unwrap().blobs, 3);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn tap_records_websocket_upstream_close_code_and_reason() {
        async fn upstream_ws(ws: WebSocketUpgrade) -> impl IntoResponse {
            ws.on_upgrade(move |mut socket| async move {
                if let Some(Ok(AxumWsMessage::Text(_))) = socket.recv().await {
                    let _ = socket
                        .send(AxumWsMessage::Close(Some(CloseFrame {
                            code: close_code::ERROR,
                            reason: "backend_overloaded".into(),
                        })))
                        .await;
                }
            })
        }

        let upstream = Router::new().route("/backend-api/codex/realtime", any(upstream_ws));
        let up_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(up_listener, upstream).await.unwrap() });

        let traces = Arc::new(TraceLog::in_memory(16));
        let cfg = TapConfig {
            id: "codex-tap".to_string(),
            bind: "127.0.0.1:0".to_string(),
            upstream: format!("http://{up_addr}"),
            capture_bodies: false,
            headers: Default::default(),
        };
        let tap_app = build_tap_app(&cfg, traces.clone(), None);
        let tap_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tap_addr = tap_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(tap_listener, tap_app).await.unwrap() });

        let (mut socket, response) =
            tokio_tungstenite::connect_async(format!("ws://{tap_addr}/backend-api/codex/realtime"))
                .await
                .unwrap();
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);

        socket
            .send(TungsteniteMessage::Text("client-ping".into()))
            .await
            .unwrap();
        let close = socket.next().await.unwrap().unwrap();
        match close {
            TungsteniteMessage::Close(Some(frame)) => {
                assert_eq!(u16::from(frame.code), close_code::ERROR);
                assert_eq!(frame.reason, "backend_overloaded");
            }
            other => panic!("expected upstream close frame, got {other:?}"),
        }

        let recent = wait_for_traces(&traces, 1).await;
        assert_eq!(recent.len(), 1, "the tap recorded one WebSocket trace");
        assert_eq!(recent[0].final_status, 101);
        assert!(
            recent[0]
                .warnings
                .iter()
                .any(|warning| warning == "websocket_upstream_closed:1011:backend_overloaded"),
            "upstream WebSocket close metadata should be visible in the trace: {recent:?}"
        );
    }
}
