import 'package:flutter/foundation.dart';
import 'package:rinf/rinf.dart';

import 'request_failure.dart';
import '../../bindings/bindings.dart';

// These ids match replies to calls; they do not route requests.
var _nextCallId = 0;

/// Asks which of the given interned ids are not in the open library.
Future<FetchLibraryMembershipResponse> fetchLibraryMembership({
  List<int> trackIds = const [],
  List<int> albumIds = const [],
  List<int> artistIds = const [],
}) => _fetchLibraryMembership(
  trackIds,
  albumIds,
  artistIds,
  responses: FetchLibraryMembershipResponse.rustSignalStream,
  sendRequest: (request) => request.sendSignalToRust(),
);

@visibleForTesting
Future<FetchLibraryMembershipResponse> fetchLibraryMembershipWithStreams({
  List<int> trackIds = const [],
  List<int> albumIds = const [],
  List<int> artistIds = const [],
  required Stream<RustSignalPack<FetchLibraryMembershipResponse>> responses,
  required Stream<RequestFailedResponse> failures,
  required void Function(FetchLibraryMembershipRequest) sendRequest,
}) => _fetchLibraryMembership(
  trackIds,
  albumIds,
  artistIds,
  responses: responses,
  failures: failures,
  sendRequest: sendRequest,
);

Future<FetchLibraryMembershipResponse> _fetchLibraryMembership(
  List<int> trackIds,
  List<int> albumIds,
  List<int> artistIds, {
  required Stream<RustSignalPack<FetchLibraryMembershipResponse>> responses,
  Stream<RequestFailedResponse>? failures,
  required void Function(FetchLibraryMembershipRequest) sendRequest,
}) async {
  final callId = ++_nextCallId;
  return (await awaitResponse(
    'FetchLibraryMembershipRequest',
    responses,
    failures: failures,
    callId: callId,
    responseCallId: (signal) => signal.message.callId?.toInt(),
    send: () => sendRequest(
      FetchLibraryMembershipRequest(
        callId: Uint64.fromBigInt(BigInt.from(callId)),
        trackIds: trackIds,
        albumIds: albumIds,
        artistIds: artistIds,
      ),
    ),
  )).message;
}
