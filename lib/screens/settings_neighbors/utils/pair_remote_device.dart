import 'package:fluent_ui/fluent_ui.dart';
import 'package:provider/provider.dart';

import '../../../bindings/bindings.dart';
import '../../../providers/library_path.dart';
import '../../../utils/api/add_trusted_server.dart';
import '../../../utils/api/register_device_on_server.dart';
import '../../../utils/api/request_failure.dart';
import '../../../utils/dialogs/information/error.dart';
import '../../../utils/discovery_url.dart';
import '../../../utils/l10n.dart';

import 'show_server_status_dialog.dart';

/// Trusts the server, registers this device on it, checks that it lets us in
/// and stores it in the library history. Returns the stored library path, or
/// null when a step failed or was dismissed.
Future<String?> pairRemoteDevice(
  BuildContext context, {
  required String fingerprint,
  required List<String> hosts,
  required String alias,
}) async {
  final s = S.of(context);
  final libraryPath = Provider.of<LibraryPathProvider>(context, listen: false);

  try {
    await addTrustedServer(fingerprint, hosts);
  } catch (e) {
    if (!context.mounted) return null;

    showErrorDialog(
      context: context,
      title: s.unknownError,
      errorMessage: requestErrorText(S.of(context), e),
    );
    // Do not register with or open a device we failed to trust.
    return null;
  }

  try {
    await registerDeviceOnServer(hosts, fingerprint);
  } catch (e) {
    if (!context.mounted) return null;

    showErrorDialog(
      context: context,
      title: s.unknownError,
      errorMessage: requestErrorText(S.of(context), e),
    );

    return null;
  }

  if (!context.mounted) return null;
  final serverStatus = await showServerStatusDialog(
    context,
    hosts,
    fingerprint,
  );

  if (serverStatus == null) {
    return null;
  }

  if (!context.mounted) return null;
  if (serverStatus.status == ClientStatus.blocked) {
    await showErrorDialog(
      context: context,
      title: s.clientBlockedTitle,
      errorMessage: s.clientBlockedMessage,
    );

    return null;
  }

  if (!context.mounted) return null;
  final path = '@RR|${encodeRnSrvUrl(hosts)}';
  await libraryPath.addLibraryPath(context, path, alias);
  return path;
}
