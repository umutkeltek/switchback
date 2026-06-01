//! Global admission control + bounded backpressure (Oracle #8).
//!
//! A request waits up to the admission timeout for a permit (bounded
//! backpressure — bursts queue instead of overwhelming upstreams); if the wait
//! is exceeded the request is SHED with 503 rather than queuing unboundedly. When
//! a state store is configured, permits are durable coordination slots shared
//! across gateway processes. Otherwise a process-local [`Semaphore`] is used.
//! The permit is held for the request's whole life (moved into the SSE encoder
//! closure for streams), so it is released only when the response is fully
//! delivered or the client hangs up.
//!
//! This is the global counterpart to the per-tenant concurrency limit in
//! [`crate::tenancy`]: global protects the gateway, per-tenant protects fairness.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use sb_store::StateStore;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::AppState;

#[derive(Clone)]
pub struct Admission {
    /// `None` = unlimited (no global cap configured).
    sem: Option<Arc<Semaphore>>,
    timeout: Duration,
    limit: Option<u32>,
}

impl Admission {
    pub fn new(max_concurrency: Option<u32>, timeout_ms: u64) -> Self {
        Self {
            sem: max_concurrency.map(|n| Arc::new(Semaphore::new(n as usize))),
            timeout: Duration::from_millis(timeout_ms),
            limit: max_concurrency,
        }
    }

    /// Acquire a process-local in-flight permit. Returns the held permit (None when
    /// unlimited) + the queue-wait in ms, or `Err(503)` if the wait exceeds the
    /// admission timeout (load shed).
    async fn acquire_local(&self) -> Result<(Option<AdmissionGuard>, u64), Response> {
        let Some(sem) = &self.sem else {
            return Ok((None, 0));
        };
        let start = Instant::now();
        match tokio::time::timeout(self.timeout, sem.clone().acquire_owned()).await {
            Ok(Ok(permit)) => Ok((
                Some(AdmissionGuard::local(permit)),
                start.elapsed().as_millis() as u64,
            )),
            // The semaphore is never closed in practice; treat as unlimited.
            Ok(Err(_)) => Ok((None, start.elapsed().as_millis() as u64)),
            Err(_) => Err(overloaded_response()),
        }
    }

    /// Currently available permits (for `/v1/health`); `None` when unlimited.
    pub fn available(&self) -> Option<usize> {
        self.sem.as_ref().map(|s| s.available_permits())
    }

    pub fn limit(&self) -> Option<u32> {
        self.limit
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }
}

pub async fn acquire(state: &AppState) -> Result<(Option<AdmissionGuard>, u64), Response> {
    let Some(max) = state.admission.limit() else {
        return Ok((None, 0));
    };
    if let Some(store) = state.engine.store() {
        let ttl_ms = state.snapshot().config.server.admission_slot_ttl_ms;
        match acquire_durable(store.clone(), max, ttl_ms, state.admission.timeout()).await {
            Ok((guard, queue_ms)) => return Ok((Some(guard), queue_ms)),
            Err(AdmissionAcquireError::Timeout) => return Err(overloaded_response()),
            Err(AdmissionAcquireError::Store(e)) if state.engine.store_required() => {
                return Err(coordination_error_response(e));
            }
            Err(AdmissionAcquireError::Store(e)) => {
                tracing::warn!(error = %e, "durable global admission failed; falling back to process-local semaphore");
            }
        }
    }
    state.admission.acquire_local().await
}

pub fn available(state: &AppState) -> Option<usize> {
    let max = state.admission.limit()? as usize;
    if let Some(store) = state.engine.store() {
        match store.admission_slot_count() {
            Ok(active) => return Some(max.saturating_sub(active as usize)),
            Err(e) if state.engine.store_required() => {
                tracing::warn!(error = %e, "required global admission count failed");
                return Some(0);
            }
            Err(e) => {
                tracing::warn!(error = %e, "durable global admission count failed; falling back to process-local semaphore");
            }
        }
    }
    state.admission.available()
}

enum AdmissionAcquireError {
    Timeout,
    Store(String),
}

async fn acquire_durable(
    store: Arc<dyn StateStore>,
    max: u32,
    ttl_ms: u64,
    timeout: Duration,
) -> Result<(AdmissionGuard, u64), AdmissionAcquireError> {
    let start = Instant::now();
    loop {
        let slot_id = sb_core::new_id("admit");
        match store.admission_slot_acquire(&slot_id, max, ttl_ms) {
            Ok(true) => {
                return Ok((
                    AdmissionGuard::durable(store, slot_id, ttl_ms),
                    start.elapsed().as_millis() as u64,
                ));
            }
            Ok(false) => {}
            Err(e) => return Err(AdmissionAcquireError::Store(e.to_string())),
        }
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            return Err(AdmissionAcquireError::Timeout);
        }
        tokio::time::sleep((timeout - elapsed).min(Duration::from_millis(10))).await;
    }
}

pub struct AdmissionGuard {
    inner: AdmissionGuardInner,
}

enum AdmissionGuardInner {
    Local(OwnedSemaphorePermit),
    Durable {
        store: Arc<dyn StateStore>,
        slot_id: String,
        renewal: Option<crate::lease::RenewalGuard>,
    },
}

impl AdmissionGuard {
    fn local(permit: OwnedSemaphorePermit) -> Self {
        Self {
            inner: AdmissionGuardInner::Local(permit),
        }
    }

    fn durable(store: Arc<dyn StateStore>, slot_id: String, ttl_ms: u64) -> Self {
        let renewal =
            crate::lease::RenewalGuard::admission_slot(store.clone(), slot_id.clone(), ttl_ms);
        Self {
            inner: AdmissionGuardInner::Durable {
                store,
                slot_id,
                renewal: Some(renewal),
            },
        }
    }
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        match &mut self.inner {
            AdmissionGuardInner::Local(_permit) => {}
            AdmissionGuardInner::Durable {
                store,
                slot_id,
                renewal,
            } => {
                let _ = renewal.take();
                if let Err(e) = store.admission_slot_release(slot_id) {
                    tracing::warn!(error = %e, "durable global admission slot release failed");
                }
            }
        }
    }
}

fn overloaded_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "error": {
                "message": "gateway at capacity: admission timed out",
                "type": "overloaded"
            }
        })),
    )
        .into_response()
}

fn coordination_error_response(error: impl std::fmt::Display) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "error": {
                "message": format!("global admission store unavailable: {error}"),
                "type": "coordination_error"
            }
        })),
    )
        .into_response()
}
