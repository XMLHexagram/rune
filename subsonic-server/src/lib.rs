//! Serves any in-process [`subsonic::SubsonicApi`] as a Subsonic REST API over HTTP.
//!
//! It answers `/rest/<endpoint>` and `/rest/<endpoint>.view` with JSON inside the
//! `subsonic-response` envelope, for every endpoint [`subsonic::SubsonicClient`] calls, so the
//! client can talk to an in-process server over a socket. The envelope's identity (`version`,
//! `type`, `serverVersion`, `openSubsonic`) comes from the api's `probe`.
//!
//! # Do not use in production
//!
//! This is for tests and a future gateway. It accepts the client's usual authentication
//! parameters (`u`, `t`, `s`, `p`, `apiKey`) without checking them, so it must never be exposed
//! where the API itself is not meant to be open. Authentication must be added before any
//! gateway use. The response format is always JSON, whatever `f` says.
//!
//! # Rune extensions
//!
//! The extensions come from the api: `getOpenSubsonicExtensions` lists what its `extensions`
//! returns, and `search3` honours `runeScope` only when its probe declares `runeCatalog`, as a
//! server without the extension ignores the parameter.
//!
//! # Errors
//!
//! Errors become Subsonic error objects with `runeReason` when set, in an HTTP 200 envelope as
//! Subsonic servers send them, also for `stream` and `getCoverArt`. The API has no code for
//! "unsupported", so an unsupported endpoint answers HTTP 501 with the generic error 0 naming
//! it, on every path. An unreachable stream upstream answers 502.

mod wire;

use std::str::FromStr;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use serde_json::{Map, Value, json};
use subsonic::{
    RuneLoginCompletion, RuneScope, SearchLimits, StreamError, SubsonicApi, SubsonicError,
    SubsonicServerError,
};

const API_VERSION: &str = "1.16.1";
/// Subsonic's own default for list sizes.
const DEFAULT_COUNT: u32 = 20;
/// Recommendation groups returned when `getRuneRecommendations` names no count.
const DEFAULT_RECOMMENDATION_COUNT: u32 = 10;

type Api = Arc<dyn SubsonicApi>;

/// A router that serves `api` under `/rest`.
pub fn router(api: Arc<dyn SubsonicApi>) -> Router {
    Router::new()
        .route("/rest/{endpoint}", any(handle))
        .with_state(api)
}

/// Query and form parameters, in order, so repeated ones keep theirs.
struct Params(Vec<(String, String)>);

impl Params {
    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    fn all(&self, key: &str) -> Vec<&str> {
        self.0
            .iter()
            .filter(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
            .collect()
    }

    fn required(&self, key: &str) -> Result<&str, SubsonicError> {
        self.get(key).ok_or_else(|| missing(key))
    }

    fn number<T: FromStr>(&self, key: &str) -> Result<Option<T>, SubsonicError> {
        self.get(key)
            .map(|value| parse_number(key, value))
            .transpose()
    }

    fn numbers<T: FromStr>(&self, key: &str) -> Result<Vec<T>, SubsonicError> {
        self.all(key)
            .into_iter()
            .map(|value| parse_number(key, value))
            .collect()
    }
}

fn parse_number<T: FromStr>(key: &str, value: &str) -> Result<T, SubsonicError> {
    value
        .parse()
        .map_err(|_| bad_request(format!("parameter {key} is not a valid number")))
}

/// Query or form pairs, percent-decoded, in order.
fn pairs(bytes: &[u8]) -> Vec<(String, String)> {
    url::form_urlencoded::parse(bytes).into_owned().collect()
}

/// Subsonic has no code for a malformed value, so it shares "required parameter is missing".
fn bad_request(message: String) -> SubsonicError {
    SubsonicError::Server {
        kind: SubsonicServerError::MissingParameter,
        message,
        rune_reason: None,
    }
}

fn missing(key: &str) -> SubsonicError {
    bad_request(format!("required parameter {key} is missing"))
}

async fn handle(
    State(api): State<Api>,
    Path(endpoint): Path<String>,
    method: Method,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    let endpoint = endpoint.strip_suffix(".view").unwrap_or(&endpoint);
    let query_pairs = || pairs(query.as_deref().unwrap_or("").as_bytes());
    let result = match endpoint {
        "stream" => return stream(&api, &Params(query_pairs()), &headers).await,
        "getCoverArt" => return cover_art(&api, &Params(query_pairs())).await,
        // Its fields are secrets, so they are read from the body only.
        "completeRuneLogin" => {
            if method != Method::POST {
                let error = bad_request("completeRuneLogin must be a POST".into());
                let mut response = reply(&api, StatusCode::METHOD_NOT_ALLOWED, Err(error)).await;
                response
                    .headers_mut()
                    .insert(header::ALLOW, HeaderValue::from_static("POST"));
                return response;
            }
            let form = Params(pairs(&body));
            complete_rune_login(&api, &form)
                .await
                .map_err(|error| redact(error, &form))
        }
        _ => {
            let mut all = query_pairs();
            if method == Method::POST {
                all.extend(pairs(&body));
            }
            data(&api, endpoint, &Params(all)).await
        }
    };
    match result {
        Ok(Some(data)) => reply(&api, StatusCode::OK, Ok(data)).await,
        Ok(None) => {
            let error = server_error(
                SubsonicServerError::Generic,
                &format!("unknown endpoint {endpoint}"),
            );
            reply(&api, StatusCode::NOT_FOUND, Err(error)).await
        }
        Err(error) => reply(&api, error_status(&error), Err(error)).await,
    }
}

/// Sign-in secrets must not come back in an error, whatever the api put into its message.
fn redact(error: SubsonicError, form: &Params) -> SubsonicError {
    let scrub = |mut message: String| {
        for key in ["token", "code", "developerToken", "state"] {
            if let Some(secret) = form.get(key).filter(|secret| !secret.is_empty()) {
                message = message.replace(secret, "[REDACTED]");
            }
        }
        message
    };
    match error {
        SubsonicError::Server {
            kind,
            message,
            rune_reason,
        } => SubsonicError::Server {
            kind,
            message: scrub(message),
            rune_reason,
        },
        SubsonicError::Transport(message) => SubsonicError::Transport(scrub(message)),
        SubsonicError::Malformed(message) => SubsonicError::Malformed(scrub(message)),
        other => other,
    }
}

/// The data of a JSON endpoint, or `None` for an endpoint this server does not know.
async fn data(
    api: &Api,
    endpoint: &str,
    params: &Params,
) -> Result<Option<Map<String, Value>>, SubsonicError> {
    let mut data = Map::new();
    match endpoint {
        // The envelope carries the identity; a server that cannot describe itself still pings.
        "ping" => match api.probe().await {
            Ok(_) | Err(SubsonicError::Unsupported { .. }) => {}
            Err(error) => return Err(error),
        },
        "getOpenSubsonicExtensions" => {
            let extensions: Vec<Value> = api
                .extensions()
                .await?
                .into_iter()
                .map(|(name, versions)| json!({"name": name, "versions": versions}))
                .collect();
            data.insert("openSubsonicExtensions".into(), extensions.into());
        }
        "getUser" => {
            let roles = api.probe().await?.roles;
            let username = params.get("username").or(params.get("u")).unwrap_or("rune");
            data.insert(
                "user".into(),
                json!({
                    "username": username,
                    "streamRole": roles.stream_role,
                    "downloadRole": roles.download_role,
                    "jukeboxRole": roles.jukebox_role,
                }),
            );
        }
        "tokenInfo" => {
            data.insert("tokenInfo".into(), json!({"username": "rune"}));
        }
        "getArtists" => {
            // The client reads the indexes in order, so the artists keep theirs.
            let mut indexes: Vec<(String, Vec<Value>)> = Vec::new();
            for artist in api.artists().await? {
                let initial = artist
                    .name
                    .chars()
                    .next()
                    .map(|letter| letter.to_uppercase().to_string())
                    .unwrap_or_default();
                let value = wire::artist(&artist);
                match indexes.last_mut() {
                    Some((name, artists)) if *name == initial => artists.push(value),
                    _ => indexes.push((initial, vec![value])),
                }
            }
            let index: Vec<Value> = indexes
                .into_iter()
                .map(|(name, artist)| json!({"name": name, "artist": artist}))
                .collect();
            data.insert(
                "artists".into(),
                json!({"ignoredArticles": "", "index": index}),
            );
        }
        "getArtist" => {
            let artist = api.artist(params.required("id")?).await?;
            data.insert("artist".into(), wire::artist_detail(&artist));
        }
        "getAlbum" => {
            let album = api.album(params.required("id")?).await?;
            data.insert("album".into(), wire::album_detail(&album));
        }
        "getSong" => {
            let song = api.song(params.required("id")?).await?;
            data.insert("song".into(), wire::song(&song));
        }
        "getAlbumList2" => {
            let size = params.number("size")?.unwrap_or(10);
            let offset = params.number("offset")?.unwrap_or(0);
            let albums: Vec<Value> = api
                .album_list(size, offset)
                .await?
                .iter()
                .map(wire::album)
                .collect();
            data.insert("albumList2".into(), json!({"album": albums}));
        }
        "getStarred2" => {
            let songs: Vec<Value> = api.starred_songs().await?.iter().map(wire::song).collect();
            data.insert("starred2".into(), json!({"song": songs}));
        }
        "search3" => {
            let limits = SearchLimits {
                artists: params.number("artistCount")?.unwrap_or(DEFAULT_COUNT),
                albums: params.number("albumCount")?.unwrap_or(DEFAULT_COUNT),
                songs: params.number("songCount")?.unwrap_or(DEFAULT_COUNT),
            };
            let song_offset = params.number("songOffset")?.unwrap_or(0);
            let query = params.get("query").unwrap_or("");
            let result = match rune_scope(api, params).await? {
                Some(scope) => {
                    api.search_in_scope(query, limits, song_offset, scope)
                        .await?
                }
                None => {
                    api.search_with_song_offset(query, limits, song_offset)
                        .await?
                }
            };
            data.insert(
                "searchResult3".into(),
                json!({
                    "artist": result.artists.iter().map(wire::artist).collect::<Vec<_>>(),
                    "album": result.albums.iter().map(wire::album).collect::<Vec<_>>(),
                    "song": result.songs.iter().map(wire::song).collect::<Vec<_>>(),
                }),
            );
        }
        "getScanStatus" => {
            let status = api.scan_status().await?;
            let mut value = Map::new();
            value.insert("scanning".into(), status.scanning.into());
            if let Some(count) = status.count {
                value.insert("count".into(), count.into());
            }
            data.insert("scanStatus".into(), value.into());
        }
        "getPlaylists" => {
            let playlists: Vec<Value> = api.playlists().await?.iter().map(wire::playlist).collect();
            data.insert("playlists".into(), json!({"playlist": playlists}));
        }
        "getPlaylist" => {
            let playlist = api.playlist(params.required("id")?).await?;
            data.insert("playlist".into(), wire::playlist_detail(&playlist));
        }
        "createPlaylist" => {
            let song_ids = params.all("songId");
            match params.get("playlistId") {
                Some(playlist_id) => api.replace_playlist_songs(playlist_id, &song_ids).await?,
                None => {
                    api.create_playlist(params.required("name")?, &song_ids)
                        .await?
                }
            }
        }
        "updatePlaylist" => {
            let playlist_id = params.required("playlistId")?;
            let to_add = params.all("songIdToAdd");
            let to_remove = params.numbers("songIndexToRemove")?;
            api.update_playlist(playlist_id, params.get("name"), &to_add, &to_remove)
                .await?;
        }
        "deletePlaylist" => api.delete_playlist(params.required("id")?).await?,
        "star" => api.star(&required_ids(params)?).await?,
        "unstar" => api.unstar(&required_ids(params)?).await?,
        "scrobble" => {
            let ids = required_ids(params)?;
            let times = params.numbers::<u64>("time")?;
            let submission = params
                .get("submission")
                .is_none_or(|value| value != "false");
            for (index, id) in ids.into_iter().enumerate() {
                api.scrobble(id, times.get(index).copied(), submission)
                    .await?;
            }
        }
        "getLyrics" => {
            let lyrics = api
                .lyrics(
                    params.get("artist").unwrap_or(""),
                    params.get("title").unwrap_or(""),
                )
                .await?;
            let value = lyrics
                .plain_lyrics
                .map_or_else(|| json!({}), |text| json!({"value": text}));
            data.insert("lyrics".into(), value);
        }
        "getLyricsBySongId" => {
            let lyrics = api.lyrics_by_song_id(params.required("id")?).await?;
            let mut entries: Vec<Value> = lyrics
                .synced_lyrics
                .iter()
                .map(wire::synced_lyrics)
                .collect();
            // OpenSubsonic has no plain-text field: plain lyrics are an unsynced entry.
            let has_unsynced_lines = lyrics
                .synced_lyrics
                .iter()
                .any(|entry| !entry.synced && !entry.lines.is_empty());
            if let Some(text) = lyrics.plain_lyrics.filter(|_| !has_unsynced_lines) {
                entries.push(json!({
                    "synced": false,
                    "line": text.split('\n').map(|line| json!({"value": line})).collect::<Vec<_>>(),
                }));
            }
            data.insert("lyricsList".into(), json!({"structuredLyrics": entries}));
        }
        "jukeboxControl" => return jukebox(api, params).await.map(Some),
        "getRuneVisualizerFeed" => {
            let probe = api.probe().await?;
            let url = api.rune_visualizer_feed_url(&probe).await?;
            data.insert("runeVisualizerFeed".into(), json!({"url": url.as_str()}));
        }
        "getRuneService" => {
            let descriptor = api.rune_service().await?;
            data.insert("runeService".into(), wire::rune_service(&descriptor));
        }
        "startRuneLogin" => {
            let method = wire::login_method(params.required("method")?);
            let start = api.start_rune_login(method).await?;
            data.insert("runeLogin".into(), wire::rune_login(&start));
        }
        "addToRuneLibrary" => api.add_to_library(&required_ids(params)?).await?,
        "getRuneRecommendations" => {
            let count = params
                .number("count")?
                .unwrap_or(DEFAULT_RECOMMENDATION_COUNT);
            let groups: Vec<Value> = api
                .recommendations(count)
                .await?
                .iter()
                .map(wire::rune_recommendation_group)
                .collect();
            data.insert("runeRecommendations".into(), json!({"group": groups}));
        }
        _ => return Ok(None),
    }
    Ok(Some(data))
}

/// The `runeScope` of a `search3`, or `None` when the api does not declare `runeCatalog`.
async fn rune_scope(api: &Api, params: &Params) -> Result<Option<RuneScope>, SubsonicError> {
    let Some(value) = params.get("runeScope") else {
        return Ok(None);
    };
    if !api
        .probe()
        .await
        .is_ok_and(|probe| probe.supports_rune_catalog())
    {
        return Ok(None);
    }
    // Not a missing parameter, so the generic code rather than 10.
    RuneScope::parse(value).map(Some).ok_or_else(|| {
        server_error(
            SubsonicServerError::Generic,
            &format!("unknown runeScope {value:?}: use library, catalog or all"),
        )
    })
}

fn required_ids(params: &Params) -> Result<Vec<&str>, SubsonicError> {
    let ids = params.all("id");
    if ids.is_empty() {
        return Err(missing("id"));
    }
    Ok(ids)
}

async fn jukebox(api: &Api, params: &Params) -> Result<Map<String, Value>, SubsonicError> {
    let mut data = Map::new();
    match params.required("action")? {
        "get" => {
            let jukebox = api.jukebox_get().await?;
            let mut playlist = wire::jukebox_status(&jukebox.status);
            playlist.insert(
                "entry".into(),
                jukebox.playlist.songs.iter().map(wire::song).collect(),
            );
            data.insert("jukeboxPlaylist".into(), playlist.into());
            return Ok(data);
        }
        "status" => {}
        "set" => api.jukebox_set(&params.all("id")).await?,
        "add" => api.jukebox_add(&params.all("id")).await?,
        "remove" => {
            let index = params.number("index")?.ok_or_else(|| missing("index"))?;
            api.jukebox_remove(index).await?;
        }
        "clear" => api.jukebox_clear().await?,
        "skip" => {
            let index = params.number("index")?.ok_or_else(|| missing("index"))?;
            api.jukebox_skip(index, params.number("offset")?.unwrap_or(0))
                .await?;
        }
        "start" => api.jukebox_start().await?,
        "stop" => api.jukebox_stop().await?,
        "shuffle" => api.jukebox_shuffle().await?,
        "setGain" => {
            let gain = params.number("gain")?.ok_or_else(|| missing("gain"))?;
            api.jukebox_set_gain(gain).await?;
        }
        other => return Err(bad_request(format!("unknown jukebox action {other}"))),
    }
    // Subsonic answers every action with the state; clients here only need it for `status`.
    match api.jukebox_status().await {
        Ok(status) => {
            data.insert("jukeboxStatus".into(), wire::jukebox_status(&status).into());
        }
        Err(error) if params.get("action") == Some("status") => return Err(error),
        Err(_) => {}
    }
    Ok(data)
}

async fn complete_rune_login(
    api: &Api,
    form: &Params,
) -> Result<Option<Map<String, Value>>, SubsonicError> {
    let method = wire::login_method(form.required("method")?);
    let field = |key: &str| form.get(key).map(str::to_owned);
    let completion = RuneLoginCompletion {
        method,
        state: field("state"),
        code: field("code"),
        token: field("token"),
        developer_token: field("developerToken"),
    };
    api.complete_rune_login(&completion).await?;
    Ok(Some(Map::new()))
}

async fn stream(api: &Api, params: &Params, request_headers: &HeaderMap) -> Response {
    let Some(id) = params.get("id") else {
        return reply(api, StatusCode::OK, Err(missing("id"))).await;
    };
    let range = request_headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_range);
    let body = match api.stream(id, range).await {
        Ok(body) => body,
        Err(error) => {
            let (status, error) = match error {
                StreamError::CredentialsRejected => (
                    StatusCode::OK,
                    server_error(SubsonicServerError::WrongCredentials, "not authorized"),
                ),
                StreamError::TrackGone => (
                    StatusCode::OK,
                    server_error(SubsonicServerError::NotFound, "song not found"),
                ),
                StreamError::Rejected => (
                    StatusCode::OK,
                    server_error(SubsonicServerError::Generic, "stream was rejected"),
                ),
                StreamError::Unsupported => (
                    StatusCode::NOT_IMPLEMENTED,
                    SubsonicError::Unsupported { endpoint: "stream" },
                ),
                StreamError::Unreachable => (
                    StatusCode::BAD_GATEWAY,
                    server_error(SubsonicServerError::Generic, "stream is unreachable"),
                ),
            };
            return reply(api, status, Err(error)).await;
        }
    };
    let status = StatusCode::from_u16(body.status()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut headers = body.headers().clone();
    // The length stays when the upstream gave one; the rest is hyper's to decide.
    for name in [header::TRANSFER_ENCODING, header::CONNECTION] {
        headers.remove(name);
    }
    let upstream = body.into_response();
    (status, headers, Body::from_stream(upstream.bytes_stream())).into_response()
}

fn server_error(kind: SubsonicServerError, message: &str) -> SubsonicError {
    SubsonicError::Server {
        kind,
        message: message.into(),
        rune_reason: None,
    }
}

/// `bytes=start-end` or `bytes=start-`; other forms are served whole.
fn parse_range(value: &str) -> Option<(u64, Option<u64>)> {
    let (start, end) = value.strip_prefix("bytes=")?.split_once('-')?;
    let start = start.parse().ok()?;
    let end = if end.is_empty() {
        None
    } else {
        Some(end.parse().ok()?)
    };
    Some((start, end))
}

async fn cover_art(api: &Api, params: &Params) -> Response {
    let result = async {
        let id = params.required("id")?;
        api.cover_art(id, params.number("size")?).await
    }
    .await;
    match result {
        Ok(bytes) => (
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static(sniff_image(&bytes)),
            )],
            bytes,
        )
            .into_response(),
        Err(error) => reply(api, error_status(&error), Err(error)).await,
    }
}

/// The api returns bytes only, so the type comes from their magic number.
fn sniff_image(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        "image/jpeg"
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "application/octet-stream"
    }
}

/// Subsonic sends failures with a success status; only an endpoint the api lacks is not one.
fn error_status(error: &SubsonicError) -> StatusCode {
    match error {
        SubsonicError::Unsupported { .. } => StatusCode::NOT_IMPLEMENTED,
        _ => StatusCode::OK,
    }
}

fn error_code(kind: SubsonicServerError) -> i64 {
    match kind {
        SubsonicServerError::Generic => 0,
        SubsonicServerError::MissingParameter => 10,
        SubsonicServerError::ClientTooOld => 20,
        SubsonicServerError::ServerTooOld => 30,
        SubsonicServerError::WrongCredentials => 40,
        SubsonicServerError::TokenAuthenticationNotSupported => 41,
        SubsonicServerError::AuthenticationMechanismNotSupported => 42,
        SubsonicServerError::ConflictingAuthentication => 43,
        SubsonicServerError::InvalidApiKey => 44,
        SubsonicServerError::NotAuthorized => 50,
        SubsonicServerError::NotFound => 70,
        SubsonicServerError::Unknown(code) => code,
    }
}

/// The envelope's identity fields, from the api's own probe so that every response agrees.
async fn identity(api: &Api) -> Map<String, Value> {
    let mut map = Map::new();
    match api.probe().await {
        Ok(probe) => {
            map.insert("version".into(), probe.api_version.into());
            map.insert("openSubsonic".into(), probe.open_subsonic.into());
            if let Some(server_type) = probe.server_type {
                map.insert("type".into(), server_type.into());
            }
            if let Some(version) = probe.server_version {
                map.insert("serverVersion".into(), version.into());
            }
        }
        Err(_) => {
            map.insert("version".into(), API_VERSION.into());
            map.insert("openSubsonic".into(), false.into());
        }
    }
    map
}

fn error_object(error: &SubsonicError) -> Value {
    let (code, message, reason) = match error {
        SubsonicError::Server {
            kind,
            message,
            rune_reason,
        } => (error_code(*kind), message.clone(), *rune_reason),
        other => (0, other.to_string(), None),
    };
    let mut object = Map::new();
    object.insert("code".into(), code.into());
    object.insert("message".into(), message.into());
    if let Some(reason) = reason {
        object.insert("runeReason".into(), wire::rune_reason(reason).into());
    }
    object.into()
}

/// A JSON response in the `subsonic-response` envelope.
async fn reply(
    api: &Api,
    status: StatusCode,
    outcome: Result<Map<String, Value>, SubsonicError>,
) -> Response {
    let mut response = identity(api).await;
    match outcome {
        Ok(data) => {
            response.insert("status".into(), "ok".into());
            response.extend(data);
        }
        Err(error) => {
            response.insert("status".into(), "failed".into());
            response.insert("error".into(), error_object(&error));
        }
    }
    (status, axum::Json(json!({ "subsonic-response": response }))).into_response()
}
