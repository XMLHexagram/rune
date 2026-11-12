use anyhow::{Result, anyhow};
use database::actions::utils::generate_group_name;
use database::actions::{collection::CollectionQueryType, search::convert_to_collection_types};
use database::error::ExpectedError;
use lyric::lrc::parse_lrc;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use futures::{StreamExt, TryStreamExt, stream};
use subsonic::{
    RuneEdit, RuneScope, RuneServiceDescriptor, SearchLimits, SubsonicAlbum, SubsonicArtist,
    SubsonicError, SubsonicProbe, SubsonicServerError, SubsonicSong,
};

use crate::TypedRequest;
use crate::handlers::playback::KEEP_PLAYBACK_MODE;
use crate::macros::{RequestSlots, RequestTranslator};
use crate::messages::{
    AddItemToPlaylistRequest, AddItemToPlaylistResponse, AddToLibraryRequest, AddToLibraryResponse,
    Album, Artist, AudioAvailability, AudioOrigin, CastPreflightRequest, Collection,
    CollectionGroup, CollectionGroupSummary, CollectionGroupSummaryResponse, CollectionType,
    CreatePlaylistRequest, CreatePlaylistResponse, FetchAllPlaylistsRequest,
    FetchAllPlaylistsResponse, FetchCollectionByIdsRequest, FetchCollectionByIdsResponse,
    FetchCollectionGroupSummaryRequest, FetchCollectionGroupsRequest,
    FetchCollectionGroupsResponse, FetchLibraryMembershipRequest, FetchLibraryMembershipResponse,
    FetchMediaFileByIdsRequest, FetchMediaFileByIdsResponse, FetchMediaFilesRequest,
    FetchMediaFilesResponse, FetchMixQueriesRequest, FetchMixQueriesResponse,
    FetchParsedMediaFileRequest, FetchParsedMediaFileResponse, FetchPlaylistOccurrencesRequest,
    FetchPlaylistOccurrencesResponse, GetLikedRequest, GetLikedResponse, GetLyricByTrackIdRequest,
    GetLyricByTrackIdResponse, GetMediaFilesCountRequest, GetMediaFilesCountResponse,
    GetMixByIdRequest, GetMixByIdResponse, GetPlaylistByIdRequest, GetPlaylistByIdResponse,
    HandoffLoadRequest, LoadRequest, LoadResolvedItemsRequest, LoadResolvedItemsResponse,
    LyricContentLine, LyricContentLineSection, MediaFile, MediaFileSummary, Mix, MixQuery,
    MixQueryRequest, MixQueryResponse, MovePlaylistItemRequest, NextRequest, PauseRequest,
    PlayRequest, PlayingItemRef, Playlist, PlaylistOccurrence, PlaylistOccurrenceCursor,
    PlaylistOperateMode, PreviousRequest, RefreshSessionRequest, RemoveItemFromPlaylistRequest,
    RemoveItemFromPlaylistResponse, RemovePlaylistRequest, RemovePlaylistResponse, RemoveRequest,
    ReorderPlaylistItemPositionRequest, ReorderPlaylistItemPositionResponse,
    SearchCollectionSummaryRequest, SearchCollectionSummaryResponse, SearchForRequest,
    SearchForResponse, SearchMediaFileSummaryRequest, SearchMediaFileSummaryResponse, SeekRequest,
    SessionSnapshotRequest, SessionSnapshotResponse, SetAdaptiveSwitchingEnabledRequest,
    SetLikedRequest, SetLikedResponse, SetPlaybackModeRequest, SetRealtimeFFTEnabledRequest,
    StoredMixQuery, SwitchRequest, TakeBackHandoffBlockRequest, UpdatePlaylistRequest,
    UpdatePlaylistResponse, VolumeRequest, VolumeResponse,
};
use crate::request_error::ExpectedRequestError;
use crate::rpc::{RequestTranslationFuture, Slot, UnsupportedReason};
use crate::subsonic_binding::{
    DetailQuery, MixTarget, RecentRead, RecommendedMix, SubsonicBinding, SubsonicIdKind,
};
use database::actions::mixes::{QueryOperator, parse_query};
use uuid::Uuid;

fn subsonic_track_id(binding: &SubsonicBinding, item: &Option<PlayingItemRef>) -> Result<String> {
    match item {
        Some(PlayingItemRef::LibraryTrack { library_id, track })
            if library_id == &binding.library_id.to_string() =>
        {
            Ok(track.clone())
        }
        Some(PlayingItemRef::LibraryTrack { .. }) => {
            Err(ExpectedRequestError::invalid_input("Item belongs to another library").into())
        }
        _ => Err(ExpectedRequestError::invalid_input("Expected a Subsonic library track").into()),
    }
}

pub(crate) async fn set_liked_request(
    binding: &SubsonicBinding,
    request: &SetLikedRequest,
) -> Result<Option<SetLikedResponse>> {
    let track = subsonic_track_id(binding, &request.item)?;
    if request.liked {
        binding
            .client
            .star(&[&track])
            .await
            .map_err(subsonic_request_error)?;
    } else {
        binding
            .client
            .unstar(&[&track])
            .await
            .map_err(subsonic_request_error)?;
    }
    Ok(request.item.clone().map(|item| SetLikedResponse {
        item,
        liked: request.liked,
        success: true,
        call_id: request.call_id,
    }))
}

pub(crate) async fn get_liked_request(
    binding: &SubsonicBinding,
    request: &GetLikedRequest,
) -> Result<Option<GetLikedResponse>> {
    let track = subsonic_track_id(binding, &request.item)?;
    let song = binding
        .client
        .song(&track)
        .await
        .map_err(subsonic_request_error)?;
    Ok(request.item.clone().map(|item| GetLikedResponse {
        item,
        liked: song.starred.is_some(),
        call_id: request.call_id,
    }))
}

pub(crate) async fn get_lyric_request(
    binding: &SubsonicBinding,
    request: &GetLyricByTrackIdRequest,
) -> Result<Option<GetLyricByTrackIdResponse>> {
    let track = subsonic_track_id(binding, &request.item)?;
    let song = binding
        .client
        .song(&track)
        .await
        .map_err(subsonic_request_error)?;
    let needs_extension = binding
        .probe
        .extensions
        .iter()
        .any(|(name, _)| name == "songLyrics");
    let lyrics = if needs_extension {
        binding
            .client
            .lyrics_by_song_id(&track)
            .await
            .map_err(subsonic_request_error)?
    } else {
        binding
            .client
            .lyrics(song.artist.as_deref().unwrap_or_default(), &song.title)
            .await
            .map_err(subsonic_request_error)?
    };
    let mut lines = Vec::new();
    if let Some(lyric) = lyrics
        .synced_lyrics
        .into_iter()
        .find(|lyrics| lyrics.synced)
    {
        for (index, line) in lyric.lines.iter().enumerate() {
            let start = line.start.min(i32::MAX as u32) as i32;
            let end = lyric
                .lines
                .get(index + 1)
                .map(|next| next.start.min(i32::MAX as u32) as i32)
                .unwrap_or(start);
            lines.push(LyricContentLine {
                start_time: start,
                end_time: end,
                sections: vec![LyricContentLineSection {
                    start_time: start,
                    end_time: end,
                    content: line.value.clone(),
                }],
            });
        }
    } else if let Some(plain) = lyrics.plain_lyrics.filter(|value| !value.trim().is_empty()) {
        let parsed_lines = parse_lrc(&plain).ok().map(|parsed| parsed.lyrics);
        if let Some(parsed_lines) = parsed_lines.filter(|lines| !lines.is_empty()) {
            lines = parsed_lines
                .into_iter()
                .map(|line| {
                    let start_time = line.start_time.into();
                    let end_time = line.end_time.into();
                    let content = line.text;
                    LyricContentLine {
                        start_time,
                        end_time,
                        sections: vec![LyricContentLineSection {
                            start_time,
                            end_time,
                            content,
                        }],
                    }
                })
                .collect();
        } else {
            lines = plain
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| LyricContentLine {
                    start_time: 0,
                    end_time: 0,
                    sections: vec![LyricContentLineSection {
                        start_time: 0,
                        end_time: 0,
                        content: line.to_owned(),
                    }],
                })
                .collect();
        }
    }
    Ok(request.item.clone().map(|item| GetLyricByTrackIdResponse {
        item,
        lines,
        call_id: request.call_id,
    }))
}

fn resolve_playlist_id(binding: &SubsonicBinding, playlist_id: i32) -> Result<String> {
    let catalog = binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
    Ok(catalog
        .ids
        .resolve(SubsonicIdKind::Playlist, playlist_id)?
        .to_owned())
}

fn playlist_id(binding: &SubsonicBinding, remote_id: &str) -> Result<i32> {
    binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?
        .ids
        .intern(SubsonicIdKind::Playlist, remote_id)
}

async fn fetch_remote_playlist(
    binding: &SubsonicBinding,
    id: i32,
) -> Result<(String, Vec<String>)> {
    let remote_id = resolve_playlist_id(binding, id)?;
    let playlist = binding
        .client
        .playlist(&remote_id)
        .await
        .map_err(subsonic_request_error)?;
    remember_playlist_edit(binding, id, playlist.rune_edit)?;
    Ok((
        remote_id,
        playlist.songs.into_iter().map(|song| song.id).collect(),
    ))
}

/// Refuse a call the service says it cannot answer, when only part of a request needs it.
fn require_endpoint(
    binding: &SubsonicBinding,
    endpoint: &'static str,
    params: &[&str],
) -> Result<()> {
    if binding.rune_unsupported(endpoint, params) {
        return Err(ExpectedRequestError::Unsupported {
            request: endpoint,
            reason: UnsupportedReason::NotImplemented,
        }
        .into());
    }
    Ok(())
}

/// What an edit changes in a playlist, to compare with the server's `runeEdit`.
#[derive(Clone, Copy)]
enum PlaylistEdit {
    Rename,
    Delete,
    Append,
    RemoveItem,
    Reorder,
}

fn remember_playlist_edit(
    binding: &SubsonicBinding,
    playlist: i32,
    edit: Option<RuneEdit>,
) -> Result<()> {
    let mut catalog = binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
    match edit {
        Some(edit) => catalog.playlist_edits.insert(playlist, edit),
        None => catalog.playlist_edits.remove(&playlist),
    };
    Ok(())
}

/// Forgets the pages kept of a playlist once an edit of it was sent, whatever the answer.
fn forget_playlist_reads(binding: &SubsonicBinding, playlist: i32) -> Result<()> {
    binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?
        .forget_playlist_reads(playlist);
    Ok(())
}

/// `appendOnly` playlists only take songs added at the end; `readOnly` ones take nothing. A
/// playlist the server said nothing about is left to the server.
fn edit_allowed(known: Option<RuneEdit>, edit: PlaylistEdit) -> bool {
    match known {
        Some(RuneEdit::ReadOnly) => false,
        Some(RuneEdit::AppendOnly) => matches!(edit, PlaylistEdit::Append),
        Some(RuneEdit::Full | RuneEdit::Unknown) | None => true,
    }
}

/// Refuse an edit on what the server said when the playlist was just fetched, so the
/// remembered value is fresh.
fn check_playlist_edit(
    binding: &SubsonicBinding,
    playlist: i32,
    edit: PlaylistEdit,
    request: &'static str,
) -> Result<()> {
    let known = binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?
        .playlist_edits
        .get(&playlist)
        .copied();
    if edit_allowed(known, edit) {
        Ok(())
    } else {
        Err(ExpectedRequestError::Unsupported {
            request,
            reason: UnsupportedReason::NotImplemented,
        }
        .into())
    }
}

/// For edits that do not read the playlist first. A remembered value can be stale, so it only
/// lets the edit through; one that would refuse it is checked against a fresh read.
async fn check_unfetched_playlist_edit(
    binding: &SubsonicBinding,
    playlist: i32,
    remote_id: &str,
    edit: PlaylistEdit,
    request: &'static str,
) -> Result<()> {
    let remembered = binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?
        .playlist_edits
        .get(&playlist)
        .copied();
    if edit_allowed(remembered, edit) {
        return Ok(());
    }
    let fresh = binding
        .client
        .playlist(remote_id)
        .await
        .map_err(subsonic_request_error)?;
    remember_playlist_edit(binding, playlist, fresh.rune_edit)?;
    check_playlist_edit(binding, playlist, edit, request)
}

pub(crate) async fn fetch_all_playlists_request(
    binding: &SubsonicBinding,
    _: &FetchAllPlaylistsRequest,
) -> Result<Option<FetchAllPlaylistsResponse>> {
    let remote = binding
        .client
        .playlists()
        .await
        .map_err(subsonic_request_error)?;
    let mut catalog = binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
    let playlists = remote
        .into_iter()
        .map(|item| {
            let id = catalog.ids.intern(SubsonicIdKind::Playlist, &item.id)?;
            match item.rune_edit {
                Some(edit) => catalog.playlist_edits.insert(id, edit),
                None => catalog.playlist_edits.remove(&id),
            };
            Ok(Playlist {
                id,
                name: item.name,
                group: String::new(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(FetchAllPlaylistsResponse {
        playlists,
        call_id: None,
    }))
}

pub(crate) async fn get_playlist_by_id_request(
    binding: &SubsonicBinding,
    request: &GetPlaylistByIdRequest,
) -> Result<Option<GetPlaylistByIdResponse>> {
    let remote_id = resolve_playlist_id(binding, request.playlist_id)?;
    let remote = binding
        .client
        .playlist(&remote_id)
        .await
        .map_err(subsonic_request_error)?;
    let id = playlist_id(binding, &remote.id)?;
    remember_playlist_edit(binding, id, remote.rune_edit)?;
    Ok(Some(GetPlaylistByIdResponse {
        playlist: Playlist {
            id,
            name: remote.name,
            group: String::new(),
        },
        call_id: None,
    }))
}

pub(crate) async fn create_playlist_request(
    binding: &SubsonicBinding,
    request: &CreatePlaylistRequest,
) -> Result<Option<CreatePlaylistResponse>> {
    let existing_ids = binding
        .client
        .playlists()
        .await
        .map_err(subsonic_request_error)?
        .into_iter()
        .map(|item| item.id)
        .collect::<HashSet<_>>();
    binding
        .client
        .create_playlist(&request.name, &[])
        .await
        .map_err(subsonic_request_error)?;
    let remote = binding
        .client
        .playlists()
        .await
        .map_err(subsonic_request_error)?;
    let created = remote
        .into_iter()
        .find(|item| item.name == request.name && !existing_ids.contains(&item.id))
        .ok_or_else(|| ExpectedRequestError::UnexpectedResponse {
            expected: "created Subsonic playlist".into(),
            actual: "no matching playlist returned".into(),
        })?;
    Ok(Some(CreatePlaylistResponse {
        playlist: Playlist {
            id: playlist_id(binding, &created.id)?,
            name: created.name,
            group: String::new(),
        },
        call_id: None,
    }))
}

pub(crate) async fn update_playlist_request(
    binding: &SubsonicBinding,
    request: &UpdatePlaylistRequest,
) -> Result<Option<UpdatePlaylistResponse>> {
    let remote_id = resolve_playlist_id(binding, request.playlist_id)?;
    check_unfetched_playlist_edit(
        binding,
        request.playlist_id,
        &remote_id,
        PlaylistEdit::Rename,
        UpdatePlaylistRequest::wire_name(),
    )
    .await?;
    let renamed = binding
        .client
        .update_playlist(&remote_id, Some(&request.name), &[], &[])
        .await;
    forget_playlist_reads(binding, request.playlist_id)?;
    renamed.map_err(subsonic_request_error)?;
    Ok(Some(UpdatePlaylistResponse {
        playlist: Playlist {
            id: request.playlist_id,
            name: request.name.clone(),
            group: String::new(),
        },
        call_id: None,
    }))
}

pub(crate) async fn remove_playlist_request(
    binding: &SubsonicBinding,
    request: &RemovePlaylistRequest,
) -> Result<Option<RemovePlaylistResponse>> {
    let remote_id = resolve_playlist_id(binding, request.playlist_id)?;
    check_unfetched_playlist_edit(
        binding,
        request.playlist_id,
        &remote_id,
        PlaylistEdit::Delete,
        RemovePlaylistRequest::wire_name(),
    )
    .await?;
    let deleted = binding.client.delete_playlist(&remote_id).await;
    forget_playlist_reads(binding, request.playlist_id)?;
    deleted.map_err(subsonic_request_error)?;
    Ok(Some(RemovePlaylistResponse {
        playlist_id: request.playlist_id,
        success: true,
        call_id: None,
    }))
}

pub(crate) async fn add_item_to_playlist_request(
    binding: &SubsonicBinding,
    request: &AddItemToPlaylistRequest,
) -> Result<Option<AddItemToPlaylistResponse>> {
    let playlist_remote = resolve_playlist_id(binding, request.playlist_id)?;
    let track_remote = {
        let catalog = binding
            .catalog
            .lock()
            .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
        catalog
            .ids
            .resolve(SubsonicIdKind::Track, request.media_file_id)?
            .to_owned()
    };
    let playlist = binding
        .client
        .playlist(&playlist_remote)
        .await
        .map_err(subsonic_request_error)?;
    remember_playlist_edit(binding, request.playlist_id, playlist.rune_edit)?;
    check_playlist_edit(
        binding,
        request.playlist_id,
        PlaylistEdit::Append,
        AddItemToPlaylistRequest::wire_name(),
    )?;
    let songs = playlist
        .songs
        .iter()
        .map(|song| song.id.clone())
        .collect::<Vec<_>>();
    let append_position = i32::try_from(songs.len())
        .map_err(|_| ExpectedRequestError::invalid_input("Subsonic playlist is too large"))?;
    if request
        .position
        .is_some_and(|position| position != append_position)
    {
        return Err(ExpectedRequestError::Unsupported {
            request: AddItemToPlaylistRequest::wire_name(),
            reason: UnsupportedReason::NotImplemented,
        }
        .into());
    }
    let added = binding
        .client
        .update_playlist(&playlist_remote, None, &[&track_remote], &[])
        .await;
    forget_playlist_reads(binding, request.playlist_id)?;
    added.map_err(subsonic_request_error)?;
    Ok(Some(AddItemToPlaylistResponse {
        success: true,
        occurrence_id: append_position.to_string(),
        call_id: None,
    }))
}

fn occurrence_position(value: &str, len: usize) -> Result<usize> {
    let position = value
        .parse::<usize>()
        .map_err(|_| ExpectedRequestError::invalid_input("Invalid Subsonic playlist occurrence"))?;
    if position >= len {
        return Err(
            ExpectedRequestError::invalid_input("Unknown Subsonic playlist occurrence").into(),
        );
    }
    Ok(position)
}

pub(crate) async fn remove_item_from_playlist_request(
    binding: &SubsonicBinding,
    request: &RemoveItemFromPlaylistRequest,
) -> Result<Option<RemoveItemFromPlaylistResponse>> {
    let (remote_id, songs) = fetch_remote_playlist(binding, request.playlist_id).await?;
    check_playlist_edit(
        binding,
        request.playlist_id,
        PlaylistEdit::RemoveItem,
        RemoveItemFromPlaylistRequest::wire_name(),
    )?;
    let position = occurrence_position(&request.occurrence_id, songs.len())?;
    let position = u32::try_from(position)
        .map_err(|_| ExpectedRequestError::invalid_input("Subsonic playlist is too large"))?;
    let removed = binding
        .client
        .update_playlist(&remote_id, None, &[], &[position])
        .await;
    forget_playlist_reads(binding, request.playlist_id)?;
    removed.map_err(subsonic_request_error)?;
    Ok(Some(RemoveItemFromPlaylistResponse { call_id: None }))
}

pub(crate) async fn reorder_playlist_item_position_request(
    binding: &SubsonicBinding,
    request: &ReorderPlaylistItemPositionRequest,
) -> Result<Option<ReorderPlaylistItemPositionResponse>> {
    let (remote_id, songs) = fetch_remote_playlist(binding, request.playlist_id).await?;
    check_playlist_edit(
        binding,
        request.playlist_id,
        PlaylistEdit::Reorder,
        ReorderPlaylistItemPositionRequest::wire_name(),
    )?;
    let from = occurrence_position(&request.occurrence_id, songs.len())?;
    let target = usize::try_from(request.new_position)
        .map_err(|_| ExpectedRequestError::invalid_input("Invalid playlist position"))?;
    if target >= songs.len() {
        return Err(ExpectedRequestError::invalid_input("Invalid playlist position").into());
    }
    let mut reordered = songs;
    let song = reordered.remove(from);
    reordered.insert(target, song);
    let refs = reordered.iter().map(String::as_str).collect::<Vec<_>>();
    let reordered = binding
        .client
        .replace_playlist_songs(&remote_id, &refs)
        .await;
    forget_playlist_reads(binding, request.playlist_id)?;
    reordered.map_err(subsonic_request_error)?;
    Ok(Some(ReorderPlaylistItemPositionResponse {
        success: true,
        call_id: None,
    }))
}

/// The largest page of playlist occurrences, as for Rune's own playlists.
const MAX_OCCURRENCE_PAGE: u32 = 128;

/// A page of a playlist's entries in the server's order. Subsonic entries have no ids of their
/// own, so an occurrence is named by its 0-based position, as the playlist edits above take it.
/// The playlist read for a page serves the following pages for [`RECENT_READ`]; edits of it
/// forget it.
pub(crate) async fn fetch_playlist_occurrences_request(
    binding: &SubsonicBinding,
    request: &FetchPlaylistOccurrencesRequest,
) -> Result<Option<FetchPlaylistOccurrencesResponse>> {
    if !(1..=MAX_OCCURRENCE_PAGE).contains(&request.page_size) {
        return Err(ExpectedRequestError::invalid_input(format!(
            "Playlist page size must be 1..={MAX_OCCURRENCE_PAGE}"
        ))
        .into());
    }
    let start = match &request.after {
        None => 0,
        Some(cursor) => usize::try_from(cursor.position)
            .ok()
            .filter(|position| cursor.occurrence_id == position.to_string())
            .ok_or_else(|| ExpectedRequestError::invalid_input("Invalid Subsonic playlist cursor"))?
            .saturating_add(1),
    };
    let playlist = recent_playlist(binding, request.playlist_id).await?;
    let total = playlist.songs.len();
    // A cursor names an entry of this playlist: the last of the page before.
    if request.after.is_some() && start > total {
        return Err(ExpectedRequestError::invalid_input("Invalid Subsonic playlist cursor").into());
    }
    let mut catalog = binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
    let entries = playlist
        .songs
        .iter()
        .enumerate()
        .skip(start)
        .take(usize::try_from(request.page_size).unwrap_or(usize::MAX))
        .map(|(position, song)| {
            let id = catalog.ids.intern(SubsonicIdKind::Track, &song.id)?;
            catalog.remember_song(id, song.clone());
            let cover_art_id = song
                .cover_art
                .as_deref()
                .map(|cover| catalog.ids.intern(SubsonicIdKind::CoverArt, cover))
                .transpose()?
                .unwrap_or_default();
            let position = i32::try_from(position).map_err(|_| {
                ExpectedRequestError::invalid_input("Subsonic playlist is too large")
            })?;
            let occurrence = PlaylistOccurrence {
                occurrence_id: position.to_string(),
                position,
                track_uuid: song.id.clone(),
                file: Some(subsonic_media_file(binding, id, cover_art_id, song)),
            };
            Ok((occurrence, id, song.cover_art.clone()))
        })
        .collect::<Result<Vec<_>>>()?;
    drop(catalog);
    let next = entries
        .last()
        .filter(|_| start.saturating_add(entries.len()) < total)
        .map(|(last, _, _)| PlaylistOccurrenceCursor {
            position: last.position,
            occurrence_id: last.occurrence_id.clone(),
        });
    let cover_art_map = if request.bake_cover_arts {
        subsonic_cover_map(
            binding,
            entries
                .iter()
                .filter_map(|(_, id, cover)| cover.clone().map(|cover| (*id, cover)))
                .collect(),
        )
    } else {
        HashMap::new()
    };
    Ok(Some(FetchPlaylistOccurrencesResponse {
        request_id: request.request_id.clone(),
        entries: entries
            .into_iter()
            .map(|(occurrence, _, _)| occurrence)
            .collect(),
        next,
        cover_art_map,
        call_id: None,
    }))
}

/// A playlist as read for occurrence pages: the one kept from a read less than
/// [`RECENT_READ`] ago, else read afresh and kept.
async fn recent_playlist(
    binding: &SubsonicBinding,
    playlist_id: i32,
) -> Result<Arc<subsonic::SubsonicPlaylistDetail>> {
    let now = Instant::now();
    let recent = {
        let mut catalog = binding
            .catalog
            .lock()
            .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
        catalog
            .recent_playlists
            .retain(|_, recent| recent.fresh(&(), now, RECENT_READ).is_some());
        catalog
            .recent_playlists
            .get(&playlist_id)
            .and_then(|recent| recent.fresh(&(), now, RECENT_READ))
    };
    if let Some(playlist) = recent {
        return Ok(playlist);
    }
    let remote_id = resolve_playlist_id(binding, playlist_id)?;
    let playlist = Arc::new(
        binding
            .client
            .playlist(&remote_id)
            .await
            .map_err(subsonic_request_error)?,
    );
    remember_playlist_edit(binding, playlist_id, playlist.rune_edit)?;
    binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?
        .recent_playlists
        .insert(
            playlist_id,
            RecentRead {
                key: (),
                read: now,
                value: Arc::clone(&playlist),
            },
        );
    Ok(playlist)
}

fn subsonic_cover_map(
    binding: &SubsonicBinding,
    covers: Vec<(i32, String)>,
) -> std::collections::HashMap<i32, String> {
    let mut map = HashMap::new();
    for (object_id, cover_id) in covers {
        if let Some(path) = binding.covers.cached_path(&cover_id) {
            map.insert(object_id, path);
        } else {
            binding
                .covers
                .schedule(Arc::clone(&binding.client), cover_id);
        }
    }
    map
}

pub(crate) async fn search_for_request(
    binding: &SubsonicBinding,
    request: &SearchForRequest,
) -> Result<Option<SearchForResponse>> {
    let fields = convert_to_collection_types(request.fields.clone());
    let all = fields.is_empty();
    let count = u32::try_from(request.n.max(0)).unwrap_or(u32::MAX);
    let limits = SearchLimits {
        artists: if all || fields.contains(&CollectionQueryType::Artist) {
            count
        } else {
            0
        },
        albums: if all || fields.contains(&CollectionQueryType::Album) {
            count
        } else {
            0
        },
        songs: if all || fields.contains(&CollectionQueryType::Track) {
            count
        } else {
            0
        },
    };
    // A service with a catalogue answers from it too, after the library and marked apart.
    let scoped = binding.probe.supports_rune_catalog()
        && !binding.rune_unsupported("search3", &["runeScope"]);
    let results = if scoped {
        binding
            .client
            .search_in_scope(&request.query_str, limits, 0, RuneScope::All)
            .await
    } else {
        binding.client.search(&request.query_str, limits).await
    }
    .map_err(subsonic_request_error)?;
    let mut catalog = binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
    let artists = results
        .artists
        .iter()
        .take(limits.artists as usize)
        .map(|item| {
            let id = catalog.ids.intern(SubsonicIdKind::Artist, &item.id)?;
            catalog.remember_artist(id, item.clone());
            Ok(id)
        })
        .collect::<Result<Vec<_>>>()?;
    let albums = results
        .albums
        .iter()
        .take(limits.albums as usize)
        .map(|item| {
            let id = catalog.ids.intern(SubsonicIdKind::Album, &item.id)?;
            catalog.remember_album(id, item.clone());
            Ok(id)
        })
        .collect::<Result<Vec<_>>>()?;
    let tracks = results
        .songs
        .iter()
        .take(limits.songs as usize)
        .map(|item| {
            let id = catalog.ids.intern(SubsonicIdKind::Track, &item.id)?;
            catalog.remember_song(id, item.clone());
            Ok(id)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(SearchForResponse {
        call_id: request.call_id,
        artists,
        albums,
        playlists: Vec::new(),
        tracks,
        genres: Vec::new(),
    }))
}

pub(crate) async fn fetch_media_file_by_ids_request(
    binding: &SubsonicBinding,
    request: &FetchMediaFileByIdsRequest,
) -> Result<Option<FetchMediaFileByIdsResponse>> {
    let mut media_files = Vec::with_capacity(request.ids.len());
    let mut covers = Vec::with_capacity(request.ids.len());
    for id in &request.ids {
        let (remote_id, cached_song) = {
            let catalog = binding
                .catalog
                .lock()
                .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
            (
                catalog.ids.resolve(SubsonicIdKind::Track, *id)?.to_owned(),
                catalog.songs.get(id).cloned(),
            )
        };
        let song = match cached_song {
            Some(song) => song,
            None => {
                let song = binding
                    .client
                    .song(&remote_id)
                    .await
                    .map_err(subsonic_request_error)?;
                let mut catalog = binding
                    .catalog
                    .lock()
                    .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
                let id = catalog.ids.intern(SubsonicIdKind::Track, &song.id)?;
                catalog.remember_song(id, song.clone());
                song
            }
        };
        let cover_art_id = {
            let mut catalog = binding
                .catalog
                .lock()
                .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
            song.cover_art
                .as_deref()
                .map(|cover| catalog.ids.intern(SubsonicIdKind::CoverArt, cover))
                .transpose()?
                .unwrap_or_default()
        };
        if let Some(cover) = song.cover_art.clone() {
            covers.push((*id, cover));
        }
        media_files.push(subsonic_media_file(binding, *id, cover_art_id, &song));
    }
    Ok(Some(FetchMediaFileByIdsResponse {
        call_id: None,
        media_files,
        cover_art_map: subsonic_cover_map(binding, covers),
    }))
}

fn empty_search_error(error: SubsonicError, request: &'static str) -> anyhow::Error {
    match error {
        SubsonicError::Server {
            kind: SubsonicServerError::MissingParameter,
            ..
        } => ExpectedRequestError::Unsupported {
            request,
            reason: UnsupportedReason::NotImplemented,
        }
        .into(),
        error => subsonic_request_error(error),
    }
}

fn media_file_from_song(
    binding: &SubsonicBinding,
    id: i32,
    song: &SubsonicSong,
) -> Result<MediaFile> {
    let cover_art_id = {
        let mut catalog = binding
            .catalog
            .lock()
            .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
        song.cover_art
            .as_deref()
            .map(|cover| catalog.ids.intern(SubsonicIdKind::CoverArt, cover))
            .transpose()?
            .unwrap_or_default()
    };
    Ok(subsonic_media_file(binding, id, cover_art_id, song))
}

/// Subsonic tracks stream from the server: playable here, not stored here.
const SUBSONIC_AUDIO_AVAILABILITY: AudioAvailability = AudioAvailability::RemoteOnly;

/// The `MediaFile` for a Subsonic song whose ids are already interned.
fn subsonic_media_file(
    binding: &SubsonicBinding,
    id: i32,
    cover_art_id: i32,
    song: &SubsonicSong,
) -> MediaFile {
    MediaFile {
        id,
        library_id: binding.library_id.to_string(),
        track: song.id.clone(),
        path: String::new(),
        artist: song.artist.clone().unwrap_or_default(),
        album: song.album.clone().unwrap_or_default(),
        title: song.title.clone(),
        duration: f64::from(song.duration.unwrap_or_default()),
        cover_art_id,
        track_number: song
            .track
            .and_then(|number| i32::try_from(number).ok())
            .unwrap_or_default(),
        audio_availability: SUBSONIC_AUDIO_AVAILABILITY,
        audio_origin: AudioOrigin::Remote,
    }
}

pub(crate) async fn fetch_media_files_request(
    binding: &SubsonicBinding,
    request: &FetchMediaFilesRequest,
) -> Result<Option<FetchMediaFilesResponse>> {
    let offset = u32::try_from(request.cursor)
        .map_err(|error| ExpectedRequestError::invalid_input(format!("Invalid cursor: {error}")))?;
    let count = u32::try_from(request.page_size).map_err(|error| {
        ExpectedRequestError::invalid_input(format!("Invalid page size: {error}"))
    })?;
    let songs = binding
        .client
        .search_with_song_offset(
            "",
            SearchLimits {
                songs: count,
                ..Default::default()
            },
            offset,
        )
        .await
        .map_err(|error| empty_search_error(error, FetchMediaFilesRequest::wire_name()))?
        .songs;
    let mut media_files = Vec::with_capacity(songs.len());
    let mut covers = Vec::new();
    for song in songs {
        let id = {
            let mut catalog = binding
                .catalog
                .lock()
                .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
            let id = catalog.ids.intern(SubsonicIdKind::Track, &song.id)?;
            catalog.remember_song(id, song.clone());
            id
        };
        if let Some(cover) = song.cover_art.clone() {
            covers.push((id, cover));
        }
        media_files.push(media_file_from_song(binding, id, &song)?);
    }
    let cover_art_map = if request.bake_cover_arts {
        subsonic_cover_map(binding, covers)
    } else {
        HashMap::new()
    };
    Ok(Some(FetchMediaFilesResponse {
        media_files,
        cover_art_map,
        call_id: request.call_id,
    }))
}

pub(crate) async fn get_media_files_count_request(
    binding: &SubsonicBinding,
    request: &GetMediaFilesCountRequest,
) -> Result<Option<GetMediaFilesCountResponse>> {
    let scan_status = binding
        .client
        .scan_status()
        .await
        .map_err(subsonic_request_error)?;
    let count = scan_status
        .count
        .ok_or_else(|| ExpectedRequestError::UnexpectedResponse {
            expected: "Subsonic scan status song count".into(),
            actual: "scan status omitted count".into(),
        })?;
    Ok(Some(GetMediaFilesCountResponse {
        count: i32::try_from(count).map_err(|error| {
            ExpectedRequestError::invalid_input(format!("Invalid song count: {error}"))
        })?,
        call_id: request.call_id,
    }))
}

pub(crate) async fn fetch_parsed_media_file_request(
    binding: &SubsonicBinding,
    request: &FetchParsedMediaFileRequest,
) -> Result<Option<FetchParsedMediaFileResponse>> {
    let remote_id = {
        let catalog = binding
            .catalog
            .lock()
            .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
        catalog
            .ids
            .resolve(SubsonicIdKind::Track, request.id)?
            .to_owned()
    };
    let song = binding
        .client
        .song(&remote_id)
        .await
        .map_err(subsonic_request_error)?;
    let mut catalog = binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
    let id = catalog.ids.intern(SubsonicIdKind::Track, &song.id)?;
    catalog.remember_song(id, song.clone());
    let artists = song
        .artist
        .as_ref()
        .map(|name| -> Result<Artist> {
            let id = song
                .artist_id
                .as_deref()
                .map(|remote| catalog.ids.intern(SubsonicIdKind::Artist, remote))
                .transpose()?;
            Ok(Artist {
                id: id.unwrap_or_default(),
                name: name.clone(),
            })
        })
        .transpose()?
        .into_iter()
        .collect();
    let album_id = song
        .album_id
        .as_deref()
        .map(|remote| catalog.ids.intern(SubsonicIdKind::Album, remote))
        .transpose()?
        .unwrap_or_default();
    let album = Album {
        id: album_id,
        name: song.album.clone().unwrap_or_default(),
    };
    drop(catalog);
    Ok(Some(FetchParsedMediaFileResponse {
        file: media_file_from_song(binding, id, &song)?,
        artists,
        album,
        call_id: request.call_id,
    }))
}

pub(crate) async fn search_media_file_summary_request(
    binding: &SubsonicBinding,
    request: &SearchMediaFileSummaryRequest,
) -> Result<Option<SearchMediaFileSummaryResponse>> {
    let count = u32::try_from(request.n)
        .map_err(|error| ExpectedRequestError::invalid_input(format!("Invalid N: {error}")))?;
    let songs = binding
        .client
        .search(
            "",
            SearchLimits {
                songs: count,
                ..Default::default()
            },
        )
        .await
        .map_err(|error| empty_search_error(error, SearchMediaFileSummaryRequest::wire_name()))?
        .songs;
    let mut catalog = binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
    let result = songs
        .into_iter()
        .map(|song| {
            let id = catalog.ids.intern(SubsonicIdKind::Track, &song.id)?;
            let cover_art_id = song
                .cover_art
                .as_deref()
                .map(|cover| catalog.ids.intern(SubsonicIdKind::CoverArt, cover))
                .transpose()?
                .unwrap_or_default();
            catalog.remember_song(id, song.clone());
            Ok(MediaFileSummary {
                id,
                name: song.title,
                cover_art_id,
                audio_availability: SUBSONIC_AUDIO_AVAILABILITY,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(SearchMediaFileSummaryResponse {
        result,
        call_id: request.call_id,
    }))
}

pub(crate) async fn search_collection_summary_request(
    binding: &SubsonicBinding,
    request: &SearchCollectionSummaryRequest,
) -> Result<Option<SearchCollectionSummaryResponse>> {
    let collection_type = request.collection_type.ok_or_else(|| {
        ExpectedRequestError::invalid_input("Subsonic collection summary requires album or artist")
    })?;
    let (kind, limits) = match collection_type {
        CollectionType::Album => (
            SubsonicIdKind::Album,
            SearchLimits {
                albums: u32::try_from(request.n).map_err(|error| {
                    ExpectedRequestError::invalid_input(format!("Invalid N: {error}"))
                })?,
                ..Default::default()
            },
        ),
        CollectionType::Artist => (
            SubsonicIdKind::Artist,
            SearchLimits {
                artists: u32::try_from(request.n).map_err(|error| {
                    ExpectedRequestError::invalid_input(format!("Invalid N: {error}"))
                })?,
                ..Default::default()
            },
        ),
        _ => {
            return Err(ExpectedRequestError::Unsupported {
                request: SearchCollectionSummaryRequest::wire_name(),
                reason: UnsupportedReason::NotImplemented,
            }
            .into());
        }
    };
    let results =
        binding.client.search("", limits).await.map_err(|error| {
            empty_search_error(error, SearchCollectionSummaryRequest::wire_name())
        })?;
    let mut catalog = binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
    let entries = match kind {
        SubsonicIdKind::Album => results
            .albums
            .into_iter()
            .map(|item| {
                let id = catalog.ids.intern(kind, &item.id)?;
                let cover = item.cover_art.clone();
                catalog.remember_album(id, item.clone());
                Ok((id, item.name, cover))
            })
            .collect::<Result<Vec<_>>>()?,
        SubsonicIdKind::Artist => results
            .artists
            .into_iter()
            .map(|item| {
                let id = catalog.ids.intern(kind, &item.id)?;
                let cover = item.cover_art.clone();
                catalog.remember_artist(id, item.clone());
                Ok((id, item.name, cover))
            })
            .collect::<Result<Vec<_>>>()?,
        _ => Vec::new(),
    };
    let collections = entries
        .iter()
        .map(|(id, name, cover)| {
            let cover_art_map = subsonic_cover_map(
                binding,
                cover
                    .clone()
                    .map(|cover| vec![(*id, cover)])
                    .unwrap_or_default(),
            );
            Collection {
                id: *id,
                name: name.clone(),
                queries: Vec::new(),
                collection_type,
                cover_art_map,
                readonly: true,
            }
        })
        .collect();
    Ok(Some(SearchCollectionSummaryResponse {
        collection_type,
        result: collections,
        call_id: request.call_id,
    }))
}

pub(crate) async fn fetch_collection_by_ids_request(
    binding: &SubsonicBinding,
    request: &FetchCollectionByIdsRequest,
) -> Result<Option<FetchCollectionByIdsResponse>> {
    let kind = match request.collection_type {
        CollectionType::Artist => SubsonicIdKind::Artist,
        CollectionType::Album => SubsonicIdKind::Album,
        CollectionType::Mix if binding.probe.supports_rune_recommendations() => {
            return mixes_by_ids(binding, request).await;
        }
        _ => {
            return Err(ExpectedRequestError::Unsupported {
                request: FetchCollectionByIdsRequest::wire_name(),
                reason: UnsupportedReason::NotImplemented,
            }
            .into());
        }
    };
    let mut result = Vec::with_capacity(request.ids.len());
    for id in &request.ids {
        let (remote_id, cached_name) = {
            let catalog = binding
                .catalog
                .lock()
                .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
            let remote_id = catalog.ids.resolve(kind, *id)?.to_owned();
            let cached_name = match kind {
                SubsonicIdKind::Artist => catalog.artists.get(id).map(|artist| artist.name.clone()),
                SubsonicIdKind::Album => catalog.albums.get(id).map(|album| album.name.clone()),
                SubsonicIdKind::Track
                | SubsonicIdKind::CoverArt
                | SubsonicIdKind::Playlist
                | SubsonicIdKind::Mix => None,
            };
            (remote_id, cached_name)
        };
        let name = match cached_name {
            Some(name) => name,
            None => match kind {
                SubsonicIdKind::Artist => {
                    require_endpoint(binding, "getArtist", &["id"])?;
                    let item = binding
                        .client
                        .artist(&remote_id)
                        .await
                        .map_err(subsonic_request_error)?;
                    let name = item.name.clone();
                    let mut catalog = binding
                        .catalog
                        .lock()
                        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
                    catalog.remember_artist(
                        *id,
                        SubsonicArtist {
                            id: item.id,
                            name: item.name,
                            album_count: item.album_count,
                            cover_art: item.cover_art,
                            rune_in_library: item.rune_in_library,
                        },
                    );
                    name
                }
                SubsonicIdKind::Album => {
                    require_endpoint(binding, "getAlbum", &["id"])?;
                    let item = binding
                        .client
                        .album(&remote_id)
                        .await
                        .map_err(subsonic_request_error)?;
                    let name = item.name.clone();
                    let mut catalog = binding
                        .catalog
                        .lock()
                        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
                    catalog.remember_album(
                        *id,
                        SubsonicAlbum {
                            id: item.id,
                            name: item.name,
                            artist: item.artist,
                            artist_id: item.artist_id,
                            song_count: item.song_count,
                            duration: item.duration,
                            year: item.year,
                            genre: item.genre,
                            cover_art: item.cover_art,
                            rune_in_library: item.rune_in_library,
                        },
                    );
                    name
                }
                SubsonicIdKind::Track | SubsonicIdKind::CoverArt => {
                    return Err(anyhow!("Unexpected Subsonic track collection"));
                }
                SubsonicIdKind::Playlist | SubsonicIdKind::Mix => {
                    return Err(anyhow!("Unexpected Subsonic playlist collection"));
                }
            },
        };
        let cover_id = {
            let mut catalog = binding
                .catalog
                .lock()
                .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
            let cover = match kind {
                SubsonicIdKind::Artist => catalog
                    .artists
                    .get(id)
                    .and_then(|item| item.cover_art.clone()),
                SubsonicIdKind::Album => catalog
                    .albums
                    .get(id)
                    .and_then(|item| item.cover_art.clone()),
                SubsonicIdKind::Track
                | SubsonicIdKind::CoverArt
                | SubsonicIdKind::Playlist
                | SubsonicIdKind::Mix => None,
            };
            if let Some(cover_id) = cover.as_deref() {
                catalog.ids.intern(SubsonicIdKind::CoverArt, cover_id)?;
            }
            cover
        };
        let cover_art_map = subsonic_cover_map(
            binding,
            cover_id.map(|cover| vec![(*id, cover)]).unwrap_or_default(),
        );
        result.push(Collection {
            id: *id,
            name,
            queries: Vec::new(),
            collection_type: request.collection_type,
            cover_art_map,
            readonly: true,
        });
    }
    Ok(Some(FetchCollectionByIdsResponse {
        call_id: None,
        collection_type: request.collection_type,
        result,
    }))
}

fn jukebox_disabled(request: &'static str) -> anyhow::Error {
    ExpectedRequestError::Unsupported {
        request,
        reason: UnsupportedReason::NotImplemented,
    }
    .into()
}

fn jukebox_error(error: SubsonicError, request: &'static str) -> anyhow::Error {
    match error {
        SubsonicError::Transport(_) => ExpectedRequestError::Unavailable {
            what: "Subsonic jukebox",
        }
        .into(),
        SubsonicError::HttpStatus(status) if status == reqwest::StatusCode::UNAUTHORIZED => {
            ExpectedRequestError::SubsonicNotAuthorized.into()
        }
        SubsonicError::HttpStatus(_) => ExpectedRequestError::Unavailable {
            what: "Subsonic jukebox",
        }
        .into(),
        SubsonicError::JukeboxUnsupported => jukebox_disabled(request),
        error => subsonic_request_error(error),
    }
}

pub(crate) fn jukebox_error_for_refresh(error: SubsonicError) -> anyhow::Error {
    jukebox_error(error, RefreshSessionRequest::wire_name())
}

async fn jukebox_status(binding: &SubsonicBinding) -> Result<subsonic::SubsonicJukeboxStatus> {
    binding
        .client
        .jukebox_status()
        .await
        .map_err(|error| jukebox_error(error, "Subsonic jukebox status"))
}

async fn jukebox_play_request(binding: &SubsonicBinding, _: &PlayRequest) -> Result<Option<()>> {
    binding
        .client
        .jukebox_start()
        .await
        .map_err(|error| jukebox_error(error, PlayRequest::wire_name()))?;
    Ok(Some(()))
}

async fn jukebox_pause_request(binding: &SubsonicBinding, _: &PauseRequest) -> Result<Option<()>> {
    binding
        .client
        .jukebox_stop()
        .await
        .map_err(|error| jukebox_error(error, PauseRequest::wire_name()))?;
    Ok(Some(()))
}

async fn jukebox_load_request(
    binding: &SubsonicBinding,
    request: &LoadRequest,
) -> Result<Option<()>> {
    let index = u32::try_from(request.index).map_err(|error| {
        ExpectedRequestError::invalid_input(format!("Invalid jukebox index: {error}"))
    })?;
    binding
        .client
        .jukebox_skip(index, 0)
        .await
        .map_err(|error| jukebox_error(error, LoadRequest::wire_name()))?;
    Ok(Some(()))
}

async fn jukebox_next_request(binding: &SubsonicBinding, _: &NextRequest) -> Result<Option<()>> {
    let queue = binding
        .client
        .jukebox_get()
        .await
        .map_err(|error| jukebox_error(error, NextRequest::wire_name()))?;
    let index = queue.status.current_index.unwrap_or(0).saturating_add(1);
    binding
        .client
        .jukebox_skip(index, 0)
        .await
        .map_err(|error| jukebox_error(error, NextRequest::wire_name()))?;
    Ok(Some(()))
}

async fn jukebox_previous_request(
    binding: &SubsonicBinding,
    _: &PreviousRequest,
) -> Result<Option<()>> {
    let queue = binding
        .client
        .jukebox_get()
        .await
        .map_err(|error| jukebox_error(error, PreviousRequest::wire_name()))?;
    let index = queue.status.current_index.unwrap_or(0).saturating_sub(1);
    binding
        .client
        .jukebox_skip(index, 0)
        .await
        .map_err(|error| jukebox_error(error, PreviousRequest::wire_name()))?;
    Ok(Some(()))
}

async fn jukebox_switch_request(
    binding: &SubsonicBinding,
    request: &SwitchRequest,
) -> Result<Option<()>> {
    binding
        .client
        .jukebox_skip(request.index, 0)
        .await
        .map_err(|error| jukebox_error(error, SwitchRequest::wire_name()))?;
    Ok(Some(()))
}

async fn jukebox_seek_request(
    binding: &SubsonicBinding,
    request: &SeekRequest,
) -> Result<Option<()>> {
    if !request.position_seconds.is_finite()
        || request.position_seconds < 0.0
        || request.position_seconds > f64::from(u32::MAX)
    {
        return Err(ExpectedRequestError::invalid_input("Invalid jukebox seek position").into());
    }
    let position = request.position_seconds as u32;
    let status = jukebox_status(binding).await?;
    binding
        .client
        .jukebox_skip(status.current_index.unwrap_or(0), position)
        .await
        .map_err(|error| jukebox_error(error, SeekRequest::wire_name()))?;
    Ok(Some(()))
}

async fn jukebox_volume_request(
    binding: &SubsonicBinding,
    request: &VolumeRequest,
) -> Result<Option<VolumeResponse>> {
    binding
        .client
        .jukebox_set_gain(request.volume)
        .await
        .map_err(|error| jukebox_error(error, VolumeRequest::wire_name()))?;
    Ok(Some(VolumeResponse {
        volume: request.volume,
        call_id: request.call_id,
    }))
}

async fn jukebox_playback_mode_request(
    binding: &SubsonicBinding,
    request: &SetPlaybackModeRequest,
) -> Result<Option<()>> {
    if request.mode != 3 {
        return Err(ExpectedRequestError::Unsupported {
            request: SetPlaybackModeRequest::wire_name(),
            reason: UnsupportedReason::NotImplemented,
        }
        .into());
    }
    binding
        .client
        .jukebox_shuffle()
        .await
        .map_err(|error| jukebox_error(error, SetPlaybackModeRequest::wire_name()))?;
    Ok(Some(()))
}

async fn jukebox_remove_request(
    binding: &SubsonicBinding,
    request: &RemoveRequest,
) -> Result<Option<()>> {
    binding
        .client
        .jukebox_remove(request.index)
        .await
        .map_err(|error| jukebox_error(error, RemoveRequest::wire_name()))?;
    Ok(Some(()))
}

fn subsonic_track_ids(
    binding: &SubsonicBinding,
    request: &LoadResolvedItemsRequest,
) -> Result<Vec<String>> {
    request
        .items
        .iter()
        .map(|item| {
            let PlayingItemRef::LibraryTrack { library_id, track } = item else {
                return Err(ExpectedRequestError::invalid_input(
                    "Subsonic jukebox accepts only Subsonic library items",
                )
                .into());
            };
            let library_id = Uuid::parse_str(library_id).map_err(|error| {
                ExpectedRequestError::invalid_input(format!("Invalid Subsonic library id: {error}"))
            })?;
            if library_id != binding.library_id {
                return Err(ExpectedRequestError::invalid_input(
                    "Subsonic jukebox item belongs to another library",
                )
                .into());
            }
            Ok(track.clone())
        })
        .collect()
}

async fn jukebox_load_resolved_items_request(
    binding: &SubsonicBinding,
    request: &LoadResolvedItemsRequest,
) -> Result<Option<LoadResolvedItemsResponse>> {
    let ids = subsonic_track_ids(binding, request)?;
    let references = ids.iter().map(String::as_str).collect::<Vec<_>>();
    let result = match request.mode {
        PlaylistOperateMode::Replace if references.is_empty() => {
            binding.client.jukebox_clear().await
        }
        PlaylistOperateMode::Replace => binding.client.jukebox_set(&references).await,
        PlaylistOperateMode::AppendToEnd | PlaylistOperateMode::PlayNext => {
            binding.client.jukebox_add(&references).await
        }
    };
    result.map_err(|error| jukebox_error(error, LoadResolvedItemsRequest::wire_name()))?;
    Ok(Some(LoadResolvedItemsResponse {
        call_id: request.call_id,
        result: Default::default(),
    }))
}

async fn jukebox_refresh_session_request(
    binding: &SubsonicBinding,
    _: &RefreshSessionRequest,
) -> Result<Option<()>> {
    binding.publish_jukebox_snapshot().await?;
    Ok(Some(()))
}

async fn jukebox_session_snapshot_request(
    binding: &SubsonicBinding,
    request: &SessionSnapshotRequest,
) -> Result<Option<SessionSnapshotResponse>> {
    let (jukebox, revision) = binding.jukebox_snapshot().await?;
    let items = jukebox
        .playlist
        .songs
        .into_iter()
        .map(|song| PlayingItemRef::LibraryTrack {
            library_id: binding.library_id.to_string(),
            track: song.id,
        })
        .collect();
    Ok(Some(SessionSnapshotResponse {
        items,
        index: jukebox.status.current_index,
        position_seconds: f64::from(jukebox.status.position_secs.unwrap_or_default()),
        playback_mode: 0,
        playing: jukebox.status.playing,
        revision,
        call_id: request.call_id,
    }))
}

async fn ensure_artists(binding: &SubsonicBinding) -> Result<()> {
    let loaded = binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?
        .artists_loaded;
    if loaded {
        return Ok(());
    }
    require_endpoint(binding, "getArtists", &[])?;
    let artists = binding
        .client
        .artists()
        .await
        .map_err(subsonic_request_error)?;
    let mut catalog = binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
    for artist in artists {
        let id = catalog.ids.intern(SubsonicIdKind::Artist, &artist.id)?;
        catalog.remember_artist(id, artist);
        catalog.listed_artists.insert(id);
    }
    catalog.artists_loaded = true;
    Ok(())
}

async fn ensure_albums(binding: &SubsonicBinding) -> Result<()> {
    let loaded = binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?
        .albums_loaded;
    if loaded {
        return Ok(());
    }
    require_endpoint(binding, "getAlbumList2", &["type", "size", "offset"])?;
    let mut offset = 0_u32;
    loop {
        let albums = binding
            .client
            .album_list(500, offset)
            .await
            .map_err(subsonic_request_error)?;
        let count = u32::try_from(albums.len()).unwrap_or(u32::MAX);
        let mut catalog = binding
            .catalog
            .lock()
            .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
        for album in albums {
            let id = catalog.ids.intern(SubsonicIdKind::Album, &album.id)?;
            catalog.remember_album(id, album);
            catalog.listed_albums.insert(id);
        }
        if count < 500 {
            catalog.albums_loaded = true;
            return Ok(());
        }
        drop(catalog);
        offset = offset.checked_add(count).ok_or_else(|| {
            ExpectedRequestError::invalid_input("Subsonic album list is too large")
        })?;
    }
}

fn collection_groups(
    binding: &SubsonicBinding,
    collection_type: CollectionType,
    group_titles: &[String],
) -> Result<Vec<CollectionGroup>> {
    let catalog = binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
    let entries: Vec<(i32, String, String, Option<String>)> = match collection_type {
        // Only what the library's own lists named; artists and albums met on searches and
        // detail pages are not listed.
        CollectionType::Artist => catalog
            .artists
            .iter()
            .filter(|(id, _)| catalog.listed_artists.contains(*id))
            .map(|(id, artist)| {
                (
                    *id,
                    artist.name.clone(),
                    generate_group_name(&artist.name),
                    artist.cover_art.clone(),
                )
            })
            .collect(),
        CollectionType::Album => catalog
            .albums
            .iter()
            .filter(|(id, _)| catalog.listed_albums.contains(*id))
            .map(|(id, album)| {
                (
                    *id,
                    album.name.clone(),
                    generate_group_name(&album.name),
                    album.cover_art.clone(),
                )
            })
            .collect(),
        _ => {
            return Err(ExpectedRequestError::Unsupported {
                request: FetchCollectionGroupsRequest::wire_name(),
                reason: UnsupportedReason::NotImplemented,
            }
            .into());
        }
    };
    Ok(group_titles
        .iter()
        .map(|title| {
            let mut collections: Vec<_> = entries
                .iter()
                .filter(|(_, _, group, _)| group == title)
                .map(|(id, name, _, _)| Collection {
                    id: *id,
                    name: name.clone(),
                    queries: Vec::new(),
                    collection_type,
                    cover_art_map: Default::default(),
                    readonly: true,
                })
                .collect();
            collections.sort_by(|left, right| left.name.cmp(&right.name));
            for collection in &mut collections {
                let cover = entries
                    .iter()
                    .find(|(id, _, _, _)| *id == collection.id)
                    .and_then(|(_, _, _, cover)| cover.clone());
                collection.cover_art_map = subsonic_cover_map(
                    binding,
                    cover
                        .map(|cover| vec![(collection.id, cover)])
                        .unwrap_or_default(),
                );
            }
            CollectionGroup {
                group_title: title.clone(),
                collections,
            }
        })
        .collect())
}

pub(crate) async fn fetch_collection_group_summary_request(
    binding: &SubsonicBinding,
    request: &FetchCollectionGroupSummaryRequest,
) -> Result<Option<CollectionGroupSummaryResponse>> {
    match request.collection_type {
        CollectionType::Artist => ensure_artists(binding).await?,
        CollectionType::Album => ensure_albums(binding).await?,
        // The summary is where the Mixes page starts, so recommendations are read afresh.
        CollectionType::Mix if binding.probe.supports_rune_recommendations() => {
            load_recommendations(binding).await?;
            return mix_group_summary(binding).map(Some);
        }
        _ => {
            return Err(ExpectedRequestError::Unsupported {
                request: FetchCollectionGroupSummaryRequest::wire_name(),
                reason: UnsupportedReason::NotImplemented,
            }
            .into());
        }
    }
    let catalog = binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
    let names = match request.collection_type {
        CollectionType::Artist => catalog
            .artists
            .iter()
            .filter(|(id, _)| catalog.listed_artists.contains(*id))
            .map(|(_, item)| item.name.as_str())
            .collect(),
        CollectionType::Album => catalog
            .albums
            .iter()
            .filter(|(id, _)| catalog.listed_albums.contains(*id))
            .map(|(_, item)| item.name.as_str())
            .collect(),
        _ => Vec::new(),
    };
    let mut groups = BTreeMap::<String, i32>::new();
    for name in names {
        *groups.entry(generate_group_name(name)).or_default() += 1;
    }
    Ok(Some(CollectionGroupSummaryResponse {
        call_id: None,
        collection_type: request.collection_type,
        groups: groups
            .into_iter()
            .map(|(group_title, count)| CollectionGroupSummary { group_title, count })
            .collect(),
    }))
}

pub(crate) async fn fetch_collection_groups_request(
    binding: &SubsonicBinding,
    request: &FetchCollectionGroupsRequest,
) -> Result<Option<FetchCollectionGroupsResponse>> {
    match request.collection_type {
        CollectionType::Artist => ensure_artists(binding).await?,
        CollectionType::Album => ensure_albums(binding).await?,
        CollectionType::Mix if binding.probe.supports_rune_recommendations() => {
            ensure_recommendations(binding).await?;
            return Ok(Some(FetchCollectionGroupsResponse {
                call_id: None,
                groups: mix_collection_groups(binding, &request.group_titles)?,
            }));
        }
        _ => {
            return Err(ExpectedRequestError::Unsupported {
                request: FetchCollectionGroupsRequest::wire_name(),
                reason: UnsupportedReason::NotImplemented,
            }
            .into());
        }
    }
    Ok(Some(FetchCollectionGroupsResponse {
        call_id: None,
        groups: collection_groups(binding, request.collection_type, &request.group_titles)?,
    }))
}

fn parse_detail_query(request: &MixQueryRequest) -> Result<DetailQuery> {
    let [query] = request.queries.as_slice() else {
        return Err(unsupported_detail_query().into());
    };
    let operator = parse_query(&(query.operator.clone(), query.parameter.clone()));
    match operator {
        QueryOperator::LibArtist(id) => Ok(DetailQuery::Artist(id)),
        QueryOperator::LibAlbum(id) => Ok(DetailQuery::Album(id)),
        QueryOperator::LibPlaylist(id) => Ok(DetailQuery::Playlist(id)),
        _ => Err(unsupported_detail_query().into()),
    }
}

fn unsupported_detail_query() -> ExpectedRequestError {
    ExpectedRequestError::Unsupported {
        request: MixQueryRequest::wire_name(),
        reason: UnsupportedReason::NotImplemented,
    }
}

/// How long a read serves the following pages of the same listing.
const RECENT_READ: Duration = Duration::from_secs(30);
/// Albums of an artist read at once.
const PARALLEL_ALBUM_READS: usize = 4;

/// The songs of a playlist, in its order.
async fn playlist_songs(binding: &SubsonicBinding, id: i32) -> Result<Vec<SubsonicSong>> {
    let remote_id = resolve_playlist_id(binding, id)?;
    require_endpoint(binding, "getPlaylist", &["id"])?;
    let playlist = binding
        .client
        .playlist(&remote_id)
        .await
        .map_err(subsonic_request_error)?;
    remember_playlist_edit(binding, id, playlist.rune_edit)?;
    let mut catalog = binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
    for song in &playlist.songs {
        let id = catalog.ids.intern(SubsonicIdKind::Track, &song.id)?;
        catalog.remember_song(id, song.clone());
    }
    Ok(playlist.songs)
}

/// The songs of a detail query. The last query's songs serve its following pages for
/// [`RECENT_READ`]; playlist edits and library additions forget them.
async fn detail_songs(
    binding: &SubsonicBinding,
    query: DetailQuery,
) -> Result<Arc<Vec<SubsonicSong>>> {
    let recent = binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?
        .recent_detail
        .as_ref()
        .and_then(|recent| recent.fresh(&query, Instant::now(), RECENT_READ));
    if let Some(songs) = recent {
        return Ok(songs);
    }
    let read = Instant::now();
    let songs = Arc::new(read_detail_songs(binding, query).await?);
    binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?
        .recent_detail = Some(RecentRead {
        key: query,
        read,
        value: Arc::clone(&songs),
    });
    Ok(songs)
}

async fn read_detail_songs(
    binding: &SubsonicBinding,
    query: DetailQuery,
) -> Result<Vec<SubsonicSong>> {
    let albums = match query {
        DetailQuery::Playlist(id) => return playlist_songs(binding, id).await,
        DetailQuery::Album(id) => vec![id],
        DetailQuery::Artist(id) => {
            let remote_id = {
                let catalog = binding
                    .catalog
                    .lock()
                    .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
                catalog.ids.resolve(SubsonicIdKind::Artist, id)?.to_owned()
            };
            require_endpoint(binding, "getArtist", &["id"])?;
            let artist = binding
                .client
                .artist(&remote_id)
                .await
                .map_err(subsonic_request_error)?;
            let mut catalog = binding
                .catalog
                .lock()
                .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
            let mut albums = Vec::with_capacity(artist.albums.len());
            for album in artist.albums {
                let id = catalog.ids.intern(SubsonicIdKind::Album, &album.id)?;
                catalog.remember_album(id, album);
                albums.push(id);
            }
            albums
        }
    };
    let remote_ids = {
        let catalog = binding
            .catalog
            .lock()
            .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
        albums
            .iter()
            .map(|id| Ok(catalog.ids.resolve(SubsonicIdKind::Album, *id)?.to_owned()))
            .collect::<Result<Vec<_>>>()?
    };
    // A few at a time, in the artist's album order.
    let albums: Vec<_> = stream::iter(remote_ids)
        .map(|remote_id| async move {
            binding
                .client
                .album(&remote_id)
                .await
                .map_err(subsonic_request_error)
        })
        .buffered(PARALLEL_ALBUM_READS)
        .try_collect()
        .await?;
    let mut songs = Vec::new();
    let mut catalog = binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
    for mut album in albums {
        album.songs.sort_by_key(|song| {
            (
                song.disc.unwrap_or_default(),
                song.track.unwrap_or_default(),
            )
        });
        for song in album.songs {
            let id = catalog.ids.intern(SubsonicIdKind::Track, &song.id)?;
            catalog.remember_song(id, song.clone());
            songs.push(song);
        }
    }
    Ok(songs)
}

pub(crate) async fn mix_query_request(
    binding: &SubsonicBinding,
    request: &MixQueryRequest,
) -> Result<Option<MixQueryResponse>> {
    let query = parse_detail_query(request)?;
    let songs = detail_songs(binding, query).await?;
    let mut catalog = binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
    let translated = songs
        .iter()
        .skip(usize::try_from(request.cursor.max(0)).unwrap_or(usize::MAX))
        .take(usize::try_from(request.page_size.max(0)).unwrap_or(usize::MAX))
        .map(|song| {
            let id = catalog.ids.intern(SubsonicIdKind::Track, &song.id)?;
            catalog.remember_song(id, song.clone());
            let cover_art_id = song
                .cover_art
                .as_deref()
                .map(|cover| catalog.ids.intern(SubsonicIdKind::CoverArt, cover))
                .transpose()?
                .unwrap_or_default();
            let media_file = subsonic_media_file(binding, id, cover_art_id, song);
            Ok((media_file, song.cover_art.clone()))
        })
        .collect::<Result<Vec<_>>>()?;
    drop(catalog);
    let covers = translated
        .iter()
        .filter_map(|(file, cover)| cover.clone().map(|cover| (file.id, cover)))
        .collect();
    let files = translated.into_iter().map(|(file, _)| file).collect();
    Ok(Some(MixQueryResponse {
        call_id: None,
        files,
        cover_art_map: subsonic_cover_map(binding, covers),
    }))
}

// Catalogue -----------------------------------------------------------------

/// Which of the given items a `runeCatalog` server said are outside the user's library. Items
/// it never said that about are in it.
pub(crate) async fn fetch_library_membership_request(
    binding: &SubsonicBinding,
    request: &FetchLibraryMembershipRequest,
) -> Result<Option<FetchLibraryMembershipResponse>> {
    let catalog = binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
    let outside = |kind: SubsonicIdKind, ids: &[i32]| -> Result<Vec<i32>> {
        let mut found = Vec::new();
        for &id in ids {
            catalog.ids.resolve(kind, id)?;
            if catalog.is_outside_library(kind, id) {
                found.push(id);
            }
        }
        Ok(found)
    };
    Ok(Some(FetchLibraryMembershipResponse {
        outside_track_ids: outside(SubsonicIdKind::Track, &request.track_ids)?,
        outside_album_ids: outside(SubsonicIdKind::Album, &request.album_ids)?,
        outside_artist_ids: outside(SubsonicIdKind::Artist, &request.artist_ids)?,
        call_id: None,
    }))
}

/// Adds catalogue tracks and albums to the user's library in one call, then counts them, and
/// the known tracks of the added albums, as in it.
pub(crate) async fn add_to_library_request(
    binding: &SubsonicBinding,
    request: &AddToLibraryRequest,
) -> Result<Option<AddToLibraryResponse>> {
    let (tracks, albums) = {
        let catalog = binding
            .catalog
            .lock()
            .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
        let resolve = |kind: SubsonicIdKind, ids: &[i32]| -> Result<Vec<(i32, String)>> {
            ids.iter()
                .map(|&id| Ok((id, catalog.ids.resolve(kind, id)?.to_owned())))
                .collect()
        };
        (
            resolve(SubsonicIdKind::Track, &request.track_ids)?,
            resolve(SubsonicIdKind::Album, &request.album_ids)?,
        )
    };
    let remote: Vec<&str> = tracks
        .iter()
        .chain(&albums)
        .map(|(_, remote)| remote.as_str())
        .collect();
    if !remote.is_empty() {
        binding
            .client
            .add_to_library(&remote)
            .await
            .map_err(subsonic_request_error)?;
    }
    let mut catalog = binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
    // Kept pages carry what the server said before the addition.
    catalog.forget_recent_reads();
    let mut added: Vec<(SubsonicIdKind, i32)> = tracks
        .iter()
        .map(|(id, _)| (SubsonicIdKind::Track, *id))
        .collect();
    for (id, remote) in &albums {
        catalog.listed_albums.insert(*id);
        added.push((SubsonicIdKind::Album, *id));
        added.extend(
            catalog
                .songs
                .iter()
                .filter(|(_, song)| song.album_id.as_deref() == Some(remote.as_str()))
                .map(|(id, _)| (SubsonicIdKind::Track, *id)),
        );
    }
    for (kind, id) in added {
        catalog.note_membership(kind, id, Some(true));
        match kind {
            SubsonicIdKind::Track => {
                if let Some(song) = catalog.songs.get_mut(&id) {
                    song.rune_in_library = Some(true);
                }
            }
            SubsonicIdKind::Album => {
                if let Some(album) = catalog.albums.get_mut(&id) {
                    album.rune_in_library = Some(true);
                }
            }
            _ => {}
        }
    }
    Ok(Some(AddToLibraryResponse {
        success: true,
        call_id: None,
    }))
}

// Recommendations -----------------------------------------------------------

/// Groups asked of `getRuneRecommendations`; the server decides how many it has.
const RECOMMENDATION_GROUPS: u32 = 50;

/// Reads the recommendations and makes a mix of every recommended album and playlist. Groups
/// of the same title are merged, and a group without a title is named by its id.
async fn load_recommendations(binding: &SubsonicBinding) -> Result<()> {
    require_endpoint(binding, "getRuneRecommendations", &["count"])?;
    let groups = binding
        .client
        .recommendations(RECOMMENDATION_GROUPS)
        .await
        .map_err(subsonic_request_error)?;
    let mut catalog = binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
    let mut mix_groups: Vec<(String, Vec<i32>)> = Vec::new();
    for group in groups {
        let title = if group.title.trim().is_empty() {
            group.id.clone()
        } else {
            group.title.clone()
        };
        let mut mixes = Vec::new();
        for album in group.albums {
            let id = catalog.ids.intern(SubsonicIdKind::Album, &album.id)?;
            catalog.note_membership(SubsonicIdKind::Album, id, album.rune_in_library);
            let mix_id = catalog
                .ids
                .intern(SubsonicIdKind::Mix, &format!("album:{}", album.id))?;
            catalog.mixes.insert(
                mix_id,
                RecommendedMix {
                    name: album.name,
                    group: title.clone(),
                    target: MixTarget::Album(id),
                    cover_art: album.cover_art,
                },
            );
            mixes.push(mix_id);
        }
        for playlist in group.playlists {
            let id = catalog.ids.intern(SubsonicIdKind::Playlist, &playlist.id)?;
            if let Some(edit) = playlist.rune_edit {
                catalog.playlist_edits.insert(id, edit);
            }
            let mix_id = catalog
                .ids
                .intern(SubsonicIdKind::Mix, &format!("playlist:{}", playlist.id))?;
            catalog.mixes.insert(
                mix_id,
                RecommendedMix {
                    name: playlist.name,
                    group: title.clone(),
                    target: MixTarget::Playlist(id),
                    cover_art: playlist.cover_art,
                },
            );
            mixes.push(mix_id);
        }
        let index = match mix_groups.iter().position(|(known, _)| *known == title) {
            Some(index) => index,
            None => {
                mix_groups.push((title, Vec::new()));
                mix_groups.len() - 1
            }
        };
        let ids = &mut mix_groups[index].1;
        for mix_id in mixes {
            if !ids.contains(&mix_id) {
                ids.push(mix_id);
            }
        }
    }
    mix_groups.retain(|(_, ids)| !ids.is_empty());
    catalog.mix_groups = mix_groups;
    catalog.mixes_loaded = true;
    Ok(())
}

async fn ensure_recommendations(binding: &SubsonicBinding) -> Result<()> {
    let loaded = binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?
        .mixes_loaded;
    if loaded {
        return Ok(());
    }
    load_recommendations(binding).await
}

fn mix_group_summary(binding: &SubsonicBinding) -> Result<CollectionGroupSummaryResponse> {
    let catalog = binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
    Ok(CollectionGroupSummaryResponse {
        call_id: None,
        collection_type: CollectionType::Mix,
        groups: catalog
            .mix_groups
            .iter()
            .map(|(title, ids)| CollectionGroupSummary {
                group_title: title.clone(),
                count: i32::try_from(ids.len()).unwrap_or(i32::MAX),
            })
            .collect(),
    })
}

/// A recommended mix as Rune's local mixes are listed: one query naming what it plays,
/// read only.
fn mix_collection(binding: &SubsonicBinding, id: i32, mix: &RecommendedMix) -> Collection {
    let (operator, parameter) = match mix.target {
        MixTarget::Album(album) => ("lib::album", album),
        MixTarget::Playlist(playlist) => ("lib::playlist", playlist),
    };
    Collection {
        id,
        name: mix.name.clone(),
        queries: vec![MixQuery {
            operator: operator.to_owned(),
            parameter: parameter.to_string(),
        }],
        collection_type: CollectionType::Mix,
        cover_art_map: subsonic_cover_map(
            binding,
            mix.cover_art
                .clone()
                .map(|cover| vec![(id, cover)])
                .unwrap_or_default(),
        ),
        readonly: true,
    }
}

fn mix_collection_groups(
    binding: &SubsonicBinding,
    group_titles: &[String],
) -> Result<Vec<CollectionGroup>> {
    let groups: Vec<(String, Vec<(i32, RecommendedMix)>)> = {
        let catalog = binding
            .catalog
            .lock()
            .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?;
        group_titles
            .iter()
            .map(|title| {
                let mixes = catalog
                    .mix_groups
                    .iter()
                    .find(|(known, _)| known == title)
                    .map(|(_, ids)| {
                        ids.iter()
                            .filter_map(|id| Some((*id, catalog.mixes.get(id)?.clone())))
                            .collect()
                    })
                    .unwrap_or_default();
                (title.clone(), mixes)
            })
            .collect()
    };
    Ok(groups
        .into_iter()
        .map(|(group_title, mixes)| CollectionGroup {
            group_title,
            collections: mixes
                .iter()
                .map(|(id, mix)| mix_collection(binding, *id, mix))
                .collect(),
        })
        .collect())
}

/// A recommended mix by id, reading the recommendations once when none were read yet.
async fn recommended_mix(binding: &SubsonicBinding, id: i32) -> Result<Option<RecommendedMix>> {
    ensure_recommendations(binding).await?;
    Ok(binding
        .catalog
        .lock()
        .map_err(|_| anyhow!("Subsonic catalog lock poisoned"))?
        .mixes
        .get(&id)
        .cloned())
}

/// The known mixes among `ids`; others are left out, as Rune's own mixes are.
async fn mixes_by_ids(
    binding: &SubsonicBinding,
    request: &FetchCollectionByIdsRequest,
) -> Result<Option<FetchCollectionByIdsResponse>> {
    let mut result = Vec::with_capacity(request.ids.len());
    for &id in &request.ids {
        if let Some(mix) = recommended_mix(binding, id).await? {
            result.push(mix_collection(binding, id, &mix));
        }
    }
    Ok(Some(FetchCollectionByIdsResponse {
        call_id: None,
        collection_type: CollectionType::Mix,
        result,
    }))
}

fn mix_not_found() -> anyhow::Error {
    ExpectedError::NotFound { what: "mix" }.into()
}

pub(crate) async fn get_mix_by_id_request(
    binding: &SubsonicBinding,
    request: &GetMixByIdRequest,
) -> Result<Option<GetMixByIdResponse>> {
    let mix = recommended_mix(binding, request.mix_id)
        .await?
        .ok_or_else(mix_not_found)?;
    Ok(Some(GetMixByIdResponse {
        mix: Mix {
            id: request.mix_id,
            name: mix.name,
            group: mix.group,
            locked: true,
            // As Rune's album and playlist pages play: in the player's current mode.
            mode: KEEP_PLAYBACK_MODE as i32,
        },
        call_id: None,
    }))
}

pub(crate) async fn fetch_mix_queries_request(
    binding: &SubsonicBinding,
    request: &FetchMixQueriesRequest,
) -> Result<Option<FetchMixQueriesResponse>> {
    let mix = recommended_mix(binding, request.mix_id)
        .await?
        .ok_or_else(mix_not_found)?;
    let query = mix_collection(binding, request.mix_id, &mix)
        .queries
        .remove(0);
    Ok(Some(FetchMixQueriesResponse {
        result: vec![StoredMixQuery {
            clause_id: request.mix_id.to_string(),
            operator: query.operator,
            parameter: query.parameter,
            target_uuid: None,
            available: true,
        }],
        call_id: None,
    }))
}

fn subsonic_request_error(error: SubsonicError) -> anyhow::Error {
    match error {
        SubsonicError::Transport(_) | SubsonicError::HttpStatus(_) => {
            ExpectedRequestError::Unavailable {
                what: "Subsonic server",
            }
            .into()
        }
        SubsonicError::Malformed(_) => ExpectedRequestError::UnexpectedResponse {
            expected: "valid Subsonic response".into(),
            actual: "malformed response".into(),
        }
        .into(),
        error if error.is_rune_credentials_expired() => {
            ExpectedRequestError::SubsonicNotAuthorized.into()
        }
        SubsonicError::Server {
            rune_reason: Some(subsonic::RuneReason::Removed),
            ..
        } => ExpectedError::NotFound {
            what: "Subsonic item",
        }
        .into(),
        SubsonicError::Server {
            kind:
                SubsonicServerError::WrongCredentials
                | SubsonicServerError::NotAuthorized
                | SubsonicServerError::InvalidApiKey,
            ..
        } => ExpectedRequestError::SubsonicNotAuthorized.into(),
        SubsonicError::Server {
            kind: SubsonicServerError::NotFound,
            ..
        } => ExpectedError::NotFound {
            what: "Subsonic item",
        }
        .into(),
        SubsonicError::Server {
            kind:
                SubsonicServerError::Generic
                | SubsonicServerError::MissingParameter
                | SubsonicServerError::ClientTooOld
                | SubsonicServerError::ServerTooOld
                | SubsonicServerError::TokenAuthenticationNotSupported
                | SubsonicServerError::AuthenticationMechanismNotSupported
                | SubsonicServerError::ConflictingAuthentication
                | SubsonicServerError::Unknown(_),
            ..
        } => ExpectedRequestError::UnexpectedResponse {
            expected: "successful Subsonic request".into(),
            actual: "server rejected the request".into(),
        }
        .into(),
        SubsonicError::JukeboxUnsupported => ExpectedRequestError::Unsupported {
            request: "Subsonic jukebox request",
            reason: UnsupportedReason::NotImplemented,
        }
        .into(),
        SubsonicError::RuneVisualizerFeedUnsupported => ExpectedRequestError::Unsupported {
            request: "Subsonic visualizer feed",
            reason: UnsupportedReason::NotImplemented,
        }
        .into(),
        SubsonicError::Unsupported { endpoint } => ExpectedRequestError::Unsupported {
            request: endpoint,
            reason: UnsupportedReason::NotImplemented,
        }
        .into(),
        SubsonicError::InvalidJukeboxGain => {
            ExpectedRequestError::invalid_input("invalid Subsonic jukebox gain").into()
        }
    }
}

macro_rules! subsonic_requests {
    (
        supported { $($request:ty => $handler:ident as $method:ident),* $(,)? }
        routed_native { $($native_request:ty),* $(,)? }
        jukebox_supported { $($jukebox_request:ty => $jukebox_handler:ident as $jukebox_method:ident),* $(,)? }
        catalog_supported { $($catalog_request:ty => $catalog_handler:ident as $catalog_method:ident),* $(,)? }
        recommendations_supported { $($recommendations_request:ty => $recommendations_handler:ident as $recommendations_method:ident),* $(,)? }
        player { $($player_request:ty),* $(,)? }
    ) => {
        impl RequestTranslator for SubsonicBinding {
            $(
                fn $method<'a>(
                    &'a self,
                    request: &'a $request,
                ) -> RequestTranslationFuture<'a, <$request as crate::TypedRequest>::Response> {
                    Box::pin($handler(self, request))
                }
            )*
            $(
                fn $jukebox_method<'a>(
                    &'a self,
                    request: &'a $jukebox_request,
                ) -> RequestTranslationFuture<'a, <$jukebox_request as crate::TypedRequest>::Response> {
                    Box::pin($jukebox_handler(self, request))
                }
            )*
            $(
                fn $catalog_method<'a>(
                    &'a self,
                    request: &'a $catalog_request,
                ) -> RequestTranslationFuture<'a, <$catalog_request as crate::TypedRequest>::Response> {
                    Box::pin($catalog_handler(self, request))
                }
            )*
            $(
                fn $recommendations_method<'a>(
                    &'a self,
                    request: &'a $recommendations_request,
                ) -> RequestTranslationFuture<'a, <$recommendations_request as crate::TypedRequest>::Response> {
                    Box::pin($recommendations_handler(self, request))
                }
            )*
        }

        impl RequestSlots {
            pub(crate) fn subsonic(probe: &subsonic::SubsonicProbe) -> Self {
                let mut slots = Self::unimplemented();
                let player_reason = if probe.roles_known && probe.roles.jukebox_role {
                    UnsupportedReason::NotImplemented
                } else {
                    UnsupportedReason::AccountRoleMissing
                };
                $(slots.set::<$player_request>(Slot::Unsupported { reason: player_reason });)*
                if probe.roles_known && probe.roles.jukebox_role {
                    $(slots.set::<$jukebox_request>(Slot::Translate);)*
                }
                $(slots.set::<$request>(Slot::Translate);)*
                $(slots.set::<$native_request>(Slot::Native);)*
                if probe.supports_rune_catalog() {
                    $(slots.set::<$catalog_request>(Slot::Translate);)*
                }
                if probe.supports_rune_recommendations() {
                    $(slots.set::<$recommendations_request>(Slot::Translate);)*
                }
                slots
            }

            fn hide_player(&mut self) {
                $(self.set::<$player_request>(Slot::Unsupported {
                    reason: UnsupportedReason::NotImplemented,
                });)*
            }

            /// Hide the requests about a service's catalogue beyond the library, which only
            /// `runeCatalog` servers answer.
            pub(crate) fn hide_service_catalogue(&mut self) {
                $(self.set::<$catalog_request>(Slot::Unsupported {
                    reason: UnsupportedReason::NotImplemented,
                });)*
            }
        }
    };
}

subsonic_requests! {
    supported {
        SetLikedRequest => set_liked_request as translate_set_liked_request,
        GetLikedRequest => get_liked_request as translate_get_liked_request,
        GetLyricByTrackIdRequest => get_lyric_request as translate_get_lyric_by_track_id_request,
        FetchAllPlaylistsRequest => fetch_all_playlists_request as translate_fetch_all_playlists_request,
        GetPlaylistByIdRequest => get_playlist_by_id_request as translate_get_playlist_by_id_request,
        CreatePlaylistRequest => create_playlist_request as translate_create_playlist_request,
        UpdatePlaylistRequest => update_playlist_request as translate_update_playlist_request,
        RemovePlaylistRequest => remove_playlist_request as translate_remove_playlist_request,
        AddItemToPlaylistRequest => add_item_to_playlist_request as translate_add_item_to_playlist_request,
        RemoveItemFromPlaylistRequest => remove_item_from_playlist_request as translate_remove_item_from_playlist_request,
        ReorderPlaylistItemPositionRequest => reorder_playlist_item_position_request as translate_reorder_playlist_item_position_request,
        FetchPlaylistOccurrencesRequest => fetch_playlist_occurrences_request as translate_fetch_playlist_occurrences_request,
        FetchMediaFileByIdsRequest => fetch_media_file_by_ids_request as translate_fetch_media_file_by_ids_request,
        FetchMediaFilesRequest => fetch_media_files_request as translate_fetch_media_files_request,
        GetMediaFilesCountRequest => get_media_files_count_request as translate_get_media_files_count_request,
        FetchParsedMediaFileRequest => fetch_parsed_media_file_request as translate_fetch_parsed_media_file_request,
        SearchMediaFileSummaryRequest => search_media_file_summary_request as translate_search_media_file_summary_request,
        SearchCollectionSummaryRequest => search_collection_summary_request as translate_search_collection_summary_request,
        FetchCollectionGroupSummaryRequest => fetch_collection_group_summary_request as translate_fetch_collection_group_summary_request,
        FetchCollectionGroupsRequest => fetch_collection_groups_request as translate_fetch_collection_groups_request,
        FetchCollectionByIdsRequest => fetch_collection_by_ids_request as translate_fetch_collection_by_ids_request,
        MixQueryRequest => mix_query_request as translate_mix_query_request,
        SearchForRequest => search_for_request as translate_search_for_request,
    }
    routed_native {
        SetLikedRequest,
        GetLikedRequest,
    }
    jukebox_supported {
        VolumeRequest => jukebox_volume_request as translate_volume_request,
        SetPlaybackModeRequest => jukebox_playback_mode_request as translate_set_playback_mode_request,
        LoadRequest => jukebox_load_request as translate_load_request,
        PlayRequest => jukebox_play_request as translate_play_request,
        PauseRequest => jukebox_pause_request as translate_pause_request,
        NextRequest => jukebox_next_request as translate_next_request,
        PreviousRequest => jukebox_previous_request as translate_previous_request,
        SwitchRequest => jukebox_switch_request as translate_switch_request,
        SeekRequest => jukebox_seek_request as translate_seek_request,
        RemoveRequest => jukebox_remove_request as translate_remove_request,
        LoadResolvedItemsRequest => jukebox_load_resolved_items_request as translate_load_resolved_items_request,
        RefreshSessionRequest => jukebox_refresh_session_request as translate_refresh_session_request,
        SessionSnapshotRequest => jukebox_session_snapshot_request as translate_session_snapshot_request,
    }
    catalog_supported {
        FetchLibraryMembershipRequest => fetch_library_membership_request as translate_fetch_library_membership_request,
        AddToLibraryRequest => add_to_library_request as translate_add_to_library_request,
    }
    recommendations_supported {
        GetMixByIdRequest => get_mix_by_id_request as translate_get_mix_by_id_request,
        FetchMixQueriesRequest => fetch_mix_queries_request as translate_fetch_mix_queries_request,
    }
    player {
        VolumeRequest,
        LoadRequest,
        PlayRequest,
        PauseRequest,
        NextRequest,
        PreviousRequest,
        SwitchRequest,
        SeekRequest,
        RemoveRequest,
        SetPlaybackModeRequest,
        MovePlaylistItemRequest,
        SetRealtimeFFTEnabledRequest,
        SetAdaptiveSwitchingEnabledRequest,
        LoadResolvedItemsRequest,
        HandoffLoadRequest,
        TakeBackHandoffBlockRequest,
        CastPreflightRequest,
        RefreshSessionRequest,
        SessionSnapshotRequest,
    }
}

/// The Subsonic endpoints, with the parameter that matters, each Rune request calls through the
/// translator above. A `runeService` descriptor that lists any of them as unsupported hides the
/// request. A whole-endpoint entry covers every parameter. Lyrics and the jukebox are handled
/// apart in `hide_rune_unsupported`.
/// A Subsonic endpoint and the parameters Rune sends to it. Entries of a descriptor narrowed to
/// one of those parameters count; entries narrowed to others do not.
type EndpointUse = (&'static str, &'static [&'static str]);

/// Whether a request is hidden when any of its endpoints is unsupported, or only when all are.
/// "All" is for requests that serve several kinds of item through different endpoints: they stay
/// available and refuse at run time (`require_endpoint`) for the kind that is missing.
#[derive(Clone, Copy)]
enum Hides {
    AnyEndpoint,
    AllEndpoints,
}

type RequestEndpoints = (fn(&mut RequestSlots), Hides, &'static [EndpointUse]);

/// The Subsonic endpoints each Rune request calls through the translator above. A `runeService`
/// descriptor that lists them as unsupported hides the request. Lyrics and the jukebox are
/// handled apart in `hide_rune_unsupported`.
const REQUEST_ENDPOINTS: &[RequestEndpoints] = &[
    (
        hide::<SetLikedRequest>,
        Hides::AnyEndpoint,
        &[("star", &["id"]), ("unstar", &["id"])],
    ),
    (
        hide::<GetLikedRequest>,
        Hides::AnyEndpoint,
        &[("getSong", &["id"])],
    ),
    (
        hide::<FetchMediaFileByIdsRequest>,
        Hides::AnyEndpoint,
        &[("getSong", &["id"])],
    ),
    (
        hide::<FetchParsedMediaFileRequest>,
        Hides::AnyEndpoint,
        &[("getSong", &["id"])],
    ),
    (
        hide::<FetchAllPlaylistsRequest>,
        Hides::AnyEndpoint,
        &[("getPlaylists", &[])],
    ),
    (
        hide::<GetPlaylistByIdRequest>,
        Hides::AnyEndpoint,
        &[("getPlaylist", &["id"])],
    ),
    (
        hide::<FetchPlaylistOccurrencesRequest>,
        Hides::AnyEndpoint,
        &[("getPlaylist", &["id"])],
    ),
    (
        hide::<CreatePlaylistRequest>,
        Hides::AnyEndpoint,
        &[("createPlaylist", &["name"]), ("getPlaylists", &[])],
    ),
    (
        hide::<UpdatePlaylistRequest>,
        Hides::AnyEndpoint,
        &[("updatePlaylist", &["playlistId", "name"])],
    ),
    (
        hide::<RemovePlaylistRequest>,
        Hides::AnyEndpoint,
        &[("deletePlaylist", &["id"])],
    ),
    (
        hide::<AddItemToPlaylistRequest>,
        Hides::AnyEndpoint,
        &[
            ("getPlaylist", &["id"]),
            ("updatePlaylist", &["playlistId", "songIdToAdd"]),
        ],
    ),
    (
        hide::<RemoveItemFromPlaylistRequest>,
        Hides::AnyEndpoint,
        &[
            ("getPlaylist", &["id"]),
            ("updatePlaylist", &["playlistId", "songIndexToRemove"]),
        ],
    ),
    // Reordering rewrites the playlist through createPlaylist with a playlistId.
    (
        hide::<ReorderPlaylistItemPositionRequest>,
        Hides::AnyEndpoint,
        &[
            ("getPlaylist", &["id"]),
            ("createPlaylist", &["playlistId", "songId"]),
        ],
    ),
    (
        hide::<GetMediaFilesCountRequest>,
        Hides::AnyEndpoint,
        &[("getScanStatus", &[])],
    ),
    (
        hide::<SearchForRequest>,
        Hides::AnyEndpoint,
        &[("search3", &["query"])],
    ),
    (
        hide::<SearchMediaFileSummaryRequest>,
        Hides::AnyEndpoint,
        &[("search3", &["query"])],
    ),
    (
        hide::<SearchCollectionSummaryRequest>,
        Hides::AnyEndpoint,
        &[("search3", &["query"])],
    ),
    (
        hide::<FetchMediaFilesRequest>,
        Hides::AnyEndpoint,
        &[("search3", &["query", "songOffset"])],
    ),
    // Artist groups come from getArtists, album groups from getAlbumList2 and mix groups from
    // getRuneRecommendations.
    (
        hide::<FetchCollectionGroupSummaryRequest>,
        Hides::AllEndpoints,
        &[
            ("getAlbumList2", &["type", "size", "offset"]),
            ("getArtists", &[]),
            ("getRuneRecommendations", &["count"]),
        ],
    ),
    (
        hide::<FetchCollectionGroupsRequest>,
        Hides::AllEndpoints,
        &[
            ("getAlbumList2", &["type", "size", "offset"]),
            ("getArtists", &[]),
            ("getRuneRecommendations", &["count"]),
        ],
    ),
    (
        hide::<FetchCollectionByIdsRequest>,
        Hides::AllEndpoints,
        &[
            ("getArtist", &["id"]),
            ("getAlbum", &["id"]),
            ("getRuneRecommendations", &["count"]),
        ],
    ),
    // Both kinds of query read albums; artists need getArtist on top, checked at run time.
    (
        hide::<MixQueryRequest>,
        Hides::AnyEndpoint,
        &[("getAlbum", &["id"])],
    ),
    // Membership comes from searches with a scope.
    (
        hide::<FetchLibraryMembershipRequest>,
        Hides::AnyEndpoint,
        &[("search3", &["query", "runeScope"])],
    ),
    (
        hide::<AddToLibraryRequest>,
        Hides::AnyEndpoint,
        &[("addToRuneLibrary", &["id"])],
    ),
    (
        hide::<GetMixByIdRequest>,
        Hides::AnyEndpoint,
        &[("getRuneRecommendations", &["count"])],
    ),
    (
        hide::<FetchMixQueriesRequest>,
        Hides::AnyEndpoint,
        &[("getRuneRecommendations", &["count"])],
    ),
];

/// Whether a call to `endpoint` sending `params` cannot be made: the descriptor lists it as
/// unsupported, or it belongs to a Rune extension the server does not declare.
fn endpoint_missing(
    descriptor: &RuneServiceDescriptor,
    probe: &SubsonicProbe,
    (endpoint, params): &EndpointUse,
) -> bool {
    let declared = match *endpoint {
        "addToRuneLibrary" => probe.supports_rune_catalog(),
        "getRuneRecommendations" => probe.supports_rune_recommendations(),
        _ => true,
    };
    !declared || descriptor.is_unsupported(endpoint, params)
}

fn hide<R: crate::rpc::TargetRequest>(slots: &mut RequestSlots) {
    slots.set::<R>(Slot::Unsupported {
        reason: UnsupportedReason::NotImplemented,
    });
}

impl RequestSlots {
    /// Hide the requests the service behind a `runeService` server cannot answer.
    pub(crate) fn hide_rune_unsupported(
        &mut self,
        descriptor: &RuneServiceDescriptor,
        probe: &SubsonicProbe,
        has_song_lyrics: bool,
    ) {
        for (hide_request, hides, uses) in REQUEST_ENDPOINTS {
            let missing = |used: &EndpointUse| endpoint_missing(descriptor, probe, used);
            let hidden = match hides {
                Hides::AnyEndpoint => uses.iter().any(missing),
                Hides::AllEndpoints => uses.iter().all(missing),
            };
            if hidden {
                hide_request(self);
            }
        }
        // The translator asks getLyricsBySongId when the server has songLyrics and getLyrics
        // otherwise, after reading the song, so only those can hide lyrics.
        let lyrics_unsupported = if has_song_lyrics {
            descriptor.is_unsupported("getLyricsBySongId", &["id"])
        } else {
            descriptor.is_unsupported("getLyrics", &["artist", "title"])
        };
        if lyrics_unsupported || descriptor.is_unsupported("getSong", &["id"]) {
            hide::<GetLyricByTrackIdRequest>(self);
        }
        if descriptor.is_unsupported("jukeboxControl", &[]) {
            self.hide_player();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        net::SocketAddr,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use axum::{
        Router,
        body::Body,
        extract::{Path, Query, RawQuery, State},
        http::{Response, StatusCode},
        routing::get,
    };
    use database::library_registry::SubsonicOpenMode;
    use playback::player::PlayingItem;
    use subsonic::{
        SubsonicClient, SubsonicCredentials, SubsonicError, SubsonicProbe, SubsonicRoles,
    };
    use tempfile::TempDir;
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;
    use uuid::Uuid;

    use super::{
        add_item_to_playlist_request, create_playlist_request, fetch_all_playlists_request,
        fetch_collection_group_summary_request, fetch_collection_groups_request,
        fetch_media_file_by_ids_request, fetch_media_files_request,
        fetch_parsed_media_file_request, fetch_playlist_occurrences_request, fetch_remote_playlist,
        get_liked_request, get_lyric_request, get_media_files_count_request,
        get_playlist_by_id_request, jukebox_error, mix_query_request,
        remove_item_from_playlist_request, remove_playlist_request,
        reorder_playlist_item_position_request, search_collection_summary_request,
        search_for_request, search_media_file_summary_request, set_liked_request,
        subsonic_request_error, update_playlist_request,
    };
    use crate::{
        TypedRequest,
        macros::{RequestSlots, RequestTranslator},
        messages::{
            AddItemToPlaylistRequest, AudioAvailability, CastPreflightRequest, CollectionType,
            CreateM3u8PlaylistRequest, CreatePlaylistRequest, FetchAllPlaylistsRequest,
            FetchCollectionGroupSummaryRequest, FetchCollectionGroupsRequest,
            FetchDirectoryTreeRequest, FetchMediaFileByIdsRequest, FetchMediaFilesRequest,
            FetchParsedMediaFileRequest, FetchPlaylistOccurrencesRequest, GetLikedRequest,
            GetLyricByTrackIdRequest, GetMediaFilesCountRequest, GetPlaylistByIdRequest,
            HandoffLoadRequest, LoadRequest, LoadResolvedItemsRequest, MixQuery, MixQueryRequest,
            MovePlaylistItemRequest, NextRequest, PauseRequest, PlayRequest, PlayingItemRef,
            PlaylistOccurrenceCursor, PlaylistOperateMode, PreviousRequest, RefreshSessionRequest,
            RemoveItemFromPlaylistRequest, RemovePlaylistRequest, RemoveRequest,
            ReorderPlaylistItemPositionRequest, SearchCollectionSummaryRequest, SearchForRequest,
            SearchMediaFileSummaryRequest, SeekRequest, SessionSnapshotRequest,
            SetAdaptiveSwitchingEnabledRequest, SetLikedRequest, SetPlaybackModeRequest,
            SetRealtimeFFTEnabledRequest, SwitchRequest, TakeBackHandoffBlockRequest,
            UpdatePlaylistRequest, VolumeRequest,
        },
        request_error::{ExpectedRequestError, classify},
        request_owner::Implementation,
        rpc::{Slot, TargetRequest, UnsupportedReason},
        subsonic_binding::{SubsonicBinding, SubsonicIdKind},
        utils::{Broadcaster, RinfRustSignal},
    };

    use crate::messages::RequestErrorKind;

    #[derive(Clone)]
    struct MockState {
        cover_calls: Arc<AtomicUsize>,
        requests: Arc<Mutex<Vec<String>>>,
        jukebox: Arc<Mutex<MockJukebox>>,
        jukebox_calls: Arc<Mutex<Vec<HashMap<String, String>>>>,
        scrobble_calls: Arc<Mutex<Vec<HashMap<String, String>>>>,
        reject_scrobble: Arc<AtomicUsize>,
        playlists: Arc<Mutex<Vec<MockPlaylist>>>,
        playlist_calls: MockPlaylistCalls,
        starred: Arc<Mutex<HashSet<String>>>,
        empty_search_rejected: Arc<AtomicUsize>,
    }

    type MockPlaylistCalls = Arc<Mutex<Vec<(String, Vec<(String, String)>)>>>;

    #[derive(Clone)]
    struct MockPlaylist {
        id: String,
        name: String,
        songs: Vec<String>,
    }

    struct MockJukebox {
        entries: Vec<serde_json::Value>,
        current_index: Option<u32>,
        playing: bool,
        gain: f32,
        position: u32,
    }

    impl Default for MockJukebox {
        fn default() -> Self {
            Self {
                entries: vec![mock_song("track-a"), mock_song("track-b")],
                current_index: Some(0),
                playing: false,
                gain: 1.0,
                position: 0,
            }
        }
    }

    struct MockServer {
        url: String,
        state: MockState,
        broadcasts: Arc<AtomicUsize>,
        _task: JoinHandle<()>,
    }

    struct CountingBroadcaster(Arc<AtomicUsize>);

    impl Broadcaster for CountingBroadcaster {
        fn broadcast(&self, _: &dyn RinfRustSignal) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    async fn mock_subsonic(root: &TempDir, jukebox_role: bool) -> (SubsonicBinding, MockServer) {
        mock_subsonic_mode(root, jukebox_role, SubsonicOpenMode::RemoteControl).await
    }

    async fn mock_subsonic_mode(
        root: &TempDir,
        jukebox_role: bool,
        mode: SubsonicOpenMode,
    ) -> (SubsonicBinding, MockServer) {
        let state = MockState {
            cover_calls: Arc::new(AtomicUsize::new(0)),
            requests: Arc::new(Mutex::new(Vec::new())),
            jukebox: Arc::new(Mutex::new(MockJukebox::default())),
            jukebox_calls: Arc::new(Mutex::new(Vec::new())),
            playlists: Arc::new(Mutex::new(vec![MockPlaylist {
                id: "playlist-1".into(),
                name: "Road".into(),
                songs: vec!["song-1".into(), "song-2".into(), "song-1".into()],
            }])),
            playlist_calls: Arc::new(Mutex::new(Vec::new())),
            scrobble_calls: Arc::new(Mutex::new(Vec::new())),
            reject_scrobble: Arc::new(AtomicUsize::new(0)),
            starred: Arc::new(Mutex::new(HashSet::new())),
            empty_search_rejected: Arc::new(AtomicUsize::new(0)),
        };
        let app = Router::new()
            .route("/rest/{*endpoint}", get(mock_endpoint))
            .with_state(state.clone());
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let url = format!("http://{address}");
        let client = SubsonicClient::new(
            url::Url::parse(&url).unwrap(),
            SubsonicCredentials::Password {
                user: "alice".into(),
                password: "cover-secret-password".into(),
            },
            reqwest::Client::new(),
        );
        let broadcasts = Arc::new(AtomicUsize::new(0));
        let binding = SubsonicBinding::new(
            Arc::new(client),
            SubsonicProbe {
                api_version: "1.16.1".into(),
                open_subsonic: false,
                server_type: None,
                server_version: None,
                extensions: Vec::new(),
                roles: SubsonicRoles {
                    jukebox_role,
                    ..SubsonicRoles::default()
                },
                roles_known: true,
            },
            root.path().to_path_buf(),
            Uuid::new_v4(),
            mode == SubsonicOpenMode::PlayOnThisDevice,
            Arc::new(CountingBroadcaster(Arc::clone(&broadcasts))),
        );
        (
            binding,
            MockServer {
                url,
                state,
                broadcasts,
                _task: task,
            },
        )
    }

    async fn mock_endpoint(
        State(state): State<MockState>,
        Path(endpoint): Path<String>,
        Query(query): Query<HashMap<String, String>>,
        RawQuery(raw_query): RawQuery,
    ) -> Response<Body> {
        let endpoint = endpoint.trim_end_matches(".view").to_owned();
        let params = raw_query
            .as_deref()
            .map(|query| {
                url::form_urlencoded::parse(query.as_bytes())
                    .map(|(key, value)| (key.into_owned(), value.into_owned()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let cover_id = query.get("id").cloned().unwrap_or_default();
        state.requests.lock().unwrap().push(endpoint.clone());
        if endpoint == "jukeboxControl" {
            state.jukebox_calls.lock().unwrap().push(query.clone());
            let mut jukebox = state.jukebox.lock().unwrap();
            let inner = match query.get("action").map(String::as_str) {
                Some("get") | Some("status") => {
                    let status = serde_json::json!({
                        "currentIndex": jukebox.current_index,
                        "playing": jukebox.playing,
                        "gain": jukebox.gain,
                        "position": jukebox.position,
                    });
                    if query.get("action").is_some_and(|action| action == "get") {
                        serde_json::json!({"jukeboxPlaylist": {
                            "entry": jukebox.entries.clone(),
                            "currentIndex": jukebox.current_index,
                            "playing": jukebox.playing,
                            "gain": jukebox.gain,
                            "position": jukebox.position,
                        }})
                    } else {
                        serde_json::json!({"jukeboxStatus": status})
                    }
                }
                Some("set") => {
                    jukebox.entries = vec![mock_song(
                        query.get("id").map(String::as_str).unwrap_or_default(),
                    )];
                    jukebox.current_index = Some(0);
                    serde_json::json!({})
                }
                Some("add") => {
                    jukebox.entries.push(mock_song(
                        query.get("id").map(String::as_str).unwrap_or_default(),
                    ));
                    serde_json::json!({})
                }
                Some("remove") => {
                    if let Some(index) = query
                        .get("index")
                        .and_then(|value| value.parse::<usize>().ok())
                        && index < jukebox.entries.len()
                    {
                        jukebox.entries.remove(index);
                    }
                    serde_json::json!({})
                }
                Some("clear") => {
                    jukebox.entries.clear();
                    jukebox.current_index = None;
                    serde_json::json!({})
                }
                Some("skip") => {
                    jukebox.current_index = query.get("index").and_then(|value| value.parse().ok());
                    jukebox.position = query
                        .get("offset")
                        .and_then(|value| value.parse().ok())
                        .unwrap_or_default();
                    serde_json::json!({})
                }
                Some("start") => {
                    jukebox.playing = true;
                    serde_json::json!({})
                }
                Some("stop") => {
                    jukebox.playing = false;
                    serde_json::json!({})
                }
                Some("setGain") => {
                    jukebox.gain = query
                        .get("gain")
                        .and_then(|value| value.parse().ok())
                        .unwrap_or_default();
                    serde_json::json!({})
                }
                Some("shuffle") => {
                    jukebox.entries.reverse();
                    serde_json::json!({})
                }
                _ => serde_json::json!({}),
            };
            return mock_response(inner);
        }
        if endpoint == "scrobble" {
            state.scrobble_calls.lock().unwrap().push(query.clone());
            if state.reject_scrobble.load(Ordering::SeqCst) != 0 {
                return Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":50,"message":"account is no longer authorized"}}}"#,
                    ))
                    .unwrap();
            }
        }
        if endpoint == "getCoverArt" {
            state.cover_calls.fetch_add(1, Ordering::SeqCst);
            if cover_id == "error-cover" {
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::from("cover-secret-password"))
                    .unwrap();
            }
            return Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "image/jpeg")
                .body(Body::from(format!("image-{cover_id}")))
                .unwrap();
        }
        if matches!(
            endpoint.as_str(),
            "getPlaylists" | "getPlaylist" | "createPlaylist" | "updatePlaylist" | "deletePlaylist"
        ) {
            state
                .playlist_calls
                .lock()
                .unwrap()
                .push((endpoint.clone(), params.clone()));
            let mut playlists = state.playlists.lock().unwrap();
            let get = |key: &str| {
                params
                    .iter()
                    .rev()
                    .find(|(name, _)| name == key)
                    .map(|(_, value)| value.as_str())
            };
            let inner = match endpoint.as_str() {
                "getPlaylists" => serde_json::json!({"playlists":{"playlist":playlists.iter().map(|playlist| serde_json::json!({"id":playlist.id,"name":playlist.name,"songCount":playlist.songs.len()})).collect::<Vec<_>>()}}),
                "getPlaylist" => playlists.iter().find(|playlist| Some(playlist.id.as_str()) == get("id")).map(|playlist| serde_json::json!({"playlist":{"id":playlist.id,"name":playlist.name,"entry":playlist.songs.iter().map(|id|mock_song(id)).collect::<Vec<_>>()}})).unwrap_or_else(|| serde_json::json!({})),
                "createPlaylist" => {
                    let songs = params.iter().filter(|(key, _)| key == "songId").map(|(_, value)| value.clone()).collect::<Vec<_>>();
                    if let Some(id) = get("playlistId") {
                        if let Some(playlist) = playlists.iter_mut().find(|playlist| playlist.id == id) { playlist.songs = songs; }
                    } else {
                        let id = format!("playlist-{}", playlists.len() + 1);
                        playlists.push(MockPlaylist { id, name: get("name").unwrap_or_default().into(), songs });
                    }
                    serde_json::json!({})
                }
                "updatePlaylist" => {
                    if let Some(playlist) = playlists.iter_mut().find(|playlist| Some(playlist.id.as_str()) == get("playlistId")) {
                        if let Some(name) = get("name") { playlist.name = name.into(); }
                        for (_, song) in params.iter().filter(|(key, _)| key == "songIdToAdd") { playlist.songs.push(song.clone()); }
                        let mut indexes = params.iter().filter(|(key, _)| key == "songIndexToRemove").filter_map(|(_, value)| value.parse::<usize>().ok()).collect::<Vec<_>>();
                        indexes.sort_unstable_by(|left, right| right.cmp(left));
                        for index in indexes { if index < playlist.songs.len() { playlist.songs.remove(index); } }
                    }
                    serde_json::json!({})
                }
                "deletePlaylist" => { playlists.retain(|playlist| Some(playlist.id.as_str()) != get("id")); serde_json::json!({}) }
                _ => serde_json::json!({}),
            };
            return mock_response(inner);
        }
        let inner = match endpoint.as_str() {
            "star" => {
                for (_, id) in params.iter().filter(|(key, _)| key == "id") {
                    state.starred.lock().unwrap().insert(id.clone());
                }
                serde_json::json!({})
            }
            "unstar" => {
                for (_, id) in params.iter().filter(|(key, _)| key == "id") {
                    state.starred.lock().unwrap().remove(id);
                }
                serde_json::json!({})
            }
            "getStarred2" => {
                serde_json::json!({"starred2":{"song":state.starred.lock().unwrap().iter().map(|id| mock_song(id)).collect::<Vec<_>>()}})
            }
            "getSong" => {
                let id = query.get("id").map(String::as_str).unwrap_or_default();
                if id == "minimal" {
                    return mock_response(
                        serde_json::json!({"song":{"id":"minimal","title":"Minimal"}}),
                    );
                }
                let mut song = mock_song(id);
                if state.starred.lock().unwrap().contains(id) {
                    song["starred"] = serde_json::json!("2026-01-01T00:00:00Z");
                }
                serde_json::json!({"song":song})
            }
            "getLyrics" if query.get("title").is_some_and(|title| title == "empty") => {
                serde_json::json!({})
            }
            "getLyrics" => {
                serde_json::json!({"lyrics":{"artist":"Artist","title":"song-1","value":"[00:01.00]plain line"}})
            }
            "getLyricsBySongId" if query.get("id").is_some_and(|id| id == "plain-extension") => {
                serde_json::json!({"lyricsList":{"plainLyrics":{"lang":"en","text":"extension plain text"}}})
            }
            "getLyricsBySongId" => {
                serde_json::json!({"lyricsList":{"structuredLyrics":{"lang":"en","synced":true,"line":[{"start":1200,"value":"timed line"}]}}})
            }
            "getScanStatus" => serde_json::json!({"scanStatus":{"scanning":false,"count":4}}),
            "search3"
                if query.get("query").is_some_and(String::is_empty)
                    && state.empty_search_rejected.load(Ordering::SeqCst) != 0 =>
            {
                return Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":10,"message":"query is required"}}}"#,
                    ))
                    .unwrap();
            }
            "search3" => {
                if query.get("query").is_some_and(|value| !value.is_empty()) {
                    return mock_response(serde_json::json!({"searchResult3": {
                        "song": [
                            {"id":"song-1","title":"One","coverArt":"cover-1"},
                            {"id":"song-2","title":"Two","coverArt":"cover-2"}
                        ]
                    }}));
                }
                let songs = ["song-1", "song-2", "song-3", "song-4"];
                let offset = query
                    .get("songOffset")
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or_default();
                let count = query
                    .get("songCount")
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or_default();
                let mut search = serde_json::Map::new();
                if query
                    .get("songCount")
                    .and_then(|value| value.parse::<u32>().ok())
                    .unwrap_or_default()
                    > 0
                {
                    search.insert(
                        "song".into(),
                        serde_json::json!(
                            songs
                                .iter()
                                .skip(offset)
                                .take(count)
                                .map(|id| mock_song(id))
                                .collect::<Vec<_>>()
                        ),
                    );
                }
                if query
                    .get("albumCount")
                    .and_then(|value| value.parse::<u32>().ok())
                    .unwrap_or_default()
                    > 0
                {
                    search.insert(
                        "album".into(),
                        serde_json::json!([{"id":"album-1","name":"Album One","artist":"Artist","coverArt":"cover-album"}]),
                    );
                }
                if query
                    .get("artistCount")
                    .and_then(|value| value.parse::<u32>().ok())
                    .unwrap_or_default()
                    > 0
                {
                    search.insert(
                        "artist".into(),
                        serde_json::json!([{"id":"artist-1","name":"Artist One","coverArt":"cover-artist"}]),
                    );
                }
                serde_json::json!({"searchResult3":search})
            }
            "getAlbum" => {
                let songs = ["song-1", "song-2"].map(mock_song);
                serde_json::json!({"album":{
                    "id":"album-1","name":"Album One","artist":"Artist","artistId":"artist-1",
                    "song":songs
                }})
            }
            "getArtists" => serde_json::json!({"artists":{"index":{
                "name":"A","artist":[
                    {"id":"artist-1","name":"Artist One","coverArt":"cover-1"},
                    {"id":"artist-2","name":"Artist Two","coverArt":"cover-2"}
                ]
            }}}),
            _ => serde_json::json!({}),
        };
        mock_response(inner)
    }

    fn mock_song(id: &str) -> serde_json::Value {
        serde_json::json!({"id":id,"title":id,"artist":"Artist","artistId":"artist-1","album":"Album","albumId":"album-1","duration":180,"track":7})
    }

    fn mock_response(inner: serde_json::Value) -> Response<Body> {
        let mut envelope = serde_json::json!({"subsonic-response":{
            "status":"ok","version":"1.16.1"
        }});
        envelope["subsonic-response"]
            .as_object_mut()
            .unwrap()
            .extend(inner.as_object().unwrap().clone());
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Body::from(envelope.to_string()))
            .unwrap()
    }

    async fn wait_for_covers(binding: &SubsonicBinding, cover_ids: &[&str]) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if cover_ids
                    .iter()
                    .all(|cover| binding.covers.path_for(cover).is_file())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn searched_and_browsed_covers_are_cached_locally_and_reused() {
        let root = TempDir::new().unwrap();
        let (binding, server) = mock_subsonic(&root, false).await;
        let search = search_for_request(
            &binding,
            &SearchForRequest {
                call_id: Some(1),
                query_str: "song".into(),
                fields: vec!["track".into()],
                n: 10,
            },
        )
        .await
        .unwrap()
        .unwrap();
        let fetch = |ids| FetchMediaFileByIdsRequest {
            call_id: None,
            ids,
            bake_cover_arts: true,
        };
        let first = fetch_media_file_by_ids_request(&binding, &fetch(search.tracks.clone()))
            .await
            .unwrap()
            .unwrap();
        assert!(first.cover_art_map.is_empty());
        assert!(first.media_files.iter().all(|file| file.cover_art_id > 0));
        wait_for_covers(&binding, &["cover-1", "cover-2"]).await;
        let second = fetch_media_file_by_ids_request(&binding, &fetch(search.tracks))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.cover_art_map.len(), 2);
        assert!(second.cover_art_map.values().all(|path| {
            path.starts_with(root.path().to_str().unwrap()) && !path.starts_with("http")
        }));
        assert_secret_free(&serde_json::to_string(&second).unwrap());
        let summary = FetchCollectionGroupSummaryRequest {
            call_id: None,
            collection_type: CollectionType::Artist,
        };
        fetch_collection_group_summary_request(&binding, &summary)
            .await
            .unwrap();
        let groups = fetch_collection_groups_request(
            &binding,
            &FetchCollectionGroupsRequest {
                call_id: None,
                collection_type: CollectionType::Artist,
                bake_cover_arts: true,
                group_titles: vec!["A".into()],
            },
        )
        .await
        .unwrap()
        .unwrap();
        let group = groups.groups.first().unwrap();
        assert_eq!(group.collections.len(), 2);
        assert!(
            group
                .collections
                .iter()
                .all(|collection| collection.cover_art_map.len() == 1)
        );
        assert_eq!(server.state.cover_calls.load(Ordering::SeqCst), 2);
        let error = binding
            .client
            .cover_art("error-cover", None)
            .await
            .unwrap_err();
        let error = format!("{error:?}");
        for secret in ["t=", "s=", "p=", "cover-secret-password"] {
            assert!(!error.contains(secret));
        }
        assert!(!server.url.contains("cover-secret-password"));
    }

    #[tokio::test]
    async fn playlist_translations_mutate_remote_state_and_reject_unknown_track_before_http() {
        let root = TempDir::new().unwrap();
        let (binding, server) = mock_subsonic(&root, false).await;
        let search = search_for_request(
            &binding,
            &SearchForRequest {
                call_id: None,
                query_str: "song".into(),
                fields: vec!["track".into()],
                n: 10,
            },
        )
        .await
        .unwrap()
        .unwrap();
        let playlists =
            fetch_all_playlists_request(&binding, &FetchAllPlaylistsRequest { call_id: None })
                .await
                .unwrap()
                .unwrap();
        let playlist_id = playlists.playlists[0].id;
        assert_eq!(playlists.playlists[0].name, "Road");
        assert_eq!(
            get_playlist_by_id_request(
                &binding,
                &GetPlaylistByIdRequest {
                    playlist_id,
                    call_id: None
                }
            )
            .await
            .unwrap()
            .unwrap()
            .playlist
            .name,
            "Road"
        );

        update_playlist_request(
            &binding,
            &UpdatePlaylistRequest {
                playlist_id,
                name: "Renamed".into(),
                group: String::new(),
                call_id: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            fetch_remote_playlist(&binding, playlist_id)
                .await
                .unwrap()
                .1,
            ["song-1", "song-2", "song-1"]
        );

        let call_count = server.state.playlist_calls.lock().unwrap().len();
        let error = match add_item_to_playlist_request(
            &binding,
            &AddItemToPlaylistRequest {
                playlist_id,
                media_file_id: i32::MAX,
                position: None,
                call_id: None,
            },
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("unknown track ID must be rejected"),
        };
        assert!(matches!(
            error.downcast_ref::<ExpectedRequestError>(),
            Some(ExpectedRequestError::InvalidInput { .. })
        ));
        assert_eq!(
            server.state.playlist_calls.lock().unwrap().len(),
            call_count
        );

        let added = add_item_to_playlist_request(
            &binding,
            &AddItemToPlaylistRequest {
                playlist_id,
                media_file_id: search.tracks[0],
                position: None,
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(added.occurrence_id, "3");
        assert_eq!(
            fetch_remote_playlist(&binding, playlist_id)
                .await
                .unwrap()
                .1
                .len(),
            4
        );

        remove_item_from_playlist_request(
            &binding,
            &RemoveItemFromPlaylistRequest {
                playlist_id,
                occurrence_id: added.occurrence_id,
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            fetch_remote_playlist(&binding, playlist_id)
                .await
                .unwrap()
                .1,
            ["song-1", "song-2", "song-1"]
        );

        reorder_playlist_item_position_request(
            &binding,
            &ReorderPlaylistItemPositionRequest {
                playlist_id,
                occurrence_id: "0".into(),
                new_position: 2,
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        let reordered = fetch_remote_playlist(&binding, playlist_id)
            .await
            .unwrap()
            .1;
        assert_eq!(reordered, ["song-2", "song-1", "song-1"]);
        let replace_call = server
            .state
            .playlist_calls
            .lock()
            .unwrap()
            .iter()
            .find(|(name, params)| {
                name == "createPlaylist" && params.iter().any(|(key, _)| key == "playlistId")
            })
            .cloned()
            .unwrap();
        let replaced_ids = replace_call
            .1
            .iter()
            .filter(|(key, _)| key == "songId")
            .map(|(_, value)| value.as_str())
            .collect::<Vec<_>>();
        assert_eq!(replaced_ids, ["song-2", "song-1", "song-1"]);

        remove_playlist_request(
            &binding,
            &RemovePlaylistRequest {
                playlist_id,
                call_id: None,
            },
        )
        .await
        .unwrap();
        assert!(binding.client.playlists().await.unwrap().is_empty());
        let created = create_playlist_request(
            &binding,
            &CreatePlaylistRequest {
                name: "Fresh".into(),
                group: String::new(),
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(created.playlist.name, "Fresh");
        assert_eq!(
            fetch_all_playlists_request(&binding, &FetchAllPlaylistsRequest { call_id: None })
                .await
                .unwrap()
                .unwrap()
                .playlists
                .len(),
            1
        );
    }

    #[test]
    fn subsonic_playlist_capabilities_support_only_expressible_playlist_requests() {
        let slots = RequestSlots::subsonic(&SubsonicProbe {
            api_version: "1.16.1".into(),
            open_subsonic: false,
            server_type: None,
            server_version: None,
            extensions: Vec::new(),
            roles: SubsonicRoles::default(),
            roles_known: true,
        });
        for slot in [
            FetchAllPlaylistsRequest::slot(&slots),
            GetPlaylistByIdRequest::slot(&slots),
            CreatePlaylistRequest::slot(&slots),
            UpdatePlaylistRequest::slot(&slots),
            RemovePlaylistRequest::slot(&slots),
            AddItemToPlaylistRequest::slot(&slots),
            RemoveItemFromPlaylistRequest::slot(&slots),
            ReorderPlaylistItemPositionRequest::slot(&slots),
            FetchPlaylistOccurrencesRequest::slot(&slots),
        ] {
            assert_eq!(slot, Slot::Translate);
        }
        assert_eq!(
            CreateM3u8PlaylistRequest::slot(&slots),
            Slot::Unsupported {
                reason: UnsupportedReason::NotImplemented
            }
        );
    }

    fn occurrences_request(
        playlist_id: i32,
        after: Option<PlaylistOccurrenceCursor>,
        page_size: u32,
    ) -> FetchPlaylistOccurrencesRequest {
        FetchPlaylistOccurrencesRequest {
            request_id: "page".into(),
            playlist_id,
            after,
            page_size,
            bake_cover_arts: true,
            call_id: None,
        }
    }

    /// `(occurrence id, position, track)` of every entry of a page.
    fn occurrence_rows(
        response: &crate::messages::FetchPlaylistOccurrencesResponse,
    ) -> Vec<(String, i32, String)> {
        response
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.occurrence_id.clone(),
                    entry.position,
                    entry.track_uuid.clone(),
                )
            })
            .collect()
    }

    fn row(occurrence: &str, position: i32, track: &str) -> (String, i32, String) {
        (occurrence.into(), position, track.into())
    }

    #[tokio::test]
    async fn subsonic_playlist_occurrences_page_by_position_and_keep_duplicates_apart() {
        let root = TempDir::new().unwrap();
        let (binding, _server) = mock_subsonic(&root, false).await;
        let playlists =
            fetch_all_playlists_request(&binding, &FetchAllPlaylistsRequest { call_id: None })
                .await
                .unwrap()
                .unwrap();
        let playlist_id = playlists.playlists[0].id;

        let first = fetch_playlist_occurrences_request(
            &binding,
            &occurrences_request(playlist_id, None, 2),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(first.request_id, "page");
        assert_eq!(
            occurrence_rows(&first),
            [row("0", 0, "song-1"), row("1", 1, "song-2")]
        );
        let next = first.next.clone().expect("a second page");
        assert_eq!((next.position, next.occurrence_id.as_str()), (1, "1"));
        let file = first.entries[0].file.as_ref().unwrap();
        assert_eq!(file.track, "song-1");
        assert_eq!(file.title, "song-1");
        assert_eq!(file.library_id, binding.library_id.to_string());
        assert_eq!(
            binding.catalog.lock().unwrap().songs[&file.id].title,
            "song-1"
        );

        let second = fetch_playlist_occurrences_request(
            &binding,
            &occurrences_request(playlist_id, Some(next), 2),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(occurrence_rows(&second), [row("2", 2, "song-1")]);
        assert!(second.next.is_none());
        // The same song twice: one file, two occurrences.
        assert_eq!(
            second.entries[0].file.as_ref().unwrap().id,
            first.entries[0].file.as_ref().unwrap().id
        );
        assert_ne!(
            second.entries[0].occurrence_id,
            first.entries[0].occurrence_id
        );

        // A page that fits the whole playlist names no next page.
        let whole = fetch_playlist_occurrences_request(
            &binding,
            &occurrences_request(playlist_id, None, 3),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(whole.entries.len(), 3);
        assert!(whole.next.is_none());

        for request in [
            occurrences_request(playlist_id, None, 0),
            occurrences_request(playlist_id, None, 129),
            occurrences_request(
                playlist_id,
                Some(PlaylistOccurrenceCursor {
                    position: 1,
                    occurrence_id: "2".into(),
                }),
                2,
            ),
            occurrences_request(
                playlist_id,
                Some(PlaylistOccurrenceCursor {
                    position: -1,
                    occurrence_id: "-1".into(),
                }),
                2,
            ),
        ] {
            let error = fetch_playlist_occurrences_request(&binding, &request)
                .await
                .err()
                .expect("bad pages are refused");
            assert_eq!(classify(&error), RequestErrorKind::InvalidInput);
        }
    }

    #[tokio::test]
    async fn subsonic_occurrence_ids_are_what_playlist_edits_take() {
        let root = TempDir::new().unwrap();
        let (binding, server) = mock_subsonic(&root, false).await;
        let playlists =
            fetch_all_playlists_request(&binding, &FetchAllPlaylistsRequest { call_id: None })
                .await
                .unwrap()
                .unwrap();
        let playlist_id = playlists.playlists[0].id;
        let before = fetch_playlist_occurrences_request(
            &binding,
            &occurrences_request(playlist_id, None, 10),
        )
        .await
        .unwrap()
        .unwrap();
        // Remove the second copy of song-1, by the id the page gave it.
        let duplicate = before.entries[2].occurrence_id.clone();
        remove_item_from_playlist_request(
            &binding,
            &RemoveItemFromPlaylistRequest {
                playlist_id,
                occurrence_id: duplicate,
                call_id: None,
            },
        )
        .await
        .unwrap();
        let removal = server
            .state
            .playlist_calls
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(endpoint, _)| endpoint == "updatePlaylist")
            .cloned()
            .unwrap();
        assert!(
            removal
                .1
                .contains(&("songIndexToRemove".to_owned(), "2".to_owned()))
        );

        let after = fetch_playlist_occurrences_request(
            &binding,
            &occurrences_request(playlist_id, None, 10),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            occurrence_rows(&after),
            [row("0", 0, "song-1"), row("1", 1, "song-2")]
        );
    }

    fn get_playlist_calls(server: &MockServer) -> usize {
        server
            .state
            .playlist_calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(endpoint, _)| endpoint == "getPlaylist")
            .count()
    }

    #[tokio::test]
    async fn occurrence_pages_read_the_playlist_once_until_an_edit() {
        let root = TempDir::new().unwrap();
        let (binding, server) = mock_subsonic(&root, false).await;
        let playlist_id =
            fetch_all_playlists_request(&binding, &FetchAllPlaylistsRequest { call_id: None })
                .await
                .unwrap()
                .unwrap()
                .playlists[0]
                .id;
        let mut after = None;
        let mut rows = Vec::new();
        for _ in 0..3 {
            let page = fetch_playlist_occurrences_request(
                &binding,
                &occurrences_request(playlist_id, after.clone(), 1),
            )
            .await
            .unwrap()
            .unwrap();
            rows.extend(occurrence_rows(&page));
            after = page.next;
        }
        assert!(after.is_none());
        assert_eq!(
            rows,
            [
                row("0", 0, "song-1"),
                row("1", 1, "song-2"),
                row("2", 2, "song-1")
            ]
        );
        assert_eq!(get_playlist_calls(&server), 1);

        remove_item_from_playlist_request(
            &binding,
            &RemoveItemFromPlaylistRequest {
                playlist_id,
                occurrence_id: "0".into(),
                call_id: None,
            },
        )
        .await
        .unwrap();
        let reads = get_playlist_calls(&server);
        let fresh = fetch_playlist_occurrences_request(
            &binding,
            &occurrences_request(playlist_id, None, 10),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(get_playlist_calls(&server), reads + 1);
        assert_eq!(
            occurrence_rows(&fresh),
            [row("0", 0, "song-2"), row("1", 1, "song-1")]
        );

        // A cursor past this playlist's entries is another playlist's.
        let error = fetch_playlist_occurrences_request(
            &binding,
            &occurrences_request(
                playlist_id,
                Some(PlaylistOccurrenceCursor {
                    position: 5,
                    occurrence_id: "5".into(),
                }),
                10,
            ),
        )
        .await
        .err()
        .expect("a cursor of another playlist is refused");
        assert_eq!(classify(&error), RequestErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn an_ordinary_server_opens_a_playlist_detail_query() {
        let root = TempDir::new().unwrap();
        let (binding, server) = mock_subsonic(&root, false).await;
        let playlist_id =
            fetch_all_playlists_request(&binding, &FetchAllPlaylistsRequest { call_id: None })
                .await
                .unwrap()
                .unwrap()
                .playlists[0]
                .id;
        let page = |cursor| MixQueryRequest {
            queries: vec![MixQuery {
                operator: "lib::playlist".into(),
                parameter: playlist_id.to_string(),
            }],
            cursor,
            page_size: 2,
            bake_cover_arts: false,
            call_id: None,
        };
        let titles = |response: crate::messages::MixQueryResponse| {
            response
                .files
                .into_iter()
                .map(|file| file.title)
                .collect::<Vec<_>>()
        };
        let first = mix_query_request(&binding, &page(0))
            .await
            .unwrap()
            .unwrap();
        let second = mix_query_request(&binding, &page(2))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(titles(first), ["song-1", "song-2"]);
        assert_eq!(titles(second), ["song-1"]);
        assert_eq!(get_playlist_calls(&server), 1);

        // An edit of the playlist makes the next page read it again.
        remove_item_from_playlist_request(
            &binding,
            &RemoveItemFromPlaylistRequest {
                playlist_id,
                occurrence_id: "1".into(),
                call_id: None,
            },
        )
        .await
        .unwrap();
        let reads = get_playlist_calls(&server);
        let edited = mix_query_request(&binding, &page(0))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(get_playlist_calls(&server), reads + 1);
        assert_eq!(titles(edited), ["song-1", "song-1"]);
    }

    #[test]
    fn playlist_occurrences_hide_with_get_playlist() {
        assert_eq!(
            FetchPlaylistOccurrencesRequest::slot(&rune_slots(&[], false)),
            Slot::Translate
        );
        assert_eq!(
            FetchPlaylistOccurrencesRequest::slot(&rune_slots(&[("getPlaylist", None)], false)),
            hidden()
        );
        // Only the parameter Rune sends counts.
        assert_eq!(
            FetchPlaylistOccurrencesRequest::slot(&rune_slots(
                &[("getPlaylist", Some("other"))],
                false
            )),
            Slot::Translate
        );
    }

    #[tokio::test]
    async fn subsonic_likes_and_lyrics_translate_and_reject_cross_library_items() {
        let root = TempDir::new().unwrap();
        let (mut binding, server) = mock_subsonic(&root, false).await;
        let item = PlayingItemRef::LibraryTrack {
            library_id: binding.library_id.to_string(),
            track: "song-1".into(),
        };
        let request = SetLikedRequest {
            item: Some(item.clone()),
            liked: true,
            call_id: Some(9),
        };
        let response = set_liked_request(&binding, &request)
            .await
            .unwrap()
            .unwrap();
        assert!(response.success && response.liked);
        assert!(
            get_liked_request(
                &binding,
                &GetLikedRequest {
                    item: Some(item.clone()),
                    call_id: None
                }
            )
            .await
            .unwrap()
            .unwrap()
            .liked
        );
        let before = server.state.requests.lock().unwrap().len();
        let foreign_item = PlayingItemRef::LibraryTrack {
            library_id: Uuid::new_v4().to_string(),
            track: "song-1".into(),
        };
        assert!(
            set_liked_request(
                &binding,
                &SetLikedRequest {
                    item: Some(foreign_item.clone()),
                    liked: false,
                    call_id: None
                }
            )
            .await
            .is_err()
        );
        assert!(
            get_liked_request(
                &binding,
                &GetLikedRequest {
                    item: Some(foreign_item.clone()),
                    call_id: None
                }
            )
            .await
            .is_err()
        );
        assert!(
            get_lyric_request(
                &binding,
                &GetLyricByTrackIdRequest {
                    item: Some(foreign_item),
                    call_id: None
                }
            )
            .await
            .is_err()
        );
        assert_eq!(server.state.requests.lock().unwrap().len(), before);
        set_liked_request(
            &binding,
            &SetLikedRequest {
                item: Some(item.clone()),
                liked: false,
                call_id: None,
            },
        )
        .await
        .unwrap();
        assert!(
            !get_liked_request(
                &binding,
                &GetLikedRequest {
                    item: Some(item.clone()),
                    call_id: None
                }
            )
            .await
            .unwrap()
            .unwrap()
            .liked
        );

        binding
            .probe
            .extensions
            .push(("songLyrics".into(), vec![1]));
        let lyrics = get_lyric_request(
            &binding,
            &GetLyricByTrackIdRequest {
                item: Some(item.clone()),
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(lyrics.lines[0].start_time, 1200);
        assert_eq!(lyrics.lines[0].sections[0].content, "timed line");
        let plain_extension_item = PlayingItemRef::LibraryTrack {
            library_id: binding.library_id.to_string(),
            track: "plain-extension".into(),
        };
        let lyrics = get_lyric_request(
            &binding,
            &GetLyricByTrackIdRequest {
                item: Some(plain_extension_item),
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(lyrics.lines[0].start_time, 0);
        assert_eq!(lyrics.lines[0].sections[0].content, "extension plain text");
        binding.probe.extensions.clear();
        let lyrics = get_lyric_request(
            &binding,
            &GetLyricByTrackIdRequest {
                item: Some(item),
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(lyrics.lines[0].sections[0].content, "plain line");
        let empty = PlayingItemRef::LibraryTrack {
            library_id: binding.library_id.to_string(),
            track: "empty".into(),
        };
        let lyrics = get_lyric_request(
            &binding,
            &GetLyricByTrackIdRequest {
                item: Some(empty),
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert!(lyrics.lines.is_empty());
    }

    #[test]
    fn subsonic_likes_and_lyrics_capabilities_are_supported_without_extensions() {
        let slots = RequestSlots::subsonic(&SubsonicProbe {
            api_version: "1.16.1".into(),
            open_subsonic: false,
            server_type: None,
            server_version: None,
            extensions: Vec::new(),
            roles: SubsonicRoles::default(),
            roles_known: true,
        });
        assert_eq!(SetLikedRequest::slot(&slots), Slot::Native);
        assert_eq!(GetLikedRequest::slot(&slots), Slot::Native);
        assert_eq!(GetLyricByTrackIdRequest::slot(&slots), Slot::Translate);
    }

    #[test]
    fn subsonic_track_and_summary_capabilities_exclude_directory_tree() {
        let slots = RequestSlots::subsonic(&SubsonicProbe {
            api_version: "1.16.1".into(),
            open_subsonic: false,
            server_type: None,
            server_version: None,
            extensions: Vec::new(),
            roles: SubsonicRoles::default(),
            roles_known: true,
        });
        for slot in [
            FetchMediaFilesRequest::slot(&slots),
            GetMediaFilesCountRequest::slot(&slots),
            FetchParsedMediaFileRequest::slot(&slots),
            SearchMediaFileSummaryRequest::slot(&slots),
            SearchCollectionSummaryRequest::slot(&slots),
        ] {
            assert_eq!(slot, Slot::Translate);
        }
        assert_eq!(
            FetchDirectoryTreeRequest::slot(&slots),
            Slot::Unsupported {
                reason: UnsupportedReason::NotImplemented
            }
        );
    }

    #[tokio::test]
    async fn subsonic_track_pages_walk_every_song_once_in_order() {
        let root = TempDir::new().unwrap();
        let (binding, _) = mock_subsonic(&root, false).await;
        let mut songs = Vec::new();
        for cursor in [0, 2, 4] {
            songs.extend(
                fetch_media_files_request(
                    &binding,
                    &FetchMediaFilesRequest {
                        cursor,
                        page_size: 2,
                        bake_cover_arts: false,
                        call_id: Some(22),
                    },
                )
                .await
                .unwrap()
                .unwrap()
                .media_files
                .into_iter()
                .map(|file| file.track),
            );
        }
        assert_eq!(songs, ["song-1", "song-2", "song-3", "song-4"]);
    }

    #[tokio::test]
    async fn subsonic_track_count_matches_scan_status_song_total() {
        let root = TempDir::new().unwrap();
        let (binding, server) = mock_subsonic(&root, false).await;
        let count = get_media_files_count_request(
            &binding,
            &GetMediaFilesCountRequest { call_id: Some(23) },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(count.count, 4);
        assert_eq!(count.call_id, Some(23));
        let requests = server.state.requests.lock().unwrap();
        assert_eq!(requests.as_slice(), ["getScanStatus"]);
    }

    #[tokio::test]
    async fn subsonic_track_details_map_server_fields_and_keep_missing_fields_empty() {
        let root = TempDir::new().unwrap();
        let (binding, _) = mock_subsonic(&root, false).await;
        let page = fetch_media_files_request(
            &binding,
            &FetchMediaFilesRequest {
                cursor: 0,
                page_size: 1,
                bake_cover_arts: false,
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        let details = fetch_parsed_media_file_request(
            &binding,
            &FetchParsedMediaFileRequest {
                id: page.media_files[0].id,
                call_id: Some(24),
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(details.file.track, "song-1");
        assert_eq!(details.file.title, "song-1");
        assert_eq!(details.file.artist, "Artist");
        assert_eq!(details.file.album, "Album");
        assert_eq!(details.file.duration, 180.0);
        assert_eq!(details.file.track_number, 7);
        assert_eq!(details.album.name, "Album");
        assert_eq!(details.artists[0].name, "Artist");
        assert_eq!(details.call_id, Some(24));
        let missing_id = binding
            .catalog
            .lock()
            .unwrap()
            .ids
            .intern(SubsonicIdKind::Track, "minimal")
            .unwrap();
        let missing = fetch_parsed_media_file_request(
            &binding,
            &FetchParsedMediaFileRequest {
                id: missing_id,
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert!(missing.artists.is_empty());
        assert_eq!(missing.album.id, 0);
        assert!(missing.album.name.is_empty());
    }

    #[tokio::test]
    async fn subsonic_tracks_are_playable_in_every_track_list() {
        let root = TempDir::new().unwrap();
        let (binding, _) = mock_subsonic(&root, false).await;
        let page = fetch_media_files_request(
            &binding,
            &FetchMediaFilesRequest {
                cursor: 0,
                page_size: 4,
                bake_cover_arts: false,
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap()
        .media_files;
        let by_ids = fetch_media_file_by_ids_request(
            &binding,
            &FetchMediaFileByIdsRequest {
                call_id: None,
                ids: page.iter().map(|file| file.id).collect(),
                bake_cover_arts: false,
            },
        )
        .await
        .unwrap()
        .unwrap()
        .media_files;
        let album = search_collection_summary_request(
            &binding,
            &SearchCollectionSummaryRequest {
                collection_type: Some(CollectionType::Album),
                bake_cover_arts: Some(false),
                n: 1,
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap()
        .result
        .remove(0);
        let album_tracks = mix_query_request(
            &binding,
            &MixQueryRequest {
                queries: vec![MixQuery {
                    operator: "lib::album".into(),
                    parameter: album.id.to_string(),
                }],
                cursor: 0,
                page_size: 10,
                bake_cover_arts: false,
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap()
        .files;
        let summaries = search_media_file_summary_request(
            &binding,
            &SearchMediaFileSummaryRequest {
                n: 4,
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap()
        .result;
        assert!(!page.is_empty() && !by_ids.is_empty() && !album_tracks.is_empty());
        assert!(!summaries.is_empty());
        let playable = AudioAvailability::RemoteOnly;
        for file in page.iter().chain(&by_ids).chain(&album_tracks) {
            assert_eq!(file.audio_availability, playable, "{}", file.track);
        }
        for summary in &summaries {
            assert_eq!(summary.audio_availability, playable, "{}", summary.name);
        }
    }

    #[tokio::test]
    async fn subsonic_track_and_collection_summaries_return_requested_kinds() {
        let root = TempDir::new().unwrap();
        let (binding, _) = mock_subsonic(&root, false).await;
        let tracks = search_media_file_summary_request(
            &binding,
            &SearchMediaFileSummaryRequest {
                n: 3,
                call_id: Some(25),
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(tracks.result.len(), 3);
        assert_eq!(tracks.result[0].name, "song-1");
        let albums = search_collection_summary_request(
            &binding,
            &SearchCollectionSummaryRequest {
                collection_type: Some(CollectionType::Album),
                bake_cover_arts: Some(false),
                n: 2,
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(albums.collection_type, CollectionType::Album);
        assert_eq!(albums.result[0].name, "Album One");
        let artists = search_collection_summary_request(
            &binding,
            &SearchCollectionSummaryRequest {
                collection_type: Some(CollectionType::Artist),
                bake_cover_arts: Some(false),
                n: 2,
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(artists.collection_type, CollectionType::Artist);
        assert_eq!(artists.result[0].name, "Artist One");
    }

    #[tokio::test]
    async fn empty_search_rejection_is_typed_unsupported() {
        let root = TempDir::new().unwrap();
        let (binding, server) = mock_subsonic(&root, false).await;
        server
            .state
            .empty_search_rejected
            .store(1, Ordering::SeqCst);
        let error = fetch_media_files_request(
            &binding,
            &FetchMediaFilesRequest {
                cursor: 0,
                page_size: 10,
                bake_cover_arts: false,
                call_id: None,
            },
        )
        .await
        .err()
        .expect("empty search should be rejected");
        assert!(matches!(
            error.downcast_ref::<ExpectedRequestError>(),
            Some(ExpectedRequestError::Unsupported {
                reason: UnsupportedReason::NotImplemented,
                ..
            })
        ));
        assert_eq!(
            server.state.requests.lock().unwrap().as_slice(),
            ["search3"]
        );
    }

    struct InMemoryCatalog;

    impl subsonic::api::Browsing for InMemoryCatalog {
        fn artists<'a>(&'a self) -> subsonic::ApiFuture<'a, Vec<subsonic::SubsonicArtist>> {
            Box::pin(async {
                Ok(vec![
                    subsonic::SubsonicArtist {
                        id: "artist-1".into(),
                        name: "Artist One".into(),
                        ..Default::default()
                    },
                    subsonic::SubsonicArtist {
                        id: "artist-2".into(),
                        name: "Artist Two".into(),
                        ..Default::default()
                    },
                ])
            })
        }
    }

    impl subsonic::api::Searching for InMemoryCatalog {
        fn search_with_song_offset<'a>(
            &'a self,
            query: &'a str,
            _limits: subsonic::SearchLimits,
            _song_offset: u32,
        ) -> subsonic::ApiFuture<'a, subsonic::SubsonicSearchResult> {
            Box::pin(async move {
                Ok(subsonic::SubsonicSearchResult {
                    songs: vec![subsonic::SubsonicSong {
                        id: "song-1".into(),
                        title: format!("match for {query}"),
                        ..Default::default()
                    }],
                    ..Default::default()
                })
            })
        }
    }

    impl subsonic::api::System for InMemoryCatalog {}
    impl subsonic::api::Lists for InMemoryCatalog {}
    impl subsonic::api::Playlists for InMemoryCatalog {}
    impl subsonic::api::Annotation for InMemoryCatalog {}
    impl subsonic::api::MediaRetrieval for InMemoryCatalog {}
    impl subsonic::api::Jukebox for InMemoryCatalog {}
    impl subsonic::api::Scanning for InMemoryCatalog {}
    impl subsonic::api::RuneVisualizerFeed for InMemoryCatalog {}
    impl subsonic::api::RuneCatalog for InMemoryCatalog {}
    impl subsonic::api::RuneRecommendations for InMemoryCatalog {}
    impl subsonic::api::RuneService for InMemoryCatalog {}

    #[tokio::test]
    async fn in_memory_api_serves_browse_and_search_and_reports_missing_facets_as_unsupported() {
        let root = TempDir::new().unwrap();
        let binding = SubsonicBinding::new(
            Arc::new(InMemoryCatalog),
            SubsonicProbe {
                api_version: "1.16.1".into(),
                open_subsonic: false,
                server_type: None,
                server_version: None,
                extensions: Vec::new(),
                roles: SubsonicRoles::default(),
                roles_known: true,
            },
            root.path().to_path_buf(),
            Uuid::new_v4(),
            false,
            Arc::new(CountingBroadcaster(Arc::new(AtomicUsize::new(0)))),
        );

        let groups = fetch_collection_group_summary_request(
            &binding,
            &FetchCollectionGroupSummaryRequest {
                collection_type: CollectionType::Artist,
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            groups.groups.iter().map(|group| group.count).sum::<i32>(),
            2
        );

        let search = search_for_request(
            &binding,
            &SearchForRequest {
                call_id: None,
                query_str: "song".into(),
                fields: vec!["track".into()],
                n: 10,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(search.tracks.len(), 1);
        assert_eq!(
            binding.catalog.lock().unwrap().songs[&search.tracks[0]].title,
            "match for song"
        );

        let error =
            fetch_all_playlists_request(&binding, &FetchAllPlaylistsRequest { call_id: None })
                .await
                .err()
                .expect("playlists are not implemented by the in-memory API");
        assert!(matches!(
            error.downcast_ref::<ExpectedRequestError>(),
            Some(ExpectedRequestError::Unsupported {
                request: "getPlaylists",
                reason: UnsupportedReason::NotImplemented,
            })
        ));
    }

    struct HostedCatalog {
        playback: Option<subsonic::RunePlayback>,
        song_calls: Arc<AtomicUsize>,
    }

    impl subsonic::api::Browsing for HostedCatalog {
        fn song<'a>(&'a self, id: &'a str) -> subsonic::ApiFuture<'a, subsonic::SubsonicSong> {
            self.song_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok(subsonic::SubsonicSong {
                    id: id.into(),
                    title: "Song".into(),
                    rune_playback: self.playback.clone(),
                    ..Default::default()
                })
            })
        }
    }

    impl subsonic::api::System for HostedCatalog {}
    impl subsonic::api::Lists for HostedCatalog {}
    impl subsonic::api::Searching for HostedCatalog {}
    impl subsonic::api::Playlists for HostedCatalog {}
    impl subsonic::api::Annotation for HostedCatalog {}
    impl subsonic::api::MediaRetrieval for HostedCatalog {}
    impl subsonic::api::Jukebox for HostedCatalog {}
    impl subsonic::api::Scanning for HostedCatalog {}
    impl subsonic::api::RuneVisualizerFeed for HostedCatalog {}
    impl subsonic::api::RuneCatalog for HostedCatalog {}
    impl subsonic::api::RuneRecommendations for HostedCatalog {}
    impl subsonic::api::RuneService for HostedCatalog {}

    fn hosted_binding(
        root: &TempDir,
        playback: Option<subsonic::RunePlayback>,
        rune_service: bool,
    ) -> (Arc<SubsonicBinding>, Arc<AtomicUsize>) {
        let song_calls = Arc::new(AtomicUsize::new(0));
        let binding = SubsonicBinding::new(
            Arc::new(HostedCatalog {
                playback,
                song_calls: song_calls.clone(),
            }),
            SubsonicProbe {
                api_version: "1.16.1".into(),
                open_subsonic: true,
                server_type: None,
                server_version: None,
                extensions: if rune_service {
                    vec![("runeService".into(), vec![1])]
                } else {
                    Vec::new()
                },
                roles: SubsonicRoles::default(),
                roles_known: true,
            },
            root.path().to_path_buf(),
            Uuid::new_v4(),
            false,
            Arc::new(CountingBroadcaster(Arc::new(AtomicUsize::new(0)))),
        );
        (Arc::new(binding), song_calls)
    }

    fn hosted_playback(mode: subsonic::RunePlaybackMode) -> subsonic::RunePlayback {
        subsonic::RunePlayback {
            mode,
            host: Some("appleMusic".into()),
            key: Some("i.x".into()),
            reason: None,
        }
    }

    fn subsonic_item() -> playback::player::SubsonicItem {
        playback::player::SubsonicItem {
            library_id: Uuid::new_v4(),
            track_id: "song-1".into(),
        }
    }

    #[tokio::test]
    async fn rune_service_server_hosts_a_hosted_song() {
        let root = TempDir::new().unwrap();
        let (binding, calls) = hosted_binding(
            &root,
            Some(hosted_playback(subsonic::RunePlaybackMode::Hosted)),
            true,
        );
        match binding.playback_target(subsonic_item()).await.unwrap() {
            playback::player::SubsonicTarget::Hosted(track) => {
                assert_eq!(track.host, "appleMusic");
                assert_eq!(track.key, "i.x");
            }
            playback::player::SubsonicTarget::Stream(_) => panic!("expected a hosted track"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn rune_service_songs_without_hosting_stream_with_one_lookup() {
        let root = TempDir::new().unwrap();
        for playback in [
            None,
            Some(hosted_playback(subsonic::RunePlaybackMode::Stream)),
            Some(hosted_playback(subsonic::RunePlaybackMode::Unknown)),
        ] {
            let (binding, calls) = hosted_binding(&root, playback, true);
            assert!(matches!(
                binding.playback_target(subsonic_item()).await.unwrap(),
                playback::player::SubsonicTarget::Stream(_)
            ));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn unavailable_songs_fail_with_the_closest_reason() {
        use playback::player::UnavailableReason;
        use subsonic::RunePlaybackReason as Reason;
        let root = TempDir::new().unwrap();
        for (reason, expected) in [
            (
                Some(Reason::NotSubscribed),
                UnavailableReason::AccessRevoked,
            ),
            (Some(Reason::Region), UnavailableReason::SourceUnreachable),
            (Some(Reason::Removed), UnavailableReason::TrackGone),
            (None, UnavailableReason::SourceUnreachable),
        ] {
            let mut playback = hosted_playback(subsonic::RunePlaybackMode::Unavailable);
            playback.reason = reason;
            let (binding, _) = hosted_binding(&root, Some(playback), true);
            let error = binding
                .playback_target(subsonic_item())
                .await
                .err()
                .expect("unavailable songs do not play");
            assert_eq!(UnavailableReason::classify(&error), expected);
        }
    }

    #[tokio::test]
    async fn a_server_without_rune_service_streams_and_never_looks_at_hosting() {
        let root = TempDir::new().unwrap();
        let (binding, calls) = hosted_binding(
            &root,
            Some(hosted_playback(subsonic::RunePlaybackMode::Hosted)),
            false,
        );
        assert!(matches!(
            binding.playback_target(subsonic_item()).await.unwrap(),
            playback::player::SubsonicTarget::Stream(_)
        ));
        // The stream path looks the song up once for its size and suffix, as it always did.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    fn assert_secret_free(value: &str) {
        for secret in ["t=", "s=", "p=", "cover-secret-password"] {
            assert!(!value.contains(secret));
        }
    }

    #[test]
    fn jukebox_capabilities_expose_only_controls_supported_by_subsonic() {
        let probe = SubsonicProbe {
            api_version: "1.16.1".into(),
            open_subsonic: false,
            server_type: None,
            server_version: None,
            extensions: Vec::new(),
            roles: SubsonicRoles {
                jukebox_role: true,
                ..SubsonicRoles::default()
            },
            roles_known: true,
        };
        let slots = RequestSlots::subsonic(&probe);
        assert_eq!(PlayRequest::slot(&slots), Slot::Translate);
        assert_eq!(PauseRequest::slot(&slots), Slot::Translate);
        assert_eq!(NextRequest::slot(&slots), Slot::Translate);
        assert_eq!(PreviousRequest::slot(&slots), Slot::Translate);
        assert_eq!(SwitchRequest::slot(&slots), Slot::Translate);
        assert_eq!(SeekRequest::slot(&slots), Slot::Translate);
        assert_eq!(VolumeRequest::slot(&slots), Slot::Translate);
        assert_eq!(LoadRequest::slot(&slots), Slot::Translate);
        assert_eq!(LoadResolvedItemsRequest::slot(&slots), Slot::Translate);
        assert_eq!(
            HandoffLoadRequest::slot(&slots),
            Slot::Unsupported {
                reason: UnsupportedReason::NotImplemented
            }
        );
        assert_eq!(
            TakeBackHandoffBlockRequest::slot(&slots),
            Slot::Unsupported {
                reason: UnsupportedReason::NotImplemented
            }
        );
        assert_eq!(RefreshSessionRequest::slot(&slots), Slot::Translate);
        assert_eq!(RemoveRequest::slot(&slots), Slot::Translate);
        // Existing capability defaults remain in place for unsupported jukebox operations.
        assert_eq!(
            MovePlaylistItemRequest::slot(&slots),
            Slot::Unsupported {
                reason: UnsupportedReason::NotImplemented
            }
        );
        assert_eq!(SetPlaybackModeRequest::slot(&slots), Slot::Translate);
        assert_eq!(
            SetRealtimeFFTEnabledRequest::slot(&slots),
            Slot::Unsupported {
                reason: UnsupportedReason::NotImplemented
            }
        );
        assert_eq!(
            SetAdaptiveSwitchingEnabledRequest::slot(&slots),
            Slot::Unsupported {
                reason: UnsupportedReason::NotImplemented
            }
        );
        assert_eq!(
            CastPreflightRequest::slot(&slots),
            Slot::Unsupported {
                reason: UnsupportedReason::NotImplemented
            }
        );
        assert_eq!(SessionSnapshotRequest::slot(&slots), Slot::Translate);
    }

    #[tokio::test]
    async fn play_on_device_reports_now_playing_then_the_completed_track() {
        let root = TempDir::new().unwrap();
        let (binding, server) =
            mock_subsonic_mode(&root, false, SubsonicOpenMode::PlayOnThisDevice).await;
        binding
            .report_track_started_at("song-42", 1_700_000_123_456)
            .await;
        binding.report_track_played("song-42").await;

        let calls = server.state.scrobble_calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["id"], "song-42");
        assert_eq!(calls[0]["submission"], "false");
        assert!(!calls[0].contains_key("time"));
        assert_eq!(calls[1]["id"], "song-42");
        assert_eq!(calls[1]["time"], "1700000123456");
        assert_eq!(calls[1]["submission"], "true");
    }

    #[tokio::test]
    async fn jukebox_binding_does_not_report_local_scrobbles() {
        let root = TempDir::new().unwrap();
        let (binding, server) =
            mock_subsonic_mode(&root, true, SubsonicOpenMode::RemoteControl).await;
        binding
            .report_track_started_at("song-42", 1_700_000_123_456)
            .await;
        binding.report_track_played("song-42").await;

        assert!(server.state.scrobble_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn rune_library_playback_facts_do_not_report_scrobbles() {
        let root = TempDir::new().unwrap();
        let (_, server) =
            mock_subsonic_mode(&root, false, SubsonicOpenMode::PlayOnThisDevice).await;
        let implementation = Implementation::Local;
        let item = PlayingItem::LibraryTrack {
            library_id: Uuid::new_v4(),
            track: "rune-track".into(),
        };

        implementation.track_started(&item, 1_700_000_123_456).await;
        implementation.track_played_through(&item).await;

        assert!(server.state.scrobble_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn skipped_track_keeps_only_the_now_playing_notification() {
        let root = TempDir::new().unwrap();
        let (binding, server) =
            mock_subsonic_mode(&root, false, SubsonicOpenMode::PlayOnThisDevice).await;
        binding
            .report_track_started_at("song-42", 1_700_000_123_456)
            .await;

        let calls = server.state.scrobble_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "song-42");
        assert_eq!(calls[0]["submission"], "false");
    }

    #[tokio::test]
    async fn unreachable_scrobble_server_does_not_propagate_into_playback() {
        let root = TempDir::new().unwrap();
        let (binding, server) =
            mock_subsonic_mode(&root, false, SubsonicOpenMode::PlayOnThisDevice).await;
        server._task.abort();

        tokio::time::timeout(
            Duration::from_secs(1),
            binding.report_track_started_at("song-42", 1_700_000_123_456),
        )
        .await
        .expect("scrobble failure blocked the playback caller");
    }

    #[tokio::test]
    async fn rejected_scrobble_credentials_emit_the_existing_relogin_failure() {
        let root = TempDir::new().unwrap();
        let (binding, server) =
            mock_subsonic_mode(&root, false, SubsonicOpenMode::PlayOnThisDevice).await;
        server.state.reject_scrobble.store(1, Ordering::SeqCst);
        binding
            .report_track_started_at("song-42", 1_700_000_123_456)
            .await;

        assert_eq!(server.broadcasts.load(Ordering::SeqCst), 1);
    }

    fn subsonic_load(
        library_id: Uuid,
        track_id: &str,
        mode: PlaylistOperateMode,
    ) -> LoadResolvedItemsRequest {
        LoadResolvedItemsRequest {
            items: vec![PlayingItemRef::LibraryTrack {
                library_id: library_id.to_string(),
                track: track_id.to_owned(),
            }],
            start_index: Some(0),
            mode,
            expected_revision: None,
            playback_mode: None,
            position_seconds: None,
            play: None,
            call_id: Some(17),
        }
    }

    #[tokio::test]
    async fn translated_player_calls_issue_jukebox_actions_and_change_status() {
        let root = TempDir::new().unwrap();
        let (binding, server) = mock_subsonic(&root, true).await;
        let library_id = binding.library_id;
        RequestTranslator::translate_play_request(&binding, &PlayRequest { call_id: None })
            .await
            .unwrap()
            .unwrap();
        assert!(binding.client.jukebox_status().await.unwrap().playing);
        RequestTranslator::translate_pause_request(&binding, &PauseRequest { call_id: None })
            .await
            .unwrap()
            .unwrap();
        assert!(!binding.client.jukebox_status().await.unwrap().playing);

        RequestTranslator::translate_set_playback_mode_request(
            &binding,
            &SetPlaybackModeRequest {
                mode: 3,
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        let unsupported_mode = RequestTranslator::translate_set_playback_mode_request(
            &binding,
            &SetPlaybackModeRequest {
                mode: 1,
                call_id: None,
            },
        )
        .await;
        assert!(matches!(
            unsupported_mode
                .err()
                .and_then(|error| error.downcast::<ExpectedRequestError>().ok()),
            Some(ExpectedRequestError::Unsupported { .. })
        ));

        RequestTranslator::translate_switch_request(
            &binding,
            &SwitchRequest {
                index: 1,
                expected_revision: None,
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        RequestTranslator::translate_seek_request(
            &binding,
            &SeekRequest {
                position_seconds: 43.0,
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        let status = binding.client.jukebox_status().await.unwrap();
        assert_eq!(status.current_index, Some(1));
        assert_eq!(status.position_secs, Some(43));

        let volume = RequestTranslator::translate_volume_request(
            &binding,
            &VolumeRequest {
                volume: 0.42,
                call_id: Some(8),
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(volume.call_id, Some(8));
        assert_eq!(binding.client.jukebox_status().await.unwrap().gain, 0.42);

        RequestTranslator::translate_load_resolved_items_request(
            &binding,
            &subsonic_load(library_id, "track-c", PlaylistOperateMode::Replace),
        )
        .await
        .unwrap()
        .unwrap();
        let queue = binding.client.jukebox_get().await.unwrap();
        assert_eq!(queue.playlist.songs.len(), 1);
        assert_eq!(queue.playlist.songs[0].id, "track-c");
        RequestTranslator::translate_load_resolved_items_request(
            &binding,
            &subsonic_load(library_id, "track-d", PlaylistOperateMode::AppendToEnd),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            binding
                .client
                .jukebox_get()
                .await
                .unwrap()
                .playlist
                .songs
                .len(),
            2
        );
        RequestTranslator::translate_load_resolved_items_request(
            &binding,
            &subsonic_load(library_id, "track-e", PlaylistOperateMode::PlayNext),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            binding
                .client
                .jukebox_get()
                .await
                .unwrap()
                .playlist
                .songs
                .len(),
            3
        );

        RequestTranslator::translate_load_request(
            &binding,
            &LoadRequest {
                index: 0,
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        RequestTranslator::translate_next_request(&binding, &NextRequest { call_id: None })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            binding.client.jukebox_status().await.unwrap().current_index,
            Some(1)
        );
        RequestTranslator::translate_previous_request(&binding, &PreviousRequest { call_id: None })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            binding.client.jukebox_status().await.unwrap().current_index,
            Some(0)
        );
        RequestTranslator::translate_remove_request(
            &binding,
            &RemoveRequest {
                index: 1,
                expected_revision: None,
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            binding
                .client
                .jukebox_get()
                .await
                .unwrap()
                .playlist
                .songs
                .len(),
            2
        );

        {
            let calls = server.state.jukebox_calls.lock().unwrap();
            assert!(
                calls
                    .iter()
                    .any(|call| call.get("action").is_some_and(|action| action == "set"))
            );
            assert!(
                calls
                    .iter()
                    .any(|call| call.get("action").is_some_and(|action| action == "add"))
            );
            assert!(
                calls
                    .iter()
                    .any(|call| call.get("action").is_some_and(|action| action == "setGain"))
            );
            assert!(
                calls
                    .iter()
                    .any(|call| call.get("action").is_some_and(|action| action == "remove"))
            );
            assert!(calls.iter().any(|call| {
                call.get("action").is_some_and(|action| action == "skip")
                    && call.get("index").is_some_and(|index| index == "1")
                    && call.get("offset").is_some_and(|offset| offset == "43")
            }));
            assert!(
                calls
                    .iter()
                    .any(|call| call.get("action").is_some_and(|action| action == "shuffle"))
            );
            assert!(
                calls
                    .iter()
                    .any(|call| call.get("action").is_some_and(|action| action == "start"))
            );
            assert!(
                calls
                    .iter()
                    .any(|call| call.get("action").is_some_and(|action| action == "stop"))
            );
        }
        let events_before_refresh = server.broadcasts.load(Ordering::SeqCst);
        RequestTranslator::translate_refresh_session_request(
            &binding,
            &RefreshSessionRequest { call_id: None },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            server.broadcasts.load(Ordering::SeqCst),
            events_before_refresh + 2
        );
    }

    #[tokio::test]
    async fn non_subsonic_load_items_are_rejected_before_http() {
        let root = TempDir::new().unwrap();
        let (binding, server) = mock_subsonic(&root, true).await;
        let mut request = subsonic_load(binding.library_id, "unused", PlaylistOperateMode::Replace);
        request.items[0] = PlayingItemRef::from(playback::player::LibraryItem {
            library_id: Uuid::new_v4(),
            file_uuid: Uuid::new_v4(),
        });
        let error = match RequestTranslator::translate_load_resolved_items_request(
            &binding, &request,
        )
        .await
        {
            Ok(_) => panic!("non-Subsonic items must be rejected"),
            Err(error) => error,
        };
        assert!(matches!(
            error.downcast_ref::<ExpectedRequestError>(),
            Some(ExpectedRequestError::InvalidInput { .. })
        ));
        assert!(server.state.jukebox_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn no_role_keeps_player_calls_unsupported_and_makes_no_jukebox_request() {
        let root = TempDir::new().unwrap();
        let (binding, server) = mock_subsonic(&root, false).await;
        let slots = RequestSlots::subsonic(&binding.probe);
        for slot in [
            PlayRequest::slot(&slots),
            PauseRequest::slot(&slots),
            LoadRequest::slot(&slots),
            NextRequest::slot(&slots),
            PreviousRequest::slot(&slots),
            SwitchRequest::slot(&slots),
            SeekRequest::slot(&slots),
            VolumeRequest::slot(&slots),
            RemoveRequest::slot(&slots),
            LoadResolvedItemsRequest::slot(&slots),
            SetPlaybackModeRequest::slot(&slots),
            MovePlaylistItemRequest::slot(&slots),
            SetRealtimeFFTEnabledRequest::slot(&slots),
            SetAdaptiveSwitchingEnabledRequest::slot(&slots),
            CastPreflightRequest::slot(&slots),
            RefreshSessionRequest::slot(&slots),
            SessionSnapshotRequest::slot(&slots),
        ] {
            assert_eq!(
                slot,
                Slot::Unsupported {
                    reason: UnsupportedReason::AccountRoleMissing
                }
            );
        }
        assert!(server.state.jukebox_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn jukebox_poll_broadcasts_changes_once_and_stops_when_closed() {
        let root = TempDir::new().unwrap();
        let (binding, server) = mock_subsonic(&root, true).await;
        let binding = Arc::new(binding);
        binding.start_jukebox_polling();
        tokio::time::timeout(Duration::from_secs(4), async {
            while server.broadcasts.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(3100)).await;
        assert_eq!(server.broadcasts.load(Ordering::SeqCst), 2);
        server.state.jukebox.lock().unwrap().playing = true;
        tokio::time::timeout(Duration::from_secs(4), async {
            while server.broadcasts.load(Ordering::SeqCst) < 4 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        binding.stop_jukebox_polling();
        let stopped_count = server.broadcasts.load(Ordering::SeqCst);
        server
            .state
            .jukebox
            .lock()
            .unwrap()
            .entries
            .push(mock_song("after-close"));
        tokio::time::sleep(Duration::from_millis(3200)).await;
        assert_eq!(server.broadcasts.load(Ordering::SeqCst), stopped_count);
    }

    #[tokio::test]
    async fn browsed_collection_returns_cached_local_path_after_background_fetch() {
        let root = TempDir::new().unwrap();
        let (binding, server) = mock_subsonic(&root, false).await;
        fetch_collection_group_summary_request(
            &binding,
            &FetchCollectionGroupSummaryRequest {
                call_id: None,
                collection_type: CollectionType::Artist,
            },
        )
        .await
        .unwrap();
        let request = FetchCollectionGroupsRequest {
            call_id: None,
            collection_type: CollectionType::Artist,
            bake_cover_arts: true,
            group_titles: vec!["A".into()],
        };
        let immediate = fetch_collection_groups_request(&binding, &request)
            .await
            .unwrap()
            .unwrap();
        assert!(
            immediate.groups[0]
                .collections
                .iter()
                .all(|collection| collection.cover_art_map.is_empty())
        );
        wait_for_covers(&binding, &["cover-1", "cover-2"]).await;
        let refreshed = fetch_collection_groups_request(&binding, &request)
            .await
            .unwrap()
            .unwrap();
        assert!(refreshed.groups[0].collections.iter().all(|collection| {
            collection.cover_art_map.len() == 1
                && collection
                    .cover_art_map
                    .values()
                    .all(|path| path.starts_with(root.path().to_str().unwrap()))
        }));
        assert_eq!(server.state.cover_calls.load(Ordering::SeqCst), 2);
    }

    fn descriptor(unsupported: &[(&str, Option<&str>)]) -> subsonic::RuneServiceDescriptor {
        subsonic::RuneServiceDescriptor {
            id: "appleMusic".into(),
            name: "Apple Music".into(),
            unsupported: unsupported
                .iter()
                .map(|(endpoint, param)| subsonic::RuneUnsupported {
                    endpoint: (*endpoint).into(),
                    param: param.map(Into::into),
                })
                .collect(),
            ..Default::default()
        }
    }

    fn rune_slots(unsupported: &[(&str, Option<&str>)], has_song_lyrics: bool) -> RequestSlots {
        rune_slots_with(unsupported, has_song_lyrics, &[])
    }

    pub(super) fn rune_slots_with(
        unsupported: &[(&str, Option<&str>)],
        has_song_lyrics: bool,
        extensions: &[&str],
    ) -> RequestSlots {
        let probe = SubsonicProbe {
            api_version: "1.16.1".into(),
            open_subsonic: true,
            server_type: None,
            server_version: None,
            extensions: std::iter::once("runeService")
                .chain(extensions.iter().copied())
                .map(|name| (name.into(), vec![1]))
                .collect(),
            roles: SubsonicRoles {
                jukebox_role: true,
                ..Default::default()
            },
            roles_known: true,
        };
        let mut slots = RequestSlots::subsonic(&probe);
        slots.hide_rune_unsupported(&descriptor(unsupported), &probe, has_song_lyrics);
        slots
    }

    fn hidden() -> Slot {
        Slot::Unsupported {
            reason: UnsupportedReason::NotImplemented,
        }
    }

    #[test]
    fn descriptor_unsupported_entries_hide_the_requests_behind_them() {
        let slots = rune_slots(
            &[
                ("deletePlaylist", None),
                ("updatePlaylist", Some("songIndexToRemove")),
                ("star", None),
                ("search3", None),
            ],
            false,
        );
        for slot in [
            RemovePlaylistRequest::slot(&slots),
            RemoveItemFromPlaylistRequest::slot(&slots),
            SetLikedRequest::slot(&slots),
            SearchForRequest::slot(&slots),
            SearchMediaFileSummaryRequest::slot(&slots),
            SearchCollectionSummaryRequest::slot(&slots),
            FetchMediaFilesRequest::slot(&slots),
        ] {
            assert_eq!(slot, hidden());
        }
        // A parameter entry hides only the requests that use that parameter.
        for slot in [
            UpdatePlaylistRequest::slot(&slots),
            AddItemToPlaylistRequest::slot(&slots),
            CreatePlaylistRequest::slot(&slots),
            ReorderPlaylistItemPositionRequest::slot(&slots),
            FetchAllPlaylistsRequest::slot(&slots),
            GetLyricByTrackIdRequest::slot(&slots),
            VolumeRequest::slot(&slots),
        ] {
            assert_ne!(slot, hidden());
        }
        assert_eq!(
            crate::messages::RequestCapability::from(RemovePlaylistRequest::slot(&slots)),
            crate::messages::RequestCapability::Hidden
        );
    }

    #[test]
    fn create_and_reorder_follow_the_create_playlist_endpoint() {
        let slots = rune_slots(&[("createPlaylist", Some("playlistId"))], false);
        assert_eq!(ReorderPlaylistItemPositionRequest::slot(&slots), hidden());
        assert_eq!(CreatePlaylistRequest::slot(&slots), Slot::Translate);
        let slots = rune_slots(&[("createPlaylist", None)], false);
        assert_eq!(CreatePlaylistRequest::slot(&slots), hidden());
        assert_eq!(ReorderPlaylistItemPositionRequest::slot(&slots), hidden());
        assert_eq!(UpdatePlaylistRequest::slot(&slots), Slot::Translate);
        let slots = rune_slots(&[("updatePlaylist", None)], false);
        for slot in [
            UpdatePlaylistRequest::slot(&slots),
            AddItemToPlaylistRequest::slot(&slots),
            RemoveItemFromPlaylistRequest::slot(&slots),
        ] {
            assert_eq!(slot, hidden());
        }
        assert_eq!(
            ReorderPlaylistItemPositionRequest::slot(&slots),
            Slot::Translate
        );
    }

    #[test]
    fn lyrics_hide_only_when_the_endpoint_the_translator_calls_is_unsupported() {
        let get_lyrics = [("getLyrics", None)];
        let by_song_id = [("getLyricsBySongId", None)];
        let both = [("getLyrics", None), ("getLyricsBySongId", None)];
        for (unsupported, has_song_lyrics, expected) in [
            (&get_lyrics[..], false, hidden()),
            (&get_lyrics[..], true, Slot::Translate),
            (&by_song_id[..], false, Slot::Translate),
            (&by_song_id[..], true, hidden()),
            (&both[..], false, hidden()),
            (&both[..], true, hidden()),
        ] {
            let slots = rune_slots(unsupported, has_song_lyrics);
            assert_eq!(GetLyricByTrackIdRequest::slot(&slots), expected);
        }
    }

    #[test]
    fn unsupported_jukebox_control_hides_the_player_requests() {
        let slots = rune_slots(&[("jukeboxControl", None)], false);
        for slot in [
            PlayRequest::slot(&slots),
            LoadRequest::slot(&slots),
            VolumeRequest::slot(&slots),
            SessionSnapshotRequest::slot(&slots),
        ] {
            assert_eq!(slot, hidden());
        }
        let slots = rune_slots(&[], false);
        assert_eq!(PlayRequest::slot(&slots), Slot::Translate);
    }

    #[test]
    fn binding_without_a_descriptor_keeps_the_plain_slots() {
        let root = TempDir::new().unwrap();
        let (binding, _) = hosted_binding(&root, None, true);
        let plain = RequestSlots::subsonic(&binding.probe);
        assert_eq!(
            RemovePlaylistRequest::slot(&binding.slots()),
            RemovePlaylistRequest::slot(&plain)
        );
        let binding = Arc::try_unwrap(binding)
            .ok()
            .unwrap()
            .with_rune_service(Some(descriptor(&[("deletePlaylist", None)])));
        assert_eq!(RemovePlaylistRequest::slot(&binding.slots()), hidden());
        assert_eq!(
            FetchAllPlaylistsRequest::slot(&binding.slots()),
            Slot::Translate
        );
    }

    struct EditCatalog {
        edit: Arc<Mutex<Option<subsonic::RuneEdit>>>,
        writes: Arc<Mutex<Vec<&'static str>>>,
    }

    impl subsonic::api::Playlists for EditCatalog {
        fn playlists<'a>(&'a self) -> subsonic::ApiFuture<'a, Vec<subsonic::SubsonicPlaylist>> {
            let edit = *self.edit.lock().unwrap();
            Box::pin(async move {
                Ok(vec![subsonic::SubsonicPlaylist {
                    id: "p1".into(),
                    name: "P".into(),
                    rune_edit: edit,
                    ..Default::default()
                }])
            })
        }

        fn playlist<'a>(
            &'a self,
            id: &'a str,
        ) -> subsonic::ApiFuture<'a, subsonic::SubsonicPlaylistDetail> {
            let edit = *self.edit.lock().unwrap();
            Box::pin(async move {
                Ok(subsonic::SubsonicPlaylistDetail {
                    id: id.into(),
                    name: "P".into(),
                    rune_edit: edit,
                    songs: vec![
                        subsonic::SubsonicSong {
                            id: "s1".into(),
                            ..Default::default()
                        },
                        subsonic::SubsonicSong {
                            id: "s2".into(),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                })
            })
        }

        fn replace_playlist_songs<'a>(
            &'a self,
            _: &'a str,
            _: &'a [&'a str],
        ) -> subsonic::ApiFuture<'a, ()> {
            self.writes.lock().unwrap().push("replace");
            Box::pin(async { Ok(()) })
        }

        fn update_playlist<'a>(
            &'a self,
            _: &'a str,
            _: Option<&'a str>,
            _: &'a [&'a str],
            _: &'a [u32],
        ) -> subsonic::ApiFuture<'a, ()> {
            self.writes.lock().unwrap().push("update");
            Box::pin(async { Ok(()) })
        }

        fn delete_playlist<'a>(&'a self, _: &'a str) -> subsonic::ApiFuture<'a, ()> {
            self.writes.lock().unwrap().push("delete");
            Box::pin(async { Ok(()) })
        }
    }

    impl subsonic::api::System for EditCatalog {}
    impl subsonic::api::Browsing for EditCatalog {}
    impl subsonic::api::Lists for EditCatalog {}
    impl subsonic::api::Searching for EditCatalog {}
    impl subsonic::api::Annotation for EditCatalog {}
    impl subsonic::api::MediaRetrieval for EditCatalog {}
    impl subsonic::api::Jukebox for EditCatalog {}
    impl subsonic::api::Scanning for EditCatalog {}
    impl subsonic::api::RuneVisualizerFeed for EditCatalog {}
    impl subsonic::api::RuneCatalog for EditCatalog {}
    impl subsonic::api::RuneRecommendations for EditCatalog {}
    impl subsonic::api::RuneService for EditCatalog {}

    fn edit_binding(
        root: &TempDir,
        edit: Option<subsonic::RuneEdit>,
    ) -> (SubsonicBinding, Arc<Mutex<Vec<&'static str>>>) {
        let (binding, writes, _) = edit_binding_with_handle(root, edit);
        (binding, writes)
    }

    #[allow(clippy::type_complexity)]
    fn edit_binding_with_handle(
        root: &TempDir,
        edit: Option<subsonic::RuneEdit>,
    ) -> (
        SubsonicBinding,
        Arc<Mutex<Vec<&'static str>>>,
        Arc<Mutex<Option<subsonic::RuneEdit>>>,
    ) {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let edit = Arc::new(Mutex::new(edit));
        let binding = SubsonicBinding::new(
            Arc::new(EditCatalog {
                edit: edit.clone(),
                writes: writes.clone(),
            }),
            SubsonicProbe {
                api_version: "1.16.1".into(),
                open_subsonic: true,
                server_type: None,
                server_version: None,
                extensions: Vec::new(),
                roles: SubsonicRoles::default(),
                roles_known: true,
            },
            root.path().to_path_buf(),
            Uuid::new_v4(),
            true,
            Arc::new(CountingBroadcaster(Arc::new(AtomicUsize::new(0)))),
        );
        (binding, writes, edit)
    }

    fn assert_refused(result: anyhow::Result<impl Sized>) {
        let error = result.err().expect("the edit must be refused");
        assert!(matches!(
            error.downcast_ref::<ExpectedRequestError>(),
            Some(ExpectedRequestError::Unsupported {
                reason: UnsupportedReason::NotImplemented,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn read_only_playlists_refuse_edits_before_calling_the_server() {
        let root = TempDir::new().unwrap();
        let (binding, writes) = edit_binding(&root, Some(subsonic::RuneEdit::ReadOnly));
        let playlist_id =
            fetch_all_playlists_request(&binding, &FetchAllPlaylistsRequest { call_id: None })
                .await
                .unwrap()
                .unwrap()
                .playlists[0]
                .id;
        assert_refused(
            update_playlist_request(
                &binding,
                &UpdatePlaylistRequest {
                    playlist_id,
                    name: "New".into(),
                    group: String::new(),
                    call_id: None,
                },
            )
            .await,
        );
        assert_refused(
            remove_playlist_request(
                &binding,
                &RemovePlaylistRequest {
                    playlist_id,
                    call_id: None,
                },
            )
            .await,
        );
        let track = binding
            .catalog
            .lock()
            .unwrap()
            .ids
            .intern(SubsonicIdKind::Track, "s9")
            .unwrap();
        assert_refused(
            add_item_to_playlist_request(
                &binding,
                &AddItemToPlaylistRequest {
                    playlist_id,
                    media_file_id: track,
                    position: None,
                    call_id: None,
                },
            )
            .await,
        );
        assert!(writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn append_only_playlists_allow_adding_but_not_removing_or_reordering() {
        let root = TempDir::new().unwrap();
        let (binding, writes) = edit_binding(&root, Some(subsonic::RuneEdit::AppendOnly));
        let playlist_id =
            fetch_all_playlists_request(&binding, &FetchAllPlaylistsRequest { call_id: None })
                .await
                .unwrap()
                .unwrap()
                .playlists[0]
                .id;
        let track = binding
            .catalog
            .lock()
            .unwrap()
            .ids
            .intern(SubsonicIdKind::Track, "s9")
            .unwrap();
        add_item_to_playlist_request(
            &binding,
            &AddItemToPlaylistRequest {
                playlist_id,
                media_file_id: track,
                position: None,
                call_id: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(writes.lock().unwrap().as_slice(), ["update"]);
        assert_refused(
            remove_item_from_playlist_request(
                &binding,
                &RemoveItemFromPlaylistRequest {
                    playlist_id,
                    occurrence_id: "0".into(),
                    call_id: None,
                },
            )
            .await,
        );
        assert_refused(
            reorder_playlist_item_position_request(
                &binding,
                &ReorderPlaylistItemPositionRequest {
                    playlist_id,
                    occurrence_id: "0".into(),
                    new_position: 1,
                    call_id: None,
                },
            )
            .await,
        );
        assert_eq!(writes.lock().unwrap().as_slice(), ["update"]);
    }

    #[tokio::test]
    async fn playlists_without_rune_edit_are_left_to_the_server() {
        let root = TempDir::new().unwrap();
        let (binding, writes) = edit_binding(&root, None);
        let playlist_id =
            fetch_all_playlists_request(&binding, &FetchAllPlaylistsRequest { call_id: None })
                .await
                .unwrap()
                .unwrap()
                .playlists[0]
                .id;
        update_playlist_request(
            &binding,
            &UpdatePlaylistRequest {
                playlist_id,
                name: "New".into(),
                group: String::new(),
                call_id: None,
            },
        )
        .await
        .unwrap();
        remove_playlist_request(
            &binding,
            &RemovePlaylistRequest {
                playlist_id,
                call_id: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(writes.lock().unwrap().as_slice(), ["update", "delete"]);
    }

    #[tokio::test]
    async fn every_edit_mode_allows_exactly_its_edits() {
        use subsonic::RuneEdit;
        let append_only = [true, false, false, false, false];
        let full = [true; 5];
        let read_only = [false; 5];
        // Add, rename, delete, remove an item, reorder.
        for (edit, expected) in [
            (Some(RuneEdit::Full), full),
            (None, full),
            (Some(RuneEdit::Unknown), full),
            (Some(RuneEdit::AppendOnly), append_only),
            (Some(RuneEdit::ReadOnly), read_only),
        ] {
            let root = TempDir::new().unwrap();
            let (binding, writes) = edit_binding(&root, edit);
            let playlist_id =
                fetch_all_playlists_request(&binding, &FetchAllPlaylistsRequest { call_id: None })
                    .await
                    .unwrap()
                    .unwrap()
                    .playlists[0]
                    .id;
            let track = binding
                .catalog
                .lock()
                .unwrap()
                .ids
                .intern(SubsonicIdKind::Track, "s9")
                .unwrap();
            let results = [
                add_item_to_playlist_request(
                    &binding,
                    &AddItemToPlaylistRequest {
                        playlist_id,
                        media_file_id: track,
                        position: None,
                        call_id: None,
                    },
                )
                .await
                .is_ok(),
                update_playlist_request(
                    &binding,
                    &UpdatePlaylistRequest {
                        playlist_id,
                        name: "New".into(),
                        group: String::new(),
                        call_id: None,
                    },
                )
                .await
                .is_ok(),
                remove_playlist_request(
                    &binding,
                    &RemovePlaylistRequest {
                        playlist_id,
                        call_id: None,
                    },
                )
                .await
                .is_ok(),
                remove_item_from_playlist_request(
                    &binding,
                    &RemoveItemFromPlaylistRequest {
                        playlist_id,
                        occurrence_id: "0".into(),
                        call_id: None,
                    },
                )
                .await
                .is_ok(),
                reorder_playlist_item_position_request(
                    &binding,
                    &ReorderPlaylistItemPositionRequest {
                        playlist_id,
                        occurrence_id: "0".into(),
                        new_position: 1,
                        call_id: None,
                    },
                )
                .await
                .is_ok(),
            ];
            assert_eq!(results, expected, "mode {edit:?}");
            assert_eq!(
                writes.lock().unwrap().len(),
                expected.iter().filter(|allowed| **allowed).count(),
                "mode {edit:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_stale_read_only_memory_never_refuses_rename_or_delete() {
        let root = TempDir::new().unwrap();
        let (binding, writes, handle) =
            edit_binding_with_handle(&root, Some(subsonic::RuneEdit::ReadOnly));
        let playlist_id =
            fetch_all_playlists_request(&binding, &FetchAllPlaylistsRequest { call_id: None })
                .await
                .unwrap()
                .unwrap()
                .playlists[0]
                .id;
        *handle.lock().unwrap() = Some(subsonic::RuneEdit::Full);
        update_playlist_request(
            &binding,
            &UpdatePlaylistRequest {
                playlist_id,
                name: "New".into(),
                group: String::new(),
                call_id: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(writes.lock().unwrap().as_slice(), ["update"]);
        // The fresh read replaced the memory, so a delete goes through as well.
        remove_playlist_request(
            &binding,
            &RemovePlaylistRequest {
                playlist_id,
                call_id: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(writes.lock().unwrap().as_slice(), ["update", "delete"]);
    }

    #[test]
    fn parameter_entries_count_only_for_parameters_rune_sends() {
        let slots = rune_slots(&[("star", Some("size"))], false);
        assert_ne!(SetLikedRequest::slot(&slots), hidden());
        let slots = rune_slots(&[("unstar", Some("id"))], false);
        assert_eq!(SetLikedRequest::slot(&slots), hidden());
        let slots = rune_slots(&[("search3", Some("songOffset"))], false);
        assert_eq!(FetchMediaFilesRequest::slot(&slots), hidden());
        assert_eq!(SearchForRequest::slot(&slots), Slot::Translate);
        assert_eq!(SearchMediaFileSummaryRequest::slot(&slots), Slot::Translate);
    }

    #[test]
    fn song_and_collection_reads_hide_the_requests_that_call_them() {
        let slots = rune_slots(&[("getSong", None)], false);
        for slot in [
            GetLikedRequest::slot(&slots),
            FetchMediaFileByIdsRequest::slot(&slots),
            FetchParsedMediaFileRequest::slot(&slots),
            GetLyricByTrackIdRequest::slot(&slots),
        ] {
            assert_eq!(slot, hidden());
        }
        let slots = rune_slots(&[("getAlbum", None)], false);
        assert_eq!(MixQueryRequest::slot(&slots), hidden());
        assert_eq!(
            crate::messages::FetchCollectionByIdsRequest::slot(&slots),
            Slot::Translate
        );
        let slots = rune_slots(&[("getAlbum", None), ("getArtist", None)], false);
        assert_eq!(
            crate::messages::FetchCollectionByIdsRequest::slot(&slots),
            hidden()
        );
    }

    #[tokio::test]
    async fn collection_groups_hide_only_when_no_kind_can_be_listed() {
        let one = rune_slots(&[("getAlbumList2", None)], false);
        assert_eq!(
            FetchCollectionGroupSummaryRequest::slot(&one),
            Slot::Translate
        );
        assert_eq!(FetchCollectionGroupsRequest::slot(&one), Slot::Translate);
        let both = rune_slots(&[("getAlbumList2", None), ("getArtists", None)], false);
        assert_eq!(FetchCollectionGroupSummaryRequest::slot(&both), hidden());
        assert_eq!(FetchCollectionGroupsRequest::slot(&both), hidden());

        // With one endpoint missing, the kind that needs it is refused at run time.
        let root = TempDir::new().unwrap();
        let binding = SubsonicBinding::new(
            Arc::new(InMemoryCatalog),
            SubsonicProbe {
                api_version: "1.16.1".into(),
                open_subsonic: true,
                server_type: None,
                server_version: None,
                extensions: Vec::new(),
                roles: SubsonicRoles::default(),
                roles_known: true,
            },
            root.path().to_path_buf(),
            Uuid::new_v4(),
            false,
            Arc::new(CountingBroadcaster(Arc::new(AtomicUsize::new(0)))),
        )
        .with_rune_service(Some(descriptor(&[("getArtists", None)])));
        let error = fetch_collection_group_summary_request(
            &binding,
            &FetchCollectionGroupSummaryRequest {
                collection_type: CollectionType::Artist,
                call_id: None,
            },
        )
        .await
        .err()
        .expect("artists are refused");
        assert!(matches!(
            error.downcast_ref::<ExpectedRequestError>(),
            Some(ExpectedRequestError::Unsupported {
                request: "getArtists",
                ..
            })
        ));
    }

    #[test]
    fn rune_reasons_map_onto_existing_request_errors() {
        use subsonic::{RuneReason, SubsonicServerError};
        let error = |kind, rune_reason| {
            subsonic_request_error(SubsonicError::Server {
                kind,
                message: "no".into(),
                rune_reason: Some(rune_reason),
            })
        };
        assert!(matches!(
            error(SubsonicServerError::Generic, RuneReason::CredentialsExpired)
                .downcast_ref::<ExpectedRequestError>(),
            Some(ExpectedRequestError::SubsonicNotAuthorized)
        ));
        assert!(matches!(
            error(SubsonicServerError::Generic, RuneReason::Removed)
                .downcast_ref::<database::error::ExpectedError>(),
            Some(database::error::ExpectedError::NotFound { .. })
        ));
        for reason in [
            RuneReason::NotSubscribed,
            RuneReason::Region,
            RuneReason::ServiceLimit,
            RuneReason::Other,
        ] {
            assert!(matches!(
                error(SubsonicServerError::Generic, reason).downcast_ref::<ExpectedRequestError>(),
                Some(ExpectedRequestError::UnexpectedResponse { .. })
            ));
        }
    }

    struct DescribedCatalog(bool);

    impl subsonic::api::RuneService for DescribedCatalog {
        fn rune_service<'a>(&'a self) -> subsonic::ApiFuture<'a, subsonic::RuneServiceDescriptor> {
            let works = self.0;
            Box::pin(async move {
                if works {
                    Ok(descriptor(&[("getLyrics", None)]))
                } else {
                    Err(SubsonicError::Transport("offline".into()))
                }
            })
        }
    }

    impl subsonic::api::System for DescribedCatalog {}
    impl subsonic::api::Browsing for DescribedCatalog {}
    impl subsonic::api::Lists for DescribedCatalog {}
    impl subsonic::api::Searching for DescribedCatalog {}
    impl subsonic::api::Playlists for DescribedCatalog {}
    impl subsonic::api::Annotation for DescribedCatalog {}
    impl subsonic::api::MediaRetrieval for DescribedCatalog {}
    impl subsonic::api::Jukebox for DescribedCatalog {}
    impl subsonic::api::Scanning for DescribedCatalog {}
    impl subsonic::api::RuneVisualizerFeed for DescribedCatalog {}
    impl subsonic::api::RuneCatalog for DescribedCatalog {}
    impl subsonic::api::RuneRecommendations for DescribedCatalog {}

    #[tokio::test]
    async fn descriptor_is_read_only_from_servers_that_declare_the_extension() {
        let probe = |declares: bool| SubsonicProbe {
            api_version: "1.16.1".into(),
            open_subsonic: true,
            server_type: None,
            server_version: None,
            extensions: if declares {
                vec![("runeService".into(), vec![1])]
            } else {
                Vec::new()
            },
            roles: SubsonicRoles::default(),
            roles_known: true,
        };
        let described = DescribedCatalog(true);
        assert!(
            crate::subsonic_binding::fetch_rune_service(&described, &probe(true))
                .await
                .is_some_and(|descriptor| descriptor.is_unsupported("getLyrics", &[]))
        );
        assert!(
            crate::subsonic_binding::fetch_rune_service(&described, &probe(false))
                .await
                .is_none()
        );
        // A failed read opens the library without a descriptor.
        assert!(
            crate::subsonic_binding::fetch_rune_service(&DescribedCatalog(false), &probe(true))
                .await
                .is_none()
        );
    }

    #[test]
    fn rejected_request_credentials_are_typed_as_not_authorized() {
        use subsonic::SubsonicServerError;
        let failures = [
            SubsonicServerError::WrongCredentials,
            SubsonicServerError::NotAuthorized,
            SubsonicServerError::InvalidApiKey,
        ];
        for kind in failures {
            let error = subsonic_request_error(SubsonicError::Server {
                kind,
                message: "rejected".into(),
                rune_reason: None,
            });
            assert!(matches!(
                error.downcast_ref::<ExpectedRequestError>(),
                Some(ExpectedRequestError::SubsonicNotAuthorized)
            ));
            assert_eq!(classify(&error), RequestErrorKind::PermissionDenied);
        }
    }

    #[test]
    fn jukebox_failures_keep_authorization_unavailable_and_disabled_types() {
        use subsonic::SubsonicServerError;

        let unauthorized = jukebox_error(
            SubsonicError::Server {
                kind: SubsonicServerError::WrongCredentials,
                message: "rejected".into(),
                rune_reason: None,
            },
            PlayRequest::wire_name(),
        );
        assert!(matches!(
            unauthorized.downcast_ref::<ExpectedRequestError>(),
            Some(ExpectedRequestError::SubsonicNotAuthorized)
        ));
        let unavailable = jukebox_error(
            SubsonicError::Transport("offline".into()),
            PlayRequest::wire_name(),
        );
        assert!(matches!(
            unavailable.downcast_ref::<ExpectedRequestError>(),
            Some(ExpectedRequestError::Unavailable { .. })
        ));
        let disabled = jukebox_error(SubsonicError::JukeboxUnsupported, PlayRequest::wire_name());
        assert!(matches!(
            disabled.downcast_ref::<ExpectedRequestError>(),
            Some(ExpectedRequestError::Unsupported {
                reason: UnsupportedReason::NotImplemented,
                ..
            })
        ));
    }
}

#[cfg(test)]
mod catalogue_tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use subsonic::{
        ApiFuture, RuneRecommendationGroup, RuneRecommendationKind, RuneUnsupported,
        SubsonicAlbumDetail, SubsonicArtistDetail, SubsonicPlaylist, SubsonicPlaylistDetail,
        SubsonicRoles, SubsonicSearchResult,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::messages::RequestErrorKind;
    use crate::request_error::classify;
    use crate::request_owner::Implementation;
    use crate::rpc::TargetRequest;
    use crate::utils::{Broadcaster, RinfRustSignal};

    struct Silent;

    impl Broadcaster for Silent {
        fn broadcast(&self, _: &dyn RinfRustSignal) {}
    }

    fn song(id: &str, album_id: Option<&str>, in_library: Option<bool>) -> SubsonicSong {
        SubsonicSong {
            id: id.into(),
            title: format!("Title of {id}"),
            album_id: album_id.map(Into::into),
            rune_in_library: in_library,
            ..SubsonicSong::default()
        }
    }

    fn album(id: &str, name: &str, in_library: Option<bool>) -> SubsonicAlbum {
        SubsonicAlbum {
            id: id.into(),
            name: name.into(),
            cover_art: Some(format!("cover-{id}")),
            rune_in_library: in_library,
            ..SubsonicAlbum::default()
        }
    }

    fn artist(id: &str, in_library: Option<bool>) -> SubsonicArtist {
        SubsonicArtist {
            id: id.into(),
            name: format!("Name of {id}"),
            rune_in_library: in_library,
            ..SubsonicArtist::default()
        }
    }

    /// A service with a library and a catalogue beyond it, recording what it is asked.
    #[derive(Default)]
    struct CatalogueApi {
        calls: Mutex<Vec<String>>,
        recommendation_reads: AtomicUsize,
        refuse_additions: AtomicBool,
        refuse_recommendations: AtomicBool,
    }

    fn refused() -> SubsonicError {
        SubsonicError::Server {
            kind: SubsonicServerError::Generic,
            message: "refused".into(),
            rune_reason: None,
        }
    }

    impl CatalogueApi {
        fn record(&self, call: String) {
            self.calls.lock().unwrap().push(call);
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    fn ready<'a, T: Send + 'a>(value: T) -> ApiFuture<'a, T> {
        Box::pin(async move { Ok(value) })
    }

    impl subsonic::api::Searching for CatalogueApi {
        fn search_with_song_offset<'a>(
            &'a self,
            query: &'a str,
            _limits: SearchLimits,
            _song_offset: u32,
        ) -> ApiFuture<'a, SubsonicSearchResult> {
            self.record(format!("search3:{query}"));
            ready(SubsonicSearchResult {
                artists: vec![artist("artist-lib", None)],
                albums: vec![album("album-lib", "Near", None)],
                songs: vec![song("song-lib", None, None)],
            })
        }
    }

    impl subsonic::api::RuneCatalog for CatalogueApi {
        fn search_in_scope<'a>(
            &'a self,
            query: &'a str,
            _limits: SearchLimits,
            song_offset: u32,
            scope: RuneScope,
        ) -> ApiFuture<'a, SubsonicSearchResult> {
            self.record(format!("search3 {}:{query}:{song_offset}", scope.as_str()));
            ready(SubsonicSearchResult {
                artists: vec![
                    artist("artist-lib", Some(true)),
                    artist("catalog:artist:1", Some(false)),
                ],
                albums: vec![
                    album("album-lib", "Near", Some(true)),
                    album("catalog:album:1", "Far", Some(false)),
                ],
                songs: vec![
                    song("song-lib", None, Some(true)),
                    song("catalog:song:1", None, Some(false)),
                    song("song-unsaid", None, None),
                ],
            })
        }

        fn add_to_library<'a>(&'a self, ids: &'a [&'a str]) -> ApiFuture<'a, ()> {
            self.record(format!("add:{}", ids.join(",")));
            if self.refuse_additions.load(Ordering::SeqCst) {
                return Box::pin(async { Err(refused()) });
            }
            ready(())
        }
    }

    impl subsonic::api::RuneRecommendations for CatalogueApi {
        fn recommendations<'a>(
            &'a self,
            count: u32,
        ) -> ApiFuture<'a, Vec<RuneRecommendationGroup>> {
            self.record(format!("recommendations:{count}"));
            self.recommendation_reads.fetch_add(1, Ordering::SeqCst);
            if self.refuse_recommendations.load(Ordering::SeqCst) {
                return Box::pin(async { Err(refused()) });
            }
            let far_mix = SubsonicPlaylist {
                id: "catalog:playlist:1".into(),
                name: "Far Mix".into(),
                cover_art: Some("cover-mix".into()),
                rune_edit: Some(RuneEdit::ReadOnly),
                rune_in_library: Some(false),
                ..SubsonicPlaylist::default()
            };
            ready(vec![
                RuneRecommendationGroup {
                    id: "g1".into(),
                    title: "Made for You".into(),
                    kind: RuneRecommendationKind::Personal,
                    albums: vec![album("catalog:album:1", "Far", Some(false))],
                    playlists: vec![far_mix],
                },
                RuneRecommendationGroup {
                    id: "g2".into(),
                    title: " ".into(),
                    kind: RuneRecommendationKind::Recent,
                    albums: vec![album("album-lib", "Near", Some(true))],
                    playlists: Vec::new(),
                },
                // Same title as the first: merged into it, without repeating its album.
                RuneRecommendationGroup {
                    id: "g3".into(),
                    title: "Made for You".into(),
                    kind: RuneRecommendationKind::Personal,
                    albums: vec![
                        album("catalog:album:1", "Far", Some(false)),
                        album("catalog:album:2", "Farther", Some(false)),
                    ],
                    playlists: Vec::new(),
                },
                RuneRecommendationGroup {
                    id: "g4".into(),
                    title: "Nothing to Show".into(),
                    ..RuneRecommendationGroup::default()
                },
            ])
        }
    }

    impl subsonic::api::Browsing for CatalogueApi {
        fn artists<'a>(&'a self) -> ApiFuture<'a, Vec<SubsonicArtist>> {
            ready(vec![artist("artist-lib", Some(true))])
        }

        fn artist<'a>(&'a self, id: &'a str) -> ApiFuture<'a, SubsonicArtistDetail> {
            self.record(format!("getArtist:{id}"));
            ready(SubsonicArtistDetail {
                id: id.into(),
                name: "Far Artist".into(),
                rune_in_library: Some(false),
                albums: vec![
                    album("catalog:album:slow", "Slow", Some(false)),
                    // The catalogue copy of the library's album "Near", which the service
                    // knows to be in the library.
                    album("catalog:album:near", "Near", Some(true)),
                    album("catalog:album:last", "Last", Some(false)),
                ],
                ..SubsonicArtistDetail::default()
            })
        }

        fn album<'a>(&'a self, id: &'a str) -> ApiFuture<'a, SubsonicAlbumDetail> {
            self.record(format!("getAlbum:{id}"));
            Box::pin(async move {
                // The first album answers last, so the order must not follow the answers.
                if id == "catalog:album:slow" {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Ok(SubsonicAlbumDetail {
                    id: id.into(),
                    name: "Far".into(),
                    rune_in_library: Some(false),
                    songs: vec![
                        song(&format!("{id}/1"), Some(id), Some(false)),
                        song(&format!("{id}/2"), Some(id), Some(false)),
                    ],
                    ..SubsonicAlbumDetail::default()
                })
            })
        }
    }

    impl subsonic::api::Playlists for CatalogueApi {
        fn playlist<'a>(&'a self, id: &'a str) -> ApiFuture<'a, SubsonicPlaylistDetail> {
            self.record(format!("getPlaylist:{id}"));
            ready(SubsonicPlaylistDetail {
                id: id.into(),
                name: "Far Mix".into(),
                rune_edit: Some(RuneEdit::ReadOnly),
                rune_in_library: Some(false),
                songs: vec![
                    song("catalog:song:9", None, Some(false)),
                    song("song-lib", None, Some(true)),
                ],
                ..SubsonicPlaylistDetail::default()
            })
        }
    }

    impl subsonic::api::Lists for CatalogueApi {
        fn album_list<'a>(&'a self, _size: u32, offset: u32) -> ApiFuture<'a, Vec<SubsonicAlbum>> {
            ready(if offset == 0 {
                vec![album("album-lib", "Near", Some(true))]
            } else {
                Vec::new()
            })
        }
    }

    impl subsonic::api::System for CatalogueApi {}
    impl subsonic::api::Annotation for CatalogueApi {}
    impl subsonic::api::MediaRetrieval for CatalogueApi {}
    impl subsonic::api::Jukebox for CatalogueApi {}
    impl subsonic::api::Scanning for CatalogueApi {}
    impl subsonic::api::RuneVisualizerFeed for CatalogueApi {}
    impl subsonic::api::RuneService for CatalogueApi {}

    fn probe(extensions: &[&str]) -> SubsonicProbe {
        SubsonicProbe {
            api_version: "1.16.1".into(),
            open_subsonic: true,
            server_type: None,
            server_version: None,
            extensions: extensions
                .iter()
                .map(|name| ((*name).into(), vec![1]))
                .collect(),
            roles: SubsonicRoles::default(),
            roles_known: true,
        }
    }

    fn binding(root: &TempDir, extensions: &[&str]) -> (SubsonicBinding, Arc<CatalogueApi>) {
        let api = Arc::new(CatalogueApi::default());
        let binding = SubsonicBinding::new(
            api.clone(),
            probe(extensions),
            root.path().to_path_buf(),
            Uuid::new_v4(),
            true,
            Arc::new(Silent),
        );
        (binding, api)
    }

    const BOTH: &[&str] = &["runeCatalog", "runeRecommendations"];

    async fn search(binding: &SubsonicBinding) -> SearchForResponse {
        search_for_request(
            binding,
            &SearchForRequest {
                call_id: None,
                query_str: "far*".into(),
                fields: Vec::new(),
                n: 10,
            },
        )
        .await
        .unwrap()
        .unwrap()
    }

    async fn membership(
        binding: &SubsonicBinding,
        found: &SearchForResponse,
    ) -> FetchLibraryMembershipResponse {
        fetch_library_membership_request(
            binding,
            &FetchLibraryMembershipRequest {
                track_ids: found.tracks.clone(),
                album_ids: found.albums.clone(),
                artist_ids: found.artists.clone(),
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap()
    }

    fn interned(binding: &SubsonicBinding, kind: SubsonicIdKind, remote: &str) -> i32 {
        binding
            .catalog
            .lock()
            .unwrap()
            .ids
            .intern(kind, remote)
            .unwrap()
    }

    fn assert_unsupported(error: anyhow::Error) {
        assert!(
            matches!(
                error.downcast_ref::<ExpectedRequestError>(),
                Some(ExpectedRequestError::Unsupported {
                    reason: UnsupportedReason::NotImplemented,
                    ..
                })
            ),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn search_covers_the_catalogue_only_with_the_extension() {
        let root = TempDir::new().unwrap();
        let (plain, api) = binding(&root, &["runeService", "runeRecommendations"]);
        let found = search(&plain).await;
        assert_eq!(api.calls(), ["search3:far*"]);
        assert_eq!(found.tracks.len(), 1);
        // Nothing was said about the library, so nothing is outside it.
        let outside = membership(&plain, &found).await;
        assert!(outside.outside_track_ids.is_empty());
        assert!(outside.outside_album_ids.is_empty());
        assert!(outside.outside_artist_ids.is_empty());

        let (catalogue, api) = binding(&root, &["runeCatalog"]);
        let found = search(&catalogue).await;
        assert_eq!(api.calls(), ["search3 all:far*:0"]);
        assert_eq!(found.tracks.len(), 3);
        assert_eq!(found.albums.len(), 2);
        assert_eq!(found.artists.len(), 2);
    }

    #[tokio::test]
    async fn membership_follows_what_the_search_said() {
        let root = TempDir::new().unwrap();
        let (binding, _) = binding(&root, BOTH);
        let found = search(&binding).await;
        let outside = membership(&binding, &found).await;
        assert_eq!(
            outside.outside_track_ids,
            [interned(&binding, SubsonicIdKind::Track, "catalog:song:1")]
        );
        assert_eq!(
            outside.outside_album_ids,
            [interned(&binding, SubsonicIdKind::Album, "catalog:album:1")]
        );
        assert_eq!(
            outside.outside_artist_ids,
            [interned(
                &binding,
                SubsonicIdKind::Artist,
                "catalog:artist:1"
            )]
        );

        let error = fetch_library_membership_request(
            &binding,
            &FetchLibraryMembershipRequest {
                track_ids: vec![9999],
                album_ids: Vec::new(),
                artist_ids: Vec::new(),
                call_id: None,
            },
        )
        .await
        .err()
        .expect("unknown ids are refused");
        assert_eq!(classify(&error), RequestErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn catalogue_items_stay_out_of_the_library_lists_until_added() {
        let root = TempDir::new().unwrap();
        let (binding, _) = binding(&root, BOTH);
        search(&binding).await;
        let summary = |binding, collection_type| async move {
            fetch_collection_group_summary_request(
                binding,
                &FetchCollectionGroupSummaryRequest {
                    collection_type,
                    call_id: None,
                },
            )
            .await
            .unwrap()
            .unwrap()
            .groups
            .iter()
            .map(|group| group.count)
            .sum::<i32>()
        };
        assert_eq!(summary(&binding, CollectionType::Album).await, 1);
        assert_eq!(summary(&binding, CollectionType::Artist).await, 1);
        let groups = fetch_collection_groups_request(
            &binding,
            &FetchCollectionGroupsRequest {
                collection_type: CollectionType::Album,
                bake_cover_arts: false,
                group_titles: vec![generate_group_name("Far"), generate_group_name("Near")],
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap()
        .groups;
        let names: Vec<&str> = groups
            .iter()
            .flat_map(|group| &group.collections)
            .map(|collection| collection.name.as_str())
            .collect();
        assert_eq!(names, ["Near"]);

        let far = interned(&binding, SubsonicIdKind::Album, "catalog:album:1");
        add_to_library_request(
            &binding,
            &AddToLibraryRequest {
                track_ids: Vec::new(),
                album_ids: vec![far],
                call_id: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(summary(&binding, CollectionType::Album).await, 2);
    }

    #[tokio::test]
    async fn adding_resolves_remote_ids_and_counts_the_items_in() {
        let root = TempDir::new().unwrap();
        let (binding, api) = binding(&root, BOTH);
        let found = search(&binding).await;
        let far_song = interned(&binding, SubsonicIdKind::Track, "catalog:song:1");
        let far_album = interned(&binding, SubsonicIdKind::Album, "catalog:album:1");
        // The album's songs, read through its detail query, are outside too.
        let files = mix_query_request(
            &binding,
            &MixQueryRequest {
                queries: vec![MixQuery {
                    operator: "lib::album".into(),
                    parameter: far_album.to_string(),
                }],
                cursor: 0,
                page_size: 10,
                bake_cover_arts: false,
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap()
        .files;
        let album_songs: Vec<i32> = files.iter().map(|file| file.id).collect();
        let outside = fetch_library_membership_request(
            &binding,
            &FetchLibraryMembershipRequest {
                track_ids: album_songs.clone(),
                album_ids: Vec::new(),
                artist_ids: Vec::new(),
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(outside.outside_track_ids, album_songs);

        let response = add_to_library_request(
            &binding,
            &AddToLibraryRequest {
                track_ids: vec![far_song],
                album_ids: vec![far_album],
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert!(response.success);
        assert_eq!(
            api.calls().last().unwrap(),
            "add:catalog:song:1,catalog:album:1"
        );

        let mut all = found.tracks.clone();
        all.extend(&album_songs);
        let outside = fetch_library_membership_request(
            &binding,
            &FetchLibraryMembershipRequest {
                track_ids: all,
                album_ids: found.albums.clone(),
                artist_ids: Vec::new(),
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert!(outside.outside_track_ids.is_empty());
        assert!(outside.outside_album_ids.is_empty());
        assert_eq!(
            binding.catalog.lock().unwrap().songs[&far_song].rune_in_library,
            Some(true)
        );

        // Unknown ids fail before the server is asked.
        let calls = api.calls().len();
        let error = add_to_library_request(
            &binding,
            &AddToLibraryRequest {
                track_ids: vec![9999],
                album_ids: Vec::new(),
                call_id: None,
            },
        )
        .await
        .err()
        .expect("unknown ids are refused");
        assert_eq!(classify(&error), RequestErrorKind::InvalidInput);
        assert_eq!(api.calls().len(), calls);
    }

    #[tokio::test]
    async fn recommendations_are_mix_groups_of_album_and_playlist_mixes() {
        let root = TempDir::new().unwrap();
        let (binding, api) = binding(&root, BOTH);

        let summary = fetch_collection_group_summary_request(
            &binding,
            &FetchCollectionGroupSummaryRequest {
                collection_type: CollectionType::Mix,
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(summary.collection_type, CollectionType::Mix);
        let groups: Vec<(&str, i32)> = summary
            .groups
            .iter()
            .map(|group| (group.group_title.as_str(), group.count))
            .collect();
        // Apple's order; a blank title is replaced by the group's id; empty groups are left out.
        assert_eq!(groups, [("Made for You", 3), ("g2", 1)]);
        assert_eq!(api.calls(), ["recommendations:50"]);

        let groups = fetch_collection_groups_request(
            &binding,
            &FetchCollectionGroupsRequest {
                collection_type: CollectionType::Mix,
                bake_cover_arts: true,
                group_titles: vec!["Made for You".into(), "g2".into(), "Missing".into()],
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap()
        .groups;
        assert_eq!(groups.len(), 3);
        assert!(groups[2].collections.is_empty());
        let made = &groups[0].collections;
        let names: Vec<&str> = made
            .iter()
            .map(|collection| collection.name.as_str())
            .collect();
        assert_eq!(names, ["Far", "Far Mix", "Farther"]);
        let far = interned(&binding, SubsonicIdKind::Album, "catalog:album:1");
        let far_mix = interned(&binding, SubsonicIdKind::Playlist, "catalog:playlist:1");
        let queries = |collection: &Collection| {
            collection
                .queries
                .iter()
                .map(|query| (query.operator.clone(), query.parameter.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            queries(&made[0]),
            [("lib::album".to_owned(), far.to_string())]
        );
        assert_eq!(
            queries(&made[1]),
            [("lib::playlist".to_owned(), far_mix.to_string())]
        );
        for collection in made.iter().chain(&groups[1].collections) {
            assert_eq!(collection.collection_type, CollectionType::Mix);
            assert!(collection.readonly);
        }
        // Groups come from the read the summary made.
        assert_eq!(api.recommendation_reads.load(Ordering::SeqCst), 1);

        let by_ids = fetch_collection_by_ids_request(
            &binding,
            &FetchCollectionByIdsRequest {
                collection_type: CollectionType::Mix,
                bake_cover_arts: false,
                ids: vec![made[1].id, 9999],
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(by_ids.collection_type, CollectionType::Mix);
        assert_eq!(by_ids.result.len(), 1);
        assert_eq!(by_ids.result[0].name, "Far Mix");

        let mix = get_mix_by_id_request(
            &binding,
            &GetMixByIdRequest {
                mix_id: made[1].id,
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap()
        .mix;
        assert_eq!(mix.name, "Far Mix");
        assert_eq!(mix.group, "Made for You");
        assert!(mix.locked);
        assert_eq!(mix.mode, 99);
        let stored = fetch_mix_queries_request(
            &binding,
            &FetchMixQueriesRequest {
                mix_id: made[1].id,
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap()
        .result;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].operator, "lib::playlist");
        assert_eq!(stored[0].parameter, far_mix.to_string());
        assert!(stored[0].available);
        let missing = get_mix_by_id_request(
            &binding,
            &GetMixByIdRequest {
                mix_id: 9999,
                call_id: None,
            },
        )
        .await
        .err()
        .expect("unknown mixes are not found");
        assert_eq!(classify(&missing), RequestErrorKind::NotFound);

        // The playlist mix's query reads the playlist, in its order.
        let files = mix_query_request(
            &binding,
            &MixQueryRequest {
                queries: made[1]
                    .queries
                    .iter()
                    .map(|query| MixQuery {
                        operator: query.operator.clone(),
                        parameter: query.parameter.clone(),
                    })
                    .collect(),
                cursor: 0,
                page_size: 10,
                bake_cover_arts: false,
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap()
        .files;
        let titles: Vec<&str> = files.iter().map(|file| file.title.as_str()).collect();
        assert_eq!(titles, ["Title of catalog:song:9", "Title of song-lib"]);
        assert!(
            api.calls()
                .contains(&"getPlaylist:catalog:playlist:1".to_owned())
        );
        // A recommended playlist the service calls read-only refuses edits before any call.
        assert_eq!(
            binding.catalog.lock().unwrap().playlist_edits.get(&far_mix),
            Some(&RuneEdit::ReadOnly)
        );

        // A later summary reads the recommendations again; the mix ids stay the same.
        fetch_collection_group_summary_request(
            &binding,
            &FetchCollectionGroupSummaryRequest {
                collection_type: CollectionType::Mix,
                call_id: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(api.recommendation_reads.load(Ordering::SeqCst), 2);
        let again = fetch_collection_by_ids_request(
            &binding,
            &FetchCollectionByIdsRequest {
                collection_type: CollectionType::Mix,
                bake_cover_arts: false,
                ids: vec![made[0].id],
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(again.result[0].name, "Far");
    }

    #[tokio::test]
    async fn mixes_stay_unsupported_without_recommendations() {
        let root = TempDir::new().unwrap();
        let (binding, api) = binding(&root, &["runeService", "runeCatalog"]);
        let summary = fetch_collection_group_summary_request(
            &binding,
            &FetchCollectionGroupSummaryRequest {
                collection_type: CollectionType::Mix,
                call_id: None,
            },
        )
        .await
        .err()
        .expect("mixes are unsupported");
        assert_unsupported(summary);
        let groups = fetch_collection_groups_request(
            &binding,
            &FetchCollectionGroupsRequest {
                collection_type: CollectionType::Mix,
                bake_cover_arts: false,
                group_titles: vec!["Made for You".into()],
                call_id: None,
            },
        )
        .await
        .err()
        .expect("mixes are unsupported");
        assert_unsupported(groups);
        let by_ids = fetch_collection_by_ids_request(
            &binding,
            &FetchCollectionByIdsRequest {
                collection_type: CollectionType::Mix,
                bake_cover_arts: false,
                ids: vec![1],
                call_id: None,
            },
        )
        .await
        .err()
        .expect("mixes are unsupported");
        assert_unsupported(by_ids);
        assert!(api.calls().is_empty());
    }

    #[tokio::test]
    async fn a_recommended_playlist_mix_opens_as_playlist_occurrences() {
        let root = TempDir::new().unwrap();
        let (binding, api) = binding(&root, BOTH);
        fetch_collection_group_summary_request(
            &binding,
            &FetchCollectionGroupSummaryRequest {
                collection_type: CollectionType::Mix,
                call_id: None,
            },
        )
        .await
        .unwrap();
        let mix = fetch_collection_groups_request(
            &binding,
            &FetchCollectionGroupsRequest {
                collection_type: CollectionType::Mix,
                bake_cover_arts: true,
                group_titles: vec!["Made for You".into()],
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap()
        .groups
        .remove(0)
        .collections
        .into_iter()
        .find(|collection| collection.name == "Far Mix")
        .unwrap();
        // Dart's track list opens a single `lib::playlist` query as playlist occurrences.
        let [query] = mix.queries.as_slice() else {
            panic!("a mix plays one query");
        };
        assert_eq!(query.operator, "lib::playlist");
        let playlist_id: i32 = query.parameter.parse().unwrap();

        let page = fetch_playlist_occurrences_request(
            &binding,
            &FetchPlaylistOccurrencesRequest {
                request_id: "mix".into(),
                playlist_id,
                after: None,
                page_size: 50,
                bake_cover_arts: false,
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        let rows: Vec<(&str, i32, &str)> = page
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.occurrence_id.as_str(),
                    entry.position,
                    entry.track_uuid.as_str(),
                )
            })
            .collect();
        assert_eq!(rows, [("0", 0, "catalog:song:9"), ("1", 1, "song-lib")]);
        assert!(page.next.is_none());
        assert!(page.cover_art_map.is_empty());
        assert_eq!(
            page.entries[0].file.as_ref().unwrap().title,
            "Title of catalog:song:9"
        );
        assert!(
            api.calls()
                .contains(&"getPlaylist:catalog:playlist:1".to_owned())
        );
    }

    fn detail_page(query: MixQuery, cursor: i32, page_size: i32) -> MixQueryRequest {
        MixQueryRequest {
            queries: vec![query],
            cursor,
            page_size,
            bake_cover_arts: false,
            call_id: None,
        }
    }

    fn query(operator: &str, id: i32) -> MixQuery {
        MixQuery {
            operator: operator.into(),
            parameter: id.to_string(),
        }
    }

    async fn page_titles(binding: &SubsonicBinding, request: &MixQueryRequest) -> Vec<String> {
        mix_query_request(binding, request)
            .await
            .unwrap()
            .unwrap()
            .files
            .into_iter()
            .map(|file| file.title)
            .collect()
    }

    async fn album_page(binding: &SubsonicBinding) -> Vec<String> {
        let summary = fetch_collection_group_summary_request(
            binding,
            &FetchCollectionGroupSummaryRequest {
                collection_type: CollectionType::Album,
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        let groups = fetch_collection_groups_request(
            binding,
            &FetchCollectionGroupsRequest {
                collection_type: CollectionType::Album,
                bake_cover_arts: false,
                group_titles: summary
                    .groups
                    .iter()
                    .map(|group| group.group_title.clone())
                    .collect(),
                call_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        let mut names: Vec<String> = groups
            .groups
            .into_iter()
            .flat_map(|group| group.collections)
            .map(|collection| collection.name)
            .collect();
        names.sort();
        assert_eq!(
            summary.groups.iter().map(|group| group.count).sum::<i32>(),
            i32::try_from(names.len()).unwrap()
        );
        names
    }

    #[tokio::test]
    async fn artist_pages_read_albums_in_order_once_for_all_pages() {
        let root = TempDir::new().unwrap();
        let (binding, api) = binding(&root, BOTH);
        search(&binding).await;
        let artist = interned(&binding, SubsonicIdKind::Artist, "catalog:artist:1");
        let first = page_titles(&binding, &detail_page(query("lib::artist", artist), 0, 4)).await;
        let calls = api.calls().len();
        let second = page_titles(&binding, &detail_page(query("lib::artist", artist), 4, 4)).await;
        // The following page comes from the songs the first one read.
        assert_eq!(api.calls().len(), calls);
        let all: Vec<String> = first.into_iter().chain(second).collect();
        assert_eq!(
            all,
            [
                "Title of catalog:album:slow/1",
                "Title of catalog:album:slow/2",
                "Title of catalog:album:near/1",
                "Title of catalog:album:near/2",
                "Title of catalog:album:last/1",
                "Title of catalog:album:last/2",
            ]
        );
        let reads = api
            .calls()
            .into_iter()
            .filter(|call| call.starts_with("getArtist") || call.starts_with("getAlbum"))
            .count();
        assert_eq!(reads, 4);
    }

    #[tokio::test]
    async fn library_pages_list_only_the_library_lists_and_additions() {
        let root = TempDir::new().unwrap();
        let (binding, _) = binding(&root, BOTH);
        search(&binding).await;
        // A catalogue artist's page brings in a catalogue copy of the library's "Near",
        // which the service says is in the library.
        let artist = interned(&binding, SubsonicIdKind::Artist, "catalog:artist:1");
        page_titles(&binding, &detail_page(query("lib::artist", artist), 0, 10)).await;
        assert_eq!(album_page(&binding).await, ["Near"]);

        // An added catalogue album shows once.
        let far = interned(&binding, SubsonicIdKind::Album, "catalog:album:1");
        add_to_library_request(
            &binding,
            &AddToLibraryRequest {
                track_ids: Vec::new(),
                album_ids: vec![far],
                call_id: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(album_page(&binding).await, ["Far", "Near"]);
    }

    #[tokio::test]
    async fn a_service_that_refuses_the_scope_is_searched_plainly() {
        let root = TempDir::new().unwrap();
        let (binding, api) = binding(&root, &["runeService", "runeCatalog"]);
        let binding = binding.with_rune_service(Some(RuneServiceDescriptor {
            unsupported: vec![RuneUnsupported {
                endpoint: "search3".into(),
                param: Some("runeScope".into()),
            }],
            ..RuneServiceDescriptor::default()
        }));
        search(&binding).await;
        assert_eq!(api.calls(), ["search3:far*"]);
    }

    #[tokio::test]
    async fn a_refused_addition_changes_nothing() {
        let root = TempDir::new().unwrap();
        let (binding, api) = binding(&root, BOTH);
        let found = search(&binding).await;
        let far_song = interned(&binding, SubsonicIdKind::Track, "catalog:song:1");
        let far_album = interned(&binding, SubsonicIdKind::Album, "catalog:album:1");
        api.refuse_additions.store(true, Ordering::SeqCst);
        add_to_library_request(
            &binding,
            &AddToLibraryRequest {
                track_ids: vec![far_song],
                album_ids: vec![far_album],
                call_id: None,
            },
        )
        .await
        .err()
        .expect("the service refused");
        let outside = membership(&binding, &found).await;
        assert_eq!(outside.outside_track_ids, [far_song]);
        assert_eq!(outside.outside_album_ids, [far_album]);
        let catalog = binding.catalog.lock().unwrap();
        assert_eq!(catalog.songs[&far_song].rune_in_library, Some(false));
        assert_eq!(catalog.albums[&far_album].rune_in_library, Some(false));
        assert!(!catalog.listed_albums.contains(&far_album));
    }

    #[tokio::test]
    async fn additions_without_items_or_cached_songs() {
        let root = TempDir::new().unwrap();
        let (binding, api) = binding(&root, BOTH);
        add_to_library_request(
            &binding,
            &AddToLibraryRequest {
                track_ids: Vec::new(),
                album_ids: Vec::new(),
                call_id: None,
            },
        )
        .await
        .unwrap();
        assert!(api.calls().is_empty());

        // An album whose songs were never read is added on its own.
        let found = search(&binding).await;
        let far_album = interned(&binding, SubsonicIdKind::Album, "catalog:album:1");
        add_to_library_request(
            &binding,
            &AddToLibraryRequest {
                track_ids: Vec::new(),
                album_ids: vec![far_album],
                call_id: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(api.calls().last().unwrap(), "add:catalog:album:1");
        let outside = membership(&binding, &found).await;
        assert!(outside.outside_album_ids.is_empty());
        // The search's catalogue song is not one of its songs.
        assert_eq!(outside.outside_track_ids.len(), 1);
    }

    #[tokio::test]
    async fn failed_recommendations_are_read_again() {
        let root = TempDir::new().unwrap();
        let (binding, api) = binding(&root, BOTH);
        api.refuse_recommendations.store(true, Ordering::SeqCst);
        let summary = FetchCollectionGroupSummaryRequest {
            collection_type: CollectionType::Mix,
            call_id: None,
        };
        fetch_collection_group_summary_request(&binding, &summary)
            .await
            .err()
            .expect("the service refused");
        assert!(!binding.catalog.lock().unwrap().mixes_loaded);
        let groups = FetchCollectionGroupsRequest {
            collection_type: CollectionType::Mix,
            bake_cover_arts: false,
            group_titles: vec!["Made for You".into()],
            call_id: None,
        };
        fetch_collection_groups_request(&binding, &groups)
            .await
            .err()
            .expect("still refused");

        api.refuse_recommendations.store(false, Ordering::SeqCst);
        let made = fetch_collection_groups_request(&binding, &groups)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(made.groups[0].collections.len(), 3);
        assert!(binding.catalog.lock().unwrap().mixes_loaded);
        assert_eq!(api.recommendation_reads.load(Ordering::SeqCst), 3);
    }

    fn catalogue_slots(slots: &RequestSlots) -> [Slot; 4] {
        [
            FetchLibraryMembershipRequest::slot(slots),
            AddToLibraryRequest::slot(slots),
            GetMixByIdRequest::slot(slots),
            FetchMixQueriesRequest::slot(slots),
        ]
    }

    fn hidden() -> Slot {
        Slot::Unsupported {
            reason: UnsupportedReason::NotImplemented,
        }
    }

    #[test]
    fn catalogue_requests_show_only_for_servers_that_declare_them() {
        let plain = RequestSlots::subsonic(&probe(&["runeService", "songLyrics"]));
        assert_eq!(catalogue_slots(&plain), [hidden(); 4]);
        let catalogue = RequestSlots::subsonic(&probe(&["runeCatalog"]));
        assert_eq!(
            catalogue_slots(&catalogue),
            [Slot::Translate, Slot::Translate, hidden(), hidden()]
        );
        let recommendations = RequestSlots::subsonic(&probe(&["runeRecommendations"]));
        assert_eq!(
            catalogue_slots(&recommendations),
            [hidden(), hidden(), Slot::Translate, Slot::Translate]
        );
        // A plain Subsonic server cannot declare extensions at all.
        let mut legacy = probe(BOTH);
        legacy.open_subsonic = false;
        assert_eq!(
            catalogue_slots(&RequestSlots::subsonic(&legacy)),
            [hidden(); 4]
        );

        // A local library has no catalogue, and keeps its own mixes.
        let local = Implementation::Local.slots();
        assert_eq!(
            catalogue_slots(&local),
            [hidden(), hidden(), Slot::Native, Slot::Native]
        );
        assert_eq!(SearchForRequest::slot(&local), Slot::Native);
    }

    #[test]
    fn a_descriptor_hides_what_the_service_refuses_of_the_extensions() {
        let slots = super::tests::rune_slots_with(&[("addToRuneLibrary", None)], false, BOTH);
        assert_eq!(
            catalogue_slots(&slots),
            [Slot::Translate, hidden(), Slot::Translate, Slot::Translate]
        );
        let slots = super::tests::rune_slots_with(&[("search3", Some("runeScope"))], false, BOTH);
        assert_eq!(FetchLibraryMembershipRequest::slot(&slots), hidden());
        assert_eq!(SearchForRequest::slot(&slots), Slot::Translate);

        let slots = super::tests::rune_slots_with(&[("getRuneRecommendations", None)], false, BOTH);
        assert_eq!(GetMixByIdRequest::slot(&slots), hidden());
        assert_eq!(FetchMixQueriesRequest::slot(&slots), hidden());
        assert_eq!(
            FetchCollectionGroupSummaryRequest::slot(&slots),
            Slot::Translate
        );

        // Without albums or artists to list, recommendations keep the collection pages.
        let no_lists = [("getAlbumList2", None), ("getArtists", None)];
        let slots = super::tests::rune_slots_with(&no_lists, false, BOTH);
        assert_eq!(
            FetchCollectionGroupSummaryRequest::slot(&slots),
            Slot::Translate
        );
        assert_eq!(FetchCollectionGroupsRequest::slot(&slots), Slot::Translate);
        let slots = super::tests::rune_slots_with(&no_lists, false, &[]);
        assert_eq!(FetchCollectionGroupSummaryRequest::slot(&slots), hidden());
        assert_eq!(FetchCollectionGroupsRequest::slot(&slots), hidden());
    }
}
