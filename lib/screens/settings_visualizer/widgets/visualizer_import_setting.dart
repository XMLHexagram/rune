import 'package:provider/provider.dart';
import 'package:fluent_ui/fluent_ui.dart';
import 'package:file_selector/file_selector.dart';
import 'package:material_symbols_icons/symbols.dart';

import '../../../utils/l10n.dart';
import '../../../utils/visualizer_supported.dart';
import '../../../utils/show_visualizer_import.dart';
import '../../../widgets/settings/settings_button.dart';
import '../../../providers/visualizer_scenes.dart';

/// Imports a scene file (`.runescene`) picked by the user.
class VisualizerImportSetting extends StatelessWidget {
  const VisualizerImportSetting({super.key});

  Future<void> _import(BuildContext context) async {
    final provider = context.read<VisualizerScenesProvider>();
    const group = XTypeGroup(
      label: 'Rune scenes',
      extensions: ['runescene'],
      uniformTypeIdentifiers: ['ci.not.rune.scene'],
      mimeTypes: ['application/x-rune-scene'],
    );
    final files = await openFiles(acceptedTypeGroups: const [group]);
    for (final file in files) {
      final result = await provider.importScene(file.path);
      if (!context.mounted) return;
      showVisualizerImport(context, result);
    }
  }

  @override
  Widget build(BuildContext context) {
    if (!isVisualizerSupported) {
      return const SizedBox.shrink();
    }

    final s = S.of(context);

    return SettingsButton(
      icon: Symbols.add,
      title: s.visualizerImportScene,
      subtitle: s.visualizerImportSceneSubtitle,
      onPressed: () => _import(context),
    );
  }
}
