use anyhow::{Context, Result};
use std::collections::HashMap;

use crate::library_pool::LibraryHandle;
use ::database::actions::collection::CollectionQueryType;
use ::database::actions::search::convert_to_collection_types;
use ::database::actions::search::search_for;

use crate::{
    Session, Signal, TypedRequest,
    messages::*,
    request_error::ExpectedRequestError,
    rpc::UnsupportedReason,
    utils::{GlobalParams, ParamsExtractor, require_library},
};

impl ParamsExtractor for SearchForRequest {
    type Params = (Option<LibraryHandle>,);

    fn extract_params(&self, all_params: &GlobalParams) -> Self::Params {
        (all_params.library.clone(),)
    }
}

impl Signal for SearchForRequest {
    type Params = (Option<LibraryHandle>,);
    type Response = SearchForResponse;

    async fn handle(
        &self,
        (library,): Self::Params,
        _session: Option<Session>,
        dart_signal: &Self,
    ) -> Result<Option<Self::Response>> {
        let request = dart_signal;
        let library = require_library(library)?;
        let query_str = &request.query_str;
        let search_fields = convert_to_collection_types(request.fields.clone());
        let n = request.n as usize;

        let results = search_for(
            &library.main_db,
            query_str,
            if search_fields.is_empty() {
                None
            } else {
                Some(search_fields)
            },
            n,
        )
        .await
        .with_context(|| format!("Search request failed: query_str={query_str}, n={n}"))?;

        Ok(Some(search_response(request.call_id, results)))
    }
}

/// A Rune library holds everything it lists: it has no catalogue beyond it, so these requests
/// are only served by libraries of services that have one.
macro_rules! catalogue_only {
    ($($request:ty => $response:ty),* $(,)?) => {
        $(
            impl ParamsExtractor for $request {
                type Params = ();

                fn extract_params(&self, _: &GlobalParams) -> Self::Params {}
            }

            impl Signal for $request {
                type Params = ();
                type Response = $response;

                async fn handle(
                    &self,
                    _params: Self::Params,
                    _session: Option<Session>,
                    _dart_signal: &Self,
                ) -> Result<Option<Self::Response>> {
                    Err(ExpectedRequestError::Unsupported {
                        request: <$request as TypedRequest>::wire_name(),
                        reason: UnsupportedReason::NotImplemented,
                    }
                    .into())
                }
            }
        )*
    };
}

catalogue_only! {
    FetchLibraryMembershipRequest => FetchLibraryMembershipResponse,
    AddToLibraryRequest => AddToLibraryResponse,
}

fn search_response(
    call_id: Option<u64>,
    results: HashMap<CollectionQueryType, Vec<i64>>,
) -> SearchForResponse {
    let mut artists = Vec::new();
    let mut albums = Vec::new();
    let mut playlists = Vec::new();
    let mut tracks = Vec::new();
    let mut genres = Vec::new();

    for (collection_type, ids) in results {
        let ids: Vec<i32> = ids.iter().map(|&x| x as i32).collect();
        match collection_type {
            CollectionQueryType::Artist => artists.extend(ids),
            CollectionQueryType::Album => albums.extend(ids),
            CollectionQueryType::Playlist => playlists.extend(ids),
            CollectionQueryType::Track => tracks.extend(ids),
            CollectionQueryType::Genre => genres.extend(ids),
            _ => {}
        }
    }

    SearchForResponse {
        call_id,
        artists,
        albums,
        playlists,
        tracks,
        genres,
    }
}

#[cfg(test)]
mod tests {
    use super::search_response;
    use std::collections::HashMap;

    #[test]
    fn search_handler_response_echoes_call_id() {
        let response = search_response(Some(42), HashMap::new());
        assert_eq!(response.call_id, Some(42));
    }
}
