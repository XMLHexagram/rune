//! A Subsonic-shaped server over the Apple Music API, so Rune can open Apple Music like
//! any Subsonic server. Apple Music audio is DRM protected: songs carry a hosted
//! `runePlayback` that the platform's MusicKit host plays, and `stream` is unsupported.

mod cache;
mod facets;
mod map;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::slice;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::future::{join, try_join, try_join3};
use futures_util::{stream, StreamExt, TryStreamExt};
use subsonic::{
    RuneLoginCompletion, RuneLoginMethod, SearchLimits, SubsonicAlbum, SubsonicAlbumDetail,
    SubsonicArtist, SubsonicArtistDetail, SubsonicError, SubsonicPlaylist, SubsonicPlaylistDetail,
    SubsonicScanStatus, SubsonicSearchResult, SubsonicServerError, SubsonicSong,
};

use crate::client::{AppleMusicClient, ClientBuilder};
use crate::error::Error;
use crate::models::{
    LibraryEntry, LibraryPlaylist, LibrarySearchResults, LibrarySearchType, LibrarySong, Page,
    RatingValue,
};
use crate::tokens::TokenSource;
use crate::{ID_BATCH, LIBRARY_SEARCH_LIMIT};
use cache::{Caches, SongUnion, TTL};
use map::SongRef;

/// Relationships read with library songs, for their album and artist ids.
const SONG_INCLUDE: &[&str] = &["albums", "artists"];
/// Library pages, playlists or batches of ratings read at once.
const PARALLEL_READS: usize = 4;
/// Largest cover accepted.
const MAX_COVER_BYTES: usize = 8 * 1024 * 1024;

/// Serves the Subsonic facets from the user's Apple Music library.
///
/// It starts without tokens: the platform host signs in through `completeRuneLogin`
/// with method `platform`, handing over the Music User Token and the developer token,
/// which are kept in memory only. Until then, and whenever Apple rejects them, calls
/// fail with the Subsonic not-authorized error and `runeReason` `credentialsExpired`.
pub struct AppleMusicServer {
    client: AppleMusicClient,
    tokens: Arc<SessionTokens>,
    artwork_http: reqwest::Client,
    caches: Caches,
}

/// A failure inside the server. Apple's are mapped once the facet call is over, when
/// a rejected token can be told from a refused request.
enum Failure {
    Apple(Error),
    Subsonic(SubsonicError),
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self::Apple(error)
    }
}

impl From<SubsonicError> for Failure {
    fn from(error: SubsonicError) -> Self {
        Self::Subsonic(error)
    }
}

type Outcome<T> = Result<T, Failure>;

impl Default for AppleMusicServer {
    fn default() -> Self {
        Self::new()
    }
}

impl AppleMusicServer {
    /// A server for the production Apple Music API.
    pub fn new() -> Self {
        Self::with_client(|builder| builder)
    }

    /// A server whose client is configured by `configure`, for another host, retry
    /// policy or HTTP client. Its tokens always come from sign-in.
    pub fn with_client(configure: impl FnOnce(ClientBuilder) -> ClientBuilder) -> Self {
        Self::try_with_client(configure)
            .expect("an HTTP client with only a timeout set always builds")
    }

    /// Like [`new`](Self::new), returning the HTTP client build error instead of panicking.
    pub fn try_new() -> Result<Self, reqwest::Error> {
        Self::try_with_client(|builder| builder)
    }

    /// Like [`with_client`](Self::with_client), returning the HTTP client build error
    /// instead of panicking.
    pub fn try_with_client(
        configure: impl FnOnce(ClientBuilder) -> ClientBuilder,
    ) -> Result<Self, reqwest::Error> {
        let tokens = Arc::new(SessionTokens::default());
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        let client = configure(AppleMusicClient::builder(tokens.clone()).http_client(http)).build();
        let artwork_http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            client,
            tokens,
            artwork_http,
            caches: Caches::default(),
        })
    }

    /// Runs a facet call that needs Apple: the sign-in is checked before anything else,
    /// so every such call answers the same way without one.
    async fn signed_in<T>(
        &self,
        call: impl Future<Output = Outcome<T>>,
    ) -> Result<T, SubsonicError> {
        if !self.tokens.signed_in() {
            return Err(map::credentials_expired());
        }
        match call.await {
            Ok(value) => Ok(value),
            Err(Failure::Subsonic(error)) => Err(error),
            Err(Failure::Apple(error)) => Err(self.apple_error(error).await),
        }
    }

    /// A 401 or 403 means an expired sign-in only when a plain read with the same
    /// tokens fails too; otherwise Apple refused just this request, as it does writes
    /// its policy or the subscription forbids.
    async fn apple_error(&self, error: Error) -> SubsonicError {
        if matches!(error, Error::Unauthorized | Error::Forbidden)
            && matches!(
                self.client.user_storefront().await,
                Err(Error::Unauthorized
                    | Error::Forbidden
                    | Error::MissingUserToken
                    | Error::Token(_))
            )
        {
            return map::credentials_expired();
        }
        map::subsonic_error(error)
    }

    fn sign_in(&self, completion: &RuneLoginCompletion) -> Result<(), SubsonicError> {
        if completion.method != RuneLoginMethod::Platform {
            return Err(map::unsupported("completeRuneLogin"));
        }
        let given = |value: &Option<String>| value.clone().filter(|value| !value.is_empty());
        let (Some(user), Some(developer)) =
            (given(&completion.token), given(&completion.developer_token))
        else {
            return Err(SubsonicError::Server {
                kind: SubsonicServerError::MissingParameter,
                message: "platform sign-in needs token and developerToken".into(),
                rune_reason: None,
            });
        };
        // New tokens may belong to another account.
        if self.tokens.replace(Tokens { developer, user }) {
            self.caches.start_session();
            self.client.forget_storefront();
        }
        Ok(())
    }

    // All songs ------------------------------------------------------------

    /// The most complete list of the user's songs: library songs, then the songs of
    /// the user's own playlists and Favorites that are not in the library.
    async fn song_union(&self) -> Outcome<Arc<SongUnion>> {
        self.caches
            .songs
            .get(Instant::now(), TTL, || self.build_song_union())
            .await
    }

    async fn build_song_union(&self) -> Outcome<SongUnion> {
        let artworks = &self.caches.artworks;
        let mut union = SongUnion::default();
        // Library songs whose catalog id only their `catalog` relationship names.
        let mut unresolved = Vec::new();
        let library = async {
            let mut pages = self.client.library_song_pages(SONG_INCLUDE, PARALLEL_READS);
            while let Some(page) = pages.try_next().await? {
                for entry in &page {
                    union.extend(map::library_song(entry, artworks));
                    match entry.store_id() {
                        Some(catalog_id) => union.add_library_catalog_id(catalog_id),
                        None if map::is_library_song_id(&entry.id) => {
                            unresolved.push(entry.id.clone());
                        }
                        None => {}
                    }
                }
            }
            Ok::<_, Error>(())
        };
        let ((), playlists) = try_join(
            library,
            self.client.library_playlists().try_collect::<Vec<_>>(),
        )
        .await?;

        // Owned ids: closures taking borrowed items here fail the compiler's Send check.
        let joining: Vec<String> = playlists
            .into_iter()
            .filter(map::joins_song_union)
            .map(|playlist| playlist.id)
            .collect();
        let mut tracks = stream::iter(joining)
            .map(|id| async move {
                let tracks = self.playlist_tracks(&id).await?;
                Ok::<_, Failure>(
                    tracks
                        .iter()
                        .filter_map(|track| map::library_song(track, artworks))
                        .collect::<Vec<_>>(),
                )
            })
            .buffered(PARALLEL_READS);
        let mut extra = Vec::new();
        while let Some(songs) = tracks.try_next().await? {
            extra.extend(songs.into_iter().filter(|song| !union.lists(&song.id)));
        }
        drop(tracks);

        let catalog_copies = extra
            .iter()
            .any(|song| matches!(SongRef::parse(&song.id), Ok(SongRef::Catalog(_))));
        if catalog_copies && !unresolved.is_empty() {
            // Best effort: without these ids a catalog copy of such a song is listed too.
            if let Ok(songs) = self
                .client
                .library_songs_by_ids(&unresolved, &["catalog"])
                .await
            {
                for catalog_id in songs.iter().filter_map(LibrarySong::store_id) {
                    union.add_library_catalog_id(catalog_id);
                }
            }
        }
        union.extend(extra);
        Ok(union)
    }

    async fn playlist_tracks(&self, id: &str) -> Outcome<Vec<LibrarySong>> {
        match self.client.library_playlist_tracks(id).try_collect().await {
            Ok(tracks) => Ok(tracks),
            // Apple answers 404 for the tracks of an empty playlist.
            Err(Error::NotFound(_)) => Ok(Vec::new()),
            Err(error) => Err(error.into()),
        }
    }

    /// A song of the kept union, without asking Apple.
    fn known_song(&self, id: &str) -> Option<SubsonicSong> {
        self.caches.songs.peek()?.get(id).cloned()
    }

    /// Adds the songs a write put into the user's playlists to the kept union, at its
    /// end so paging stays stable. When they cannot be read, the union is rebuilt on
    /// its next read instead.
    async fn patch_union(&self, written: &[SongRef<'_>]) {
        let Some(union) = self.caches.songs.peek() else {
            return;
        };
        let mut library = Vec::new();
        let mut catalog = Vec::new();
        for song in written {
            match *song {
                SongRef::Library(id) if !union.lists(id) => library.push(id.to_owned()),
                SongRef::Catalog(id) if !union.lists(&map::catalog_song_id(id)) => {
                    catalog.push(id.to_owned());
                }
                _ => {}
            }
        }
        drop(union);
        if library.is_empty() && catalog.is_empty() {
            return;
        }
        let artworks = &self.caches.artworks;
        let read = try_join(
            async {
                if library.is_empty() {
                    return Ok(Vec::new());
                }
                self.client
                    .library_songs_by_ids(&library, SONG_INCLUDE)
                    .await
            },
            async {
                if catalog.is_empty() {
                    return Ok(Vec::new());
                }
                self.client.catalog_songs(&catalog).await
            },
        )
        .await;
        let Ok((library, catalog)) = read else {
            self.caches.songs.invalidate();
            return;
        };
        self.caches.songs.patch(|union| {
            for entry in &library {
                union.extend(map::library_song(entry, artworks));
                if let Some(catalog_id) = entry.store_id() {
                    union.add_library_catalog_id(catalog_id);
                }
            }
            union.extend(catalog.iter().map(|song| map::catalog_song(song, artworks)));
        });
    }

    // Ratings --------------------------------------------------------------

    fn with_starred(&self, songs: impl IntoIterator<Item = SubsonicSong>) -> Vec<SubsonicSong> {
        let ratings = self.caches.ratings();
        songs
            .into_iter()
            .map(|mut song| {
                song.starred = map::starred(ratings.get(&song.id).copied());
                song
            })
            .collect()
    }

    /// Marks the liked songs, reading the ratings not read yet. Likes are extras in a
    /// listing: when they cannot be read, the songs are listed without them.
    async fn with_likes(&self, songs: Vec<SubsonicSong>) -> Vec<SubsonicSong> {
        let unread: Vec<String> = {
            let ratings = self.caches.ratings();
            let mut seen = HashSet::new();
            songs
                .iter()
                .filter(|song| !ratings.contains_key(&song.id) && seen.insert(&song.id))
                .map(|song| song.id.clone())
                .collect()
        };
        if !unread.is_empty() {
            self.read_ratings(&unread).await.ok();
        }
        self.with_starred(songs)
    }

    /// Reads the ratings of `ids` in batches and keeps them; unrated songs keep 0.
    async fn read_ratings(&self, ids: &[String]) -> Outcome<()> {
        let session = self.caches.session();
        let mut library = Vec::new();
        let mut catalog = Vec::new();
        for id in ids {
            match SongRef::parse(id) {
                Ok(SongRef::Library(id)) => library.push(id.to_owned()),
                Ok(SongRef::Catalog(id)) => catalog.push(id.to_owned()),
                Err(_) => {}
            }
        }
        let batches: Vec<(bool, Vec<String>)> = library
            .chunks(ID_BATCH)
            .map(|batch| (false, batch.to_vec()))
            .chain(catalog.chunks(ID_BATCH).map(|batch| (true, batch.to_vec())))
            .collect();
        let read: Vec<_> = stream::iter(batches)
            .map(|(is_catalog, batch)| async move {
                let found = if is_catalog {
                    self.client.catalog_song_ratings(&batch).await
                } else {
                    self.client.library_song_ratings(&batch).await
                };
                match found {
                    Ok(found) => Ok((is_catalog, batch, found)),
                    Err(Error::NotFound(_)) => Ok((is_catalog, batch, Vec::new())),
                    Err(error) => Err(Failure::from(error)),
                }
            })
            .buffer_unordered(PARALLEL_READS)
            .try_collect()
            .await?;
        self.caches.update_ratings(session, |ratings| {
            for (is_catalog, batch, found) in read {
                let song_id = |id: &str| {
                    if is_catalog {
                        map::catalog_song_id(id)
                    } else {
                        id.to_owned()
                    }
                };
                for id in &batch {
                    ratings.insert(song_id(id), 0);
                }
                for rating in found {
                    let value = rating.attributes.and_then(|attributes| attributes.value);
                    ratings.insert(song_id(&rating.id), value.unwrap_or(0));
                }
            }
        });
        Ok(())
    }

    async fn rate(&self, song_ids: &[&str], like: bool) -> Outcome<()> {
        let songs = song_ids
            .iter()
            .map(|id| SongRef::parse(id).map(|song| (*id, song)))
            .collect::<Result<Vec<_>, _>>()?;
        let session = self.caches.session();
        for (id, song) in songs {
            match (song, like) {
                (SongRef::Library(id), true) => self
                    .client
                    .set_library_song_rating(id, RatingValue::Like)
                    .await
                    .map(drop)?,
                (SongRef::Catalog(id), true) => self
                    .client
                    .set_catalog_song_rating(id, RatingValue::Like)
                    .await
                    .map(drop)?,
                (SongRef::Library(id), false) => self.client.delete_library_song_rating(id).await?,
                (SongRef::Catalog(id), false) => self.client.delete_catalog_song_rating(id).await?,
            }
            self.caches.update_ratings(session, |ratings| {
                ratings.insert(id.to_owned(), i8::from(like));
            });
        }
        Ok(())
    }

    async fn starred_songs(&self) -> Outcome<Vec<SubsonicSong>> {
        let union = self.song_union().await?;
        let unread: Vec<String> = {
            let ratings = self.caches.ratings();
            union
                .songs
                .iter()
                .filter(|song| !ratings.contains_key(&song.id))
                .map(|song| song.id.clone())
                .collect()
        };
        self.read_ratings(&unread).await?;
        Ok(self
            .with_starred(union.songs.iter().cloned())
            .into_iter()
            .filter(|song| song.starred.is_some())
            .collect())
    }

    // Browsing -------------------------------------------------------------

    async fn artists(&self) -> Outcome<Vec<SubsonicArtist>> {
        let artists: Vec<_> = self.client.library_artists().try_collect().await?;
        Ok(artists.iter().map(map::artist).collect())
    }

    async fn artist(&self, id: &str) -> Outcome<SubsonicArtistDetail> {
        let id = map::library_id(id, "artist")?;
        let (artist, albums) = try_join(
            self.client.library_artist(id),
            self.client
                .library_artist_albums(id)
                .try_collect::<Vec<_>>(),
        )
        .await?;
        let artist = map::artist(&artist);
        let albums: Vec<SubsonicAlbum> = albums
            .iter()
            .map(|album| {
                let mut album = map::album(album, &self.caches.artworks);
                album.artist_id.get_or_insert_with(|| artist.id.clone());
                album.artist.get_or_insert_with(|| artist.name.clone());
                album
            })
            .collect();
        Ok(SubsonicArtistDetail {
            album_count: u32::try_from(albums.len()).ok(),
            id: artist.id,
            name: artist.name,
            cover_art: albums.iter().find_map(|album| album.cover_art.clone()),
            albums,
        })
    }

    async fn album(&self, id: &str) -> Outcome<SubsonicAlbumDetail> {
        let id = map::library_id(id, "album")?;
        let (album, tracks) = try_join(
            self.client.library_album(id, &["artists"]),
            self.client.library_album_tracks(id).try_collect::<Vec<_>>(),
        )
        .await?;
        let album = map::album(&album, &self.caches.artworks);
        let songs = tracks
            .iter()
            .filter_map(|track| map::library_song(track, &self.caches.artworks))
            .map(|mut song| {
                song.album_id.get_or_insert_with(|| album.id.clone());
                if song.artist_id.is_none() && song.artist == album.artist {
                    song.artist_id.clone_from(&album.artist_id);
                }
                if song.cover_art.is_none() {
                    song.cover_art.clone_from(&album.cover_art);
                }
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
            songs,
        })
    }

    async fn song(&self, id: &str) -> Outcome<SubsonicSong> {
        let song = match self.known_song(id) {
            Some(song) => song,
            None => self.read_song(id, SongRef::parse(id)?).await?,
        };
        // Rune reads a song's like through here, so its rating is read afresh; the song
        // is still worth returning when that fails.
        self.read_ratings(slice::from_ref(&song.id)).await.ok();
        Ok(self.with_starred([song]).remove(0))
    }

    async fn read_song(&self, id: &str, song: SongRef<'_>) -> Outcome<SubsonicSong> {
        let artworks = &self.caches.artworks;
        let found = match song {
            SongRef::Catalog(catalog_id) => self
                .client
                .catalog_songs(&[catalog_id.to_owned()])
                .await
                .map(|songs| songs.first().map(|song| map::catalog_song(song, artworks))),
            SongRef::Library(library_id) => match self
                .client
                .library_songs_by_ids(&[library_id.to_owned()], SONG_INCLUDE)
                .await
            {
                Ok(songs) => Ok(songs
                    .iter()
                    .find_map(|song| map::library_song(song, artworks))),
                Err(Error::NotFound(_)) => Ok(None),
                Err(error) => Err(error),
            },
        };
        // The union may know a song the lookup missed, or have been built meanwhile.
        match found {
            Ok(Some(song)) => Ok(song),
            Ok(None) => self
                .known_song(id)
                .ok_or_else(|| map::not_found("song").into()),
            Err(error) => self.known_song(id).ok_or(Failure::Apple(error)),
        }
    }

    // Lists ----------------------------------------------------------------

    async fn album_page(&self, size: u32, offset: u32) -> Outcome<Vec<SubsonicAlbum>> {
        let albums = self
            .caches
            .albums
            .get(Instant::now(), TTL, || self.build_album_list())
            .await?;
        Ok(albums
            .iter()
            .skip(usize::try_from(offset).unwrap_or(usize::MAX))
            .take(usize::try_from(size).unwrap_or(usize::MAX))
            .cloned()
            .collect())
    }

    /// Recently added albums first, in Apple's order, then every other library album:
    /// Rune reads its whole album list through here.
    async fn build_album_list(&self) -> Outcome<Vec<SubsonicAlbum>> {
        let artworks = &self.caches.artworks;
        let library = async {
            let mut library = Vec::new();
            let mut pages = self
                .client
                .library_album_pages(&["artists"], PARALLEL_READS);
            while let Some(page) = pages.try_next().await? {
                library.extend(page.iter().map(|album| map::album(album, artworks)));
            }
            Ok::<_, Error>(library)
        };
        let (recent, library) = join(
            self.client.recently_added().try_collect::<Vec<_>>(),
            library,
        )
        .await;
        let library = library?;
        // Recently added only orders the list; without it the list is still complete.
        let recent = recent.unwrap_or_default();
        let by_id: HashMap<&str, &SubsonicAlbum> = library
            .iter()
            .map(|album| (album.id.as_str(), album))
            .collect();
        let mut seen = HashSet::new();
        let mut albums = Vec::with_capacity(library.len());
        for entry in &recent {
            let LibraryEntry::Album(album) = entry else {
                continue;
            };
            if seen.insert(album.id.clone()) {
                albums.push(match by_id.get(album.id.as_str()) {
                    Some(known) => (*known).clone(),
                    None => map::album(album, artworks),
                });
            }
        }
        albums.extend(
            library
                .iter()
                .filter(|album| seen.insert(album.id.clone()))
                .cloned(),
        );
        Ok(albums)
    }

    // Search ---------------------------------------------------------------

    async fn search(
        &self,
        query: &str,
        limits: SearchLimits,
        song_offset: u32,
    ) -> Outcome<SubsonicSearchResult> {
        let query = query.trim();
        if query.is_empty() || query == "\"\"" {
            // Rune lists every song through an empty search.
            if limits.songs == 0 {
                return Ok(SubsonicSearchResult::default());
            }
            let union = self.song_union().await?;
            let page = union
                .songs
                .iter()
                .skip(usize::try_from(song_offset).unwrap_or(usize::MAX))
                .take(usize::try_from(limits.songs).unwrap_or(usize::MAX))
                .cloned()
                .collect();
            drop(union);
            return Ok(SubsonicSearchResult {
                songs: self.with_likes(page).await,
                ..SubsonicSearchResult::default()
            });
        }
        let artworks = &self.caches.artworks;
        let (songs, albums, artists) = try_join3(
            self.search_library(
                query,
                LibrarySearchType::Songs,
                limits.songs,
                song_offset,
                |results| results.songs,
            ),
            self.search_library(
                query,
                LibrarySearchType::Albums,
                limits.albums,
                0,
                |results| results.albums,
            ),
            self.search_library(
                query,
                LibrarySearchType::Artists,
                limits.artists,
                0,
                |results| results.artists,
            ),
        )
        .await?;
        let songs = songs
            .iter()
            .filter_map(|song| map::library_song(song, artworks))
            .collect();
        Ok(SubsonicSearchResult {
            artists: artists.iter().map(map::artist).collect(),
            albums: albums
                .iter()
                .map(|album| map::album(album, artworks))
                .collect(),
            songs: self.with_likes(songs).await,
        })
    }

    /// Up to `count` results of one type, in pages of Apple's largest search limit.
    async fn search_library<T>(
        &self,
        term: &str,
        kind: LibrarySearchType,
        count: u32,
        offset: u32,
        pick: fn(LibrarySearchResults) -> Page<T>,
    ) -> Outcome<Vec<T>> {
        let mut found = Vec::new();
        let mut read = 0_u32;
        while read < count {
            let limit = (count - read).min(LIBRARY_SEARCH_LIMIT);
            let page = match self
                .client
                .search_library(term, &[kind], limit, offset.saturating_add(read))
                .await
            {
                Ok(results) => pick(results),
                Err(Error::NotFound(_)) => Page::default(),
                Err(error) => return Err(error.into()),
            };
            let got = u32::try_from(page.data.len()).unwrap_or(u32::MAX);
            found.extend(page.data);
            read = read.saturating_add(got);
            if page.next.is_none() || got < limit {
                break;
            }
        }
        found.truncate(usize::try_from(count).unwrap_or(usize::MAX));
        Ok(found)
    }

    // Playlists ------------------------------------------------------------

    async fn playlists(&self) -> Outcome<Vec<SubsonicPlaylist>> {
        let mut playlists: Vec<LibraryPlaylist> =
            self.client.library_playlists().try_collect().await?;
        // Apple may list a new playlist only a while after creating it.
        let mut created = self.caches.created();
        created.retain(|new| playlists.iter().all(|listed| listed.id != new.id));
        playlists.extend(created.iter().cloned());
        drop(created);
        Ok(playlists
            .iter()
            .map(|playlist| map::playlist(playlist, &self.caches.artworks))
            .collect())
    }

    async fn playlist(&self, id: &str) -> Outcome<SubsonicPlaylistDetail> {
        let id = map::library_id(id, "playlist")?;
        let (playlist, tracks) = try_join(
            async { Ok(self.client.library_playlist(id).await?) },
            self.playlist_tracks(id),
        )
        .await?;
        let playlist = map::playlist(&playlist, &self.caches.artworks);
        let songs = tracks
            .iter()
            .filter_map(|track| map::library_song(track, &self.caches.artworks))
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
            songs,
        })
    }

    async fn create_playlist(&self, name: &str, song_ids: &[&str]) -> Outcome<()> {
        let songs = song_refs(song_ids)?;
        let tracks: Vec<_> = songs.iter().map(|song| song.track()).collect();
        let session = self.caches.session();
        let created = self
            .client
            .create_library_playlist(name, None, &tracks)
            .await?;
        self.caches.remember_created(session, created);
        self.patch_union(&songs).await;
        Ok(())
    }

    /// Apple's API can only append to a playlist: renaming or removing is unsupported.
    async fn append_to_playlist(
        &self,
        playlist_id: &str,
        name: Option<&str>,
        song_ids: &[&str],
        song_indexes_to_remove: &[u32],
    ) -> Outcome<()> {
        if name.is_some() || !song_indexes_to_remove.is_empty() {
            return Err(map::unsupported("updatePlaylist").into());
        }
        let id = map::library_id(playlist_id, "playlist")?;
        let songs = song_refs(song_ids)?;
        if songs.is_empty() {
            return Ok(());
        }
        // Without this check Apple would refuse writes to Favorites or subscribed
        // playlists only after the request.
        let playlist = self.client.library_playlist(id).await?;
        if !map::can_append(&playlist) {
            return Err(map::unsupported("updatePlaylist").into());
        }
        let tracks: Vec<_> = songs.iter().map(|song| song.track()).collect();
        self.client
            .add_tracks_to_library_playlist(id, &tracks)
            .await?;
        self.patch_union(&songs).await;
        Ok(())
    }

    async fn scan_status(&self) -> Outcome<SubsonicScanStatus> {
        let union = self.song_union().await?;
        Ok(SubsonicScanStatus {
            scanning: false,
            count: Some(u32::try_from(union.songs.len()).unwrap_or(u32::MAX)),
        })
    }

    // Media ----------------------------------------------------------------

    /// Artwork URLs are public: no Apple token is sent to them, and no sign-in needed.
    async fn cover_art(&self, id: &str, size: Option<u32>) -> Result<Vec<u8>, SubsonicError> {
        let url = self
            .caches
            .artworks
            .url(id, size)
            .ok_or_else(|| map::not_found("cover art"))?;
        let transport =
            |error: reqwest::Error| SubsonicError::Transport(error.without_url().to_string());
        let mut response = self.artwork_http.get(url).send().await.map_err(transport)?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(map::not_found("cover art"));
        }
        if !status.is_success() {
            return Err(SubsonicError::HttpStatus(status));
        }
        let too_large = || SubsonicError::Malformed("cover art is too large".into());
        if response
            .content_length()
            .is_some_and(|length| length > MAX_COVER_BYTES as u64)
        {
            return Err(too_large());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport)? {
            if bytes.len().saturating_add(chunk.len()) > MAX_COVER_BYTES {
                return Err(too_large());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

fn song_refs<'a>(song_ids: &[&'a str]) -> Result<Vec<SongRef<'a>>, SubsonicError> {
    song_ids.iter().map(|id| SongRef::parse(id)).collect()
}

fn total_duration(songs: &[SubsonicSong]) -> Option<u32> {
    songs
        .iter()
        .try_fold(0_u32, |total, song| total.checked_add(song.duration?))
}

impl Extend<SubsonicSong> for SongUnion {
    fn extend<I: IntoIterator<Item = SubsonicSong>>(&mut self, songs: I) {
        for song in songs {
            self.push(song);
        }
    }
}

#[derive(PartialEq, Eq)]
struct Tokens {
    developer: String,
    user: String,
}

/// The tokens of the current sign-in, in memory only. Nothing newer can be fetched on
/// refresh, so a rejected token stays rejected until the host signs in again.
#[derive(Default)]
struct SessionTokens(Mutex<Option<Tokens>>);

impl SessionTokens {
    /// Keeps `tokens`; false when they are the ones already kept.
    fn replace(&self, tokens: Tokens) -> bool {
        let mut current = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if current.as_ref() == Some(&tokens) {
            return false;
        }
        *current = Some(tokens);
        true
    }

    fn signed_in(&self) -> bool {
        self.read(|_| ()).is_some()
    }

    fn read<T>(&self, pick: impl FnOnce(&Tokens) -> T) -> Option<T> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(pick)
    }
}

#[async_trait]
impl TokenSource for SessionTokens {
    async fn developer_token(&self, _refresh: bool) -> crate::Result<String> {
        self.read(|tokens| tokens.developer.clone())
            .ok_or_else(|| Error::Token("not signed in to Apple Music".into()))
    }

    async fn user_token(&self, _refresh: bool) -> crate::Result<Option<String>> {
        Ok(self.read(|tokens| tokens.user.clone()))
    }
}
