# A4 — Clean-Exit Restart (design)

Scope source: PM dispatch 2026-09-10T21:14:58Z — "clean-exit restart mechanics — census re-based
(the one real error-loop, not the noise class), relay-trigger only (no timer-spawn restarts),
detached driver." Branch: `atlas/a4-clean-exit-restart` @ e4909d9b0. Build discipline per PR-3:
no running-seat touches, no deploy before PM GO, Lens HARD review, one doc leading.

## 1. Restart census, re-based

The one real error-loop today: a wedged seat cannot recover itself, so every restart requires an
external actor with host access (atlas via launchctl / bounce.sh; Chief as break-glass for atlas
only). The loop's failure mode is compositional: external actor down or unreachable ⇒ stuck seat
stays stuck (quill, 2.3 days). The noise class — natural bounces, deploy restarts, in-process
harness-child crash/respawn (circuit breaker, lib.rs `SlotCircuit`) — needs no new machinery and
gets none.

Restart tiers after A4:

| Tier | Mechanism | When | Change |
|------|-----------|------|--------|
| 0 (new) | Relay `!restart` from owner | seat reachable on relay, even when dispatch is wedged | kills the error-loop |
| 1 | `launchctl kickstart` (ops, host access) | relay unreachable, host reachable | unchanged |
| 2 | `bounce.sh` (atlas self-bounce) | atlas needs itself bounced | RETIRED by A4; runbook stays break-glass until A4 is fleet-served |
| 3 | Chief break-glass on ATLAS seat | atlas wedged AND owner absent | narrowed: owner-reachable `!restart` replaces the common case |

Explicitly out (ruling, not omission): auto-respawn on abnormal exit (KeepAlive stays unset —
crash-class restarts stay commanded, never automatic); timer-spawn restarts (PM scope);
cross-seat orchestration (one seat restarting another — stays with the census owners).

## 2. Relay trigger: `!restart` control command

New member of the existing owner control-command family (`!shutdown` / `!cancel` / `!rotate`,
lib.rs:2350-2452): kind:9, content exactly `!restart`, from owner, mentioning this agent.
Matched in intake, consumed, never queued, never dispatched to the agent.

- **Placement**: handled immediately after the `!shutdown` arm — BEFORE the inbound author gate,
  same as the rest of the family. Owner check is `owner_cache` equality, stricter than any gate
  mode (the family's existing stance: "owner can always act regardless of gate mode").
- **Why intake, not dispatch**: the queue-stick wedge class leaves the relay read loop and the
  control-command arms alive — every recorded recovery of a stuck seat came through a steer
  processed by intake (chief re-arm captures #1-#4, quill post-resubscribe). A restart that only
  worked from a healthy dispatch loop would miss the exact state it exists for.
- **Wedge caveat, stated honestly**: if the hang ever moves INTO the read loop itself (no
  recorded instance to date), tier-0 dies with it and tier 1/2 remain the answer. The quill
  capture (threads parked, control path live) is the current evidence boundary.
- **Idle vs in-flight**: no special-casing. `!restart` follows the same path as SIGTERM:
  shutdown signal → in-flight drain → exit. A running turn gets its grace budget; nothing is
  cancelled that a TERM wouldn't cancel.

## 3. Clean exit path

`!restart` fires `shutdown_tx` (the existing watch channel, lib.rs:1945-1966 — same consumers as
SIGTERM/ctrl_c). Exit code 0 on the drained path. The drain, transport close, and agent-slot
teardown are the existing shutdown behavior — A4 adds no second shutdown path, it only adds a
new trigger and a driver (§4) before it. One canonical exit sequence, four triggers
(SIGNAL/ctrl_c/!shutdown/!restart); only `!restart` schedules a respawn.

## 4. Detached driver

Spawned BEFORE `shutdown_tx.send(())` — after the send, the drain clock is running and the
executor may die mid-turn (the 9/6 self-bounce trap, lib.rs in-flight drain).

- **Spawn** (as implemented): std-only, no new crates — detachment is
  `CommandExt::process_group(0)`, not a full setsid. The driver leaves the daemon's process
  group so launchd's group-kill of the job misses it; there is no controlling tty to detach
  from. Honest residual: a name-class kill (`killall buzz-acp`) reaches the driver (same
  binary name) — that failure leaves the seat down, tier-1/3 territory, never worse than
  today. Driver = the daemon binary itself invoked as
  `buzz-acp --restart-driver <pid> --label <label|-> --script <start.sh|-> --uid <uid|->`,
  stdio redirected to the seat's acp.log, no key material in argv, no shell.
  Liveness via `/bin/kill -0` (std has no kill(2); one short-lived process per poll, exit
  code only). `kill -0` succeeds on zombies — production parents reap (launchd / the script
  daemonizer), so this is a test-harness concern only, noted in the tests.
- **Driver behavior**: poll `kill(pid, 0)` for parent exit (bounded: drain budget 30s + 90s
  slack, then give up and log-exit — never park forever); on parent exit, respawn per mode:
  `launchd` → `launchctl kickstart gui/<uid>/<label>` (label from env `BUZZ_ACP_LAUNCHD_LABEL`
  where present); `script` → exec the seat's `start.sh` absolute path from env
  `BUZZ_ACP_START_SCRIPT` (atlas seat only today). Verify respawn (process exists after 10s),
  then exit 0. Single-instance: `restart-driver.pid` lock beside the seat log; second spawn
  finds the lock alive and exits silently (bounce.pid lesson).
- **Failure mode**: driver dies or gives up ⇒ seat stays down exactly as a TERM without restart
  would — tier 1/2 recover it. No worse than today; no retry loops (§1's ruling).
- **Boot-integration check** (new, cheap): at startup, if `restart-driver.pid` names a live
  driver, the daemon logs one INFO line "restarted by driver" — the receipt the census reads.

## 5. Config

No new required config. Optional env: `BUZZ_ACP_LAUNCHD_LABEL`, `BUZZ_ACP_START_SCRIPT`
(defaults resolved from the daemon's own launch context where detectable, else the `launchd`
mode is skipped with one WARN). No token, no URL, nothing secret.

## 6. Tests

- Unit: command-match arms — `!restart` owner+mention consumed; non-owner falls through to
  prompt handling (family precedent); wrong content/kind/no-mention not consumed. Driver-flag
  parse (`--restart-driver`) rejects malformed invocations.
- Driver: spawn/poll/respawn exercised against a scratch sleep-process, not a live seat
  (PR-3's injected-client pattern — no fleet dependency in tests). Lock contention arm.
- Suite targets unchanged: acp lib, self_report, cli, clippy, fmt. Full-workspace with
  `--no-fail-fast`, disclosed flakes listed (standing).

## 7. Security

Owner-only pre-gate (family stance); the command never reaches the agent or the queue; driver
argv carries no secrets; WARN/INFO lines log label+pid, never env. A `!restart` from any other
author is an ordinary message (dispatch-gated like any other).

## 8. Rollout

Rides the next post-merge rebuild + wave; no dedicated bounces. Post-rollout census: tier-2
bounce.sh retired after the fleet serves A4 (runbook moves to break-glass appendix); tier-3
narrowed to owner-absent cases. First `!restart` use in anger = a #ops receipt with the
boot-integration line as evidence.

## 9. Rulings — RESOLVED by PM 2026-09-10T23:15:09Z

- **R1 — author set for `!restart`**: owner-only (family precedent; rides NO profile-tag
  surface — the A6 sibling-tag class is exactly the fragile thing we should not put restart
  authority on) vs owner+siblings (Chief/PM gain wedge-rescue without host access). My
  recommendation: **owner-only v1**; widen only after the attestation mesh is stable. If PM
  wants sibling reach sooner, the widening is a one-line change + tests.
- **R2 — default state**: enabled by default (owner-gated, no config burden) vs opt-in env
  flag for a cautious first wave. Recommendation: enabled by default.
- **R3 = retire bounce.sh at fleet-serve, runbook → break-glass appendix.** Scope requirement
  adopted: the appendix explicitly OWNS the residual the in-binary path cannot reach —
  binary DEAD (not wedged: intake not processing at all). That case stays host-side
  launchctl/start.sh territory permanently, documented as the named break-glass trigger,
  not a footnote. GUIDES/AGENT_SELF_BOUNCE.md gains the A4 section + the named trigger.

Rider on R1, carried with the design: a widening revisit tied to tag provisioning —
post-attestation, owner+siblings is the right end state, and the widening is a one-line
change plus tests (the owner check in the `!restart` arm widens to the sibling set).

## 10. Out of scope

Timer-based or schedule-based restarts; automatic crash respawn; cross-seat restart authority;
restart of the relay/dashboard services (those are launchd labels with their own story).
