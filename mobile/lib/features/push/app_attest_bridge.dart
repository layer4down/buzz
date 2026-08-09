import 'dart:io';

import 'package:flutter/services.dart';

/// Bridge to the native iOS App Attest framework.
///
/// App Attest proves to the push gateway that the device is a genuine Apple
/// device running a legitimate app instance. The native side wraps
/// `DCAppAttestService`.
///
/// Key generation is separated from attestation because the enroll transcript
/// includes the key ID — the orchestrator must generate the key first, then
/// build the transcript with the real key ID, then hash and attest.
///
/// On non-iOS platforms, all calls throw [UnsupportedError].
abstract class AppAttestBridge {
  /// Generate a new App Attest key pair. Returns the key ID (hex string).
  Future<String> generateKey();

  /// Obtain an attestation from Apple for [keyId].
  ///
  /// [clientDataHash] is the hex-encoded SHA-256 hash of the canonical
  /// transcript string. Returns the base64-encoded CBOR attestation object.
  Future<String> attestKey({
    required String keyId,
    required String clientDataHash,
  });

  /// Generate an assertion signature over [clientDataHash] with [keyId].
  ///
  /// Used for mutations: delegate, rotate endpoint, revoke.
  /// Returns the base64-encoded assertion object.
  Future<String> generateAssertion({
    required String keyId,
    required String clientDataHash,
  });
}

/// Production implementation using a platform method channel.
///
/// The native side (Swift) must register handlers for:
/// - `generate_key()` → `{key_id}`
/// - `attest_key(key_id, client_data_hash)` → `{attestation}`
/// - `generate_assertion(key_id, client_data_hash)` → `{assertion}`
class MethodChannelAppAttestBridge implements AppAttestBridge {
  static const _channel = MethodChannel('com.block.buzz/app_attest');

  @override
  Future<String> generateKey() async {
    if (!Platform.isIOS) {
      throw UnsupportedError('App Attest is only available on iOS');
    }

    final result = await _channel.invokeMapMethod<String, dynamic>('generate_key');
    if (result == null) {
      throw PlatformException(
        code: 'app_attest_failed',
        message: 'Native generate_key returned null',
      );
    }
    return result['key_id'] as String;
  }

  @override
  Future<String> attestKey({
    required String keyId,
    required String clientDataHash,
  }) async {
    if (!Platform.isIOS) {
      throw UnsupportedError('App Attest is only available on iOS');
    }

    final result = await _channel.invokeMapMethod<String, dynamic>('attest_key', {
      'key_id': keyId,
      'client_data_hash': clientDataHash,
    });

    if (result == null) {
      throw PlatformException(
        code: 'app_attest_failed',
        message: 'Native attest_key returned null',
      );
    }
    return result['attestation'] as String;
  }

  @override
  Future<String> generateAssertion({
    required String keyId,
    required String clientDataHash,
  }) async {
    if (!Platform.isIOS) {
      throw UnsupportedError('App Attest is only available on iOS');
    }

    final result = await _channel.invokeMapMethod<String, dynamic>('generate_assertion', {
      'key_id': keyId,
      'client_data_hash': clientDataHash,
    });

    if (result == null) {
      throw PlatformException(
        code: 'app_attest_failed',
        message: 'Native generate_assertion returned null',
      );
    }
    return result['assertion'] as String;
  }
}

/// A fake bridge for testing — returns deterministic values without touching
/// the platform layer.
class FakeAppAttestBridge implements AppAttestBridge {
  int _keyCounter = 0;
  String _nextAttestation = 'fake-attestation';
  String _nextAssertion = 'fake-assertion';

  /// Keys generated so far, in order.
  final List<String> generatedKeys = [];

  void setAttestationResponse(String attestation) {
    _nextAttestation = attestation;
  }

  void setAssertionResponse(String assertion) {
    _nextAssertion = assertion;
  }

  @override
  Future<String> generateKey() async {
    final keyId = 'fake-key-${_keyCounter++}';
    generatedKeys.add(keyId);
    return keyId;
  }

  @override
  Future<String> attestKey({
    required String keyId,
    required String clientDataHash,
  }) async {
    return _nextAttestation;
  }

  @override
  Future<String> generateAssertion({
    required String keyId,
    required String clientDataHash,
  }) async {
    return _nextAssertion;
  }
}
