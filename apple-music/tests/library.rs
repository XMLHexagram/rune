//! The library layer against the fixture server.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use apple_music::library::{
    AppleLibrary, Clock, LibraryError, SignInChange, SongList, Tokens, Track, TrackKey,
};
use common::{fast_retry, FixtureServer, Reply, Seen};
use serde_json::{json, Value};

const USER_TOKEN: &str = "music-user-token";
const DEVELOPER_TOKEN: &str = "developer-token";
const LIBRARY_PAGE_1: &str = "/v1/me/library/songs?limit=100&include=albums,artists";
const LIBRARY_PAGE_2: &str = "/v1/me/library/songs?offset=2&limit=100&include=albums,artists";
const TTL: Duration = Duration::from_secs(10 * 60);

/// A clock that moves only when told to.
struct ManualClock {
    start: Instant,
    elapsed: Mutex<Duration>,
}

impl ManualClock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            start: Instant::now(),
            elapsed: Mutex::default(),
        })
    }

    fn advance(&self, by: Duration) {
        *self.elapsed.lock().unwrap() += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        self.start + *self.elapsed.lock().unwrap()
    }
}

fn library_for(fixtures: &FixtureServer) -> AppleLibrary {
    let base_url = fixtures.base_url.clone();
    AppleLibrary::with_client(|builder| {
        builder
            .base_url(base_url)
            .storefront("it")
            .retry_policy(fast_retry())
    })
}

fn tokens() -> Tokens {
    Tokens::new(DEVELOPER_TOKEN, USER_TOKEN).unwrap()
}

fn tokens_of(user: &str) -> Tokens {
    Tokens::new(DEVELOPER_TOKEN, user).unwrap()
}

/// Waits until `condition` holds.
async fn until(condition: impl Fn() -> bool) {
    for _ in 0..500 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out");
}

/// Waits until the fixture server has seen a request for `method path`.
async fn seen_request(fixtures: &FixtureServer, method: &str, path: &str) {
    until(|| !fixtures.seen_for(method, path).is_empty()).await;
}

fn signed_in(fixtures: &FixtureServer) -> AppleLibrary {
    let library = library_for(fixtures);
    library.sign_in(tokens());
    library
}

fn fixture(name: &str) -> Reply {
    Reply::fixture(name)
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

fn key(text: &str) -> TrackKey {
    text.parse().unwrap()
}

fn keys(tracks: &[Track]) -> Vec<String> {
    tracks.iter().map(|track| track.key.to_string()).collect()
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

fn assert_signed_out<T: std::fmt::Debug>(result: Result<T, LibraryError>) {
    assert!(matches!(result, Err(LibraryError::SignedOut)), "{result:?}");
}

fn assert_not_found<T: std::fmt::Debug>(result: Result<T, LibraryError>) {
    assert!(
        matches!(result, Err(LibraryError::NotFound(_))),
        "{result:?}"
    );
}

fn assert_refused<T: std::fmt::Debug>(result: Result<T, LibraryError>) {
    assert!(matches!(result, Err(LibraryError::Refused)), "{result:?}");
}

/// The library, three joining playlists (one empty), Favorites and a subscribed one.
fn serve_library(fixtures: &FixtureServer) {
    fixtures
        .on_target(
            "GET",
            LIBRARY_PAGE_1,
            fixture("union_library_songs_page1.json"),
        )
        .on_target(
            "GET",
            LIBRARY_PAGE_2,
            fixture("union_library_songs_page2.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists",
            fixture("union_playlists.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists/p.mine1/tracks",
            fixture("union_playlist_mine_tracks.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists/p.favorites/tracks",
            fixture("union_playlist_favorites_tracks.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists/p.editorial/tracks",
            fixture("union_playlist_editorial_tracks.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists/p.empty/tracks",
            Reply::fixture_status(404, "error_not_found.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists/p.public/tracks",
            fixture("union_playlist_public_tracks.json"),
        );
}

const UNION: [&str; 6] = [
    "i.lib1",
    "i.lib2",
    "i.lib3",
    "i.pl1",
    "catalog:3001",
    "catalog:4001",
];

async fn song_keys(library: &AppleLibrary) -> Vec<String> {
    let list: Arc<SongList> = library.songs().await.unwrap();
    keys(list.songs())
}

// Sign-in ----------------------------------------------------------------

#[tokio::test]
async fn every_call_needing_apple_asks_for_sign_in_first() {
    let fixtures = FixtureServer::start().await;
    serve_library(&fixtures);
    let library = library_for(&fixtures);
    let song = key("i.lib1");

    assert_signed_out(library.check().await);
    assert_signed_out(library.songs().await);
    assert_signed_out(library.track(&song).await);
    assert_signed_out(library.track(&key("catalog:1")).await);
    assert_signed_out(library.albums().await);
    assert_signed_out(library.album("l.alb1").await);
    assert_signed_out(library.artists().await);
    assert_signed_out(library.artist("r.art1").await);
    assert_signed_out(library.playlists().await);
    assert_signed_out(library.playlist_tracks("p.mine1").await);
    assert_signed_out(library.liked(std::slice::from_ref(&song)).await);
    assert_signed_out(library.liked(&[]).await);
    assert_signed_out(library.set_liked(&song, true).await);
    assert_signed_out(library.set_liked(&song, false).await);
    assert_signed_out(library.create_playlist("Mix", &[]).await);
    assert_signed_out(library.append_to_playlist("p.mine1", &[song]).await);
    assert_signed_out(library.append_to_playlist("p.mine1", &[]).await);
    assert!(fixtures.seen().is_empty());
}

#[tokio::test]
async fn incomplete_tokens_are_refused_and_sign_out_forgets_everything() {
    assert!(Tokens::new(DEVELOPER_TOKEN, "").is_err());
    assert!(Tokens::new("", USER_TOKEN).is_err());

    let fixtures = FixtureServer::start().await;
    fixtures
        .on_target(
            "GET",
            LIBRARY_PAGE_1,
            fixture("union_library_songs_page2.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists",
            Reply::json(json!({"data": []})),
        );
    let library = signed_in(&fixtures);
    assert_eq!(library.songs().await.unwrap().len(), 1);

    library.sign_out();
    assert_signed_out(library.songs().await);
    assert_eq!(library.sign_in(tokens()), SignInChange::Changed);
    assert_eq!(library.songs().await.unwrap().len(), 1);
    assert_eq!(library_page_reads(&fixtures), 2);
}

#[tokio::test]
async fn sign_in_hands_both_tokens_to_calls_and_keeps_them_out_of_debug() {
    let fixtures = FixtureServer::start().await;
    fixtures.on(
        "GET",
        "/v1/me/library/artists",
        fixture("browse_artists_page2.json"),
    );
    let library = library_for(&fixtures);
    assert_signed_out(library.artists().await);

    assert_eq!(library.sign_in(tokens()), SignInChange::Changed);
    assert_eq!(library.sign_in(tokens()), SignInChange::Unchanged);
    library.artists().await.unwrap();

    let seen = fixtures.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].authorization.as_deref(),
        Some("Bearer developer-token")
    );
    assert_eq!(seen[0].user_token.as_deref(), Some(USER_TOKEN));

    for debug in [format!("{:?}", tokens()), format!("{library:?}")] {
        assert!(!debug.contains(USER_TOKEN), "{debug}");
        assert!(!debug.contains(DEVELOPER_TOKEN), "{debug}");
    }
}

#[tokio::test]
async fn check_reads_the_storefront_and_reports_rejected_tokens() {
    let fixtures = FixtureServer::start().await;
    fixtures.on("GET", "/v1/me/storefront", fixture("storefront.json"));
    let library = signed_in(&fixtures);
    assert_eq!(library.check().await.unwrap().id, "it");

    let rejected = FixtureServer::start().await;
    rejected.on(
        "GET",
        "/v1/me/storefront",
        Reply::fixture_status(401, "error_unauthorized.json"),
    );
    assert_signed_out(signed_in(&rejected).check().await);
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
    let library = signed_in(&fixtures);

    assert_signed_out(library.artists().await);
    // The client retried with a refreshed user token, then developer token.
    assert_eq!(fixtures.seen_for("GET", "/v1/me/library/artists").len(), 3);
    assert!(!fixtures.seen_for("GET", "/v1/me/storefront").is_empty());
    assert_signed_out(library.playlists().await);
}

#[tokio::test]
async fn refused_requests_with_working_tokens_are_not_an_expired_sign_in() {
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
    let library = signed_in(&fixtures);

    assert_refused(library.set_liked(&key("i.lib1"), true).await);
    assert_refused(library.set_liked(&key("catalog:3001"), false).await);
    assert_eq!(fixtures.seen_for("GET", "/v1/me/storefront").len(), 2);
}

#[tokio::test]
async fn only_new_tokens_start_over() {
    let fixtures = FixtureServer::start().await;
    fixtures
        .on_target(
            "GET",
            LIBRARY_PAGE_1,
            fixture("union_library_songs_page2.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists",
            Reply::json(json!({"data": []})),
        )
        .on(
            "POST",
            "/v1/me/library/playlists",
            Reply::fixture_status(201, "playlist_created.json"),
        )
        .on(
            "GET",
            "/v1/me/ratings/library-songs",
            fixture("likes_library_songs.json"),
        );
    let library = signed_in(&fixtures);
    let ratings_reads = || {
        fixtures
            .seen_for("GET", "/v1/me/ratings/library-songs")
            .len()
    };
    let created_ids = |playlists: Vec<apple_music::library::Playlist>| {
        playlists
            .into_iter()
            .map(|playlist| playlist.id)
            .collect::<Vec<_>>()
    };

    assert_eq!(library.songs().await.unwrap().len(), 1);
    library.create_playlist("Mix", &[]).await.unwrap();
    assert_eq!(created_ids(library.playlists().await.unwrap()).len(), 1);
    assert!(library.liked(&[key("i.lib2")]).await.unwrap()[&key("i.lib2")]);

    // The same tokens keep everything.
    assert_eq!(library.sign_in(tokens()), SignInChange::Unchanged);
    assert_eq!(library.songs().await.unwrap().len(), 1);
    library.liked(&[key("i.lib2")]).await.unwrap();
    assert_eq!(library_page_reads(&fixtures), 1);
    assert_eq!(ratings_reads(), 1);
    assert_eq!(created_ids(library.playlists().await.unwrap()).len(), 1);

    // Other tokens may be another account.
    let other = tokens_of("another-user-token");
    assert_eq!(library.sign_in(other), SignInChange::Changed);
    assert_eq!(library.songs().await.unwrap().len(), 1);
    library.liked(&[key("i.lib2")]).await.unwrap();
    assert_eq!(library_page_reads(&fixtures), 2);
    assert_eq!(ratings_reads(), 2);
    assert!(created_ids(library.playlists().await.unwrap()).is_empty());
    let last = fixtures.seen().pop().unwrap();
    assert_eq!(last.user_token.as_deref(), Some("another-user-token"));
}

#[tokio::test]
async fn reads_that_arrive_after_new_tokens_are_repeated_under_them_and_not_kept() {
    let fixtures = FixtureServer::start().await;
    fixtures
        .on_target(
            "GET",
            LIBRARY_PAGE_1,
            fixture("union_library_songs_page2.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists",
            Reply::json(json!({"data": []})),
        )
        .on(
            "GET",
            "/v1/me/ratings/library-songs",
            fixture("likes_library_songs.json"),
        )
        // The reads of the old sign-in are held until the tokens have changed.
        .delay("GET", "/v1/me/library/songs", Duration::from_millis(300))
        .delay(
            "GET",
            "/v1/me/ratings/library-songs",
            Duration::from_millis(300),
        );
    let library = signed_in(&fixtures);
    let other = tokens_of("another-user-token");
    let song = key("i.lib2");

    let asked = [song.clone()];
    let (songs, liked, ()) = tokio::join!(library.songs(), library.liked(&asked), async {
        seen_request(&fixtures, "GET", "/v1/me/library/songs").await;
        seen_request(&fixtures, "GET", "/v1/me/ratings/library-songs").await;
        library.sign_in(other);
    });

    // The answers belong to the new tokens: the reads were repeated under them.
    assert_eq!(songs.unwrap().len(), 1);
    assert!(liked.unwrap()[&song]);
    assert_eq!(library_page_reads(&fixtures), 2);
    let ratings = fixtures.seen_for("GET", "/v1/me/ratings/library-songs");
    assert_eq!(ratings.len(), 2);
    assert_eq!(ratings[0].user_token.as_deref(), Some(USER_TOKEN));
    assert_eq!(ratings[1].user_token.as_deref(), Some("another-user-token"));

    // And what the new sign-in read is kept, what the old one read is not.
    library.songs().await.unwrap();
    library.liked(&[song]).await.unwrap();
    assert_eq!(library_page_reads(&fixtures), 2);
    assert_eq!(
        fixtures
            .seen_for("GET", "/v1/me/ratings/library-songs")
            .len(),
        2
    );
}

#[tokio::test]
async fn a_like_set_while_ratings_are_read_is_not_overwritten_by_the_read() {
    let fixtures = FixtureServer::start().await;
    fixtures
        .on(
            "GET",
            "/v1/me/ratings/library-songs",
            Reply::json(json!({"data": []})),
        )
        .on(
            "PUT",
            "/v1/me/ratings/library-songs/i.lib1",
            Reply::fixture("rating_put_library_song.json"),
        )
        .delay(
            "GET",
            "/v1/me/ratings/library-songs",
            Duration::from_millis(300),
        );
    let library = signed_in(&fixtures);
    let song = key("i.lib1");

    let asked = [song.clone()];
    let (read, ()) = tokio::join!(library.liked(&asked), async {
        seen_request(&fixtures, "GET", "/v1/me/ratings/library-songs").await;
        library.set_liked(&song, true).await.unwrap();
    });

    assert!(read.unwrap()[&song]);
    assert!(library.liked(std::slice::from_ref(&song)).await.unwrap()[&song]);
    assert_eq!(
        fixtures
            .seen_for("GET", "/v1/me/ratings/library-songs")
            .len(),
        1
    );
}

#[tokio::test]
async fn ratings_are_kept_for_ten_minutes() {
    let fixtures = FixtureServer::start().await;
    fixtures.on(
        "GET",
        "/v1/me/ratings/library-songs",
        fixture("likes_library_songs.json"),
    );
    let clock = ManualClock::new();
    let library = signed_in(&fixtures).with_clock(clock.clone());
    let song = key("i.lib2");
    let reads = || {
        fixtures
            .seen_for("GET", "/v1/me/ratings/library-songs")
            .len()
    };

    assert!(library.liked(std::slice::from_ref(&song)).await.unwrap()[&song]);
    clock.advance(TTL - Duration::from_secs(1));
    library.liked(std::slice::from_ref(&song)).await.unwrap();
    assert_eq!(reads(), 1);

    // Likes made in the Apple Music app show up.
    clock.advance(Duration::from_secs(1));
    library.liked(std::slice::from_ref(&song)).await.unwrap();
    assert_eq!(reads(), 2);
}

// Songs ------------------------------------------------------------------

#[tokio::test]
async fn songs_join_library_and_own_playlists_once() {
    let fixtures = FixtureServer::start().await;
    serve_library(&fixtures);
    let library = signed_in(&fixtures);

    let first = library.songs().await.unwrap();
    assert_eq!(keys(first.songs()), UNION);
    // Built once; the second page read by its offset.
    assert_eq!(song_keys(&library).await, UNION);
    assert_eq!(library_page_reads(&fixtures), 2);
    let pages: Vec<String> = fixtures
        .seen_for("GET", "/v1/me/library/songs")
        .into_iter()
        .map(|request| request.target)
        .filter(|target| !target.contains("ids="))
        .collect();
    assert_eq!(pages, [LIBRARY_PAGE_1, LIBRARY_PAGE_2]);

    // Editorial playlists are not the user's own music: their songs are not read.
    assert!(fixtures
        .seen_for("GET", "/v1/me/library/playlists/p.editorial/tracks")
        .is_empty());
    assert!(!first.lists(&key("catalog:9001")));
    // Favorites and the user's playlists are.
    assert_eq!(
        fixtures
            .seen_for("GET", "/v1/me/library/playlists/p.favorites/tracks")
            .len(),
        1
    );
    assert!(first.lists(&key("catalog:3001")));
    assert!(first.lists(&key("i.pl1")));

    // A playlist track outside the library is a catalog song, not in the library.
    let loved = first.get(&key("catalog:3001")).unwrap();
    assert_eq!(loved.title, "Loved Elsewhere");
    assert!(!loved.in_library);
    assert_eq!(loved.catalog_id.as_deref(), Some("3001"));
    // A library song keeps its relationships when a playlist lists it too.
    let second = first.get(&key("i.lib2")).unwrap();
    assert_eq!(second.album_id.as_deref(), Some("l.alb1"));
    assert!(second.in_library);
    // What the library has a copy of is in the library, listed or not.
    assert!(first.in_library(&key("catalog:2001")));
    assert!(!first.lists(&key("catalog:2001")) || first.get(&key("catalog:2001")).is_none());
    assert!(!first.in_library(&key("catalog:3001")));
    assert!(first.in_library(&key("i.pl1")));
}

#[tokio::test]
async fn catalog_copies_of_library_songs_are_dropped_by_catalog_id_relationship_or_purchase() {
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
    let library = signed_in(&fixtures);

    // i.up1 names its catalog id only in its relationship, i.buy1 only as purchased.
    assert_eq!(
        song_keys(&library).await,
        ["i.up1", "i.buy1", "catalog:7006"]
    );
    let lookups: Vec<String> = fixtures
        .seen_for("GET", "/v1/me/library/songs")
        .into_iter()
        .map(|request| request.target)
        .filter(|target| target.contains("ids="))
        .collect();
    assert_eq!(lookups, ["/v1/me/library/songs?ids=i.up1&include=catalog"]);
}

#[tokio::test]
async fn a_large_library_is_read_by_offset_from_its_total() {
    let fixtures = FixtureServer::start().await;
    let page = |from: usize, to: usize, next: Option<usize>| {
        let songs: Vec<Value> = (from..to)
            .map(|n| library_song(&format!("i.s{n:03}"), &format!("Song {n}")))
            .collect();
        let mut body = json!({"data": songs, "meta": {"total": 250}});
        if let Some(next) = next {
            body["next"] = json!(format!("/v1/me/library/songs?offset={next}"));
        }
        Reply::json(body)
    };
    fixtures
        .on_target("GET", LIBRARY_PAGE_1, page(0, 100, Some(100)))
        .on_target(
            "GET",
            "/v1/me/library/songs?offset=100&limit=100&include=albums,artists",
            // Only the total leads to the last page: this one names no next.
            page(100, 200, None),
        )
        .on_target(
            "GET",
            "/v1/me/library/songs?offset=200&limit=100&include=albums,artists",
            page(200, 250, None),
        )
        .on(
            "GET",
            "/v1/me/library/playlists",
            Reply::json(json!({"data": []})),
        );
    let library = signed_in(&fixtures);

    let songs = library.songs().await.unwrap();

    assert_eq!(songs.len(), 250);
    let ids = keys(songs.songs());
    assert_eq!(ids[0], "i.s000");
    assert_eq!(ids[100], "i.s100");
    assert_eq!(ids[249], "i.s249");
    let mut offsets: Vec<String> = fixtures
        .seen_for("GET", "/v1/me/library/songs")
        .into_iter()
        .map(|request| request.target)
        .collect();
    offsets.sort();
    assert_eq!(offsets.len(), 3);
    assert!(offsets.iter().any(|target| target.contains("offset=100")));
    assert!(offsets.iter().any(|target| target.contains("offset=200")));
}

#[tokio::test]
async fn the_song_list_is_kept_for_ten_minutes_then_served_while_it_is_rebuilt() {
    let fixtures = FixtureServer::start().await;
    serve_library(&fixtures);
    let clock = ManualClock::new();
    let library = signed_in(&fixtures).with_clock(clock.clone());

    assert_eq!(library.songs().await.unwrap().len(), 6);
    clock.advance(TTL - Duration::from_secs(1));
    library.songs().await.unwrap();
    assert_eq!(library_page_reads(&fixtures), 2);

    // Expired, and the library has changed: the old list is returned at once, the new
    // one is read in the background.
    clock.advance(Duration::from_secs(1));
    fixtures
        .on_target(
            "GET",
            LIBRARY_PAGE_1,
            fixture("union_library_songs_page2.json"),
        )
        .delay("GET", "/v1/me/library/songs", Duration::from_millis(200));
    assert_eq!(library.songs().await.unwrap().len(), 6);
    let listed = key("i.lib2");
    assert_eq!(library.track(&listed).await.unwrap().title, "Second Wind");
    assert!(fixtures
        .seen_for("GET", "/v1/me/library/songs")
        .iter()
        .all(|request| !request.target.contains("ids=i.lib2")));
    // Asking again meanwhile does not start another read.
    assert_eq!(library.songs().await.unwrap().len(), 6);
    until(|| library_page_reads(&fixtures) >= 3).await;
    let mut first_song = String::new();
    for _ in 0..200 {
        first_song = song_keys(&library).await[0].clone();
        if first_song != "i.lib1" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(first_song, "i.lib3");
    assert_eq!(library_page_reads(&fixtures), 3);

    // The rebuilt list counts its age from its own build.
    clock.advance(TTL / 2);
    library.songs().await.unwrap();
    assert_eq!(library_page_reads(&fixtures), 3);
}

#[tokio::test]
async fn a_write_during_the_first_build_retires_that_build() {
    let fixtures = FixtureServer::start().await;
    serve_library(&fixtures);
    fixtures
        .on(
            "POST",
            "/v1/me/library/playlists",
            Reply::fixture_status(201, "playlist_created.json"),
        )
        .delay("GET", "/v1/me/library/songs", Duration::from_millis(300));
    let library = signed_in(&fixtures);

    let (first, ()) = tokio::join!(library.songs(), async {
        seen_request(&fixtures, "GET", "/v1/me/library/songs").await;
        library
            .create_playlist("Mix", &[key("catalog:8001")])
            .await
            .unwrap();
    });
    first.unwrap();

    // The list built meanwhile may lack the song, so it was not kept.
    library.songs().await.unwrap();
    assert_eq!(library_page_reads(&fixtures), 4);
}

#[tokio::test]
async fn a_song_is_mapped_from_apple_or_taken_from_the_list() {
    let fixtures = FixtureServer::start().await;
    serve_library(&fixtures);
    fixtures
        .on(
            "GET",
            "/v1/catalog/it/songs",
            Reply::fixture("catalog_songs.json"),
        )
        .on_target(
            "GET",
            "/v1/me/library/songs?ids=i.lib1&include=albums,artists",
            fixture("library_song_single.json"),
        )
        .on_target(
            "GET",
            "/v1/me/library/songs?ids=i.gone&include=albums,artists",
            Reply::fixture_status(404, "error_not_found.json"),
        );
    let library = signed_in(&fixtures);

    let song = library.track(&key("i.lib1")).await.unwrap();
    assert_eq!(song.key, key("i.lib1"));
    assert_eq!(song.title, "First Light");
    assert_eq!(song.album.as_deref(), Some("Dawn Album"));
    assert_eq!(song.artist.as_deref(), Some("Artist One"));
    assert_eq!(song.album_id.as_deref(), Some("l.alb1"));
    assert_eq!(song.artist_id.as_deref(), Some("r.art1"));
    assert_eq!(song.track_number, Some(1));
    assert_eq!(song.disc_number, Some(1));
    assert_eq!(song.year, Some(2020));
    assert_eq!(song.genre.as_deref(), Some("Ambient"));
    assert_eq!(song.duration, Some(Duration::from_millis(245_600)));
    assert!(song.in_library);
    assert_eq!(song.catalog_id.as_deref(), Some("1001"));
    let artwork = song.artwork.unwrap();
    assert_eq!(
        artwork.url(300),
        format!("{}/dawn/300x300bb.jpg", "http://artwork.invalid")
    );
    assert_eq!(
        fixtures.seen_for("GET", "/v1/me/library/songs")[0].target,
        "/v1/me/library/songs?ids=i.lib1&include=albums,artists"
    );

    let catalog = library.track(&key("catalog:617154366")).await.unwrap();
    assert_eq!(catalog.title, "Get Lucky");
    assert_eq!(catalog.track_number, Some(8));
    assert_eq!(catalog.year, Some(2013));
    assert_eq!(catalog.duration, Some(Duration::from_millis(369_629)));
    assert!(!catalog.in_library);
    assert_eq!(catalog.album_id.as_deref(), Some("617154241"));
    assert_eq!(catalog.artist_id.as_deref(), Some("5468295"));
    let lookup = &fixtures.seen_for("GET", "/v1/catalog/it/songs")[0];
    assert_eq!(lookup.target, "/v1/catalog/it/songs?ids=617154366");
    // The catalog call needs only the developer token.
    assert_eq!(lookup.user_token, None);

    // Songs of the list need no lookup.
    library.songs().await.unwrap();
    let before = fixtures.seen().len();
    assert_eq!(
        library.track(&key("catalog:3001")).await.unwrap().title,
        "Loved Elsewhere"
    );
    assert_eq!(fixtures.seen().len(), before);
    assert_not_found(library.track(&key("i.gone")).await);
}

// Likes ------------------------------------------------------------------

#[tokio::test]
async fn likes_are_read_in_batches_of_100_per_kind_and_kept() {
    let fixtures = FixtureServer::start().await;
    fixtures
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
            "/v1/me/ratings/songs",
            fixture("likes_catalog_songs.json"),
        );
    let library = signed_in(&fixtures);
    let mut asked: Vec<TrackKey> = (0..150).map(|n| key(&format!("i.t{n}"))).collect();
    asked.push(key("catalog:3001"));
    asked.push(key("catalog:3002"));
    // Asked twice, listed once.
    asked.push(key("i.t0"));

    let liked = library.liked(&asked).await.unwrap();

    let mut likes: Vec<String> = liked
        .iter()
        .filter(|(_, liked)| **liked)
        .map(|(key, _)| key.to_string())
        .collect();
    likes.sort();
    assert_eq!(likes, ["catalog:3001", "i.t0", "i.t120"]);
    assert_eq!(liked.len(), 152);
    let mut batches: Vec<usize> = fixtures
        .seen_for("GET", "/v1/me/ratings/library-songs")
        .iter()
        .map(id_count)
        .collect();
    batches.sort_unstable();
    assert_eq!(batches, [50, 100]);
    // Catalog songs are read as catalog songs, not as library ones.
    assert_eq!(
        fixtures.seen_for("GET", "/v1/me/ratings/songs")[0].target,
        "/v1/me/ratings/songs?ids=3001,3002"
    );

    // Kept: asking again reads nothing, and only the unread are read.
    library.liked(&asked).await.unwrap();
    library
        .liked(&[key("i.t120"), key("i.t200")])
        .await
        .unwrap();
    assert_eq!(
        fixtures
            .seen_for("GET", "/v1/me/ratings/library-songs")
            .len(),
        3
    );
    assert_eq!(fixtures.seen_for("GET", "/v1/me/ratings/songs").len(), 1);
}

#[tokio::test]
async fn unrated_songs_read_as_not_liked_however_apple_answers() {
    let fixtures = FixtureServer::start().await;
    fixtures
        // Apple answers the ratings of songs never rated with an error message...
        .on(
            "GET",
            "/v1/me/ratings/songs",
            Reply::json(json!({"message": "An unexpected error occurred"})),
        )
        // ...and those of songs whose rating was removed with no data.
        .on(
            "GET",
            "/v1/me/ratings/library-songs",
            Reply::json(json!({"data": []})),
        );
    let library = signed_in(&fixtures);

    let liked = library
        .liked(&[key("catalog:3001"), key("i.lib1")])
        .await
        .unwrap();
    assert_eq!(liked.len(), 2);
    assert!(liked.values().all(|liked| !liked));

    // A 404 is the same, and a rating Apple lists without a value is no like.
    let other = FixtureServer::start().await;
    other
        .on(
            "GET",
            "/v1/me/ratings/songs",
            Reply::fixture_status(404, "error_not_found.json"),
        )
        .on(
            "GET",
            "/v1/me/ratings/library-songs",
            Reply::json(json!({"data": [{"id": "i.lib1", "type": "ratings", "attributes": {}}]})),
        );
    let library = signed_in(&other);
    let liked = library
        .liked(&[key("catalog:3001"), key("i.lib1")])
        .await
        .unwrap();
    assert!(liked.values().all(|liked| !liked));

    // Any other failure is not hidden.
    let failing = FixtureServer::start().await;
    failing.on(
        "GET",
        "/v1/me/ratings/songs",
        Reply::fixture_status(400, "error_not_found.json"),
    );
    let result = signed_in(&failing).liked(&[key("catalog:3001")]).await;
    assert!(matches!(result, Err(LibraryError::Apple(_))), "{result:?}");
}

#[tokio::test]
async fn set_liked_puts_or_deletes_the_rating_and_keeps_it() {
    let fixtures = FixtureServer::start().await;
    fixtures
        .on(
            "PUT",
            "/v1/me/ratings/library-songs/i.lib1",
            Reply::fixture("rating_put_library_song.json"),
        )
        .on(
            "PUT",
            "/v1/me/ratings/songs/4001",
            Reply::fixture("rating_put_catalog_song.json"),
        )
        .on("DELETE", "/v1/me/ratings/songs/3001", Reply::status(204))
        .on(
            "DELETE",
            "/v1/me/ratings/library-songs/i.lib2",
            Reply::status(204),
        )
        // Apple has no rating to remove.
        .on(
            "DELETE",
            "/v1/me/ratings/library-songs/i.unrated",
            Reply::fixture_status(404, "error_not_found.json"),
        );
    let library = signed_in(&fixtures);

    library.set_liked(&key("i.lib1"), true).await.unwrap();
    library.set_liked(&key("catalog:4001"), true).await.unwrap();
    library
        .set_liked(&key("catalog:3001"), false)
        .await
        .unwrap();
    library.set_liked(&key("i.lib2"), false).await.unwrap();
    library.set_liked(&key("i.unrated"), false).await.unwrap();

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

    // The kept ratings answer: no reads.
    let liked = library
        .liked(&[
            key("i.lib1"),
            key("catalog:4001"),
            key("catalog:3001"),
            key("i.lib2"),
            key("i.unrated"),
        ])
        .await
        .unwrap();
    let likes: Vec<bool> = [
        "i.lib1",
        "catalog:4001",
        "catalog:3001",
        "i.lib2",
        "i.unrated",
    ]
    .iter()
    .map(|text| liked[&key(text)])
    .collect();
    assert_eq!(likes, [true, true, false, false, false]);
    assert!(fixtures.seen_for("GET", "/v1/me/ratings/songs").is_empty());
    assert!(fixtures
        .seen_for("GET", "/v1/me/ratings/library-songs")
        .is_empty());
}

// Browsing ---------------------------------------------------------------

#[tokio::test]
async fn artists_artist_and_album_browse_the_library() {
    let fixtures = FixtureServer::start().await;
    fixtures
        .on(
            "GET",
            "/v1/me/library/artists",
            fixture("browse_artists_page1.json"),
        )
        .on(
            "GET",
            "/v1/me/library/artists",
            fixture("browse_artists_page2.json"),
        )
        .on(
            "GET",
            "/v1/me/library/artists/r.art1",
            fixture("browse_artist.json"),
        )
        .on(
            "GET",
            "/v1/me/library/artists/r.art1/albums",
            fixture("browse_artist_albums.json"),
        )
        .on(
            "GET",
            "/v1/me/library/albums/l.alb1",
            fixture("browse_album.json"),
        )
        .on(
            "GET",
            "/v1/me/library/albums/l.alb1/tracks",
            fixture("browse_album_tracks.json"),
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
        );
    let library = signed_in(&fixtures);

    let artists = library.artists().await.unwrap();
    let names: Vec<(&str, &str)> = artists
        .iter()
        .map(|artist| (artist.id.as_str(), artist.name.as_str()))
        .collect();
    assert_eq!(names, [("r.art1", "Artist One"), ("r.art2", "Artist Two")]);
    assert_eq!(
        fixtures.seen_for("GET", "/v1/me/library/artists")[1].target,
        "/v1/me/library/artists?offset=1&limit=100"
    );

    let artist = library.artist("r.art1").await.unwrap();
    assert_eq!(artist.artist.name, "Artist One");
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

    let detail = library.album("l.alb1").await.unwrap();
    let album = &detail.album;
    assert_eq!(album.name, "Dawn Album");
    assert_eq!(album.artist.as_deref(), Some("Artist One"));
    assert_eq!(album.artist_id.as_deref(), Some("r.art1"));
    assert_eq!(album.year, Some(2020));
    assert_eq!(album.track_count, Some(3));
    assert_eq!(keys(&detail.tracks), ["i.lib1", "i.lib2", "i.lib4"]);
    assert!(detail
        .tracks
        .iter()
        .all(|song| song.album_id.as_deref() == Some("l.alb1")));
    assert_eq!(detail.tracks[0].artist_id.as_deref(), Some("r.art1"));
    // A guest's track names another artist than the album's.
    assert_eq!(detail.tracks[2].artist_id, None);
    // Without artwork of its own, a track shows the album's.
    assert_eq!(detail.tracks[2].artwork, album.artwork);
    assert_eq!(
        fixtures.seen_for("GET", "/v1/me/library/albums/l.alb1")[0].target,
        "/v1/me/library/albums/l.alb1?include=artists"
    );

    assert_not_found(library.album("l.missing").await);
    assert_not_found(library.album("catalog:1").await);
    assert_not_found(library.artist("").await);
    assert_not_found(library.playlist_tracks("catalog:pl.1").await);
}

#[tokio::test]
async fn albums_list_recently_added_first_and_stay_complete_without_them() {
    let fixtures = FixtureServer::start().await;
    fixtures
        .on(
            "GET",
            "/v1/me/library/recently-added",
            fixture("browse_recently_added.json"),
        )
        .on(
            "GET",
            "/v1/me/library/albums",
            fixture("browse_albums.json"),
        );
    let clock = ManualClock::new();
    let library = signed_in(&fixtures).with_clock(clock.clone());

    let albums = library.albums().await.unwrap();
    let listed: Vec<&str> = albums.iter().map(|album| album.id.as_str()).collect();
    assert_eq!(listed, ["l.alb2", "l.alb1", "l.alb3"]);
    assert_eq!(albums[0].artist_id.as_deref(), Some("r.art2"));
    assert_eq!(albums[0].track_count, Some(1));
    assert_eq!(
        fixtures.seen_for("GET", "/v1/me/library/albums")[0].target,
        "/v1/me/library/albums?limit=100&include=artists"
    );
    assert_eq!(
        fixtures.seen_for("GET", "/v1/me/library/recently-added")[0].target,
        "/v1/me/library/recently-added?limit=25"
    );
    library.albums().await.unwrap();
    assert_eq!(fixtures.seen_for("GET", "/v1/me/library/albums").len(), 1);
    // Old: served at once, refreshed in the background.
    clock.advance(TTL);
    assert_eq!(library.albums().await.unwrap().len(), 3);
    until(|| fixtures.seen_for("GET", "/v1/me/library/albums").len() == 2).await;

    let without = FixtureServer::start().await;
    without
        .on(
            "GET",
            "/v1/me/library/recently-added",
            Reply::fixture_status(503, "error_server.json"),
        )
        .on(
            "GET",
            "/v1/me/library/albums",
            fixture("browse_albums.json"),
        );
    let albums = signed_in(&without).albums().await.unwrap();
    let listed: Vec<&str> = albums.iter().map(|album| album.id.as_str()).collect();
    assert_eq!(listed, ["l.alb1", "l.alb2", "l.alb3"]);
}

// Playlists --------------------------------------------------------------

#[tokio::test]
async fn playlists_carry_how_far_they_can_be_edited_and_list_new_ones_at_once() {
    let fixtures = FixtureServer::start().await;
    serve_library(&fixtures);
    fixtures
        .on(
            "POST",
            "/v1/me/library/playlists",
            Reply::fixture_status(201, "playlist_created.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists/p.mine1/tracks",
            fixture("union_playlist_mine_tracks.json"),
        );
    let library = signed_in(&fixtures);

    let playlists = library.playlists().await.unwrap();
    let edits: Vec<(&str, bool)> = playlists
        .iter()
        .map(|playlist| (playlist.id.as_str(), playlist.can_append))
        .collect();
    assert_eq!(
        edits,
        [
            ("p.mine1", true),
            ("p.favorites", false),
            ("p.editorial", false),
            ("p.empty", true),
            ("p.public", true),
        ]
    );
    assert_eq!(playlists[0].name, "Road Trip");
    assert_eq!(playlists[0].is_public, Some(false));
    assert_eq!(
        playlists[0].artwork.as_ref().unwrap().url(500),
        "http://artwork.invalid/roadtrip/500x500bb.jpg"
    );
    assert!(playlists[1].artwork.is_none());

    assert_eq!(
        keys(&library.playlist_tracks("p.mine1").await.unwrap()),
        ["i.lib2", "i.pl1", "catalog:1001"]
    );
    // Apple answers 404 for the tracks of an empty playlist.
    assert!(library.playlist_tracks("p.empty").await.unwrap().is_empty());

    // Listed before Apple lists it, so a caller finds what it created.
    let created = library
        .create_playlist("Mix", &[key("i.lib1"), key("catalog:3001")])
        .await
        .unwrap();
    assert_eq!(created.id, "p.RB1AAkGsv74Zkl");
    assert!(created.can_append);
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
    let after = library.playlists().await.unwrap();
    assert_eq!(after.len(), 6);
    assert_eq!(after.last().unwrap().id, "p.RB1AAkGsv74Zkl");
}

#[tokio::test]
async fn appending_to_a_playlist_that_cannot_be_edited_is_refused_before_the_write() {
    let fixtures = FixtureServer::start().await;
    fixtures
        .on(
            "GET",
            "/v1/me/library/playlists/p.mine1",
            fixture("library_playlist_editable.json"),
        )
        .on(
            "GET",
            "/v1/me/library/playlists/p.favorites",
            fixture("library_playlist_favorites.json"),
        )
        .on(
            "POST",
            "/v1/me/library/playlists/p.mine1/tracks",
            Reply::status(204),
        )
        // What Apple would answer to the write: a 403, which would read as a sign-in
        // that expired if the write were sent.
        .on(
            "POST",
            "/v1/me/library/playlists/p.favorites/tracks",
            Reply::fixture_status(403, "error_forbidden.json"),
        )
        .on(
            "GET",
            "/v1/me/storefront",
            Reply::fixture_status(401, "error_unauthorized.json"),
        );
    let library = signed_in(&fixtures);

    let refused = library
        .append_to_playlist("p.favorites", &[key("i.lib1")])
        .await;
    assert!(
        matches!(refused, Err(LibraryError::ReadOnlyPlaylist)),
        "{refused:?}"
    );
    assert!(fixtures
        .seen_for("POST", "/v1/me/library/playlists/p.favorites/tracks")
        .is_empty());

    library
        .append_to_playlist("p.mine1", &[key("i.lib3"), key("catalog:4001")])
        .await
        .unwrap();
    assert_eq!(
        body_json(&fixtures.seen_for("POST", "/v1/me/library/playlists/p.mine1/tracks")[0].body),
        json!({"data": [
            {"id": "i.lib3", "type": "library-songs"},
            {"id": "4001", "type": "songs"}
        ]})
    );
    // Nothing to add, nothing sent.
    library.append_to_playlist("p.mine1", &[]).await.unwrap();
    assert_eq!(
        fixtures
            .seen_for("POST", "/v1/me/library/playlists/p.mine1/tracks")
            .len(),
        1
    );
    assert_not_found(
        library
            .append_to_playlist("catalog:pl.1", &[key("i.lib1")])
            .await,
    );
}

#[tokio::test]
async fn writes_patch_the_song_list_without_a_rebuild() {
    let fixtures = FixtureServer::start().await;
    serve_library(&fixtures);
    fixtures
        .on(
            "GET",
            "/v1/me/library/playlists/p.mine1",
            fixture("library_playlist_editable.json"),
        )
        .on(
            "POST",
            "/v1/me/library/playlists/p.mine1/tracks",
            Reply::status(204),
        )
        .on(
            "POST",
            "/v1/me/library/playlists",
            Reply::fixture_status(201, "playlist_created.json"),
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
        );
    let library = signed_in(&fixtures);
    assert_eq!(song_keys(&library).await, UNION);

    // i.lib1 is listed, and 1001 is its catalog id: only 8001 is new.
    library
        .append_to_playlist(
            "p.mine1",
            &[key("catalog:8001"), key("i.lib1"), key("catalog:1001")],
        )
        .await
        .unwrap();
    library
        .create_playlist("Mix", &[key("i.new1"), key("catalog:8001")])
        .await
        .unwrap();

    let keys = song_keys(&library).await;
    assert_eq!(keys[..6], UNION);
    assert_eq!(keys[6..], ["catalog:8001", "i.new1"]);
    assert_eq!(library_page_reads(&fixtures), 2);
    assert_eq!(fixtures.seen_for("GET", "/v1/catalog/it/songs").len(), 1);
}

#[tokio::test]
async fn liking_a_catalog_song_updates_its_library_copy_and_joins_the_song_list() {
    let fixtures = FixtureServer::start().await;
    serve_library(&fixtures);
    fixtures
        .on(
            "PUT",
            "/v1/me/ratings/songs/1001",
            Reply::fixture("rating_put_catalog_song.json"),
        )
        .on(
            "PUT",
            "/v1/me/ratings/songs/8001",
            Reply::fixture("rating_put_catalog_song.json"),
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
        );
    let library = signed_in(&fixtures);
    assert_eq!(song_keys(&library).await, UNION);

    // i.lib1 has catalog id 1001.
    library.set_liked(&key("catalog:1001"), true).await.unwrap();
    library.set_liked(&key("catalog:8001"), true).await.unwrap();

    let liked = library
        .liked(&[key("i.lib1"), key("catalog:1001"), key("catalog:8001")])
        .await
        .unwrap();
    assert!(liked.values().all(|liked| *liked));
    assert!(fixtures
        .seen_for("GET", "/v1/me/ratings/library-songs")
        .is_empty());
    assert!(fixtures.seen_for("GET", "/v1/me/ratings/songs").is_empty());
    // The liked catalog song joins the list, once.
    let keys = song_keys(&library).await;
    assert_eq!(keys[..6], UNION);
    assert_eq!(keys[6..], ["catalog:8001"]);
}
