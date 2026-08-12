import 'dart:convert';

import 'package:buzz/features/push/app_attest_bridge.dart';
import 'package:buzz/features/push/enrollment_orchestrator.dart';
import 'package:buzz/features/push/push_gateway_client.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart' as http_testing;

void main() {
  group('EnrollmentOrchestrator', () {
    test('enrollAndDelegate calls gateway and app attest in correct sequence',
        () async {
      final fakeBridge = FakeAppAttestBridge();
      final requestLog = <String>[];

      final client = PushGatewayClient(
        baseUrl: 'https://push.example.com',
        httpClient: http_testing.MockClient((request) async {
          final path = request.url.path;
          requestLog.add(path);

          if (path == '/v1/installations/challenges') {
            return http.Response(
              jsonEncode({
                'challenge_id': 'chg-${requestLog.length}',
                'challenge': 'Y2hhbGxlbmdl',
                'expires_at': 1700000300,
              }),
              200,
            );
          }

          if (path == '/v1/installations') {
            return http.Response(
              jsonEncode({
                'installation_handle': 'inst-abc',
                'endpoint_epoch': 1,
                'expires_at': 1700000300,
              }),
              200,
            );
          }

          if (path == '/v1/delegations') {
            return http.Response(
              jsonEncode({'endpoint_grant': 'grant-opaque'}),
              200,
            );
          }

          return http.Response('{}', 404);
        }),
      );

      final orchestrator = EnrollmentOrchestrator(
        gateway: client,
        appAttest: fakeBridge,
      );

      final result = await orchestrator.enrollAndDelegate(
        deviceToken: 'abc123token',
        relayPubkey: 'a'.padRight(64, 'a'),
      );

      // Verify the HTTP call sequence.
      expect(requestLog, [
        '/v1/installations/challenges', // enroll challenge
        '/v1/installations', // enroll
        '/v1/installations/challenges', // delegate challenge
        '/v1/delegations', // delegate
      ]);

      // Verify the result.
      expect(result.installationHandle, 'inst-abc');
      expect(result.endpointGrant, 'grant-opaque');
      expect(result.keyId, 'fake-key-0');
      expect(result.endpointEpoch, 1);

      // Verify a key was generated.
      expect(fakeBridge.generatedKeys, hasLength(1));
    });

    test('revokeInstallation calls gateway in correct sequence', () async {
      final fakeBridge = FakeAppAttestBridge();
      final requestLog = <String>[];

      final client = PushGatewayClient(
        baseUrl: 'https://push.example.com',
        httpClient: http_testing.MockClient((request) async {
          final path = request.url.path;
          requestLog.add(path);

          if (path == '/v1/installations/challenges') {
            return http.Response(
              jsonEncode({
                'challenge_id': 'chg-1',
                'challenge': 'Y2hhbGxlbmdl',
                'expires_at': 1700000300,
              }),
              200,
            );
          }

          if (path == '/v1/installations/revoke') {
            return http.Response(
              jsonEncode({'status': 'ok'}),
              200,
            );
          }

          return http.Response('{}', 404);
        }),
      );

      final orchestrator = EnrollmentOrchestrator(
        gateway: client,
        appAttest: fakeBridge,
      );

      await orchestrator.revokeInstallation(
        installationHandle: 'inst-abc',
        keyId: 'key-xyz',
        endpointEpoch: 1,
      );

      expect(requestLog, [
        '/v1/installations/challenges',
        '/v1/installations/revoke',
      ]);
    });

    test('enrollAndDelegate propagates gateway errors', () async {
      final fakeBridge = FakeAppAttestBridge();

      final client = PushGatewayClient(
        baseUrl: 'https://push.example.com',
        httpClient: http_testing.MockClient((request) async {
          return http.Response(
            jsonEncode({'error': 'invalid_attestation'}),
            401,
          );
        }),
      );

      final orchestrator = EnrollmentOrchestrator(
        gateway: client,
        appAttest: fakeBridge,
      );

      expect(
        () => orchestrator.enrollAndDelegate(
          deviceToken: 'token',
          relayPubkey: 'a'.padRight(64, 'a'),
        ),
        throwsA(isA<PushGatewayException>()),
      );
    });

    test('enroll transcript includes correct fields', () async {
      // Verify the enroll request body contains all expected fields.
      Map<String, dynamic>? enrollBody;

      final client = PushGatewayClient(
        baseUrl: 'https://push.example.com',
        httpClient: http_testing.MockClient((request) async {
          if (request.url.path == '/v1/installations') {
            enrollBody = jsonDecode(request.body) as Map<String, dynamic>;
            return http.Response(
              jsonEncode({
                'installation_handle': 'inst-1',
                'endpoint_epoch': 1,
                'expires_at': 999999999,
              }),
              200,
            );
          }
          if (request.url.path == '/v1/installations/challenges') {
            return http.Response(
              jsonEncode({
                'challenge_id': 'c1',
                'challenge': 'ch',
                'expires_at': 999999999,
              }),
              200,
            );
          }
          return http.Response(
            jsonEncode({'endpoint_grant': 'g'}),
            200,
          );
        }),
      );

      final orchestrator = EnrollmentOrchestrator(
        gateway: client,
        appAttest: FakeAppAttestBridge(),
      );

      await orchestrator.enrollAndDelegate(
        deviceToken: 'deadbeef',
        relayPubkey: 'b'.padRight(64, 'b'),
        appProfile: AppProfile.buzzIosProduction,
      );

      expect(enrollBody, isNotNull);
      expect(enrollBody!['v'], 1);
      expect(enrollBody!['key_id'], 'fake-key-0');
      expect(enrollBody!['app_profile'], 'buzz-ios-production');
      expect(enrollBody!['endpoint'], 'deadbeef');
      expect(enrollBody!['endpoint_epoch'], 1);
      expect(enrollBody!['attestation'], 'fake-attestation');
    });
  });
}
