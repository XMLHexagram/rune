import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:flutter/services.dart';

import '../logger.dart';
import 'hosted_player.dart';

/// Plays Apple Music through the macOS runner's MusicKit host. Dart only forwards: batches go
/// to the runner as JSON text, and the runner's events come back the same way.
class AppleMusicHost implements SelfReportingHost {
  AppleMusicHost({MethodChannel? channel})
    : _channel = channel ?? const MethodChannel(channelName);

  static const channelName = 'ci.not.rune/apple_music';

  final MethodChannel _channel;
  void Function(Map<String, dynamic>)? _emit;
  Future<void>? _announced;
  Future<void> _queue = Future.value();

  /// The latest `hostReady` passed on: the announcement's, or one the runner sent since.
  Map<String, dynamic>? _ready;

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

  /// Asks the runner what it can do and passes that on, before the player hands over anything.
  /// The runner drops whatever it still held, since what it reports is a host that just started.
  @override
  void attach(void Function(Map<String, dynamic>) emit) {
    _listen(emit);
    _announced = _announce();
  }

  @override
  void handle(
    Map<String, dynamic> command,
    void Function(Map<String, dynamic>) emit,
  ) {
    _listen(emit);
    // A host that was not attached announces itself before its first batch. Batches wait in
    // line behind the announcement, in order.
    final announced = _announced ??= _announce();
    _queue = _queue.then((_) async {
      await announced;
      final ready = _ready;
      if (ready != null && !_canPlay(ready)) {
        // The hub drops what comes before it is ready, as an announcement made at startup can.
        // A host that cannot play says so again, so the player fails the track at once rather
        // than waiting for a report. One that can is not repeated, since the player would hand
        // its track over again.
        _emit?.call(ready);
        return;
      }
      await _forward(command);
    });
  }

  static bool _canPlay(Map<String, dynamic> ready) =>
      ready['authorized'] == true && ready['canPlay'] == true;

  void _listen(void Function(Map<String, dynamic>) emit) {
    if (_emit == null) {
      _channel.setMethodCallHandler(_onCall);
    }
    _emit = emit;
  }

  /// Asks the runner what it can do and passes the answer on. Never throws. When the runner
  /// cannot be reached, Rust is told that nothing can be played here.
  Future<void> _announce() async {
    try {
      final reply = await _channel.invokeMethod<Object>('status');
      if (reply is String && _deliver(reply)) return;
      logger.w('Apple Music status was not usable');
    } on PlatformException catch (error) {
      logger.w('Apple Music status failed: ${error.code}');
    } on MissingPluginException {
      // The runner has no host.
    } catch (error) {
      logger.w('Apple Music status failed: ${error.runtimeType}');
    }
    _ready = _unavailable;
    _emit?.call(_unavailable);
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
    if (decoded['type'] == 'hostReady') _ready = decoded;
    _emit?.call(decoded);
    return true;
  }
}
