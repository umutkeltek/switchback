use std::collections::BTreeSet;
use std::sync::Arc;

use super::Engine;

pub(crate) fn record_trace_to(
    traces: &Arc<sb_trace::TraceLog>,
    store: Option<&Arc<dyn sb_store::StateStore>>,
    record: sb_trace::TraceRecord,
) {
    if let Some(store) = store {
        if let Err(e) = persist_trace(store.as_ref(), &record) {
            tracing::warn!(
                error = %e,
                request_id = %record.request_id,
                "state store: trace metadata write failed"
            );
        }
    }
    traces.record(record);
}

impl Engine {
    pub(crate) fn record_trace(&self, record: sb_trace::TraceRecord) {
        record_trace_to(&self.traces, self.store.as_ref(), record);
    }
}

fn persist_trace(
    store: &dyn sb_store::StateStore,
    record: &sb_trace::TraceRecord,
) -> sb_store::Result<()> {
    let trace_json = serde_json::to_string(record)
        .map_err(|e| sb_store::StoreError(format!("serialize trace metadata: {e}")))?;
    let attempted_providers = record
        .attempts
        .iter()
        .map(|attempt| attempt.provider_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let event = sb_store::TraceEvent {
        request_id: record.request_id.clone(),
        revision: record.revision,
        tenant: record.tenant.clone(),
        project: record.project.clone(),
        session_id: record.session_id.clone(),
        inbound_model: record.inbound_model.clone(),
        route: record.route.clone(),
        selected_target: record
            .decision
            .selected
            .as_ref()
            .map(|target| target.target_id.clone()),
        final_status: record.final_status,
        total_latency_ms: record.total_latency_ms,
        streamed: record.streamed,
        cost_micros: record.cost_micros,
        attempted_providers,
        created_at_ms: (record.timestamp_unix as i64).saturating_mul(1000),
        trace_json,
    };
    store.record_trace(&event).map(|_| ())
}
