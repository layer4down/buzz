import 'dart:io';

import 'package:flutter/services.dart';

/// Bridge to the native iOS App Attest framework.
///
/// App Attest proves to the push gateway that the device is a genuine Apple
/// device running a legitimate app instance. The native side (BuzzPushKit)
/// wraps `DCAppAttestService`:
///
/// - [attest] generates a new App Attest key, requests an attestation object
///   from Apple, and returns the CBOR attestation + key ID.
/// - [assertion] signs a client-data hash with an existing key, proving
///   possession for mutations (delegate, rotate, revoke).
///
/// On non-iOS platforms, all calls throw [UnsupportedError].
abstract class AppAttestBridge {
  /// Generate (or reuse) an App Attest key, obtain an attestation from Apple,
  /// and return the base64-encoded CBOR attestation object.
  ///
  /// [challenge] is the gateway-issued challenge string that gets embedded in
  /// the attestation client data.
  ///
  /// Returns a record of (keyId, attestationBase64).
  Future<({String keyId, String attestation})> attest(String challenge);

  /// Produce a base64-encoded assertion signature over [clientDataHash].
  ///
  /// [keyId] must be a key previously created via [attest].
  /// [clientDataHash] is the SHA-256 hash of the canonical transcript string
  /// (domain separator + JSON body), hex-encoded.
  Future<String> assertion({
    required String keyId,
    required String clientDataHash,
  });
}

/// Production implementation using a platform method channel.
///
/// The native side (Swift) must register handlers for:
/// - `attest(challenge: String)` → `{key_id, attestation}`
/// - `assert(key_id, client_data_hash)` → `{assertion}`
class MethodChannelAppAttestBridge implements AppAttestBridge {
  static const _channel = MethodChannel('com.block.buzz/app_attest');

  @override
  Future<({String keyId, String attestation})> attest(
    String challenge,
  ) async {
    if (!Platform.isIOS) {
      throw UnsupportedError('App Attest is only available on iOS');
    }

    final result = await _channel.invokeMapMethod<String, dynamic>('attest', {
      'challenge': challenge,
    });

    if (result == null) {
      throw PlatformException(
        code: 'app_attest_failed',
        message: 'Native attest returned null',
      );
    }

    return (
      keyId: result['key_id'] as String,
      attestation: result['attestation'] as String,
    );
  }

  @override
  Future<String> assertion({
    required String keyId,
    required String clientDataHash,
  }) async {
    if (!Platform.isIOS) {
      throw UnsupportedError('App Attest is only available on iOS');
    }

    final result = await _channel.invokeMapMethod<String, dynamic>('assert', {
      'key_id': keyId,
      'client_data_hash': clientDataHash,
    });

    if (result == null) {
      throw PlatformException(
        code: 'app_attest_failed',
        message: 'Native assert returned null',
      );
    }

    return result['assertion'] as String;
  }
}

/// A fake bridge for testing — returns deterministic values without touching
/// the platform layer.
class FakeAppAttestBridge implements AppAttestBridge {
  String? _nextKeyId = 'fake-key-id';
  String _nextAttestation = 'fake-attestation';
  String _nextAssertion = 'fake-assertion';

  void setAttestationResponse({
    String? keyId,
    String? attestation,
  }) {
    if (keyId != null) _nextKeyId = keyId;
    if (attestation != null) _nextAttestation = attestation;
  }

  void setAssertionResponse(String assertion) {
    _nextAssertion = assertion;
  }

  @override
  Future<({String keyId, String attestation})> attest(
    String challenge,
  ) async {
    final result = (keyId: _nextKeyId!, attestation: _nextAttestation);
    return result;
  }

  @override
  Future<String> assertion({
    required String keyId,
    required String clientDataHash,
  }) async {
    return _nextAssertion;
  }
}
