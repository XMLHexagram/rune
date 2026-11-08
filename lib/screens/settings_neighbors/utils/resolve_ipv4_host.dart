import 'dart:io';

import '../../../utils/discovery_url.dart';

typedef HostLookup =
    Future<List<InternetAddress>> Function(
      String host, {
      InternetAddressType type,
    });

/// Turns what the user typed into an IPv4 address: a literal is used as is,
/// a name is looked up. Returns null when no IPv4 address can be found.
Future<String?> resolveIpv4Host(String input, {HostLookup? lookup}) async {
  final literal = parseIpv4Host(input);
  if (literal != null) return literal;

  final host = input.trim();
  if (host.isEmpty) return null;

  try {
    final addresses = await (lookup ?? InternetAddress.lookup)(
      host,
      type: InternetAddressType.IPv4,
    );
    for (final address in addresses) {
      if (address.type == InternetAddressType.IPv4) return address.address;
    }
  } catch (_) {}
  return null;
}
