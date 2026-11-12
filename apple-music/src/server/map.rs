//! The server's id scheme, and how Apple Music resources and errors become Subsonic ones.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use reqwest::StatusCode;
use subsonic::{
    RuneEdit, RunePlayback, RunePlaybackMode, RuneReason, SubsonicAlbum, SubsonicArtist,
    SubsonicError, SubsonicPlaylist, SubsonicServerError, SubsonicSong,
};

use crate::error::Error;
use crate::models::{
    Artwork, LibraryAlbum, LibraryArtist, LibraryPlaylist, LibrarySong, LibrarySongAttributes,
    Relationship, Song, SongAttributes, TrackRef,
};

/// The platform host family that plays this server's songs.
pub(super) const HOST: &str = "appleMusic";

const CATALOG_SONG: &str = "catalog:song:";
const CATALOG: &str = "catalog:";
const LIBRARY_SONG: &str = "i.";
const ARTWORK: &str = "art:";

/// Cover size used when the caller asks for none.
const DEFAULT_COVER_SIZE: u32 = 1200;
/// Apple's crop code for a cover fitted into the requested box.
const COVER_CROP: &str = "bb";

/// Apple keeps no time for a rating, and Rune only reads whether `starred` is set.
const STARRED: &str = "1970-01-01T00:00:00Z";

/// A song id of this server: an Apple library id, or a prefixed catalog id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SongRef<'a> {
    Library(&'a str),
    Catalog(&'a str),
}

impl<'a> SongRef<'a> {
    pub(super) fn parse(id: &'a str) -> Result<Self, SubsonicError> {
        if let Some(catalog) = id.strip_prefix(CATALOG_SONG) {
            if !catalog.is_empty() {
                return Ok(Self::Catalog(catalog));
            }
        }
        library_id(id, "song").map(Self::Library)
    }

    pub(super) fn track(self) -> TrackRef {
        match self {
            Self::Library(id) => TrackRef::library_song(id),
            Self::Catalog(id) => TrackRef::song(id),
        }
    }
}

/// The song id for a catalog id.
pub(super) fn catalog_song_id(catalog_id: &str) -> String {
    format!("{CATALOG_SONG}{catalog_id}")
}

/// Whether an Apple id names a song in the user's library.
pub(super) fn is_library_song_id(id: &str) -> bool {
    id.starts_with(LIBRARY_SONG)
}

/// An Apple library id (`l.` album, `r.` artist, `p.` playlist, `i.` song). Catalog ids
/// name nothing in the library.
pub(super) fn library_id<'a>(id: &'a str, what: &str) -> Result<&'a str, SubsonicError> {
    if id.is_empty() || id.starts_with(CATALOG) || id.starts_with(ARTWORK) {
        return Err(not_found(what));
    }
    Ok(id)
}

/// Artwork URL templates by cover art id. Ids hash the template, so songs of one
/// album share a cover.
#[derive(Default)]
pub(super) struct Artworks(Mutex<HashMap<String, Template>>);

struct Template {
    url: String,
    width: Option<u32>,
}

impl Artworks {
    pub(super) fn register(&self, artwork: Option<&Artwork>) -> Option<String> {
        let artwork = artwork.filter(|artwork| !artwork.url.is_empty())?;
        let id = format!("{ARTWORK}{:016x}", fnv1a(artwork.url.as_bytes()));
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(id.clone())
            .or_insert_with(|| Template {
                url: artwork.url.clone(),
                width: artwork.width,
            });
        Some(id)
    }

    /// The image URL for a cover art id, `size` pixels square, never above the
    /// largest size Apple offers.
    pub(super) fn url(&self, id: &str, size: Option<u32>) -> Option<String> {
        let templates = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let template = templates.get(id)?;
        let side = size
            .unwrap_or(DEFAULT_COVER_SIZE)
            .min(template.width.unwrap_or(u32::MAX))
            .max(1)
            .to_string();
        Some(
            template
                .url
                .replace("{w}", &side)
                .replace("{h}", &side)
                .replace("{c}", COVER_CROP)
                .replace("{f}", "jpg"),
        )
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The attributes library and catalog songs share.
struct Track<'a> {
    name: Option<&'a str>,
    album: Option<&'a str>,
    artist: Option<&'a str>,
    track: Option<u32>,
    disc: Option<u32>,
    release_date: Option<&'a str>,
    genres: &'a [String],
    duration_ms: Option<u64>,
    artwork: Option<&'a Artwork>,
}

impl<'a> From<&'a LibrarySongAttributes> for Track<'a> {
    fn from(attributes: &'a LibrarySongAttributes) -> Self {
        Self {
            name: attributes.name.as_deref(),
            album: attributes.album_name.as_deref(),
            artist: attributes.artist_name.as_deref(),
            track: attributes.track_number,
            disc: attributes.disc_number,
            release_date: attributes.release_date.as_deref(),
            genres: &attributes.genre_names,
            duration_ms: attributes.duration_in_millis,
            artwork: attributes.artwork.as_ref(),
        }
    }
}

impl<'a> From<&'a SongAttributes> for Track<'a> {
    fn from(attributes: &'a SongAttributes) -> Self {
        Self {
            name: attributes.name.as_deref(),
            album: attributes.album_name.as_deref(),
            artist: attributes.artist_name.as_deref(),
            track: attributes.track_number,
            disc: attributes.disc_number,
            release_date: attributes.release_date.as_deref(),
            genres: &attributes.genre_names,
            duration_ms: attributes.duration_in_millis,
            artwork: attributes.artwork.as_ref(),
        }
    }
}

fn song(
    artworks: &Artworks,
    id: String,
    key: String,
    track: Track<'_>,
    album_id: Option<String>,
    artist_id: Option<String>,
) -> SubsonicSong {
    SubsonicSong {
        id,
        title: track.name.unwrap_or_default().to_owned(),
        album: track.album.map(str::to_owned),
        artist: track.artist.map(str::to_owned),
        album_id,
        artist_id,
        track: track.track,
        disc: track.disc,
        year: year(track.release_date),
        genre: track.genres.first().cloned(),
        duration: track
            .duration_ms
            .and_then(|ms| u32::try_from(ms.saturating_add(500) / 1000).ok()),
        cover_art: artworks.register(track.artwork),
        rune_playback: Some(RunePlayback {
            mode: RunePlaybackMode::Hosted,
            host: Some(HOST.to_owned()),
            key: Some(key),
            reason: None,
        }),
        ..SubsonicSong::default()
    }
}

/// A library song or playlist track; `None` for a music video. Entries that are not
/// library songs (playlist tracks outside the library) become catalog songs when their
/// catalog id is known, so they play and rate as such.
pub(super) fn library_song(entry: &LibrarySong, artworks: &Artworks) -> Option<SubsonicSong> {
    if entry.kind.contains("music-video") {
        return None;
    }
    let (id, key) = match entry.store_id() {
        Some(catalog) if !is_library_song_id(&entry.id) => {
            (catalog_song_id(catalog), format!("{CATALOG}{catalog}"))
        }
        _ => (entry.id.clone(), entry.id.clone()),
    };
    let relationships = entry.relationships.as_ref();
    let album_id = first_id(relationships.and_then(|r| r.albums.as_ref()));
    let artist_id = first_id(relationships.and_then(|r| r.artists.as_ref()));
    let attributes = entry.attributes.clone().unwrap_or_default();
    Some(song(
        artworks,
        id,
        key,
        Track::from(&attributes),
        album_id,
        artist_id,
    ))
}

fn first_id<A>(relationship: Option<&Relationship<A>>) -> Option<String> {
    relationship
        .and_then(|relationship| relationship.data.first())
        .map(|resource| resource.id.clone())
}

/// A catalog song. It has no library album or artist to browse.
pub(super) fn catalog_song(entry: &Song, artworks: &Artworks) -> SubsonicSong {
    let attributes = entry.attributes.clone().unwrap_or_default();
    song(
        artworks,
        catalog_song_id(&entry.id),
        format!("{CATALOG}{}", entry.id),
        Track::from(&attributes),
        None,
        None,
    )
}

pub(super) fn album(entry: &LibraryAlbum, artworks: &Artworks) -> SubsonicAlbum {
    let attributes = entry.attributes.clone().unwrap_or_default();
    SubsonicAlbum {
        id: entry.id.clone(),
        name: attributes.name.unwrap_or_default(),
        artist: attributes.artist_name,
        artist_id: entry
            .relationships
            .as_ref()
            .and_then(|relationships| relationships.pointer("/artists/data/0/id"))
            .and_then(|id| id.as_str())
            .map(str::to_owned),
        song_count: attributes.track_count,
        duration: None,
        year: year(attributes.release_date.as_deref()),
        genre: attributes.genre_names.first().cloned(),
        cover_art: artworks.register(attributes.artwork.as_ref()),
    }
}

pub(super) fn artist(entry: &LibraryArtist) -> SubsonicArtist {
    SubsonicArtist {
        id: entry.id.clone(),
        name: entry
            .attributes
            .as_ref()
            .and_then(|attributes| attributes.name.clone())
            .unwrap_or_default(),
        album_count: None,
        cover_art: None,
    }
}

pub(super) fn playlist(entry: &LibraryPlaylist, artworks: &Artworks) -> SubsonicPlaylist {
    let attributes = entry.attributes.clone().unwrap_or_default();
    SubsonicPlaylist {
        id: entry.id.clone(),
        name: attributes.name.clone().unwrap_or_default(),
        song_count: None,
        duration: None,
        owner: None,
        public: attributes.is_public,
        cover_art: artworks.register(attributes.artwork.as_ref()),
        // Apple's API can only append to a playlist; Favorites and subscribed ones not even that.
        rune_edit: Some(if can_append(entry) {
            RuneEdit::AppendOnly
        } else {
            RuneEdit::ReadOnly
        }),
    }
}

pub(super) fn can_append(entry: &LibraryPlaylist) -> bool {
    entry
        .attributes
        .as_ref()
        .is_some_and(|attributes| attributes.can_edit == Some(true))
}

/// Whether a playlist's songs belong to the user's own music: playlists the user made
/// (editable, or without a catalog counterpart, as Favorites) do; subscribed catalog
/// playlists do not.
pub(super) fn joins_song_union(entry: &LibraryPlaylist) -> bool {
    let attributes = entry.attributes.as_ref();
    can_append(entry) || attributes.and_then(|attributes| attributes.has_catalog) != Some(true)
}

pub(super) fn starred(rating: Option<i8>) -> Option<String> {
    (rating == Some(1)).then(|| STARRED.to_owned())
}

fn year(release_date: Option<&str>) -> Option<u32> {
    release_date?.get(..4)?.parse().ok()
}

pub(super) fn not_found(what: &str) -> SubsonicError {
    SubsonicError::Server {
        kind: SubsonicServerError::NotFound,
        message: format!("Apple Music {what} not found"),
        rune_reason: None,
    }
}

pub(super) fn credentials_expired() -> SubsonicError {
    SubsonicError::Server {
        kind: SubsonicServerError::NotAuthorized,
        message: "sign in to Apple Music".into(),
        rune_reason: Some(RuneReason::CredentialsExpired),
    }
}

pub(super) fn unsupported(endpoint: &'static str) -> SubsonicError {
    SubsonicError::Unsupported { endpoint }
}

/// Maps client errors so the hub's handling of Subsonic errors applies. A 401 or 403
/// maps to a refused request here; the server decides beforehand whether it means an
/// expired sign-in. Messages carry no tokens: Apple's error bodies and the client's own
/// messages never contain them.
pub(super) fn subsonic_error(error: Error) -> SubsonicError {
    match error {
        Error::MissingUserToken | Error::Token(_) => credentials_expired(),
        Error::Unauthorized | Error::Forbidden => SubsonicError::Server {
            kind: SubsonicServerError::Generic,
            message: "Apple Music refused the request".into(),
            rune_reason: None,
        },
        Error::NotFound(_) => not_found("item"),
        Error::RateLimited { .. } => SubsonicError::Server {
            kind: SubsonicServerError::Generic,
            message: error.to_string(),
            rune_reason: Some(RuneReason::ServiceLimit),
        },
        Error::Server { status, .. } => SubsonicError::HttpStatus(
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
        ),
        Error::ClientNotRegistered | Error::Api { .. } => SubsonicError::Server {
            kind: SubsonicServerError::Generic,
            message: error.to_string(),
            rune_reason: None,
        },
        Error::Http(error) => SubsonicError::Transport(error.without_url().to_string()),
        Error::Decode(error) => SubsonicError::Malformed(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn song_ids_parse_by_kind() {
        assert_eq!(SongRef::parse("i.abc"), Ok(SongRef::Library("i.abc")));
        assert_eq!(
            SongRef::parse("catalog:song:123"),
            Ok(SongRef::Catalog("123"))
        );
        for id in ["", "catalog:song:", "catalog:album:1", "art:00"] {
            assert!(
                matches!(
                    SongRef::parse(id),
                    Err(SubsonicError::Server {
                        kind: SubsonicServerError::NotFound,
                        ..
                    })
                ),
                "{id}"
            );
        }
    }

    #[test]
    fn covers_share_an_id_per_template_and_fill_the_size() {
        let artworks = Artworks::default();
        let artwork = |url: &str| Artwork {
            width: Some(1000),
            height: Some(1000),
            url: url.into(),
            bg_color: None,
            text_color1: None,
            text_color2: None,
            text_color3: None,
            text_color4: None,
        };
        let a = artworks.register(Some(&artwork("https://a.invalid/{w}x{h}{c}.{f}")));
        let b = artworks.register(Some(&artwork("https://a.invalid/{w}x{h}{c}.{f}")));
        let c = artworks.register(Some(&artwork("https://c.invalid/{w}x{h}.jpg")));
        assert_eq!(a, b);
        assert_ne!(a, c);
        let a = a.unwrap();
        assert_eq!(
            artworks.url(&a, Some(300)).unwrap(),
            "https://a.invalid/300x300bb.jpg"
        );
        assert_eq!(
            artworks.url(&a, None).unwrap(),
            "https://a.invalid/1000x1000bb.jpg"
        );
        assert_eq!(artworks.url("art:unknown", None), None);
        assert_eq!(artworks.register(Some(&artwork(""))), None);
    }
}
