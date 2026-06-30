use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sb_core::{
    ArtifactKind, ArtifactRecord, ComfyUiWorkflowConfig, Config, ImageGenerationRequest, JobEvent,
    JobRecord, JobStatus, Json, ProviderKind, RouteDecision, TargetRef, WorkflowField,
    WorkflowTemplate, WorkloadKind,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::time::sleep;

#[derive(Debug)]
pub enum WorkloadError {
    InvalidRequest(String),
    Upstream(String),
    Internal(String),
}

impl WorkloadError {
    pub fn message(&self) -> &str {
        match self {
            Self::InvalidRequest(message) | Self::Upstream(message) | Self::Internal(message) => {
                message
            }
        }
    }
}

#[derive(Clone, Default)]
pub struct WorkloadStore {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    jobs: HashMap<String, JobRecord>,
    artifacts: HashMap<String, StoredArtifact>,
}

#[derive(Clone)]
pub struct StoredArtifact {
    pub record: ArtifactRecord,
    pub bytes: Vec<u8>,
}

impl WorkloadStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn create_image_job(
        &self,
        cfg: &Config,
        request: ImageGenerationRequest,
        tenant: Option<String>,
        project: Option<String>,
    ) -> Result<JobRecord, WorkloadError> {
        if let Some((provider, workflow)) = resolve_comfyui_workflow(cfg, &request.model) {
            return self
                .create_comfyui_image_job(cfg, provider, workflow, request, tenant, project)
                .await;
        }
        self.create_mock_image_job(request, tenant, project)
    }

    pub fn workflows(&self, cfg: &Config) -> Vec<WorkflowTemplate> {
        let mut workflows = vec![WorkflowTemplate {
            id: "mock-image".to_string(),
            kind: WorkloadKind::ImageGeneration,
            provider: "switchback-mock".to_string(),
            version: "2026-06-30".to_string(),
            inputs: vec![
                WorkflowField {
                    name: "prompt".to_string(),
                    field_type: "string".to_string(),
                    required: true,
                    media_type: None,
                },
                WorkflowField {
                    name: "size".to_string(),
                    field_type: "string".to_string(),
                    required: false,
                    media_type: None,
                },
                WorkflowField {
                    name: "seed".to_string(),
                    field_type: "integer".to_string(),
                    required: false,
                    media_type: None,
                },
            ],
            outputs: vec![WorkflowField {
                name: "image".to_string(),
                field_type: "artifact".to_string(),
                required: true,
                media_type: Some("image/png".to_string()),
            }],
        }];
        for provider in &cfg.providers {
            let ProviderKind::ComfyUi {
                workflows: provider_workflows,
                ..
            } = &provider.kind
            else {
                continue;
            };
            workflows.extend(provider_workflows.iter().map(|workflow| {
                WorkflowTemplate {
                    id: format!("{}/{}", provider.id, workflow.id),
                    kind: workflow.kind,
                    provider: provider.id.clone(),
                    version: workflow
                        .version
                        .clone()
                        .unwrap_or_else(|| "configured".to_string()),
                    inputs: workflow_fields_for(workflow.kind),
                    outputs: workflow_outputs_for(workflow.kind),
                }
            }));
        }
        workflows
    }

    pub fn create_mock_image_job(
        &self,
        request: ImageGenerationRequest,
        tenant: Option<String>,
        project: Option<String>,
    ) -> Result<JobRecord, WorkloadError> {
        if request.prompt.trim().is_empty() {
            return Err(WorkloadError::InvalidRequest(
                "prompt is required".to_string(),
            ));
        }
        if request.n == 0 {
            return Err(WorkloadError::InvalidRequest(
                "n must be at least 1".to_string(),
            ));
        }
        if request.n > 1 {
            return Err(WorkloadError::InvalidRequest(
                "mock image workflow supports n=1 in this slice".to_string(),
            ));
        }

        let now = now_ms();
        let job_id = sb_core::new_id("job");
        let artifact_id = sb_core::new_id("art");
        let bytes = mock_png();
        let sha256 = sha256_hex(&bytes);
        let (width, height) = parse_size(request.size.as_deref()).unwrap_or((1, 1));

        let mut provenance = BTreeMap::new();
        provenance.insert("provider".to_string(), "switchback-mock".to_string());
        provenance.insert("model".to_string(), request.model.clone());
        provenance.insert("workflow".to_string(), "mock-image".to_string());

        let artifact = ArtifactRecord {
            artifact_id: artifact_id.clone(),
            job_id: job_id.clone(),
            kind: ArtifactKind::Image,
            media_type: "image/png".to_string(),
            bytes: bytes.len() as u64,
            sha256,
            storage_ref: format!("memory://{artifact_id}"),
            width: Some(width),
            height: Some(height),
            duration_ms: None,
            fps: None,
            created_at_ms: now,
            retention: "process_memory".to_string(),
            provenance,
        };

        let mut decision = RouteDecision::new(job_id.clone(), "workload/mock_image");
        decision.selected = Some(TargetRef::new(request.model.clone()));
        decision.add_reason("workload=image_generation");
        decision.add_reason("workflow=mock-image");
        decision.add_reason("adapter=mock");

        let mut accepted = BTreeMap::new();
        accepted.insert("model".to_string(), request.model.clone());
        accepted.insert("prompt_stored".to_string(), "false".to_string());
        let mut artifact_detail = BTreeMap::new();
        artifact_detail.insert("artifact_id".to_string(), artifact_id.clone());
        artifact_detail.insert("media_type".to_string(), "image/png".to_string());

        let events = vec![
            JobEvent {
                event: "accepted".to_string(),
                status: JobStatus::Accepted,
                created_at_ms: now,
                detail: accepted,
            },
            JobEvent {
                event: "artifact_ready".to_string(),
                status: JobStatus::Running,
                created_at_ms: now,
                detail: artifact_detail,
            },
            JobEvent {
                event: "succeeded".to_string(),
                status: JobStatus::Succeeded,
                created_at_ms: now,
                detail: BTreeMap::new(),
            },
        ];

        let job = JobRecord {
            id: job_id,
            kind: WorkloadKind::ImageGeneration,
            status: JobStatus::Succeeded,
            target: request.model,
            tenant,
            project,
            created_at_ms: now,
            updated_at_ms: now,
            route_decision: decision,
            artifacts: vec![artifact.clone()],
            events,
            prompt_stored: false,
        };

        let mut inner = self
            .inner
            .lock()
            .map_err(|_| WorkloadError::Internal("workload store lock poisoned".to_string()))?;
        inner.artifacts.insert(
            artifact_id,
            StoredArtifact {
                record: artifact,
                bytes,
            },
        );
        inner.jobs.insert(job.id.clone(), job.clone());
        Ok(job)
    }

    async fn create_comfyui_image_job(
        &self,
        cfg: &Config,
        provider: &sb_core::ProviderConfig,
        workflow: &ComfyUiWorkflowConfig,
        request: ImageGenerationRequest,
        tenant: Option<String>,
        project: Option<String>,
    ) -> Result<JobRecord, WorkloadError> {
        if request.prompt.trim().is_empty() {
            return Err(WorkloadError::InvalidRequest(
                "prompt is required".to_string(),
            ));
        }
        if request.n == 0 {
            return Err(WorkloadError::InvalidRequest(
                "n must be at least 1".to_string(),
            ));
        }
        if request.n > 1 {
            return Err(WorkloadError::InvalidRequest(
                "comfyui image workflow supports n=1 per request".to_string(),
            ));
        }

        let ProviderKind::ComfyUi { base_url, .. } = &provider.kind else {
            return Err(WorkloadError::InvalidRequest(format!(
                "provider `{}` is not a comfyui provider",
                provider.id
            )));
        };
        let mut graph = workflow.graph.clone();
        bind_comfyui_image_inputs(&mut graph, workflow, &request)?;

        let now = now_ms();
        let job_id = sb_core::new_id("job");
        let client_id = format!("switchback-{job_id}");
        let queue_url = comfy_endpoint(base_url, "prompt")?;
        guard_provider_url(cfg, queue_url.as_str()).await?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| WorkloadError::Internal(format!("build comfyui client: {e}")))?;

        let mut accepted = BTreeMap::new();
        accepted.insert("model".to_string(), request.model.clone());
        accepted.insert("provider".to_string(), provider.id.clone());
        accepted.insert("workflow".to_string(), workflow.id.clone());
        accepted.insert("prompt_stored".to_string(), "false".to_string());
        let mut events = vec![JobEvent {
            event: "accepted".to_string(),
            status: JobStatus::Accepted,
            created_at_ms: now,
            detail: accepted,
        }];

        let queued: ComfyPromptResponse = send_json(
            client
                .post(queue_url)
                .json(&serde_json::json!({"prompt": graph, "client_id": client_id})),
            "queue comfyui prompt",
        )
        .await?;
        if queued
            .node_errors
            .as_ref()
            .is_some_and(|value| !json_is_empty(value))
        {
            return Err(WorkloadError::Upstream(
                "comfyui rejected workflow graph with node_errors".to_string(),
            ));
        }
        let mut queued_detail = BTreeMap::new();
        queued_detail.insert("prompt_id".to_string(), queued.prompt_id.clone());
        queued_detail.insert(
            "number".to_string(),
            queued.number.unwrap_or_default().to_string(),
        );
        events.push(JobEvent {
            event: "queued".to_string(),
            status: JobStatus::Queued,
            created_at_ms: now_ms(),
            detail: queued_detail,
        });

        let outputs = poll_comfyui_history(
            cfg,
            &client,
            base_url,
            &queued.prompt_id,
            &workflow.output_node_ids,
        )
        .await?;
        let mut poll_detail = BTreeMap::new();
        poll_detail.insert("prompt_id".to_string(), queued.prompt_id.clone());
        poll_detail.insert("outputs".to_string(), outputs.len().to_string());
        events.push(JobEvent {
            event: "history_polled".to_string(),
            status: JobStatus::Running,
            created_at_ms: now_ms(),
            detail: poll_detail,
        });

        let mut records = Vec::new();
        let mut stored = Vec::new();
        for output in outputs {
            let view_url = comfy_endpoint(base_url, "view")?;
            guard_provider_url(cfg, view_url.as_str()).await?;
            let response = client
                .get(view_url)
                .query(&[
                    ("filename", output.filename.as_str()),
                    ("subfolder", output.subfolder.as_str()),
                    ("type", output.type_.as_str()),
                ])
                .send()
                .await
                .map_err(|e| WorkloadError::Upstream(format!("fetch comfyui artifact: {e}")))?;
            let status = response.status();
            if !status.is_success() {
                return Err(WorkloadError::Upstream(format!(
                    "fetch comfyui artifact failed with status {status}"
                )));
            }
            let media_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
                .unwrap_or_else(|| media_type_for_filename(&output.filename).to_string());
            let bytes = response
                .bytes()
                .await
                .map_err(|e| WorkloadError::Upstream(format!("read comfyui artifact: {e}")))?
                .to_vec();
            let artifact_id = sb_core::new_id("art");
            let mut provenance = BTreeMap::new();
            provenance.insert("provider".to_string(), provider.id.clone());
            provenance.insert("workflow".to_string(), workflow.id.clone());
            provenance.insert("prompt_id".to_string(), queued.prompt_id.clone());
            provenance.insert("node_id".to_string(), output.node_id.clone());
            provenance.insert("filename".to_string(), output.filename.clone());
            provenance.insert("subfolder".to_string(), output.subfolder.clone());
            provenance.insert("type".to_string(), output.type_.clone());
            let record = ArtifactRecord {
                artifact_id: artifact_id.clone(),
                job_id: job_id.clone(),
                kind: artifact_kind_for_media_type(&media_type),
                media_type: media_type.clone(),
                bytes: bytes.len() as u64,
                sha256: sha256_hex(&bytes),
                storage_ref: format!("memory://{artifact_id}"),
                width: None,
                height: None,
                duration_ms: None,
                fps: None,
                created_at_ms: now_ms(),
                retention: "process_memory".to_string(),
                provenance,
            };
            let mut artifact_detail = BTreeMap::new();
            artifact_detail.insert("artifact_id".to_string(), artifact_id.clone());
            artifact_detail.insert("media_type".to_string(), media_type);
            events.push(JobEvent {
                event: "artifact_ready".to_string(),
                status: JobStatus::Running,
                created_at_ms: now_ms(),
                detail: artifact_detail,
            });
            records.push(record.clone());
            stored.push(StoredArtifact { record, bytes });
        }

        if records.is_empty() {
            return Err(WorkloadError::Upstream(
                "comfyui history completed without artifacts".to_string(),
            ));
        }
        events.push(JobEvent {
            event: "succeeded".to_string(),
            status: JobStatus::Succeeded,
            created_at_ms: now_ms(),
            detail: BTreeMap::new(),
        });

        let mut decision = RouteDecision::new(job_id.clone(), "workload/comfyui");
        decision.selected = Some(TargetRef::new(request.model.clone()));
        decision.add_reason("workload=image_generation");
        decision.add_reason(format!("provider={}", provider.id));
        decision.add_reason(format!("workflow={}", workflow.id));
        decision.add_reason("adapter=comfyui");
        let job = JobRecord {
            id: job_id,
            kind: WorkloadKind::ImageGeneration,
            status: JobStatus::Succeeded,
            target: request.model,
            tenant,
            project,
            created_at_ms: now,
            updated_at_ms: now_ms(),
            route_decision: decision,
            artifacts: records,
            events,
            prompt_stored: false,
        };
        self.insert_job(job, stored)
    }

    fn insert_job(
        &self,
        job: JobRecord,
        artifacts: Vec<StoredArtifact>,
    ) -> Result<JobRecord, WorkloadError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| WorkloadError::Internal("workload store lock poisoned".to_string()))?;
        for artifact in artifacts {
            inner
                .artifacts
                .insert(artifact.record.artifact_id.clone(), artifact);
        }
        inner.jobs.insert(job.id.clone(), job.clone());
        Ok(job)
    }

    pub fn job(&self, id: &str) -> Option<JobRecord> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.jobs.get(id).cloned())
    }

    pub fn artifact(&self, id: &str) -> Option<StoredArtifact> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.artifacts.get(id).cloned())
    }
}

#[derive(Debug, Deserialize)]
struct ComfyPromptResponse {
    prompt_id: String,
    #[serde(default)]
    number: Option<u64>,
    #[serde(default)]
    node_errors: Option<Json>,
}

#[derive(Debug, Clone)]
struct ComfyOutputRef {
    node_id: String,
    filename: String,
    subfolder: String,
    type_: String,
}

fn resolve_comfyui_workflow<'a>(
    cfg: &'a Config,
    model: &str,
) -> Option<(&'a sb_core::ProviderConfig, &'a ComfyUiWorkflowConfig)> {
    let (provider_id, workflow_id) = model.split_once('/')?;
    let provider = cfg
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)?;
    let ProviderKind::ComfyUi { workflows, .. } = &provider.kind else {
        return None;
    };
    let workflow = workflows
        .iter()
        .find(|workflow| workflow.id == workflow_id)?;
    Some((provider, workflow))
}

fn workflow_fields_for(kind: WorkloadKind) -> Vec<WorkflowField> {
    match kind {
        WorkloadKind::ImageGeneration | WorkloadKind::WorkflowExecution => vec![
            WorkflowField {
                name: "prompt".to_string(),
                field_type: "string".to_string(),
                required: true,
                media_type: None,
            },
            WorkflowField {
                name: "size".to_string(),
                field_type: "string".to_string(),
                required: false,
                media_type: None,
            },
            WorkflowField {
                name: "seed".to_string(),
                field_type: "integer".to_string(),
                required: false,
                media_type: None,
            },
        ],
        WorkloadKind::TextGeneration | WorkloadKind::Embedding | WorkloadKind::VideoGeneration => {
            Vec::new()
        }
    }
}

fn workflow_outputs_for(kind: WorkloadKind) -> Vec<WorkflowField> {
    match kind {
        WorkloadKind::ImageGeneration | WorkloadKind::WorkflowExecution => vec![WorkflowField {
            name: "artifact".to_string(),
            field_type: "artifact".to_string(),
            required: true,
            media_type: None,
        }],
        WorkloadKind::TextGeneration | WorkloadKind::Embedding | WorkloadKind::VideoGeneration => {
            Vec::new()
        }
    }
}

fn bind_comfyui_image_inputs(
    graph: &mut Json,
    workflow: &ComfyUiWorkflowConfig,
    request: &ImageGenerationRequest,
) -> Result<(), WorkloadError> {
    bind_required(
        graph,
        workflow,
        "prompt",
        Json::String(request.prompt.clone()),
    )?;
    if let Some(seed) = request.seed {
        bind_optional(graph, workflow, "seed", Json::Number(seed.into()))?;
    }
    if let Some((width, height)) = parse_size(request.size.as_deref()) {
        bind_optional(graph, workflow, "width", Json::Number(width.into()))?;
        bind_optional(graph, workflow, "height", Json::Number(height.into()))?;
    }
    for (key, value) in &request.metadata {
        bind_optional(graph, workflow, key, Json::String(value.clone()))?;
    }
    Ok(())
}

fn bind_required(
    graph: &mut Json,
    workflow: &ComfyUiWorkflowConfig,
    name: &str,
    value: Json,
) -> Result<(), WorkloadError> {
    let binding = workflow.bindings.get(name).ok_or_else(|| {
        WorkloadError::InvalidRequest(format!(
            "workflow `{}` missing required `{name}` binding",
            workflow.id
        ))
    })?;
    set_json_path(graph, &binding.path, value)
}

fn bind_optional(
    graph: &mut Json,
    workflow: &ComfyUiWorkflowConfig,
    name: &str,
    value: Json,
) -> Result<(), WorkloadError> {
    let Some(binding) = workflow.bindings.get(name) else {
        return Ok(());
    };
    set_json_path(graph, &binding.path, value)
}

fn set_json_path(target: &mut Json, path: &[String], value: Json) -> Result<(), WorkloadError> {
    if path.is_empty() {
        return Err(WorkloadError::InvalidRequest(
            "binding path must not be empty".to_string(),
        ));
    }
    let mut cursor = target;
    for segment in &path[..path.len() - 1] {
        cursor = cursor.get_mut(segment).ok_or_else(|| {
            WorkloadError::InvalidRequest(format!("binding path segment `{segment}` not found"))
        })?;
    }
    let last = path.last().expect("non-empty path");
    let object = cursor.as_object_mut().ok_or_else(|| {
        WorkloadError::InvalidRequest(format!("binding parent for `{last}` is not an object"))
    })?;
    object.insert(last.clone(), value);
    Ok(())
}

fn comfy_endpoint(base_url: &str, path: &str) -> Result<reqwest::Url, WorkloadError> {
    let normalized = format!("{}/", base_url.trim_end_matches('/'));
    reqwest::Url::parse(&normalized)
        .and_then(|base| base.join(path))
        .map_err(|e| WorkloadError::InvalidRequest(format!("invalid comfyui base_url: {e}")))
}

async fn guard_provider_url(cfg: &Config, url: &str) -> Result<(), WorkloadError> {
    sb_net::guard_url(
        url,
        sb_net::NetworkUrlKind::ProviderUpstream,
        cfg.server.block_private_networks,
    )
    .await
    .map_err(|e| WorkloadError::InvalidRequest(e.to_string()))
}

async fn send_json<T: for<'de> Deserialize<'de>>(
    request: reqwest::RequestBuilder,
    action: &str,
) -> Result<T, WorkloadError> {
    let response = request
        .send()
        .await
        .map_err(|e| WorkloadError::Upstream(format!("{action}: {e}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(WorkloadError::Upstream(format!(
            "{action} failed with status {status}"
        )));
    }
    response
        .json::<T>()
        .await
        .map_err(|e| WorkloadError::Upstream(format!("{action}: invalid JSON: {e}")))
}

async fn poll_comfyui_history(
    cfg: &Config,
    client: &reqwest::Client,
    base_url: &str,
    prompt_id: &str,
    output_node_ids: &[String],
) -> Result<Vec<ComfyOutputRef>, WorkloadError> {
    let history_url = comfy_endpoint(base_url, &format!("history/{prompt_id}"))?;
    guard_provider_url(cfg, history_url.as_str()).await?;
    for _ in 0..60 {
        let history: Json =
            send_json(client.get(history_url.clone()), "poll comfyui history").await?;
        let outputs = extract_comfy_outputs(&history, prompt_id, output_node_ids);
        if !outputs.is_empty() {
            return Ok(outputs);
        }
        sleep(Duration::from_millis(500)).await;
    }
    Err(WorkloadError::Upstream(format!(
        "comfyui prompt `{prompt_id}` did not produce artifacts before timeout"
    )))
}

fn extract_comfy_outputs(
    history: &Json,
    prompt_id: &str,
    output_node_ids: &[String],
) -> Vec<ComfyOutputRef> {
    let Some(job) = history.get(prompt_id).or_else(|| {
        if history.get("outputs").is_some() {
            Some(history)
        } else {
            None
        }
    }) else {
        return Vec::new();
    };
    let Some(outputs) = job.get("outputs").and_then(Json::as_object) else {
        return Vec::new();
    };

    let mut refs = Vec::new();
    if !output_node_ids.is_empty() {
        for node_id in output_node_ids {
            if let Some(node) = outputs.get(node_id) {
                collect_output_refs(node_id, node, &mut refs);
            }
        }
    }
    if refs.is_empty() {
        for (node_id, node) in outputs {
            collect_output_refs(node_id, node, &mut refs);
        }
    }
    refs
}

fn collect_output_refs(node_id: &str, node: &Json, refs: &mut Vec<ComfyOutputRef>) {
    for key in ["images", "gifs", "videos"] {
        let Some(items) = node.get(key).and_then(Json::as_array) else {
            continue;
        };
        for item in items {
            let Some(filename) = item.get("filename").and_then(Json::as_str) else {
                continue;
            };
            refs.push(ComfyOutputRef {
                node_id: node_id.to_string(),
                filename: filename.to_string(),
                subfolder: item
                    .get("subfolder")
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .to_string(),
                type_: item
                    .get("type")
                    .and_then(Json::as_str)
                    .unwrap_or("output")
                    .to_string(),
            });
        }
    }
}

fn json_is_empty(value: &Json) -> bool {
    match value {
        Json::Null => true,
        Json::Object(map) => map.is_empty(),
        Json::Array(items) => items.is_empty(),
        _ => false,
    }
}

fn media_type_for_filename(filename: &str) -> &'static str {
    match filename
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
    {
        Some(ext) if ext == "jpg" || ext == "jpeg" => "image/jpeg",
        Some(ext) if ext == "webp" => "image/webp",
        Some(ext) if ext == "gif" => "image/gif",
        Some(ext) if ext == "mp4" => "video/mp4",
        Some(ext) if ext == "webm" => "video/webm",
        Some(ext) if ext == "mov" => "video/quicktime",
        _ => "image/png",
    }
}

fn artifact_kind_for_media_type(media_type: &str) -> ArtifactKind {
    if media_type.starts_with("video/") {
        ArtifactKind::Video
    } else if media_type.starts_with("audio/") {
        ArtifactKind::Audio
    } else if media_type.starts_with("image/") {
        ArtifactKind::Image
    } else {
        ArtifactKind::File
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn parse_size(size: Option<&str>) -> Option<(u32, u32)> {
    let size = size?;
    let (width, height) = size.split_once('x')?;
    Some((width.parse().ok()?, height.parse().ok()?))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn mock_png() -> Vec<u8> {
    vec![
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 10, 73, 68, 65, 84, 120, 156, 99, 0, 1, 0, 0, 5, 0, 1,
        13, 10, 45, 180, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ]
}
