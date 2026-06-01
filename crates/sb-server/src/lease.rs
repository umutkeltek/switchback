use std::sync::Arc;
use std::time::Duration;

use sb_store::StateStore;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

pub(crate) struct RenewalGuard {
    stop: Option<oneshot::Sender<()>>,
    handle: JoinHandle<()>,
}

enum RenewalKind {
    AdmissionSlot { slot_id: String },
    TenantSlot { slot_id: String },
    IdempotencyClaim { key: String },
}

impl RenewalGuard {
    pub(crate) fn admission_slot(store: Arc<dyn StateStore>, slot_id: String, ttl_ms: u64) -> Self {
        Self::spawn(
            store,
            RenewalKind::AdmissionSlot { slot_id },
            ttl_ms,
            "global admission slot",
        )
    }

    pub(crate) fn tenant_slot(store: Arc<dyn StateStore>, slot_id: String, ttl_ms: u64) -> Self {
        Self::spawn(
            store,
            RenewalKind::TenantSlot { slot_id },
            ttl_ms,
            "tenant concurrency slot",
        )
    }

    pub(crate) fn idempotency_claim(store: Arc<dyn StateStore>, key: String, ttl_ms: u64) -> Self {
        Self::spawn(
            store,
            RenewalKind::IdempotencyClaim { key },
            ttl_ms,
            "idempotency claim",
        )
    }

    fn spawn(
        store: Arc<dyn StateStore>,
        kind: RenewalKind,
        ttl_ms: u64,
        label: &'static str,
    ) -> Self {
        let interval = renewal_interval(ttl_ms);
        let (stop_tx, mut stop_rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {
                        match renew(&*store, &kind, ttl_ms) {
                            Ok(true) => {}
                            Ok(false) => {
                                tracing::warn!(lease = label, "durable lease renewal stopped because the lease is missing or expired");
                                break;
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, lease = label, "durable lease renewal failed");
                            }
                        }
                    }
                    _ = &mut stop_rx => break,
                }
            }
        });
        Self {
            stop: Some(stop_tx),
            handle,
        }
    }
}

impl Drop for RenewalGuard {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.handle.abort();
    }
}

fn renewal_interval(ttl_ms: u64) -> Duration {
    let millis = if ttl_ms <= 2 {
        1
    } else {
        (ttl_ms / 2).clamp(1, 60_000)
    };
    Duration::from_millis(millis)
}

fn renew(store: &dyn StateStore, kind: &RenewalKind, ttl_ms: u64) -> sb_store::Result<bool> {
    match kind {
        RenewalKind::AdmissionSlot { slot_id } => store.admission_slot_renew(slot_id, ttl_ms),
        RenewalKind::TenantSlot { slot_id } => store.tenant_slot_renew(slot_id, ttl_ms),
        RenewalKind::IdempotencyClaim { key } => store.idempotency_renew(key, ttl_ms),
    }
}
