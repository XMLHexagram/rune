//! Endpoint methods of [`AppleMusicClient`].

use std::collections::HashSet;

use futures_util::stream::{self, BoxStream};
use futures_util::{StreamExt, TryStreamExt};
use reqwest::Method;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::client::{AppleMusicClient, Auth};
use crate::error::{Error, Result};
use crate::models::{
    Album, Artist, CatalogSearchResults, CatalogSearchType, LibraryAlbum, LibraryArtist,
    LibraryEntry, LibraryPlaylist, LibraryPlaylistFolder, LibrarySearchResults, LibrarySearchType,
    LibrarySong, Page, Playlist, Rating, RatingValue, Recommendation, Song, SongSearchResult,
    Storefront, TrackRef,
};
use crate::{
    CATALOG_SEARCH_LIMIT, CATALOG_SONGS_BATCH, ID_BATCH, LIBRARY_PAGE_SIZE, LIBRARY_SEARCH_LIMIT,
    LIBRARY_SONGS_BATCH, RECENTLY_ADDED_LIMIT, RECENTLY_ADDED_MAX_PAGES, RECOMMENDATIONS_LIMIT,
    RECOMMENDATIONS_MAX_PAGES,
};

/// The id prefix of a catalog playlist that is a personal mix.
const PERSONAL_MIX: &str = "pl.pm-";

/// Percent-encodes `s`; `keep` lists extra bytes left as they are.
fn encode(s: &str, keep: &[u8], space_as_plus: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) || keep.contains(&b) {
            out.push(b as char);
        } else if b == b' ' && space_as_plus {
            out.push('+');
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn segment(s: &str) -> String {
    encode(s, &[], false)
}

/// Appends a query string to `path`. Keys and values keep `,[]` as Apple's
/// examples write them; spaces become `+`.
fn target(path: &str, query: &[(&str, String)]) -> String {
    if query.is_empty() {
        return path.to_owned();
    }
    let pairs: Vec<String> = query
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k, b",[]", true), encode(v, b",[]", true)))
        .collect();
    format!("{path}?{}", pairs.join("&"))
}

/// Adds the pairs of `carry` whose keys a `next` subpath lacks: Apple's `next`
/// links keep the offset but drop `limit` and `include`.
fn carry_query(next: &str, carry: &[(&str, String)]) -> String {
    let query = next.split_once('?').map_or("", |(_, query)| query);
    let mut out = next.to_owned();
    for (key, value) in carry {
        let key = encode(key, b",[]", true);
        if query
            .split('&')
            .any(|pair| pair.split('=').next() == Some(&key))
        {
            continue;
        }
        out.push(if out.contains('?') { '&' } else { '?' });
        out.push_str(&format!("{key}={}", encode(value, b",[]", true)));
    }
    out
}

/// The `offset` a `next` subpath asks for.
fn next_offset(next: &str) -> Option<u32> {
    let (_, query) = next.split_once('?')?;
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("offset="))?
        .parse()
        .ok()
}

fn include_query(include: &[&str]) -> Option<(&'static str, String)> {
    (!include.is_empty()).then(|| ("include", include.join(",")))
}

fn first_of<T>(page: Page<T>, what: &str) -> Result<T> {
    page.data
        .into_iter()
        .next()
        .ok_or_else(|| Error::NotFound(what.to_owned()))
}

/// The catalog needs only the developer token; personal mixes belong to the user.
fn catalog_auth(kind: &str, id: &str) -> Auth {
    if kind == "playlists" && id.starts_with(PERSONAL_MIX) {
        Auth::User
    } else {
        Auth::Developer
    }
}

impl AppleMusicClient {
    /// Follows `next` links and yields every page of a collection. `carry` lists
    /// query pairs that every page must keep.
    fn page_stream<T>(
        &self,
        auth: Auth,
        first: String,
        carry: Vec<(&'static str, String)>,
    ) -> BoxStream<'static, Result<Vec<T>>>
    where
        T: DeserializeOwned + Send + 'static,
    {
        let client = self.clone();
        stream::try_unfold(Some(first), move |current| {
            let client = client.clone();
            let carry = carry.clone();
            async move {
                let Some(current) = current else {
                    return Ok::<_, Error>(None);
                };
                let page: Page<T> = client
                    .execute_json(Method::GET, &current, None, auth)
                    .await?;
                let next = page
                    .next
                    .map(|n| carry_query(&n, &carry))
                    .filter(|n| *n != current && !page.data.is_empty());
                Ok(Some((page.data, next)))
            }
        })
        .boxed()
    }

    /// Follows `next` links and yields every item of a collection.
    fn paged<T>(
        &self,
        auth: Auth,
        first: String,
        carry: Vec<(&'static str, String)>,
    ) -> BoxStream<'static, Result<T>>
    where
        T: DeserializeOwned + Send + 'static,
    {
        self.page_stream(auth, first, carry)
            .map_ok(|items| stream::iter(items.into_iter().map(Ok::<T, Error>)))
            .try_flatten()
            .boxed()
    }

    /// Every page of a library collection, in order. When the first page states the
    /// collection's total and its `next` offset, the other pages are read by offset,
    /// `parallel` at a time; otherwise `next` is followed one page after another.
    fn library_pages<T>(
        &self,
        path: &str,
        include: &[&str],
        parallel: usize,
    ) -> BoxStream<'static, Result<Vec<T>>>
    where
        T: DeserializeOwned + Send + 'static,
    {
        let mut carry = vec![("limit", LIBRARY_PAGE_SIZE.to_string())];
        carry.extend(include_query(include));
        let first = target(path, &carry);
        let path = path.to_owned();
        let client = self.clone();
        stream::once(async move {
            let page: Page<T> = client
                .execute_json(Method::GET, &first, None, Auth::User)
                .await?;
            let total = page.meta.as_ref().and_then(|meta| meta.total);
            let step = page.next.as_deref().and_then(next_offset);
            let rest = match (page.next.as_deref(), total, step) {
                _ if page.data.is_empty() => stream::empty().boxed(),
                (Some(_), Some(total), Some(step)) if step > 0 => {
                    let offsets = (1..)
                        .map_while(move |n: u32| step.checked_mul(n))
                        .take_while(move |offset| *offset < total);
                    stream::iter(offsets)
                        .map(move |offset| {
                            let client = client.clone();
                            let page = carry_query(&format!("{path}?offset={offset}"), &carry);
                            async move {
                                client
                                    .execute_json::<Page<T>>(Method::GET, &page, None, Auth::User)
                                    .await
                                    .map(|page| page.data)
                            }
                        })
                        .buffered(parallel.max(1))
                        .boxed()
                }
                (Some(next), _, _) => {
                    client.page_stream(Auth::User, carry_query(next, &carry), carry)
                }
                (None, _, _) => stream::empty().boxed(),
            };
            Ok::<_, Error>(stream::once(async move { Ok(page.data) }).chain(rest))
        })
        .try_flatten()
        .boxed()
    }

    fn library_collection<T>(&self, path: &str, include: &[&str]) -> BoxStream<'static, Result<T>>
    where
        T: DeserializeOwned + Send + 'static,
    {
        let mut q = vec![("limit", LIBRARY_PAGE_SIZE.to_string())];
        q.extend(include_query(include));
        self.paged(Auth::User, target(path, &q), q)
    }

    async fn library_resource<T>(&self, path: &str, include: &[&str], what: &str) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let q: Vec<_> = include_query(include).into_iter().collect();
        let page: Page<T> = self
            .execute_json(Method::GET, &target(path, &q), None, Auth::User)
            .await?;
        first_of(page, what)
    }

    // Storefront -----------------------------------------------------------

    /// `GET /v1/me/storefront`: the user's storefront. Needs a user token.
    pub async fn user_storefront(&self) -> Result<Storefront> {
        let page: Page<Storefront> = self
            .execute_json(Method::GET, "/v1/me/storefront", None, Auth::User)
            .await?;
        first_of(page, "storefront")
    }

    /// The storefront used for catalog calls: the configured one, else the
    /// user's, which is looked up once and cached.
    pub async fn storefront(&self) -> Result<String> {
        let (epoch, cached) = self.cached_storefront();
        if let Some(s) = cached {
            return Ok(s);
        }
        let id = self.user_storefront().await?.id;
        self.set_storefront(epoch, id.clone());
        Ok(id)
    }

    // Library reads --------------------------------------------------------

    /// `GET /v1/me/library/songs`: every song, 100 per request, following `next`.
    pub fn library_songs(&self) -> BoxStream<'static, Result<LibrarySong>> {
        self.library_songs_with(&[])
    }

    /// Like [`Self::library_songs`], including relationships such as `albums`,
    /// `artists` or `catalog` on every page.
    pub fn library_songs_with(&self, include: &[&str]) -> BoxStream<'static, Result<LibrarySong>> {
        self.library_collection("/v1/me/library/songs", include)
    }

    /// The pages of [`Self::library_songs_with`], in order, read `parallel` at a time
    /// when Apple states the total.
    pub fn library_song_pages(
        &self,
        include: &[&str],
        parallel: usize,
    ) -> BoxStream<'static, Result<Vec<LibrarySong>>> {
        self.library_pages("/v1/me/library/songs", include, parallel)
    }

    /// `GET /v1/me/library/songs?ids=...`: library songs by id, 300 per request.
    /// `include` names relationships to include, for example `["catalog"]`.
    pub async fn library_songs_by_ids(
        &self,
        ids: &[String],
        include: &[&str],
    ) -> Result<Vec<LibrarySong>> {
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(LIBRARY_SONGS_BATCH) {
            let mut q = vec![("ids", chunk.join(","))];
            if !include.is_empty() {
                q.push(("include", include.join(",")));
            }
            let page: Page<LibrarySong> = self
                .execute_json(
                    Method::GET,
                    &target("/v1/me/library/songs", &q),
                    None,
                    Auth::User,
                )
                .await?;
            out.extend(page.data);
        }
        Ok(out)
    }

    /// `GET /v1/me/library/albums`: every album, 100 per request.
    pub fn library_albums(&self) -> BoxStream<'static, Result<LibraryAlbum>> {
        self.library_albums_with(&[])
    }

    /// Like [`Self::library_albums`], including relationships such as `artists`
    /// on every page.
    pub fn library_albums_with(
        &self,
        include: &[&str],
    ) -> BoxStream<'static, Result<LibraryAlbum>> {
        self.library_collection("/v1/me/library/albums", include)
    }

    /// The pages of [`Self::library_albums_with`], in order, read `parallel` at a time
    /// when Apple states the total.
    pub fn library_album_pages(
        &self,
        include: &[&str],
        parallel: usize,
    ) -> BoxStream<'static, Result<Vec<LibraryAlbum>>> {
        self.library_pages("/v1/me/library/albums", include, parallel)
    }

    /// `GET /v1/me/library/albums/{id}`: one library album.
    pub async fn library_album(&self, id: &str, include: &[&str]) -> Result<LibraryAlbum> {
        let path = format!("/v1/me/library/albums/{}", segment(id));
        self.library_resource(&path, include, "library album").await
    }

    /// `GET /v1/me/library/albums/{id}/tracks`: the tracks of a library album,
    /// 100 per request.
    pub fn library_album_tracks(&self, id: &str) -> BoxStream<'static, Result<LibrarySong>> {
        let path = format!("/v1/me/library/albums/{}/tracks", segment(id));
        self.library_collection(&path, &[])
    }

    /// `GET /v1/me/library/artists`: every artist, 100 per request.
    pub fn library_artists(&self) -> BoxStream<'static, Result<LibraryArtist>> {
        self.library_collection("/v1/me/library/artists", &[])
    }

    /// `GET /v1/me/library/artists/{id}`: one library artist.
    pub async fn library_artist(&self, id: &str) -> Result<LibraryArtist> {
        let path = format!("/v1/me/library/artists/{}", segment(id));
        self.library_resource(&path, &[], "library artist").await
    }

    /// `GET /v1/me/library/artists/{id}/albums`: the library albums of an artist,
    /// 100 per request.
    pub fn library_artist_albums(&self, id: &str) -> BoxStream<'static, Result<LibraryAlbum>> {
        let path = format!("/v1/me/library/artists/{}/albums", segment(id));
        self.library_collection(&path, &[])
    }

    /// `GET /v1/me/library/playlists`: every playlist, 100 per request.
    pub fn library_playlists(&self) -> BoxStream<'static, Result<LibraryPlaylist>> {
        self.library_collection("/v1/me/library/playlists", &[])
    }

    /// `GET /v1/me/library/playlists/{id}`: one library playlist, without its tracks.
    pub async fn library_playlist(&self, id: &str) -> Result<LibraryPlaylist> {
        let path = format!("/v1/me/library/playlists/{}", segment(id));
        self.library_resource(&path, &[], "library playlist").await
    }

    /// `GET /v1/me/library/playlists/{id}/tracks`: the tracks of a playlist,
    /// 100 per request. Apple answers 404 for a playlist without tracks.
    pub fn library_playlist_tracks(&self, id: &str) -> BoxStream<'static, Result<LibrarySong>> {
        let path = format!("/v1/me/library/playlists/{}/tracks", segment(id));
        self.library_collection(&path, &[])
    }

    /// `GET /v1/me/library/search`: searches the library for `types`. Apple applies
    /// `limit` (at most [`LIBRARY_SEARCH_LIMIT`]) and `offset` to every type.
    pub async fn search_library(
        &self,
        term: &str,
        types: &[LibrarySearchType],
        limit: u32,
        offset: u32,
    ) -> Result<LibrarySearchResults> {
        #[derive(Deserialize)]
        struct Response {
            #[serde(default)]
            results: Results,
        }
        #[derive(Default, Deserialize)]
        struct Results {
            #[serde(default, rename = "library-songs")]
            songs: Page<LibrarySong>,
            #[serde(default, rename = "library-albums")]
            albums: Page<LibraryAlbum>,
            #[serde(default, rename = "library-artists")]
            artists: Page<LibraryArtist>,
            #[serde(default, rename = "library-playlists")]
            playlists: Page<LibraryPlaylist>,
        }
        let types = types
            .iter()
            .map(|kind| kind.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let q = [
            ("term", term.to_owned()),
            ("types", types),
            ("limit", limit.min(LIBRARY_SEARCH_LIMIT).to_string()),
            ("offset", offset.to_string()),
        ];
        let response: Response = self
            .execute_json(
                Method::GET,
                &target("/v1/me/library/search", &q),
                None,
                Auth::User,
            )
            .await?;
        let results = response.results;
        Ok(LibrarySearchResults {
            songs: results.songs,
            albums: results.albums,
            artists: results.artists,
            playlists: results.playlists,
        })
    }

    /// `GET /v1/me/library/playlist-folders?filter[identity]=playlistsroot`:
    /// the root folder. Apple documents the root's id in `meta.filters`, and
    /// `data` may be empty, so the id is read from either place. `None` when
    /// the response names no root.
    pub async fn root_playlist_folder(&self) -> Result<Option<LibraryPlaylistFolder>> {
        #[derive(Deserialize)]
        struct Root {
            #[serde(default)]
            data: Vec<LibraryPlaylistFolder>,
            meta: Option<Value>,
        }
        let q = [("filter[identity]", "playlistsroot".to_owned())];
        let root: Root = self
            .execute_json(
                Method::GET,
                &target("/v1/me/library/playlist-folders", &q),
                None,
                Auth::User,
            )
            .await?;
        if let Some(folder) = root.data.into_iter().next() {
            return Ok(Some(folder));
        }
        let listed = root
            .meta
            .as_ref()
            .and_then(|m| m.pointer("/filters/identity/playlistsroot/0"))
            .cloned();
        Ok(listed.and_then(|v| serde_json::from_value(v).ok()))
    }

    /// `GET /v1/me/library/playlist-folders/{id}/children`: playlists and
    /// sub-folders inside a folder. Use the id from [`Self::root_playlist_folder`]
    /// for the top level.
    pub fn library_folder_children(&self, id: &str) -> BoxStream<'static, Result<LibraryEntry>> {
        self.library_collection(
            &format!("/v1/me/library/playlist-folders/{}/children", segment(id)),
            &[],
        )
    }

    /// `GET /v1/me/library/recently-added`: recently added albums and playlists,
    /// newest first, [`RECENTLY_ADDED_LIMIT`] per page, for at most
    /// [`RECENTLY_ADDED_MAX_PAGES`] pages.
    pub fn recently_added(&self) -> BoxStream<'static, Result<LibraryEntry>> {
        let q = vec![("limit", RECENTLY_ADDED_LIMIT.to_string())];
        self.page_stream(Auth::User, target("/v1/me/library/recently-added", &q), q)
            .take(RECENTLY_ADDED_MAX_PAGES)
            .map_ok(|items| stream::iter(items.into_iter().map(Ok::<LibraryEntry, Error>)))
            .try_flatten()
            .boxed()
    }

    // Catalog reads --------------------------------------------------------

    /// `GET /v1/catalog/{storefront}/search?types=songs`: searches songs.
    /// Apple documents neither default nor maximum `limit`; the web service
    /// accepts up to 25 for songs in practice.
    pub async fn search_songs(
        &self,
        term: &str,
        limit: u32,
        offset: u32,
    ) -> Result<SongSearchResult> {
        #[derive(Deserialize)]
        struct Response {
            results: Results,
        }
        #[derive(Deserialize)]
        struct Results {
            songs: Option<Page<Song>>,
        }
        let storefront = self.storefront().await?;
        let q = [
            ("term", term.to_owned()),
            ("types", "songs".to_owned()),
            ("limit", limit.to_string()),
            ("offset", offset.to_string()),
        ];
        let response: Response = self
            .execute_json(
                Method::GET,
                &target(&format!("/v1/catalog/{}/search", segment(&storefront)), &q),
                None,
                Auth::Developer,
            )
            .await?;
        let page = response.results.songs.unwrap_or_default();
        Ok(SongSearchResult {
            songs: page.data,
            next: page.next,
        })
    }

    /// `GET /v1/catalog/{storefront}/search`: searches the catalog for `types`. Apple applies
    /// `limit` (at most [`CATALOG_SEARCH_LIMIT`]) and `offset` to every type. Needs only the
    /// developer token.
    pub async fn search_catalog(
        &self,
        term: &str,
        types: &[CatalogSearchType],
        limit: u32,
        offset: u32,
    ) -> Result<CatalogSearchResults> {
        #[derive(Deserialize)]
        struct Response {
            #[serde(default)]
            results: Results,
        }
        #[derive(Default, Deserialize)]
        struct Results {
            #[serde(default)]
            songs: Page<Song>,
            #[serde(default)]
            albums: Page<Album>,
            #[serde(default)]
            artists: Page<Artist>,
        }
        let storefront = self.storefront().await?;
        let types = types
            .iter()
            .map(|kind| kind.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let q = [
            ("term", term.to_owned()),
            ("types", types),
            ("limit", limit.min(CATALOG_SEARCH_LIMIT).to_string()),
            ("offset", offset.to_string()),
        ];
        let response: Response = self
            .execute_json(
                Method::GET,
                &target(&format!("/v1/catalog/{}/search", segment(&storefront)), &q),
                None,
                Auth::Developer,
            )
            .await?;
        let results = response.results;
        Ok(CatalogSearchResults {
            songs: results.songs,
            albums: results.albums,
            artists: results.artists,
        })
    }

    /// `GET /v1/catalog/{storefront}/songs?ids=...`: catalog songs by id, in
    /// batches of 300. Ids the catalog does not know are absent from the result.
    pub async fn catalog_songs(&self, ids: &[String]) -> Result<Vec<Song>> {
        let storefront = self.storefront().await?;
        let path = format!("/v1/catalog/{}/songs", segment(&storefront));
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(CATALOG_SONGS_BATCH) {
            let q = [("ids", chunk.join(","))];
            let page: Page<Song> = self
                .execute_json(Method::GET, &target(&path, &q), None, Auth::Developer)
                .await?;
            out.extend(page.data);
        }
        Ok(out)
    }

    /// `GET /v1/me/recommendations`: the user's personal recommendations in Apple's order,
    /// following `next` for at most [`RECOMMENDATIONS_MAX_PAGES`] pages. Needs a user token.
    pub async fn recommendations(&self) -> Result<Vec<Recommendation>> {
        let q = [("limit", RECOMMENDATIONS_LIMIT.to_string())];
        let mut current = target("/v1/me/recommendations", &q);
        let mut out = Vec::new();
        for _ in 0..RECOMMENDATIONS_MAX_PAGES {
            let page: Page<Recommendation> = self
                .execute_json(Method::GET, &current, None, Auth::User)
                .await?;
            let empty = page.data.is_empty();
            out.extend(page.data);
            match page.next.map(|n| carry_query(&n, &q)) {
                Some(next) if !empty && next != current => current = next,
                _ => break,
            }
        }
        Ok(out)
    }

    /// `GET /v1/catalog/{storefront}/playlists/{id}/tracks`: the songs of a catalog playlist,
    /// following `next`. Music videos are skipped. Personal mixes need a user token.
    pub async fn catalog_playlist_tracks(&self, id: &str) -> Result<Vec<Song>> {
        let storefront = self.storefront().await?;
        self.catalog_songs_of(
            catalog_auth("playlists", id),
            format!(
                "/v1/catalog/{}/playlists/{}/tracks",
                segment(&storefront),
                segment(id)
            ),
        )
        .await
    }

    /// `GET /v1/catalog/{storefront}/albums/{id}/tracks`: the songs of a catalog album,
    /// following `next`. Music videos are skipped.
    pub async fn catalog_album_tracks(&self, id: &str) -> Result<Vec<Song>> {
        let storefront = self.storefront().await?;
        self.catalog_songs_of(
            Auth::Developer,
            format!(
                "/v1/catalog/{}/albums/{}/tracks",
                segment(&storefront),
                segment(id)
            ),
        )
        .await
    }

    /// `GET /v1/catalog/{storefront}/playlists/{id}`: a catalog playlist without its tracks.
    /// A personal mix (`pl.pm-` id) is read with the user token, other playlists without it.
    pub async fn catalog_playlist(&self, id: &str) -> Result<Playlist> {
        self.catalog_resource("playlists", id, &[], "catalog playlist")
            .await
    }

    /// `GET /v1/catalog/{storefront}/albums/{id}?include=artists`: a catalog album with its
    /// artists. Read its tracks with [`Self::catalog_album_tracks`].
    pub async fn catalog_album(&self, id: &str) -> Result<Album> {
        self.catalog_resource("albums", id, &["artists"], "catalog album")
            .await
    }

    /// `GET /v1/catalog/{storefront}/artists/{id}`: a catalog artist.
    pub async fn catalog_artist(&self, id: &str) -> Result<Artist> {
        self.catalog_resource("artists", id, &[], "catalog artist")
            .await
    }

    /// `GET /v1/catalog/{storefront}/artists/{id}/albums`: the albums of a catalog artist,
    /// following `next`.
    pub async fn catalog_artist_albums(&self, id: &str) -> Result<Vec<Album>> {
        let storefront = self.storefront().await?;
        let path = format!(
            "/v1/catalog/{}/artists/{}/albums",
            segment(&storefront),
            segment(id)
        );
        self.paged(Auth::Developer, path, Vec::new())
            .try_collect()
            .await
    }

    async fn catalog_resource<T: DeserializeOwned>(
        &self,
        kind: &str,
        id: &str,
        include: &[&str],
        what: &str,
    ) -> Result<T> {
        let storefront = self.storefront().await?;
        let path = format!(
            "/v1/catalog/{}/{kind}/{}",
            segment(&storefront),
            segment(id)
        );
        let q: Vec<_> = include_query(include).into_iter().collect();
        // The catalog needs only the developer token; personal mixes belong to the user.
        let page: Page<T> = self
            .execute_json(
                Method::GET,
                &target(&path, &q),
                None,
                catalog_auth(kind, id),
            )
            .await?;
        first_of(page, what)
    }

    async fn catalog_songs_of(&self, auth: Auth, path: String) -> Result<Vec<Song>> {
        let songs: Vec<Song> = self.paged(auth, path, Vec::new()).try_collect().await?;
        Ok(songs
            .into_iter()
            .filter(|song| song.kind == "songs")
            .collect())
    }

    /// `GET /v1/me/library/playlists/{id}?include=catalog`: the id of the catalog playlist a
    /// library playlist belongs to, `None` when it has none.
    pub async fn catalog_playlist_id_of_library_playlist(
        &self,
        id: &str,
    ) -> Result<Option<String>> {
        let q = [("include", "catalog".to_owned())];
        let page: Page<LibraryPlaylist> = self
            .execute_json(
                Method::GET,
                &target(&format!("/v1/me/library/playlists/{}", segment(id)), &q),
                None,
                Auth::User,
            )
            .await?;
        let playlist = first_of(page, "library playlist")?;
        Ok(playlist
            .relationships
            .and_then(|r| r.catalog)
            .and_then(|c| c.data.into_iter().next())
            .map(|c| c.id))
    }

    // Ratings --------------------------------------------------------------

    async fn ratings(&self, path: &str, ids: &[String]) -> Result<Vec<Rating>> {
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(ID_BATCH) {
            let q = [("ids", chunk.join(","))];
            let page: Page<Rating> = self
                .execute_json(Method::GET, &target(path, &q), None, Auth::User)
                .await?;
            out.extend(page.data);
        }
        Ok(out)
    }

    /// `GET /v1/me/ratings/library-songs?ids=...`: ratings of library songs.
    /// Unrated songs are absent from the result or carry no value.
    pub async fn library_song_ratings(&self, ids: &[String]) -> Result<Vec<Rating>> {
        self.ratings("/v1/me/ratings/library-songs", ids).await
    }

    /// `GET /v1/me/ratings/songs?ids=...`: ratings of catalog songs.
    pub async fn catalog_song_ratings(&self, ids: &[String]) -> Result<Vec<Rating>> {
        self.ratings("/v1/me/ratings/songs", ids).await
    }

    async fn put_rating(&self, path: String, value: RatingValue) -> Result<Rating> {
        let body = json!({"type": "rating", "attributes": {"value": value.as_i8()}});
        let page: Page<Rating> = self
            .execute_json(Method::PUT, &path, Some(&body), Auth::User)
            .await?;
        first_of(page, "rating")
    }

    /// `PUT /v1/me/ratings/library-songs/{id}`: rates a library song.
    pub async fn set_library_song_rating(&self, id: &str, value: RatingValue) -> Result<Rating> {
        self.put_rating(
            format!("/v1/me/ratings/library-songs/{}", segment(id)),
            value,
        )
        .await
    }

    /// `PUT /v1/me/ratings/songs/{id}`: rates a catalog song.
    pub async fn set_catalog_song_rating(&self, id: &str, value: RatingValue) -> Result<Rating> {
        self.put_rating(format!("/v1/me/ratings/songs/{}", segment(id)), value)
            .await
    }

    /// `DELETE /v1/me/ratings/library-songs/{id}`: removes a library song rating.
    pub async fn delete_library_song_rating(&self, id: &str) -> Result<()> {
        let path = format!("/v1/me/ratings/library-songs/{}", segment(id));
        self.execute(Method::DELETE, &path, None, Auth::User)
            .await?;
        Ok(())
    }

    /// `DELETE /v1/me/ratings/songs/{id}`: removes a catalog song rating.
    pub async fn delete_catalog_song_rating(&self, id: &str) -> Result<()> {
        let path = format!("/v1/me/ratings/songs/{}", segment(id));
        self.execute(Method::DELETE, &path, None, Auth::User)
            .await?;
        Ok(())
    }

    // Library writes -------------------------------------------------------

    /// `POST /v1/me/library?ids[songs]=...`: adds catalog songs to the library.
    /// Apple answers 202; the songs may show up later. Ids that cannot be
    /// added are ignored by the server.
    pub async fn add_songs_to_library(&self, catalog_ids: &[String]) -> Result<()> {
        self.add_to_library("songs", catalog_ids).await
    }

    /// `POST /v1/me/library?ids[albums]=...`: adds catalog albums to the library.
    pub async fn add_albums_to_library(&self, catalog_ids: &[String]) -> Result<()> {
        self.add_to_library("albums", catalog_ids).await
    }

    /// `POST /v1/me/library?ids[songs]=...&ids[albums]=...&ids[playlists]=...`: adds catalog
    /// songs, albums and playlists in as few requests as the batch size allows. Apple answers
    /// 202; the items may show up later.
    pub async fn add_catalog_items_to_library(
        &self,
        songs: &[String],
        albums: &[String],
        playlists: &[String],
    ) -> Result<()> {
        self.add_kinds_to_library(&[
            ("songs", songs),
            ("albums", albums),
            ("playlists", playlists),
        ])
        .await
    }

    async fn add_to_library(&self, kind: &str, ids: &[String]) -> Result<()> {
        self.add_kinds_to_library(&[(kind, ids)]).await
    }

    /// One request per batch of every kind at once, kinds in the order given.
    async fn add_kinds_to_library(&self, kinds: &[(&str, &[String])]) -> Result<()> {
        let batches: Vec<(String, Vec<Vec<&str>>)> = kinds
            .iter()
            .map(|(kind, ids)| {
                let mut seen = HashSet::new();
                let unique: Vec<&str> = ids
                    .iter()
                    .map(String::as_str)
                    .filter(|id| seen.insert(*id))
                    .collect();
                let chunks = unique.chunks(ID_BATCH).map(<[&str]>::to_vec).collect();
                (format!("ids[{kind}]"), chunks)
            })
            .collect();
        let rounds = batches
            .iter()
            .map(|(_, chunks)| chunks.len())
            .max()
            .unwrap_or(0);
        for round in 0..rounds {
            let q: Vec<(&str, String)> = batches
                .iter()
                .filter_map(|(key, chunks)| Some((key.as_str(), chunks.get(round)?.join(","))))
                .collect();
            self.execute(
                Method::POST,
                &target("/v1/me/library", &q),
                None,
                Auth::User,
            )
            .await?;
        }
        Ok(())
    }

    /// `POST /v1/me/library/playlists`: creates a playlist, optionally with
    /// initial tracks. Returns the created playlist.
    pub async fn create_library_playlist(
        &self,
        name: &str,
        description: Option<&str>,
        tracks: &[TrackRef],
    ) -> Result<LibraryPlaylist> {
        let mut attributes = json!({"name": name});
        if let Some(d) = description {
            attributes["description"] = json!(d);
        }
        let mut body = json!({"attributes": attributes});
        if !tracks.is_empty() {
            body["relationships"] = json!({"tracks": {"data": track_data(tracks)}});
        }
        let page: Page<LibraryPlaylist> = self
            .execute_json(
                Method::POST,
                "/v1/me/library/playlists",
                Some(&body),
                Auth::User,
            )
            .await?;
        first_of(page, "created playlist")
    }

    /// `POST /v1/me/library/playlists/{id}/tracks`: appends tracks to a
    /// playlist. Apple answers 204.
    pub async fn add_tracks_to_library_playlist(
        &self,
        playlist_id: &str,
        tracks: &[TrackRef],
    ) -> Result<()> {
        if tracks.is_empty() {
            return Ok(());
        }
        let body = json!({"data": track_data(tracks)});
        let path = format!("/v1/me/library/playlists/{}/tracks", segment(playlist_id));
        self.execute(Method::POST, &path, Some(&body), Auth::User)
            .await?;
        Ok(())
    }
}

fn track_data(tracks: &[TrackRef]) -> Value {
    Value::Array(
        tracks
            .iter()
            .map(|t| json!({"id": t.id, "type": t.kind.as_str()}))
            .collect(),
    )
}
