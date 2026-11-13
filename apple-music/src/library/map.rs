//! How Apple Music resources become the library's types.

use std::time::Duration;

use super::types::{Album, Artist, Artwork, Playlist, Track, TrackKey};
use crate::models::{
    self, LibraryAlbum, LibraryArtist, LibraryPlaylist, LibrarySong, LibrarySongAttributes,
    Relationship, Song, SongAttributes, TrackRef,
};

impl TrackKey {
    pub(super) fn track_ref(&self) -> TrackRef {
        match self.library_id() {
            Some(id) => TrackRef::library_song(id),
            None => TrackRef::song(self.catalog_id().unwrap_or_default()),
        }
    }
}

fn artwork(artwork: Option<&models::Artwork>) -> Option<Artwork> {
    let artwork = artwork.filter(|artwork| !artwork.url.is_empty())?;
    Some(Artwork {
        template: artwork.url.clone(),
        max_size: artwork.width,
    })
}

/// The attributes library and catalog songs share.
struct Attributes<'a> {
    name: Option<&'a str>,
    album: Option<&'a str>,
    artist: Option<&'a str>,
    track: Option<u32>,
    disc: Option<u32>,
    release_date: Option<&'a str>,
    genres: &'a [String],
    duration_ms: Option<u64>,
    artwork: Option<&'a models::Artwork>,
}

impl<'a> From<&'a LibrarySongAttributes> for Attributes<'a> {
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

impl<'a> From<&'a SongAttributes> for Attributes<'a> {
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

fn track(
    key: TrackKey,
    attributes: Attributes<'_>,
    album_id: Option<String>,
    artist_id: Option<String>,
    in_library: bool,
    catalog_id: Option<String>,
) -> Track {
    Track {
        key,
        title: attributes.name.unwrap_or_default().to_owned(),
        album: attributes.album.map(str::to_owned),
        artist: attributes.artist.map(str::to_owned),
        album_id,
        artist_id,
        track_number: attributes.track,
        disc_number: attributes.disc,
        year: year(attributes.release_date),
        genre: attributes.genres.first().cloned(),
        duration: attributes.duration_ms.map(Duration::from_millis),
        artwork: artwork(attributes.artwork),
        in_library,
        catalog_id,
    }
}

/// A library song or playlist track; `None` for a music video, and for an entry that is
/// neither a library song nor known in the catalog, which could not be played or rated.
/// Entries that are not library songs (playlist tracks outside the library) become
/// catalog songs when their catalog id is known, so they play and rate as such.
pub(super) fn library_track(entry: &LibrarySong) -> Option<Track> {
    if entry.kind.contains("music-video") {
        return None;
    }
    let store_id = entry.store_id();
    let (key, in_library) = match TrackKey::library(&entry.id) {
        Ok(key) => (key, true),
        Err(_) => (TrackKey::catalog(store_id?).ok()?, false),
    };
    let relationships = entry.relationships.as_ref();
    let album_id = first_id(relationships.and_then(|r| r.albums.as_ref()));
    let artist_id = first_id(relationships.and_then(|r| r.artists.as_ref()));
    let attributes = entry.attributes.clone().unwrap_or_default();
    Some(track(
        key,
        Attributes::from(&attributes),
        album_id,
        artist_id,
        in_library,
        store_id.map(str::to_owned),
    ))
}

fn first_id<A>(relationship: Option<&Relationship<A>>) -> Option<String> {
    relationship
        .and_then(|relationship| relationship.data.first())
        .map(|resource| resource.id.clone())
}

/// A catalog song, outside the library until marked otherwise. Its catalog album and
/// artist are known only when Apple sends the song's relationships.
pub(super) fn catalog_track(entry: &Song) -> Option<Track> {
    let attributes = entry.attributes.clone().unwrap_or_default();
    let related = |relationship: &str| {
        let id = entry
            .relationships
            .as_ref()?
            .pointer(&format!("/{relationship}/data/0/id"))?
            .as_str()?;
        Some(id.to_owned())
    };
    Some(track(
        TrackKey::catalog(&entry.id).ok()?,
        Attributes::from(&attributes),
        related("albums"),
        related("artists"),
        false,
        Some(entry.id.clone()),
    ))
}

pub(super) fn album(entry: &LibraryAlbum) -> Album {
    let attributes = entry.attributes.clone().unwrap_or_default();
    Album {
        id: entry.id.clone(),
        name: attributes.name.unwrap_or_default(),
        artist: attributes.artist_name,
        artist_id: entry
            .relationships
            .as_ref()
            .and_then(|relationships| relationships.pointer("/artists/data/0/id"))
            .and_then(|id| id.as_str())
            .map(str::to_owned),
        track_count: attributes.track_count,
        year: year(attributes.release_date.as_deref()),
        genre: attributes.genre_names.first().cloned(),
        artwork: artwork(attributes.artwork.as_ref()),
        catalog_id: entry.catalog_id().map(str::to_owned),
    }
}

pub(super) fn artist(entry: &LibraryArtist) -> Artist {
    Artist {
        id: entry.id.clone(),
        name: entry
            .attributes
            .as_ref()
            .and_then(|attributes| attributes.name.clone())
            .unwrap_or_default(),
    }
}

pub(super) fn playlist(entry: &LibraryPlaylist) -> Playlist {
    let attributes = entry.attributes.clone().unwrap_or_default();
    Playlist {
        id: entry.id.clone(),
        name: attributes.name.clone().unwrap_or_default(),
        // Apple's API can only append to a playlist; Favorites and subscribed ones not even that.
        can_append: can_append(entry),
        is_public: attributes.is_public,
        artwork: artwork(attributes.artwork.as_ref()),
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
pub(super) fn joins_song_list(entry: &LibraryPlaylist) -> bool {
    let attributes = entry.attributes.as_ref();
    can_append(entry) || attributes.and_then(|attributes| attributes.has_catalog) != Some(true)
}

fn year(release_date: Option<&str>) -> Option<u32> {
    release_date?.get(..4)?.parse().ok()
}
