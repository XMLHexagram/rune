import 'package:fluent_ui/fluent_ui.dart';

import '../../../utils/l10n.dart';
import '../utils/add_remote_device_form_controller.dart';

// There is no port or secured switch: the backend only speaks rnsrv on port
// 7863 over TLS, so an address is all a device needs.
class AddRemoteDeviceForm extends StatelessWidget {
  final AddRemoteDeviceFormController controller;
  final VoidCallback onSubmit;

  const AddRemoteDeviceForm({
    super.key,
    required this.controller,
    required this.onSubmit,
  });

  @override
  Widget build(BuildContext context) {
    final s = S.of(context);
    return Form(
      key: controller.formKey,
      child: Column(
        mainAxisSize: MainAxisSize.min,
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          const SizedBox(height: 16),
          InfoLabel(
            label: s.hostname,
            child: TextFormBox(
              controller: controller.hostnameController,
              autofocus: true,
              placeholder: '192.168.1.20',
              validator: (value) =>
                  (value ?? '').trim().isEmpty ? s.hostnameRequired : null,
              onFieldSubmitted: (_) => onSubmit(),
            ),
          ),
        ],
      ),
    );
  }
}
