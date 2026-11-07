import 'package:fluent_ui/fluent_ui.dart';
import 'package:provider/provider.dart';

import '../../../providers/library_path.dart';
import '../../../utils/api/close_library.dart';
import '../../../utils/dialogs/failed_to_initialize_library.dart';
import '../../../utils/dialogs/information/error.dart';
import '../../../utils/dialogs/information/information.dart';
import '../../../utils/l10n.dart';
import '../../../utils/remote_connection_messages.dart';
import '../../../utils/router/navigation.dart';
import '../../../widgets/no_shortcuts.dart';

import '../utils/add_remote_device_form_controller.dart';
import '../utils/pair_remote_device.dart';
import '../utils/resolve_ipv4_host.dart';
import '../utils/show_fingerprint_quiz_dialog.dart';

import 'add_remote_device_form.dart';

class AddRemoteDeviceDialog extends StatefulWidget {
  final bool navigateIfFailed;
  final void Function(void) $close;

  const AddRemoteDeviceDialog({
    super.key,
    required this.navigateIfFailed,
    required this.$close,
  });

  @override
  AddRemoteDeviceDialogState createState() => AddRemoteDeviceDialogState();
}

class AddRemoteDeviceDialogState extends State<AddRemoteDeviceDialog> {
  late AddRemoteDeviceFormController controller;

  @override
  void initState() {
    super.initState();
    controller = AddRemoteDeviceFormController();
  }

  @override
  void dispose() {
    controller.dispose();
    super.dispose();
  }

  // Pairing waits on a lookup, two dialogs and the other device, so a second
  // press must not start another round.
  bool _busy = false;

  Future<void> _addConnection() async {
    if (_busy) return;
    setState(() => _busy = true);
    try {
      await _connect();
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  Future<void> _connect() async {
    if (!(controller.formKey.currentState?.validate() ?? false)) return;

    final s = S.of(context);
    final typed = controller.hostnameController.text.trim();
    final libraryPath = Provider.of<LibraryPathProvider>(
      context,
      listen: false,
    );

    final ip = await resolveIpv4Host(typed);
    if (!mounted) return;

    if (ip == null) {
      await showErrorDialog(
        context: context,
        title: s.unknownError,
        errorMessage: s.hostLookupFailed,
      );
      return;
    }

    final quiz = await showFingerprintQuizDialog(context, ip);
    if (quiz == null) return;

    if (!quiz.correct) {
      if (!mounted) return;
      showInformationDialog(
        context: context,
        title: s.pairingFailureTitle,
        subtitle: s.pairingFailureMessage,
      );
      return;
    }

    if (!mounted) return;
    final path = await pairRemoteDevice(
      context,
      fingerprint: quiz.fingerprint,
      hosts: [ip],
      alias: typed,
    );
    if (path == null) return;
    if (!mounted) return;

    await closeLibrary(context);

    if (!mounted) return;

    final (switched, cancelled, _, errorKind) = await libraryPath
        .setLibraryPath(context, path, null);

    widget.$close(null);
    if (!switched && !cancelled) {
      if (!context.mounted) return;
      await showFailedToInitializeLibrary(
        context,
        remoteOpenErrorMessage(S.of(context), errorKind),
      );
      if (widget.navigateIfFailed) {
        $$replace('/');
      }
    }
  }

  @override
  Widget build(BuildContext context) {
    final s = S.of(context);

    return NoShortcuts(
      ContentDialog(
        title: Column(children: [SizedBox(height: 8), Text(s.addConnection)]),
        content: AddRemoteDeviceForm(
          controller: controller,
          onSubmit: _addConnection,
        ),
        actions: [
          FilledButton(
            onPressed: _busy ? null : _addConnection,
            child: Text(s.addConnection),
          ),
          Button(onPressed: () => widget.$close(null), child: Text(s.cancel)),
        ],
      ),
    );
  }
}
