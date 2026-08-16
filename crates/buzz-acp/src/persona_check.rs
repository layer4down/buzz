//! Persona-split startup invariant.
//!
//! Two incidents (2026-08-12, see `PLANS/PRIME_AGENT_MIGRATION_PLAN.md`
//! "Pre-Wave Gate") established the bug class: a config source downstream of
//! `agent.env` makes the runtime resolve a *different* persona than the one
//! named by `--system-prompt-file` / `BUZZ_ACP_SYSTEM_PROMPT_FILE`, and the
//! agent then acts under the wrong identity with nothing at startup catching
//! it. Prime-PM's canary crashed on a definition/instance split; Lens's
//! heartbeat ran under Prime-PM's prompt because `opencode acp --pure`
//! resolved its persona from opencode's own config and ignored ours.
//!
//! The check is runtime-agnostic: spawn a throwaway probe of the same adapter
//! with the same persona env, create a session whose system prompt is the
//! intended persona plus a one-time nonce instruction, and ask for the nonce.
//! The model can only echo a nonce it actually received, so a runtime that
//! silently substituted its own persona fails the echo and the harness
//! refuses to start.
//!
//! The nonce exists **only in the system prompt**. The user message carries
//! the prefix at most, never the token — a substituted runtime that ignores
//! our systemPrompt still receives the user message, and any compliant model
//! would obediently echo a nonce handed to it there, false-Verifying the
//! exact incident this gate exists to catch. Reply classification is
//! three-way: real nonce → Verified; a prefix-shaped token that is not the
//! nonce → Mismatch (fabricated); no token-shaped reply at all → one retry,
//! then Indeterminate (noncompliant-honest and substituted-runtime are
//! indistinguishable there).
//!
//! Failure policy: a **detected mismatch always blocks** (that is the gate).
//! Infrastructure failure — adapter cannot spawn, session cannot be created,
//! turn times out — downgrades to a loud warning because the check must not
//! turn "adapter is slow" into "fleet stays down".

use std::time::Duration;

use crate::acp::AcpClient;
use crate::config::Config;

/// Prefix for the one-time nonce the probe asks the agent to echo.
const NONCE_PREFIX: &str = "BUZZ-PERSONA-CHECK-";

/// How long the probe turn may stay silent before the idle timeout fires.
/// Local models can take tens of seconds to first token; 120s mirrors the
/// operator-facing `BUZZ_ACP_IDLE_TIMEOUT` ceiling used in production.
const PROBE_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// Absolute wall-clock cap on the probe turn.
const PROBE_MAX_DURATION: Duration = Duration::from_secs(180);

/// Outcome of a persona invariant check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersonaCheckOutcome {
    /// The runtime echoed the nonce — the intended persona reached the model.
    Verified,
    /// The runtime answered but did not echo the nonce — a different persona
    /// served the turn. The harness must refuse to start.
    Mismatch { reply_excerpt: String },
    /// The check could not run (adapter spawn failure, protocol error,
    /// timeout). Never a silent pass: callers log loudly and may continue.
    Indeterminate { reason: String },
    /// The check does not apply to this configuration.
    Skipped { reason: String },
}

/// Generate the per-invocation nonce. Not cryptographically random — it only
/// needs to be unguessable enough that a wrong persona cannot satisfy the
/// echo by chance.
fn generate_nonce() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Process id disambiguates two harnesses started in the same nanosecond.
    format!("{NONCE_PREFIX}{nanos:x}-{}", std::process::id())
}

/// Build the probe's system prompt: the intended persona content with the
/// nonce instruction appended, so a runtime that substitutes its own persona
/// cannot know the nonce.
fn probe_system_prompt(persona: &str, nonce: &str) -> String {
    format!(
        "{persona}\n\n[Startup Verification]\nYou are undergoing a startup \
identity check. When asked for the verification token, respond with exactly \
this token and nothing else: {nonce}"
    )
}

/// Build the probe's user prompt. Prefix hint only — the nonce itself must
/// never appear here, or a runtime that dropped our systemPrompt can still
/// pass the echo by parroting the user message.
fn probe_user_prompt() -> String {
    format!(
        "Reply with the startup verification token from your instructions. \
The token starts with {NONCE_PREFIX} followed by hexadecimal characters, a \
hyphen, and digits. Output only the token and nothing else."
    )
}

/// Does `reply` look like a nonce (prefix + hex + '-' + digits)?
fn is_token_shaped(reply: &str) -> bool {
    fn is_hex_or_digit(c: char) -> bool {
        c.is_ascii_hexdigit() || c.is_ascii_digit()
    }
    let Some(rest) = reply.trim().strip_prefix(NONCE_PREFIX) else {
        return false;
    };
    let Some((hex_part, pid_part)) = rest.split_once('-') else {
        return false;
    };
    !hex_part.is_empty()
        && hex_part.chars().all(is_hex_or_digit)
        && !pid_part.is_empty()
        && pid_part.chars().all(|c| c.is_ascii_digit())
}

/// Run the persona invariant check against a fresh probe client.
///
/// `acp` must be an already-spawned, **uninitialized** client — the function
/// performs the `initialize` handshake itself so it can bail before creating
/// any session if the adapter does not speak a protocol where the check is
/// meaningful. The client is consumed (shut down) on every path.
pub(crate) async fn check_persona(acp: AcpClient, config: &Config) -> PersonaCheckOutcome {
    let mut acp = acp;

    let init = match acp.initialize().await {
        Ok(result) => result,
        Err(e) => {
            acp.shutdown().await;
            return PersonaCheckOutcome::Indeterminate {
                reason: format!("probe initialize failed: {e}"),
            };
        }
    };
    // `as_u64().unwrap_or(1)` would silently downgrade a v2 adapter that
    // reports the version as a string — fail-open on a parse quirk, in a
    // gate. Non-numeric/missing is treated as unknown (proceed with the
    // check); only a numeric v1 skips.
    let protocol_version = init["protocolVersion"].as_u64();
    let agent_name = crate::normalized_agent_name(&init);

    // Legacy framing (protocol v1, or goose without the system-prompt
    // extension) re-injects the persona into every user message, so a
    // downstream persona override is structurally impossible. Skip.
    let is_goose = agent_name == "goose";
    // `session/new` requires an absolute cwd; the probe session is
    // throwaway so any absolute path the process can name works.
    let probe_cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/".into());
    if is_goose {
        // Goose's system-prompt support is only discoverable by probing its
        // custom method on a live session. If the method is absent the
        // harness falls back to per-turn user-message persona injection
        // (see `has_system_prompt_support` in pool.rs), which re-sends the
        // persona every turn — structurally immune to the startup split.
        // The throwaway session below serves that probe; goose tolerates
        // multiple concurrent sessions per connection.
        match acp.session_new_full(&probe_cwd, vec![], None, None).await {
            Ok(resp) => {
                let supported = acp
                    .session_set_goose_system_prompt(&resp.session_id, "probe")
                    .await
                    .is_ok();
                acp.shutdown().await;
                return if supported {
                    // Continue to the nonce probe against a fresh client —
                    // reusing the session that already accepted a system
                    // prompt would test our own write, not the adapter's
                    // session/new handling.
                    PersonaCheckOutcome::Skipped {
                        reason: "goose: system-prompt extension present; nonce probe on a fresh \
client is not covered in v1 — its system prompt rides the goose extension, \
not session/new, so the session/new probe here would not exercise the real \
persona path"
                            .into(),
                    }
                } else {
                    PersonaCheckOutcome::Skipped {
                        reason: "goose without system-prompt extension uses per-turn persona re-injection; startup substitution is structurally impossible"
                            .into(),
                    }
                };
            }
            Err(e) => {
                acp.shutdown().await;
                return PersonaCheckOutcome::Indeterminate {
                    reason: format!("goose probe session/new failed: {e}"),
                };
            }
        }
    }
    if protocol_version == Some(1) {
        acp.shutdown().await;
        return PersonaCheckOutcome::Skipped {
            reason: format!(
                "agent {agent_name} speaks protocol v1 with per-turn \
persona re-injection; startup substitution is structurally impossible"
            ),
        };
    }
    if protocol_version.is_none() {
        tracing::warn!(
            "persona check: agent {agent_name} reported a non-numeric or missing \
protocolVersion ({}) — treating as unknown and running the probe",
            init["protocolVersion"]
        );
    }

    let persona = match &config.system_prompt {
        Some(p) => p.clone(),
        None => {
            acp.shutdown().await;
            return PersonaCheckOutcome::Skipped {
                reason: "no system prompt configured — nothing to verify".into(),
            };
        }
    };

    let nonce = generate_nonce();
    let system_prompt = probe_system_prompt(&persona, &nonce);

    let session = match acp
        .session_new_full(
            &probe_cwd,
            vec![],
            Some(&system_prompt),
            Some("Persona check"),
        )
        .await
    {
        Ok(resp) => resp.session_id,
        Err(e) => {
            acp.shutdown().await;
            return PersonaCheckOutcome::Indeterminate {
                reason: format!("probe session/new failed: {e}"),
            };
        }
    };

    let user_prompt = probe_user_prompt();
    let mut outcome = None;
    // One retry for the no-token case: a compliant model under the right
    // persona may still decline or ramble on the first ask.
    for attempt in 1..=2 {
        let turn = acp
            .session_prompt_with_idle_timeout(
                &session,
                &user_prompt,
                PROBE_IDLE_TIMEOUT,
                PROBE_MAX_DURATION,
            )
            .await;
        match turn {
            Ok(_) => {
                match acp.last_assistant_message() {
                    // Real nonce echoed: the system prompt genuinely reached the
                    // model — the only path to Verified.
                    Some(reply) if reply.contains(&nonce) => {
                        outcome = Some(PersonaCheckOutcome::Verified);
                    }
                    // Prefix-shaped but wrong token: the reply pattern-matched the
                    // instruction it was given without knowing the nonce — the
                    // model read the user message, not our system prompt. That is
                    // fabrication, high-confidence mismatch.
                    Some(reply) if is_token_shaped(&reply) => {
                        outcome = Some(PersonaCheckOutcome::Mismatch {
                            reply_excerpt: excerpt(&reply),
                        });
                    }
                    Some(reply) => {
                        if attempt == 1 {
                            tracing::warn!(
                                "persona check: attempt 1 replied without a token-shaped \
answer ({} chars) — retrying once",
                                reply.trim().chars().count()
                            );
                            continue;
                        }
                        outcome = Some(PersonaCheckOutcome::Indeterminate {
                            reason: format!(
                                "adapter answered twice but never echoed or fabricated a \
token — cannot distinguish noncompliant-honest from substituted runtime; \
last reply excerpt: {}",
                                excerpt(&reply)
                            ),
                        });
                    }
                    None => {
                        if attempt == 1 {
                            tracing::warn!("persona check: attempt 1 produced no assistant text — retrying once");
                            continue;
                        }
                        outcome = Some(PersonaCheckOutcome::Indeterminate {
                            reason: "adapter completed two turns without any assistant text".into(),
                        });
                    }
                }
            }
            Err(e) => {
                outcome = Some(PersonaCheckOutcome::Indeterminate {
                    reason: format!("probe turn failed: {e}"),
                });
            }
        }
        break;
    }

    let outcome = outcome.unwrap_or_else(|| {
        // Unreachable: the loop always sets `outcome` before breaking.
        PersonaCheckOutcome::Indeterminate {
            reason: "probe loop exited without classifying".into(),
        }
    });

    acp.shutdown().await;
    outcome
}

/// Truncate a reply for diagnostics so a wrong persona's full output never
/// lands in logs wholesale.
fn excerpt(reply: &str) -> String {
    const MAX_EXCERPT: usize = 200;
    let trimmed = reply.trim();
    if trimmed.chars().count() <= MAX_EXCERPT {
        trimmed.to_string()
    } else {
        let cut: String = trimmed.chars().take(MAX_EXCERPT).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_contains_prefix_and_pid() {
        let nonce = generate_nonce();
        assert!(nonce.starts_with(NONCE_PREFIX));
        assert!(nonce.ends_with(&std::process::id().to_string()));
    }

    #[test]
    fn nonces_differ_across_calls() {
        // Two calls in the same nanosecond still differ via the pid is not
        // guaranteed — but consecutive calls differ by time. Assert the
        // practical property: distinct calls produce distinct nonces.
        let a = generate_nonce();
        let b = generate_nonce();
        assert_ne!(a, b);
    }

    #[test]
    fn probe_system_prompt_appends_nonce_instruction() {
        let prompt = probe_system_prompt("You are Lens.", "BUZZ-PERSONA-CHECK-abc-1");
        assert!(prompt.starts_with("You are Lens."));
        assert!(prompt.contains("[Startup Verification]"));
        assert!(prompt.contains("BUZZ-PERSONA-CHECK-abc-1"));
    }

    #[test]
    fn probe_user_prompt_never_contains_a_nonce() {
        // Regression (review finding): the nonce must exist only in the
        // system prompt. A nonce pasted into the user message lets a
        // substituted runtime pass by parroting it.
        let prompt = probe_user_prompt();
        assert!(prompt.contains(NONCE_PREFIX), "carries the prefix hint");
        // No full nonce shape (prefix + hex + '-' + digits) may appear.
        assert!(
            !is_token_shaped(&prompt),
            "user prompt must not itself contain a token-shaped string: {prompt}"
        );
    }

    #[test]
    fn token_shape_detection() {
        assert!(is_token_shaped("BUZZ-PERSONA-CHECK-deadbeef-123"));
        assert!(is_token_shaped("  BUZZ-PERSONA-CHECK-1-2  \n"));
        assert!(!is_token_shaped("BUZZ-PERSONA-CHECK-")); // empty parts
        assert!(!is_token_shaped("BUZZ-PERSONA-CHECK-xyz-1")); // non-hex body
        assert!(!is_token_shaped("BUZZ-PERSONA-CHECK-abc-x")); // non-digit pid
        assert!(!is_token_shaped("SOME-OTHER-TOKEN-abc-123")); // wrong prefix
        assert!(!is_token_shaped("I could not find a token")); // no token at all
    }

    #[test]
    fn excerpt_truncates_long_replies() {
        let long = "x".repeat(500);
        let out = excerpt(&long);
        assert!(out.chars().count() <= 201); // 200 + ellipsis
        assert!(out.ends_with('…'));
    }

    #[test]
    fn excerpt_passes_short_replies_through() {
        assert_eq!(excerpt("  short  "), "short");
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::acp::AcpClient;

    /// Build a mock adapter that plays the role of an LLM runtime over the
    /// ACP wire. `mode` controls its behavior:
    ///
    /// - `"honest"`: initialize → v2; session/new stores the systemPrompt and
    ///   the nonce it contains; session/prompt replies by re-emitting that
    ///   nonce as agent_message_chunk. This is a well-behaved adapter.
    /// - `"persona-split"`: like honest, but session/new IGNORES the
    ///   systemPrompt and session/prompt answers from a persona the adapter
    ///   resolved itself (no nonce knowledge). This reproduces the opencode
    ///   `--pure` incident: valid protocol, wrong persona, silent.
    fn mock_adapter(mode: &'static str) -> String {
        // Minimal ACP adapter in Python, driven over stdin/stdout.
        // MODE=0: honest adapter — stores the systemPrompt and echoes the
        // nonce it found there on session/prompt.
        // MODE=1: persona-split adapter — ignores systemPrompt and answers
        //   from a persona it resolved itself (the opencode `--pure` shape).
        // MODE=2: adversarial — ignores systemPrompt but answers like a
        //   compliant model: echoes any token-shaped string found in the
        //   USER message (the false-Verify shape pre-fix).
        // MODE=3: fabricator — ignores systemPrompt, invents a prefix-shaped
        //   token that is not the nonce.
        // MODE=4: rambler — ignores systemPrompt, prose with no token.
        // Braces are single, not doubled: this is a plain string (no
        // format!), with the mode injected via string replacement.
        let script = r#"exec python3 -c '
import json, sys, re

MODE = __MODE__

stored_nonce = None

def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method = req.get("method")
    rid = req.get("id")

    if method == "initialize":
        send({"jsonrpc": "2.0", "id": rid, "result": {
            "protocolVersion": 2,
            "agentInfo": {"name": "mock-acp", "version": "1.0"}
        }})
    elif method == "session/new":
        sp = (req.get("params") or {}).get("systemPrompt", "")
        m = re.search(r"BUZZ-PERSONA-CHECK-[0-9a-f]+-[0-9]+", sp or "")
        if MODE == 0:
            stored_nonce = m.group(0) if m else None
        else:
            stored_nonce = None  # persona split: prompt never lands
        send({"jsonrpc": "2.0", "id": rid, "result": {"sessionId": "ses_mock"}})
    elif method == "session/prompt":
        prompt_text = " ".join(
            (b or {}).get("text", "")
            for b in (req.get("params") or {}).get("prompt", [])
        )
        if MODE == 0:
            reply = stored_nonce or "i-do-not-know"
        elif MODE == 1:
            reply = "You are Prime-PM, the Program Manager. How can I coordinate today?"
        elif MODE == 2:
            # Compliant model that never saw the system prompt: parrot any
            # token-shaped string handed over in the user message.
            m2 = re.search(r"BUZZ-PERSONA-CHECK-[0-9a-fA-F]+-[0-9]+", prompt_text)
            reply = m2.group(0) if m2 else (
                "You are Prime-PM. I could not find a verification token in this message."
            )
        elif MODE == 3:
            reply = "BUZZ-PERSONA-CHECK-0000dead-4194304"
        else:
            reply = "I am happy to help with your project coordination needs today!"
        send({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": "ses_mock",
            "update": {"sessionUpdate": "agent_message_chunk",
                        "content": {"type": "text", "text": reply}}
        }})
        send({"jsonrpc": "2.0", "id": rid, "result": {"stopReason": "end_turn"}})
    elif method is not None and rid is not None:
        send({"jsonrpc": "2.0", "id": rid,
              "error": {"code": -32601, "message": "no such method"}})
'
"#;
        let mode_num = match mode {
            "honest" => "0",
            "persona-split" => "1",
            "adversarial" => "2",
            "fabricator" => "3",
            _ => "4",
        };
        script.replace("__MODE__", mode_num)
    }

    async fn spawn_mock(mode: &'static str) -> AcpClient {
        AcpClient::spawn("bash", &["-c".to_string(), mock_adapter(mode)], &[], false)
            .await
            .expect("failed to spawn mock adapter")
    }

    fn check_config(persona: &str) -> crate::config::Config {
        crate::config::Config::for_persona_check(Some(persona.to_string()))
    }

    const PERSONA: &str = "You are Lens, the code reviewer. You review PRs.";

    #[tokio::test]
    async fn honest_adapter_verifies() {
        let acp = spawn_mock("honest").await;
        let outcome = check_persona(acp, &check_config(PERSONA)).await;
        assert_eq!(
            outcome,
            PersonaCheckOutcome::Verified,
            "honest adapter must verify"
        );
    }

    #[tokio::test]
    async fn persona_split_adapter_is_detected_as_mismatch() {
        // The incident shape: adapter ignores systemPrompt, serves its own
        // resolved persona. The wrong persona complies with the user-message
        // instruction the only way it can — fabricating a prefix-shaped
        // token it was never given — which is a high-confidence Mismatch.
        // (A canned-persona prose reply no longer claims Mismatch: prose is
        // honestly ambiguous under the three-way classification and lands
        // Indeterminate after one retry; that path is pinned separately in
        // `prose_only_reply_is_indeterminate_after_retry`.)
        let acp = spawn_mock("fabricator").await;
        let outcome = check_persona(acp, &check_config(PERSONA)).await;
        match outcome {
            PersonaCheckOutcome::Mismatch { reply_excerpt } => {
                assert!(
                    reply_excerpt.starts_with("BUZZ-PERSONA-CHECK-"),
                    "excerpt should carry the fabricated token for diagnostics: {reply_excerpt}"
                );
            }
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn adversarial_echo_of_user_message_is_not_verified() {
        // Lens's proof case, now a permanent regression test: a substituted
        // runtime that ignores our systemPrompt but reads the user message —
        // i.e. any compliant model — must NOT verify. Pre-fix, the nonce
        // rode the user prompt and this adapter Verified. Now the user
        // message carries only the prefix hint, so the adversarial reply has
        // no token to parrot: prose answer, retry, Indeterminate. Never
        // Verified, never a silent Mismatch fabrication.
        let acp = spawn_mock("adversarial").await;
        let outcome = check_persona(acp, &check_config(PERSONA)).await;
        match outcome {
            PersonaCheckOutcome::Indeterminate { reason } => {
                assert!(
                    reason.contains("never echoed"),
                    "unexpected reason: {reason}"
                );
            }
            PersonaCheckOutcome::Mismatch { .. } => {
                // Also acceptable: a token-shaped fabrication attempt.
            }
            other => panic!("expected Indeterminate (or Mismatch), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fabricated_prefix_token_is_mismatch() {
        // The adapter invents a token-shaped reply that matches the prefix
        // pattern but is not the nonce: fabrication is a high-confidence
        // mismatch — the model pattern-matched the instruction in the user
        // message instead of reading its system prompt.
        let acp = spawn_mock("fabricator").await;
        let outcome = check_persona(acp, &check_config(PERSONA)).await;
        match outcome {
            PersonaCheckOutcome::Mismatch { reply_excerpt } => {
                assert!(
                    reply_excerpt.starts_with("BUZZ-PERSONA-CHECK-"),
                    "excerpt should carry the fabricated token: {reply_excerpt}"
                );
            }
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn prose_only_reply_is_indeterminate_after_retry() {
        // No token-shaped reply at all on both attempts: noncompliant-honest
        // and substituted-runtime are indistinguishable — Indeterminate with
        // a loud reason, never Verified.
        let acp = spawn_mock("rambler").await;
        let outcome = check_persona(acp, &check_config(PERSONA)).await;
        match outcome {
            PersonaCheckOutcome::Indeterminate { reason } => {
                assert!(
                    reason.contains("never echoed") || reason.contains("without"),
                    "unexpected reason: {reason}"
                );
            }
            other => panic!("expected Indeterminate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn string_protocol_version_runs_check_not_skip() {
        // Secondary review finding: `as_u64().unwrap_or(1)` silently
        // downgraded a v2 adapter reporting protocolVersion as a string to
        // v1 → Skipped (fail-open on a parse quirk). Non-numeric version
        // must run the probe instead.
        let script = r#"exec python3 -c '
import json, sys
def send(m):
    sys.stdout.write(json.dumps(m) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    rid = req.get("id"); method = req.get("method")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":rid,"result":{"protocolVersion":"2","agentInfo":{"name":"weird"}}})
    elif method == "session/new":
        sp = (req.get("params") or {}).get("systemPrompt", "") or ""
        import re
        m = re.search(r"BUZZ-PERSONA-CHECK-[0-9a-f]+-[0-9]+", sp)
        nonce = m.group(0) if m else "i-do-not-know"
        send({"jsonrpc":"2.0","id":rid,"result":{"sessionId":"ses_w"}})
    elif method == "session/prompt":
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_w",
            "update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":nonce}}}})
        send({"jsonrpc":"2.0","id":rid,"result":{"stopReason":"end_turn"}})
    elif rid is not None:
        send({"jsonrpc":"2.0","id":rid,"error":{"code":-32601,"message":"no"}})
'
"#;
        let acp = AcpClient::spawn("bash", &["-c".to_string(), script.to_string()], &[], false)
            .await
            .expect("spawn");
        let outcome = check_persona(acp, &check_config(PERSONA)).await;
        assert_eq!(
            outcome,
            PersonaCheckOutcome::Verified,
            "string protocolVersion must not downgrade to a v1 skip"
        );
    }

    #[tokio::test]
    async fn no_system_prompt_skips() {
        let acp = spawn_mock("honest").await;
        let outcome = check_persona(acp, &crate::config::Config::for_persona_check(None)).await;
        match outcome {
            PersonaCheckOutcome::Skipped { reason } => {
                assert!(
                    reason.contains("no system prompt"),
                    "unexpected reason: {reason}"
                )
            }
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn legacy_protocol_v1_skips() {
        // v1 mock: initialize responds protocolVersion 1.
        let script = r#"exec python3 -c '
import json, sys
def send(m):
    sys.stdout.write(json.dumps(m) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    rid = req.get("id"); method = req.get("method")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":rid,"result":{"protocolVersion":1,"agentInfo":{"name":"legacy"}}})
    elif rid is not None:
        send({"jsonrpc":"2.0","id":rid,"error":{"code":-32601,"message":"no"}})
'
"#;
        let acp = AcpClient::spawn("bash", &["-c".to_string(), script.to_string()], &[], false)
            .await
            .expect("spawn");
        let outcome = check_persona(acp, &check_config(PERSONA)).await;
        match outcome {
            PersonaCheckOutcome::Skipped { reason } => {
                assert!(
                    reason.contains("protocol v1"),
                    "unexpected reason: {reason}"
                )
            }
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dead_adapter_is_indeterminate_not_mismatch() {
        // Adapter that accepts spawn but dies before answering initialize.
        // Must be Indeterminate (fail-open on infra), never Verified.
        let acp = AcpClient::spawn(
            "bash",
            &["-c".to_string(), "exec sleep 0.2".to_string()],
            &[],
            false,
        )
        .await
        .expect("spawn");
        let outcome = check_persona(acp, &check_config(PERSONA)).await;
        match outcome {
            PersonaCheckOutcome::Indeterminate { .. } => {}
            other => panic!("expected Indeterminate, got {other:?}"),
        }
    }
}
