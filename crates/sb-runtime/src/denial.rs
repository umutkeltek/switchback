use std::time::Instant;

use sb_core::RouteDecision;

use super::Engine;

pub(crate) struct DenialTrace<'a> {
    pub(crate) request_id: &'a str,
    pub(crate) revision: u64,
    pub(crate) inbound_model: &'a str,
    pub(crate) status: u16,
    pub(crate) error_type: &'a str,
    pub(crate) message: &'a str,
    pub(crate) started: Instant,
    pub(crate) streamed: bool,
}

impl Engine {
    pub(crate) fn record_denial_trace(&self, denial: DenialTrace<'_>) {
        let mut decision = RouteDecision::new(denial.request_id, "denied");
        decision.add_reason(format!("{}: {}", denial.error_type, denial.message));
        decision.reject(denial.inbound_model, denial.error_type);
        let trace = sb_trace::RequestTrace::start(
            denial.request_id,
            denial.revision,
            denial.inbound_model,
            "denied",
            decision,
        )
        .finish(
            denial.status,
            denial.started.elapsed().as_millis() as u64,
            denial.streamed,
        );
        self.traces.record(trace);
    }
}
