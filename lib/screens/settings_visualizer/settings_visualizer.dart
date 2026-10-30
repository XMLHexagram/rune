import 'package:fluent_ui/fluent_ui.dart';

import '../../utils/settings_page_padding.dart';
import '../../widgets/unavailable_page_on_band.dart';
import '../../widgets/navigation_bar/page_content_frame.dart';

import 'widgets/visualizer_log_setting.dart';
import 'widgets/visualizer_import_setting.dart';
import 'widgets/visualizer_scene_order_setting.dart';
import 'widgets/visualizer_scenes_setting.dart';

class SettingsVisualizerPage extends StatelessWidget {
  const SettingsVisualizerPage({super.key});

  @override
  Widget build(BuildContext context) {
    return PageContentFrame(
      child: UnavailablePageOnBand(
        child: SingleChildScrollView(
          padding: getScrollContainerPadding(context),
          child: SettingsPagePadding(
            child: Column(
              children: [
                VisualizerScenesSetting(),
                VisualizerSceneOrderSetting(),
                VisualizerImportSetting(),
                VisualizerLogSetting(),
              ],
            ),
          ),
        ),
      ),
    );
  }
}
