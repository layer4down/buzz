import 'dart:convert';

/// NIP-PL push notification priority class.
///
/// Maps to iOS notification interruption levels:
/// - `silent` → no banner, badge only
/// - `default` → standard banner
/// - `time_sensitive` → breaks through Focus modes
/// - `urgent` → highest priority, Critical Alert (requires Apple entitlement)
enum PushClass {
  silent,
  defaultClass,
  timeSensitive,
  urgent;

  String get wire => switch (this) {
        silent => 'silent',
        defaultClass => 'default',
        timeSensitive => 'time_sensitive',
        urgent => 'urgent',
      };
}

/// One subscription entry inside a push lease.
///
/// The [filter] is a standard Nostr filter (kinds + tags). When the relay
/// matches an event against it, a wake push is dispatched with [pushClass].
class PushSubscription {
  final Map<String, dynamic> filter;
  final PushClass pushClass;

  /// Optional ignore filters — events matching ANY ignore are suppressed.
  final List<Map<String, dynamic>> ignore;

  const PushSubscription({
    required this.filter,
    this.pushClass = PushClass.defaultClass,
    this.ignore = const [],
  });

  Map<String, dynamic> toJson() => {
        'filter': filter,
        'class': pushClass.wire,
        if (ignore.isNotEmpty) 'ignore': ignore,
      };

  /// Standard "@mention in any channel" subscription for [userPubkey].
  factory PushSubscription.mentions(String userPubkey) => PushSubscription(
        filter: {
          'kinds': [9],
          '#p': [userPubkey],
        },
        pushClass: PushClass.timeSensitive,
      );

  /// DMs (gift-wrapped, kind 1059) subscription for [userPubkey].
  factory PushSubscription.dms(String userPubkey) => PushSubscription(
        filter: {
          'kinds': [1059],
          '#p': [userPubkey],
        },
        pushClass: PushClass.timeSensitive,
      );

  /// Agent activity (kind 40007) subscription for [userPubkey].
  factory PushSubscription.agentActivity(String userPubkey) => PushSubscription(
        filter: {
          'kinds': [40007],
          '#p': [userPubkey],
        },
        pushClass: PushClass.defaultClass,
      );
}

/// Decoded NIP-11 push descriptor — tells the client which executor key
/// to encrypt lease content to and which kinds the relay supports pushing.
class PushDescriptor {
  /// Executor key ID (the `exec` tag value to use in the lease event).
  final String executorKeyId;

  /// Executor encryption pubkey (hex) — NIP-44 encrypt lease content to this.
  final String executorPubkey;

  /// Relay origin identifier — byte-for-byte copied into lease plaintext.
  final String origin;

  /// Event kinds the relay will push.
  final List<int> pushKinds;

  const PushDescriptor({
    required this.executorKeyId,
    required this.executorPubkey,
    required this.origin,
    required this.pushKinds,
  });

  /// Parse the `push` object from a NIP-11 relay descriptor JSON.
  /// Returns null if push is not configured.
  static PushDescriptor? fromNip11(
    Map<String, dynamic> descriptor,
    String relayOrigin,
  ) {
    final push = descriptor['push'];
    if (push is! Map<String, dynamic>) return null;

    final keyId = push['id'] as String?;
    final key = push['key'] as String?;
    if (keyId == null || key == null) return null;

    final kinds = push['push_kinds'];
    final pushKinds = kinds is List
        ? kinds.map((e) => (e as num).toInt()).toList()
        : <int>[7, 9, 1059, 40007];

    return PushDescriptor(
      executorKeyId: keyId,
      executorPubkey: key,
      origin: push['origin'] as String? ?? relayOrigin,
      pushKinds: pushKinds,
    );
  }
}

/// Plaintext content encrypted inside a kind:30350 lease event.
class LeaseContent {
  static const version = 1;

  final String origin;
  final String appProfile;
  final String transport;
  final String endpoint;
  final int generation;
  final bool active;
  final List<PushSubscription> subscriptions;

  const LeaseContent({
    required this.origin,
    required this.appProfile,
    required this.transport,
    required this.endpoint,
    required this.generation,
    required this.active,
    required this.subscriptions,
  });

  Map<String, dynamic> toJson() => {
        'v': version,
        'origin': origin,
        'app_profile': appProfile,
        'transport': transport,
        'endpoint': endpoint,
        'generation': generation,
        'active': active,
        'subscriptions': subscriptions.map((s) => s.toJson()).toList(),
      };

  String toJsonString() => jsonEncode(toJson());

  /// Create a revocation tombstone (active: false) for this lease.
  LeaseContent revoke() => LeaseContent(
        origin: origin,
        appProfile: appProfile,
        transport: transport,
        endpoint: endpoint,
        generation: generation + 1,
        active: false,
        subscriptions: const [],
      );
}
