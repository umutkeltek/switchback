//! Speed tricks at run time ("optimize"), layer A and the evidence for
//! layer B:
//!
//! - **stickiness**: a conversation stays on the target (machine) that
//!   served it last, so the engine reuses its prefix cache, while that
//!   target is up, healthy and has room (a preference, never a reason to
//!   wake or to queue); its engine slot rides along as `id_slot` when known;
//! - **loaded-model preference**: among targets on one awake machine, the
//!   one whose model is loaded goes first (a swap costs seconds);
//! - **big-prompt placement**: a big prompt goes to the awake target with
//!   the best measured prompt speed;
//! - **observation**: per target, the prefix-cache hits, speculative
//!   acceptance, model swaps, empty answers after thinking, reasoning
//!   guard uses and queue waits, for the speed report.
//!
//! State, not config: it lives on the [`crate::Engine`] and survives a
//! config publish. Nothing here touches a machine: the loaded-model view
//! comes from the engine control's last listing ([`EngineView`]) and from
//! what the gateway itself served.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use futures::StreamExt;
use serde::Serialize;

use sb_core::{AiResponse, AiStreamEvent, ContentPart, FinishReason, RouteOptimize, Usage};

/// What the engine control last saw loaded on a lane (`sb-server`). Never
/// calls a machine.
pub trait EngineView: Send + Sync {
    /// The models the engine of `provider_id`'s lane had loaded at its last
    /// listing, and when that listing was taken (ms). `None` = no listing.
    fn loaded_models(&self, provider_id: &str) -> Option<(i64, Vec<String>)>;
}

/// Swaps older than this leave the per-target record.
const SWAP_WINDOW_MS: i64 = 24 * 3_600_000;
/// Prompt keys remembered per target (to tell a repeated system prompt).
const PROMPT_KEYS_PER_TARGET: usize = 512;
const PROMPT_TPS_ALPHA: f64 = 0.3;

/// Where a conversation was served last.
#[derive(Debug, Clone, PartialEq)]
pub struct Sticky {
    pub target_id: String,
    pub provider_id: String,
    /// The engine model it ran on (a variant's own name included).
    pub engine_model: String,
    /// The engine slot that served it, when the engine said.
    pub slot: Option<u32>,
    /// The target's settings revision when it was served: a settings
    /// write (a reload with other slots, perhaps) drops the slot.
    pub settings_revision: u64,
    pub at_ms: i64,
}

#[derive(Debug, Default)]
struct Loaded {
    /// Engine models believed loaded, with when each was last seen so.
    models: Vec<(String, i64)>,
    listing_ms: i64,
    /// Whether the set is evidence (a listing or a served request), so a
    /// load can be told apart from the first request after start.
    known: bool,
}

#[derive(Debug, Default, Clone)]
struct Counters {
    requests: u64,
    prompt_tokens: u64,
    cached_tokens: u64,
    cache_hit_requests: u64,
    repeat_prompt_requests: u64,
    repeat_prompt_hits: u64,
    prompt_keys: VecDeque<String>,
    draft_tokens: u64,
    draft_accepted: u64,
    spec_requests: u64,
    empty_after_thinking: u64,
    last_empty_after_thinking_ms: Option<i64>,
    guard_applied: u64,
    sticky_kept: u64,
    swaps: VecDeque<(i64, String)>,
    swaps_total: u64,
    prompt_tps_ewma: Option<f64>,
    last_used_ms: i64,
}

#[derive(Default)]
struct Inner {
    conversations: HashMap<String, Sticky>,
    /// Insertion order of `conversations`, oldest first (for the bound).
    conversation_order: VecDeque<String>,
    loaded: HashMap<String, Loaded>,
    targets: HashMap<String, Counters>,
    queue_waits: HashMap<String, u64>,
}

/// The per-target evidence the speed report reads (metadata only).
#[derive(Debug, Clone, Default, Serialize)]
pub struct TargetObservations {
    pub requests: u64,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    /// Requests that reused some prefix-cache tokens.
    pub cache_hit_requests: u64,
    /// Requests whose system prompt (and first message) this target had
    /// seen before, and how many of them hit the cache.
    pub repeat_prompt_requests: u64,
    pub repeat_prompt_hits: u64,
    pub draft_tokens: u64,
    pub draft_accepted: u64,
    /// Requests that reported speculative drafts.
    pub spec_requests: u64,
    pub empty_after_thinking: u64,
    pub last_empty_after_thinking_ms: Option<i64>,
    pub reasoning_guard_applied: u64,
    pub sticky_kept: u64,
    pub swaps_last_hour: u64,
    pub swaps_last_day: u64,
    pub swaps_total: u64,
    /// Models a swap to this target replaced in the last hour, with counts.
    pub swapped_out_last_hour: BTreeMap<String, u64>,
    pub prompt_tps_ewma: Option<f64>,
    pub last_used_ms: Option<i64>,
}

/// One request a target served, as the observer sees it.
pub(crate) struct Served<'a> {
    pub target_id: &'a str,
    pub provider_id: &'a str,
    pub engine_model: &'a str,
    pub conversation: Option<&'a str>,
    pub prompt_key: Option<&'a str>,
    pub usage: &'a Usage,
    pub prompt_tps: Option<f64>,
    pub empty_after_thinking: bool,
    pub guard_applied: bool,
    /// `Some(models_max)` when the provider's lane has an engine control
    /// (so a load is a swap), with the router's model limit when known.
    pub engine_lane: Option<Option<u32>>,
    pub sticky_ttl_ms: i64,
    pub sticky_max: usize,
    /// The target's settings revision when the attempt was planned.
    pub settings_revision: u64,
}

/// A tuning run's exclusive hold on a lane (`sb ai tune`): while it lasts,
/// only requests that carry its id (`x-switchback-tune-lock`) use the
/// lane's targets; every other request goes elsewhere.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TuneLock {
    pub id: String,
    pub target: String,
    pub lane: String,
    pub host: String,
    pub reason: String,
    pub created_ms: i64,
    pub expires_at_ms: i64,
}

/// The speed-trick state of one gateway.
#[derive(Default)]
pub struct OptimizeState {
    inner: Mutex<Inner>,
    view: OnceLock<Arc<dyn EngineView>>,
    /// Tuning locks by lane.
    locks: Mutex<BTreeMap<String, TuneLock>>,
    /// The tuning ledger when there is no state store (process lifetime).
    experiments: Mutex<Vec<sb_store::ExperimentRecord>>,
}

fn now_ms() -> i64 {
    sb_store::now_millis()
}

impl OptimizeState {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Attach the engine control's view (once).
    pub fn set_engine_view(&self, view: Arc<dyn EngineView>) {
        let _ = self.view.set(view);
    }

    /// Where a conversation was served last, if within `ttl_ms`.
    pub fn sticky(&self, conversation: &str, ttl_ms: i64) -> Option<Sticky> {
        let inner = self.lock();
        inner
            .conversations
            .get(conversation)
            .filter(|sticky| now_ms() - sticky.at_ms <= ttl_ms)
            .cloned()
    }

    /// Whether the engine of `provider_id` has `engine_model` loaded, as far
    /// as the gateway knows (`None` = no evidence).
    pub fn is_loaded(&self, provider_id: &str, engine_model: &str) -> Option<bool> {
        let mut inner = self.lock();
        let lane = inner.loaded.entry(provider_id.to_string()).or_default();
        self.merge_listing(provider_id, lane);
        lane.known
            .then(|| lane.models.iter().any(|(model, _)| model == engine_model))
    }

    fn merge_listing(&self, provider_id: &str, lane: &mut Loaded) {
        let Some((seen_ms, models)) = self
            .view
            .get()
            .and_then(|view| view.loaded_models(provider_id))
        else {
            return;
        };
        let newest_served = lane.models.iter().map(|(_, at)| *at).max().unwrap_or(0);
        if seen_ms > lane.listing_ms && seen_ms >= newest_served {
            lane.models = models.into_iter().map(|model| (model, seen_ms)).collect();
            lane.listing_ms = seen_ms;
            lane.known = true;
        }
    }

    /// Count a request that waited in a lane's queue.
    pub(crate) fn count_queue_wait(&self, provider_id: &str) {
        *self
            .lock()
            .queue_waits
            .entry(provider_id.to_string())
            .or_default() += 1;
    }

    /// Requests that waited in each lane's queue.
    pub fn queue_waits(&self) -> HashMap<String, u64> {
        self.lock().queue_waits.clone()
    }

    /// Count a conversation kept on its machine.
    pub(crate) fn count_sticky_kept(&self, target_id: &str) {
        self.lock()
            .targets
            .entry(target_id.to_string())
            .or_default()
            .sticky_kept += 1;
    }

    /// Record one served request.
    pub(crate) fn observe(&self, served: Served<'_>) {
        let now = now_ms();
        let mut inner = self.lock();
        let timings = served.usage.engine_timings.as_ref();
        let cached = served
            .usage
            .cached_input_tokens
            .max(timings.and_then(|t| t.cache_tokens).unwrap_or(0));
        // Model swaps: a request served on a model the engine did not have
        // loaded made it load (and, at the router's limit, unload another).
        let mut swap: Option<String> = None;
        if let Some(models_max) = served.engine_lane {
            let mut lane = inner.loaded.remove(served.provider_id).unwrap_or_default();
            self.merge_listing(served.provider_id, &mut lane);
            let present = lane
                .models
                .iter()
                .any(|(model, _)| model == served.engine_model);
            if lane.known && !present {
                let replaced = lane
                    .models
                    .iter()
                    .max_by_key(|(_, at)| *at)
                    .map(|(model, _)| model.clone())
                    .unwrap_or_default();
                swap = Some(replaced);
            }
            lane.models
                .retain(|(model, _)| model != served.engine_model);
            lane.models.push((served.engine_model.to_string(), now));
            lane.known = true;
            if let Some(max) = models_max.filter(|max| *max > 0) {
                while lane.models.len() > max as usize {
                    let oldest = lane
                        .models
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, (_, at))| *at)
                        .map(|(index, _)| index)
                        .unwrap_or(0);
                    lane.models.remove(oldest);
                }
            }
            inner.loaded.insert(served.provider_id.to_string(), lane);
        }
        let counters = inner
            .targets
            .entry(served.target_id.to_string())
            .or_default();
        counters.requests += 1;
        counters.last_used_ms = now;
        counters.prompt_tokens += served.usage.input_tokens.saturating_add(cached);
        counters.cached_tokens += cached;
        if cached > 0 {
            counters.cache_hit_requests += 1;
        }
        if let Some(key) = served.prompt_key {
            if counters.prompt_keys.iter().any(|seen| seen == key) {
                counters.repeat_prompt_requests += 1;
                if cached > 0 {
                    counters.repeat_prompt_hits += 1;
                }
            } else {
                counters.prompt_keys.push_back(key.to_string());
                if counters.prompt_keys.len() > PROMPT_KEYS_PER_TARGET {
                    counters.prompt_keys.pop_front();
                }
            }
        }
        if let Some(t) = timings {
            if let (Some(drafted), Some(accepted)) = (t.draft_tokens, t.draft_accepted) {
                if drafted > 0 {
                    counters.draft_tokens += drafted;
                    counters.draft_accepted += accepted.min(drafted);
                    counters.spec_requests += 1;
                }
            }
        }
        if served.empty_after_thinking {
            counters.empty_after_thinking += 1;
            counters.last_empty_after_thinking_ms = Some(now);
        }
        if served.guard_applied {
            counters.guard_applied += 1;
        }
        if let Some(tps) = served
            .prompt_tps
            .filter(|tps| tps.is_finite() && *tps > 0.0)
        {
            counters.prompt_tps_ewma = Some(match counters.prompt_tps_ewma {
                Some(old) => old + PROMPT_TPS_ALPHA * (tps - old),
                None => tps,
            });
        }
        if let Some(replaced) = swap {
            counters.swaps.push_back((now, replaced));
            counters.swaps_total += 1;
        }
        while counters
            .swaps
            .front()
            .is_some_and(|(at, _)| now - at > SWAP_WINDOW_MS)
        {
            counters.swaps.pop_front();
        }
        // Stickiness: remember where the conversation went.
        if let Some(conversation) = served.conversation {
            let sticky = Sticky {
                target_id: served.target_id.to_string(),
                provider_id: served.provider_id.to_string(),
                engine_model: served.engine_model.to_string(),
                slot: timings.and_then(|t| t.slot),
                settings_revision: served.settings_revision,
                at_ms: now,
            };
            if inner
                .conversations
                .insert(conversation.to_string(), sticky)
                .is_none()
            {
                inner.conversation_order.push_back(conversation.to_string());
            }
            // Bound: forget conversations past the TTL at the front (in
            // insertion order), then the oldest beyond the limit.
            let Inner {
                conversations,
                conversation_order,
                ..
            } = &mut *inner;
            while let Some(key) = conversation_order.front() {
                let expired = conversations
                    .get(key)
                    .is_none_or(|s| now - s.at_ms > served.sticky_ttl_ms);
                if !expired && conversation_order.len() <= served.sticky_max.max(1) {
                    break;
                }
                if let Some(key) = conversation_order.pop_front() {
                    conversations.remove(&key);
                }
            }
        }
    }

    /// Every target's evidence (metadata only).
    pub fn observations(&self) -> BTreeMap<String, TargetObservations> {
        let now = now_ms();
        let inner = self.lock();
        inner
            .targets
            .iter()
            .map(|(target, c)| {
                let mut swapped_out = BTreeMap::new();
                let mut last_hour = 0;
                for (at, replaced) in &c.swaps {
                    if now - at <= 3_600_000 {
                        last_hour += 1;
                        if !replaced.is_empty() {
                            *swapped_out.entry(replaced.clone()).or_insert(0) += 1;
                        }
                    }
                }
                (
                    target.clone(),
                    TargetObservations {
                        requests: c.requests,
                        prompt_tokens: c.prompt_tokens,
                        cached_tokens: c.cached_tokens,
                        cache_hit_requests: c.cache_hit_requests,
                        repeat_prompt_requests: c.repeat_prompt_requests,
                        repeat_prompt_hits: c.repeat_prompt_hits,
                        draft_tokens: c.draft_tokens,
                        draft_accepted: c.draft_accepted,
                        spec_requests: c.spec_requests,
                        empty_after_thinking: c.empty_after_thinking,
                        last_empty_after_thinking_ms: c.last_empty_after_thinking_ms,
                        reasoning_guard_applied: c.guard_applied,
                        sticky_kept: c.sticky_kept,
                        swaps_last_hour: last_hour,
                        swaps_last_day: c.swaps.len() as u64,
                        swaps_total: c.swaps_total,
                        swapped_out_last_hour: swapped_out,
                        prompt_tps_ewma: c.prompt_tps_ewma,
                        last_used_ms: (c.last_used_ms > 0).then_some(c.last_used_ms),
                    },
                )
            })
            .collect()
    }

    /// The live tuning lock of a lane, if any (an expired one is dropped).
    pub fn lock_for_lane(&self, lane: &str) -> Option<TuneLock> {
        let now = now_ms();
        let mut locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        locks.retain(|_, lock| lock.expires_at_ms > now);
        locks.get(lane).cloned()
    }

    /// Every live tuning lock.
    pub fn locks(&self) -> Vec<TuneLock> {
        let now = now_ms();
        let mut locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        locks.retain(|_, lock| lock.expires_at_ms > now);
        locks.values().cloned().collect()
    }

    /// Take a lane's tuning lock; `Err` is the lock that already holds it.
    pub fn take_lock(&self, lock: TuneLock) -> Result<TuneLock, Box<TuneLock>> {
        let now = now_ms();
        let mut locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        locks.retain(|_, held| held.expires_at_ms > now);
        if let Some(held) = locks.get(&lock.lane) {
            return Err(Box::new(held.clone()));
        }
        locks.insert(lock.lane.clone(), lock.clone());
        Ok(lock)
    }

    /// Set a lock's end to `expires_at_ms`; `None` when no such lock.
    pub fn extend_lock(&self, id: &str, expires_at_ms: i64) -> Option<TuneLock> {
        let mut locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        let lock = locks.values_mut().find(|lock| lock.id == id)?;
        lock.expires_at_ms = expires_at_ms;
        Some(lock.clone())
    }

    /// End a lock; `None` when no such lock.
    pub fn release_lock(&self, id: &str) -> Option<TuneLock> {
        let mut locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        let lane = locks
            .iter()
            .find(|(_, lock)| lock.id == id)
            .map(|(lane, _)| lane.clone())?;
        locks.remove(&lane)
    }

    /// Keep an experiment row in memory (no state store); answers its id.
    pub fn remember_experiment(&self, mut record: sb_store::ExperimentRecord) -> u64 {
        let mut rows = self.experiments.lock().unwrap_or_else(|e| e.into_inner());
        record.id = rows.len() as u64 + 1;
        let id = record.id;
        rows.push(record);
        id
    }

    /// Remembered experiment rows, newest first.
    pub fn remembered_experiments(
        &self,
        target: Option<&str>,
        limit: usize,
    ) -> Vec<sb_store::ExperimentRecord> {
        let rows = self.experiments.lock().unwrap_or_else(|e| e.into_inner());
        rows.iter()
            .rev()
            .filter(|row| target.is_none_or(|target| row.target == target))
            .take(limit)
            .cloned()
            .collect()
    }

    /// The measured prompt speed of a target (EWMA), if any.
    pub(crate) fn prompt_tps(&self, target_id: &str) -> Option<f64> {
        self.lock()
            .targets
            .get(target_id)
            .and_then(|c| c.prompt_tps_ewma)
    }
}

// ---------------------------------------------------------------- ordering

/// What the ordering pass knows about one request's candidates, computed
/// once before planning (so every plan of the request agrees).
#[derive(Debug, Clone, Default)]
pub(crate) struct Hints {
    pub switches: RouteOptimize,
    /// The conversation's last target, when sticky and still known.
    pub sticky: Option<String>,
    /// Targets that are up, healthy and have room now.
    pub up: HashSet<String>,
    /// Why a target is not usable as a preference (asleep, full, ...).
    pub not_up: HashMap<String, String>,
    /// The machine of each target (lane `host`, lane name, or provider).
    pub host: HashMap<String, String>,
    /// Whether each target's model is loaded (only where known).
    pub loaded: HashMap<String, bool>,
    pub prompt_tps: HashMap<String, f64>,
    /// The request's estimated prompt tokens when at or over the
    /// big-prompt threshold on a route where placement may run.
    pub big_prompt: Option<u64>,
}

fn move_to(candidates: &mut Vec<sb_core::ExecutionTarget>, from: usize, to: usize) {
    if from == to || from >= candidates.len() {
        return;
    }
    let target = candidates.remove(from);
    candidates.insert(to.min(candidates.len()), target);
}

fn usable(target: &sb_core::ExecutionTarget) -> bool {
    target.healthy_accounts != Some(0)
        && target
            .outcome
            .is_none_or(|outcome| outcome.tier != sb_core::OutcomeTier::Demoted)
}

/// Reorder a plan's candidates by the hints: the sticky target first when
/// usable; else a big prompt to the best measured prompt speed; then, on
/// the first choice's machine, a target whose model is loaded ahead of one
/// whose model is not. Only targets that are up move forward, so nothing
/// that needs a wake ever overtakes an awake target. A pinned order is
/// kept. Returns whether the order changed.
pub(crate) fn apply_hints(plan: &mut sb_router::RoutePlan, hints: &Hints, pin_order: bool) -> bool {
    if plan.candidates.is_empty() || !hints.switches.any() {
        return false;
    }
    if pin_order {
        return false;
    }
    let before = plan.candidates.first().map(|t| t.id.clone());
    let mut changed = false;
    let mut sticky_applied = false;
    if hints.switches.stickiness {
        if let Some(sticky) = &hints.sticky {
            match plan.candidates.iter().position(|t| &t.id == sticky) {
                Some(index) if hints.up.contains(sticky) && usable(&plan.candidates[index]) => {
                    if index > 0 {
                        move_to(&mut plan.candidates, index, 0);
                        changed = true;
                    }
                    sticky_applied = true;
                    plan.decision.add_optimize(format!("sticky={sticky}"));
                }
                Some(_) => {
                    let why = hints
                        .not_up
                        .get(sticky)
                        .cloned()
                        .unwrap_or_else(|| "not healthy".to_string());
                    plan.decision
                        .add_optimize(format!("sticky={sticky} not kept ({why})"));
                }
                None => {}
            }
        }
    }
    if !sticky_applied {
        if let Some(tokens) = hints.big_prompt {
            let best = plan
                .candidates
                .iter()
                .enumerate()
                .filter(|(_, t)| hints.up.contains(&t.id) && usable(t))
                .filter_map(|(i, t)| hints.prompt_tps.get(&t.id).map(|tps| (i, *tps)))
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
            match best {
                Some((0, tps)) => plan.decision.add_optimize(format!(
                    "big_prompt ~{tokens} tok: {} already first ({tps:.0} prompt tok/s)",
                    plan.candidates[0].id
                )),
                Some((index, tps)) => {
                    let id = plan.candidates[index].id.clone();
                    move_to(&mut plan.candidates, index, 0);
                    changed = true;
                    plan.decision.add_optimize(format!(
                        "big_prompt ~{tokens} tok -> {id} ({tps:.0} prompt tok/s)"
                    ));
                }
                None => {}
            }
        }
    }
    if hints.switches.loaded_aware {
        // The first target that is up decides the machine.
        if let Some(first) = plan
            .candidates
            .iter()
            .position(|t| hints.up.contains(&t.id))
        {
            let first_id = plan.candidates[first].id.clone();
            if hints.loaded.get(&first_id) == Some(&false) {
                let host = hints.host.get(&first_id);
                let swap_free = plan
                    .candidates
                    .iter()
                    .enumerate()
                    .skip(first + 1)
                    .find(|(_, t)| {
                        hints.up.contains(&t.id)
                            && usable(t)
                            && hints.host.get(&t.id) == host
                            && hints.loaded.get(&t.id) == Some(&true)
                    })
                    .map(|(i, t)| (i, t.id.clone()));
                if let Some((index, id)) = swap_free {
                    if !sticky_applied || first > 0 {
                        move_to(&mut plan.candidates, index, first);
                        changed = true;
                        plan.decision
                            .add_optimize(format!("loaded={id} over {first_id} (no model swap)"));
                    }
                }
            }
        }
    }
    if changed {
        plan.decision.selected = plan
            .candidates
            .first()
            .map(|t| sb_core::TargetRef::new(t.id.clone()));
        plan.decision.fallbacks = plan
            .candidates
            .iter()
            .skip(1)
            .map(|t| sb_core::TargetRef::new(t.id.clone()))
            .collect();
        if let (Some(old), Some(new)) = (before, plan.candidates.first()) {
            if let Some(awake) = plan.decision.prefer_awake.clone() {
                let from = format!("chose {old}");
                if awake.contains(&from) && old != new.id {
                    plan.decision
                        .set_prefer_awake(awake.replace(&from, &format!("chose {}", new.id)));
                }
            }
        }
    }
    changed
}

// ---------------------------------------------------------------- content

/// Whether a stream carried answer text and thinking.
#[derive(Clone, Default)]
pub(crate) struct ContentSeen(Arc<(AtomicBool, AtomicBool)>);

impl ContentSeen {
    pub(crate) fn text(&self) -> bool {
        self.0 .0.load(Ordering::Relaxed)
    }
    pub(crate) fn reasoning(&self) -> bool {
        self.0 .1.load(Ordering::Relaxed)
    }
}

/// Watch a stream for answer text and thinking (for the empty-after-
/// thinking count). Passes every item through unchanged.
pub(crate) fn watch_content(
    stream: sb_adapter::EventStream,
) -> (sb_adapter::EventStream, ContentSeen) {
    let seen = ContentSeen::default();
    let flags = seen.clone();
    let stream = stream
        .inspect(move |item| match item {
            Ok(AiStreamEvent::TextDelta { text }) if !text.trim().is_empty() => {
                flags.0 .0.store(true, Ordering::Relaxed);
            }
            Ok(AiStreamEvent::ToolCallStart(_)) => {
                flags.0 .0.store(true, Ordering::Relaxed);
            }
            Ok(AiStreamEvent::ReasoningDelta { text }) if !text.is_empty() => {
                flags.0 .1.store(true, Ordering::Relaxed);
            }
            _ => {}
        })
        .boxed();
    (stream, seen)
}

/// The failure the reasoning guard prevents: the token budget ran out
/// (`finish_reason: length`) while the model was still thinking, so the
/// answer is empty.
pub(crate) fn empty_after_thinking(
    finish: Option<FinishReason>,
    answered: bool,
    thought: bool,
    usage: &Usage,
) -> bool {
    finish == Some(FinishReason::Length) && !answered && (thought || usage.reasoning_tokens > 0)
}

/// The same check on a collected response.
pub(crate) fn response_empty_after_thinking(response: &AiResponse) -> bool {
    let mut answered = false;
    let mut thought = false;
    for part in &response.message.content {
        match part {
            ContentPart::Text { text } if !text.trim().is_empty() => answered = true,
            ContentPart::ToolUse { .. } => answered = true,
            ContentPart::Reasoning { text, .. } if !text.is_empty() => thought = true,
            _ => {}
        }
    }
    empty_after_thinking(
        Some(response.finish_reason),
        answered,
        thought,
        &response.usage,
    )
}

// ---------------------------------------------------------------- engine glue

/// The conversation a request belongs to: its session (header or
/// metadata) or its `user` plus the hash of its opening.
pub(crate) fn conversation_of(req: &sb_core::AiRequest) -> Option<String> {
    sb_core::conversation_key(req, crate::helpers::session_affinity_key(req))
}

/// The reasoning-guard part of a settings summary, when the guard turned
/// thinking off.
pub(crate) fn guard_note(summary: &str) -> Option<String> {
    summary
        .split("; ")
        .find(|part| part.starts_with("reasoning_guard=off"))
        .map(|part| part.replace("reasoning_guard=off", "reasoning_guard=thinking off"))
}

/// Apply a target's reasoning guard to the request one attempt sends.
/// `Some(note)` for the settings summary (`reasoning_guard=off (max_tokens
/// 256 < 1024)`, or why it did nothing).
pub(crate) fn apply_reasoning_guard(
    prepared: &mut sb_core::AiRequest,
    original: &sb_core::AiRequest,
    request: Option<&sb_core::RequestSettings>,
    doc: &sb_core::ModelSettingsDoc,
    guard: &sb_core::ReasoningGuard,
) -> Option<String> {
    if sb_core::request_asks_reasoning(original, request.map(|r| &r.settings)) {
        return Some("reasoning_guard=kept (the request asked for thinking)".to_string());
    }
    let already_off = prepared
        .passthrough
        .get("reasoning_effort")
        .and_then(serde_json::Value::as_str)
        == Some("none")
        || prepared
            .passthrough
            .get("chat_template_kwargs")
            .and_then(|kwargs| kwargs.get("enable_thinking"))
            .and_then(serde_json::Value::as_bool)
            == Some(false);
    if already_off {
        return None;
    }
    let max_tokens = prepared.max_output_tokens.map(u64::from).or_else(|| {
        ["max_tokens", "max_completion_tokens"]
            .iter()
            .find_map(|key| {
                prepared
                    .passthrough
                    .get(*key)
                    .and_then(serde_json::Value::as_u64)
            })
    })?;
    if max_tokens >= u64::from(guard.min_tokens) {
        return None;
    }
    if doc.is_locked("reasoning.enabled") {
        return Some("reasoning_guard=skipped (reasoning is locked)".to_string());
    }
    sb_core::turn_thinking_off(prepared);
    Some(format!(
        "reasoning_guard=off (max_tokens {max_tokens} < {})",
        guard.min_tokens
    ))
}

/// Records one attempt's outcome into the speed-trick state.
pub(crate) struct Observer {
    state: Arc<OptimizeState>,
    target_id: String,
    provider_id: String,
    engine_model: String,
    conversation: Option<String>,
    prompt_key: Option<String>,
    guard_applied: bool,
    engine_lane: Option<Option<u32>>,
    ttl_ms: i64,
    max: usize,
    settings_revision: u64,
}

impl Observer {
    pub(crate) fn served(&self, usage: &Usage, prompt_tps: Option<f64>, empty: bool) {
        self.state.observe(Served {
            target_id: &self.target_id,
            provider_id: &self.provider_id,
            engine_model: &self.engine_model,
            conversation: self.conversation.as_deref(),
            prompt_key: self.prompt_key.as_deref(),
            usage,
            prompt_tps,
            empty_after_thinking: empty,
            guard_applied: self.guard_applied,
            engine_lane: self.engine_lane,
            sticky_ttl_ms: self.ttl_ms,
            sticky_max: self.max,
            settings_revision: self.settings_revision,
        });
    }
}

/// The sticky slot to pin on a request, or why not. The slot is pinned only
/// on the engine model that served it (the target's own, not a variant
/// whose slot count is unknown), under the same settings revision, and
/// only when it exists under the current `parallel`: llama.cpp may defer a
/// task naming a slot it does not have indefinitely.
pub(crate) fn sticky_slot(
    sticky: &Sticky,
    engine_model: &str,
    own_model: &str,
    settings_revision: u64,
    parallel: u64,
) -> Result<u32, String> {
    let slot = sticky.slot.ok_or_else(|| "no slot known".to_string())?;
    if engine_model != sticky.engine_model {
        return Err(format!("slot {slot} not pinned (another engine model)"));
    }
    if engine_model != own_model {
        return Err(format!("slot {slot} not pinned (a load-settings variant)"));
    }
    if settings_revision != sticky.settings_revision {
        return Err(format!("slot {slot} not pinned (settings changed)"));
    }
    if u64::from(slot) >= parallel {
        return Err(format!("slot {slot} not pinned (parallel {parallel})"));
    }
    Ok(slot)
}

fn lane_of<'a>(
    snap: &'a crate::Snapshot,
    provider_id: &str,
) -> Option<&'a sb_core::LocalExecutorConfig> {
    snap.config
        .local_executors
        .iter()
        .find(|lane| lane.name == provider_id)
}

fn stickiness_configured(snap: &crate::Snapshot) -> bool {
    snap.config.server.optimize.stickiness
        || snap
            .config
            .routes
            .iter()
            .any(|route| route.optimize.is_some_and(|o| o.stickiness == Some(true)))
}

impl crate::Engine {
    /// Tuning locks: a candidate on a lane under a tuning run leaves the
    /// request unless the request carries that run's lock id. Returns the
    /// rejections for the decision; `Err` (503 `lane_tuning`) when nothing
    /// is left to serve.
    ///
    /// A quiet window in force (`server.optimize.quiet_windows`) sets every
    /// lock aside: a lock left by a tuner that died never blocks the work
    /// the window protects.
    pub(crate) fn apply_tune_locks(
        &self,
        snap: &crate::Snapshot,
        req: &sb_core::AiRequest,
        resolved: &mut crate::profiles::CandidateResolution,
    ) -> Result<Vec<(String, String)>, crate::ExecError> {
        if resolved.candidates.is_empty() {
            return Ok(Vec::new());
        }
        let locked = |candidate: &sb_core::ExecutionTarget| {
            self.optimize.lock_for_lane(&candidate.provider_id)
        };
        if !resolved.candidates.iter().any(|c| locked(c).is_some())
            || quiet_window_now(&snap.config.server.optimize, now_ms()).is_some()
        {
            return Ok(Vec::new());
        }
        let carried = req
            .metadata
            .get(sb_core::TUNE_LOCK_METADATA_KEY)
            .map(String::as_str);
        let mut rejected = Vec::new();
        resolved
            .candidates
            .retain(|candidate| match locked(candidate) {
                Some(lock) if Some(lock.id.as_str()) != carried => {
                    rejected.push((
                        candidate.id.clone(),
                        format!(
                            "local lane `{}` is under a tuning run ({}); not counted as a failure",
                            candidate.provider_id, lock.target
                        ),
                    ));
                    false
                }
                _ => true,
            });
        if resolved.candidates.is_empty() {
            let lanes: Vec<&str> = rejected.iter().map(|(target, _)| target.as_str()).collect();
            return Err(crate::ExecError::new(
                503,
                "lane_tuning",
                format!(
                    "every target that can serve this ({}) is on a lane under a tuning run; retry later",
                    lanes.join(",")
                ),
                None,
            )
            .with_retry_after(std::time::Duration::from_secs(30)));
        }
        Ok(rejected)
    }

    /// Where the request's conversation was served last, when stickiness
    /// is on anywhere and the conversation is known.
    pub(crate) fn sticky_for(
        &self,
        snap: &crate::Snapshot,
        conversation: Option<&str>,
    ) -> Option<Sticky> {
        if !stickiness_configured(snap) {
            return None;
        }
        let ttl = (snap.config.server.optimize.sticky_ttl_secs as i64).saturating_mul(1000);
        self.optimize.sticky(conversation?, ttl)
    }

    /// The ordering pass's view of a request's candidates, or `None` when
    /// the route has no speed trick on.
    pub(crate) fn optimize_hints(
        &self,
        snap: &crate::Snapshot,
        req: &sb_core::AiRequest,
        resolved: &crate::profiles::CandidateResolution,
        sticky: Option<&Sticky>,
    ) -> Option<Hints> {
        let route = snap
            .config
            .routes
            .iter()
            .find(|route| route.name == resolved.route_name);
        let switches = RouteOptimize::resolve(
            &snap.config.server.optimize,
            route.and_then(|route| route.optimize.as_ref()),
        );
        if !switches.any() {
            return None;
        }
        let mut hints = Hints {
            switches,
            sticky: sticky
                .filter(|_| switches.stickiness)
                .map(|sticky| sticky.target_id.clone()),
            ..Hints::default()
        };
        let lanes = self.lanes.as_deref();
        for candidate in &resolved.candidates {
            let lane = lane_of(snap, &candidate.provider_id);
            let host = lane
                .map(|lane| lane.host.clone().unwrap_or_else(|| lane.name.clone()))
                .unwrap_or_else(|| candidate.provider_id.clone());
            let why_not = match (lane, lanes) {
                (Some(_), Some(lanes)) => lanes
                    .known_down(&candidate.provider_id)
                    .map(|detail| format!("{host} down: {detail}"))
                    .or_else(|| {
                        lanes
                            .wake_need(&candidate.provider_id)
                            .map(|wake| wake.describe())
                    })
                    .or_else(|| {
                        lanes
                            .lane_full(&candidate.provider_id)
                            .map(|full| format!("{} {full}", candidate.provider_id))
                    }),
                _ => None,
            };
            match why_not {
                Some(why) => {
                    hints.not_up.insert(candidate.id.clone(), why);
                }
                None => {
                    hints.up.insert(candidate.id.clone());
                }
            }
            if switches.loaded_aware && lane.is_some_and(|lane| lane.engine.is_some()) {
                if let Some(loaded) = self
                    .optimize
                    .is_loaded(&candidate.provider_id, &candidate.model)
                {
                    hints.loaded.insert(candidate.id.clone(), loaded);
                }
            }
            if let Some(tps) = self.optimize.prompt_tps(&candidate.id) {
                hints.prompt_tps.insert(candidate.id.clone(), tps);
            }
            hints.host.insert(candidate.id.clone(), host);
        }
        if let Some(threshold) = switches.big_prompt_tokens {
            let placement_route = snap.runtime.latency_aware || resolved.prefer_awake.is_some();
            let tokens = req.estimated_input_tokens();
            if placement_route && tokens >= u64::from(threshold) {
                hints.big_prompt = Some(tokens);
            }
        }
        Some(hints)
    }

    /// The sticky conversation's engine slot rides along as `id_slot` on a
    /// llama.cpp target with more than one slot, when the engine said which
    /// slot served it and nothing else of ours runs on that model now (so
    /// the pinned slot cannot make the request wait).
    pub(crate) fn pin_sticky_slot(
        &self,
        snap: &crate::Snapshot,
        req: &sb_core::AiRequest,
        sticky: Option<&Sticky>,
        plan: &mut sb_router::RoutePlan,
        settings_plan: &mut crate::model_settings::SettingsPlan,
    ) {
        let (Some(sticky), Some(first)) = (sticky, plan.candidates.first().cloned()) else {
            return;
        };
        if sticky.slot.is_none() {
            return;
        }
        if first.id != sticky.target_id
            || !plan
                .decision
                .optimize
                .as_deref()
                .is_some_and(|line| line.contains(&format!("sticky={}", first.id)))
        {
            return;
        }
        if crate::model_settings::settings_engine(snap, &first.provider_id)
            != sb_core::SettingsEngine::LlamaCpp
            || req.passthrough.contains_key("id_slot")
        {
            return;
        }
        let parallel = self
            .model_settings
            .get(&first.id)
            .and_then(|entry| {
                entry
                    .doc
                    .load
                    .get("parallel")
                    .and_then(serde_json::Value::as_u64)
            })
            .unwrap_or(1);
        if parallel <= 1 {
            return;
        }
        let slot = match sticky_slot(
            sticky,
            &settings_plan.engine_model(&first),
            &first.model,
            self.model_settings.revision(&first.id),
            parallel,
        ) {
            Ok(slot) => slot,
            Err(why) => {
                plan.decision.add_optimize(why);
                return;
            }
        };
        // Others on the model: every admitted request but this one's own
        // admission (taken before routing on a lane with an engine control).
        let key = format!("{}/{}", first.provider_id, sticky.engine_model);
        let own = usize::from(settings_plan.holds_admission(&first.id, &key));
        if self.model_settings.in_flight(&key).saturating_sub(own) > 0 {
            plan.decision
                .add_optimize(format!("slot {slot} not pinned (model busy)"));
            return;
        }
        settings_plan.set_passthrough(&first, req, "id_slot", serde_json::Value::from(slot));
        plan.decision.add_optimize(format!("slot={slot}"));
        self.optimize.count_sticky_kept(&first.id);
    }

    /// The observer of one attempt on `target`.
    pub(crate) fn observer(
        &self,
        snap: &crate::Snapshot,
        target: &sb_core::ExecutionTarget,
        settings_plan: &crate::model_settings::SettingsPlan,
        conversation: Option<String>,
        prompt_key: Option<String>,
    ) -> Observer {
        let optimize = &snap.config.server.optimize;
        Observer {
            state: self.optimize.clone(),
            target_id: target.id.clone(),
            provider_id: target.provider_id.clone(),
            engine_model: settings_plan.engine_model(target),
            conversation: conversation.filter(|_| stickiness_configured(snap)),
            prompt_key,
            guard_applied: settings_plan
                .summary_for(&target.id)
                .is_some_and(|summary| summary.contains("reasoning_guard=off")),
            engine_lane: lane_of(snap, &target.provider_id)
                .and_then(|lane| lane.engine.as_ref())
                .map(|engine| engine.models_max),
            ttl_ms: (optimize.sticky_ttl_secs as i64).saturating_mul(1000),
            max: optimize.sticky_max_conversations,
            settings_revision: self.model_settings.revision(&target.id),
        }
    }
}

/// Minutes since local midnight at `now_ms` in IANA zone `tz` (`None` =
/// UTC). `None` when the zone is unknown.
pub fn local_minute(now_ms: i64, tz: Option<&str>) -> Option<u32> {
    let instant = jiff::Timestamp::from_millisecond(now_ms).ok()?;
    let zone = match tz {
        None => jiff::tz::TimeZone::UTC,
        Some(name) => jiff::tz::TimeZone::get(name).ok()?,
    };
    let local = zone.to_datetime(instant);
    Some(local.hour() as u32 * 60 + local.minute() as u32)
}

/// The quiet window (`server.optimize.quiet_windows`) holding `now_ms`, as
/// its label (`00:30-03:00 Asia/Tokyo`), each window read in its own
/// time zone.
pub fn quiet_window_now(config: &sb_core::OptimizeConfig, now_ms: i64) -> Option<String> {
    config.quiet_window_at(|tz| local_minute(now_ms, tz))
}

/// When the next quiet window begins after `now_ms` (epoch ms, on a minute
/// boundary), within two days; `None` with no quiet window. Each window is
/// read in its own time zone, so a daylight-saving change is honored.
pub fn next_quiet_window_start(config: &sb_core::OptimizeConfig, now_ms: i64) -> Option<i64> {
    if config.quiet_windows.is_empty() {
        return None;
    }
    const MINUTE: i64 = 60_000;
    let first = now_ms.div_euclid(MINUTE) * MINUTE + MINUTE;
    let mut was_quiet = quiet_window_now(config, now_ms).is_some();
    (0..2 * 24 * 60).map(|m| first + m * MINUTE).find(|at| {
        let quiet = quiet_window_now(config, *at).is_some();
        let starts = quiet && !was_quiet;
        was_quiet = quiet;
        starts
    })
}

/// Every time zone the quiet windows name must be a known IANA name.
pub fn validate_time_zones(config: &sb_core::OptimizeConfig) -> Result<(), String> {
    for (_, tz) in config.windows() {
        if let Some(name) = tz {
            jiff::tz::TimeZone::get(name).map_err(|_| {
                format!(
                    "server.optimize: time zone `{name}` is not a known IANA name \
                     (for example Asia/Tokyo or UTC)"
                )
            })?;
        }
    }
    if let Some(name) = &config.timezone {
        jiff::tz::TimeZone::get(name)
            .map_err(|_| format!("server.optimize.timezone `{name}` is not a known IANA name"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_windows_are_read_in_their_own_time_zone() {
        // 2026-01-15 16:15 UTC = 2026-01-16 01:15 in Tokyo (UTC+9).
        let now = jiff::Timestamp::from_second(1_768_493_700)
            .unwrap()
            .as_millisecond();
        assert_eq!(local_minute(now, None), Some(16 * 60 + 15));
        assert_eq!(local_minute(now, Some("Asia/Tokyo")), Some(75));
        assert_eq!(local_minute(now, Some("Mars/Base")), None);
        let cfg: sb_core::OptimizeConfig = serde_json::from_value(serde_json::json!({
            "quiet_windows": [{"start": "00:30", "end": "03:00", "tz": "Asia/Tokyo"}]
        }))
        .unwrap();
        assert_eq!(
            quiet_window_now(&cfg, now).as_deref(),
            Some("00:30-03:00 Asia/Tokyo")
        );
        // The same span in UTC (a plain string) does not hold at 16:15 UTC.
        let utc: sb_core::OptimizeConfig = serde_json::from_value(serde_json::json!({
            "quiet_windows": ["00:30-03:00"]
        }))
        .unwrap();
        assert_eq!(quiet_window_now(&utc, now), None);
        // A server zone applies to plain strings.
        let zoned: sb_core::OptimizeConfig = serde_json::from_value(serde_json::json!({
            "timezone": "Asia/Tokyo", "quiet_windows": ["00:30-03:00"]
        }))
        .unwrap();
        assert!(quiet_window_now(&zoned, now).is_some());
        validate_time_zones(&zoned).unwrap();
        let bad: sb_core::OptimizeConfig = serde_json::from_value(serde_json::json!({
            "quiet_windows": [{"start": "00:30", "end": "03:00", "tz": "Mars/Base"}]
        }))
        .unwrap();
        assert!(validate_time_zones(&bad).unwrap_err().contains("Mars/Base"));
    }

    #[test]
    fn a_sticky_slot_is_pinned_only_where_it_still_exists() {
        let sticky = Sticky {
            target_id: "box/a".into(),
            provider_id: "box".into(),
            engine_model: "a".into(),
            slot: Some(3),
            settings_revision: 7,
            at_ms: 0,
        };
        // Same model, same revision, a slot under `parallel`: pinned.
        assert_eq!(sticky_slot(&sticky, "a", "a", 7, 4), Ok(3));
        // Recorded under `parallel 4`, now 2 (same revision: the engine
        // reported a slot the settings do not have): not pinned.
        assert!(sticky_slot(&sticky, "a", "a", 7, 2)
            .unwrap_err()
            .contains("parallel 2"));
        assert!(sticky_slot(&sticky, "a", "a", 7, 3).is_err());
        // A settings write since: the slot is dropped.
        assert!(sticky_slot(&sticky, "a", "a", 8, 4)
            .unwrap_err()
            .contains("settings changed"));
        // Another engine model (a variant of the request's own load
        // settings, or another model): not pinned.
        assert!(sticky_slot(&sticky, "a~ctx8k", "a", 7, 4)
            .unwrap_err()
            .contains("another engine model"));
        let on_variant = Sticky {
            engine_model: "a~ctx8k".into(),
            ..sticky.clone()
        };
        assert!(sticky_slot(&on_variant, "a~ctx8k", "a", 7, 4)
            .unwrap_err()
            .contains("variant"));
    }

    #[test]
    fn the_next_quiet_window_start_is_found_in_its_own_zone() {
        // 2026-01-15 16:15 UTC = 2026-01-16 01:15 in Tokyo.
        let now = jiff::Timestamp::from_second(1_768_493_700)
            .unwrap()
            .as_millisecond();
        let at = |second: i64| second * 1000;
        let cfg: sb_core::OptimizeConfig =
            serde_json::from_value(serde_json::json!({"quiet_windows": ["01:30-03:00"]})).unwrap();
        // 2026-01-16 01:30 UTC.
        assert_eq!(next_quiet_window_start(&cfg, now), Some(at(1_768_527_000)));
        // Tokyo 02:00 = 17:00 UTC, the same day.
        let tokyo: sb_core::OptimizeConfig = serde_json::from_value(serde_json::json!({
            "quiet_windows": [{"start": "02:00", "end": "03:00", "tz": "Asia/Tokyo"}]
        }))
        .unwrap();
        assert_eq!(
            next_quiet_window_start(&tokyo, now),
            Some(at(1_768_496_400))
        );
        // Inside a window: the next one, a day later.
        let inside: sb_core::OptimizeConfig =
            serde_json::from_value(serde_json::json!({"quiet_windows": ["16:00-17:00"]})).unwrap();
        assert_eq!(
            next_quiet_window_start(&inside, now),
            Some(at(1_768_579_200))
        );
        let none = sb_core::OptimizeConfig::default();
        assert_eq!(next_quiet_window_start(&none, now), None);
    }
    use sb_core::{EngineTimings, ExecutionTarget, ExecutionTargetKind};

    fn target(provider: &str, model: &str) -> ExecutionTarget {
        ExecutionTarget::new(provider, model, ExecutionTargetKind::ModelApi)
    }

    fn plan(ids: &[(&str, &str)]) -> sb_router::RoutePlan {
        let candidates: Vec<ExecutionTarget> = ids.iter().map(|(p, m)| target(p, m)).collect();
        let mut decision = sb_core::RouteDecision::new("r", "ordered_fallback");
        decision.selected = candidates
            .first()
            .map(|t| sb_core::TargetRef::new(t.id.clone()));
        decision.fallbacks = candidates
            .iter()
            .skip(1)
            .map(|t| sb_core::TargetRef::new(t.id.clone()))
            .collect();
        sb_router::RoutePlan {
            candidates,
            decision,
        }
    }

    fn order(plan: &sb_router::RoutePlan) -> Vec<String> {
        plan.candidates.iter().map(|t| t.id.clone()).collect()
    }

    fn served<'a>(
        target: &'a str,
        provider: &'a str,
        model: &'a str,
        usage: &'a Usage,
    ) -> Served<'a> {
        Served {
            target_id: target,
            provider_id: provider,
            engine_model: model,
            conversation: None,
            prompt_key: None,
            usage,
            prompt_tps: None,
            empty_after_thinking: false,
            guard_applied: false,
            engine_lane: Some(Some(1)),
            sticky_ttl_ms: 60_000,
            sticky_max: 100,
            settings_revision: 0,
        }
    }

    #[test]
    fn sticky_target_moves_first_only_when_up() {
        let mut hints = Hints {
            switches: RouteOptimize {
                stickiness: true,
                ..RouteOptimize::default()
            },
            sticky: Some("small/m".into()),
            ..Hints::default()
        };
        hints.up.insert("small/m".into());
        hints.up.insert("big/m".into());
        let mut p = plan(&[("big", "m"), ("small", "m")]);
        assert!(apply_hints(&mut p, &hints, false));
        assert_eq!(order(&p), ["small/m", "big/m"]);
        assert_eq!(p.decision.selected.as_ref().unwrap().target_id, "small/m");
        assert!(p.decision.summary().contains("optimize=[sticky=small/m]"));
        // Asleep (not up): never moved, and said why.
        hints.up.remove("small/m");
        hints.not_up.insert("small/m".into(), "small asleep".into());
        let mut p = plan(&[("big", "m"), ("small", "m")]);
        assert!(!apply_hints(&mut p, &hints, false));
        assert_eq!(order(&p), ["big/m", "small/m"]);
        assert!(p
            .decision
            .optimize
            .as_deref()
            .unwrap()
            .contains("not kept (small asleep)"));
        // A pinned order is never touched.
        hints.up.insert("small/m".into());
        let mut p = plan(&[("big", "m"), ("small", "m")]);
        assert!(!apply_hints(&mut p, &hints, true));
        assert_eq!(order(&p), ["big/m", "small/m"]);
    }

    #[test]
    fn loaded_model_goes_first_on_the_same_machine_only() {
        let mut hints = Hints {
            switches: RouteOptimize {
                loaded_aware: true,
                ..RouteOptimize::default()
            },
            ..Hints::default()
        };
        for (id, host, loaded) in [
            ("box/a", "gpu", false),
            ("box/b", "gpu", true),
            ("other/c", "small", true),
        ] {
            hints.up.insert(id.into());
            hints.host.insert(id.into(), host.into());
            hints.loaded.insert(id.into(), loaded);
        }
        let mut p = plan(&[("box", "a"), ("other", "c"), ("box", "b")]);
        assert!(apply_hints(&mut p, &hints, false));
        assert_eq!(order(&p), ["box/b", "box/a", "other/c"]);
        assert!(p
            .decision
            .optimize
            .as_deref()
            .unwrap()
            .contains("loaded=box/b over box/a"));
        // A loaded target on a sleeping machine never overtakes.
        hints.up.remove("box/b");
        let mut p = plan(&[("box", "a"), ("box", "b")]);
        assert!(!apply_hints(&mut p, &hints, false));
    }

    #[test]
    fn big_prompt_goes_to_the_fastest_measured_prefill() {
        let mut hints = Hints {
            switches: RouteOptimize {
                big_prompt_tokens: Some(1000),
                ..RouteOptimize::default()
            },
            big_prompt: Some(5000),
            ..Hints::default()
        };
        hints.up.insert("mac/m".into());
        hints.up.insert("box/m".into());
        hints.prompt_tps.insert("mac/m".into(), 1380.0);
        hints.prompt_tps.insert("box/m".into(), 10_000.0);
        let mut p = plan(&[("mac", "m"), ("box", "m")]);
        assert!(apply_hints(&mut p, &hints, false));
        assert_eq!(order(&p), ["box/m", "mac/m"]);
        assert!(p
            .decision
            .optimize
            .as_deref()
            .unwrap()
            .contains("big_prompt"));
        // Already first: said so, nothing moves.
        let mut p = plan(&[("box", "m"), ("mac", "m")]);
        assert!(!apply_hints(&mut p, &hints, false));
        assert!(p
            .decision
            .optimize
            .as_deref()
            .unwrap()
            .contains("already first"));
    }

    #[test]
    fn swaps_are_counted_when_a_request_loads_another_model() {
        let state = OptimizeState::new();
        let usage = Usage::default();
        // First request after start: no evidence, no swap.
        state.observe(served("box/a", "box", "a", &usage));
        state.observe(served("box/a", "box", "a", &usage));
        state.observe(served("box/b", "box", "b", &usage));
        state.observe(served("box/a", "box", "a", &usage));
        let obs = state.observations();
        assert_eq!(obs["box/b"].swaps_last_hour, 1);
        assert_eq!(obs["box/a"].swaps_last_hour, 1);
        assert_eq!(obs["box/a"].swapped_out_last_hour["b"], 1);
        // models_max 1: only the last one is loaded now.
        assert_eq!(state.is_loaded("box", "a"), Some(true));
        assert_eq!(state.is_loaded("box", "b"), Some(false));
        assert_eq!(state.is_loaded("elsewhere", "a"), None);
    }

    #[test]
    fn cache_draft_and_empty_answers_are_counted() {
        let state = OptimizeState::new();
        let usage = Usage {
            input_tokens: 100,
            engine_timings: Some(EngineTimings {
                cache_tokens: Some(80),
                draft_tokens: Some(10),
                draft_accepted: Some(4),
                slot: Some(1),
                ..EngineTimings::default()
            }),
            ..Usage::default()
        };
        let mut first = served("box/a", "box", "a", &usage);
        first.prompt_key = Some("k");
        first.conversation = Some("c1");
        state.observe(first);
        let mut again = served("box/a", "box", "a", &usage);
        again.prompt_key = Some("k");
        again.empty_after_thinking = true;
        state.observe(again);
        let obs = &state.observations()["box/a"];
        assert_eq!((obs.cached_tokens, obs.cache_hit_requests), (160, 2));
        assert_eq!((obs.repeat_prompt_requests, obs.repeat_prompt_hits), (1, 1));
        assert_eq!((obs.draft_tokens, obs.draft_accepted), (20, 8));
        assert_eq!(obs.empty_after_thinking, 1);
        let sticky = state.sticky("c1", 60_000).unwrap();
        assert_eq!((sticky.target_id.as_str(), sticky.slot), ("box/a", Some(1)));
    }

    #[test]
    fn empty_after_thinking_needs_length_no_answer_and_thinking() {
        let usage = Usage::default();
        assert!(empty_after_thinking(
            Some(FinishReason::Length),
            false,
            true,
            &usage
        ));
        assert!(!empty_after_thinking(
            Some(FinishReason::Stop),
            false,
            true,
            &usage
        ));
        assert!(!empty_after_thinking(
            Some(FinishReason::Length),
            true,
            true,
            &usage
        ));
        assert!(!empty_after_thinking(
            Some(FinishReason::Length),
            false,
            false,
            &usage
        ));
        let thinking = Usage {
            reasoning_tokens: 50,
            ..Usage::default()
        };
        assert!(empty_after_thinking(
            Some(FinishReason::Length),
            false,
            false,
            &thinking
        ));
    }
}
