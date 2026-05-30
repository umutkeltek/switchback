//! Global admission control + bounded backpressure (Oracle #8).
//!
//! A single [`Semaphore`] caps total in-flight requests. A request waits up to
//! the admission timeout for a permit (bounded backpressure — bursts queue
//! instead of overwhelming upstreams); if the wait is exceeded the request is
//! SHED with 503 rather than queuing unboundedly. The permit is held for the
//! request's whole life (moved into the SSE encoder closure for streams), so it
//! is released only when the response is fully delivered or the client hangs up.
//!
//! This is the global counterpart to the per-tenant concurrency limit in
//! [`crate::tenancy`]: global protects the gateway, per-tenant protects fairness.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

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

    /// Acquire a global in-flight permit. Returns the held permit (None when
    /// unlimited) + the queue-wait in ms, or `Err(503)` if the wait exceeds the
    /// admission timeout (load shed).
    pub async fn acquire(&self) -> Result<(Option<OwnedSemaphorePermit>, u64), Response> {
        let Some(sem) = &self.sem else {
            return Ok((None, 0));
        };
        let start = Instant::now();
        match tokio::time::timeout(self.timeout, sem.clone().acquire_owned()).await {
            Ok(Ok(permit)) => Ok((Some(permit), start.elapsed().as_millis() as u64)),
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
