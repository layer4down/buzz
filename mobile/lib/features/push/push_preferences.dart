import 'package:flutter_secure_storage/flutter_secure_storage.dart';
import 'package:hooks_riverpod/hooks_riverpod.dart';

/// Notification categories the user can toggle independently.
enum PushCategory {
  /// @mentions (kind 9 with the user's pubkey in `p` tags).
  mentions,

  /// Direct messages (gift-wrapped, kind 1059).
  dms,

  /// Agent activity events (kind 40007).
  agentActivity,
}

/// User-facing metadata for each category.
extension PushCategoryMeta on PushCategory {
  String get label => switch (this) {
        PushCategory.mentions => 'Mentions',
        PushCategory.dms => 'Direct Messages',
        PushCategory.agentActivity => 'Agent Activity',
      };

  String get description => switch (this) {
        PushCategory.mentions => 'When someone @mentions you',
        PushCategory.dms => 'Direct messages to you',
        PushCategory.agentActivity => 'Agent completions and blockers',
      };
}

/// Per-category push notification preferences.
///
/// Defaults to all categories enabled. Persisted in secure storage so the
/// preference survives app restarts. The [PushNotifier] reads this to decide
/// which subscriptions to include in the lease.
class PushPreferences {
  final bool mentions;
  final bool dms;
  final bool agentActivity;

  const PushPreferences({
    this.mentions = true,
    this.dms = true,
    this.agentActivity = true,
  });

  /// Whether [category] is enabled.
  bool isEnabled(PushCategory category) => switch (category) {
        PushCategory.mentions => mentions,
        PushCategory.dms => dms,
        PushCategory.agentActivity => agentActivity,
      };

  PushPreferences copyWith({
    bool? mentions,
    bool? dms,
    bool? agentActivity,
  }) =>
      PushPreferences(
        mentions: mentions ?? this.mentions,
        dms: dms ?? this.dms,
        agentActivity: agentActivity ?? this.agentActivity,
      );

  /// Serialize to a compact string for secure storage.
  String serialize() => '${mentions ? 1 : 0}'
      '${dms ? 1 : 0}'
      '${agentActivity ? 1 : 0}';

  /// Deserialize from the compact string. Returns defaults on parse failure.
  static PushPreferences deserialize(String? raw) {
    if (raw == null || raw.length < 3) return const PushPreferences();
    return PushPreferences(
      mentions: raw[0] == '1',
      dms: raw[1] == '1',
      agentActivity: raw[2] == '1',
    );
  }
}

/// Loads and persists [PushPreferences] in secure storage.
///
/// On first build, reads the stored value (or defaults). Each [toggle] call
/// persists immediately so preferences survive crashes and restarts.
class PushPreferencesNotifier extends Notifier<PushPreferences> {
  static const _storage = FlutterSecureStorage();
  static const _key = 'push_preferences';

  @override
  PushPreferences build() {
    _loadAsync();
    return const PushPreferences();
  }

  Future<void> _loadAsync() async {
    final raw = await _storage.read(key: _key);
    state = PushPreferences.deserialize(raw);
  }

  /// Toggle [category] on or off and persist.
  void toggle(PushCategory category, {required bool value}) {
    state = switch (category) {
      PushCategory.mentions => state.copyWith(mentions: value),
      PushCategory.dms => state.copyWith(dms: value),
      PushCategory.agentActivity => state.copyWith(agentActivity: value),
    };
    _storage.write(key: _key, value: state.serialize());
  }
}

final pushPreferencesProvider =
    NotifierProvider<PushPreferencesNotifier, PushPreferences>(
  PushPreferencesNotifier.new,
);
