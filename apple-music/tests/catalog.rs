//! The catalog layer of the library against the fixture server.
//!
//! The `catalog_*` and `library_search_*` fixtures are hand-written in the shape of Apple's
//! documented responses (Search for Catalog Resources, Get a Catalog Album, Artist or
//! Playlist), not recorded from Apple.

mod common;

use std::sync::Arc;
use std::time::Duration;

use apple_music::library::{
    Additions, AlbumItem, AppleLibrary, ArtistItem, Candidates, ItemId, LibraryError, PlaylistItem,
    RecommendationKind, SearchScope, Tokens, Track, DEFAULT_RECOMMENDATIONS,
};
use common::{
    assert_not_found, fast_retry, fixture, key, keys, until, FixtureServer, ManualClock, Reply,
    DETAIL_TTL, DEVELOPER_TOKEN, TTL, USER_TOKEN,
};
use serde_json::json;

const LIBRARY_PAGE_1: &str = "/v1/me/library/songs?limit=100&include=albums,artists";
const LIBRARY_PAGE_2: &str = "/v1/me/library/songs?offset=2&limit=100&include=albums,artists";

fn signed_in(fixtures: &FixtureServer) -> (AppleLibrary, Arc<ManualClock>) {
    let base_url = fixtures.base_url.clone();
    let clock = ManualClock::new();
    let library = AppleLibrary::with_client(|builder| {
        builder
            .base_url(base_url)
            .storefront("it")
            .retry_policy(fast_retry())
    })
    .with_clock(clock.clone());
    library.sign_in(Tokens::new(DEVELOPER_TOKEN, USER_TOKEN).unwrap());
    (library, clock)
}

fn catalog(id: &str) -> ItemId {
    ItemId::catalog(id).unwrap()
}

fn library_item(id: &str) -> ItemId {
    ItemId::library(id).unwrap()
}

fn marks(tracks: &[Track]) -> Vec<(String, bool)> {
    tracks
        .iter()
        .map(|track| (track.key.to_string(), track.in_library))
        .collect()
}

fn album_marks(albums: &[AlbumItem]) -> Vec<(ItemId, bool)> {
    albums
        .iter()
        .map(|album| (album.id.clone(), album.in_library))
        .collect()
}

fn album_item(id: ItemId, name: &str, artist: &str) -> AlbumItem {
    AlbumItem {
        id,
        name: name.to_owned(),
        artist: Some(artist.to_owned()),
        artist_id: None,
        track_count: None,
        year: None,
        genre: None,
        artwork: None,
        in_library: false,
    }
}

fn artist_item(id: ItemId, name: &str) -> ArtistItem {
    ArtistItem {
        id,
        name: name.to_owned(),
        artwork: None,
        in_library: false,
    }
}

fn playlist_item(id: ItemId, name: &str) -> PlaylistItem {
    PlaylistItem {
        id,
        name: name.to_owned(),
        curator: None,
        can_append: false,
        artwork: None,
        in_library: false,
    }
}

/// Every catalog request carried the developer token only, and every request for the
/// user's own data the Music User Token as well.
fn assert_tokens(fixtures: &FixtureServer) {
    let seen = fixtures.seen();
    assert!(!seen.is_empty());
    for request in seen {
        let user = request.path.starts_with("/v1/me/");
        let catalog = request.path.starts_with("/v1/catalog/");
        assert!(user || catalog, "{request:?}");
        assert_eq!(
            request.user_token.is_some(),
            user,
            "{} {}",
            request.method,
            request.target
        );
        assert_eq!(
            request.authorization.as_deref(),
            Some(format!("Bearer {DEVELOPER_TOKEN}").as_str())
        );
    }
}

fn serve_search(fixtures: &FixtureServer) {
    fixtures
        .on(
            "GET",
            "/v1/me/library/search",
            fixture("library_search_dawn.json"),
        )
        .on(
            "GET",
            "/v1/catalog/it/search",
            fixture("catalog_search_dawn.json"),
        );
}

fn serve_catalog_items(fixtures: &FixtureServer) {
    fixtures
        .on(
            "GET",
            "/v1/catalog/it/albums/7002",
            fixture("catalog_album_sunrise.json"),
        )
        .on(
            "GET",
            "/v1/catalog/it/albums/7002/tracks",
            fixture("catalog_album_sunrise_tracks.json"),
        )
        .on(
            "GET",
            "/v1/catalog/it/artists/8002",
            fixture("catalog_artist_nine.json"),
        )
        .on(
            "GET",
            "/v1/catalog/it/artists/8002/albums",
            fixture("catalog_artist_nine_albums_page1.json"),
        )
        .on(
            "GET",
            "/v1/catalog/it/artists/8002/albums",
            fixture("catalog_artist_nine_albums_page2.json"),
        )
        .on(
            "GET",
            "/v1/catalog/it/playlists/pl.morning",
            fixture("catalog_playlist_morning.json"),
        )
        .on(
            "GET",
            "/v1/catalog/it/playlists/pl.morning/tracks",
            fixture("catalog_collection_tracks_page2.json"),
        )
        .on(
            "GET",
            "/v1/catalog/it/albums/404",
            Reply::fixture_status(404, "error_not_found.json"),
        )
        .on(
            "GET",
            "/v1/catalog/it/albums/404/tracks",
            Reply::fixture_status(404, "error_not_found.json"),
        )
        .on(
            "GET",
            "/v1/me/library/artists",
            Reply::json(json!({"data": [
                {"id": "r.art1", "type": "library-artists", "attributes": {"name": "Artist One"}},
                {"id": "r.art2", "type": "library-artists", "attributes": {"name": "Artist Two"}}
            ]})),
        );
}

/// The library's songs and albums, as the kept lists the catalog marks read.
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
        )
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
}

const UNION: [&str; 6] = [
    "i.lib1",
    "i.lib2",
    "i.lib3",
    "i.pl1",
    "catalog:3001",
    "catalog:4001",
];

fn library_page_reads(fixtures: &FixtureServer) -> usize {
    fixtures
        .seen_for("GET", "/v1/me/library/songs")
        .iter()
        .filter(|request| !request.target.contains("ids="))
        .count()
}

// Search ------------------------------------------------------------------

#[tokio::test]
async fn searching_everything_lists_the_library_first_then_new_catalog_items() {
    let fixtures = FixtureServer::start().await;
    serve_search(&fixtures);
    let (library, _) = signed_in(&fixtures);

    // A search box may append `*`; Apple's search has no wildcards.
    let found = library.search("dawn*", SearchScope::All).await.unwrap();

    // 1001 is i.lib1's catalog id; the album and the artist match by name.
    assert_eq!(
        marks(&found.songs),
        [
            ("i.lib1".to_owned(), true),
            ("catalog:6001".to_owned(), false)
        ]
    );
    assert_eq!(
        album_marks(&found.albums),
        [(library_item("l.alb1"), true), (catalog("7002"), false)]
    );
    let artists: Vec<_> = found
        .artists
        .iter()
        .map(|artist| (artist.id.clone(), artist.in_library))
        .collect();
    assert_eq!(
        artists,
        [(library_item("r.art1"), true), (catalog("8002"), false)]
    );
    assert_eq!(found.playlists.len(), 1);
    assert_eq!(found.playlists[0].id, library_item("p.dawn"));
    assert!(found.playlists[0].in_library && found.playlists[0].can_append);

    let new_song = &found.songs[1];
    assert_eq!(new_song.title, "Dawn Chorus");
    // Taken from the song's relationships, as Apple sent them.
    assert_eq!(new_song.album_id.as_deref(), Some("7002"));
    assert_eq!(new_song.artist_id.as_deref(), Some("8002"));
    assert_eq!(found.albums[1].artist.as_deref(), Some("Artist Nine"));
    assert!(found.artists[1].artwork.is_some());

    let searches = fixtures.seen_for("GET", "/v1/catalog/it/search");
    assert_eq!(searches.len(), 1);
    assert_eq!(
        searches[0].target,
        "/v1/catalog/it/search?term=dawn&types=songs,albums,artists&limit=25&offset=0"
    );
    let library_searches = fixtures.seen_for("GET", "/v1/me/library/search");
    assert_eq!(library_searches.len(), 1);
    assert!(library_searches[0].target.starts_with(
        "/v1/me/library/search?term=dawn&types=library-songs,library-albums,library-artists,library-playlists&limit=25"
    ));
    assert_tokens(&fixtures);
}

#[tokio::test]
async fn searching_the_catalog_marks_what_the_library_holds() {
    let fixtures = FixtureServer::start().await;
    serve_search(&fixtures);
    let (library, _) = signed_in(&fixtures);

    let found = library.search("dawn", SearchScope::Catalog).await.unwrap();
    assert_eq!(
        marks(&found.songs),
        [
            ("catalog:1001".to_owned(), true),
            ("catalog:6001".to_owned(), false)
        ]
    );
    assert_eq!(
        album_marks(&found.albums),
        [(catalog("7001"), true), (catalog("7002"), false)]
    );
    let artists: Vec<_> = found
        .artists
        .iter()
        .map(|artist| (artist.id.clone(), artist.in_library))
        .collect();
    assert_eq!(artists, [(catalog("8001"), true), (catalog("8002"), false)]);
    assert!(found.playlists.is_empty());

    // A library search lists the library only, and asks the catalog nothing.
    let own = library.search("dawn", SearchScope::Library).await.unwrap();
    assert_eq!(keys(&own.songs), ["i.lib1"]);
    assert_eq!(own.albums.len(), 1);
    assert_eq!(own.playlists.len(), 1);
    assert_eq!(fixtures.seen_for("GET", "/v1/catalog/it/search").len(), 1);
}

#[tokio::test]
async fn an_empty_search_finds_nothing_and_asks_nobody() {
    let fixtures = FixtureServer::start().await;
    serve_search(&fixtures);
    let (library, _) = signed_in(&fixtures);

    for query in ["", "  ", "\"\"", "*"] {
        for scope in [SearchScope::Library, SearchScope::Catalog, SearchScope::All] {
            let found = library.search(query, scope).await.unwrap();
            assert!(
                found.songs.is_empty() && found.albums.is_empty(),
                "{query:?}"
            );
        }
    }
    assert!(fixtures.seen().is_empty());
}

#[tokio::test]
async fn a_failing_catalog_leaves_the_library_matches() {
    for (status, body) in [(500, "error_server.json"), (401, "error_unauthorized.json")] {
        let fixtures = FixtureServer::start().await;
        fixtures
            .on(
                "GET",
                "/v1/me/library/search",
                fixture("library_search_dawn.json"),
            )
            .on(
                "GET",
                "/v1/catalog/it/search",
                Reply::fixture_status(status, body),
            )
            .on(
                "GET",
                "/v1/me/storefront",
                Reply::fixture("storefront.json"),
            );
        let (library, _) = signed_in(&fixtures);

        let found = library.search("dawn", SearchScope::All).await.unwrap();
        assert_eq!(keys(&found.songs), ["i.lib1"], "{status}");
        assert_eq!(found.albums.len(), 1, "{status}");
        assert_eq!(found.artists.len(), 1, "{status}");
        assert_eq!(found.playlists.len(), 1, "{status}");

        // The catalog alone fails as Apple did.
        let error = library
            .search("dawn", SearchScope::Catalog)
            .await
            .unwrap_err();
        if status == 500 {
            assert!(matches!(error, LibraryError::Unavailable(_)), "{error:?}");
        } else {
            // The tokens work, so Apple refused just this request.
            assert!(matches!(error, LibraryError::Refused), "{error:?}");
        }
    }
}

#[tokio::test]
async fn a_catalog_that_finds_nothing_is_empty() {
    let fixtures = FixtureServer::start().await;
    fixtures
        .on(
            "GET",
            "/v1/me/library/search",
            fixture("library_search_dawn.json"),
        )
        .on(
            "GET",
            "/v1/catalog/it/search",
            Reply::fixture_status(404, "error_not_found.json"),
        );
    let (library, _) = signed_in(&fixtures);

    let catalog = library.search("dawn", SearchScope::Catalog).await.unwrap();
    assert!(catalog.songs.is_empty() && catalog.albums.is_empty() && catalog.artists.is_empty());
    let all = library.search("dawn", SearchScope::All).await.unwrap();
    assert_eq!(keys(&all.songs), ["i.lib1"]);
}

// Adding ------------------------------------------------------------------

fn serve_additions(fixtures: &FixtureServer) {
    serve_library(fixtures);
    serve_search(fixtures);
    serve_catalog_items(fixtures);
    fixtures.on("POST", "/v1/me/library", Reply::status(202));
    serve_song_6001(fixtures);
}

fn serve_song_6001(fixtures: &FixtureServer) {
    fixtures.on_target(
        "GET",
        "/v1/catalog/it/songs?ids=6001",
        Reply::json(json!({"data": [{
            "id": "6001", "type": "songs",
            "attributes": {"name": "Dawn Chorus", "artistName": "Artist Nine",
                "durationInMillis": 200000, "playParams": {"id": "6001", "kind": "song"}}
        }]})),
    );
}

#[tokio::test]
async fn additions_post_one_request_and_show_at_once() {
    let fixtures = FixtureServer::start().await;
    serve_additions(&fixtures);
    let (library, _) = signed_in(&fixtures);
    assert_eq!(library.songs().await.unwrap().len(), 6);
    library.albums().await.unwrap();

    library
        .add_to_library(Additions {
            songs: &[key("catalog:6001"), key("i.lib1")],
            albums: &[catalog("7002"), library_item("l.alb1")],
            playlists: &[catalog("pl.morning")],
        })
        .await
        .unwrap();
    let posts = fixtures.seen_for("POST", "/v1/me/library");
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0].target,
        "/v1/me/library?ids[songs]=6001&ids[albums]=7002&ids[playlists]=pl.morning"
    );
    assert_eq!(posts[0].user_token.as_deref(), Some(USER_TOKEN));

    // The kept song list takes the songs, and those of the album, in without a rebuild.
    let songs = library.songs().await.unwrap();
    assert_eq!(
        keys(songs.songs()),
        [&UNION[..], &["catalog:6001", "catalog:6002"]].concat()
    );
    assert!(songs.songs()[6..].iter().all(|song| song.in_library));
    assert_eq!(library_page_reads(&fixtures), 2);
    // The album heads the kept album list, listed under its catalog id.
    let albums = library.albums().await.unwrap();
    assert_eq!(albums.len(), 4);
    assert_eq!(albums[0].id, "catalog:7002");
    assert_eq!(albums[0].catalog_id.as_deref(), Some("7002"));
    assert_eq!(fixtures.seen_for("GET", "/v1/me/library/albums").len(), 1);

    // The catalog shows them in the library before Apple lists them.
    let found = library.search("dawn", SearchScope::Catalog).await.unwrap();
    assert!(found.songs[1].in_library);
    assert!(found.albums[1].in_library);
    let playlist = library
        .catalog_playlist(&catalog("pl.morning"))
        .await
        .unwrap();
    assert!(playlist.playlist.in_library);
    let album = library.catalog_album(&catalog("7002")).await.unwrap();
    assert!(album.album.in_library && album.tracks.iter().all(|song| song.in_library));
    // Searching everything keeps them listed, in the library; what duplicates a library
    // copy is left out.
    let all = library.search("dawn", SearchScope::All).await.unwrap();
    assert_eq!(
        marks(&all.songs),
        [
            ("i.lib1".to_owned(), true),
            ("catalog:6001".to_owned(), true)
        ]
    );
    assert_eq!(
        album_marks(&all.albums),
        [(library_item("l.alb1"), true), (catalog("7002"), true)]
    );

    // Library items need no request.
    library
        .add_to_library(Additions {
            songs: &[key("i.lib1")],
            albums: &[library_item("l.alb1")],
            playlists: &[],
        })
        .await
        .unwrap();
    library
        .add_to_library(Additions {
            songs: &[],
            albums: &[],
            playlists: &[],
        })
        .await
        .unwrap();
    assert_eq!(fixtures.seen_for("POST", "/v1/me/library").len(), 1);
    assert_tokens(&fixtures);
}

#[tokio::test]
async fn an_added_album_counts_its_songs_in_without_anything_kept() {
    let fixtures = FixtureServer::start().await;
    serve_additions(&fixtures);
    let (library, _) = signed_in(&fixtures);

    library
        .add_to_library(Additions {
            songs: &[],
            albums: &[catalog("7002")],
            playlists: &[],
        })
        .await
        .unwrap();

    let found = library.search("dawn", SearchScope::Catalog).await.unwrap();
    // 6001 is a song of album 7002, which the search did not name.
    assert_eq!(found.songs[1].key.to_string(), "catalog:6001");
    assert!(found.songs[1].in_library);
    assert!(found.albums[1].in_library);
    // Nothing was kept, so nothing is rebuilt.
    assert_eq!(library_page_reads(&fixtures), 0);
}

#[tokio::test]
async fn a_refused_addition_changes_nothing() {
    let fixtures = FixtureServer::start().await;
    serve_library(&fixtures);
    serve_search(&fixtures);
    serve_catalog_items(&fixtures);
    serve_song_6001(&fixtures);
    fixtures
        .on(
            "POST",
            "/v1/me/library",
            Reply::fixture_status(403, "error_forbidden.json"),
        )
        .on("GET", "/v1/me/storefront", fixture("storefront.json"));
    let (library, _) = signed_in(&fixtures);
    library.songs().await.unwrap();

    let error = library
        .add_to_library(Additions {
            songs: &[key("catalog:6001")],
            albums: &[catalog("7002")],
            playlists: &[],
        })
        .await
        .unwrap_err();
    assert!(matches!(error, LibraryError::Refused), "{error:?}");
    let found = library.search("dawn", SearchScope::Catalog).await.unwrap();
    assert!(!found.songs[1].in_library);
    assert!(!found.albums[1].in_library);
    assert_eq!(library.songs().await.unwrap().len(), 6);
}

// Membership --------------------------------------------------------------

#[tokio::test]
async fn membership_tells_what_the_library_holds() {
    let fixtures = FixtureServer::start().await;
    serve_additions(&fixtures);
    let (library, _) = signed_in(&fixtures);

    let songs = [key("i.lib1"), key("catalog:1001"), key("catalog:6001")];
    let albums = [
        // Named as the library's album is, apart from case and spacing.
        album_item(catalog("7001"), "Dawn  Album", "ARTIST ONE"),
        album_item(catalog("7002"), "Sunrise EP", "Artist Nine"),
        album_item(library_item("l.alb2"), "Night Album", "Artist Two"),
    ];
    let artists = [
        artist_item(catalog("8001"), "artist one"),
        artist_item(catalog("8002"), "Artist Nine"),
        artist_item(library_item("r.art1"), "Artist One"),
    ];
    let playlists = [
        playlist_item(catalog("pl.morning"), "Morning Mix"),
        playlist_item(library_item("p.mine1"), "Mine"),
    ];
    let candidates = Candidates {
        songs: &songs,
        albums: &albums,
        artists: &artists,
        playlists: &playlists,
    };

    let found = library.membership(candidates).await.unwrap();
    assert_eq!(
        found.songs,
        [key("i.lib1"), key("catalog:1001")].into_iter().collect()
    );
    assert_eq!(
        found.albums,
        [catalog("7001"), library_item("l.alb2")]
            .into_iter()
            .collect()
    );
    assert_eq!(
        found.artists,
        [catalog("8001"), library_item("r.art1")]
            .into_iter()
            .collect()
    );
    assert_eq!(
        found.playlists,
        [library_item("p.mine1")].into_iter().collect()
    );
    // What was not kept is read once, and kept.
    library.membership(candidates).await.unwrap();
    assert_eq!(library_page_reads(&fixtures), 2);
    assert_eq!(fixtures.seen_for("GET", "/v1/me/library/albums").len(), 1);

    // Added items join, the playlist among them.
    library
        .add_to_library(Additions {
            songs: &[key("catalog:6001")],
            albums: &[catalog("7002")],
            playlists: &[catalog("pl.morning")],
        })
        .await
        .unwrap();
    let found = library.membership(candidates).await.unwrap();
    assert!(found.songs.contains(&key("catalog:6001")));
    assert!(found.albums.contains(&catalog("7002")));
    assert!(found.playlists.contains(&catalog("pl.morning")));
    assert!(!found.artists.contains(&catalog("8002")));

    // Nothing catalog is asked about, so nothing is read.
    let before = fixtures.seen().len();
    let own = [key("i.lib1")];
    let found = library
        .membership(Candidates {
            songs: &own,
            ..Candidates::default()
        })
        .await
        .unwrap();
    assert!(found.songs.contains(&own[0]));
    assert_eq!(fixtures.seen().len(), before);
}

// Browsing ----------------------------------------------------------------

#[tokio::test]
async fn catalog_albums_open_by_their_ids_and_are_kept_for_thirty_seconds() {
    let fixtures = FixtureServer::start().await;
    serve_catalog_items(&fixtures);
    let (library, clock) = signed_in(&fixtures);

    let detail = library.catalog_album(&catalog("7002")).await.unwrap();
    let album = &detail.album;
    assert_eq!(album.id, catalog("7002"));
    assert_eq!(album.name, "Sunrise EP");
    assert_eq!(album.artist_id, Some(catalog("8002")));
    assert_eq!(album.year, Some(2021));
    assert!(!album.in_library);
    // The music video is left out.
    assert_eq!(keys(&detail.tracks), ["catalog:6001", "catalog:6002"]);
    assert!(detail.tracks.iter().all(|song| {
        song.album_id.as_deref() == Some("7002")
            && !song.in_library
            && song.artwork == album.artwork
    }));
    assert_eq!(detail.tracks[0].artist_id.as_deref(), Some("8002"));
    assert_eq!(detail.tracks[1].artist_id, None);
    assert_eq!(
        fixtures.seen_for("GET", "/v1/catalog/it/albums/7002")[0].target,
        "/v1/catalog/it/albums/7002?include=artists"
    );

    let reads = || {
        (
            fixtures.seen_for("GET", "/v1/catalog/it/albums/7002").len(),
            fixtures
                .seen_for("GET", "/v1/catalog/it/albums/7002/tracks")
                .len(),
        )
    };
    assert_eq!(reads(), (1, 1));
    assert_eq!(
        library.catalog_album(&catalog("7002")).await.unwrap(),
        detail
    );
    clock.advance(DETAIL_TTL - Duration::from_secs(1));
    library.catalog_album(&catalog("7002")).await.unwrap();
    assert_eq!(reads(), (1, 1));
    clock.advance(Duration::from_secs(1));
    library.catalog_album(&catalog("7002")).await.unwrap();
    assert_eq!(reads(), (2, 2));

    assert_not_found(library.catalog_album(&catalog("404")).await);
    // A library item names nothing in the catalog.
    assert_not_found(library.catalog_album(&library_item("l.alb1")).await);
    assert_not_found(library.catalog_artist(&library_item("r.art1")).await);
    assert_tokens(&fixtures);
}

#[tokio::test]
async fn catalog_artists_open_with_all_their_albums() {
    let fixtures = FixtureServer::start().await;
    serve_catalog_items(&fixtures);
    let (library, clock) = signed_in(&fixtures);

    let detail = library.catalog_artist(&catalog("8002")).await.unwrap();
    assert_eq!(detail.artist.name, "Artist Nine");
    assert!(!detail.artist.in_library);
    assert!(detail.artist.artwork.is_some());
    let ids: Vec<_> = detail.albums.iter().map(|album| album.id.clone()).collect();
    assert_eq!(ids, [catalog("7002"), catalog("7003")]);
    assert!(detail
        .albums
        .iter()
        .all(|album| album.artist_id == Some(catalog("8002"))));
    assert_eq!(
        fixtures
            .seen_for("GET", "/v1/catalog/it/artists/8002/albums")
            .len(),
        2
    );

    library.catalog_artist(&catalog("8002")).await.unwrap();
    assert_eq!(
        fixtures
            .seen_for("GET", "/v1/catalog/it/artists/8002")
            .len(),
        1
    );
    clock.advance(DETAIL_TTL);
    library.catalog_artist(&catalog("8002")).await.unwrap();
    assert_eq!(
        fixtures
            .seen_for("GET", "/v1/catalog/it/artists/8002")
            .len(),
        2
    );
    assert_tokens(&fixtures);
}

#[tokio::test]
async fn catalog_playlists_open_with_their_songs_and_mixes_with_the_user_token() {
    let fixtures = FixtureServer::start().await;
    serve_catalog_items(&fixtures);
    fixtures
        .on(
            "GET",
            "/v1/catalog/it/playlists/pl.pm-mix1",
            Reply::json(json!({"data": [{"id": "pl.pm-mix1", "type": "playlists",
                "attributes": {"name": "Mix One", "playlistType": "personal-mix"}}]})),
        )
        .on(
            "GET",
            "/v1/catalog/it/playlists/pl.pm-mix1/tracks",
            fixture("catalog_collection_tracks_page2.json"),
        );
    let (library, _) = signed_in(&fixtures);

    let playlist = library
        .catalog_playlist(&catalog("pl.morning"))
        .await
        .unwrap();
    assert_eq!(playlist.playlist.name, "Morning Mix");
    assert_eq!(playlist.playlist.curator.as_deref(), Some("Apple Music"));
    assert!(!playlist.playlist.can_append && !playlist.playlist.in_library);
    assert_eq!(keys(&playlist.tracks), ["catalog:t2"]);
    // Any other playlist needs only the developer token.
    for path in [
        "/v1/catalog/it/playlists/pl.morning",
        "/v1/catalog/it/playlists/pl.morning/tracks",
    ] {
        assert!(fixtures
            .seen_for("GET", path)
            .iter()
            .all(|request| request.user_token.is_none()));
    }

    let before = fixtures.seen().len();
    let mix = library
        .catalog_playlist(&catalog("pl.pm-mix1"))
        .await
        .unwrap();
    assert_eq!(mix.playlist.name, "Mix One");
    assert_eq!(keys(&mix.tracks), ["catalog:t2"]);
    for request in &fixtures.seen()[before..] {
        assert_eq!(
            request.user_token.as_deref(),
            Some(USER_TOKEN),
            "{request:?}"
        );
    }
    assert_not_found(library.catalog_playlist(&library_item("p.mine1")).await);
}

#[tokio::test]
async fn catalog_reads_use_the_developer_token_and_the_users_data_the_user_token() {
    let fixtures = FixtureServer::start().await;
    serve_additions(&fixtures);
    fixtures.on(
        "GET",
        "/v1/me/recommendations",
        fixture("recommendations_page2.json"),
    );
    let (library, _) = signed_in(&fixtures);
    library.songs().await.unwrap();
    library.search("dawn", SearchScope::All).await.unwrap();
    library.catalog_album(&catalog("7002")).await.unwrap();
    library.catalog_artist(&catalog("8002")).await.unwrap();
    library
        .catalog_playlist(&catalog("pl.morning"))
        .await
        .unwrap();
    library
        .recommendations(DEFAULT_RECOMMENDATIONS)
        .await
        .unwrap();
    library
        .add_to_library(Additions {
            songs: &[key("catalog:6001")],
            albums: &[catalog("7002")],
            playlists: &[],
        })
        .await
        .unwrap();

    assert_tokens(&fixtures);
    // The check is not vacuous: every kind of request was made.
    for path in [
        "/v1/catalog/it/search",
        "/v1/catalog/it/albums/7002",
        "/v1/catalog/it/albums/7002/tracks",
        "/v1/catalog/it/artists/8002",
        "/v1/catalog/it/artists/8002/albums",
        "/v1/catalog/it/playlists/pl.morning",
        "/v1/catalog/it/playlists/pl.morning/tracks",
        "/v1/catalog/it/songs",
        "/v1/me/library/search",
        "/v1/me/recommendations",
        "/v1/me/library",
    ] {
        assert!(
            fixtures.seen().iter().any(|request| request.path == path),
            "{path}"
        );
    }
}

// Recommendations ---------------------------------------------------------

#[tokio::test]
async fn recommendations_group_albums_and_playlists_and_are_kept() {
    let fixtures = FixtureServer::start().await;
    fixtures
        .on(
            "GET",
            "/v1/me/recommendations",
            fixture("recommendations_page1.json"),
        )
        .on(
            "GET",
            "/v1/me/recommendations",
            fixture("recommendations_page2.json"),
        )
        .on("POST", "/v1/me/library", Reply::status(202));
    let (library, _) = signed_in(&fixtures);

    let groups = library
        .recommendations(DEFAULT_RECOMMENDATIONS)
        .await
        .unwrap();
    // The group of stations only is left out, and so are the stations in the others.
    let titles: Vec<&str> = groups.iter().map(|group| group.title.as_str()).collect();
    assert_eq!(titles, ["Made for Someone", "Recently Played"]);
    let made = &groups[0];
    assert_eq!(made.id, "rec-1");
    assert_eq!(made.kind, RecommendationKind::Personal);
    assert!(made.albums.is_empty());
    assert_eq!(made.playlists.len(), 1);
    let mix = &made.playlists[0];
    assert_eq!(mix.id, catalog("pl.pm-mix1"));
    assert_eq!(mix.name, "Mix One");
    assert_eq!(mix.curator.as_deref(), Some("Curator"));
    assert!(!mix.can_append && !mix.in_library);
    assert!(mix.artwork.is_some());
    let recent = &groups[1];
    assert_eq!(recent.kind, RecommendationKind::Recent);
    assert_eq!(recent.albums[0].id, catalog("alb1"));
    assert_eq!(recent.albums[0].artist.as_deref(), Some("Artist"));
    assert!(!recent.albums[0].in_library);

    // The count limits the groups, and defaults to ten.
    assert_eq!(library.recommendations(1).await.unwrap().len(), 1);
    assert_eq!(library.recommendations(0).await.unwrap().len(), 0);
    assert_eq!(fixtures.seen_for("GET", "/v1/me/recommendations").len(), 2);
}

#[tokio::test]
async fn recommendations_are_marked_on_every_read_and_read_again_when_old() {
    let fixtures = FixtureServer::start().await;
    fixtures
        .on(
            "GET",
            "/v1/me/recommendations",
            fixture("recommendations_page1.json"),
        )
        .on(
            "GET",
            "/v1/me/recommendations",
            fixture("recommendations_page2.json"),
        )
        .on("POST", "/v1/me/library", Reply::status(202))
        .on(
            "GET",
            "/v1/catalog/it/albums/alb1",
            Reply::json(json!({"data": [{"id": "alb1", "type": "albums",
                "attributes": {"name": "Album One", "artistName": "Artist"}}]})),
        )
        .on(
            "GET",
            "/v1/catalog/it/albums/alb1/tracks",
            Reply::json(json!({"data": []})),
        );
    let (library, clock) = signed_in(&fixtures);

    assert_eq!(library.recommendations(10).await.unwrap().len(), 2);
    library
        .add_to_library(Additions {
            songs: &[],
            albums: &[catalog("alb1")],
            playlists: &[],
        })
        .await
        .unwrap();
    let again = library.recommendations(10).await.unwrap();
    assert!(again[1].albums[0].in_library);
    // Read from Apple once: the first page and the one it names.
    assert_eq!(fixtures.seen_for("GET", "/v1/me/recommendations").len(), 2);

    clock.advance(TTL);
    library
        .recommendations(DEFAULT_RECOMMENDATIONS)
        .await
        .unwrap();
    assert_eq!(fixtures.seen_for("GET", "/v1/me/recommendations").len(), 3);
}

// Sign-in -----------------------------------------------------------------

#[tokio::test]
async fn new_tokens_forget_additions_pages_and_recommendations() {
    let fixtures = FixtureServer::start().await;
    serve_additions(&fixtures);
    fixtures.on(
        "GET",
        "/v1/me/recommendations",
        fixture("recommendations_page2.json"),
    );
    let (library, _) = signed_in(&fixtures);
    library
        .add_to_library(Additions {
            songs: &[key("catalog:6001")],
            albums: &[],
            playlists: &[],
        })
        .await
        .unwrap();
    library
        .recommendations(DEFAULT_RECOMMENDATIONS)
        .await
        .unwrap();
    library.catalog_album(&catalog("7002")).await.unwrap();
    let marked = || async {
        library
            .search("dawn", SearchScope::Catalog)
            .await
            .unwrap()
            .songs[1]
            .in_library
    };
    assert!(marked().await);

    library.sign_in(Tokens::new(DEVELOPER_TOKEN, "another-user-token").unwrap());
    assert!(!marked().await);
    library
        .recommendations(DEFAULT_RECOMMENDATIONS)
        .await
        .unwrap();
    library.catalog_album(&catalog("7002")).await.unwrap();
    assert_eq!(fixtures.seen_for("GET", "/v1/me/recommendations").len(), 2);
    assert_eq!(
        fixtures.seen_for("GET", "/v1/catalog/it/albums/7002").len(),
        2
    );
}

#[tokio::test]
async fn catalog_calls_need_a_sign_in() {
    let fixtures = FixtureServer::start().await;
    serve_search(&fixtures);
    let library = AppleLibrary::with_client(|builder| {
        builder
            .base_url(fixtures.base_url.clone())
            .storefront("it")
            .retry_policy(fast_retry())
    });
    let signed_out = |result: Result<(), LibraryError>| {
        assert!(matches!(result, Err(LibraryError::SignedOut)), "{result:?}");
    };
    signed_out(library.search("dawn", SearchScope::All).await.map(drop));
    signed_out(library.catalog_album(&catalog("7002")).await.map(drop));
    signed_out(library.catalog_artist(&catalog("8002")).await.map(drop));
    signed_out(
        library
            .catalog_playlist(&catalog("pl.morning"))
            .await
            .map(drop),
    );
    signed_out(
        library
            .recommendations(DEFAULT_RECOMMENDATIONS)
            .await
            .map(drop),
    );
    signed_out(library.membership(Candidates::default()).await.map(drop));
    signed_out(
        library
            .add_to_library(Additions {
                songs: &[key("catalog:1")],
                albums: &[],
                playlists: &[],
            })
            .await
            .map(drop),
    );
    assert!(fixtures.seen().is_empty());
}

#[test]
fn item_ids_are_checked_and_round_trip_through_text() {
    for text in ["l.alb1", "7002", "catalog:7002", "catalog:l.alb1"] {
        let id: ItemId = text.parse().unwrap();
        assert_eq!(id.to_string(), text);
        assert_eq!(id.is_catalog(), text.starts_with("catalog:"));
    }
    assert_eq!(catalog("7002").catalog_id(), Some("7002"));
    assert_eq!(catalog("7002").library_id(), None);
    assert_eq!(library_item("l.1").library_id(), Some("l.1"));
    for text in ["", "catalog:", "catalog:catalog:1"] {
        assert!(text.parse::<ItemId>().is_err(), "{text:?}");
    }
    for id in ["", "catalog:1"] {
        assert!(ItemId::catalog(id).is_err(), "{id:?}");
        assert!(ItemId::library(id).is_err(), "{id:?}");
    }
}

// Editions ----------------------------------------------------------------

#[tokio::test]
async fn an_edition_with_another_catalog_id_is_not_the_librarys_album() {
    let fixtures = FixtureServer::start().await;
    fixtures
        .on(
            "GET",
            "/v1/me/library/search",
            fixture("library_search_editions.json"),
        )
        .on(
            "GET",
            "/v1/catalog/it/search",
            fixture("catalog_search_editions.json"),
        );
    let (library, _) = signed_in(&fixtures);

    // The library's album names catalog id 7010: the edition 7011 shares its name and
    // artist, and is another album.
    let found = library.search("dawn", SearchScope::Catalog).await.unwrap();
    assert_eq!(
        album_marks(&found.albums),
        [(catalog("7010"), true), (catalog("7011"), false)]
    );
    let all = library.search("dawn", SearchScope::All).await.unwrap();
    assert_eq!(
        album_marks(&all.albums),
        [(library_item("l.alb9"), true), (catalog("7011"), false)]
    );
}

#[tokio::test]
async fn kept_albums_match_by_catalog_id_when_they_name_one() {
    let fixtures = FixtureServer::start().await;
    fixtures
        .on(
            "GET",
            "/v1/me/library/recently-added",
            Reply::json(json!({"data": []})),
        )
        .on(
            "GET",
            "/v1/me/library/albums",
            Reply::json(json!({"data": [
                {"id": "l.alb9", "type": "library-albums", "attributes": {
                    "name": "Dawn Album", "artistName": "Artist One",
                    "playParams": {"id": "l.alb9", "kind": "album", "catalogId": "7010"}}},
                {"id": "l.nameless", "type": "library-albums", "attributes": {}}
            ]})),
        );
    let (library, _) = signed_in(&fixtures);
    let albums = [
        album_item(catalog("7010"), "Dawn Album", "Artist One"),
        album_item(catalog("7011"), "Dawn Album", "Artist One"),
        // Nothing is named by an empty name.
        album_item(catalog("7012"), "", ""),
    ];
    let found = library
        .membership(Candidates {
            albums: &albums,
            ..Candidates::default()
        })
        .await
        .unwrap();
    assert_eq!(found.albums, [catalog("7010")].into_iter().collect());
}

#[tokio::test]
async fn artists_without_a_name_never_match_and_the_library_artists_are_kept() {
    let fixtures = FixtureServer::start().await;
    serve_catalog_items(&fixtures);
    fixtures.on(
        "GET",
        "/v1/me/library/artists",
        Reply::json(json!({"data": [
            {"id": "r.art1", "type": "library-artists", "attributes": {"name": "Artist One"}},
            {"id": "r.none", "type": "library-artists", "attributes": {}}
        ]})),
    );
    let (library, clock) = signed_in(&fixtures);
    let artists = [
        artist_item(catalog("8001"), "Artist One"),
        artist_item(catalog("8003"), ""),
    ];
    let candidates = Candidates {
        artists: &artists,
        ..Candidates::default()
    };

    let found = library.membership(candidates).await.unwrap();
    assert_eq!(found.artists, [catalog("8001")].into_iter().collect());
    // The artist page and further asks share the kept names.
    library.catalog_artist(&catalog("8002")).await.unwrap();
    library.membership(candidates).await.unwrap();
    assert_eq!(fixtures.seen_for("GET", "/v1/me/library/artists").len(), 1);
    clock.advance(TTL);
    library.membership(candidates).await.unwrap();
    assert_eq!(fixtures.seen_for("GET", "/v1/me/library/artists").len(), 2);
}

#[tokio::test]
async fn unreadable_library_artists_read_as_not_in_the_library() {
    let fixtures = FixtureServer::start().await;
    // The first read of the library's artists fails.
    fixtures.on(
        "GET",
        "/v1/me/library/artists",
        Reply::fixture_status(404, "error_not_found.json"),
    );
    serve_catalog_items(&fixtures);
    let (library, _) = signed_in(&fixtures);
    let artists = [artist_item(catalog("8001"), "Artist One")];
    let candidates = Candidates {
        artists: &artists,
        ..Candidates::default()
    };
    assert!(library
        .membership(candidates)
        .await
        .unwrap()
        .artists
        .is_empty());
    assert!(
        !library
            .catalog_artist(&catalog("8002"))
            .await
            .unwrap()
            .artist
            .in_library
    );
}

// Marks after adding --------------------------------------------------------

#[tokio::test]
async fn album_pages_mark_each_song_by_its_own_id() {
    let fixtures = FixtureServer::start().await;
    serve_additions(&fixtures);
    let (library, _) = signed_in(&fixtures);

    library
        .add_to_library(Additions {
            songs: &[key("catalog:6001")],
            ..Additions::default()
        })
        .await
        .unwrap();
    let detail = library.catalog_album(&catalog("7002")).await.unwrap();
    assert!(!detail.album.in_library);
    assert_eq!(
        marks(&detail.tracks),
        [
            ("catalog:6001".to_owned(), true),
            ("catalog:6002".to_owned(), false)
        ]
    );
    let found = library
        .membership(Candidates {
            songs: &[key("catalog:6001"), key("catalog:6002")],
            ..Candidates::default()
        })
        .await
        .unwrap();
    assert_eq!(found.songs, [key("catalog:6001")].into_iter().collect());
}

#[tokio::test]
async fn an_added_song_the_list_holds_from_a_playlist_joins_the_library() {
    let fixtures = FixtureServer::start().await;
    serve_library(&fixtures);
    fixtures
        .on("POST", "/v1/me/library", Reply::status(202))
        .on_target(
            "GET",
            "/v1/catalog/it/songs?ids=3001",
            Reply::json(json!({"data": [{"id": "3001", "type": "songs",
            "attributes": {"name": "Three", "playParams": {"id": "3001", "kind": "song"}}}]})),
        );
    let (library, _) = signed_in(&fixtures);
    let listed = |songs: &apple_music::library::SongList| {
        songs.get(&key("catalog:3001")).map(|song| song.in_library)
    };
    assert_eq!(listed(&library.songs().await.unwrap()), Some(false));

    library
        .add_to_library(Additions {
            songs: &[key("catalog:3001")],
            ..Additions::default()
        })
        .await
        .unwrap();
    assert_eq!(listed(&library.songs().await.unwrap()), Some(true));
    assert!(
        library
            .track(&key("catalog:3001"))
            .await
            .unwrap()
            .in_library
    );
    let found = library
        .membership(Candidates {
            songs: &[key("catalog:3001")],
            ..Candidates::default()
        })
        .await
        .unwrap();
    assert!(found.songs.contains(&key("catalog:3001")));
    assert_eq!(library_page_reads(&fixtures), 2);
}

#[tokio::test]
async fn a_song_read_after_adding_counts_as_in_the_library() {
    let fixtures = FixtureServer::start().await;
    serve_additions(&fixtures);
    let (library, _) = signed_in(&fixtures);
    library
        .add_to_library(Additions {
            songs: &[key("catalog:6001")],
            ..Additions::default()
        })
        .await
        .unwrap();
    // Nothing was kept, so the song is read from the catalog.
    assert!(
        library
            .track(&key("catalog:6001"))
            .await
            .unwrap()
            .in_library
    );
}

// Adding while something else runs ------------------------------------------

#[tokio::test]
async fn adding_while_the_first_song_list_builds_retires_that_build() {
    let fixtures = FixtureServer::start().await;
    serve_additions(&fixtures);
    // The build ends while the addition still reads the album, so only retiring the build
    // when the addition starts keeps it from being kept without the addition.
    fixtures.delay("GET", "/v1/me/library/songs", Duration::from_millis(300));
    fixtures.delay(
        "GET",
        "/v1/catalog/it/albums/7002",
        Duration::from_millis(1500),
    );
    let (library, _) = signed_in(&fixtures);

    let building = tokio::spawn({
        let library = library.clone();
        async move { library.songs().await.map(|songs| songs.len()) }
    });
    until(|| library_page_reads(&fixtures) > 0).await;
    library
        .add_to_library(Additions {
            albums: &[catalog("7002")],
            ..Additions::default()
        })
        .await
        .unwrap();
    assert!(building.await.unwrap().is_ok());
    fixtures.delay("GET", "/v1/me/library/songs", Duration::ZERO);

    // The build that ran meanwhile may not have seen the addition, so it is not kept.
    let reads = library_page_reads(&fixtures);
    library.songs().await.unwrap();
    assert!(library_page_reads(&fixtures) > reads);
}

#[tokio::test]
async fn an_album_apple_does_not_know_does_not_spoil_the_other_additions() {
    let fixtures = FixtureServer::start().await;
    serve_additions(&fixtures);
    let (library, _) = signed_in(&fixtures);
    library.songs().await.unwrap();
    library.albums().await.unwrap();

    library
        .add_to_library(Additions {
            albums: &[catalog("404"), catalog("7002")],
            ..Additions::default()
        })
        .await
        .unwrap();
    let songs = library.songs().await.unwrap();
    assert!(songs
        .get(&key("catalog:6002"))
        .is_some_and(|song| song.in_library));
    let albums = library.albums().await.unwrap();
    assert_eq!(albums[0].id, "catalog:7002");
    assert_eq!(albums.len(), 4);
    // Nothing was rebuilt.
    assert_eq!(library_page_reads(&fixtures), 2);
    assert_eq!(fixtures.seen_for("GET", "/v1/me/library/albums").len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn additions_at_once_both_reach_the_kept_list_without_a_rebuild() {
    let fixtures = FixtureServer::start().await;
    serve_additions(&fixtures);
    fixtures
        .on_target(
            "GET",
            "/v1/catalog/it/songs?ids=6002",
            Reply::json(json!({"data": [{"id": "6002", "type": "songs",
                "attributes": {"name": "Noon", "playParams": {"id": "6002", "kind": "song"}}}]})),
        )
        .on(
            "PUT",
            "/v1/me/ratings/songs/6002",
            fixture("rating_put_catalog_song.json"),
        );
    let (library, _) = signed_in(&fixtures);
    library.songs().await.unwrap();

    let (one, two) = ([key("catalog:6001")], [key("catalog:6002")]);
    let (first, second) = tokio::join!(
        library.add_to_library(Additions {
            songs: &one,
            ..Additions::default()
        }),
        library.set_liked(&two[0], true),
    );
    first.unwrap();
    second.unwrap();
    let songs = library.songs().await.unwrap();
    assert!(songs.get(&key("catalog:6001")).is_some());
    assert!(songs.get(&key("catalog:6002")).is_some());
    assert_eq!(library_page_reads(&fixtures), 2);

    let (first, second) = tokio::join!(
        library.add_to_library(Additions {
            songs: &one,
            ..Additions::default()
        }),
        library.add_to_library(Additions {
            songs: &two,
            ..Additions::default()
        }),
    );
    first.unwrap();
    second.unwrap();
    assert_eq!(library_page_reads(&fixtures), 2);
}

#[tokio::test]
async fn the_library_search_does_not_hide_the_catalog_behind_its_cap() {
    let fixtures = FixtureServer::start().await;
    let songs: Vec<_> = (0..30)
        .map(|n| {
            json!({"id": format!("i.many{n}"), "type": "library-songs",
                "attributes": {"name": format!("Many {n}"),
                    "playParams": {"id": format!("i.many{n}"), "kind": "song", "isLibrary": true}}})
        })
        .collect();
    fixtures
        .on(
            "GET",
            "/v1/me/library/search",
            Reply::json(json!({"results": {"library-songs": {"data": songs}}})),
        )
        .on(
            "GET",
            "/v1/catalog/it/search",
            fixture("catalog_search_dawn.json"),
        );
    let (library, _) = signed_in(&fixtures);

    let all = library.search("many", SearchScope::All).await.unwrap();
    let listed = keys(&all.songs);
    assert_eq!(listed.len(), 27);
    assert_eq!(listed[24], "i.many24");
    assert_eq!(&listed[25..], ["catalog:1001", "catalog:6001"]);
    let own = library.search("many", SearchScope::Library).await.unwrap();
    assert_eq!(own.songs.len(), 25);
}

// Sign-in during a call ---------------------------------------------------

fn another_account() -> Tokens {
    Tokens::new(DEVELOPER_TOKEN, "another-user-token").unwrap()
}

#[tokio::test]
async fn an_addition_in_flight_when_the_tokens_change_is_not_kept() {
    let fixtures = FixtureServer::start().await;
    serve_additions(&fixtures);
    fixtures.on_target(
        "GET",
        "/v1/catalog/it/songs?ids=6002",
        Reply::json(json!({"data": [{"id": "6002", "type": "songs",
            "attributes": {"name": "Noon", "playParams": {"id": "6002", "kind": "song"}}}]})),
    );
    fixtures.delay("POST", "/v1/me/library", Duration::from_millis(500));
    let (library, _) = signed_in(&fixtures);
    library.songs().await.unwrap();

    let adding = tokio::spawn({
        let library = library.clone();
        async move {
            library
                .add_to_library(Additions {
                    songs: &[key("catalog:6001")],
                    albums: &[catalog("7002")],
                    playlists: &[catalog("pl.morning")],
                })
                .await
        }
    });
    until(|| !fixtures.seen_for("POST", "/v1/me/library").is_empty()).await;
    library.sign_in(another_account());
    assert_eq!(adding.await.unwrap().unwrap(), 3);

    // Neither the song, nor the album and the songs it holds, nor the playlist are the new
    // account's.
    let found = library.search("dawn", SearchScope::Catalog).await.unwrap();
    assert!(!found.songs[1].in_library);
    assert!(!found.albums[1].in_library);
    let playlist = library
        .catalog_playlist(&catalog("pl.morning"))
        .await
        .unwrap();
    assert!(!playlist.playlist.in_library);
    let album = library.catalog_album(&catalog("7002")).await.unwrap();
    assert!(album.tracks.iter().all(|song| !song.in_library));
    assert!(
        !library
            .track(&key("catalog:6002"))
            .await
            .unwrap()
            .in_library
    );
}

#[tokio::test]
async fn an_album_read_in_flight_when_the_tokens_change_is_read_again() {
    let fixtures = FixtureServer::start().await;
    fixtures
        .on(
            "GET",
            "/v1/catalog/it/albums/7002",
            fixture("catalog_album_sunrise.json"),
        )
        .on(
            "GET",
            "/v1/catalog/it/albums/7002",
            Reply::json(json!({"data": [{"id": "7002", "type": "albums",
                "attributes": {"name": "Sunrise Reissue"}}]})),
        )
        .on(
            "GET",
            "/v1/catalog/it/albums/7002/tracks",
            fixture("catalog_album_sunrise_tracks.json"),
        );
    fixtures.delay(
        "GET",
        "/v1/catalog/it/albums/7002",
        Duration::from_millis(500),
    );
    let (library, _) = signed_in(&fixtures);

    let reading = tokio::spawn({
        let library = library.clone();
        async move { library.catalog_album(&catalog("7002")).await }
    });
    until(|| {
        !fixtures
            .seen_for("GET", "/v1/catalog/it/albums/7002")
            .is_empty()
    })
    .await;
    library.sign_in(another_account());
    let detail = reading.await.unwrap().unwrap();

    // The answer is the second read's, and that is what was kept.
    assert_eq!(detail.album.name, "Sunrise Reissue");
    assert_eq!(
        fixtures.seen_for("GET", "/v1/catalog/it/albums/7002").len(),
        2
    );
    library.catalog_album(&catalog("7002")).await.unwrap();
    assert_eq!(
        fixtures.seen_for("GET", "/v1/catalog/it/albums/7002").len(),
        2
    );
}

#[tokio::test]
async fn recommendations_in_flight_when_the_tokens_change_are_read_again() {
    let fixtures = FixtureServer::start().await;
    fixtures
        .on(
            "GET",
            "/v1/me/recommendations",
            fixture("recommendations_page2.json"),
        )
        .on(
            "GET",
            "/v1/me/recommendations",
            Reply::json(
                json!({"data": [{"id": "rec-9", "type": "personal-recommendation",
                "attributes": {"title": {"stringForDisplay": "Fresh"}, "kind": "recently-played"},
                "relationships": {"contents": {"data": [
                    {"id": "alb9", "type": "albums", "attributes": {"name": "Nine"}}]}}}]}),
            ),
        );
    fixtures.delay("GET", "/v1/me/recommendations", Duration::from_millis(500));
    let (library, _) = signed_in(&fixtures);

    let reading = tokio::spawn({
        let library = library.clone();
        async move { library.recommendations(DEFAULT_RECOMMENDATIONS).await }
    });
    until(|| {
        !fixtures
            .seen_for("GET", "/v1/me/recommendations")
            .is_empty()
    })
    .await;
    library.sign_in(another_account());
    let groups = reading.await.unwrap().unwrap();

    let titles: Vec<&str> = groups.iter().map(|group| group.title.as_str()).collect();
    assert_eq!(titles, ["Fresh"]);
    assert_eq!(fixtures.seen_for("GET", "/v1/me/recommendations").len(), 2);
    assert_eq!(
        fixtures.seen_for("GET", "/v1/me/recommendations")[1]
            .user_token
            .as_deref(),
        Some("another-user-token")
    );
}
