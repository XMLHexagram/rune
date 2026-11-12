import 'dart:async';
import 'dart:convert';

import 'package:flutter_test/flutter_test.dart';
import 'package:rune/utils/hosted_player/hosted_player.dart';

class _RecordingHost implements HostedPlayerHost {
  final commands = <Map<String, dynamic>>[];

  @override
  void handle(
    Map<String, dynamic> command,
    void Function(Map<String, dynamic>) emit,
  ) {
    commands.add(command);
    emit({
      'type': 'hostReady',
      'host': 'other',
      'authorized': true,
      'canPlay': true,
    });
  }
}

void main() {
  late StreamController<String> commands;
  late List<String> sent;

  setUp(() {
    commands = StreamController<String>();
    sent = [];
  });

  tearDown(() => commands.close());

  String command(String host, String action) =>
      jsonEncode({'host': host, 'revision': 1, 'action': action});

  test(
    'a host without an implementation answers once that it cannot play',
    () async {
      final bridge = HostedPlayerBridge(
        commands: commands.stream,
        send: sent.add,
      );
      commands.add(command('appleMusic', 'set'));
      commands.add(command('appleMusic', 'start'));
      commands.add(command('other', 'set'));
      await pumpEventQueue();

      expect(sent.map(jsonDecode), [
        {
          'type': 'hostReady',
          'host': 'appleMusic',
          'authorized': true,
          'canPlay': false,
        },
        {
          'type': 'hostReady',
          'host': 'other',
          'authorized': true,
          'canPlay': false,
        },
      ]);
      bridge.dispose();
    },
  );

  test('a registered host gets the command and speaks for itself', () async {
    final host = _RecordingHost();
    final bridge = HostedPlayerBridge(
      commands: commands.stream,
      send: sent.add,
      hosts: {'other': host},
    );
    commands.add(command('other', 'set'));
    commands.add('not json');
    await pumpEventQueue();

    expect(host.commands.single['action'], 'set');
    expect(sent.map(jsonDecode), [
      {
        'type': 'hostReady',
        'host': 'other',
        'authorized': true,
        'canPlay': true,
      },
    ]);
    bridge.dispose();
  });
}
