import 'dart:async';
import 'dart:math';
import 'dart:typed_data';

import 'package:flutter/foundation.dart';
import 'package:flutter_secure_storage/flutter_secure_storage.dart';
import 'package:hooks_riverpod/hooks_riverpod.dart';
import 'package:nostr/nostr.dart' as nostr;

import '../../shared/relay/relay.dart';
import 'apns_token_service.dart';
import 'push_lease_service.dart';
import 'push_models.dart';
import 'push_preferences.dart';

/// State of the push notification lifecycle.
enum PushStatus {
  /// Not started — user not authenticated or platform unsupported.
  idle,

  /// Requesting APNs permission and device token.
  registering,

  /// Fetching relay descriptor and creating the push lease.
  activating,

  /// Lease is live — pushes will be delivered.
  active,

  /// Permission denied by the user.
  denied,

  /// Something went wrong. See [PushState.error].
  error,

  /// User disabled push in settings.
  disabled,
}

class PushState {
  final PushStatus status;
  final String? deviceToken;
  final String? error;

  const PushState({
    this.status = PushStatus.idle,
    this.deviceToken,
    this.error,
  });

  PushState copyWith({
    PushStatus? status,
    String? deviceToken,
    String? error,
  }) =>
      PushState(
        status: status ?? this.status,
        deviceToken: deviceToken ?? this.deviceToken,
        error: error,
      );
}

/// Manages the full push notification lifecycle:
/// APNs registration → descriptor fetch → lease creation → token refresh.
///
/// Persists the installation ID and generation counter in secure storage
/// so they survive app restarts. Token rotation reuses the same lease `d`
/// tag with an incremented generation (per NIP-PL spec).
class PushNotifier extends Notifier<PushState> {
  static const _storage = FlutterSecureStorage();
  static const _keyInstallationId = 'push_installation_id';
  static const _keyGeneration = 'push_generation';
  static const _keyEnabled = 'push_enabled';

  late final PushLeaseService _leaseService;
  StreamSubscription<String>? _tokenRefreshSub;

  @override
  PushState build() {
    // Don't auto-start — the app calls [activate] after auth is confirmed.
    return const PushState();
  }

  /// Initialize the lease service. Called when relay config is available.
  void _initService() {
    final config = ref.read(relayConfigProvider);
    final nsec = config.nsec;
    if (nsec == null || nsec.isEmpty) return;

    final privKeyHex = nostr.Nip19.decode(payload: nsec).data;
    _leaseService = PushLeaseService(
      relay: SignedEventRelay(
        session: ref.read(relaySessionProvider.notifier),
        nsec: nsec,
      ),
      privKeyHex: privKeyHex,
    );
  }

  /// Enable push notifications: request APNs token and publish a lease.
  ///
  /// Safe to call multiple times — if already active, returns immediately.
  /// If the token has changed, rotates the lease with an incremented generation.
  Future<void> activate() async {
    if (state.status == PushStatus.active ||
        state.status == PushStatus.activating ||
        state.status == PushStatus.registering) {
      return;
    }

    // Respect user's disable preference.
    final enabled = await _storage.read(key: _keyEnabled);
    if (enabled == 'false') {
      state = const PushState(status: PushStatus.disabled);
      return;
    }

    _initService();

    state = const PushState(status: PushStatus.registering);

    // 1. Get APNs device token.
    final token = await ApnsTokenService.register();
    if (token == null) {
      final hasPermission = await ApnsTokenService.hasPermission();
      state = PushState(
        status: hasPermission ? PushStatus.error : PushStatus.denied,
        error: hasPermission ? 'APNs registration returned no token' : null,
      );
      return;
    }

    // 2. Fetch relay descriptor and create lease.
    state = PushState(status: PushStatus.activating, deviceToken: token);

    try {
      final config = ref.read(relayConfigProvider);
      final descriptor = await _leaseService.fetchDescriptor(config.baseUrl);

      if (descriptor == null) {
        state = PushState(
          status: PushStatus.error,
          deviceToken: token,
          error: 'Relay does not support push notifications (NIP-PL)',
        );
        return;
      }

      // 3. Build subscriptions for the current user.
      final pubkey = ref.read(myPubkeyProvider);
      if (pubkey == null) {
        state = PushState(
          status: PushStatus.error,
          error: 'No pubkey available',
        );
        return;
      }

      final subscriptions = _buildSubscriptions(pubkey, descriptor);

      // 4. Load or create installation ID + generation.
      final installationId = await _getOrCreateInstallationId();
      final generation = await _incrementGeneration();

      // 5. Publish the lease.
      await _leaseService.createLease(
        deviceToken: token,
        descriptor: descriptor,
        subscriptions: subscriptions,
        installationId: installationId,
        generation: generation,
      );

      state = PushState(status: PushStatus.active, deviceToken: token);

      // 6. Listen for token refresh.
      _tokenRefreshSub?.cancel();
      _tokenRefreshSub = ApnsTokenService.tokenStream.listen((newToken) {
        _onTokenRefreshed(newToken);
      });
    } catch (e) {
      state = PushState(
        status: PushStatus.error,
        deviceToken: token,
        error: e.toString(),
      );
    }
  }

  /// Disable push: revoke the lease and stop listening.
  Future<void> disable() async {
    await _storage.write(key: _keyEnabled, value: 'false');
    _tokenRefreshSub?.cancel();
    await _revokeCurrentLease();
    state = const PushState(status: PushStatus.disabled);
  }

  /// Re-enable push after it was disabled.
  Future<void> enable() async {
    await _storage.write(key: _keyEnabled, value: 'true');
    await activate();
  }

  /// Handle APNs token rotation: update the lease with the new token.
  Future<void> _onTokenRefreshed(String newToken) async {
    if (state.status != PushStatus.active) return;
    if (state.deviceToken == newToken) return;

    try {
      final config = ref.read(relayConfigProvider);
      final descriptor = await _leaseService.fetchDescriptor(config.baseUrl);
      if (descriptor == null) return;

      final pubkey = ref.read(myPubkeyProvider);
      if (pubkey == null) return;

      final installationId = await _getOrCreateInstallationId();
      final generation = await _incrementGeneration();

      await _leaseService.createLease(
        deviceToken: newToken,
        descriptor: descriptor,
        subscriptions: _buildSubscriptions(pubkey, descriptor),
        installationId: installationId,
        generation: generation,
      );

      state = PushState(status: PushStatus.active, deviceToken: newToken);
      debugPrint('Push: lease rotated for new APNs token');
    } catch (e) {
      debugPrint('Push: token rotation failed: $e');
    }
  }

  /// Revoke the current lease (if any).
  Future<void> _revokeCurrentLease() async {
    if (state.deviceToken == null) return;
    _initService();

    try {
      final config = ref.read(relayConfigProvider);
      final descriptor = await _leaseService.fetchDescriptor(config.baseUrl);
      if (descriptor == null) return;

      final installationId = await _getOrCreateInstallationId();
      final generation = await _readGeneration();

      await _leaseService.revokeLease(
        descriptor: descriptor,
        installationId: installationId,
        currentGeneration: generation,
        lastDeviceToken: state.deviceToken!,
      );
    } catch (e) {
      debugPrint('Push: lease revocation failed: $e');
    }
  }

  /// Build the subscription list based on user preferences and relay support.
  ///
  /// Reads [PushPreferences] so the user can toggle categories in settings.
  List<PushSubscription> _buildSubscriptions(
    String pubkey,
    PushDescriptor descriptor,
  ) {
    final prefs = ref.read(pushPreferencesProvider);
    final subs = <PushSubscription>[];

    // @mentions — time-sensitive.
    if (prefs.mentions && descriptor.pushKinds.contains(9)) {
      subs.add(PushSubscription.mentions(pubkey));
    }

    // DMs — time-sensitive.
    if (prefs.dms && descriptor.pushKinds.contains(1059)) {
      subs.add(PushSubscription.dms(pubkey));
    }

    // Agent activity — default priority.
    if (prefs.agentActivity && descriptor.pushKinds.contains(40007)) {
      subs.add(PushSubscription.agentActivity(pubkey));
    }

    return subs;
  }

  /// Re-publish the lease with updated subscriptions after preference changes.
  ///
  /// Called from the settings UI when the user toggles a category. If push
  /// is not active, this is a no-op — the new preferences will take effect
  /// on the next [activate].
  Future<void> refreshSubscriptions() async {
    if (state.status != PushStatus.active) return;
    if (state.deviceToken == null) return;

    _initService();

    try {
      final config = ref.read(relayConfigProvider);
      final descriptor = await _leaseService.fetchDescriptor(config.baseUrl);
      if (descriptor == null) return;

      final pubkey = ref.read(myPubkeyProvider);
      if (pubkey == null) return;

      final installationId = await _getOrCreateInstallationId();
      final generation = await _incrementGeneration();

      await _leaseService.createLease(
        deviceToken: state.deviceToken!,
        descriptor: descriptor,
        subscriptions: _buildSubscriptions(pubkey, descriptor),
        installationId: installationId,
        generation: generation,
      );

      debugPrint('Push: lease refreshed with updated preferences');
    } catch (e) {
      debugPrint('Push: subscription refresh failed: $e');
    }
  }

  /// Generate or load the per-installation random ID (NIP-PL `d` tag).
  Future<String> _getOrCreateInstallationId() async {
    var id = await _storage.read(key: _keyInstallationId);
    if (id == null) {
      final random = Random.secure();
      final bytes = Uint8List(16);
      for (var i = 0; i < 16; i++) {
        bytes[i] = random.nextInt(256);
      }
      id = bytes.map((b) => b.toRadixString(16).padLeft(2, '0')).join();
      await _storage.write(key: _keyInstallationId, value: id);
    }
    return id;
  }

  /// Read and increment the generation counter.
  Future<int> _incrementGeneration() async {
    final raw = await _storage.read(key: _keyGeneration);
    final current = raw == null ? 0 : int.tryParse(raw) ?? 0;
    final next = current + 1;
    await _storage.write(key: _keyGeneration, value: next.toString());
    return next;
  }

  /// Read the current generation without incrementing.
  Future<int> _readGeneration() async {
    final raw = await _storage.read(key: _keyGeneration);
    return raw == null ? 1 : int.tryParse(raw) ?? 1;
  }

  @override
  void dispose() {
    _tokenRefreshSub?.cancel();
    super.dispose();
  }
}

final pushProvider = NotifierProvider<PushNotifier, PushState>(
  PushNotifier.new,
);
