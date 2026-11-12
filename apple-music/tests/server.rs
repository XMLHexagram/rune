//! The Subsonic-shaped server against the fixture server, called the way the hub calls
//! it: through `Arc<dyn SubsonicApi>`.

mod common;

use std::collections::HashSet;
use std::sync::Arc;

use apple_music::AppleMusicServer;
use common::{fast_retry, FixtureServer, Reply, Seen};
use serde_json::{json, Value};
use subsonic::{
    RuneEdit, RuneLoginCompletion, RuneLoginMethod, RuneLoginStart, RunePlayback, RunePlaybackMode,
    RuneReason, SearchLimits, StreamError, SubsonicApi, SubsonicError, SubsonicServerError,
    SubsonicSong,
};

const USER_TOKEN: &str = "music-user-token";
const DEVELOPER_TOKEN: &str = "developer-token";
const LIBRARY_PAGE_1: &str = "/v1/me/library/songs?limit=100&include=albums,artists";
const LIBRARY_PAGE_2: &str = "/v1/me/library/songs?offset=2&limit=100&include=albums,artists";

fn server_for(fixtures: &FixtureServer) -> Arc<dyn SubsonicApi> {
    let base_url = fixtures.base_url.clone();
    Arc::new(AppleMusicServer::with_client(|builder| {
        builder
            .base_url(base_url)
            .storefront("it")
            .retry_policy(fast_retry())
    }))
}

fn platform(token: Option<&str>, developer_token: Option<&str>) -> RuneLoginCompletion {
    RuneLoginCompletion {
        method: RuneLoginMethod::Platform,
        state: None,
        code: None,
        token: token.map(Into::into),
        developer_token: developer_token.map(Into::into),
    }
}

async fn signed_in(fixtures: &FixtureServer) -> Arc<dyn SubsonicApi> {
    let server = server_for(fixtures);
    server
        .complete_rune_login(&platform(Some(USER_TOKEN), Some(DEVELOPER_TOKEN)))
        .await
        .unwrap();
    server
}

fn fixture(name: &str) -> Reply {
    Reply::fixture(&format!("server/{name}"))
}

/// A fixture whose artwork URLs point at the fixture server.
fn fixture_with_artwork(fixtures: &FixtureServer, name: &str) -> Reply {
    let mut reply = fixture(name);
    reply.body = reply
        .body
        .replace("http://artwork.invalid", &fixtures.base_url);
    reply
}

fn library_song(id: &str, name: &str) -> Value {
    json!({
        "id": id,
        "type": "library-songs",
        "href": format!("/v1/me/library/songs/{id}"),
        "attributes": {
            "name": name,
            "albumName": "Long Album",
            "artistName": "Artist Seven",
            "durationInMillis": 120000,
            "genreNames": ["Jazz"],
            "playParams": {"id": id, "kind": "song", "isLibrary": true}
        }
    })
}

fn body_json(body: &str) -> Value {
    serde_json::from_str(body).unwrap()
}

fn ids(songs: &[SubsonicSong]) -> Vec<&str> {
    songs.iter().map(|song| song.id.as_str()).collect()
}

fn id_count(request: &Seen) -> usize {
    request
        .target
        .split("ids=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap()
        .split(',')
        .count()
}

/// Reads of library song pages, not lookups by id.
fn library_page_reads(fixtures: &FixtureServer) -> usize {
    fixtures
        .seen_for("GET", "/v1/me/library/songs")
        .iter()
        .filter(|request| !request.target.contains("ids="))
        .count()
}

fn assert_credentials_expired(error: &SubsonicError) {
    assert!(
        matches!(
            error,
            SubsonicError::Server {
                kind: SubsonicServerError::NotAuthorized,
                rune_reason: Some(RuneReason::CredentialsExpired),
                ..
            }
        ),
        "{error:?}"
    );
}

fn assert_not_found<T: std::fmt::Debug>(result: Result<T, SubsonicError>) {
    assert!(
        matches!(
            result,
            Err(SubsonicError::Server {
                kind: SubsonicServerError::NotFound,
                ..
            })
        ),
        "{result:?}"
    );
}

fn assert_refused<T: std::fmt::Debug>(result: Result<T, SubsonicError>) {
    assert!(
        matches!(
            result,
            Err(SubsonicError::Server {
                kind: SubsonicServerError::Generic,
                rune_reason: None,
                ..
            })
        ),
        "{result:?}"
    );
}

fn assert_unsupported(result: Result<(), SubsonicError>, expected: &str) {
    match result {
        Err(SubsonicError::Unsupported { endpoint }) => assert_eq!(endpoint, expected),
        other => panic!("expected {expected} to be unsupported, got {other:?}"),
    }
}

/// The library, three joining playlists (one empty), Favorites and a subscribed one.
fn serve_library(fixtures: &FixtureServer) {
    fixtures
        .on_target("GET", LIBRARY_PAGE_1, fixture("library_songs_page1.json"))
        .on_target("GET", LIBRARY_PAGE_2, fixture("library_songs_page2.json"))
        .on(
            "GET",
            "/v1/me/library/playlists",
            fixture("library_playlists.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists/p.mine1/tracks",
            fixture("playlist_mine_tracks.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists/p.favorites/tracks",
            fixture("playlist_favorites_tracks.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists/p.editorial/tracks",
            fixture("playlist_editorial_tracks.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists/p.empty/tracks",
            Reply::fixture_status(404, "error_not_found.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists/p.public/tracks",
            fixture("playlist_public_tracks.json"),
        );
}

const UNION: [&str; 6] = [
    "i.lib1",
    "i.lib2",
    "i.lib3",
    "i.pl1",
    "catalog:song:3001",
    "catalog:song:4001",
];

fn songs_only(count: u32) -> SearchLimits {
    SearchLimits {
        songs: count,
        ..SearchLimits::default()
    }
}

async fn all_songs(server: &dyn SubsonicApi, page: u32) -> Vec<SubsonicSong> {
    let mut songs = Vec::new();
    loop {
        let offset = u32::try_from(songs.len()).unwrap();
        let found = server
            .search_with_song_offset("", songs_only(page), offset)
            .await
            .unwrap();
        assert!(found.artists.is_empty() && found.albums.is_empty());
        if found.songs.is_empty() {
            return songs;
        }
        songs.extend(found.songs);
    }
}

#[tokio::test]
async fn probe_extensions_and_descriptor_answer_without_tokens() {
    let fixtures = FixtureServer::start().await;
    let server = server_for(&fixtures);

    let probe = server.probe().await.unwrap();
    assert_eq!(probe.api_version, "1.16.1");
    assert!(probe.open_subsonic);
    assert_eq!(probe.server_type.as_deref(), Some("rune-apple-music"));
    assert_eq!(probe.extensions, vec![("runeService".to_owned(), vec![1])]);
    assert!(probe.supports_rune_service());
    assert!(probe.roles_known);
    assert!(probe.roles.stream_role);
    assert!(!probe.roles.jukebox_role);
    assert_eq!(
        server.extensions().await.unwrap(),
        vec![("runeService".to_owned(), vec![1])]
    );

    let descriptor = server.rune_service().await.unwrap();
    assert_eq!(descriptor.id, "appleMusic");
    assert_eq!(descriptor.name, "Apple Music");
    let attribution = descriptor.attribution.as_ref().unwrap();
    assert_eq!(attribution.text, "Apple Music");
    assert_eq!(attribution.link.as_deref(), Some("https://music.apple.com"));
    assert_eq!(descriptor.login.methods, vec![RuneLoginMethod::Platform]);
    assert_eq!(descriptor.hosts, vec!["appleMusic".to_owned()]);
    for endpoint in [
        "getLyrics",
        "getLyricsBySongId",
        "deletePlaylist",
        "jukeboxControl",
        "stream",
    ] {
        assert!(descriptor.is_unsupported(endpoint, &[]), "{endpoint}");
    }
    assert!(descriptor.is_unsupported("updatePlaylist", &["playlistId", "name"]));
    assert!(descriptor.is_unsupported("updatePlaylist", &["playlistId", "songIndexToRemove"]));
    assert!(!descriptor.is_unsupported("updatePlaylist", &["playlistId", "songIdToAdd"]));
    assert!(descriptor.is_unsupported("createPlaylist", &["playlistId", "songId"]));
    assert!(!descriptor.is_unsupported("createPlaylist", &["name", "songId"]));
    assert_eq!(descriptor.unsupported.len(), 8);

    assert_eq!(
        server
            .start_rune_login(RuneLoginMethod::Platform)
            .await
            .unwrap(),
        RuneLoginStart::Platform
    );
    for method in [
        RuneLoginMethod::Password,
        RuneLoginMethod::Browser,
        RuneLoginMethod::DeviceCode,
    ] {
        assert!(matches!(
            server.start_rune_login(method).await,
            Err(SubsonicError::Unsupported {
                endpoint: "startRuneLogin"
            })
        ));
    }
    assert!(fixtures.seen().is_empty());
}

#[tokio::test]
async fn every_call_needing_apple_asks_for_sign_in_first() {
    let fixtures = FixtureServer::start().await;
    serve_library(&fixtures);
    let server = server_for(&fixtures);
    let none = &[][..];

    let errors = [
        ("getArtists", server.artists().await.err()),
        ("getArtist", server.artist("r.art1").await.err()),
        (
            "getArtist catalog",
            server.artist("catalog:artist:1").await.err(),
        ),
        ("getAlbum", server.album("l.alb1").await.err()),
        ("getSong", server.song("i.lib1").await.err()),
        ("getSong catalog", server.song("catalog:song:1").await.err()),
        (
            "getSong unknown kind",
            server.song("catalog:album:1").await.err(),
        ),
        ("getAlbumList2", server.album_list(10, 0).await.err()),
        ("getStarred2", server.starred_songs().await.err()),
        (
            "search3 all",
            server
                .search_with_song_offset("", songs_only(10), 0)
                .await
                .err(),
        ),
        (
            "search3 nothing",
            server
                .search_with_song_offset("", SearchLimits::default(), 0)
                .await
                .err(),
        ),
        ("search3", server.search("dawn", songs_only(10)).await.err()),
        ("getScanStatus", server.scan_status().await.err()),
        ("getPlaylists", server.playlists().await.err()),
        ("getPlaylist", server.playlist("p.mine1").await.err()),
        (
            "createPlaylist",
            server.create_playlist("Mix", none).await.err(),
        ),
        (
            "updatePlaylist add",
            server
                .update_playlist("p.mine1", None, &["i.lib1"], &[])
                .await
                .err(),
        ),
        (
            "updatePlaylist add nothing",
            server
                .update_playlist("p.mine1", None, none, &[])
                .await
                .err(),
        ),
        (
            "updatePlaylist rename",
            server
                .update_playlist("p.mine1", Some("New"), none, &[])
                .await
                .err(),
        ),
        (
            "updatePlaylist remove",
            server
                .update_playlist("p.mine1", None, none, &[0])
                .await
                .err(),
        ),
        ("star", server.star(&["i.lib1"]).await.err()),
        ("star nothing", server.star(none).await.err()),
        ("unstar", server.unstar(&["catalog:song:1"]).await.err()),
    ];
    for (call, error) in errors {
        let error = error.unwrap_or_else(|| panic!("{call} answered without a sign-in"));
        assert!(error.is_rune_credentials_expired(), "{call}: {error:?}");
        assert_credentials_expired(&error);
    }
    assert!(fixtures.seen().is_empty());
    // What needs no Apple token still answers.
    server.scrobble("i.lib1", None, false).await.unwrap();
    assert_not_found(server.cover_art("art:0000000000000000", None).await);
}

#[tokio::test]
async fn rejected_tokens_read_as_an_expired_sign_in() {
    let fixtures = FixtureServer::start().await;
    fixtures
        .on(
            "GET",
            "/v1/me/library/artists",
            Reply::fixture_status(401, "error_unauthorized.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists",
            Reply::fixture_status(403, "error_forbidden.json"),
        )
        .on(
            "GET",
            "/v1/me/storefront",
            Reply::fixture_status(401, "error_unauthorized.json"),
        );
    let server = signed_in(&fixtures).await;

    assert_credentials_expired(&server.artists().await.unwrap_err());
    // The client retried with a refreshed user token, then developer token.
    assert_eq!(fixtures.seen_for("GET", "/v1/me/library/artists").len(), 3);
    assert!(!fixtures.seen_for("GET", "/v1/me/storefront").is_empty());
    assert_credentials_expired(&server.playlists().await.unwrap_err());
}

#[tokio::test]
async fn refused_requests_with_working_tokens_are_server_errors() {
    let fixtures = FixtureServer::start().await;
    fixtures
        .on(
            "PUT",
            "/v1/me/ratings/library-songs/i.lib1",
            Reply::fixture_status(401, "error_unauthorized.json"),
        )
        .on(
            "DELETE",
            "/v1/me/ratings/songs/3001",
            Reply::fixture_status(403, "error_forbidden.json"),
        )
        .on(
            "GET",
            "/v1/me/storefront",
            Reply::fixture("storefront.json"),
        );
    let server = signed_in(&fixtures).await;

    let refused = server.star(&["i.lib1"]).await;
    assert!(!refused.as_ref().unwrap_err().is_rune_credentials_expired());
    assert_refused(refused);
    assert_refused(server.unstar(&["catalog:song:3001"]).await);
    assert_eq!(fixtures.seen_for("GET", "/v1/me/storefront").len(), 2);
}

#[tokio::test]
async fn sign_in_hands_both_tokens_to_library_calls() {
    let fixtures = FixtureServer::start().await;
    fixtures.on(
        "GET",
        "/v1/me/library/artists",
        fixture("library_artists_page2.json"),
    );
    let server = server_for(&fixtures);

    for incomplete in [
        platform(Some(USER_TOKEN), None),
        platform(Some(""), Some(DEVELOPER_TOKEN)),
    ] {
        assert!(matches!(
            server.complete_rune_login(&incomplete).await,
            Err(SubsonicError::Server {
                kind: SubsonicServerError::MissingParameter,
                ..
            })
        ));
    }
    let mut password = platform(Some(USER_TOKEN), Some(DEVELOPER_TOKEN));
    password.method = RuneLoginMethod::Password;
    assert_unsupported(
        server.complete_rune_login(&password).await,
        "completeRuneLogin",
    );
    assert_credentials_expired(&server.artists().await.unwrap_err());

    server
        .complete_rune_login(&platform(Some(USER_TOKEN), Some(DEVELOPER_TOKEN)))
        .await
        .unwrap();
    server.artists().await.unwrap();

    let seen = fixtures.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].authorization.as_deref(),
        Some("Bearer developer-token")
    );
    assert_eq!(seen[0].user_token.as_deref(), Some(USER_TOKEN));
}

#[tokio::test]
async fn only_new_tokens_start_over() {
    let fixtures = FixtureServer::start().await;
    fixtures
        .on_target("GET", LIBRARY_PAGE_1, fixture("library_songs_page2.json"))
        .on(
            "GET",
            "/v1/me/library/playlists",
            Reply::json(json!({"data": []})),
        )
        .on(
            "POST",
            "/v1/me/library/playlists",
            Reply::fixture_status(201, "server/playlist_created.json"),
        );
    let server = signed_in(&fixtures).await;

    assert_eq!(server.scan_status().await.unwrap().count, Some(1));
    server.create_playlist("Mix", &[]).await.unwrap();
    let listed = |playlists: Vec<subsonic::SubsonicPlaylist>| {
        playlists
            .into_iter()
            .map(|playlist| playlist.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(listed(server.playlists().await.unwrap()), ["p.new1"]);

    server
        .complete_rune_login(&platform(Some(USER_TOKEN), Some(DEVELOPER_TOKEN)))
        .await
        .unwrap();
    server.scan_status().await.unwrap();
    assert_eq!(library_page_reads(&fixtures), 1);
    assert_eq!(listed(server.playlists().await.unwrap()), ["p.new1"]);

    server
        .complete_rune_login(&platform(Some("another-user-token"), Some(DEVELOPER_TOKEN)))
        .await
        .unwrap();
    server.scan_status().await.unwrap();
    assert_eq!(library_page_reads(&fixtures), 2);
    assert!(listed(server.playlists().await.unwrap()).is_empty());
    let last = fixtures.seen().pop().unwrap();
    assert_eq!(last.user_token.as_deref(), Some("another-user-token"));
}

#[tokio::test]
async fn library_and_catalog_songs_map_with_hosted_playback() {
    let fixtures = FixtureServer::start().await;
    fixtures
        .on("GET", "/v1/me/library/songs", fixture("library_song.json"))
        .on(
            "GET",
            "/v1/me/ratings/library-songs",
            fixture("ratings_library.json"),
        )
        .on(
            "GET",
            "/v1/catalog/it/songs",
            Reply::fixture("catalog_songs.json"),
        )
        .on(
            "GET",
            "/v1/me/ratings/songs",
            Reply::fixture("ratings_songs.json"),
        );
    let server = signed_in(&fixtures).await;

    let song = server.song("i.lib1").await.unwrap();
    assert_eq!(song.id, "i.lib1");
    assert_eq!(song.title, "First Light");
    assert_eq!(song.album.as_deref(), Some("Dawn Album"));
    assert_eq!(song.artist.as_deref(), Some("Artist One"));
    assert_eq!(song.album_id.as_deref(), Some("l.alb1"));
    assert_eq!(song.artist_id.as_deref(), Some("r.art1"));
    assert_eq!(song.track, Some(1));
    assert_eq!(song.disc, Some(1));
    assert_eq!(song.year, Some(2020));
    assert_eq!(song.genre.as_deref(), Some("Ambient"));
    assert_eq!(song.duration, Some(246));
    assert!(song.cover_art.as_deref().unwrap().starts_with("art:"));
    // i.lib1 is absent from the ratings answer: read, and unrated.
    assert_eq!(song.starred, None);
    assert_eq!(
        song.rune_playback,
        Some(RunePlayback {
            mode: RunePlaybackMode::Hosted,
            host: Some("appleMusic".into()),
            key: Some("i.lib1".into()),
            reason: None,
        })
    );
    assert_eq!(
        fixtures.seen_for("GET", "/v1/me/library/songs")[0].target,
        "/v1/me/library/songs?ids=i.lib1&include=albums,artists"
    );
    assert_eq!(
        fixtures.seen_for("GET", "/v1/me/ratings/library-songs")[0].target,
        "/v1/me/ratings/library-songs?ids=i.lib1"
    );

    let catalog = server.song("catalog:song:617154366").await.unwrap();
    assert_eq!(catalog.id, "catalog:song:617154366");
    assert_eq!(catalog.title, "Get Lucky");
    assert_eq!(catalog.track, Some(8));
    assert_eq!(catalog.year, Some(2013));
    assert_eq!(catalog.duration, Some(370));
    assert_eq!(catalog.album_id, None);
    let playback = catalog.rune_playback.unwrap();
    assert_eq!(playback.mode, RunePlaybackMode::Hosted);
    assert_eq!(playback.host.as_deref(), Some("appleMusic"));
    assert_eq!(playback.key.as_deref(), Some("catalog:617154366"));
    let lookup = &fixtures.seen_for("GET", "/v1/catalog/it/songs")[0];
    assert_eq!(lookup.target, "/v1/catalog/it/songs?ids=617154366");
    // The catalog call needs only the developer token.
    assert_eq!(lookup.user_token, None);

    assert_not_found(server.song("catalog:album:1").await);
}

#[tokio::test]
async fn songs_the_union_knows_need_no_lookup() {
    let fixtures = FixtureServer::start().await;
    serve_library(&fixtures);
    fixtures
        .on(
            "GET",
            "/v1/me/ratings/songs",
            fixture("ratings_catalog.json"),
        )
        .on(
            "GET",
            "/v1/catalog/it/songs",
            Reply::fixture_status(503, "error_server.json"),
        );
    let server = signed_in(&fixtures).await;
    server.scan_status().await.unwrap();

    let loved = server.song("catalog:song:3001").await.unwrap();
    assert_eq!(loved.title, "Loved Elsewhere");
    assert!(loved.starred.is_some());
    let library = server.song("i.lib2").await.unwrap();
    assert_eq!(library.album_id.as_deref(), Some("l.alb1"));

    assert!(fixtures.seen_for("GET", "/v1/catalog/it/songs").is_empty());
    assert!(fixtures
        .seen_for("GET", "/v1/me/library/songs")
        .iter()
        .all(|request| !request.target.contains("ids=i.lib2")));
    // The rating is still read afresh.
    assert_eq!(fixtures.seen_for("GET", "/v1/me/ratings/songs").len(), 1);
}

#[tokio::test]
async fn all_songs_join_library_and_own_playlists_once() {
    let fixtures = FixtureServer::start().await;
    serve_library(&fixtures);
    let server = signed_in(&fixtures).await;

    let first = all_songs(server.as_ref(), 4).await;
    assert_eq!(ids(&first), UNION);
    let status = server.scan_status().await.unwrap();
    assert!(!status.scanning);
    assert_eq!(status.count, Some(6));

    // Built once for every page and the count; the second page read by its offset.
    assert_eq!(library_page_reads(&fixtures), 2);
    let pages: Vec<String> = fixtures
        .seen_for("GET", "/v1/me/library/songs")
        .into_iter()
        .map(|request| request.target)
        .filter(|target| !target.contains("ids="))
        .collect();
    assert_eq!(pages, [LIBRARY_PAGE_1, LIBRARY_PAGE_2]);
    assert!(fixtures
        .seen_for("GET", "/v1/me/library/playlists/p.editorial/tracks")
        .is_empty());
    // A playlist track outside the library plays and rates as a catalog song.
    let loved = &first[4];
    assert_eq!(loved.title, "Loved Elsewhere");
    assert_eq!(
        loved.rune_playback.as_ref().unwrap().key.as_deref(),
        Some("catalog:3001")
    );
    // A library song keeps its relationships when a playlist lists it too.
    assert_eq!(first[1].album_id.as_deref(), Some("l.alb1"));
    assert_eq!(
        first[3].rune_playback.as_ref().unwrap().key.as_deref(),
        Some("i.pl1")
    );

    let page = server
        .search_with_song_offset("", songs_only(2), 5)
        .await
        .unwrap();
    assert_eq!(ids(&page.songs), ["catalog:song:4001"]);
    let past_the_end = server
        .search_with_song_offset("", songs_only(10), 100)
        .await
        .unwrap();
    assert!(past_the_end.songs.is_empty());
    let none = server
        .search_with_song_offset("", SearchLimits::default(), 0)
        .await
        .unwrap();
    assert!(none.songs.is_empty());
    assert_eq!(library_page_reads(&fixtures), 2);
}

#[tokio::test]
async fn catalog_copies_of_library_songs_without_catalog_ids_are_dropped() {
    let fixtures = FixtureServer::start().await;
    fixtures
        .on_target("GET", LIBRARY_PAGE_1, fixture("dedup_library_songs.json"))
        .on_target(
            "GET",
            "/v1/me/library/songs?ids=i.up1&include=catalog",
            fixture("dedup_library_catalog.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists",
            fixture("library_playlist_favorites.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists/p.favorites/tracks",
            fixture("dedup_favorites_tracks.json"),
        );
    let server = signed_in(&fixtures).await;

    let songs = all_songs(server.as_ref(), 10).await;

    // i.up1 names its catalog id only in its relationship, i.buy1 only as purchased.
    assert_eq!(ids(&songs), ["i.up1", "i.buy1", "catalog:song:7006"]);
    let lookups: Vec<String> = fixtures
        .seen_for("GET", "/v1/me/library/songs")
        .into_iter()
        .map(|request| request.target)
        .filter(|target| target.contains("ids="))
        .collect();
    assert_eq!(lookups, ["/v1/me/library/songs?ids=i.up1&include=catalog"]);
}

#[tokio::test]
async fn writes_patch_all_songs_without_a_rebuild() {
    let fixtures = FixtureServer::start().await;
    serve_library(&fixtures);
    fixtures
        .on(
            "GET",
            "/v1/me/library/playlists/p.mine1",
            fixture("library_playlist_mine.json"),
        )
        .on(
            "POST",
            "/v1/me/library/playlists/p.mine1/tracks",
            Reply::status(204),
        )
        .on(
            "POST",
            "/v1/me/library/playlists",
            Reply::fixture_status(201, "server/playlist_created.json"),
        )
        .on_target(
            "GET",
            "/v1/catalog/it/songs?ids=8001",
            Reply::json(json!({"data": [{
                "id": "8001",
                "type": "songs",
                "attributes": {"name": "Appended", "artistName": "Artist Eight",
                    "durationInMillis": 100000, "playParams": {"id": "8001", "kind": "song"}}
            }]})),
        )
        .on_target(
            "GET",
            "/v1/me/library/songs?ids=i.new1&include=albums,artists",
            Reply::json(json!({"data": [library_song("i.new1", "Added Later")]})),
        )
        .on(
            "GET",
            "/v1/me/library/recently-added",
            fixture("recently_added.json"),
        )
        .on(
            "GET",
            "/v1/me/library/albums",
            fixture("library_albums.json"),
        );
    let server = signed_in(&fixtures).await;
    assert_eq!(server.scan_status().await.unwrap().count, Some(6));
    server.album_list(10, 0).await.unwrap();

    // i.lib1 is listed, and 1001 is its catalog id: only 8001 is new.
    server
        .update_playlist(
            "p.mine1",
            None,
            &["catalog:song:8001", "i.lib1", "catalog:song:1001"],
            &[],
        )
        .await
        .unwrap();
    server
        .create_playlist("Mix", &["i.new1", "catalog:song:8001"])
        .await
        .unwrap();

    let songs = all_songs(server.as_ref(), 4).await;
    assert_eq!(&ids(&songs)[..6], UNION);
    assert_eq!(&ids(&songs)[6..], ["catalog:song:8001", "i.new1"]);
    assert_eq!(server.scan_status().await.unwrap().count, Some(8));
    assert_eq!(library_page_reads(&fixtures), 2);
    assert_eq!(fixtures.seen_for("GET", "/v1/catalog/it/songs").len(), 1);
    server.album_list(10, 0).await.unwrap();
    assert_eq!(fixtures.seen_for("GET", "/v1/me/library/albums").len(), 1);
}

#[tokio::test]
async fn stars_set_and_delete_ratings_and_list_the_liked_songs() {
    let fixtures = FixtureServer::start().await;
    serve_library(&fixtures);
    fixtures
        .on(
            "GET",
            "/v1/me/ratings/library-songs",
            fixture("ratings_library.json"),
        )
        .on(
            "GET",
            "/v1/me/ratings/songs",
            fixture("ratings_catalog.json"),
        )
        .on(
            "PUT",
            "/v1/me/ratings/library-songs/i.lib1",
            fixture("rating_put_library.json"),
        )
        .on(
            "PUT",
            "/v1/me/ratings/songs/4001",
            fixture("rating_put_catalog.json"),
        )
        .on("DELETE", "/v1/me/ratings/songs/3001", Reply::status(204))
        .on(
            "DELETE",
            "/v1/me/ratings/library-songs/i.lib2",
            Reply::status(204),
        );
    let server = signed_in(&fixtures).await;

    let starred = server.starred_songs().await.unwrap();
    assert_eq!(ids(&starred), ["i.lib2", "catalog:song:3001"]);
    assert!(starred.iter().all(|song| song.starred.is_some()));
    assert_eq!(
        fixtures.seen_for("GET", "/v1/me/ratings/library-songs")[0].target,
        "/v1/me/ratings/library-songs?ids=i.lib1,i.lib2,i.lib3,i.pl1"
    );
    assert_eq!(
        fixtures.seen_for("GET", "/v1/me/ratings/songs")[0].target,
        "/v1/me/ratings/songs?ids=3001,4001"
    );
    let listed = all_songs(server.as_ref(), 10).await;
    let liked: Vec<&str> = listed
        .iter()
        .filter(|song| song.starred.is_some())
        .map(|song| song.id.as_str())
        .collect();
    assert_eq!(liked, ["i.lib2", "catalog:song:3001"]);

    server.star(&["i.lib1", "catalog:song:4001"]).await.unwrap();
    server
        .unstar(&["catalog:song:3001", "i.lib2"])
        .await
        .unwrap();

    let put = fixtures.seen_for("PUT", "/v1/me/ratings/library-songs/i.lib1");
    assert_eq!(
        body_json(&put[0].body),
        json!({"type": "rating", "attributes": {"value": 1}})
    );
    assert_eq!(
        fixtures.seen_for("PUT", "/v1/me/ratings/songs/4001").len(),
        1
    );
    assert_eq!(
        fixtures
            .seen_for("DELETE", "/v1/me/ratings/songs/3001")
            .len(),
        1
    );
    assert_eq!(
        fixtures
            .seen_for("DELETE", "/v1/me/ratings/library-songs/i.lib2")
            .len(),
        1
    );

    let starred = server.starred_songs().await.unwrap();
    assert_eq!(ids(&starred), ["i.lib1", "catalog:song:4001"]);
    // The kept ratings answered: no further reads.
    assert_eq!(
        fixtures
            .seen_for("GET", "/v1/me/ratings/library-songs")
            .len(),
        1
    );
    assert_eq!(fixtures.seen_for("GET", "/v1/me/ratings/songs").len(), 1);

    assert_not_found(server.star(&["catalog:artist:1"]).await);
}

#[tokio::test]
async fn listings_carry_likes_read_in_batches_of_100() {
    let fixtures = FixtureServer::start().await;
    let tracks: Vec<Value> = (0..150)
        .map(|n| library_song(&format!("i.t{n}"), &format!("Track {n}")))
        .collect();
    fixtures
        .on(
            "GET",
            "/v1/me/library/albums/l.long",
            Reply::json(json!({"data": [{
                "id": "l.long", "type": "library-albums",
                "attributes": {"name": "Long Album", "artistName": "Artist Seven", "trackCount": 150}
            }]})),
        )
        .on(
            "GET",
            "/v1/me/library/albums/l.long/tracks",
            Reply::json(json!({"data": tracks})),
        )
        .on(
            "GET",
            "/v1/me/ratings/library-songs",
            Reply::json(json!({"data": [
                {"id": "i.t0", "type": "ratings", "attributes": {"value": 1}},
                {"id": "i.t120", "type": "ratings", "attributes": {"value": 1}},
                {"id": "i.t121", "type": "ratings", "attributes": {"value": -1}}
            ]})),
        )
        .on(
            "GET",
            "/v1/me/library/playlists/p.mine1",
            fixture("library_playlist_mine.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists/p.mine1/tracks",
            fixture("playlist_mine_tracks.json"),
        )
        .on(
            "GET",
            "/v1/me/ratings/songs",
            Reply::fixture_status(400, "error_not_found.json"),
        );
    let server = signed_in(&fixtures).await;

    let album = server.album("l.long").await.unwrap();
    let liked: Vec<&str> = album
        .songs
        .iter()
        .filter(|song| song.starred.is_some())
        .map(|song| song.id.as_str())
        .collect();
    assert_eq!(liked, ["i.t0", "i.t120"]);
    let mut batches: Vec<usize> = fixtures
        .seen_for("GET", "/v1/me/ratings/library-songs")
        .iter()
        .map(id_count)
        .collect();
    batches.sort_unstable();
    assert_eq!(batches, [50, 100]);

    // Catalog ratings fail here: the playlist is still listed, those songs unmarked.
    let playlist = server.playlist("p.mine1").await.unwrap();
    assert_eq!(playlist.songs.len(), 3);
    assert!(playlist.songs.iter().all(|song| song.starred.is_none()));
    assert_eq!(fixtures.seen_for("GET", "/v1/me/ratings/songs").len(), 1);
}

#[tokio::test]
async fn playlists_carry_how_far_they_can_be_edited() {
    let fixtures = FixtureServer::start().await;
    serve_library(&fixtures);
    fixtures
        .on(
            "GET",
            "/v1/me/library/playlists/p.mine1",
            fixture("library_playlist_mine.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists/p.favorites",
            fixture("library_playlist_favorites.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists/p.empty",
            Reply::json(
                json!({"data": [{"id": "p.empty", "type": "library-playlists",
                "attributes": {"name": "Empty Draft", "canEdit": true, "hasCatalog": false}}]}),
            ),
        )
        .on(
            "POST",
            "/v1/me/library/playlists/p.mine1/tracks",
            Reply::status(204),
        )
        .on(
            "POST",
            "/v1/me/library/playlists",
            Reply::fixture_status(201, "server/playlist_created.json"),
        );
    let server = signed_in(&fixtures).await;

    let playlists = server.playlists().await.unwrap();
    let edits: Vec<(&str, Option<RuneEdit>)> = playlists
        .iter()
        .map(|playlist| (playlist.id.as_str(), playlist.rune_edit))
        .collect();
    assert_eq!(
        edits,
        [
            ("p.mine1", Some(RuneEdit::AppendOnly)),
            ("p.favorites", Some(RuneEdit::ReadOnly)),
            ("p.editorial", Some(RuneEdit::ReadOnly)),
            ("p.empty", Some(RuneEdit::AppendOnly)),
            ("p.public", Some(RuneEdit::AppendOnly)),
        ]
    );
    assert_eq!(playlists[0].name, "Road Trip");
    assert!(playlists[0].cover_art.is_some());
    // Apple's library playlists carry no track count.
    assert_eq!(playlists[0].song_count, None);

    let detail = server.playlist("p.mine1").await.unwrap();
    assert_eq!(detail.name, "Road Trip");
    assert_eq!(detail.rune_edit, Some(RuneEdit::AppendOnly));
    assert_eq!(ids(&detail.songs), ["i.lib2", "i.pl1", "catalog:song:1001"]);
    let keys: Vec<&str> = detail
        .songs
        .iter()
        .map(|song| song.rune_playback.as_ref().unwrap().key.as_deref().unwrap())
        .collect();
    assert_eq!(keys, ["i.lib2", "i.pl1", "catalog:1001"]);
    assert_eq!(detail.song_count, Some(3));
    // Apple answers 404 for the tracks of an empty playlist.
    let empty = server.playlist("p.empty").await.unwrap();
    assert_eq!(empty.name, "Empty Draft");
    assert!(empty.songs.is_empty());

    server
        .create_playlist("Mix", &["i.lib1", "catalog:song:3001"])
        .await
        .unwrap();
    assert_eq!(
        body_json(&fixtures.seen_for("POST", "/v1/me/library/playlists")[0].body),
        json!({
            "attributes": {"name": "Mix"},
            "relationships": {"tracks": {"data": [
                {"id": "i.lib1", "type": "library-songs"},
                {"id": "3001", "type": "songs"}
            ]}}
        })
    );
    // Listed before Apple lists it, so the hub finds what it created.
    let after = server.playlists().await.unwrap();
    assert_eq!(after.last().unwrap().id, "p.new1");
    assert_eq!(after.len(), 6);

    server
        .update_playlist("p.mine1", None, &["i.lib3", "catalog:song:4001"], &[])
        .await
        .unwrap();
    assert_eq!(
        body_json(&fixtures.seen_for("POST", "/v1/me/library/playlists/p.mine1/tracks")[0].body),
        json!({"data": [
            {"id": "i.lib3", "type": "library-songs"},
            {"id": "4001", "type": "songs"}
        ]})
    );
    server
        .update_playlist("p.mine1", None, &[], &[])
        .await
        .unwrap();

    assert_unsupported(
        server
            .update_playlist("p.favorites", None, &["i.lib1"], &[])
            .await,
        "updatePlaylist",
    );
    assert!(fixtures
        .seen_for("POST", "/v1/me/library/playlists/p.favorites/tracks")
        .is_empty());
    assert_unsupported(
        server
            .update_playlist("p.mine1", Some("Renamed"), &[], &[])
            .await,
        "updatePlaylist",
    );
    assert_unsupported(
        server
            .update_playlist("p.mine1", None, &["i.lib1"], &[0])
            .await,
        "updatePlaylist",
    );
    assert_unsupported(server.delete_playlist("p.mine1").await, "deletePlaylist");
    assert_unsupported(
        server.replace_playlist_songs("p.mine1", &["i.lib1"]).await,
        "createPlaylist",
    );
    assert_eq!(
        fixtures
            .seen_for("POST", "/v1/me/library/playlists/p.mine1/tracks")
            .len(),
        1
    );
}

#[tokio::test]
async fn cover_art_is_fetched_without_tokens_within_its_limits() {
    let fixtures = FixtureServer::start().await;
    let cover = |body: &str| Reply {
        status: 200,
        body: body.into(),
        headers: vec![("content-type".into(), "image/jpeg".into())],
    };
    fixtures
        .on(
            "GET",
            "/v1/me/library/songs",
            fixture_with_artwork(&fixtures, "library_song.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists",
            fixture_with_artwork(&fixtures, "library_playlists.json"),
        )
        .on(
            "GET",
            "/v1/me/library/albums/l.alb1",
            fixture_with_artwork(&fixtures, "library_album.json"),
        )
        .on(
            "GET",
            "/v1/me/library/albums/l.alb1/tracks",
            Reply::json(json!({"data": []})),
        )
        .on("GET", "/dawn/300x300bb.jpg", cover("small cover"))
        .on("GET", "/dawn/1200x1200bb.jpg", cover("large cover"))
        .on("GET", "/dawn/1400x1400bb.jpg", cover("original cover"))
        .on(
            "GET",
            "/roadtrip/1000x1000bb.jpg",
            Reply::fixture_status(404, "error_not_found.json"),
        )
        .on(
            "GET",
            "/roadtrip/100x100bb.jpg",
            cover(&"x".repeat(8 * 1024 * 1024 + 1)),
        );
    let server = signed_in(&fixtures).await;

    let song_cover = server.song("i.lib1").await.unwrap().cover_art.unwrap();
    assert_eq!(
        server.cover_art(&song_cover, Some(300)).await.unwrap(),
        b"small cover"
    );
    assert_eq!(
        server.cover_art(&song_cover, None).await.unwrap(),
        b"large cover"
    );
    // Never above the original's 1400 pixels.
    assert_eq!(
        server.cover_art(&song_cover, Some(3000)).await.unwrap(),
        b"original cover"
    );
    for request in fixtures
        .seen()
        .iter()
        .filter(|request| request.path.starts_with("/dawn/"))
    {
        assert_eq!(request.authorization, None);
        assert_eq!(request.user_token, None);
    }
    // The album shares the song's artwork, and so its cover id.
    let album = server.album("l.alb1").await.unwrap();
    assert_eq!(album.cover_art.as_deref(), Some(song_cover.as_str()));

    let playlist_cover = server.playlists().await.unwrap()[0]
        .cover_art
        .clone()
        .unwrap();
    assert_not_found(server.cover_art(&playlist_cover, Some(1000)).await);
    assert!(matches!(
        server.cover_art(&playlist_cover, Some(100)).await,
        Err(SubsonicError::Malformed(_))
    ));
    assert_not_found(server.cover_art("art:0000000000000000", None).await);
}

#[tokio::test]
async fn artists_artist_and_album_browse_the_library() {
    let fixtures = FixtureServer::start().await;
    fixtures
        .on(
            "GET",
            "/v1/me/library/artists",
            fixture("library_artists_page1.json"),
        )
        .on(
            "GET",
            "/v1/me/library/artists",
            fixture("library_artists_page2.json"),
        )
        .on(
            "GET",
            "/v1/me/library/artists/r.art1",
            fixture("library_artist.json"),
        )
        .on(
            "GET",
            "/v1/me/library/artists/r.art1/albums",
            fixture("library_artist_albums.json"),
        )
        .on(
            "GET",
            "/v1/me/library/albums/l.alb1",
            fixture("library_album.json"),
        )
        .on(
            "GET",
            "/v1/me/library/albums/l.alb1/tracks",
            fixture("library_album_tracks.json"),
        )
        .on(
            "GET",
            "/v1/me/library/albums/l.missing",
            Reply::fixture_status(404, "error_not_found.json"),
        )
        .on(
            "GET",
            "/v1/me/library/albums/l.missing/tracks",
            Reply::fixture_status(404, "error_not_found.json"),
        )
        .on(
            "GET",
            "/v1/me/ratings/library-songs",
            fixture("ratings_library.json"),
        );
    let server = signed_in(&fixtures).await;

    let artists = server.artists().await.unwrap();
    let names: Vec<(&str, &str)> = artists
        .iter()
        .map(|artist| (artist.id.as_str(), artist.name.as_str()))
        .collect();
    assert_eq!(names, [("r.art1", "Artist One"), ("r.art2", "Artist Two")]);
    assert_eq!(
        fixtures.seen_for("GET", "/v1/me/library/artists")[1].target,
        "/v1/me/library/artists?offset=1&limit=100"
    );

    let artist = server.artist("r.art1").await.unwrap();
    assert_eq!(artist.name, "Artist One");
    assert_eq!(artist.album_count, Some(2));
    let albums: Vec<&str> = artist
        .albums
        .iter()
        .map(|album| album.id.as_str())
        .collect();
    assert_eq!(albums, ["l.alb1", "l.alb3"]);
    assert!(artist
        .albums
        .iter()
        .all(|album| album.artist_id.as_deref() == Some("r.art1")));

    let album = server.album("l.alb1").await.unwrap();
    assert_eq!(album.name, "Dawn Album");
    assert_eq!(album.artist.as_deref(), Some("Artist One"));
    assert_eq!(album.artist_id.as_deref(), Some("r.art1"));
    assert_eq!(album.year, Some(2020));
    assert_eq!(album.song_count, Some(3));
    assert_eq!(album.duration, Some(246 + 180 + 100));
    assert_eq!(ids(&album.songs), ["i.lib1", "i.lib2", "i.lib4"]);
    assert!(album
        .songs
        .iter()
        .all(|song| song.album_id.as_deref() == Some("l.alb1")));
    assert_eq!(album.songs[0].artist_id.as_deref(), Some("r.art1"));
    // A guest's track names another artist than the album's.
    assert_eq!(album.songs[2].artist_id, None);
    // Without artwork of its own, a track shows the album's.
    assert_eq!(album.songs[2].cover_art, album.cover_art);
    // Likes, as a Subsonic album lists them.
    assert!(album.songs[1].starred.is_some());
    assert!(album.songs[0].starred.is_none());
    assert_eq!(
        fixtures.seen_for("GET", "/v1/me/library/albums/l.alb1")[0].target,
        "/v1/me/library/albums/l.alb1?include=artists"
    );

    assert_not_found(server.album("l.missing").await);
    assert_not_found(server.artist("catalog:artist:1").await);
}

#[tokio::test]
async fn album_list_puts_recently_added_albums_first() {
    let fixtures = FixtureServer::start().await;
    fixtures
        .on(
            "GET",
            "/v1/me/library/recently-added",
            fixture("recently_added.json"),
        )
        .on(
            "GET",
            "/v1/me/library/albums",
            fixture("library_albums.json"),
        );
    let server = signed_in(&fixtures).await;

    let first = server.album_list(2, 0).await.unwrap();
    let rest = server.album_list(2, 2).await.unwrap();
    let listed: Vec<&str> = first
        .iter()
        .chain(&rest)
        .map(|album| album.id.as_str())
        .collect();
    assert_eq!(listed, ["l.alb2", "l.alb1", "l.alb3"]);
    assert_eq!(first[0].artist_id.as_deref(), Some("r.art2"));
    assert_eq!(first[0].song_count, Some(1));
    assert!(server.album_list(2, 4).await.unwrap().is_empty());
    assert_eq!(
        fixtures.seen_for("GET", "/v1/me/library/albums")[0].target,
        "/v1/me/library/albums?limit=100&include=artists"
    );
    assert_eq!(
        fixtures.seen_for("GET", "/v1/me/library/recently-added")[0].target,
        "/v1/me/library/recently-added?limit=25"
    );
    assert_eq!(fixtures.seen_for("GET", "/v1/me/library/albums").len(), 1);
}

#[tokio::test]
async fn album_list_stays_complete_without_recently_added() {
    let fixtures = FixtureServer::start().await;
    fixtures
        .on(
            "GET",
            "/v1/me/library/recently-added",
            Reply::fixture_status(503, "error_server.json"),
        )
        .on(
            "GET",
            "/v1/me/library/albums",
            fixture("library_albums.json"),
        );
    let server = signed_in(&fixtures).await;

    let albums = server.album_list(10, 0).await.unwrap();
    let listed: Vec<&str> = albums.iter().map(|album| album.id.as_str()).collect();
    assert_eq!(listed, ["l.alb1", "l.alb2", "l.alb3"]);
}

#[tokio::test]
async fn search_reads_the_library_search_per_type() {
    let fixtures = FixtureServer::start().await;
    fixtures.on(
        "GET",
        "/v1/me/library/search",
        fixture("library_search.json"),
    );
    let server = signed_in(&fixtures).await;

    let found = server
        .search_with_song_offset(
            "dawn light",
            SearchLimits {
                artists: 3,
                albums: 2,
                songs: 30,
            },
            5,
        )
        .await
        .unwrap();

    assert_eq!(ids(&found.songs), ["i.lib1"]);
    assert_eq!(
        found.songs[0]
            .rune_playback
            .as_ref()
            .unwrap()
            .key
            .as_deref(),
        Some("i.lib1")
    );
    assert_eq!(found.albums[0].id, "l.alb1");
    assert_eq!(found.artists[0].name, "Artist One");
    let targets: HashSet<String> = fixtures
        .seen_for("GET", "/v1/me/library/search")
        .into_iter()
        .map(|request| request.target)
        .collect();
    assert_eq!(
        targets,
        HashSet::from([
            "/v1/me/library/search?term=dawn+light&types=library-songs&limit=25&offset=5".into(),
            "/v1/me/library/search?term=dawn+light&types=library-albums&limit=2&offset=0".into(),
            "/v1/me/library/search?term=dawn+light&types=library-artists&limit=3&offset=0".into(),
        ])
    );
}

#[tokio::test]
async fn search_pages_past_25_results_and_past_the_end() {
    let fixtures = FixtureServer::start().await;
    let page = |range: std::ops::Range<u32>, next: Option<&str>| {
        let songs: Vec<Value> = range
            .map(|n| library_song(&format!("i.s{n}"), &format!("Many {n}")))
            .collect();
        Reply::json(json!({"results": {"library-songs": {"data": songs, "next": next}}}))
    };
    let search = "/v1/me/library/search?term=many&types=library-songs";
    fixtures
        .on_target(
            "GET",
            &format!("{search}&limit=25&offset=0"),
            page(0..25, Some("/v1/me/library/search?offset=25")),
        )
        .on_target(
            "GET",
            &format!("{search}&limit=5&offset=25"),
            page(25..30, Some("/v1/me/library/search?offset=30")),
        )
        .on_target(
            "GET",
            &format!("{search}&limit=10&offset=100"),
            Reply::json(json!({"results": {}, "meta": {"results": {"order": []}}})),
        );
    let server = signed_in(&fixtures).await;

    let found = server.search("many", songs_only(30)).await.unwrap();
    assert_eq!(found.songs.len(), 30);
    assert_eq!(found.songs[29].id, "i.s29");
    let past_the_end = server
        .search_with_song_offset("many", songs_only(10), 100)
        .await
        .unwrap();
    assert!(past_the_end.songs.is_empty());
    assert_eq!(fixtures.seen_for("GET", "/v1/me/library/search").len(), 3);
}

#[tokio::test]
async fn client_errors_map_onto_subsonic_errors() {
    let fixtures = FixtureServer::start().await;
    fixtures
        .on("GET", "/v1/me/library/artists", Reply::status(429))
        .on(
            "GET",
            "/v1/me/library/playlists",
            Reply::fixture_status(503, "error_server.json"),
        )
        .on(
            "GET",
            "/v1/me/library/songs",
            Reply::fixture_status(404, "error_not_found.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists/p.missing",
            Reply::fixture_status(404, "error_not_found.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists/p.missing/tracks",
            Reply::fixture_status(404, "error_not_found.json"),
        )
        .on(
            "GET",
            "/v1/catalog/it/songs",
            Reply::json(json!({"data": []})),
        );
    let server = signed_in(&fixtures).await;

    assert!(matches!(
        server.artists().await,
        Err(SubsonicError::Server {
            kind: SubsonicServerError::Generic,
            rune_reason: Some(RuneReason::ServiceLimit),
            ..
        })
    ));
    assert!(matches!(
        server.playlists().await,
        Err(SubsonicError::HttpStatus(status)) if status.as_u16() == 503
    ));
    assert_not_found(server.song("i.missing").await);
    assert_not_found(server.song("catalog:song:999").await);
    assert_not_found(server.playlist("p.missing").await);

    let unreachable: Arc<dyn SubsonicApi> = Arc::new(AppleMusicServer::with_client(|builder| {
        builder
            .base_url("http://127.0.0.1:1")
            .storefront("it")
            .retry_policy(fast_retry())
    }));
    unreachable
        .complete_rune_login(&platform(Some(USER_TOKEN), Some(DEVELOPER_TOKEN)))
        .await
        .unwrap();
    assert!(matches!(
        unreachable.artists().await,
        Err(SubsonicError::Transport(_))
    ));
}

#[tokio::test]
async fn failures_while_building_all_songs_are_reported() {
    let fixtures = FixtureServer::start().await;
    fixtures
        .on_target(
            "GET",
            LIBRARY_PAGE_1,
            Reply {
                status: 200,
                body: "not json".into(),
                headers: Vec::new(),
            },
        )
        .on(
            "GET",
            "/v1/me/library/playlists",
            Reply::json(json!({"data": []})),
        );
    let server = signed_in(&fixtures).await;
    assert!(matches!(
        server.scan_status().await,
        Err(SubsonicError::Malformed(_))
    ));

    let broken = FixtureServer::start().await;
    broken
        .on_target("GET", LIBRARY_PAGE_1, fixture("library_songs_page2.json"))
        .on(
            "GET",
            "/v1/me/library/playlists",
            fixture("library_playlist_mine.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists/p.mine1/tracks",
            Reply::fixture_status(500, "error_server.json"),
        );
    let server = signed_in(&broken).await;
    assert!(matches!(
        server.search_with_song_offset("", songs_only(10), 0).await,
        Err(SubsonicError::HttpStatus(status)) if status.as_u16() == 500
    ));
}

#[tokio::test]
async fn streaming_lyrics_and_scrobbles_stay_with_apple() {
    let fixtures = FixtureServer::start().await;
    let server = signed_in(&fixtures).await;

    assert!(matches!(
        server.stream("i.lib1", None).await,
        Err(StreamError::Unsupported)
    ));
    assert!(matches!(
        server.lyrics("Artist One", "First Light").await,
        Err(SubsonicError::Unsupported { .. })
    ));
    assert!(matches!(
        server.lyrics_by_song_id("i.lib1").await,
        Err(SubsonicError::Unsupported { .. })
    ));
    assert!(matches!(
        server.jukebox_status().await,
        Err(SubsonicError::Unsupported {
            endpoint: "jukeboxControl"
        })
    ));
    server.scrobble("i.lib1", Some(1), true).await.unwrap();
    assert!(fixtures.seen().is_empty());
}
