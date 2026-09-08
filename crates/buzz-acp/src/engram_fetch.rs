//! Fetch the agent's NIP-AE `core` engram at session creation and render it
//! into a prompt section.
//!
//! Scope per Tyler's spec:
//! - Fire one synchronous query for the core head when a *new* session is born.
//! - If a body is found, emit `[Agent Memory — core]\n<profile>`.
//! - If no body is found, emit an onboarding nudge so the agent learns how
//!   to set its own core.
//! - On any *error* (transport, parse), log and emit nothing. We must not
//!   mistake a relay outage for "no core" — that would invite the agent to
//!   overwrite real, just-unreachable memory with a fresh profile.
//! - Either way, session creation is never blocked.

use buzz_core::engram::{conversation_key, d_tag, select_head, validate_and_decrypt, Body};
use buzz_core::kind::KIND_AGENT_ENGRAM;
use nostr::{Event, Keys, PublicKey};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::relay::RestClient;

/// Section header rendered into the prompt.
const SECTION_LABEL: &str = "Agent Memory — core";

/// Onboarding nudge for new agents with no core yet.
///
/// Wording is from Tyler's brief: "No core memory found. Use `buzz mem`
/// to create a core memory. Ask your user about yourself."
pub const ONBOARDING_NUDGE: &str = "No core memory found. \
Use `buzz mem set core \"…\"` to create one (it will hold your identity, \
rules, and goals across sessions). Ask your user about yourself.";

/// Build the rendered prompt section for the agent's core.
///
/// Returns:
/// - `Some(profile_section)` when a valid core exists,
/// - `Some(nudge_section)` when the relay confirmed absence,
/// - `None` when the fetch failed (transport, parse, decrypt) — the caller
///   should inject no section in that case so the agent doesn't conclude
///   memory is empty.
pub async fn build_core_section(
    rest: &RestClient,
    agent_keys: &Keys,
    owner: &PublicKey,
) -> Option<String> {
    match fetch_core_section(rest, agent_keys, owner).await {
        CoreFetch::Section(section) => Some(section),
        CoreFetch::Absent => Some(nudge_section()),
        CoreFetch::Failed(reason) => {
            tracing::warn!(
                target: "engram::core",
                "core fetch failed: {reason} — emitting no section to avoid \
                 confusing a relay outage with an absent core"
            );
            None
        }
    }
}

/// Outcome of a core-head fetch. The spawn-time path and the A1 per-turn
/// refresh path treat these three states differently, so they must not be
/// collapsed into one:
/// - `Section` — a verified, decryptable core exists (rendered).
/// - `Absent` — the relay *confirmed* no core exists. At spawn this renders
///   the onboarding nudge; mid-session the refresh keeps last-known-good
///   instead (a populated cache is only ever replaced by a verified core).
/// - `Failed` — transport / decrypt / parse error. Spawn emits nothing;
///   refresh keeps last-known-good.
pub enum CoreFetch {
    Section(String),
    Absent,
    Failed(String),
}

/// Monotonic per-reason refresh-failure counters (the PR-1 `drops_total`
/// discipline): a silently-failing refresh is MEM-FREEZE rebuilt via the
/// network route, and WARN lines ride the freeze-prone log queue — the
/// counter survives where the log line may not. In-memory by design
/// (reset-on-bounce bounded by B6b's POST cadence, same accepted gap as
/// the gate drop counters).
static REFRESH_FAILED_TOTAL: AtomicU64 = AtomicU64::new(0);
static REFRESH_TIMEOUT_TOTAL: AtomicU64 = AtomicU64::new(0);
static REFRESH_ABSENT_LKG_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Count a refresh that errored (transport/decrypt/parse); returns the
/// post-increment value for the WARN line.
pub(crate) fn note_refresh_failed() -> u64 {
    REFRESH_FAILED_TOTAL.fetch_add(1, Ordering::Relaxed) + 1
}

/// Count a refresh that exceeded its timeout budget.
pub(crate) fn note_refresh_timeout() -> u64 {
    REFRESH_TIMEOUT_TOTAL.fetch_add(1, Ordering::Relaxed) + 1
}

/// Count a mid-session confirmed absence that fell back to last-known-good.
pub(crate) fn note_refresh_absent_lkg() -> u64 {
    REFRESH_ABSENT_LKG_TOTAL.fetch_add(1, Ordering::Relaxed) + 1
}

/// Query, decode, and classify the core head, keeping "the relay answered
/// with a core", "the relay confirmed absence", and "the fetch failed"
/// distinct so each caller (spawn vs per-turn refresh) can map them to its
/// own failure semantics.
pub async fn fetch_core_section(
    rest: &RestClient,
    agent_keys: &Keys,
    owner: &PublicKey,
) -> CoreFetch {
    match fetch_core_body(rest, agent_keys, owner).await {
        Ok(Some(profile)) => CoreFetch::Section(format!("[{SECTION_LABEL}]\n{profile}")),
        Ok(None) => CoreFetch::Absent,
        Err(reason) => CoreFetch::Failed(reason),
    }
}

/// Strip the rendered section header, returning the bare profile body — the
/// string whose sha256 matches `buzz mem hash core` output.
pub(crate) fn section_profile(section: &str) -> &str {
    section
        .strip_prefix(&format!("[{SECTION_LABEL}]\n"))
        .unwrap_or(section)
}

/// Short sha256 prefix of a profile body, for per-turn refresh receipts —
/// cross-referenceable with `buzz mem hash core` and the CLI's write-verify
/// output (same underlying hash, truncated to 12 hex chars).
pub(crate) fn core_hash12(profile: &str) -> String {
    hex::encode(Sha256::digest(profile.as_bytes()))[..12].to_string()
}

/// Render the onboarding-nudge section — shared by the spawn path and the
/// mid-session confirmed-absence case when the cache is genuinely empty, so
/// the two can never drift apart.
pub(crate) fn nudge_section() -> String {
    format!("[{SECTION_LABEL}]\n{ONBOARDING_NUDGE}")
}

/// Query the relay for the core head and decode it. Returns:
/// - `Ok(Some(profile))` if a valid core body was found,
/// - `Ok(None)` only if the relay confirmed absence (empty result set),
/// - `Err(reason)` if the relay returned candidates we could not parse,
///   verify, or decrypt — those are NOT treated as absence (would let an
///   unreadable but real core be silently overwritten by the onboarding nudge),
/// - `Err` for transport / parse errors.
async fn fetch_core_body(
    rest: &RestClient,
    agent_keys: &Keys,
    owner: &PublicKey,
) -> Result<Option<String>, String> {
    let k_c = conversation_key(agent_keys.secret_key(), owner);
    let d = d_tag(&k_c, buzz_core::engram::CORE_SLUG);

    let filter = nostr::Filter::new()
        .kind(nostr::Kind::Custom(KIND_AGENT_ENGRAM as u16))
        .author(agent_keys.public_key())
        .custom_tags(nostr::SingleLetterTag::lowercase(nostr::Alphabet::D), [d])
        .custom_tags(
            nostr::SingleLetterTag::lowercase(nostr::Alphabet::P),
            [owner.to_hex()],
        )
        .limit(16);

    let value = rest
        .query(&[filter])
        .await
        .map_err(|e| format!("relay query failed: {e}"))?;
    let arr = value
        .as_array()
        .ok_or_else(|| "relay query returned non-array".to_string())?;
    decode_core_body(arr, agent_keys, owner)
}

/// Pure decoder: given the relay's JSON array, decide whether we have a
/// readable core, confirmed absence, or an ambiguous unreadable-state.
///
/// - Empty array → `Ok(None)` (confirmed absence; caller renders the nudge).
/// - At least one event decrypts → use the winning head's body.
///   * Body::Core → `Ok(Some(profile))`
///   * Body::Tombstone or unexpected shape → `Ok(None)` (treat as absent).
/// - Non-empty array but nothing decrypts → `Err` (fail closed; caller
///   emits no section, so the agent does not assume memory is empty and
///   try to overwrite a real-but-unreadable core).
fn decode_core_body(
    arr: &[serde_json::Value],
    agent_keys: &Keys,
    owner: &PublicKey,
) -> Result<Option<String>, String> {
    if arr.is_empty() {
        return Ok(None);
    }
    let mut valid_with_body: Vec<(Event, Body)> = Vec::with_capacity(arr.len());
    let mut candidates_seen = 0usize;
    let mut last_decrypt_err: Option<String> = None;
    for ev_json in arr {
        let event: Event = match serde_json::from_value(ev_json.clone()) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if event.verify().is_err() {
            continue;
        }
        candidates_seen += 1;
        match validate_and_decrypt(
            &event,
            &agent_keys.public_key(),
            owner,
            agent_keys.secret_key(),
            owner,
        ) {
            Ok(body) => valid_with_body.push((event, body)),
            Err(e) => {
                last_decrypt_err = Some(e.to_string());
                continue;
            }
        }
    }
    if valid_with_body.is_empty() {
        if candidates_seen > 0 {
            return Err(format!(
                "{candidates_seen} core candidate(s) returned but none decryptable                  (last error: {})",
                last_decrypt_err.as_deref().unwrap_or("unknown")
            ));
        }
        return Err(
            "relay returned core candidate(s) that could not be parsed or verified".to_string(),
        );
    }
    let events: Vec<Event> = valid_with_body.iter().map(|(e, _)| e.clone()).collect();
    // `select_head` returns `None` only on an empty iterator, which we
    // ruled out above.
    let Some(head) = select_head(events) else {
        return Ok(None);
    };
    let head_id = head.id;
    let body = valid_with_body
        .into_iter()
        .find(|(e, _)| e.id == head_id)
        .map(|(_, b)| b);
    match body {
        Some(Body::Core { profile }) => Ok(Some(profile)),
        // A tombstone or unexpectedly-shaped head means "no usable core."
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_core::engram::{build_event, Body};
    use serde_json::json;

    #[test]
    fn section_profile_strips_rendered_header() {
        let section = format!("[{SECTION_LABEL}]\nprofile body");
        assert_eq!(section_profile(&section), "profile body");
        // A string without the header passes through unchanged.
        assert_eq!(section_profile("bare"), "bare");
    }

    #[test]
    fn core_hash12_matches_value_sha_prefix() {
        // Same digest as `buzz mem hash core` / the CLI's sha256_hex,
        // truncated — the cross-reference contract for A1 receipts.
        assert_eq!(core_hash12("abc"), "ba7816bf8f01");
        assert_eq!(core_hash12("").len(), 12);
    }

    /// Empty array → confirmed absence → Ok(None), so the caller emits the
    /// onboarding nudge. This is the only path that maps to "no core."
    #[test]
    fn decode_empty_array_is_confirmed_absence() {
        let agent = Keys::generate();
        let owner = Keys::generate();
        let out = decode_core_body(&[], &agent, &owner.public_key()).unwrap();
        assert_eq!(out, None);
    }

    /// Happy path: a real, decryptable core event yields the profile.
    #[test]
    fn decode_valid_core_returns_profile() {
        let agent = Keys::generate();
        let owner = Keys::generate();
        let body = Body::Core {
            profile: "I am Sami.".to_string(),
        };
        let ev = build_event(&agent, &owner.public_key(), &body, 1_700_000_000).unwrap();
        let arr = vec![serde_json::to_value(&ev).unwrap()];
        let out = decode_core_body(&arr, &agent, &owner.public_key()).unwrap();
        assert_eq!(out.as_deref(), Some("I am Sami."));
    }

    /// Regression: when the relay returns a kind:30174 event addressed to
    /// this agent that we cannot decrypt (here: encrypted to a *different*
    /// owner's key, so the MAC fails for this agent↔owner pair), we MUST
    /// return Err and NOT Ok(None). Returning Ok(None) would cause the
    /// harness to emit the onboarding nudge, inviting the agent to overwrite
    /// a real-but-unreadable core.
    #[test]
    fn decode_undecryptable_candidate_is_err_not_absent() {
        let agent = Keys::generate();
        let owner = Keys::generate();
        let wrong_owner = Keys::generate();
        // Build an engram encrypted to wrong_owner (not owner). It will pass
        // sig verification but fail MAC/decrypt for the agent↔owner pair.
        let body = Body::Core {
            profile: "secret".to_string(),
        };
        let ev = build_event(&agent, &wrong_owner.public_key(), &body, 1_700_000_000).unwrap();
        let arr = vec![serde_json::to_value(&ev).unwrap()];
        let result = decode_core_body(&arr, &agent, &owner.public_key());
        assert!(result.is_err(), "expected Err, got: {result:?}");
        let msg = result.unwrap_err();
        assert!(msg.contains("decryptable"), "got: {msg}");
    }

    /// An unexpectedly-shaped head (here: a Memory body in what was supposed
    /// to be the core slot) is a legitimate, decryptable "no usable core" —
    /// Ok(None). Real `rm core` is refused at the CLI, so this is a defensive
    /// branch for malformed data on the wire.
    #[test]
    fn decode_non_core_body_is_absent() {
        let agent = Keys::generate();
        let owner = Keys::generate();
        let body = Body::Memory {
            slug: "mem/x".to_string(),
            value: None,
        };
        let ev = build_event(&agent, &owner.public_key(), &body, 1_700_000_000).unwrap();
        let arr = vec![serde_json::to_value(&ev).unwrap()];
        let out = decode_core_body(&arr, &agent, &owner.public_key()).unwrap();
        assert_eq!(out, None);
    }

    /// Non-empty array with only garbage entries (not even parseable as
    /// events) is also treated as a fetch error, not absence.
    #[test]
    fn decode_unparseable_candidates_is_err() {
        let agent = Keys::generate();
        let owner = Keys::generate();
        let arr = vec![json!({"not": "an event"}), json!("garbage")];
        let result = decode_core_body(&arr, &agent, &owner.public_key());
        assert!(result.is_err(), "expected Err, got: {result:?}");
    }
}
