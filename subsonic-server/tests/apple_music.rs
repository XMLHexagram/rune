//! `AppleMusicServer` behind the facade, read by a real `SubsonicClient`, without network.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use apple_music::AppleMusicServer;
use axum::Router;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::routing::get;
use common::serve;
use subsonic::{
    RuneLoginCompletion, RuneLoginMethod, RuneLoginStart, RunePlaybackMode, RuneReason,
    SubsonicError, SubsonicServerError,
};

fn platform(token: Option<&str>, developer: Option<&str>) -> RuneLoginCompletion {
    RuneLoginCompletion {
        method: RuneLoginMethod::Platform,
        state: None,
        code: None,
        token: token.map(Into::into),
        developer_token: developer.map(Into::into),
    }
}

#[tokio::test]
async fn probe_and_extensions_declare_rune_service() {
    let served = serve(Arc::new(AppleMusicServer::new())).await;
    let client = served.client();
    let probe = client.probe().await.unwrap();
    assert!(probe.open_subsonic);
    assert!(probe.supports_rune_service());
    assert!(!probe.supports_rune_visualizer_feed());
    assert_eq!(probe.server_type.as_deref(), Some("rune-apple-music"));
    assert!(probe.roles.stream_role);
    assert!(probe.roles_known);
    assert_eq!(
        client.extensions().await.unwrap(),
        [("runeService".to_owned(), vec![1])]
    );
}

#[tokio::test]
async fn descriptor_comes_through_unchanged() {
    let direct = Arc::new(AppleMusicServer::new());
    let served = serve(direct.clone()).await;
    let descriptor = served.client().rune_service().await.unwrap();
    assert_eq!(
        descriptor,
        subsonic::api::RuneService::rune_service(&*direct)
            .await
            .unwrap()
    );
    assert_eq!(descriptor.id, "appleMusic");
    assert_eq!(descriptor.login.methods, [RuneLoginMethod::Platform]);
    assert_eq!(descriptor.hosts, ["appleMusic"]);
    assert!(descriptor.is_unsupported("stream", &[]));
    assert!(descriptor.is_unsupported("updatePlaylist", &["name"]));
    assert!(!descriptor.is_unsupported("updatePlaylist", &["songIdToAdd"]));
}

#[tokio::test]
async fn library_calls_before_sign_in_say_credentials_expired() {
    let served = serve(Arc::new(AppleMusicServer::new())).await;
    let client = served.client();
    let errors = [
        client.song("i.x").await.unwrap_err(),
        client.artists().await.unwrap_err(),
        client.playlists().await.unwrap_err(),
        client.album_list(10, 0).await.unwrap_err(),
    ];
    for error in errors {
        assert!(
            matches!(
                &error,
                SubsonicError::Server {
                    kind: SubsonicServerError::NotAuthorized,
                    rune_reason: Some(RuneReason::CredentialsExpired),
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(error.is_rune_credentials_expired());
    }
}

#[tokio::test]
async fn login_starts_on_the_platform_only() {
    let served = serve(Arc::new(AppleMusicServer::new())).await;
    let client = served.client();
    assert_eq!(
        client
            .start_rune_login(RuneLoginMethod::Platform)
            .await
            .unwrap(),
        RuneLoginStart::Platform
    );
    // Unsupported has no wire form the client could map back: it sees the HTTP 501.
    for method in [RuneLoginMethod::Browser, RuneLoginMethod::DeviceCode] {
        assert_eq!(
            client.start_rune_login(method).await.unwrap_err(),
            SubsonicError::HttpStatus(reqwest::StatusCode::NOT_IMPLEMENTED)
        );
    }

    let error = client
        .complete_rune_login(&platform(Some("user"), None))
        .await
        .unwrap_err();
    assert!(
        matches!(
            &error,
            SubsonicError::Server {
                kind: SubsonicServerError::MissingParameter,
                ..
            }
        ),
        "{error:?}"
    );
}

const USER_TOKEN: &str = "music-user/token+==";
const DEVELOPER_TOKEN: &str = "developer.jwt";

/// A request Apple saw: path and query as sent, and the two token headers.
struct Request {
    target: String,
    path: String,
    authorization: Option<String>,
    user_token: Option<String>,
}

struct Apple {
    base: String,
    seen: Arc<Mutex<Vec<Request>>>,
}

impl Apple {
    fn requests(&self) -> Vec<Request> {
        std::mem::take(&mut *self.seen.lock().unwrap())
    }
}

/// The recorded answers of the `apple-music` crate's fixtures.
fn fixture(name: &str) -> String {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../apple-music/tests/fixtures/"
    );
    std::fs::read_to_string(format!("{path}{name}")).unwrap()
}

/// A tiny Apple answering `routes` by path with a status and a body, and an empty list
/// for everything else.
async fn apple(routes: Vec<(&'static str, u16, String)>) -> Apple {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let routes: Arc<HashMap<&'static str, (u16, String)>> = Arc::new(
        routes
            .into_iter()
            .map(|(path, status, body)| (path, (status, body)))
            .collect(),
    );
    let app = Router::new().fallback(get(move |uri: Uri, headers: HeaderMap| {
        let log = log.clone();
        let routes = routes.clone();
        async move {
            let header = |name: &str| {
                headers
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned)
            };
            log.lock().unwrap().push(Request {
                target: uri.to_string(),
                path: uri.path().to_owned(),
                authorization: header("authorization"),
                user_token: header("music-user-token"),
            });
            let (status, body) = routes
                .get(uri.path())
                .cloned()
                .unwrap_or((200, r#"{"data":[]}"#.to_owned()));
            (
                StatusCode::from_u16(status).unwrap(),
                [("content-type", "application/json")],
                body,
            )
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Apple { base, seen }
}

/// A client of an `AppleMusicServer` of `apple`, behind the facade, signed in.
async fn signed_in(apple: &Apple) -> (subsonic::SubsonicClient, common::Served) {
    let base = apple.base.clone();
    let server = AppleMusicServer::with_client(|builder| builder.base_url(base).storefront("it"));
    let served = serve(Arc::new(server)).await;
    let client = served.client();
    client
        .complete_rune_login(&platform(Some(USER_TOKEN), Some(DEVELOPER_TOKEN)))
        .await
        .unwrap();
    (client, served)
}

/// Every Apple request carries the developer token, the user-library ones carry the user
/// token as well, and neither is ever part of a URL, on either hop.
fn assert_tokens_travel_in_headers(apple: &[Request], served: &common::Served) {
    assert!(!apple.is_empty());
    for request in apple {
        assert_eq!(
            request.authorization.as_deref(),
            Some("Bearer developer.jwt")
        );
        if request.path.starts_with("/v1/me/") {
            assert_eq!(
                request.user_token.as_deref(),
                Some(USER_TOKEN),
                "{}",
                request.target
            );
        }
        assert!(!request.target.contains("developer"), "{}", request.target);
        assert!(!request.target.contains("token"), "{}", request.target);
    }
    for target in served.wire.targets.lock().unwrap().iter() {
        assert!(
            !target.contains("developer.jwt") && !target.contains("music-user"),
            "{target}"
        );
    }
}

#[tokio::test]
async fn sign_in_through_the_facade_unlocks_hosted_songs() {
    let apple = apple(vec![
        ("/v1/catalog/it/songs", 200, fixture("catalog_songs.json")),
        (
            "/v1/me/library/songs",
            200,
            fixture("server/library_song.json"),
        ),
        (
            "/v1/me/library/playlists",
            200,
            fixture("server/library_playlists.json"),
        ),
        (
            "/v1/me/library/search",
            200,
            fixture("server/library_search.json"),
        ),
    ])
    .await;
    let (client, served) = signed_in(&apple).await;

    let catalog = client.song("catalog:song:617154366").await.unwrap();
    assert_eq!(catalog.title, "Get Lucky");
    assert_eq!(catalog.track, Some(8));
    assert_eq!(catalog.duration, Some(370));
    let playback = catalog.rune_playback.unwrap();
    assert_eq!(playback.mode, RunePlaybackMode::Hosted);
    assert_eq!(playback.host.as_deref(), Some("appleMusic"));
    assert_eq!(playback.key.as_deref(), Some("catalog:617154366"));

    let library = client.song("i.lib1").await.unwrap();
    assert_eq!(library.title, "First Light");
    assert_eq!(library.album.as_deref(), Some("Dawn Album"));
    assert_eq!(library.genre.as_deref(), Some("Ambient"));
    assert_eq!(
        library.rune_playback.unwrap().key.as_deref(),
        Some("i.lib1")
    );

    let playlists = client.playlists().await.unwrap();
    let names: Vec<_> = playlists
        .iter()
        .map(|playlist| playlist.name.as_str())
        .collect();
    assert!(names.contains(&"Road Trip"), "{names:?}");
    assert_eq!(playlists[0].id, "p.mine1");

    let found = client
        .search(
            "dawn",
            subsonic::SearchLimits {
                artists: 5,
                albums: 5,
                songs: 5,
            },
        )
        .await
        .unwrap();
    assert_eq!(found.songs[0].title, "First Light");
    assert_eq!(found.albums[0].name, "Dawn Album");

    assert_tokens_travel_in_headers(&apple.requests(), &served);
}

#[tokio::test]
async fn apple_rejecting_the_tokens_reads_as_an_expired_sign_in() {
    let unauthorized = fixture("error_unauthorized.json");
    let apple = apple(vec![
        ("/v1/me/library/artists", 401, unauthorized.clone()),
        ("/v1/me/storefront", 401, unauthorized),
    ])
    .await;
    let (client, served) = signed_in(&apple).await;
    let error = client.artists().await.unwrap_err();
    assert!(
        matches!(
            &error,
            SubsonicError::Server {
                kind: SubsonicServerError::NotAuthorized,
                rune_reason: Some(RuneReason::CredentialsExpired),
                ..
            }
        ),
        "{error:?}"
    );
    assert_tokens_travel_in_headers(&apple.requests(), &served);
}

#[tokio::test]
async fn other_refusals_keep_their_meaning() {
    let apple = apple(vec![
        (
            "/v1/me/library/playlists",
            403,
            fixture("error_forbidden.json"),
        ),
        ("/v1/me/storefront", 200, fixture("storefront.json")),
        ("/v1/me/library/songs", 404, fixture("error_not_found.json")),
    ])
    .await;
    let (client, _served) = signed_in(&apple).await;

    // Apple refused this request, but the tokens work: not an expired sign-in.
    let error = client.playlists().await.unwrap_err();
    assert!(!error.is_rune_credentials_expired(), "{error:?}");
    assert!(
        matches!(&error, SubsonicError::Server { kind: SubsonicServerError::Generic, message, .. }
            if message == "Apple Music refused the request"),
        "{error:?}"
    );

    let error = client.song("i.missing").await.unwrap_err();
    assert!(
        matches!(
            &error,
            SubsonicError::Server {
                kind: SubsonicServerError::NotFound,
                ..
            }
        ),
        "{error:?}"
    );
}
