import 'package:fluent_ui/fluent_ui.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:rune/bindings/bindings.dart';
import 'package:rune/l10n/app_localizations.dart';
import 'package:rune/providers/library_registry.dart';
import 'package:rune/utils/dialogs/subsonic/subsonic_relogin_dialog.dart';

LibraryRecordSummary _record() => const LibraryRecordSummary(
  recordId: 'subsonic-record',
  alias: 'Subsonic',
  kind: LibraryRecordKindSummarySubsonic(
    endpoint: 'https://music.example',
    account: '',
  ),
);

OpenLibraryResponse _openResponse(int callId, {required bool authorized}) =>
    OpenLibraryResponse(
      callId: Uint64.fromBigInt(BigInt.from(callId)),
      recordId: 'subsonic-record',
      generation: Uint64.fromBigInt(BigInt.one),
      result: authorized
          ? const OpenLibraryResultOpened()
          : const OpenLibraryResultFailed(
              value: OpenLibraryFailure(
                kind: OpenLibraryFailureKind.remoteNotAuthorized,
                detail: 'The Subsonic credentials were rejected',
              ),
            ),
    );

LibraryRegistryProvider _registry({required bool succeed}) {
  var attempts = 0;
  return LibraryRegistryProvider(
    loadRecords: () async => ListLibraryRecordsResponse(
      records: [_record()],
      invalidRecords: const [],
      lastOpenRecord: null,
    ),
    openRecord: (_, callId) async => _openResponse(
      callId,
      authorized: attempts++ > 0,
    ),
    loginMethods: (_) async => const LibraryLoginMethodsResponse(
      methods: [LibraryLoginMethod.password],
      hosts: [],
      serviceName: null,
      failure: null,
    ),
    replaceCredential: (_, _, _) async => ReplaceSubsonicCredentialResponse(
      result: succeed
          ? ReplaceSubsonicCredentialResult.updated
          : ReplaceSubsonicCredentialResult.rejected,
    ),
  );
}

Widget _app(Widget child) => FluentApp(
  localizationsDelegates: AppLocalizations.localizationsDelegates,
  supportedLocales: AppLocalizations.supportedLocales,
  home: child,
);

void main() {
  testWidgets('authorization failure prompts and successful sign-in reopens', (
    tester,
  ) async {
    final registry = _registry(succeed: true);
    addTearDown(registry.dispose);
    await registry.ready;
    await tester.pumpWidget(
      _app(
        Builder(
          builder: (context) => FilledButton(
            onPressed: () => openLibraryWithSubsonicRelogin(
              context,
              registry,
              'subsonic-record',
            ).catchError((_) {}),
            child: const Text('Open'),
          ),
        ),
      ),
    );

    await tester.tap(find.text('Open'));
    await tester.pumpAndSettle();
    expect(find.text('Retry Login'), findsOneWidget);
    expect(find.text('Username'), findsOneWidget);
    expect(find.text('Password'), findsOneWidget);
    await tester.enterText(find.byType(TextBox).first, 'alice');
    await tester.enterText(find.byType(TextBox).last, 'new-password');
    await tester.tap(find.text('Login'));
    await tester.pumpAndSettle();

    expect(registry.openRecordId, 'subsonic-record');
  });

  testWidgets('cancel leaves the library closed', (tester) async {
    final registry = _registry(succeed: true);
    addTearDown(registry.dispose);
    await registry.ready;
    await tester.pumpWidget(
      _app(
        Builder(
          builder: (context) => FilledButton(
            onPressed: () => openLibraryWithSubsonicRelogin(
              context,
              registry,
              'subsonic-record',
            ).catchError((_) {}),
            child: const Text('Open'),
          ),
        ),
      ),
    );
    await tester.tap(find.text('Open'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Cancel'));
    await tester.pumpAndSettle();

    expect(registry.openRecordId, isNull);
  });

  group('when the methods request fails', () {
    Future<Object? Function()> openAndFail(
      WidgetTester tester, {
      required String endpoint,
      required Future<LibraryLoginMethodsResponse> Function(String) methods,
    }) async {
      final registry = LibraryRegistryProvider(
        loadRecords: () async => ListLibraryRecordsResponse(
          records: [
            LibraryRecordSummary(
              recordId: 'subsonic-record',
              alias: 'Library',
              kind: LibraryRecordKindSummarySubsonic(
                endpoint: endpoint,
                account: '',
              ),
            ),
          ],
          invalidRecords: const [],
          lastOpenRecord: null,
        ),
        openRecord: (_, callId) async =>
            _openResponse(callId, authorized: false),
        loginMethods: methods,
      );
      addTearDown(registry.dispose);
      await registry.ready;
      Object? thrown;
      await tester.pumpWidget(
        _app(
          Builder(
            builder: (context) => FilledButton(
              onPressed: () async {
                try {
                  await openLibraryWithSubsonicRelogin(
                    context,
                    registry,
                    'subsonic-record',
                  );
                } catch (error) {
                  thrown = error;
                }
              },
              child: const Text('Open'),
            ),
          ),
        ),
      );
      await tester.tap(find.text('Open'));
      await tester.pumpAndSettle();
      return () => thrown;
    }

    /// Closes the open dialog; the open then fails with the original authorization error.
    Future<void> dismissAndExpectFailure(
      WidgetTester tester,
      Object? Function() thrown,
    ) async {
      await tester.sendKeyEvent(LogicalKeyboardKey.escape);
      await tester.pumpAndSettle();
      expect(
        thrown(),
        isA<LibraryRegistryFailure>().having(
          (failure) => failure.kind,
          'kind',
          OpenLibraryFailureKind.remoteNotAuthorized,
        ),
      );
    }

    Future<LibraryLoginMethodsResponse> failed(String _) async =>
        const LibraryLoginMethodsResponse(
          methods: [],
          hosts: [],
          serviceName: null,
          failure: LibraryLoginFailure.unreachable,
        );

    Future<LibraryLoginMethodsResponse> throwing(String _) async =>
        throw StateError('no answer');

    for (final (name, methods) in [
      ('throws', throwing),
      ('reports failure', failed),
    ]) {
      testWidgets('an in-process record shows an error when it $name', (
        tester,
      ) async {
        final thrown = await openAndFail(
          tester,
          endpoint: 'rune-service://apple-music',
          methods: methods,
        );

        expect(find.text('Login Failed'), findsOneWidget);
        expect(
          find.text('An error occurred while trying to login'),
          findsOneWidget,
        );
        expect(find.byType(TextBox), findsNothing);
        await dismissAndExpectFailure(tester, thrown);
      });

      testWidgets(
        'a plain record still shows the password dialog when it $name',
        (tester) async {
          await openAndFail(
            tester,
            endpoint: 'https://music.example',
            methods: methods,
          );

          expect(find.byType(TextBox), findsWidgets);
          expect(find.text('Login Failed'), findsNothing);
        },
      );
    }
  });
}
