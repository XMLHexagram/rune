//! Subsonic JSON for the `subsonic` models, built by hand because the models only parse.

use serde_json::{Map, Value, json};
use subsonic::{
    RuneEdit, RuneLoginMethod, RuneLoginStart, RunePlayback, RunePlaybackMode, RunePlaybackReason,
    RuneReason, RuneServiceDescriptor, SubsonicAlbum, SubsonicAlbumDetail, SubsonicArtist,
    SubsonicArtistDetail, SubsonicJukeboxStatus, SubsonicPlaylist, SubsonicPlaylistDetail,
    SubsonicSong, SubsonicSyncedLyrics,
};

fn put<T: Into<Value> + Clone>(map: &mut Map<String, Value>, key: &str, value: &Option<T>) {
    if let Some(value) = value {
        map.insert(key.to_owned(), value.clone().into());
    }
}

pub(crate) fn artist(artist: &SubsonicArtist) -> Value {
    let mut map = Map::new();
    map.insert("id".into(), artist.id.clone().into());
    map.insert("name".into(), artist.name.clone().into());
    put(&mut map, "albumCount", &artist.album_count);
    put(&mut map, "coverArt", &artist.cover_art);
    map.into()
}

pub(crate) fn artist_detail(detail: &SubsonicArtistDetail) -> Value {
    let mut value = artist(&SubsonicArtist {
        id: detail.id.clone(),
        name: detail.name.clone(),
        album_count: detail.album_count,
        cover_art: detail.cover_art.clone(),
    });
    value["album"] = detail.albums.iter().map(album).collect();
    value
}

pub(crate) fn album(album: &SubsonicAlbum) -> Value {
    let mut map = Map::new();
    map.insert("id".into(), album.id.clone().into());
    map.insert("name".into(), album.name.clone().into());
    put(&mut map, "artist", &album.artist);
    put(&mut map, "artistId", &album.artist_id);
    put(&mut map, "songCount", &album.song_count);
    put(&mut map, "duration", &album.duration);
    put(&mut map, "year", &album.year);
    put(&mut map, "genre", &album.genre);
    put(&mut map, "coverArt", &album.cover_art);
    map.into()
}

pub(crate) fn album_detail(detail: &SubsonicAlbumDetail) -> Value {
    let mut value = album(&SubsonicAlbum {
        id: detail.id.clone(),
        name: detail.name.clone(),
        artist: detail.artist.clone(),
        artist_id: detail.artist_id.clone(),
        song_count: detail.song_count,
        duration: detail.duration,
        year: detail.year,
        genre: detail.genre.clone(),
        cover_art: detail.cover_art.clone(),
    });
    value["song"] = detail.songs.iter().map(song).collect();
    value
}

pub(crate) fn song(song: &SubsonicSong) -> Value {
    let mut map = Map::new();
    map.insert("id".into(), song.id.clone().into());
    map.insert("title".into(), song.title.clone().into());
    map.insert("isDir".into(), false.into());
    put(&mut map, "album", &song.album);
    put(&mut map, "artist", &song.artist);
    put(&mut map, "albumId", &song.album_id);
    put(&mut map, "artistId", &song.artist_id);
    put(&mut map, "track", &song.track);
    put(&mut map, "discNumber", &song.disc);
    put(&mut map, "year", &song.year);
    put(&mut map, "genre", &song.genre);
    put(&mut map, "duration", &song.duration);
    put(&mut map, "size", &song.size);
    put(&mut map, "suffix", &song.suffix);
    put(&mut map, "contentType", &song.content_type);
    put(&mut map, "coverArt", &song.cover_art);
    put(&mut map, "bitRate", &song.bit_rate);
    put(&mut map, "starred", &song.starred);
    if let Some(playback) = &song.rune_playback {
        map.insert("runePlayback".into(), rune_playback(playback));
    }
    map.into()
}

fn rune_playback(playback: &RunePlayback) -> Value {
    let mode = match playback.mode {
        RunePlaybackMode::Stream => "stream",
        RunePlaybackMode::Hosted => "hosted",
        RunePlaybackMode::Unavailable => "unavailable",
        RunePlaybackMode::Unknown => "unknown",
    };
    let mut map = Map::new();
    map.insert("mode".into(), mode.into());
    put(&mut map, "host", &playback.host);
    put(&mut map, "key", &playback.key);
    if let Some(reason) = playback.reason {
        let reason = match reason {
            RunePlaybackReason::NotSubscribed => "notSubscribed",
            RunePlaybackReason::Region => "region",
            RunePlaybackReason::Removed => "removed",
            RunePlaybackReason::Other => "other",
        };
        map.insert("reason".into(), reason.into());
    }
    map.into()
}

pub(crate) fn playlist(playlist: &SubsonicPlaylist) -> Value {
    let mut map = Map::new();
    map.insert("id".into(), playlist.id.clone().into());
    map.insert("name".into(), playlist.name.clone().into());
    put(&mut map, "songCount", &playlist.song_count);
    put(&mut map, "duration", &playlist.duration);
    put(&mut map, "owner", &playlist.owner);
    put(&mut map, "public", &playlist.public);
    put(&mut map, "coverArt", &playlist.cover_art);
    if let Some(edit) = playlist.rune_edit {
        map.insert("runeEdit".into(), rune_edit(edit).into());
    }
    map.into()
}

pub(crate) fn playlist_detail(detail: &SubsonicPlaylistDetail) -> Value {
    let mut value = playlist(&SubsonicPlaylist {
        id: detail.id.clone(),
        name: detail.name.clone(),
        song_count: detail.song_count,
        duration: detail.duration,
        owner: detail.owner.clone(),
        public: detail.public,
        cover_art: detail.cover_art.clone(),
        rune_edit: detail.rune_edit,
    });
    value["entry"] = detail.songs.iter().map(song).collect();
    value
}

fn rune_edit(edit: RuneEdit) -> &'static str {
    match edit {
        RuneEdit::Full => "full",
        RuneEdit::AppendOnly => "appendOnly",
        RuneEdit::ReadOnly => "readOnly",
        RuneEdit::Unknown => "unknown",
    }
}

pub(crate) fn synced_lyrics(lyrics: &SubsonicSyncedLyrics) -> Value {
    let mut map = Map::new();
    put(&mut map, "lang", &lyrics.lang);
    map.insert("synced".into(), lyrics.synced.into());
    map.insert(
        "line".into(),
        lyrics
            .lines
            .iter()
            .map(|line| {
                if lyrics.synced {
                    json!({"start": line.start, "value": line.value})
                } else {
                    json!({"value": line.value})
                }
            })
            .collect(),
    );
    map.into()
}

pub(crate) fn jukebox_status(status: &SubsonicJukeboxStatus) -> Map<String, Value> {
    let mut map = Map::new();
    put(&mut map, "currentIndex", &status.current_index);
    map.insert("playing".into(), status.playing.into());
    map.insert("gain".into(), status.gain.into());
    put(&mut map, "position", &status.position_secs);
    map
}

pub(crate) fn rune_reason(reason: RuneReason) -> &'static str {
    match reason {
        RuneReason::CredentialsExpired => "credentialsExpired",
        RuneReason::NotSubscribed => "notSubscribed",
        RuneReason::Region => "region",
        RuneReason::Removed => "removed",
        RuneReason::ServiceLimit => "serviceLimit",
        RuneReason::Other => "other",
    }
}

pub(crate) fn rune_service(descriptor: &RuneServiceDescriptor) -> Value {
    let mut map = Map::new();
    map.insert("id".into(), descriptor.id.clone().into());
    map.insert("name".into(), descriptor.name.clone().into());
    if let Some(attribution) = &descriptor.attribution {
        let mut value = Map::new();
        value.insert("text".into(), attribution.text.clone().into());
        put(&mut value, "link", &attribution.link);
        map.insert("attribution".into(), value.into());
    }
    let methods: Vec<&str> = descriptor
        .login
        .methods
        .iter()
        .filter_map(|method| method.as_str())
        .collect();
    map.insert("login".into(), json!({"methods": methods}));
    map.insert("hosts".into(), json!(descriptor.hosts));
    map.insert(
        "unsupported".into(),
        descriptor
            .unsupported
            .iter()
            .map(|entry| {
                let mut value = Map::new();
                value.insert("endpoint".into(), entry.endpoint.clone().into());
                put(&mut value, "param", &entry.param);
                Value::from(value)
            })
            .collect(),
    );
    map.into()
}

pub(crate) fn rune_login(start: &RuneLoginStart) -> Value {
    match start {
        RuneLoginStart::Platform => json!({"method": "platform"}),
        RuneLoginStart::Browser { url, state } => {
            json!({"method": "browser", "url": url, "state": state})
        }
        RuneLoginStart::DeviceCode {
            user_code,
            verification_url,
            interval_secs,
        } => json!({
            "method": "deviceCode",
            "userCode": user_code,
            "verificationUrl": verification_url,
            "interval": interval_secs,
        }),
        RuneLoginStart::Other => json!({"method": "other"}),
    }
}

pub(crate) fn login_method(name: &str) -> RuneLoginMethod {
    match name {
        "password" => RuneLoginMethod::Password,
        "platform" => RuneLoginMethod::Platform,
        "browser" => RuneLoginMethod::Browser,
        "deviceCode" => RuneLoginMethod::DeviceCode,
        _ => RuneLoginMethod::Other,
    }
}
