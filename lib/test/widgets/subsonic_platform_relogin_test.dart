import 'package:fluent_ui/fluent_ui.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:rune/bindings/bindings.dart';
import 'package:rune/l10n/app_localizations.dart';
import 'package:rune/providers/library_registry.dart';
import 'package:rune/utils/dialogs/subsonic/subsonic_relogin_dialog.dart';
import 'package:rune/utils/hosted_player/hosted_player.dart';

class _Host implements HostedPlayerHost {
  _Host(this.results);

  /// What each call answers, in order. The last one repeats.
  final List<HostAuthorization?> results;
  final calls = <bool>[];

  @override
  Future<HostAuthorization?> authorize({bool interactive = false}) async {
    calls.add(interactive);
    return results[(calls.length - 1).clamp(0, results.length - 1)];
  }

  @override
  void handle(
    Map<String, dynamic> command,
    void Function(Map<String, dynamic>) emit,
  ) {}
}

const _authorization = HostAuthorization(
  token: 'user-token',
  developerToken: 'dev-token',
);

/// Everything the registry was asked, in order, to check that a start comes before a complete.
final _requests = <String>[];

class _Completion {
  _Completion(this.method, this.token, this.developerToken);

  final LibraryLoginMethod method;
  final String? token;
  final String? developerToken;
}

LibraryRegistryProvider _registry({
  required List<LibraryLoginMethod> methods,
  List<String> hosts = const ['appleMusic'],
  LibraryLoginFailure? completionFailure,
  bool completionThrows = false,
  List<_Completion>? completions,
}) => LibraryRegistryProvider(
  loadRecords: () async => const ListLibraryRecordsResponse(
    records: [],
    invalidRecords: [],
    lastOpenRecord: null,
  ),
  loginMethods: (_) async => LibraryLoginMethodsResponse(
    methods: methods,
    hosts: hosts,
    serviceName: 'Apple Music',
    failure: null,
  ),
  startLogin: (_, method) async {
    _requests.add('start');
    return const StartLibraryLoginResponse(
      start: LibraryLoginStartPlatform(),
      failure: null,
    );
  },
  completeLogin: (_, method, token, developerToken) async {
    _requests.add('complete');
    if (completionThrows) throw StateError('offline');
    completions?.add(_Completion(method, token, developerToken));
    return CompleteLibraryLoginResponse(failure: completionFailure);
  },
);

Widget _app(
  void Function(bool) done,
  LibraryRegistryProvider registry,
  Map<String, HostedPlayerHost> hosts,
) => FluentApp(
  localizationsDelegates: AppLocalizations.localizationsDelegates,
  supportedLocales: AppLocalizations.supportedLocales,
  home: Builder(
    builder: (context) => FilledButton(
      onPressed: () async => done(
        await showSubsonicReloginDialog(
          context,
          registry,
          'record',
          hosts: hosts,
        ),
      ),
      child: const Text('Open'),
    ),
  ),
);

void main() {
  setUp(() {
    _requests.clear();
    resetSilentSignIns();
  });

  testWidgets('a host that authorizes silently signs in without a dialog', (
    tester,
  ) async {
    final completions = <_Completion>[];
    final registry = _registry(
      methods: [LibraryLoginMethod.platform],
      completions: completions,
    );
    addTearDown(registry.dispose);
    final host = _Host([_authorization]);
    bool? result;
    await tester.pumpWidget(
      _app((value) => result = value, registry, {'appleMusic': host}),
    );

    await tester.tap(find.text('Open'));
    await tester.pumpAndSettle();

    expect(result, isTrue);
    expect(host.calls, [false]);
    expect(find.text('Sign In Again'), findsNothing);
    expect(completions.single.method, LibraryLoginMethod.platform);
    expect(completions.single.token, 'user-token');
    expect(completions.single.developerToken, 'dev-token');
    expect(_requests, ['start', 'complete']);
  });

  testWidgets(
    'a silent failure asks, and signing in again authorizes interactively',
    (tester) async {
      final registry = _registry(methods: [LibraryLoginMethod.platform]);
      addTearDown(registry.dispose);
      final host = _Host([null, _authorization]);
      bool? result;
      await tester.pumpWidget(
        _app((value) => result = value, registry, {'appleMusic': host}),
      );

      await tester.tap(find.text('Open'));
      await tester.pumpAndSettle();
      expect(result, isNull);
      expect(find.text('Sign In Again'), findsWidgets);
      expect(
        find.textContaining('could not sign in to Apple Music automatically'),
        findsOneWidget,
      );
      await tester.tap(find.widgetWithText(FilledButton, 'Sign In Again'));
      await tester.pumpAndSettle();

      expect(result, isTrue);
      expect(host.calls, [false, true]);
    },
  );

  testWidgets(
    'a rejected completion or a missing host ends in the dialog, and cancel gives up',
    (tester) async {
      final completions = <_Completion>[];
      final rejected = _registry(
        methods: [LibraryLoginMethod.platform],
        completionFailure: LibraryLoginFailure.rejected,
        completions: completions,
      );
      addTearDown(rejected.dispose);
      bool? result;
      await tester.pumpWidget(
        _app((value) => result = value, rejected, {
          'appleMusic': _Host([_authorization]),
        }),
      );
      await tester.tap(find.text('Open'));
      await tester.pumpAndSettle();
      expect(completions, isNotEmpty);
      expect(find.text('Sign In Again'), findsWidgets);
      await tester.tap(find.text('Cancel'));
      await tester.pumpAndSettle();
      expect(result, isFalse);
    },
  );

  testWidgets('browser-only services say the method is not supported', (
    tester,
  ) async {
    final registry = _registry(methods: [LibraryLoginMethod.browser]);
    addTearDown(registry.dispose);
    bool? result;
    await tester.pumpWidget(_app((value) => result = value, registry, {}));

    await tester.tap(find.text('Open'));
    await tester.pumpAndSettle();
    expect(find.textContaining('does not support'), findsOneWidget);
    await tester.tap(find.text('Close'));
    await tester.pumpAndSettle();

    expect(result, isFalse);
  });

  testWidgets('a password-only library keeps the password dialog', (
    tester,
  ) async {
    final registry = _registry(methods: [LibraryLoginMethod.password]);
    addTearDown(registry.dispose);
    await tester.pumpWidget(_app((_) {}, registry, {}));

    await tester.tap(find.text('Open'));
    await tester.pumpAndSettle();

    expect(find.text('Retry Login'), findsOneWidget);
    expect(find.text('Sign In Again'), findsNothing);
  });

  testWidgets('a password-only offer never reaches a platform host', (
    tester,
  ) async {
    final registry = _registry(methods: [LibraryLoginMethod.password]);
    addTearDown(registry.dispose);
    final host = _Host([_authorization]);
    await tester.pumpWidget(_app((_) {}, registry, {'appleMusic': host}));

    await tester.tap(find.text('Open'));
    await tester.pumpAndSettle();

    expect(host.calls, isEmpty);
    expect(_requests, isEmpty);
    expect(find.text('Retry Login'), findsOneWidget);
  });

  testWidgets('without a host the password dialog is used if offered', (
    tester,
  ) async {
    final registry = _registry(
      methods: [LibraryLoginMethod.platform, LibraryLoginMethod.password],
    );
    addTearDown(registry.dispose);
    await tester.pumpWidget(_app((_) {}, registry, {}));

    await tester.tap(find.text('Open'));
    await tester.pumpAndSettle();

    expect(find.text('Retry Login'), findsOneWidget);
    expect(find.text('Sign In Again'), findsNothing);
    expect(_requests, isEmpty);
  });

  testWidgets(
    'without a host and without a password the method is unsupported',
    (tester) async {
      final registry = _registry(methods: [LibraryLoginMethod.platform]);
      addTearDown(registry.dispose);
      bool? result;
      await tester.pumpWidget(_app((value) => result = value, registry, {}));

      await tester.tap(find.text('Open'));
      await tester.pumpAndSettle();
      expect(find.textContaining('does not support'), findsOneWidget);
      expect(find.text('Sign In Again'), findsNothing);
      await tester.tap(find.text('Close'));
      await tester.pumpAndSettle();

      expect(result, isFalse);
      expect(_requests, isEmpty);
    },
  );

  testWidgets('a failing completion shows an error after the interactive try', (
    tester,
  ) async {
    final registry = _registry(
      methods: [LibraryLoginMethod.platform],
      completionThrows: true,
    );
    addTearDown(registry.dispose);
    final host = _Host([_authorization]);
    bool? result;
    await tester.pumpWidget(
      _app((value) => result = value, registry, {'appleMusic': host}),
    );

    await tester.tap(find.text('Open'));
    await tester.pumpAndSettle();
    expect(find.text('Sign In Again'), findsWidgets);
    await tester.tap(find.widgetWithText(FilledButton, 'Sign In Again'));
    await tester.pumpAndSettle();

    // The same dialog is not shown again: the error is.
    expect(host.calls, [false, true]);
    expect(find.text('Login Failed'), findsOneWidget);
    expect(find.text('Sign In Again'), findsNothing);
    await tester.tap(find.text('Close'));
    await tester.pumpAndSettle();
    expect(result, isFalse);
  });

  testWidgets('cancel after a failing silent try stops without another try', (
    tester,
  ) async {
    final registry = _registry(
      methods: [LibraryLoginMethod.platform],
      completionThrows: true,
    );
    addTearDown(registry.dispose);
    final host = _Host([_authorization]);
    bool? result;
    await tester.pumpWidget(
      _app((value) => result = value, registry, {'appleMusic': host}),
    );

    await tester.tap(find.text('Open'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Cancel'));
    await tester.pumpAndSettle();

    expect(result, isFalse);
    expect(host.calls, [false]);
  });

  testWidgets('a second failure right after a silent success asks the user', (
    tester,
  ) async {
    final registry = _registry(methods: [LibraryLoginMethod.platform]);
    addTearDown(registry.dispose);
    final host = _Host([_authorization]);
    final results = <bool>[];
    await tester.pumpWidget(_app(results.add, registry, {'appleMusic': host}));

    await tester.tap(find.text('Open'));
    await tester.pumpAndSettle();
    expect(results, [true]);

    await tester.tap(find.text('Open'));
    await tester.pumpAndSettle();
    // No second silent try: the dialog is up and the host was asked once.
    expect(host.calls, [false]);
    expect(find.text('Sign In Again'), findsWidgets);
    await tester.tap(find.text('Cancel'));
    await tester.pumpAndSettle();
    expect(results, [true, false]);
  });
}
