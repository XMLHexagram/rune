//! Endpoint behaviour: paths, parameters, paging, batching, request bodies, models.

mod common;

use apple_music::models::{LibraryEntry, RatingValue, TrackKind, TrackRef};
use apple_music::{CATALOG_SONGS_BATCH, LIBRARY_SONGS_BATCH};
use common::{client, FixtureServer, Reply, TestTokens};
use futures_util::TryStreamExt;
use serde_json::{json, Value};

fn ids(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("{}", 1000 + i)).collect()
}

fn body_json(s: &str) -> Value {
    serde_json::from_str(s).unwrap()
}

#[tokio::test]
async fn library_songs_follow_next_with_limit_100() {
    let server = FixtureServer::start().await;
    server
        .on(
            "GET",
            "/v1/me/library/songs",
            Reply::fixture("library_songs_page1.json"),
        )
        .on(
            "GET",
            "/v1/me/library/songs",
            Reply::fixture("library_songs_page2.json"),
        );
    let c = client(&server, TestTokens::new());

    let songs: Vec<_> = c.library_songs().try_collect().await.unwrap();

    assert_eq!(songs.len(), 3);
    assert_eq!(songs[0].id, "i.PkdJNdAIrQozOW");
    assert_eq!(songs[2].id, "i.xrXvO5vFv0VrNA");
    let seen = server.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].target, "/v1/me/library/songs?limit=100");
    assert_eq!(seen[1].target, "/v1/me/library/songs?offset=2&limit=100");
}

#[tokio::test]
async fn library_songs_models_keep_play_params_and_tolerate_unknown_fields() {
    let server = FixtureServer::start().await;
    server
        .on(
            "GET",
            "/v1/me/library/songs",
            Reply::fixture("library_songs_page1.json"),
        )
        .on(
            "GET",
            "/v1/me/library/songs",
            Reply::fixture("library_songs_page2.json"),
        );
    let c = client(&server, TestTokens::new());

    let songs: Vec<_> = c.library_songs().try_collect().await.unwrap();

    let first = songs[0].attributes.as_ref().unwrap();
    assert_eq!(first.name.as_deref(), Some("Efecto"));
    assert_eq!(first.duration_in_millis, Some(213_061));
    let play = first.play_params.as_ref().unwrap();
    assert_eq!(play.kind, "song");
    assert_eq!(play.is_library, Some(true));
    assert_eq!(play.catalog_id.as_deref(), Some("1622045954"));
    assert_eq!(songs[0].catalog_id(), Some("1622045954"));
    assert_eq!(songs[1].catalog_id(), None);
    assert_eq!(songs[2].catalog_id(), Some("907242702"));
    assert_eq!(
        first.artwork.as_ref().unwrap().url_max().unwrap(),
        "https://is5-ssl.mzstatic.com/image/thumb/Music112/v4/3e/04/eb/3e04ebf6-370f-f59d-ec84-2c2643db92f1/196626945068.jpg/1200x1200bb.jpg"
    );
}

#[tokio::test]
async fn library_song_catalog_id_falls_back_to_the_catalog_relationship() {
    let server = FixtureServer::start().await;
    server.on(
        "GET",
        "/v1/me/library/songs",
        Reply::fixture("library_songs_include_catalog.json"),
    );
    let c = client(&server, TestTokens::new());

    let songs = c
        .library_songs_by_ids(&["i.7PJNN4mfXlD68R".to_owned()], &["catalog"])
        .await
        .unwrap();

    assert_eq!(songs[0].catalog_id(), Some("1151618586"));
    assert_eq!(
        server.seen()[0].target,
        "/v1/me/library/songs?ids=i.7PJNN4mfXlD68R&include=catalog"
    );
}

#[tokio::test]
async fn library_songs_by_ids_batch_at_300() {
    let server = FixtureServer::start().await;
    server.on(
        "GET",
        "/v1/me/library/songs",
        Reply::fixture("library_songs_page2.json"),
    );
    let c = client(&server, TestTokens::new());

    c.library_songs_by_ids(&ids(LIBRARY_SONGS_BATCH * 2 + 1), &[])
        .await
        .unwrap();

    let sizes: Vec<usize> = server
        .seen()
        .iter()
        .map(|s| s.target.split("ids=").nth(1).unwrap().split(',').count())
        .collect();
    assert_eq!(sizes, vec![300, 300, 1]);
}

#[tokio::test]
async fn library_albums_and_playlists_decode() {
    let server = FixtureServer::start().await;
    server
        .on(
            "GET",
            "/v1/me/library/albums",
            Reply::fixture("library_albums.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists",
            Reply::fixture("library_playlists.json"),
        );
    let c = client(&server, TestTokens::new());

    let albums: Vec<_> = c.library_albums().try_collect().await.unwrap();
    assert_eq!(albums[0].id, "l.NWqWi1m");
    assert_eq!(albums[0].attributes.as_ref().unwrap().track_count, Some(1));

    let playlists: Vec<_> = c.library_playlists().try_collect().await.unwrap();
    assert_eq!(playlists.len(), 2);
    let editable = playlists[0].attributes.as_ref().unwrap();
    assert_eq!(editable.can_edit, Some(true));
    assert_eq!(editable.has_catalog, Some(false));
    assert_eq!(
        editable.description.as_ref().unwrap().standard.as_deref(),
        Some("My library playlist")
    );
    let subscribed = playlists[1].attributes.as_ref().unwrap();
    assert_eq!(subscribed.can_edit, Some(false));
    assert_eq!(subscribed.has_catalog, Some(true));
    // Keys the model does not name survive for the player.
    assert_eq!(
        subscribed
            .play_params
            .as_ref()
            .unwrap()
            .extra
            .get("globalId"),
        Some(&json!("pl.u-example"))
    );
}

#[tokio::test]
async fn playlist_tracks_use_the_tracks_path() {
    let server = FixtureServer::start().await;
    server.on(
        "GET",
        "/v1/me/library/playlists/p.ldvAAZ3C3Qmop9/tracks",
        Reply::fixture("library_playlist_tracks.json"),
    );
    let c = client(&server, TestTokens::new());

    let tracks: Vec<_> = c
        .library_playlist_tracks("p.ldvAAZ3C3Qmop9")
        .try_collect()
        .await
        .unwrap();

    assert_eq!(tracks[0].id, "a.1542568135");
    assert_eq!(
        server.seen()[0].target,
        "/v1/me/library/playlists/p.ldvAAZ3C3Qmop9/tracks?limit=100"
    );
}

#[tokio::test]
async fn recently_added_follows_next_and_keeps_unknown_types() {
    let server = FixtureServer::start().await;
    server
        .on(
            "GET",
            "/v1/me/library/recently-added",
            Reply::fixture("recently_added_page1.json"),
        )
        .on(
            "GET",
            "/v1/me/library/recently-added",
            Reply::fixture("recently_added_page2.json"),
        );
    let c = client(&server, TestTokens::new());

    let items: Vec<_> = c.recently_added().try_collect().await.unwrap();

    assert_eq!(items.len(), 3);
    assert!(matches!(&items[0], LibraryEntry::Album(a) if a.id == "l.NWqWi1m"));
    assert!(matches!(&items[1], LibraryEntry::Playlist(p) if p.id == "p.RB1AAkGsv74Zkl"));
    assert!(matches!(&items[2], LibraryEntry::Other(v) if v["id"] == "x.unknown"));
    let seen = server.seen();
    assert_eq!(seen[0].target, "/v1/me/library/recently-added?limit=25");
    assert_eq!(
        seen[1].target,
        "/v1/me/library/recently-added?offset=2&limit=25"
    );
}

#[tokio::test]
async fn recently_added_stops_after_four_pages() {
    let server = FixtureServer::start().await;
    // Every page names another one.
    for offset in [25, 50, 75, 100, 125] {
        server.on(
            "GET",
            "/v1/me/library/recently-added",
            Reply::json(json!({
                "next": format!("/v1/me/library/recently-added?offset={offset}"),
                "data": [{"id": format!("l.{offset}"), "type": "library-albums"}]
            })),
        );
    }
    let c = client(&server, TestTokens::new());

    let items: Vec<_> = c.recently_added().try_collect().await.unwrap();

    assert_eq!(items.len(), 4);
    assert_eq!(server.seen().len(), 4);
}

#[tokio::test]
async fn library_pages_are_read_by_offset_when_the_total_is_known() {
    let server = FixtureServer::start().await;
    let page = |ids: &[&str], next: Option<&str>| {
        let data: Vec<_> = ids
            .iter()
            .map(|id| json!({"id": id, "type": "library-songs"}))
            .collect();
        Reply::json(json!({"data": data, "next": next, "meta": {"total": 5}}))
    };
    server
        .on_target(
            "GET",
            "/v1/me/library/songs?limit=100&include=catalog",
            page(&["i.1", "i.2"], Some("/v1/me/library/songs?offset=2")),
        )
        .on_target(
            "GET",
            "/v1/me/library/songs?offset=2&limit=100&include=catalog",
            page(&["i.3", "i.4"], Some("/v1/me/library/songs?offset=4")),
        )
        .on_target(
            "GET",
            "/v1/me/library/songs?offset=4&limit=100&include=catalog",
            page(&["i.5"], None),
        );
    let c = client(&server, TestTokens::new());

    let pages: Vec<Vec<_>> = c
        .library_song_pages(&["catalog"], 4)
        .try_collect()
        .await
        .unwrap();

    let ids: Vec<Vec<&str>> = pages
        .iter()
        .map(|page| page.iter().map(|song| song.id.as_str()).collect())
        .collect();
    assert_eq!(ids, [vec!["i.1", "i.2"], vec!["i.3", "i.4"], vec!["i.5"]]);
    assert_eq!(server.seen().len(), 3);
}

#[tokio::test]
async fn library_pages_follow_next_without_a_total() {
    let server = FixtureServer::start().await;
    server
        .on(
            "GET",
            "/v1/me/library/songs",
            Reply::fixture("library_songs_page1.json"),
        )
        .on(
            "GET",
            "/v1/me/library/songs",
            Reply::fixture("library_songs_page2.json"),
        );
    let c = client(&server, TestTokens::new());

    let pages: Vec<Vec<_>> = c.library_song_pages(&[], 4).try_collect().await.unwrap();

    assert_eq!(pages.iter().map(Vec::len).collect::<Vec<_>>(), [2, 1]);
    assert_eq!(
        server.seen()[1].target,
        "/v1/me/library/songs?offset=2&limit=100"
    );
}

#[test]
fn static_tokens_do_not_print_their_secrets() {
    let tokens = apple_music::StaticTokens::new("developer-secret", Some("user-secret".into()));
    let printed = format!("{tokens:?}");
    assert!(!printed.contains("secret"), "{printed}");
}

#[tokio::test]
async fn root_folder_id_is_read_from_meta_and_children_are_listed() {
    let server = FixtureServer::start().await;
    server
        .on(
            "GET",
            "/v1/me/library/playlist-folders",
            Reply::fixture("playlist_folders_root.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlist-folders/p.playlistsroot/children",
            Reply::fixture("playlist_folder_children.json"),
        );
    let c = client(&server, TestTokens::new());

    let root = c.root_playlist_folder().await.unwrap().unwrap();
    assert_eq!(root.id, "p.playlistsroot");
    assert_eq!(
        server.seen()[0].target,
        "/v1/me/library/playlist-folders?filter[identity]=playlistsroot"
    );

    let children: Vec<_> = c
        .library_folder_children(&root.id)
        .try_collect()
        .await
        .unwrap();
    assert!(matches!(&children[0], LibraryEntry::PlaylistFolder(f)
        if f.attributes.as_ref().unwrap().name.as_deref() == Some("Chill")));
    assert!(matches!(&children[1], LibraryEntry::Playlist(_)));
}

#[tokio::test]
async fn storefront_is_looked_up_once_and_cached() {
    let server = FixtureServer::start().await;
    server
        .on(
            "GET",
            "/v1/me/storefront",
            Reply::fixture("storefront.json"),
        )
        .on(
            "GET",
            "/v1/catalog/it/songs",
            Reply::fixture("catalog_songs.json"),
        );
    let c = apple_music::AppleMusicClient::builder(TestTokens::new())
        .base_url(server.base_url.clone())
        .build();

    c.catalog_songs(&["617154366".to_owned()]).await.unwrap();
    c.catalog_songs(&["617154366".to_owned()]).await.unwrap();

    assert_eq!(server.seen_for("GET", "/v1/me/storefront").len(), 1);
    assert_eq!(server.seen_for("GET", "/v1/catalog/it/songs").len(), 2);
}

#[tokio::test]
async fn catalog_songs_batch_at_300_and_concatenate() {
    let server = FixtureServer::start().await;
    server.on(
        "GET",
        "/v1/catalog/it/songs",
        Reply::fixture("catalog_songs.json"),
    );
    let c = client(&server, TestTokens::new());

    let songs = c
        .catalog_songs(&ids(CATALOG_SONGS_BATCH + 1))
        .await
        .unwrap();

    let seen = server.seen();
    assert_eq!(seen.len(), 2);
    let count = |t: &str| t.split("ids=").nth(1).unwrap().split(',').count();
    assert_eq!(count(&seen[0].target), 300);
    assert_eq!(count(&seen[1].target), 1);
    // Both requests answer with the same recorded page, so the results are concatenated in order.
    assert_eq!(songs.len() % 2, 0);
    let half = songs.len() / 2;
    assert_eq!(songs[..half], songs[half..]);
}

#[tokio::test]
async fn catalog_song_models_decode_from_a_recorded_response() {
    let server = FixtureServer::start().await;
    server.on(
        "GET",
        "/v1/catalog/it/songs",
        Reply::fixture("catalog_songs.json"),
    );
    let c = client(&server, TestTokens::new());

    let songs = c.catalog_songs(&["617154366".to_owned()]).await.unwrap();

    let first = &songs[0];
    assert_eq!(first.id, "617154366");
    assert_eq!(first.kind, "songs");
    let attrs = first.attributes.as_ref().unwrap();
    assert_eq!(attrs.name.as_deref(), Some("Get Lucky"));
    assert_eq!(attrs.isrc.as_deref(), Some("USQX91300108"));
    assert_eq!(attrs.play_params.as_ref().unwrap().id, "617154366");
    assert_eq!(attrs.previews.len(), 1);
}

#[tokio::test]
async fn search_songs_sends_term_types_and_paging() {
    let server = FixtureServer::start().await;
    server.on(
        "GET",
        "/v1/catalog/it/search",
        Reply::fixture("catalog_search_songs.json"),
    );
    let c = client(&server, TestTokens::new());

    let result = c.search_songs("daft punk get lucky", 3, 0).await.unwrap();

    assert_eq!(
        server.seen()[0].target,
        "/v1/catalog/it/search?term=daft+punk+get+lucky&types=songs&limit=3&offset=0"
    );
    assert!(!result.songs.is_empty());
    assert_eq!(result.songs[0].kind, "songs");
}

#[test]
fn artwork_template_is_filled() {
    let artwork: apple_music::models::Artwork = serde_json::from_value(json!({
        "width": 3000,
        "height": 2000,
        "url": "https://example.test/image/{w}x{h}bb.jpg",
        "bgColor": "202020"
    }))
    .unwrap();

    assert_eq!(
        artwork.url_for(300, 300),
        "https://example.test/image/300x300bb.jpg"
    );
    assert_eq!(
        artwork.url_max().unwrap(),
        "https://example.test/image/3000x2000bb.jpg"
    );
    assert_eq!(artwork.bg_color.as_deref(), Some("202020"));
}

#[tokio::test]
async fn ratings_get_batches_and_decodes() {
    let server = FixtureServer::start().await;
    server
        .on(
            "GET",
            "/v1/me/ratings/songs",
            Reply::fixture("ratings_songs.json"),
        )
        .on(
            "GET",
            "/v1/me/ratings/library-songs",
            Reply::fixture("ratings_library_songs.json"),
        );
    let c = client(&server, TestTokens::new());

    let catalog = c
        .catalog_song_ratings(&["907242702".into(), "1151618586".into()])
        .await
        .unwrap();
    assert_eq!(catalog[0].attributes.as_ref().unwrap().value, Some(1));
    assert_eq!(catalog[1].attributes.as_ref().unwrap().value, Some(-1));
    assert_eq!(
        server.seen()[0].target,
        "/v1/me/ratings/songs?ids=907242702,1151618586"
    );

    let library = c.library_song_ratings(&ids(250)).await.unwrap();
    assert_eq!(library.len(), 6);
    let library_requests = server.seen_for("GET", "/v1/me/ratings/library-songs");
    assert_eq!(library_requests.len(), 3);
}

#[tokio::test]
async fn set_rating_sends_put_with_the_documented_body() {
    let server = FixtureServer::start().await;
    server
        .on(
            "PUT",
            "/v1/me/ratings/library-songs/i.7PJNN4mfXlD68R",
            Reply::fixture("rating_put_library_song.json"),
        )
        .on(
            "PUT",
            "/v1/me/ratings/songs/907242702",
            Reply::fixture("rating_put_catalog_song.json"),
        );
    let c = client(&server, TestTokens::new());

    let r = c
        .set_library_song_rating("i.7PJNN4mfXlD68R", RatingValue::Like)
        .await
        .unwrap();
    assert_eq!(r.attributes.unwrap().value, Some(1));
    let r = c
        .set_catalog_song_rating("907242702", RatingValue::Dislike)
        .await
        .unwrap();
    assert_eq!(r.attributes.unwrap().value, Some(-1));

    let seen = server.seen();
    assert_eq!(seen[0].method, "PUT");
    assert_eq!(
        body_json(&seen[0].body),
        json!({"type": "rating", "attributes": {"value": 1}})
    );
    assert_eq!(
        body_json(&seen[1].body),
        json!({"type": "rating", "attributes": {"value": -1}})
    );
    assert_eq!(seen[0].user_token.as_deref(), Some("user-0"));
}

#[tokio::test]
async fn delete_rating_sends_delete_without_body() {
    let server = FixtureServer::start().await;
    server
        .on(
            "DELETE",
            "/v1/me/ratings/library-songs/i.7PJNN4mfXlD68R",
            Reply::status(204),
        )
        .on(
            "DELETE",
            "/v1/me/ratings/songs/907242702",
            Reply::status(204),
        );
    let c = client(&server, TestTokens::new());

    c.delete_library_song_rating("i.7PJNN4mfXlD68R")
        .await
        .unwrap();
    c.delete_catalog_song_rating("907242702").await.unwrap();

    let seen = server.seen();
    assert_eq!(seen.len(), 2);
    assert!(seen
        .iter()
        .all(|s| s.method == "DELETE" && s.body.is_empty()));
}

#[tokio::test]
async fn add_to_library_posts_typed_ids_and_accepts_202() {
    let server = FixtureServer::start().await;
    server.on("POST", "/v1/me/library", Reply::status(202));
    let c = client(&server, TestTokens::new());

    c.add_songs_to_library(&["900".into(), "901".into(), "900".into()])
        .await
        .unwrap();
    c.add_albums_to_library(&["1577502911".into()])
        .await
        .unwrap();

    let seen = server.seen();
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].target, "/v1/me/library?ids[songs]=900,901");
    assert_eq!(seen[1].target, "/v1/me/library?ids[albums]=1577502911");
    assert!(seen[0].body.is_empty());
}

#[tokio::test]
async fn create_playlist_posts_attributes_and_tracks() {
    let server = FixtureServer::start().await;
    server.on(
        "POST",
        "/v1/me/library/playlists",
        Reply::fixture_status(201, "playlist_created.json"),
    );
    let c = client(&server, TestTokens::new());

    let playlist = c
        .create_library_playlist(
            "New Playlist",
            Some("My library playlist"),
            &[TrackRef::song("900"), TrackRef::library_song("i.abc")],
        )
        .await
        .unwrap();

    assert_eq!(playlist.id, "p.RB1AAkGsv74Zkl");
    assert_eq!(playlist.attributes.as_ref().unwrap().can_edit, Some(true));
    assert_eq!(
        body_json(&server.seen()[0].body),
        json!({
            "attributes": {"name": "New Playlist", "description": "My library playlist"},
            "relationships": {"tracks": {"data": [
                {"id": "900", "type": "songs"},
                {"id": "i.abc", "type": "library-songs"}
            ]}}
        })
    );
}

#[tokio::test]
async fn create_playlist_without_tracks_omits_relationships() {
    let server = FixtureServer::start().await;
    server.on(
        "POST",
        "/v1/me/library/playlists",
        Reply::fixture_status(201, "playlist_created.json"),
    );
    let c = client(&server, TestTokens::new());

    c.create_library_playlist("Empty", None, &[]).await.unwrap();

    assert_eq!(
        body_json(&server.seen()[0].body),
        json!({"attributes": {"name": "Empty"}})
    );
}

#[tokio::test]
async fn add_tracks_posts_data_array_and_accepts_204() {
    let server = FixtureServer::start().await;
    server.on(
        "POST",
        "/v1/me/library/playlists/p.RB1AARBIv74Zkl/tracks",
        Reply::status(204),
    );
    let c = client(&server, TestTokens::new());

    c.add_tracks_to_library_playlist(
        "p.RB1AARBIv74Zkl",
        &[
            TrackRef {
                id: "i.xyz".into(),
                kind: TrackKind::LibrarySong,
            },
            TrackRef::song("901"),
        ],
    )
    .await
    .unwrap();

    assert_eq!(
        body_json(&server.seen()[0].body),
        json!({"data": [
            {"id": "i.xyz", "type": "library-songs"},
            {"id": "901", "type": "songs"}
        ]})
    );
}

#[tokio::test]
async fn recommendations_follow_next_with_limit_and_keep_stations_in_the_models() {
    let server = FixtureServer::start().await;
    server
        .on(
            "GET",
            "/v1/me/recommendations",
            Reply::fixture("recommendations_page1.json"),
        )
        .on(
            "GET",
            "/v1/me/recommendations",
            Reply::fixture("recommendations_page2.json"),
        );
    let c = client(&server, TestTokens::new());

    let groups = c.recommendations().await.unwrap();

    assert_eq!(groups.len(), 3);
    let first = groups[0].attributes.as_ref().unwrap();
    assert_eq!(
        first.title.as_ref().unwrap().string_for_display.as_deref(),
        Some("Made for Someone")
    );
    assert_eq!(first.kind.as_deref(), Some("music-recommendations"));
    let contents = &groups[0]
        .relationships
        .as_ref()
        .unwrap()
        .contents
        .as_ref()
        .unwrap()
        .data;
    assert_eq!(
        contents.iter().map(|i| i.kind.as_str()).collect::<Vec<_>>(),
        ["playlists", "stations"]
    );
    let mix = contents[0].attributes.as_ref().unwrap();
    assert_eq!(mix.curator_name.as_deref(), Some("Curator"));
    assert_eq!(
        mix.artwork.as_ref().unwrap().url,
        "https://art.invalid/mix1/{w}x{h}.jpg"
    );
    let seen = server.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].target, "/v1/me/recommendations?limit=10");
    assert_eq!(seen[1].target, "/v1/me/recommendations?offset=10&limit=10");
}

#[tokio::test]
async fn recommendations_stop_after_three_pages() {
    let server = FixtureServer::start().await;
    // Every page names another one.
    for offset in [10, 20, 30, 40] {
        server.on(
            "GET",
            "/v1/me/recommendations",
            Reply {
                status: 200,
                body: format!(
                    r#"{{"data": [{{"id": "r{offset}", "type": "personal-recommendation"}}],
                        "next": "/v1/me/recommendations?offset={offset}"}}"#
                ),
                headers: Vec::new(),
            },
        );
    }
    let c = client(&server, TestTokens::new());

    let groups = c.recommendations().await.unwrap();

    assert_eq!(groups.len(), 3);
    assert_eq!(server.seen().len(), 3);
}

#[tokio::test]
async fn catalog_playlist_and_album_tracks_follow_next_and_skip_music_videos() {
    let server = FixtureServer::start().await;
    server
        .on(
            "GET",
            "/v1/catalog/it/playlists/pl.pm-mix1/tracks",
            Reply::fixture("catalog_collection_tracks_page1.json"),
        )
        .on(
            "GET",
            "/v1/catalog/it/playlists/pl.pm-mix1/tracks",
            Reply::fixture("catalog_collection_tracks_page2.json"),
        )
        .on(
            "GET",
            "/v1/catalog/it/albums/alb1/tracks",
            Reply::fixture("catalog_collection_tracks_page2.json"),
        );
    let c = client(&server, TestTokens::new());

    let playlist = c.catalog_playlist_tracks("pl.pm-mix1").await.unwrap();
    let album = c.catalog_album_tracks("alb1").await.unwrap();

    assert_eq!(
        playlist.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        ["t1", "t2"]
    );
    assert_eq!(album.len(), 1);
    let seen = server.seen();
    assert_eq!(seen[0].target, "/v1/catalog/it/playlists/pl.pm-mix1/tracks");
    assert_eq!(
        seen[1].target,
        "/v1/catalog/it/playlists/pl.pm-mix1/tracks?offset=2"
    );
    assert_eq!(
        seen[0].user_token.as_deref().map(str::is_empty),
        Some(false)
    );
}

#[tokio::test]
async fn library_playlist_resolves_to_its_catalog_playlist() {
    let server = FixtureServer::start().await;
    server
        .on(
            "GET",
            "/v1/me/library/playlists/p.lib1",
            Reply::fixture("library_playlist_with_catalog.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists/p.lib2",
            Reply::fixture("library_playlist_without_catalog.json"),
        );
    let c = client(&server, TestTokens::new());

    let with = c
        .catalog_playlist_id_of_library_playlist("p.lib1")
        .await
        .unwrap();
    let without = c
        .catalog_playlist_id_of_library_playlist("p.lib2")
        .await
        .unwrap();

    assert_eq!(with.as_deref(), Some("pl.cat1"));
    assert_eq!(without, None);
    assert_eq!(
        server.seen()[0].target,
        "/v1/me/library/playlists/p.lib1?include=catalog"
    );
}
