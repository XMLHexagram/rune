//! The plain types the library layer hands out.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

/// Prefix of the string form of a catalog key.
pub(super) const CATALOG_PREFIX: &str = "catalog:";
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

/// Names an album, artist or playlist of the user's library or of Apple's catalog.
///
/// Songs are named by [`TrackKey`], which also says how a song plays and rates and links a
/// catalog song to the library's copy of it. Albums, artists and playlists are only named,
/// so this just keeps Apple's id and which of the two it is.
///
/// The string form is the library id, or `catalog:<id>`, and parsing it gives the same
/// item back.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ItemId {
    catalog: bool,
    id: String,
}

impl ItemId {
    /// The item with Apple's library id `id`, which is not empty and does not start with
    /// `catalog:`.
    pub fn library(id: impl Into<String>) -> Result<Self, ParseItemIdError> {
        Self::checked(false, id.into())
    }

    /// The item with catalog id `id`, which is not empty and does not start with
    /// `catalog:`.
    pub fn catalog(id: impl Into<String>) -> Result<Self, ParseItemIdError> {
        Self::checked(true, id.into())
    }

    fn checked(catalog: bool, id: String) -> Result<Self, ParseItemIdError> {
        if is_library_id(&id) {
            Ok(Self { catalog, id })
        } else {
            Err(ParseItemIdError(id))
        }
    }

    /// An id as Apple sent it, which is taken as it is.
    pub(super) fn from_apple(catalog: bool, id: String) -> Self {
        Self { catalog, id }
    }

    pub(super) fn library_apple(id: String) -> Self {
        Self::from_apple(false, id)
    }

    pub(super) fn catalog_apple(id: String) -> Self {
        Self::from_apple(true, id)
    }

    /// The id as Apple writes it, without saying which of the two it is.
    pub fn as_str(&self) -> &str {
        &self.id
    }

    /// The catalog id, for a catalog item.
    pub fn catalog_id(&self) -> Option<&str> {
        self.catalog.then_some(self.id.as_str())
    }

    /// Apple's library id, for a library item.
    pub fn library_id(&self) -> Option<&str> {
        (!self.catalog).then_some(self.id.as_str())
    }

    /// Whether this names a catalog item.
    pub fn is_catalog(&self) -> bool {
        self.catalog
    }
}

impl fmt::Display for ItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.catalog {
            write!(f, "{CATALOG_PREFIX}{}", self.id)
        } else {
            f.write_str(&self.id)
        }
    }
}

/// The text is neither a library id nor `catalog:<id>`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not an item id: {0:?}")]
pub struct ParseItemIdError(String);

impl FromStr for ItemId {
    type Err = ParseItemIdError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text.strip_prefix(CATALOG_PREFIX) {
            Some(id) => Self::catalog(id).map_err(|_| ParseItemIdError(text.to_owned())),
            None => Self::library(text),
        }
    }
}

/// An album in a search, a recommendation or a catalog page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumItem {
    /// The library album's id or the catalog album's.
    pub id: ItemId,
    /// The album's name.
    pub name: String,
    /// The artist's name.
    pub artist: Option<String>,
    /// The artist's id, of the same kind as `id`, when Apple names one.
    pub artist_id: Option<ItemId>,
    /// Number of songs.
    pub track_count: Option<u32>,
    /// Year of release.
    pub year: Option<u32>,
    /// First genre.
    pub genre: Option<String>,
    /// The cover.
    pub artwork: Option<Artwork>,
    /// Whether the user's library has the album.
    pub in_library: bool,
}

/// An artist in a search or on a catalog page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistItem {
    /// The library artist's id or the catalog artist's.
    pub id: ItemId,
    /// The artist's name.
    pub name: String,
    /// The artist's picture.
    pub artwork: Option<Artwork>,
    /// Whether the user's library has the artist.
    pub in_library: bool,
}

/// A playlist in a search, a recommendation or a catalog page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistItem {
    /// The library playlist's id or the catalog playlist's.
    pub id: ItemId,
    /// The playlist's name.
    pub name: String,
    /// Who curated a catalog playlist.
    pub curator: Option<String>,
    /// Whether songs can be added: never for a catalog playlist.
    pub can_append: bool,
    /// The cover.
    pub artwork: Option<Artwork>,
    /// Whether the user's library has the playlist.
    pub in_library: bool,
}

/// Where a search looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchScope {
    /// The user's library.
    Library,
    /// Apple's catalog.
    Catalog,
    /// The library first, then what the catalog adds to it.
    All,
}

/// What a search found, each list in order of relevance with the library's matches first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchResults {
    /// Matching songs.
    pub songs: Vec<Track>,
    /// Matching albums.
    pub albums: Vec<AlbumItem>,
    /// Matching artists.
    pub artists: Vec<ArtistItem>,
    /// Matching playlists. Apple's catalog search does not offer them, so these are the
    /// library's.
    pub playlists: Vec<PlaylistItem>,
}

/// A catalog album with its songs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogAlbumDetail {
    /// The album.
    pub album: AlbumItem,
    /// Its songs, in order.
    pub tracks: Vec<Track>,
}

/// A catalog playlist with its songs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogPlaylistDetail {
    /// The playlist.
    pub playlist: PlaylistItem,
    /// Its songs, in order.
    pub tracks: Vec<Track>,
}

/// A catalog artist with their albums.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogArtistDetail {
    /// The artist.
    pub artist: ArtistItem,
    /// Their albums.
    pub albums: Vec<AlbumItem>,
}

/// Things to ask the library about. A library item is in the library by definition. A
/// catalog album or artist is matched by its name (and its artist's, for an album), as the
/// library names no catalog id for an artist and not always for an album, so the items
/// must carry their names.
#[derive(Debug, Clone, Copy, Default)]
pub struct Candidates<'a> {
    /// Songs.
    pub songs: &'a [TrackKey],
    /// Albums, by catalog id or by name and artist.
    pub albums: &'a [AlbumItem],
    /// Artists, by name.
    pub artists: &'a [ArtistItem],
    /// Playlists.
    pub playlists: &'a [PlaylistItem],
}

/// Catalog items to add to the library. Library items in these lists are in the library
/// already and are skipped.
#[derive(Debug, Clone, Copy, Default)]
pub struct Additions<'a> {
    /// Songs.
    pub songs: &'a [TrackKey],
    /// Albums.
    pub albums: &'a [ItemId],
    /// Playlists.
    pub playlists: &'a [ItemId],
}

/// Which of the [`Candidates`] are in the library.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Membership {
    /// Songs in the library.
    pub songs: HashSet<TrackKey>,
    /// Albums in the library.
    pub albums: HashSet<ItemId>,
    /// Artists in the library.
    pub artists: HashSet<ItemId>,
    /// Playlists in the library.
    pub playlists: HashSet<ItemId>,
}

/// What a recommendation group is, as far as Apple's kind says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecommendationKind {
    /// Made for the user.
    Personal,
    /// What the user played lately.
    Recent,
    /// Music of a genre.
    Genre,
    /// Anything else.
    Other,
}

/// A titled group of recommended albums and playlists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecommendationGroup {
    /// Apple's id of the group.
    pub id: String,
    /// The title to show.
    pub title: String,
    /// What the group is.
    pub kind: RecommendationKind,
    /// Its albums.
    pub albums: Vec<AlbumItem>,
    /// Its playlists.
    pub playlists: Vec<PlaylistItem>,
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

    /// Whether the library has a copy of the catalog song `catalog_id`.
    pub(super) fn has_library_copy(&self, catalog_id: &str) -> bool {
        self.library_twins.contains_key(catalog_id)
    }

    /// The library song that is the library's copy of catalog song `key`.
    pub fn library_twin(&self, key: &TrackKey) -> Option<TrackKey> {
        let id = self.library_twins.get(key.catalog_id()?)?;
        TrackKey::library(id.clone()).ok()
    }

    /// Lists `song` as in the library: one that is listed already, as a song of a playlist
    /// the library does not hold, is marked, one that is not is added.
    pub(super) fn push_in_library(&mut self, mut song: Track) {
        match self.index.get(&song.key) {
            Some(&at) => self.songs[at].in_library = true,
            None => {
                song.in_library = true;
                self.push(song);
            }
        }
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
