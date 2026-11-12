import 'package:fluent_ui/fluent_ui.dart';

import '../../../bindings/bindings.dart';
import '../../../providers/request_capabilities.dart';

bool userGenerated(CollectionType collectionType) {
  return collectionType == CollectionType.playlist ||
      collectionType == CollectionType.mix;
}

/// Whether the user can create an item of this type in the open library.
///
/// A library that serves its mixes itself, such as a service library, does not
/// let the user create them, so its empty page should not suggest it.
bool canCreateCollection(BuildContext context, CollectionType collectionType) {
  if (collectionType != CollectionType.mix) return true;
  return capabilityForRequest(
        context,
        (provider) => provider.createMixRequest,
        listen: false,
      )
      is RequestCapabilityAvailable;
}
