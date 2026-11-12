//! Subsonic API surface as facet traits, so the hub can talk to either the HTTP
//! client or an in-process server. Every method defaults to `Unsupported`.

use std::future::Future;
use std::pin::Pin;

use bytes::Bytes;
use reqwest::header::HeaderMap;
use url::Url;

use crate::{
    RuneLoginCompletion, RuneLoginMethod, RuneLoginStart, RuneRecommendationGroup, RuneScope,
    RuneServiceDescriptor, SearchLimits, StreamError, SubsonicAlbum, SubsonicAlbumDetail,
    SubsonicArtist, SubsonicArtistDetail, SubsonicClient, SubsonicError, SubsonicJukebox,
    SubsonicJukeboxStatus, SubsonicLyrics, SubsonicPlaylist, SubsonicPlaylistDetail, SubsonicProbe,
    SubsonicScanStatus, SubsonicSearchResult, SubsonicSong,
};

pub type ApiFuture<'a, T, E = SubsonicError> =
    Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'a>>;

fn unsupported<'a, T: Send + 'a>(endpoint: &'static str) -> ApiFuture<'a, T> {
    Box::pin(async move { Err(SubsonicError::Unsupported { endpoint }) })
}

fn stream_unsupported<'a>() -> ApiFuture<'a, StreamBody, StreamError> {
    Box::pin(async { Err(StreamError::Unsupported) })
}

/// A successful stream response, either from the HTTP client or assembled in process.
pub struct StreamBody(StreamBodyKind);

enum StreamBodyKind {
    Http(reqwest::Response),
    Parts {
        status: u16,
        headers: HeaderMap,
        body: Bytes,
    },
}

impl StreamBody {
    pub fn from_response(response: reqwest::Response) -> Self {
        Self(StreamBodyKind::Http(response))
    }

    pub fn from_parts(status: u16, headers: HeaderMap, body: Bytes) -> Self {
        Self(StreamBodyKind::Parts {
            status,
            headers,
            body,
        })
    }

    pub fn status(&self) -> u16 {
        match &self.0 {
            StreamBodyKind::Http(response) => response.status().as_u16(),
            StreamBodyKind::Parts { status, .. } => *status,
        }
    }

    pub fn headers(&self) -> &HeaderMap {
        match &self.0 {
            StreamBodyKind::Http(response) => response.headers(),
            StreamBodyKind::Parts { headers, .. } => headers,
        }
    }

    pub async fn bytes(self) -> Result<Bytes, StreamError> {
        match self.0 {
            StreamBodyKind::Http(response) => {
                response.bytes().await.map_err(|_| StreamError::Unreachable)
            }
            StreamBodyKind::Parts { body, .. } => Ok(body),
        }
    }

    pub fn into_response(self) -> reqwest::Response {
        match self.0 {
            StreamBodyKind::Http(response) => response,
            StreamBodyKind::Parts {
                status,
                headers,
                body,
            } => {
                let mut response = http::Response::new(body);
                *response.status_mut() = reqwest::StatusCode::from_u16(status)
                    .unwrap_or(reqwest::StatusCode::INTERNAL_SERVER_ERROR);
                *response.headers_mut() = headers;
                reqwest::Response::from(response)
            }
        }
    }
}

/// Server identity and capability discovery.
#[allow(unused_variables)]
pub trait System: Send + Sync {
    fn probe<'a>(&'a self) -> ApiFuture<'a, SubsonicProbe> {
        unsupported("ping")
    }

    /// The declared OpenSubsonic extensions, without authenticating.
    fn extensions<'a>(&'a self) -> ApiFuture<'a, Vec<(String, Vec<u32>)>> {
        unsupported("getOpenSubsonicExtensions")
    }
}

/// Library browsing by ID3 tags.
#[allow(unused_variables)]
pub trait Browsing: Send + Sync {
    fn artists<'a>(&'a self) -> ApiFuture<'a, Vec<SubsonicArtist>> {
        unsupported("getArtists")
    }

    fn artist<'a>(&'a self, id: &'a str) -> ApiFuture<'a, SubsonicArtistDetail> {
        unsupported("getArtist")
    }

    fn album<'a>(&'a self, id: &'a str) -> ApiFuture<'a, SubsonicAlbumDetail> {
        unsupported("getAlbum")
    }

    fn song<'a>(&'a self, id: &'a str) -> ApiFuture<'a, SubsonicSong> {
        unsupported("getSong")
    }
}

/// Album lists and starred items.
#[allow(unused_variables)]
pub trait Lists: Send + Sync {
    fn album_list<'a>(&'a self, size: u32, offset: u32) -> ApiFuture<'a, Vec<SubsonicAlbum>> {
        unsupported("getAlbumList2")
    }

    fn starred_songs<'a>(&'a self) -> ApiFuture<'a, Vec<SubsonicSong>> {
        unsupported("getStarred2")
    }
}

/// Library search.
#[allow(unused_variables)]
pub trait Searching: Send + Sync {
    fn search<'a>(
        &'a self,
        query: &'a str,
        limits: SearchLimits,
    ) -> ApiFuture<'a, SubsonicSearchResult> {
        self.search_with_song_offset(query, limits, 0)
    }

    fn search_with_song_offset<'a>(
        &'a self,
        query: &'a str,
        limits: SearchLimits,
        song_offset: u32,
    ) -> ApiFuture<'a, SubsonicSearchResult> {
        unsupported("search3")
    }
}

/// Playlist reads and edits.
#[allow(unused_variables)]
pub trait Playlists: Send + Sync {
    fn playlists<'a>(&'a self) -> ApiFuture<'a, Vec<SubsonicPlaylist>> {
        unsupported("getPlaylists")
    }

    fn playlist<'a>(&'a self, id: &'a str) -> ApiFuture<'a, SubsonicPlaylistDetail> {
        unsupported("getPlaylist")
    }

    fn create_playlist<'a>(&'a self, name: &'a str, song_ids: &'a [&'a str]) -> ApiFuture<'a, ()> {
        unsupported("createPlaylist")
    }

    fn replace_playlist_songs<'a>(
        &'a self,
        playlist_id: &'a str,
        song_ids: &'a [&'a str],
    ) -> ApiFuture<'a, ()> {
        unsupported("createPlaylist")
    }

    fn update_playlist<'a>(
        &'a self,
        playlist_id: &'a str,
        name: Option<&'a str>,
        song_ids_to_add: &'a [&'a str],
        song_indexes_to_remove: &'a [u32],
    ) -> ApiFuture<'a, ()> {
        unsupported("updatePlaylist")
    }

    fn delete_playlist<'a>(&'a self, playlist_id: &'a str) -> ApiFuture<'a, ()> {
        unsupported("deletePlaylist")
    }
}

/// Stars and listen reports.
#[allow(unused_variables)]
pub trait Annotation: Send + Sync {
    fn star<'a>(&'a self, song_ids: &'a [&'a str]) -> ApiFuture<'a, ()> {
        unsupported("star")
    }

    fn unstar<'a>(&'a self, song_ids: &'a [&'a str]) -> ApiFuture<'a, ()> {
        unsupported("unstar")
    }

    fn scrobble<'a>(
        &'a self,
        id: &'a str,
        time: Option<u64>,
        submission: bool,
    ) -> ApiFuture<'a, ()> {
        unsupported("scrobble")
    }
}

/// Audio, cover art and lyrics.
#[allow(unused_variables)]
pub trait MediaRetrieval: Send + Sync {
    fn stream<'a>(
        &'a self,
        song_id: &'a str,
        range: Option<(u64, Option<u64>)>,
    ) -> ApiFuture<'a, StreamBody, StreamError> {
        stream_unsupported()
    }

    fn cover_art<'a>(&'a self, id: &'a str, size: Option<u32>) -> ApiFuture<'a, Vec<u8>> {
        unsupported("getCoverArt")
    }

    fn lyrics<'a>(&'a self, artist: &'a str, title: &'a str) -> ApiFuture<'a, SubsonicLyrics> {
        unsupported("getLyrics")
    }

    fn lyrics_by_song_id<'a>(&'a self, id: &'a str) -> ApiFuture<'a, SubsonicLyrics> {
        unsupported("getLyricsBySongId")
    }
}

/// Server-side jukebox control.
#[allow(unused_variables)]
pub trait Jukebox: Send + Sync {
    fn jukebox_get<'a>(&'a self) -> ApiFuture<'a, SubsonicJukebox> {
        unsupported("jukeboxControl")
    }

    fn jukebox_status<'a>(&'a self) -> ApiFuture<'a, SubsonicJukeboxStatus> {
        unsupported("jukeboxControl")
    }

    fn jukebox_set<'a>(&'a self, ids: &'a [&'a str]) -> ApiFuture<'a, ()> {
        unsupported("jukeboxControl")
    }

    fn jukebox_add<'a>(&'a self, ids: &'a [&'a str]) -> ApiFuture<'a, ()> {
        unsupported("jukeboxControl")
    }

    fn jukebox_remove<'a>(&'a self, index: u32) -> ApiFuture<'a, ()> {
        unsupported("jukeboxControl")
    }

    fn jukebox_clear<'a>(&'a self) -> ApiFuture<'a, ()> {
        unsupported("jukeboxControl")
    }

    fn jukebox_skip<'a>(&'a self, index: u32, offset_secs: u32) -> ApiFuture<'a, ()> {
        unsupported("jukeboxControl")
    }

    fn jukebox_start<'a>(&'a self) -> ApiFuture<'a, ()> {
        unsupported("jukeboxControl")
    }

    fn jukebox_stop<'a>(&'a self) -> ApiFuture<'a, ()> {
        unsupported("jukeboxControl")
    }

    fn jukebox_shuffle<'a>(&'a self) -> ApiFuture<'a, ()> {
        unsupported("jukeboxControl")
    }

    fn jukebox_set_gain<'a>(&'a self, gain: f32) -> ApiFuture<'a, ()> {
        unsupported("jukeboxControl")
    }
}

/// Library scan state.
#[allow(unused_variables)]
pub trait Scanning: Send + Sync {
    fn scan_status<'a>(&'a self) -> ApiFuture<'a, SubsonicScanStatus> {
        unsupported("getScanStatus")
    }
}

/// Rune visualizer feed extension.
#[allow(unused_variables)]
pub trait RuneVisualizerFeed: Send + Sync {
    fn rune_visualizer_feed_url<'a>(&'a self, probe: &'a SubsonicProbe) -> ApiFuture<'a, Url> {
        unsupported("runeVisualizerFeed")
    }
}

/// Rune service extension: the service descriptor and generic sign-in.
#[allow(unused_variables)]
pub trait RuneService: Send + Sync {
    fn rune_service<'a>(&'a self) -> ApiFuture<'a, RuneServiceDescriptor> {
        unsupported("getRuneService")
    }

    fn start_rune_login<'a>(&'a self, method: RuneLoginMethod) -> ApiFuture<'a, RuneLoginStart> {
        unsupported("startRuneLogin")
    }

    fn complete_rune_login<'a>(&'a self, completion: &'a RuneLoginCompletion) -> ApiFuture<'a, ()> {
        unsupported("completeRuneLogin")
    }
}

/// Rune catalogue extension: search beyond the library and add catalogue items to it.
#[allow(unused_variables)]
pub trait RuneCatalog: Send + Sync {
    /// `search3` with `runeScope`, taking the same limits as
    /// [`Searching::search_with_song_offset`].
    fn search_in_scope<'a>(
        &'a self,
        query: &'a str,
        limits: SearchLimits,
        song_offset: u32,
        scope: RuneScope,
    ) -> ApiFuture<'a, SubsonicSearchResult> {
        unsupported("search3")
    }

    /// Adds catalogue songs, albums or playlists, by id, to the user's library.
    fn add_to_library<'a>(&'a self, ids: &'a [&'a str]) -> ApiFuture<'a, ()> {
        unsupported("addToRuneLibrary")
    }
}

/// Rune recommendations extension.
#[allow(unused_variables)]
pub trait RuneRecommendations: Send + Sync {
    /// Up to `count` titled groups of recommended albums and playlists.
    fn recommendations<'a>(&'a self, count: u32) -> ApiFuture<'a, Vec<RuneRecommendationGroup>> {
        unsupported("getRuneRecommendations")
    }
}

/// Everything the hub needs from a Subsonic-shaped server.
pub trait SubsonicApi:
    System
    + Browsing
    + Lists
    + Searching
    + Playlists
    + Annotation
    + MediaRetrieval
    + Jukebox
    + Scanning
    + RuneVisualizerFeed
    + RuneService
    + RuneCatalog
    + RuneRecommendations
    + Send
    + Sync
{
}

impl<T> SubsonicApi for T where
    T: System
        + Browsing
        + Lists
        + Searching
        + Playlists
        + Annotation
        + MediaRetrieval
        + Jukebox
        + Scanning
        + RuneVisualizerFeed
        + RuneService
        + RuneCatalog
        + RuneRecommendations
        + Send
        + Sync
{
}

pub mod prelude {
    pub use super::{
        Annotation, Browsing, Jukebox, Lists, MediaRetrieval, Playlists, RuneCatalog,
        RuneRecommendations, RuneService, RuneVisualizerFeed, Scanning, Searching, System,
    };
}

impl System for SubsonicClient {
    fn probe<'a>(&'a self) -> ApiFuture<'a, SubsonicProbe> {
        Box::pin(SubsonicClient::probe(self))
    }

    fn extensions<'a>(&'a self) -> ApiFuture<'a, Vec<(String, Vec<u32>)>> {
        Box::pin(SubsonicClient::extensions(self))
    }
}

impl Browsing for SubsonicClient {
    fn artists<'a>(&'a self) -> ApiFuture<'a, Vec<SubsonicArtist>> {
        Box::pin(SubsonicClient::artists(self))
    }

    fn artist<'a>(&'a self, id: &'a str) -> ApiFuture<'a, SubsonicArtistDetail> {
        Box::pin(SubsonicClient::artist(self, id))
    }

    fn album<'a>(&'a self, id: &'a str) -> ApiFuture<'a, SubsonicAlbumDetail> {
        Box::pin(SubsonicClient::album(self, id))
    }

    fn song<'a>(&'a self, id: &'a str) -> ApiFuture<'a, SubsonicSong> {
        Box::pin(SubsonicClient::song(self, id))
    }
}

impl Lists for SubsonicClient {
    fn album_list<'a>(&'a self, size: u32, offset: u32) -> ApiFuture<'a, Vec<SubsonicAlbum>> {
        Box::pin(SubsonicClient::album_list(self, size, offset))
    }

    fn starred_songs<'a>(&'a self) -> ApiFuture<'a, Vec<SubsonicSong>> {
        Box::pin(SubsonicClient::starred_songs(self))
    }
}

impl Searching for SubsonicClient {
    fn search<'a>(
        &'a self,
        query: &'a str,
        limits: SearchLimits,
    ) -> ApiFuture<'a, SubsonicSearchResult> {
        Box::pin(SubsonicClient::search(self, query, limits))
    }

    fn search_with_song_offset<'a>(
        &'a self,
        query: &'a str,
        limits: SearchLimits,
        song_offset: u32,
    ) -> ApiFuture<'a, SubsonicSearchResult> {
        Box::pin(SubsonicClient::search_with_song_offset(
            self,
            query,
            limits,
            song_offset,
        ))
    }
}

impl Playlists for SubsonicClient {
    fn playlists<'a>(&'a self) -> ApiFuture<'a, Vec<SubsonicPlaylist>> {
        Box::pin(SubsonicClient::playlists(self))
    }

    fn playlist<'a>(&'a self, id: &'a str) -> ApiFuture<'a, SubsonicPlaylistDetail> {
        Box::pin(SubsonicClient::playlist(self, id))
    }

    fn create_playlist<'a>(&'a self, name: &'a str, song_ids: &'a [&'a str]) -> ApiFuture<'a, ()> {
        Box::pin(SubsonicClient::create_playlist(self, name, song_ids))
    }

    fn replace_playlist_songs<'a>(
        &'a self,
        playlist_id: &'a str,
        song_ids: &'a [&'a str],
    ) -> ApiFuture<'a, ()> {
        Box::pin(SubsonicClient::replace_playlist_songs(
            self,
            playlist_id,
            song_ids,
        ))
    }

    fn update_playlist<'a>(
        &'a self,
        playlist_id: &'a str,
        name: Option<&'a str>,
        song_ids_to_add: &'a [&'a str],
        song_indexes_to_remove: &'a [u32],
    ) -> ApiFuture<'a, ()> {
        Box::pin(SubsonicClient::update_playlist(
            self,
            playlist_id,
            name,
            song_ids_to_add,
            song_indexes_to_remove,
        ))
    }

    fn delete_playlist<'a>(&'a self, playlist_id: &'a str) -> ApiFuture<'a, ()> {
        Box::pin(SubsonicClient::delete_playlist(self, playlist_id))
    }
}

impl Annotation for SubsonicClient {
    fn star<'a>(&'a self, song_ids: &'a [&'a str]) -> ApiFuture<'a, ()> {
        Box::pin(SubsonicClient::star(self, song_ids))
    }

    fn unstar<'a>(&'a self, song_ids: &'a [&'a str]) -> ApiFuture<'a, ()> {
        Box::pin(SubsonicClient::unstar(self, song_ids))
    }

    fn scrobble<'a>(
        &'a self,
        id: &'a str,
        time: Option<u64>,
        submission: bool,
    ) -> ApiFuture<'a, ()> {
        Box::pin(SubsonicClient::scrobble(self, id, time, submission))
    }
}

impl MediaRetrieval for SubsonicClient {
    fn stream<'a>(
        &'a self,
        song_id: &'a str,
        range: Option<(u64, Option<u64>)>,
    ) -> ApiFuture<'a, StreamBody, StreamError> {
        Box::pin(async move {
            SubsonicClient::stream(self, song_id, range)
                .await
                .map(StreamBody::from_response)
        })
    }

    fn cover_art<'a>(&'a self, id: &'a str, size: Option<u32>) -> ApiFuture<'a, Vec<u8>> {
        Box::pin(SubsonicClient::cover_art(self, id, size))
    }

    fn lyrics<'a>(&'a self, artist: &'a str, title: &'a str) -> ApiFuture<'a, SubsonicLyrics> {
        Box::pin(SubsonicClient::lyrics(self, artist, title))
    }

    fn lyrics_by_song_id<'a>(&'a self, id: &'a str) -> ApiFuture<'a, SubsonicLyrics> {
        Box::pin(SubsonicClient::lyrics_by_song_id(self, id))
    }
}

impl Jukebox for SubsonicClient {
    fn jukebox_get<'a>(&'a self) -> ApiFuture<'a, SubsonicJukebox> {
        Box::pin(SubsonicClient::jukebox_get(self))
    }

    fn jukebox_status<'a>(&'a self) -> ApiFuture<'a, SubsonicJukeboxStatus> {
        Box::pin(SubsonicClient::jukebox_status(self))
    }

    fn jukebox_set<'a>(&'a self, ids: &'a [&'a str]) -> ApiFuture<'a, ()> {
        Box::pin(SubsonicClient::jukebox_set(self, ids))
    }

    fn jukebox_add<'a>(&'a self, ids: &'a [&'a str]) -> ApiFuture<'a, ()> {
        Box::pin(SubsonicClient::jukebox_add(self, ids))
    }

    fn jukebox_remove<'a>(&'a self, index: u32) -> ApiFuture<'a, ()> {
        Box::pin(SubsonicClient::jukebox_remove(self, index))
    }

    fn jukebox_clear<'a>(&'a self) -> ApiFuture<'a, ()> {
        Box::pin(SubsonicClient::jukebox_clear(self))
    }

    fn jukebox_skip<'a>(&'a self, index: u32, offset_secs: u32) -> ApiFuture<'a, ()> {
        Box::pin(SubsonicClient::jukebox_skip(self, index, offset_secs))
    }

    fn jukebox_start<'a>(&'a self) -> ApiFuture<'a, ()> {
        Box::pin(SubsonicClient::jukebox_start(self))
    }

    fn jukebox_stop<'a>(&'a self) -> ApiFuture<'a, ()> {
        Box::pin(SubsonicClient::jukebox_stop(self))
    }

    fn jukebox_shuffle<'a>(&'a self) -> ApiFuture<'a, ()> {
        Box::pin(SubsonicClient::jukebox_shuffle(self))
    }

    fn jukebox_set_gain<'a>(&'a self, gain: f32) -> ApiFuture<'a, ()> {
        Box::pin(SubsonicClient::jukebox_set_gain(self, gain))
    }
}

impl Scanning for SubsonicClient {
    fn scan_status<'a>(&'a self) -> ApiFuture<'a, SubsonicScanStatus> {
        Box::pin(SubsonicClient::scan_status(self))
    }
}

impl RuneVisualizerFeed for SubsonicClient {
    fn rune_visualizer_feed_url<'a>(&'a self, probe: &'a SubsonicProbe) -> ApiFuture<'a, Url> {
        Box::pin(SubsonicClient::rune_visualizer_feed_url(self, probe))
    }
}

impl RuneService for SubsonicClient {
    fn rune_service<'a>(&'a self) -> ApiFuture<'a, RuneServiceDescriptor> {
        Box::pin(SubsonicClient::rune_service(self))
    }

    fn start_rune_login<'a>(&'a self, method: RuneLoginMethod) -> ApiFuture<'a, RuneLoginStart> {
        Box::pin(SubsonicClient::start_rune_login(self, method))
    }

    fn complete_rune_login<'a>(&'a self, completion: &'a RuneLoginCompletion) -> ApiFuture<'a, ()> {
        Box::pin(SubsonicClient::complete_rune_login(self, completion))
    }
}

impl RuneCatalog for SubsonicClient {
    fn search_in_scope<'a>(
        &'a self,
        query: &'a str,
        limits: SearchLimits,
        song_offset: u32,
        scope: RuneScope,
    ) -> ApiFuture<'a, SubsonicSearchResult> {
        Box::pin(SubsonicClient::search_in_scope(
            self,
            query,
            limits,
            song_offset,
            scope,
        ))
    }

    fn add_to_library<'a>(&'a self, ids: &'a [&'a str]) -> ApiFuture<'a, ()> {
        Box::pin(SubsonicClient::add_to_library(self, ids))
    }
}

impl RuneRecommendations for SubsonicClient {
    fn recommendations<'a>(&'a self, count: u32) -> ApiFuture<'a, Vec<RuneRecommendationGroup>> {
        Box::pin(SubsonicClient::recommendations(self, count))
    }
}
