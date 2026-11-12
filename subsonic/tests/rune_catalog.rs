//! What the client sends for Rune's `runeCatalog` and `runeRecommendations` extensions.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::routing::get;
use subsonic::{
    RuneRecommendationKind, RuneScope, SearchLimits, SubsonicClient, SubsonicCredentials,
};

mod support;

type Seen = Arc<Mutex<Vec<(String, Vec<(String, String)>)>>>;

async fn handle(
    State(seen): State<Seen>,
    Path(endpoint): Path<String>,
    RawQuery(query): RawQuery,
) -> String {
    let endpoint = endpoint.trim_end_matches(".view").to_owned();
    let pairs = url::form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .into_owned()
        .filter(|(key, _)| !matches!(key.as_str(), "u" | "t" | "s" | "c" | "v" | "f"))
        .collect();
    seen.lock().unwrap().push((endpoint.clone(), pairs));
    let payload = match endpoint.as_str() {
        "search3" => {
            r#","searchResult3":{"song":[{"id":"catalog:song:1","title":"T","runeInLibrary":false}]}"#
        }
        "getRuneRecommendations" => {
            r#","runeRecommendations":{"group":[{"id":"g","title":"For You","kind":"personal",
                "album":[{"id":"catalog:album:1","name":"A"}]}]}"#
        }
        _ => "",
    };
    format!(r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1"{payload}}}}}"#)
}

async fn start() -> (SubsonicClient, Seen, support::TestServer) {
    let seen = Seen::default();
    let app = Router::new()
        .route("/rest/{*endpoint}", get(handle))
        .with_state(seen.clone());
    let server = support::serve(app).await;
    let client = SubsonicClient::new(
        server.url.clone(),
        SubsonicCredentials::Password {
            user: "alice".into(),
            password: "secret-password".into(),
        },
        reqwest::Client::new(),
    );
    (client, seen, server)
}

fn value<'a>(pairs: &'a [(String, String)], key: &str) -> Vec<&'a str> {
    pairs
        .iter()
        .filter(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
        .collect()
}

#[tokio::test]
async fn plain_search_sends_no_scope_and_scoped_search_does() {
    let (client, seen, _server) = start().await;
    let limits = SearchLimits {
        artists: 1,
        albums: 2,
        songs: 3,
    };
    client.search("dawn", limits).await.unwrap();
    let found = client
        .search_in_scope("dawn", limits, 4, RuneScope::All)
        .await
        .unwrap();
    assert_eq!(found.songs[0].rune_in_library, Some(false));
    client
        .search_in_scope("dawn", limits, 0, RuneScope::Catalog)
        .await
        .unwrap();

    let seen = seen.lock().unwrap();
    assert!(value(&seen[0].1, "runeScope").is_empty());
    assert_eq!(value(&seen[1].1, "runeScope"), ["all"]);
    assert_eq!(value(&seen[1].1, "songOffset"), ["4"]);
    assert_eq!(value(&seen[1].1, "songCount"), ["3"]);
    assert_eq!(value(&seen[2].1, "runeScope"), ["catalog"]);
}

#[tokio::test]
async fn library_additions_repeat_the_id_and_recommendations_send_the_count() {
    let (client, seen, _server) = start().await;
    client
        .add_to_library(&["catalog:song:1", "catalog:album:2"])
        .await
        .unwrap();
    let groups = client.recommendations(7).await.unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].kind, RuneRecommendationKind::Personal);
    assert_eq!(groups[0].albums[0].id, "catalog:album:1");

    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].0, "addToRuneLibrary");
    assert_eq!(
        value(&seen[0].1, "id"),
        ["catalog:song:1", "catalog:album:2"]
    );
    assert_eq!(seen[1].0, "getRuneRecommendations");
    assert_eq!(value(&seen[1].1, "count"), ["7"]);
}
