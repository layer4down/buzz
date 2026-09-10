# PR-3 Design: Seat Self-Report — one primitive for A2 (log-freeze watchdog), A5 (serving proof), B6b (producer)

Spec: PLANS/SPRINT3_SCOPE.md v3.2 rows A2/A5/B6b. Status: design-of-record for the PR; Lens reviews per the HARD rule.

## The one primitive
Every buzz-acp daemon maintains an in-memory `SeatStatus` and publishes it over an
INDEPENDENT channel: fire-and-forget HTTP POST to the dashboard ingest endpoint.
One producer serves three tickets: B6b (the self-report itself), A2 (the watchdog
channel that cannot ride the frozen log path), A5 (the serving signal + latency).
A4 (down-class restarts) consumes the same primitive later.

## Why HTTP-to-dashboard (alternatives rejected)
- Log-derived signals: dead by incident class (9/6 fleet audit, kernel-proven;
  ratified rule: log silence is non-evidence). The watchdog never reads the log
  it guards — not even a stat dependency in the authoritative path.
- Relay observer frames (observer.rs): independent and exists, but carry
  process-external events, not seat internals (pid, latency, counters); the
  dashboard already renders activity_log-shaped data (B6a deployed) and is the
  natural sink for a row the seat itself attests.
- reqwest is already a dependency (channel discovery) — zero new deps.

## Components
1. `self_report.rs` (new): `SeatStatus { agent, pid, boot_unix, seq,
   last_authored_event_id, last_authored_at_unix, turns_total,
   dispatch_response_latency_ms { last, p50_window },
   log_emitted { total, by_target { pool, lib, other } },
   log_written { lines, bytes },
   freeze { state: ok|stalled_write, emitted_minus_written, detected_at_unix },
   report_drops_total }`. RwLock snapshot; single reporter task owns publication.
2. Emission counting: custom `Layer` counting `on_event` by target prefix
   (`buzz_acp::pool…` vs other). Composes into the existing
   `tracing_subscriber::fmt().with_env_filter(…).compact().init()` at lib.rs:1567.
3. Write counting: custom `MakeWriter` wrapping the default writer; counts
   write() invocations + bytes. HONEST LIMIT: per-target write attribution is
   not available at this boundary (bytes only), so seat-side DETECTION is total
   write-stall only (`stalled_write`: emitted advanced while written bytes did
   not, measured report-to-report). Pool-scoped loss — the 9/9 class, pool
   lines stalling while lib/gate lines keep landing — does NOT set the flag:
   it is INSTRUMENTED, not detected. The payload carries per-target emission
   counters (`log_emitted.pool` beside `log_emitted.lib`) and written lines, so
   the pool-emitting-while-written-advances pattern is observable by
   consumers/forensics, with disk confirmation staying post-hoc. A seat-side
   pool-loss boolean is unsound at this boundary (bytes-only attribution;
   multi-line events break gap-growth heuristics) and is deliberately absent —
   revisit only with per-target write attribution or a demonstrated consumer
   need, and then as an explicitly-heuristic field never conflated with
   `stalled_write`.
4. Reporter task: interval 60s + immediate flush on last-authored change;
   bounded channel with try_send (full → drop + report_drops_total++);
   POST 2s hard timeout; token from BUZZ_ACP_DASHBOARD_TOKEN validated
   `[A-Za-z0-9]+`; URL from BUZZ_ACP_DASHBOARD_URL (unset → producer disabled,
   logged once at boot); backoff 2^n to 15m cap; no await on the report path
   anywhere in dispatch/turn code (Lens fail-soft constraint).
5. Update points: last-authored set at the relay publish-ack site (relay.rs);
   latency = dispatch intake ts → turn response, recorded at turn completion;
   counters in-memory only (survive the freeze by construction, drops_total
   discipline from PR-2).

## A5 sidecar fields (schema v1 FROZEN — additive only)
`agents[]` entries gain: `last_event {id, at}`, `serving` (recency-derived),
`dispatch_response_latency_ms`, `freeze_state`. `log_age`/`log_state` REMAIN
emitted but marked deprecated; T7 stops consuming them; dropping them is a
schema-v2 decision with Forge, not unilateral.

## Endpoint contract (producer side here; dashboard route = Forge follow-on)
`POST {dashboard}/api/ingest/seat-status`, `Authorization: Bearer <token>`,
body = SeatStatus JSON. Idempotent per (agent, seq). Payload capped at what the
dashboard already renders (B6b constraint).

Security note: the Bearer token travels cleartext when the URL is non-local
`http://` — the producer validates charset, not scheme. Point
BUZZ_ACP_DASHBOARD_URL at the tailnet `https://` front (serve 8446) or
loopback in production; that is an operator obligation, not enforced here.

## Proof obligations (tests)
- channel-full → drop + counter, zero awaits added to turn path
- endpoint down → backoff sequence bounded, recovery on success
- token charset rejection; missing URL → disabled producer, single boot log
- freeze simulation: emission advances, writer stalls → stalled_write ≤ 2 intervals
  (derivation-level guarantee: the flag computes on the first snapshot after
  onset; the reporter loop's interval timing itself is tokio's and untested here)
- mock ingest round-trip; payload size cap enforced

## Split criteria (stated per PM: don't force)
If review finds the Layer/MakeWriter wrappers too intrusive for the hot path,
split is: (a) producer + B6b/A5 fields without counters ships first;
(b) counter instrumentation follows with a benchmark. Reason goes in the PR body
if exercised.
