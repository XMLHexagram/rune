import 'package:flutter_test/flutter_test.dart';

import 'package:rune/utils/discovery_url.dart';

void main() {
  test('round-trips one and several IPv4 addresses', () {
    for (final ips in [
      ['192.168.1.5'],
      ['10.0.0.1', '255.255.255.255', '0.0.0.0'],
    ]) {
      expect(decodeRnSrvUrl(encodeRnSrvUrl(ips)), ips);
    }
  });

  test('rejects a URL with the wrong scheme', () {
    expect(() => decodeRnSrvUrl('rnsrv:/abc'), throwsFormatException);
  });
  group('parseIpv4Host', () {
    test('accepts dotted IPv4 literals and trims spaces', () {
      expect(parseIpv4Host('192.168.1.20'), '192.168.1.20');
      expect(parseIpv4Host('  10.0.0.1 '), '10.0.0.1');
      expect(parseIpv4Host('0.0.0.0'), '0.0.0.0');
      expect(parseIpv4Host('255.255.255.255'), '255.255.255.255');
    });

    test('rejects out-of-range octets', () {
      expect(parseIpv4Host('256.1.1.1'), isNull);
      expect(parseIpv4Host('1.1.1.300'), isNull);
    });

    test('rejects leading zeros', () {
      expect(parseIpv4Host('192.168.01.1'), isNull);
      expect(parseIpv4Host('010.0.0.1'), isNull);
    });

    test('rejects hostnames and malformed input', () {
      expect(parseIpv4Host('my-mac.local'), isNull);
      expect(parseIpv4Host('1.2.3'), isNull);
      expect(parseIpv4Host('1.2.3.4.5'), isNull);
      expect(parseIpv4Host('1.2.3.-4'), isNull);
      expect(parseIpv4Host(''), isNull);
    });
  });

  test('encoder rejects an invalid address', () {
    expect(() => encodeRnSrvUrl(['1.2.3.01']), throwsFormatException);
  });
}
