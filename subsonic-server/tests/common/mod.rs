//! A fake with known data behind every facet, and a client wired to it over a local port.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Request, State};
use axum::middleware::{self, Next};
use axum::response::Response;
use bytes::Bytes;
use reqwest::header::{HeaderMap, HeaderValue};
use subsonic::api::prelude::*;
use subsonic::{
    ApiFuture, RuneAttribution, RuneEdit, RuneLoginCompletion, RuneLoginMethod, RuneLoginMethods,
    RuneLoginStart, RunePlayback, RunePlaybackMode, RunePlaybackReason, RuneReason,
    RuneRecommendationGroup, RuneRecommendationKind, RuneScope, RuneServiceDescriptor,
    RuneUnsupported, SearchLimits, StreamBody, StreamError, SubsonicAlbum, SubsonicAlbumDetail,
    SubsonicArtist, SubsonicArtistDetail, SubsonicClient, SubsonicCredentials, SubsonicError,
    SubsonicJukebox, SubsonicJukeboxPlaylist, SubsonicJukeboxStatus, SubsonicLyrics,
    SubsonicPlaylist, SubsonicPlaylistDetail, SubsonicProbe, SubsonicRoles, SubsonicScanStatus,
    SubsonicSearchResult, SubsonicServerError, SubsonicSong, SubsonicSyncedLine,
    SubsonicSyncedLyrics,
};
use url::Url;

pub const AUDIO: &[u8] = b"0123456789abcdef";
pub const FEED: &str = "http://feed.example/rune/feed";

pub fn ready<'a, T: Send + 'a>(value: Result<T, SubsonicError>) -> ApiFuture<'a, T> {
    Box::pin(async move { value })
}

pub fn server_error(kind: SubsonicServerError, message: &str) -> SubsonicError {
    SubsonicError::Server {
        kind,
        message: message.into(),
        rune_reason: None,
    }
}

pub fn not_found(what: &str) -> SubsonicError {
    server_error(SubsonicServerError::NotFound, &format!("{what} not found"))
}

pub fn songs() -> Vec<SubsonicSong> {
    vec![
        SubsonicSong {
            id: "s1".into(),
            title: "Opening \"Night\" & More".into(),
            album: Some("First Light".into()),
            artist: Some("Ada".into()),
            album_id: Some("al1".into()),
            artist_id: Some("ar1".into()),
            track: Some(3),
            disc: Some(2),
            year: Some(1999),
            genre: Some("Jazz".into()),
            duration: Some(215),
            size: Some(5_000_000_000),
            suffix: Some("flac".into()),
            content_type: Some("audio/flac".into()),
            cover_art: Some("c1".into()),
            bit_rate: Some(900),
            starred: Some("2024-01-02T03:04:05Z".into()),
            rune_playback: Some(RunePlayback {
                mode: RunePlaybackMode::Hosted,
                host: Some("appleMusic".into()),
                key: Some("i.abc".into()),
                reason: None,
            }),
            rune_in_library: Some(true),
        },
        SubsonicSong {
            id: "s2".into(),
            title: "Closed".into(),
            rune_playback: Some(RunePlayback {
                mode: RunePlaybackMode::Unavailable,
                host: None,
                key: None,
                reason: Some(RunePlaybackReason::Region),
            }),
            ..SubsonicSong::default()
        },
        SubsonicSong {
            id: "s3".into(),
            title: "Plain".into(),
            rune_playback: Some(RunePlayback {
                mode: RunePlaybackMode::Stream,
                host: None,
                key: None,
                reason: None,
            }),
            rune_in_library: Some(false),
            ..SubsonicSong::default()
        },
        SubsonicSong {
            id: "s4".into(),
            title: "Bare".into(),
            ..SubsonicSong::default()
        },
    ]
}

pub fn artists() -> Vec<SubsonicArtist> {
    vec![
        SubsonicArtist {
            id: "ar1".into(),
            name: "Ada".into(),
            album_count: Some(2),
            cover_art: Some("c1".into()),
            rune_in_library: Some(true),
        },
        SubsonicArtist {
            id: "ar2".into(),
            name: "Alma".into(),
            album_count: None,
            cover_art: None,
            rune_in_library: None,
        },
        SubsonicArtist {
            id: "ar3".into(),
            name: "Zed".into(),
            album_count: Some(1),
            cover_art: None,
            rune_in_library: Some(false),
        },
    ]
}

pub fn albums() -> Vec<SubsonicAlbum> {
    vec![
        SubsonicAlbum {
            id: "al1".into(),
            name: "First Light".into(),
            artist: Some("Ada".into()),
            artist_id: Some("ar1".into()),
            song_count: Some(2),
            duration: Some(400),
            year: Some(1999),
            genre: Some("Jazz".into()),
            cover_art: Some("c1".into()),
            rune_in_library: Some(true),
        },
        SubsonicAlbum {
            id: "al2".into(),
            name: "Second".into(),
            ..SubsonicAlbum::default()
        },
        SubsonicAlbum {
            id: "al3".into(),
            name: "Third".into(),
            ..SubsonicAlbum::default()
        },
    ]
}

pub fn artist_detail() -> SubsonicArtistDetail {
    SubsonicArtistDetail {
        id: "ar1".into(),
        name: "Ada".into(),
        album_count: Some(2),
        cover_art: Some("c1".into()),
        rune_in_library: Some(true),
        albums: albums()[..2].to_vec(),
    }
}

pub fn album_detail() -> SubsonicAlbumDetail {
    let album = albums().remove(0);
    SubsonicAlbumDetail {
        id: album.id,
        name: album.name,
        artist: album.artist,
        artist_id: album.artist_id,
        song_count: album.song_count,
        duration: album.duration,
        year: album.year,
        genre: album.genre,
        cover_art: album.cover_art,
        rune_in_library: album.rune_in_library,
        songs: songs()[..2].to_vec(),
    }
}

pub fn playlists() -> Vec<SubsonicPlaylist> {
    vec![
        SubsonicPlaylist {
            id: "p1".into(),
            name: "Mine".into(),
            song_count: Some(2),
            duration: Some(300),
            owner: Some("rune".into()),
            public: Some(false),
            cover_art: Some("c1".into()),
            rune_edit: Some(RuneEdit::Full),
            rune_in_library: Some(true),
        },
        SubsonicPlaylist {
            id: "p2".into(),
            name: "Theirs".into(),
            rune_edit: Some(RuneEdit::ReadOnly),
            ..SubsonicPlaylist::default()
        },
        SubsonicPlaylist {
            id: "p3".into(),
            name: "Appendable".into(),
            public: Some(true),
            rune_edit: Some(RuneEdit::AppendOnly),
            ..SubsonicPlaylist::default()
        },
        SubsonicPlaylist {
            id: "p4".into(),
            name: "Plain Server".into(),
            ..SubsonicPlaylist::default()
        },
    ]
}

pub fn playlist_detail() -> SubsonicPlaylistDetail {
    let playlist = playlists().remove(0);
    SubsonicPlaylistDetail {
        id: playlist.id,
        name: playlist.name,
        song_count: playlist.song_count,
        duration: playlist.duration,
        owner: playlist.owner,
        public: playlist.public,
        cover_art: playlist.cover_art,
        rune_edit: playlist.rune_edit,
        rune_in_library: playlist.rune_in_library,
        songs: songs()[..2].to_vec(),
    }
}

pub fn probe() -> SubsonicProbe {
    SubsonicProbe {
        api_version: "1.16.1".into(),
        open_subsonic: true,
        server_type: Some("fake".into()),
        server_version: Some("9.9".into()),
        extensions: vec![
            ("runeService".into(), vec![1]),
            ("runeVisualizerFeed".into(), vec![1]),
            ("songLyrics".into(), vec![1, 2]),
            ("runeCatalog".into(), vec![1]),
            ("runeRecommendations".into(), vec![1]),
        ],
        roles: SubsonicRoles {
            stream_role: true,
            download_role: false,
            jukebox_role: true,
        },
        roles_known: true,
    }
}

/// The probe of a server without Rune's catalogue and recommendations extensions.
pub fn plain_probe() -> SubsonicProbe {
    let mut probe = probe();
    probe
        .extensions
        .retain(|(name, _)| name != "runeCatalog" && name != "runeRecommendations");
    probe
}

/// Catalogue items, outside the user's library.
pub fn catalog_result() -> SubsonicSearchResult {
    SubsonicSearchResult {
        artists: vec![SubsonicArtist {
            id: "catalog:artist:9".into(),
            name: "Far Artist".into(),
            rune_in_library: Some(false),
            ..SubsonicArtist::default()
        }],
        albums: vec![SubsonicAlbum {
            id: "catalog:album:9".into(),
            name: "Far Album".into(),
            artist: Some("Far Artist".into()),
            rune_in_library: Some(false),
            ..SubsonicAlbum::default()
        }],
        songs: vec![SubsonicSong {
            id: "catalog:song:9".into(),
            title: "Far Song".into(),
            rune_in_library: Some(false),
            ..SubsonicSong::default()
        }],
    }
}

pub fn recommendation_groups() -> Vec<RuneRecommendationGroup> {
    vec![
        RuneRecommendationGroup {
            id: "g1".into(),
            title: "Made for You".into(),
            kind: RuneRecommendationKind::Personal,
            albums: vec![albums().remove(0), catalog_result().albums.remove(0)],
            playlists: vec![SubsonicPlaylist {
                id: "catalog:playlist:9".into(),
                name: "Far Mix".into(),
                rune_edit: Some(RuneEdit::ReadOnly),
                rune_in_library: Some(false),
                ..SubsonicPlaylist::default()
            }],
        },
        RuneRecommendationGroup {
            id: "g2".into(),
            title: "Recently Played".into(),
            kind: RuneRecommendationKind::Recent,
            albums: Vec::new(),
            playlists: vec![playlists().remove(0)],
        },
        RuneRecommendationGroup {
            id: "g3".into(),
            title: "Elsewhere".into(),
            kind: RuneRecommendationKind::Other,
            albums: Vec::new(),
            playlists: Vec::new(),
        },
    ]
}

pub fn descriptor() -> RuneServiceDescriptor {
    RuneServiceDescriptor {
        id: "fake".into(),
        name: "Fake Service".into(),
        attribution: Some(RuneAttribution {
            text: "Fake".into(),
            link: Some("https://fake.example".into()),
        }),
        login: RuneLoginMethods {
            methods: vec![
                RuneLoginMethod::Password,
                RuneLoginMethod::Platform,
                RuneLoginMethod::Browser,
                RuneLoginMethod::DeviceCode,
            ],
        },
        hosts: vec!["appleMusic".into(), "other".into()],
        unsupported: vec![
            RuneUnsupported {
                endpoint: "deletePlaylist".into(),
                param: None,
            },
            RuneUnsupported {
                endpoint: "updatePlaylist".into(),
                param: Some("songIndexToRemove".into()),
            },
        ],
    }
}

pub fn lyrics_by_song() -> SubsonicLyrics {
    SubsonicLyrics {
        plain_lyrics: Some("la la\nlo lo".into()),
        synced_lyrics: vec![
            SubsonicSyncedLyrics {
                lang: Some("en".into()),
                synced: true,
                lines: vec![
                    SubsonicSyncedLine {
                        start: 0,
                        value: "la la".into(),
                    },
                    SubsonicSyncedLine {
                        start: 1500,
                        value: "lo lo".into(),
                    },
                ],
            },
            SubsonicSyncedLyrics {
                lang: Some("und".into()),
                synced: false,
                lines: vec![
                    SubsonicSyncedLine {
                        start: 0,
                        value: "la la".into(),
                    },
                    SubsonicSyncedLine {
                        start: 0,
                        value: "lo lo".into(),
                    },
                ],
            },
        ],
    }
}

pub struct JukeboxState {
    pub songs: Vec<String>,
    pub current: Option<u32>,
    pub playing: bool,
    pub gain: f32,
    pub position: Option<u32>,
}

/// Everything the fake was asked to change, in order, as `name:detail`.
pub struct Fake {
    pub calls: Mutex<Vec<String>>,
    pub jukebox: Mutex<JukeboxState>,
    /// Probes as [`plain_probe`] instead of [`probe`].
    pub plain: bool,
}

impl Fake {
    pub fn new() -> Arc<Self> {
        Self::with_probe(false)
    }

    /// A fake whose probe declares neither `runeCatalog` nor `runeRecommendations`.
    pub fn plain() -> Arc<Self> {
        Self::with_probe(true)
    }

    fn with_probe(plain: bool) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            jukebox: Mutex::new(JukeboxState {
                songs: Vec::new(),
                current: None,
                playing: false,
                gain: 1.0,
                position: None,
            }),
            plain,
        })
    }

    fn current_probe(&self) -> SubsonicProbe {
        if self.plain { plain_probe() } else { probe() }
    }

    fn record(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    /// Ids like `code:42` and `reason:region` fail the way their names say.
    fn failure(id: &str) -> Option<SubsonicError> {
        if let Some(code) = id.strip_prefix("code:") {
            let code: i64 = code.parse().unwrap();
            return Some(server_error(code.into(), &format!("failed with {code}")));
        }
        let reason = match id.strip_prefix("reason:")? {
            "credentialsExpired" => RuneReason::CredentialsExpired,
            "notSubscribed" => RuneReason::NotSubscribed,
            "region" => RuneReason::Region,
            "removed" => RuneReason::Removed,
            "serviceLimit" => RuneReason::ServiceLimit,
            _ => RuneReason::Other,
        };
        Some(SubsonicError::Server {
            kind: SubsonicServerError::NotAuthorized,
            message: "refused".into(),
            rune_reason: Some(reason),
        })
    }

    fn jukebox_songs(&self, ids: &[String]) -> Vec<SubsonicSong> {
        let all = songs();
        ids.iter()
            .filter_map(|id| all.iter().find(|song| &song.id == id).cloned())
            .collect()
    }

    fn jukebox_status_now(&self) -> SubsonicJukeboxStatus {
        let state = self.jukebox.lock().unwrap();
        SubsonicJukeboxStatus {
            current_index: state.current,
            playing: state.playing,
            gain: state.gain,
            position_secs: state.position,
        }
    }

    fn jukebox_change(
        &self,
        call: String,
        change: impl FnOnce(&mut JukeboxState),
    ) -> ApiFuture<'_, ()> {
        self.record(call);
        change(&mut self.jukebox.lock().unwrap());
        ready(Ok(()))
    }
}

fn owned(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| (*id).to_owned()).collect()
}

impl System for Fake {
    fn probe<'a>(&'a self) -> ApiFuture<'a, SubsonicProbe> {
        ready(Ok(self.current_probe()))
    }

    fn extensions<'a>(&'a self) -> ApiFuture<'a, Vec<(String, Vec<u32>)>> {
        ready(Ok(self.current_probe().extensions))
    }
}

impl Browsing for Fake {
    fn artists<'a>(&'a self) -> ApiFuture<'a, Vec<SubsonicArtist>> {
        ready(Ok(artists()))
    }

    fn artist<'a>(&'a self, id: &'a str) -> ApiFuture<'a, SubsonicArtistDetail> {
        ready(match id {
            "ar1" => Ok(artist_detail()),
            _ => Err(not_found("artist")),
        })
    }

    fn album<'a>(&'a self, id: &'a str) -> ApiFuture<'a, SubsonicAlbumDetail> {
        ready(match id {
            "al1" => Ok(album_detail()),
            _ => Err(not_found("album")),
        })
    }

    fn song<'a>(&'a self, id: &'a str) -> ApiFuture<'a, SubsonicSong> {
        ready(match Fake::failure(id) {
            Some(error) => Err(error),
            None => songs()
                .into_iter()
                .find(|song| song.id == id)
                .ok_or_else(|| not_found("song")),
        })
    }
}

impl Lists for Fake {
    fn album_list<'a>(&'a self, size: u32, offset: u32) -> ApiFuture<'a, Vec<SubsonicAlbum>> {
        ready(Ok(albums()
            .into_iter()
            .skip(offset as usize)
            .take(size as usize)
            .collect()))
    }

    fn starred_songs<'a>(&'a self) -> ApiFuture<'a, Vec<SubsonicSong>> {
        ready(Ok(vec![songs().remove(0), songs().remove(3)]))
    }
}

impl Searching for Fake {
    fn search_with_song_offset<'a>(
        &'a self,
        query: &'a str,
        limits: SearchLimits,
        song_offset: u32,
    ) -> ApiFuture<'a, SubsonicSearchResult> {
        self.record(format!("search:{query}"));
        ready(Ok(SubsonicSearchResult {
            artists: artists()
                .into_iter()
                .take(limits.artists as usize)
                .collect(),
            albums: albums().into_iter().take(limits.albums as usize).collect(),
            songs: songs()
                .into_iter()
                .skip(song_offset as usize)
                .take(limits.songs as usize)
                .collect(),
        }))
    }
}

fn take<T>(items: Vec<T>, count: u32) -> Vec<T> {
    items.into_iter().take(count as usize).collect()
}

impl RuneCatalog for Fake {
    fn search_in_scope<'a>(
        &'a self,
        query: &'a str,
        limits: SearchLimits,
        song_offset: u32,
        scope: RuneScope,
    ) -> ApiFuture<'a, SubsonicSearchResult> {
        self.record(format!("search {}:{query}:{song_offset}", scope.as_str()));
        let library = SubsonicSearchResult {
            artists: artists(),
            albums: albums(),
            songs: songs(),
        };
        let found = match scope {
            RuneScope::Library => library,
            RuneScope::Catalog => catalog_result(),
            RuneScope::All => {
                let mut all = library;
                let catalog = catalog_result();
                all.artists.extend(catalog.artists);
                all.albums.extend(catalog.albums);
                all.songs.extend(catalog.songs);
                all
            }
        };
        ready(Ok(SubsonicSearchResult {
            artists: take(found.artists, limits.artists),
            albums: take(found.albums, limits.albums),
            songs: take(
                found.songs.into_iter().skip(song_offset as usize).collect(),
                limits.songs,
            ),
        }))
    }

    fn add_to_library<'a>(&'a self, ids: &'a [&'a str]) -> ApiFuture<'a, ()> {
        self.record(format!("add:{}", ids.join(",")));
        ready(match ids.iter().find_map(|id| Fake::failure(id)) {
            Some(error) => Err(error),
            None => Ok(()),
        })
    }
}

impl RuneRecommendations for Fake {
    fn recommendations<'a>(&'a self, count: u32) -> ApiFuture<'a, Vec<RuneRecommendationGroup>> {
        self.record(format!("recommendations:{count}"));
        ready(Ok(take(recommendation_groups(), count)))
    }
}

impl Playlists for Fake {
    fn playlists<'a>(&'a self) -> ApiFuture<'a, Vec<SubsonicPlaylist>> {
        ready(Ok(playlists()))
    }

    fn playlist<'a>(&'a self, id: &'a str) -> ApiFuture<'a, SubsonicPlaylistDetail> {
        ready(match id {
            "p1" => Ok(playlist_detail()),
            _ => Err(not_found("playlist")),
        })
    }

    fn create_playlist<'a>(&'a self, name: &'a str, song_ids: &'a [&'a str]) -> ApiFuture<'a, ()> {
        self.record(format!("create:{name}:{}", song_ids.join(",")));
        ready(Ok(()))
    }

    fn replace_playlist_songs<'a>(
        &'a self,
        playlist_id: &'a str,
        song_ids: &'a [&'a str],
    ) -> ApiFuture<'a, ()> {
        self.record(format!("replace:{playlist_id}:{}", song_ids.join(",")));
        ready(Ok(()))
    }

    fn update_playlist<'a>(
        &'a self,
        playlist_id: &'a str,
        name: Option<&'a str>,
        song_ids_to_add: &'a [&'a str],
        song_indexes_to_remove: &'a [u32],
    ) -> ApiFuture<'a, ()> {
        let removed: Vec<String> = song_indexes_to_remove.iter().map(u32::to_string).collect();
        self.record(format!(
            "update:{playlist_id}:{name:?}:{}:{}",
            song_ids_to_add.join(","),
            removed.join(",")
        ));
        ready(Ok(()))
    }

    fn delete_playlist<'a>(&'a self, playlist_id: &'a str) -> ApiFuture<'a, ()> {
        self.record(format!("delete:{playlist_id}"));
        ready(Ok(()))
    }
}

impl Annotation for Fake {
    fn star<'a>(&'a self, song_ids: &'a [&'a str]) -> ApiFuture<'a, ()> {
        self.record(format!("star:{}", song_ids.join(",")));
        ready(Ok(()))
    }

    fn unstar<'a>(&'a self, song_ids: &'a [&'a str]) -> ApiFuture<'a, ()> {
        self.record(format!("unstar:{}", song_ids.join(",")));
        ready(Ok(()))
    }

    fn scrobble<'a>(
        &'a self,
        id: &'a str,
        time: Option<u64>,
        submission: bool,
    ) -> ApiFuture<'a, ()> {
        self.record(format!("scrobble:{id}:{time:?}:{submission}"));
        ready(Ok(()))
    }
}

impl MediaRetrieval for Fake {
    fn stream<'a>(
        &'a self,
        song_id: &'a str,
        range: Option<(u64, Option<u64>)>,
    ) -> ApiFuture<'a, StreamBody, StreamError> {
        Box::pin(async move {
            match song_id {
                "s1" => {}
                "gone" => return Err(StreamError::TrackGone),
                "denied" => return Err(StreamError::CredentialsRejected),
                "unreachable" => return Err(StreamError::Unreachable),
                "unsupported" => return Err(StreamError::Unsupported),
                _ => return Err(StreamError::Rejected),
            }
            let mut headers = HeaderMap::new();
            headers.insert("content-type", HeaderValue::from_static("audio/flac"));
            headers.insert("accept-ranges", HeaderValue::from_static("bytes"));
            let Some((start, end)) = range else {
                headers.insert("content-length", HeaderValue::from(AUDIO.len()));
                return Ok(StreamBody::from_parts(
                    200,
                    headers,
                    Bytes::from_static(AUDIO),
                ));
            };
            let last = end
                .map_or(AUDIO.len() - 1, |end| end as usize)
                .min(AUDIO.len() - 1);
            headers.insert(
                "content-range",
                HeaderValue::from_str(&format!("bytes {start}-{last}/{}", AUDIO.len())).unwrap(),
            );
            let part = &AUDIO[start as usize..=last];
            headers.insert("content-length", HeaderValue::from(part.len()));
            Ok(StreamBody::from_parts(
                206,
                headers,
                Bytes::from_static(part),
            ))
        })
    }

    fn cover_art<'a>(&'a self, id: &'a str, size: Option<u32>) -> ApiFuture<'a, Vec<u8>> {
        ready(match id {
            "c1" => {
                let mut bytes = vec![0xff, 0xd8, 0xff, 0x01, 0xfe];
                bytes.extend(size.map(|size| size as u8));
                Ok(bytes)
            }
            "png" => Ok(vec![0x89, b'P', b'N', b'G', 0x0d]),
            "webp" => Ok(b"RIFF\x04\x00\x00\x00WEBPVP8 ".to_vec()),
            "blob" => Ok(vec![1, 2, 3]),
            _ => Err(not_found("cover art")),
        })
    }

    fn lyrics<'a>(&'a self, artist: &'a str, title: &'a str) -> ApiFuture<'a, SubsonicLyrics> {
        ready(match (artist, title) {
            ("Ada", "Opening") => Ok(SubsonicLyrics {
                plain_lyrics: Some("la la\nlo lo".into()),
                synced_lyrics: Vec::new(),
            }),
            ("Ada", "Silent") => Ok(SubsonicLyrics::default()),
            _ => Err(not_found("lyrics")),
        })
    }

    fn lyrics_by_song_id<'a>(&'a self, id: &'a str) -> ApiFuture<'a, SubsonicLyrics> {
        ready(match id {
            "s1" => Ok(lyrics_by_song()),
            "s2" => Ok(SubsonicLyrics::default()),
            "s3" => Ok(SubsonicLyrics {
                plain_lyrics: Some("only\nplain".into()),
                synced_lyrics: Vec::new(),
            }),
            _ => Err(not_found("lyrics")),
        })
    }
}

impl Jukebox for Fake {
    fn jukebox_get<'a>(&'a self) -> ApiFuture<'a, SubsonicJukebox> {
        let ids = self.jukebox.lock().unwrap().songs.clone();
        ready(Ok(SubsonicJukebox {
            playlist: SubsonicJukeboxPlaylist {
                songs: self.jukebox_songs(&ids),
            },
            status: self.jukebox_status_now(),
        }))
    }

    fn jukebox_status<'a>(&'a self) -> ApiFuture<'a, SubsonicJukeboxStatus> {
        ready(Ok(self.jukebox_status_now()))
    }

    fn jukebox_set<'a>(&'a self, ids: &'a [&'a str]) -> ApiFuture<'a, ()> {
        let ids = owned(ids);
        self.jukebox_change(format!("jukebox set:{}", ids.join(",")), |state| {
            state.songs = ids;
            state.current = None;
        })
    }

    fn jukebox_add<'a>(&'a self, ids: &'a [&'a str]) -> ApiFuture<'a, ()> {
        let ids = owned(ids);
        self.jukebox_change(format!("jukebox add:{}", ids.join(",")), |state| {
            state.songs.extend(ids);
        })
    }

    fn jukebox_remove<'a>(&'a self, index: u32) -> ApiFuture<'a, ()> {
        self.jukebox_change(format!("jukebox remove:{index}"), |state| {
            state.songs.remove(index as usize);
        })
    }

    fn jukebox_clear<'a>(&'a self) -> ApiFuture<'a, ()> {
        self.jukebox_change("jukebox clear".into(), |state| state.songs.clear())
    }

    fn jukebox_skip<'a>(&'a self, index: u32, offset_secs: u32) -> ApiFuture<'a, ()> {
        self.jukebox_change(format!("jukebox skip:{index}:{offset_secs}"), |state| {
            state.current = Some(index);
            state.position = Some(offset_secs);
        })
    }

    fn jukebox_start<'a>(&'a self) -> ApiFuture<'a, ()> {
        self.jukebox_change("jukebox start".into(), |state| state.playing = true)
    }

    fn jukebox_stop<'a>(&'a self) -> ApiFuture<'a, ()> {
        self.jukebox_change("jukebox stop".into(), |state| state.playing = false)
    }

    fn jukebox_shuffle<'a>(&'a self) -> ApiFuture<'a, ()> {
        self.jukebox_change("jukebox shuffle".into(), |state| state.songs.reverse())
    }

    fn jukebox_set_gain<'a>(&'a self, gain: f32) -> ApiFuture<'a, ()> {
        self.jukebox_change(format!("jukebox gain:{gain}"), |state| state.gain = gain)
    }
}

impl Scanning for Fake {
    fn scan_status<'a>(&'a self) -> ApiFuture<'a, SubsonicScanStatus> {
        ready(Ok(SubsonicScanStatus {
            scanning: true,
            count: Some(1234),
        }))
    }
}

impl RuneVisualizerFeed for Fake {
    fn rune_visualizer_feed_url<'a>(&'a self, _probe: &'a SubsonicProbe) -> ApiFuture<'a, Url> {
        ready(Ok(Url::parse(FEED).unwrap()))
    }
}

pub const BAD_TOKEN: &str = "secret-token";

impl RuneService for Fake {
    fn rune_service<'a>(&'a self) -> ApiFuture<'a, RuneServiceDescriptor> {
        ready(Ok(descriptor()))
    }

    fn start_rune_login<'a>(&'a self, method: RuneLoginMethod) -> ApiFuture<'a, RuneLoginStart> {
        ready(match method {
            RuneLoginMethod::Platform => Ok(RuneLoginStart::Platform),
            RuneLoginMethod::Browser => Ok(RuneLoginStart::Browser {
                url: "https://login.example/auth?x=1&y=2".into(),
                state: "st&te".into(),
            }),
            RuneLoginMethod::DeviceCode => Ok(RuneLoginStart::DeviceCode {
                user_code: "ABCD-EFGH".into(),
                verification_url: "https://login.example/device".into(),
                interval_secs: 7,
            }),
            RuneLoginMethod::Password => Ok(RuneLoginStart::Other),
            RuneLoginMethod::Other => Err(SubsonicError::Unsupported {
                endpoint: "startRuneLogin",
            }),
        })
    }

    fn complete_rune_login<'a>(&'a self, completion: &'a RuneLoginCompletion) -> ApiFuture<'a, ()> {
        self.record(format!("login:{completion:?}"));
        if let Some(token) = completion.token.as_deref() {
            self.record(format!("login token:{token}"));
        }
        if let Some(code) = completion.code.as_deref() {
            self.record(format!("login code:{code}"));
        }
        if let Some(state) = completion.state.as_deref() {
            self.record(format!("login state:{state}"));
        }
        if let Some(developer) = completion.developer_token.as_deref() {
            self.record(format!("login developer:{developer}"));
        }
        ready(if completion.token.as_deref() == Some(BAD_TOKEN) {
            // A careless server echoes its input; neither side may pass the secrets on.
            let field = |value: &Option<String>| value.clone().unwrap_or_default();
            Err(server_error(
                SubsonicServerError::WrongCredentials,
                &format!(
                    "rejected token={} code={} state={} developer={}",
                    field(&completion.token),
                    field(&completion.code),
                    field(&completion.state),
                    field(&completion.developer_token),
                ),
            ))
        } else {
            Ok(())
        })
    }
}

/// What the wire carried: request targets (path and query) and POST bodies.
#[derive(Default)]
pub struct Wire {
    pub targets: Mutex<Vec<String>>,
    pub bodies: Mutex<Vec<String>>,
}

async fn record(State(wire): State<Arc<Wire>>, request: Request, next: Next) -> Response {
    let target = request
        .uri()
        .path_and_query()
        .map(|target| target.to_string())
        .unwrap_or_default();
    wire.targets.lock().unwrap().push(target);
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    wire.bodies
        .lock()
        .unwrap()
        .push(String::from_utf8_lossy(&bytes).into_owned());
    next.run(Request::from_parts(parts, axum::body::Body::from(bytes)))
        .await
}

pub struct Served {
    pub base: Url,
    pub wire: Arc<Wire>,
}

impl Served {
    pub fn client(&self) -> SubsonicClient {
        self.client_with(SubsonicCredentials::Password {
            user: "rune".into(),
            password: "pass-word".into(),
        })
    }

    pub fn client_with(&self, credentials: SubsonicCredentials) -> SubsonicClient {
        SubsonicClient::new(self.base.clone(), credentials, reqwest::Client::new())
    }
}

/// Serves `api` through the facade on a free local port.
pub async fn serve(api: Arc<dyn subsonic::SubsonicApi>) -> Served {
    let wire = Arc::new(Wire::default());
    let app: Router =
        subsonic_server::router(api).layer(middleware::from_fn_with_state(wire.clone(), record));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Served { base, wire }
}
