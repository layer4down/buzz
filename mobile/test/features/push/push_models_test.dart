import 'dart:convert';

import 'package:buzz/features/push/push_models.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  group('PushClass', () {
    test('wire names match NIP-PL spec', () {
      expect(PushClass.silent.wire, 'silent');
      expect(PushClass.defaultClass.wire, 'default');
      expect(PushClass.timeSensitive.wire, 'time_sensitive');
      expect(PushClass.urgent.wire, 'urgent');
    });
  });

  group('PushSubscription', () {
    const pubkey = 'abcdef0123456789'.padRight(64, '0');

    test('mentions factory creates correct filter', () {
      final sub = PushSubscription.mentions(pubkey);
      expect(sub.filter['kinds'], [9]);
      expect(sub.filter['#p'], [pubkey]);
      expect(sub.pushClass, PushClass.timeSensitive);
    });

    test('dms factory creates correct filter', () {
      final sub = PushSubscription.dms(pubkey);
      expect(sub.filter['kinds'], [1059]);
      expect(sub.filter['#p'], [pubkey]);
      expect(sub.pushClass, PushClass.timeSensitive);
    });

    test('agentActivity factory creates correct filter', () {
      final sub = PushSubscription.agentActivity(pubkey);
      expect(sub.filter['kinds'], [40007]);
      expect(sub.filter['#p'], [pubkey]);
      expect(sub.pushClass, PushClass.defaultClass);
    });

    test('toJson serializes correctly', () {
      final sub = PushSubscription.mentions(pubkey);
      final json = sub.toJson();
      expect(json['filter'], isA<Map>());
      expect(json['class'], 'time_sensitive');
    });
  });

  group('PushDescriptor', () {
    test('fromNip11 parses a valid push descriptor', () {
      final descriptor = {
        'push': {
          'id': 'executor-key-1',
          'key': 'a'.padRight(64, 'a'),
          'push_kinds': [7, 9, 1059],
          'origin': 'relay.example.com',
        },
      };

      final push = PushDescriptor.fromNip11(descriptor, 'relay.example.com');
      expect(push, isNotNull);
      expect(push!.executorKeyId, 'executor-key-1');
      expect(push.executorPubkey, 'a'.padRight(64, 'a'));
      expect(push.origin, 'relay.example.com');
      expect(push.pushKinds, [7, 9, 1059]);
    });

    test('fromNip11 returns null when push is not configured', () {
      final descriptor = <String, dynamic>{};
      expect(PushDescriptor.fromNip11(descriptor, 'example.com'), isNull);
    });

    test('fromNip11 returns null when push is missing id or key', () {
      final descriptor = {
        'push': {'id': 'executor-key-1'},
      };
      expect(PushDescriptor.fromNip11(descriptor, 'example.com'), isNull);
    });

    test('fromNip11 falls back to default push kinds when absent', () {
      final descriptor = {
        'push': {
          'id': 'key-1',
          'key': 'b'.padRight(64, 'b'),
        },
      };
      final push = PushDescriptor.fromNip11(descriptor, 'example.com');
      expect(push!.pushKinds, containsAll([7, 9, 1059, 40007]));
    });

    test('fromNip11 uses relayOrigin when descriptor has no origin', () {
      final descriptor = {
        'push': {
          'id': 'key-1',
          'key': 'c'.padRight(64, 'c'),
        },
      };
      final push = PushDescriptor.fromNip11(descriptor, 'my-relay.com');
      expect(push!.origin, 'my-relay.com');
    });
  });

  group('LeaseContent', () {
    final subs = [
      PushSubscription.mentions('d'.padRight(64, 'd')),
    ];

    test('toJson produces correct NIP-PL structure', () {
      final content = LeaseContent(
        origin: 'relay.example.com',
        appProfile: 'com.block.buzz/ios',
        transport: 'apns',
        endpoint: 'abc123token',
        generation: 1,
        active: true,
        subscriptions: subs,
      );

      final json = content.toJson();
      expect(json['v'], 1);
      expect(json['origin'], 'relay.example.com');
      expect(json['app_profile'], 'com.block.buzz/ios');
      expect(json['transport'], 'apns');
      expect(json['endpoint'], 'abc123token');
      expect(json['generation'], 1);
      expect(json['active'], true);
      expect(json['subscriptions'], isA<List>());
      expect((json['subscriptions'] as List).length, 1);
    });

    test('toJsonString produces valid JSON', () {
      final content = LeaseContent(
        origin: 'test.com',
        appProfile: 'com.block.buzz/ios',
        transport: 'apns',
        endpoint: 'token',
        generation: 3,
        active: true,
        subscriptions: const [],
      );

      final decoded = jsonDecode(content.toJsonString());
      expect(decoded, isA<Map>());
      expect(decoded['v'], 1);
      expect(decoded['generation'], 3);
    });

    test('revoke produces tombstone with active=false and incremented generation', () {
      final content = LeaseContent(
        origin: 'test.com',
        appProfile: 'com.block.buzz/ios',
        transport: 'apns',
        endpoint: 'token',
        generation: 5,
        active: true,
        subscriptions: subs,
      );

      final tombstone = content.revoke();
      expect(tombstone.active, false);
      expect(tombstone.generation, 6);
      expect(tombstone.subscriptions, isEmpty);
      expect(tombstone.origin, content.origin);
    });
  });
}
