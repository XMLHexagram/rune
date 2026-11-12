import 'dart:async';
import 'dart:typed_data';

import 'package:fluent_ui/fluent_ui.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:material_symbols_icons/symbols.dart';
import 'package:provider/provider.dart';
import 'package:provider/single_child_widget.dart';
import '../../bindings/bindings.dart';
import '../../l10n/app_localizations.dart';
import '../../providers/request_capabilities.dart';
import '../../providers/responsive_providers.dart';
import '../../screens/search/utils/search_membership.dart';
import '../../screens/search/widgets/not_in_library_marker.dart';
import '../../screens/search/widgets/search_card.dart';
import '../../screens/collection/utils/is_user_generated.dart';
import '../../utils/api/add_to_library.dart';
import '../../utils/api/request_failure.dart';
import '../../utils/context_menu/add_to_library_item.dart';
import '../../widgets/ax_reveal/widgets/reveal_effect_context.dart';
import '../../widgets/start_screen/providers/start_screen_layout_manager.dart';

import 'package:rinf/rinf.dart';

import 'capability_gate_test.dart' show snapshot;

class _Responsive extends ChangeNotifier implements ResponsiveProvider {
  @override
  DeviceType getActiveDeviceType(List<DeviceType> deviceTypes) => DeviceType.tv;
  @override
  bool smallerOrEqualTo(DeviceType type, [bool autoOrientation = true]) =>
      false;
  @override
  dynamic noSuchMethod(Invocation invocation) => super.noSuchMethod(invocation);
}

class _Screen extends ChangeNotifier implements ScreenSizeProvider {
  @override
  Size get screenSize => const Size(800, 600);
  @override
  Size get layoutSize => const Size(800, 600);
  @override
  dynamic noSuchMethod(Invocation invocation) => super.noSuchMethod(invocation);
}

class _Card extends SearchCard {
  const _Card({required super.index});

  @override
  State<_Card> createState() => _CardState();
}

class _CardState extends SearchCardState<_Card> {
  @override
  int getItemId() => widget.index;
  @override
  String getItemTitle() => 'Item ${widget.index}';
  @override
  CollectionType getItemType() => CollectionType.track;
  @override
  Widget buildLeadingWidget(double size) => SizedBox.square(dimension: size);
  @override
  void onPressed(BuildContext context) {}
  @override
  void onMiddleClick(BuildContext context, Offset position) {}
  @override
  void onContextMenu(BuildContext context, Offset position) {}
}

Widget app(Widget home, {List<SingleChildWidget> providers = const []}) {
  final fluent = FluentApp(
    locale: const Locale('en'),
    localizationsDelegates: AppLocalizations.localizationsDelegates,
    supportedLocales: AppLocalizations.supportedLocales,
    home: ScaffoldPage(content: home),
  );
  return providers.isEmpty
      ? fluent
      : MultiProvider(providers: providers, child: fluent);
}

RequestCapabilitiesProvider capabilitiesWith(RequestCapability capability) {
  final provider = RequestCapabilitiesProvider(
    changes: const Stream.empty(),
    initial: snapshot().copyWith(
      fetchLibraryMembershipRequest: capability,
      addToLibraryRequest: capability,
    ),
  );
  addTearDown(provider.dispose);
  return provider;
}

FetchLibraryMembershipResponse answer({
  List<int> tracks = const [],
  List<int> albums = const [],
  List<int> artists = const [],
}) => FetchLibraryMembershipResponse(
  outsideTrackIds: tracks,
  outsideAlbumIds: albums,
  outsideArtistIds: artists,
);

void main() {
  group('marker', () {
    testWidgets('a title shows the marker only when outside', (tester) async {
      await tester.pumpWidget(
        app(
          const Column(
            children: [
              SearchItemTitle(title: 'Outside', outside: true),
              SearchItemTitle(title: 'Inside', outside: false),
            ],
          ),
        ),
      );
      expect(find.text('Outside'), findsOneWidget);
      expect(find.text('Inside'), findsOneWidget);
      expect(find.byIcon(Symbols.cloud), findsOneWidget);
      expect(find.byType(Tooltip), findsOneWidget);
      expect(
        tester.widget<Tooltip>(find.byType(Tooltip)).message,
        'Not in Your Library',
      );
    });

    testWidgets('a card is marked only when its id is outside, and the '
        'marker goes after a successful add', (tester) async {
      tester.view.physicalSize = const Size(800, 600);
      tester.view.devicePixelRatio = 1;
      addTearDown(tester.view.reset);
      final layout = StartScreenLayoutManager();
      addTearDown(layout.dispose);
      final membership = SearchMembership(
        fetch:
            ({
              trackIds = const [],
              albumIds = const [],
              artistIds = const [],
            }) async => answer(tracks: [1]),
      );
      addTearDown(membership.dispose);

      await tester.pumpWidget(
        MultiProvider(
          providers: [
            ChangeNotifierProvider<ResponsiveProvider>.value(
              value: _Responsive(),
            ),
            ChangeNotifierProvider<ScreenSizeProvider>.value(value: _Screen()),
            ChangeNotifierProvider<StartScreenLayoutManager>.value(
              value: layout,
            ),
            ChangeNotifierProvider<SearchMembership>.value(value: membership),
          ],
          child: FluentApp(
            localizationsDelegates: AppLocalizations.localizationsDelegates,
            supportedLocales: AppLocalizations.supportedLocales,
            builder: (context, child) => RevealEffectContext(child: child!),
            home: const ScaffoldPage(
              content: Column(
                children: [
                  SizedBox(height: 80, child: _Card(index: 1)),
                  SizedBox(height: 80, child: _Card(index: 2)),
                ],
              ),
            ),
          ),
        ),
      );
      await membership.refresh(trackIds: [1, 2]);
      layout.playAnimations();
      await tester.pumpAndSettle();

      expect(find.byIcon(Symbols.cloud), findsOneWidget);
      final outsideCard = tester.element(find.byType(_Card).first);
      final state = tester.state<_CardState>(find.byType(_Card).first);
      final insideState = tester.state<_CardState>(find.byType(_Card).last);
      expect(state.addedToLibraryCallback(outsideCard), isNotNull);
      expect(insideState.addedToLibraryCallback(outsideCard), isNull);

      state.addedToLibraryCallback(outsideCard)!();
      await tester.pumpAndSettle();
      expect(find.byIcon(Symbols.cloud), findsNothing);
      expect(find.text('Item 1'), findsOneWidget);
    });

    testWidgets('cards without any membership state show no marker', (
      tester,
    ) async {
      tester.view.physicalSize = const Size(800, 600);
      tester.view.devicePixelRatio = 1;
      addTearDown(tester.view.reset);
      final layout = StartScreenLayoutManager();
      addTearDown(layout.dispose);
      await tester.pumpWidget(
        MultiProvider(
          providers: [
            ChangeNotifierProvider<ResponsiveProvider>.value(
              value: _Responsive(),
            ),
            ChangeNotifierProvider<ScreenSizeProvider>.value(value: _Screen()),
            ChangeNotifierProvider<StartScreenLayoutManager>.value(
              value: layout,
            ),
          ],
          child: FluentApp(
            localizationsDelegates: AppLocalizations.localizationsDelegates,
            supportedLocales: AppLocalizations.supportedLocales,
            builder: (context, child) => RevealEffectContext(child: child!),
            home: const ScaffoldPage(
              content: SizedBox(height: 80, child: _Card(index: 1)),
            ),
          ),
        ),
      );
      layout.playAnimations();
      await tester.pumpAndSettle();
      expect(find.text('Item 1'), findsOneWidget);
      expect(find.byIcon(Symbols.cloud), findsNothing);
    });
  });

  group('menu item', () {
    Future<MenuFlyoutItem?> itemFor(
      WidgetTester tester, {
      required RequestCapabilitiesProvider? capabilities,
      required CollectionType type,
      required bool outside,
    }) async {
      MenuFlyoutItem? item;
      await tester.pumpWidget(
        app(
          Builder(
            builder: (context) {
              item = addToLibraryMenuItem(
                context,
                type: type,
                id: 5,
                onAdded: outside ? () {} : null,
              );
              return const SizedBox();
            },
          ),
          providers: [
            if (capabilities != null)
              ChangeNotifierProvider<RequestCapabilitiesProvider>.value(
                value: capabilities,
              ),
          ],
        ),
      );
      return item;
    }

    testWidgets('is offered for an outside track or album when available', (
      tester,
    ) async {
      final available = capabilitiesWith(const RequestCapabilityAvailable());
      for (final type in [CollectionType.track, CollectionType.album]) {
        expect(
          await itemFor(
            tester,
            capabilities: available,
            type: type,
            outside: true,
          ),
          isNotNull,
        );
      }
    });

    testWidgets('is not offered for an item inside the library', (
      tester,
    ) async {
      final available = capabilitiesWith(const RequestCapabilityAvailable());
      expect(
        await itemFor(
          tester,
          capabilities: available,
          type: CollectionType.track,
          outside: false,
        ),
        isNull,
      );
    });

    testWidgets('is not offered for artists', (tester) async {
      final available = capabilitiesWith(const RequestCapabilityAvailable());
      expect(
        await itemFor(
          tester,
          capabilities: available,
          type: CollectionType.artist,
          outside: true,
        ),
        isNull,
      );
    });

    testWidgets('is not offered when the capability is unavailable', (
      tester,
    ) async {
      final hidden = capabilitiesWith(const RequestCapabilityHidden());
      expect(
        await itemFor(
          tester,
          capabilities: hidden,
          type: CollectionType.track,
          outside: true,
        ),
        isNull,
      );
      expect(
        await itemFor(
          tester,
          capabilities: null,
          type: CollectionType.track,
          outside: true,
        ),
        isNull,
      );
    });
  });

  group('membership', () {
    test(
      'keeps the answer of the newest request when an older one is late',
      () async {
        final pending = <Completer<FetchLibraryMembershipResponse>>[];
        final membership = SearchMembership(
          fetch:
              ({
                trackIds = const [],
                albumIds = const [],
                artistIds = const [],
              }) {
                final completer = Completer<FetchLibraryMembershipResponse>();
                pending.add(completer);
                return completer.future;
              },
        );
        addTearDown(membership.dispose);

        final first = membership.refresh(trackIds: [1]);
        final second = membership.refresh(trackIds: [2]);
        pending[1].complete(answer(tracks: [2]));
        await second;
        pending[0].complete(answer(tracks: [1]));
        await first;

        expect(membership.isOutside(CollectionType.track, 2), isTrue);
        expect(membership.isOutside(CollectionType.track, 1), isFalse);
      },
    );

    test(
      'ignores an answer after the search was invalidated or reset',
      () async {
        final pending = <Completer<FetchLibraryMembershipResponse>>[];
        final membership = SearchMembership(
          fetch:
              ({
                trackIds = const [],
                albumIds = const [],
                artistIds = const [],
              }) {
                final completer = Completer<FetchLibraryMembershipResponse>();
                pending.add(completer);
                return completer.future;
              },
        );
        addTearDown(membership.dispose);

        final first = membership.refresh(trackIds: [1]);
        membership.invalidate();
        pending[0].complete(answer(tracks: [1]));
        await first;
        expect(membership.isOutside(CollectionType.track, 1), isFalse);

        final second = membership.refresh(albumIds: [3]);
        membership.reset();
        pending[1].complete(answer(albums: [3]));
        await second;
        expect(membership.isOutside(CollectionType.album, 3), isFalse);
      },
    );

    test(
      'a failed request marks nothing and markAdded clears one id',
      () async {
        var fail = true;
        final membership = SearchMembership(
          fetch:
              ({
                trackIds = const [],
                albumIds = const [],
                artistIds = const [],
              }) async {
                if (fail) throw StateError('no');
                return answer(tracks: [1, 2], artists: [9]);
              },
        );
        addTearDown(membership.dispose);

        await membership.refresh(trackIds: [1, 2]);
        expect(membership.isOutside(CollectionType.track, 1), isFalse);

        fail = false;
        await membership.refresh(trackIds: [1, 2], artistIds: [9]);
        expect(membership.isOutside(CollectionType.artist, 9), isTrue);
        membership.markAdded(CollectionType.track, 1);
        expect(membership.isOutside(CollectionType.track, 1), isFalse);
        expect(membership.isOutside(CollectionType.track, 2), isTrue);
      },
    );
  });

  group('capability helper', () {
    Future<bool> availability(
      WidgetTester tester,
      RequestCapabilitiesProvider? provider,
    ) async {
      late bool result;
      await tester.pumpWidget(
        app(
          Builder(
            builder: (context) {
              result = catalogueRequestAvailable(
                context,
                (p) => p.fetchLibraryMembershipRequest,
                listen: false,
              );
              return const SizedBox();
            },
          ),
          providers: [
            if (provider != null)
              ChangeNotifierProvider<RequestCapabilitiesProvider>.value(
                value: provider,
              ),
          ],
        ),
      );
      return result;
    }

    testWidgets('is false without a provider', (tester) async {
      expect(await availability(tester, null), isFalse);
    });

    testWidgets('is false with a null snapshot', (tester) async {
      final provider = RequestCapabilitiesProvider(
        changes: const Stream.empty(),
      );
      addTearDown(provider.dispose);
      if (provider.snapshot != null) return;
      expect(await availability(tester, provider), isFalse);
    });

    testWidgets('follows the snapshot', (tester) async {
      expect(
        await availability(
          tester,
          capabilitiesWith(const RequestCapabilityAvailable()),
        ),
        isTrue,
      );
      expect(
        await availability(
          tester,
          capabilitiesWith(const RequestCapabilityHidden()),
        ),
        isFalse,
      );
    });
  });

  group('results', () {
    SearchMembership counting(List<List<int>> asked, {List<int>? outside}) =>
        SearchMembership(
          fetch:
              ({
                trackIds = const [],
                albumIds = const [],
                artistIds = const [],
              }) async {
                asked.add([...trackIds, ...albumIds, ...artistIds]);
                return answer(tracks: outside ?? trackIds);
              },
        );

    test('nothing is asked when the capability is not available', () async {
      final asked = <List<int>>[];
      final membership = counting(asked);
      addTearDown(membership.dispose);
      await membership.updateForResults(available: true, trackIds: [1]);
      expect(membership.isOutside(CollectionType.track, 1), isTrue);

      await membership.updateForResults(available: false, trackIds: [1]);
      expect(asked, hasLength(1));
      expect(membership.isOutside(CollectionType.track, 1), isFalse);
    });

    test('nothing is asked for a search with no results', () async {
      final asked = <List<int>>[];
      final membership = counting(asked);
      addTearDown(membership.dispose);
      await membership.updateForResults(available: true);
      expect(asked, isEmpty);
    });

    test('a new search clears the marks of the old one', () async {
      final asked = <List<int>>[];
      final membership = counting(asked, outside: const []);
      addTearDown(membership.dispose);
      await membership.refresh(trackIds: [1]);
      membership.markAdded(CollectionType.track, 1);
      final second = SearchMembership(
        fetch:
            ({
              trackIds = const [],
              albumIds = const [],
              artistIds = const [],
            }) async => answer(tracks: trackIds),
      );
      addTearDown(second.dispose);
      await second.refresh(trackIds: [1]);
      expect(second.isOutside(CollectionType.track, 1), isTrue);
      final pending = second.refresh(trackIds: [2]);
      // Old marks are gone as soon as the next result set is applied.
      expect(second.isOutside(CollectionType.track, 1), isFalse);
      await pending;
      expect(second.isOutside(CollectionType.track, 2), isTrue);
    });

    test('marking an item after dispose does nothing', () async {
      final membership = SearchMembership(
        fetch:
            ({
              trackIds = const [],
              albumIds = const [],
              artistIds = const [],
            }) async => answer(tracks: [1]),
      );
      await membership.refresh(trackIds: [1]);
      membership.dispose();
      membership.markAdded(CollectionType.track, 1);
    });
  });

  group('add flow', () {
    Widget host(
      Future<void> Function({List<int> trackIds, List<int> albumIds}) add,
      void Function() onAdded, {
      CollectionType type = CollectionType.track,
      Widget Function(BuildContext context)? open,
    }) {
      return app(
        Builder(
          builder: (context) => Button(
            onPressed: () => runAddToLibrary(
              context,
              type: type,
              id: 7,
              onAdded: onAdded,
              add: add,
            ),
            child: const Text('go'),
          ),
        ),
      );
    }

    testWidgets('passes the track or album id and reports success', (
      tester,
    ) async {
      final calls = <String>[];
      var added = 0;
      Future<void> add({
        List<int> trackIds = const [],
        List<int> albumIds = const [],
      }) async => calls.add('$trackIds $albumIds');

      await tester.pumpWidget(host(add, () => added++));
      await tester.tap(find.text('go'));
      await tester.pumpAndSettle();
      await tester.pumpWidget(
        host(add, () => added++, type: CollectionType.album),
      );
      await tester.tap(find.text('go'));
      await tester.pumpAndSettle();

      expect(calls, ['[7] []', '[] [7]']);
      expect(added, 2);
      expect(find.byType(ContentDialog), findsNothing);
    });

    for (final failure in <String, Object>{
      'a request failure': const RequestFailure(
        'AddToLibraryRequest',
        RequestErrorKind.unavailable,
        'x',
      ),
      'a timeout': TimeoutException('slow'),
    }.entries) {
      testWidgets('${failure.key} shows the error dialog and adds nothing', (
        tester,
      ) async {
        var added = 0;
        await tester.pumpWidget(
          host(({trackIds = const [], albumIds = const []}) async {
            throw failure.value;
          }, () => added++),
        );
        await tester.tap(find.text('go'));
        await tester.pumpAndSettle();
        expect(find.byType(ContentDialog), findsOneWidget);
        expect(added, 0);
      });
    }

    testWidgets('the menu item adds when pressed', (tester) async {
      final controller = FlyoutController();
      addTearDown(controller.dispose);
      final calls = <List<int>>[];
      var added = 0;
      await tester.pumpWidget(
        app(
          Builder(
            builder: (context) => FlyoutTarget(
              controller: controller,
              child: Button(
                onPressed: () => controller.showFlyout(
                  builder: (flyoutContext) => MenuFlyout(
                    items: [
                      addToLibraryMenuItem(
                        flyoutContext,
                        type: CollectionType.track,
                        id: 4,
                        onAdded: () => added++,
                        add:
                            ({
                              trackIds = const [],
                              albumIds = const [],
                            }) async => calls.add(trackIds),
                      )!,
                    ],
                  ),
                ),
                child: const Text('open'),
              ),
            ),
          ),
          providers: [
            ChangeNotifierProvider<RequestCapabilitiesProvider>.value(
              value: capabilitiesWith(const RequestCapabilityAvailable()),
            ),
          ],
        ),
      );
      await tester.tap(find.text('open'));
      await tester.pumpAndSettle();
      await tester.tap(find.text('Add to Library'));
      await tester.pumpAndSettle();
      expect(calls, [
        [4],
      ]);
      expect(added, 1);
    });

    testWidgets('the compact menu button adds when pressed', (tester) async {
      final calls = <List<int>>[];
      var added = 0;
      await tester.pumpWidget(
        app(
          Builder(
            builder: (context) => CommandBar(
              primaryItems: [
                addToLibraryCommandButton(
                  context,
                  type: CollectionType.album,
                  id: 9,
                  onAdded: () => added++,
                  add: ({trackIds = const [], albumIds = const []}) async =>
                      calls.add(albumIds),
                )!,
              ],
            ),
          ),
          providers: [
            ChangeNotifierProvider<RequestCapabilitiesProvider>.value(
              value: capabilitiesWith(const RequestCapabilityAvailable()),
            ),
          ],
        ),
      );
      await tester.tap(find.byIcon(Symbols.library_add));
      await tester.pumpAndSettle();
      expect(calls, [
        [9],
      ]);
      expect(added, 1);
    });

    testWidgets('the compact button is absent when not available or inside', (
      tester,
    ) async {
      late CommandBarButton? hidden;
      late CommandBarButton? inside;
      await tester.pumpWidget(
        app(
          Builder(
            builder: (context) {
              hidden = addToLibraryCommandButton(
                context,
                type: CollectionType.track,
                id: 1,
                onAdded: () {},
              );
              inside = addToLibraryCommandButton(
                context,
                type: CollectionType.track,
                id: 1,
                onAdded: null,
              );
              return const SizedBox();
            },
          ),
          providers: [
            ChangeNotifierProvider<RequestCapabilitiesProvider>.value(
              value: capabilitiesWith(const RequestCapabilityHidden()),
            ),
          ],
        ),
      );
      expect(hidden, isNull);
      expect(inside, isNull);
    });

    testWidgets('a card gone while the add is pending causes no error and '
        'the live membership is still updated', (tester) async {
      final membership = SearchMembership(
        fetch:
            ({
              trackIds = const [],
              albumIds = const [],
              artistIds = const [],
            }) async => answer(tracks: [7]),
      );
      addTearDown(membership.dispose);
      await membership.refresh(trackIds: [7]);
      final done = Completer<void>();
      void onAdded() => membership.markAdded(CollectionType.track, 7);

      await tester.pumpWidget(
        app(
          Builder(
            builder: (context) => Button(
              onPressed: () => runAddToLibrary(
                context,
                type: CollectionType.track,
                id: 7,
                onAdded: onAdded,
                add: ({trackIds = const [], albumIds = const []}) =>
                    done.future,
              ),
              child: const Text('go'),
            ),
          ),
        ),
      );
      await tester.tap(find.text('go'));
      await tester.pump();
      await tester.pumpWidget(const SizedBox());
      done.complete();
      await tester.pumpAndSettle();

      expect(tester.takeException(), isNull);
      expect(membership.isOutside(CollectionType.track, 7), isFalse);
    });

    testWidgets('a card gone while the add fails causes no error', (
      tester,
    ) async {
      final done = Completer<void>();
      await tester.pumpWidget(
        app(
          Builder(
            builder: (context) => Button(
              onPressed: () => runAddToLibrary(
                context,
                type: CollectionType.track,
                id: 7,
                onAdded: () {},
                add: ({trackIds = const [], albumIds = const []}) =>
                    done.future,
              ),
              child: const Text('go'),
            ),
          ),
        ),
      );
      await tester.tap(find.text('go'));
      await tester.pump();
      await tester.pumpWidget(const SizedBox());
      done.completeError(TimeoutException('slow'));
      await tester.pumpAndSettle();
      expect(tester.takeException(), isNull);
    });
  });

  group('add_to_library api', () {
    test('an answer without success throws', () async {
      final responses =
          StreamController<RustSignalPack<AddToLibraryResponse>>();
      final failures = StreamController<RequestFailedResponse>();
      addTearDown(responses.close);
      addTearDown(failures.close);
      final future = addToLibraryWithStreams(
        trackIds: [1],
        responses: responses.stream,
        failures: failures.stream,
        sendRequest: (request) => responses.add(
          RustSignalPack(
            AddToLibraryResponse(success: false, callId: request.callId),
            Uint8List(0),
          ),
        ),
      );
      await expectLater(future, throwsA(isA<RequestFailure>()));
    });

    test('an answer for another call does not complete the request', () async {
      final responses =
          StreamController<RustSignalPack<AddToLibraryResponse>>();
      final failures = StreamController<RequestFailedResponse>();
      addTearDown(responses.close);
      addTearDown(failures.close);
      var completed = false;
      final future = addToLibraryWithStreams(
        trackIds: [1],
        responses: responses.stream,
        failures: failures.stream,
        sendRequest: (request) {
          responses.add(
            RustSignalPack(
              AddToLibraryResponse(
                success: true,
                callId: Uint64.fromBigInt(
                  request.callId!.toBigInt() + BigInt.one,
                ),
              ),
              Uint8List(0),
            ),
          );
        },
      ).then((_) => completed = true, onError: (_) => false);
      await Future<void>.delayed(const Duration(milliseconds: 20));
      expect(completed, isFalse);
      unawaited(future);
    });
  });

  group('empty page', () {
    testWidgets('a mix may be created only when the capability allows it', (
      tester,
    ) async {
      final provider = RequestCapabilitiesProvider(
        changes: const Stream.empty(),
        initial: snapshot().copyWith(
          createMixRequest: const RequestCapabilityHidden(),
        ),
      );
      addTearDown(provider.dispose);
      final results = <CollectionType, bool>{};
      await tester.pumpWidget(
        app(
          Builder(
            builder: (context) {
              for (final type in [
                CollectionType.mix,
                CollectionType.playlist,
              ]) {
                results[type] = canCreateCollection(context, type);
              }
              return const SizedBox();
            },
          ),
          providers: [
            ChangeNotifierProvider<RequestCapabilitiesProvider>.value(
              value: provider,
            ),
          ],
        ),
      );
      expect(results, {
        CollectionType.mix: false,
        CollectionType.playlist: true,
      });
    });

    testWidgets('a mix may be created without any capability state', (
      tester,
    ) async {
      late bool result;
      await tester.pumpWidget(
        app(
          Builder(
            builder: (context) {
              result = canCreateCollection(context, CollectionType.mix);
              return const SizedBox();
            },
          ),
        ),
      );
      expect(result, isTrue);
    });
  });
}
