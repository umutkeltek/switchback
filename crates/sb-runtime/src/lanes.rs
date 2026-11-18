//! Managed local lanes on the request path, for lanes that opted in with
//! `wake_on_request: true`: the busy guard held while a request is in flight
//! on the lane, and the bounded wake before the first byte.
//!
//! Both are decided from the request's pinned snapshot, like the slice of
//! routing that skips a lane known down (`profiles::known_down_lanes`): a lane
//! that did not opt in costs nothing here, with no call into the lane view.

use std::time::Duration;

use sb_core::{AiRequest, ClientProfileConfig};

use super::profiles::{plan_candidates, plan_resolved_route, CandidateResolution};
use super::{Engine, ExecError, LaneAcquire, LaneBusy, LaneWake, Snapshot};

/// The provider's lane is listed in the pinned config with `wake_on_request`
/// (which config validation only accepts together with `route_health`).
pub(crate) fn wakes_on_request(snap: &Snapshot, provider_id: &str) -> bool {
    snap.config
        .local_executors
        .iter()
        .any(|lane| lane.name == provider_id && lane.route_health && lane.wake_on_request)
}

/// The provider's lane is listed in the pinned config with an
/// `exclusive_group` (a GPU it shares with other lanes of its host).
pub(crate) fn in_gpu_group(snap: &Snapshot, provider_id: &str) -> bool {
    snap.config
        .local_executors
        .iter()
        .any(|lane| lane.name == provider_id && lane.exclusive_group.is_some())
}

/// The provider's lane is listed in the pinned config with a `host` (a
/// machine it shares with other lanes): its requests count as work on the
/// machine, so a power-off another lane's idle timer asks for waits for them.
pub(crate) fn on_shared_host(snap: &Snapshot, provider_id: &str) -> bool {
    snap.config
        .local_executors
        .iter()
        .any(|lane| lane.name == provider_id && lane.host.is_some())
}

/// What taking the first choice's GPU group did, plus the busy guard that
/// holds the group from here until the dispatch loop reaches that lane.
pub(crate) struct LaneGpuNote {
    note: LaneWakeNote,
    busy: Option<LaneBusy>,
}

impl LaneGpuNote {
    /// Say on the decision what happened; a 503 when the lane did not get
    /// its group and the plan still selected it (see [`LaneWakeNote::apply`]).
    pub(crate) fn apply(&self, plan: &mut sb_router::RoutePlan) -> Result<(), ExecError> {
        self.note.apply(plan)
    }

    /// The held guard, for the first dispatch to the lane it was taken for.
    pub(crate) fn take_busy(&mut self, provider_id: &str) -> Option<LaneBusy> {
        if self.note.provider_id == provider_id {
            self.busy.take()
        } else {
            None
        }
    }
}

/// What a request-triggered wake did, kept until the plan is known.
pub(crate) struct LaneWakeNote {
    provider_id: String,
    wake: LaneWake,
}

impl LaneWakeNote {
    /// Say on the decision what the wake did. When the lane did not come up
    /// and the plan still selected it (no other target can serve), answer a
    /// retryable 503 instead of dispatching into a lane known down: the
    /// evidence is a fresh probe, not a stale one, so fail-open would only
    /// turn into a slow failure.
    pub(crate) fn apply(&self, plan: &mut sb_router::RoutePlan) -> Result<(), ExecError> {
        match &self.wake {
            LaneWake::Ready { detail } => {
                plan.decision
                    .add_reason(format!("local lane `{}` {detail}", self.provider_id));
                Ok(())
            }
            LaneWake::NotReady {
                detail,
                retry_after,
            } => {
                plan.decision.add_reason(format!(
                    "local lane `{}` {detail}, fell through",
                    self.provider_id
                ));
                let selected_on_lane = plan.decision.selected.as_ref().is_some_and(|selected| {
                    plan.candidates.iter().any(|candidate| {
                        candidate.id == selected.target_id
                            && candidate.provider_id == self.provider_id
                    })
                });
                if !selected_on_lane {
                    return Ok(());
                }
                Err(ExecError::new(
                    503,
                    "local_lane_unavailable",
                    format!(
                        "local lane `{}` is not ready ({detail}) and no other target can serve; retry later",
                        self.provider_id
                    ),
                    Some(plan.decision.summary()),
                )
                .with_retry_after((*retry_after).max(Duration::from_secs(1))))
            }
        }
    }
}

impl Engine {
    /// Count one dispatch as in-flight work on the target's lane. `Ok(None)`
    /// for a lane that did not opt in (the default), so nothing is held.
    /// `Err(reason)` when the lane is in an exclusive GPU group it does not
    /// hold right now: the target is skipped (never waits here; only the
    /// route's first choice waits, before planning).
    pub(crate) fn lane_busy(
        &self,
        snap: &Snapshot,
        provider_id: &str,
    ) -> Result<Option<LaneBusy>, String> {
        let Some(lanes) = self.lanes.as_deref() else {
            return Ok(None);
        };
        if !wakes_on_request(snap, provider_id)
            && !in_gpu_group(snap, provider_id)
            && !on_shared_host(snap, provider_id)
        {
            return Ok(None);
        }
        lanes.try_begin_request(provider_id)
    }

    /// Before planning, after any wake: when the target this route would
    /// choose first sits on a lane in an exclusive GPU group, wait (bounded
    /// by the lane's `group_wait_secs`) until the lane holds the group. On
    /// success the guard holds the group until the dispatch uses it; on
    /// expiry or a failed release the lane's targets are skipped with the
    /// reason. At most one group wait per request.
    pub(crate) async fn take_gpu_before_dispatch(
        &self,
        snap: &Snapshot,
        req: &AiRequest,
        client_profile: Option<&ClientProfileConfig>,
        resolved: &mut CandidateResolution,
    ) -> Option<LaneGpuNote> {
        let lanes = self.lanes.as_deref()?;
        // Lanes outside a GPU group (the default) cost nothing here.
        if !resolved
            .candidates
            .iter()
            .any(|candidate| in_gpu_group(snap, &candidate.provider_id))
        {
            return None;
        }
        let (_, would_be) = plan_resolved_route(
            &self.combo_rr,
            snap,
            req,
            client_profile,
            resolved.clone(),
            false,
        )
        .ok()?;
        let first = would_be.decision.selected.as_ref()?.target_id.clone();
        // A first choice still known down is the wake note's answer.
        if resolved
            .lane_down
            .iter()
            .any(|(target_id, _)| *target_id == first)
        {
            return None;
        }
        let provider_id = resolved
            .candidates
            .iter()
            .find(|candidate| candidate.id == first)?
            .provider_id
            .clone();
        if !in_gpu_group(snap, &provider_id) {
            return None;
        }
        match lanes.acquire_for_request(&provider_id).await {
            LaneAcquire::Ready { busy, detail } => Some(LaneGpuNote {
                note: LaneWakeNote {
                    wake: LaneWake::Ready {
                        detail: detail.unwrap_or_else(|| "holds its gpu group".to_string()),
                    },
                    provider_id,
                },
                busy,
            }),
            LaneAcquire::NotReady {
                detail,
                retry_after,
            } => {
                for candidate in &resolved.candidates {
                    if candidate.provider_id == provider_id {
                        resolved.lane_down.push((
                            candidate.id.clone(),
                            format!(
                                "local lane `{provider_id}` {detail}; skipped before dispatch, not counted as a failure"
                            ),
                        ));
                    }
                }
                Some(LaneGpuNote {
                    note: LaneWakeNote {
                        wake: LaneWake::NotReady {
                            detail,
                            retry_after,
                        },
                        provider_id,
                    },
                    busy: None,
                })
            }
        }
    }

    /// Before planning: when the target this route would choose first sits
    /// on an opted-in lane that routing is about to skip as down, start the
    /// lane's wake and wait for it (bounded by the lane's `wake_wait_secs`).
    /// On success the lane's targets are no longer skipped; otherwise their
    /// rejection says what happened. At most one lane is woken per request,
    /// so the added wait is never more than one lane's bound.
    pub(crate) async fn wake_lane_before_dispatch(
        &self,
        snap: &Snapshot,
        req: &AiRequest,
        client_profile: Option<&ClientProfileConfig>,
        resolved: &mut CandidateResolution,
    ) -> Option<LaneWakeNote> {
        let lanes = self.lanes.as_deref()?;
        // Opt-out lanes (the default) cost nothing here: no extra planning.
        let any_opted_in = resolved.lane_down.iter().any(|(target_id, _)| {
            resolved
                .candidates
                .iter()
                .any(|c| c.id == *target_id && wakes_on_request(snap, &c.provider_id))
        });
        if !any_opted_in {
            return None;
        }
        // The target the plan would select if every lane were up. Never
        // advances a round-robin cursor: the real plan below does that.
        let (_, would_be) = plan_candidates(
            &self.combo_rr,
            snap,
            req,
            client_profile,
            resolved.clone(),
            false,
        )
        .ok()?;
        let first = would_be.decision.selected.as_ref()?.target_id.clone();
        if !resolved
            .lane_down
            .iter()
            .any(|(target_id, _)| *target_id == first)
        {
            return None;
        }
        let provider_id = resolved
            .candidates
            .iter()
            .find(|candidate| candidate.id == first)?
            .provider_id
            .clone();
        if !wakes_on_request(snap, &provider_id) {
            return None;
        }
        let wake = lanes.wake_for_request(&provider_id).await;
        let on_lane: Vec<String> = resolved
            .candidates
            .iter()
            .filter(|candidate| candidate.provider_id == provider_id)
            .map(|candidate| candidate.id.clone())
            .collect();
        match &wake {
            LaneWake::Ready { .. } => {
                resolved
                    .lane_down
                    .retain(|(target_id, _)| !on_lane.contains(target_id));
            }
            LaneWake::NotReady { detail, .. } => {
                for (target_id, reason) in resolved.lane_down.iter_mut() {
                    if on_lane.contains(target_id) {
                        *reason = format!(
                            "local lane `{provider_id}` is down ({detail}); skipped before dispatch, not counted as a failure"
                        );
                    }
                }
            }
        }
        Some(LaneWakeNote { provider_id, wake })
    }
}
