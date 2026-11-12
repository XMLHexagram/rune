import 'package:fluent_ui/fluent_ui.dart';
import 'package:material_symbols_icons/symbols.dart';

import '../../bindings/bindings.dart';
import '../../providers/request_capabilities.dart';
import '../api/add_to_library.dart';
import '../api/request_failure.dart';
import '../dialogs/information/error.dart';
import '../l10n.dart';

/// Whether the context menu of an item may offer "Add to Library".
///
/// Only a track or album that is outside the library qualifies, and only
/// when the open library supports adding. [onAdded] is given only for items
/// that are outside.
bool canAddToLibrary(
  BuildContext context,
  CollectionType type,
  void Function()? onAdded,
) =>
    onAdded != null &&
    (type == CollectionType.track || type == CollectionType.album) &&
    catalogueRequestAvailable(
      context,
      (provider) => provider.addToLibraryRequest,
      listen: false,
    );

typedef AddToLibraryAction =
    Future<void> Function({List<int> trackIds, List<int> albumIds});

/// Adds one item to the library after the menu was used.
///
/// [onAdded] runs only when the add succeeded. Any error is shown in the same
/// error dialog the other context menu actions use. The caller checks
/// [canAddToLibrary] first.
Future<void> runAddToLibrary(
  BuildContext context, {
  required CollectionType type,
  required int id,
  required void Function() onAdded,
  VoidCallback? closeMenu,
  AddToLibraryAction add = addToLibrary,
}) async {
  final host = Navigator.of(context).context;
  closeMenu?.call();
  try {
    await add(
      trackIds: type == CollectionType.track ? [id] : const [],
      albumIds: type == CollectionType.album ? [id] : const [],
    );
  } catch (error) {
    if (!host.mounted) return;
    await showErrorDialog(
      context: host,
      title: S.of(host).unknownError,
      errorMessage: requestErrorText(S.of(host), error),
    );
    return;
  }
  onAdded();
}

/// The "Add to Library" entry, or null when the item does not qualify.
MenuFlyoutItem? addToLibraryMenuItem(
  BuildContext context, {
  required CollectionType type,
  required int id,
  required void Function()? onAdded,
  AddToLibraryAction add = addToLibrary,
}) {
  if (!canAddToLibrary(context, type, onAdded)) return null;
  return MenuFlyoutItem(
    leading: const Icon(Symbols.library_add),
    text: Text(S.of(context).addToLibrary),
    onPressed: () => runAddToLibrary(
      context,
      type: type,
      id: id,
      onAdded: onAdded!,
      closeMenu: () => Flyout.of(context).close(),
      add: add,
    ),
  );
}

/// The compact (band) menu button, or null when the item does not qualify.
CommandBarButton? addToLibraryCommandButton(
  BuildContext context, {
  required CollectionType type,
  required int id,
  required void Function()? onAdded,
  AddToLibraryAction add = addToLibrary,
}) {
  if (!canAddToLibrary(context, type, onAdded)) return null;
  return CommandBarButton(
    icon: Tooltip(
      message: S.of(context).addToLibrary,
      child: const Icon(Symbols.library_add),
    ),
    onPressed: () => runAddToLibrary(
      context,
      type: type,
      id: id,
      onAdded: onAdded!,
      add: add,
    ),
  );
}
