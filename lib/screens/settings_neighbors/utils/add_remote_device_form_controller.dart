import 'package:fluent_ui/fluent_ui.dart';

class AddRemoteDeviceFormController {
  final TextEditingController hostnameController = TextEditingController();
  final GlobalKey<FormState> formKey = GlobalKey<FormState>();

  void dispose() {
    hostnameController.dispose();
  }
}
