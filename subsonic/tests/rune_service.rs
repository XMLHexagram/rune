use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::IntoResponse;
use axum::routing::any;
use subsonic::{
    RuneEdit, RuneLoginCompletion, RuneLoginMethod, RuneLoginStart, RuneReason, SubsonicClient,
    SubsonicCredentials, SubsonicError, SubsonicServerError,
};

mod support;

struct Seen {
    method: Method,
    endpoint: String,
    query: String,
    content_type: Option<String>,
    body: String,
}

#[derive(Clone)]
struct Fixture {
    responses: Arc<Mutex<Vec<(String, String)>>>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

async fn handle(
    State(state): State<Fixture>,
    method: Method,
    Path(endpoint): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let name = endpoint.trim_end_matches(".view").to_owned();
    state.seen.lock().unwrap().push(Seen {
        method,
        endpoint: name.clone(),
        query: query.unwrap_or_default(),
        content_type: headers
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        body: String::from_utf8_lossy(&body).into_owned(),
    });
    let body = state
        .responses
        .lock()
        .unwrap()
        .iter()
        .find(|(endpoint, _)| *endpoint == name)
        .map(|(_, body)| body.clone())
        .unwrap_or_else(|| r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#.into());
    (StatusCode::OK, body)
}

async fn start(responses: &[(&str, &str)]) -> (SubsonicClient, Fixture, support::TestServer) {
    let fixture = Fixture {
        responses: Arc::new(Mutex::new(
            responses
                .iter()
                .map(|(endpoint, body)| ((*endpoint).into(), (*body).into()))
                .collect(),
        )),
        seen: Arc::new(Mutex::new(Vec::new())),
    };
    let app = Router::new()
        .route("/rest/{*endpoint}", any(handle))
        .with_state(fixture.clone());
    let server = support::serve(app).await;
    let client = SubsonicClient::new(
        server.url.clone(),
        SubsonicCredentials::Password {
            user: "alice".into(),
            password: "secret-password".into(),
        },
        reqwest::Client::new(),
    );
    (client, fixture, server)
}

fn ok(payload: &str) -> String {
    format!(r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1",{payload}}}}}"#)
}

#[tokio::test]
async fn descriptor_parses_and_tolerates_unknown_methods() {
    let body = ok(r#""runeService":{"id":"appleMusic","name":"Apple Music",
        "attribution":{"text":"Apple Music","link":"https://music.apple.com"},
        "login":{"methods":["platform","passkey","deviceCode"]},
        "hosts":["appleMusic"],
        "unsupported":[{"endpoint":"getLyrics"},
        {"endpoint":"updatePlaylist","param":"songIndexToRemove"}]}"#);
    let (client, _, _server) = start(&[("getRuneService", &body)]).await;
    let descriptor = client.rune_service().await.unwrap();
    assert_eq!(descriptor.id, "appleMusic");
    assert_eq!(descriptor.attribution.as_ref().unwrap().text, "Apple Music");
    assert_eq!(
        descriptor.login.methods,
        [
            RuneLoginMethod::Platform,
            RuneLoginMethod::Other,
            RuneLoginMethod::DeviceCode
        ]
    );
    assert_eq!(descriptor.hosts, ["appleMusic"]);
    assert!(descriptor.is_unsupported("getLyrics", &[]));
    assert!(descriptor.is_unsupported("getLyrics", &["artist"]));
    assert!(descriptor.is_unsupported("updatePlaylist", &["songIndexToRemove"]));
    assert!(descriptor.is_unsupported("updatePlaylist", &["name", "songIndexToRemove"]));
    assert!(!descriptor.is_unsupported("updatePlaylist", &["name"]));
    assert!(!descriptor.is_unsupported("updatePlaylist", &[]));
}

#[test]
fn unknown_values_never_fail_to_parse() {
    let methods: Vec<RuneLoginMethod> =
        serde_json::from_str(r#"["password","platform","browser","deviceCode","passkey",""]"#)
            .unwrap();
    assert_eq!(
        methods,
        [
            RuneLoginMethod::Password,
            RuneLoginMethod::Platform,
            RuneLoginMethod::Browser,
            RuneLoginMethod::DeviceCode,
            RuneLoginMethod::Other,
            RuneLoginMethod::Other,
        ]
    );
    assert_eq!(RuneLoginMethod::Other.as_str(), None);
    assert_eq!(RuneLoginMethod::DeviceCode.as_str(), Some("deviceCode"));
}

#[tokio::test]
async fn extensions_come_from_the_public_endpoints_only() {
    let ping = r#"{"subsonic-response":{"status":"ok","version":"1.16.1","openSubsonic":true}}"#;
    let extensions = ok(
        r#""openSubsonicExtensions":[{"name":"runeService","versions":[1]},{"name":"songLyrics","versions":[1]}]"#,
    );
    let (client, fixture, _server) =
        start(&[("ping", ping), ("getOpenSubsonicExtensions", &extensions)]).await;
    assert_eq!(
        client.extensions().await.unwrap(),
        [
            ("runeService".to_owned(), vec![1]),
            ("songLyrics".to_owned(), vec![1])
        ]
    );
    let seen: Vec<_> = fixture
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|seen| seen.endpoint.clone())
        .collect();
    assert_eq!(seen, ["ping", "getOpenSubsonicExtensions"]);
}

#[tokio::test]
async fn minimal_descriptor_defaults_every_list() {
    let body = ok(r#""runeService":{"id":"x","name":"X"}"#);
    let (client, _, _server) = start(&[("getRuneService", &body)]).await;
    let descriptor = client.rune_service().await.unwrap();
    assert!(descriptor.login.methods.is_empty());
    assert!(descriptor.hosts.is_empty());
    assert!(descriptor.unsupported.is_empty());
    assert_eq!(descriptor.attribution, None);
}

#[tokio::test]
async fn start_variants_parse() {
    let platform = ok(r#""runeLogin":{"method":"platform"}"#);
    let browser = ok(r#""runeLogin":{"method":"browser","url":"https://x/y","state":"st"}"#);
    let device = ok(
        r#""runeLogin":{"method":"deviceCode","userCode":"AB-12","verificationUrl":"https://x/d","interval":7}"#,
    );
    let future = ok(r#""runeLogin":{"method":"qr"}"#);
    let (client, fixture, _server) = start(&[("startRuneLogin", &platform)]).await;
    assert_eq!(
        client
            .start_rune_login(RuneLoginMethod::Platform)
            .await
            .unwrap(),
        RuneLoginStart::Platform
    );
    assert!(
        fixture.seen.lock().unwrap()[0]
            .query
            .contains("method=platform")
    );
    for (body, expected) in [
        (
            browser,
            RuneLoginStart::Browser {
                url: "https://x/y".into(),
                state: "st".into(),
            },
        ),
        (
            device,
            RuneLoginStart::DeviceCode {
                user_code: "AB-12".into(),
                verification_url: "https://x/d".into(),
                interval_secs: 7,
            },
        ),
        (future, RuneLoginStart::Other),
    ] {
        *fixture.responses.lock().unwrap() = vec![("startRuneLogin".into(), body)];
        assert_eq!(
            client
                .start_rune_login(RuneLoginMethod::Browser)
                .await
                .unwrap(),
            expected
        );
    }
}

#[tokio::test]
async fn browser_start_without_url_is_malformed() {
    let body = ok(r#""runeLogin":{"method":"browser","state":"st"}"#);
    let (client, _, _server) = start(&[("startRuneLogin", &body)]).await;
    assert!(matches!(
        client.start_rune_login(RuneLoginMethod::Browser).await,
        Err(SubsonicError::Malformed(_))
    ));
}

#[tokio::test]
async fn complete_posts_a_form_and_keeps_tokens_out_of_the_url() {
    let (client, fixture, _server) = start(&[]).await;
    client
        .complete_rune_login(&RuneLoginCompletion {
            method: RuneLoginMethod::Platform,
            state: None,
            code: None,
            token: Some("user-token-123".into()),
            developer_token: Some("dev-token-456".into()),
        })
        .await
        .unwrap();
    let seen = fixture.seen.lock().unwrap();
    let request = &seen[0];
    assert_eq!(request.method, Method::POST);
    assert_eq!(request.endpoint, "completeRuneLogin");
    assert!(
        request
            .content_type
            .as_deref()
            .unwrap()
            .starts_with("application/x-www-form-urlencoded")
    );
    let body: Vec<(String, String)> = url::form_urlencoded::parse(request.body.as_bytes())
        .into_owned()
        .collect();
    assert!(body.contains(&("method".into(), "platform".into())));
    assert!(body.contains(&("token".into(), "user-token-123".into())));
    assert!(body.contains(&("developerToken".into(), "dev-token-456".into())));
    assert!(!body.iter().any(|(key, _)| key == "state" || key == "code"));
    assert!(!request.query.contains("token-"));
    assert!(!request.query.contains("platform"));
    assert!(request.query.contains("u=alice"));
}

#[tokio::test]
async fn complete_failures_redact_the_posted_secrets() {
    let body = r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":50,"message":"bad token user-token-123","runeReason":"credentialsExpired"}}}"#;
    let (client, _, _server) = start(&[("completeRuneLogin", body)]).await;
    let completion = RuneLoginCompletion {
        method: RuneLoginMethod::Platform,
        state: None,
        code: None,
        token: Some("user-token-123".into()),
        developer_token: None,
    };
    let error = client.complete_rune_login(&completion).await.unwrap_err();
    assert_eq!(
        error,
        SubsonicError::Server {
            kind: SubsonicServerError::NotAuthorized,
            message: "bad token [REDACTED]".into(),
            rune_reason: Some(RuneReason::CredentialsExpired),
        }
    );
    assert!(!format!("{completion:?}").contains("user-token-123"));
}

#[tokio::test]
async fn unknown_login_method_is_not_sent() {
    let (client, fixture, _server) = start(&[]).await;
    let completion = RuneLoginCompletion {
        method: RuneLoginMethod::Other,
        state: None,
        code: None,
        token: None,
        developer_token: None,
    };
    assert!(matches!(
        client.complete_rune_login(&completion).await,
        Err(SubsonicError::Unsupported { .. })
    ));
    assert!(matches!(
        client.start_rune_login(RuneLoginMethod::Other).await,
        Err(SubsonicError::Unsupported { .. })
    ));
    assert!(fixture.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn rune_reasons_parse_and_unknown_values_become_other() {
    for (wire, expected) in [
        ("credentialsExpired", RuneReason::CredentialsExpired),
        ("notSubscribed", RuneReason::NotSubscribed),
        ("region", RuneReason::Region),
        ("removed", RuneReason::Removed),
        ("serviceLimit", RuneReason::ServiceLimit),
        ("tomorrow", RuneReason::Other),
    ] {
        let body = format!(
            r#"{{"subsonic-response":{{"status":"failed","version":"1.16.1","error":{{"code":0,"message":"no","runeReason":"{wire}"}}}}}}"#
        );
        let (client, _, _server) = start(&[("getSong", &body)]).await;
        assert_eq!(
            client.song("s1").await.unwrap_err(),
            SubsonicError::Server {
                kind: SubsonicServerError::Generic,
                message: "no".into(),
                rune_reason: Some(expected),
            }
        );
    }
    let plain = r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":70,"message":"gone"}}}"#;
    let (client, _, _server) = start(&[("getSong", plain)]).await;
    assert!(matches!(
        client.song("s1").await.unwrap_err(),
        SubsonicError::Server {
            rune_reason: None,
            ..
        }
    ));
}

#[tokio::test]
async fn playlists_carry_rune_edit() {
    let list = ok(
        r#""playlists":{"playlist":[{"id":"a","name":"A","runeEdit":"appendOnly"},{"id":"b","name":"B","runeEdit":"readOnly"},{"id":"c","name":"C","runeEdit":"sometimes"},{"id":"d","name":"D"}]}"#,
    );
    let detail = ok(r#""playlist":{"id":"a","name":"A","runeEdit":"full","entry":[]}"#);
    let (client, _, _server) = start(&[("getPlaylists", &list), ("getPlaylist", &detail)]).await;
    let edits: Vec<_> = client
        .playlists()
        .await
        .unwrap()
        .into_iter()
        .map(|playlist| playlist.rune_edit)
        .collect();
    assert_eq!(
        edits,
        [
            Some(RuneEdit::AppendOnly),
            Some(RuneEdit::ReadOnly),
            Some(RuneEdit::Unknown),
            None
        ]
    );
    assert_eq!(
        client.playlist("a").await.unwrap().rune_edit,
        Some(RuneEdit::Full)
    );
}
