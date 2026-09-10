//! Seat self-report primitive (PR-3): A2's independent watchdog channel,
//! A5's serving proof, B6b's producer. Design of record:
//! docs/pr3-seat-self-report.md
//!
//! Fail-soft by construction (Lens constraints): the report path never
//! awaits inside dispatch/turn code. Wakeups go through a bounded channel
//! with `try_send`; a full channel or a down dashboard degrades to
//! drop-and-count, never to a wedged turn loop.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Monotonic log-emission/write counters (PR-1 `drops_total` discipline:
/// lock-free atomics, relaxed ordering — advisory diagnostics, never gates).
#[derive(Default)]
pub struct LogCounters {
    pub emitted_total: AtomicU64,
    /// Events whose tracing target starts with `buzz_acp::pool`.
    pub emitted_pool: AtomicU64,
    /// Other events whose target starts with `buzz_acp` (lib/gate paths).
    pub emitted_lib: AtomicU64,
    /// Lines handed to the writer (post-formatter).
    pub written_lines: AtomicU64,
    /// Bytes handed to the writer (post-formatter).
    pub written_bytes: AtomicU64,
}

impl LogCounters {
    pub fn snapshot(&self) -> EmittedCounts {
        EmittedCounts {
            total: self.emitted_total.load(Ordering::Relaxed),
            pool: self.emitted_pool.load(Ordering::Relaxed),
            lib: self.emitted_lib.load(Ordering::Relaxed),
        }
    }

    pub fn written(&self) -> WrittenCounts {
        WrittenCounts {
            lines: self.written_lines.load(Ordering::Relaxed),
            bytes: self.written_bytes.load(Ordering::Relaxed),
        }
    }
}

/// Target classification shared by the Layer and its tests: `buzz_acp::pool*`
/// buckets to pool, other `buzz_acp*` to lib/gate, everything else counts
/// only toward the total.
pub fn classify_event(target: &str, counters: &LogCounters) {
    counters.emitted_total.fetch_add(1, Ordering::Relaxed);
    if target.starts_with("buzz_acp::pool") {
        counters.emitted_pool.fetch_add(1, Ordering::Relaxed);
    } else if target.starts_with("buzz_acp") {
        counters.emitted_lib.fetch_add(1, Ordering::Relaxed);
    }
}

/// Counts events at the subscriber boundary (post-filter, pre-writer).
pub struct EmissionCounterLayer {
    counters: Arc<LogCounters>,
}

impl<S> tracing_subscriber::Layer<S> for EmissionCounterLayer
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        classify_event(event.metadata().target(), &self.counters);
    }
}

/// Wraps the underlying writer and counts what actually reached `write`.
pub struct CountingMakeWriter<W> {
    counters: Arc<LogCounters>,
    inner: W,
}

impl<W> CountingMakeWriter<W> {
    pub fn new(counters: Arc<LogCounters>, inner: W) -> Self {
        Self { counters, inner }
    }
}

impl<'a, W> tracing_subscriber::fmt::MakeWriter<'a> for CountingMakeWriter<W>
where
    W: tracing_subscriber::fmt::MakeWriter<'a>,
{
    type Writer = CountingWriter<W::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        CountingWriter {
            counters: self.counters.clone(),
            inner: self.inner.make_writer(),
        }
    }
}

pub struct CountingWriter<T> {
    counters: Arc<LogCounters>,
    inner: T,
}

impl<T: Write> Write for CountingWriter<T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        if n > 0 {
            self.counters
                .written_bytes
                .fetch_add(n as u64, Ordering::Relaxed);
            let newlines = buf[..n].iter().filter(|b| **b == b'\n').count() as u64;
            self.written_lines_newlines(newlines);
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<T> CountingWriter<T> {
    fn written_lines_newlines(&mut self, newlines: u64) {
        if newlines > 0 {
            self.counters
                .written_lines
                .fetch_add(newlines, Ordering::Relaxed);
        }
    }
}

#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct EmittedCounts {
    pub total: u64,
    pub pool: u64,
    pub lib: u64,
}

#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct WrittenCounts {
    pub lines: u64,
    pub bytes: u64,
}

#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct Latency {
    pub last_ms: Option<u64>,
    pub p50_window_ms: Option<u64>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct FreezeState {
    /// `ok` | `stalled_write` — emitted advanced while writes stalled,
    /// measured report-to-report.
    pub state: &'static str,
    pub emitted_minus_written: u64,
    pub detected_at_unix: Option<u64>,
}
impl Default for FreezeState {
    fn default() -> Self {
        Self {
            state: "ok",
            emitted_minus_written: 0,
            detected_at_unix: None,
        }
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct SeatStatus {
    /// Agent pubkey hex — unambiguous seat identity for the dashboard.
    pub agent_pubkey: String,
    pub pid: u32,
    pub boot_unix: u64,
    pub seq: u64,
    pub last_authored_event_id: Option<String>,
    pub last_authored_at_unix: Option<u64>,
    pub turns_total: u64,
    pub dispatch_response_latency_ms: Latency,
    pub log_emitted: EmittedCounts,
    pub log_written: WrittenCounts,
    pub freeze: FreezeState,
    pub report_drops_total: u64,
}

struct Inner {
    agent_pubkey: String,
    pid: u32,
    boot_unix: u64,
    seq: u64,
    last_authored: Option<(String, u64)>,
    turns_total: u64,
    last_latency_ms: Option<u64>,
    latency_window: VecDeque<u64>,
    report_drops: u64,
    dispatch_times: HashMap<uuid::Uuid, std::time::Instant>,
    prev_emitted_total: u64,
    prev_written_bytes: u64,
    freeze: FreezeState,
}

/// Shared seat state; the reporter task is its sole publisher.
pub struct SelfReportState {
    counters: Arc<LogCounters>,
    inner: RwLock<Inner>,
}

pub const REPORT_INTERVAL: Duration = Duration::from_secs(60);
pub const REPORT_TIMEOUT: Duration = Duration::from_secs(2);
pub const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);
const LATENCY_WINDOW: usize = 32;
/// Payload cap: the self-report must stay well under what the dashboard
/// already renders (B6b constraint). Hard guard, tested.
pub const PAYLOAD_CAP_BYTES: usize = 4096;

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl SelfReportState {
    pub fn new(agent_pubkey: String, counters: Arc<LogCounters>) -> Arc<Self> {
        Arc::new(Self {
            counters,
            inner: RwLock::new(Inner {
                agent_pubkey,
                pid: std::process::id(),
                boot_unix: now_unix(),
                seq: 0,
                last_authored: None,
                turns_total: 0,
                last_latency_ms: None,
                latency_window: VecDeque::new(),
                report_drops: 0,
                dispatch_times: HashMap::new(),
                prev_emitted_total: 0,
                prev_written_bytes: 0,
                freeze: FreezeState::default(),
            }),
        })
    }

    /// Record a relay event this seat authored (presence, typing, observer,
    /// or an echo of our own pubkey on a subscribed channel). Never blocks.
    pub fn note_published(&self, event_id: String, created_at_unix: u64) {
        if let Ok(mut inner) = self.inner.write() {
            inner.last_authored = Some((event_id, created_at_unix));
            inner.seq += 1;
        }
        wake_reporter();
    }

    /// Record a completed dispatch->response cycle. Never blocks.
    pub fn note_turn_complete(&self, latency_ms: u64) {
        if let Ok(mut inner) = self.inner.write() {
            inner.turns_total += 1;
            inner.last_latency_ms = Some(latency_ms);
            inner.latency_window.push_back(latency_ms);
            if inner.latency_window.len() > LATENCY_WINDOW {
                inner.latency_window.pop_front();
            }
            inner.seq += 1;
        }
    }

    /// Track per-channel dispatch time so results can compute dispatch→response latency.
    pub fn note_dispatched(&self, channel: uuid::Uuid) {
        if let Ok(mut inner) = self.inner.write() {
            inner
                .dispatch_times
                .insert(channel, std::time::Instant::now());
        }
    }

    pub fn note_result(&self, channel: uuid::Uuid) {
        let start = self
            .inner
            .write()
            .ok()
            .and_then(|mut inner| inner.dispatch_times.remove(&channel));
        if let Some(start) = start {
            let latency_ms = start.elapsed().as_millis() as u64;
            self.note_turn_complete(latency_ms);
        }
    }

    fn note_report_drop(&self) {
        if let Ok(mut inner) = self.inner.write() {
            inner.report_drops += 1;
        }
    }

    /// Freeze derivation happens report-to-report: emitted advanced while
    /// written bytes did not ⇒ `stalled_write`.
    pub fn snapshot(&self) -> SeatStatus {
        let emitted = self.counters.snapshot();
        let written = self.counters.written();
        let mut inner = self.inner.write().expect("self-report state lock");
        inner.seq += 1;
        let emitted_delta = emitted.total.saturating_sub(inner.prev_emitted_total);
        let written_delta = written.bytes.saturating_sub(inner.prev_written_bytes);
        let stalled = emitted_delta > 0 && written_delta == 0;
        if stalled {
            if inner.freeze.state != "stalled_write" {
                inner.freeze = FreezeState {
                    state: "stalled_write",
                    emitted_minus_written: emitted.total.saturating_sub(written.lines),
                    detected_at_unix: Some(now_unix()),
                };
            }
        } else {
            inner.freeze = FreezeState::default();
        }
        inner.prev_emitted_total = emitted.total;
        inner.prev_written_bytes = written.bytes;
        let mut window: Vec<u64> = inner.latency_window.iter().copied().collect();
        window.sort_unstable();
        let p50 = if window.is_empty() {
            None
        } else {
            Some(window[window.len() / 2])
        };
        SeatStatus {
            agent_pubkey: inner.agent_pubkey.clone(),
            pid: inner.pid,
            boot_unix: inner.boot_unix,
            seq: inner.seq,
            last_authored_event_id: inner.last_authored.as_ref().map(|(id, _)| id.clone()),
            last_authored_at_unix: inner.last_authored.as_ref().map(|(_, at)| *at),
            turns_total: inner.turns_total,
            dispatch_response_latency_ms: Latency {
                last_ms: inner.last_latency_ms,
                p50_window_ms: p50,
            },
            log_emitted: emitted,
            log_written: written,
            freeze: inner.freeze.clone(),
            report_drops_total: inner.report_drops,
        }
    }
}

// ---- Global accessors (LazyLock precedent: filter.rs) ----

static LOG_COUNTERS: OnceLock<Arc<LogCounters>> = OnceLock::new();
static STATE: OnceLock<Arc<SelfReportState>> = OnceLock::new();
static WAKE_TX: OnceLock<tokio::sync::mpsc::Sender<()>> = OnceLock::new();

/// Fire-and-forget wakeup; a full channel counts as a dropped report wake,
/// never an error on the caller's path.
fn wake_reporter() {
    if let Some(tx) = WAKE_TX.get() {
        if tx.try_send(()).is_err() {
            if let Some(st) = STATE.get() {
                st.note_report_drop();
            }
        }
    }
}

// ---- Reporter ----

pub struct ReporterConfig {
    pub url: String,
    pub token: String,
    pub interval: Duration,
    pub timeout: Duration,
}

/// Token charset per the B6b contract: `[A-Za-z0-9]+`.
pub fn valid_token(token: &str) -> bool {
    !token.is_empty() && token.chars().all(|c| c.is_ascii_alphanumeric())
}

pub fn next_backoff(current: Duration) -> Duration {
    current.saturating_mul(2).min(MAX_BACKOFF)
}

/// One POST. Isolated so tests can drive it against a local listener.
pub async fn publish_once(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    status: &SeatStatus,
) -> Result<(), String> {
    let body = serde_json::to_vec(status).map_err(|e| format!("serialize: {e}"))?;
    if body.len() > PAYLOAD_CAP_BYTES {
        return Err(format!(
            "payload {} bytes exceeds cap {}",
            body.len(),
            PAYLOAD_CAP_BYTES
        ));
    }
    client
        .post(url)
        .timeout(REPORT_TIMEOUT)
        .bearer_auth(token)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("post: {e}"))?
        .error_for_status()
        .map_err(|e| format!("status: {e}"))?;
    Ok(())
}

/// Spawns the fire-and-forget reporter. Called once at boot; disabled (one
/// INFO line) when BUZZ_ACP_DASHBOARD_URL is unset.
pub fn init_from_env(agent_pubkey: String) {
    let counters = LOG_COUNTERS
        .get_or_init(|| Arc::new(LogCounters::default()))
        .clone();
    let state = SelfReportState::new(agent_pubkey, counters.clone());
    let _ = STATE.set(state.clone());

    let url = match std::env::var("BUZZ_ACP_DASHBOARD_URL") {
        Ok(u) if !u.trim().is_empty() => u.trim().to_string(),
        _ => {
            tracing::info!("self-report disabled (BUZZ_ACP_DASHBOARD_URL unset)");
            return;
        }
    };
    let token = std::env::var("BUZZ_ACP_DASHBOARD_TOKEN").unwrap_or_default();
    if !valid_token(&token) {
        tracing::warn!(
            "self-report disabled (BUZZ_ACP_DASHBOARD_TOKEN unset or invalid charset [A-Za-z0-9]+)"
        );
        return;
    }
    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(8);
    let _ = WAKE_TX.set(tx);

    let config = ReporterConfig {
        url,
        token,
        interval: REPORT_INTERVAL,
        timeout: REPORT_TIMEOUT,
    };
    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .unwrap_or_default();
        let mut ticker = tokio::time::interval(config.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut backoff_until: Option<tokio::time::Instant> = None;
        let mut backoff = Duration::from_secs(1);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                wake = rx.recv() => {
                    if wake.is_none() {
                        // Sender dropped (shouldn't happen); interval still drives.
                    }
                }
            }
            if let Some(until) = backoff_until {
                if tokio::time::Instant::now() < until {
                    continue;
                }
                backoff_until = None;
            }
            let status = state.snapshot();
            match publish_once(&client, &config.url, &config.token, &status).await {
                Ok(()) => backoff = Duration::from_secs(1),
                Err(e) => {
                    tracing::warn!("self-report publish failed: {e}");
                    backoff_until = Some(tokio::time::Instant::now() + backoff);
                    backoff = next_backoff(backoff);
                }
            }
        }
    });
}

impl EmissionCounterLayer {
    pub fn new(counters: Arc<LogCounters>) -> Self {
        Self { counters }
    }
}

/// Install (or fetch) the global counters; called at subscriber init.
pub fn init_counters() -> Arc<LogCounters> {
    LOG_COUNTERS
        .get_or_init(|| Arc::new(LogCounters::default()))
        .clone()
}

/// Free-fn wrappers for update sites that hold no handle.
pub fn note_published(event_id: String, created_at_unix: u64) {
    if let Some(st) = STATE.get() {
        st.note_published(event_id, created_at_unix);
    }
}

pub fn note_dispatched(channel: uuid::Uuid) {
    if let Some(st) = STATE.get() {
        st.note_dispatched(channel);
    }
}

pub fn note_result(channel: uuid::Uuid) {
    if let Some(st) = STATE.get() {
        st.note_result(channel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_charset() {
        assert!(valid_token("abcXYZ012"));
        assert!(!valid_token(""));
        assert!(!valid_token("abc-123"));
        assert!(!valid_token("abc 123"));
        assert!(!valid_token("abc.123"));
    }

    #[test]
    fn backoff_doubles_to_cap() {
        let mut d = Duration::from_secs(1);
        let mut seq = vec![d];
        for _ in 0..12 {
            d = next_backoff(d);
            seq.push(d);
        }
        assert_eq!(seq[1], Duration::from_secs(2));
        assert_eq!(seq[10], MAX_BACKOFF);
        assert!(seq[11] == MAX_BACKOFF);
    }

    #[test]
    fn freeze_derivation_report_to_report() {
        let counters = Arc::new(LogCounters::default());
        let state = SelfReportState::new("aa".repeat(32), counters.clone());
        // Baseline report: nothing emitted -> ok.
        assert_eq!(state.snapshot().freeze.state, "ok");
        // Emit without writing -> next snapshot must show stalled_write.
        counters.emitted_total.store(5, Ordering::Relaxed);
        counters.written_bytes.store(0, Ordering::Relaxed);
        let s = state.snapshot();
        assert_eq!(s.freeze.state, "stalled_write");
        assert!(s.freeze.detected_at_unix.is_some());
        // Writes resume -> recovers to ok.
        counters.written_bytes.store(4096, Ordering::Relaxed);
        assert_eq!(state.snapshot().freeze.state, "ok");
    }

    #[test]
    fn emission_classifies_targets() {
        let counters = LogCounters::default();
        classify_event("buzz_acp::pool::agent", &counters);
        classify_event("buzz_acp::pool", &counters);
        classify_event("buzz_acp::gate", &counters);
        classify_event("other_crate::x", &counters);
        assert_eq!(counters.emitted_total.load(Ordering::Relaxed), 4);
        assert_eq!(counters.emitted_pool.load(Ordering::Relaxed), 2);
        assert_eq!(counters.emitted_lib.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn payload_under_cap() {
        let counters = Arc::new(LogCounters::default());
        let state = SelfReportState::new("a".repeat(64), counters.clone());
        let status = state.snapshot();
        let body = serde_json::to_vec(&status).expect("serialize");
        assert!(
            body.len() < PAYLOAD_CAP_BYTES,
            "payload {} >= cap",
            body.len()
        );
    }

    #[tokio::test]
    async fn mock_ingest_round_trip_and_backoff_on_down_endpoint() {
        let counters = Arc::new(LogCounters::default());
        let state = SelfReportState::new("b".repeat(64), counters.clone());
        let status = state.snapshot();

        // Minimal HTTP listener on an ephemeral port.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let srv = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            use std::io::Read;
            let mut buf = [0u8; 8192];
            let n = sock.read(&mut buf).expect("read");
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let ok = b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n";
            sock.write_all(ok).expect("respond");
            req
        });

        let client = reqwest::Client::builder()
            .timeout(REPORT_TIMEOUT)
            .build()
            .expect("client");
        let url = format!("http://127.0.0.1:{port}/api/ingest/seat-status");
        publish_once(&client, &url, "tok123", &status)
            .await
            .expect("publish ok");
        let req = srv.join().expect("server thread");
        assert!(
            req.starts_with("POST /api/ingest/seat-status"),
            "path: {req}"
        );
        assert!(req.contains("authorization: Bearer tok123"), "auth: {req}");

        // Down endpoint: refused connection maps to Err (drives backoff).
        let dead = "http://127.0.0.1:9/api/ingest/seat-status";
        assert!(publish_once(&client, dead, "tok123", &status)
            .await
            .is_err());
    }
}
