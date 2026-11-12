//! Verbatim examples from the Subsonic and OpenSubsonic documentation, served by a plain axum
//! route to the real `SubsonicClient`, which must parse what real servers send. Each sample
//! names its source page.

use std::sync::Arc;

use axum::{Router, extract::State, http::header, response::IntoResponse, routing::get};
use subsonic::{SubsonicClient, SubsonicCredentials, SubsonicError, SubsonicServerError};
use tokio::net::TcpListener;
use url::Url;

/// https://opensubsonic.netlify.app/docs/endpoints/ping/
const PING: &str = r##"{
  "subsonic-response": {
    "status": "ok",
    "version": "1.16.1",
    "type": "AwesomeServerName",
    "serverVersion": "0.1.3 (tag)",
    "openSubsonic": true
  }
}"##;

/// https://opensubsonic.netlify.app/docs/endpoints/getopensubsonicextensions/
const EXTENSIONS: &str = r##"{
    "subsonic-response": {
        "status": "ok",
        "version": "1.16.1",
        "type": "AwesomeServerName",
        "serverVersion": "0.1.3 (tag)",
        "openSubsonic": true,
        "openSubsonicExtensions": [
            {
                "name": "template",
                "versions": [
                    1,
                    2
                ]
            },
            {
                "name": "transcodeOffset",
                "versions": [
                    1
                ]
            },
            {
                "name": "sonicSimilarity",
                "versions": [
                    1
                ]
            },
            {
                "name": "topSongsByArtistId",
                "versions": [
                    1
                ]
            }
        ]
    }
}"##;

/// https://opensubsonic.netlify.app/docs/endpoints/getartists/
const ARTISTS: &str = r##"{
  "subsonic-response": {
    "status": "ok",
    "version": "1.16.1",
    "type": "AwesomeServerName",
    "serverVersion": "0.1.3 (tag)",
    "openSubsonic": true,
    "artists": {
      "ignoredArticles": "The An A Die Das Ein Eine Les Le La",
      "index": [
        {
          "name": "C",
          "artist": [
            {
              "id": "100000016",
              "name": "CARNÚN",
              "coverArt": "ar-100000016",
              "albumCount": 1
            },
            {
              "id": "100000027",
              "name": "Chi.Otic",
              "coverArt": "ar-100000027",
              "albumCount": 0
            }
          ]
        },
        {
          "name": "I",
          "artist": [
            {
              "id": "100000013",
              "name": "IOK-1",
              "coverArt": "ar-100000013",
              "albumCount": 1
            }
          ]
        }
      ]
    }
  }
}"##;

/// https://opensubsonic.netlify.app/docs/endpoints/getalbum/
const ALBUM: &str = r##"{
  "subsonic-response": {
    "status": "ok",
    "version": "1.16.1",
    "type": "AwesomeServerName",
    "serverVersion": "0.1.3 (tag)",
    "openSubsonic": true,
    "album": {
      "id": "200000021",
      "parent": "100000036",
      "album": "Forget and Remember",
      "title": "Forget and Remember",
      "name": "Forget and Remember",
      "isDir": true,
      "coverArt": "al-200000021",
      "songCount": 20,
      "created": "2021-07-22T02:09:31+00:00",
      "duration": 4248,
      "playCount": 0,
      "artistId": "100000036",
      "artist": "Comfort Fit",
      "year": 2005,
      "genre": "Hip-Hop",
      "song": [
        {
          "id": "300000116",
          "parent": "200000021",
          "title": "Can I Help U?",
          "isDir": false,
          "isVideo": false,
          "type": "music",
          "albumId": "200000021",
          "album": "Forget and Remember",
          "artistId": "100000036",
          "artist": "Comfort Fit",
          "coverArt": "300000116",
          "duration": 103,
          "bitRate": 216,
          "bitDepth": 16,
          "samplingRate": 44100,
          "channelCount": 2,
          "track": 1,
          "year": 2005,
          "genre": "Hip-Hop",
          "size": 2811819,
          "discNumber": 1,
          "suffix": "mp3",
          "contentType": "audio/mpeg",
          "path": "user/Comfort Fit/Forget And Remember/1 - Can I Help U?.mp3"
        },
        {
          "id": "300000121",
          "parent": "200000021",
          "title": "Planetary Picknick",
          "isDir": false,
          "isVideo": false,
          "type": "music",
          "albumId": "200000021",
          "album": "Forget and Remember",
          "artistId": "100000036",
          "artist": "Comfort Fit",
          "coverArt": "300000121",
          "duration": 358,
          "bitRate": 238,
          "bitDepth": 16,
          "samplingRate": 44100,
          "channelCount": 2,
          "track": 2,
          "year": 2005,
          "genre": "Hip-Hop",
          "size": 10715592,
          "discNumber": 1,
          "suffix": "mp3",
          "contentType": "audio/mpeg",
          "path": "user/Comfort Fit/Forget And Remember/2 - Planetary Picknick.mp3"
        }
      ]
    }
  }
}"##;

/// https://opensubsonic.netlify.app/docs/endpoints/getplaylist/
const PLAYLIST: &str = r##"{
  "subsonic-response": {
    "status": "ok",
    "version": "1.16.1",
    "type": "AwesomeServerName",
    "serverVersion": "0.1.3 (tag)",
    "openSubsonic": true,
    "playlist": {
      "id": "800000075",
      "name": "testcreate",
      "owner": "user",
      "public": true,
      "created": "2023-03-16T03:18:41+00:00",
      "changed": "2023-03-16T03:18:41+00:00",
      "songCount": 1,
      "duration": 304,
      "entry": [
        {
          "id": "300000060",
          "parent": "200000002",
          "title": "BrownSmoke",
          "isDir": false,
          "isVideo": false,
          "type": "music",
          "albumId": "200000002",
          "album": "Colorsmoke EP",
          "artistId": "100000002",
          "artist": "Synthetic",
          "coverArt": "300000060",
          "duration": 304,
          "bitRate": 20,
          "bitDepth": 16,
          "samplingRate": 44100,
          "channelCount": 2,
          "userRating": 5,
          "averageRating": 5,
          "track": 4,
          "year": 2007,
          "genre": "Electronic",
          "size": 792375,
          "discNumber": 1,
          "suffix": "wma",
          "contentType": "audio/x-ms-wma",
          "path": "Synthetic/Synthetic_-_Colorsmoke_EP-20k217-2007/04-Synthetic_-_BrownSmokeYSBM20k22khS.wma"
        }
      ]
    }
  }
}"##;

/// https://opensubsonic.netlify.app/docs/endpoints/getlyricsbysongid/
const LYRICS: &str = r##"{
  "subsonic-response": {
    "status": "ok",
    "version": "1.16.1",
    "type": "AwesomeServerName",
    "serverVersion": "0.1.3 (tag)",
    "openSubsonic": true,
    "lyricsList": {
      "structuredLyrics": [
        {
          "displayArtist": "Muse",
          "displayTitle": "Hysteria",
          "lang": "eng",
          "offset": -100,
          "synced": true,
          "line": [
            {
              "start": 0,
              "value": "It's bugging me"
            },
            {
              "start": 2000,
              "value": "Grating me"
            },
            {
              "start": 3001,
              "value": "And twisting me around..."
            }
          ]
        },
        {
          "displayArtist": "Muse",
          "displayTitle": "Hysteria",
          "lang": "und",
          "offset": 100,
          "synced": false,
          "line": [
            {
              "value": "It's bugging me"
            },
            {
              "value": "Grating me"
            },
            {
              "value": "And twisting me around..."
            }
          ]
        }
      ]
    }
  }
}"##;

/// https://opensubsonic.netlify.app/docs/endpoints/getlyricsbysongid/
const LYRICS_KINDS: &str = r##"{
  "subsonic-response": {
    "status": "ok",
    "version": "1.16.1",
    "type": "AwesomeServerName",
    "serverVersion": "0.1.3 (tag)",
    "openSubsonic": true,
    "lyricsList": {
      "structuredLyrics": [
        {
          "kind": "main",
          "lang": "ko",
          "synced": true,
          "line": [
            { "start": 2747, "end": 6214, "value": "눈을 뜬 순간" },
            { "start": 6214, "end": 9000, "value": "모든 게 달라졌어" }
          ],
          "cueLine": [
            {
              "index": 0,
              "start": 2747,
              "end": 6214,
              "value": "눈을 뜬 순간",
              "cue": [
                { "start": 2747, "end": 3018, "value": "눈", "byteStart": 0, "byteEnd": 2 },
                { "start": 3018, "end": 3179, "value": "을", "byteStart": 3, "byteEnd": 5 },
                { "start": 3179, "end": 3582, "value": " ", "byteStart": 6, "byteEnd": 6 },
                { "start": 3582, "end": 4100, "value": "뜬", "byteStart": 7, "byteEnd": 9 },
                { "start": 4100, "end": 4500, "value": " ", "byteStart": 10, "byteEnd": 10 },
                { "start": 4500, "end": 5200, "value": "순", "byteStart": 11, "byteEnd": 13 },
                { "start": 5200, "end": 6214, "value": "간", "byteStart": 14, "byteEnd": 16 }
              ]
            },
            {
              "index": 1,
              "start": 6214,
              "end": 9000,
              "value": "모든 게 달라졌어",
              "cue": [
                { "start": 6214, "end": 6800, "value": "모", "byteStart": 0, "byteEnd": 2 },
                { "start": 6800, "end": 7200, "value": "든", "byteStart": 3, "byteEnd": 5 },
                { "start": 7200, "end": 7600, "value": " ", "byteStart": 6, "byteEnd": 6 },
                { "start": 7600, "end": 8000, "value": "게", "byteStart": 7, "byteEnd": 9 },
                { "start": 8000, "end": 8400, "value": " ", "byteStart": 10, "byteEnd": 10 },
                { "start": 8400, "end": 9000, "value": "달라졌어", "byteStart": 11, "byteEnd": 22 }
              ]
            }
          ]
        },
        {
          "kind": "translation",
          "lang": "eng",
          "synced": true,
          "line": [
            { "start": 2747, "value": "The moment I opened my eyes" },
            { "start": 6214, "value": "Everything had changed" }
          ]
        },
        {
          "kind": "pronunciation",
          "lang": "ko-Latn",
          "synced": true,
          "line": [
            { "start": 2747, "value": "nuneul tteun sungan" },
            { "start": 6214, "value": "modeun ge dallajyeosseo" }
          ],
          "cueLine": [
            {
              "index": 0,
              "start": 2747,
              "end": 6214,
              "value": "nuneul tteun sungan",
              "cue": [
                { "start": 2747, "end": 3179, "value": "nuneul", "byteStart": 0, "byteEnd": 5 },
                { "start": 3582, "end": 4100, "value": "tteun", "byteStart": 7, "byteEnd": 11 },
                { "start": 4500, "end": 6214, "value": "sungan", "byteStart": 13, "byteEnd": 18 }
              ]
            },
            {
              "index": 1,
              "start": 6214,
              "end": 9000,
              "value": "modeun ge dallajyeosseo",
              "cue": [
                { "start": 6214, "end": 7200, "value": "modeun", "byteStart": 0, "byteEnd": 5 },
                { "start": 7600, "end": 8000, "value": "ge", "byteStart": 7, "byteEnd": 8 },
                { "start": 8400, "end": 9000, "value": "dallajyeosseo", "byteStart": 10, "byteEnd": 22 }
              ]
            }
          ]
        }
      ]
    }
  }
}"##;

/// https://opensubsonic.netlify.app/docs/endpoints/jukeboxcontrol/
const JUKEBOX_STATUS: &str = r##"{
  "subsonic-response": {
    "status": "ok",
    "version": "1.16.1",
    "type": "AwesomeServerName",
    "serverVersion": "0.1.3 (tag)",
    "openSubsonic": true,
    "jukeboxStatus": {
      "currentIndex": 7,
      "playing": true,
      "gain": 0.9,
      "position": 67
    }
  }
}"##;

/// https://opensubsonic.netlify.app/docs/endpoints/jukeboxcontrol/
const JUKEBOX_PLAYLIST: &str = r##"{
  "subsonic-response": {
    "status": "ok",
    "version": "1.16.1",
    "type": "AwesomeServerName",
    "serverVersion": "0.1.3 (tag)",
    "openSubsonic": true,
    "jukeboxPlaylist": {
      "currentIndex": 7,
      "playing": true,
      "gain": 0.9,
      "position": 67,
      "entry": [
        {
          "id": "300000116",
          "parent": "200000021",
          "title": "Can I Help U?",
          "isDir": false,
          "isVideo": false,
          "type": "music",
          "albumId": "200000021",
          "album": "Forget and Remember",
          "artistId": "100000036",
          "artist": "Comfort Fit",
          "coverArt": "300000116",
          "duration": 103,
          "bitRate": 216,
          "bitDepth": 16,
          "samplingRate": 44100,
          "channelCount": 2,
          "track": 1,
          "year": 2005,
          "genre": "Hip-Hop",
          "size": 2811819,
          "discNumber": 1,
          "suffix": "mp3",
          "contentType": "audio/mpeg",
          "path": "user/Comfort Fit/Forget And Remember/1 - Can I Help U?.mp3"
        }
      ]
    }
  }
}"##;

/// https://opensubsonic.netlify.app/docs/endpoints/getscanstatus/
const SCAN_STATUS: &str = r##"{
  "subsonic-response": {
    "status": "ok",
    "version": "1.16.1",
    "type": "AwesomeServerName",
    "serverVersion": "0.1.3 (tag)",
    "openSubsonic": true,
    "scanStatus": {
      "scanning": true,
      "count": 1
    }
  }
}"##;

/// https://opensubsonic.netlify.app/docs/endpoints/getsong/
const SONG: &str = r##"{
  "subsonic-response": {
    "status": "ok",
    "version": "1.16.1",
    "song": {
      "id": "082f435a363c32c57d5edb6a678a28d4",
      "parent": "e8a0685e3f3ec6f251649af2b58b8617",
      "isDir": false,
      "title": "\"polar expedition\"",
      "album": "Live at The Casbah - 2005-04-29",
      "artist": "The New Deal",
      "track": 4,
      "year": 2005,
      "coverArt": "mf-082f435a363c32c57d5edb6a678a28d4_6410b3ce",
      "size": 19866778,
      "contentType": "audio/flac",
      "suffix": "flac",
      "starred": "2023-03-27T09:45:27Z",
      "duration": 178,
      "bitRate": 880,
      "bitDepth": 16,
      "samplingRate": 44100,
      "channelCount": 2,
      "path": "The New Deal/Live at The Casbah - 2005-04-29/04 - \"polar expedition\".flac",
      "playCount": 8,
      "discNumber": 1,
      "created": "2023-03-14T17:51:22.112827504Z",
      "albumId": "e8a0685e3f3ec6f251649af2b58b8617",
      "artistId": "97e0398acf63f9fb930d7d4ce209a52b",
      "type": "music",
      "isVideo": false
    }
  }
}"##;

/// The error objects of https://opensubsonic.netlify.app/docs/responses/error/ (codes 42 and
/// 40) and https://www.subsonic.org/pages/api.jsp (the 40 message), wrapped in the envelope
/// they are sent in. The 70 message is the spec's table text for that code.
const ERROR_40: &str = r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":40,"message":"Wrong username or password."}}}"#;
const ERROR_42: &str = r#"{"subsonic-response":{"status":"failed","version":"1.16.1","openSubsonic":true,"error":{"code":42,"message":"Authentication mechanism not supported. Use API keys","helpUrl":"https://example.org/users/apiKey"}}}"#;
const ERROR_70: &str = r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":70,"message":"The requested data was not found."}}}"#;

/// A client of a server that answers every `/rest/*` call with `body`.
async fn client(body: &'static str) -> SubsonicClient {
    let app = Router::new()
        .route(
            "/rest/{endpoint}",
            get(|State(body): State<Arc<&'static str>>| async move {
                ([(header::CONTENT_TYPE, "application/json")], *body).into_response()
            }),
        )
        .with_state(Arc::new(body));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    SubsonicClient::new(
        base,
        SubsonicCredentials::Password {
            user: "alice".into(),
            password: "secret".into(),
        },
        reqwest::Client::new(),
    )
}

#[tokio::test]
async fn ping_and_extensions() {
    // The ping sample says `openSubsonic`, so the client asks for the extensions too; this
    // server answers that with the ping again, which declares none.
    assert!(client(PING).await.extensions().await.unwrap().is_empty());
    // Served for both calls, the extensions sample is also a valid ping.
    let extensions = client(EXTENSIONS).await.extensions().await.unwrap();
    assert_eq!(
        extensions,
        [
            ("template".to_owned(), vec![1, 2]),
            ("transcodeOffset".to_owned(), vec![1]),
            ("sonicSimilarity".to_owned(), vec![1]),
            ("topSongsByArtistId".to_owned(), vec![1]),
        ]
    );
}

#[tokio::test]
async fn artists_and_their_indexes() {
    let artists = client(ARTISTS).await.artists().await.unwrap();
    let names: Vec<_> = artists.iter().map(|artist| artist.name.as_str()).collect();
    assert_eq!(names, ["CARNÚN", "Chi.Otic", "IOK-1"]);
    assert_eq!(artists[0].id, "100000016");
    assert_eq!(artists[0].cover_art.as_deref(), Some("ar-100000016"));
    assert_eq!(artists[1].album_count, Some(0));
}

#[tokio::test]
async fn album_with_songs() {
    let album = client(ALBUM).await.album("200000021").await.unwrap();
    assert_eq!(album.name, "Forget and Remember");
    assert_eq!(album.artist_id.as_deref(), Some("100000036"));
    assert_eq!(album.year, Some(2005));
    assert_eq!(album.duration, Some(4248));
    assert_eq!(album.songs.len(), 2);
    let song = &album.songs[1];
    assert_eq!(song.title, "Planetary Picknick");
    assert_eq!(song.track, Some(2));
    assert_eq!(song.disc, Some(1));
    assert_eq!(song.size, Some(10_715_592));
    assert_eq!(song.content_type.as_deref(), Some("audio/mpeg"));
    assert_eq!(song.rune_playback, None);
}

#[tokio::test]
async fn song_with_the_open_subsonic_extras() {
    let song = client(SONG).await.song("x").await.unwrap();
    assert_eq!(song.title, "\"polar expedition\"");
    assert_eq!(song.starred.as_deref(), Some("2023-03-27T09:45:27Z"));
    assert_eq!(song.bit_rate, Some(880));
    assert_eq!(song.duration, Some(178));
}

#[tokio::test]
async fn playlist_with_entries() {
    let playlist = client(PLAYLIST).await.playlist("800000075").await.unwrap();
    assert_eq!(playlist.name, "testcreate");
    assert_eq!(playlist.owner.as_deref(), Some("user"));
    assert_eq!(playlist.public, Some(true));
    assert_eq!(playlist.song_count, Some(1));
    assert_eq!(playlist.rune_edit, None);
    assert_eq!(playlist.songs[0].title, "BrownSmoke");
}

#[tokio::test]
async fn lyrics_with_a_synced_and_an_unsynced_entry() {
    let lyrics = client(LYRICS).await.lyrics_by_song_id("x").await.unwrap();
    assert_eq!(lyrics.synced_lyrics.len(), 2);
    assert!(lyrics.synced_lyrics[0].synced);
    assert_eq!(lyrics.synced_lyrics[0].lines[2].start, 3001);
    assert_eq!(lyrics.synced_lyrics[0].lang.as_deref(), Some("eng"));
    assert!(!lyrics.synced_lyrics[1].synced);
    assert_eq!(
        lyrics.plain_lyrics.as_deref(),
        Some("It's bugging me\nGrating me\nAnd twisting me around...")
    );
}

#[tokio::test]
async fn lyrics_with_kinds_cues_and_end_times() {
    let lyrics = client(LYRICS_KINDS)
        .await
        .lyrics_by_song_id("x")
        .await
        .unwrap();
    assert_eq!(lyrics.synced_lyrics.len(), 3);
    assert_eq!(lyrics.synced_lyrics[0].lines[1].value, "모든 게 달라졌어");
    assert_eq!(lyrics.synced_lyrics[1].lang.as_deref(), Some("eng"));
    assert_eq!(lyrics.plain_lyrics, None);
}

#[tokio::test]
async fn errors_map_to_their_kinds() {
    let wrong = client(ERROR_40).await.song("x").await.unwrap_err();
    assert_eq!(
        wrong,
        SubsonicError::Server {
            kind: SubsonicServerError::WrongCredentials,
            message: "Wrong username or password.".into(),
            rune_reason: None,
        }
    );
    let mechanism = client(ERROR_42).await.song("x").await.unwrap_err();
    assert!(matches!(
        mechanism,
        SubsonicError::Server {
            kind: SubsonicServerError::AuthenticationMechanismNotSupported,
            ..
        }
    ));
    let missing = client(ERROR_70).await.song("x").await.unwrap_err();
    assert!(matches!(
        &missing,
        SubsonicError::Server {
            kind: SubsonicServerError::NotFound,
            rune_reason: None,
            ..
        }
    ));
    assert!(!missing.is_rune_credentials_expired());
}

#[tokio::test]
async fn jukebox_status_and_playlist() {
    let status = client(JUKEBOX_STATUS).await.jukebox_status().await.unwrap();
    assert_eq!(status.current_index, Some(7));
    assert!(status.playing);
    assert_eq!(status.gain, 0.9);
    assert_eq!(status.position_secs, Some(67));

    let jukebox = client(JUKEBOX_PLAYLIST).await.jukebox_get().await.unwrap();
    assert_eq!(jukebox.status, status);
    assert_eq!(jukebox.playlist.songs[0].title, "Can I Help U?");
}

#[tokio::test]
async fn scan_status() {
    let status = client(SCAN_STATUS).await.scan_status().await.unwrap();
    assert!(status.scanning);
    assert_eq!(status.count, Some(1));
}
