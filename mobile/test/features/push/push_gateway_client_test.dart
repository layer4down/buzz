import 'dart:convert';

import 'package:buzz/features/push/push_gateway_client.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart' as http_testing;

void main() {
  group('PushGatewayClient', () {
    test('getChallenge parses response correctly', () async {
      final client = PushGatewayClient(
        baseUrl: 'https://push.example.com',
        httpClient: http_testing.MockClient((request) async {
          expect(request.url.path, '/v1/installations/challenges');
          final body = jsonDecode(request.body) as Map<String, dynamic>;
          expect(body['v'], 1);

          return http.Response(
            jsonEncode({
              'challenge_id': 'chg-123',
              'challenge': 'Y2hhbGxlbmdlLXZhbHVl',
              'expires_at': 1700000300,
            }),
            200,
          );
        }),
      );

      final challenge = await client.getChallenge();
      expect(challenge.challengeId, 'chg-123');
      expect(challenge.challenge, 'Y2hhbGxlbmdlLXZhbHVl');
      expect(challenge.expiresAt, 1700000300);
    });

    test('enroll sends correct body and parses response', () async {
      final client = PushGatewayClient(
        baseUrl: 'https://push.example.com/',
        httpClient: http_testing.MockClient((request) async {
          expect(request.url.path, '/v1/installations');
          final body = jsonDecode(request.body) as Map<String, dynamic>;
          expect(body['v'], 1);
          expect(body['key_id'], 'key-abc');
          expect(body['attestation'], 'att-xyz');
          expect(body['app_profile'], 'buzz-ios-production');
          expect(body['endpoint'], 'abc123token');

          return http.Response(
            jsonEncode({
              'installation_handle': 'inst-456',
              'endpoint_epoch': 1,
              'expires_at': 1700000300,
            }),
            200,
          );
        }),
      );

      final result = await client.enroll(
        challengeId: 'chg-123',
        challenge: 'Y2hhbGxlbmdlLXZhbHVl',
        keyId: 'key-abc',
        attestation: 'att-xyz',
        appProfile: AppProfile.buzzIosProduction,
        endpoint: 'abc123token',
        endpointEpoch: 1,
        expiresAt: 1700000300,
      );

      expect(result.installationHandle, 'inst-456');
      expect(result.endpointEpoch, 1);
    });

    test('delegate sends correct body and parses endpoint_grant', () async {
      final client = PushGatewayClient(
        baseUrl: 'https://push.example.com',
        httpClient: http_testing.MockClient((request) async {
          expect(request.url.path, '/v1/delegations');
          final body = jsonDecode(request.body) as Map<String, dynamic>;
          expect(body['relay_pubkey'], 'a'.padRight(64, 'a'));
          expect(body['assertion'], 'assertion-base64');

          return http.Response(
            jsonEncode({'endpoint_grant': 'grant-opaque-ciphertext'}),
            200,
          );
        }),
      );

      final result = await client.delegate(
        challengeId: 'chg-123',
        challenge: 'challenge',
        installationHandle: 'inst-456',
        endpointEpoch: 1,
        generation: 1,
        relayPubkey: 'a'.padRight(64, 'a'),
        notBefore: 1700000000,
        expiresAt: 1700000300,
        assertion: 'assertion-base64',
      );

      expect(result.endpointGrant, 'grant-opaque-ciphertext');
    });

    test('revokeDelegation succeeds on 200', () async {
      final client = PushGatewayClient(
        baseUrl: 'https://push.example.com',
        httpClient: http_testing.MockClient((request) async {
          expect(request.url.path, '/v1/delegations/revoke');
          return http.Response(
            jsonEncode({'status': 'ok'}),
            200,
          );
        }),
      );

      await client.revokeDelegation(
        challengeId: 'chg-123',
        challenge: 'challenge',
        installationHandle: 'inst-456',
        relayPubkey: 'a'.padRight(64, 'a'),
        generation: 1,
        assertion: 'assertion',
      );
    });

    test('rotateEndpoint succeeds on 200', () async {
      final client = PushGatewayClient(
        baseUrl: 'https://push.example.com',
        httpClient: http_testing.MockClient((request) async {
          expect(request.url.path, '/v1/installations/endpoint');
          final body = jsonDecode(request.body) as Map<String, dynamic>;
          expect(body['new_endpoint_epoch'], 2);

          return http.Response(
            jsonEncode({'status': 'ok'}),
            200,
          );
        }),
      );

      await client.rotateEndpoint(
        challengeId: 'chg-123',
        challenge: 'challenge',
        installationHandle: 'inst-456',
        endpointEpoch: 1,
        newEndpointEpoch: 2,
        endpoint: 'newtoken',
        assertion: 'assertion',
      );
    });

    test('revokeInstallation succeeds on 200', () async {
      final client = PushGatewayClient(
        baseUrl: 'https://push.example.com',
        httpClient: http_testing.MockClient((request) async {
          expect(request.url.path, '/v1/installations/revoke');
          return http.Response(
            jsonEncode({'status': 'ok'}),
            200,
          );
        }),
      );

      await client.revokeInstallation(
        challengeId: 'chg-123',
        challenge: 'challenge',
        installationHandle: 'inst-456',
        endpointEpoch: 1,
        newEndpointEpoch: 2,
        assertion: 'assertion',
      );
    });

    test('throws PushGatewayException on error status', () async {
      final client = PushGatewayClient(
        baseUrl: 'https://push.example.com',
        httpClient: http_testing.MockClient((request) async {
          return http.Response(
            jsonEncode({'error': 'invalid_attestation'}),
            401,
          );
        }),
      );

      expect(
        () => client.getChallenge(),
        throwsA(
          isA<PushGatewayException>()
              .having((e) => e.statusCode, 'statusCode', 401)
              .having((e) => e.errorCode, 'errorCode', 'invalid_attestation'),
        ),
      );
    });

    test('handles error response without JSON body', () async {
      final client = PushGatewayClient(
        baseUrl: 'https://push.example.com',
        httpClient: http_testing.MockClient((request) async {
          return http.Response('Internal Server Error', 500);
        }),
      );

      expect(
        () => client.getChallenge(),
        throwsA(
          isA<PushGatewayException>()
              .having((e) => e.statusCode, 'statusCode', 500)
              .having((e) => e.errorCode, 'errorCode', 'unknown'),
        ),
      );
    });

    test('strips trailing slash from baseUrl', () async {
      String? capturedPath;
      final client = PushGatewayClient(
        baseUrl: 'https://push.example.com/',
        httpClient: http_testing.MockClient((request) async {
          capturedPath = request.url.path;
          return http.Response(
            jsonEncode({
              'challenge_id': 'c1',
              'challenge': 'ch',
              'expires_at': 1,
            }),
            200,
          );
        }),
      );

      await client.getChallenge();
      expect(capturedPath, '/v1/installations/challenges');
    });
  });

  group('AppProfile', () {
    test('wire names match gateway spec', () {
      expect(AppProfile.buzzIosProduction.wire, 'buzz-ios-production');
      expect(AppProfile.buzzIosSandbox.wire, 'buzz-ios-sandbox');
    });

    test('development returns sandbox profile', () {
      expect(AppProfile.development, AppProfile.buzzIosSandbox);
    });
  });
}
