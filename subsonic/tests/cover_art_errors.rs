use axum::{Router, http::header, response::IntoResponse, routing::get};
use subsonic::{SubsonicClient, SubsonicCredentials, SubsonicError, SubsonicServerError};

mod support;

async fn client(app: Router) -> (SubsonicClient, support::TestServer) {
    let server = support::serve(app).await;
    let client = SubsonicClient::new(
        server.url.clone(),
        SubsonicCredentials::Password {
            user: "alice".into(),
            password: "secret".into(),
        },
        reqwest::Client::new(),
    );
    (client, server)
}

fn reply(content_type: &'static str, body: &'static str) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, content_type)], body)
}

#[tokio::test]
async fn json_error_with_a_success_status_is_an_error_not_an_image() {
    let app = Router::new().route(
        "/rest/getCoverArt.view",
        get(|| async {
            reply(
                "application/json",
                r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":70,"message":"Cover art not found for secret"}}}"#,
            )
        }),
    );
    let (client, _server) = client(app).await;
    assert_eq!(
        client.cover_art("x", None).await.unwrap_err(),
        SubsonicError::Server {
            kind: SubsonicServerError::NotFound,
            message: "Cover art not found for [REDACTED]".into(),
        }
    );
}

#[tokio::test]
async fn other_documents_are_malformed() {
    let app = Router::new()
        .route(
            "/rest/getCoverArt.view",
            get(|| async {
                reply(
                    "text/xml",
                    r#"<subsonic-response status="failed"><error code="70"/></subsonic-response>"#,
                )
            }),
        )
        .route(
            "/rest/getCoverArt",
            get(|| async {
                reply(
                    "application/json",
                    r#"{"subsonic-response":{"status":"ok"}}"#,
                )
            }),
        );
    let (client, _server) = client(app).await;
    assert!(matches!(
        client.cover_art("x", None).await,
        Err(SubsonicError::Malformed(_))
    ));
}

#[tokio::test]
async fn images_pass_through() {
    let app = Router::new().route(
        "/rest/getCoverArt.view",
        get(|| async {
            (
                [(header::CONTENT_TYPE, "image/png")],
                vec![0x89, b'P', b'N', b'G'],
            )
        }),
    );
    let (client, _server) = client(app).await;
    assert_eq!(
        client.cover_art("x", None).await.unwrap(),
        [0x89, b'P', b'N', b'G']
    );
}
