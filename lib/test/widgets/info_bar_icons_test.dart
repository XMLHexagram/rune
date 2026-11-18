import 'package:fluent_ui/fluent_ui.dart';
import 'package:flutter_test/flutter_test.dart';

import '../../theme.dart';
import '../../widgets/visualizer/visualizer_dark_theme.dart';

void main() {
  Future<IconData?> drawnIcon(
    WidgetTester tester,
    FluentThemeData theme,
    InfoBarSeverity severity,
  ) async {
    await tester.pumpWidget(
      FluentApp(
        theme: theme,
        home: ScaffoldPage(
          content: InfoBar(title: const Text('Notice'), severity: severity),
        ),
      ),
    );
    final icon = find.descendant(
      of: find.byType(InfoBar),
      matching: find.byType(Icon),
    );
    return tester.widget<Icon>(icon).icon;
  }

  testWidgets('every info bar draws a Fluent icon the app bundles', (
    tester,
  ) async {
    final theme = AppTheme().themeData(Brightness.light);
    for (final severity in InfoBarSeverity.values) {
      final icon = await drawnIcon(tester, theme, severity);
      expect(icon, infoBarIcon(severity), reason: '$severity');
      expect(icon?.fontFamily, FluentIcons.info_solid.fontFamily);
    }
  });

  testWidgets('the visualizer dark theme keeps the info bar icons', (
    tester,
  ) async {
    final theme = darkVariantOf(AppTheme().themeData(Brightness.light));
    for (final severity in InfoBarSeverity.values) {
      expect(
        await drawnIcon(tester, theme, severity),
        infoBarIcon(severity),
        reason: '$severity',
      );
    }
  });
}
