//! Managed local lanes on the request path, for lanes that opted in with
//! `wake_on_request: true`: the busy guard held while a request is in flight
//! on the lane, and the bounded wake before the first byte.
//!
//! Both are decided from the request's pinned snapshot, like the slice of
//! routing that skips a lane known down (`profiles::known_down_lanes`): a lane
//! that did not opt in costs nothing here, with no call into the lane view.

use std::time::Duration;

use sb_core::{AiRequest, ClientProfileConfig, LaneOnFull, LocalExecutorConfig};

use super::profiles::{plan_candidates, plan_resolved_route, CandidateResolution};
use super::{Engine, ExecError, LaneAcquire, LaneBusy, LaneWake, Snapshot, LANE_QUEUE_RETRY_AFTER};

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

/// The provider's lane entry in the pinned config, if it is a managed lane.
fn lane_config<'a>(snap: &'a Snapshot, provider_id: &str) -> Option<&'a LocalExecutorConfig> {
    snap.config
        .local_executors
        .iter()
        .find(|lane| lane.name == provider_id)
}

/// The provider's lane is listed in the pinned config with a `max_in_flight`:
/// every request to it takes a place under the limit.
pub(crate) fn has_slot_limit(snap: &Snapshot, provider_id: &str) -> bool {
    lane_config(snap, provider_id).is_some_and(|lane| lane.max_in_flight.is_some())
}

/// The targets of `provider_id` in `resolved`, skipped with `reason`.
fn skip_lane(resolved: &mut CandidateResolution, provider_id: &str, reason: &str) {
    for candidate in &resolved.candidates {
        if candidate.provider_id == provider_id
            && !resolved.lane_down.iter().any(|(id, _)| *id == candidate.id)
        {
            resolved
                .lane_down
                .push((candidate.id.clone(), reason.to_string()));
        }
    }
}

/// What taking the first choice's lane did (a place under `max_in_flight`,
/// the GPU group, or a spill to another target), plus the busy guard that
/// holds the place and the group from here until the dispatch loop reaches
/// that lane.
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

    /// The held guard, for the first dispatch when it goes to the lane it
    /// was taken for. When the plan dispatches another target first (say,
    /// other requests moved a round-robin cursor during the wait), the
    /// guard is let go here, so the lane's GPU group is not held for the
    /// length of an unrelated request; the lane, reached later as a
    /// fallback, is then used only while it still holds its group.
    pub(crate) fn take_busy(&mut self, provider_id: &str) -> Option<LaneBusy> {
        let busy = self.busy.take();
        if self.note.provider_id == provider_id {
            busy
        } else {
            None
        }
    }

    /// Lend the held guard to the hedge race, which runs before the
    /// dispatch loop: the lane's own racer uses the place taken for it
    /// instead of asking for a second one (with `max_in_flight: 1` that
    /// second ask always fails, and with more it costs two places).
    pub(crate) fn lend_to_hedge(&mut self) -> HeldPlace {
        HeldPlace {
            provider_id: self.note.provider_id.clone(),
            busy: std::sync::Mutex::new(self.busy.take()),
        }
    }

    /// Take back a lent guard the race did not use, for the dispatch loop.
    pub(crate) fn give_back(&mut self, held: HeldPlace) {
        if let Some(busy) = held.into_unused() {
            self.busy = Some(busy);
        }
    }
}

/// The first choice's held place, lent to the hedge race (see
/// [`LaneGpuNote::lend_to_hedge`]). The first racer on that lane takes it;
/// any other racer asks the lane for its own place as usual.
pub(crate) struct HeldPlace {
    provider_id: String,
    busy: std::sync::Mutex<Option<LaneBusy>>,
}

impl HeldPlace {
    /// The lent guard, once, for a racer on the lane it was taken for.
    pub(crate) fn take_for(&self, provider_id: &str) -> Option<LaneBusy> {
        if self.provider_id != provider_id {
            return None;
        }
        self.busy
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    fn into_unused(self) -> Option<LaneBusy> {
        self.busy
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// What a request-triggered wake did, kept until the plan is known.
pub(crate) struct LaneWakeNote {
    provider_id: String,
    wake: LaneWake,
    /// The typed error of the 503 when the lane did not get ready and the
    /// plan still selected it.
    code: &'static str,
    /// The lane was full and the request went to another target at once.
    spilled: bool,
    prefer_awake: Option<String>,
}

impl LaneWakeNote {
    /// Say on the decision what the wake did. When the lane did not come up
    /// and the plan still selected it (no other target can serve), answer a
    /// retryable 503 instead of dispatching into a lane known down: the
    /// evidence is a fresh probe, not a stale one, so fail-open would only
    /// turn into a slow failure.
    pub(crate) fn apply(&self, plan: &mut sb_router::RoutePlan) -> Result<(), ExecError> {
        if let Some(effect) = &self.prefer_awake {
            plan.decision.set_prefer_awake(effect.clone());
        }
        match &self.wake {
            LaneWake::Ready { detail } => {
                // A place taken at once on a lane with room says nothing.
                if !detail.is_empty() {
                    plan.decision
                        .add_reason(format!("local lane `{}` {detail}", self.provider_id));
                }
                Ok(())
            }
            LaneWake::NotReady {
                detail,
                retry_after,
            } => {
                plan.decision.add_reason(if self.spilled {
                    format!(
                        "local lane `{}` {detail}, spilled to the route's next target",
                        self.provider_id
                    )
                } else {
                    format!("local lane `{}` {detail}, fell through", self.provider_id)
                });
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
                    self.code,
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
            && !has_slot_limit(snap, provider_id)
        {
            return Ok(None);
        }
        lanes.try_begin_request(provider_id)
    }

    /// Before planning, after any wake: when the target this route would
    /// choose first sits on a lane with `max_in_flight` or in an exclusive
    /// GPU group, take the lane before the first byte.
    ///
    /// A full lane with `on_full: spill` (the default) sends the request to
    /// the route's next usable target at once (the lane's targets are
    /// skipped with the reason, not counted as failures); when no other
    /// target can serve, the request waits like `on_full: queue`. Waiting
    /// means the lane's FIFO queue (bounded by `queue_timeout_secs`), then
    /// the GPU group (bounded by `group_wait_secs`). On success the guard
    /// holds the place and the group until the dispatch uses them; on
    /// expiry or a failed release the lane's targets are skipped with the
    /// reason, and a 503 answers when nothing else can serve. At most one
    /// lane wait per request.
    pub(crate) async fn take_lane_before_dispatch(
        &self,
        snap: &Snapshot,
        req: &AiRequest,
        client_profile: Option<&ClientProfileConfig>,
        resolved: &mut CandidateResolution,
    ) -> Option<LaneGpuNote> {
        let lanes = self.lanes.as_deref()?;
        // Lanes without a limit or a GPU group (the default) cost nothing.
        if !resolved.candidates.iter().any(|candidate| {
            in_gpu_group(snap, &candidate.provider_id)
                || has_slot_limit(snap, &candidate.provider_id)
        }) {
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
        let grouped = in_gpu_group(snap, &provider_id);
        let limited = has_slot_limit(snap, &provider_id);
        if !grouped && !limited {
            return None;
        }
        let spill =
            lane_config(snap, &provider_id).is_some_and(|lane| lane.on_full == LaneOnFull::Spill);
        if limited && spill {
            if let Some(full) = lanes.lane_full(&provider_id) {
                let reason = format!(
                    "local lane `{provider_id}` {full}; spilled before dispatch, not counted as a failure"
                );
                let mut spilled = resolved.clone();
                skip_lane(&mut spilled, &provider_id, &reason);
                let elsewhere = plan_resolved_route(
                    &self.combo_rr,
                    snap,
                    req,
                    client_profile,
                    spilled.clone(),
                    false,
                )
                .ok()
                .and_then(|(_, plan)| plan.decision.selected)
                .is_some_and(|selected| {
                    spilled.candidates.iter().any(|candidate| {
                        candidate.id == selected.target_id && candidate.provider_id != provider_id
                    })
                });
                if elsewhere {
                    *resolved = spilled;
                    return Some(LaneGpuNote {
                        note: LaneWakeNote {
                            wake: LaneWake::NotReady {
                                detail: full,
                                retry_after: LANE_QUEUE_RETRY_AFTER,
                            },
                            provider_id,
                            // Never answered: a spill happens only when
                            // another target serves the request.
                            code: "local_lane_unavailable",
                            spilled: true,
                            prefer_awake: None,
                        },
                        busy: None,
                    });
                }
                // Nothing else can serve: wait in the lane's queue instead.
            }
        }
        let (detail, retry_after, code) = match lanes.acquire_for_request(&provider_id).await {
            LaneAcquire::Ready { busy, detail } => {
                let detail = match detail {
                    Some(detail) => detail,
                    None if grouped => "holds its gpu group".to_string(),
                    None => String::new(),
                };
                return Some(LaneGpuNote {
                    note: LaneWakeNote {
                        wake: LaneWake::Ready { detail },
                        provider_id,
                        code: "local_lane_unavailable",
                        spilled: false,
                        prefer_awake: None,
                    },
                    busy,
                });
            }
            LaneAcquire::NotReady {
                detail,
                retry_after,
            } => (detail, retry_after, "local_lane_unavailable"),
            LaneAcquire::QueueTimeout {
                detail,
                retry_after,
            } => (detail, retry_after, "local_lane_queue_timeout"),
        };
        skip_lane(
            resolved,
            &provider_id,
            &format!(
                "local lane `{provider_id}` {detail}; skipped before dispatch, not counted as a failure"
            ),
        );
        Some(LaneGpuNote {
            note: LaneWakeNote {
                wake: LaneWake::NotReady {
                    detail,
                    retry_after,
                },
                provider_id,
                code,
                spilled: false,
                prefer_awake: None,
            },
            busy: None,
        })
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
        if resolved.prefer_awake.is_some() {
            return self
                .wake_first_choice_if_asleep(snap, req, client_profile, resolved, lanes)
                .await;
        }
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
        Some(LaneWakeNote {
            provider_id,
            wake,
            code: "local_lane_unavailable",
            spilled: false,
            prefer_awake: None,
        })
    }

    /// `prefer_awake` route: when the target the plan would dispatch first
    /// still needs a wake (nothing that is up can serve, or the request is
    /// big, or the client said `always`), wake its lane now, bounded, before
    /// the first byte, like [`Self::wake_lane_before_dispatch`] does for a
    /// lane known down. A small request whose first choice is up wakes
    /// nothing here: a target that needs a wake is woken only if the request
    /// falls back to it ([`Self::wake_for_fallback`]).
    async fn wake_first_choice_if_asleep(
        &self,
        snap: &Snapshot,
        req: &AiRequest,
        client_profile: Option<&ClientProfileConfig>,
        resolved: &mut CandidateResolution,
        lanes: &dyn crate::LaneStatus,
    ) -> Option<LaneWakeNote> {
        if !resolved.candidates.iter().any(|c| c.wake.is_some()) {
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
        let provider_id = resolved
            .candidates
            .iter()
            .find(|candidate| candidate.id == first && candidate.wake.is_some())?
            .provider_id
            .clone();
        let prefer_awake = would_be.decision.prefer_awake.clone();
        let wake = lanes.wake_for_request(&provider_id).await;
        settle_wake(resolved, &provider_id, &wake);
        Some(LaneWakeNote {
            provider_id,
            wake,
            code: "local_lane_unavailable",
            spilled: false,
            prefer_awake,
        })
    }

    /// `prefer_awake` fallback: the request reached a target whose machine
    /// still needs a wake (every target before it was skipped or failed).
    /// Wake its lane, bounded by the lane's `wake_wait_secs`.
    pub(crate) async fn wake_for_fallback(&self, provider_id: &str) -> LaneWake {
        match self.lanes.as_deref() {
            Some(lanes) => lanes.wake_for_request(provider_id).await,
            None => LaneWake::NotReady {
                detail: "no managed lanes".to_string(),
                retry_after: Duration::from_secs(1),
            },
        }
    }
}

/// After a `prefer_awake` wake of `provider_id`'s lane: its targets no
/// longer need one (woken), or are skipped like a lane known down (the wake
/// did not bring it up).
fn settle_wake(resolved: &mut CandidateResolution, provider_id: &str, wake: &LaneWake) {
    let mut on_lane = Vec::new();
    for candidate in resolved.candidates.iter_mut() {
        if candidate.provider_id == provider_id {
            candidate.wake = None;
            on_lane.push(candidate.id.clone());
        }
    }
    match wake {
        LaneWake::Ready { .. } => resolved
            .lane_down
            .retain(|(target_id, _)| !on_lane.contains(target_id)),
        LaneWake::NotReady { detail, .. } => {
            let reason = format!(
                "local lane `{provider_id}` down ({detail}); skipped before dispatch, not counted as a failure"
            );
            for target_id in on_lane {
                match resolved
                    .lane_down
                    .iter_mut()
                    .find(|(id, _)| *id == target_id)
                {
                    Some((_, existing)) => existing.clone_from(&reason),
                    None => resolved.lane_down.push((target_id, reason.clone())),
                }
            }
        }
    }
}
