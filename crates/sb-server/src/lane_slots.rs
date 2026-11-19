//! Places under a lane's `max_in_flight`, handed out oldest waiter first.
//!
//! A lane with a limit serves at most that many requests and media jobs at
//! once. Work past it waits here in a FIFO queue, bounded by the lane's
//! `queue_timeout_secs` on the activator's [`Clock`] seam. A waiter that goes
//! away (client disconnect, cancelled job, dropped future) leaves the queue
//! when its future is dropped; a held place is given back when its
//! [`SlotPermit`] is dropped, on every path: success, error, panic unwinding
//! or task abort. A lane without a limit hands out places at once and only
//! counts them.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;

use crate::activator::Clock;

/// One lane's places and queue. Kept across config reloads (the lane keeps
/// its executor), so a reload that changes `max_in_flight` keeps the work
/// that holds a place and the order of the queue.
pub(crate) struct LaneSlots {
    state: Mutex<SlotState>,
    /// Bumped on every change (a place given back, a waiter gone, a new
    /// limit), so waiters re-check without polling.
    changed: tokio::sync::watch::Sender<u64>,
}

#[derive(Debug)]
struct SlotState {
    limit: Option<u32>,
    in_use: u32,
    queue: VecDeque<u64>,
    next_ticket: u64,
}

impl SlotState {
    fn has_room(&self) -> bool {
        self.limit.is_none_or(|limit| self.in_use < limit)
    }
}

/// Counts for the capacity report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SlotCounts {
    pub limit: Option<u32>,
    pub in_use: u32,
    pub queued: u32,
}

/// The lane's limit and queue as `GET /v1/workloads/capacity` shows them.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LaneSlotsReport {
    /// The configured `max_in_flight`; `null` = no limit.
    pub max_in_flight: Option<u32>,
    /// Requests and media jobs holding a place now.
    pub in_use: u32,
    /// Requests and media jobs waiting in the lane's queue for a place.
    pub queued: u32,
    /// `spill` or `queue` (see `on_full`).
    pub on_full: &'static str,
    pub queue_timeout_secs: u64,
}

/// Why a wait for a place ended without one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SlotTimeout {
    pub waited_ms: u64,
    pub counts: SlotCounts,
}

/// A place under the limit. Dropping it gives the place back and wakes the
/// queue.
pub(crate) struct SlotPermit {
    slots: Arc<LaneSlots>,
}

impl std::fmt::Debug for SlotPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SlotPermit")
    }
}

impl Drop for SlotPermit {
    fn drop(&mut self) {
        {
            let mut state = self.slots.lock();
            state.in_use = state.in_use.saturating_sub(1);
        }
        self.slots.bump();
    }
}

/// A waiter's place in the queue; dropping it (the waiter went away, or its
/// wait ran out) takes it out of the queue so the next one moves up.
struct Ticket {
    slots: Arc<LaneSlots>,
    id: u64,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let removed = {
            let mut state = self.slots.lock();
            let before = state.queue.len();
            state.queue.retain(|ticket| *ticket != self.id);
            state.queue.len() != before
        };
        if removed {
            self.slots.bump();
        }
    }
}

impl LaneSlots {
    pub(crate) fn new(limit: Option<u32>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(SlotState {
                limit,
                in_use: 0,
                queue: VecDeque::new(),
                next_ticket: 0,
            }),
            changed: tokio::sync::watch::channel(0).0,
        })
    }

    fn lock(&self) -> MutexGuard<'_, SlotState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn bump(&self) {
        self.changed
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    /// A reload changed `max_in_flight`. Work holding a place keeps it; a
    /// lower limit only makes new work wait until enough of it ends.
    pub(crate) fn set_limit(&self, limit: Option<u32>) {
        let changed = {
            let mut state = self.lock();
            let changed = state.limit != limit;
            state.limit = limit;
            changed
        };
        if changed {
            self.bump();
        }
    }

    pub(crate) fn counts(&self) -> SlotCounts {
        let state = self.lock();
        SlotCounts {
            limit: state.limit,
            in_use: state.in_use,
            queued: u32::try_from(state.queue.len()).unwrap_or(u32::MAX),
        }
    }

    /// `Some(counts)` when new work would have to wait: the limit is reached
    /// or others already wait (they go first). `None` without a limit.
    pub(crate) fn full(&self) -> Option<SlotCounts> {
        let state = self.lock();
        state.limit?;
        if state.has_room() && state.queue.is_empty() {
            return None;
        }
        Some(SlotCounts {
            limit: state.limit,
            in_use: state.in_use,
            queued: u32::try_from(state.queue.len()).unwrap_or(u32::MAX),
        })
    }

    /// Take a place without waiting: only when there is room and nobody
    /// waits (a newcomer never jumps the queue). `Err` carries the counts.
    pub(crate) fn try_take(self: &Arc<Self>) -> Result<SlotPermit, SlotCounts> {
        let mut state = self.lock();
        if state.has_room() && state.queue.is_empty() {
            state.in_use += 1;
            return Ok(SlotPermit {
                slots: self.clone(),
            });
        }
        Err(SlotCounts {
            limit: state.limit,
            in_use: state.in_use,
            queued: u32::try_from(state.queue.len()).unwrap_or(u32::MAX),
        })
    }

    /// Take a place, waiting in the queue up to `wait_ms` on `clock`. Waiters
    /// get places in the order they arrived. Dropping the returned future
    /// leaves the queue.
    pub(crate) async fn take(
        self: &Arc<Self>,
        wait_ms: u64,
        clock: &dyn Clock,
    ) -> Result<(SlotPermit, u64), SlotTimeout> {
        let mut changes = self.changed.subscribe();
        let ticket = {
            let mut state = self.lock();
            if state.has_room() && state.queue.is_empty() {
                state.in_use += 1;
                return Ok((
                    SlotPermit {
                        slots: self.clone(),
                    },
                    0,
                ));
            }
            let id = state.next_ticket;
            state.next_ticket = state.next_ticket.wrapping_add(1);
            state.queue.push_back(id);
            Ticket {
                slots: self.clone(),
                id,
            }
        };
        let started = clock.now_ms();
        loop {
            changes.borrow_and_update();
            let waited = clock.now_ms().saturating_sub(started);
            {
                let mut state = self.lock();
                if state.queue.front() == Some(&ticket.id) && state.has_room() {
                    state.queue.pop_front();
                    state.in_use += 1;
                    drop(state);
                    // Out of the queue already, so dropping the ticket is a
                    // no-op; the next waiter is now first and may fit.
                    drop(ticket);
                    self.bump();
                    return Ok((
                        SlotPermit {
                            slots: self.clone(),
                        },
                        waited,
                    ));
                }
                if waited >= wait_ms {
                    drop(state);
                    let counts_before = self.counts();
                    drop(ticket);
                    return Err(SlotTimeout {
                        waited_ms: waited,
                        counts: SlotCounts {
                            queued: counts_before.queued.saturating_sub(1),
                            ..counts_before
                        },
                    });
                }
            }
            tokio::select! {
                _ = changes.changed() => {}
                () = clock.sleep_ms(wait_ms - waited) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
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

    #[async_trait]
    impl Clock for ManualClock {
        fn now_ms(&self) -> u64 {
            self.now.load(Ordering::SeqCst)
        }
        async fn sleep_ms(&self, ms: u64) {
            let deadline = self.now_ms().saturating_add(ms);
            let mut moved = self.moved.subscribe();
            while self.now_ms() < deadline {
                if moved.changed().await.is_err() {
                    return;
                }
            }
        }
    }

    async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }

    fn waiter(
        slots: &Arc<LaneSlots>,
        clock: &Arc<ManualClock>,
        wait_ms: u64,
        name: &'static str,
        order: &Arc<Mutex<Vec<&'static str>>>,
    ) -> tokio::task::JoinHandle<Result<SlotPermit, SlotTimeout>> {
        let (slots, clock, order) = (slots.clone(), clock.clone(), order.clone());
        tokio::spawn(async move {
            let taken = slots.take(wait_ms, &*clock).await;
            if taken.is_ok() {
                order.lock().unwrap().push(name);
            }
            taken.map(|(permit, _)| permit)
        })
    }

    // Three requests wait behind a full lane of one place; each freed place
    // goes to the oldest waiter, never to a newcomer.
    #[tokio::test]
    async fn places_go_to_waiters_in_arrival_order() {
        let clock = ManualClock::new();
        let slots = LaneSlots::new(Some(1));
        let order = Arc::new(Mutex::new(Vec::new()));
        let first = slots.try_take().expect("room for one");
        let a = waiter(&slots, &clock, 60_000, "a", &order);
        settle().await;
        let b = waiter(&slots, &clock, 60_000, "b", &order);
        settle().await;
        let c = waiter(&slots, &clock, 60_000, "c", &order);
        settle().await;
        assert_eq!(slots.counts().queued, 3);
        assert!(
            slots.try_take().is_err(),
            "a newcomer never jumps the queue"
        );

        drop(first);
        let a = a.await.unwrap().expect("a goes first");
        settle().await;
        assert_eq!(*order.lock().unwrap(), ["a"]);
        assert!(!b.is_finished() && !c.is_finished());
        drop(a);
        let b = b.await.unwrap().expect("then b");
        drop(b);
        let c = c.await.unwrap().expect("then c");
        assert_eq!(*order.lock().unwrap(), ["a", "b", "c"]);
        drop(c);
        let counts = slots.counts();
        assert_eq!((counts.in_use, counts.queued), (0, 0));
    }

    // A wait that runs out says how long it waited, leaves the queue, and
    // the waiter behind it moves up.
    #[tokio::test]
    async fn a_wait_that_runs_out_leaves_the_queue() {
        let clock = ManualClock::new();
        let slots = LaneSlots::new(Some(1));
        let order = Arc::new(Mutex::new(Vec::new()));
        let held = slots.try_take().unwrap();
        let short = waiter(&slots, &clock, 5_000, "short", &order);
        settle().await;
        let long = waiter(&slots, &clock, 60_000, "long", &order);
        settle().await;
        clock.advance(4_999);
        settle().await;
        assert!(!short.is_finished(), "not yet");
        clock.advance(1);
        let timeout = short.await.unwrap().expect_err("ran out");
        assert_eq!(timeout.waited_ms, 5_000);
        assert_eq!(timeout.counts.in_use, 1);
        assert_eq!(slots.counts().queued, 1, "only `long` waits now");
        drop(held);
        long.await.unwrap().expect("the next waiter gets the place");
        assert_eq!(*order.lock().unwrap(), ["long"]);
    }

    // A waiter that goes away (its future dropped, as when a client
    // disconnects) leaves the queue at once and never takes a place.
    #[tokio::test]
    async fn a_waiter_that_goes_away_leaves_the_queue() {
        let clock = ManualClock::new();
        let slots = LaneSlots::new(Some(1));
        let order = Arc::new(Mutex::new(Vec::new()));
        let held = slots.try_take().unwrap();
        let gone = waiter(&slots, &clock, 60_000, "gone", &order);
        settle().await;
        let next = waiter(&slots, &clock, 60_000, "next", &order);
        settle().await;
        assert_eq!(slots.counts().queued, 2);
        gone.abort();
        settle().await;
        assert_eq!(slots.counts().queued, 1);
        drop(held);
        next.await
            .unwrap()
            .expect("the place goes to the waiter still there");
        assert_eq!(*order.lock().unwrap(), ["next"]);
    }

    // A place held across a panic is given back while the task unwinds.
    #[tokio::test]
    async fn a_panicking_holder_gives_its_place_back() {
        let slots = LaneSlots::new(Some(1));
        let held = slots.try_take().unwrap();
        let task = tokio::spawn(async move {
            let _held = held;
            panic!("a bug while holding a place");
        });
        assert!(task.await.unwrap_err().is_panic());
        assert_eq!(slots.counts().in_use, 0);
        assert!(slots.try_take().is_ok());
    }

    // A lower limit after a reload lets running work finish and makes new
    // work wait; a higher one lets waiters in at once.
    #[tokio::test]
    async fn a_new_limit_applies_to_new_work() {
        let clock = ManualClock::new();
        let slots = LaneSlots::new(Some(2));
        let order = Arc::new(Mutex::new(Vec::new()));
        let one = slots.try_take().unwrap();
        let two = slots.try_take().unwrap();
        slots.set_limit(Some(1));
        drop(one);
        assert!(
            slots.full().is_some(),
            "two is still running under a limit of one"
        );
        let waiting = waiter(&slots, &clock, 60_000, "w", &order);
        settle().await;
        assert!(!waiting.is_finished());
        slots.set_limit(Some(3));
        waiting.await.unwrap().expect("room under the new limit");
        drop(two);
        slots.set_limit(None);
        assert_eq!(slots.full(), None, "no limit, never full");
    }
}
