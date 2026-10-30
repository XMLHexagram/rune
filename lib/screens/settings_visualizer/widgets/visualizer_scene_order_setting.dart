import 'package:provider/provider.dart';
import 'package:fluent_ui/fluent_ui.dart';

import '../../../utils/l10n.dart';
import '../../../utils/visualizer_supported.dart';
import '../../../widgets/settings/settings_box_combo_box.dart';
import '../../../providers/visualizer_scenes.dart';

/// Whether each song gets the next checked scene in order, or a random one.
class VisualizerSceneOrderSetting extends StatelessWidget {
  const VisualizerSceneOrderSetting({super.key});

  @override
  Widget build(BuildContext context) {
    if (!isVisualizerSupported) {
      return const SizedBox.shrink();
    }

    final s = S.of(context);
    final provider = context.watch<VisualizerScenesProvider>();

    return SettingsBoxComboBox<bool>(
      title: s.visualizerSceneOrder,
      subtitle: s.visualizerSceneOrderSubtitle,
      value: provider.shuffle,
      items: [
        SettingsBoxComboBoxItem(
          value: false,
          title: s.visualizerSceneOrderInOrder,
        ),
        SettingsBoxComboBoxItem(
          value: true,
          title: s.visualizerSceneOrderShuffle,
        ),
      ],
      onChanged: (shuffle) {
        if (shuffle != null) provider.setShuffle(shuffle);
      },
    );
  }
}
