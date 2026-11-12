//! Rune's `runeCatalog` and `runeRecommendations` extensions: Apple's catalog searched and
//! browsed beside the library, catalog items added to the library, and recommendations.

use std::collections::HashSet;
use std::time::Instant;

use futures_util::future::{join, try_join, try_join3, try_join_all};
use subsonic::{
    RuneRecommendationGroup, RuneScope, SearchLimits, SubsonicAlbum, SubsonicAlbumDetail,
    SubsonicArtistDetail, SubsonicError, SubsonicPlaylistDetail, SubsonicSearchResult,
    SubsonicSong,
};

use super::cache::TTL;
use super::map::{self, CatalogKind};
use super::{
    lists_everything, search_term, total_duration, AppleMusicServer, Failure, LibraryFound, Outcome,
};
use crate::error::Error;
use crate::models::{Album, Artist, CatalogSearchResults, CatalogSearchType, Page, Song};
use crate::CATALOG_SEARCH_LIMIT;

/// What the server knows to be in the user's library, to tell catalog items apart from it.
/// Albums whose library copy names its catalog id match by that id, the others by name and
/// artist; artists match by name, as library artists name no catalog id.
///
/// Two sets are made: one of the library's own copies (a search's library matches, and the
/// catalog ids the kept song union and album list carry from the library), which a catalog
/// item that duplicates one is left out for; and one of everything known, this sign-in's
/// additions included, which marks `runeInLibrary`.
#[derive(Default)]
pub(super) struct LibraryMarks {
    songs: HashSet<String>,
    albums: HashSet<String>,
    album_names: HashSet<(String, String)>,
    artist_names: HashSet<String>,
    playlists: HashSet<String>,
}

impl LibraryMarks {
    fn add_found(&mut self, found: &LibraryFound) {
        self.songs.extend(
            found
                .songs
                .iter()
                .filter_map(|song| song.store_id())
                .map(str::to_owned),
        );
        for album in &found.albums {
            let attributes = album.attributes.as_ref();
            self.add_album(
                album.catalog_id(),
                attributes
                    .and_then(|attributes| attributes.name.as_deref())
                    .unwrap_or_default(),
                attributes.and_then(|attributes| attributes.artist_name.as_deref()),
            );
        }
        self.artist_names.extend(found.artists.iter().map(|artist| {
            map::name_key(
                artist
                    .attributes
                    .as_ref()
                    .and_then(|attributes| attributes.name.as_deref())
                    .unwrap_or_default(),
            )
        }));
    }

    fn add_album(&mut self, catalog_id: Option<&str>, name: &str, artist: Option<&str>) {
        match catalog_id {
            Some(catalog_id) => {
                self.albums.insert(catalog_id.to_owned());
            }
            None => {
                self.album_names.insert(album_key(name, artist));
            }
        }
    }

    pub(super) fn song(&self, catalog_id: &str) -> bool {
        self.songs.contains(catalog_id)
    }

    fn album(&self, catalog_id: &str, name: &str, artist: Option<&str>) -> bool {
        self.albums.contains(catalog_id) || self.album_names.contains(&album_key(name, artist))
    }

    fn artist(&self, name: &str) -> bool {
        self.artist_names.contains(&map::name_key(name))
    }

    fn playlist(&self, catalog_id: &str) -> bool {
        self.playlists.contains(catalog_id)
    }
}

fn album_key(name: &str, artist: Option<&str>) -> (String, String) {
    (
        map::name_key(name),
        map::name_key(artist.unwrap_or_default()),
    )
}

/// What a catalog search found, before mapping.
#[derive(Default)]
struct CatalogFound {
    songs: Vec<Song>,
    albums: Vec<Album>,
    artists: Vec<Artist>,
}

fn take<T>(items: impl IntoIterator<Item = T>, skip: u32, count: u32) -> Vec<T> {
    items
        .into_iter()
        .skip(usize::try_from(skip).unwrap_or(usize::MAX))
        .take(usize::try_from(count).unwrap_or(usize::MAX))
        .collect()
}

impl AppleMusicServer {
    /// The library's own copies: the library items of a search, when given, and the catalog
    /// ids the kept song union and album list carry from the library. Nothing is read from
    /// Apple for it.
    fn library_copies(&self, found: Option<&LibraryFound>) -> LibraryMarks {
        let mut marks = LibraryMarks::default();
        if let Some(union) = self.caches.songs.peek() {
            marks
                .songs
                .extend(union.library_catalog_ids().iter().cloned());
        }
        if let Some(list) = self.caches.albums.peek() {
            marks
                .albums
                .extend(list.library_catalog_ids.iter().cloned());
        }
        if let Some(found) = found {
            marks.add_found(found);
        }
        marks
    }

    /// Everything known to be in the library: [`Self::library_copies`], the names of the
    /// kept album list, and the items added during this sign-in with the known songs of the
    /// albums among them.
    pub(super) fn library_marks(&self, found: Option<&LibraryFound>) -> LibraryMarks {
        let mut marks = self.library_copies(found);
        if let Some(list) = self.caches.albums.peek() {
            for album in &list.albums {
                marks.add_album(
                    map::catalog_part(&album.id, CatalogKind::Album),
                    &album.name,
                    album.artist.as_deref(),
                );
            }
        }
        let added = self.caches.added();
        marks.songs.extend(added.songs.iter().cloned());
        marks.albums.extend(added.albums.iter().cloned());
        marks.playlists.extend(added.playlists.iter().cloned());
        let album_tracks = self.caches.album_tracks();
        for album in &added.albums {
            if let Some(tracks) = album_tracks.get(album) {
                marks.songs.extend(tracks.iter().cloned());
            }
        }
        marks
    }

    // Search ---------------------------------------------------------------

    /// `search3` with a scope. `all` lists the library's matches first, then the catalog's
    /// matches that duplicate none of the library's own copies; `catalog` lists the catalog's
    /// matches only. Catalog items are marked against everything known of the library, so
    /// an item added during this sign-in stays listed, in the library, until Apple's library
    /// search lists its library copy.
    ///
    /// With `all`, a failing catalog search leaves the library's matches: they are what the
    /// search is mostly for. With `catalog`, it fails the search.
    pub(super) async fn search_in_scope(
        &self,
        query: &str,
        limits: SearchLimits,
        song_offset: u32,
        scope: RuneScope,
    ) -> Outcome<SubsonicSearchResult> {
        let term = search_term(query);
        if scope == RuneScope::Library || lists_everything(query) || term.is_empty() {
            // An empty search lists the library's songs; the catalog has no such listing.
            return match scope {
                RuneScope::Catalog => Ok(SubsonicSearchResult::default()),
                _ => self.search(query, limits, song_offset).await,
            };
        }
        // Both are read from their start, so that `song_offset` pages the merged songs.
        let wanted = SearchLimits {
            songs: limits.songs.saturating_add(song_offset),
            ..limits
        };
        let (library, catalog) = join(
            self.library_search(term, wanted, 0),
            self.catalog_search(term, wanted),
        )
        .await;
        let library = library?;
        let catalog = match catalog {
            Ok(catalog) => catalog,
            Err(failure) if scope == RuneScope::All => {
                log::warn!(
                    "Apple Music catalog search failed, listing the library only: {}",
                    describe(failure)
                );
                CatalogFound::default()
            }
            Err(failure) => return Err(failure),
        };
        let marks = self.library_marks(Some(&library));
        let copies = self.library_copies(Some(&library));
        let artworks = &self.caches.artworks;

        let catalog_songs = catalog.songs.iter().map(|song| {
            let mut mapped = map::catalog_song(song, artworks);
            mapped.rune_in_library = Some(marks.song(&song.id));
            mapped
        });
        let catalog_albums = catalog.albums.iter().map(|album| {
            let mut mapped = map::catalog_album(album, artworks);
            mapped.rune_in_library =
                Some(marks.album(&album.id, &mapped.name, mapped.artist.as_deref()));
            mapped
        });
        let catalog_artists = catalog.artists.iter().map(|artist| {
            let mut mapped = map::catalog_artist(artist, artworks);
            mapped.rune_in_library = Some(marks.artist(&mapped.name));
            mapped
        });
        let (songs, albums, artists) = match scope {
            RuneScope::All => {
                let library = self.map_library_found(&library);
                let mut songs = library.songs;
                songs.extend(
                    catalog_songs
                        .zip(&catalog.songs)
                        .filter(|(_, song)| !copies.song(&song.id))
                        .map(|(mapped, _)| mapped),
                );
                let mut albums = library.albums;
                albums.extend(
                    catalog_albums
                        .zip(&catalog.albums)
                        .filter(|(mapped, album)| {
                            !copies.album(&album.id, &mapped.name, mapped.artist.as_deref())
                        })
                        .map(|(mapped, _)| mapped),
                );
                let mut artists = library.artists;
                artists.extend(catalog_artists.filter(|artist| !copies.artist(&artist.name)));
                (songs, albums, artists)
            }
            RuneScope::Catalog | RuneScope::Library => (
                catalog_songs.collect(),
                catalog_albums.collect(),
                catalog_artists.collect(),
            ),
        };
        Ok(SubsonicSearchResult {
            artists: take(artists, 0, limits.artists),
            albums: take(albums, 0, limits.albums),
            songs: self
                .with_likes(take(songs, song_offset, limits.songs))
                .await,
        })
    }

    /// Up to the limits of each type from the catalog, from its start: one request for every
    /// type, then pages of [`CATALOG_SEARCH_LIMIT`] for the types that need more.
    async fn catalog_search(&self, term: &str, limits: SearchLimits) -> Outcome<CatalogFound> {
        let wanted: Vec<(CatalogSearchType, u32)> = [
            (CatalogSearchType::Songs, limits.songs),
            (CatalogSearchType::Albums, limits.albums),
            (CatalogSearchType::Artists, limits.artists),
        ]
        .into_iter()
        .filter(|(_, count)| *count > 0)
        .collect();
        let Some(largest) = wanted.iter().map(|(_, count)| *count).max() else {
            return Ok(CatalogFound::default());
        };
        let limit = largest.min(CATALOG_SEARCH_LIMIT);
        let types: Vec<CatalogSearchType> = wanted.iter().map(|(kind, _)| *kind).collect();
        let first = self.read_catalog_search(term, &types, limit, 0).await?;
        let (songs, albums, artists) = try_join3(
            self.catalog_search_rest(
                term,
                CatalogSearchType::Songs,
                limits.songs,
                (first.songs, limit),
                |results| results.songs,
            ),
            self.catalog_search_rest(
                term,
                CatalogSearchType::Albums,
                limits.albums,
                (first.albums, limit),
                |results| results.albums,
            ),
            self.catalog_search_rest(
                term,
                CatalogSearchType::Artists,
                limits.artists,
                (first.artists, limit),
                |results| results.artists,
            ),
        )
        .await?;
        Ok(CatalogFound {
            songs,
            albums,
            artists,
        })
    }

    /// `count` results of one type: the first page, read with `limit`, then further pages
    /// while Apple offers them.
    async fn catalog_search_rest<T>(
        &self,
        term: &str,
        kind: CatalogSearchType,
        count: u32,
        (first, limit): (Page<T>, u32),
        pick: fn(CatalogSearchResults) -> Page<T>,
    ) -> Outcome<Vec<T>> {
        let mut found = Vec::new();
        let mut page = first;
        let mut limit = limit;
        loop {
            let got = u32::try_from(page.data.len()).unwrap_or(u32::MAX);
            let more = page.next.is_some() && got >= limit;
            found.extend(page.data);
            let read = u32::try_from(found.len()).unwrap_or(u32::MAX);
            if read >= count || !more {
                break;
            }
            limit = (count - read).min(CATALOG_SEARCH_LIMIT);
            page = pick(self.read_catalog_search(term, &[kind], limit, read).await?);
        }
        found.truncate(usize::try_from(count).unwrap_or(usize::MAX));
        Ok(found)
    }

    async fn read_catalog_search(
        &self,
        term: &str,
        types: &[CatalogSearchType],
        limit: u32,
        offset: u32,
    ) -> Outcome<CatalogSearchResults> {
        match self.client.search_catalog(term, types, limit, offset).await {
            Ok(results) => Ok(results),
            Err(Error::NotFound(_)) => Ok(CatalogSearchResults::default()),
            Err(error) => Err(error.into()),
        }
    }

    // Browsing -------------------------------------------------------------

    /// A catalog album with its songs. The songs of an album in the library count as in it.
    pub(super) async fn catalog_album(&self, catalog_id: &str) -> Outcome<SubsonicAlbumDetail> {
        let session = self.caches.session();
        let (album, tracks) = try_join(
            async { Ok(self.client.catalog_album(catalog_id).await?) },
            self.catalog_tracks(catalog_id, CatalogKind::Album),
        )
        .await?;
        let artworks = &self.caches.artworks;
        self.caches.remember_album_tracks(
            session,
            catalog_id,
            tracks.iter().map(|track| track.id.clone()).collect(),
        );
        let marks = self.library_marks(None);
        let mut album = map::catalog_album(&album, artworks);
        let in_library = marks.album(catalog_id, &album.name, album.artist.as_deref());
        album.rune_in_library = Some(in_library);
        let songs: Vec<SubsonicSong> = tracks
            .iter()
            .map(|track| {
                let mut song = map::catalog_song(track, artworks);
                song.album_id = Some(album.id.clone());
                if song.artist == album.artist {
                    song.artist_id.clone_from(&album.artist_id);
                }
                if song.cover_art.is_none() {
                    song.cover_art.clone_from(&album.cover_art);
                }
                song.rune_in_library = Some(in_library || marks.song(&track.id));
                song
            })
            .collect();
        let songs = self.with_likes(songs).await;
        Ok(SubsonicAlbumDetail {
            id: album.id,
            name: album.name,
            artist: album.artist,
            artist_id: album.artist_id,
            song_count: u32::try_from(songs.len()).ok(),
            duration: total_duration(&songs),
            year: album.year,
            genre: album.genre,
            cover_art: album.cover_art,
            rune_in_library: album.rune_in_library,
            songs,
        })
    }

    /// A catalog artist with its catalog albums.
    pub(super) async fn catalog_artist(&self, catalog_id: &str) -> Outcome<SubsonicArtistDetail> {
        let (artist, albums) = try_join(
            self.client.catalog_artist(catalog_id),
            self.client.catalog_artist_albums(catalog_id),
        )
        .await?;
        let artworks = &self.caches.artworks;
        let marks = self.library_marks(None);
        let mut artist = map::catalog_artist(&artist, artworks);
        artist.rune_in_library = Some(marks.artist(&artist.name));
        let albums: Vec<SubsonicAlbum> = albums
            .iter()
            .map(|album| {
                let mut mapped = map::catalog_album(album, artworks);
                mapped.artist_id.get_or_insert_with(|| artist.id.clone());
                mapped.artist.get_or_insert_with(|| artist.name.clone());
                mapped.rune_in_library =
                    Some(marks.album(&album.id, &mapped.name, mapped.artist.as_deref()));
                mapped
            })
            .collect();
        Ok(SubsonicArtistDetail {
            album_count: u32::try_from(albums.len()).ok(),
            cover_art: artist
                .cover_art
                .clone()
                .or_else(|| albums.iter().find_map(|album| album.cover_art.clone())),
            id: artist.id,
            name: artist.name,
            rune_in_library: artist.rune_in_library,
            albums,
        })
    }

    /// A catalog playlist with its songs. It can only be read.
    pub(super) async fn catalog_playlist(
        &self,
        catalog_id: &str,
    ) -> Outcome<SubsonicPlaylistDetail> {
        let (playlist, tracks) = try_join(
            async { Ok(self.client.catalog_playlist(catalog_id).await?) },
            self.catalog_tracks(catalog_id, CatalogKind::Playlist),
        )
        .await?;
        let artworks = &self.caches.artworks;
        let marks = self.library_marks(None);
        let playlist = map::catalog_playlist(&playlist, artworks);
        let songs: Vec<SubsonicSong> = tracks
            .iter()
            .map(|track| {
                let mut song = map::catalog_song(track, artworks);
                song.rune_in_library = Some(marks.song(&track.id));
                song
            })
            .collect();
        let songs = self.with_likes(songs).await;
        Ok(SubsonicPlaylistDetail {
            id: playlist.id,
            name: playlist.name,
            song_count: u32::try_from(songs.len()).ok(),
            duration: total_duration(&songs),
            owner: playlist.owner,
            public: playlist.public,
            cover_art: playlist.cover_art,
            rune_edit: playlist.rune_edit,
            rune_in_library: Some(marks.playlist(catalog_id)),
            songs,
        })
    }

    /// The songs of a catalog album or playlist; none when Apple says it has no tracks.
    async fn catalog_tracks(&self, catalog_id: &str, kind: CatalogKind) -> Outcome<Vec<Song>> {
        let tracks = match kind {
            CatalogKind::Playlist => self.client.catalog_playlist_tracks(catalog_id).await,
            _ => self.client.catalog_album_tracks(catalog_id).await,
        };
        match tracks {
            Ok(tracks) => Ok(tracks),
            Err(Error::NotFound(_)) => Ok(Vec::new()),
            Err(error) => Err(error.into()),
        }
    }

    // Library additions ----------------------------------------------------

    /// Adds catalog songs, albums and playlists to the library in one request. Library ids
    /// are in the library already and are skipped; anything else, an artist included, is
    /// not found.
    pub(super) async fn add_to_library(&self, ids: &[&str]) -> Outcome<()> {
        let mut songs = Vec::new();
        let mut albums = Vec::new();
        let mut playlists = Vec::new();
        for id in ids {
            if let Some(catalog_id) = map::catalog_part(id, CatalogKind::Song) {
                songs.push(catalog_id.to_owned());
            } else if let Some(catalog_id) = map::catalog_part(id, CatalogKind::Album) {
                albums.push(catalog_id.to_owned());
            } else if let Some(catalog_id) = map::catalog_part(id, CatalogKind::Playlist) {
                playlists.push(catalog_id.to_owned());
            } else {
                map::library_id(id, "item")?;
            }
        }
        if songs.is_empty() && albums.is_empty() && playlists.is_empty() {
            return Ok(());
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
        Ok(())
    }

    /// Puts added songs, and the songs of added albums, into the kept song union, and added
    /// albums first in the kept album list, so they show before Apple lists them. When they
    /// cannot be read, those caches are rebuilt on their next read instead. Nothing changes
    /// when a sign-in happened since `session`.
    async fn patch_after_adding(&self, session: u64, songs: &[String], albums: &[String]) {
        if self.caches.session() != session {
            return;
        }
        let union_kept = self.caches.songs.peek().is_some();
        let albums_kept = self.caches.albums.peek().is_some();
        if !union_kept && !albums_kept {
            return;
        }
        let read = try_join(
            async {
                if union_kept && !songs.is_empty() {
                    self.client.catalog_songs(songs).await.map_err(Into::into)
                } else {
                    Ok(Vec::new())
                }
            },
            try_join_all(albums.iter().map(|id| {
                try_join(
                    async { Ok(self.client.catalog_album(id).await?) },
                    self.catalog_tracks(id, CatalogKind::Album),
                )
            })),
        )
        .await;
        if self.caches.session() != session {
            return;
        }
        let Ok((mut tracks, albums)) = read else {
            self.caches.songs.invalidate();
            self.caches.albums.invalidate();
            return;
        };
        for (album, songs) in &albums {
            self.caches.remember_album_tracks(
                session,
                &album.id,
                songs.iter().map(|song| song.id.clone()).collect(),
            );
        }
        tracks.extend(albums.iter().flat_map(|(_, songs)| songs.iter().cloned()));
        let artworks = &self.caches.artworks;
        // Their catalog ids are not recorded as the library's: until Apple lists their library
        // copies, nothing duplicates them.
        self.caches.songs.patch(|union| {
            for track in &tracks {
                let mut song = map::catalog_song(track, artworks);
                song.rune_in_library = Some(true);
                union.push(song);
            }
        });
        self.caches.albums.patch(|list| {
            for (album, _) in albums.iter().rev() {
                let mut mapped = map::catalog_album(album, artworks);
                mapped.rune_in_library = Some(true);
                list.albums.retain(|known| known.id != mapped.id);
                list.albums.insert(0, mapped);
            }
        });
    }

    // Recommendations ------------------------------------------------------

    /// Up to `count` groups of the user's recommendations, kept for [`TTL`]. Catalog items
    /// are marked against the library on every read, so additions show at once.
    pub(super) async fn recommendations(
        &self,
        count: u32,
    ) -> Outcome<Vec<RuneRecommendationGroup>> {
        let groups = self
            .caches
            .recommendations
            .get(Instant::now(), TTL, || self.build_recommendations())
            .await?;
        let marks = self.library_marks(None);
        Ok(take(groups.iter().cloned(), 0, count)
            .into_iter()
            .map(|mut group| {
                for album in &mut group.albums {
                    if let Some(catalog_id) = map::catalog_part(&album.id, CatalogKind::Album) {
                        album.rune_in_library =
                            Some(marks.album(catalog_id, &album.name, album.artist.as_deref()));
                    }
                }
                for playlist in &mut group.playlists {
                    if let Some(catalog_id) = map::catalog_part(&playlist.id, CatalogKind::Playlist)
                    {
                        playlist.rune_in_library = Some(marks.playlist(catalog_id));
                    }
                }
                group
            })
            .collect())
    }

    async fn build_recommendations(&self) -> Outcome<Vec<RuneRecommendationGroup>> {
        let recommendations = match self.client.recommendations().await {
            Ok(recommendations) => recommendations,
            Err(Error::NotFound(_)) => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(recommendations
            .iter()
            .filter_map(|entry| map::recommendation_group(entry, &self.caches.artworks))
            .collect())
    }
}

/// A failure as it can be logged: Apple's errors mapped as for a reply, which carry no tokens.
fn describe(failure: Failure) -> SubsonicError {
    match failure {
        Failure::Apple(error) => map::subsonic_error(error),
        Failure::Subsonic(error) => error,
    }
}
