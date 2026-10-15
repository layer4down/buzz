import 'dart:async';
import 'dart:io';

import 'package:flutter/foundation.dart';
import 'package:flutter/services.dart';

/// Platform channel bridge for iOS APNs token registration.
///
/// On iOS, this requests notification permission and retrieves the APNs
/// device token via a method channel. On non-iOS platforms, all calls
/// are no-ops that return null.
///
/// The native side (AppDelegate.swift) must:
/// 1. Call `UIApplication.shared.registerForRemoteNotifications()` when
///    the `register` method is invoked.
/// 2. Forward the token from `application(_:didRegisterForRemoteNotificationsWithDeviceToken:)`
///    back through the channel as a hex string.
class ApnsTokenService {
  static const _channel = MethodChannel('com.block.buzz/apns');

  /// Request notification permission and register for remote notifications.
  ///
  /// Returns the APNs device token as a hex string, or null if:
  /// - The platform is not iOS
  /// - Permission was denied
  /// - Registration failed
  ///
  /// On subsequent calls after a successful registration, returns the
  /// cached token immediately.
  static Future<String?> register() async {
    if (!Platform.isIOS) return null;

    try {
      final token = await _channel.invokeMethod<String>('register');
      return token;
    } on PlatformException catch (e) {
      debugPrint('APNs registration failed: ${e.code}: ${e.message}');
      return null;
    } on MissingPluginException {
      debugPrint('APns: native side not wired up yet');
      return null;
    }
  }

  /// Check whether notification permission has been granted.
  static Future<bool> hasPermission() async {
    if (!Platform.isIOS) return false;

    try {
      final granted = await _channel.invokeMethod<bool>('hasPermission');
      return granted ?? false;
    } on PlatformException catch (e) {
      debugPrint('APNs permission check failed: ${e.code}: ${e.message}');
      return false;
    } on MissingPluginException {
      return false;
    }
  }

  /// Stream of token refresh events. APNs tokens can rotate; the native
  /// side pushes new tokens through this channel as they arrive.
  static Stream<String> get tokenStream {
    if (!Platform.isIOS) return const Stream.empty();

    return _eventChannel.receiveBroadcastStream().map(
      (dynamic token) => token as String,
    );
  }

  static const _eventChannel = EventChannel('com.block.buzz/apns/events');
}
