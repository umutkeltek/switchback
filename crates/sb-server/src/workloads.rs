use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sb_core::{
    ArtifactKind, ArtifactRecord, ComfyUiWorkflowConfig, Config, CredentialLease,
    ImageGenerationRequest, JobEvent, JobRecord, JobStatus, Json, PricingUnit, ProviderKind,
    RouteDecision, TargetRef, UnitPrice, WorkflowField, WorkflowTemplate, WorkloadKind,
};
use sb_credentials::{CredentialResolver, ResolveOutcome};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::time::sleep;

use crate::activator::{CapacityActivator, JobError, LocalExecutor};
use crate::artifact_store::{ArtifactStore, Owner};
use crate::scheme_auth::{error_text, redirect_policy, refused, with_credential, SchemeCredential};

#[derive(Debug)]
pub enum WorkloadError {
    InvalidRequest(String),
    /// The tenant has `server.jobs.max_pending_per_tenant` background jobs
    /// queued or running already (429).
    TooManyJobs(String),
    Upstream(String),
    Internal(String),
    /// A managed lane stayed full (`max_in_flight`) for its whole
    /// `queue_timeout_secs` (503 `local_lane_queue_timeout`, `Retry-After`).
    LaneQueueTimeout {
        message: String,
        retry_after_secs: u64,
    },
}

impl WorkloadError {
    pub fn message(&self) -> &str {
        match self {
            Self::InvalidRequest(message)
            | Self::TooManyJobs(message)
            | Self::Upstream(message)
            | Self::Internal(message)
            | Self::LaneQueueTimeout { message, .. } => message,
        }
    }
}

impl std::fmt::Display for WorkloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

#[derive(Clone)]
pub struct WorkloadStore {
    inner: Arc<Mutex<Inner>>,
    artifacts: ArtifactStore,
}

impl Default for WorkloadStore {
    /// Artifacts in process memory under the default memory caps (a library
    /// caller; the gateway attaches the configured store, see
    /// [`WorkloadStore::with_artifacts`]).
    fn default() -> Self {
        Self::with_artifacts(ArtifactStore::from_config(
            &sb_core::ArtifactsConfig {
                storage: sb_core::ArtifactStorage::Memory,
                ..sb_core::ArtifactsConfig::default()
            },
            None,
        ))
    }
}

#[derive(Default)]
struct Inner {
    jobs: HashMap<String, JobRecord>,
    fal_runs: HashMap<String, FalRunControl>,
    /// Ids of the failed jobs kept in `jobs`, oldest first (see
    /// [`KEPT_FAILED_COMFY_JOBS`]).
    failed_comfy: VecDeque<String>,
    /// The task of each unfinished job, blocking or background.
    tasks: HashMap<String, JobTask>,
    /// The ComfyUI prompt each running ComfyUI job queued, for cancel.
    comfy_prompts: HashMap<String, ComfyRun>,
    /// Jobs whose ComfyUI `/prompt` call is in flight: ComfyUI may have
    /// queued the prompt while its id is not known yet. A cancel then leaves
    /// the task running; the task stops the prompt itself once the id
    /// arrives, and only then releases its lane.
    submitting: HashSet<String>,
}

/// How many failed ComfyUI jobs `/v1/jobs` keeps; the oldest go first. A
/// client that retries a failing job in a loop (a refused credential fails
/// at once) for a month cannot fill memory with them.
const KEPT_FAILED_COMFY_JOBS: usize = 256;

/// How many jobs the store keeps, in memory and on disk; the oldest
/// finished ones go first (down to nine tenths, so the trim is rare).
const KEPT_JOBS: usize = 10_000;

/// Why a job that was not finished at start-up is failed: jobs are not
/// resumed after a restart.
const RESTART_REASON: &str =
    "the gateway restarted before the job finished; the job was not resumed";

/// Cancels a blocking request's job when the request is dropped before the
/// job ends (see [`WorkloadStore::create_image_job`]).
struct CancelOnDrop {
    store: WorkloadStore,
    snapshot: Arc<sb_runtime::Snapshot>,
    /// `None` once the job ended.
    job_id: Option<String>,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let Some(id) = self.job_id.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let store = self.store.clone();
        let snapshot = self.snapshot.clone();
        runtime.spawn(async move {
            if let Err(error) = store
                .cancel_job_from(&snapshot.config, &snapshot.resolver, &id, "client_gone")
                .await
            {
                // A job that ended meanwhile cannot be cancelled: nothing to do.
                tracing::debug!(job = %id, %error, "blocking job not cancelled after its client left");
            }
        });
    }
}

/// What [`WorkloadStore::begin_cancel`] decided.
enum CancelPlan {
    /// Already cancelled.
    Done(JobRecord),
    /// A fal job: cancel it at fal.
    Fal(JobRecord, FalRunControl),
    /// Marked cancelled. Without a queued ComfyUI prompt its task is
    /// stopped already; with one, the prompt is to be stopped first and the
    /// task (holding the lane until then) after: dropping the guard stops it.
    Stopped(Option<ComfyRun>, AbortOnDrop),
}

/// Stops a job's task when dropped, so a cancel whose caller goes away
/// half-way still frees the lane.
struct AbortOnDrop(Option<tokio::task::AbortHandle>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(abort) = self.0.take() {
            abort.abort();
        }
    }
}

/// Records the end of a job's task that ended without a result: dropped
/// unfinished means the task panicked or was aborted (see
/// [`WorkloadStore::spawn_task`]).
struct TaskEnd {
    store: WorkloadStore,
    job_id: String,
    background: bool,
    finished: bool,
}

impl TaskEnd {
    fn finish(&mut self, result: &Result<JobRecord, WorkloadError>) {
        self.finished = true;
        self.store
            .finish_task(&self.job_id, result, self.background);
    }
}

impl Drop for TaskEnd {
    fn drop(&mut self) {
        if !self.finished {
            self.store
                .finish_task_without_result(&self.job_id, self.background);
        }
    }
}

/// The task of one unfinished job. `abort` is set once it is spawned.
struct JobTask {
    abort: Option<tokio::task::AbortHandle>,
    tenant: Option<String>,
    background: bool,
}

/// The ComfyUI prompt a job queued: what a cancel interrupts.
#[derive(Clone)]
struct ComfyRun {
    provider_id: String,
    prompt_id: String,
    target: String,
}

/// Everything a job's task needs, owned, so a background job outlives the
/// request that started it. The snapshot is the one pinned at submission.
#[derive(Clone)]
pub struct JobContext {
    pub snapshot: Arc<sb_runtime::Snapshot>,
    pub ledger: Arc<sb_ledger::UsageLedger>,
    pub capacity: Arc<CapacityActivator>,
}

/// What a job runs: an image request (resolved the way
/// `/v1/images/generations` resolves it) plus the inputs only a ComfyUI
/// workflow can bind.
struct JobSpec {
    kind: WorkloadKind,
    request: ImageGenerationRequest,
    image: Option<String>,
    frames: Option<u32>,
    fps: Option<u32>,
    /// The name the caller sent, when an alias was rewritten to the
    /// workflow's public id in `request.model`.
    requested_model: Option<String>,
}

/// The inputs bound into a ComfyUI workflow.
struct ComfyInputs {
    request: ImageGenerationRequest,
    /// The kind the job runs as: a workflow may run without a prompt, an
    /// image or a video may not.
    kind: WorkloadKind,
    image: Option<String>,
    frames: Option<u32>,
    fps: Option<u32>,
}

fn is_terminal(status: JobStatus) -> bool {
    matches!(
        status,
        JobStatus::Succeeded | JobStatus::Failed | JobStatus::Cancelled
    )
}

#[derive(Clone)]
struct FalRunControl {
    provider_id: String,
    model: String,
    request_id: String,
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

    /// A store whose artifacts live in `artifacts`. Jobs kept there by an
    /// earlier run come back; one that was not finished is failed with
    /// [`RESTART_REASON`] (jobs are not resumed), never silently dropped.
    pub fn with_artifacts(artifacts: ArtifactStore) -> Self {
        let mut inner = Inner::default();
        let now = now_ms();
        let cutoff = now.saturating_sub(artifacts.policy().max_age_ms);
        for mut job in artifacts.load_jobs() {
            if is_terminal(job.status) && job.updated_at_ms < cutoff {
                artifacts.forget_job(&job.id);
                continue;
            }
            if !is_terminal(job.status) {
                job.status = JobStatus::Failed;
                job.updated_at_ms = now;
                job.route_decision.add_reason("state=failed");
                job.events.push(JobEvent {
                    event: "failed".to_string(),
                    status: JobStatus::Failed,
                    created_at_ms: now,
                    detail: BTreeMap::from([
                        ("error_class".to_string(), "restart".to_string()),
                        ("error".to_string(), RESTART_REASON.to_string()),
                    ]),
                });
                artifacts.persist_job(&job);
            }
            inner.jobs.insert(job.id.clone(), job);
        }
        let store = Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            artifacts,
        };
        {
            let mut guard = store.lock_inner();
            *guard = inner;
            store.trim_jobs(&mut guard);
        }
        store
    }

    /// The artifact store (bodies and their owners).
    pub fn artifacts(&self) -> &ArtifactStore {
        &self.artifacts
    }

    /// `/v1/images/generations`: run one image job and wait for its end. It
    /// runs on the job path of `POST /v1/jobs` (one task per job, the same
    /// lane, GPU-group and wake rules) and is cancellable by id while it
    /// runs. A video workflow is refused here: video runs only as a
    /// background job.
    pub async fn create_image_job(
        &self,
        ctx: JobContext,
        mut request: ImageGenerationRequest,
        owner: Owner,
    ) -> Result<JobRecord, WorkloadError> {
        let requested_model = canonical_workflow_name(&ctx.snapshot.config, &mut request.model);
        ensure_known_image_target(&ctx.snapshot.config, &request.model)?;
        if let Some((provider, workflow)) =
            resolve_comfyui_workflow(&ctx.snapshot.config, &request.model)
        {
            if workflow.kind == WorkloadKind::VideoGeneration {
                return Err(WorkloadError::InvalidRequest(format!(
                    "workflow `{}` makes video; video runs only as a background job (POST /v1/jobs)",
                    workflow.public_id(&provider.id)
                )));
            }
            ensure_image_inputs_bound(provider, workflow, &request)?;
        }
        let spec = JobSpec {
            kind: WorkloadKind::ImageGeneration,
            request,
            image: None,
            frames: None,
            fps: None,
            requested_model,
        };
        let snapshot = ctx.snapshot.clone();
        let (job, handle) = self.start(ctx, spec, owner, false)?;
        // The caller waits for this job: when it leaves (the request is
        // dropped), the job is cancelled like `POST /v1/jobs/{id}/cancel`
        // does, so no lane, GPU or machine stays busy for nobody. Only a
        // background job outlives its caller.
        let mut on_drop = CancelOnDrop {
            store: self.clone(),
            snapshot,
            job_id: Some(job.id.clone()),
        };
        let ended = handle.await;
        on_drop.job_id = None;
        match ended {
            // A job cancelled by id while it ran answers with its record.
            Ok(Err(error)) => match self.job(&job.id) {
                Some(job) if job.status == JobStatus::Cancelled => Ok(job),
                _ => Err(error),
            },
            Ok(result) => result,
            Err(error) if error.is_cancelled() => self
                .job(&job.id)
                .ok_or_else(|| WorkloadError::Internal("cancelled job disappeared".to_string())),
            Err(_) => Err(WorkloadError::Internal(
                "job task ended abnormally".to_string(),
            )),
        }
    }

    /// `POST /v1/jobs`: validate, then run the job in the background and
    /// answer at once with its id and state. `reader_may_read` says whether
    /// the caller may read a given input artifact.
    pub fn submit_job(
        &self,
        ctx: JobContext,
        body: sb_core::JobSubmitRequest,
        owner: Owner,
        reader_may_read: impl Fn(&Owner) -> bool,
    ) -> Result<JobRecord, WorkloadError> {
        let cfg = ctx.snapshot.config.clone();
        let mut target = match (&body.workflow, &body.model) {
            (Some(workflow), None) if workflow == "mock-image" => MOCK_IMAGE_MODEL.to_string(),
            (Some(workflow), None) => {
                if resolve_comfyui_workflow(&cfg, workflow).is_none() {
                    return Err(WorkloadError::InvalidRequest(format!(
                        "unknown workflow `{workflow}` (see GET /v1/workflows)"
                    )));
                }
                workflow.clone()
            }
            (None, Some(model)) => model.clone(),
            _ => {
                return Err(WorkloadError::InvalidRequest(
                    "give exactly one of `workflow` or `model`".to_string(),
                ))
            }
        };
        let requested_model = canonical_workflow_name(&cfg, &mut target);
        let spec = JobSpec {
            kind: body.kind,
            request: ImageGenerationRequest {
                model: target,
                prompt: body.inputs.prompt,
                size: body.inputs.size,
                seed: body.inputs.seed,
                response_format: None,
                n: 1,
                metadata: BTreeMap::new(),
            },
            image: body.inputs.image,
            frames: body.inputs.frames,
            fps: body.inputs.fps,
            requested_model,
        };
        self.validate_job(&cfg, &spec, &reader_may_read)?;
        self.start(ctx, spec, owner, true).map(|(job, _)| job)
    }

    /// Refuse a job before it exists: a kind `/v1/jobs` does not run, a
    /// workflow of another kind, a missing prompt, a bad size, an input the
    /// workflow does not bind, an input image the caller may not read.
    fn validate_job(
        &self,
        cfg: &Config,
        spec: &JobSpec,
        reader_may_read: &impl Fn(&Owner) -> bool,
    ) -> Result<(), WorkloadError> {
        if !matches!(
            spec.kind,
            WorkloadKind::ImageGeneration
                | WorkloadKind::VideoGeneration
                | WorkloadKind::WorkflowExecution
        ) {
            return Err(WorkloadError::InvalidRequest(format!(
                "kind `{}` does not run on /v1/jobs (image_generation, video_generation, workflow_execution)",
                kind_name(spec.kind)
            )));
        }
        validate_single_image_request(&spec.request, "job", spec.kind)?;
        let extras = [
            ("image", spec.image.is_some()),
            ("frames", spec.frames.is_some()),
            ("fps", spec.fps.is_some()),
        ];
        match resolve_comfyui_workflow(cfg, &spec.request.model) {
            Some((provider, workflow)) => {
                if workflow.kind != spec.kind {
                    return Err(WorkloadError::InvalidRequest(format!(
                        "workflow `{}` is a {} workflow, not {}",
                        workflow.public_id(&provider.id),
                        kind_name(workflow.kind),
                        kind_name(spec.kind)
                    )));
                }
                ensure_image_inputs_bound(provider, workflow, &spec.request)?;
                check_input_transforms(workflow, &spec.request, spec.frames, spec.fps)?;
                // A prompt the workflow cannot take is refused now, before a
                // machine is woken for a job that would fail at binding.
                if !spec.request.prompt.trim().is_empty()
                    && !workflow.bindings.contains_key("prompt")
                {
                    return Err(WorkloadError::InvalidRequest(format!(
                        "workflow `{}` does not bind `prompt`",
                        workflow.public_id(&provider.id)
                    )));
                }
                // A workflow that binds an input image runs on one: without
                // it the graph would run on whatever its own node holds.
                if spec.kind == WorkloadKind::WorkflowExecution
                    && workflow.bindings.contains_key("image")
                    && spec.image.is_none()
                {
                    return Err(WorkloadError::InvalidRequest(format!(
                        "workflow `{}` needs an `image` input (an uploaded artifact id)",
                        workflow.public_id(&provider.id)
                    )));
                }
                for (name, given) in extras {
                    if given && !workflow.bindings.contains_key(name) {
                        return Err(WorkloadError::InvalidRequest(format!(
                            "workflow `{}` does not bind `{name}`",
                            workflow.public_id(&provider.id)
                        )));
                    }
                }
            }
            None => {
                if spec.kind == WorkloadKind::WorkflowExecution {
                    return Err(WorkloadError::InvalidRequest(format!(
                        "`{}` is not a configured workflow (see GET /v1/workflows)",
                        spec.request.model
                    )));
                }
                ensure_known_image_target(cfg, &spec.request.model)?;
                if spec.kind != WorkloadKind::ImageGeneration {
                    return Err(WorkloadError::InvalidRequest(format!(
                        "`{}` is not a {} workflow",
                        spec.request.model,
                        kind_name(spec.kind)
                    )));
                }
                if let Some((name, _)) = extras.iter().find(|(_, given)| *given) {
                    return Err(WorkloadError::InvalidRequest(format!(
                        "`{}` takes no `{name}` input",
                        spec.request.model
                    )));
                }
            }
        }
        if let Some(image) = &spec.image {
            let readable = self
                .artifacts
                .record(image)
                .is_ok_and(|(_, owner)| reader_may_read(&owner));
            if !readable {
                return Err(WorkloadError::InvalidRequest(format!(
                    "input artifact `{image}` not found"
                )));
            }
        }
        Ok(())
    }

    /// Create the job (`accepted`) and spawn its task. A background job
    /// counts against its tenant's `server.jobs.max_pending_per_tenant`
    /// until its task ends; one over the limit is refused before it exists.
    #[allow(clippy::type_complexity)]
    fn start(
        &self,
        ctx: JobContext,
        spec: JobSpec,
        owner: Owner,
        background: bool,
    ) -> Result<
        (
            JobRecord,
            tokio::task::JoinHandle<Result<JobRecord, WorkloadError>>,
        ),
        WorkloadError,
    > {
        let cfg = ctx.snapshot.config.clone();
        let job_id = sb_core::new_id("job");
        let now = now_ms();
        let (mut decision, mut accepted) =
            initial_decision(&cfg, &job_id, spec.kind, &spec.request.model);
        accepted.insert("model".to_string(), spec.request.model.clone());
        if let Some(requested) = &spec.requested_model {
            accepted.insert("requested_model".to_string(), requested.clone());
            decision.add_reason(format!("requested_model={requested}"));
        }
        accepted.insert("prompt_stored".to_string(), "false".to_string());
        if background {
            accepted.insert("mode".to_string(), "background".to_string());
        }
        let job = JobRecord {
            id: job_id.clone(),
            kind: spec.kind,
            status: JobStatus::Accepted,
            target: spec.request.model.clone(),
            requested_model: spec.requested_model.clone(),
            tenant: owner.tenant.clone(),
            project: owner.project.clone(),
            created_at_ms: now,
            updated_at_ms: now,
            route_decision: decision,
            artifacts: Vec::new(),
            events: vec![JobEvent {
                event: "accepted".to_string(),
                status: JobStatus::Accepted,
                created_at_ms: now,
                detail: accepted,
            }],
            prompt_stored: false,
        };
        {
            let mut inner = self.lock_inner();
            if background {
                let limit = cfg.server.jobs.max_pending_per_tenant as usize;
                let pending = inner
                    .tasks
                    .values()
                    .filter(|task| task.background && task.tenant == owner.tenant)
                    .count();
                if pending >= limit {
                    return Err(WorkloadError::TooManyJobs(format!(
                        "{pending} background jobs of this tenant are queued or running (server.jobs.max_pending_per_tenant = {limit}); wait for one to finish"
                    )));
                }
            }
            inner.tasks.insert(
                job_id.clone(),
                JobTask {
                    abort: None,
                    tenant: owner.tenant.clone(),
                    background,
                },
            );
            self.put_job(&mut inner, job.clone());
        }
        let store = self.clone();
        let id = job_id.clone();
        let handle = self.spawn_task(&job_id, background, async move {
            store.run(&ctx, &id, spec, &owner, background).await
        });
        Ok((job, handle))
    }

    /// Spawn the task of job `job_id` (already in `tasks`). Its end is
    /// recorded on every path: a result, a panic, an abort. A task that
    /// ends without a result (it panicked) still frees its pending slot
    /// and fails its job, so a month of unattended jobs cannot leak either.
    fn spawn_task(
        &self,
        job_id: &str,
        background: bool,
        body: impl std::future::Future<Output = Result<JobRecord, WorkloadError>> + Send + 'static,
    ) -> tokio::task::JoinHandle<Result<JobRecord, WorkloadError>> {
        let mut end = TaskEnd {
            store: self.clone(),
            job_id: job_id.to_string(),
            background,
            finished: false,
        };
        let handle = tokio::spawn(async move {
            let result = body.await;
            end.finish(&result);
            result
        });
        if let Some(task) = self.lock_inner().tasks.get_mut(job_id) {
            task.abort = Some(handle.abort_handle());
        }
        handle
    }

    /// A job's task ended without a result (a panic, or an abort by a
    /// cancel). Frees what the task held in the store; an unfinished job
    /// (not cancelled) is failed with class `internal`.
    fn finish_task_without_result(&self, job_id: &str, background: bool) {
        {
            let mut inner = self.lock_inner();
            inner.submitting.remove(job_id);
            inner.fal_runs.remove(job_id);
        }
        let error = WorkloadError::Internal(
            "the job's task stopped unexpectedly before it finished".to_string(),
        );
        self.finish_task(job_id, &Err(error), background);
    }

    /// The body of a job's task: the adapter the target resolves to.
    async fn run(
        &self,
        ctx: &JobContext,
        job_id: &str,
        spec: JobSpec,
        owner: &Owner,
        background: bool,
    ) -> Result<JobRecord, WorkloadError> {
        let cfg = &ctx.snapshot.config;
        let request = spec.request;
        let (tenant, project) = (owner.tenant.clone(), owner.project.clone());
        if let Some((provider, model)) = resolve_fal_model(cfg, &request.model) {
            return self
                .create_fal_image_job(
                    cfg,
                    &ctx.snapshot.resolver,
                    &ctx.snapshot.registry,
                    &ctx.ledger,
                    provider,
                    &model,
                    request,
                    tenant,
                    project,
                    job_id.to_string(),
                )
                .await;
        }
        if let Some((provider, workflow)) = resolve_comfyui_workflow(cfg, &request.model) {
            let inputs = ComfyInputs {
                request,
                kind: spec.kind,
                image: spec.image,
                frames: spec.frames,
                fps: spec.fps,
            };
            return self
                .run_comfy(ctx, provider, workflow, inputs, owner, job_id, background)
                .await;
        }
        ensure_known_image_target(cfg, &request.model)?;
        let job = self.create_mock_image_job(request, tenant, project, job_id.to_string())?;
        record_unpriced_image_usage(
            &ctx.ledger,
            &job,
            "switchback-mock",
            None,
            WorkloadKind::ImageGeneration,
        );
        Ok(job)
    }

    /// A job's task ended: it holds no lane and counts as pending no more.
    /// An error the adapter did not already record fails the job, with the
    /// error the caller gets (metadata only). A blocking request rejected
    /// as invalid never became a job and is not kept, as before.
    fn finish_task(
        &self,
        job_id: &str,
        result: &Result<JobRecord, WorkloadError>,
        background: bool,
    ) {
        let mut inner = self.lock_inner();
        inner.tasks.remove(job_id);
        inner.comfy_prompts.remove(job_id);
        let Err(error) = result else {
            return;
        };
        let unfinished = inner
            .jobs
            .get(job_id)
            .is_some_and(|job| !is_terminal(job.status));
        if !unfinished {
            return;
        }
        if matches!(error, WorkloadError::InvalidRequest(_)) && !background {
            inner.jobs.remove(job_id);
            self.artifacts.forget_job(job_id);
        } else {
            self.fail_locked(&mut inner, job_id, error);
        }
    }

    /// Mark a job failed with `error`. Failed jobs are bounded (see
    /// [`KEPT_FAILED_COMFY_JOBS`]); the oldest leave first.
    fn fail_locked(&self, inner: &mut Inner, job_id: &str, error: &WorkloadError) {
        let class = match error {
            WorkloadError::InvalidRequest(_) => "invalid_request",
            WorkloadError::TooManyJobs(_) => "too_many_jobs",
            WorkloadError::Upstream(_) => "upstream",
            WorkloadError::Internal(_) => "internal",
            WorkloadError::LaneQueueTimeout { .. } => "local_lane_queue_timeout",
        };
        self.fail_class_locked(inner, job_id, class, error.message());
    }

    /// Mark an unfinished job failed with an error class of its own.
    fn fail_with_class(&self, job_id: &str, class: &str, message: &str) {
        let mut inner = self.lock_inner();
        let unfinished = inner
            .jobs
            .get(job_id)
            .is_some_and(|job| !is_terminal(job.status));
        if unfinished {
            self.fail_class_locked(&mut inner, job_id, class, message);
        }
    }

    fn fail_class_locked(&self, inner: &mut Inner, job_id: &str, class: &str, message: &str) {
        let Some(mut job) = inner.jobs.get(job_id).cloned() else {
            return;
        };
        let now = now_ms();
        job.status = JobStatus::Failed;
        job.updated_at_ms = now;
        job.route_decision.add_reason("state=failed");
        job.events.push(JobEvent {
            event: "failed".to_string(),
            status: JobStatus::Failed,
            created_at_ms: now,
            detail: BTreeMap::from([
                ("error_class".to_string(), class.to_string()),
                ("error".to_string(), message.to_string()),
            ]),
        });
        self.put_job(inner, job);
        inner.failed_comfy.push_back(job_id.to_string());
        while inner.failed_comfy.len() > KEPT_FAILED_COMFY_JOBS {
            if let Some(oldest) = inner.failed_comfy.pop_front() {
                inner.jobs.remove(&oldest);
                self.artifacts.forget_job(&oldest);
            }
        }
    }

    /// Keep `job` (memory, and disk on a disk store), then trim to
    /// [`KEPT_JOBS`].
    fn put_job(&self, inner: &mut Inner, job: JobRecord) {
        self.artifacts.persist_job(&job);
        inner.jobs.insert(job.id.clone(), job);
        if inner.jobs.len() > KEPT_JOBS {
            self.trim_jobs(inner);
        }
    }

    /// Drop the oldest finished jobs until nine tenths of [`KEPT_JOBS`]
    /// remain (unfinished jobs are never dropped).
    fn trim_jobs(&self, inner: &mut Inner) {
        if inner.jobs.len() <= KEPT_JOBS {
            return;
        }
        let mut finished: Vec<(u64, String)> = inner
            .jobs
            .values()
            .filter(|job| is_terminal(job.status))
            .map(|job| (job.created_at_ms, job.id.clone()))
            .collect();
        finished.sort();
        let target = KEPT_JOBS - KEPT_JOBS / 10;
        for (_, id) in finished {
            if inner.jobs.len() <= target {
                break;
            }
            inner.jobs.remove(&id);
            inner.failed_comfy.retain(|kept| kept != &id);
            self.artifacts.forget_job(&id);
        }
    }

    fn lock_inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        // Every mutation is a few map operations: a panic elsewhere leaves
        // the maps consistent, so keep serving.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Append an event to a running job (no-op once it is cancelled: a
    /// cancel is final).
    fn push_event(
        &self,
        job_id: &str,
        status: JobStatus,
        event: &str,
        detail: BTreeMap<String, String>,
    ) {
        let mut inner = self.lock_inner();
        let Some(mut job) = inner.jobs.get(job_id).cloned() else {
            return;
        };
        if job.status == JobStatus::Cancelled {
            return;
        }
        let now = now_ms();
        job.status = status;
        job.updated_at_ms = now;
        job.events.push(JobEvent {
            event: event.to_string(),
            status,
            created_at_ms: now,
            detail,
        });
        self.put_job(&mut inner, job);
    }

    fn is_cancelled(&self, job_id: &str) -> bool {
        self.lock_inner()
            .jobs
            .get(job_id)
            .is_some_and(|job| job.status == JobStatus::Cancelled)
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
                    id: workflow.public_id(&provider.id),
                    kind: workflow.kind,
                    provider: provider.id.clone(),
                    version: workflow
                        .version
                        .clone()
                        .unwrap_or_else(|| "configured".to_string()),
                    inputs: workflow_fields_for(workflow),
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
        job_id: String,
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
                status: JobStatus::ArtifactReady,
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
            requested_model: None,
            tenant,
            project,
            created_at_ms: now,
            updated_at_ms: now,
            route_decision: decision,
            artifacts: vec![artifact.clone()],
            events,
            prompt_stored: false,
        };
        self.insert_job(
            job,
            vec![StoredArtifact {
                record: artifact,
                bytes,
            }],
        )
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_fal_image_job(
        &self,
        cfg: &Config,
        resolver: &CredentialResolver,
        registry: &sb_adapters::AdapterRegistry,
        ledger: &sb_ledger::UsageLedger,
        provider: &sb_core::ProviderConfig,
        model: &str,
        request: ImageGenerationRequest,
        tenant: Option<String>,
        project: Option<String>,
        job_id: String,
    ) -> Result<JobRecord, WorkloadError> {
        validate_single_image_request(&request, "fal", WorkloadKind::ImageGeneration)?;
        let ProviderKind::Fal { base_url, .. } = &provider.kind else {
            return Err(WorkloadError::InvalidRequest(format!(
                "provider `{}` is not a fal provider",
                provider.id
            )));
        };

        let (account_id, lease) = resolve_fal_lease(resolver, &provider.id, model).await?;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| WorkloadError::Internal(format!("build fal client: {e}")))?;
        let submit_url = fal_model_endpoint(base_url, model, None)?;
        guard_provider_url(cfg, submit_url.as_str()).await?;
        let mut body = serde_json::Map::new();
        body.insert("prompt".to_string(), Json::String(request.prompt.clone()));
        body.insert("num_images".to_string(), Json::from(request.n));
        if let Some(seed) = request.seed {
            body.insert("seed".to_string(), Json::from(seed));
        }
        if let Some((width, height)) = parse_size(request.size.as_deref()) {
            body.insert(
                "image_size".to_string(),
                serde_json::json!({"width": width, "height": height}),
            );
        }

        let now = now_ms();
        let mut accepted_detail = BTreeMap::new();
        accepted_detail.insert("model".to_string(), model.to_string());
        accepted_detail.insert("provider".to_string(), provider.id.clone());
        accepted_detail.insert("prompt_stored".to_string(), "false".to_string());
        let mut events = vec![JobEvent {
            event: "accepted".to_string(),
            status: JobStatus::Accepted,
            created_at_ms: now,
            detail: accepted_detail,
        }];

        // This POST is the fal provider-side commit point. Its outcome may be
        // ambiguous on network failure, so this adapter never falls through to
        // another target after the request is issued.
        let submitted: FalSubmitResponse = match send_json(
            client
                .post(submit_url)
                .header(
                    reqwest::header::AUTHORIZATION,
                    format!("Key {}", lease.secret.expose()),
                )
                .json(&body),
            "submit fal queue request (commit point; fallback disabled)",
        )
        .await
        {
            Ok(submitted) => submitted,
            Err(error) => {
                self.persist_fal_failure(
                    ledger,
                    registry,
                    &job_id,
                    &request.model,
                    &provider.id,
                    model,
                    &account_id,
                    &tenant,
                    &project,
                    now,
                    events,
                    "queue_submission",
                )?;
                return Err(error);
            }
        };
        if submitted.request_id.trim().is_empty() {
            let error = WorkloadError::Upstream(
                "fal queue submission returned an empty request_id".to_string(),
            );
            self.persist_fal_failure(
                ledger,
                registry,
                &job_id,
                &request.model,
                &provider.id,
                model,
                &account_id,
                &tenant,
                &project,
                now,
                events,
                "queue_submission_response",
            )?;
            return Err(error);
        }
        events.push(JobEvent {
            event: "queued".to_string(),
            status: JobStatus::Queued,
            created_at_ms: now_ms(),
            detail: BTreeMap::from([
                (
                    "provider_request_id".to_string(),
                    submitted.request_id.clone(),
                ),
                ("commit_point".to_string(), "queue_submission".to_string()),
            ]),
        });

        let queued_job = JobRecord {
            id: job_id.clone(),
            kind: WorkloadKind::ImageGeneration,
            status: JobStatus::Queued,
            target: request.model.clone(),
            requested_model: None,
            tenant: tenant.clone(),
            project: project.clone(),
            created_at_ms: now,
            updated_at_ms: now_ms(),
            route_decision: fal_route_decision(&job_id, &request.model, &provider.id, model),
            artifacts: Vec::new(),
            events: events.clone(),
            prompt_stored: false,
        };
        if let Err(error) = self.insert_fal_run(
            queued_job,
            FalRunControl {
                provider_id: provider.id.clone(),
                model: model.to_string(),
                request_id: submitted.request_id.clone(),
            },
        ) {
            record_fal_usage(
                ledger,
                registry,
                &job_id,
                &provider.id,
                model,
                &account_id,
                &tenant,
                &project,
                now,
                None,
            );
            return Err(error);
        }

        let poll_outcome = match poll_fal_status(
            self,
            &job_id,
            cfg,
            &client,
            base_url,
            model,
            &submitted.request_id,
            lease.secret.expose(),
            &mut events,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                self.persist_fal_failure(
                    ledger,
                    registry,
                    &job_id,
                    &request.model,
                    &provider.id,
                    model,
                    &account_id,
                    &tenant,
                    &project,
                    now,
                    events,
                    "poll_status",
                )?;
                return Err(error);
            }
        };
        if poll_outcome == FalPollOutcome::Cancelled {
            record_fal_usage(
                ledger,
                registry,
                &job_id,
                &provider.id,
                model,
                &account_id,
                &tenant,
                &project,
                now,
                None,
            );
            self.remove_fal_run(&job_id)?;
            return self.job(&job_id).ok_or_else(|| {
                WorkloadError::Internal("cancelled fal job disappeared".to_string())
            });
        }
        let captured = async {
            let result_url = fal_model_endpoint(
                base_url,
                model,
                Some(&format!("requests/{}", submitted.request_id)),
            )?;
            guard_provider_url(cfg, result_url.as_str()).await?;
            let result: Json = send_json(
                client.get(result_url).header(
                    reqwest::header::AUTHORIZATION,
                    format!("Key {}", lease.secret.expose()),
                ),
                "fetch fal queue result",
            )
            .await?;
            let outputs = extract_fal_outputs(&result);
            if outputs.is_empty() {
                return Err(WorkloadError::Upstream(
                    "fal result completed without image artifacts".to_string(),
                ));
            }

            let mut records = Vec::with_capacity(outputs.len());
            let mut stored = Vec::with_capacity(outputs.len());
            for output in outputs {
                guard_provider_url(cfg, &output.url).await?;
                let response = client
                    .get(&output.url)
                    .send()
                    .await
                    .map_err(|e| WorkloadError::Upstream(format!("fetch fal artifact: {e}")))?;
                let status = response.status();
                if !status.is_success() {
                    return Err(WorkloadError::Upstream(format!(
                        "fetch fal artifact failed with status {status}"
                    )));
                }
                let media_type = response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string)
                    .or(output.media_type)
                    .unwrap_or_else(|| media_type_for_filename(&output.url).to_string());
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|e| WorkloadError::Upstream(format!("read fal artifact: {e}")))?
                    .to_vec();
                let artifact_id = sb_core::new_id("art");
                let provenance = BTreeMap::from([
                    ("provider".to_string(), "fal".to_string()),
                    ("provider_id".to_string(), provider.id.clone()),
                    ("model".to_string(), model.to_string()),
                    ("route_decision_id".to_string(), job_id.clone()),
                    (
                        "provider_request_id".to_string(),
                        submitted.request_id.clone(),
                    ),
                ]);
                let record = ArtifactRecord {
                    artifact_id: artifact_id.clone(),
                    job_id: job_id.clone(),
                    kind: artifact_kind_for_media_type(&media_type),
                    media_type: media_type.clone(),
                    bytes: bytes.len() as u64,
                    sha256: sha256_hex(&bytes),
                    storage_ref: format!("memory://{artifact_id}"),
                    width: output.width,
                    height: output.height,
                    duration_ms: None,
                    fps: None,
                    created_at_ms: now_ms(),
                    retention: "process_memory".to_string(),
                    provenance,
                };
                records.push(record.clone());
                stored.push(StoredArtifact { record, bytes });
            }
            Ok::<_, WorkloadError>((records, stored))
        }
        .await;
        let (records, stored) = match captured {
            Ok(captured) => captured,
            Err(error) => {
                self.persist_fal_failure(
                    ledger,
                    registry,
                    &job_id,
                    &request.model,
                    &provider.id,
                    model,
                    &account_id,
                    &tenant,
                    &project,
                    now,
                    events,
                    "result_or_artifact_capture",
                )?;
                return Err(error);
            }
        };
        for record in &records {
            events.push(JobEvent {
                event: "artifact_ready".to_string(),
                status: JobStatus::ArtifactReady,
                created_at_ms: now_ms(),
                detail: BTreeMap::from([
                    ("artifact_id".to_string(), record.artifact_id.clone()),
                    ("media_type".to_string(), record.media_type.clone()),
                ]),
            });
        }
        events.push(JobEvent {
            event: "succeeded".to_string(),
            status: JobStatus::Succeeded,
            created_at_ms: now_ms(),
            detail: BTreeMap::new(),
        });

        let decision = fal_route_decision(&job_id, &request.model, &provider.id, model);
        record_fal_usage(
            ledger,
            registry,
            &job_id,
            &provider.id,
            model,
            &account_id,
            &tenant,
            &project,
            now,
            Some(&records),
        );
        let job = JobRecord {
            id: job_id,
            kind: WorkloadKind::ImageGeneration,
            status: JobStatus::Succeeded,
            target: request.model,
            requested_model: None,
            tenant,
            project,
            created_at_ms: now,
            updated_at_ms: now_ms(),
            route_decision: decision,
            artifacts: records,
            events,
            prompt_stored: false,
        };
        let job = self.insert_job(job, stored)?;
        self.remove_fal_run(&job.id)?;
        Ok(job)
    }

    #[allow(clippy::too_many_arguments)]
    fn persist_fal_failure(
        &self,
        ledger: &sb_ledger::UsageLedger,
        registry: &sb_adapters::AdapterRegistry,
        job_id: &str,
        target: &str,
        provider_id: &str,
        model: &str,
        account_id: &str,
        tenant: &Option<String>,
        project: &Option<String>,
        started_at_ms: u64,
        mut events: Vec<JobEvent>,
        stage: &str,
    ) -> Result<(), WorkloadError> {
        let updated_at_ms = now_ms();
        events.push(JobEvent {
            event: "failed".to_string(),
            status: JobStatus::Failed,
            created_at_ms: updated_at_ms,
            detail: BTreeMap::from([
                ("stage".to_string(), stage.to_string()),
                ("fallback_legal".to_string(), "false".to_string()),
                ("commit_point".to_string(), "queue_submission".to_string()),
            ]),
        });
        let job = JobRecord {
            id: job_id.to_string(),
            kind: WorkloadKind::ImageGeneration,
            status: JobStatus::Failed,
            target: target.to_string(),
            requested_model: None,
            tenant: tenant.clone(),
            project: project.clone(),
            created_at_ms: started_at_ms,
            updated_at_ms,
            route_decision: fal_route_decision(job_id, target, provider_id, model),
            artifacts: Vec::new(),
            events,
            prompt_stored: false,
        };
        record_fal_usage(
            ledger,
            registry,
            job_id,
            provider_id,
            model,
            account_id,
            tenant,
            project,
            started_at_ms,
            None,
        );
        self.insert_job(job, Vec::new())?;
        self.remove_fal_run(job_id)
    }

    /// One ComfyUI job: on a managed lane, wake the machine when it is
    /// offline, wait for the lane's GPU group, then dispatch with self-heal
    /// (the job holds the lane busy from its wait to its end, exactly as a
    /// blocking request does); otherwise dispatch at once. An unconfigured
    /// wake leaves the job `queued` — never a fake success.
    #[allow(clippy::too_many_arguments)]
    async fn run_comfy(
        &self,
        ctx: &JobContext,
        provider: &sb_core::ProviderConfig,
        workflow: &ComfyUiWorkflowConfig,
        inputs: ComfyInputs,
        owner: &Owner,
        job_id: &str,
        background: bool,
    ) -> Result<JobRecord, WorkloadError> {
        // Validate cheaply before waking a machine for a doomed request.
        validate_comfyui_request(&inputs.request, inputs.kind)?;
        // A ComfyUI behind a token proxy: the provider's credential goes on
        // every call (none configured: none sent, as before).
        // Accounts are keyed by the provider's own `<provider>/<workflow>`
        // name, whatever public id callers use.
        let internal_name = format!("{}/{}", provider.id, workflow.id);
        let credential =
            SchemeCredential::resolve(&ctx.snapshot.resolver, provider, &internal_name)
                .await
                .map_err(WorkloadError::Upstream)?;
        let attempt = ComfyAttempt {
            cfg: &ctx.snapshot.config,
            store: self,
            job_id,
            provider,
            workflow,
            inputs: &inputs,
            owner,
            credential: credential.as_ref(),
        };
        let (records, note) = match ctx.capacity.executor(&provider.id) {
            Some(executor) => {
                self.note_waiting(job_id, &executor);
                let mut unconfigured_until = None;
                loop {
                    match executor
                        .run_job_noted(|_attempt| comfyui_dispatch_once(&attempt))
                        .await
                    {
                        Ok(done) => break done,
                        Err(JobError::Queued) if !background => {
                            // The caller gets the job as `queued`, as
                            // before; nothing will run it, so its record
                            // ends `failed` with the reason.
                            let held = self.hold_queued(job_id, WAKE_UNCONFIGURED)?;
                            self.fail_with_class(
                                job_id,
                                "wake_unconfigured",
                                &format!(
                                    "local executor `{}` {WAKE_UNCONFIGURED}; the job was not run",
                                    provider.id
                                ),
                            );
                            return Ok(held);
                        }
                        Err(JobError::Queued) => {
                            // A background job waits, `queued`, for the
                            // lane to come up some other way, bounded by
                            // the workflow's timeout; then it fails.
                            let timeout = comfy_job_timeout(workflow);
                            let until = *unconfigured_until.get_or_insert_with(|| {
                                    if let Err(error) = self.hold_queued(job_id, WAKE_UNCONFIGURED) {
                                        tracing::debug!(job = %job_id, %error, "queued event not recorded");
                                    }
                                    tokio::time::Instant::now() + timeout
                                });
                            let now = tokio::time::Instant::now();
                            if now >= until {
                                return Err(WorkloadError::Upstream(format!(
                                        "local executor `{}` {WAKE_UNCONFIGURED}; the lane did not come up within {} s (the workflow's timeout)",
                                        provider.id,
                                        timeout.as_secs()
                                    )));
                            }
                            tokio::time::sleep(WAKE_UNCONFIGURED_RECHECK.min(until - now)).await;
                        }
                        Err(error) => return Err(lane_error(&provider.id, error)),
                    }
                }
            }
            None => (comfyui_dispatch_once(&attempt).await?, None),
        };
        let note = note.map(|note| format!("local lane `{}` {note}", provider.id));
        let job = self.complete_job(job_id, records, note)?;
        if job.status == JobStatus::Succeeded {
            record_unpriced_media_usage(&ctx.ledger, &job, &provider.id);
        }
        Ok(job)
    }

    /// Before a managed lane's capacity wait: the job is `queued`, and its
    /// event says on what (the lane's state, its GPU group).
    fn note_waiting(&self, job_id: &str, executor: &Arc<LocalExecutor>) {
        let report = executor.report();
        let state = serde_json::to_value(report.state)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_default();
        let mut detail = BTreeMap::from([
            ("lane".to_string(), report.name.clone()),
            ("lane_state".to_string(), state),
        ]);
        if let Some(group) = report.gpu_group {
            detail.insert("gpu_group".to_string(), group);
            detail.insert(
                "holds_gpu".to_string(),
                report.holds_gpu.unwrap_or(false).to_string(),
            );
        }
        self.push_event(job_id, JobStatus::Queued, "waiting_for_lane", detail);
    }

    /// A job waiting for local capacity that cannot be secured (wake
    /// unconfigured): explicitly `queued`, no artifacts. The doctor reports
    /// the gap; it is never rendered as a completed image.
    fn hold_queued(&self, job_id: &str, reason: &str) -> Result<JobRecord, WorkloadError> {
        self.push_event(
            job_id,
            JobStatus::Queued,
            "queued",
            BTreeMap::from([
                ("reason".to_string(), "wake_unconfigured".to_string()),
                ("detail".to_string(), reason.to_string()),
            ]),
        );
        let mut inner = self.lock_inner();
        let mut job = inner
            .jobs
            .get(job_id)
            .cloned()
            .ok_or_else(|| WorkloadError::Internal(format!("job `{job_id}` disappeared")))?;
        if job.status == JobStatus::Queued {
            job.route_decision.add_reason("state=queued");
            self.put_job(&mut inner, job.clone());
        }
        Ok(job)
    }

    /// A job's artifacts are stored: it succeeded (unless it was cancelled
    /// meanwhile; a cancel is final).
    fn complete_job(
        &self,
        job_id: &str,
        records: Vec<ArtifactRecord>,
        note: Option<String>,
    ) -> Result<JobRecord, WorkloadError> {
        let mut inner = self.lock_inner();
        let mut job = inner
            .jobs
            .get(job_id)
            .cloned()
            .ok_or_else(|| WorkloadError::Internal(format!("job `{job_id}` disappeared")))?;
        if job.status == JobStatus::Cancelled {
            return Ok(job);
        }
        let now = now_ms();
        job.artifacts = records;
        job.status = JobStatus::Succeeded;
        job.updated_at_ms = now;
        if let Some(note) = note {
            job.route_decision.add_reason(note);
        }
        job.events.push(JobEvent {
            event: "succeeded".to_string(),
            status: JobStatus::Succeeded,
            created_at_ms: now,
            detail: BTreeMap::new(),
        });
        self.put_job(&mut inner, job.clone());
        Ok(job)
    }

    /// The job's `/prompt` call is about to be sent (see `submitting`).
    fn begin_submit(&self, job_id: &str) {
        self.lock_inner().submitting.insert(job_id.to_string());
    }

    /// The job's `/prompt` call ended; `run` is the prompt ComfyUI queued, if
    /// it answered with one. Returns that prompt when the job was cancelled
    /// meanwhile: the caller must stop it before it ends.
    fn end_submit(&self, job_id: &str, run: Option<ComfyRun>) -> Option<ComfyRun> {
        let mut inner = self.lock_inner();
        inner.submitting.remove(job_id);
        let run = run?;
        let cancelled = inner
            .jobs
            .get(job_id)
            .is_some_and(|job| job.status == JobStatus::Cancelled);
        if cancelled {
            return Some(run);
        }
        inner.comfy_prompts.insert(job_id.to_string(), run);
        None
    }

    /// Keep a job, writing the bodies it made to the artifact store first
    /// (the job then lists each artifact as stored: size, digest, storage
    /// reference, retention).
    fn insert_job(
        &self,
        mut job: JobRecord,
        artifacts: Vec<StoredArtifact>,
    ) -> Result<JobRecord, WorkloadError> {
        let owner = Owner::new(job.tenant.clone(), job.project.clone());
        for artifact in artifacts {
            let stored = self
                .artifacts
                .put_bytes(artifact.record, owner.clone(), &artifact.bytes)
                .map_err(|error| WorkloadError::Upstream(error.to_string()))?;
            if let Some(listed) = job
                .artifacts
                .iter_mut()
                .find(|listed| listed.artifact_id == stored.artifact_id)
            {
                *listed = stored;
            }
        }
        let mut inner = self.lock_inner();
        if inner
            .jobs
            .get(&job.id)
            .is_some_and(|kept| kept.status == JobStatus::Cancelled)
        {
            // Cancelled while it ran: the cancel is final.
            return Ok(inner.jobs[&job.id].clone());
        }
        self.put_job(&mut inner, job.clone());
        Ok(job)
    }

    fn insert_fal_run(&self, job: JobRecord, control: FalRunControl) -> Result<(), WorkloadError> {
        let mut inner = self.lock_inner();
        inner.fal_runs.insert(job.id.clone(), control);
        self.put_job(&mut inner, job);
        Ok(())
    }

    fn update_job_event(
        &self,
        id: &str,
        status: JobStatus,
        event: JobEvent,
    ) -> Result<(), WorkloadError> {
        let mut inner = self.lock_inner();
        let mut job = inner
            .jobs
            .get(id)
            .cloned()
            .ok_or_else(|| WorkloadError::Internal(format!("job `{id}` disappeared")))?;
        if job.status == JobStatus::Cancelled {
            return Ok(());
        }
        job.status = status;
        job.updated_at_ms = event.created_at_ms;
        job.events.push(event);
        self.put_job(&mut inner, job);
        Ok(())
    }

    fn remove_fal_run(&self, id: &str) -> Result<(), WorkloadError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| WorkloadError::Internal("workload store lock poisoned".to_string()))?;
        inner.fal_runs.remove(id);
        Ok(())
    }

    pub fn job(&self, id: &str) -> Option<JobRecord> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.jobs.get(id).cloned())
    }

    pub fn jobs(&self) -> Vec<JobRecord> {
        let mut jobs = self
            .inner
            .lock()
            .map(|inner| inner.jobs.values().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        jobs.sort_by_key(|job| job.created_at_ms);
        jobs
    }

    /// The synchronous part of a cancel, under the store lock: a non-fal
    /// job is marked `cancelled` and its task stopped here, so nothing the
    /// task does afterwards can overwrite the cancel.
    fn begin_cancel(&self, id: &str, source: &str) -> Result<CancelPlan, WorkloadError> {
        let mut inner = self.lock_inner();
        let mut job = inner
            .jobs
            .get(id)
            .cloned()
            .ok_or_else(|| WorkloadError::InvalidRequest("job not found".to_string()))?;
        if job.status == JobStatus::Cancelled {
            return Ok(CancelPlan::Done(job));
        }
        if matches!(job.status, JobStatus::Succeeded | JobStatus::Failed) {
            return Err(WorkloadError::InvalidRequest(
                "terminal job cannot be cancelled".to_string(),
            ));
        }
        if let Some(control) = inner.fal_runs.get(id).cloned() {
            return Ok(CancelPlan::Fal(job, control));
        }
        let mut task = inner.tasks.remove(id);
        let prompt = inner.comfy_prompts.remove(id);
        // Its `/prompt` call is in flight: the task stops the prompt itself
        // when the id arrives (see `end_submit`) and is not aborted here, so
        // its lane stays held until ComfyUI is told.
        let submitting = inner.submitting.contains(id);
        if submitting {
            task = None;
        }
        let now = now_ms();
        job.status = JobStatus::Cancelled;
        job.updated_at_ms = now;
        job.events.push(JobEvent {
            event: "cancelled".to_string(),
            status: JobStatus::Cancelled,
            created_at_ms: now,
            detail: BTreeMap::from([
                ("source".to_string(), source.to_string()),
                (
                    "upstream_cancel_requested".to_string(),
                    (prompt.is_some() || submitting).to_string(),
                ),
            ]),
        });
        self.put_job(&mut inner, job);
        // Stopping the task drops its lane guard, queue slot and GPU permit.
        // A job still waiting is stopped at once; one whose prompt ComfyUI
        // has is stopped once ComfyUI is told (the caller drops the guard).
        let abort = AbortOnDrop(task.and_then(|task| task.abort));
        if prompt.is_none() {
            drop(abort);
            return Ok(CancelPlan::Stopped(None, AbortOnDrop(None)));
        }
        Ok(CancelPlan::Stopped(prompt, abort))
    }

    /// Record how the ComfyUI side of a cancel went on the `cancelled` event.
    fn note_upstream_cancel(&self, id: &str, outcome: String) {
        let mut inner = self.lock_inner();
        if let Some(mut job) = inner.jobs.get(id).cloned() {
            if let Some(event) = job.events.last_mut() {
                event.detail.insert("upstream_cancel".to_string(), outcome);
            }
            self.put_job(&mut inner, job);
        }
    }

    /// Cancel a job. A fal job is cancelled at fal. Any other unfinished job
    /// is marked `cancelled` at once and its task is stopped: a job still
    /// waiting (for a wake, the GPU group, its lane) is dropped, and the
    /// lane's in-flight slot, queue slot and GPU group are released with
    /// it; a ComfyUI prompt it queued is removed from ComfyUI's queue and
    /// interrupted, with the provider's credential.
    pub async fn cancel_job(
        &self,
        cfg: &Config,
        resolver: &CredentialResolver,
        id: &str,
    ) -> Result<JobRecord, WorkloadError> {
        self.cancel_job_from(cfg, resolver, id, "client").await
    }

    /// [`Self::cancel_job`], naming who cancelled on the `cancelled` event
    /// (`client`, or `client_gone` for a blocking request whose caller left).
    async fn cancel_job_from(
        &self,
        cfg: &Config,
        resolver: &CredentialResolver,
        id: &str,
        source: &str,
    ) -> Result<JobRecord, WorkloadError> {
        let (job, control) = match self.begin_cancel(id, source)? {
            CancelPlan::Done(job) => return Ok(job),
            CancelPlan::Fal(job, control) => (job, control),
            CancelPlan::Stopped(prompt, abort) => {
                if let Some(run) = prompt {
                    let outcome = match cancel_comfy_prompt(cfg, resolver, &run).await {
                        Ok(()) => "sent".to_string(),
                        Err(error) => format!("failed: {error}"),
                    };
                    self.note_upstream_cancel(id, outcome);
                }
                // ComfyUI is told: now the task, and with it the lane.
                drop(abort);
                return self.job(id).ok_or_else(|| {
                    WorkloadError::Internal("cancelled job disappeared".to_string())
                });
            }
        };
        if matches!(job.status, JobStatus::Succeeded | JobStatus::Failed) {
            return Err(WorkloadError::InvalidRequest(
                "terminal job cannot be cancelled".to_string(),
            ));
        }
        let provider = cfg
            .providers
            .iter()
            .find(|provider| provider.id == control.provider_id)
            .ok_or_else(|| {
                WorkloadError::Internal(format!(
                    "fal provider `{}` disappeared",
                    control.provider_id
                ))
            })?;
        let ProviderKind::Fal { base_url, .. } = &provider.kind else {
            return Err(WorkloadError::Internal(format!(
                "provider `{}` is no longer a fal provider",
                provider.id
            )));
        };
        let (_account_id, lease) =
            resolve_fal_lease(resolver, &provider.id, &control.model).await?;
        let cancel_url = fal_model_endpoint(
            base_url,
            &control.model,
            Some(&format!("requests/{}/cancel", control.request_id)),
        )?;
        guard_provider_url(cfg, cancel_url.as_str()).await?;

        let event = JobEvent {
            event: "cancelled".to_string(),
            status: JobStatus::Cancelled,
            created_at_ms: now_ms(),
            detail: BTreeMap::from([
                ("source".to_string(), source.to_string()),
                (
                    "provider_request_id".to_string(),
                    control.request_id.clone(),
                ),
                ("upstream_cancel_requested".to_string(), "true".to_string()),
            ]),
        };
        self.update_job_event(id, JobStatus::Cancelled, event)?;

        let _: Json = send_json(
            reqwest::Client::new().put(cancel_url).header(
                reqwest::header::AUTHORIZATION,
                format!("Key {}", lease.secret.expose()),
            ),
            "cancel fal queue request",
        )
        .await?;
        self.job(id)
            .ok_or_else(|| WorkloadError::Internal("cancelled job disappeared".to_string()))
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

#[derive(Debug, Deserialize)]
struct FalSubmitResponse {
    request_id: String,
}

#[derive(Debug, Deserialize)]
struct FalStatusResponse {
    status: String,
}

#[derive(Debug)]
struct FalOutputRef {
    url: String,
    media_type: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FalPollOutcome {
    Completed,
    Cancelled,
}

#[derive(Debug, Clone)]
struct ComfyOutputRef {
    node_id: String,
    filename: String,
    subfolder: String,
    type_: String,
    /// Frames per second, when the output node reports it (video).
    frame_rate: Option<f64>,
}

/// The mock image target (`mock-image` in `/v1/workflows`): a 1x1 PNG for
/// smoke tests. It is never a stand-in for a name that does not resolve.
const MOCK_IMAGE_MODEL: &str = "mock/image";

/// Refuse an image target that names no ComfyUI workflow, no fal model and
/// not the mock (400), instead of answering with the mock's placeholder.
fn ensure_known_image_target(cfg: &Config, model: &str) -> Result<(), WorkloadError> {
    if model == MOCK_IMAGE_MODEL
        || resolve_comfyui_workflow(cfg, model).is_some()
        || resolve_fal_model(cfg, model).is_some()
    {
        return Ok(());
    }
    Err(WorkloadError::InvalidRequest(format!(
        "unknown image model or workflow `{model}` (see GET /v1/workflows)"
    )))
}

fn resolve_comfyui_workflow<'a>(
    cfg: &'a Config,
    model: &str,
) -> Option<(&'a sb_core::ProviderConfig, &'a ComfyUiWorkflowConfig)> {
    cfg.resolve_workflow(model)
}

/// Point a request at a workflow's public id: a name that resolves through
/// an alias is replaced by the workflow's public id, and the name the caller
/// sent is returned so the job keeps it. `None` when the name already is
/// the public id or names no workflow (then nothing changes).
fn canonical_workflow_name(cfg: &Config, model: &mut String) -> Option<String> {
    let (provider, workflow) = resolve_comfyui_workflow(cfg, model)?;
    let public_id = workflow.public_id(&provider.id);
    (public_id != *model).then(|| std::mem::replace(model, public_id))
}

fn resolve_fal_model<'a>(
    cfg: &'a Config,
    target: &str,
) -> Option<(&'a sb_core::ProviderConfig, String)> {
    let (provider_id, model) = target.split_once('/')?;
    if model.is_empty() {
        return None;
    }
    let provider = cfg
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)?;
    matches!(provider.kind, ProviderKind::Fal { .. }).then_some((provider, model.to_string()))
}

fn fal_route_decision(job_id: &str, target: &str, provider_id: &str, model: &str) -> RouteDecision {
    let mut decision = RouteDecision::new(job_id, "workload/fal");
    decision.selected = Some(TargetRef::new(target));
    decision.add_reason("workload=image_generation");
    decision.add_reason(format!("provider={provider_id}"));
    decision.add_reason(format!("model={model}"));
    decision.add_reason("adapter=fal");
    decision.add_reason("commit_point=queue_submission");
    decision
}

async fn resolve_fal_lease(
    resolver: &CredentialResolver,
    provider_id: &str,
    model: &str,
) -> Result<(String, CredentialLease), WorkloadError> {
    let (account_id, lease) = match resolver.resolve(provider_id, model, &HashSet::new()) {
        ResolveOutcome::Selected { account_id, lease } => (account_id, lease),
        ResolveOutcome::AllUnavailable { .. } => {
            return Err(WorkloadError::Upstream(format!(
                "fal provider `{provider_id}` has no available credential account"
            )))
        }
        ResolveOutcome::NoAccounts => {
            return Err(WorkloadError::InvalidRequest(format!(
                "fal provider `{provider_id}` has no configured credential account"
            )))
        }
    };
    let lease = resolver
        .fresh_lease(provider_id, &account_id, lease)
        .await
        .map_err(|error| WorkloadError::Upstream(format!("resolve fal credential: {error}")))?;
    if lease.secret.is_empty() {
        return Err(WorkloadError::InvalidRequest(format!(
            "fal provider `{provider_id}` credential is empty"
        )));
    }
    Ok((account_id, lease))
}

#[allow(clippy::too_many_arguments)]
fn record_fal_usage(
    ledger: &sb_ledger::UsageLedger,
    registry: &sb_adapters::AdapterRegistry,
    job_id: &str,
    configured_provider_id: &str,
    model: &str,
    account_id: &str,
    tenant: &Option<String>,
    project: &Option<String>,
    started_at_ms: u64,
    artifacts: Option<&[ArtifactRecord]>,
) {
    let price = registry
        .unit_price(configured_provider_id, model)
        .or_else(|| registry.unit_price("fal", model));
    let units_consumed = artifacts.and_then(|artifacts| media_units(price, artifacts));
    let cost_micros = sb_ledger::compute_unit_cost_micros(price, units_consumed);
    ledger.record(
        sb_ledger::UsageRecord::workload(
            job_id,
            "fal",
            model,
            Some(account_id.to_string()),
            WorkloadKind::ImageGeneration,
            price.map(|price| price.pricing_unit),
            units_consumed,
            cost_micros,
            now_ms().saturating_sub(started_at_ms),
        )
        .with_tenant(tenant.clone())
        .with_project(project.clone()),
    );
}

/// Usage of a ComfyUI job, by its kind: a video job is recorded as
/// `video_generation` in `per_video` units (one per video artifact), an
/// image job as before. ComfyUI has no price: the cost stays unknown.
fn record_unpriced_media_usage(
    ledger: &sb_ledger::UsageLedger,
    job: &JobRecord,
    provider_id: &str,
) {
    if job.kind != WorkloadKind::VideoGeneration {
        // A workflow job is billed as what it ran: the ledger knows the
        // kind, and the artifacts are images either way.
        record_unpriced_image_usage(ledger, job, provider_id, None, job.kind);
        return;
    }
    let videos = job
        .artifacts
        .iter()
        .filter(|artifact| artifact.kind == ArtifactKind::Video)
        .count();
    ledger.record(
        sb_ledger::UsageRecord::workload(
            &job.id,
            provider_id,
            &job.target,
            None,
            WorkloadKind::VideoGeneration,
            Some(PricingUnit::PerVideo),
            Some(videos as f64),
            None,
            job.updated_at_ms.saturating_sub(job.created_at_ms),
        )
        .with_tenant(job.tenant.clone())
        .with_project(job.project.clone())
        .with_requested_model(job.requested_model.clone()),
    );
}

fn record_unpriced_image_usage(
    ledger: &sb_ledger::UsageLedger,
    job: &JobRecord,
    provider_id: &str,
    account_id: Option<String>,
    kind: WorkloadKind,
) {
    ledger.record(
        sb_ledger::UsageRecord::workload(
            &job.id,
            provider_id,
            &job.target,
            account_id,
            kind,
            None,
            None,
            None,
            job.updated_at_ms.saturating_sub(job.created_at_ms),
        )
        .with_tenant(job.tenant.clone())
        .with_project(job.project.clone())
        .with_requested_model(job.requested_model.clone()),
    );
}

fn media_units(price: Option<UnitPrice>, artifacts: &[ArtifactRecord]) -> Option<f64> {
    match price?.pricing_unit {
        PricingUnit::PerImage => Some(artifacts.len() as f64),
        PricingUnit::PerMegapixel => artifacts.iter().try_fold(0.0, |total, artifact| {
            let pixels = artifact.width?.checked_mul(artifact.height?)? as f64;
            Some(total + pixels / 1_000_000.0)
        }),
        PricingUnit::PerSecond => artifacts.iter().try_fold(0.0, |total, artifact| {
            Some(total + artifact.duration_ms? as f64 / 1_000.0)
        }),
        PricingUnit::PerVideo => Some(
            artifacts
                .iter()
                .filter(|artifact| artifact.kind == ArtifactKind::Video)
                .count() as f64,
        ),
        PricingUnit::PerCharacter
        | PricingUnit::TokenMetered
        | PricingUnit::Credits
        | PricingUnit::Quota => None,
    }
}

fn workflow_field(
    name: &str,
    field_type: &str,
    required: bool,
    media_type: Option<&str>,
) -> WorkflowField {
    WorkflowField {
        name: name.to_string(),
        field_type: field_type.to_string(),
        required,
        media_type: media_type.map(str::to_string),
    }
}

/// The inputs a configured workflow takes: the image inputs, plus `image`
/// (an artifact id), `frames` and `fps` where the workflow binds them.
/// `size` and `seed` show only where the workflow binds them (`width` and
/// `height`, `seed`): the same rule [`ensure_image_inputs_bound`] refuses a
/// request by, so the list never offers an input that answers 400.
fn workflow_fields_for(workflow: &ComfyUiWorkflowConfig) -> Vec<WorkflowField> {
    let binds = |name: &str| workflow.bindings.contains_key(name);
    let mut fields = workflow_kind_fields(workflow);
    fields.retain(|field| match field.name.as_str() {
        "size" => binds("width") && binds("height"),
        "seed" => binds("seed"),
        _ => true,
    });
    fields
}

fn workflow_kind_fields(workflow: &ComfyUiWorkflowConfig) -> Vec<WorkflowField> {
    let mut fields = match workflow.kind {
        WorkloadKind::VideoGeneration => vec![
            workflow_field("prompt", "string", true, None),
            workflow_field("seed", "integer", false, None),
        ],
        // A workflow may run without a prompt, and takes one only where it
        // binds it.
        WorkloadKind::WorkflowExecution => image_fields_for(workflow.kind)
            .into_iter()
            .filter(|field| field.name != "prompt" || workflow.bindings.contains_key("prompt"))
            .map(|field| WorkflowField {
                required: field.required && field.name != "prompt",
                ..field
            })
            .collect(),
        kind => image_fields_for(kind),
    };
    // A workflow job runs on its input image: where it binds one, it is
    // required.
    let image_required = workflow.kind == WorkloadKind::WorkflowExecution;
    for (name, field_type, media_type) in [
        ("image", "artifact", Some("image/*")),
        ("frames", "integer", None),
        ("fps", "integer", None),
    ] {
        if workflow.bindings.contains_key(name) {
            let required = name == "image" && image_required;
            fields.push(workflow_field(name, field_type, required, media_type));
        }
    }
    fields
}

fn image_fields_for(kind: WorkloadKind) -> Vec<WorkflowField> {
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
        WorkloadKind::TextGeneration
        | WorkloadKind::Embedding
        | WorkloadKind::VideoGeneration
        | WorkloadKind::AudioTranscription
        | WorkloadKind::AudioTranslation
        | WorkloadKind::AudioSpeech => Vec::new(),
    }
}

fn workflow_outputs_for(kind: WorkloadKind) -> Vec<WorkflowField> {
    match kind {
        WorkloadKind::VideoGeneration => {
            vec![workflow_field("video", "artifact", true, Some("video/*"))]
        }
        WorkloadKind::ImageGeneration | WorkloadKind::WorkflowExecution => vec![WorkflowField {
            name: "artifact".to_string(),
            field_type: "artifact".to_string(),
            required: true,
            media_type: None,
        }],
        WorkloadKind::TextGeneration
        | WorkloadKind::Embedding
        | WorkloadKind::AudioTranscription
        | WorkloadKind::AudioTranslation
        | WorkloadKind::AudioSpeech => Vec::new(),
    }
}

/// Refuse a `size` or `seed` the workflow cannot take (400) instead of
/// dropping it: `size` needs both `width` and `height` bindings, `seed` a
/// `seed` binding.
fn ensure_image_inputs_bound(
    provider: &sb_core::ProviderConfig,
    workflow: &ComfyUiWorkflowConfig,
    request: &ImageGenerationRequest,
) -> Result<(), WorkloadError> {
    let binds = |name: &str| workflow.bindings.contains_key(name);
    if request.size.is_some() && !(binds("width") && binds("height")) {
        return Err(WorkloadError::InvalidRequest(format!(
            "workflow `{}` does not bind `size` (it needs `width` and `height` bindings)",
            workflow.public_id(&provider.id)
        )));
    }
    if request.seed.is_some() && !binds("seed") {
        return Err(WorkloadError::InvalidRequest(format!(
            "workflow `{}` does not bind `seed`",
            workflow.public_id(&provider.id)
        )));
    }
    check_input_transforms(workflow, request, None, None)
}

/// One input whose binding transforms it: what the caller sent and what
/// the graph got. Kept in the job's `running` event and the artifacts'
/// provenance, so a caller sees what was rendered.
#[derive(Debug, Clone, PartialEq)]
struct BoundInput {
    name: String,
    sent: Json,
    used: Json,
}

/// The inputs of a ComfyUI job as `(binding, value, required)`, in binding
/// order. `image` (the uploaded file name) is bound apart, last.
///
/// The prompt is an input like any other: an image or a video needs one
/// (an empty one is refused before the job exists), a workflow may run
/// without one (a segmentation that segments everything). An empty prompt
/// is not bound at all, so the graph keeps its own default.
fn comfy_input_values(
    request: &ImageGenerationRequest,
    frames: Option<u32>,
    fps: Option<u32>,
) -> Vec<(String, Json, bool)> {
    let mut inputs = Vec::new();
    if !request.prompt.trim().is_empty() {
        inputs.push((
            "prompt".to_string(),
            Json::String(request.prompt.clone()),
            true,
        ));
    }
    if let Some(seed) = request.seed {
        inputs.push(("seed".to_string(), Json::Number(seed.into()), false));
    }
    if let Some((width, height)) = parse_size(request.size.as_deref()) {
        inputs.push(("width".to_string(), Json::Number(width.into()), false));
        inputs.push(("height".to_string(), Json::Number(height.into()), false));
    }
    for (key, value) in &request.metadata {
        inputs.push((key.clone(), Json::String(value.clone()), false));
    }
    if let Some(frames) = frames {
        inputs.push(("frames".to_string(), Json::from(frames), true));
    }
    if let Some(fps) = fps {
        inputs.push(("fps".to_string(), Json::from(fps), true));
    }
    inputs
}

/// Apply a binding's transforms to `value` (see `values`, `values_strict`,
/// `round_up`, `multiple_of` on [`sb_core::ComfyUiBindingConfig`]). A binding
/// with none returns the value untouched.
fn transform_input(
    binding: &sb_core::ComfyUiBindingConfig,
    name: &str,
    value: Json,
) -> Result<Json, WorkloadError> {
    let mut value = value;
    if let Some(values) = &binding.values {
        let key = match &value {
            Json::String(text) => text.clone(),
            other => other.to_string(),
        };
        match values.get(&key) {
            Some(mapped) => value = mapped.clone(),
            None if binding.values_strict => {
                let allowed: Vec<&str> = values.keys().map(String::as_str).collect();
                return Err(WorkloadError::InvalidRequest(format!(
                    "`{name}` must be one of: {}",
                    allowed.join(", ")
                )));
            }
            None => {}
        }
    }
    if binding.round_up.is_none() && binding.multiple_of.is_none() {
        return Ok(value);
    }
    let number = match &value {
        Json::Number(number) => number.as_u64(),
        Json::String(text) => text.trim().parse::<u64>().ok(),
        _ => None,
    };
    let Some(number) = number else {
        return Err(WorkloadError::InvalidRequest(format!(
            "`{name}` must be a whole number of 0 or more (its binding rounds it)"
        )));
    };
    let mut rounded = number;
    if let Some(round) = binding.round_up.filter(|round| round.step > 0) {
        rounded = if rounded <= round.offset {
            round.offset
        } else {
            let steps = (rounded - round.offset).div_ceil(round.step);
            round
                .offset
                .saturating_add(steps.saturating_mul(round.step))
        };
    }
    if let Some(multiple) = binding.multiple_of.filter(|multiple| *multiple > 0) {
        rounded = (rounded.saturating_add(multiple / 2) / multiple * multiple).max(multiple);
    }
    Ok(Json::from(rounded))
}

/// Check every input's transforms before a job exists, so a value a
/// binding refuses is a 400, not a failed job.
fn check_input_transforms(
    workflow: &ComfyUiWorkflowConfig,
    request: &ImageGenerationRequest,
    frames: Option<u32>,
    fps: Option<u32>,
) -> Result<(), WorkloadError> {
    for (name, value, _) in comfy_input_values(request, frames, fps) {
        if let Some(binding) = workflow.bindings.get(&name) {
            transform_input(binding, &name, value)?;
        }
    }
    Ok(())
}

/// Bind every input of a ComfyUI job into its graph. `image`, `frames` and
/// `fps` are checked against the workflow's bindings at submission;
/// `uploaded` is the name ComfyUI gave the uploaded input image, bound
/// last, so nothing else can replace it. Returns the inputs a binding
/// transformed.
fn bind_comfyui_inputs(
    graph: &mut Json,
    workflow: &ComfyUiWorkflowConfig,
    inputs: &ComfyInputs,
    uploaded: Option<&str>,
) -> Result<Vec<BoundInput>, WorkloadError> {
    let mut bound = Vec::new();
    for (name, value, required) in comfy_input_values(&inputs.request, inputs.frames, inputs.fps) {
        bind_input(graph, workflow, &name, value, required, &mut bound)?;
    }
    if let Some(name) = uploaded {
        bind_input(
            graph,
            workflow,
            "image",
            Json::String(name.to_string()),
            true,
            &mut bound,
        )?;
    }
    Ok(bound)
}

/// Bind one input: a missing binding is an error when `required`, else the
/// input is skipped. A binding with transforms records what it did.
fn bind_input(
    graph: &mut Json,
    workflow: &ComfyUiWorkflowConfig,
    name: &str,
    value: Json,
    required: bool,
    bound: &mut Vec<BoundInput>,
) -> Result<(), WorkloadError> {
    let Some(binding) = workflow.bindings.get(name) else {
        if required {
            return Err(WorkloadError::InvalidRequest(format!(
                "workflow `{}` missing required `{name}` binding",
                workflow.id
            )));
        }
        return Ok(());
    };
    let transforms =
        binding.values.is_some() || binding.round_up.is_some() || binding.multiple_of.is_some();
    let used = transform_input(binding, name, value.clone())?;
    if transforms {
        bound.push(BoundInput {
            name: name.to_string(),
            sent: value,
            used: used.clone(),
        });
    }
    set_json_path(graph, &binding.path, used)
}

/// A bound input's value as text, for event detail and provenance.
fn input_text(value: &Json) -> String {
    match value {
        Json::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// `input.<name>.sent` and `input.<name>.used` for every transformed input.
fn bound_input_detail(bound: &[BoundInput]) -> Vec<(String, String)> {
    bound
        .iter()
        .flat_map(|input| {
            [
                (
                    format!("input.{}.sent", input.name),
                    input_text(&input.sent),
                ),
                (
                    format!("input.{}.used", input.name),
                    input_text(&input.used),
                ),
            ]
        })
        .collect()
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

fn validate_single_image_request(
    request: &ImageGenerationRequest,
    adapter: &str,
    kind: WorkloadKind,
) -> Result<(), WorkloadError> {
    // A workflow may run without a prompt (a segmentation that segments
    // everything); an image or a video needs one.
    if request.prompt.trim().is_empty() && kind != WorkloadKind::WorkflowExecution {
        return Err(WorkloadError::InvalidRequest(
            "prompt is required".to_string(),
        ));
    }
    if request.n != 1 {
        return Err(WorkloadError::InvalidRequest(format!(
            "{adapter} image workflow supports n=1 per request"
        )));
    }
    if request
        .size
        .as_deref()
        .is_some_and(|size| parse_size(Some(size)).is_none())
    {
        return Err(WorkloadError::InvalidRequest(
            "size must be WIDTHxHEIGHT".to_string(),
        ));
    }
    Ok(())
}

fn fal_model_endpoint(
    base_url: &str,
    model: &str,
    suffix: Option<&str>,
) -> Result<reqwest::Url, WorkloadError> {
    if model.is_empty()
        || model.starts_with('/')
        || model.contains("..")
        || model.contains('?')
        || model.contains('#')
    {
        return Err(WorkloadError::InvalidRequest(
            "invalid fal model slug".to_string(),
        ));
    }
    let normalized = format!("{}/", base_url.trim_end_matches('/'));
    let path = suffix
        .map(|suffix| format!("{model}/{}", suffix.trim_start_matches('/')))
        .unwrap_or_else(|| model.to_string());
    reqwest::Url::parse(&normalized)
        .and_then(|base| base.join(&path))
        .map_err(|e| WorkloadError::InvalidRequest(format!("invalid fal base_url: {e}")))
}

#[allow(clippy::too_many_arguments)]
async fn poll_fal_status(
    store: &WorkloadStore,
    job_id: &str,
    cfg: &Config,
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    request_id: &str,
    key: &str,
    events: &mut Vec<JobEvent>,
) -> Result<FalPollOutcome, WorkloadError> {
    let status_url = fal_model_endpoint(
        base_url,
        model,
        Some(&format!("requests/{request_id}/status")),
    )?;
    guard_provider_url(cfg, status_url.as_str()).await?;
    let mut running_emitted = false;
    for _ in 0..120 {
        if store
            .job(job_id)
            .is_some_and(|job| job.status == JobStatus::Cancelled)
        {
            return Ok(FalPollOutcome::Cancelled);
        }
        let status: FalStatusResponse = send_json(
            client
                .get(status_url.clone())
                .header(reqwest::header::AUTHORIZATION, format!("Key {key}")),
            "poll fal queue status",
        )
        .await?;
        match status.status.to_ascii_uppercase().as_str() {
            "IN_QUEUE" | "QUEUED" => {}
            "IN_PROGRESS" | "RUNNING" => {
                if !running_emitted {
                    let event = JobEvent {
                        event: "running".to_string(),
                        status: JobStatus::Running,
                        created_at_ms: now_ms(),
                        detail: BTreeMap::from([(
                            "provider_request_id".to_string(),
                            request_id.to_string(),
                        )]),
                    };
                    events.push(event.clone());
                    store.update_job_event(job_id, JobStatus::Running, event)?;
                    running_emitted = true;
                }
            }
            "COMPLETED" | "SUCCEEDED" => return Ok(FalPollOutcome::Completed),
            "FAILED" | "ERROR" => {
                return Err(WorkloadError::Upstream(format!(
                    "fal request `{request_id}` failed"
                )));
            }
            "CANCELLED" | "CANCELED" => {
                return Err(WorkloadError::Upstream(format!(
                    "fal request `{request_id}` was cancelled"
                )));
            }
            other => {
                return Err(WorkloadError::Upstream(format!(
                    "fal request `{request_id}` returned unsupported status `{other}`"
                )));
            }
        }
        sleep(Duration::from_millis(500)).await;
    }
    Err(WorkloadError::Upstream(format!(
        "fal request `{request_id}` did not complete before timeout"
    )))
}

fn extract_fal_outputs(result: &Json) -> Vec<FalOutputRef> {
    let values: Vec<&Json> = result
        .get("images")
        .and_then(Json::as_array)
        .map(|images| images.iter().collect())
        .or_else(|| result.get("image").map(|image| vec![image]))
        .unwrap_or_default();
    values
        .into_iter()
        .filter_map(|value| {
            let url = value.get("url").and_then(Json::as_str)?.to_string();
            Some(FalOutputRef {
                url,
                media_type: value
                    .get("content_type")
                    .or_else(|| value.get("media_type"))
                    .and_then(Json::as_str)
                    .map(str::to_string),
                width: value
                    .get("width")
                    .and_then(Json::as_u64)
                    .and_then(|value| u32::try_from(value).ok()),
                height: value
                    .get("height")
                    .and_then(Json::as_u64)
                    .and_then(|value| u32::try_from(value).ok()),
            })
        })
        .collect()
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

/// [`send_json`] for a ComfyUI call: carries the provider's credential when
/// it has one, never quotes a URL that carries it, and names a refused
/// credential as such.
async fn send_comfy_json<T: for<'de> Deserialize<'de>>(
    request: reqwest::RequestBuilder,
    action: &str,
    credential: Option<&SchemeCredential>,
) -> Result<T, WorkloadError> {
    let response = with_credential(request, credential)
        .send()
        .await
        .map_err(|e| WorkloadError::Upstream(format!("{action}: {}", error_text(e, credential))))?;
    let status = response.status();
    if !status.is_success() {
        return Err(comfy_status_error(action, status));
    }
    response.json::<T>().await.map_err(|e| {
        WorkloadError::Upstream(format!(
            "{action}: invalid JSON: {}",
            error_text(e, credential)
        ))
    })
}

/// A ComfyUI answer that is not a success. 401 and 403 say that the
/// credential was missing or refused, so a ComfyUI behind a token proxy
/// without `api_key_env` reads as an auth failure, not a broken server.
fn comfy_status_error(action: &str, status: reqwest::StatusCode) -> WorkloadError {
    if refused(status) {
        WorkloadError::Upstream(format!(
            "{action} failed with status {status}: ComfyUI refused the credential (missing or wrong; set api_key_env and auth_scheme on the comfyui provider)"
        ))
    } else {
        WorkloadError::Upstream(format!("{action} failed with status {status}"))
    }
}

/// How long one ComfyUI job may take, queue time included, before
/// Switchback stops waiting for it. A cold model load after a wake, a slow
/// sampler or a video run each take well over a minute; a job that ComfyUI
/// itself reports as failed ends the wait at once (see [`comfy_job_failure`]).
const COMFYUI_JOB_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Why a job on a lane without a wake command cannot start by itself.
const WAKE_UNCONFIGURED: &str =
    "local executor wake command is unconfigured; awaiting operator wiring";

/// How often a background job on such a lane looks again whether the lane
/// came up (powered on by hand, or by another lane on the same machine).
const WAKE_UNCONFIGURED_RECHECK: Duration = Duration::from_secs(5);

/// Pause between two looks at a ComfyUI job's history.
const COMFYUI_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How long one ComfyUI job of `workflow` may take, queue time included.
/// `timeout_secs` on the workflow, else [`COMFYUI_JOB_TIMEOUT`].
fn comfy_job_timeout(workflow: &ComfyUiWorkflowConfig) -> Duration {
    workflow
        .timeout_secs
        .map(Duration::from_secs)
        .unwrap_or(COMFYUI_JOB_TIMEOUT)
}

async fn poll_comfyui_history(
    run: &ComfyAttempt<'_>,
    client: &reqwest::Client,
    base_url: &str,
    prompt_id: &str,
) -> Result<Vec<ComfyOutputRef>, WorkloadError> {
    let history_url = comfy_endpoint(base_url, &format!("history/{prompt_id}"))?;
    guard_provider_url(run.cfg, history_url.as_str()).await?;
    let timeout = comfy_job_timeout(run.workflow);
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if run.store.is_cancelled(run.job_id) {
            // The cancel stops this prompt in ComfyUI, then this task: keep
            // the lane until then (bounded by the deadline).
            if std::time::Instant::now() >= deadline {
                return Err(WorkloadError::Upstream(format!(
                    "comfyui prompt `{prompt_id}`: the job was cancelled"
                )));
            }
            sleep(COMFYUI_POLL_INTERVAL).await;
            continue;
        }
        let history: Json = send_comfy_json(
            client.get(history_url.clone()),
            "poll comfyui history",
            run.credential,
        )
        .await?;
        let outputs = extract_comfy_outputs(&history, prompt_id, &run.workflow.output_node_ids);
        if !outputs.is_empty() {
            return Ok(outputs);
        }
        if let Some(failure) = comfy_job_failure(&history, prompt_id) {
            return Err(WorkloadError::Upstream(format!(
                "comfyui prompt `{prompt_id}` {failure}"
            )));
        }
        if std::time::Instant::now() >= deadline {
            return Err(WorkloadError::Upstream(format!(
                "comfyui prompt `{prompt_id}` did not produce artifacts within {} s",
                timeout.as_secs()
            )));
        }
        sleep(COMFYUI_POLL_INTERVAL).await;
    }
}

/// Why a ComfyUI job that has finished will never produce an artifact: an
/// execution error, or a run that completed without outputs. `None` while
/// the job is still queued or running (ComfyUI answers `{}` until then).
/// Metadata only: the failing node's type and the exception's type, never
/// the exception's message, which can quote the job's inputs.
fn comfy_job_failure(history: &Json, prompt_id: &str) -> Option<String> {
    let status = history.get(prompt_id)?.get("status")?;
    if status.get("status_str").and_then(Json::as_str) == Some("error") {
        let error = status
            .get("messages")
            .and_then(Json::as_array)
            .and_then(|messages| {
                messages.iter().find(|message| {
                    message.get(0).and_then(Json::as_str) == Some("execution_error")
                })
            })
            .and_then(|message| message.get(1));
        let field = |name: &str| {
            error
                .and_then(|error| error.get(name))
                .and_then(Json::as_str)
                .map(str::to_string)
        };
        let node = field("node_type").unwrap_or_else(|| "unknown node".to_string());
        let kind = field("exception_type").unwrap_or_else(|| "unknown error".to_string());
        return Some(format!("failed in ComfyUI ({node}: {kind})"));
    }
    if status.get("completed").and_then(Json::as_bool) == Some(true) {
        return Some("completed in ComfyUI without artifacts".to_string());
    }
    None
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
    // Saved images and core video nodes answer `images` (a video with
    // `animated`); common video node packs answer `gifs` (with `format` and
    // `frame_rate`), others `videos` or `video`. Only `filename`,
    // `subfolder`, `type` and `frame_rate` are read: never a local path.
    for key in ["images", "gifs", "videos", "video"] {
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
                frame_rate: item.get("frame_rate").and_then(Json::as_f64),
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

/// The media types an artifact is stored and served with: images and videos
/// a browser shows and never runs. Anything else an upstream names
/// (`text/html`, `image/svg+xml`, ...) is not trusted: it would be served
/// back from the gateway's own origin.
const SAFE_MEDIA_TYPES: [&str; 7] = [
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
    "video/mp4",
    "video/webm",
    "video/quicktime",
];

/// Whether `media_type` is on [`SAFE_MEDIA_TYPES`].
pub(crate) fn is_safe_media_type(media_type: &str) -> bool {
    SAFE_MEDIA_TYPES.contains(&media_type)
}

fn media_type_for_filename(filename: &str) -> &'static str {
    match filename
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
    {
        Some(ext) if ext == "png" => "image/png",
        Some(ext) if ext == "jpg" || ext == "jpeg" => "image/jpeg",
        Some(ext) if ext == "webp" => "image/webp",
        Some(ext) if ext == "gif" => "image/gif",
        Some(ext) if ext == "mp4" => "video/mp4",
        Some(ext) if ext == "webm" => "video/webm",
        Some(ext) if ext == "mov" => "video/quicktime",
        _ => "application/octet-stream",
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

/// One ComfyUI attempt's inputs. The managed lane may run several (a
/// self-heal requeues the job after a health loss).
struct ComfyAttempt<'a> {
    cfg: &'a Config,
    store: &'a WorkloadStore,
    job_id: &'a str,
    provider: &'a sb_core::ProviderConfig,
    workflow: &'a ComfyUiWorkflowConfig,
    inputs: &'a ComfyInputs,
    owner: &'a Owner,
    credential: Option<&'a SchemeCredential>,
}

/// One ComfyUI dispatch attempt: bind the inputs, queue the prompt, poll
/// the history, and stream each output into the artifact store. Progress
/// goes to the job as events (`running` with ComfyUI's prompt id,
/// `history_polled`, `artifact_ready`); the caller marks it succeeded.
async fn comfyui_dispatch_once(
    run: &ComfyAttempt<'_>,
) -> Result<Vec<ArtifactRecord>, WorkloadError> {
    let ProviderKind::ComfyUi { base_url, .. } = &run.provider.kind else {
        return Err(WorkloadError::InvalidRequest(format!(
            "provider `{}` is not a comfyui provider",
            run.provider.id
        )));
    };
    let credential = run.credential;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .redirect(redirect_policy(credential))
        .build()
        .map_err(|e| WorkloadError::Internal(format!("build comfyui client: {e}")))?;
    let uploaded = match &run.inputs.image {
        Some(artifact_id) => Some(upload_input_image(run, &client, base_url, artifact_id).await?),
        None => None,
    };
    let mut graph = run.workflow.graph.clone();
    let bound = bind_comfyui_inputs(&mut graph, run.workflow, run.inputs, uploaded.as_deref())?;
    let bound = bound_input_detail(&bound);

    let client_id = format!("switchback-{}", run.job_id);
    let queue_url = comfy_endpoint(base_url, "prompt")?;
    guard_provider_url(run.cfg, queue_url.as_str()).await?;

    run.store.begin_submit(run.job_id);
    let queued: Result<ComfyPromptResponse, WorkloadError> = send_comfy_json(
        client
            .post(queue_url)
            .json(&serde_json::json!({"prompt": graph, "client_id": client_id})),
        "queue comfyui prompt",
        credential,
    )
    .await;
    let queued_run = queued.as_ref().ok().map(|queued| ComfyRun {
        provider_id: run.provider.id.clone(),
        prompt_id: queued.prompt_id.clone(),
        target: run.inputs.request.model.clone(),
    });
    if let Some(stale) = run.store.end_submit(run.job_id, queued_run) {
        // Cancelled while ComfyUI was queueing it: stop it now, while this
        // task still holds the lane.
        let outcome = match stop_comfy_prompt(run.cfg, base_url, credential, &stale.prompt_id).await
        {
            Ok(()) => "sent".to_string(),
            Err(error) => format!("failed: {error}"),
        };
        run.store.note_upstream_cancel(run.job_id, outcome);
        return Err(WorkloadError::Upstream(format!(
            "comfyui prompt `{}`: the job was cancelled",
            stale.prompt_id
        )));
    }
    let queued = queued?;
    if queued
        .node_errors
        .as_ref()
        .is_some_and(|value| !json_is_empty(value))
    {
        return Err(WorkloadError::Upstream(
            "comfyui rejected workflow graph with node_errors".to_string(),
        ));
    }
    run.store.push_event(
        run.job_id,
        JobStatus::Running,
        "running",
        BTreeMap::from([
            ("prompt_id".to_string(), queued.prompt_id.clone()),
            (
                "number".to_string(),
                queued.number.unwrap_or_default().to_string(),
            ),
        ])
        .into_iter()
        .chain(bound.iter().cloned())
        .collect(),
    );

    let outputs = poll_comfyui_history(run, &client, base_url, &queued.prompt_id).await?;
    run.store.push_event(
        run.job_id,
        JobStatus::Running,
        "history_polled",
        BTreeMap::from([
            ("prompt_id".to_string(), queued.prompt_id.clone()),
            ("outputs".to_string(), outputs.len().to_string()),
        ]),
    );

    let mut records = Vec::new();
    for output in outputs {
        let view_url = comfy_endpoint(base_url, "view")?;
        guard_provider_url(run.cfg, view_url.as_str()).await?;
        // A video can take minutes to fetch: the body read gets the
        // workflow's whole timeout, not the client's minute.
        let view = client
            .get(view_url)
            .timeout(comfy_job_timeout(run.workflow))
            .query(&[
                ("filename", output.filename.as_str()),
                ("subfolder", output.subfolder.as_str()),
                ("type", output.type_.as_str()),
            ]);
        let mut response = with_credential(view, credential)
            .send()
            .await
            .map_err(|e| {
                WorkloadError::Upstream(format!(
                    "fetch comfyui artifact: {}",
                    error_text(e, credential)
                ))
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(comfy_status_error("fetch comfyui artifact", status));
        }
        let media_type = output_media_type(
            response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            &output.filename,
        );
        let artifact_id = sb_core::new_id("art");
        let provenance = BTreeMap::from([
            ("provider".to_string(), run.provider.id.clone()),
            ("workflow".to_string(), run.workflow.id.clone()),
            ("prompt_id".to_string(), queued.prompt_id.clone()),
            ("node_id".to_string(), output.node_id.clone()),
            ("filename".to_string(), output.filename.clone()),
            ("subfolder".to_string(), output.subfolder.clone()),
            ("type".to_string(), output.type_.clone()),
        ])
        .into_iter()
        .chain(bound.iter().cloned())
        // The public id, when the workflow sets one (absent before).
        .chain(
            run.workflow
                .model_id
                .clone()
                .map(|model_id| ("model".to_string(), model_id)),
        )
        .collect::<BTreeMap<_, _>>();
        let record = ArtifactRecord {
            artifact_id: artifact_id.clone(),
            job_id: run.job_id.to_string(),
            kind: artifact_kind_for_media_type(&media_type),
            media_type: media_type.clone(),
            bytes: 0,
            sha256: String::new(),
            storage_ref: String::new(),
            width: None,
            height: None,
            duration_ms: None,
            fps: output.frame_rate,
            created_at_ms: now_ms(),
            retention: String::new(),
            provenance,
        };
        // Stream the body into the store as it arrives: a long video never
        // sits in memory whole on a disk store.
        let mut writer = run
            .store
            .artifacts
            .writer(record, run.owner.clone())
            .map_err(|error| WorkloadError::Internal(error.to_string()))?;
        while let Some(chunk) = response.chunk().await.map_err(|e| {
            WorkloadError::Upstream(format!(
                "read comfyui artifact: {}",
                error_text(e, credential)
            ))
        })? {
            writer
                .write(&chunk)
                .map_err(|error| WorkloadError::Upstream(error.to_string()))?;
        }
        let record = writer
            .finish()
            .map_err(|error| WorkloadError::Upstream(error.to_string()))?;
        run.store.push_event(
            run.job_id,
            JobStatus::ArtifactReady,
            "artifact_ready",
            BTreeMap::from([
                ("artifact_id".to_string(), artifact_id),
                ("media_type".to_string(), media_type),
            ]),
        );
        records.push(record);
    }

    if records.is_empty() {
        return Err(WorkloadError::Upstream(
            "comfyui history completed without artifacts".to_string(),
        ));
    }
    Ok(records)
}

#[derive(Debug, Deserialize)]
struct ComfyUploadResponse {
    name: String,
    #[serde(default)]
    subfolder: String,
}

/// Upload a job's input image (an artifact) to ComfyUI with the provider's
/// credential (`POST /upload/image`, multipart field `image`, into ComfyUI's
/// input folder) and return the name a `LoadImage` node takes. The file is
/// named after the artifact id, never after anything the caller wrote.
async fn upload_input_image(
    run: &ComfyAttempt<'_>,
    client: &reqwest::Client,
    base_url: &str,
    artifact_id: &str,
) -> Result<String, WorkloadError> {
    let (record, _, bytes) = run.store.artifacts.read_all(artifact_id).map_err(|_| {
        WorkloadError::InvalidRequest(format!(
            "input artifact `{artifact_id}` is no longer stored"
        ))
    })?;
    let url = comfy_endpoint(base_url, "upload/image")?;
    guard_provider_url(run.cfg, url.as_str()).await?;
    let boundary = format!("switchback-{}", sb_core::new_id("part"));
    let filename = format!(
        "switchback-{artifact_id}.{}",
        extension_for_media_type(&record.media_type)
    );
    let body = multipart_image(&boundary, &filename, &record.media_type, &bytes);
    let uploaded: ComfyUploadResponse = send_comfy_json(
        client
            .post(url)
            .header(
                reqwest::header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body),
        "upload input image to comfyui",
        run.credential,
    )
    .await?;
    run.store.push_event(
        run.job_id,
        JobStatus::Running,
        "input_uploaded",
        BTreeMap::from([("artifact_id".to_string(), artifact_id.to_string())]),
    );
    Ok(if uploaded.subfolder.is_empty() {
        uploaded.name
    } else {
        format!("{}/{}", uploaded.subfolder, uploaded.name)
    })
}

/// A `multipart/form-data` body with one image file (field `image`) and
/// `type=input`, `overwrite=true`: the form ComfyUI's upload takes.
fn multipart_image(boundary: &str, filename: &str, media_type: &str, bytes: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(bytes.len() + 512);
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"{filename}\"\r\nContent-Type: {media_type}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    for (name, value) in [("type", "input"), ("overwrite", "true")] {
        body.extend_from_slice(
            format!(
                "\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

fn extension_for_media_type(media_type: &str) -> &'static str {
    match media_type {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        _ => "png",
    }
}

/// The media type of one ComfyUI output: the `/view` answer's when it is an
/// image or video type on [`SAFE_MEDIA_TYPES`], else (missing, generic such
/// as `application/octet-stream`, or anything else, `text/html` included)
/// the file name's, which is also only ever a safe type or
/// `application/octet-stream`.
fn output_media_type(header: Option<&str>, filename: &str) -> String {
    match header.map(|value| {
        value
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
    }) {
        Some(value) if is_safe_media_type(&value) => value,
        _ => media_type_for_filename(filename).to_string(),
    }
}

/// Why a managed lane could not run a job, as the caller reads it.
fn lane_error(provider_id: &str, error: JobError<WorkloadError>) -> WorkloadError {
    match error {
        JobError::Queued => WorkloadError::Upstream(format!(
            "local executor `{provider_id}` wake command is unconfigured"
        )),
        JobError::BootTimeout {
            wake_path,
            elapsed_ms,
        } => WorkloadError::Upstream(format!(
            "local executor `{provider_id}` did not reach health after wake `{wake_path}` in {elapsed_ms}ms"
        )),
        JobError::WakeFailed(error) => WorkloadError::Upstream(format!(
            "local executor `{provider_id}` wake command failed: {error}"
        )),
        JobError::RetriesExhausted { budget, last_error } => WorkloadError::Upstream(format!(
            "local executor `{provider_id}` self-heal budget {budget} exhausted: {last_error}"
        )),
        JobError::Dispatch(error) => error,
        JobError::GpuUnavailable(detail) | JobError::Refused(detail) => {
            WorkloadError::Upstream(format!("local executor `{provider_id}` {detail}"))
        }
        JobError::QueueTimeout {
            detail,
            retry_after,
        } => WorkloadError::LaneQueueTimeout {
            message: format!("local executor `{provider_id}` {detail}; retry later"),
            retry_after_secs: retry_after.as_secs().max(1),
        },
    }
}

/// The decision a job starts with, and the provider facts for its
/// `accepted` event. The adapter adds to it as the job runs.
fn initial_decision(
    cfg: &Config,
    job_id: &str,
    kind: WorkloadKind,
    target: &str,
) -> (RouteDecision, BTreeMap<String, String>) {
    let mut detail = BTreeMap::new();
    if let Some((provider, workflow)) = resolve_comfyui_workflow(cfg, target) {
        let mut decision = RouteDecision::new(job_id, "workload/comfyui");
        decision.selected = Some(TargetRef::new(target));
        decision.add_reason(format!("workload={}", kind_name(kind)));
        decision.add_reason(format!("provider={}", provider.id));
        decision.add_reason(format!("workflow={}", workflow.id));
        decision.add_reason("adapter=comfyui");
        detail.insert("provider".to_string(), provider.id.clone());
        detail.insert("workflow".to_string(), workflow.id.clone());
        return (decision, detail);
    }
    if let Some((provider, model)) = resolve_fal_model(cfg, target) {
        detail.insert("provider".to_string(), provider.id.clone());
        return (
            fal_route_decision(job_id, target, &provider.id, &model),
            detail,
        );
    }
    let mut decision = RouteDecision::new(job_id, "workload/mock_image");
    decision.selected = Some(TargetRef::new(target));
    decision.add_reason("workload=image_generation");
    decision.add_reason("workflow=mock-image");
    decision.add_reason("adapter=mock");
    (decision, detail)
}

fn kind_name(kind: WorkloadKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Stop a ComfyUI prompt a cancelled job queued, with the provider's
/// credential: remove it from ComfyUI's queue (`POST /queue` with
/// `{"delete": [id]}`; nothing happens once it runs), then interrupt it
/// (`POST /interrupt` with `{"prompt_id": id}`), only while `GET /queue`
/// lists it as running: an older ComfyUI ignores the field and interrupts
/// whatever runs. Never quotes a URL in its error.
async fn cancel_comfy_prompt(
    cfg: &Config,
    resolver: &CredentialResolver,
    run: &ComfyRun,
) -> Result<(), String> {
    let provider = cfg
        .providers
        .iter()
        .find(|provider| provider.id == run.provider_id)
        .ok_or_else(|| format!("provider `{}` is no longer configured", run.provider_id))?;
    let ProviderKind::ComfyUi { base_url, .. } = &provider.kind else {
        return Err(format!("provider `{}` is no longer comfyui", provider.id));
    };
    let credential = SchemeCredential::resolve(resolver, provider, &run.target).await?;
    stop_comfy_prompt(cfg, base_url, credential.as_ref(), &run.prompt_id).await
}

/// The ComfyUI calls of [`cancel_comfy_prompt`], with the credential at hand.
async fn stop_comfy_prompt(
    cfg: &Config,
    base_url: &str,
    credential: Option<&SchemeCredential>,
    prompt_id: &str,
) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(redirect_policy(credential))
        .build()
        .map_err(|e| format!("build comfyui client: {e}"))?;
    let post = |path: &'static str, body: Json, action: &'static str| {
        let client = client.clone();
        async move {
            let url = comfy_endpoint(base_url, path).map_err(|error| error.to_string())?;
            guard_provider_url(cfg, url.as_str())
                .await
                .map_err(|error| error.to_string())?;
            let response = with_credential(client.post(url).json(&body), credential)
                .send()
                .await
                .map_err(|e| format!("{action}: {}", error_text(e, credential)))?;
            if !response.status().is_success() {
                return Err(comfy_status_error(action, response.status()).to_string());
            }
            Ok::<(), String>(())
        }
    };
    post(
        "queue",
        serde_json::json!({"delete": [prompt_id]}),
        "remove comfyui prompt from its queue",
    )
    .await?;
    // An older ComfyUI ignores `prompt_id` on `/interrupt` and stops
    // whatever runs: on a shared ComfyUI that may be another caller's job.
    // Interrupt only while ComfyUI says this prompt is the one running.
    let queue_url = comfy_endpoint(base_url, "queue").map_err(|error| error.to_string())?;
    guard_provider_url(cfg, queue_url.as_str())
        .await
        .map_err(|error| error.to_string())?;
    let queue: Json = send_comfy_json(client.get(queue_url), "read comfyui queue", credential)
        .await
        .map_err(|error| error.to_string())?;
    if !prompt_is_running(&queue, prompt_id) {
        return Ok(());
    }
    post(
        "interrupt",
        serde_json::json!({"prompt_id": prompt_id}),
        "interrupt comfyui prompt",
    )
    .await
}

/// Whether ComfyUI's `GET /queue` answer lists `prompt_id` as running
/// (`queue_running` entries are `[number, prompt_id, ...]`).
fn prompt_is_running(queue: &Json, prompt_id: &str) -> bool {
    queue
        .get("queue_running")
        .and_then(Json::as_array)
        .is_some_and(|running| {
            running
                .iter()
                .any(|entry| entry.get(1).and_then(Json::as_str) == Some(prompt_id))
        })
}

/// Validate a ComfyUI image request (prompt + single-image n) before dispatch.
fn validate_comfyui_request(
    request: &ImageGenerationRequest,
    kind: WorkloadKind,
) -> Result<(), WorkloadError> {
    // A workflow may run without a prompt; an image or a video needs one.
    if request.prompt.trim().is_empty() && kind != WorkloadKind::WorkflowExecution {
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accepted(id: &str, created_at_ms: u64) -> JobRecord {
        JobRecord {
            id: id.to_string(),
            kind: WorkloadKind::ImageGeneration,
            status: JobStatus::Accepted,
            target: "comfy/txt2img".to_string(),
            requested_model: None,
            tenant: None,
            project: None,
            created_at_ms,
            updated_at_ms: created_at_ms,
            route_decision: RouteDecision::new(id, "workload/comfyui"),
            artifacts: Vec::new(),
            events: Vec::new(),
            prompt_stored: false,
        }
    }

    fn binding(yaml: &str) -> sb_core::ComfyUiBindingConfig {
        serde_json::from_value(serde_yaml_value(yaml)).unwrap()
    }

    fn serde_yaml_value(text: &str) -> Json {
        let cfg = sb_core::Config::from_yaml(&format!(
            r#"
providers:
  - id: comfy
    type: comfyui
    base_url: "http://comfy.example:8188"
    workflows:
      - id: wf
        kind: image_generation
        graph: {{}}
        bindings:
          x: {text}
"#
        ))
        .unwrap();
        let ProviderKind::ComfyUi { workflows, .. } = &cfg.providers[0].kind else {
            unreachable!()
        };
        serde_json::to_value(&workflows[0].bindings["x"]).unwrap()
    }

    fn bad_request(result: Result<Json, WorkloadError>) -> String {
        match result {
            Err(WorkloadError::InvalidRequest(message)) => message,
            other => panic!("expected a 400, got {other:?}"),
        }
    }

    // `values`: a mapped value is replaced; an unmapped one passes through
    // unless `values_strict`, which refuses it and names the keys allowed.
    #[test]
    fn a_values_map_replaces_known_values_and_strict_refuses_others() {
        let open = binding(
            r#"{ path: ["1"], values: { "16:9": "16:9 (Widescreen)", "1:1": "1:1 (Square)" } }"#,
        );
        assert_eq!(
            transform_input(&open, "aspect", Json::from("16:9")).unwrap(),
            Json::from("16:9 (Widescreen)")
        );
        assert_eq!(
            transform_input(&open, "aspect", Json::from("4:3 (Classic)")).unwrap(),
            Json::from("4:3 (Classic)"),
            "a full label passes through"
        );
        let strict = binding(
            r#"{ path: ["1"], values: { "16:9": "16:9 (Widescreen)", "1:1": "1:1 (Square)" }, values_strict: true }"#,
        );
        let message = bad_request(transform_input(&strict, "aspect", Json::from("4:3")));
        assert_eq!(message, "`aspect` must be one of: 16:9, 1:1");
    }

    // `round_up`: 48 with step 17, offset 5 becomes 56; 56 stays 56; a
    // value below the offset becomes the offset.
    #[test]
    fn round_up_raises_to_the_next_step() {
        let round = binding(r#"{ path: ["1"], round_up: { step: 17, offset: 5 } }"#);
        assert_eq!(
            transform_input(&round, "frames", Json::from(48)).unwrap(),
            Json::from(56)
        );
        assert_eq!(
            transform_input(&round, "frames", Json::from(56)).unwrap(),
            Json::from(56)
        );
        assert_eq!(
            transform_input(&round, "frames", Json::from(57)).unwrap(),
            Json::from(73)
        );
        assert_eq!(
            transform_input(&round, "frames", Json::from("48")).unwrap(),
            Json::from(56)
        );
        assert_eq!(
            transform_input(&round, "frames", Json::from(2)).unwrap(),
            Json::from(5)
        );
    }

    // `multiple_of` on the width and height of a size: nearest multiple,
    // never below one.
    #[test]
    fn multiple_of_rounds_a_size_to_the_nearest_multiple() {
        let multiple = binding(r#"{ path: ["1"], multiple_of: 32 }"#);
        for (sent, used) in [(1000, 992), (1010, 1024), (1024, 1024), (5, 32), (0, 32)] {
            assert_eq!(
                transform_input(&multiple, "width", Json::from(sent)).unwrap(),
                Json::from(used),
                "{sent}"
            );
        }
        let request = ImageGenerationRequest {
            model: "comfy/wf".to_string(),
            prompt: "p".to_string(),
            size: Some("1000x1010".to_string()),
            seed: None,
            response_format: None,
            n: 1,
            metadata: BTreeMap::new(),
        };
        let workflow = ComfyUiWorkflowConfig {
            id: "wf".to_string(),
            kind: WorkloadKind::ImageGeneration,
            version: None,
            graph: serde_json::json!({"3": {"inputs": {"width": 1, "height": 1}}, "6": {"inputs": {"text": ""}}}),
            bindings: BTreeMap::from([
                (
                    "prompt".to_string(),
                    binding(r#"{ path: ["6", "inputs", "text"] }"#),
                ),
                (
                    "width".to_string(),
                    binding(r#"{ path: ["3", "inputs", "width"], multiple_of: 32 }"#),
                ),
                (
                    "height".to_string(),
                    binding(r#"{ path: ["3", "inputs", "height"], multiple_of: 32 }"#),
                ),
            ]),
            output_node_ids: Vec::new(),
            timeout_secs: None,
            model_id: None,
            aliases: Vec::new(),
        };
        let inputs = ComfyInputs {
            request,
            kind: WorkloadKind::ImageGeneration,
            image: None,
            frames: None,
            fps: None,
        };
        let mut graph = workflow.graph.clone();
        let bound = bind_comfyui_inputs(&mut graph, &workflow, &inputs, None).unwrap();
        assert_eq!(graph["3"]["inputs"]["width"], 992);
        assert_eq!(graph["3"]["inputs"]["height"], 1024);
        assert_eq!(
            bound_input_detail(&bound),
            [
                ("input.width.sent".to_string(), "1000".to_string()),
                ("input.width.used".to_string(), "992".to_string()),
                ("input.height.sent".to_string(), "1010".to_string()),
                ("input.height.used".to_string(), "1024".to_string()),
            ]
        );
    }

    // A rounding binding refuses what is not a whole number (400).
    #[test]
    fn a_rounding_binding_refuses_a_non_number() {
        let round = binding(r#"{ path: ["1"], round_up: { step: 8 } }"#);
        for sent in [
            Json::from("many"),
            Json::from(-3),
            Json::from(2.5),
            Json::Null,
        ] {
            let message = bad_request(transform_input(&round, "frames", sent));
            assert!(
                message.contains("`frames` must be a whole number"),
                "{message}"
            );
        }
    }

    // A binding with no transform binds exactly as before: the value is
    // untouched (byte for byte) and nothing is recorded.
    #[test]
    fn a_binding_without_a_transform_is_unchanged() {
        let plain = binding(r#"{ path: ["1"] }"#);
        assert_eq!(
            serde_json::to_string(&plain).unwrap(),
            r#"{"path":["1"]}"#,
            "no new keys appear in a plain binding"
        );
        for sent in [Json::from("16:9"), Json::from(48), Json::from("x y  z")] {
            let used = transform_input(&plain, "any", sent.clone()).unwrap();
            assert_eq!(
                serde_json::to_vec(&used).unwrap(),
                serde_json::to_vec(&sent).unwrap()
            );
        }
        let mut graph = serde_json::json!({"6": {"inputs": {"text": ""}}});
        let mut bound = Vec::new();
        bind_input(
            &mut graph,
            &ComfyUiWorkflowConfig {
                id: "wf".to_string(),
                kind: WorkloadKind::ImageGeneration,
                version: None,
                graph: Json::Null,
                bindings: BTreeMap::from([(
                    "prompt".to_string(),
                    binding(r#"{ path: ["6", "inputs", "text"] }"#),
                )]),
                output_node_ids: Vec::new(),
                timeout_secs: None,
                model_id: None,
                aliases: Vec::new(),
            },
            "prompt",
            Json::from("a lighthouse"),
            true,
            &mut bound,
        )
        .unwrap();
        assert_eq!(
            graph,
            serde_json::json!({"6": {"inputs": {"text": "a lighthouse"}}})
        );
        assert!(bound.is_empty());
    }

    // A job task that panics frees its pending slot (the tenant's count of
    // queued or running jobs) and fails its job, instead of leaving both
    // behind for good.
    #[tokio::test]
    async fn a_job_task_that_panics_frees_its_slot_and_fails_its_job() {
        let store = WorkloadStore::new();
        let id = "job_panics";
        {
            let mut inner = store.lock_inner();
            inner.tasks.insert(
                id.to_string(),
                JobTask {
                    abort: None,
                    tenant: Some("team".to_string()),
                    background: true,
                },
            );
            inner.submitting.insert(id.to_string());
            store.put_job(&mut inner, accepted(id, 1));
        }
        let handle = store.spawn_task(id, true, async {
            panic!("a bug in the job's task");
        });
        assert!(handle.await.unwrap_err().is_panic());
        let inner = store.lock_inner();
        assert!(inner.tasks.is_empty(), "the pending slot is freed");
        assert!(inner.submitting.is_empty());
        drop(inner);
        let job = store.job(id).expect("the job is kept");
        assert_eq!(job.status, JobStatus::Failed);
        let failed = job.events.last().expect("a failed event");
        assert_eq!(failed.detail["error_class"], "internal");
    }

    // A ComfyUI job that keeps failing (a client retrying in a loop) keeps
    // at most `KEPT_FAILED_COMFY_JOBS` failed jobs, the newest.
    #[test]
    fn failed_comfyui_jobs_are_bounded_oldest_first() {
        let store = WorkloadStore::new();
        for n in 0..KEPT_FAILED_COMFY_JOBS + 10 {
            let id = format!("job_{n}");
            let mut inner = store.lock_inner();
            store.put_job(&mut inner, accepted(&id, n as u64));
            store.fail_locked(
                &mut inner,
                &id,
                &WorkloadError::Upstream(format!("failure {n}")),
            );
        }
        let jobs = store.jobs();
        assert_eq!(jobs.len(), KEPT_FAILED_COMFY_JOBS);
        assert!(jobs.iter().all(|job| job.status == JobStatus::Failed));
        let oldest = jobs.iter().map(|job| job.created_at_ms).min();
        assert_eq!(oldest, Some(10), "the ten oldest are gone");
    }

    // The store never holds more than `KEPT_JOBS` jobs: the oldest finished
    // ones go first, an unfinished one is never dropped.
    #[test]
    fn the_job_list_is_bounded_and_keeps_unfinished_jobs() {
        let store = WorkloadStore::new();
        let mut inner = store.lock_inner();
        store.put_job(&mut inner, accepted("job_running", 0));
        for n in 1..=KEPT_JOBS as u64 {
            let mut job = accepted(&format!("job_{n}"), n);
            job.status = JobStatus::Succeeded;
            store.put_job(&mut inner, job);
        }
        assert!(inner.jobs.len() <= KEPT_JOBS);
        assert!(inner.jobs.contains_key("job_running"));
        assert!(
            !inner.jobs.contains_key("job_1"),
            "the oldest finished job went"
        );
        assert!(inner.jobs.contains_key(&format!("job_{KEPT_JOBS}")));
    }

    #[test]
    fn an_upstream_media_type_is_kept_only_when_it_is_a_safe_image_or_video() {
        assert_eq!(output_media_type(Some("image/png"), "a.png"), "image/png");
        assert_eq!(
            output_media_type(Some("Video/MP4; codecs=avc1"), "a.bin"),
            "video/mp4"
        );
        // A generic answer: the file name decides.
        assert_eq!(
            output_media_type(Some("application/octet-stream"), "clip.webm"),
            "video/webm"
        );
        // Never a type a browser would run from the gateway's origin.
        assert_eq!(output_media_type(Some("text/html"), "out.png"), "image/png");
        assert_eq!(
            output_media_type(Some("image/svg+xml"), "out.svg"),
            "application/octet-stream"
        );
        assert_eq!(
            output_media_type(Some("text/html"), "page.html"),
            "application/octet-stream"
        );
        assert_eq!(output_media_type(None, "noext"), "application/octet-stream");
    }
}
