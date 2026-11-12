//! The Subsonic facets of [`AppleMusicServer`]. Facets left at their defaults answer
//! `Unsupported`: streaming, lyrics, jukebox, playlist deletion and song replacement.

use subsonic::api::{
    Annotation, Browsing, Jukebox, Lists, MediaRetrieval, Playlists, RuneCatalog,
    RuneRecommendations, RuneService, RuneVisualizerFeed, Scanning, Searching, System,
};
use subsonic::{
    ApiFuture, RuneAttribution, RuneLoginCompletion, RuneLoginMethod, RuneLoginMethods,
    RuneLoginStart, RuneRecommendationGroup, RuneScope, RuneServiceDescriptor, RuneUnsupported,
    SearchLimits, SubsonicAlbum, SubsonicAlbumDetail, SubsonicArtist, SubsonicArtistDetail,
    SubsonicPlaylist, SubsonicPlaylistDetail, SubsonicProbe, SubsonicRoles, SubsonicScanStatus,
    SubsonicSearchResult, SubsonicSong,
};

use super::{map, AppleMusicServer};

/// The Subsonic API version Rune's client speaks.
const API_VERSION: &str = "1.16.1";
const SERVER_TYPE: &str = "rune-apple-music";

/// The OpenSubsonic extensions this server declares, with their versions.
fn declared_extensions() -> Vec<(String, Vec<u32>)> {
    ["runeService", "runeCatalog", "runeRecommendations"]
        .into_iter()
        .map(|name| (name.to_owned(), vec![1]))
        .collect()
}

fn ready<'a, T: Send + 'a>(value: T) -> ApiFuture<'a, T> {
    Box::pin(async move { Ok(value) })
}

impl System for AppleMusicServer {
    fn probe<'a>(&'a self) -> ApiFuture<'a, SubsonicProbe> {
        ready(SubsonicProbe {
            api_version: API_VERSION.into(),
            open_subsonic: true,
            server_type: Some(SERVER_TYPE.into()),
            server_version: Some(env!("CARGO_PKG_VERSION").into()),
            extensions: declared_extensions(),
            roles: SubsonicRoles {
                // Songs do play on this device, through the platform host, which is
                // what Rune's open mode asks; only the `stream` endpoint is unsupported.
                stream_role: true,
                download_role: false,
                jukebox_role: false,
            },
            roles_known: true,
        })
    }

    fn extensions<'a>(&'a self) -> ApiFuture<'a, Vec<(String, Vec<u32>)>> {
        ready(declared_extensions())
    }
}

impl RuneService for AppleMusicServer {
    fn rune_service<'a>(&'a self) -> ApiFuture<'a, RuneServiceDescriptor> {
        let unsupported = |endpoint: &str, param: Option<&str>| RuneUnsupported {
            endpoint: endpoint.into(),
            param: param.map(Into::into),
        };
        ready(RuneServiceDescriptor {
            id: "appleMusic".into(),
            name: "Apple Music".into(),
            attribution: Some(RuneAttribution {
                text: "Apple Music".into(),
                link: Some("https://music.apple.com".into()),
            }),
            login: RuneLoginMethods {
                methods: vec![RuneLoginMethod::Platform],
            },
            hosts: vec![map::HOST.into()],
            unsupported: vec![
                unsupported("getLyrics", None),
                unsupported("getLyricsBySongId", None),
                unsupported("deletePlaylist", None),
                unsupported("updatePlaylist", Some("name")),
                unsupported("updatePlaylist", Some("songIndexToRemove")),
                unsupported("createPlaylist", Some("playlistId")),
                unsupported("jukeboxControl", None),
                unsupported("stream", None),
            ],
        })
    }

    fn start_rune_login<'a>(&'a self, method: RuneLoginMethod) -> ApiFuture<'a, RuneLoginStart> {
        Box::pin(async move {
            match method {
                RuneLoginMethod::Platform => Ok(RuneLoginStart::Platform),
                _ => Err(map::unsupported("startRuneLogin")),
            }
        })
    }

    fn complete_rune_login<'a>(&'a self, completion: &'a RuneLoginCompletion) -> ApiFuture<'a, ()> {
        Box::pin(async move { self.sign_in(completion) })
    }
}

impl Browsing for AppleMusicServer {
    fn artists<'a>(&'a self) -> ApiFuture<'a, Vec<SubsonicArtist>> {
        Box::pin(self.signed_in(AppleMusicServer::artists(self)))
    }

    fn artist<'a>(&'a self, id: &'a str) -> ApiFuture<'a, SubsonicArtistDetail> {
        Box::pin(self.signed_in(AppleMusicServer::artist(self, id)))
    }

    fn album<'a>(&'a self, id: &'a str) -> ApiFuture<'a, SubsonicAlbumDetail> {
        Box::pin(self.signed_in(AppleMusicServer::album(self, id)))
    }

    fn song<'a>(&'a self, id: &'a str) -> ApiFuture<'a, SubsonicSong> {
        Box::pin(self.signed_in(AppleMusicServer::song(self, id)))
    }
}

impl Lists for AppleMusicServer {
    fn album_list<'a>(&'a self, size: u32, offset: u32) -> ApiFuture<'a, Vec<SubsonicAlbum>> {
        Box::pin(self.signed_in(self.album_page(size, offset)))
    }

    fn starred_songs<'a>(&'a self) -> ApiFuture<'a, Vec<SubsonicSong>> {
        Box::pin(self.signed_in(AppleMusicServer::starred_songs(self)))
    }
}

impl Searching for AppleMusicServer {
    fn search_with_song_offset<'a>(
        &'a self,
        query: &'a str,
        limits: SearchLimits,
        song_offset: u32,
    ) -> ApiFuture<'a, SubsonicSearchResult> {
        Box::pin(self.signed_in(AppleMusicServer::search(self, query, limits, song_offset)))
    }
}

impl RuneCatalog for AppleMusicServer {
    fn search_in_scope<'a>(
        &'a self,
        query: &'a str,
        limits: SearchLimits,
        song_offset: u32,
        scope: RuneScope,
    ) -> ApiFuture<'a, SubsonicSearchResult> {
        Box::pin(self.signed_in(AppleMusicServer::search_in_scope(
            self,
            query,
            limits,
            song_offset,
            scope,
        )))
    }

    fn add_to_library<'a>(&'a self, ids: &'a [&'a str]) -> ApiFuture<'a, ()> {
        Box::pin(self.signed_in(AppleMusicServer::add_to_library(self, ids)))
    }
}

impl RuneRecommendations for AppleMusicServer {
    fn recommendations<'a>(&'a self, count: u32) -> ApiFuture<'a, Vec<RuneRecommendationGroup>> {
        Box::pin(self.signed_in(AppleMusicServer::recommendations(self, count)))
    }
}

impl Playlists for AppleMusicServer {
    fn playlists<'a>(&'a self) -> ApiFuture<'a, Vec<SubsonicPlaylist>> {
        Box::pin(self.signed_in(AppleMusicServer::playlists(self)))
    }

    fn playlist<'a>(&'a self, id: &'a str) -> ApiFuture<'a, SubsonicPlaylistDetail> {
        Box::pin(self.signed_in(AppleMusicServer::playlist(self, id)))
    }

    fn create_playlist<'a>(&'a self, name: &'a str, song_ids: &'a [&'a str]) -> ApiFuture<'a, ()> {
        Box::pin(self.signed_in(AppleMusicServer::create_playlist(self, name, song_ids)))
    }

    fn update_playlist<'a>(
        &'a self,
        playlist_id: &'a str,
        name: Option<&'a str>,
        song_ids_to_add: &'a [&'a str],
        song_indexes_to_remove: &'a [u32],
    ) -> ApiFuture<'a, ()> {
        Box::pin(self.signed_in(self.append_to_playlist(
            playlist_id,
            name,
            song_ids_to_add,
            song_indexes_to_remove,
        )))
    }
}

impl Annotation for AppleMusicServer {
    fn star<'a>(&'a self, song_ids: &'a [&'a str]) -> ApiFuture<'a, ()> {
        Box::pin(self.signed_in(self.rate(song_ids, true)))
    }

    fn unstar<'a>(&'a self, song_ids: &'a [&'a str]) -> ApiFuture<'a, ()> {
        Box::pin(self.signed_in(self.rate(song_ids, false)))
    }

    /// Apple records plays made through MusicKit itself.
    fn scrobble<'a>(&'a self, _: &'a str, _: Option<u64>, _: bool) -> ApiFuture<'a, ()> {
        ready(())
    }
}

impl MediaRetrieval for AppleMusicServer {
    fn cover_art<'a>(&'a self, id: &'a str, size: Option<u32>) -> ApiFuture<'a, Vec<u8>> {
        Box::pin(AppleMusicServer::cover_art(self, id, size))
    }
}

impl Scanning for AppleMusicServer {
    fn scan_status<'a>(&'a self) -> ApiFuture<'a, SubsonicScanStatus> {
        Box::pin(self.signed_in(AppleMusicServer::scan_status(self)))
    }
}

impl Jukebox for AppleMusicServer {}

impl RuneVisualizerFeed for AppleMusicServer {}
