import 'package:flutter_test/flutter_test.dart';
import 'package:rune/providers/responsive_providers.dart';

void main() {
  testWidgets('stops forcing a layout once the setting is removed', (
    tester,
  ) async {
    final sizes = ScreenSizeProvider.withWriter(null, (_) async {});
    String? stored = 'zune';
    final responsive = ResponsiveProvider(sizes, () async => stored);
    await tester.pump();
    expect(responsive.currentDeviceType, DeviceType.zune);

    var notified = 0;
    responsive.addListener(() => notified++);
    stored = null;
    await responsive.updateForceLayoutConfig();
    expect(responsive.currentDeviceType, isNot(DeviceType.zune));
    expect(notified, 1);

    responsive.dispose();
    sizes.dispose();
  });
}
