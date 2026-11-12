import 'package:fluent_ui/fluent_ui.dart';

import '../../../bindings/bindings.dart';
import '../../l10n.dart';
import '../../router/navigation.dart';
import '../information/error.dart';
import '../scrobble/utils/scrobble_login_controller.dart';
import '../scrobble/widgets/scrobble_login_form.dart';
import '../../../providers/library_registry.dart';
import '../../hosted_player/hosted_player.dart';
import '../../logger.dart';

/// Signs in again to a library whose stored sign-in stopped working, the way its server asks.
/// Servers that only know passwords, or that cannot be asked, get the password dialog.
Future<bool> showSubsonicReloginDialog(
  BuildContext context,
  LibraryRegistryProvider registry,
  String recordId, {
  Map<String, HostedPlayerHost>? hosts,
}) async {
  LibraryLoginMethodsResponse? methods;
  try {
    methods = await registry.libraryLoginMethods(recordId);
  } catch (_) {
    // Fall back to the password dialog, as before.
  }
  if (!context.mounted) return false;
  if (methods == null || methods.failure != null) {
    return _showPasswordReloginDialog(context, registry, recordId);
  }
  if (methods.methods.contains(LibraryLoginMethod.platform)) {
    return _platformRelogin(
      context,
      registry,
      recordId,
      methods,
      hosts ?? platformHosts,
    );
  }
  if (methods.methods.isEmpty ||
      methods.methods.contains(LibraryLoginMethod.password)) {
    return _showPasswordReloginDialog(context, registry, recordId);
  }
  return _showUnsupportedMethod(context);
}

/// When each record last signed in silently. A silent sign-in that works but is followed by
/// another authorization failure within [_silentCooldown] would otherwise repeat forever.
final Map<String, DateTime> _lastSilentSignIn = {};
const _silentCooldown = Duration(seconds: 60);

@visibleForTesting
void resetSilentSignIns() => _lastSilentSignIn.clear();

Future<bool> _platformRelogin(
  BuildContext context,
  LibraryRegistryProvider registry,
  String recordId,
  LibraryLoginMethodsResponse methods,
  Map<String, HostedPlayerHost> hosts,
) async {
  final host = methods.hosts
      .map((family) => hosts[family])
      .where((host) => host != null)
      .firstOrNull;
  if (host == null) {
    // This device has no host for the service, so there is nothing to authorize with.
    if (methods.methods.contains(LibraryLoginMethod.password)) {
      return _showPasswordReloginDialog(context, registry, recordId);
    }
    return _showUnsupportedMethod(context);
  }

  Future<bool> attempt({required bool interactive}) async {
    final start = await registry.startLibraryLogin(
      recordId,
      LibraryLoginMethod.platform,
    );
    if (start.failure != null) {
      throw StateError('Starting the sign-in failed: ${start.failure}');
    }
    final authorization = await host.authorize(interactive: interactive);
    if (authorization == null) return false;
    final response = await registry.completeLibraryLogin(
      recordId,
      LibraryLoginMethod.platform,
      token: authorization.token,
      developerToken: authorization.developerToken,
    );
    if (response.failure != null) {
      logger.w('The service refused the sign-in: ${response.failure}');
      return false;
    }
    return true;
  }

  final lastSilent = _lastSilentSignIn[recordId];
  final silentAllowed =
      lastSilent == null ||
      DateTime.now().difference(lastSilent) >= _silentCooldown;
  if (silentAllowed) {
    try {
      if (await attempt(interactive: false)) {
        _lastSilentSignIn[recordId] = DateTime.now();
        return true;
      }
    } catch (error) {
      logger.w('Silent sign-in failed: $error');
    }
  }
  if (!context.mounted) return false;
  final again =
      await $showModal<bool>(
        context,
        (context, close) => ContentDialog(
          title: Text(S.of(context).signInAgain),
          content: Text(
            S
                .of(context)
                .signInAgainSubtitle(methods.serviceName ?? 'Subsonic'),
          ),
          actions: [
            FilledButton(
              onPressed: () => close(true),
              child: Text(S.of(context).signInAgain),
            ),
            Button(
              onPressed: () => close(false),
              child: Text(S.of(context).cancel),
            ),
          ],
        ),
        barrierDismissible: true,
        dismissWithEsc: true,
      ) ??
      false;
  if (!again || !context.mounted) return false;
  try {
    if (await attempt(interactive: true)) return true;
  } catch (error) {
    logger.w('Interactive sign-in failed: $error');
  }
  if (!context.mounted) return false;
  final s = S.of(context);
  await showErrorDialog(
    context: context,
    title: s.loginFailed,
    subtitle: s.loginFailedSubtitle,
    errorMessage: s.loginFailedSubtitle,
  );
  return false;
}

Future<bool> _showUnsupportedMethod(BuildContext context) async {
  final s = S.of(context);
  await showErrorDialog(
    context: context,
    title: s.loginFailed,
    errorMessage: s.signInMethodUnsupported,
  );
  return false;
}

Future<bool> _showPasswordReloginDialog(
  BuildContext context,
  LibraryRegistryProvider registry,
  String recordId,
) async {
  final controller = ScrobbleLoginController();
  try {
    return await $showModal<bool>(
          context,
          (context, close) => _SubsonicReloginDialog(
            controller: controller,
            registry: registry,
            recordId: recordId,
            close: close,
          ),
          barrierDismissible: true,
          dismissWithEsc: true,
        ) ??
        false;
  } finally {
    controller.dispose();
  }
}

class _SubsonicReloginDialog extends StatefulWidget {
  const _SubsonicReloginDialog({
    required this.controller,
    required this.registry,
    required this.recordId,
    required this.close,
  });

  final ScrobbleLoginController controller;
  final LibraryRegistryProvider registry;
  final String recordId;
  final void Function(bool?) close;

  @override
  State<_SubsonicReloginDialog> createState() => _SubsonicReloginDialogState();
}

class _SubsonicReloginDialogState extends State<_SubsonicReloginDialog> {
  bool _loading = false;

  Future<void> _login() async {
    setState(() => _loading = true);
    try {
      final result = await widget.registry.replaceSubsonicCredential(
        widget.recordId,
        widget.controller.usernameController.text,
        widget.controller.passwordController.text,
      );
      if (!mounted) return;
      if (result.result == ReplaceSubsonicCredentialResult.updated) {
        widget.close(true);
      } else {
        final s = S.of(context);
        await showErrorDialog(
          context: context,
          title: s.loginFailed,
          subtitle: s.loginFailedSubtitle,
          errorMessage: s.loginFailedSubtitle,
        );
      }
    } catch (_) {
      if (!mounted) return;
      final s = S.of(context);
      await showErrorDialog(
        context: context,
        title: s.loginFailed,
        subtitle: s.loginFailedSubtitle,
        errorMessage: s.loginFailedSubtitle,
      );
    } finally {
      if (mounted) setState(() => _loading = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final s = S.of(context);
    return ContentDialog(
      title: Text(s.retryLogin),
      content: ScrobbleLoginForm(
        serviceName: 'Subsonic',
        controller: widget.controller,
      ),
      actions: [
        FilledButton(onPressed: _loading ? null : _login, child: Text(s.login)),
        Button(
          onPressed: _loading ? null : () => widget.close(false),
          child: Text(s.cancel),
        ),
      ],
    );
  }
}

Future<void> openLibraryWithSubsonicRelogin(
  BuildContext context,
  LibraryRegistryProvider registry,
  String recordId, {
  Map<String, HostedPlayerHost>? hosts,
}) async {
  try {
    await registry.open(recordId);
  } on LibraryRegistryFailure catch (error) {
    if (error.kind != OpenLibraryFailureKind.remoteNotAuthorized ||
        !context.mounted) {
      rethrow;
    }
    final updated = await showSubsonicReloginDialog(
      context,
      registry,
      recordId,
      hosts: hosts,
    );
    if (!updated) rethrow;
    if (!context.mounted) return;
    await registry.open(recordId);
  }
}
