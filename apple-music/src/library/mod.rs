//! The user's Apple Music library and the catalog songs around it, through a neutral
//! API: plain types in, plain types out, with the library's own cache.
//!
//! Sign-in is explicit: a host hands over the developer token and the Music User Token
//! with [`AppleLibrary::sign_in`], and they live in memory only. Until then, and whenever
//! Apple rejects them, calls fail with [`LibraryError::SignedOut`].
//!
//! Songs are named by [`TrackKey`]: library songs by Apple's library id (`i.…`), catalog
//! songs as `catalog:<id>`. Library albums, artists and playlists keep Apple's library ids.

mod cache;
mod map;
mod types;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::slice;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use async_trait::async_trait;
use futures_util::future::{join, try_join, BoxFuture};
use futures_util::{stream, FutureExt, StreamExt, TryStreamExt};

use crate::client::{AppleMusicClient, ClientBuilder};
use crate::error::Error;
use crate::models::{LibraryEntry, LibraryPlaylist, LibrarySong, RatingValue, Storefront};
use crate::tokens::TokenSource;
use crate::ID_BATCH;
use cache::{Caches, Slot, TTL};
use types::is_library_id;
pub use types::{
    Album, AlbumDetail, Artist, ArtistDetail, Artwork, ParseTrackKeyError, Playlist, SongList,
    Track, TrackKey,
};

/// Relationships read with library songs, for their album and artist ids.
const SONG_INCLUDE: &[&str] = &["albums", "artists"];
/// Library pages, playlists or batches of ratings read at once.
const PARALLEL_READS: usize = 4;

/// What a call could not find.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    /// A song.
    Song,
    /// An album.
    Album,
    /// An artist.
    Artist,
    /// A playlist.
    Playlist,
}

impl fmt::Display for Missing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Song => "song",
            Self::Album => "album",
            Self::Artist => "artist",
            Self::Playlist => "playlist",
        })
    }
}

/// Why a call failed.
#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    /// No tokens were handed over, or Apple rejects them: the user has to sign in again.
    #[error("sign in to Apple Music")]
    SignedOut,
    /// The tokens changed twice while the call ran, so it has no answer that belongs to
    /// one sign-in. Asking again is safe.
    #[error("the Apple Music sign-in changed during the call")]
    SessionChanged,
    /// Apple refused this request though the tokens work, as it does for writes its
    /// policy or the subscription forbids.
    #[error("Apple Music refused the request")]
    Refused,
    /// The thing asked for is not in the library or the catalog.
    #[error("Apple Music {0} not found")]
    NotFound(Missing),
    /// Songs cannot be added to the playlist: Favorites, a subscribed playlist or any
    /// other that Apple does not let the user edit.
    #[error("the playlist cannot be changed")]
    ReadOnlyPlaylist,
    /// Apple Music or the network cannot answer now: rate limited, a server error that
    /// outlasted the retries, or a transport failure. Trying later may work.
    #[error("Apple Music is unavailable: {0}")]
    Unavailable(Error),
    /// Any other failure of the Apple Music API.
    #[error(transparent)]
    Apple(Error),
}

/// What a library call returns.
pub type Result<T> = std::result::Result<T, LibraryError>;

/// A failure inside a call. Apple's are mapped once the call is over, when a rejected
/// token can be told from a refused request.
enum Failure {
    Apple(Error),
    Library(LibraryError),
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self::Apple(error)
    }
}

impl From<LibraryError> for Failure {
    fn from(error: LibraryError) -> Self {
        Self::Library(error)
    }
}

type Outcome<T> = std::result::Result<T, Failure>;

/// The time the caches age by. Tests replace it to move time without waiting.
#[doc(hidden)]
pub trait Clock: Send + Sync {
    /// The current instant.
    fn now(&self) -> Instant;
}

/// The system's monotonic clock.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// A token is empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("both the developer token and the Music User Token are needed")]
pub struct IncompleteTokens;

/// The two tokens of a sign-in.
#[derive(Clone, PartialEq, Eq)]
pub struct Tokens {
    developer: String,
    user: String,
}

impl Tokens {
    /// The developer token and the Music User Token. Both must be given.
    pub fn new(
        developer: impl Into<String>,
        user: impl Into<String>,
    ) -> std::result::Result<Self, IncompleteTokens> {
        let (developer, user) = (developer.into(), user.into());
        if developer.is_empty() || user.is_empty() {
            return Err(IncompleteTokens);
        }
        Ok(Self { developer, user })
    }
}

impl fmt::Debug for Tokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tokens")
            .field("developer", &"<redacted>")
            .field("user", &"<redacted>")
            .finish()
    }
}

/// What a sign-in did to what the library kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignInChange {
    /// The same tokens as before: nothing was forgotten.
    Unchanged,
    /// New tokens, which may belong to another account: everything kept was dropped.
    Changed,
}

/// The user's songs, albums, artists and playlists from Apple Music.
///
/// Clones are handles to the same sign-in and caches.
#[derive(Clone)]
pub struct AppleLibrary {
    client: AppleMusicClient,
    tokens: Arc<SessionTokens>,
    caches: Arc<Caches>,
    clock: Arc<dyn Clock>,
}

impl fmt::Debug for AppleLibrary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppleLibrary")
            .field("signed_in", &self.tokens.signed_in())
            .finish_non_exhaustive()
    }
}

impl Default for AppleLibrary {
    fn default() -> Self {
        Self::new()
    }
}

impl AppleLibrary {
    /// A library for the production Apple Music API, signed out.
    pub fn new() -> Self {
        Self::with_client(|builder| builder)
    }

    /// A library whose client is configured by `configure`, for another host, retry
    /// policy or HTTP client. Its tokens always come from [`sign_in`](Self::sign_in).
    pub fn with_client(configure: impl FnOnce(ClientBuilder) -> ClientBuilder) -> Self {
        let tokens = Arc::new(SessionTokens::default());
        let client = configure(AppleMusicClient::builder(tokens.clone())).build();
        Self {
            client,
            tokens,
            caches: Arc::default(),
            clock: Arc::new(SystemClock),
        }
    }

    /// Ages what is kept by `clock` instead of the system's.
    #[doc(hidden)]
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Hands over the tokens of a sign-in. The same tokens as before keep what the
    /// library has read; new ones, which may belong to another account, drop it, and
    /// reads still in flight from the old sign-in are not kept.
    pub fn sign_in(&self, tokens: Tokens) -> SignInChange {
        if !self.tokens.replace(Some(tokens)) {
            return SignInChange::Unchanged;
        }
        self.caches.start_session();
        self.client.forget_storefront();
        SignInChange::Changed
    }

    /// Forgets the tokens and everything kept. Calls fail with
    /// [`LibraryError::SignedOut`] until the next sign-in.
    pub fn sign_out(&self) {
        if self.tokens.replace(None) {
            self.caches.start_session();
            self.client.forget_storefront();
        }
    }

    /// Whether Apple accepts the tokens: the user's storefront, read with them.
    pub async fn check(&self) -> Result<Storefront> {
        if !self.tokens.signed_in() {
            return Err(LibraryError::SignedOut);
        }
        self.client.user_storefront().await.map_err(|error| {
            if rejects_tokens(&error) {
                LibraryError::SignedOut
            } else if matches!(error, Error::NotFound(_)) {
                LibraryError::Apple(error)
            } else {
                library_error(error, Missing::Song)
            }
        })
    }

    /// Runs a call that needs Apple once: the sign-in is checked before anything else,
    /// so every such call answers the same way without one.
    async fn signed_in<T, Fut>(&self, what: Missing, call: impl FnOnce() -> Fut) -> Result<T>
    where
        Fut: Future<Output = Outcome<T>>,
    {
        if !self.tokens.signed_in() {
            return Err(LibraryError::SignedOut);
        }
        match call().await {
            Ok(value) => Ok(value),
            Err(Failure::Library(error)) => Err(error),
            Err(Failure::Apple(error)) => Err(self.apple_error(error, what).await),
        }
    }

    /// Like [`signed_in`](Self::signed_in) for a read. An answer that arrives after the
    /// tokens changed belongs to the old account, so the read is repeated under the new
    /// tokens, once.
    async fn signed_in_read<T, Fut>(&self, what: Missing, call: impl Fn() -> Fut) -> Result<T>
    where
        Fut: Future<Output = Outcome<T>>,
    {
        for _ in 0..2 {
            let session = self.caches.session();
            let result = self.signed_in(what, &call).await;
            if self.caches.session() == session {
                return result;
            }
        }
        Err(LibraryError::SessionChanged)
    }

    /// A 401 or 403 means an expired sign-in only when a plain read with the same tokens
    /// fails too; otherwise Apple refused just this request.
    async fn apple_error(&self, error: Error, what: Missing) -> LibraryError {
        if matches!(error, Error::Unauthorized | Error::Forbidden)
            && matches!(self.client.user_storefront().await, Err(ref again) if rejects_tokens(again))
        {
            return LibraryError::SignedOut;
        }
        library_error(error, what)
    }

    /// The value of a slot: the kept one while it is young. Once it is old, a kept value
    /// is still returned at once and refreshed in the background; only the first build
    /// waits.
    async fn kept<T>(
        &self,
        slot: fn(&Caches) -> &Slot<T>,
        build: fn(AppleLibrary) -> BoxFuture<'static, Outcome<T>>,
    ) -> Outcome<Arc<T>>
    where
        T: Clone + Send + Sync + 'static,
    {
        let now = self.clock.now();
        let kept = slot(&self.caches);
        if let Some(value) = kept.fresh(now, TTL) {
            return Ok(value);
        }
        if let Some(old) = kept.peek() {
            if kept.claim_refresh() {
                let library = self.clone();
                tokio::spawn(async move {
                    let _end = EndRefresh {
                        caches: library.caches.clone(),
                        slot,
                    };
                    // A failed refresh keeps nothing: the old value is served until a
                    // later one succeeds.
                    let _ = slot(&library.caches)
                        .get(now, TTL, || build(library.clone()))
                        .await;
                });
            }
            return Ok(old);
        }
        kept.get(now, TTL, || build(self.clone())).await
    }

    // All songs ------------------------------------------------------------

    /// The most complete list of the user's songs: library songs, then the songs of the
    /// user's own playlists and Favorites that are not in the library. Kept for ten
    /// minutes; the kept list answers [`track`](Self::track) meanwhile it is rebuilt.
    pub async fn songs(&self) -> Result<Arc<SongList>> {
        self.signed_in_read(Missing::Song, || self.song_list())
            .await
    }

    async fn song_list(&self) -> Outcome<Arc<SongList>> {
        self.kept(
            |caches| &caches.songs,
            |library| async move { library.build_song_list().await }.boxed(),
        )
        .await
    }

    async fn build_song_list(&self) -> Outcome<SongList> {
        let mut list = SongList::default();
        // Library songs whose catalog id only their `catalog` relationship names.
        let mut unresolved = Vec::new();
        let library = async {
            let mut pages = self.client.library_song_pages(SONG_INCLUDE, PARALLEL_READS);
            while let Some(page) = pages.try_next().await? {
                for entry in &page {
                    list.extend(map::library_track(entry));
                    match (TrackKey::library(&entry.id), entry.store_id()) {
                        (Ok(key), Some(catalog_id)) => list.add_library_twin(catalog_id, &key),
                        (Ok(_), None) => unresolved.push(entry.id.clone()),
                        (Err(_), _) => {}
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
            .filter(map::joins_song_list)
            .map(|playlist| playlist.id)
            .collect();
        let mut tracks = stream::iter(joining)
            .map(|id| async move {
                let tracks = self.playlist_songs(&id).await?;
                Ok::<_, Error>(
                    tracks
                        .iter()
                        .filter_map(map::library_track)
                        .collect::<Vec<_>>(),
                )
            })
            .buffered(PARALLEL_READS);
        let mut extra = Vec::new();
        while let Some(songs) = tracks.try_next().await? {
            extra.extend(songs.into_iter().filter(|song| !list.lists(&song.key)));
        }
        drop(tracks);

        let catalog_copies = extra.iter().any(|song| !song.key.is_library());
        if catalog_copies && !unresolved.is_empty() {
            // Best effort: without these ids a catalog copy of such a song is listed too.
            if let Ok(songs) = self
                .client
                .library_songs_by_ids(&unresolved, &["catalog"])
                .await
            {
                for song in &songs {
                    if let (Ok(key), Some(catalog_id)) =
                        (TrackKey::library(&song.id), song.store_id())
                    {
                        list.add_library_twin(catalog_id, &key);
                    }
                }
            }
        }
        // Their catalog ids first, as when songs are patched in, so a rebuild and a patch
        // list the same songs.
        for song in &extra {
            if let Some(catalog_id) = song.catalog_id.as_deref().filter(|_| song.key.is_library()) {
                list.add_library_twin(catalog_id, &song.key);
            }
        }
        list.extend(extra);
        Ok(list)
    }

    /// The raw tracks of a library playlist.
    async fn playlist_songs(&self, id: &str) -> std::result::Result<Vec<LibrarySong>, Error> {
        match self.client.library_playlist_tracks(id).try_collect().await {
            Ok(tracks) => Ok(tracks),
            // Apple answers 404 for the tracks of an empty playlist.
            Err(Error::NotFound(_)) => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }

    /// A song of the kept list, without asking Apple.
    fn known_song(&self, key: &TrackKey) -> Option<Track> {
        self.caches.songs.peek()?.get(key).cloned()
    }

    /// Adds the songs a write put into the user's playlists to the kept list, at its end
    /// so paging stays stable. When they cannot be read, the list is rebuilt on its next
    /// read instead.
    async fn patch_song_list(&self, written: &[TrackKey]) {
        let Some(list) = self.caches.songs.peek() else {
            // A first build may be running, and may not see the write.
            self.caches.songs.invalidate();
            return;
        };
        let mut library = Vec::new();
        let mut catalog = Vec::new();
        for key in written.iter().filter(|key| !list.lists(key)) {
            match (key.library_id(), key.catalog_id()) {
                (Some(id), _) => library.push(id.to_owned()),
                (_, Some(id)) => catalog.push(id.to_owned()),
                _ => {}
            }
        }
        drop(list);
        if library.is_empty() && catalog.is_empty() {
            return;
        }
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
        self.caches.songs.patch(|list| {
            for entry in &library {
                if let (Ok(key), Some(catalog_id)) =
                    (TrackKey::library(&entry.id), entry.store_id())
                {
                    list.add_library_twin(catalog_id, &key);
                }
                list.extend(map::library_track(entry));
            }
            list.extend(catalog.iter().filter_map(map::catalog_track));
        });
    }

    /// One song: from the kept song list when it is listed there, else read from Apple.
    pub async fn track(&self, key: &TrackKey) -> Result<Track> {
        self.signed_in_read(Missing::Song, || async {
            match self.known_song(key) {
                Some(song) => Ok(song),
                None => self.read_song(key).await,
            }
        })
        .await
    }

    async fn read_song(&self, key: &TrackKey) -> Outcome<Track> {
        let found = match (key.library_id(), key.catalog_id()) {
            (_, Some(catalog_id)) => self
                .client
                .catalog_songs(&[catalog_id.to_owned()])
                .await
                .map(|songs| {
                    songs.first().and_then(map::catalog_track).map(|mut song| {
                        song.in_library = self
                            .caches
                            .songs
                            .peek()
                            .is_some_and(|list| list.in_library(key));
                        song
                    })
                }),
            (Some(library_id), _) => match self
                .client
                .library_songs_by_ids(&[library_id.to_owned()], SONG_INCLUDE)
                .await
            {
                Ok(songs) => Ok(songs.iter().find_map(map::library_track)),
                Err(Error::NotFound(_)) => Ok(None),
                Err(error) => Err(error),
            },
            _ => Ok(None),
        };
        // The list may know a song the lookup missed, or have been built meanwhile.
        match found {
            Ok(Some(song)) => Ok(song),
            Ok(None) => self
                .known_song(key)
                .ok_or_else(|| LibraryError::NotFound(Missing::Song).into()),
            Err(error) => self.known_song(key).ok_or(Failure::Apple(error)),
        }
    }

    // Likes ----------------------------------------------------------------

    /// Whether each of `keys` is liked. Ratings are read in batches of 100, four at a
    /// time, and kept for ten minutes; liking and unliking here keeps them current.
    pub async fn liked(&self, keys: &[TrackKey]) -> Result<HashMap<TrackKey, bool>> {
        self.signed_in_read(Missing::Song, || async {
            let mut ratings = self.caches.rated(keys, self.clock.now());
            let mut seen = HashSet::new();
            let unread: Vec<TrackKey> = keys
                .iter()
                .filter(|key| !ratings.contains_key(*key) && seen.insert(*key))
                .cloned()
                .collect();
            if !unread.is_empty() {
                let read = self.read_ratings(&unread).await?;
                // A like set while the read ran is newer than the read: the kept rating
                // wins over it.
                ratings.extend(self.caches.rated(&unread, self.clock.now()));
                for (key, value) in read {
                    ratings.entry(key).or_insert(value);
                }
            }
            Ok(keys
                .iter()
                .map(|key| (key.clone(), ratings.get(key) == Some(&1)))
                .collect())
        })
        .await
    }

    /// Reads the ratings of `keys` in batches and keeps them; unrated songs read 0.
    async fn read_ratings(&self, keys: &[TrackKey]) -> Outcome<HashMap<TrackKey, i8>> {
        let session = self.caches.session();
        let (library, catalog): (Vec<&TrackKey>, Vec<&TrackKey>) =
            keys.iter().partition(|key| key.is_library());
        let ids = |keys: Vec<&TrackKey>| -> Vec<String> {
            keys.into_iter().map(ToString::to_string).collect()
        };
        let library = ids(library);
        let catalog: Vec<String> = catalog
            .into_iter()
            .filter_map(|key| key.catalog_id().map(str::to_owned))
            .collect();
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
                    // Apple answers an unrated song's rating with an error or no data at
                    // all: both mean it is not liked.
                    Err(Error::NotFound(_)) => Ok((is_catalog, batch, Vec::new())),
                    Err(error) => Err(Failure::from(error)),
                }
            })
            .buffer_unordered(PARALLEL_READS)
            .try_collect()
            .await?;
        let mut read_ratings = HashMap::new();
        for (is_catalog, batch, found) in read {
            let key = |id: String| {
                if is_catalog {
                    TrackKey::catalog(id)
                } else {
                    TrackKey::library(id)
                }
            };
            for id in &batch {
                read_ratings.extend(key(id.clone()).ok().map(|key| (key, 0)));
            }
            for rating in found {
                let value = rating.attributes.and_then(|attributes| attributes.value);
                read_ratings.extend(key(rating.id).ok().map(|key| (key, value.unwrap_or(0))));
            }
        }
        self.caches
            .merge_read_ratings(session, self.clock.now(), &read_ratings);
        Ok(read_ratings)
    }

    /// Likes or unlikes a song: sets Apple's rating to 1, or removes it. Removing the
    /// rating of a song that has none counts as done. Liking a catalog song adds it to
    /// the library and Favorites on Apple's side, so it joins the song list.
    pub async fn set_liked(&self, key: &TrackKey, liked: bool) -> Result<()> {
        self.signed_in(Missing::Song, || async {
            let session = self.caches.session();
            match (key.library_id(), key.catalog_id(), liked) {
                (Some(id), _, true) => self
                    .client
                    .set_library_song_rating(id, RatingValue::Like)
                    .await
                    .map(drop)?,
                (_, Some(id), true) => self
                    .client
                    .set_catalog_song_rating(id, RatingValue::Like)
                    .await
                    .map(drop)?,
                (Some(id), _, false) => {
                    ignore_not_found(self.client.delete_library_song_rating(id).await)?;
                }
                (_, Some(id), false) => {
                    ignore_not_found(self.client.delete_catalog_song_rating(id).await)?;
                }
                _ => {}
            }
            let now = self.clock.now();
            let rating = i8::from(liked);
            self.caches.set_rating(session, now, key, rating);
            // The library's copy of a catalog song is the same song to the user.
            let twin = self
                .caches
                .songs
                .peek()
                .and_then(|list| list.library_twin(key));
            if let Some(twin) = twin {
                self.caches.set_rating(session, now, &twin, rating);
            }
            if liked && !key.is_library() {
                self.patch_song_list(slice::from_ref(key)).await;
            }
            Ok(())
        })
        .await
    }

    // Browsing -------------------------------------------------------------

    /// The library's artists.
    pub async fn artists(&self) -> Result<Vec<Artist>> {
        self.signed_in_read(Missing::Artist, || async {
            let artists: Vec<_> = self.client.library_artists().try_collect().await?;
            Ok(artists.iter().map(map::artist).collect())
        })
        .await
    }

    /// One library artist with their albums.
    pub async fn artist(&self, id: &str) -> Result<ArtistDetail> {
        self.signed_in_read(Missing::Artist, || async {
            let id = library_id(id, Missing::Artist)?;
            let (artist, albums) = try_join(
                self.client.library_artist(id),
                self.client
                    .library_artist_albums(id)
                    .try_collect::<Vec<_>>(),
            )
            .await?;
            let artist = map::artist(&artist);
            let albums = albums
                .iter()
                .map(|album| {
                    let mut album = map::album(album);
                    album.artist_id.get_or_insert_with(|| artist.id.clone());
                    album.artist.get_or_insert_with(|| artist.name.clone());
                    album
                })
                .collect();
            Ok(ArtistDetail { artist, albums })
        })
        .await
    }

    /// One library album with its songs.
    pub async fn album(&self, id: &str) -> Result<AlbumDetail> {
        self.signed_in_read(Missing::Album, || async {
            let id = library_id(id, Missing::Album)?;
            let (album, tracks) = try_join(
                self.client.library_album(id, &["artists"]),
                self.client.library_album_tracks(id).try_collect::<Vec<_>>(),
            )
            .await?;
            let album = map::album(&album);
            let tracks = tracks
                .iter()
                .filter_map(map::library_track)
                .map(|mut song| {
                    song.album_id.get_or_insert_with(|| album.id.clone());
                    if song.artist_id.is_none() && song.artist == album.artist {
                        song.artist_id.clone_from(&album.artist_id);
                    }
                    if song.artwork.is_none() {
                        song.artwork.clone_from(&album.artwork);
                    }
                    song
                })
                .collect();
            Ok(AlbumDetail { album, tracks })
        })
        .await
    }

    /// Every album of the library: recently added ones first, in Apple's order, then the
    /// rest. The list is kept for ten minutes, and refreshed in the background once old, so
    /// it is shared behind an `Arc`; the shorter lists of artists and playlists are read
    /// afresh on every call.
    pub async fn albums(&self) -> Result<Arc<Vec<Album>>> {
        self.signed_in_read(Missing::Album, || async {
            self.kept(
                |caches| &caches.albums,
                |library| async move { library.build_album_list().await }.boxed(),
            )
            .await
        })
        .await
    }

    async fn build_album_list(&self) -> Outcome<Vec<Album>> {
        let library = async {
            let mut library = Vec::new();
            let mut pages = self
                .client
                .library_album_pages(&["artists"], PARALLEL_READS);
            while let Some(page) = pages.try_next().await? {
                library.extend(page.iter().map(map::album));
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
        let by_id: HashMap<&str, &Album> = library
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
                    None => map::album(album),
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

    // Playlists ------------------------------------------------------------

    /// The library's playlists, with how far each can be edited.
    pub async fn playlists(&self) -> Result<Vec<Playlist>> {
        self.signed_in_read(Missing::Playlist, || async {
            let playlists: Vec<LibraryPlaylist> =
                self.client.library_playlists().try_collect().await?;
            // Apple may list a new playlist only a while after creating it.
            let mut created = self.caches.created();
            created.retain(|new| playlists.iter().all(|listed| listed.id != new.id));
            let mut playlists: Vec<Playlist> = playlists.iter().map(map::playlist).collect();
            playlists.extend(created.iter().cloned());
            Ok(playlists)
        })
        .await
    }

    /// The songs of a library playlist, in order; none for an empty one.
    pub async fn playlist_tracks(&self, id: &str) -> Result<Vec<Track>> {
        self.signed_in_read(Missing::Playlist, || async {
            let id = library_id(id, Missing::Playlist)?;
            let tracks = self.playlist_songs(id).await?;
            Ok(tracks.iter().filter_map(map::library_track).collect())
        })
        .await
    }

    /// Creates a playlist holding `tracks`, and lists it before Apple does.
    pub async fn create_playlist(&self, name: &str, tracks: &[TrackKey]) -> Result<Playlist> {
        self.signed_in(Missing::Playlist, || async {
            let refs: Vec<_> = tracks.iter().map(TrackKey::track_ref).collect();
            let session = self.caches.session();
            let created = self
                .client
                .create_library_playlist(name, None, &refs)
                .await?;
            let created = map::playlist(&created);
            self.caches.remember_created(session, created.clone());
            self.patch_song_list(tracks).await;
            Ok(created)
        })
        .await
    }

    /// Appends songs to a playlist. Apple's API cannot rename, reorder or remove, so
    /// nothing else can be changed. A playlist that cannot be edited is refused here,
    /// before the write, since Apple's 403 would read as an expired sign-in.
    pub async fn append_to_playlist(&self, playlist_id: &str, tracks: &[TrackKey]) -> Result<()> {
        self.signed_in(Missing::Playlist, || async {
            let id = library_id(playlist_id, Missing::Playlist)?;
            if tracks.is_empty() {
                return Ok(());
            }
            let playlist = self.client.library_playlist(id).await?;
            if !map::can_append(&playlist) {
                return Err(LibraryError::ReadOnlyPlaylist.into());
            }
            let refs: Vec<_> = tracks.iter().map(TrackKey::track_ref).collect();
            self.client
                .add_tracks_to_library_playlist(id, &refs)
                .await?;
            self.patch_song_list(tracks).await;
            Ok(())
        })
        .await
    }
}

/// An Apple library id (`l.` album, `r.` artist, `p.` playlist). Catalog ids name nothing
/// in the library.
fn library_id(id: &str, what: Missing) -> std::result::Result<&str, LibraryError> {
    if is_library_id(id) {
        Ok(id)
    } else {
        Err(LibraryError::NotFound(what))
    }
}

fn ignore_not_found(result: std::result::Result<(), Error>) -> std::result::Result<(), Error> {
    match result {
        Err(Error::NotFound(_)) => Ok(()),
        other => other,
    }
}

/// Whether an error says Apple does not take the tokens.
fn rejects_tokens(error: &Error) -> bool {
    matches!(
        error,
        Error::Unauthorized | Error::Forbidden | Error::MissingUserToken | Error::Token(_)
    )
}

/// Maps a client error without deciding what a 401 or 403 means: the library does that
/// beforehand, by reading with the same tokens.
fn library_error(error: Error, what: Missing) -> LibraryError {
    match error {
        Error::MissingUserToken | Error::Token(_) => LibraryError::SignedOut,
        Error::Unauthorized | Error::Forbidden => LibraryError::Refused,
        Error::NotFound(_) => LibraryError::NotFound(what),
        Error::RateLimited { .. } | Error::Server { .. } | Error::Http(_) => {
            LibraryError::Unavailable(error)
        }
        error => LibraryError::Apple(error),
    }
}

/// Ends a refresh in the background, however it ends.
struct EndRefresh<T: Clone + 'static> {
    caches: Arc<Caches>,
    slot: fn(&Caches) -> &Slot<T>,
}

impl<T: Clone> Drop for EndRefresh<T> {
    fn drop(&mut self) {
        (self.slot)(&self.caches).end_refresh();
    }
}

/// The tokens of the current sign-in, in memory only. Nothing newer can be fetched on
/// refresh, so a rejected token stays rejected until the host signs in again.
#[derive(Default)]
struct SessionTokens(Mutex<Option<Tokens>>);

impl SessionTokens {
    /// Keeps `tokens`, or none; false when they are the ones already kept.
    fn replace(&self, tokens: Option<Tokens>) -> bool {
        let mut current = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if *current == tokens {
            return false;
        }
        *current = tokens;
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
