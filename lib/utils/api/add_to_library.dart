import 'package:flutter/foundation.dart';
import 'package:rinf/rinf.dart';

import 'request_failure.dart';
import '../../bindings/bindings.dart';

// These ids match replies to calls; they do not route requests.
var _nextCallId = 0;

/// Adds catalogue tracks and albums to the open library.
///
/// Throws a [RequestFailure] when the hub reports a failure or answers that
/// nothing was added.
Future<void> addToLibrary({
  List<int> trackIds = const [],
  List<int> albumIds = const [],
}) => _addToLibrary(
  trackIds,
  albumIds,
  responses: AddToLibraryResponse.rustSignalStream,
  sendRequest: (request) => request.sendSignalToRust(),
);

@visibleForTesting
Future<void> addToLibraryWithStreams({
  List<int> trackIds = const [],
  List<int> albumIds = const [],
  required Stream<RustSignalPack<AddToLibraryResponse>> responses,
  required Stream<RequestFailedResponse> failures,
  required void Function(AddToLibraryRequest) sendRequest,
}) => _addToLibrary(
  trackIds,
  albumIds,
  responses: responses,
  failures: failures,
  sendRequest: sendRequest,
);

Future<void> _addToLibrary(
  List<int> trackIds,
  List<int> albumIds, {
  required Stream<RustSignalPack<AddToLibraryResponse>> responses,
  Stream<RequestFailedResponse>? failures,
  required void Function(AddToLibraryRequest) sendRequest,
}) async {
  final callId = ++_nextCallId;
  final response = (await awaitResponse(
    'AddToLibraryRequest',
    responses,
    failures: failures,
    callId: callId,
    responseCallId: (signal) => signal.message.callId?.toInt(),
    send: () => sendRequest(
      AddToLibraryRequest(
        callId: Uint64.fromBigInt(BigInt.from(callId)),
        trackIds: trackIds,
        albumIds: albumIds,
      ),
    ),
  )).message;
  if (!response.success) {
    throw const RequestFailure(
      'AddToLibraryRequest',
      RequestErrorKind.internal,
      'Adding to the library failed',
    );
  }
}
