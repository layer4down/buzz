import 'dart:convert';

import 'package:crypto/crypto.dart';

import 'app_attest_bridge.dart';
import 'push_gateway_client.dart';

/// Result of a successful enrollment + delegation flow.
///
/// The [endpointGrant] is the opaque ciphertext that the relay stores and
/// presents to the gateway when dispatching pushes. The [installationHandle]
/// and [keyId] are needed for subsequent mutations (rotate, revoke).
class EnrollmentResult {
  final String installationHandle;
  final String keyId;
  final String endpointGrant;
  final int endpointEpoch;

  const EnrollmentResult({
    required this.installationHandle,
    required this.keyId,
    required this.endpointGrant,
    required this.endpointEpoch,
  });
}

/// Orchestrates the App Attest enrollment and delegation flow against the
/// push gateway.
///
/// This is the high-level sequence that ties together [PushGatewayClient]
/// (HTTP) and [AppAttestBridge] (native crypto):
///
/// **Enroll:**
/// 1. Generate App Attest key → keyId
/// 2. Get challenge from gateway
/// 3. Build enroll transcript (with keyId), hash it
/// 4. Native: attest key with the hash
/// 5. Submit enrollment → get installation handle + endpoint epoch
///
/// **Delegate:**
/// 6. Get a fresh challenge from gateway
/// 7. Build delegate transcript, hash it
/// 8. Native: generate assertion over the hash
/// 9. Submit delegation → get endpoint grant
///
/// The orchestrator owns the transcript construction so the client (Flutter)
/// and server (Rust) agree byte-for-byte on what was signed.
class EnrollmentOrchestrator {
  final PushGatewayClient _gateway;
  final AppAttestBridge _appAttest;

  /// Default installation lifetime (365 days).
  static const _defaultInstallationTtl = Duration(days: 365);

  /// Default delegation lifetime (30 days).
  static const _defaultDelegationTtl = Duration(days: 30);

  EnrollmentOrchestrator({
    required PushGatewayClient gateway,
    required AppAttestBridge appAttest,
  })  : _gateway = gateway,
        _appAttest = appAttest;

  /// Run the full enrollment + delegation flow.
  ///
  /// [deviceToken] is the hex APNs token.
  /// [relayPubkey] is the hex pubkey of the relay authorized to push.
  /// [appProfile] selects production vs sandbox.
  ///
  /// Returns an [EnrollmentResult] with the installation handle, key ID,
  /// endpoint epoch, and endpoint grant.
  Future<EnrollmentResult> enrollAndDelegate({
    required String deviceToken,
    required String relayPubkey,
    AppProfile appProfile = AppProfile.buzzIosSandbox,
  }) async {
    // --- Phase 1: Enrollment ---

    // 1. Generate the App Attest key first — we need the key ID for the
    //    transcript.
    final keyId = await _appAttest.generateKey();

    // 2. Get a challenge from the gateway.
    final challenge = await _gateway.getChallenge();

    final now = DateTime.now().millisecondsSinceEpoch ~/ 1000;
    final installExpiresAt = now + _defaultInstallationTtl.inSeconds;

    // 3. Build the enroll transcript and hash it.
    final enrollTranscript = _buildEnrollTranscript(
      challengeId: challenge.challengeId,
      challenge: challenge.challenge,
      keyId: keyId,
      appProfile: appProfile,
      endpoint: deviceToken,
      expiresAt: installExpiresAt,
    );
    final enrollHash = _hashTranscript(
      'buzz.push.enroll.v1',
      enrollTranscript,
    );

    // 4. Attest the key with the transcript hash.
    final attestation = await _appAttest.attestKey(
      keyId: keyId,
      clientDataHash: enrollHash,
    );

    // 5. Submit enrollment.
    final enrollResponse = await _gateway.enroll(
      challengeId: challenge.challengeId,
      challenge: challenge.challenge,
      keyId: keyId,
      attestation: attestation,
      appProfile: appProfile,
      endpoint: deviceToken,
      endpointEpoch: 1,
      expiresAt: installExpiresAt,
    );

    // --- Phase 2: Delegation ---

    // 6. Get a fresh challenge for the delegation.
    final delegateChallenge = await _gateway.getChallenge();

    final delegateExpiresAt = now + _defaultDelegationTtl.inSeconds;

    // 7. Build the delegate transcript and hash it.
    final delegateTranscript = _buildDelegateTranscript(
      challengeId: delegateChallenge.challengeId,
      challenge: delegateChallenge.challenge,
      installationHandle: enrollResponse.installationHandle,
      endpointEpoch: enrollResponse.endpointEpoch,
      generation: 1,
      relayPubkey: relayPubkey,
      notBefore: now,
      expiresAt: delegateExpiresAt,
    );
    final delegateHash = _hashTranscript(
      'buzz.push.delegate.v1',
      delegateTranscript,
    );

    // 8. Generate the assertion.
    final assertion = await _appAttest.generateAssertion(
      keyId: keyId,
      clientDataHash: delegateHash,
    );

    // 9. Submit delegation.
    final delegateResponse = await _gateway.delegate(
      challengeId: delegateChallenge.challengeId,
      challenge: delegateChallenge.challenge,
      installationHandle: enrollResponse.installationHandle,
      endpointEpoch: enrollResponse.endpointEpoch,
      generation: 1,
      relayPubkey: relayPubkey,
      notBefore: now,
      expiresAt: delegateExpiresAt,
      assertion: assertion,
    );

    return EnrollmentResult(
      installationHandle: enrollResponse.installationHandle,
      keyId: keyId,
      endpointGrant: delegateResponse.endpointGrant,
      endpointEpoch: enrollResponse.endpointEpoch,
    );
  }

  /// Revoke the installation (unregister the device entirely).
  Future<void> revokeInstallation({
    required String installationHandle,
    required String keyId,
    required int endpointEpoch,
  }) async {
    final challenge = await _gateway.getChallenge();
    final newEpoch = endpointEpoch + 1;

    final transcript = _buildRevokeInstallationTranscript(
      challengeId: challenge.challengeId,
      challenge: challenge.challenge,
      installationHandle: installationHandle,
      endpointEpoch: endpointEpoch,
      newEndpointEpoch: newEpoch,
    );

    final assertion = await _appAttest.generateAssertion(
      keyId: keyId,
      clientDataHash: _hashTranscript('buzz.push.revoke_installation.v1', transcript),
    );

    await _gateway.revokeInstallation(
      challengeId: challenge.challengeId,
      challenge: challenge.challenge,
      installationHandle: installationHandle,
      endpointEpoch: endpointEpoch,
      newEndpointEpoch: newEpoch,
      assertion: assertion,
    );
  }

  // --- Transcript builders ---
  //
  // These mirror the Rust structs byte-for-byte. The domain prefix and JSON
  // serialization must match the server exactly for the App Attest signature
  // to verify.

  Map<String, dynamic> _buildEnrollTranscript({
    required String challengeId,
    required String challenge,
    required String keyId,
    required AppProfile appProfile,
    required String endpoint,
    required int expiresAt,
  }) =>
      {
        'v': 1,
        'audience': 'https://push.buzz.xyz/v1/installations',
        'challenge_id': challengeId,
        'challenge': challenge,
        'key_id': keyId,
        'app_profile': appProfile.wire,
        'endpoint': endpoint,
        'endpoint_epoch': 1,
        'expires_at': expiresAt,
      };

  Map<String, dynamic> _buildDelegateTranscript({
    required String challengeId,
    required String challenge,
    required String installationHandle,
    required int endpointEpoch,
    required int generation,
    required String relayPubkey,
    required int notBefore,
    required int expiresAt,
  }) =>
      {
        'v': 1,
        'audience': 'https://push.buzz.xyz/v1/delegations',
        'challenge_id': challengeId,
        'challenge': challenge,
        'installation_handle': installationHandle,
        'endpoint_epoch': endpointEpoch,
        'generation': generation,
        'relay_pubkey': relayPubkey,
        'not_before': notBefore,
        'expires_at': expiresAt,
      };

  Map<String, dynamic> _buildRevokeInstallationTranscript({
    required String challengeId,
    required String challenge,
    required String installationHandle,
    required int endpointEpoch,
    required int newEndpointEpoch,
  }) =>
      {
        'v': 1,
        'audience': 'https://push.buzz.xyz/v1/installations/revoke',
        'challenge_id': challengeId,
        'challenge': challenge,
        'installation_handle': installationHandle,
        'endpoint_epoch': endpointEpoch,
        'new_endpoint_epoch': newEndpointEpoch,
      };

  /// Build the canonical transcript string and return its SHA-256 hash as
  /// a lowercase hex string.
  ///
  /// Format: `"{domain}\n{json_body}"`
  String _hashTranscript(String domain, Map<String, dynamic> body) {
    final transcriptStr = '$domain\n${jsonEncode(body)}';
    return sha256.convert(utf8.encode(transcriptStr)).toString();
  }
}
