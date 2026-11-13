//! Authentication, retries and error mapping against the fixture server.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use apple_music::{AppleMusicClient, Error, RetryPolicy, StaticTokens};
use common::{client, FixtureServer, Reply, TestTokens};

#[tokio::test]
async fn user_endpoint_sends_both_tokens() {
    let server = FixtureServer::start().await;
    server.on(
        "GET",
        "/v1/me/storefront",
        Reply::fixture("storefront.json"),
    );
    let c = client(&server, TestTokens::new());

    let storefront = c.user_storefront().await.unwrap();
    assert_eq!(storefront.id, "it");

    let seen = server.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].authorization.as_deref(), Some("Bearer dev-0"));
    assert_eq!(seen[0].user_token.as_deref(), Some("user-0"));
}

#[tokio::test]
async fn catalog_endpoint_sends_only_the_developer_token() {
    let server = FixtureServer::start().await;
    server.on(
        "GET",
        "/v1/catalog/it/songs",
        Reply::fixture("catalog_songs.json"),
    );
    let c = client(&server, TestTokens::without_user());

    c.catalog_songs(&["617154366".to_owned()]).await.unwrap();

    let seen = server.seen();
    assert_eq!(seen[0].authorization.as_deref(), Some("Bearer dev-0"));
    assert_eq!(seen[0].user_token, None);
}

#[tokio::test]
async fn user_endpoint_without_user_token_fails_before_sending() {
    let server = FixtureServer::start().await;
    let c = client(&server, TestTokens::without_user());

    let err = c.user_storefront().await.unwrap_err();
    assert!(matches!(err, Error::MissingUserToken), "{err:?}");
    assert!(server.seen().is_empty());
}

#[tokio::test]
async fn unauthorized_refreshes_user_token_then_succeeds() {
    let server = FixtureServer::start().await;
    server
        .on(
            "GET",
            "/v1/me/storefront",
            Reply::fixture_status(401, "error_unauthorized.json"),
        )
        .on(
            "GET",
            "/v1/me/storefront",
            Reply::fixture("storefront.json"),
        );
    let tokens = TestTokens::new();
    let c = client(&server, tokens.clone());

    c.user_storefront().await.unwrap();

    let seen = server.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[1].user_token.as_deref(), Some("user-1"));
    assert_eq!(seen[1].authorization.as_deref(), Some("Bearer dev-0"));
    assert_eq!(tokens.user_refreshes(), 1);
    assert_eq!(tokens.dev_refreshes(), 0);
}

#[tokio::test]
async fn unauthorized_twice_refreshes_developer_token_then_succeeds() {
    let server = FixtureServer::start().await;
    server
        .on(
            "GET",
            "/v1/me/storefront",
            Reply::fixture_status(401, "error_unauthorized.json"),
        )
        .on(
            "GET",
            "/v1/me/storefront",
            Reply::fixture_status(401, "error_unauthorized.json"),
        )
        .on(
            "GET",
            "/v1/me/storefront",
            Reply::fixture("storefront.json"),
        );
    let tokens = TestTokens::new();
    let c = client(&server, tokens.clone());

    c.user_storefront().await.unwrap();

    let seen = server.seen();
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[2].authorization.as_deref(), Some("Bearer dev-1"));
    assert_eq!(tokens.user_refreshes(), 1);
    assert_eq!(tokens.dev_refreshes(), 1);
}

#[tokio::test]
async fn unauthorized_forever_gives_up_after_one_refresh_of_each() {
    let server = FixtureServer::start().await;
    server.on(
        "GET",
        "/v1/me/storefront",
        Reply::fixture_status(401, "error_unauthorized.json"),
    );
    let tokens = TestTokens::new();
    let c = client(&server, tokens.clone());

    let err = c.user_storefront().await.unwrap_err();
    assert!(matches!(err, Error::Unauthorized), "{err:?}");
    assert_eq!(server.seen().len(), 3);
    assert_eq!(tokens.user_refreshes(), 1);
    assert_eq!(tokens.dev_refreshes(), 1);
}

#[tokio::test]
async fn catalog_unauthorized_refreshes_only_the_developer_token() {
    let server = FixtureServer::start().await;
    server.on(
        "GET",
        "/v1/catalog/it/songs",
        Reply::fixture_status(401, "error_unauthorized.json"),
    );
    let tokens = TestTokens::new();
    let c = client(&server, tokens.clone());

    let err = c.catalog_songs(&["1".to_owned()]).await.unwrap_err();
    assert!(matches!(err, Error::Unauthorized), "{err:?}");
    assert_eq!(server.seen().len(), 2);
    assert_eq!(tokens.user_refreshes(), 0);
    assert_eq!(tokens.dev_refreshes(), 1);
}

#[tokio::test]
async fn forbidden_maps_to_forbidden() {
    let server = FixtureServer::start().await;
    server.on(
        "GET",
        "/v1/me/storefront",
        Reply::fixture_status(403, "error_forbidden.json"),
    );
    let tokens = TestTokens::new();
    let c = client(&server, tokens.clone());

    let err = c.user_storefront().await.unwrap_err();
    assert!(matches!(err, Error::Forbidden), "{err:?}");
    // One user token refresh is tried: Apple answers 403 for an expired user token.
    assert_eq!(server.seen().len(), 2);
}

#[tokio::test]
async fn not_found_with_client_not_found_body_is_client_not_registered() {
    let server = FixtureServer::start().await;
    server.on(
        "GET",
        "/v1/me/storefront",
        Reply::fixture_status(404, "error_client_not_found.json"),
    );
    let c = client(&server, TestTokens::new());

    let err = c.user_storefront().await.unwrap_err();
    assert!(matches!(err, Error::ClientNotRegistered), "{err:?}");
    assert_eq!(server.seen().len(), 1, "404 must not be retried");
}

#[tokio::test]
async fn other_not_found_is_not_found() {
    let server = FixtureServer::start().await;
    server.on(
        "GET",
        "/v1/me/library/playlists/p.missing/tracks",
        Reply::fixture_status(404, "error_not_found.json"),
    );
    let c = client(&server, TestTokens::new());

    use futures_util::TryStreamExt;
    let err = c
        .library_playlist_tracks("p.missing")
        .try_collect::<Vec<_>>()
        .await
        .unwrap_err();
    match err {
        Error::NotFound(message) => assert!(message.contains("p.missing"), "{message}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn rate_limit_honours_retry_after_then_succeeds() {
    let server = FixtureServer::start().await;
    server
        .on(
            "GET",
            "/v1/me/storefront",
            Reply::status(429).header("Retry-After", "1"),
        )
        .on(
            "GET",
            "/v1/me/storefront",
            Reply::fixture("storefront.json"),
        );
    let c = client(&server, TestTokens::new());

    let started = Instant::now();
    c.user_storefront().await.unwrap();

    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(server.seen().len(), 2);
}

#[tokio::test]
async fn rate_limit_without_retry_after_backs_off_and_gives_up() {
    let server = FixtureServer::start().await;
    server.on("GET", "/v1/me/storefront", Reply::status(429));
    let c = client(&server, TestTokens::new());

    let err = c.user_storefront().await.unwrap_err();
    assert!(
        matches!(err, Error::RateLimited { retry_after: None }),
        "{err:?}"
    );
    assert_eq!(server.seen().len(), 5);
}

#[tokio::test]
async fn server_errors_are_retried_five_times_then_reported() {
    let server = FixtureServer::start().await;
    server.on(
        "GET",
        "/v1/me/storefront",
        Reply::fixture_status(503, "error_server.json"),
    );
    let c = client(&server, TestTokens::new());

    let err = c.user_storefront().await.unwrap_err();
    match err {
        Error::Server { status, errors } => {
            assert_eq!(status, 503);
            assert_eq!(errors[0].title.as_deref(), Some("Internal Server Error"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(server.seen().len(), 5);
}

#[tokio::test]
async fn server_error_then_success_recovers() {
    let server = FixtureServer::start().await;
    server
        .on(
            "GET",
            "/v1/me/storefront",
            Reply::fixture_status(500, "error_server.json"),
        )
        .on(
            "GET",
            "/v1/me/storefront",
            Reply::fixture("storefront.json"),
        );
    let c = client(&server, TestTokens::new());

    assert_eq!(c.user_storefront().await.unwrap().id, "it");
}

#[tokio::test]
async fn backoff_delays_grow_and_stay_capped() {
    let server = FixtureServer::start().await;
    server.on("GET", "/v1/me/storefront", Reply::status(500));
    let c = AppleMusicClient::builder(TestTokens::new())
        .base_url(server.base_url.clone())
        .retry_policy(RetryPolicy {
            max_attempts: 4,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(200),
            max_retry_after: Duration::from_secs(1),
        })
        .build();

    let started = Instant::now();
    let _ = c.user_storefront().await;
    // Three waits: 100, 200 and 200 ms, each with jitter between 50% and 100%.
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(250), "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(1500), "{elapsed:?}");
}

#[tokio::test]
async fn concurrency_is_capped() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::routing::get;

    let current = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let app = {
        let (current, peak) = (current.clone(), peak.clone());
        axum::Router::new().route(
            "/v1/catalog/it/songs",
            get(move || {
                let (current, peak) = (current.clone(), peak.clone());
                async move {
                    let now = current.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    current.fetch_sub(1, Ordering::SeqCst);
                    r#"{"data":[]}"#
                }
            }),
        )
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let c = AppleMusicClient::builder(Arc::new(StaticTokens::new("dev", None)))
        .base_url(base)
        .storefront("it")
        .max_concurrent_requests(2)
        .build();

    let tasks: Vec<_> = (0..8)
        .map(|i| {
            let c = c.clone();
            tokio::spawn(async move { c.catalog_songs(&[i.to_string()]).await.unwrap() })
        })
        .collect();
    for t in tasks {
        t.await.unwrap();
    }
    assert_eq!(peak.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn post_is_not_retried_on_server_error() {
    let server = FixtureServer::start().await;
    server.on(
        "POST",
        "/v1/me/library/playlists",
        Reply::fixture_status(500, "error_server.json"),
    );
    let c = client(&server, TestTokens::new());

    let err = c
        .create_library_playlist("Once", None, &[])
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Server { status: 500, .. }), "{err:?}");
    assert_eq!(server.seen().len(), 1);
}

#[tokio::test]
async fn post_is_not_retried_on_transport_error() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let accepted = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    {
        let accepted = accepted.clone();
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                accepted.fetch_add(1, Ordering::SeqCst);
                drop(socket);
            }
        });
    }
    let c = AppleMusicClient::builder(TestTokens::new())
        .base_url(base)
        .retry_policy(common::fast_retry())
        .build();

    let err = c
        .create_library_playlist("Once", None, &[])
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Http(_)), "{err:?}");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn post_is_retried_on_rate_limit() {
    let server = FixtureServer::start().await;
    server
        .on(
            "POST",
            "/v1/me/library/playlists",
            Reply::status(429).header("Retry-After", "1"),
        )
        .on(
            "POST",
            "/v1/me/library/playlists",
            Reply::fixture_status(201, "playlist_created.json"),
        );
    let c = client(&server, TestTokens::new());

    let started = Instant::now();
    c.create_library_playlist("Retried", None, &[])
        .await
        .unwrap();

    assert!(started.elapsed() >= Duration::from_secs(1));
    assert_eq!(server.seen().len(), 2);
}

#[tokio::test]
async fn put_is_still_retried_on_server_error() {
    let server = FixtureServer::start().await;
    server
        .on(
            "PUT",
            "/v1/me/ratings/songs/1",
            Reply::fixture_status(500, "error_server.json"),
        )
        .on(
            "PUT",
            "/v1/me/ratings/songs/1",
            Reply::fixture("rating_put_catalog_song.json"),
        );
    let c = client(&server, TestTokens::new());

    c.set_catalog_song_rating("1", apple_music::models::RatingValue::Like)
        .await
        .unwrap();
    assert_eq!(server.seen().len(), 2);
}
