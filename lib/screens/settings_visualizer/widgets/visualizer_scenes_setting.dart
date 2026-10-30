import 'dart:ui';

import 'package:fluent_ui/fluent_ui.dart';
import 'package:material_symbols_icons/symbols.dart';
import 'package:provider/provider.dart';
import 'package:reorderables/reorderables.dart';

import '../../../providers/visualizer_scenes.dart';
import '../../../utils/build_draggable_feedback.dart';
import '../../../utils/l10n.dart';
import '../../../utils/visualizer_supported.dart';
import '../../../widgets/pointer_type.dart';
import '../../../widgets/settings/settings_block_title.dart';

/// The scenes of the visualizer: check the ones to pick from, and drag them
/// into the order the cover wall page's menu lists them in. The last checked
/// scene cannot be unchecked.
class VisualizerScenesSetting extends StatelessWidget {
  const VisualizerScenesSetting({super.key});

  @override
  Widget build(BuildContext context) {
    if (!isVisualizerSupported) return const SizedBox.shrink();

    final s = S.of(context);
    final theme = FluentTheme.of(context);
    final provider = context.watch<VisualizerScenesProvider>();
    final scenes = provider.scenes;
    final enabledCount = scenes.where((scene) => scene.enabled).length;

    return Padding(
      padding: const EdgeInsets.all(4),
      child: Expander(
        initiallyExpanded: true,
        header: Padding(
          padding: const EdgeInsets.symmetric(vertical: 11),
          child: SettingsBlockTitle(
            title: s.visualizerScenes,
            subtitle: s.visualizerScenesSubtitle,
          ),
        ),
        content: SizedBox(
          width: double.maxFinite,
          child: PointerTypeBuilder(
            builder: (kind) {
              return ReorderableColumn(
                needsLongPressDraggable: kind == PointerDeviceKind.touch,
                onReorder: provider.reorder,
                buildDraggableFeedback: buildDraggableFeedback,
                children: [
                  for (final scene in scenes)
                    Padding(
                      key: ValueKey(scene.path),
                      padding: const EdgeInsets.symmetric(vertical: 4),
                      child: Row(
                        children: [
                          Expanded(
                            child: Align(
                              alignment: Alignment.centerLeft,
                              child: Checkbox(
                                content: Text.rich(
                                  TextSpan(
                                    children: [
                                      TextSpan(
                                        text: scene.isCoverWall
                                            ? s.coverWall
                                            : scene.name,
                                      ),
                                      if (scene.group.isNotEmpty)
                                        TextSpan(
                                          text: '  ${scene.group}',
                                          style: TextStyle(
                                            color: theme
                                                .resources
                                                .textFillColorSecondary,
                                          ),
                                        ),
                                    ],
                                  ),
                                ),
                                checked: scene.enabled,
                                onChanged: scene.enabled && enabledCount <= 1
                                    ? null
                                    : (checked) {
                                        if (checked != null) {
                                          provider.setEnabled(
                                            scene.path,
                                            checked,
                                          );
                                        }
                                      },
                              ),
                            ),
                          ),
                          Icon(
                            Symbols.drag_indicator,
                            color: theme.resources.textFillColorSecondary,
                          ),
                        ],
                      ),
                    ),
                ],
              );
            },
          ),
        ),
      ),
    );
  }
}
