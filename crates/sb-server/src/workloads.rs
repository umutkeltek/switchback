use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use sb_core::{
    ArtifactKind, ArtifactRecord, ImageGenerationRequest, JobEvent, JobRecord, JobStatus,
    RouteDecision, TargetRef, WorkflowField, WorkflowTemplate, WorkloadKind,
};
use sha2::{Digest, Sha256};

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

    pub fn workflows(&self) -> Vec<WorkflowTemplate> {
        vec![WorkflowTemplate {
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
        }]
    }

    pub fn create_mock_image_job(
        &self,
        request: ImageGenerationRequest,
        tenant: Option<String>,
        project: Option<String>,
    ) -> Result<JobRecord, String> {
        if request.prompt.trim().is_empty() {
            return Err("prompt is required".to_string());
        }
        if request.n == 0 {
            return Err("n must be at least 1".to_string());
        }
        if request.n > 1 {
            return Err("mock image workflow supports n=1 in this slice".to_string());
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
            .map_err(|_| "workload store lock poisoned".to_string())?;
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
