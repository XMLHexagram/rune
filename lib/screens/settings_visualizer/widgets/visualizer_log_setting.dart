import 'package:provider/provider.dart';
import 'package:fluent_ui/fluent_ui.dart';

import '../../../utils/l10n.dart';
import '../../../utils/visualizer_supported.dart';
import '../../../widgets/settings/settings_box_toggle.dart';
import '../../../providers/visualizer_scenes.dart';

/// Whether the visualizer's diagnostics (which GPU it runs on, how frames reach
/// the screen) are written to the Log page, for debugging. Its errors are
/// written there in any case.
class VisualizerLogSetting extends StatelessWidget {
  const VisualizerLogSetting({super.key});

  @override
  Widget build(BuildContext context) {
    if (!isVisualizerSupported) {
      return const SizedBox.shrink();
    }

    final s = S.of(context);
    final provider = context.watch<VisualizerScenesProvider>();

    return SettingsBoxToggle(
      title: s.visualizerLog,
      subtitle: s.visualizerLogSubtitle,
      value: provider.log,
      onChanged: provider.setLog,
    );
  }
}
