//! Every endpoint of `SubsonicClient`, through the facade, against a fake with known data.

mod common;

use std::sync::Arc;

use common::*;
use subsonic::{
    RuneLoginCompletion, RuneLoginMethod, RuneLoginStart, RuneReason, SearchLimits, StreamError,
    SubsonicApi, SubsonicCredentials, SubsonicError, SubsonicJukebox, SubsonicJukeboxPlaylist,
    SubsonicJukeboxStatus, SubsonicLyrics, SubsonicServerError,
};

async fn fixture() -> (Arc<Fake>, Served) {
    let fake = Fake::new();
    let served = serve(fake.clone()).await;
    (fake, served)
}

#[tokio::test]
async fn probe_carries_the_identity_extensions_and_roles() {
    let (_, served) = fixture().await;
    let client = served.client();
    assert_eq!(client.probe().await.unwrap(), probe());
    assert_eq!(client.extensions().await.unwrap(), probe().extensions);
}

#[tokio::test]
async fn browsing_returns_the_fake_data() {
    let (_, served) = fixture().await;
    let client = served.client();
    assert_eq!(client.artists().await.unwrap(), artists());
    assert_eq!(client.artist("ar1").await.unwrap(), artist_detail());
    assert_eq!(client.album("al1").await.unwrap(), album_detail());
    for song in songs() {
        assert_eq!(client.song(&song.id).await.unwrap(), song);
    }
}

#[tokio::test]
async fn lists_and_search_pass_their_parameters() {
    let (fake, served) = fixture().await;
    let client = served.client();
    assert_eq!(
        client.album_list(2, 1).await.unwrap(),
        albums()[1..].to_vec()
    );
    assert_eq!(
        client.album_list(1, 0).await.unwrap(),
        albums()[..1].to_vec()
    );
    assert_eq!(
        client.starred_songs().await.unwrap(),
        vec![songs()[0].clone(), songs()[3].clone()]
    );

    let limits = SearchLimits {
        artists: 1,
        albums: 2,
        songs: 2,
    };
    let result = client
        .search_with_song_offset("Björk & co/+", limits, 1)
        .await
        .unwrap();
    assert_eq!(result.artists, artists()[..1].to_vec());
    assert_eq!(result.albums, albums()[..2].to_vec());
    assert_eq!(result.songs, songs()[1..3].to_vec());
    assert_eq!(fake.calls(), ["search:Björk & co/+"]);

    let first = client.search("x", limits).await.unwrap();
    assert_eq!(first.songs, songs()[..2].to_vec());

    let status = client.scan_status().await.unwrap();
    assert!(status.scanning);
    assert_eq!(status.count, Some(1234));
}

#[tokio::test]
async fn playlists_read_and_edit() {
    let (fake, served) = fixture().await;
    let client = served.client();
    assert_eq!(client.playlists().await.unwrap(), playlists());
    assert_eq!(client.playlist("p1").await.unwrap(), playlist_detail());

    client
        .create_playlist("New & Old", &["s1", "s2"])
        .await
        .unwrap();
    client.create_playlist("Empty", &[]).await.unwrap();
    client.replace_playlist_songs("p1", &["s3"]).await.unwrap();
    client.replace_playlist_songs("p1", &[]).await.unwrap();
    client
        .update_playlist("p1", Some("Renamed"), &["s1", "s4"], &[0, 2])
        .await
        .unwrap();
    client.update_playlist("p1", None, &[], &[]).await.unwrap();
    client.delete_playlist("p1").await.unwrap();
    assert_eq!(
        fake.calls(),
        [
            "create:New & Old:s1,s2",
            "create:Empty:",
            "replace:p1:s3",
            "replace:p1:",
            "update:p1:Some(\"Renamed\"):s1,s4:0,2",
            "update:p1:None::",
            "delete:p1",
        ]
    );
}

#[tokio::test]
async fn annotation_reaches_the_fake() {
    let (fake, served) = fixture().await;
    let client = served.client();
    client.star(&["s1", "s2"]).await.unwrap();
    client.unstar(&["s2"]).await.unwrap();
    client
        .scrobble("s1", Some(1_700_000_000_000), true)
        .await
        .unwrap();
    client.scrobble("s3", None, false).await.unwrap();
    assert_eq!(
        fake.calls(),
        [
            "star:s1,s2",
            "unstar:s2",
            "scrobble:s1:Some(1700000000000):true",
            "scrobble:s3:None:false",
        ]
    );
}

#[tokio::test]
async fn lyrics_round_trip() {
    let (_, served) = fixture().await;
    let client = served.client();
    let plain = client.lyrics("Ada", "Opening").await.unwrap();
    assert_eq!(plain.plain_lyrics.as_deref(), Some("la la\nlo lo"));
    assert!(plain.synced_lyrics.is_empty());
    assert_eq!(
        client.lyrics("Ada", "Silent").await.unwrap(),
        SubsonicLyrics::default()
    );
    assert_eq!(
        client.lyrics_by_song_id("s1").await.unwrap(),
        lyrics_by_song()
    );
    assert_eq!(
        client.lyrics_by_song_id("s2").await.unwrap(),
        SubsonicLyrics::default()
    );
    assert!(matches!(
        client.lyrics_by_song_id("nope").await,
        Err(SubsonicError::Server {
            kind: SubsonicServerError::NotFound,
            ..
        })
    ));
}

#[tokio::test]
async fn cover_art_returns_the_bytes() {
    let (_, served) = fixture().await;
    let client = served.client();
    assert_eq!(
        client.cover_art("c1", None).await.unwrap(),
        [0xff, 0xd8, 0xff, 0x01, 0xfe]
    );
    assert_eq!(
        client.cover_art("c1", Some(64)).await.unwrap(),
        [0xff, 0xd8, 0xff, 0x01, 0xfe, 64]
    );
    // A missing image is an error document with a success status, which the client reads.
    assert_eq!(
        client.cover_art("nope", None).await.unwrap_err(),
        SubsonicError::Server {
            kind: SubsonicServerError::NotFound,
            message: "cover art not found".into(),
            rune_reason: None,
        }
    );
}

#[tokio::test]
async fn cover_art_content_type_follows_the_bytes() {
    let (_, served) = fixture().await;
    for (id, content_type) in [
        ("c1", "image/jpeg"),
        ("png", "image/png"),
        ("webp", "image/webp"),
        ("blob", "application/octet-stream"),
    ] {
        let response = reqwest::get(format!("{}rest/getCoverArt?id={id}", served.base))
            .await
            .unwrap();
        assert_eq!(response.headers()["content-type"], content_type, "{id}");
    }
}

#[tokio::test]
async fn stream_honours_range() {
    let (_, served) = fixture().await;
    let client = served.client();

    let whole = client.stream("s1", None).await.unwrap();
    assert_eq!(whole.status(), 200);
    assert_eq!(whole.headers()["content-type"], "audio/flac");
    assert_eq!(whole.headers()["accept-ranges"], "bytes");
    assert_eq!(whole.headers()["content-length"], "16");
    assert_eq!(whole.bytes().await.unwrap(), AUDIO);

    let part = client.stream("s1", Some((4, Some(7)))).await.unwrap();
    assert_eq!(part.status(), 206);
    assert_eq!(part.headers()["content-range"], "bytes 4-7/16");
    assert_eq!(part.bytes().await.unwrap(), &AUDIO[4..=7]);

    let tail = client.stream("s1", Some((10, None))).await.unwrap();
    assert_eq!(tail.status(), 206);
    assert_eq!(tail.headers()["content-range"], "bytes 10-15/16");
    assert_eq!(tail.bytes().await.unwrap(), &AUDIO[10..]);

    // The API trait goes through the same facade.
    let api: &dyn SubsonicApi = &client;
    let via_trait = api.stream("s1", Some((0, Some(1)))).await.unwrap();
    assert_eq!(via_trait.bytes().await.unwrap(), &AUDIO[..2]);
}

#[tokio::test]
async fn stream_failures_keep_their_class() {
    let (_, served) = fixture().await;
    let client = served.client();
    for (id, expected) in [
        ("gone", StreamError::TrackGone),
        ("denied", StreamError::CredentialsRejected),
        ("unreachable", StreamError::Unreachable),
        ("other", StreamError::Rejected),
        // The client reads HTTP 501 as an upstream failure.
        ("unsupported", StreamError::Unreachable),
    ] {
        assert_eq!(client.stream(id, None).await.err(), Some(expected), "{id}");
    }
}

#[tokio::test]
async fn stream_failures_are_json_envelopes_with_a_success_status() {
    let (_, served) = fixture().await;
    for (query, status, code) in [
        ("id=gone", 200, 70),
        ("id=denied", 200, 40),
        ("id=other", 200, 0),
        ("", 200, 10),
        ("id=unsupported", 501, 0),
        ("id=unreachable", 502, 0),
    ] {
        let response = reqwest::get(format!("{}rest/stream?{query}", served.base))
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{query}");
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .contains("json")
        );
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["subsonic-response"]["status"], "failed", "{query}");
        assert_eq!(body["subsonic-response"]["error"]["code"], code, "{query}");
    }
}

#[tokio::test]
async fn jukebox_runs_every_action() {
    let (fake, served) = fixture().await;
    let client = served.client();

    client.jukebox_set(&["s1", "s2"]).await.unwrap();
    client.jukebox_add(&["s3"]).await.unwrap();
    client.jukebox_skip(1, 30).await.unwrap();
    client.jukebox_start().await.unwrap();
    client.jukebox_set_gain(0.8).await.unwrap();
    let status = SubsonicJukeboxStatus {
        current_index: Some(1),
        playing: true,
        gain: 0.8,
        position_secs: Some(30),
    };
    assert_eq!(client.jukebox_status().await.unwrap(), status);
    assert_eq!(
        client.jukebox_get().await.unwrap(),
        SubsonicJukebox {
            playlist: SubsonicJukeboxPlaylist {
                songs: vec![songs()[0].clone(), songs()[1].clone(), songs()[2].clone()],
            },
            status: status.clone(),
        }
    );

    client.jukebox_shuffle().await.unwrap();
    client.jukebox_remove(0).await.unwrap();
    client.jukebox_stop().await.unwrap();
    let after = client.jukebox_get().await.unwrap();
    assert_eq!(
        after.playlist.songs,
        vec![songs()[1].clone(), songs()[0].clone()]
    );
    assert!(!after.status.playing);

    client.jukebox_clear().await.unwrap();
    assert!(
        client
            .jukebox_get()
            .await
            .unwrap()
            .playlist
            .songs
            .is_empty()
    );
    assert_eq!(
        fake.calls(),
        [
            "jukebox set:s1,s2",
            "jukebox add:s3",
            "jukebox skip:1:30",
            "jukebox start",
            "jukebox gain:0.8",
            "jukebox shuffle",
            "jukebox remove:0",
            "jukebox stop",
            "jukebox clear",
        ]
    );
}

#[tokio::test]
async fn visualizer_feed_url_is_resolved_with_the_account() {
    let (_, served) = fixture().await;
    let client = served.client();
    let probe = client.probe().await.unwrap();
    let url = client.rune_visualizer_feed_url(&probe).await.unwrap();
    assert_eq!(url.scheme(), "ws");
    assert_eq!(url.host_str(), Some("feed.example"));
    assert_eq!(url.path(), "/rune/feed");
    assert!(
        url.query_pairs()
            .any(|(key, value)| key == "u" && value == "rune")
    );
}

#[tokio::test]
async fn rune_service_descriptor_and_login_start() {
    let (_, served) = fixture().await;
    let client = served.client();
    assert_eq!(client.rune_service().await.unwrap(), descriptor());
    assert_eq!(
        client
            .start_rune_login(RuneLoginMethod::Platform)
            .await
            .unwrap(),
        RuneLoginStart::Platform
    );
    assert_eq!(
        client
            .start_rune_login(RuneLoginMethod::Browser)
            .await
            .unwrap(),
        RuneLoginStart::Browser {
            url: "https://login.example/auth?x=1&y=2".into(),
            state: "st&te".into(),
        }
    );
    assert_eq!(
        client
            .start_rune_login(RuneLoginMethod::DeviceCode)
            .await
            .unwrap(),
        RuneLoginStart::DeviceCode {
            user_code: "ABCD-EFGH".into(),
            verification_url: "https://login.example/device".into(),
            interval_secs: 7,
        }
    );
    assert_eq!(
        client
            .start_rune_login(RuneLoginMethod::Password)
            .await
            .unwrap(),
        RuneLoginStart::Other
    );
}

#[tokio::test]
async fn login_completion_travels_in_the_body_only() {
    let (fake, served) = fixture().await;
    let client = served.client();
    client
        .complete_rune_login(&RuneLoginCompletion {
            method: RuneLoginMethod::Platform,
            state: Some("st&te".into()),
            code: Some("co de".into()),
            token: Some("user/token+==".into()),
            developer_token: Some("dev.jwt.token".into()),
        })
        .await
        .unwrap();
    let calls = fake.calls();
    for expected in [
        "login token:user/token+==",
        "login code:co de",
        "login state:st&te",
        "login developer:dev.jwt.token",
    ] {
        assert!(
            calls.iter().any(|call| call == expected),
            "{expected}: {calls:?}"
        );
    }

    let targets = served.wire.targets.lock().unwrap().clone();
    let target = targets.last().unwrap();
    assert!(
        target.starts_with("/rest/completeRuneLogin.view?"),
        "{target}"
    );
    for secret in ["user", "dev.jwt", "co+de", "co%20de", "st%26te", "st&te"] {
        assert!(!target.contains(secret), "{secret} leaked into {target}");
    }
    let body = served.wire.bodies.lock().unwrap().last().unwrap().clone();
    assert!(body.contains("method=platform"));
    assert!(body.contains("developerToken=dev.jwt.token"));

    // The facade answers a GET without reading the fake: secrets do not belong in URLs.
    let before = fake.calls().len();
    let response = reqwest::get(format!(
        "{}rest/completeRuneLogin?method=platform&token=x",
        served.base
    ))
    .await
    .unwrap();
    assert_eq!(response.status(), 405);
    assert_eq!(fake.calls().len(), before);
}

fn failing_login() -> RuneLoginCompletion {
    RuneLoginCompletion {
        method: RuneLoginMethod::Platform,
        state: Some("the-state".into()),
        code: Some("the-code".into()),
        token: Some(BAD_TOKEN.into()),
        developer_token: Some("the-developer".into()),
    }
}

#[tokio::test]
async fn login_failure_never_echoes_the_secrets() {
    let (_, served) = fixture().await;
    let error = served
        .client()
        .complete_rune_login(&failing_login())
        .await
        .unwrap_err();
    let SubsonicError::Server { kind, message, .. } = error else {
        panic!("{error:?}");
    };
    assert_eq!(kind, SubsonicServerError::WrongCredentials);
    assert_eq!(
        message,
        "rejected token=[REDACTED] code=[REDACTED] state=[REDACTED] developer=[REDACTED]"
    );

    // The facade strips them itself, before the client's own redaction.
    let response = reqwest::Client::new()
        .post(format!("{}rest/completeRuneLogin.view", served.base))
        .form(&[
            ("method", "platform"),
            ("state", "the-state"),
            ("code", "the-code"),
            ("token", BAD_TOKEN),
            ("developerToken", "the-developer"),
        ])
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for secret in [BAD_TOKEN, "the-state", "the-code", "the-developer"] {
        assert!(!response.contains(secret), "{secret} in {response}");
    }
    assert!(response.contains("rejected token=[REDACTED]"));
}

#[tokio::test]
async fn errors_keep_their_code_and_reason() {
    let (_, served) = fixture().await;
    let client = served.client();

    assert_eq!(
        client.song("missing").await.unwrap_err(),
        SubsonicError::Server {
            kind: SubsonicServerError::NotFound,
            message: "song not found".into(),
            rune_reason: None,
        }
    );
    for (code, kind) in [
        (0, SubsonicServerError::Generic),
        (10, SubsonicServerError::MissingParameter),
        (20, SubsonicServerError::ClientTooOld),
        (30, SubsonicServerError::ServerTooOld),
        (40, SubsonicServerError::WrongCredentials),
        (41, SubsonicServerError::TokenAuthenticationNotSupported),
        (42, SubsonicServerError::AuthenticationMechanismNotSupported),
        (43, SubsonicServerError::ConflictingAuthentication),
        (44, SubsonicServerError::InvalidApiKey),
        (50, SubsonicServerError::NotAuthorized),
        (70, SubsonicServerError::NotFound),
        (99, SubsonicServerError::Unknown(99)),
    ] {
        assert_eq!(
            client.song(&format!("code:{code}")).await.unwrap_err(),
            SubsonicError::Server {
                kind,
                message: format!("failed with {code}"),
                rune_reason: None,
            }
        );
    }
    for (name, reason) in [
        ("credentialsExpired", RuneReason::CredentialsExpired),
        ("notSubscribed", RuneReason::NotSubscribed),
        ("region", RuneReason::Region),
        ("removed", RuneReason::Removed),
        ("serviceLimit", RuneReason::ServiceLimit),
        ("later", RuneReason::Other),
    ] {
        let error = client.song(&format!("reason:{name}")).await.unwrap_err();
        assert_eq!(
            error,
            SubsonicError::Server {
                kind: SubsonicServerError::NotAuthorized,
                message: "refused".into(),
                rune_reason: Some(reason),
            }
        );
        assert_eq!(
            error.is_rune_credentials_expired(),
            reason == RuneReason::CredentialsExpired
        );
    }
}

/// Implements every facet and so answers `Unsupported` everywhere.
struct Nothing;
impl subsonic::api::System for Nothing {}
impl subsonic::api::Browsing for Nothing {}
impl subsonic::api::Lists for Nothing {}
impl subsonic::api::Searching for Nothing {}
impl subsonic::api::Playlists for Nothing {}
impl subsonic::api::Annotation for Nothing {}
impl subsonic::api::MediaRetrieval for Nothing {}
impl subsonic::api::Jukebox for Nothing {}
impl subsonic::api::Scanning for Nothing {}
impl subsonic::api::RuneVisualizerFeed for Nothing {}
impl subsonic::api::RuneService for Nothing {}

#[tokio::test]
async fn unsupported_endpoints_answer_501_on_every_path() {
    let served = serve(Arc::new(Nothing)).await;
    let client = served.client();

    // The client's error type has no wire form for "unsupported": it sees the HTTP status.
    assert_eq!(
        client.song("s1").await.unwrap_err(),
        SubsonicError::HttpStatus(reqwest::StatusCode::NOT_IMPLEMENTED)
    );
    let body: serde_json::Value = reqwest::get(format!("{}rest/getSong?id=s1", served.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let error = &body["subsonic-response"]["error"];
    assert_eq!(error["code"], 0);
    assert!(error["message"].as_str().unwrap().contains("getSong"));

    // A server that cannot describe itself still pings, as a plain Subsonic server.
    assert!(client.extensions().await.unwrap().is_empty());

    // Jukebox control maps 501 to its own error.
    assert_eq!(
        client.jukebox_get().await.unwrap_err(),
        SubsonicError::JukeboxUnsupported
    );
    assert_eq!(
        client.jukebox_start().await.unwrap_err(),
        SubsonicError::JukeboxUnsupported
    );

    // Streams and covers answer 501 too.
    assert_eq!(
        client.stream("s1", None).await.err(),
        Some(StreamError::Unreachable)
    );
    let error = client.cover_art("c1", None).await.unwrap_err();
    assert!(matches!(&error, SubsonicError::Transport(message) if message.contains("501")));
}

#[tokio::test]
async fn every_envelope_carries_the_probe_identity() {
    // A server that cannot probe is a plain Subsonic server on every endpoint.
    let served = serve(Arc::new(Nothing)).await;
    for endpoint in ["ping", "getSong?id=s1", "getArtists", "noSuchThing"] {
        let body: serde_json::Value = reqwest::get(format!("{}rest/{endpoint}", served.base))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let envelope = &body["subsonic-response"];
        assert_eq!(envelope["openSubsonic"], false, "{endpoint}");
        assert_eq!(envelope["version"], "1.16.1", "{endpoint}");
        assert!(envelope.get("type").is_none(), "{endpoint}");
    }

    let (_, served) = fixture().await;
    for endpoint in [
        "ping",
        "getSong?id=s1",
        "getSong?id=missing",
        "getScanStatus",
    ] {
        let body: serde_json::Value = reqwest::get(format!("{}rest/{endpoint}", served.base))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let envelope = &body["subsonic-response"];
        assert_eq!(envelope["openSubsonic"], true, "{endpoint}");
        assert_eq!(envelope["type"], "fake", "{endpoint}");
        assert_eq!(envelope["serverVersion"], "9.9", "{endpoint}");
    }
}

#[tokio::test]
async fn url_forms_and_unknown_endpoints() {
    let (_, served) = fixture().await;
    for path in ["rest/ping", "rest/ping.view"] {
        let response = reqwest::get(format!("{}{path}?f=json&u=anyone&p=whatever", served.base))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: serde_json::Value = response.json().await.unwrap();
        let envelope = &body["subsonic-response"];
        assert_eq!(envelope["status"], "ok");
        assert_eq!(envelope["version"], "1.16.1");
        assert_eq!(envelope["openSubsonic"], true);
        assert_eq!(envelope["type"], "fake");
    }

    let response = reqwest::get(format!("{}rest/noSuchThing.view", served.base))
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["subsonic-response"]["status"], "failed");
    assert_eq!(body["subsonic-response"]["error"]["code"], 0);

    let body: serde_json::Value = reqwest::get(format!("{}rest/getSong.view", served.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["subsonic-response"]["error"]["code"], 10);
}

#[tokio::test]
async fn raw_json_uses_the_documented_key_names() {
    let (_, served) = fixture().await;
    let get = |endpoint: &str| {
        let url = format!("{}rest/{endpoint}", served.base);
        async move {
            reqwest::get(url)
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        }
    };
    let body = get("getArtists?f=json").await;
    let artists = &body["subsonic-response"]["artists"];
    assert_eq!(artists["index"][0]["name"], "A");
    assert_eq!(artists["index"][0]["artist"][0]["name"], "Ada");
    assert_eq!(
        artists["index"][0]["artist"][1]["albumCount"],
        serde_json::Value::Null
    );
    assert_eq!(artists["index"][1]["artist"][0]["id"], "ar3");

    let body = get("getAlbum?id=al1").await;
    assert_eq!(
        body["subsonic-response"]["album"]["song"][0]["discNumber"],
        2
    );
    assert_eq!(
        body["subsonic-response"]["album"]["song"][0]["runePlayback"]["host"],
        "appleMusic"
    );

    let body = get("getPlaylist?id=p1").await;
    assert_eq!(
        body["subsonic-response"]["playlist"]["entry"][1]["id"],
        "s2"
    );
    assert_eq!(body["subsonic-response"]["playlist"]["runeEdit"], "full");

    let body = get("getLyricsBySongId?id=s1").await;
    let lyrics = &body["subsonic-response"]["lyricsList"];
    assert!(lyrics.get("plainLyrics").is_none());
    assert_eq!(lyrics["structuredLyrics"][0]["line"][1]["start"], 1500);
    assert_eq!(lyrics["structuredLyrics"][1]["synced"], false);
    // Unsynced lines carry no start.
    assert_eq!(lyrics["structuredLyrics"][1]["line"][0]["value"], "la la");
    assert!(
        lyrics["structuredLyrics"][1]["line"][0]
            .get("start")
            .is_none()
    );

    let body = get("getOpenSubsonicExtensions").await;
    assert_eq!(
        body["subsonic-response"]["openSubsonicExtensions"][0]["name"],
        "runeService"
    );
}

#[tokio::test]
async fn tokeninfo_serves_api_key_accounts() {
    let (_, served) = fixture().await;
    let client = served.client_with(SubsonicCredentials::ApiKey { key: "k-1".into() });
    assert_eq!(client.probe().await.unwrap(), probe());
    let targets = served.wire.targets.lock().unwrap().clone();
    assert!(
        targets
            .iter()
            .any(|target| target.starts_with("/rest/tokenInfo.view?"))
    );
    assert!(targets.iter().all(|target| target.contains("apiKey=k-1")));
    assert!(
        targets
            .iter()
            .all(|target| !target.contains("&t=") && !target.contains("&u="))
    );
}

#[tokio::test]
async fn the_client_sends_its_auth_parameters() {
    let (_, served) = fixture().await;
    served.client().song("s1").await.unwrap();
    let targets = served.wire.targets.lock().unwrap().clone();
    let target = targets.last().unwrap();
    let url = url::Url::parse(&format!("http://x{target}")).unwrap();
    let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
    assert_eq!(url.path(), "/rest/getSong.view");
    assert_eq!(query["c"], "rune");
    assert_eq!(query["v"], "1.16.1");
    assert_eq!(query["f"], "json");
    assert_eq!(query["u"], "rune");
    assert_eq!(query["id"], "s1");
    assert_eq!(query["t"].len(), 32);
    assert!(!query["s"].is_empty());
    assert!(!target.contains("pass-word") && !query.contains_key("p"));
}

#[tokio::test]
async fn ordinary_endpoints_also_take_a_form_post() {
    let (_, served) = fixture().await;
    let body: serde_json::Value = reqwest::Client::new()
        .post(format!("{}rest/getSong.view", served.base))
        .form(&[("id", "s1"), ("u", "rune")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["subsonic-response"]["song"]["id"], "s1");
}

#[tokio::test]
async fn repeated_parameters_keep_their_order() {
    let (fake, served) = fixture().await;
    let client = served.client();
    client
        .create_playlist("Order", &["s4", "s1", "s3", "s2"])
        .await
        .unwrap();
    client
        .update_playlist("p1", None, &["s3", "s1", "s2"], &[5, 1, 3, 0])
        .await
        .unwrap();
    client.star(&["s3", "s1", "s2"]).await.unwrap();
    assert_eq!(
        fake.calls(),
        [
            "create:Order:s4,s1,s3,s2",
            "update:p1:None:s3,s1,s2:5,1,3,0",
            "star:s3,s1,s2",
        ]
    );
}
