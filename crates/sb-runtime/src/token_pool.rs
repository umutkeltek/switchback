//! Token-pool admission for local llama.cpp models.
//!
//! A llama.cpp model started with `--kv-unified` gives all of its
//! `--parallel` slots one shared pool of `--ctx-size` tokens. Requests that
//! run at the same time and together need more tokens than the pool holds
//! (prompt plus generated) all fail at once: the engine answers "context
//! size has been exceeded" to every one of them. Counting requests does not
//! prevent it; counting tokens does.
//!
//! Before an attempt reaches such a model, it reserves its need in the
//! model's pool (one pool per lane and engine model): its prompt tokens plus
//! a generation reserve, capped at what one request may use. It is sent
//! while the pool has room; otherwise it waits in the pool's FIFO queue
//! (bounded by its lane's `queue_timeout_secs`) or spills to the route's
//! next target. A request that alone needs more than one request may use is
//! refused (400) and never sent. A request alone in an empty pool is always
//! admitted, so a missing `max_tokens` never starves or deadlocks. The
//! reservation is given back when its [`PoolReservation`] is dropped: on
//! success, error, client disconnect and stream cut alike.
//!
//! The pool's size is read at every admission from the engine's own numbers
//! (its last model listing, through [`PoolView`]: the loaded model's
//! arguments, else the preset arguments it lists for a model not loaded
//! yet), else from the model's settings document; when neither is known the request is sent unchecked,
//! as without admission, but its need is still counted so later requests
//! see it once the size is known.
//!
//! The prompt estimate: a quick bound over the request's text (no call),
//! measured against the Qwen tokenizer on English prose, code, JSON, six
//! other scripts, hex, base64, UUIDs, digits and emoji: at least the real
//! count on every sample but random mixed-case words (0.99x) and at most
//! about 1.7x on prose and code. A prompt whose bound reaches the lane's
//! `count_above_tokens` is counted by the engine's own tokenizer instead
//! (one bounded call), so a large prompt is neither under- nor
//! over-counted; when that call fails the bound is used.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use sb_core::{AiRequest, ContentPart, PoolAdmissionConfig, ToolResultContentPart};
use serde::Serialize;

use crate::BoxFuture;

/// Tokens one image costs a vision model (the flat figure of
/// [`AiRequest::estimated_input_tokens`]).
pub const IMAGE_TOKENS: u64 = 1_600;
/// The chat template's fixed tokens (generation prompt, system frame).
const BASE_TEMPLATE_TOKENS: u64 = 16;
/// The chat template's tokens around each message.
const MESSAGE_TEMPLATE_TOKENS: u64 = 8;
/// The chat template's tool instructions when tools are offered.
const TOOLS_TEMPLATE_TOKENS: u64 = 256;
/// The quick bound is at most this many times the real count on every
/// text measured (Arabic reached 3.2x); a refusal on the bound alone needs
/// the request to be too large even at this ratio.
const QUICK_MAX_OVERCOUNT: f64 = 3.5;
/// How often a waiter reads the pool's size again while nothing else wakes
/// it (a reload that grows the pool lets it in without a release).
const TRUTH_RECHECK_MS: u64 = 1_000;

// ---------------------------------------------------------------- truth

/// Where a pool's size came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PoolSource {
    /// The engine's own listing of the loaded model (its arguments).
    Engine,
    /// The engine's listing of a model it has not loaded yet: the
    /// arguments its preset will load it with.
    Preset,
    /// The model's settings document (its load settings), while the
    /// engine lists no arguments for the model.
    Settings,
}

impl PoolSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Engine => "engine",
            Self::Preset => "preset",
            Self::Settings => "settings",
        }
    }
}

/// A model's context pool: the tokens its slots share and the most one
/// request may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PoolTruth {
    /// `--ctx-size`: the tokens all slots share (kv-unified), or their sum.
    pub pool_tokens: u64,
    /// The most one request may use: the whole pool with kv-unified, one
    /// slot's share (`ctx-size / parallel`) without.
    pub per_request_tokens: u64,
    pub slots: Option<u64>,
    pub kv_unified: Option<bool>,
    pub source: PoolSource,
}

/// The engine side of admission (`sb-server`'s engine control).
pub trait PoolView: Send + Sync {
    /// The pool of engine model `model` on `provider_id`'s lane from what
    /// is already known (the engine's last listing, else the settings
    /// document). Never calls a machine.
    fn cached(&self, provider_id: &str, model: &str) -> Option<PoolTruth>;

    /// Like [`Self::cached`], but when the engine has not listed the model
    /// loaded yet it may read the engine's listing again first (bounded,
    /// at most once a second per lane).
    fn fresh<'a>(
        &'a self,
        provider_id: &'a str,
        model: &'a str,
    ) -> BoxFuture<'a, Option<PoolTruth>> {
        Box::pin(std::future::ready(self.cached(provider_id, model)))
    }

    /// The engine's own token count of `text` for `model` (bounded), or
    /// `None` when it could not be had.
    fn count_tokens<'a>(
        &'a self,
        _provider_id: &'a str,
        _model: &'a str,
        _text: String,
    ) -> BoxFuture<'a, Option<u64>> {
        Box::pin(std::future::ready(None))
    }
}

// ---------------------------------------------------------------- estimate

fn is_wide_script(ch: char) -> bool {
    matches!(ch as u32,
        0x2E80..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x20000..)
}

/// 0 digit, 1 lower-case letter, 2 upper-case letter.
fn ascii_class(byte: u8) -> u8 {
    if byte.is_ascii_digit() {
        0
    } else if byte.is_ascii_lowercase() {
        1
    } else {
        2
    }
}

/// Tokens of one run of ASCII letters and digits. Words split into
/// segments (at digits and at a lower-to-upper case change) of about four
/// letters per token; every digit is one token; a run that looks random
/// (frequent letter/digit/case changes, or many tiny segments: hex, base64,
/// UUIDs, keys) counts 1.3 letters per token.
fn alnum_run_tokens(run: &[u8]) -> f64 {
    let digits = run.iter().filter(|byte| byte.is_ascii_digit()).count();
    let letters = run.len() - digits;
    let changes = run
        .windows(2)
        .filter(|pair| ascii_class(pair[0]) != ascii_class(pair[1]))
        .count();
    let (mut segments, mut segment, mut segment_tokens) = (0usize, 0usize, 0.0f64);
    let mut prev = None;
    for &byte in run {
        let class = ascii_class(byte);
        if class == 0 {
            if segment > 0 {
                segments += 1;
                segment_tokens += (segment as f64 / 4.0).max(1.0);
                segment = 0;
            }
        } else if segment > 0 && class == 2 && prev == Some(1) {
            segments += 1;
            segment_tokens += (segment as f64 / 4.0).max(1.0);
            segment = 1;
        } else {
            segment += 1;
        }
        prev = Some(class);
    }
    if segment > 0 {
        segments += 1;
        segment_tokens += (segment as f64 / 4.0).max(1.0);
    }
    let mixed = digits > 0 && letters > 0 && changes * 4 >= run.len();
    let choppy = segments >= 4 && (letters as f64) < 2.5 * segments as f64;
    if mixed || choppy {
        digits as f64 + letters as f64 / 1.3
    } else {
        digits as f64 + segment_tokens
    }
}

/// A quick upper estimate of the tokens of `text` (no call). See the module
/// documentation for how it was measured.
pub fn quick_text_tokens(text: &str) -> f64 {
    let bytes = text.as_bytes();
    let mut total = 0.0f64;
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte.is_ascii_alphanumeric() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_alphanumeric() {
                i += 1;
            }
            total += alnum_run_tokens(&bytes[start..i]);
        } else if byte.is_ascii_whitespace() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let len = i - start;
            // A single space before a word merges into the word's token.
            let joins_word =
                len == 1 && byte == b' ' && bytes.get(i).is_some_and(u8::is_ascii_alphabetic);
            if !joins_word {
                total += len.div_ceil(8) as f64;
            }
        } else if byte < 0x80 {
            total += 1.0;
            i += 1;
        } else {
            let Some(ch) = text.get(i..).and_then(|rest| rest.chars().next()) else {
                break;
            };
            let len = ch.len_utf8();
            total += if is_wide_script(ch) {
                1.0
            } else if len >= 4 {
                0.7 * len as f64
            } else {
                0.6 * len as f64
            };
            i += len;
        }
    }
    total * 1.05
}

/// The text a request sends a model (system, messages, tool calls and
/// results, tool definitions), joined for counting, and the template's
/// part the text does not show.
pub struct PromptText {
    pub text: String,
    pub images: u64,
    pub messages: u64,
    pub tools: bool,
}

impl PromptText {
    pub fn of(req: &AiRequest) -> Self {
        let mut text = String::new();
        let mut images = 0u64;
        let mut push = |piece: &str| {
            if !piece.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(piece);
            }
        };
        if let Some(system) = &req.system {
            push(system);
        }
        for message in &req.messages {
            for part in &message.content {
                match part {
                    ContentPart::Text { text } | ContentPart::Reasoning { text, .. } => push(text),
                    ContentPart::Image { .. } => images += 1,
                    ContentPart::ToolUse { name, args, .. }
                    | ContentPart::ServerToolUse { name, args, .. } => {
                        push(name);
                        push(&args.to_string());
                    }
                    ContentPart::ToolResult {
                        content,
                        content_parts,
                        ..
                    } => {
                        push(content);
                        for part in content_parts {
                            match part {
                                ToolResultContentPart::Text { text } => push(text),
                                ToolResultContentPart::Image { .. } => images += 1,
                            }
                        }
                    }
                    ContentPart::ServerToolResult { content, .. } => push(content),
                    ContentPart::Citation { url, .. } => push(url),
                }
            }
        }
        for tool in &req.tools {
            push(&tool.name);
            if let Some(description) = &tool.description {
                push(description);
            }
            push(&tool.parameters.to_string());
        }
        PromptText {
            text,
            images,
            messages: req.messages.len() as u64 + u64::from(req.system.is_some()),
            tools: !req.tools.is_empty(),
        }
    }

    /// The template's tokens around the text, plus the images.
    pub fn overhead(&self) -> u64 {
        BASE_TEMPLATE_TOKENS
            + MESSAGE_TEMPLATE_TOKENS * self.messages
            + if self.tools { TOOLS_TEMPLATE_TOKENS } else { 0 }
            + IMAGE_TOKENS * self.images
    }

    /// The quick upper estimate of the prompt (text bound plus overhead).
    pub fn quick_tokens(&self) -> u64 {
        quick_text_tokens(&self.text).ceil() as u64 + self.overhead()
    }
}

/// The request's own generation limit (`max_tokens`, or OpenAI's
/// `max_completion_tokens`), if it sets one.
pub fn requested_max_tokens(req: &AiRequest) -> Option<u64> {
    req.max_output_tokens.map(u64::from).or_else(|| {
        ["max_tokens", "max_completion_tokens"]
            .iter()
            .find_map(|key| {
                req.passthrough
                    .get(*key)
                    .and_then(serde_json::Value::as_u64)
            })
    })
}

/// How a prompt's tokens were had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptSource {
    /// Counted by the engine's tokenizer (plus the template overhead).
    Engine,
    /// The quick bound.
    Estimate,
}

impl PromptSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Engine => "engine",
            Self::Estimate => "estimate",
        }
    }
}

/// What one request reserves in a pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolNeed {
    pub prompt: u64,
    pub prompt_source: PromptSource,
    pub reserve: u64,
    /// The reserve is the request's own `max_tokens` (else the lane's
    /// default).
    pub reserve_from_request: bool,
    /// Tokens reserved: `prompt + reserve`, capped at what one request may
    /// use.
    pub need: u64,
}

/// A request that alone needs more than one request may use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TooLarge {
    pub prompt: u64,
    pub prompt_source: PromptSource,
    pub per_request: u64,
}

/// The need of a request with `prompt` tokens (from `source`) and generation
/// limit `max_tokens`, in a pool `truth` (`None` = unknown: nothing is
/// capped or refused). The reserve is `max_tokens`, else `default_reserve`,
/// capped at what is left of one request's context after the prompt; the
/// need is capped at one request's context, so a request alone in an empty
/// pool always fits. Refused only when the prompt alone fills one request's
/// context: by an engine count, or by the quick bound even at its largest
/// overcount.
pub fn need_for(
    prompt: u64,
    source: PromptSource,
    max_tokens: Option<u64>,
    default_reserve: u64,
    truth: Option<&PoolTruth>,
) -> Result<PoolNeed, TooLarge> {
    let asked = max_tokens.unwrap_or(default_reserve);
    let Some(truth) = truth else {
        return Ok(PoolNeed {
            prompt,
            prompt_source: source,
            reserve: asked,
            reserve_from_request: max_tokens.is_some(),
            need: prompt.saturating_add(asked),
        });
    };
    let per_request = truth.per_request_tokens.min(truth.pool_tokens).max(1);
    let floor = match source {
        PromptSource::Engine => prompt,
        PromptSource::Estimate => (prompt as f64 / QUICK_MAX_OVERCOUNT) as u64,
    };
    if floor >= per_request {
        return Err(TooLarge {
            prompt,
            prompt_source: source,
            per_request,
        });
    }
    let reserve = asked.min(per_request.saturating_sub(prompt));
    Ok(PoolNeed {
        prompt,
        prompt_source: source,
        reserve,
        reserve_from_request: max_tokens.is_some(),
        need: prompt.saturating_add(reserve).clamp(1, per_request),
    })
}

// ---------------------------------------------------------------- clock

/// Time for the pool's waits: real (tokio) in the gateway, moved by hand in
/// tests.
pub trait PoolClock: Send + Sync {
    fn now_ms(&self) -> u64;
    fn sleep_ms(&self, ms: u64) -> BoxFuture<'_, ()>;
}

/// Tokio's clock.
pub struct TokioClock {
    origin: tokio::time::Instant,
}

impl Default for TokioClock {
    fn default() -> Self {
        Self {
            origin: tokio::time::Instant::now(),
        }
    }
}

impl PoolClock for TokioClock {
    fn now_ms(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }
    fn sleep_ms(&self, ms: u64) -> BoxFuture<'_, ()> {
        Box::pin(tokio::time::sleep(std::time::Duration::from_millis(ms)))
    }
}

// ---------------------------------------------------------------- pool

/// One engine model's pool: its size, what admitted requests reserved, and
/// the FIFO queue of requests waiting for room. Kept across config reloads,
/// so a reload that changes the size keeps the reservations of requests
/// already running (they are never cut) and the order of the queue.
pub struct TokenPool {
    key: String,
    state: Mutex<PoolState>,
    /// Bumped on every change (a reservation given back, a waiter gone, a
    /// new size), so waiters re-check without polling.
    changed: tokio::sync::watch::Sender<u64>,
}

#[derive(Debug, Default)]
struct PoolState {
    truth: Option<PoolTruth>,
    reserved: u64,
    running: u32,
    /// Waiters in arrival order: (ticket, need).
    queue: VecDeque<(u64, u64)>,
    next_ticket: u64,
    admitted_total: u64,
    waited_total: u64,
    last_wait_ms: Option<u64>,
    max_wait_ms: u64,
    refused_total: u64,
    timed_out_total: u64,
    unchecked_total: u64,
}

impl PoolState {
    /// Room for `need` now: an unknown size has room; an empty pool always
    /// has room (a request alone is never starved); otherwise the
    /// reservations plus `need` must fit.
    fn fits(&self, need: u64) -> bool {
        match self.truth {
            None => true,
            Some(truth) => {
                self.running == 0 || self.reserved.saturating_add(need) <= truth.pool_tokens
            }
        }
    }

    fn queued_tokens(&self) -> u64 {
        self.queue.iter().map(|(_, need)| need).sum()
    }
}

/// Tokens reserved in a pool. Dropping it gives them back and wakes the
/// queue: on success, error, client disconnect, stream cut, panic unwind.
pub struct PoolReservation {
    pool: Arc<TokenPool>,
    need: u64,
}

impl std::fmt::Debug for PoolReservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PoolReservation({} tokens of {})",
            self.need, self.pool.key
        )
    }
}

impl PoolReservation {
    pub fn tokens(&self) -> u64 {
        self.need
    }
}

impl Drop for PoolReservation {
    fn drop(&mut self) {
        {
            let mut state = self.pool.lock();
            state.reserved = state.reserved.saturating_sub(self.need);
            state.running = state.running.saturating_sub(1);
        }
        self.pool.bump();
    }
}

/// A waiter's place in the queue; dropping it (the waiter went away or its
/// wait ran out) takes it out so the next one moves up.
struct Ticket {
    pool: Arc<TokenPool>,
    id: u64,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let removed = {
            let mut state = self.pool.lock();
            let before = state.queue.len();
            state.queue.retain(|(ticket, _)| *ticket != self.id);
            state.queue.len() != before
        };
        if removed {
            self.pool.bump();
        }
    }
}

/// Why a wait for room ended without it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolTimeout {
    pub waited_ms: u64,
    pub reserved: u64,
    pub pool_tokens: Option<u64>,
}

/// The pool was full (or others waited) and the request did not wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolFull {
    pub reserved: u64,
    pub waiting: u32,
    pub pool_tokens: Option<u64>,
}

impl TokenPool {
    fn new(key: String) -> Arc<Self> {
        Arc::new(Self {
            key,
            state: Mutex::new(PoolState::default()),
            changed: tokio::sync::watch::channel(0).0,
        })
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    fn lock(&self) -> MutexGuard<'_, PoolState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn bump(&self) {
        self.changed
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    /// The size the engine (or the settings) gives now. A smaller pool
    /// never cuts admitted work: new requests wait until enough of it ends.
    pub fn set_truth(&self, truth: Option<PoolTruth>) {
        let changed = {
            let mut state = self.lock();
            let changed = state.truth != truth;
            state.truth = truth;
            changed
        };
        if changed {
            self.bump();
        }
    }

    pub fn truth(&self) -> Option<PoolTruth> {
        self.lock().truth
    }

    fn reservation(self: &Arc<Self>, state: &mut PoolState, need: u64) -> PoolReservation {
        state.reserved = state.reserved.saturating_add(need);
        state.running += 1;
        state.admitted_total += 1;
        PoolReservation {
            pool: self.clone(),
            need,
        }
    }

    /// Reserve `need` without waiting: only with room and nobody waiting (a
    /// newcomer never jumps the queue).
    pub fn try_reserve(self: &Arc<Self>, need: u64) -> Result<PoolReservation, PoolFull> {
        let mut state = self.lock();
        if state.queue.is_empty() && state.fits(need) {
            if state.truth.is_none() {
                state.unchecked_total += 1;
            }
            return Ok(self.reservation(&mut state, need));
        }
        Err(PoolFull {
            reserved: state.reserved,
            waiting: u32::try_from(state.queue.len()).unwrap_or(u32::MAX),
            pool_tokens: state.truth.map(|truth| truth.pool_tokens),
        })
    }

    /// Reserve `need`, waiting in the FIFO queue up to `wait_ms` on `clock`.
    /// The first waiter is let in once its need fits; the ones behind it
    /// wait their turn (a large request is never starved by small ones).
    /// `truth` is read again every second and on every change, so a size
    /// change reaches waiters. Dropping the future leaves the queue.
    pub async fn reserve(
        self: &Arc<Self>,
        need: u64,
        wait_ms: u64,
        clock: &dyn PoolClock,
        truth: &(dyn Fn() -> Option<PoolTruth> + Send + Sync),
    ) -> Result<(PoolReservation, u64), PoolTimeout> {
        if let Ok(reservation) = self.try_reserve(need) {
            return Ok((reservation, 0));
        }
        let mut changes = self.changed.subscribe();
        let ticket = {
            let mut state = self.lock();
            let id = state.next_ticket;
            state.next_ticket = state.next_ticket.wrapping_add(1);
            state.queue.push_back((id, need));
            Ticket {
                pool: self.clone(),
                id,
            }
        };
        let started = clock.now_ms();
        loop {
            changes.borrow_and_update();
            self.set_truth(truth());
            changes.borrow_and_update();
            let waited = clock.now_ms().saturating_sub(started);
            {
                let mut state = self.lock();
                let first = state.queue.front().map(|(id, _)| *id) == Some(ticket.id);
                if first && state.fits(need) {
                    state.queue.pop_front();
                    if state.truth.is_none() {
                        state.unchecked_total += 1;
                    }
                    let reservation = self.reservation(&mut state, need);
                    state.waited_total += 1;
                    state.last_wait_ms = Some(waited);
                    state.max_wait_ms = state.max_wait_ms.max(waited);
                    drop(state);
                    // Already out of the queue; the next waiter may fit too.
                    drop(ticket);
                    self.bump();
                    return Ok((reservation, waited));
                }
                if waited >= wait_ms {
                    state.timed_out_total += 1;
                    let timeout = PoolTimeout {
                        waited_ms: waited,
                        reserved: state.reserved,
                        pool_tokens: state.truth.map(|truth| truth.pool_tokens),
                    };
                    drop(state);
                    drop(ticket);
                    return Err(timeout);
                }
            }
            let nap = (wait_ms - waited).min(TRUTH_RECHECK_MS);
            tokio::select! {
                _ = changes.changed() => {}
                () = clock.sleep_ms(nap) => {}
            }
        }
    }

    fn count_refused(&self) {
        self.lock().refused_total += 1;
    }

    pub fn report(&self) -> PoolReport {
        let state = self.lock();
        let (lane, model) = self
            .key
            .split_once('/')
            .map_or((self.key.clone(), String::new()), |(lane, model)| {
                (lane.to_string(), model.to_string())
            });
        PoolReport {
            lane,
            model,
            pool_tokens: state.truth.map(|truth| truth.pool_tokens),
            per_request_tokens: state.truth.map(|truth| truth.per_request_tokens),
            slots: state.truth.and_then(|truth| truth.slots),
            kv_unified: state.truth.and_then(|truth| truth.kv_unified),
            source: state.truth.map(|truth| truth.source),
            reserved_tokens: state.reserved,
            running: state.running,
            waiting: u32::try_from(state.queue.len()).unwrap_or(u32::MAX),
            waiting_tokens: state.queued_tokens(),
            admitted_total: state.admitted_total,
            waited_total: state.waited_total,
            last_wait_ms: state.last_wait_ms,
            max_wait_ms: state.max_wait_ms,
            refused_total: state.refused_total,
            timed_out_total: state.timed_out_total,
            unchecked_total: state.unchecked_total,
        }
    }
}

/// One pool as `GET /v1/workloads/capacity` and `sb ai status` show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PoolReport {
    pub lane: String,
    pub model: String,
    /// The pool's size (`null` = not known: requests go unchecked).
    pub pool_tokens: Option<u64>,
    pub per_request_tokens: Option<u64>,
    pub slots: Option<u64>,
    pub kv_unified: Option<bool>,
    /// `engine`, `preset` or `settings`.
    pub source: Option<PoolSource>,
    /// Tokens the requests running now reserved.
    pub reserved_tokens: u64,
    pub running: u32,
    /// Requests waiting for room, and the tokens they need.
    pub waiting: u32,
    pub waiting_tokens: u64,
    pub admitted_total: u64,
    /// Requests that waited before they were sent.
    pub waited_total: u64,
    pub last_wait_ms: Option<u64>,
    pub max_wait_ms: u64,
    /// Requests refused (400) because alone they need more than one
    /// request may use.
    pub refused_total: u64,
    /// Requests whose wait ran out.
    pub timed_out_total: u64,
    /// Requests sent while the size was not known.
    pub unchecked_total: u64,
}

/// Every pool of the gateway, by `provider/model`. State, not config: kept
/// across reloads.
pub struct TokenPools {
    pools: Mutex<BTreeMap<String, Arc<TokenPool>>>,
    view: OnceLock<Arc<dyn PoolView>>,
    clock: OnceLock<Arc<dyn PoolClock>>,
}

impl Default for TokenPools {
    fn default() -> Self {
        Self {
            pools: Mutex::new(BTreeMap::new()),
            view: OnceLock::new(),
            clock: OnceLock::new(),
        }
    }
}

impl TokenPools {
    /// The engine side (set once at start-up).
    pub fn set_view(&self, view: Arc<dyn PoolView>) {
        let _ = self.view.set(view);
    }

    /// The clock for waits (tests); tokio's when unset.
    pub fn set_clock(&self, clock: Arc<dyn PoolClock>) {
        let _ = self.clock.set(clock);
    }

    pub(crate) fn view(&self) -> Option<&Arc<dyn PoolView>> {
        self.view.get()
    }

    pub(crate) fn clock(&self) -> Arc<dyn PoolClock> {
        self.clock
            .get_or_init(|| Arc::new(TokioClock::default()))
            .clone()
    }

    /// The pool of `key` (`provider/model`), made on first use.
    pub fn pool(&self, key: &str) -> Arc<TokenPool> {
        self.pools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(key.to_string())
            .or_insert_with(|| TokenPool::new(key.to_string()))
            .clone()
    }

    /// Every pool, ordered by lane and model.
    pub fn reports(&self) -> Vec<PoolReport> {
        let pools: Vec<Arc<TokenPool>> = self
            .pools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect();
        pools.iter().map(|pool| pool.report()).collect()
    }
}

// ---------------------------------------------------------------- admission

/// What admission did for one attempt.
pub(crate) enum PoolStep {
    /// The lane has no pool admission: sent as before.
    Off,
    /// Admitted: the reservation is held until the answer or stream ends.
    Admitted {
        reservation: PoolReservation,
        note: sb_trace::PoolAdmissionTrace,
    },
    /// Alone too large for one request: never sent (400 when nothing else
    /// can serve).
    TooLarge(String),
    /// No room and the request did not wait: skipped (spilled).
    Full(String),
    /// The wait for room ran out.
    TimedOut(String),
}

/// `12,345`.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn secs(ms: u64) -> String {
    format!("{:.1} s", ms as f64 / 1000.0)
}

/// The lane's admission settings for `provider_id`, when it is on.
pub(crate) fn lane_admission(
    snap: &crate::Snapshot,
    provider_id: &str,
) -> Option<(PoolAdmissionConfig, u64)> {
    let lane = snap
        .config
        .local_executors
        .iter()
        .find(|lane| lane.name == provider_id)?;
    Some((lane.pool_admission()?, lane.queue_timeout_secs))
}

impl crate::Engine {
    /// Every token pool (the capacity report and tests).
    pub fn token_pools(&self) -> Arc<TokenPools> {
        self.token_pools.clone()
    }

    /// Reserve the need of `req` (the request as sent to `target`) in the
    /// pool of engine model `engine_model`. Waits (FIFO, bounded by the
    /// lane's `queue_timeout_secs`) only when `may_wait`; otherwise a full
    /// pool is a skip. See the module documentation.
    pub(crate) async fn pool_admission(
        &self,
        snap: &crate::Snapshot,
        req: &AiRequest,
        provider_id: &str,
        engine_model: &str,
        may_wait: bool,
    ) -> PoolStep {
        let Some((config, queue_timeout_secs)) = lane_admission(snap, provider_id) else {
            return PoolStep::Off;
        };
        let key = format!("{provider_id}/{engine_model}");
        let view = self.token_pools.view().cloned();
        let truth = match &view {
            Some(view) => view.fresh(provider_id, engine_model).await,
            None => None,
        };
        let prompt_text = PromptText::of(req);
        let quick = prompt_text.quick_tokens();
        // Count a large prompt with the engine's own tokenizer, but only
        // for a model the engine lists loaded (a model not loaded yet would
        // be loaded by the count itself).
        let (prompt, source) = match (&view, truth) {
            (Some(view), Some(known))
                if known.source == PoolSource::Engine
                    && quick >= u64::from(config.count_above_tokens) =>
            {
                let overhead = prompt_text.overhead();
                match view
                    .count_tokens(provider_id, engine_model, prompt_text.text)
                    .await
                {
                    Some(counted) => (counted.saturating_add(overhead), PromptSource::Engine),
                    None => (quick, PromptSource::Estimate),
                }
            }
            _ => (quick, PromptSource::Estimate),
        };
        let pool = self.token_pools.pool(&key);
        pool.set_truth(truth);
        let need = match need_for(
            prompt,
            source,
            requested_max_tokens(req),
            u64::from(config.default_reserve_tokens),
            truth.as_ref(),
        ) {
            Ok(need) => need,
            Err(too_large) => {
                pool.count_refused();
                let how = match too_large.prompt_source {
                    PromptSource::Engine => "counted by the engine",
                    PromptSource::Estimate => "estimated",
                };
                return PoolStep::TooLarge(format!(
                    "the prompt is {} tokens ({how}); one request to `{key}` may use at most {} \
                     tokens of context",
                    grouped(too_large.prompt),
                    grouped(too_large.per_request)
                ));
            }
        };
        let note = |waited_ms: u64| sb_trace::PoolAdmissionTrace {
            pool: key.clone(),
            need_tokens: need.need,
            prompt_tokens: need.prompt,
            prompt_source: need.prompt_source.as_str().to_string(),
            reserve_tokens: need.reserve,
            reserve_source: if need.reserve_from_request {
                "max_tokens"
            } else {
                "default"
            }
            .to_string(),
            pool_tokens: truth.map(|truth| truth.pool_tokens),
            waited_ms,
        };
        match pool.try_reserve(need.need) {
            Ok(reservation) => {
                return PoolStep::Admitted {
                    reservation,
                    note: note(0),
                }
            }
            Err(full) if !may_wait => {
                return PoolStep::Full(format!(
                    "token pool of `{key}` is full ({} of {} tokens reserved, {} waiting; this \
                     request needs {})",
                    grouped(full.reserved),
                    full.pool_tokens.map_or_else(|| "?".to_string(), grouped),
                    full.waiting,
                    grouped(need.need)
                ));
            }
            Err(_) => {}
        }
        let clock = self.token_pools.clock();
        let (provider, model) = (provider_id.to_string(), engine_model.to_string());
        let recheck = move || {
            view.as_ref()
                .and_then(|view| view.cached(&provider, &model))
        };
        match pool
            .reserve(
                need.need,
                queue_timeout_secs.saturating_mul(1000),
                &*clock,
                &recheck,
            )
            .await
        {
            Ok((reservation, waited)) => {
                tracing::info!(
                    pool = %key,
                    need = need.need,
                    waited_ms = waited,
                    "token pool: waited for room"
                );
                PoolStep::Admitted {
                    reservation,
                    note: note(waited),
                }
            }
            Err(timeout) => {
                tracing::warn!(
                    pool = %key,
                    need = need.need,
                    waited_ms = timeout.waited_ms,
                    reserved = timeout.reserved,
                    "token pool stayed full for the whole queue timeout"
                );
                PoolStep::TimedOut(format!(
                    "waited {} for room in the token pool of `{key}` and it is still full ({} of \
                     {} tokens reserved; this request needs {})",
                    secs(timeout.waited_ms),
                    grouped(timeout.reserved),
                    timeout.pool_tokens.map_or_else(|| "?".to_string(), grouped),
                    grouped(need.need)
                ))
            }
        }
    }
}

/// The decision/trace line for an admission.
pub(crate) fn admitted_line(note: &sb_trace::PoolAdmissionTrace) -> String {
    let pool = note.pool_tokens.map_or_else(
        || "size unknown, sent unchecked".to_string(),
        |size| format!("pool {}", grouped(size)),
    );
    let waited = if note.waited_ms > 0 {
        format!("; waited {} for room", secs(note.waited_ms))
    } else {
        String::new()
    };
    format!(
        "token pool `{}`: reserved {} tokens (prompt {} by {}, reserve {} from {}; {pool}){waited}",
        note.pool,
        grouped(note.need_tokens),
        grouped(note.prompt_tokens),
        note.prompt_source,
        grouped(note.reserve_tokens),
        note.reserve_source,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Moves only when the test moves it; `sleep_ms` waits for that.
    struct ManualClock {
        now: AtomicU64,
        moved: tokio::sync::watch::Sender<u64>,
    }

    impl ManualClock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                now: AtomicU64::new(0),
                moved: tokio::sync::watch::channel(0).0,
            })
        }
        fn advance(&self, ms: u64) {
            self.now.fetch_add(ms, Ordering::SeqCst);
            self.moved.send_modify(|version| *version += 1);
        }
    }

    impl PoolClock for ManualClock {
        fn now_ms(&self) -> u64 {
            self.now.load(Ordering::SeqCst)
        }
        fn sleep_ms(&self, ms: u64) -> BoxFuture<'_, ()> {
            Box::pin(async move {
                let deadline = self.now_ms().saturating_add(ms);
                let mut moved = self.moved.subscribe();
                while self.now_ms() < deadline {
                    if moved.changed().await.is_err() {
                        return;
                    }
                }
            })
        }
    }

    async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }

    fn unified(pool: u64) -> PoolTruth {
        PoolTruth {
            pool_tokens: pool,
            per_request_tokens: pool,
            slots: Some(3),
            kv_unified: Some(true),
            source: PoolSource::Engine,
        }
    }

    fn waiter(
        pool: &Arc<TokenPool>,
        clock: &Arc<ManualClock>,
        need: u64,
        truth: Option<PoolTruth>,
    ) -> tokio::task::JoinHandle<Result<(PoolReservation, u64), PoolTimeout>> {
        let (pool, clock) = (pool.clone(), clock.clone());
        tokio::spawn(async move { pool.reserve(need, 60_000, &*clock, &move || truth).await })
    }

    // Three requests of ~41K tokens in a pool of 98,304: two fit together;
    // the third waits and runs once one of them ends. A request counter
    // would have let all three in.
    #[tokio::test]
    async fn the_third_large_request_waits_until_one_ends() {
        let clock = ManualClock::new();
        let pool = TokenPool::new("text/qwen".into());
        let truth = Some(unified(98_304));
        pool.set_truth(truth);
        let a = pool.try_reserve(41_000).expect("room for one");
        let b = pool.try_reserve(41_000).expect("room for two");
        assert!(
            pool.try_reserve(41_000).is_err(),
            "82,000 + 41,000 > 98,304"
        );
        let c = waiter(&pool, &clock, 41_000, truth);
        settle().await;
        assert!(!c.is_finished());
        assert_eq!(pool.report().waiting, 1);
        assert_eq!(pool.report().reserved_tokens, 82_000);
        clock.advance(21_400);
        settle().await;
        assert!(!c.is_finished(), "time alone frees nothing");
        drop(a);
        let (c, waited) = c.await.unwrap().expect("in once one ended");
        assert_eq!(waited, 21_400);
        let report = pool.report();
        assert_eq!(
            (report.reserved_tokens, report.running, report.waiting),
            (82_000, 2, 0)
        );
        assert_eq!(
            (report.waited_total, report.last_wait_ms),
            (1, Some(21_400))
        );
        drop((b, c));
        assert_eq!(pool.report().reserved_tokens, 0);
    }

    // FIFO: a large request at the head is not passed by small ones that
    // would fit (it is never starved).
    #[tokio::test]
    async fn waiters_get_room_in_arrival_order() {
        let clock = ManualClock::new();
        let pool = TokenPool::new("text/m".into());
        let truth = Some(unified(100));
        pool.set_truth(truth);
        let held = pool.try_reserve(60).unwrap();
        let big = waiter(&pool, &clock, 70, truth);
        settle().await;
        let small = waiter(&pool, &clock, 10, truth);
        settle().await;
        assert!(!small.is_finished(), "10 would fit, but `big` waits first");
        assert!(
            pool.try_reserve(10).is_err(),
            "a newcomer never jumps the queue"
        );
        drop(held);
        let (big, _) = big.await.unwrap().unwrap();
        let (small, _) = small.await.unwrap().unwrap();
        assert_eq!(pool.report().reserved_tokens, 80);
        drop((big, small));
    }

    // A waiter whose client goes away leaves the queue at once; the one
    // behind it moves up; a running request's reservation comes back when
    // its task ends any way at all (here: aborted, like a cut stream).
    #[tokio::test]
    async fn disconnects_give_reservations_back() {
        let clock = ManualClock::new();
        let pool = TokenPool::new("text/m".into());
        let truth = Some(unified(100));
        pool.set_truth(truth);
        let held = pool.try_reserve(90).unwrap();
        let running = tokio::spawn(async move {
            let _held = held;
            std::future::pending::<()>().await;
        });
        let gone = waiter(&pool, &clock, 50, truth);
        settle().await;
        let next = waiter(&pool, &clock, 50, truth);
        settle().await;
        assert_eq!(pool.report().waiting, 2);
        gone.abort();
        settle().await;
        assert_eq!(pool.report().waiting, 1, "the waiter that went away left");
        running.abort();
        let (next, _) = next
            .await
            .unwrap()
            .expect("room once the running one ended");
        assert_eq!(pool.report().reserved_tokens, 50);
        drop(next);
        let report = pool.report();
        assert_eq!(
            (report.reserved_tokens, report.running, report.waiting),
            (0, 0, 0)
        );
    }

    // A panic while holding a reservation gives it back while unwinding.
    #[tokio::test]
    async fn a_panicking_holder_gives_its_tokens_back() {
        let pool = TokenPool::new("text/m".into());
        pool.set_truth(Some(unified(100)));
        let held = pool.try_reserve(100).unwrap();
        let task = tokio::spawn(async move {
            let _held = held;
            panic!("a bug while holding tokens");
        });
        assert!(task.await.unwrap_err().is_panic());
        assert_eq!(pool.report().reserved_tokens, 0);
    }

    // A wait that runs out says how long it waited and leaves the queue.
    #[tokio::test]
    async fn a_wait_that_runs_out_leaves_the_queue() {
        let clock = ManualClock::new();
        let pool = TokenPool::new("text/m".into());
        let truth = Some(unified(100));
        pool.set_truth(truth);
        let _held = pool.try_reserve(100).unwrap();
        let short = {
            let (pool, clock) = (pool.clone(), clock.clone());
            tokio::spawn(async move { pool.reserve(10, 5_000, &*clock, &move || truth).await })
        };
        settle().await;
        for _ in 0..5 {
            clock.advance(1_000);
            settle().await;
        }
        let timeout = short.await.unwrap().expect_err("ran out");
        assert_eq!(timeout.waited_ms, 5_000);
        assert_eq!(timeout.reserved, 100);
        let report = pool.report();
        assert_eq!((report.waiting, report.timed_out_total), (0, 1));
    }

    // A reload that shrinks the pool cuts nothing admitted; new requests
    // see the new size. One that grows it lets a waiter in without any
    // release (the waiter reads the size again).
    #[tokio::test]
    async fn a_new_size_applies_to_new_requests_only() {
        let clock = ManualClock::new();
        let pool = TokenPool::new("text/m".into());
        pool.set_truth(Some(unified(100)));
        let a = pool.try_reserve(40).unwrap();
        let b = pool.try_reserve(40).unwrap();
        pool.set_truth(Some(unified(50)));
        assert_eq!(
            pool.report().reserved_tokens,
            80,
            "nothing admitted was cut"
        );
        assert!(pool.try_reserve(10).is_err(), "80 + 10 > 50");
        drop(a);
        assert!(pool.try_reserve(20).is_err(), "40 + 20 > 50");
        let fits = pool.try_reserve(10).expect("40 + 10 <= 50");
        drop(fits);
        let grown = Arc::new(Mutex::new(Some(unified(50))));
        let reader = grown.clone();
        let waiting = {
            let (pool, clock) = (pool.clone(), clock.clone());
            tokio::spawn(async move {
                pool.reserve(30, 60_000, &*clock, &move || *reader.lock().unwrap())
                    .await
            })
        };
        settle().await;
        assert!(!waiting.is_finished());
        *grown.lock().unwrap() = Some(unified(200));
        clock.advance(TRUTH_RECHECK_MS);
        let (grown_in, _) = waiting.await.unwrap().expect("the larger pool has room");
        assert_eq!(pool.truth().unwrap().pool_tokens, 200);
        drop((b, grown_in));
    }

    // A request alone in an empty pool is always admitted, even when the
    // pool is smaller than its (already capped) need.
    #[test]
    fn an_empty_pool_always_admits() {
        let pool = TokenPool::new("text/m".into());
        pool.set_truth(Some(unified(100)));
        let alone = pool.try_reserve(150).expect("alone in an empty pool");
        assert!(pool.try_reserve(1).is_err());
        drop(alone);
    }

    // Size unknown: admitted unchecked, but counted so that a size learned
    // later sees it.
    #[test]
    fn an_unknown_size_admits_and_counts() {
        let pool = TokenPool::new("text/m".into());
        let a = pool.try_reserve(70).unwrap();
        let b = pool.try_reserve(70).unwrap();
        assert_eq!(pool.report().unchecked_total, 2);
        pool.set_truth(Some(unified(100)));
        assert!(
            pool.try_reserve(10).is_err(),
            "the 140 sent unchecked still count"
        );
        drop((a, b));
        assert!(pool.try_reserve(10).is_ok());
    }

    #[test]
    fn need_caps_the_reserve_and_refuses_only_a_prompt_too_large_alone() {
        let truth = unified(98_304);
        // max_tokens sent: prompt + max_tokens.
        let need = need_for(
            40_000,
            PromptSource::Engine,
            Some(3_072),
            8_192,
            Some(&truth),
        )
        .unwrap();
        assert_eq!(
            (need.need, need.reserve, need.reserve_from_request),
            (43_072, 3_072, true)
        );
        // No max_tokens: the default reserve.
        let need = need_for(40_000, PromptSource::Engine, None, 8_192, Some(&truth)).unwrap();
        assert_eq!((need.need, need.reserve_from_request), (48_192, false));
        // A reserve larger than what is left is capped at it: never more
        // than one request's context, so alone it always fits.
        let need = need_for(95_000, PromptSource::Engine, None, 8_192, Some(&truth)).unwrap();
        assert_eq!((need.need, need.reserve), (98_304, 3_304));
        let need = need_for(
            1_000,
            PromptSource::Engine,
            Some(500_000),
            8_192,
            Some(&truth),
        )
        .unwrap();
        assert_eq!(need.need, 98_304);
        // A counted prompt that fills one request's context: refused.
        let refused = need_for(98_304, PromptSource::Engine, Some(1), 8_192, Some(&truth));
        assert_eq!(refused.unwrap_err().per_request, 98_304);
        // An estimated one only when it is too large even at the bound's
        // largest overcount; otherwise it runs capped (alone).
        let need = need_for(150_000, PromptSource::Estimate, None, 8_192, Some(&truth)).unwrap();
        assert_eq!(need.need, 98_304);
        assert!(need_for(400_000, PromptSource::Estimate, None, 8_192, Some(&truth)).is_err());
        // Without kv-unified one request may use only its slot's share.
        let split = PoolTruth {
            pool_tokens: 98_304,
            per_request_tokens: 32_768,
            slots: Some(3),
            kv_unified: Some(false),
            source: PoolSource::Settings,
        };
        assert!(need_for(40_000, PromptSource::Engine, None, 8_192, Some(&split)).is_err());
        // Unknown size: nothing capped or refused.
        let need = need_for(400_000, PromptSource::Engine, None, 8_192, None).unwrap();
        assert_eq!(need.need, 408_192);
    }

    #[test]
    fn the_quick_bound_does_not_undercount_hard_text() {
        // Hand-checked against the Qwen tokenizer: each sample's real count
        // is at most the bound.
        let cases: &[(&str, u64)] = &[
            // 32 hex digits: 28 tokens.
            ("3f9a0c7e5b1d2468ace0bdf13579aceb", 28),
            // A UUID: 25 tokens.
            ("8c1f6c2e-4d4b-4f0a-9a57-2f1d3b6e9c01", 25),
            // English: 12 tokens.
            (
                "Hello, can you help me write a function that adds two numbers?",
                14,
            ),
            // Chinese: 9 characters, 7 tokens.
            ("今天天气很好我们去", 7),
            // Digits are one token each.
            ("31415926", 8),
        ];
        for (text, real) in cases {
            let bound = quick_text_tokens(text);
            assert!(
                bound >= *real as f64 * 0.99,
                "{text:?}: bound {bound} < real {real}"
            );
        }
        // Prose is not wildly over-counted (about 1.4x measured).
        let prose = "The quick brown fox jumps over the lazy dog. ".repeat(200);
        let words = 9.0 * 200.0 + 200.0;
        assert!(quick_text_tokens(&prose) < words * 2.0);
    }

    #[test]
    fn the_prompt_text_holds_every_part_and_the_template_overhead() {
        use sb_core::{Message, ToolSpec};
        let mut req = AiRequest::new("text/m", vec![Message::user("hello there")]);
        req.system = Some("be brief".into());
        req.tools.push(ToolSpec {
            name: "lookup".into(),
            description: Some("find a thing".into()),
            parameters: serde_json::json!({"type": "object"}),
            defer_loading: false,
        });
        let text = PromptText::of(&req);
        for piece in [
            "be brief",
            "hello there",
            "lookup",
            "find a thing",
            "object",
        ] {
            assert!(text.text.contains(piece), "{piece} missing");
        }
        assert_eq!(text.messages, 2);
        assert_eq!(
            text.overhead(),
            BASE_TEMPLATE_TOKENS + 2 * MESSAGE_TEMPLATE_TOKENS + TOOLS_TEMPLATE_TOKENS
        );
        assert_eq!(requested_max_tokens(&req), None);
        req.passthrough
            .insert("max_completion_tokens".into(), serde_json::json!(77));
        assert_eq!(requested_max_tokens(&req), Some(77));
        req.max_output_tokens = Some(55);
        assert_eq!(requested_max_tokens(&req), Some(55));
    }
}
