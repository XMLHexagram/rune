//! Apple's catalog beside the library: searched and browsed, its items added to the
//! library, and the user's recommendations.

use std::collections::HashSet;
use std::sync::Arc;

use futures_util::future::{join, try_join};
use futures_util::{stream, StreamExt, TryStreamExt};

use super::types::{
    Additions, AlbumItem, Candidates, CatalogAlbumDetail, CatalogArtistDetail,
    CatalogPlaylistDetail, ItemId, Membership, RecommendationGroup, SearchResults, SearchScope,
    SongList, Track, CATALOG_PREFIX,
};
use super::{map, AppleLibrary, Failure, LibraryError, Missing, Outcome, PARALLEL_READS};
use crate::error::Error;
use crate::models::{
    self, CatalogSearchResults, CatalogSearchType, LibraryAlbum, LibraryArtist, LibraryPlaylist,
    LibrarySearchResults, LibrarySearchType, LibrarySong, Song,
};
use crate::{CATALOG_SEARCH_LIMIT, LIBRARY_SEARCH_LIMIT};

/// Recommendation groups [`AppleLibrary::recommendations`] is usually asked for.
pub const DEFAULT_RECOMMENDATIONS: usize = 10;

/// The most results of each kind a search lists from the library, and from the catalog.
const SEARCH_LIMIT: usize = 25;
const _: () = assert!(CATALOG_SEARCH_LIMIT == 25 && LIBRARY_SEARCH_LIMIT == 25);

/// What the library knows to be its own, to tell catalog items apart from it. Albums match
/// by catalog id when the library names one, and by name and artist only when it names
/// none; artists match by name, as library artists name no catalog id.
///
/// Two readings are made: the library's own copies (what a catalog item that duplicates
/// is left out for), and everything known, this sign-in's additions included, which marks
/// what is in the library.
#[derive(Default)]
struct Marks {
    songs: Option<Arc<SongList>>,
    found_songs: HashSet<String>,
    /// Catalog ids of the library's albums.
    album_ids: HashSet<String>,
    /// Catalog ids of albums added here, or listed under their catalog ids until Apple
    /// lists their library copies.
    added_album_ids: HashSet<String>,
    /// Names of library albums that name no catalog id.
    album_names: HashSet<(String, String)>,
    found_artists: HashSet<String>,
    artists: Option<Arc<HashSet<String>>>,
    added_songs: HashSet<String>,
    added_playlists: HashSet<String>,
}

impl Marks {
    fn add_found(&mut self, found: &LibraryFound) {
        self.found_songs.extend(
            found
                .songs
                .iter()
                .filter_map(|song| song.store_id())
                .map(str::to_owned),
        );
        for album in &found.albums {
            let attributes = album.attributes.as_ref();
            self.add_library_album(
                album.catalog_id(),
                attributes
                    .and_then(|attributes| attributes.name.as_deref())
                    .unwrap_or_default(),
                attributes.and_then(|attributes| attributes.artist_name.as_deref()),
            );
        }
        for artist in &found.artists {
            let name = artist
                .attributes
                .as_ref()
                .and_then(|attributes| attributes.name.as_deref())
                .unwrap_or_default();
            self.found_artists.extend(name_of(name));
        }
    }

    fn add_library_album(&mut self, catalog_id: Option<&str>, name: &str, artist: Option<&str>) {
        match catalog_id {
            Some(id) => {
                self.album_ids.insert(id.to_owned());
            }
            None => self.album_names.extend(album_key(name, artist)),
        }
    }

    fn song_copy(&self, catalog_id: &str) -> bool {
        self.found_songs.contains(catalog_id)
            || self
                .songs
                .as_ref()
                .is_some_and(|list| list.has_library_copy(catalog_id))
    }

    fn song(&self, catalog_id: &str) -> bool {
        self.song_copy(catalog_id) || self.added_songs.contains(catalog_id)
    }

    fn album_copy(&self, item: &AlbumItem) -> bool {
        item.id
            .catalog_id()
            .is_some_and(|id| self.album_ids.contains(id))
            || self.album_named(item)
    }

    fn album(&self, item: &AlbumItem) -> bool {
        self.album_copy(item)
            || item
                .id
                .catalog_id()
                .is_some_and(|id| self.added_album_ids.contains(id))
    }

    fn album_named(&self, item: &AlbumItem) -> bool {
        album_key(&item.name, item.artist.as_deref())
            .is_some_and(|key| self.album_names.contains(&key))
    }

    fn artist(&self, name: &str) -> bool {
        name_of(name).is_some_and(|key| {
            self.found_artists.contains(&key)
                || self
                    .artists
                    .as_ref()
                    .is_some_and(|names| names.contains(&key))
        })
    }

    fn playlist(&self, catalog_id: &str) -> bool {
        self.added_playlists.contains(catalog_id)
    }
}

/// A name as it is compared, `None` when it is empty and so names nothing.
fn name_of(name: &str) -> Option<String> {
    let key = map::name_key(name);
    (!key.is_empty()).then_some(key)
}

fn album_key(name: &str, artist: Option<&str>) -> Option<(String, String)> {
    Some((name_of(name)?, map::name_key(artist.unwrap_or_default())))
}

/// What a library search found, before mapping.
#[derive(Default)]
struct LibraryFound {
    songs: Vec<LibrarySong>,
    albums: Vec<LibraryAlbum>,
    artists: Vec<LibraryArtist>,
    playlists: Vec<LibraryPlaylist>,
}

/// What a catalog search found, before mapping.
#[derive(Default)]
struct CatalogFound {
    songs: Vec<Song>,
    albums: Vec<models::Album>,
    artists: Vec<models::Artist>,
}

/// The term Apple is asked to search for. Apple's search has no wildcards, so a trailing
/// `*`, as a search box may append, is left out, and so are the quotes of an empty search.
fn search_term(query: &str) -> &str {
    let term = query.trim().trim_end_matches('*').trim_end();
    if term == "\"\"" {
        ""
    } else {
        term
    }
}

/// The catalog id of `item`; a library item names nothing in the catalog.
fn catalog_of(item: &ItemId, what: Missing) -> std::result::Result<&str, LibraryError> {
    item.catalog_id().ok_or(LibraryError::NotFound(what))
}

impl AppleLibrary {
    /// What the library knows to be its own, read from what is kept, and from `found`.
    /// Nothing is read from Apple for it.
    fn library_marks(&self, found: Option<&LibraryFound>) -> Marks {
        let mut marks = Marks {
            songs: self.caches.songs.peek(),
            artists: self.caches.artists.peek(),
            ..Marks::default()
        };
        if let Some(list) = self.caches.albums.peek() {
            for album in list.iter() {
                match album.id.strip_prefix(CATALOG_PREFIX) {
                    Some(id) => {
                        marks.added_album_ids.insert(id.to_owned());
                    }
                    None => marks.add_library_album(
                        album.catalog_id.as_deref(),
                        &album.name,
                        album.artist.as_deref(),
                    ),
                }
            }
        }
        if let Some(found) = found {
            marks.add_found(found);
        }
        let added = self.caches.added();
        marks.added_songs.extend(added.songs.iter().cloned());
        marks.added_album_ids.extend(added.albums.iter().cloned());
        marks
            .added_playlists
            .extend(added.playlists.iter().cloned());
        let album_tracks = self.caches.album_tracks();
        for album in &added.albums {
            if let Some(tracks) = album_tracks.get(album) {
                marks.added_songs.extend(tracks.iter().cloned());
            }
        }
        marks
    }

    /// Whether the catalog song was added to the library during this sign-in, on its own
    /// or with an album.
    pub(super) fn added_song(&self, catalog_id: &str) -> bool {
        let added = self.caches.added();
        let album_tracks = self.caches.album_tracks();
        added.songs.contains(catalog_id)
            || added.albums.iter().any(|album| {
                album_tracks
                    .get(album)
                    .is_some_and(|tracks| tracks.iter().any(|id| id == catalog_id))
            })
    }

    /// The names of the library's artists, kept for ten minutes. Best effort: `None` when
    /// they cannot be read, so artists read as not in the library.
    async fn artist_names(&self) -> Option<Arc<HashSet<String>>> {
        self.caches
            .artists
            .get(self.clock.now(), super::cache::TTL, || async {
                let artists: Vec<LibraryArtist> =
                    self.client.library_artists().try_collect().await?;
                Ok::<_, Failure>(
                    artists
                        .iter()
                        .filter_map(|artist| name_of(&map::artist(artist).name))
                        .collect(),
                )
            })
            .await
            .ok()
    }

    // Search ---------------------------------------------------------------

    /// Searches for songs, albums, artists and playlists. `Library` lists the library's
    /// matches; `Catalog` lists the catalog's, albums and artists as well as songs, and
    /// marks which of them the library holds; `All` lists the library's matches first, then
    /// the catalog's that duplicate none of the library's own copies. Catalog items are
    /// marked against everything known of the library, so one added during this sign-in
    /// stays listed, in the library, until Apple's library search lists its copy.
    ///
    /// The library's part and the catalog's part are each capped at 25 of every kind before
    /// they are merged, so many library matches never hide the catalog's. There is no paging
    /// beyond the cap in this version. Catalog playlists are not searched: Apple's search
    /// does not offer them, so the playlists are the library's. With `All`, a failing
    /// catalog search leaves the library's matches, which are what the search is mostly for;
    /// with `Catalog` it fails the search. An empty query finds nothing:
    /// [`songs`](Self::songs) lists everything. The catalog is read with the developer
    /// token only.
    pub async fn search(&self, query: &str, scope: SearchScope) -> super::Result<SearchResults> {
        // Apple's 404 answers a search with nothing and never reaches the caller, so the
        // kind named here only stands in.
        self.signed_in_read(Missing::Song, || self.search_once(query, scope))
            .await
    }

    async fn search_once(&self, query: &str, scope: SearchScope) -> Outcome<SearchResults> {
        let term = search_term(query);
        if term.is_empty() {
            return Ok(SearchResults::default());
        }
        let (library, catalog) = join(self.library_search(term), async {
            if scope == SearchScope::Library {
                return Ok(CatalogFound::default());
            }
            self.catalog_search(term).await
        })
        .await;
        let library = library?;
        let catalog = match catalog {
            Ok(catalog) => catalog,
            Err(_) if scope == SearchScope::All => CatalogFound::default(),
            Err(failure) => return Err(failure),
        };
        let marks = self.library_marks(Some(&library));

        let mut results = SearchResults::default();
        if scope != SearchScope::Catalog {
            results.songs = library
                .songs
                .iter()
                .filter_map(map::library_track)
                .take(SEARCH_LIMIT)
                .collect();
            results.albums = library
                .albums
                .iter()
                .map(map::library_album_item)
                .take(SEARCH_LIMIT)
                .collect();
            results.artists = library
                .artists
                .iter()
                .map(map::library_artist_item)
                .take(SEARCH_LIMIT)
                .collect();
            results.playlists = library
                .playlists
                .iter()
                .map(map::library_playlist_item)
                .take(SEARCH_LIMIT)
                .collect();
        }
        let dropping = scope == SearchScope::All;
        results.songs.extend(
            catalog
                .songs
                .iter()
                .take(SEARCH_LIMIT)
                .filter(|song| !dropping || !marks.song_copy(&song.id))
                .filter_map(map::catalog_track)
                .map(|mut song| {
                    song.in_library = catalog_song_id(&song).is_some_and(|id| marks.song(id));
                    song
                }),
        );
        results.albums.extend(
            catalog
                .albums
                .iter()
                .take(SEARCH_LIMIT)
                .map(map::catalog_album_item)
                .filter(|album| !dropping || !marks.album_copy(album))
                .map(|mut album| {
                    album.in_library = marks.album(&album);
                    album
                }),
        );
        results.artists.extend(
            catalog
                .artists
                .iter()
                .take(SEARCH_LIMIT)
                .map(map::catalog_artist_item)
                .filter(|artist| !dropping || !marks.artist(&artist.name))
                .map(|mut artist| {
                    artist.in_library = marks.artist(&artist.name);
                    artist
                }),
        );
        Ok(results)
    }

    async fn library_search(&self, term: &str) -> Outcome<LibraryFound> {
        let types = [
            LibrarySearchType::Songs,
            LibrarySearchType::Albums,
            LibrarySearchType::Artists,
            LibrarySearchType::Playlists,
        ];
        let found = match self
            .client
            .search_library(term, &types, LIBRARY_SEARCH_LIMIT, 0)
            .await
        {
            Ok(found) => found,
            Err(Error::NotFound(_)) => LibrarySearchResults::default(),
            Err(error) => return Err(error.into()),
        };
        Ok(LibraryFound {
            songs: found.songs.data,
            albums: found.albums.data,
            artists: found.artists.data,
            playlists: found.playlists.data,
        })
    }

    async fn catalog_search(&self, term: &str) -> Outcome<CatalogFound> {
        let types = [
            CatalogSearchType::Songs,
            CatalogSearchType::Albums,
            CatalogSearchType::Artists,
        ];
        let found = match self
            .client
            .search_catalog(term, &types, CATALOG_SEARCH_LIMIT, 0)
            .await
        {
            Ok(found) => found,
            Err(Error::NotFound(_)) => CatalogSearchResults::default(),
            Err(error) => return Err(error.into()),
        };
        Ok(CatalogFound {
            songs: found.songs.data,
            albums: found.albums.data,
            artists: found.artists.data,
        })
    }

    // Membership -----------------------------------------------------------

    /// Which of `items` are in the library. A library item is, by definition. A catalog
    /// song is when the library has a copy of it or it was added during this sign-in, a
    /// catalog album when the library's album names its catalog id, or names none and has
    /// the same name and artist, or it was added, a catalog artist when the library has an
    /// artist of that name, a catalog playlist only when it was added here. Albums and
    /// artists are matched by the names the items carry; an item with no name matches
    /// nothing.
    ///
    /// Unlike the marks on a search, this reads the song list and the album list when
    /// catalog songs or albums are asked about and nothing is kept yet, and fails when it
    /// cannot. Artists are best effort, here and in [`catalog_artist`](Self::catalog_artist):
    /// the library's artists are read once and kept for ten minutes, and when they cannot be
    /// read no catalog artist reads as in the library.
    pub async fn membership(&self, items: Candidates<'_>) -> super::Result<Membership> {
        // Apple's 404 is absorbed or never asked for, so the kind named here only stands in.
        self.signed_in_read(Missing::Song, || async {
            if items.songs.iter().any(|key| !key.is_library()) {
                self.song_list().await?;
            }
            if items.albums.iter().any(|album| album.id.is_catalog()) {
                self.album_list().await?;
            }
            if items.artists.iter().any(|artist| artist.id.is_catalog()) {
                self.artist_names().await;
            }
            let marks = self.library_marks(None);
            let mut found = Membership::default();
            found.songs.extend(
                items
                    .songs
                    .iter()
                    .filter(|key| key.catalog_id().is_none_or(|id| marks.song(id)))
                    .cloned(),
            );
            found.albums.extend(
                items
                    .albums
                    .iter()
                    .filter(|album| !album.id.is_catalog() || marks.album(album))
                    .map(|album| album.id.clone()),
            );
            found.artists.extend(
                items
                    .artists
                    .iter()
                    .filter(|artist| !artist.id.is_catalog() || marks.artist(&artist.name))
                    .map(|artist| artist.id.clone()),
            );
            found.playlists.extend(
                items
                    .playlists
                    .iter()
                    .filter(|playlist| playlist.id.catalog_id().is_none_or(|id| marks.playlist(id)))
                    .map(|playlist| playlist.id.clone()),
            );
            Ok(found)
        })
        .await
    }

    // Browsing -------------------------------------------------------------

    /// A catalog album with its songs, kept for 30 seconds. Its album and each of its songs
    /// are marked by their own ids, as [`membership`](Self::membership) does. A library
    /// item names nothing in the catalog and is not found. Read with the developer token
    /// only.
    pub async fn catalog_album(&self, id: &ItemId) -> super::Result<CatalogAlbumDetail> {
        self.signed_in_read(Missing::Album, || async {
            let id = catalog_of(id, Missing::Album)?;
            let session = self.caches.session();
            let now = self.clock.now();
            let detail = match self.caches.album_details.fresh(id, session, now) {
                Some(detail) => detail,
                None => {
                    let detail = Arc::new(self.read_catalog_album(id).await?);
                    self.caches
                        .keep_detail(session, |c| &c.album_details, id, now, detail.clone());
                    detail
                }
            };
            let marks = self.library_marks(None);
            let mut detail = CatalogAlbumDetail::clone(&detail);
            detail.album.in_library = marks.album(&detail.album);
            for track in &mut detail.tracks {
                track.in_library = catalog_song_id(track).is_some_and(|id| marks.song(id));
            }
            Ok(detail)
        })
        .await
    }

    async fn read_catalog_album(&self, id: &str) -> Outcome<CatalogAlbumDetail> {
        let (album, tracks) = try_join(
            async { Ok(self.client.catalog_album(id).await?) },
            self.catalog_tracks(id, false),
        )
        .await?;
        let album = map::catalog_album_item(&album);
        let tracks = tracks
            .iter()
            .filter_map(map::catalog_track)
            .map(|mut song| {
                song.album_id = Some(id.to_owned());
                if song.artist == album.artist {
                    song.artist_id = album.artist_id.as_ref().map(|id| id.as_str().to_owned());
                }
                if song.artwork.is_none() {
                    song.artwork.clone_from(&album.artwork);
                }
                song
            })
            .collect();
        Ok(CatalogAlbumDetail { album, tracks })
    }

    /// A catalog playlist with its songs, kept for 30 seconds. A personal mix (`pl.pm-...`)
    /// is read with the user token, any other playlist with the developer token only.
    pub async fn catalog_playlist(&self, id: &ItemId) -> super::Result<CatalogPlaylistDetail> {
        self.signed_in_read(Missing::Playlist, || async {
            let id = catalog_of(id, Missing::Playlist)?;
            let session = self.caches.session();
            let now = self.clock.now();
            let detail = match self.caches.playlist_details.fresh(id, session, now) {
                Some(detail) => detail,
                None => {
                    let (playlist, tracks) = try_join(
                        async { Ok(self.client.catalog_playlist(id).await?) },
                        self.catalog_tracks(id, true),
                    )
                    .await?;
                    let detail = Arc::new(CatalogPlaylistDetail {
                        playlist: map::catalog_playlist_item(&playlist),
                        tracks: tracks.iter().filter_map(map::catalog_track).collect(),
                    });
                    self.caches.keep_detail(
                        session,
                        |c| &c.playlist_details,
                        id,
                        now,
                        detail.clone(),
                    );
                    detail
                }
            };
            let marks = self.library_marks(None);
            let mut detail = CatalogPlaylistDetail::clone(&detail);
            detail.playlist.in_library = marks.playlist(id);
            for track in &mut detail.tracks {
                track.in_library = catalog_song_id(track).is_some_and(|id| marks.song(id));
            }
            Ok(detail)
        })
        .await
    }

    /// A catalog artist with their catalog albums, kept for 30 seconds. Whether the artist
    /// is in the library is told by name, as [`membership`](Self::membership) does.
    pub async fn catalog_artist(&self, id: &ItemId) -> super::Result<CatalogArtistDetail> {
        self.signed_in_read(Missing::Artist, || async {
            let id = catalog_of(id, Missing::Artist)?;
            let session = self.caches.session();
            let now = self.clock.now();
            let detail = match self.caches.artist_details.fresh(id, session, now) {
                Some(detail) => detail,
                None => {
                    let (artist, albums) = try_join(
                        self.client.catalog_artist(id),
                        self.client.catalog_artist_albums(id),
                    )
                    .await?;
                    let artist = map::catalog_artist_item(&artist);
                    let albums = albums
                        .iter()
                        .map(|album| {
                            let mut album = map::catalog_album_item(album);
                            album.artist_id.get_or_insert_with(|| artist.id.clone());
                            album.artist.get_or_insert_with(|| artist.name.clone());
                            album
                        })
                        .collect();
                    let detail = Arc::new(CatalogArtistDetail { artist, albums });
                    self.caches.keep_detail(
                        session,
                        |c| &c.artist_details,
                        id,
                        now,
                        detail.clone(),
                    );
                    detail
                }
            };
            self.artist_names().await;
            let marks = self.library_marks(None);
            let mut detail = CatalogArtistDetail::clone(&detail);
            detail.artist.in_library = marks.artist(&detail.artist.name);
            for album in &mut detail.albums {
                album.in_library = marks.album(album);
            }
            Ok(detail)
        })
        .await
    }

    /// The songs of a catalog album or playlist; none when Apple says it has no tracks.
    async fn catalog_tracks(&self, id: &str, playlist: bool) -> Outcome<Vec<Song>> {
        let tracks = if playlist {
            self.client.catalog_playlist_tracks(id).await
        } else {
            self.client.catalog_album_tracks(id).await
        };
        match tracks {
            Ok(tracks) => Ok(tracks),
            Err(Error::NotFound(_)) => Ok(Vec::new()),
            Err(error) => Err(error.into()),
        }
    }

    // Library additions ----------------------------------------------------

    /// Adds catalog songs, albums and playlists to the library in one request, and tells
    /// how many were sent. Library songs, albums and playlists are in it already and are
    /// skipped. Afterwards the added songs, and the songs of the added albums, count as in
    /// the library, and join the kept song list; added albums head the kept album list,
    /// listed under `catalog:<id>` until Apple lists their library copies.
    pub async fn add_to_library(&self, additions: Additions<'_>) -> super::Result<usize> {
        self.signed_in(Missing::Song, || async {
            let songs: Vec<String> = additions
                .songs
                .iter()
                .filter_map(|key| key.catalog_id().map(str::to_owned))
                .collect();
            let catalog = |items: &[ItemId]| -> Vec<String> {
                items
                    .iter()
                    .filter_map(|item| item.catalog_id().map(str::to_owned))
                    .collect()
            };
            let (albums, playlists) = (catalog(additions.albums), catalog(additions.playlists));
            let sent = songs.len() + albums.len() + playlists.len();
            if sent == 0 {
                return Ok(0);
            }
            let session = self.caches.session();
            self.client
                .add_catalog_items_to_library(&songs, &albums, &playlists)
                .await?;
            self.caches.remember_added(session, |added| {
                added.songs.extend(songs.iter().cloned());
                added.albums.extend(albums.iter().cloned());
                added.playlists.extend(playlists.iter().cloned());
            });
            self.patch_after_adding(session, &songs, &albums).await;
            Ok(sent)
        })
        .await
    }

    /// Puts added songs, and the songs of added albums, into the kept song list, and added
    /// albums first in the kept album list, so they show before Apple lists them. The songs
    /// of added albums are always read, so they count as in the library; an album Apple
    /// does not know is skipped. When what is kept cannot be patched, it is rebuilt on its
    /// next read instead. Nothing changes when a sign-in happened since `session`.
    async fn patch_after_adding(&self, session: u64, songs: &[String], albums: &[String]) {
        if self.caches.session() != session {
            return;
        }
        let songs_kept = self.caches.songs.peek().is_some();
        if !songs_kept {
            // A first build may be running, and may not see the addition: retire it.
            self.caches.songs.patch(|_| {});
        }
        let read = try_join(
            async {
                if songs_kept && !songs.is_empty() {
                    self.client
                        .catalog_songs(songs)
                        .await
                        .map_err(Failure::from)
                } else {
                    Ok(Vec::new())
                }
            },
            // Owned ids: closures taking borrowed items here fail the compiler's Send check.
            stream::iter(albums.to_vec())
                .map(|id| async move {
                    let read = try_join(
                        async { Ok(self.client.catalog_album(&id).await?) },
                        self.catalog_tracks(&id, false),
                    )
                    .await;
                    match read {
                        Ok(read) => Ok(Some(read)),
                        Err(Failure::Apple(Error::NotFound(_))) => Ok(None),
                        Err(failure) => Err(failure),
                    }
                })
                .buffered(PARALLEL_READS)
                .try_collect::<Vec<_>>(),
        )
        .await;
        if self.caches.session() != session {
            return;
        }
        let Ok((songs, albums)) = read else {
            self.caches.songs.invalidate();
            self.caches.albums.invalidate();
            return;
        };
        let albums: Vec<_> = albums.into_iter().flatten().collect();
        let mut tracks: Vec<Track> = songs.iter().filter_map(map::catalog_track).collect();
        for (album, songs) in &albums {
            let mut album_tracks: Vec<Track> =
                songs.iter().filter_map(map::catalog_track).collect();
            self.caches.remember_album_tracks(
                session,
                &album.id,
                album_tracks
                    .iter()
                    .filter_map(|track| catalog_song_id(track).map(str::to_owned))
                    .collect(),
            );
            for song in &mut album_tracks {
                song.album_id = Some(album.id.clone());
            }
            tracks.extend(album_tracks);
        }
        // Their catalog ids are not recorded as the library's: until Apple lists their
        // library copies, nothing duplicates them.
        self.caches.songs.patch(|list| {
            for song in tracks {
                list.push_in_library(song);
            }
        });
        self.caches.albums.patch(|list| {
            for (album, _) in albums.iter().rev() {
                let added = map::added_album(album);
                if list
                    .iter()
                    .any(|known| known.catalog_id == added.catalog_id)
                {
                    continue;
                }
                list.insert(0, added);
            }
        });
    }

    // Recommendations ------------------------------------------------------

    /// Up to `limit` groups of the user's recommendations (see
    /// [`DEFAULT_RECOMMENDATIONS`]), in Apple's order, with their catalog playlists and
    /// albums. Stations and groups left with nothing else are dropped. The groups are kept
    /// for ten minutes; the items are marked against the library on every read, so
    /// additions show at once. Read with the user token.
    pub async fn recommendations(&self, limit: usize) -> super::Result<Vec<RecommendationGroup>> {
        // Apple's 404 is absorbed, so the kind named here only stands in.
        self.signed_in_read(Missing::Playlist, || async {
            let groups = self
                .caches
                .recommendations
                .get(self.clock.now(), super::cache::TTL, || {
                    self.build_recommendations()
                })
                .await?;
            let marks = self.library_marks(None);
            Ok(groups
                .iter()
                .take(limit)
                .cloned()
                .map(|mut group| {
                    for album in &mut group.albums {
                        album.in_library = album.in_library || marks.album(album);
                    }
                    for playlist in &mut group.playlists {
                        playlist.in_library = playlist.in_library
                            || playlist
                                .id
                                .catalog_id()
                                .is_some_and(|id| marks.playlist(id));
                    }
                    group
                })
                .collect())
        })
        .await
    }

    async fn build_recommendations(&self) -> Outcome<Vec<RecommendationGroup>> {
        let recommendations = match self.client.recommendations().await {
            Ok(recommendations) => recommendations,
            Err(Error::NotFound(_)) => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(recommendations
            .iter()
            .filter_map(map::recommendation_group)
            .collect())
    }
}

/// The catalog id of a catalog song.
fn catalog_song_id(track: &Track) -> Option<&str> {
    track.key.catalog_id()
}
