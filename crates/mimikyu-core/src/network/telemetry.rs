use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::driver::{now_ms, ConsoleEntry, NetworkEvent, PageError};

pub const DEFAULT_TELEMETRY_CAPACITY: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryConfig {
    pub max_console_entries: usize,
    pub max_page_errors: usize,
    pub max_network_events: usize,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            max_console_entries: DEFAULT_TELEMETRY_CAPACITY,
            max_page_errors: DEFAULT_TELEMETRY_CAPACITY,
            max_network_events: DEFAULT_TELEMETRY_CAPACITY,
        }
    }
}

impl TelemetryConfig {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            max_console_entries: cap,
            max_page_errors: cap,
            max_network_events: cap,
        }
    }
}

/// Bounded FIFO ring buffer tracking evicted (`dropped_count`) and total items seen.
#[derive(Debug, Clone)]
pub struct RingBuffer<T> {
    buf: VecDeque<T>,
    capacity: usize,
    dropped_count: u64,
    total_seen: u64,
}

impl<T: Clone> RingBuffer<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(capacity.min(256)),
            capacity,
            dropped_count: 0,
            total_seen: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn dropped_count(&self) -> u64 {
        self.dropped_count
    }

    pub fn total_seen(&self) -> u64 {
        self.total_seen
    }

    pub fn push(&mut self, item: T) {
        self.total_seen += 1;
        if self.capacity == 0 {
            self.dropped_count += 1;
            return;
        }
        if self.buf.len() >= self.capacity {
            self.buf.pop_front();
            self.dropped_count += 1;
        }
        self.buf.push_back(item);
    }

    pub fn snapshot(&self) -> Vec<T> {
        self.buf.iter().cloned().collect()
    }

    pub fn drain(&mut self) -> Vec<T> {
        self.buf.drain(..).collect()
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetrySnapshot {
    pub console_logs: Vec<ConsoleEntry>,
    pub page_errors: Vec<PageError>,
    pub network_events: Vec<NetworkEvent>,
    pub inflight_count: usize,
    pub dropped_console: u64,
    pub dropped_page_errors: u64,
    pub dropped_network: u64,
}

/// Thread-safe telemetry buffer shared by CDP and BiDi session states.
///
/// Canonical lock acquisition order when multiple locks are held simultaneously:
/// `inflight` -> `pending_requests` -> `network`.
/// `console` and `page_errors` are independent leaf locks, and snapshot/drain
/// methods acquire at most one lock at a time (dropping each guard before
/// acquiring the next) to prevent lock-order inversions.
pub struct TelemetryStore {
    console: Mutex<RingBuffer<ConsoleEntry>>,
    page_errors: Mutex<RingBuffer<PageError>>,
    network: Mutex<RingBuffer<NetworkEvent>>,
    pending_requests: Mutex<HashMap<String, NetworkEvent>>,
    inflight: Mutex<HashSet<String>>,
    activity_seq: AtomicU64,
}

impl Default for TelemetryStore {
    fn default() -> Self {
        Self::new(TelemetryConfig::default())
    }
}

impl TelemetryStore {
    pub fn new(config: TelemetryConfig) -> Self {
        Self {
            console: Mutex::new(RingBuffer::new(config.max_console_entries)),
            page_errors: Mutex::new(RingBuffer::new(config.max_page_errors)),
            network: Mutex::new(RingBuffer::new(config.max_network_events)),
            pending_requests: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashSet::new()),
            activity_seq: AtomicU64::new(0),
        }
    }

    pub fn push_console(&self, entry: ConsoleEntry) {
        self.console.lock().push(entry);
    }

    pub fn push_page_error(&self, err: PageError) {
        self.page_errors.lock().push(err);
    }

    /// Record request start. If `request_id` is already pending (e.g. HTTP 30x redirect in CDP),
    /// finalize the previous hop with optional `redirect_status`/`redirect_headers` before tracking the new hop.
    pub fn on_request_started(
        &self,
        ev: NetworkEvent,
        redirect_status: Option<u16>,
        redirect_mime: Option<String>,
        redirect_headers: BTreeMap<String, String>,
    ) {
        self.activity_seq.fetch_add(1, Ordering::SeqCst);
        self.inflight.lock().insert(ev.request_id.clone());
        let mut pending = self.pending_requests.lock();
        if let Some(mut prev) = pending.insert(ev.request_id.clone(), ev) {
            if prev.status.is_none() {
                prev.status = redirect_status;
            }
            if prev.mime_type.is_none() {
                prev.mime_type = redirect_mime;
            }
            if !redirect_headers.is_empty() {
                prev.response_headers = redirect_headers;
            }
            self.network.lock().push(prev);
        }
    }

    pub fn set_pending_resource_type(&self, req_id: &str, resource_type: &str) {
        if let Some(ev) = self.pending_requests.lock().get_mut(req_id) {
            ev.resource_type = resource_type.to_string();
        }
    }

    pub fn on_response_received(
        &self,
        req_id: &str,
        status: Option<u16>,
        mime_type: Option<String>,
        response_headers: BTreeMap<String, String>,
    ) {
        if let Some(ev) = self.pending_requests.lock().get_mut(req_id) {
            ev.status = status;
            ev.mime_type = mime_type;
            ev.response_headers = response_headers;
        }
    }

    pub fn on_request_finished(&self, req_id: &str) {
        self.activity_seq.fetch_add(1, Ordering::SeqCst);
        self.inflight.lock().remove(req_id);
        if let Some(ev) = self.pending_requests.lock().remove(req_id) {
            self.network.lock().push(ev);
        }
    }

    pub fn on_request_failed(&self, req_id: &str, error_text: Option<String>) {
        self.activity_seq.fetch_add(1, Ordering::SeqCst);
        self.inflight.lock().remove(req_id);
        if let Some(mut ev) = self.pending_requests.lock().remove(req_id) {
            ev.failed = true;
            ev.error_text = error_text;
            self.network.lock().push(ev);
        }
    }

    /// Idempotently finalize a request resolved locally via `failRequest` or `provideResponse`.
    /// Prevents `inflight_requests` leaks if an engine omits follow-up `fetchError`/`responseCompleted`.
    pub fn on_intercept_resolved_locally(
        &self,
        req_id: &str,
        status: Option<u16>,
        response_headers: BTreeMap<String, String>,
        failed: bool,
        error_text: Option<String>,
    ) {
        self.activity_seq.fetch_add(1, Ordering::SeqCst);
        self.inflight.lock().remove(req_id);
        if let Some(mut ev) = self.pending_requests.lock().remove(req_id) {
            if let Some(s) = status {
                ev.status = Some(s);
            }
            if !response_headers.is_empty() {
                ev.response_headers = response_headers;
            }
            ev.failed = failed;
            ev.error_text = error_text;
            self.network.lock().push(ev);
        }
    }

    pub fn snapshot_console(&self) -> Vec<ConsoleEntry> {
        self.console.lock().snapshot()
    }

    pub fn drain_console(&self) -> Vec<ConsoleEntry> {
        self.console.lock().drain()
    }

    pub fn snapshot_page_errors(&self) -> Vec<PageError> {
        self.page_errors.lock().snapshot()
    }

    pub fn drain_page_errors(&self) -> Vec<PageError> {
        self.page_errors.lock().drain()
    }

    pub fn snapshot_network(&self) -> Vec<NetworkEvent> {
        let mut out = self.network.lock().snapshot();
        out.extend(self.pending_requests.lock().values().cloned());
        out
    }

    pub fn drain_network(&self) -> Vec<NetworkEvent> {
        self.network.lock().drain()
    }

    pub fn clear(&self) {
        self.console.lock().clear();
        self.page_errors.lock().clear();
        self.network.lock().clear();
    }

    pub fn inflight_count(&self) -> usize {
        self.inflight.lock().len()
    }

    pub fn activity_seq(&self) -> u64 {
        self.activity_seq.load(Ordering::SeqCst)
    }

    /// Returns `(inflight_count, activity_seq)` preserving canonical lock order
    /// (`inflight` -> `pending_requests`). When `ignore_long_lived` is true,
    /// `WebSocket` and `EventSource` connections are excluded from the inflight count.
    pub fn inflight_stats(&self, ignore_long_lived: bool) -> (usize, u64) {
        let inflight = self.inflight.lock();
        let seq = self.activity_seq.load(Ordering::SeqCst);
        if !ignore_long_lived {
            return (inflight.len(), seq);
        }
        let pending = self.pending_requests.lock();
        let count = inflight
            .iter()
            .filter(|req_id| {
                pending
                    .get(*req_id)
                    .map(|ev| {
                        let rt = ev.resource_type.to_ascii_lowercase();
                        rt != "websocket" && rt != "eventsource"
                    })
                    .unwrap_or(true)
            })
            .count();
        (count, seq)
    }

    pub fn find_latest_status_for_url(&self, url: &str) -> Option<u16> {
        let net = self.network.lock();
        net.buf
            .iter()
            .rev()
            .find(|ev| ev.url == url && ev.status.is_some())
            .and_then(|ev| ev.status)
    }

    pub fn snapshot_all(&self) -> TelemetrySnapshot {
        let (console_logs, dropped_console) = {
            let c = self.console.lock();
            (c.snapshot(), c.dropped_count())
        };
        let (page_errors, dropped_page_errors) = {
            let p = self.page_errors.lock();
            (p.snapshot(), p.dropped_count())
        };
        let (mut network_events, dropped_network) = {
            let n = self.network.lock();
            (n.snapshot(), n.dropped_count())
        };
        network_events.extend(self.pending_requests.lock().values().cloned());
        let inflight_count = self.inflight.lock().len();
        TelemetrySnapshot {
            console_logs,
            page_errors,
            network_events,
            inflight_count,
            dropped_console,
            dropped_page_errors,
            dropped_network,
        }
    }

    pub fn push_synthetic_network(&self, url: &str, method: &str, status: u16) {
        self.network.lock().push(NetworkEvent {
            timestamp_ms: now_ms(),
            request_id: format!("synthetic-{}", now_ms()),
            url: url.to_string(),
            method: method.to_string(),
            resource_type: "Other".to_string(),
            status: Some(status),
            mime_type: None,
            failed: false,
            error_text: None,
            request_headers: BTreeMap::new(),
            response_headers: BTreeMap::new(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_evicts_oldest_and_counts_dropped() {
        let mut rb = RingBuffer::new(3);
        for i in 1..=5 {
            rb.push(i);
        }
        assert_eq!(rb.len(), 3);
        assert_eq!(rb.total_seen(), 5);
        assert_eq!(rb.dropped_count(), 2);
        assert_eq!(rb.snapshot(), vec![3, 4, 5]);
        assert_eq!(rb.drain(), vec![3, 4, 5]);
        assert!(rb.is_empty());
        assert_eq!(rb.dropped_count(), 2);
    }

    #[test]
    fn ring_buffer_zero_capacity_drops_all() {
        let mut rb = RingBuffer::new(0);
        rb.push(42);
        assert_eq!(rb.len(), 0);
        assert_eq!(rb.dropped_count(), 1);
        assert_eq!(rb.total_seen(), 1);
    }

    #[test]
    fn telemetry_store_finalizes_redirects_and_local_intercepts() {
        let store = TelemetryStore::new(TelemetryConfig::with_capacity(10));
        store.on_request_started(
            NetworkEvent {
                timestamp_ms: 1,
                request_id: "req1".to_string(),
                url: "http://127.0.0.1/old".to_string(),
                method: "GET".to_string(),
                resource_type: "Document".to_string(),
                status: None,
                mime_type: None,
                failed: false,
                error_text: None,
                request_headers: BTreeMap::new(),
                response_headers: BTreeMap::new(),
            },
            None,
            None,
            BTreeMap::new(),
        );
        // Redirect hop
        store.on_request_started(
            NetworkEvent {
                timestamp_ms: 2,
                request_id: "req1".to_string(),
                url: "http://127.0.0.1/new".to_string(),
                method: "GET".to_string(),
                resource_type: "Document".to_string(),
                status: None,
                mime_type: None,
                failed: false,
                error_text: None,
                request_headers: BTreeMap::new(),
                response_headers: BTreeMap::new(),
            },
            Some(302),
            None,
            BTreeMap::new(),
        );
        store.on_response_received("req1", Some(200), Some("text/html".into()), BTreeMap::new());
        store.on_request_finished("req1");

        let events = store.snapshot_network();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].status, Some(302));
        assert_eq!(events[1].status, Some(200));
        assert_eq!(store.inflight_count(), 0);

        // Local intercept resolution cleans inflight idempotently
        store.on_request_started(
            NetworkEvent {
                timestamp_ms: 3,
                request_id: "req2".to_string(),
                url: "http://127.0.0.1/blocked".to_string(),
                method: "GET".to_string(),
                resource_type: "Image".to_string(),
                status: None,
                mime_type: None,
                failed: false,
                error_text: None,
                request_headers: BTreeMap::new(),
                response_headers: BTreeMap::new(),
            },
            None,
            None,
            BTreeMap::new(),
        );
        assert_eq!(store.inflight_count(), 1);
        store.on_intercept_resolved_locally(
            "req2",
            None,
            BTreeMap::new(),
            true,
            Some("BlockedByClient".to_string()),
        );
        // Second call (e.g. duplicate event from browser) is a no-op
        store.on_request_failed("req2", Some("NS_ERROR_ABORT".to_string()));
        assert_eq!(store.inflight_count(), 0);
        assert_eq!(store.snapshot_network().len(), 3);
    }

    #[test]
    fn telemetry_store_concurrent_snapshot_all_stress() {
        use std::sync::Arc;

        let store = Arc::new(TelemetryStore::new(TelemetryConfig::with_capacity(64)));
        std::thread::scope(|s| {
            for worker_id in 0..4u64 {
                let st = Arc::clone(&store);
                s.spawn(move || {
                    for iter in 0..1000u64 {
                        let req_id = format!("w{worker_id}-r{}", iter % 16);
                        st.on_request_started(
                            NetworkEvent {
                                timestamp_ms: iter,
                                request_id: req_id.clone(),
                                url: format!("http://127.0.0.1/item/{iter}"),
                                method: "GET".to_string(),
                                resource_type: "XHR".to_string(),
                                status: None,
                                mime_type: None,
                                failed: false,
                                error_text: None,
                                request_headers: BTreeMap::new(),
                                response_headers: BTreeMap::new(),
                            },
                            Some(302),
                            None,
                            BTreeMap::new(),
                        );
                        st.on_response_received(
                            &req_id,
                            Some(200),
                            Some("application/json".to_string()),
                            BTreeMap::new(),
                        );
                        match iter % 3 {
                            0 => st.on_request_finished(&req_id),
                            1 => st.on_request_failed(&req_id, Some("err".to_string())),
                            _ => st.on_intercept_resolved_locally(
                                &req_id,
                                Some(200),
                                BTreeMap::new(),
                                false,
                                None,
                            ),
                        }
                        st.push_console(ConsoleEntry {
                            timestamp_ms: iter,
                            level: "log".to_string(),
                            text: format!("c-{iter}"),
                            url: None,
                            line: None,
                        });
                        st.push_page_error(PageError {
                            timestamp_ms: iter,
                            message: format!("e-{iter}"),
                            name: None,
                            stack: None,
                            url: None,
                            line: None,
                            column: None,
                        });
                    }
                });
            }

            for _ in 0..4 {
                let st = Arc::clone(&store);
                s.spawn(move || {
                    for _ in 0..1000 {
                        let snap = st.snapshot_all();
                        assert!(snap.console_logs.len() <= 64);
                        assert!(snap.page_errors.len() <= 64);
                        let _ = st.snapshot_network();
                    }
                });
            }
        });

        assert_eq!(store.inflight_count(), 0);
        let final_snap = store.snapshot_all();
        assert_eq!(final_snap.console_logs.len(), 64);
        assert_eq!(final_snap.page_errors.len(), 64);
        assert_eq!(final_snap.network_events.len(), 64);
        assert!(final_snap.dropped_network > 0);
    }

    #[test]
    fn telemetry_store_activity_seq_and_inflight_stats_ignore_long_lived() {
        let store = TelemetryStore::new(TelemetryConfig::with_capacity(16));
        assert_eq!(store.inflight_stats(true), (0, 0));

        store.on_request_started(
            NetworkEvent {
                timestamp_ms: 1,
                request_id: "ws1".to_string(),
                url: "ws://127.0.0.1/live".to_string(),
                method: "GET".to_string(),
                resource_type: "WebSocket".to_string(),
                status: None,
                mime_type: None,
                failed: false,
                error_text: None,
                request_headers: BTreeMap::new(),
                response_headers: BTreeMap::new(),
            },
            None,
            None,
            BTreeMap::new(),
        );
        store.on_request_started(
            NetworkEvent {
                timestamp_ms: 2,
                request_id: "sse1".to_string(),
                url: "http://127.0.0.1/events".to_string(),
                method: "GET".to_string(),
                resource_type: "EventSource".to_string(),
                status: None,
                mime_type: None,
                failed: false,
                error_text: None,
                request_headers: BTreeMap::new(),
                response_headers: BTreeMap::new(),
            },
            None,
            None,
            BTreeMap::new(),
        );
        assert_eq!(store.inflight_stats(false), (2, 2));
        assert_eq!(store.inflight_stats(true), (0, 2));

        store.on_request_started(
            NetworkEvent {
                timestamp_ms: 3,
                request_id: "xhr1".to_string(),
                url: "http://127.0.0.1/api".to_string(),
                method: "GET".to_string(),
                resource_type: "Fetch".to_string(),
                status: None,
                mime_type: None,
                failed: false,
                error_text: None,
                request_headers: BTreeMap::new(),
                response_headers: BTreeMap::new(),
            },
            None,
            None,
            BTreeMap::new(),
        );
        assert_eq!(store.inflight_stats(true), (1, 3));
        store.on_request_finished("xhr1");
        assert_eq!(store.inflight_stats(true), (0, 4));
        assert_eq!(store.activity_seq(), 4);
    }
}
