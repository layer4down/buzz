import 'dart:convert';

import 'package:http/http.dart' as http;

/// Wire version for all push gateway requests.
const _wireVersion = 1;

/// App profile values the gateway recognizes (kebab-case over the wire).
enum AppProfile {
  buzzIosProduction('buzz-ios-production'),
  buzzIosSandbox('buzz-ios-sandbox');

  final String wire;
  const AppProfile(this.wire);

  /// Returns the sandbox profile for development builds.
  static AppProfile get development => buzzIosSandbox;
}

// ---------------------------------------------------------------------------
// Request / Response models
// ---------------------------------------------------------------------------

class ChallengeResponse {
  final String challengeId;
  final String challenge;
  final int expiresAt;

  const ChallengeResponse({
    required this.challengeId,
    required this.challenge,
    required this.expiresAt,
  });

  factory ChallengeResponse.fromJson(Map<String, dynamic> json) =>
      ChallengeResponse(
        challengeId: json['challenge_id'] as String,
        challenge: json['challenge'] as String,
        expiresAt: json['expires_at'] as int,
      );
}

class EnrollResponse {
  final String installationHandle;
  final int endpointEpoch;
  final int expiresAt;

  const EnrollResponse({
    required this.installationHandle,
    required this.endpointEpoch,
    required this.expiresAt,
  });

  factory EnrollResponse.fromJson(Map<String, dynamic> json) => EnrollResponse(
        installationHandle: json['installation_handle'] as String,
        endpointEpoch: json['endpoint_epoch'] as int,
        expiresAt: json['expires_at'] as int,
      );
}

class DelegateResponse {
  /// Opaque ciphertext grant that the relay stores and presents for delivery.
  final String endpointGrant;

  const DelegateResponse({required this.endpointGrant});

  factory DelegateResponse.fromJson(Map<String, dynamic> json) =>
      DelegateResponse(endpointGrant: json['endpoint_grant'] as String);
}

/// Thrown when the gateway returns a non-2xx status.
class PushGatewayException implements Exception {
  final int statusCode;
  final String errorCode;

  const PushGatewayException(this.statusCode, this.errorCode);

  @override
  String toString() => 'PushGatewayException($statusCode: $errorCode)';
}

// ---------------------------------------------------------------------------
// Gateway client
// ---------------------------------------------------------------------------

/// HTTP client for the push gateway stateful API.
///
/// Wraps the six REST endpoints:
/// - `POST /v1/installations/challenges` — get a fresh App Attest challenge
/// - `POST /v1/installations` — enroll device with attestation
/// - `POST /v1/delegations` — delegate push authority to a relay
/// - `POST /v1/delegations/revoke` — revoke a delegation
/// - `POST /v1/installations/endpoint` — rotate the APNs endpoint (token)
/// - `POST /v1/installations/revoke` — revoke the installation
///
/// All mutation endpoints require a challenge obtained from the challenges
/// endpoint. The challenge is embedded in the App Attest transcript that the
/// native side signs; the gateway verifies it before accepting the mutation.
class PushGatewayClient {
  final String _baseUrl;
  final http.Client _httpClient;
  final Duration _timeout;

  PushGatewayClient({
    required String baseUrl,
    http.Client? httpClient,
    Duration timeout = const Duration(seconds: 15),
  })  : _baseUrl = baseUrl.endsWith('/')
            ? baseUrl.substring(0, baseUrl.length - 1)
            : baseUrl,
        _httpClient = httpClient ?? http.Client(),
        _timeout = timeout;

  /// Request a fresh App Attest challenge.
  Future<ChallengeResponse> getChallenge() async {
    final json = await _post('/v1/installations/challenges', {
      'v': _wireVersion,
    });
    return ChallengeResponse.fromJson(json);
  }

  /// Enroll a device installation.
  ///
  /// [attestation] is the base64-encoded Apple App Attest CBOR object.
  /// [keyId] is the App Attest key identifier.
  /// [endpoint] is the hex APNs device token.
  Future<EnrollResponse> enroll({
    required String challengeId,
    required String challenge,
    required String keyId,
    required String attestation,
    required AppProfile appProfile,
    required String endpoint,
    required int endpointEpoch,
    required int expiresAt,
  }) async {
    final json = await _post('/v1/installations', {
      'v': _wireVersion,
      'challenge_id': challengeId,
      'challenge': challenge,
      'key_id': keyId,
      'attestation': attestation,
      'app_profile': appProfile.wire,
      'endpoint': endpoint,
      'endpoint_epoch': endpointEpoch,
      'expires_at': expiresAt,
    });
    return EnrollResponse.fromJson(json);
  }

  /// Delegate push authority to a relay.
  ///
  /// [assertion] is the base64-encoded App Attest assertion over the
  /// delegation transcript.
  Future<DelegateResponse> delegate({
    required String challengeId,
    required String challenge,
    required String installationHandle,
    required int endpointEpoch,
    required int generation,
    required String relayPubkey,
    required int notBefore,
    required int expiresAt,
    required String assertion,
  }) async {
    final json = await _post('/v1/delegations', {
      'v': _wireVersion,
      'challenge_id': challengeId,
      'challenge': challenge,
      'installation_handle': installationHandle,
      'endpoint_epoch': endpointEpoch,
      'generation': generation,
      'relay_pubkey': relayPubkey,
      'not_before': notBefore,
      'expires_at': expiresAt,
      'assertion': assertion,
    });
    return DelegateResponse.fromJson(json);
  }

  /// Revoke a delegation.
  Future<void> revokeDelegation({
    required String challengeId,
    required String challenge,
    required String installationHandle,
    required String relayPubkey,
    required int generation,
    required String assertion,
  }) async {
    await _post('/v1/delegations/revoke', {
      'v': _wireVersion,
      'challenge_id': challengeId,
      'challenge': challenge,
      'installation_handle': installationHandle,
      'relay_pubkey': relayPubkey,
      'generation': generation,
      'assertion': assertion,
    });
  }

  /// Rotate the APNs endpoint token.
  Future<void> rotateEndpoint({
    required String challengeId,
    required String challenge,
    required String installationHandle,
    required int endpointEpoch,
    required int newEndpointEpoch,
    required String endpoint,
    required String assertion,
  }) async {
    await _post('/v1/installations/endpoint', {
      'v': _wireVersion,
      'challenge_id': challengeId,
      'challenge': challenge,
      'installation_handle': installationHandle,
      'endpoint_epoch': endpointEpoch,
      'new_endpoint_epoch': newEndpointEpoch,
      'endpoint': endpoint,
      'assertion': assertion,
    });
  }

  /// Revoke the installation entirely.
  Future<void> revokeInstallation({
    required String challengeId,
    required String challenge,
    required String installationHandle,
    required int endpointEpoch,
    required int newEndpointEpoch,
    required String assertion,
  }) async {
    await _post('/v1/installations/revoke', {
      'v': _wireVersion,
      'challenge_id': challengeId,
      'challenge': challenge,
      'installation_handle': installationHandle,
      'endpoint_epoch': endpointEpoch,
      'new_endpoint_epoch': newEndpointEpoch,
      'assertion': assertion,
    });
  }

  /// POST [body] to [path], returning the decoded JSON response.
  ///
  /// Throws [PushGatewayException] on non-2xx status codes.
  Future<Map<String, dynamic>> _post(
    String path,
    Map<String, dynamic> body,
  ) async {
    final response = await _httpClient
        .post(
          Uri.parse('$_baseUrl$path'),
          headers: {'Content-Type': 'application/json'},
          body: jsonEncode(body),
        )
        .timeout(_timeout);

    if (response.statusCode < 200 || response.statusCode >= 300) {
      String errorCode = 'unknown';
      try {
        final decoded = jsonDecode(response.body) as Map<String, dynamic>;
        errorCode = decoded['error'] as String? ?? errorCode;
      } catch (_) {}
      throw PushGatewayException(response.statusCode, errorCode);
    }

    return jsonDecode(response.body) as Map<String, dynamic>;
  }
}
