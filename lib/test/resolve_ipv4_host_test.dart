import 'dart:io';

import 'package:flutter_test/flutter_test.dart';

import 'package:rune/screens/settings_neighbors/utils/resolve_ipv4_host.dart';

void main() {
  test('an IPv4 literal skips the lookup', () async {
    var called = false;
    final ip = await resolveIpv4Host(
      ' 192.168.1.20 ',
      lookup: (host, {type = InternetAddressType.any}) async {
        called = true;
        return [];
      },
    );
    expect(ip, '192.168.1.20');
    expect(called, isFalse);
  });

  test('a hostname resolves to the first IPv4 address', () async {
    String? asked;
    final ip = await resolveIpv4Host(
      'my-mac.local',
      lookup: (host, {type = InternetAddressType.any}) async {
        asked = host;
        expect(type, InternetAddressType.IPv4);
        return [InternetAddress('10.0.0.7'), InternetAddress('10.0.0.8')];
      },
    );
    expect(asked, 'my-mac.local');
    expect(ip, '10.0.0.7');
  });

  test('no addresses gives null', () async {
    final ip = await resolveIpv4Host(
      'nothing.local',
      lookup: (host, {type = InternetAddressType.any}) async => [],
    );
    expect(ip, isNull);
  });

  test('a failing lookup gives null', () async {
    final ip = await resolveIpv4Host(
      'broken.local',
      lookup: (host, {type = InternetAddressType.any}) async =>
          throw const SocketException('nope'),
    );
    expect(ip, isNull);
  });
}
