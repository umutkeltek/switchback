//! Provider-agnostic workload/job/artifact IR for non-text execution planes.
//! Text generation keeps using [`crate::AiRequest`]; this module is the small
//! interface for async image/video/workflow work without diluting chat IR.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::{Json, RouteDecision};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadKind {
    TextGeneration,
    Embedding,
    ImageGeneration,
    VideoGeneration,
    WorkflowExecution,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Accepted,
    Queued,
    Routing,
    Leased,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Image,
    Video,
    Audio,
    File,
    Thumbnail,
    Metadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageGenerationRequest {
    pub model: String,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<String>,
    #[serde(default = "default_one")]
    pub n: u32,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

fn default_one() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRunRequest {
    pub workflow_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default)]
    pub inputs: BTreeMap<String, Json>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "spec", rename_all = "snake_case")]
pub enum WorkloadSpec {
    ImageGeneration(ImageGenerationRequest),
    WorkflowExecution(WorkflowRunRequest),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkloadRequest {
    pub id: String,
    pub kind: WorkloadKind,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    pub spec: WorkloadSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub artifact_id: String,
    pub job_id: String,
    pub kind: ArtifactKind,
    pub media_type: String,
    pub bytes: u64,
    pub sha256: String,
    pub storage_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<f64>,
    pub created_at_ms: u64,
    pub retention: String,
    #[serde(default)]
    pub provenance: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobEvent {
    pub event: String,
    pub status: JobStatus,
    pub created_at_ms: u64,
    #[serde(default)]
    pub detail: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: String,
    pub kind: WorkloadKind,
    pub status: JobStatus,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub route_decision: RouteDecision,
    #[serde(default)]
    pub artifacts: Vec<ArtifactRecord>,
    #[serde(default)]
    pub events: Vec<JobEvent>,
    /// Explicit receipt that prompt/raw graph bodies are not persisted in job
    /// metadata. Artifact bytes live separately behind artifact endpoints.
    pub prompt_stored: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowTemplate {
    pub id: String,
    pub kind: WorkloadKind,
    pub provider: String,
    pub version: String,
    #[serde(default)]
    pub inputs: Vec<WorkflowField>,
    #[serde(default)]
    pub outputs: Vec<WorkflowField>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowField {
    pub name: String,
    pub field_type: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
}
