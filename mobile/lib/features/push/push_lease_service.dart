import 'dart:convert';

import 'package:http/http.dart' as http;

import '../../shared/crypto/nip44.dart';
import '../../shared/relay/relay.dart';
import 'push_models.dart';

/// NIP-PL kind for push lease events.
const kPushLeaseKind = 30350;

/// Default lease TTL: 30 days (matching NIP-PL max_lease_ttl default).
const _defaultLeaseTtl = Duration(days: 30);

/// Creates, publishes, and revokes NIP-PL push leases.
///
/// A push lease is a kind:30350 Nostr event that asks the relay to keep
/// a filter active after the client disconnects and wake the device via
/// APNs when it matches. The lease content is NIP-44 encrypted to the
/// relay's executor key.
class PushLeaseService {
  final SignedEventRelay _relay;

  /// The hex private key derived from the user's nsec.
  final String _privKeyHex;

  PushLeaseService({
    required SignedEventRelay relay,
    required String privKeyHex,
  })  : _relay = relay,
        _privKeyHex = privKeyHex;

  /// Fetch the relay's NIP-11 descriptor and extract push configuration.
  ///
  /// Returns null if the relay does not support push (no `push` field in
  /// its NIP-11 descriptor).
  Future<PushDescriptor?> fetchDescriptor(String relayBaseUrl) async {
    final response = await http.get(
      Uri.parse(relayBaseUrl),
      headers: {'Accept': 'application/nostr+json'},
    );

    if (response.statusCode != 200) {
      throw Exception(
        'NIP-11 descriptor fetch failed: ${response.statusCode}',
      );
    }

    final descriptor = jsonDecode(response.body) as Map<String, dynamic>;
    final origin = Uri.parse(relayBaseUrl).host;
    return PushDescriptor.fromNip11(descriptor, origin);
  }

  /// Create and publish a push lease.
  ///
  /// [deviceToken] is the hex APNs device token.
  /// [descriptor] is the relay's push descriptor (from [fetchDescriptor]).
  /// [subscriptions] defines which events trigger a push.
  /// [installationId] is the `d` tag — persisted by the caller so token
  ///   rotation reuses the same lease address.
  /// [generation] is the lease generation counter — increment on each update.
  Future<NostrEvent> createLease({
    required String deviceToken,
    required PushDescriptor descriptor,
    required List<PushSubscription> subscriptions,
    required String installationId,
    int generation = 1,
    Duration ttl = _defaultLeaseTtl,
  }) async {
    final content = LeaseContent(
      origin: descriptor.origin,
      appProfile: 'com.block.buzz/ios',
      transport: 'apns',
      endpoint: deviceToken,
      generation: generation,
      active: true,
      subscriptions: subscriptions,
    );

    final ciphertext = _encryptToExecutor(content.toJsonString(), descriptor);

    final expiration = DateTime.now().add(ttl).millisecondsSinceEpoch ~/ 1000;

    return _relay.submit(
      kind: kPushLeaseKind,
      content: ciphertext,
      tags: [
        ['d', installationId],
        ['expiration', expiration.toString()],
        ['exec', descriptor.executorKeyId],
        ['alt', 'Push lease'],
      ],
    );
  }

  /// Revoke (tombstone) an existing lease.
  ///
  /// Publishes an updated lease with `active: false` and an incremented
  /// generation. The relay stops matching immediately.
  Future<NostrEvent> revokeLease({
    required PushDescriptor descriptor,
    required String installationId,
    required int currentGeneration,
    required String lastDeviceToken,
  }) async {
    final content = LeaseContent(
      origin: descriptor.origin,
      appProfile: 'com.block.buzz/ios',
      transport: 'apns',
      endpoint: lastDeviceToken,
      generation: currentGeneration + 1,
      active: false,
      subscriptions: const [],
    );

    final ciphertext = _encryptToExecutor(content.toJsonString(), descriptor);

    // Tombstones use a short expiration — it's dated, not live.
    final expiration = DateTime.now().add(const Duration(hours: 1))
        .millisecondsSinceEpoch ~/ 1000;

    return _relay.submit(
      kind: kPushLeaseKind,
      content: ciphertext,
      tags: [
        ['d', installationId],
        ['expiration', expiration.toString()],
        ['exec', descriptor.executorKeyId],
        ['alt', 'Push lease (revoked)'],
      ],
    );
  }

  /// NIP-44 encrypt [plaintext] to the executor's advertised pubkey.
  String _encryptToExecutor(String plaintext, PushDescriptor descriptor) {
    final conversationKey = getConversationKey(
      _privKeyHex,
      descriptor.executorPubkey,
    );
    return nip44Encrypt(conversationKey, plaintext);
  }
}
