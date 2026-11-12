import 'package:flutter/foundation.dart';

import '../../../bindings/bindings.dart';
import '../../../utils/api/fetch_library_membership.dart';
import '../../../utils/rune_log.dart';

typedef MembershipFetcher =
    Future<FetchLibraryMembershipResponse> Function({
      List<int> trackIds,
      List<int> albumIds,
      List<int> artistIds,
    });

/// Which of the shown search results are not in the open library.
class SearchMembership extends ChangeNotifier {
  SearchMembership({MembershipFetcher fetch = fetchLibraryMembership})
    : _fetch = fetch;

  final MembershipFetcher _fetch;
  Set<int> _tracks = const {};
  Set<int> _albums = const {};
  Set<int> _artists = const {};
  int _generation = 0;
  bool _disposed = false;

  bool isOutside(CollectionType type, int id) => switch (type) {
    CollectionType.track => _tracks.contains(id),
    CollectionType.album => _albums.contains(id),
    CollectionType.artist => _artists.contains(id),
    _ => false,
  };

  /// Drops answers that are still on their way, for example when a newer
  /// search starts.
  void invalidate() => _generation++;

  /// Forgets every answer and drops the ones still on their way.
  void reset() {
    invalidate();
    _replace(const {}, const {}, const {});
  }

  /// Applies a new result set. When the open library cannot answer, nothing is
  /// asked and nothing is marked.
  Future<void> updateForResults({
    required bool available,
    List<int> trackIds = const [],
    List<int> albumIds = const [],
    List<int> artistIds = const [],
  }) {
    if (!available) {
      reset();
      return Future.value();
    }
    return refresh(
      trackIds: trackIds,
      albumIds: albumIds,
      artistIds: artistIds,
    );
  }

  /// Asks for the membership of the shown results. An answer that arrives
  /// after a newer [refresh], [invalidate] or [reset] is ignored.
  Future<void> refresh({
    List<int> trackIds = const [],
    List<int> albumIds = const [],
    List<int> artistIds = const [],
  }) async {
    final generation = ++_generation;
    _replace(const {}, const {}, const {});
    if (trackIds.isEmpty && albumIds.isEmpty && artistIds.isEmpty) return;

    final FetchLibraryMembershipResponse answer;
    try {
      answer = await _fetch(
        trackIds: trackIds,
        albumIds: albumIds,
        artistIds: artistIds,
      );
    } catch (e) {
      // Without an answer nothing is marked.
      error$('Library membership request failed: $e');
      return;
    }
    if (_disposed || generation != _generation) return;
    _replace(
      answer.outsideTrackIds.toSet(),
      answer.outsideAlbumIds.toSet(),
      answer.outsideArtistIds.toSet(),
    );
  }

  /// Called once an item has been added to the library.
  void markAdded(CollectionType type, int id) {
    if (_disposed) return;
    final set = switch (type) {
      CollectionType.track => _tracks,
      CollectionType.album => _albums,
      CollectionType.artist => _artists,
      _ => null,
    };
    if (set == null || !set.contains(id)) return;
    final next = {...set}..remove(id);
    switch (type) {
      case CollectionType.track:
        _tracks = next;
      case CollectionType.album:
        _albums = next;
      default:
        _artists = next;
    }
    notifyListeners();
  }

  void _replace(Set<int> tracks, Set<int> albums, Set<int> artists) {
    if (_disposed) return;
    final changed =
        _tracks.isNotEmpty ||
        _albums.isNotEmpty ||
        _artists.isNotEmpty ||
        tracks.isNotEmpty ||
        albums.isNotEmpty ||
        artists.isNotEmpty;
    _tracks = tracks;
    _albums = albums;
    _artists = artists;
    if (changed) notifyListeners();
  }

  @override
  void dispose() {
    _disposed = true;
    super.dispose();
  }
}
