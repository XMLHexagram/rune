import 'package:player/messages/mix.pbserver.dart';

Future<List<MixWithoutCoverIds>> getAllMixes() async {
  final fetchRequest = FetchAllMixesRequest();
  fetchRequest.sendSignalToRust(); // GENERATED

  // Listen for the response from Rust
  final rustSignal = await FetchAllMixesResponse.rustSignalStream.first;
  final response = rustSignal.message;

  return response.mixes;
}
