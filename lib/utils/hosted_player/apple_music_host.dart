import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:flutter/services.dart';

import '../logger.dart';
import 'hosted_player.dart';

/// Plays Apple Music through the macOS runner's MusicKit host. Dart only forwards: batches go
/// to the runner as JSON text, and the runner's events come back the same way.
class AppleMusicHost implements HostedPlayerHost {
  AppleMusicHost({MethodChannel? channel})
    : _channel = channel ?? const MethodChannel(channelName);

  static const channelName = 'ci.not.rune/apple_music';

  final MethodChannel _channel;
  void Function(Map<String, dynamic>)? _emit;
  Future<bool>? _announced;
  Future<void> _queue = Future.value();

  /// Registers the host on macOS, where the runner provides the channel.
  static void registerOnPlatform() {
    if (Platform.isMacOS) platformHosts['appleMusic'] = AppleMusicHost();
  }

  @override
  Future<HostAuthorization?> authorize({bool interactive = false}) async {
    final Object? reply;
    try {
      reply = await _channel.invokeMethod<Object>('authorize', {
        'interactive': interactive,
      });
    } on PlatformException catch (error) {
      logger.w('Apple Music authorization failed: ${error.code}');
      return null;
    } on MissingPluginException {
      return null;
    } catch (error) {
      logger.w('Apple Music authorization failed: ${error.runtimeType}');
      return null;
    }
    return parseAuthorization(reply);
  }

  /// Reads the runner's reply. Without both an authorization and a user token there is nothing to
  /// sign in with.
  static HostAuthorization? parseAuthorization(Object? reply) {
    if (reply is! Map || reply['authorized'] != true) return null;
    final token = reply['token'];
    if (token is! String || token.isEmpty) return null;
    final developerToken = reply['developerToken'];
    return HostAuthorization(
      token: token,
      developerToken: developerToken is String && developerToken.isNotEmpty
          ? developerToken
          : null,
    );
  }

  @override
  void handle(
    Map<String, dynamic> command,
    void Function(Map<String, dynamic>) emit,
  ) {
    if (_emit == null) {
      _channel.setMethodCallHandler(_onCall);
    }
    _emit = emit;
    // The first batch is preceded by what this device can do, so Rust knows before it plays.
    // Batches wait in line behind it, in order.
    final announced = _announced ??= _announce();
    _queue = _queue.then((_) async {
      if (await announced) await _forward(command);
    });
  }

  /// Asks the runner what it can do and passes the answer on. Never throws; false when the
  /// runner cannot be reached, in which case Rust is told that nothing can be played here.
  Future<bool> _announce() async {
    try {
      final reply = await _channel.invokeMethod<Object>('status');
      if (reply is String && _deliver(reply)) return true;
      logger.w('Apple Music status was not usable');
    } on PlatformException catch (error) {
      logger.w('Apple Music status failed: ${error.code}');
    } on MissingPluginException {
      // The runner has no host.
    } catch (error) {
      logger.w('Apple Music status failed: ${error.runtimeType}');
    }
    _emit?.call(_unavailable);
    return false;
  }

  static const _unavailable = {
    'type': 'hostReady',
    'host': 'appleMusic',
    'authorized': true,
    'canPlay': false,
  };

  Future<void> _forward(Map<String, dynamic> command) async {
    try {
      await _channel.invokeMethod<void>('batch', jsonEncode(command));
    } on PlatformException catch (error) {
      logger.w('Apple Music batch failed: ${error.code}');
    } on MissingPluginException {
      // The runner has no host; the announcement already said so.
    } catch (error) {
      logger.w('Apple Music batch failed: ${error.runtimeType}');
    }
  }

  Future<Object?> _onCall(MethodCall call) async {
    if (call.method == 'event' && call.arguments is String) {
      _deliver(call.arguments as String);
    }
    return null;
  }

  /// Passes an event of the runner on; false when it is not a JSON object.
  bool _deliver(String json) {
    final Object? decoded;
    try {
      decoded = jsonDecode(json);
    } on FormatException {
      logger.w('Ignoring an Apple Music event that does not parse');
      return false;
    }
    if (decoded is! Map<String, dynamic>) return false;
    _emit?.call(decoded);
    return true;
  }
}
