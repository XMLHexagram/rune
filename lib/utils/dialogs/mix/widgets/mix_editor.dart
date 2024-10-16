import 'package:fluent_ui/fluent_ui.dart';
import 'package:rune/messages/collection.pb.dart';
import 'package:rune/utils/api/fetch_collection_by_ids.dart';
import 'package:rune/utils/api/fetch_collection_group_summary_title.dart';
import 'package:rune/utils/api/fetch_media_file_by_ids.dart';
import 'package:rune/utils/api/search_collection_summary.dart';

import 'package:provider/provider.dart';

import 'mix_editor_controller.dart';
import 'toggle_switch_section.dart';
import 'search_chip_input_section.dart';

import '../../../../messages/search.pb.dart';

import '../../../api/fetch_track_summary.dart';
import '../../../dialogs/mix/widgets/input_section.dart';
import '../../../dialogs/mix/widgets/editable_combo_box_section.dart';

import '../config/mode_select_items.dart';
import '../config/sort_select_items.dart';
import '../config/recommend_select_items.dart';

import 'slider_section.dart';
import 'directory_section.dart';
import 'select_input_section.dart';

class MixEditor extends StatefulWidget {
  final MixEditorController? controller;
  const MixEditor({super.key, this.controller});

  @override
  State<MixEditor> createState() => _MixEditorState();
}

class _MixEditorState extends State<MixEditor> {
  late final MixEditorController _controller;

  @override
  void initState() {
    super.initState();

    _controller = widget.controller ?? MixEditorController();
  }

  @override
  void dispose() {
    if (widget.controller == null) {
      _controller.dispose();
    }
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return ChangeNotifierProvider<MixEditorController>(
      create: (_) => _controller,
      child: ListView(
        padding: const EdgeInsets.only(right: 16),
        children: [
          InputSection(
            controller: _controller.titleController,
            title: 'Title',
          ),
          EditableComboBoxSection(
            controller: _controller.groupController,
            title: 'Group',
            getItems: () =>
                fetchCollectionGroupSummaryTitle(CollectionType.Mix),
          ),
          SearchChipInputSection(
            controller: _controller.artistsController,
            title: 'Artists',
            getInitResult: () => getInitResult(CollectionType.Artist),
            searchForItems: (query) => _searchItems(
              query,
              'artists',
              (x) async {
                return (await fetchCollectionByIds(CollectionType.Artist, x))
                    .map((x) => (x.id, x.name))
                    .toList();
              },
            ),
          ),
          SearchChipInputSection(
            controller: _controller.albumsController,
            title: 'Albums',
            getInitResult: () => getInitResult(CollectionType.Album),
            searchForItems: (query) => _searchItems(
              query,
              'albums',
              (x) async {
                return (await fetchCollectionByIds(CollectionType.Album, x))
                    .map((x) => (x.id, x.name))
                    .toList();
              },
            ),
          ),
          SearchChipInputSection(
            controller: _controller.playlistsController,
            title: 'Playlists',
            getInitResult: () => getInitResult(CollectionType.Playlist),
            searchForItems: (query) => _searchItems(
              query,
              'playlists',
              (x) async {
                return (await fetchCollectionByIds(CollectionType.Playlist, x))
                    .map((x) => (x.id, x.name))
                    .toList();
              },
            ),
          ),
          SearchChipInputSection(
            controller: _controller.tracksController,
            title: 'Tracks',
            getInitResult: () async {
              return (await fetchTrackSummary())
                  .map((x) => AutoSuggestBoxItem<int>(value: x.$1, label: x.$2))
                  .toList();
            },
            searchForItems: (query) => _searchItems(
              query,
              'tracks',
              (x) async {
                return (await fetchMediaFileByIds(x, false))
                    .map((x) => (x.id, x.title))
                    .toList();
              },
            ),
          ),
          DirectorySection(controller: _controller.directoryController),
          SliderSection(
            title: 'Amount',
            controller: _controller.limitController,
          ),
          SelectInputSection(
            controller: _controller.modeController,
            title: "Mode",
            items: modeSelectItems,
            defaultValue: '99',
          ),
          SelectInputSection(
            controller: _controller.recommendationController,
            title: "Recommendation",
            items: recommendSelectItems,
            defaultValue: '',
          ),
          SelectInputSection(
            controller: _controller.sortByController,
            title: "Sort By",
            items: sortSelectItems,
            defaultValue: 'default',
          ),
          ToggleSwitchSection(
            controller: _controller.likedController,
            content: const Text("Liked Only"),
          ),
        ],
      ),
    );
  }
}

Future<Map<String, List<int>>> searchFor(String query, String field) async {
  final searchRequest =
      SearchForRequest(queryStr: query, fields: [field], n: 30);
  searchRequest.sendSignalToRust(); // GENERATED

  final message = (await SearchForResponse.rustSignalStream.first).message;

  final Map<String, List<int>> result = {};

  result['artists'] = message.artists;
  result['albums'] = message.albums;
  result['playlists'] = message.playlists;
  result['tracks'] = message.tracks;

  return result;
}

Future<List<AutoSuggestBoxItem<int>>> getInitResult(
  CollectionType collectionType,
) async {
  final summary = await fetchCollectionSummary(collectionType);
  return summary
      .map((x) => AutoSuggestBoxItem<int>(value: x.$1, label: x.$2))
      .toList();
}

Future<List<AutoSuggestBoxItem<int>>> _searchItems<T>(
    String query,
    String field,
    Future<List<(int, String)>> Function(List<int>) fetchByIds) async {
  final ids = (await searchFor(query, field))[field];

  if (ids == null) return [];

  final items = await fetchByIds(ids);
  return items
      .map((x) => AutoSuggestBoxItem<int>(value: x.$1, label: x.$2))
      .toList();
}
