import 'dart:async';
import 'dart:convert';

import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:rune/utils/hosted_player/apple_music_host.dart';

final _ready = jsonEncode({
  'type': 'hostReady',
  'host': 'appleMusic',
  'authorized': true,
  'canPlay': true,
});

const _unavailable = {
  'type': 'hostReady',
  'host': 'appleMusic',
  'authorized': true,
  'canPlay': false,
};

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  late MethodChannel channel;
  late AppleMusicHost host;
  late List<MethodCall> calls;
  late List<Map<String, dynamic>> events;
  Object? Function(MethodCall)? answer;
  Future<Object?> Function()? statusAnswer;

  setUp(() {
    channel = const MethodChannel(AppleMusicHost.channelName);
    host = AppleMusicHost(channel: channel);
    calls = [];
    events = [];
    answer = null;
    statusAnswer = null;
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(channel, (call) async {
          calls.add(call);
          if (call.method == 'status') {
            return statusAnswer != null ? statusAnswer!() : _ready;
          }
          return answer?.call(call);
        });
  });

  tearDown(() {
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(channel, null);
  });

  Future<void> fromRunner(Object? payload) => TestDefaultBinaryMessengerBinding
      .instance
      .defaultBinaryMessenger
      .handlePlatformMessage(
        channel.name,
        channel.codec.encodeMethodCall(MethodCall('event', payload)),
        (_) {},
      );

  Map<String, dynamic> batch(int revision) => {
    'host': 'appleMusic',
    'revision': revision,
    'actions': [
      {'action': 'start'},
    ],
  };

  test('announces the host, then forwards a batch as it is', () async {
    final batch = {
      'host': 'appleMusic',
      'revision': 12,
      'actions': [
        {
          'action': 'set',
          'ids': ['i.abc'],
        },
        {'action': 'start'},
      ],
    };
    host.handle(batch, events.add);
    await pumpEventQueue();

    expect(calls.map((call) => call.method), ['status', 'batch']);
    expect(jsonDecode(calls.last.arguments as String), batch);
    expect(events.single['type'], 'hostReady');
    expect(events.single['canPlay'], true);

    host.handle(batch, events.add);
    await pumpEventQueue();
    expect(calls.map((call) => call.method), ['status', 'batch', 'batch']);
  });

  test('attaching announces the host once, before any batch', () async {
    host.attach(events.add);
    await pumpEventQueue();
    expect(calls.map((call) => call.method), ['status']);
    expect(events.single['type'], 'hostReady');

    final batch = {
      'host': 'appleMusic',
      'revision': 1,
      'actions': [
        {'action': 'start'},
      ],
    };
    host.handle(batch, events.add);
    await pumpEventQueue();
    expect(calls.map((call) => call.method), ['status', 'batch']);
    expect(jsonDecode(calls.last.arguments as String), batch);
    expect(events, hasLength(1));
  });

  test('batches wait for the announcement made on attach', () async {
    final reply = Completer<Object?>();
    statusAnswer = () => reply.future;
    host.attach(events.add);
    host.handle({
      'host': 'appleMusic',
      'revision': 1,
      'actions': [],
    }, events.add);
    await pumpEventQueue();
    expect(calls.map((call) => call.method), ['status']);

    reply.complete(_ready);
    await pumpEventQueue();
    expect(calls.map((call) => call.method), ['status', 'batch']);
    expect(events.map((event) => event['type']), ['hostReady']);
  });

  test('events from the runner reach the bridge once attached', () async {
    host.attach(events.add);
    await pumpEventQueue();
    events.clear();

    const status = {
      'type': 'jukeboxStatus',
      'host': 'appleMusic',
      'revision': 1,
      'currentIndex': 0,
      'playing': true,
      'positionMs': 900,
    };
    const ready = {
      'type': 'hostReady',
      'host': 'appleMusic',
      'authorized': false,
      'canPlay': false,
    };
    await fromRunner(jsonEncode(status));
    await fromRunner(jsonEncode(ready));
    expect(events, [status, ready]);
  });

  test('authorize passes interactive and parses tokens', () async {
    answer = (call) => {
      'authorized': true,
      'token': 'user',
      'developerToken': 'dev',
    };
    final authorization = await host.authorize(interactive: true);
    expect(calls.single.arguments, {'interactive': true});
    expect(authorization?.token, 'user');
    expect(authorization?.developerToken, 'dev');
  });

  test('authorize without tokens or authorization is null', () {
    expect(AppleMusicHost.parseAuthorization({'authorized': false}), isNull);
    expect(AppleMusicHost.parseAuthorization({'authorized': true}), isNull);
    expect(AppleMusicHost.parseAuthorization(null), isNull);
    final noDeveloper = AppleMusicHost.parseAuthorization({
      'authorized': true,
      'token': 'user',
    });
    expect(noDeveloper?.token, 'user');
    expect(noDeveloper?.developerToken, isNull);
  });

  test('batches sent before the status reply wait for it, in order', () async {
    final reply = Completer<Object?>();
    statusAnswer = () => reply.future;
    host.handle(batch(1), events.add);
    host.handle(batch(2), events.add);
    await pumpEventQueue();
    expect(calls.map((call) => call.method), ['status']);
    expect(events, isEmpty);

    reply.complete(_ready);
    await pumpEventQueue();
    expect(events.map((event) => event['type']), ['hostReady']);
    expect(calls.map((call) => call.method), ['status', 'batch', 'batch']);
    expect(calls.skip(1).map((call) => jsonDecode(call.arguments as String)), [
      batch(1),
      batch(2),
    ]);
  });

  final failures = <String, Future<Object?> Function()>{
    'PlatformException': () async => throw PlatformException(code: 'x'),
    'MissingPluginException': () async => throw MissingPluginException(),
    'another error': () async => throw StateError('boom'),
    'null': () async => null,
    'a reply that is not text': () async => 42,
    'text that is not an object': () async => '[1]',
    'text that does not parse': () async => '{',
  };
  for (final entry in failures.entries) {
    test('status failing with ${entry.key} settles on unavailable', () async {
      statusAnswer = entry.value;
      host.handle(batch(1), events.add);
      host.handle(batch(2), events.add);
      await pumpEventQueue();
      // The announcement, then once more for each batch, which goes no further.
      expect(events, [_unavailable, _unavailable, _unavailable]);
      expect(calls.map((call) => call.method), ['status']);
    });
  }

  test(
    'a batch after a failed announcement on attach hears it again',
    () async {
      statusAnswer = () async => throw MissingPluginException();
      host.attach(events.add);
      await pumpEventQueue();
      expect(events, [_unavailable]);

      host.handle(batch(1), events.add);
      await pumpEventQueue();
      expect(events, [_unavailable, _unavailable]);
      expect(calls.map((call) => call.method), ['status']);
    },
  );

  test('a host that cannot play holds batches until it says it can', () async {
    final revoked = {
      'type': 'hostReady',
      'host': 'appleMusic',
      'authorized': false,
      'canPlay': false,
    };
    statusAnswer = () async => jsonEncode(revoked);
    host.attach(events.add);
    host.handle(batch(1), events.add);
    await pumpEventQueue();
    expect(events, [revoked, revoked]);
    expect(calls.map((call) => call.method), ['status']);

    // Access granted later; a positive answer is never repeated.
    await fromRunner(_ready);
    host.handle(batch(2), events.add);
    await pumpEventQueue();
    expect(events, [revoked, revoked, jsonDecode(_ready)]);
    expect(calls.map((call) => call.method), ['status', 'batch']);
    expect(jsonDecode(calls.last.arguments as String), batch(2));
  });

  test('authorize failing is null', () async {
    answer = (call) => throw PlatformException(code: 'denied');
    expect(await host.authorize(interactive: true), isNull);
    answer = (call) => throw StateError('boom');
    expect(await host.authorize(), isNull);
  });

  test('events that do not parse are ignored', () async {
    host.handle({
      'host': 'appleMusic',
      'revision': 1,
      'actions': [],
    }, events.add);
    await pumpEventQueue();
    events.clear();
    for (final payload in <Object?>['{', '[1]', 7, null]) {
      await fromRunner(payload);
    }
    expect(events, isEmpty);
  });
}
