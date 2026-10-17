//! Event bridge: the global bounded replay ring for UI events.
//!
//! Every routed agent event is converted to a [`protocol::Event`], wrapped in
//! an [`Envelope`] with a process-global monotonic cursor, and both appended to
//! a bounded in-memory ring and notify live consumers. The preferred cursor
//! subscription uses a coalescing watch signal and bounded ring reads; the
//! legacy broadcast subscription remains available for compatibility. When a
//! requested cursor has been evicted from the ring, the consumer is told to
//! `resync` — refetch the snapshot and message page rather than guess the
//! missing state.
//!
//! Both limits (event count and total bytes) are configurable and clamped; a
//! slow cursor consumer that falls behind the ring is detected and told to
//! resync rather than dropping events silently.

use std::{
    collections::VecDeque,
    sync::{Arc, RwLock},
};

use tokio::sync::{broadcast, watch};

use crate::protocol::{Envelope, Event};

/// Result of a replay request from a cursor.
#[derive(Clone, Debug)]
pub enum ReplayResult {
    /// Buffered events strictly after the requested cursor, oldest first.
    Replay(Vec<Arc<Envelope>>),
    /// The requested cursor was evicted; the consumer must resync.
    ResyncRequired,
}

/// An atomic live subscription plus the buffered events that preceded it.
///
/// Returned by [`EventBridge::subscribe_from`]. `replay` holds every event with
/// a cursor strictly after the consumer's last-processed cursor that existed
/// when the ring boundary was snapshotted; `live` delivers every event pushed
/// after the receiver was created. An event pushed between the two steps (the
/// receiver creation and the boundary snapshot) is delivered in **both** `replay`
/// and `live`, so consumers must deduplicate by cursor — skip live events whose
/// cursor is `<=` the last cursor they processed from `replay`.
pub struct Subscription {
    pub replay: Vec<Arc<Envelope>>,
    pub live: broadcast::Receiver<Arc<Envelope>>,
}

/// Cursor based subscription. The watch channel only signals that the replay
/// ring may have advanced; consumers fetch bounded batches with
/// [`EventBridge::events_after`] and never queue one notification per event.
pub struct CursorSubscription {
    pub replay: Vec<Arc<Envelope>>,
    pub changed: watch::Receiver<u64>,
}

/// The requested replay cursor was evicted from the ring; the consumer must
/// refetch the snapshot and message page and subscribe again from the fresh
/// cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResyncRequired;

impl core::fmt::Display for ResyncRequired {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "event cursor evicted; resync required")
    }
}

impl std::error::Error for ResyncRequired {}

/// Default maximum number of events retained in the ring.
pub const DEFAULT_MAX_EVENTS: usize = 512;
/// Default maximum total bytes retained in the ring.
pub const DEFAULT_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Bounds applied to the ring capacity on construction.
pub const MIN_MAX_EVENTS: usize = 16;
pub const MAX_MAX_EVENTS: usize = 4096;
pub const MIN_MAX_BYTES: usize = 1024 * 1024;
pub const MAX_MAX_BYTES: usize = 16 * 1024 * 1024;

struct Ring {
    events: VecDeque<Arc<Envelope>>,
    bytes: usize,
    last_cursor: u64,
}

impl Ring {
    fn new() -> Self {
        Self {
            events: VecDeque::new(),
            bytes: 0,
            last_cursor: 0,
        }
    }
}

fn string_bytes(value: &str) -> usize {
    value.len()
}

fn tool_call_bytes(call: &crate::provider::ToolCall) -> usize {
    call.id.len() + call.name.len() + call.arguments.to_string().len() + 32
}

fn event_payload_bytes(event: &Event) -> usize {
    use crate::protocol::Event as E;
    match event {
        E::ReasoningDelta { delta } => string_bytes(delta),
        E::ReasoningCompleted => 0,
        E::ProviderRetry {
            reason, delay_ms, ..
        } => string_bytes(reason) + 16 + (*delay_ms as usize / 16),
        E::ModelStreaming => 0,
        E::WebSearchStarted { query } => string_bytes(query),
        E::WebSearchResult {
            title,
            url,
            snippet,
        } => string_bytes(title) + string_bytes(url) + string_bytes(snippet),
        E::WebSearchCompleted { .. } => 8,
        E::Cancelled { reason } => string_bytes(reason),
        E::TextDelta { delta } => string_bytes(delta),
        E::ToolCallStreaming { name, .. } => name.as_ref().map_or(0, String::len) + 16,
        E::Approval {
            call,
            reason,
            source_session_id,
            source_title,
            ..
        } => {
            tool_call_bytes(call)
                + string_bytes(reason)
                + source_session_id.as_ref().map_or(0, String::len)
                + source_title.as_ref().map_or(0, String::len)
        }
        E::ApprovalResolved { .. } => 24,
        E::ToolStarted { call } => tool_call_bytes(call),
        E::ToolFinished { call, result } => tool_call_bytes(call) + string_bytes(result),
        E::Usage { .. } => 24,
        E::Completed => 0,
        E::Failed { error } => string_bytes(error),
        E::SessionsChanged => 0,
        E::ChildSessionProgress {
            child_session_id,
            status,
            tool,
            ..
        } => {
            string_bytes(child_session_id)
                + string_bytes(status)
                + tool.as_ref().map_or(0, String::len)
        }
        E::LocalCommandFinished { command, result } => string_bytes(command) + string_bytes(result),
        E::CompactionStarted => 0,
        E::CompactionCompleted { .. } => 8,
        E::CompactionFailed { error } => string_bytes(error),
        E::TodoUpdated { tasks } => tasks
            .iter()
            .map(|task| {
                task.id.len() + task.title.len() + task.created_at.len() + task.updated_at.len()
            })
            .sum(),
        E::TranscriptInvalidated => 0,
        E::ContextUpdated { .. } => 48,
        E::ResyncRequired => 0,
    }
}

fn envelope_bytes(envelope: &Envelope) -> usize {
    // A cheap upper bound: cursor + session_id + serialized payload length.
    envelope.session_id.len() + 16 + event_payload_bytes(&envelope.event)
}

/// Shared event fan-out with a global replay ring.
///
/// All methods are synchronous: `push` only touches an in-memory ring and a
/// broadcast channel, so the state-machine task never blocks on I/O.
#[derive(Clone)]
pub struct EventBridge {
    tx: broadcast::Sender<Arc<Envelope>>,
    changed: watch::Sender<u64>,
    ring: Arc<RwLock<Ring>>,
    max_events: usize,
    max_bytes: usize,
}

impl EventBridge {
    /// Live cursor watchers; useful for consumer shutdown/cancellation audits.
    pub fn cursor_subscriber_count(&self) -> usize {
        self.changed.receiver_count()
    }
    pub fn new(max_events: usize, max_bytes: usize) -> Self {
        let max_events = max_events.clamp(MIN_MAX_EVENTS, MAX_MAX_EVENTS);
        let max_bytes = max_bytes.clamp(MIN_MAX_BYTES, MAX_MAX_BYTES);
        let (tx, _) = broadcast::channel(256);
        let (changed, _) = watch::channel(0);
        Self {
            tx,
            changed,
            ring: Arc::new(RwLock::new(Ring::new())),
            max_events,
            max_bytes,
        }
    }

    /// Registers an event: assigns the next process-global cursor, appends it to
    /// the replay ring (evicting the oldest entry when a limit is exceeded), and
    /// broadcasts it live. Returns the envelope so callers can reuse it.
    pub fn push(&self, session_id: String, event: Event) -> Arc<Envelope> {
        let envelope = {
            let mut ring = self
                .ring
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // Cursor zero is the empty-stream baseline returned in snapshots;
            // assign the next value in the same critical section that publishes
            // the envelope so readers cannot observe an event before its cursor
            // is available from the replay ring.
            let cursor = ring.last_cursor + 1;
            let envelope = Arc::new(Envelope {
                cursor,
                session_id,
                event,
            });
            ring.last_cursor = cursor;
            ring.bytes = ring.bytes.saturating_add(envelope_bytes(&envelope));
            ring.events.push_back(envelope.clone());
            while ring.events.len() > self.max_events || ring.bytes > self.max_bytes {
                let Some(oldest) = ring.events.pop_front() else {
                    break;
                };
                ring.bytes = ring.bytes.saturating_sub(envelope_bytes(&oldest));
            }
            envelope
        };
        // Receivers that lag the channel fall back to the ring on reconnect.
        let _ = self.tx.send(envelope.clone());
        self.changed.send_replace(envelope.cursor);
        envelope
    }

    /// The latest assigned cursor, or zero before the first event. Snapshot
    /// responses return this as `event_cursor` so a fresh consumer subscribes
    /// strictly after exactly this position.
    pub fn current_cursor(&self) -> u64 {
        self.ring
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .last_cursor
    }

    /// Subscribes to live events. The consumer must also call
    /// [`Self::replay_after`] *before* subscribing (replay first, then live) so
    /// events pushed between the two calls are never missed or duplicated.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Envelope>> {
        self.tx.subscribe()
    }

    /// Returns the buffered events strictly after `after`, oldest first.
    /// Returns [`ReplayResult::ResyncRequired`] when `after` is older than the
    /// oldest retained event (or predates every retained event), meaning the
    /// consumer must refetch snapshot + message page.
    pub fn replay_after(&self, after: u64) -> ReplayResult {
        let ring = self
            .ring
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(oldest) = ring.events.front() else {
            if after < ring.last_cursor {
                return ReplayResult::ResyncRequired;
            }
            return ReplayResult::Replay(Vec::new());
        };
        if after.saturating_add(1) < oldest.cursor {
            return ReplayResult::ResyncRequired;
        }
        ReplayResult::Replay(
            ring.events
                .iter()
                .filter(|envelope| envelope.cursor > after)
                .cloned()
                .collect(),
        )
    }

    /// Atomically subscribes from a known last-processed cursor.
    ///
    /// Subscribes to the live stream first, then snapshots the replay ring under
    /// its read lock so `replay` covers exactly the events with a cursor
    /// strictly after `after` that existed at the boundary snapshot. Events
    /// pushed between the two steps land in both `replay` and `live`; consumers
    /// deduplicate by cursor (skip live events with `cursor <= last_processed`).
    ///
    /// This removes the race in the old "replay then subscribe" pattern, where
    /// an event pushed between the two calls was neither replayed nor received
    /// live. Returns [`ResyncRequired`] when `after` has been evicted from the
    /// ring, in which case the caller must refetch the snapshot and message
    /// page and subscribe again from the fresh cursor.
    pub fn subscribe_from(&self, after: u64) -> Result<Subscription, ResyncRequired> {
        // Subscribe before snapshotting the boundary so anything pushed after
        // the snapshot is still delivered live (never lost, never double when
        // deduplicated by cursor).
        let live = self.tx.subscribe();
        let ring = self
            .ring
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // No push (which needs the write lock) can interleave with this cursor
        // and ring snapshot.
        let boundary = ring.last_cursor + 1;
        let Some(oldest) = ring.events.front() else {
            if after < ring.last_cursor {
                return Err(ResyncRequired);
            }
            return Ok(Subscription {
                replay: Vec::new(),
                live,
            });
        };
        if after.saturating_add(1) < oldest.cursor {
            return Err(ResyncRequired);
        }
        let replay = ring
            .events
            .iter()
            .filter(|envelope| envelope.cursor > after && envelope.cursor < boundary)
            .cloned()
            .collect();
        Ok(Subscription { replay, live })
    }

    /// Subscribes to bounded replay batches from a cursor. The watch receiver
    /// is installed before the ring boundary is read, so a push racing with
    /// subscription is either in `replay` or leaves a pending wakeup (often
    /// both); consumers deduplicate by cursor.
    pub fn subscribe_cursor_from(&self, after: u64) -> Result<CursorSubscription, ResyncRequired> {
        let changed = self.changed.subscribe();
        let ring = self
            .ring
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let boundary = ring.last_cursor + 1;
        let Some(oldest) = ring.events.front() else {
            if after < ring.last_cursor {
                return Err(ResyncRequired);
            }
            return Ok(CursorSubscription {
                replay: Vec::new(),
                changed,
            });
        };
        if after.saturating_add(1) < oldest.cursor {
            return Err(ResyncRequired);
        }
        let replay = ring
            .events
            .iter()
            .filter(|envelope| envelope.cursor > after && envelope.cursor < boundary)
            .cloned()
            .collect();
        Ok(CursorSubscription { replay, changed })
    }

    /// Reads at most `limit` retained events strictly after `after`.
    /// `None` means the cursor was evicted and the consumer must resync.
    pub fn events_after(&self, after: u64, limit: usize) -> Option<Vec<Arc<Envelope>>> {
        let ring = self
            .ring
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(oldest) = ring.events.front() else {
            return (after >= ring.last_cursor).then(Vec::new);
        };
        if after.saturating_add(1) < oldest.cursor {
            return None;
        }
        Some(
            ring.events
                .iter()
                .filter(|envelope| envelope.cursor > after)
                .take(limit.max(1))
                .cloned()
                .collect(),
        )
    }

    /// The configured ring capacity (clamped).
    pub fn max_events(&self) -> usize {
        self.max_events
    }

    /// The configured ring byte cap (clamped).
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Event;

    fn text(delta: &str) -> Event {
        Event::TextDelta {
            delta: delta.into(),
        }
    }

    #[test]
    fn cursors_are_global_and_monotonic() {
        let bridge = EventBridge::new(64, DEFAULT_MAX_BYTES);
        assert_eq!(bridge.current_cursor(), 0);
        for i in 0..10 {
            bridge.push("a".into(), text(&format!("{i}")));
            assert_eq!(bridge.current_cursor(), i + 1);
        }
        bridge.push("b".into(), text("b"));
        assert_eq!(bridge.current_cursor(), 11);
    }

    #[test]
    fn replay_after_returns_only_newer_events_in_order() {
        let bridge = EventBridge::new(64, DEFAULT_MAX_BYTES);
        for i in 0..10 {
            bridge.push("a".into(), text(&format!("{i}")));
        }
        match bridge.replay_after(3) {
            ReplayResult::Replay(events) => {
                assert_eq!(events.len(), 7);
                assert_eq!(events[0].cursor, 4);
                assert_eq!(events[6].cursor, 10);
            }
            ReplayResult::ResyncRequired => panic!("cursor 3 must still be buffered"),
        }
    }

    #[test]
    fn evicted_cursor_requires_resync() {
        let bridge = EventBridge::new(16, DEFAULT_MAX_BYTES);
        for i in 0..32 {
            bridge.push("a".into(), text(&format!("{i}")));
        }
        // The ring only keeps the last 16 (cursors 16..=31); 15 is evicted.
        assert!(matches!(
            bridge.replay_after(15),
            ReplayResult::ResyncRequired
        ));
        // The oldest retained cursor replays the tail without resync.
        match bridge.replay_after(16) {
            ReplayResult::Replay(events) => assert_eq!(events.len(), 16),
            ReplayResult::ResyncRequired => panic!("cursor 16 must still be buffered"),
        }
    }

    #[test]
    fn byte_cap_evicts_largest_payloads_first_in_fifo_order() {
        // Small byte cap forces eviction even with a large event budget.
        let bridge = EventBridge::new(256, 2 * 1024 * 1024);
        let big = "x".repeat(4 * 1024 * 1024);
        bridge.push("a".into(), text(&big));
        // The single event exceeds the byte cap, so the ring must evict it.
        assert!(matches!(
            bridge.replay_after(0),
            ReplayResult::ResyncRequired
        ));
    }

    #[test]
    fn empty_ring_replays_nothing_without_resync_when_up_to_date() {
        let bridge = EventBridge::new(16, DEFAULT_MAX_BYTES);
        match bridge.replay_after(0) {
            ReplayResult::Replay(events) => assert!(events.is_empty()),
            ReplayResult::ResyncRequired => panic!("nothing was pushed, nothing was evicted"),
        }
    }

    #[test]
    fn reasoning_completed_is_replayed_in_order_with_zero_payload() {
        let bridge = EventBridge::new(64, DEFAULT_MAX_BYTES);
        // The replay-after-1 starts at the reasoning sequence.
        bridge.push("a".into(), text("lead"));
        bridge.push("a".into(), Event::ReasoningDelta { delta: "d".into() });
        bridge.push("a".into(), Event::ReasoningCompleted);
        bridge.push("a".into(), text("answer"));
        match bridge.replay_after(1) {
            ReplayResult::Replay(events) => {
                assert_eq!(events.len(), 3);
                assert!(matches!(events[0].event, Event::ReasoningDelta { .. }));
                assert!(matches!(events[1].event, Event::ReasoningCompleted));
                assert!(matches!(events[2].event, Event::TextDelta { .. }));
            }
            ReplayResult::ResyncRequired => panic!("cursor 0 must still be buffered"),
        }
    }

    #[test]
    fn live_broadcast_carries_arc_envelopes() {
        let bridge = EventBridge::new(64, DEFAULT_MAX_BYTES);
        let mut receiver = bridge.subscribe();
        let pushed = bridge.push("s".into(), text("hi"));
        let received = receiver.try_recv().expect("live event");
        assert_eq!(received.cursor, pushed.cursor);
        assert_eq!(received.session_id, "s");
        assert!(Arc::ptr_eq(&received, &pushed));
    }

    #[test]
    fn subscribe_from_replays_then_streams_live_without_gap() {
        let bridge = EventBridge::new(64, DEFAULT_MAX_BYTES);
        for i in 0..5 {
            bridge.push("a".into(), text(&format!("{i}")));
        }
        let Subscription { replay, mut live } = bridge.subscribe_from(2).unwrap();
        // Cursors 1..=5 are pushed; replay is strictly after 2 through 5.
        assert_eq!(replay.len(), 3);
        assert_eq!(replay[0].cursor, 3);
        assert_eq!(replay[1].cursor, 4);
        assert_eq!(replay[2].cursor, 5);
        // An event pushed after the atomic subscription arrives live only.
        let pushed = bridge.push("a".into(), text("5"));
        let received = live.try_recv().expect("live event after subscribe");
        assert_eq!(received.cursor, pushed.cursor);
        assert_eq!(received.cursor, 6);
    }

    #[test]
    fn subscribe_from_from_current_cursor_replays_nothing() {
        let bridge = EventBridge::new(64, DEFAULT_MAX_BYTES);
        for i in 0..5 {
            bridge.push("a".into(), text(&format!("{i}")));
        }
        let Subscription { replay, .. } = bridge.subscribe_from(5).unwrap();
        assert!(replay.is_empty());
    }

    #[test]
    fn subscribe_from_evicted_cursor_requires_resync() {
        let bridge = EventBridge::new(16, DEFAULT_MAX_BYTES);
        for i in 0..32 {
            bridge.push("a".into(), text(&format!("{i}")));
        }
        assert!(matches!(bridge.subscribe_from(15), Err(ResyncRequired)));
        // The oldest retained cursor succeeds.
        assert!(bridge.subscribe_from(16).is_ok());
    }

    #[test]
    fn subscribe_from_duplicate_overlap_is_deduplicatable_by_cursor() {
        // An event pushed between the live subscription and the ring snapshot
        // lands in both `replay` and `live`; consumers skip any live event whose
        // cursor is <= the last cursor processed from `replay`. Exercised with
        // the consumer-side rule so the gapless, exactly-once property holds.
        let bridge = EventBridge::new(64, DEFAULT_MAX_BYTES);
        for i in 0..5 {
            bridge.push("a".into(), text(&format!("{i}")));
        }
        let Subscription { replay, live } = bridge.subscribe_from(2).unwrap();
        let mut last = 2;
        let mut applied: Vec<u64> = Vec::new();
        for envelope in &replay {
            if envelope.cursor > last {
                applied.push(envelope.cursor);
                last = envelope.cursor;
            }
        }
        assert_eq!(applied, vec![3, 4, 5]);
        // Fresh live events (cursors 6, 7) pass the rule.
        let mut live = live;
        bridge.push("a".into(), text("5"));
        bridge.push("a".into(), text("6"));
        while let Ok(envelope) = live.try_recv() {
            if envelope.cursor > last {
                applied.push(envelope.cursor);
                last = envelope.cursor;
            }
        }
        assert_eq!(applied, vec![3, 4, 5, 6, 7]);
        assert_eq!(last, 7);
    }

    #[tokio::test]
    async fn cursor_subscription_coalesces_wakeups_and_reads_bounded_batches() {
        let bridge = EventBridge::new(64, DEFAULT_MAX_BYTES);
        let mut subscription = bridge.subscribe_cursor_from(0).unwrap();
        for _ in 0..10 {
            bridge.push("s".into(), Event::TextDelta { delta: "x".into() });
        }

        subscription.changed.changed().await.unwrap();
        let first = bridge.events_after(0, 4).unwrap();
        assert_eq!(first.len(), 4);
        assert_eq!(first[0].cursor, 1);
        assert_eq!(first[3].cursor, 4);
        let second = bridge
            .events_after(first.last().unwrap().cursor, 64)
            .unwrap();
        assert_eq!(second.len(), 6);
        assert_eq!(second.last().unwrap().cursor, 10);
        assert!(bridge.events_after(10, 1).unwrap().is_empty());
    }

    #[tokio::test]
    async fn thirty_two_cursor_consumers_keep_up_with_one_thousand_events_per_second() {
        use tokio::time::{Duration, interval};

        let bridge = EventBridge::new(DEFAULT_MAX_EVENTS, DEFAULT_MAX_BYTES);
        let mut consumers: Vec<_> = (0..32)
            .map(|_| bridge.subscribe_cursor_from(0).unwrap())
            .collect();
        let mut cursors = vec![0; consumers.len()];
        let mut received = vec![0; consumers.len()];
        let mut tick = interval(Duration::from_millis(1));

        for index in 0..1000 {
            tick.tick().await;
            bridge.push("s".into(), text(&index.to_string()));
            if index % 16 == 15 {
                for (consumer_index, consumer) in consumers.iter_mut().enumerate() {
                    if consumer.changed.has_changed().unwrap() {
                        consumer.changed.changed().await.unwrap();
                    }
                    let events = bridge
                        .events_after(cursors[consumer_index], 128)
                        .expect("healthy consumer stays within the default replay window");
                    for event in &events {
                        assert_eq!(event.cursor, cursors[consumer_index] + 1);
                        cursors[consumer_index] = event.cursor;
                    }
                    received[consumer_index] += events.len();
                }
            }
        }
        for (consumer_index, consumer) in consumers.iter_mut().enumerate() {
            let events = bridge
                .events_after(cursors[consumer_index], 128)
                .expect("tail remains in the default replay window");
            received[consumer_index] += events.len();
            assert_eq!(received[consumer_index], 1000);
            assert_eq!(events.last().map(|event| event.cursor), Some(1000));
            assert!(consumer.changed.has_changed().is_ok());
        }
    }

    #[test]
    fn two_thousand_event_burst_fits_explicit_two_thousand_forty_eight_capacity() {
        let bridge = EventBridge::new(2048, 4 * 1024 * 1024);
        let _subscription = bridge.subscribe_cursor_from(0).unwrap();
        for index in 0..2048 {
            bridge.push("s".into(), text(&index.to_string()));
        }
        let events = bridge
            .events_after(0, 2048)
            .expect("burst is within configured event and byte budgets");
        assert_eq!(events.len(), 2048);
        assert_eq!(events.first().unwrap().cursor, 1);
        assert_eq!(events.last().unwrap().cursor, 2048);
    }

    #[test]
    fn cursor_subscription_requires_resync_after_ring_eviction() {
        let bridge = EventBridge::new(16, DEFAULT_MAX_BYTES);
        for _ in 0..20 {
            bridge.push("s".into(), Event::TextDelta { delta: "x".into() });
        }
        assert!(bridge.subscribe_cursor_from(0).is_err());
        assert!(bridge.events_after(0, 1).is_none());
    }
}
