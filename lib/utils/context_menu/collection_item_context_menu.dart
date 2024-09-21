import 'package:fluent_ui/fluent_ui.dart';
import 'package:material_symbols_icons/symbols.dart';

import '../../utils/api/get_all_mixes.dart';
import '../../utils/api/add_item_to_mix.dart';
import '../../utils/dialogs/mix/mix_studio.dart';
import '../../utils/dialogs/mix/create_edit_mix.dart';
import '../../utils/dialogs/mix/remove_mix_dialog.dart';
import '../../utils/dialogs/playlist/remove_playlist_dialog.dart';
import '../../utils/dialogs/playlist/create_edit_playlist.dart';

import '../../messages/playback.pb.dart';
import '../../messages/mix.pbserver.dart';

final Map<String, String> typeToOperator = {
  "album": "lib::album",
  "artist": "lib::artist",
  "playlist": "lib::playlist",
  "track": "lib::track",
};

final Map<String, void Function(BuildContext context, int id)> typeToEdit = {
  "playlist": (context, id) async {
    showCreateEditPlaylistDialog(context, playlistId: id);
  },
  "mix": (context, id) {
    showMixStudioDialog(context, mixId: id);
  },
};

final Map<String, String> typeToEditLabel = {
  "playlist": "Edit Playlist",
  "mix": "Edit Mix",
};

final Map<String, void Function(BuildContext context, int id)> typeToRemove = {
  "playlist": (context, id) {
    showRemovePlaylistDialog(context, id);
  },
  "mix": (context, id) {
    showRemoveMixDialog(context, id);
  },
};

final Map<String, String> typeToRemoveLabel = {
  "playlist": "Remove Playlist",
  "mix": "Remove Mix",
};

void openCollectionItemContextMenu(
  Offset localPosition,
  BuildContext context,
  GlobalKey contextAttachKey,
  FlyoutController contextController,
  String type,
  int id,
) async {
  final targetContext = contextAttachKey.currentContext;

  if (targetContext == null) return;
  final box = targetContext.findRenderObject() as RenderBox;
  final position = box.localToGlobal(
    localPosition,
    ancestor: Navigator.of(context).context.findRenderObject(),
  );

  final mixes = await getAllMixes();

  contextController.showFlyout(
    position: position,
    builder: (context) =>
        buildCollectionItemContextMenu(context, type, id, mixes),
  );
}

Widget buildCollectionItemContextMenu(
  BuildContext context,
  String type,
  int id,
  List<MixWithoutCoverIds> mixes,
) {
  final operator = typeToOperator[type];
  final edit = typeToEdit[type];
  final remove = typeToRemove[type];

  final List<MenuFlyoutItem> mixItems = mixes.map((mix) {
    return MenuFlyoutItem(
      leading: const Icon(Symbols.magic_button),
      text: Text(mix.name),
      onPressed: () {
        addItemToMix(
          mix.id,
          operator ?? "lib::unknown",
          id.toString(),
        );

        Flyout.of(context).close();
      },
    );
  }).toList();

  List<MenuFlyoutItemBase> items = [
    MenuFlyoutItem(
      leading: const Icon(Symbols.play_circle),
      text: const Text('Start Playing'),
      onPressed: () => {
        StartPlayingCollectionRequest(type: type, id: id).sendSignalToRust()
      },
    ),
    MenuFlyoutItem(
      leading: const Icon(Symbols.playlist_add),
      text: const Text('Add to Queue'),
      onPressed: () =>
          {AddToQueueCollectionRequest(type: type, id: id).sendSignalToRust()},
    ),
    MenuFlyoutItem(
      leading: const Icon(Symbols.rocket),
      text: const Text('Start Roaming'),
      onPressed: () => {
        StartRoamingCollectionRequest(type: type, id: id).sendSignalToRust()
      },
    ),
  ];

  if (edit != null) {
    items.add(const MenuFlyoutSeparator());
    items.add(
      MenuFlyoutItem(
        leading: const Icon(Symbols.edit),
        text: Text(typeToEditLabel[type] ?? 'Edit'),
        onPressed: () {
          edit(context, id);
        },
      ),
    );
  }

  if (remove != null) {
    items.add(
      MenuFlyoutItem(
        leading: const Icon(Symbols.delete),
        text: Text(typeToRemoveLabel[type] ?? 'Remove'),
        onPressed: () => {remove(context, id)},
      ),
    );
  }

  if (operator != null) {
    items.add(const MenuFlyoutSeparator());
    items.add(
      MenuFlyoutSubItem(
        leading: const Icon(Symbols.magic_button),
        text: const Text('Add to Mix'),
        items: (context) => [
          MenuFlyoutItem(
            leading: const Icon(Symbols.add),
            text: const Text('New Mix'),
            onPressed: () async {
              Flyout.of(context).close();

              await showCreateEditMixDialog(
                context,
                mixId: null,
                operator: (operator, id.toString()),
              );
            },
          ),
          if (mixItems.isNotEmpty) const MenuFlyoutSeparator(),
          ...mixItems
        ],
      ),
    );
  }

  return MenuFlyout(
    items: items,
  );
}
