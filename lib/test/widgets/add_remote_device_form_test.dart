import 'package:fluent_ui/fluent_ui.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:rune/l10n/app_localizations.dart';
import 'package:rune/screens/settings_neighbors/utils/add_remote_device_form_controller.dart';
import 'package:rune/screens/settings_neighbors/widgets/add_remote_device_form.dart';

void main() {
  testWidgets('the form asks for an address only and validates it', (
    tester,
  ) async {
    final controller = AddRemoteDeviceFormController();
    addTearDown(controller.dispose);
    var submitted = 0;

    await tester.pumpWidget(
      FluentApp(
        locale: const Locale('en'),
        localizationsDelegates: AppLocalizations.localizationsDelegates,
        supportedLocales: AppLocalizations.supportedLocales,
        home: ScaffoldPage(
          content: AddRemoteDeviceForm(
            controller: controller,
            onSubmit: () => submitted++,
          ),
        ),
      ),
    );

    expect(find.byType(TextFormBox), findsOneWidget);
    expect(find.byType(ToggleSwitch), findsNothing);
    expect(controller.hostnameController.text, isEmpty);

    expect(controller.formKey.currentState!.validate(), isFalse);
    await tester.pump();
    expect(find.text("Enter the device's IP address or hostname."), findsOne);

    await tester.enterText(find.byType(TextFormBox), '192.168.1.20');
    await tester.testTextInput.receiveAction(TextInputAction.done);
    expect(submitted, 1);
    expect(controller.formKey.currentState!.validate(), isTrue);
  });
}
