import 'dart:math';

import 'package:fluent_ui/fluent_ui.dart';
import 'package:provider/provider.dart';

import '../../../utils/router/router_aware_flyout_controller.dart';
import '../../../widgets/ax_pressure.dart';
import '../../../widgets/hover_opacity.dart';
import '../../../widgets/ax_reveal/ax_reveal.dart';
import '../../../widgets/context_menu_wrapper.dart';
import '../../../providers/responsive_providers.dart';
import '../../../bindings/bindings.dart';
import '../utils/search_membership.dart';
import 'not_in_library_marker.dart';

abstract class SearchCard extends StatefulWidget {
  final int index;

  const SearchCard({super.key, required this.index});
}

abstract class SearchCardState<T extends SearchCard> extends State<T> {
  final contextController = RouterAwareFlyoutController();
  final contextAttachKey = GlobalKey();

  int getItemId();

  String getItemTitle();

  CollectionType getItemType();

  /// The callback for the context menu when this item is outside the library,
  /// otherwise null.
  ///
  /// The membership is looked up now, while the menu opens. The callback runs
  /// after the add finished, when this card may be gone.
  void Function()? addedToLibraryCallback(BuildContext context) {
    final membership = context.read<SearchMembership?>();
    final type = getItemType();
    final id = getItemId();
    if (membership == null || !membership.isOutside(type, id)) return null;
    return () => membership.markAdded(type, id);
  }

  Widget buildLeadingWidget(double size);

  void onPressed(BuildContext context);

  void onMiddleClick(BuildContext context, Offset position);

  void onContextMenu(BuildContext context, Offset position);

  @override
  void dispose() {
    super.dispose();
    contextController.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final outside =
        context.watch<SearchMembership?>()?.isOutside(
          getItemType(),
          getItemId(),
        ) ??
        false;
    return ContextMenuWrapper(
      contextAttachKey: contextAttachKey,
      contextController: contextController,
      onMiddleClick: (position) => onMiddleClick(context, position),
      onContextMenu: (position) => onContextMenu(context, position),
      child: AxPressure(
        child: DeviceTypeBuilder(
          deviceType: const [
            DeviceType.dock,
            DeviceType.belt,
            DeviceType.band,
            DeviceType.zune,
            DeviceType.tv
          ],
          builder: (context, deviceType) {
            if (deviceType == DeviceType.dock ||
                deviceType == DeviceType.band ||
                deviceType == DeviceType.belt) {
              return Padding(
                padding: const EdgeInsets.symmetric(
                  vertical: 1,
                  horizontal: 2,
                ),
                child: Button(
                  style: const ButtonStyle(
                    padding: WidgetStatePropertyAll(EdgeInsets.all(0)),
                  ),
                  onPressed: () => onPressed(context),
                  child: ClipRRect(
                    borderRadius: BorderRadius.circular(3),
                    child: LayoutBuilder(
                      builder: (context, constraints) {
                        final size =
                            min(constraints.maxWidth, constraints.maxHeight);
                        return buildLeadingWidget(size);
                      },
                    ),
                  ),
                ),
              );
            }

            if (deviceType == DeviceType.zune) {
              final typography = FluentTheme.of(context).typography;

              return Padding(
                padding: const EdgeInsets.symmetric(vertical: 4),
                child: Row(
                  children: [
                    SizedBox(
                      width: 40,
                      height: 40,
                      child: buildLeadingWidget(40),
                    ),
                    Expanded(
                      child: Padding(
                        padding: const EdgeInsets.all(8),
                        child: HoverOpacity(
                          child: SearchItemTitle(
                            title: getItemTitle(),
                            outside: outside,
                            style: typography.bodyLarge
                                ?.apply(fontSizeFactor: 0.9),
                          ),
                        ),
                      ),
                    ),
                  ],
                ),
              );
            }

            return AxReveal0(
              child: Button(
                style: const ButtonStyle(
                  padding: WidgetStatePropertyAll(EdgeInsets.all(0)),
                ),
                onPressed: () => onPressed(context),
                child: ClipRRect(
                  borderRadius: BorderRadius.circular(3),
                  child: LayoutBuilder(
                    builder: (context, constraints) {
                      final size =
                          min(constraints.maxWidth, constraints.maxHeight);
                      return Row(
                        children: [
                          buildLeadingWidget(size),
                          Expanded(
                            child: Padding(
                              padding: const EdgeInsets.all(8),
                              child: Column(
                                crossAxisAlignment: CrossAxisAlignment.start,
                                mainAxisAlignment: MainAxisAlignment.center,
                                children: [
                                  SearchItemTitle(
                                    title: getItemTitle(),
                                    outside: outside,
                                  ),
                                ],
                              ),
                            ),
                          ),
                        ],
                      );
                    },
                  ),
                ),
              ),
            );
          },
        ),
      ),
    );
  }
}
