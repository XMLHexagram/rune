//! The plain types the library layer hands out.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

/// Prefix of the string form of a catalog key.
const CATALOG_PREFIX: &str = "catalog:";
/// Prefix of Apple's library song ids.
const LIBRARY_SONG_PREFIX: &str = "i.";
/// Apple's crop code for a cover fitted into the requested box.
const COVER_CROP: &str = "bb";

/// Names a song for playing and rating: a song of the user's library, or a catalog song.
///
/// The string form is the library id (`i.…`) or `catalog:<id>`, and parsing it gives the
/// same key back.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TrackKey {
    library: bool,
    id: String,
}

impl TrackKey {
    /// The key of the library song with Apple's library id `id`, which starts with `i.`.
    pub fn library(id: impl Into<String>) -> Result<Self, ParseTrackKeyError> {
        let id = id.into();
        if is_library_song_id(&id) {
            Ok(Self { library: true, id })
        } else {
            Err(ParseTrackKeyError(id))
        }
    }

    /// The key of the catalog song with catalog id `id`, which is not empty and does not
    /// start with `catalog:`.
    pub fn catalog(id: impl Into<String>) -> Result<Self, ParseTrackKeyError> {
        let id = id.into();
        if id.is_empty() || id.starts_with(CATALOG_PREFIX) {
            Err(ParseTrackKeyError(id))
        } else {
            Ok(Self { library: false, id })
        }
    }

    /// Whether this names a song of the library.
    pub fn is_library(&self) -> bool {
        self.library
    }

    /// Apple's library id, for a library song.
    pub fn library_id(&self) -> Option<&str> {
        self.library.then_some(self.id.as_str())
    }

    /// The catalog id, for a catalog song.
    pub fn catalog_id(&self) -> Option<&str> {
        (!self.library).then_some(self.id.as_str())
    }
}

/// Whether an Apple id names something in the user's library: catalog ids name nothing there.
pub(super) fn is_library_id(id: &str) -> bool {
    !id.is_empty() && !id.starts_with(CATALOG_PREFIX)
}

/// Whether an Apple id names a song in the user's library.
fn is_library_song_id(id: &str) -> bool {
    id.len() > LIBRARY_SONG_PREFIX.len() && id.starts_with(LIBRARY_SONG_PREFIX)
}

impl fmt::Display for TrackKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.library {
            f.write_str(&self.id)
        } else {
            write!(f, "{CATALOG_PREFIX}{}", self.id)
        }
    }
}

/// The text is neither a library song id nor `catalog:<id>`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a track key: {0:?}")]
pub struct ParseTrackKeyError(String);

impl FromStr for TrackKey {
    type Err = ParseTrackKeyError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text.strip_prefix(CATALOG_PREFIX) {
            Some(id) => Self::catalog(id).map_err(|_| ParseTrackKeyError(text.to_owned())),
            None => Self::library(text),
        }
    }
}

/// An image Apple serves in any size up to a limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artwork {
    /// URL template with `{w}`, `{h}`, `{c}` and `{f}` placeholders.
    pub template: String,
    /// Largest side offered, in pixels.
    pub max_size: Option<u32>,
}

impl Artwork {
    /// The image URL for a square of `size` pixels, never above the largest size offered.
    pub fn url(&self, size: u32) -> String {
        let side = size.min(self.max_size.unwrap_or(u32::MAX)).max(1);
        let side = side.to_string();
        self.template
            .replace("{w}", &side)
            .replace("{h}", &side)
            .replace("{c}", COVER_CROP)
            .replace("{f}", "jpg")
    }
}

/// A song of the library or of the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    /// How the song is played and rated.
    pub key: TrackKey,
    /// The song's name.
    pub title: String,
    /// The album's name.
    pub album: Option<String>,
    /// The artist's name.
    pub artist: Option<String>,
    /// The library album's id, or the catalog album's for a catalog song.
    pub album_id: Option<String>,
    /// The library artist's id, or the catalog artist's for a catalog song.
    pub artist_id: Option<String>,
    /// Number on its disc.
    pub track_number: Option<u32>,
    /// Number of its disc.
    pub disc_number: Option<u32>,
    /// Year of release.
    pub year: Option<u32>,
    /// First genre.
    pub genre: Option<String>,
    /// Length of the song.
    pub duration: Option<Duration>,
    /// The cover.
    pub artwork: Option<Artwork>,
    /// Whether the song is in the user's library.
    pub in_library: bool,
    /// The catalog id of the song, when Apple names one.
    pub catalog_id: Option<String>,
}

/// An album in the user's library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Album {
    /// Apple's library id.
    pub id: String,
    /// The album's name.
    pub name: String,
    /// The artist's name.
    pub artist: Option<String>,
    /// The library artist's id.
    pub artist_id: Option<String>,
    /// Number of songs.
    pub track_count: Option<u32>,
    /// Year of release.
    pub year: Option<u32>,
    /// First genre.
    pub genre: Option<String>,
    /// The cover.
    pub artwork: Option<Artwork>,
    /// The catalog id of the album, when Apple names one.
    pub catalog_id: Option<String>,
}

/// An album with its songs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumDetail {
    /// The album.
    pub album: Album,
    /// Its songs, in order.
    pub tracks: Vec<Track>,
}

/// An artist in the user's library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artist {
    /// Apple's library id.
    pub id: String,
    /// The artist's name.
    pub name: String,
}

/// An artist with their albums in the library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistDetail {
    /// The artist.
    pub artist: Artist,
    /// Their albums.
    pub albums: Vec<Album>,
}

/// A playlist in the user's library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Playlist {
    /// Apple's library id.
    pub id: String,
    /// The playlist's name.
    pub name: String,
    /// Whether songs can be added. Apple's API cannot rename, reorder or remove, and
    /// Favorites and subscribed playlists accept nothing.
    pub can_append: bool,
    /// Whether the playlist is public.
    pub is_public: Option<bool>,
    /// The cover.
    pub artwork: Option<Artwork>,
}

/// The user's songs in order: library songs first, then the songs of the user's own
/// playlists and Favorites that are not in the library. No song is listed twice.
#[derive(Debug, Clone, Default)]
pub struct SongList {
    songs: Vec<Track>,
    index: HashMap<TrackKey, usize>,
    /// Library song ids by catalog id, so a catalog copy of a library song is not listed twice.
    library_twins: HashMap<String, String>,
}

impl SongList {
    /// The songs, in order.
    pub fn songs(&self) -> &[Track] {
        &self.songs
    }

    /// Number of songs.
    pub fn len(&self) -> usize {
        self.songs.len()
    }

    /// Whether there are no songs.
    pub fn is_empty(&self) -> bool {
        self.songs.is_empty()
    }

    /// The listed song with `key`.
    pub fn get(&self, key: &TrackKey) -> Option<&Track> {
        self.index.get(key).map(|&at| &self.songs[at])
    }

    /// Whether `key`, or the library song a catalog key stands for, is listed.
    pub fn lists(&self, key: &TrackKey) -> bool {
        self.index.contains_key(key) || self.library_twin(key).is_some()
    }

    /// Whether the song is in the user's library: a listed library song, or a catalog
    /// song the library has a copy of.
    pub fn in_library(&self, key: &TrackKey) -> bool {
        (key.is_library() && self.index.contains_key(key)) || self.library_twin(key).is_some()
    }

    /// The library song that is the library's copy of catalog song `key`.
    pub fn library_twin(&self, key: &TrackKey) -> Option<TrackKey> {
        let id = self.library_twins.get(key.catalog_id()?)?;
        TrackKey::library(id.clone()).ok()
    }

    pub(crate) fn push(&mut self, song: Track) {
        if !self.lists(&song.key) {
            self.index.insert(song.key.clone(), self.songs.len());
            self.songs.push(song);
        }
    }

    /// Records that catalog song `catalog_id` is the library song `library`.
    pub(crate) fn add_library_twin(&mut self, catalog_id: &str, library: &TrackKey) {
        if let Some(id) = library.library_id() {
            self.library_twins
                .entry(catalog_id.to_owned())
                .or_insert_with(|| id.to_owned());
        }
    }
}

impl Extend<Track> for SongList {
    fn extend<I: IntoIterator<Item = Track>>(&mut self, songs: I) {
        for song in songs {
            self.push(song);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_keys_round_trip_through_text() {
        for text in ["i.abc", "catalog:123", "catalog:i.abc", "catalog:a:b"] {
            let key: TrackKey = text.parse().unwrap();
            assert_eq!(key.to_string(), text);
        }
        let library = TrackKey::library("i.abc").unwrap();
        assert_eq!(library.library_id(), Some("i.abc"));
        assert_eq!(library.catalog_id(), None);
        let catalog = TrackKey::catalog("123").unwrap();
        assert_eq!(catalog.catalog_id(), Some("123"));
        assert!(!catalog.is_library());
        for key in [library, catalog, TrackKey::catalog("i.abc").unwrap()] {
            assert_eq!(key.to_string().parse(), Ok(key));
        }
        // Nothing that cannot be written back can be built.
        for id in ["", "i.", "a.123", "123", "catalog:1"] {
            assert!(TrackKey::library(id).is_err(), "{id}");
        }
        for id in ["", "catalog:1"] {
            assert!(TrackKey::catalog(id).is_err(), "{id}");
        }
        for text in [
            "",
            "i.",
            "catalog:",
            "catalog:catalog:1",
            "a.123",
            "123",
            "p.1",
            "l.1",
        ] {
            assert!(text.parse::<TrackKey>().is_err(), "{text}");
        }
    }

    #[test]
    fn artwork_fills_the_size_up_to_its_limit() {
        let artwork = Artwork {
            template: "https://a.invalid/{w}x{h}{c}.{f}".into(),
            max_size: Some(1000),
        };
        assert_eq!(artwork.url(300), "https://a.invalid/300x300bb.jpg");
        assert_eq!(artwork.url(3000), "https://a.invalid/1000x1000bb.jpg");
        assert_eq!(artwork.url(0), "https://a.invalid/1x1bb.jpg");
    }
}
