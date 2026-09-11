//! A4 — clean-exit restart: detached driver + spawn helper.
//!
//! `!restart` (owner control command, matched in intake before the author
//! gate — see lib.rs) initiates the one canonical shutdown path. Before the
//! shutdown signal fires, a short-lived **detached driver** is spawned that
//! outlives the daemon, waits for it to exit, and respawns it exactly once.
//!
//! Design constraints (docs/a4-clean-exit-restart.md):
//! - std-only, no new crates: detachment is `process_group(0)`, not a full
//!   `setsid` — the driver leaves the daemon's process group so launchd's
//!   group-kill of the job misses it, and there is no controlling tty in play.
//!   Residual, named honestly: a name-class kill (`killall buzz-acp`) still
//!   reaches the driver (same binary name); that failure leaves the seat down,
//!   which is tier-1/3 territory (host-side restart), never worse than today.
//! - Bounded: 30s drain budget + 90s slack. A driver that outlives its bound
//!   gives up, logs, and exits — no retry loops, no timers, no auto-respawn
//!   (ruled out: crash-class restarts stay commanded).
//! - Fail-soft: no respawn target configured, spawn failure, or give-up all
//!   degrade to "clean exit, seat stays down" — identical to `!shutdown`.
//! - Liveness is `/bin/kill -0`, which succeeds on zombies: the parent must
//!   be reaped for the driver to see it gone. Production parents reap
//!   (launchd for label seats, the script daemonizer otherwise); the
//!   unreaped-parent case is a test-harness concern, noted in the tests.

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const DRIVER_PID_FILE: &str = "restart-driver.pid";
const DRIVER_RECEIPT_FILE: &str = "restart-driver.receipt";
const DRIVER_ARG: &str = "--restart-driver";

/// Drain budget the daemon may take to exit (matches the in-flight drain),
/// plus slack for transport close and teardown.
const PARENT_EXIT_BUDGET: Duration = Duration::from_secs(120);
const RESPAWN_VERIFY_WAIT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Where the respawn target comes from, resolved at spawn time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RespawnTarget {
    /// launchd-managed seat: `launchctl kickstart gui/<uid>/<label>`.
    Launchd { uid: String, label: String },
    /// Script-started seat (atlas today): exec the seat's start.sh.
    Script(PathBuf),
    /// Neither env var set — driver would be pointless; caller logs and skips.
    None,
}

/// Pure restart decision for the `!restart` owner control command (A4, R1
/// owner-only per PM 2026-09-10T23:15:09Z). Consumed iff the event shape-matches
/// the control family — kind:9 (`KIND_STREAM_MESSAGE`), content exactly
/// `!restart` (trim semantics shared with `is_owner_control_command`), mentions
/// THIS agent — AND the resolved owner equals the event author.
///
/// Missing owner (`owner_cache` empty) is FAIL-CLOSED: returns `false`, the
/// event falls through to normal prompt handling — a restart without a
/// verified owner is not authorized. Non-owner `!restart` likewise falls
/// through: an ordinary message, family precedent.
///
/// Kept pure (no state, no I/O) so the four table arms in the tests are the
/// complete coverage — the mutation probe that motivated this function
/// (Lens A4 review F1, 2026-09-11T01:22:33Z) must never pass again.
pub fn restart_decision(
    event: &nostr::Event,
    kind_u32: u32,
    agent_pubkey_hex: &str,
    owner: Option<&str>,
) -> bool {
    kind_u32 == buzz_core::kind::KIND_STREAM_MESSAGE
        && event.content.trim() == "!restart"
        && crate::event_mentions_agent(event, agent_pubkey_hex)
        && owner == Some(event.pubkey.to_hex().as_str())
}

/// Resolve the respawn target from the daemon's environment. Precedence:
/// launchd label wins (fleet default posture), then start script.
pub fn resolve_target() -> RespawnTarget {
    if let Ok(label) = std::env::var("BUZZ_ACP_LAUNCHD_LABEL") {
        if !label.trim().is_empty() {
            // std has no getuid(); resolve once at spawn time and carry it in argv.
            // N4: $UID is a shell variable, rarely exported into a
            // launchd/spawned env — the `id -u` fallback below is the real
            // path in practice; the env read is just a cheap first try.
            let uid = std::env::var("UID").unwrap_or_else(|_| {
                Command::new("id")
                    .arg("-u")
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default()
            });
            if !uid.is_empty() {
                return RespawnTarget::Launchd { uid, label };
            }
        }
    }
    if let Ok(script) = std::env::var("BUZZ_ACP_START_SCRIPT") {
        if !script.trim().is_empty() {
            return RespawnTarget::Script(PathBuf::from(script));
        }
    }
    RespawnTarget::None
}

/// Spawn the detached driver for `parent_pid`. The daemon calls this BEFORE
/// firing the shutdown signal — after that, the drain clock is running and
/// the executor may die mid-turn (the 9/6 self-bounce trap).
///
/// Returns `Ok(true)` if a driver was spawned, `Ok(false)` if no respawn
/// target is configured (caller should still exit cleanly), `Err` on spawn
/// failure (same caller behavior — never block the restart on the driver).
pub fn spawn_restart_driver(parent_pid: u32) -> std::io::Result<bool> {
    let target = resolve_target();
    let (label, script) = match &target {
        RespawnTarget::None => return Ok(false),
        RespawnTarget::Launchd { label, .. } => (label.clone(), "-".to_string()),
        RespawnTarget::Script(p) => ("-".to_string(), p.display().to_string()),
    };
    let uid = match &target {
        RespawnTarget::Launchd { uid, .. } => uid.clone(),
        _ => "-".to_string(),
    };
    let exe = std::env::current_exe()?;
    // The driver appends to the seat's acp.log (daemon cwd is the seat dir),
    // so its few lines land where the census reads them. Opening the file
    // here (pre-spawn) means the fd is valid even if the daemon dies first.
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("acp.log")?;
    let mut cmd = Command::new(exe);
    cmd.arg(DRIVER_ARG)
        .arg(parent_pid.to_string())
        .arg("--label")
        .arg(&label)
        .arg("--script")
        .arg(&script)
        .arg("--uid")
        .arg(&uid)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        // Detach from the daemon's process group: launchd group-kills the
        // job's group on TERM; a fresh group keeps the driver out of it.
        // (Not a full setsid — no controlling tty exists to detach from;
        // see module docs for the honest residual.)
        .process_group(0);
    // Intentionally never waited on: the driver must outlive this process.
    #[allow(clippy::zombie_processes)]
    let _child = cmd.spawn()?;
    Ok(true)
}

/// Entry point for `buzz-acp --restart-driver …` invocations. Parses argv,
/// runs the bounded poll → single respawn → verify → receipt loop, and
/// returns a process exit code. Synchronous by design: no runtime, no relay,
/// no config — the driver does as little as possible.
pub fn run_driver_from_args() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut pid: Option<u32> = None;
    let mut label = String::from("-");
    let mut script = String::from("-");
    let mut uid = String::from("-");
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            DRIVER_ARG => {
                if i + 1 >= args.len() {
                    eprintln!("restart-driver: missing pid after {DRIVER_ARG}");
                    return 2;
                }
                pid = args[i + 1].parse().ok();
                i += 2;
            }
            "--label" if i + 1 < args.len() => {
                label = args[i + 1].clone();
                i += 2;
            }
            "--script" if i + 1 < args.len() => {
                script = args[i + 1].clone();
                i += 2;
            }
            "--uid" if i + 1 < args.len() => {
                uid = args[i + 1].clone();
                i += 2;
            }
            _ => {
                eprintln!("restart-driver: unexpected arg {}", args[i]);
                return 2;
            }
        }
    }
    let Some(pid) = pid else {
        eprintln!("restart-driver: no valid pid");
        return 2;
    };
    driver_main(
        pid,
        &label,
        &script,
        &uid,
        Path::new("."),
        POLL_INTERVAL,
        PARENT_EXIT_BUDGET,
        RESPAWN_VERIFY_WAIT,
    )
}

/// The driver body, factored for tests (interval/bound injectable, cwd under
/// test control). Runs in the daemon's (soon dead) cwd — the seat dir — where
/// the pid lock and receipt live beside acp.log.
#[allow(clippy::too_many_arguments)]
fn driver_main(
    parent_pid: u32,
    label: &str,
    script: &str,
    uid: &str,
    workdir: &Path,
    poll: Duration,
    budget: Duration,
    verify_budget: Duration,
) -> i32 {
    // Single-instance lock: a live prior driver owns the restart; exit quietly.
    if acquire_lock(&workdir.join(DRIVER_PID_FILE)) {
        eprintln!(
            "restart-driver: another driver holds {DRIVER_PID_FILE} — exiting without respawn"
        );
        return 0;
    }
    let _lock = LockGuard::new(workdir.join(DRIVER_PID_FILE)); // removed on drop

    // Phase 1 — bounded wait for parent exit.
    let start = Instant::now();
    loop {
        if !process_alive(parent_pid) {
            break;
        }
        if start.elapsed() >= budget {
            eprintln!(
                "restart-driver: parent {parent_pid} still alive after {:?}s — giving up (seat stays up; no respawn)",
                budget.as_secs()
            );
            return 1;
        }
        std::thread::sleep(poll);
    }

    // Phase 2 — single respawn launch. The launch call only fires the
    // mechanism; a slow launchd respawn or a start.sh still daemonizing
    // means the new pid appears LATE (Lens N1, adopted: poll, don't
    // parse-once).
    if label != "-" {
        if let Err(msg) = kickstart_launchd(uid, label) {
            eprintln!("restart-driver: respawn failed — {msg}");
            return 1;
        }
    } else if script != "-" {
        if let Err(msg) = exec_start_script(script, workdir) {
            eprintln!("restart-driver: respawn failed — {msg}");
            return 1;
        }
    } else {
        eprintln!("restart-driver: no respawn target in argv — nothing to do");
        return 1;
    }

    // Phase 3 — poll for the respawned pid across the whole verify budget.
    // Success = a pid that is ALIVE (and, in script mode, not the dead
    // parent: a stale acp.pid naming the old daemon must not read as the
    // respawn). A respawn slower than the budget misreports failure and
    // drops the receipt while the seat still comes up — the census line is
    // the only loss (documented, doc §4).
    match wait_for_respawn_pid(label, script, workdir, uid, parent_pid, poll, verify_budget) {
        Ok(new_pid) => {
            write_receipt(&workdir.join(DRIVER_RECEIPT_FILE), parent_pid, new_pid);
            eprintln!("restart-driver: respawned as pid {new_pid} (was {parent_pid})");
            0
        }
        Err(msg) => {
            eprintln!("restart-driver: respawn failed — {msg}");
            1
        }
    }
}

/// Poll for the respawned process across `budget` at `poll` cadence.
/// launchd mode: the `pid = N` line of `launchctl print`, pid alive.
/// script mode: `acp.pid` parses to a live pid that is not the dead parent
/// (stale-pidfile guard — the old daemon's pid sits in the file until the
/// start script overwrites it).
fn wait_for_respawn_pid(
    label: &str,
    script: &str,
    workdir: &Path,
    uid: &str,
    parent_pid: u32,
    poll: Duration,
    budget: Duration,
) -> Result<u32, String> {
    let start = Instant::now();
    loop {
        let found = if label != "-" {
            launchctl_print_pid(uid, label).filter(|p| process_alive(*p))
        } else if script != "-" {
            fs::read_to_string(workdir.join("acp.pid"))
                .ok()
                .and_then(|c| c.trim().parse::<u32>().ok())
                .filter(|p| *p != parent_pid && process_alive(*p))
        } else {
            None
        };
        if let Some(pid) = found {
            return Ok(pid);
        }
        if start.elapsed() >= budget {
            return Err("no live respawned pid within verify budget".to_string());
        }
        std::thread::sleep(poll);
    }
}

/// True if a *different* live driver already holds the lock file.
fn acquire_lock(path: &Path) -> bool {
    if let Ok(content) = fs::read_to_string(path) {
        if let Ok(pid) = content.trim().parse::<u32>() {
            if pid != std::process::id() && process_alive(pid) {
                return true;
            }
        }
    }
    let _ = fs::write(path, format!("{}\n", std::process::id()));
    false
}

struct LockGuard(PathBuf);

impl LockGuard {
    fn new(path: PathBuf) -> Self {
        LockGuard(path)
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Liveness via `/bin/kill -0` — std has no kill(2) and the crate carries no
/// libc (no-new-crates discipline). One short-lived process per poll, no
/// secrets, exit code is the only signal read.
fn process_alive(pid: u32) -> bool {
    Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn kickstart_launchd(uid: &str, label: &str) -> Result<(), String> {
    let status = Command::new("launchctl")
        .args(["kickstart", &format!("gui/{uid}/{label}")])
        .status()
        .map_err(|e| format!("launchctl spawn: {e}"))?;
    if !status.success() {
        return Err(format!(
            "launchctl kickstart exited {}",
            status.code().unwrap_or(-1)
        ));
    }
    Ok(())
}

/// The job's pid from `launchctl print`, if the line is present yet.
fn launchctl_print_pid(uid: &str, label: &str) -> Option<u32> {
    let out = Command::new("launchctl")
        .arg("print")
        .arg(format!("gui/{uid}/{label}"))
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().find_map(|l| {
        let l = l.trim();
        l.strip_prefix("pid = ")
            .and_then(|v| v.trim().parse::<u32>().ok())
    })
}

fn exec_start_script(script: &str, workdir: &Path) -> Result<(), String> {
    let path = Path::new(script);
    if !path.exists() {
        return Err(format!("start script not found: {script}"));
    }
    // The start scripts are their own daemonizers (pidfile + background);
    // run them detached the same way the driver itself is. current_dir pins
    // the pidfile's directory for scripts that don't self-cd (the real seat
    // scripts do — cd "$AGENT_DIR" — but the driver shouldn't depend on it).
    // The pid itself is polled later (wait_for_respawn_pid), not read here:
    // the script may still be daemonizing when this returns (N1).
    Command::new(path)
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .status()
        .map_err(|e| format!("start script spawn: {e}"))?;
    Ok(())
}

fn write_receipt(path: &Path, old_pid: u32, new_pid: u32) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = fs::write(path, format!("{ts} {old_pid} {new_pid}\n"));
}

/// Boot-integration receipt: called at daemon startup; if the previous boot
/// was a driver restart, log it once and consume the marker.
pub fn log_boot_receipt() {
    // Daemon cwd is the seat dir; the receipt lives beside acp.log.
    log_boot_receipt_at(Path::new(DRIVER_RECEIPT_FILE));
}

fn log_boot_receipt_at(path: &Path) {
    if let Ok(content) = fs::read_to_string(path) {
        let c = content.trim();
        if !c.is_empty() {
            tracing::info!(receipt = %c, "restarted by driver");
        }
        let _ = fs::remove_file(path);
    }
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("a4-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lock_blocks_only_live_foreign_driver() {
        let dir = scratch("lock");
        let lock = dir.join(DRIVER_PID_FILE);
        // Own pid in the lock: not blocking (stale/self).
        fs::write(&lock, format!("{}\n", std::process::id())).unwrap();
        assert!(!acquire_lock(&lock));
        // A live foreign pid: blocking.
        // Intentionally not waited on here (killed at test end).
        #[allow(clippy::zombie_processes)]
        let sleeper = Command::new("sleep").arg("30").spawn().unwrap();
        fs::write(&lock, format!("{}\n", sleeper.id())).unwrap();
        assert!(acquire_lock(&lock));
        let _ = Command::new("kill").arg(sleeper.id().to_string()).output();
    }

    #[test]
    fn driver_poll_respawn_verify_end_to_end_script_mode() {
        let dir = scratch("e2e");

        // Fake parent that exits on its own shortly. A reaper thread waits
        // on it so the exit is reaped — kill -0 succeeds on zombies, and an
        // unreaped child would read "alive" forever (production parents
        // reap: launchd for label seats, the script daemonizer otherwise).
        let mut parent = Command::new("sleep").arg("2").spawn().unwrap();
        let parent_pid = parent.id();
        let _reaper = std::thread::spawn(move || {
            let _ = parent.wait();
        });

        // Fake start.sh: writes acp.pid with a long-lived marker process id,
        // under the driver's workdir (start.sh contract: pidfile beside logs).
        #[allow(clippy::zombie_processes)]
        let marker = Command::new("sleep").arg("60").spawn().unwrap();
        let marker_pid = marker.id();
        let start_sh = dir.join("start.sh");
        fs::write(
            &start_sh,
            format!("#!/bin/sh\necho {} > acp.pid\n", marker.id()),
        )
        .unwrap();
        fs::set_permissions(&start_sh, fs::Permissions::from_mode(0o755)).unwrap();

        let code = driver_main(
            parent_pid,
            "-",
            start_sh.to_str().unwrap(),
            "-",
            &dir,
            Duration::from_millis(100),
            Duration::from_secs(10),
            Duration::from_secs(10),
        );
        assert_eq!(code, 0, "driver should succeed");
        let receipt = fs::read_to_string(dir.join(DRIVER_RECEIPT_FILE)).unwrap();
        let parts: Vec<&str> = receipt.trim().split(' ').collect();
        assert_eq!(parts.len(), 3, "receipt = ts old new");
        assert_eq!(parts[1], parent_pid.to_string());
        assert_eq!(parts[2], marker_pid.to_string());
        // Lock released on drop.
        assert!(!dir.join(DRIVER_PID_FILE).exists());
        let _ = Command::new("kill").arg(marker_pid.to_string()).output();
    }

    #[test]
    fn driver_gives_up_when_parent_never_exits() {
        let dir = scratch("bound");
        #[allow(clippy::zombie_processes)]
        let parent = Command::new("sleep").arg("60").spawn().unwrap();
        let code = driver_main(
            parent.id(),
            "-",
            "-",
            "-",
            &dir,
            Duration::from_millis(50),
            Duration::from_millis(300),
            Duration::from_secs(10),
        );
        assert_eq!(code, 1, "bound hit with parent alive must give up");
        assert!(
            !dir.join(DRIVER_RECEIPT_FILE).exists(),
            "no receipt on give-up"
        );
        let _ = Command::new("kill").arg(parent.id().to_string()).output();
    }

    #[test]
    fn lock_guard_releases_on_drop() {
        let dir = scratch("guard");
        let lock = dir.join(DRIVER_PID_FILE);
        fs::write(&lock, "stale\n").unwrap();
        {
            let _g = LockGuard::new(lock.clone());
        }
        assert!(!lock.exists(), "guard drop removes the lock");
    }

    // ── F1 table arms: restart_decision (owner-only gate, fail-closed) ──
    // The mutation probe that motivated these (Lens A4 review F1, inverted
    // owner equality, suite byte-identical) must never pass again: each arm
    // pins one disjunct of the decision.
    fn make_event(kind: u32, content: &str, p_hex: Option<&str>) -> nostr::Event {
        use nostr::{EventBuilder, Keys, Kind, Tag};
        let keys = Keys::generate();
        let tags = match p_hex {
            Some(hex) => vec![Tag::parse(["p", hex]).expect("p tag")],
            None => vec![],
        };
        EventBuilder::new(Kind::Custom(kind as u16), content)
            .tags(tags)
            .sign_with_keys(&keys)
            .unwrap()
    }

    #[test]
    fn restart_decision_owner_mention_is_consumed() {
        let agent = "ab".repeat(32);
        let event = make_event(9, "!restart", Some(&agent));
        let owner = event.pubkey.to_hex();
        assert!(restart_decision(&event, 9, &agent, Some(&owner)));
    }

    #[test]
    fn restart_decision_non_owner_falls_through() {
        let agent = "ab".repeat(32);
        let event = make_event(9, "!restart", Some(&agent));
        let someone_else = "cd".repeat(32);
        assert!(!restart_decision(&event, 9, &agent, Some(&someone_else)));
    }

    #[test]
    fn restart_decision_wrong_shape_not_consumed() {
        let agent = "ab".repeat(32);
        let event = make_event(9, "!restart", Some(&agent));
        let owner = event.pubkey.to_hex();
        // wrong kind
        assert!(!restart_decision(&event, 1, &agent, Some(&owner)));
        // wrong content
        let wrong_content = make_event(9, "!shutdown", Some(&agent));
        assert!(!restart_decision(&wrong_content, 9, &agent, Some(&owner)));
        // no mention of this agent
        let no_mention = make_event(9, "!restart", Some(&"ef".repeat(32)));
        assert!(!restart_decision(&no_mention, 9, &agent, Some(&owner)));
        // trimmed content still matches (family trim semantics) — note the
        // owner must be PADDED's author: make_event signs with fresh keys.
        let padded = make_event(9, "  !restart  ", Some(&agent));
        let padded_owner = padded.pubkey.to_hex();
        assert!(restart_decision(&padded, 9, &agent, Some(&padded_owner)));
    }

    #[test]
    fn restart_decision_missing_owner_fail_closed() {
        let agent = "ab".repeat(32);
        let event = make_event(9, "!restart", Some(&agent));
        assert!(!restart_decision(&event, 9, &agent, None));
    }

    #[test]
    fn driver_verify_budget_expiry_reports_failure_no_receipt() {
        let dir = scratch("verify-budget");
        // Parent dies fast; the start script NEVER writes a live acp.pid
        // (writes a dead stale pid = the old daemon's, exercising the
        // stale-pidfile guard too).
        let mut parent = Command::new("sleep").arg("1").spawn().unwrap();
        let parent_pid = parent.id();
        let _reaper = std::thread::spawn(move || {
            let _ = parent.wait();
        });
        let start_sh = dir.join("start.sh");
        fs::write(&start_sh, "#!/bin/sh\necho 1 > acp.pid\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&start_sh, fs::Permissions::from_mode(0o755)).unwrap();

        let code = driver_main(
            parent_pid,
            "-",
            start_sh.to_str().unwrap(),
            "-",
            &dir,
            Duration::from_millis(50),
            Duration::from_secs(5),
            Duration::from_millis(400),
        );
        assert_eq!(code, 1, "no live respawn pid within budget must fail");
        assert!(
            !dir.join(DRIVER_RECEIPT_FILE).exists(),
            "no receipt on verify failure"
        );
    }

    #[test]
    fn boot_receipt_consumed_once() {
        let dir = scratch("receipt");
        let receipt = dir.join(DRIVER_RECEIPT_FILE);
        fs::write(&receipt, "123 456 789\n").unwrap();
        log_boot_receipt_at(&receipt); // logs + removes
        assert!(!receipt.exists());
        log_boot_receipt_at(&receipt); // second call is a clean no-op
    }
}
