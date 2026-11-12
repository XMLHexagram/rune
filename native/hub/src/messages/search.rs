use rinf::{DartSignal, RustSignal};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, DartSignal)]
pub struct SearchForRequest {
    #[serde(default)]
    pub call_id: Option<u64>,
    pub query_str: String,
    pub fields: Vec<String>,
    pub n: i32,
}

#[derive(Deserialize, Serialize, RustSignal)]
pub struct SearchForResponse {
    #[serde(default)]
    pub call_id: Option<u64>,
    pub artists: Vec<i32>,
    pub albums: Vec<i32>,
    pub playlists: Vec<i32>,
    pub tracks: Vec<i32>,
    pub genres: Vec<i32>,
}

/// Which of the given items are outside the user's library, for libraries whose service has
/// a catalogue beyond it. Ids are those of search results and collections.
#[derive(Serialize, Deserialize, DartSignal)]
pub struct FetchLibraryMembershipRequest {
    pub track_ids: Vec<i32>,
    pub album_ids: Vec<i32>,
    pub artist_ids: Vec<i32>,
    #[serde(default)]
    pub call_id: Option<u64>,
}

/// The ids of the request that are outside the user's library; the others are in it.
#[derive(Deserialize, Serialize, RustSignal)]
pub struct FetchLibraryMembershipResponse {
    pub outside_track_ids: Vec<i32>,
    pub outside_album_ids: Vec<i32>,
    pub outside_artist_ids: Vec<i32>,
    #[serde(default)]
    pub call_id: Option<u64>,
}

/// Adds catalogue tracks and albums to the user's library.
#[derive(Serialize, Deserialize, DartSignal)]
pub struct AddToLibraryRequest {
    pub track_ids: Vec<i32>,
    pub album_ids: Vec<i32>,
    #[serde(default)]
    pub call_id: Option<u64>,
}

#[derive(Deserialize, Serialize, RustSignal)]
pub struct AddToLibraryResponse {
    pub success: bool,
    #[serde(default)]
    pub call_id: Option<u64>,
}
