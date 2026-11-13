use crate::stream_utils::ManagedStream;
use std::{
    collections::HashMap,
    fmt::Debug,
    future::Future,
    io::{BufReader, Cursor, Error, ErrorKind},
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use fsio::{FileIoError, FileStream, FsIo};
use log::{debug, error, info, warn};
use rodio::{PlayError, Sink, Source, source::SeekError};
use tokio::{
    sync::{mpsc, oneshot},
    time::{Instant, interval, sleep_until},
};
use tokio_util::sync::CancellationToken;

use crate::buffered::{RuneBuffered, rune_buffered};
use crate::decoder::RuneDecoder;
use crate::hosted::{HostAction, HostBridge, HostCommand, HostEvent, HostedTrack};
use crate::output_stream::{RuneOutputStream, RuneOutputStreamHandle};
use crate::pcm_tap::PcmTap;
use crate::player::{
    AudioPlaybackNote, AudioSourceMode, ClearHandoffBlocksResult, HandoffBlockId,
    HandoffLoadResult, InterruptedItem, LibraryTrackKey, PlaybackSource, PlaybackSourceResolver,
    PlayerCommandError, PlayerStatus, PlayingItem, PlaylistEdit, PlaylistPlacement, QueueEntry,
    RemoteAudioError, RemoteAudioSource, ServiceItem, ServiceTarget, TakeBackBlockResult,
    UnavailableReason,
};
use crate::realtime_fft::RealTimeFFT;
use crate::shared_source::SharedSource;
use crate::strategies::{
    AddMode, PlaybackStrategy, RepeatAllStrategy, RepeatOneStrategy, SequentialStrategy,
    ShuffleStrategy, UpdateReason,
};
use crate::stream_utils::create_stream_with_authorizer;

const SOURCE_START_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a host that was told to play a track may take to report on it, before the track counts
/// as unreachable.
const HOST_STATUS_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a host that plays a track may go without reporting, before the track counts as
/// unreachable.
const HOST_SILENCE_TIMEOUT: Duration = Duration::from_secs(15);
/// How long finding out whether a host plays the next track may take. A track that takes longer
/// is not given to the host in advance, and is handed over as usual once it is current.
const HOST_NEXT_TIMEOUT: Duration = Duration::from_secs(5);

async fn bound_source_start<T>(
    limit: Duration,
    source: impl Future<Output = Result<T>>,
) -> Result<T> {
    match tokio::time::timeout(limit, source).await {
        Ok(result) => result,
        Err(_) => Err(anyhow!(RemoteAudioError::unreachable(Error::new(
            ErrorKind::TimedOut,
            "audio source did not start before the deadline",
        )))),
    }
}

async fn probe_decoder<R>(
    reader: R,
    extension: Option<String>,
    known_len: Option<u64>,
) -> Result<RuneDecoder<R>>
where
    R: std::io::Read + std::io::Seek + Send + Sync + 'static,
{
    tokio::task::spawn_blocking(move || {
        RuneDecoder::new_with_length(reader, extension.as_deref(), known_len)
    })
    .await
    .context("decoder probe task failed")?
}

async fn open_local_source(fsio: Arc<FsIo>, path: PathBuf, remote: bool) -> Result<AnySource> {
    let open_path = path.clone();
    let stream = tokio::task::spawn_blocking(move || fsio.open(&open_path, "r"))
        .await
        .context("audio file open task failed")?
        .map_err(|error| {
            if remote {
                return match error {
                    FileIoError::PermissionDenied(_) => anyhow!(RemoteAudioError::AccessRevoked),
                    error @ (FileIoError::Remote(_) | FileIoError::PathNotFound(_)) => {
                        anyhow!(RemoteAudioError::unreachable(error))
                    }
                    other => anyhow::Error::new(other),
                };
            }
            anyhow::Error::new(error)
        })?;
    let (stream, failure) = if remote {
        let failure = Arc::new(Mutex::new(None));
        (
            Box::new(FailureTrackingFile {
                inner: stream,
                failure: failure.clone(),
            }) as Box<dyn FileStream>,
            Some(failure),
        )
    } else {
        (stream, None)
    };
    let extension = path.extension().and_then(|value| value.to_str());
    let decoder = match probe_decoder(BufReader::new(stream), extension.map(str::to_owned), None)
        .await
    {
        Ok(decoder) => decoder,
        Err(error) => {
            if let Some(reason) = failure
                .as_ref()
                .and_then(|failure| failure.lock().ok().and_then(|value| *value))
            {
                return Err(match reason {
                    UnavailableReason::AccessRevoked => anyhow!(RemoteAudioError::AccessRevoked),
                    UnavailableReason::SourceUnreachable => {
                        anyhow!(RemoteAudioError::unreachable(error))
                    }
                    UnavailableReason::LibraryUnavailable => {
                        anyhow!(RemoteAudioError::LibraryUnavailable)
                    }
                    UnavailableReason::Unreadable => error,
                    UnavailableReason::CredentialsRejected => {
                        anyhow!(RemoteAudioError::CredentialsRejected)
                    }
                    UnavailableReason::TrackGone => anyhow!(RemoteAudioError::TrackGone),
                });
            }
            return Err(error);
        }
    };
    Ok(AnySource::Local(rune_buffered(decoder), failure))
}

struct FailureTrackingFile {
    inner: Box<dyn FileStream>,
    failure: Arc<Mutex<Option<UnavailableReason>>>,
}

impl FailureTrackingFile {
    fn record(&self, error: &std::io::Error) {
        let reason = match error.kind() {
            ErrorKind::PermissionDenied => UnavailableReason::AccessRevoked,
            ErrorKind::NotFound => UnavailableReason::SourceUnreachable,
            _ => UnavailableReason::SourceUnreachable,
        };
        if let Ok(mut failure) = self.failure.lock() {
            *failure = Some(reason);
        }
    }
}

impl std::io::Read for FailureTrackingFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let result = self.inner.read(buf);
        if let Err(error) = &result {
            self.record(error);
        }
        result
    }
}

impl std::io::Seek for FailureTrackingFile {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        let result = self.inner.seek(pos);
        if let Err(error) = &result {
            self.record(error);
        }
        result
    }
}

impl std::io::Write for FailureTrackingFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl FileStream for FailureTrackingFile {
    fn sync_all(&mut self) -> Result<(), FileIoError> {
        self.inner.sync_all()
    }
}

pub enum AnySource {
    Local(
        RuneBuffered<RuneDecoder<BufReader<Box<dyn FileStream>>>>,
        Option<Arc<Mutex<Option<UnavailableReason>>>>,
    ),
    Online(
        RuneBuffered<RuneDecoder<ManagedStream>>,
        Arc<Mutex<Option<UnavailableReason>>>,
    ),
    Buffered(
        RuneBuffered<RuneDecoder<Cursor<Vec<u8>>>>,
        AudioPlaybackNote,
    ),
}

async fn open_library_stream(
    resolver: &Arc<dyn PlaybackSourceResolver>,
    id: i32,
) -> Result<(AnySource, PathBuf)> {
    debug!("Resolving remote audio source for library track {id}");
    let remote = resolver.resolve(id).await?;
    let size = remote.size;
    let guard = remote.cancellation.clone().drop_guard();
    debug!("Opening initial HTTP range for library track {id}");
    let reader =
        crate::stream_utils::create_stream_with_authorizer(remote.request, &remote.url).await?;
    let failed = remote.failure_reason;
    let reader = ManagedStream::new_with_failure(reader, remote.cancellation, failed.clone());
    debug!("Probing decoder for library track {id}");
    let decoder = probe_decoder(reader, Some(remote.extension), Some(size)).await?;
    guard.disarm();
    debug!("Decoder ready for library track {id}");
    Ok((
        AnySource::Online(rune_buffered(decoder), failed),
        PathBuf::new(),
    ))
}

async fn open_service_stream(remote: RemoteAudioSource) -> Result<(AnySource, PathBuf)> {
    let size = remote.size;
    let guard = remote.cancellation.clone().drop_guard();
    let failed = remote.failure_reason;
    match remote.request.prepare().await? {
        AudioSourceMode::Buffered { bytes, note } => {
            let size = u64::try_from(bytes.len()).ok();
            let decoder = probe_decoder(Cursor::new(bytes), Some(remote.extension), size).await?;
            guard.disarm();
            return Ok((
                AnySource::Buffered(rune_buffered(decoder), note),
                PathBuf::new(),
            ));
        }
        AudioSourceMode::Ranged => {}
    }
    let reader = match create_stream_with_authorizer(remote.request, &remote.url).await {
        Ok(reader) => reader,
        Err(error) => {
            let reason = failed.lock().ok().and_then(|reason| *reason);
            return Err(match reason {
                Some(UnavailableReason::AccessRevoked) => {
                    anyhow!(RemoteAudioError::AccessRevoked)
                }
                Some(UnavailableReason::CredentialsRejected) => {
                    anyhow!(RemoteAudioError::CredentialsRejected)
                }
                Some(UnavailableReason::Unreadable | UnavailableReason::TrackGone) => {
                    anyhow!(RemoteAudioError::TrackGone)
                }
                _ => error,
            });
        }
    };
    let reader = ManagedStream::new_with_failure(reader, remote.cancellation, failed.clone());
    let decoder =
        probe_decoder(reader, Some(remote.extension), (size != 0).then_some(size)).await?;
    guard.disarm();
    Ok((
        AnySource::Online(rune_buffered(decoder), failed),
        PathBuf::new(),
    ))
}

/// What a load brings: audio this player decodes, or a track that a platform host plays.
#[derive(Debug)]
pub enum LoadedSource {
    Audio(AnySource),
    Hosted(HostedTrack),
}

impl From<AnySource> for LoadedSource {
    fn from(source: AnySource) -> Self {
        Self::Audio(source)
    }
}

// The id under which `host` plays `item`, if it does. A track the resolver fails on, or takes
// too long with, counts as one that `host` does not play.
async fn hosted_by(
    resolver: &Arc<dyn PlaybackSourceResolver>,
    item: &PlayingItem,
    host: &str,
) -> Option<HostedTrack> {
    let PlayingItem::LibraryTrack { library_id, track } = item else {
        return None;
    };
    let target = async {
        match resolver
            .resolve_track(library_id.to_string(), track.clone())
            .await?
        {
            LibraryTrackKey::Service(track) => resolver
                .resolve_service(ServiceItem {
                    library_id: *library_id,
                    track,
                })
                .await
                .map(Some),
            LibraryTrackKey::Rune(_) => Ok(None),
        }
    };
    match tokio::time::timeout(HOST_NEXT_TIMEOUT, target).await {
        Ok(Ok(Some(ServiceTarget::Hosted(track)))) if track.host == host => Some(track),
        // A stream that is not opened is not watched either.
        Ok(Ok(Some(ServiceTarget::Stream(remote)))) => {
            remote.cancellation.cancel();
            None
        }
        Ok(Ok(_)) => None,
        Ok(Err(error)) => {
            debug!("Could not tell whether host {host} plays {item}: {error:?}");
            None
        }
        Err(_) => {
            warn!("Finding out whether host {host} plays {item} took too long");
            None
        }
    }
}

impl Debug for AnySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnySource::Local(_, _) => f.debug_struct("AnySource::Local").finish(),
            AnySource::Online(_, _) => f.debug_struct("AnySource::Online").finish(),
            AnySource::Buffered(_, note) => {
                f.debug_tuple("AnySource::Buffered").field(note).finish()
            }
        }
    }
}

impl AnySource {
    fn current_samples(&self) -> Option<Vec<i16>> {
        match self {
            AnySource::Local(s, _) => s.current_samples(),
            AnySource::Online(s, _) => s.current_samples(),
            AnySource::Buffered(s, _) => s.current_samples(),
        }
    }
}

impl Iterator for AnySource {
    type Item = i16;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            AnySource::Local(s, _) => s.next(),
            AnySource::Online(s, _) => s.next(),
            AnySource::Buffered(s, _) => s.next(),
        }
    }
}

impl Source for AnySource {
    fn current_frame_len(&self) -> Option<usize> {
        match self {
            AnySource::Local(s, _) => s.current_frame_len(),
            AnySource::Online(s, _) => s.current_frame_len(),
            AnySource::Buffered(s, _) => s.current_frame_len(),
        }
    }

    fn channels(&self) -> u16 {
        match self {
            AnySource::Local(s, _) => s.channels(),
            AnySource::Online(s, _) => s.channels(),
            AnySource::Buffered(s, _) => s.channels(),
        }
    }

    fn sample_rate(&self) -> u32 {
        match self {
            AnySource::Local(s, _) => s.sample_rate(),
            AnySource::Online(s, _) => s.sample_rate(),
            AnySource::Buffered(s, _) => s.sample_rate(),
        }
    }

    fn total_duration(&self) -> Option<Duration> {
        match self {
            AnySource::Local(s, _) => s.total_duration(),
            AnySource::Online(s, _) => s.total_duration(),
            AnySource::Buffered(s, _) => s.total_duration(),
        }
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        match self {
            AnySource::Local(s, _) => s.try_seek(pos),
            AnySource::Online(s, _) => s.try_seek(pos),
            AnySource::Buffered(s, _) => s.try_seek(pos),
        }
    }
}

impl AnySource {
    fn stream_failure(&self) -> Option<Arc<Mutex<Option<UnavailableReason>>>> {
        match self {
            AnySource::Local(_, failed) => failed.clone(),
            AnySource::Online(_, failed) => Some(failed.clone()),
            AnySource::Buffered(_, _) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlaybackMode {
    Sequential,
    RepeatOne,
    RepeatAll,
    Shuffle,
}

impl From<u32> for PlaybackMode {
    fn from(value: u32) -> Self {
        match value {
            0 => PlaybackMode::Sequential,
            1 => PlaybackMode::RepeatOne,
            2 => PlaybackMode::RepeatAll,
            3 => PlaybackMode::Shuffle,
            _ => PlaybackMode::Sequential,
        }
    }
}

impl From<PlaybackMode> for u32 {
    fn from(mode: PlaybackMode) -> Self {
        match mode {
            PlaybackMode::Sequential => 0,
            PlaybackMode::RepeatOne => 1,
            PlaybackMode::RepeatAll => 2,
            PlaybackMode::Shuffle => 3,
        }
    }
}

#[derive(Debug)]
pub enum PlayerCommand {
    WithCompletion {
        command: Box<PlayerCommand>,
        response: oneshot::Sender<std::result::Result<(), PlayerCommandError>>,
    },
    Restore {
        tracks: Vec<(PlayingItem, PlaybackSource)>,
        index: Option<usize>,
        position: Duration,
        mode: PlaybackMode,
    },
    RestoreTagged {
        entries: Vec<QueueEntry>,
        index: Option<usize>,
        position: Duration,
        mode: PlaybackMode,
    },
    Load {
        index: usize,
    },
    LoadAtPosition {
        index: usize,
        position: Duration,
        play: bool,
    },
    LoadComplete {
        generation: u64,
        result: Box<Result<LoadedSource>>,
        item: PlayingItem,
        original_item: PlayingItem,
        index: usize,
        mapped: bool,
        playlist_index: usize,
        path: PathBuf,
    },
    Play,
    Pause,
    Stop,
    Next,
    Previous,
    Switch(usize),
    Seek(f64),
    AddToPlaylist {
        tracks: Vec<(PlayingItem, PlaybackSource)>,
        mode: AddMode,
    },
    EditPlaylist(PlaylistEdit),
    HandoffLoad {
        tracks: Vec<(PlayingItem, PlaybackSource)>,
        block: HandoffBlockId,
        start_index: Option<usize>,
        position: Duration,
        play: bool,
        playback_mode: Option<PlaybackMode>,
        response: oneshot::Sender<HandoffLoadResult>,
    },
    TakeBackBlock {
        block: HandoffBlockId,
        interrupted: InterruptedItem,
        response: oneshot::Sender<Option<TakeBackBlockResult>>,
    },
    ClearHandoffBlocks {
        response: oneshot::Sender<ClearHandoffBlocksResult>,
    },
    RemoveFromPlaylist {
        index: usize,
    },
    ClearPlaylist,
    MovePlayListItem {
        old_index: usize,
        new_index: usize,
    },
    SetPlaybackMode(PlaybackMode),
    SetVolume(f32),
    SetRealtimeFFTEnabled(bool),
    SetAdaptiveSwitchingEnabled(bool),
    SetMixWithOthers(bool),
    StatusAfterPending(oneshot::Sender<PlayerStatus>),
    Output(OutputEvent),
    Hosted(HostEvent),
    // Whether the host of the current track plays the track after it, as asked in `request`.
    HostNextResolved {
        request: u64,
        track: Option<HostedTrack>,
    },
}

/// What happened to this device's audio output. The player decides from its own state whether to
/// pause, resume or open the output again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputEvent {
    /// Something else took over the output, such as a call or an alarm, and the current stream
    /// stopped.
    Interrupted,
    /// The output is free again after an interruption. `resume` says whether playback that the
    /// interruption paused should go on.
    InterruptionEnded { resume: bool },
    /// The device that was playing went away, such as headphones that were unplugged or turned
    /// off.
    DeviceLost,
    /// A new device became available and the output moves to it, such as headphones that were
    /// plugged in.
    DeviceAdded,
}

#[derive(Debug)]
pub enum PlayerEvent {
    StatusAfterPending(oneshot::Sender<PlayerStatus>),
    Unavailable(PlayingItem, UnavailableReason),
    UnavailableCleared(Option<PlayingItem>),
    Stopped,
    Loaded {
        item: PlayingItem,
        index: usize,
        path: PathBuf,
        playback_mode: PlaybackMode,
        position: Duration,
    },
    Playing {
        item: PlayingItem,
        index: usize,
        path: PathBuf,
        playback_mode: PlaybackMode,
        position: Duration,
    },
    Paused {
        item: PlayingItem,
        index: usize,
        path: PathBuf,
        playback_mode: PlaybackMode,
        position: Duration,
    },
    EndOfPlaylist,
    EndOfTrack {
        item: PlayingItem,
        index: usize,
        path: PathBuf,
        playback_mode: PlaybackMode,
    },
    Error {
        item: PlayingItem,
        index: usize,
        path: PathBuf,
        error: String,
    },
    Progress {
        item: Option<PlayingItem>,
        index: Option<usize>,
        path: Option<PathBuf>,
        position: Duration,
        playback_mode: PlaybackMode,
        ready: bool,
    },
    VolumeUpdate(f32),
    // The host that plays the current track, from when a hosted track becomes current until no
    // hosted track is.
    HostedChanged(Option<String>),
    PlaylistUpdated(Vec<PlaylistItem>),
    RealtimeFFT(Vec<f32>),
    Log(InternalLog),
}

#[derive(Debug, Clone)]
pub struct PlaylistItem {
    pub item: PlayingItem,
    pub source: PlaybackSource,
    pub block: Option<HandoffBlockId>,
}

fn queue_entry(item: &PlaylistItem) -> QueueEntry {
    QueueEntry {
        item: item.item.clone(),
        block: item.block,
    }
}

#[derive(Debug, PartialEq)]
enum InternalPlaybackState {
    Playing,
    Paused,
    Stopped,
    Loading,
}

#[derive(Debug)]
struct PendingLoad {
    index: usize,
    play: bool,
    mapped: bool,
    restore_position: Option<Duration>,
    completions: Vec<CommandCompletion>,
}

#[derive(Debug)]
struct CommandCompletion {
    response: oneshot::Sender<std::result::Result<(), PlayerCommandError>>,
    requires_playing: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeekOutcome {
    Applied,
    Deferred,
    CannotApply,
}

/// A track that finished loading, as the player keeps it once it is current.
struct LoadedTrack {
    item: PlayingItem,
    // The index the load asked for, and whether the playback mode maps it to a playlist position.
    // Next and Previous go on from it, and a reopen asks for it again.
    index: usize,
    mapped: bool,
    // Where the track stands in the playlist, which the events report.
    playlist_index: usize,
    path: PathBuf,
}

/// The current track once it was handed to a platform host, which plays it. The host contract is
/// in [`crate::hosted`].
///
/// The host's queue follows the player's play order from the current track on, one track ahead.
/// A hand-over `set`s the current track at index 0. When the track that Next goes to from there
/// is one the same host plays, the host gets it next with an `add`, once the resolver said so; the
/// hand-over itself never waits for that. After every command the player works out that track
/// again, and when a queue edit or a change of the playback mode changed it, the host loses the
/// one it holds next with a `remove` at once, and gets the new one with an `add` once it is
/// known. The current track plays on throughout.
///
/// At the end of the current track the host moves on to the next one by itself, without a gap,
/// and reports a status one index higher with that track's id. The player then counts the current
/// track as played through and makes the next one current where Next would have gone, without a
/// new hand-over. A report one index higher with any other id, or while the player holds nothing
/// there, is about a track the player had taken out of the host's queue as the host moved on to
/// it: the current track counts as played through just the same, and the track that comes next
/// loads and is handed over anew, which replaces the host's queue.
///
/// Entries before the current one stay in the host's queue until the next `set`, and only the
/// entry after it is ever removed, so a host index names the same track until then. The current
/// track stands at `index`, and the next one, if the host holds it, at `index + 1`.
///
/// Next, while the host holds the track that Next goes to, is a `skip` to it. Previous, Switch
/// and any other load hand the track they bring over with a new `set`, as does Next to anything
/// else. A seek is a `skip` within the current track, and leaves the next one where it is.
struct HostedPlayback {
    track: HostedTrack,
    // Where the current track stands in the host's queue.
    index: u32,
    // The batch that gave the host the current track: the `set`, or the `add` that queued it as
    // the next one. An end or a failure counts from it on, up to the latest batch.
    since_revision: u64,
    // The latest batch that moved the host: one that changed which entry is current, where it
    // stands or whether it plays. A status from before it may undo what it did, so a status
    // counts from it on. Batches that only add or remove the next entry or set the gain move
    // nothing, so that a volume drag or a queue edit does not make the host's reports stale.
    status_revision: u64,
    latest_revision: u64,
    next: HostNext,
    playing: bool,
    // Where the track stood when the host last reported, or when it was last told to move, and
    // when that was.
    position: Duration,
    reported_at: Instant,
    // When the host was last told to play the track, and when it last reported on it since it
    // was handed over.
    started_at: Instant,
    heard_at: Option<Instant>,
    // The host lost the track once and was given it again.
    recovered: bool,
    ended: bool,
}

/// The track after the current hosted one in play order, as far as its host is concerned.
enum HostNext {
    /// No track follows that the host could be given: the playlist ends, the playback mode is
    /// repeat-one, or the next track is of another library or marked unavailable.
    Nothing,
    /// The resolver is asked whether the host plays `item`. `request` tells its answer from the
    /// answers to earlier questions, and the lookup stops when this is dropped.
    Resolving {
        request: u64,
        item: PlayingItem,
        _lookup: Lookup,
    },
    /// The host holds `item` right after the current track, given to it in `revision`.
    Queued {
        item: PlayingItem,
        track: HostedTrack,
        revision: u64,
    },
    /// `item` follows, but the host does not get it in advance: another player plays it, the
    /// resolver could not tell in time, or the host failed it. It is loaded as usual once it is
    /// current.
    Declined { item: PlayingItem },
}

impl HostNext {
    fn item(&self) -> Option<&PlayingItem> {
        match self {
            Self::Nothing => None,
            Self::Resolving { item, .. } | Self::Queued { item, .. } | Self::Declined { item } => {
                Some(item)
            }
        }
    }
}

/// A lookup of the next track, which stops once nothing waits for its answer.
struct Lookup(tokio::task::JoinHandle<()>);

impl Drop for Lookup {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl HostedPlayback {
    // Where the track stands now: it moves on from the last report while it plays.
    fn position(&self) -> Duration {
        if self.playing {
            self.position + self.reported_at.elapsed()
        } else {
            self.position
        }
    }

    fn report(&mut self, position: Duration, playing: bool) {
        self.position = position;
        self.playing = playing;
        self.reported_at = Instant::now();
    }

    // Whether an end or a failure that the host reported in `revision` for the entry at `index`
    // is about the current track.
    fn is_current(&self, revision: u64, index: u32) -> bool {
        index == self.index && (self.since_revision..=self.latest_revision).contains(&revision)
    }

    // Whether `actions` move the host: change which entry is current, where it stands or
    // whether it plays.
    fn moves(actions: &[HostAction]) -> bool {
        actions.iter().any(|action| {
            !matches!(
                action,
                HostAction::Add { .. } | HostAction::Remove { .. } | HostAction::SetGain { .. }
            )
        })
    }
}

fn offset_ms(position: Duration) -> u64 {
    u64::try_from(position.as_millis()).unwrap_or(u64::MAX)
}

/// The place a seek asks for, in seconds. A place before the start is the start, and a value
/// that names no place, such as NaN or infinity, is none.
fn seek_position(seconds: f64) -> Option<Duration> {
    if seconds.is_nan() {
        return None;
    }
    Duration::try_from_secs_f64(seconds.max(0.0)).ok()
}

fn try_new_sink(stream: &RuneOutputStreamHandle) -> Result<Sink, PlayError> {
    let (sink, queue_rx) = Sink::new_idle();
    stream.play_raw(queue_rx)?;
    Ok(sink)
}

#[derive(Debug, Clone)]
pub struct InternalLog {
    pub domain: String,
    pub error: String,
}

impl<S> Source for SharedSource<S>
where
    S: Source,
    S::Item: rodio::Sample,
{
    fn current_frame_len(&self) -> Option<usize> {
        self.inner.lock().unwrap().current_frame_len()
    }

    fn channels(&self) -> u16 {
        self.inner.lock().unwrap().channels()
    }

    fn sample_rate(&self) -> u32 {
        self.inner.lock().unwrap().sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.lock().unwrap().total_duration()
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        self.inner.lock().unwrap().try_seek(pos)
    }
}

pub(crate) struct PlayerInternal {
    commands: mpsc::UnboundedReceiver<PlayerCommand>,
    commands_sender: mpsc::UnboundedSender<PlayerCommand>,
    event_sender: mpsc::UnboundedSender<PlayerEvent>,
    fsio: Arc<FsIo>,
    source_resolver: Arc<RwLock<Option<Arc<dyn PlaybackSourceResolver>>>>,
    realtime_fft: Arc<Mutex<RealTimeFFT>>,
    fft_enabled: Arc<Mutex<bool>>,
    pcm_tap: Arc<PcmTap>,
    playlist: Vec<PlaylistItem>,
    current_item: Option<PlayingItem>,
    current_track_index: Option<usize>,
    // Whether `current_track_index` is an index the playback mode maps to a playlist position, as
    // Next, Previous and Load ask for, or a playlist position itself, as Switch and Restore ask
    // for.
    current_track_mapped: bool,
    current_track_path: Option<PathBuf>,
    active_stream_failure: Option<Arc<Mutex<Option<UnavailableReason>>>>,
    sink: Option<Sink>,
    _stream: Option<RuneOutputStream>,
    state: InternalPlaybackState,
    pending_load: Option<PendingLoad>,
    active_load_completions: Vec<CommandCompletion>,
    current_completion: Option<CommandCompletion>,
    // The playlist position of the track that is loading.
    loading_index: Option<usize>,
    load_generation: u64,
    // Whether the load in flight starts playing when it completes. Play and Pause change it
    // while the track is still loading.
    play_when_loaded: bool,
    debounce_timer: Option<Instant>,
    cancellation_token: CancellationToken,
    playback_mode: PlaybackMode,
    playback_strategy: Box<dyn PlaybackStrategy>,
    volume: f32,
    stream_error_sender: mpsc::UnboundedSender<String>,
    stream_error_receiver: mpsc::UnboundedReceiver<String>,
    adaptive_switching: bool,
    // iOS: other apps keep playing while this one does. The audio session reads it; elsewhere it
    // is only kept.
    mix_with_others: bool,
    // Where the current track starts once its output opens. Set when a load starts and taken
    // when the output opens. It stays while the track has no output that plays, so that the next
    // play opens one there: the track came up paused, no output could be opened, or a stale
    // output cannot seek.
    restore_position: Option<Duration>,
    // Where the sink goes once its stopped output runs again. A stopped output pulls no samples,
    // so the sink would wait for a seek forever, and the next play seeks once the output runs.
    pending_seek: Option<Duration>,
    // The current track has no output that can play: it came up paused and none was opened, none
    // could be opened, its stream failed or its device went away. The next play opens one where
    // the track stands.
    output_stale: bool,
    // An interruption paused playback, so playback goes on when the interruption ends. A play,
    // or a track that starts playing, settles it first.
    resume_after_interruption: bool,
    unavailable: HashMap<PlayingItem, UnavailableReason>,
    host_bridge: Arc<RwLock<Option<Arc<dyn HostBridge>>>>,
    hosted: Option<HostedPlayback>,
    // The revision of the last batch sent to a host.
    host_revision: u64,
    // Hosts that said they cannot play, and why, until they say they can.
    hosts_unable: HashMap<String, UnavailableReason>,
    // The last question about the host's next track.
    host_next_request: u64,
    // How many of the next attempts to open an output fail.
    #[cfg(test)]
    failing_opens: Arc<AtomicUsize>,
    // How many times the player tried to open an output.
    #[cfg(test)]
    output_opens: Arc<AtomicUsize>,
    // How many times the player stopped an output while paused, and tried to start one again.
    #[cfg(test)]
    output_suspends: Arc<AtomicUsize>,
    #[cfg(test)]
    output_resumes: Arc<AtomicUsize>,
    // How many of the next attempts to start a stopped output fail.
    #[cfg(test)]
    failing_resumes: Arc<AtomicUsize>,
}

impl PlayerInternal {
    pub fn new(
        commands: mpsc::UnboundedReceiver<PlayerCommand>,
        event_sender: mpsc::UnboundedSender<PlayerEvent>,
        cancellation_token: CancellationToken,
        commands_sender: mpsc::UnboundedSender<PlayerCommand>,
        fsio: Arc<FsIo>,
        pcm_tap: Arc<PcmTap>,
        source_resolver: Arc<RwLock<Option<Arc<dyn PlaybackSourceResolver>>>>,
    ) -> Self {
        let (stream_error_sender, stream_error_receiver) = mpsc::unbounded_channel();
        Self {
            commands,
            commands_sender,
            event_sender,
            fsio,
            source_resolver,
            playlist: Vec::new(),
            current_item: None,
            current_track_index: None,
            current_track_mapped: false,
            current_track_path: None,
            active_stream_failure: None,
            sink: None,
            _stream: None,
            realtime_fft: Arc::new(Mutex::new(RealTimeFFT::new(512))),
            state: InternalPlaybackState::Stopped,
            pending_load: None,
            active_load_completions: Vec::new(),
            current_completion: None,
            loading_index: None,
            load_generation: 0,
            play_when_loaded: false,
            debounce_timer: None,
            cancellation_token,
            playback_mode: PlaybackMode::Sequential,
            playback_strategy: Box::new(SequentialStrategy),
            volume: 1.0,
            fft_enabled: Arc::new(Mutex::new(false)),
            pcm_tap,
            stream_error_sender,
            stream_error_receiver,
            adaptive_switching: false,
            mix_with_others: false,
            restore_position: None,
            pending_seek: None,
            output_stale: false,
            resume_after_interruption: false,
            unavailable: HashMap::new(),
            host_bridge: Arc::new(RwLock::new(None)),
            hosted: None,
            host_revision: 0,
            hosts_unable: HashMap::new(),
            host_next_request: 0,
            #[cfg(test)]
            failing_opens: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            output_opens: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            output_suspends: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            output_resumes: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            failing_resumes: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn with_host_bridge(
        mut self,
        host_bridge: Arc<RwLock<Option<Arc<dyn HostBridge>>>>,
    ) -> Self {
        self.host_bridge = host_bridge;
        self
    }

    pub async fn run(&mut self) -> Result<()> {
        let mut progress_interval = interval(Duration::from_millis(100));

        let fft_receiver = match self.realtime_fft.lock() {
            Ok(fft) => fft.subscribe(),
            Err(e) => {
                bail!("Failed to lock realtime FFT: {e:?}");
            }
        };

        loop {
            tokio::select! {
                Some(cmd) = self.commands.recv() => {
                    if self.cancellation_token.is_cancelled() {
                        debug!("Cancellation token triggered, exiting run loop");
                        self.stop()?;
                        break;
                    }

                    debug!("Received command: {cmd:?}");
                    let (cmd, completion) = match cmd {
                        PlayerCommand::WithCompletion { command, response } => {
                            let requires_playing = match command.as_ref() {
                                PlayerCommand::Play
                                | PlayerCommand::Next
                                | PlayerCommand::Previous
                                | PlayerCommand::Switch(_) => true,
                                PlayerCommand::LoadAtPosition { play, .. } => *play,
                                PlayerCommand::EditPlaylist(edit) => {
                                    edit.load.as_ref().is_some_and(|load| load.play)
                                        || (edit.load.is_none()
                                            && !edit.tracks.is_empty()
                                            && edit.placement == PlaylistPlacement::PlayNext
                                            && self.current_queue_edit_index().is_none())
                                }
                                _ => false,
                            };
                            (
                                *command,
                                Some(CommandCompletion {
                                    response,
                                    requires_playing,
                                }),
                            )
                        }
                        command => (command, None),
                    };
                    self.current_completion = completion;
                    match cmd {
                        PlayerCommand::WithCompletion { .. } => unreachable!(),
                        PlayerCommand::Restore { tracks, index, position, mode } => {
                            self.clear_unavailable(None)?;
                            self.add_to_playlist(tracks, AddMode::AppendToEnd);
                            self.debounce_timer = None;
                            self.send_playlist_updated()?;
                            self.set_playback_mode(mode)?;
                            self.load_at(index, false, false, Some(position))?;
                        },
                        PlayerCommand::RestoreTagged { entries, index, position, mode } => {
                            self.clear_unavailable(None)?;
                            self.playlist = entries
                                .into_iter()
                                .map(|entry| PlaylistItem {
                                    source: PlaybackSource::from(&entry.item),
                                    item: entry.item,
                                    block: entry.block,
                                })
                                .collect();
                            self.debounce_timer = None;
                            self.playback_strategy.on_playlist_updated(self.playlist.len(), UpdateReason::ClearPlaylist);
                            self.playback_strategy.on_playlist_updated(self.playlist.len(), UpdateReason::AddToPlaylist { mode: AddMode::AppendToEnd, index: None, count: self.playlist.len() });
                            self.send_playlist_updated()?;
                            self.set_playback_mode(mode)?;
                            self.load_at(index, false, false, Some(position))?;
                        },
                        PlayerCommand::Load { index } => {
                            if index < self.playlist.len() {
                                let mapped = self.get_mapped_track_index(index);
                                self.clear_unavailable(Some(self.playlist[mapped].item.clone()))?;
                            }
                            self.load(Some(index), false, true)?;
                        },
                        PlayerCommand::LoadAtPosition { index, position, play } => {
                            if index < self.playlist.len() {
                                self.clear_unavailable(Some(self.playlist[index].item.clone()))?;
                            }
                            self.load_at(Some(index), play, false, Some(position))?;
                        },
                        PlayerCommand::LoadComplete { generation, result, item, original_item, index, mapped, playlist_index, path } => {
                            if generation != self.load_generation {
                                debug!("Dropping stale load completion generation {generation}; current generation is {}", self.load_generation);
                                continue;
                            }
                            let play = self.play_when_loaded;
                            self.state = InternalPlaybackState::Stopped;
                            if let Some(pending) = self.pending_load.take() {
                                debug!("Load {index} completed after a newer request; starting index {}", pending.index);
                                self.complete_load_completions(Err(PlayerCommandError::Superseded));
                                self.active_load_completions = pending.completions;
                                self.load_at(Some(pending.index), pending.play, pending.mapped, pending.restore_position)?;
                                continue;
                            }
                            let result = match *result {
                                Ok(LoadedSource::Hosted(track)) => {
                                    self.check_host(&track).map(|()| LoadedSource::Hosted(track))
                                }
                                result => result,
                            };
                            match result {
                                Ok(source) => {
                                    if let Some(entry) = self.playlist.get_mut(playlist_index)
                                        && entry.item == original_item && entry.item != item
                                    {
                                        entry.item = item.clone();
                                        self.schedule_playlist_update();
                                    }
                                    let start = self.restore_position.take();
                                    let track = LoadedTrack { item, index, mapped, playlist_index, path };
                                    match source {
                                        LoadedSource::Hosted(hosted) if play => {
                                            self.start_hosted(hosted, track, start.unwrap_or_default())?;
                                        }
                                        // A hosted track that comes up paused waits as a local one
                                        // does, and the first play loads it again for its host.
                                        source => {
                                            self.release_host()?;
                                            match source {
                                                LoadedSource::Audio(source) if play => self.setup_sink(source, track, start)?,
                                                _ => {
                                                    self.loaded_paused(track, start.unwrap_or_default())?;
                                                    self.complete_ready_loads();
                                                }
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!("Failed to load track: {e:?}");
                                    self.restore_position = None;
                                    let reason = UnavailableReason::classify(&e);
                                    self.unavailable.insert(item.clone(), reason);
                                    self.event_sender.send(PlayerEvent::Unavailable(item, reason))?;
                                    self.complete_load_completions(Err(PlayerCommandError::SourceUnavailable));
                                    // Advance from the failed load, including when no track has loaded yet.
                                    let next = self.find_available_after(playlist_index);
                                    if let Some(next) = next { self.load(Some(next), play, true)?; }
                                    else { self.stop()?; self.event_sender.send(PlayerEvent::EndOfPlaylist)?; }
                                }
                            }
                        },
                        PlayerCommand::Play => {
                            // A play asked for during an interruption settles it: a late end of
                            // the interruption must not start playback again on its own.
                            self.resume_after_interruption = false;
                            let was_loading = self.state == InternalPlaybackState::Loading;
                            self.play()?;
                            if was_loading {
                                self.attach_current_to_active_load();
                            }
                        },
                        PlayerCommand::Pause => {
                            // A pause asked for during an interruption holds after it ends.
                            self.resume_after_interruption = false;
                            self.pause()?;
                        },
                        PlayerCommand::Stop => self.stop()?,
                        PlayerCommand::Next => self.next()?,
                        PlayerCommand::Previous => self.previous()?,
                        PlayerCommand::Switch(index) => {
                            if index >= self.playlist.len() {
                                self.complete_current(Err(PlayerCommandError::CannotApply));
                            } else {
                                self.switch(index)?;
                            }
                        }
                        PlayerCommand::Seek(position) => {
                            match self.seek(position)? {
                                SeekOutcome::Applied => {}
                                SeekOutcome::Deferred => self.attach_current_to_active_load(),
                                SeekOutcome::CannotApply => {
                                    self.complete_current(Err(PlayerCommandError::CannotApply));
                                }
                            }
                        }
                        PlayerCommand::AddToPlaylist { tracks, mode } => {
                            self.add_to_playlist(tracks, mode);
                        },
                        PlayerCommand::EditPlaylist(edit) => {
                            let add_mode = match edit.placement {
                                PlaylistPlacement::Replace | PlaylistPlacement::AppendToEnd => {
                                    AddMode::AppendToEnd
                                }
                                PlaylistPlacement::PlayNext => AddMode::PlayNext,
                            };
                            let insertion = match edit.placement {
                                PlaylistPlacement::Replace => {
                                    self.clear_playlist()?;
                                    0
                                }
                                PlaylistPlacement::AppendToEnd => self.playlist.len(),
                                PlaylistPlacement::PlayNext => self.current_queue_edit_index()
                                    .map_or(self.playlist.len(), |index| index + 1),
                            };
                            let added = !edit.tracks.is_empty();
                            if added {
                                self.add_to_playlist_tagged(edit.tracks, add_mode, edit.block);
                            }
                            if let Some(mode) = edit.playback_mode {
                                self.set_playback_mode(mode)?;
                            }
                            if let Some(load) = edit.load {
                                self.load_at(
                                    Some(insertion + load.start_index),
                                    load.play,
                                    false,
                                    Some(load.position),
                                )?;
                            } else if added
                                && edit.placement == PlaylistPlacement::PlayNext
                                && self.current_queue_edit_index().is_none()
                            {
                                self.load_at(Some(insertion), true, false, Some(Duration::ZERO))?;
                            }
                        },
                        PlayerCommand::HandoffLoad { tracks, block, start_index, position, play, playback_mode, response } => {
                            let interrupted = self.interrupted_item();
                            let current_index = self.current_queue_edit_index()
                                .and_then(|_| self.current_playlist_index());
                            let insertion = current_index.map_or(self.playlist.len(), |index| index + 1);
                            if let Some(index) = current_index {
                                self.current_track_index = Some(index);
                                self.current_track_mapped = false;
                            }
                            self.add_to_playlist_tagged(tracks, AddMode::PlayNext, Some(block));
                            if let Some(mode) = playback_mode {
                                self.set_playback_mode(mode)?;
                            }
                            if let Some(start_index) = start_index {
                                self.load_at(Some(insertion + start_index), play, false, Some(position))?;
                            }
                            let _ = response.send(HandoffLoadResult {
                                interrupted,
                                items: self.playlist.iter().map(queue_entry).collect(),
                            });
                        },
                        PlayerCommand::TakeBackBlock { block, interrupted, response } => {
                            if !self.playlist.iter().any(|item| item.block == Some(block)) {
                                let _ = response.send(None);
                                continue;
                            }
                            let current_index = self.current_playlist_index();
                            let current_item = self.current_item.clone();
                            let was_playing = self.state == InternalPlaybackState::Playing
                                || (self.state == InternalPlaybackState::Loading && self.play_when_loaded);
                            let was_inside = self.state != InternalPlaybackState::Stopped
                                && current_item.is_some()
                                && current_index
                                .and_then(|index| self.playlist.get(index))
                                .is_some_and(|item| item.block == Some(block));
                            let block_items = self.playlist.iter()
                                .filter(|item| item.block == Some(block))
                                .map(queue_entry)
                                .collect::<Vec<_>>();
                            let block_index = if was_inside {
                                current_index.map(|index| self.playlist[..index]
                                    .iter().filter(|item| item.block == Some(block)).count())
                            } else {
                                None
                            };
                            let position = if was_inside { self.current_position().unwrap_or_default() } else { Duration::ZERO };
                            let indices = self.playlist.iter().enumerate()
                                .filter_map(|(index, item)| (item.block == Some(block)).then_some(index))
                                .collect::<Vec<_>>();
                            for index in indices.into_iter().rev() {
                                self.remove_from_playlist(index)?;
                            }
                            if was_inside {
                                match interrupted {
                                    InterruptedItem::Nothing => self.stop()?,
                                    InterruptedItem::Item { index_hint, item, position } => {
                                        let index = self.playlist.iter().position(|entry| entry.item == item)
                                            .unwrap_or_else(|| index_hint.min(self.playlist.len().saturating_sub(1)));
                                        if self.playlist.is_empty() {
                                            self.stop()?;
                                        } else {
                                            self.load_at(Some(index), false, false, Some(position))?;
                                        }
                                    }
                                }
                            } else if let Some(item) = current_item {
                                self.current_track_index = self.playlist.iter()
                                    .position(|entry| entry.item == item);
                                self.current_track_mapped = false;
                            }
                            let _ = response.send(Some(TakeBackBlockResult {
                                items: block_items,
                                remaining: self.playlist.iter().map(queue_entry).collect(),
                                index: block_index,
                                position,
                                playback_mode: self.playback_mode,
                                playing: was_playing,
                            }));
                        },
                        PlayerCommand::ClearHandoffBlocks { response } => {
                            let mut changed = false;
                            for item in &mut self.playlist {
                                changed |= item.block.take().is_some();
                            }
                            if changed {
                                self.schedule_playlist_update();
                            }
                            let _ = response.send(ClearHandoffBlocksResult {
                                changed,
                                items: self.playlist.iter().map(queue_entry).collect(),
                            });
                        },
                        PlayerCommand::RemoveFromPlaylist { index } => self.remove_from_playlist(index)?,
                        PlayerCommand::ClearPlaylist => self.clear_playlist()?,
                        PlayerCommand::MovePlayListItem {old_index, new_index} => {
                            self.move_playlist_item(old_index, new_index);
                        },
                        PlayerCommand::SetPlaybackMode(mode) => self.set_playback_mode(mode)?,
                        PlayerCommand::SetVolume(volume) => self.set_volume(volume)?,
                        PlayerCommand::SetRealtimeFFTEnabled(enabled) => self.set_realtime_fft_enabled(enabled)?,
                        PlayerCommand::SetAdaptiveSwitchingEnabled(enabled) => self.set_adaptive_switching(enabled)?,
                        PlayerCommand::SetMixWithOthers(enabled) => self.set_mix_with_others(enabled)?,
                        PlayerCommand::StatusAfterPending(response) => {
                            if self.debounce_timer.take().is_some() {
                                self.send_playlist_updated()?;
                            }
                            self.event_sender
                                .send(PlayerEvent::StatusAfterPending(response))
                                .context("Failed to send player status barrier")?;
                        },
                        PlayerCommand::Output(event) => self.output_event(event)?,
                        PlayerCommand::Hosted(event) => self.host_event(event)?,
                        PlayerCommand::HostNextResolved { request, track } => {
                            self.host_next_resolved(request, track);
                        }
                    };
                    // Whatever the command changed, the host holds the track that follows.
                    self.sync_host_next();
                    self.complete_current(Ok(()));
                },
                Ok(fft_data) = fft_receiver.recv() => {
                    if let Err(e) = self.event_sender.send(PlayerEvent::RealtimeFFT(fft_data)) {
                        error!("Failed to send FFT data: {e:?}");
                    }
                },
                _ = progress_interval.tick() => {
                    if self.state != InternalPlaybackState::Stopped {
                        self.send_progress()?;
                    }
                },
                _ = async {
                    if let Some(timer) = self.debounce_timer {
                        sleep_until(timer).await;
                        true
                    } else {
                        false
                    }
                }, if self.debounce_timer.is_some() => {
                    self.debounce_timer = None;
                    self.send_playlist_updated()?;
                },
                Some(error_message) = self.stream_error_receiver.recv() => {
                    self.stream_failed(&error_message)?;
                },
                _ = self.cancellation_token.cancelled() => {
                    debug!("Cancellation token triggered, exiting run loop");
                    self.stop()?;
                    break;
                }
            }
        }

        Ok(())
    }

    fn load(&mut self, index: Option<usize>, play: bool, mapped: bool) -> Result<()> {
        self.load_at(index, play, mapped, None)
    }

    // Loads a track that seeks to `restore_position` once it is ready. The position belongs to
    // this request only: a newer request replaces it, whether it starts now or is queued.
    fn load_at(
        &mut self,
        index: Option<usize>,
        play: bool,
        mapped: bool,
        restore_position: Option<Duration>,
    ) -> Result<()> {
        if self.state == InternalPlaybackState::Loading {
            if let Some(index) = index {
                debug!("Load {index} queued while another track is loading; newest request wins");
                self.complete_load_completions(Err(PlayerCommandError::Superseded));
                if let Some(previous) = self.pending_load.take() {
                    Self::complete_completions(
                        previous.completions,
                        Err(PlayerCommandError::Superseded),
                    );
                }
                self.pending_load = Some(PendingLoad {
                    index,
                    play,
                    mapped,
                    restore_position,
                    completions: self.current_completion.take().into_iter().collect(),
                });
            }
            return Ok(());
        }

        if let Some(index) = index {
            self.state = InternalPlaybackState::Loading;

            let mapped_index = if mapped {
                self.get_mapped_track_index(index)
            } else {
                index
            };

            if mapped_index >= self.playlist.len() {
                warn!("Index {mapped_index} exceed the boundary");
                self.state = InternalPlaybackState::Stopped;
                self.complete_current(Err(PlayerCommandError::CannotApply));
                self.complete_load_completions(Err(PlayerCommandError::CannotApply));
                return Ok(());
            }

            self.load_generation = self.load_generation.wrapping_add(1);
            let generation = self.load_generation;
            self.play_when_loaded = play;
            self.restore_position = restore_position;
            self.loading_index = Some(mapped_index);
            if let Some(completion) = self.current_completion.take() {
                self.active_load_completions.push(completion);
            }

            let item = self.playlist[mapped_index].clone();
            debug!("Starting track load at playlist index {mapped_index} (request index {index})");
            let commands_sender = self.commands_sender.clone();
            let fsio = Arc::clone(&self.fsio);
            let resolver = match self.source_resolver.read() {
                Ok(slot) => slot.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            };
            let load_cancel = self.cancellation_token.clone();

            tokio::spawn(async move {
                let original_item = item.item.clone();
                let source_future = async {
                    let mut fsio = fsio;
                    let mut resolver = resolver;
                    let mut remote_storage = false;
                    let library_track_key =
                        if let (PlayingItem::LibraryTrack { library_id, track }, Some(resolver)) =
                            (&item.item, resolver.as_ref())
                        {
                            Some(
                                resolver
                                    .resolve_track(library_id.to_string(), track.clone())
                                    .await?,
                            )
                        } else {
                            None
                        };
                    if matches!(library_track_key, Some(LibraryTrackKey::Rune(_)))
                        && let (PlayingItem::LibraryTrack { library_id, .. }, Some(root)) =
                            (&item.item, resolver.as_ref())
                        && let Some(library) = root.for_library(library_id.to_string()).await?
                    {
                        remote_storage = library.fsio.name() == "OpenDAL";
                        fsio = library.fsio;
                        resolver = Some(library.resolver);
                    }
                    let local_file_id =
                        if let (Some(LibraryTrackKey::Rune(file_uuid)), Some(resolver)) =
                            (&library_track_key, resolver.as_ref())
                        {
                            debug!("Resolving source identity for track load {index}");
                            Some(
                                resolver
                                    .resolve_id(*file_uuid)
                                    .await?
                                    .context("library track no longer exists")?,
                            )
                        } else {
                            None
                        };
                    let resolved_item = item.item.clone();
                    match &resolved_item {
                        PlayingItem::IndependentFile(_) => {
                            let path = match &item.source {
                                PlaybackSource::Local(path) => path.clone(),
                                PlaybackSource::LibraryStream => {
                                    bail!("library stream resolver unavailable")
                                }
                            };
                            let source = open_local_source(fsio, path.clone(), remote_storage)
                                .await
                                .with_context(|| format!("Failed to open file: {path:?}"))?;
                            Ok((source.into(), path, resolved_item.clone()))
                        }
                        PlayingItem::LibraryTrack { .. }
                            if matches!(library_track_key, Some(LibraryTrackKey::Rune(_))) =>
                        {
                            let path = if let (Some(id), Some(resolver)) =
                                (local_file_id, resolver.as_ref())
                            {
                                let requires_stream = item
                                    .source
                                    .requires_stream_at_load(id, resolver.as_ref())
                                    .await?;
                                debug!(
                                    "Track load {index} requires remote streaming: {requires_stream}"
                                );
                                if requires_stream {
                                    let (source, path) = open_library_stream(resolver, id).await?;
                                    return Ok((source.into(), path, resolved_item.clone()));
                                }
                                resolver
                                    .local_path(id)
                                    .await?
                                    .context("current local audio is unavailable")?
                            } else {
                                match &item.source {
                                    PlaybackSource::Local(path) => path.clone(),
                                    PlaybackSource::LibraryStream => {
                                        bail!("library stream resolver unavailable")
                                    }
                                }
                            };
                            let source = open_local_source(fsio, path.clone(), remote_storage)
                                .await
                                .with_context(|| format!("Failed to open file: {path:?}"))?;
                            Ok((source.into(), path, resolved_item.clone()))
                        }
                        PlayingItem::LibraryTrack { library_id, .. }
                            if matches!(library_track_key, Some(LibraryTrackKey::Service(_))) =>
                        {
                            let resolver = resolver
                                .as_ref()
                                .context("Service playback resolver unavailable")?;
                            let item = ServiceItem {
                                library_id: *library_id,
                                track: match library_track_key.as_ref() {
                                    Some(LibraryTrackKey::Service(track)) => track.clone(),
                                    _ => bail!("Service track key unavailable"),
                                },
                            };
                            match resolver.resolve_service(item).await? {
                                ServiceTarget::Stream(remote) => {
                                    let (source, path) = open_service_stream(remote).await?;
                                    Ok((source.into(), path, resolved_item.clone()))
                                }
                                ServiceTarget::Hosted(track) => {
                                    let path = track.path();
                                    Ok((LoadedSource::Hosted(track), path, resolved_item.clone()))
                                }
                            }
                        }
                        PlayingItem::LibraryTrack { .. } => {
                            bail!("library track resolver unavailable")
                        }
                        PlayingItem::Online(url, _) => {
                            info!("Downloading from url: {url}");
                            let ext = url.split('?').next().and_then(|u| u.rsplit('.').next());
                            let reader = crate::stream_utils::create_stream_from_url(url).await?;
                            let failed = Arc::new(Mutex::new(None));
                            let reader = ManagedStream::new_with_failure(
                                reader,
                                CancellationToken::new(),
                                failed.clone(),
                            );
                            let decoder =
                                probe_decoder(reader, ext.map(str::to_owned), None).await?;
                            Ok((
                                AnySource::Online(rune_buffered(decoder), failed).into(),
                                PathBuf::new(),
                                resolved_item.clone(),
                            ))
                        }
                        PlayingItem::Unknown => {
                            bail!("Cannot load unknown item");
                        }
                    }
                };
                let source_result = tokio::select! {
                    _ = load_cancel.cancelled() => Err(anyhow!("library closed")),
                    result = bound_source_start(SOURCE_START_TIMEOUT, source_future) => result,
                };

                debug!(
                    "Track load {index} source resolution finished: {}",
                    if source_result.is_ok() {
                        "ready"
                    } else {
                        "failed"
                    }
                );

                let (source_result, path, resolved_item) = match source_result {
                    Ok((source, path, resolved_item)) => (Ok(source), path, resolved_item),
                    Err(error) => (Err(error), PathBuf::new(), item.item),
                };
                let cmd = PlayerCommand::LoadComplete {
                    generation,
                    result: Box::new(source_result),
                    item: resolved_item,
                    original_item,
                    index,
                    mapped,
                    playlist_index: mapped_index,
                    path,
                };
                if commands_sender.send(cmd).is_err() {
                    error!("Failed to send LoadComplete command");
                }
            });
        }
        Ok(())
    }

    fn complete_current(&mut self, result: std::result::Result<(), PlayerCommandError>) {
        if let Some(completion) = self.current_completion.take() {
            let _ = completion.response.send(result);
        }
    }

    fn complete_load_completions(&mut self, result: std::result::Result<(), PlayerCommandError>) {
        Self::complete_completions(std::mem::take(&mut self.active_load_completions), result);
    }

    fn complete_ready_loads(&mut self) {
        let completions = std::mem::take(&mut self.active_load_completions);
        for completion in completions {
            let result = if completion.requires_playing {
                Err(PlayerCommandError::Superseded)
            } else {
                Ok(())
            };
            let _ = completion.response.send(result);
        }
    }

    fn complete_output_failure(&mut self) {
        let completions = std::mem::take(&mut self.active_load_completions);
        for completion in completions {
            let result = if completion.requires_playing {
                Err(PlayerCommandError::OutputUnavailable)
            } else {
                Ok(())
            };
            let _ = completion.response.send(result);
        }
    }

    fn complete_completions(
        completions: Vec<CommandCompletion>,
        result: std::result::Result<(), PlayerCommandError>,
    ) {
        for completion in completions {
            let _ = completion.response.send(result);
        }
    }

    fn attach_current_to_active_load(&mut self) {
        if let Some(completion) = self.current_completion.take() {
            self.active_load_completions.push(completion);
        }
    }

    fn open_output(&self) -> Result<(RuneOutputStream, RuneOutputStreamHandle)> {
        #[cfg(test)]
        {
            self.output_opens.fetch_add(1, Ordering::SeqCst);
            if self
                .failing_opens
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                bail!("the test output refused to open");
            }
        }
        // Whatever asked to play, the session is set up and active again before a stream starts:
        // an interruption or a reset of the media services leaves it inactive. A session that
        // cannot be activated, as during a call, fails the open, and the track waits paused for
        // the next play.
        #[cfg(target_os = "ios")]
        if !crate::ios_session::activate(self.mix_with_others) {
            bail!("the audio session could not be activated");
        }
        RuneOutputStream::try_default_with_callback({
            let error_sender = self.stream_error_sender.clone();
            move |error| {
                let _ = error_sender.send(error.to_string());
            }
        })
        .context("Failed to create output stream")
    }

    // Opens an output for a loaded track and plays it from `start`. Only playback opens an
    // output; a track that comes up paused waits without one, as does one whose output cannot
    // open.
    fn setup_sink(
        &mut self,
        source: AnySource,
        track: LoadedTrack,
        start: Option<Duration>,
    ) -> Result<()> {
        let output = self.open_output().and_then(|(stream, stream_handle)| {
            let sink = try_new_sink(&stream_handle).context("Failed to create sink")?;
            Ok((stream, sink))
        });
        let (stream, sink) = match output {
            Ok(output) => output,
            Err(e) => {
                self.output_failed(e, track, start.unwrap_or_default())?;
                self.complete_output_failure();
                return Ok(());
            }
        };
        self.output_stale = false;
        // The new output is the end of any interruption, so a late end must not start playback
        // again on its own.
        self.resume_after_interruption = false;
        self.pending_seek = None;

        self.active_stream_failure = source.stream_failure();
        let source = SharedSource::new(source);
        let source_for_fft = Arc::clone(&source.inner);

        let (fft_tx, mut fft_rx) = mpsc::unbounded_channel();

        let realtime_fft = Arc::clone(&self.realtime_fft);
        let fft_enabled = Arc::clone(&self.fft_enabled);
        tokio::spawn(async move {
            while let Some(data) = fft_rx.recv().await {
                if let Ok(enabled) = fft_enabled.lock()
                    && *enabled
                    && let Ok(fft) = realtime_fft.lock()
                {
                    fft.add_data(data);
                }
            }
        });

        sink.set_volume(self.volume);
        let source = source.periodic_access(
            Duration::from_millis(12),
            move |_sample: &mut SharedSource<_>| {
                if let Ok(guard) = source_for_fft.lock() {
                    let data: Option<Vec<i16>> = guard.current_samples();
                    if let Some(data) = data
                        && fft_tx.send(data).is_err()
                    {
                        error!("Failed to send FFT data");
                    }
                }
            },
        );
        // Hold the track until it stands where it starts, so that nothing before that is heard.
        sink.pause();
        sink.append(self.pcm_tap.wrap(source));
        if let Some(start) = start
            && let Err(e) = sink.try_seek(start)
        {
            error!("Failed to seek to the start position {start:?}: {e:#?}");
        }
        sink.play();
        let position = sink.get_pos();

        self.sink = Some(sink);
        self._stream = Some(stream);
        let LoadedTrack {
            item,
            index,
            mapped,
            playlist_index,
            path,
        } = track;
        self.current_track_index = Some(index);
        self.current_track_mapped = mapped;
        self.current_item = Some(item.clone());
        self.current_track_path = Some(path.clone());
        info!("Track loaded: {path:?}");

        self.event_sender
            .send(PlayerEvent::Playing {
                item,
                index: playlist_index,
                path,
                playback_mode: self.playback_mode,
                position,
            })
            .context("Failed to send Playing event")?;
        self.state = InternalPlaybackState::Playing;
        self.complete_load_completions(Ok(()));

        Ok(())
    }

    // A track that came up paused stays current without an output, as `Loaded` at `position`.
    // Opening one now would take the audio from other apps on iOS and keep an idle stream open
    // elsewhere. The first play loads the track again and opens an output there. `Loaded` also
    // tells the UI that the track is ready to play.
    fn loaded_paused(&mut self, track: LoadedTrack, position: Duration) -> Result<()> {
        info!("Track loaded paused: {:?}", track.path);
        let LoadedTrack {
            item,
            index,
            mapped,
            playlist_index,
            path,
        } = track;
        self.keep_without_output(item.clone(), index, mapped, path.clone(), position);
        self.event_sender
            .send(PlayerEvent::Loaded {
                item,
                index: playlist_index,
                path,
                playback_mode: self.playback_mode,
                position,
            })
            .context("Failed to send Loaded event")?;
        Ok(())
    }

    // Keeps a track current with no output, paused where it stands, and the next play opens one
    // there. Stopped inside, so that no progress is reported without a sink.
    fn keep_without_output(
        &mut self,
        item: PlayingItem,
        index: usize,
        mapped: bool,
        path: PathBuf,
        position: Duration,
    ) {
        self.sink = None;
        self._stream = None;
        self.active_stream_failure = None;
        self.output_stale = true;
        self.restore_position = Some(position);
        self.pending_seek = None;
        self.current_track_index = Some(index);
        self.current_track_mapped = mapped;
        self.current_item = Some(item);
        self.current_track_path = Some(path);
        self.state = InternalPlaybackState::Stopped;
    }

    fn play(&mut self) -> Result<()> {
        // While a track loads, the previous track keeps the sink. It follows Play and Pause, but
        // the state stays Loading so that newer requests still queue behind the load.
        let loading = self.state == InternalPlaybackState::Loading;
        if loading {
            self.set_play_when_loaded(true);
        }
        if self.hosted.is_some() {
            return self.set_hosted_playing(true, loading);
        }

        if !loading && self.output_stale && self.current_track_index.is_some() {
            return self.reopen_output();
        }

        // The output stopped while paused, and starts again before the sink plays. One that cannot
        // start is of no more use: a loaded track opens a new one where it stands, and a track that
        // is loading opens its own.
        if self.output_suspended()
            && let Err(e) = self.resume_output()
        {
            error!("Failed to start the audio output again: {e:?}");
            self.mark_output_stale();
            return if loading {
                Ok(())
            } else {
                self.reopen_output()
            };
        }

        let Some(sink) = &self.sink else {
            if !loading {
                info!("Loading the first track");
                self.load(Some(0), true, true)?;
            }
            return Ok(());
        };
        // A seek that came while the output was stopped goes through now that the output runs. A
        // stale output never runs again, and the track that replaces it starts at the place.
        if !self.output_stale
            && let Some(position) = self.pending_seek.take()
            && let Err(e) = sink.try_seek(position)
        {
            error!("Failed to seek to {position:?}: {e:#?}");
        }
        sink.play();
        info!("Playback started");

        if self.current_track_index.is_some() {
            let position = self.current_position().unwrap_or_default();
            self.send_track_state(true, position)?;
            if !loading {
                self.state = InternalPlaybackState::Playing;
            }
        }

        Ok(())
    }

    fn pause(&mut self) -> Result<()> {
        let loading = self.state == InternalPlaybackState::Loading;
        if loading {
            self.set_play_when_loaded(false);
        }
        if self.hosted.is_some() {
            return self.set_hosted_playing(false, loading);
        }

        if let Some(sink) = &self.sink {
            sink.pause();
            self.suspend_output();
            info!("Playback paused");

            if self.current_track_index.is_some() {
                let position = self.current_position().unwrap_or_default();
                self.send_track_state(false, position)?;
                if !loading {
                    self.state = InternalPlaybackState::Paused;
                }
            }
        }

        Ok(())
    }

    // Reports the current track as playing or paused at `position`. Without a current track
    // nothing is reported.
    fn send_track_state(&self, playing: bool, position: Duration) -> Result<()> {
        let (Some(item), Some(index), Some(path)) = (
            self.current_item.clone(),
            self.current_playlist_index(),
            self.current_track_path.clone(),
        ) else {
            return Ok(());
        };
        let playback_mode = self.playback_mode;
        let event = if playing {
            PlayerEvent::Playing {
                item,
                index,
                path,
                playback_mode,
                position,
            }
        } else {
            PlayerEvent::Paused {
                item,
                index,
                path,
                playback_mode,
                position,
            }
        };
        self.event_sender
            .send(event)
            .context("Failed to send the state of the track")?;
        Ok(())
    }

    // Decides whether the track that is loading, and a newer one queued behind it, comes up
    // playing or paused.
    fn set_play_when_loaded(&mut self, play: bool) {
        self.play_when_loaded = play;
        if let Some(pending) = &mut self.pending_load {
            pending.play = play;
        }
    }

    fn output_event(&mut self, event: OutputEvent) -> Result<()> {
        info!("Audio output event: {event:?}");
        // The host of a hosted track owns its audio session, unless a track that loads takes over.
        if self.hosted.is_some() && self.state != InternalPlaybackState::Loading {
            return Ok(());
        }
        match event {
            OutputEvent::Interrupted => {
                // The system stopped the output, and pausing stops it on this side too. The same
                // output starts again on the next play, so an interruption does not make it stale.
                // Playback goes on after the interruption only if it was wanted; anything else
                // that was heard, such as the track before one that loads paused, only pauses.
                let wanted = match self.state {
                    InternalPlaybackState::Playing => true,
                    InternalPlaybackState::Loading => self.play_when_loaded,
                    _ => false,
                };
                let heard = self.sink.as_ref().is_some_and(|sink| !sink.is_paused());
                if wanted || heard {
                    self.pause()?;
                }
                // Only a stream that stopped with the pause can start again: one that runs on
                // after the system stopped its unit plays nothing, so it gives way to a new one.
                if self
                    ._stream
                    .as_ref()
                    .is_some_and(|stream| !stream.is_suspended())
                {
                    self.mark_output_stale();
                }
                if wanted {
                    self.resume_after_interruption = true;
                }
            }
            OutputEvent::InterruptionEnded { resume } => {
                if std::mem::take(&mut self.resume_after_interruption) && resume {
                    self.play()?;
                }
            }
            OutputEvent::DeviceLost => self.output_lost()?,
            OutputEvent::DeviceAdded => {
                self.mark_output_stale();
                // Move what is playing to the new device now. A track that is loading opens its
                // stream there anyway, and a paused one does on the next play.
                if self.state == InternalPlaybackState::Playing {
                    self.reopen_output()?;
                }
            }
        }
        Ok(())
    }

    // The output can play no more, as when its device went away. Playback pauses where it was,
    // nothing resumes on its own, and the next play opens a new stream there.
    fn output_lost(&mut self) -> Result<()> {
        self.pause()?;
        self.resume_after_interruption = false;
        self.mark_output_stale();
        Ok(())
    }

    // The output stream reported an error, such as headphones that were disconnected. The track
    // pauses in place rather than playing on through another device. A stream that fails tends
    // to report it more than once, and a stale stream is done with already.
    fn stream_failed(&mut self, error: &str) -> Result<()> {
        error!("The audio output failed: {error}");
        if self.sink.is_none() || self.output_stale {
            return Ok(());
        }
        self.output_lost()
    }

    fn mark_output_stale(&mut self) {
        if self.sink.is_some() {
            self.output_stale = true;
        }
    }

    // Stops the output while nothing plays from it, which only iOS needs: it shows the player as
    // playing for as long as its output runs. See `RuneOutputStream::suspend`.
    fn suspend_output(&self) {
        let Some(stream) = &self._stream else {
            return;
        };
        if stream.is_suspended() {
            return;
        }
        if let Err(e) = stream.suspend() {
            warn!("Failed to stop the audio output while nothing plays: {e:?}");
        }
        #[cfg(test)]
        if stream.is_suspended() {
            self.output_suspends.fetch_add(1, Ordering::SeqCst);
        }
    }

    // Whether the sink's output is stopped and can start again. A stale output is not started:
    // the next play opens a new one.
    fn output_suspended(&self) -> bool {
        self.sink.is_some()
            && !self.output_stale
            && self
                ._stream
                .as_ref()
                .is_some_and(RuneOutputStream::is_suspended)
    }

    // Starts a stopped output again, after a pause or an interruption. The session is made active
    // first, as before any output starts: an interruption leaves it inactive, and a suspended app
    // may wake to a remote command before the end of the interruption reaches it.
    fn resume_output(&self) -> Result<()> {
        let Some(stream) = &self._stream else {
            return Ok(());
        };
        #[cfg(target_os = "ios")]
        if !crate::ios_session::activate(self.mix_with_others) {
            bail!("The audio session could not be made active");
        }
        #[cfg(test)]
        {
            self.output_resumes.fetch_add(1, Ordering::SeqCst);
            if self
                .failing_resumes
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                bail!("the test output refused to start again");
            }
        }
        stream
            .resume()
            .context("Failed to start the output stream again")
    }

    // Opens a new output stream for the current track and plays on from where it stands. It loads
    // the track again, so the old stream is never used for a seek, which would wait for a stream
    // that no longer runs.
    fn reopen_output(&mut self) -> Result<()> {
        if self.current_track_index.is_none() {
            return Ok(());
        }
        let position = self.current_position().unwrap_or_default();
        // The load starts at the place, so the old sink no longer keeps a seek of its own.
        self.pending_seek = None;
        if let Some(sink) = &self.sink {
            sink.pause();
        }
        let Some((index, mapped)) = self.current_track_request() else {
            // Nothing is left to load, and a track shown as paused would never play.
            warn!("The current track is no longer in the playlist, so playback stops");
            return self.stop();
        };
        info!("Opening the audio output again at {position:?}");
        self.load_at(Some(index), true, mapped, Some(position))
    }

    // How to load the current track again: the way it was loaded, while that still leads to it,
    // or else by where it is in the playlist now. Removing tracks before it moves it, and the
    // shuffled order is drawn again. None once it left the playlist.
    fn current_track_request(&self) -> Option<(usize, bool)> {
        let item = self.current_item.as_ref()?;
        let holds_item = |position: Option<usize>| {
            position
                .and_then(|position| self.playlist.get(position))
                .is_some_and(|entry| &entry.item == item)
        };
        if let Some(index) = self.current_track_index
            && holds_item(self.resolve_index(index, self.current_track_mapped))
        {
            return Some((index, self.current_track_mapped));
        }
        self.playlist
            .iter()
            .position(|entry| &entry.item == item)
            .map(|position| (position, false))
    }

    // The playlist position that a load of `index` leads to: the playback mode maps an index
    // asked for as `mapped`, and any other index is a position already. None past the end of
    // the playlist.
    fn resolve_index(&self, index: usize, mapped: bool) -> Option<usize> {
        if index >= self.playlist.len() {
            return None;
        }
        let position = if mapped {
            self.get_mapped_track_index(index)
        } else {
            index
        };
        (position < self.playlist.len()).then_some(position)
    }

    // Where the current track stands in the playlist, which the events report. An index left out
    // of range by removed tracks is reported as it is.
    fn current_playlist_index(&self) -> Option<usize> {
        let index = self.current_track_index?;
        Some(
            self.resolve_index(index, self.current_track_mapped)
                .unwrap_or(index),
        )
    }

    // Keeps the player running when no output stream could be opened, for example while another
    // app holds the audio. The track stays current and paused where it was, ready to play, and
    // the next play tries again from there.
    fn output_failed(
        &mut self,
        error: anyhow::Error,
        track: LoadedTrack,
        position: Duration,
    ) -> Result<()> {
        error!("Failed to open the audio output: {error:?}");
        self.loaded_paused(track, position)
    }

    fn stop(&mut self) -> Result<()> {
        self.load_generation = self.load_generation.wrapping_add(1);
        if let Some(pending) = self.pending_load.take() {
            Self::complete_completions(pending.completions, Err(PlayerCommandError::Superseded));
        }
        self.complete_load_completions(Err(PlayerCommandError::Superseded));
        self.state = InternalPlaybackState::Stopped;
        // The output plays nothing more: the next track opens one of its own.
        self.suspend_output();
        let hosted = self.release_host()?;
        if let Some(sink) = self.sink.take() {
            sink.stop();
            info!("Playback stopped");
            self.event_sender
                .send(PlayerEvent::Stopped)
                .with_context(|| "Failed to send Stopped event")?;
            self.state = InternalPlaybackState::Stopped;
        } else if self.output_stale || hosted {
            // A track whose output failed to open still shows as paused, and a hosted track has
            // no sink.
            self.event_sender
                .send(PlayerEvent::Stopped)
                .with_context(|| "Failed to send Stopped event")?;
        } else {
            warn!("Stop command received but no track is loaded");
        }
        self.active_stream_failure = None;
        self.output_stale = false;
        self.resume_after_interruption = false;
        self.restore_position = None;
        self.pending_seek = None;
        Ok(())
    }

    fn next(&mut self) -> Result<()> {
        if self.skip_to_host_next()? {
            return Ok(());
        }
        self.next_with(true)
    }

    // Loads the next track, playing or paused.
    fn next_with(&mut self, play: bool) -> Result<()> {
        if let Some(index) = self.current_track_index {
            if let Some(next_index) = self.playback_strategy.next(index, self.playlist.len()) {
                self.load(Some(next_index), play, true)?;
            } else {
                info!("End of playlist reached");
                self.event_sender
                    .send(PlayerEvent::EndOfPlaylist)
                    .with_context(|| "Failed to send EndOfPlaylist event")?;

                if let Some(start_index) =
                    self.playback_strategy.on_playlist_end(self.playlist.len())
                {
                    self.load(Some(start_index), false, true)?;
                } else {
                    self.stop()?;
                }
            }
        }

        Ok(())
    }

    fn do_switch_back(&mut self, index: usize) -> Result<()> {
        if let Some(prev_index) = self.playback_strategy.previous(index, self.playlist.len()) {
            self.load(Some(prev_index), true, true)?;
        } else {
            info!("Beginning of playlist reached");
        }

        Ok(())
    }

    fn previous(&mut self) -> Result<()> {
        if let Some(index) = self.current_track_index {
            match self.current_position() {
                Some(position) => {
                    let need_adaptive = position > Duration::from_secs(3);

                    if self.adaptive_switching && need_adaptive {
                        self.load(Some(index), true, true)?;
                    } else {
                        return self.do_switch_back(index);
                    }
                    return Ok(());
                }
                None => return self.do_switch_back(index),
            }
        }

        Ok(())
    }

    // Where the current track stands: where a seek put it while its output was stopped, where it
    // starts once an output opens while it has none that plays, or else where its sink is.
    fn current_position(&self) -> Option<Duration> {
        if let Some(hosted) = &self.hosted {
            return Some(hosted.position());
        }
        if let Some(position) = self.pending_seek {
            return Some(position);
        }
        if self.output_stale
            && let Some(position) = self.restore_position
        {
            return Some(position);
        }
        match &self.sink {
            Some(sink) => Some(sink.get_pos()),
            None if self.output_stale => Some(Duration::ZERO),
            None => None,
        }
    }

    fn switch(&mut self, index: usize) -> Result<()> {
        if index < self.playlist.len() {
            debug!("Switching to track at index: {}", { index });
            self.clear_unavailable(Some(self.playlist[index].item.clone()))?;
            self.load(Some(index), true, false)?;
        } else {
            warn!("Switch command received but index {index} is out of bounds");
        }

        Ok(())
    }

    fn seek(&mut self, seconds: f64) -> Result<SeekOutcome> {
        let Some(position) = seek_position(seconds) else {
            warn!("Seek to {seconds} s ignored: it is no place in a track");
            return Ok(SeekOutcome::CannotApply);
        };
        if self.hosted.is_some() {
            return self.seek_hosted(position);
        }

        if self.output_stale {
            // A stale stream never takes the seek, and waiting for it would stall the player.
            if self.state == InternalPlaybackState::Loading {
                // The load in flight brings a new output. If it brings the current track back, as
                // a reopen does, it starts at the new place.
                if !self.move_reload_to(position) {
                    warn!("Seek ignored while another track loads to replace the audio output");
                    return Ok(SeekOutcome::CannotApply);
                }
                return Ok(SeekOutcome::Deferred);
            }
            // Keep the place instead, and the next play opens a new stream there.
            self.pending_seek = None;
            if self.current_track_index.is_some() {
                self.restore_position = Some(position);
                self.send_track_state(false, position)?;
                return Ok(SeekOutcome::Applied);
            }
            return Ok(SeekOutcome::CannotApply);
        }

        if self.output_suspended() {
            // Seeking on a stopped output would wait forever. The track keeps the place, and the
            // next play seeks there once the output runs again.
            self.pending_seek = Some(position);
            self.send_track_state(false, position)?;
            return Ok(SeekOutcome::Applied);
        }

        let Some(sink) = &self.sink else {
            warn!("Seek command received but no track is loaded");
            return Ok(SeekOutcome::CannotApply);
        };
        if let Err(e) = sink.try_seek(position) {
            error!("Failed to seek: {e:#?}");
            return Ok(SeekOutcome::CannotApply);
        }
        info!("Seeking to position: {seconds} s");
        let position = sink.get_pos();
        self.send_track_state(self.state == InternalPlaybackState::Playing, position)?;
        Ok(SeekOutcome::Applied)
    }

    // Makes a load in flight that brings the current track back, such as a reopen, start at
    // `position`. A newer load queued behind it takes over once it completes, so that one moves
    // instead. Returns whether the load is of the current track.
    fn move_reload_to(&mut self, position: Duration) -> bool {
        let target = match self.pending_load {
            Some(ref pending) => self.resolve_index(pending.index, pending.mapped),
            None => self.loading_index,
        };
        let reloads_current = target
            .and_then(|target| self.playlist.get(target))
            .is_some_and(|entry| Some(&entry.item) == self.current_item.as_ref());
        if !reloads_current {
            return false;
        }
        match &mut self.pending_load {
            Some(pending) => pending.restore_position = Some(position),
            None => self.restore_position = Some(position),
        }
        true
    }

    fn add_to_playlist(&mut self, tracks: Vec<(PlayingItem, PlaybackSource)>, mode: AddMode) {
        self.add_to_playlist_tagged(tracks, mode, None);
    }

    fn add_to_playlist_tagged(
        &mut self,
        tracks: Vec<(PlayingItem, PlaybackSource)>,
        mode: AddMode,
        requested_block: Option<HandoffBlockId>,
    ) {
        debug!("Adding tracks to playlist with mode: {:?}", { mode });
        let count = tracks.len();
        let insert_index = match mode {
            AddMode::PlayNext => {
                if let Some(current_index) = self.current_queue_edit_index() {
                    Some(current_index + 1)
                } else {
                    Some(self.playlist.len())
                }
            }
            AddMode::AppendToEnd => None,
        };

        let inherited_index = insert_index.unwrap_or(self.playlist.len());
        let current_item = (self.state != InternalPlaybackState::Stopped)
            .then_some(self.current_item.as_ref())
            .flatten();
        let inherited_block =
            inherited_block_for_insert(&self.playlist, inherited_index, current_item);
        let block = requested_block.or(inherited_block);

        if let Some(index) = insert_index {
            for (i, track) in tracks.into_iter().enumerate() {
                self.playlist.insert(
                    index + i,
                    PlaylistItem {
                        item: track.0,
                        source: track.1,
                        block,
                    },
                );
            }
        } else {
            self.playlist
                .extend(tracks.into_iter().map(|track| PlaylistItem {
                    item: track.0,
                    source: track.1,
                    block,
                }));
        }

        self.playback_strategy.on_playlist_updated(
            self.playlist.len(),
            UpdateReason::AddToPlaylist {
                mode,
                index: insert_index,
                count,
            },
        );
        self.schedule_playlist_update();
    }

    fn interrupted_item(&self) -> InterruptedItem {
        match (self.current_track_index, self.current_item.as_ref()) {
            (Some(index_hint), Some(item)) if self.state != InternalPlaybackState::Stopped => {
                InterruptedItem::Item {
                    index_hint: self.current_playlist_index().unwrap_or(index_hint),
                    item: item.clone(),
                    position: self.current_position().unwrap_or_default(),
                }
            }
            _ => InterruptedItem::Nothing,
        }
    }

    fn current_queue_edit_index(&self) -> Option<usize> {
        (self.state != InternalPlaybackState::Stopped)
            .then(|| self.current_playlist_index())
            .flatten()
    }

    fn remove_from_playlist(&mut self, index: usize) -> Result<()> {
        if index < self.playlist.len() {
            debug!("Removing from playlist at index: {}", { index });
            let removed = self.playlist.remove(index);
            self.clear_unavailable(Some(removed.item))?;
            self.playback_strategy.on_playlist_updated(
                self.playlist.len(),
                UpdateReason::RemoveFromPlaylist { index },
            );
            self.schedule_playlist_update();
        } else {
            bail!(
                "Remove command received but index {} is out of bounds",
                index
            );
        }

        Ok(())
    }

    fn clear_playlist(&mut self) -> Result<()> {
        self.load_generation = self.load_generation.wrapping_add(1);
        self.complete_load_completions(Err(PlayerCommandError::Superseded));
        if let Some(pending) = self.pending_load.take() {
            Self::complete_completions(pending.completions, Err(PlayerCommandError::Superseded));
        }
        self.clear_unavailable(None)?;
        self.playlist.clear();
        self.playback_strategy
            .on_playlist_updated(0, UpdateReason::ClearPlaylist);
        self.current_track_index = None;
        self.active_stream_failure = None;
        self.pending_load = None;
        self.restore_position = None;
        self.pending_seek = None;
        self.output_stale = false;
        self.resume_after_interruption = false;
        self.sink = None;
        self._stream = None;
        self.release_host()?;
        info!("Playlist cleared");
        self.event_sender
            .send(PlayerEvent::Stopped)
            .with_context(|| "Failed to send Stopped event")?;
        self.schedule_playlist_update();
        self.state = InternalPlaybackState::Stopped;

        Ok(())
    }

    fn set_playback_mode(&mut self, mode: PlaybackMode) -> Result<()> {
        self.playback_mode = mode;
        self.playback_strategy = match mode {
            PlaybackMode::Sequential => Box::new(SequentialStrategy),
            PlaybackMode::RepeatOne => Box::new(RepeatOneStrategy),
            PlaybackMode::RepeatAll => Box::new(RepeatAllStrategy),
            PlaybackMode::Shuffle => Box::new(ShuffleStrategy::new(self.playlist.len())),
        };
        self.send_progress()?;
        info!("Playback mode set to {:?}", { mode });

        Ok(())
    }

    fn get_mapped_track_index(&self, index: usize) -> usize {
        self.playback_strategy
            .get_mapped_track_index(index, self.playlist.len())
    }

    fn find_available_after(&self, failed_playlist_index: usize) -> Option<usize> {
        let playlist_len = self.playlist.len();
        if playlist_len == 0 {
            return None;
        }
        let strategy_index = (0..playlist_len)
            .find(|index| self.get_mapped_track_index(*index) == failed_playlist_index)?;
        let repeat = matches!(
            self.playback_mode,
            PlaybackMode::RepeatAll | PlaybackMode::Shuffle
        );
        for offset in 1..=playlist_len {
            if !repeat && strategy_index + offset >= playlist_len {
                break;
            }
            let candidate = (strategy_index + offset) % playlist_len;
            let mapped = self.get_mapped_track_index(candidate);
            if !self.unavailable.contains_key(&self.playlist[mapped].item) {
                return Some(candidate);
            }
        }
        None
    }

    fn clear_unavailable(&mut self, item: Option<PlayingItem>) -> Result<()> {
        let changed = match &item {
            Some(item) => self.unavailable.remove(item).is_some(),
            None => {
                let changed = !self.unavailable.is_empty();
                self.unavailable.clear();
                changed
            }
        };
        if changed {
            self.event_sender
                .send(PlayerEvent::UnavailableCleared(item))
                .context("Failed to clear unavailable status")?;
        }
        Ok(())
    }

    fn send_progress(&mut self) -> Result<()> {
        let id = self.current_item.clone();
        let index = self.current_playlist_index();
        let path = self.current_track_path.clone();
        let playback_mode = self.playback_mode;
        // A track reports where a seek put it even while its output does not run, so the
        // reports do not take the slider back.
        let position = self.current_position().unwrap_or_default();

        if let Some(hosted) = &self.hosted {
            if self.host_overdue(hosted) {
                warn!(
                    "Host {} did not report on its track in time",
                    hosted.track.host
                );
                return self.hosted_failed(UnavailableReason::SourceUnreachable);
            }
            // The host reports the end of the track, so its position stands for the sink's.
            self.event_sender
                .send(PlayerEvent::Progress {
                    item: id,
                    index,
                    path,
                    playback_mode,
                    position,
                    ready: true,
                })
                .with_context(|| "Failed to send Progress event")?;
            return Ok(());
        }

        if let Some(sink) = &self.sink {
            // While a track loads, the previous one keeps the sink and may still run out. Its end
            // either started this load or was skipped by the listener, so it must not move the
            // queue again.
            if sink.empty() && self.state != InternalPlaybackState::Loading {
                if self
                    .active_stream_failure
                    .as_ref()
                    .is_some_and(|failed| failed.lock().is_ok_and(|reason| reason.is_some()))
                {
                    let reason = self
                        .active_stream_failure
                        .as_ref()
                        .and_then(|failed| failed.lock().ok().and_then(|reason| *reason))
                        .unwrap_or(UnavailableReason::SourceUnreachable);
                    let item = id.context("failed stream has no current item")?;
                    let failed_index = index.context("failed stream has no current index")?;
                    self.stop()?;
                    self.unavailable.insert(item.clone(), reason);
                    self.event_sender
                        .send(PlayerEvent::Unavailable(item, reason))?;
                    if let Some(next) = self.find_available_after(failed_index) {
                        self.load(Some(next), true, true)?;
                    } else {
                        self.event_sender.send(PlayerEvent::EndOfPlaylist)?;
                    }
                    return Ok(());
                }
                self.event_sender
                    .send(PlayerEvent::EndOfTrack {
                        item: id.unwrap(),
                        index: index.unwrap(),
                        path: path.unwrap(),
                        playback_mode,
                    })
                    .with_context(|| "Failed to send EndOfTrack event")?;

                if self.state != InternalPlaybackState::Stopped {
                    self.next()?;
                }
            } else {
                self.event_sender
                    .send(PlayerEvent::Progress {
                        item: id,
                        index,
                        path,
                        playback_mode,
                        position,
                        ready: true,
                    })
                    .with_context(|| "Failed to send Progress event")?;
            }
        } else {
            // A track without an output that plays is still ready, where it starts.
            self.event_sender
                .send(PlayerEvent::Progress {
                    item: id,
                    index,
                    path,
                    playback_mode,
                    position,
                    ready: self.output_stale && self.current_track_index.is_some(),
                })
                .with_context(|| "Failed to send Progress event")?;
        }

        Ok(())
    }

    fn move_playlist_item(&mut self, old_index: usize, new_index: usize) {
        if old_index >= self.playlist.len() || new_index >= self.playlist.len() {
            error!("Move command received but index is out of bounds");
            return;
        }

        if old_index == new_index {
            debug!("Move command received but old_index is the same as new_index");
            return;
        }

        debug!("Moving playlist item from index {old_index} to index {new_index}");

        let item = self.playlist.remove(old_index);
        self.playlist.insert(new_index, item);

        // Update the playback strategy to reflect changes in the playlist
        self.playback_strategy.on_playlist_updated(
            self.playlist.len(),
            UpdateReason::MovePlaylistItem {
                old_index,
                new_index,
            },
        );

        // Adjust the current track index if necessary
        if let Some(current_index) = self.current_track_index {
            if old_index == current_index {
                // The currently playing track was moved
                self.current_track_index = Some(new_index);
            } else if old_index < current_index && new_index >= current_index {
                // The track was moved past the current track
                self.current_track_index = Some(current_index - 1);
            } else if old_index > current_index && new_index <= current_index {
                // The track was moved before the current track
                self.current_track_index = Some(current_index + 1);
            }
        }

        self.schedule_playlist_update();
    }

    fn schedule_playlist_update(&mut self) {
        self.debounce_timer = Some(Instant::now() + Duration::from_millis(100));
    }

    fn send_playlist_updated(&self) -> Result<()> {
        let playlist_items: Vec<PlaylistItem> = self.playlist.clone();
        self.event_sender
            .send(PlayerEvent::PlaylistUpdated(playlist_items))
            .with_context(|| "Failed to send PlaylistUpdated event")?;

        Ok(())
    }

    fn set_volume(&mut self, volume: f32) -> Result<()> {
        self.volume = volume;
        if let Some(sink) = &self.sink {
            sink.set_volume(volume);
        }
        self.command_host(vec![HostAction::SetGain { gain: volume }]);
        self.event_sender
            .send(PlayerEvent::VolumeUpdate(volume))
            .with_context(|| "Failed to send VolumeUpdate event")?;

        Ok(())
    }

    fn set_realtime_fft_enabled(&mut self, x: bool) -> Result<()> {
        if let Ok(mut enabled) = self.fft_enabled.lock() {
            *enabled = x;
        }

        Ok(())
    }

    // Applies at once. iOS settles whether the app may be the Now Playing app, the one the lock
    // screen and Control Center control, when its session becomes active, and a session only
    // goes inactive while no output runs. So any output that runs stops, including the one an
    // older track keeps while a newer one loads, the session goes down and comes up again with
    // the new category, and the output of a track that plays starts again.
    fn set_mix_with_others(&mut self, x: bool) -> Result<()> {
        if self.mix_with_others == x {
            return Ok(());
        }
        self.mix_with_others = x;
        info!("Mix with other apps status changed: {x}");

        let running = self
            ._stream
            .as_ref()
            .is_some_and(|stream| !stream.is_suspended());
        if running {
            self.suspend_output();
        }
        #[cfg(target_os = "ios")]
        {
            crate::ios_session::deactivate();
            crate::ios_session::configure(x);
        }
        let playing = running && self.sink.as_ref().is_some_and(|sink| !sink.is_paused());
        if playing
            && self.output_suspended()
            && let Err(e) = self.resume_output()
        {
            error!("Failed to start the audio output again: {e:?}");
            self.mark_output_stale();
            // A track that is loading opens an output of its own.
            if self.state == InternalPlaybackState::Playing {
                self.reopen_output()?;
            }
        }
        Ok(())
    }

    fn set_adaptive_switching(&mut self, x: bool) -> Result<()> {
        self.adaptive_switching = x;

        info!("Adaptive switching status changed: {:#?}", { x });

        Ok(())
    }

    fn host_bridge(&self) -> Option<Arc<dyn HostBridge>> {
        match self.host_bridge.read() {
            Ok(slot) => slot.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    // Sends a batch to a host under a new revision, and returns that revision.
    fn send_to_host(&mut self, host: &str, actions: Vec<HostAction>) -> u64 {
        self.host_revision += 1;
        let revision = self.host_revision;
        match self.host_bridge() {
            Some(bridge) => bridge.send(HostCommand {
                host: host.to_owned(),
                revision,
                actions,
            }),
            None => warn!("No host bridge, so {actions:?} for host {host} is dropped"),
        }
        revision
    }

    // Sends a batch to the host of the current track, if a host plays it, and returns its
    // revision.
    fn command_host(&mut self, actions: Vec<HostAction>) -> Option<u64> {
        let host = self.hosted.as_ref()?.track.host.clone();
        let moves = HostedPlayback::moves(&actions);
        let revision = self.send_to_host(&host, actions);
        if let Some(hosted) = &mut self.hosted {
            hosted.latest_revision = revision;
            if moves {
                hosted.status_revision = revision;
            }
        }
        Some(revision)
    }

    // The batch that hands tracks over: the host's queue becomes `ids`, and the first of them
    // plays from `position`, or waits paused there.
    fn hand_over_actions(
        &self,
        ids: Vec<String>,
        position: Duration,
        play: bool,
    ) -> Vec<HostAction> {
        vec![
            HostAction::Set { ids },
            HostAction::Skip {
                index: 0,
                offset_ms: offset_ms(position),
            },
            HostAction::SetGain { gain: self.volume },
            if play {
                HostAction::Start
            } else {
                HostAction::Stop
            },
        ]
    }

    // Whether a hosted track can be handed to its host. Without a bridge, or with a host that
    // said it cannot play, it fails as a load does.
    fn check_host(&self, track: &HostedTrack) -> Result<()> {
        let (error, problem) = if self.host_bridge().is_none() {
            (
                RemoteAudioError::LibraryUnavailable,
                "no host bridge is set",
            )
        } else {
            match self.hosts_unable.get(&track.host) {
                None => return Ok(()),
                Some(UnavailableReason::AccessRevoked) => (
                    RemoteAudioError::AccessRevoked,
                    "the host is not authorized",
                ),
                Some(_) => (RemoteAudioError::LibraryUnavailable, "the host cannot play"),
            }
        };
        Err(anyhow::Error::new(error).context(format!(
            "Hosted track {} of {}: {problem}",
            track.key, track.host
        )))
    }

    // The track that Next goes to from the track at `index`, when the host that plays `current`
    // may get it in advance. Only a track of the same library is asked about, so that telling
    // costs no request to another library's service, and not one marked unavailable. Under
    // repeat-one the same track plays again, which its host is given anew at its end.
    fn host_next_after(&self, index: usize, current: &PlayingItem) -> Option<PlayingItem> {
        if self.playback_mode == PlaybackMode::RepeatOne || self.playlist.is_empty() {
            return None;
        }
        let PlayingItem::LibraryTrack { library_id, .. } = current else {
            return None;
        };
        let next = self.playback_strategy.next(index, self.playlist.len())?;
        let entry = self.playlist.get(self.resolve_index(next, true)?)?;
        match &entry.item {
            PlayingItem::LibraryTrack {
                library_id: next_library,
                ..
            } if next_library == library_id && !self.unavailable.contains_key(&entry.item) => {
                Some(entry.item.clone())
            }
            _ => None,
        }
    }

    // The track that the host of the current track holds after it, as the play order stands.
    fn wanted_host_next(&self) -> Option<PlayingItem> {
        let index = self.current_track_index?;
        self.host_next_after(index, self.current_item.as_ref()?)
    }

    // Hands a hosted track to its host, which plays it from `position` in place of any output
    // here. The new `set` replaces whatever the host held. The track after it follows with an
    // `add`, once the resolver says that the same host plays it.
    fn start_hosted(
        &mut self,
        track: HostedTrack,
        loaded: LoadedTrack,
        position: Duration,
    ) -> Result<()> {
        self.suspend_output();
        if let Some(sink) = self.sink.take() {
            sink.stop();
        }
        self._stream = None;
        self.active_stream_failure = None;
        self.output_stale = false;
        self.restore_position = None;
        self.pending_seek = None;
        self.resume_after_interruption = false;
        let previous_host = match self.hosted.take() {
            // Another host takes over, so the one that played stops and forgets its tracks.
            Some(previous) if previous.track.host != track.host => {
                self.send_to_host(
                    &previous.track.host,
                    vec![HostAction::Stop, HostAction::Clear],
                );
                Some(previous.track.host)
            }
            previous => previous.map(|previous| previous.track.host),
        };

        let host = track.host.clone();
        let actions = self.hand_over_actions(vec![track.key.clone()], position, true);
        let revision = self.send_to_host(&host, actions);
        info!("Track handed to host {host}: {}", track.key);
        let now = Instant::now();
        self.hosted = Some(HostedPlayback {
            track,
            index: 0,
            since_revision: revision,
            status_revision: revision,
            latest_revision: revision,
            next: HostNext::Nothing,
            playing: true,
            position,
            reported_at: now,
            started_at: now,
            heard_at: None,
            recovered: false,
            ended: false,
        });
        if previous_host.as_ref() != Some(&host) {
            self.event_sender
                .send(PlayerEvent::HostedChanged(Some(host)))
                .context("Failed to send HostedChanged event")?;
        }

        let LoadedTrack {
            item,
            index,
            mapped,
            playlist_index,
            path,
        } = loaded;
        self.current_track_index = Some(index);
        self.current_track_mapped = mapped;
        self.current_item = Some(item.clone());
        self.current_track_path = Some(path.clone());
        self.state = InternalPlaybackState::Playing;
        self.event_sender
            .send(PlayerEvent::Playing {
                item,
                index: playlist_index,
                path,
                playback_mode: self.playback_mode,
                position,
            })
            .context("Failed to send the state of the hosted track")?;
        self.complete_load_completions(Ok(()));
        Ok(())
    }

    // Hands the current track to its host again where it stands, playing or paused as it was,
    // after the host lost it, together with the next track if the host held that one. Not once
    // the track ended or while another track loads.
    fn hand_over_again(&mut self) {
        if self.state == InternalPlaybackState::Loading {
            return;
        }
        let Some(hosted) = &self.hosted else {
            return;
        };
        if hosted.ended {
            return;
        }
        let mut ids = vec![hosted.track.key.clone()];
        if let HostNext::Queued { track, .. } = &hosted.next {
            ids.push(track.key.clone());
        }
        let (host, position, playing) =
            (hosted.track.host.clone(), hosted.position(), hosted.playing);
        info!("Handing the track to host {host} again at {position:?}");
        let actions = self.hand_over_actions(ids, position, playing);
        let revision = self.send_to_host(&host, actions);
        if let Some(hosted) = &mut self.hosted {
            hosted.index = 0;
            hosted.since_revision = revision;
            hosted.status_revision = revision;
            hosted.latest_revision = revision;
            if let HostNext::Queued {
                revision: queued, ..
            } = &mut hosted.next
            {
                *queued = revision;
            }
            hosted.report(position, playing);
            hosted.started_at = Instant::now();
            hosted.heard_at = None;
        }
    }

    // The host no longer holds the current track where the player put it: it reported no current
    // entry, as after a restart, or another track at the current track's index. The track goes
    // to the host again once, and fails if the host loses it again.
    fn host_lost_track(&mut self) -> Result<()> {
        let Some(hosted) = &mut self.hosted else {
            return Ok(());
        };
        if hosted.ended || self.state == InternalPlaybackState::Loading {
            return Ok(());
        }
        if hosted.recovered {
            warn!("Host {} lost its track again", hosted.track.host);
            return self.hosted_failed(UnavailableReason::SourceUnreachable);
        }
        hosted.recovered = true;
        self.hand_over_again();
        Ok(())
    }

    // The current track stops being hosted, so its host stops and forgets what it held. Returns
    // whether a hosted track was current.
    fn release_host(&mut self) -> Result<bool> {
        let Some(hosted) = self.hosted.take() else {
            return Ok(false);
        };
        self.send_to_host(
            &hosted.track.host,
            vec![HostAction::Stop, HostAction::Clear],
        );
        self.event_sender
            .send(PlayerEvent::HostedChanged(None))
            .context("Failed to send HostedChanged event")?;
        Ok(true)
    }

    // Keeps the track that the host holds after the current one in step with the play order. It
    // runs after every command and acts only when the track that follows changed: the host loses
    // the one it holds at once, so that it does not move on to it, and gets the new one once the
    // resolver says that it plays that one too. A lookup still running for a track that no
    // longer follows stops.
    fn sync_host_next(&mut self) {
        let Some(hosted) = &self.hosted else {
            return;
        };
        // A track that loads replaces what the host holds.
        if hosted.ended || self.state == InternalPlaybackState::Loading {
            return;
        }
        let wanted = self.wanted_host_next();
        if hosted.next.item() == wanted.as_ref() {
            return;
        }
        let host = hosted.track.host.clone();
        let stale = matches!(hosted.next, HostNext::Queued { .. }).then_some(HostAction::Remove {
            index: hosted.index + 1,
        });
        if let Some(hosted) = &mut self.hosted {
            hosted.next = HostNext::Nothing;
        }
        if let Some(remove) = stale {
            self.command_host(vec![remove]);
        }
        let Some(item) = wanted else {
            return;
        };
        self.host_next_request += 1;
        let request = self.host_next_request;
        let resolver = match self.source_resolver.read() {
            Ok(slot) => slot.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        let commands = self.commands_sender.clone();
        let asked = item.clone();
        let lookup = tokio::spawn(async move {
            let track = match resolver {
                Some(resolver) => hosted_by(&resolver, &asked, &host).await,
                None => None,
            };
            if commands
                .send(PlayerCommand::HostNextResolved { request, track })
                .is_err()
            {
                debug!("The player stopped before it heard whether host {host} plays {asked}");
            }
        });
        if let Some(hosted) = &mut self.hosted {
            hosted.next = HostNext::Resolving {
                request,
                item,
                _lookup: Lookup(lookup),
            };
        }
    }

    // The resolver said whether the host of the current track plays the track after it. An
    // answer to an older question, or one that comes while another track loads, is dropped.
    fn host_next_resolved(&mut self, request: u64, track: Option<HostedTrack>) {
        let loading = self.state == InternalPlaybackState::Loading;
        let Some(hosted) = &mut self.hosted else {
            return;
        };
        let HostNext::Resolving {
            request: asked,
            item,
            ..
        } = &hosted.next
        else {
            return;
        };
        if *asked != request {
            return;
        }
        let item = item.clone();
        if loading || hosted.ended {
            hosted.next = HostNext::Nothing;
            return;
        }
        let Some(track) = track else {
            hosted.next = HostNext::Declined { item };
            return;
        };
        let add = HostAction::Add {
            ids: vec![track.key.clone()],
        };
        let Some(revision) = self.command_host(vec![add]) else {
            return;
        };
        info!("Host {} holds the next track: {}", track.host, track.key);
        if let Some(hosted) = &mut self.hosted {
            hosted.next = HostNext::Queued {
                item,
                track,
                revision,
            };
        }
    }

    // Makes the track that the host holds next the current one, without handing it over again,
    // as long as it is the one Next goes to from the current track. Returns whether it was.
    fn move_to_host_next(&mut self, playing: bool, position: Duration) -> Result<bool> {
        let (Some(hosted), Some(index)) = (&self.hosted, self.current_track_index) else {
            return Ok(false);
        };
        let HostNext::Queued { item, .. } = &hosted.next else {
            return Ok(false);
        };
        let next_index = (!self.playlist.is_empty())
            .then(|| self.playback_strategy.next(index, self.playlist.len()))
            .flatten();
        let Some(next_index) = next_index.filter(|next| {
            self.resolve_index(*next, true)
                .and_then(|position| self.playlist.get(position))
                .is_some_and(|entry| &entry.item == item)
        }) else {
            return Ok(false);
        };
        let Some(hosted) = &mut self.hosted else {
            return Ok(false);
        };
        let HostNext::Queued {
            item,
            track,
            revision,
        } = std::mem::replace(&mut hosted.next, HostNext::Nothing)
        else {
            return Ok(false);
        };
        let path = track.path();
        hosted.track = track;
        hosted.index += 1;
        hosted.since_revision = revision;
        hosted.report(position, playing);
        hosted.started_at = Instant::now();
        hosted.heard_at = None;
        hosted.recovered = false;
        self.current_track_index = Some(next_index);
        self.current_track_mapped = true;
        self.current_item = Some(item);
        self.current_track_path = Some(path);
        self.state = if playing {
            InternalPlaybackState::Playing
        } else {
            InternalPlaybackState::Paused
        };
        self.send_track_state(playing, position)?;
        Ok(true)
    }

    // The host moved on by itself to the entry after the current one, playing the track `id`,
    // which counts the current track as played through. When that is the track the player
    // queued there, and Next still goes to it, it becomes current without a new hand-over.
    // Otherwise the host moved on to a track that the player had taken out of its queue as it
    // did: the current track ends once, and the track that comes next loads and is handed over
    // anew, which replaces the host's queue.
    fn host_moved_on(&mut self, id: Option<&str>, playing: bool, position: Duration) -> Result<()> {
        let (Some(item), Some(index), Some(path)) = (
            self.current_item.clone(),
            self.current_playlist_index(),
            self.current_track_path.clone(),
        ) else {
            return Ok(());
        };
        self.event_sender
            .send(PlayerEvent::EndOfTrack {
                item,
                index,
                path,
                playback_mode: self.playback_mode,
            })
            .with_context(|| "Failed to send EndOfTrack event")?;
        let queued = self.hosted.as_ref().is_some_and(|hosted| {
            matches!(&hosted.next, HostNext::Queued { track, .. } if Some(track.key.as_str()) == id)
        });
        if queued && self.move_to_host_next(playing, position)? {
            if let Some(hosted) = &mut self.hosted {
                hosted.heard_at = Some(Instant::now());
            }
            return Ok(());
        }
        warn!("The host moved on to a track that does not come next: {id:?}");
        if let Some(hosted) = &mut self.hosted {
            hosted.ended = true;
        }
        self.next_with(playing)
    }

    // Next while the host holds the track that Next goes to: the host skips to it, as it would
    // move on by itself at the end of the current track, and nothing is handed over again.
    // Returns whether it did.
    fn skip_to_host_next(&mut self) -> Result<bool> {
        let Some(hosted) = &self.hosted else {
            return Ok(false);
        };
        if hosted.ended || self.state == InternalPlaybackState::Loading {
            return Ok(false);
        }
        let skip = HostAction::Skip {
            index: hosted.index + 1,
            offset_ms: 0,
        };
        if !self.move_to_host_next(true, Duration::ZERO)? {
            return Ok(false);
        }
        self.command_host(vec![skip, HostAction::Start]);
        info!("Skipped to the track that the host holds next");
        Ok(true)
    }

    // Whether the host of a track that is to play went quiet for too long: it did not report in
    // time after it was told to play, or it stopped reporting. A track that loads takes over
    // anyway.
    fn host_overdue(&self, hosted: &HostedPlayback) -> bool {
        if !hosted.playing || hosted.ended || self.state == InternalPlaybackState::Loading {
            return false;
        }
        let (since, limit) = match hosted.heard_at {
            None => (hosted.started_at, HOST_STATUS_TIMEOUT),
            Some(heard) => (heard.max(hosted.started_at), HOST_SILENCE_TIMEOUT),
        };
        since.elapsed() >= limit
    }

    // Play and Pause of a hosted track go to its host. While another track loads, the hosted one
    // follows them as an older track's sink does.
    fn set_hosted_playing(&mut self, playing: bool, loading: bool) -> Result<()> {
        let Some(hosted) = &mut self.hosted else {
            return Ok(());
        };
        let position = hosted.position();
        hosted.report(position, playing);
        if playing {
            hosted.started_at = Instant::now();
        }
        self.command_host(vec![if playing {
            HostAction::Start
        } else {
            HostAction::Stop
        }]);
        self.send_track_state(playing, position)?;
        if !loading {
            self.state = if playing {
                InternalPlaybackState::Playing
            } else {
                InternalPlaybackState::Paused
            };
        }
        Ok(())
    }

    // A seek moves the current track only: the track the host holds next stays.
    fn seek_hosted(&mut self, position: Duration) -> Result<SeekOutcome> {
        let Some(hosted) = &mut self.hosted else {
            return Ok(SeekOutcome::CannotApply);
        };
        let playing = hosted.playing;
        hosted.report(position, playing);
        let skip = HostAction::Skip {
            index: hosted.index,
            offset_ms: offset_ms(position),
        };
        self.command_host(vec![skip]);
        info!("Seeking the hosted track to {position:?}");
        self.send_track_state(playing, position)?;
        Ok(SeekOutcome::Applied)
    }

    fn host_event(&mut self, event: HostEvent) -> Result<()> {
        let loading = self.state == InternalPlaybackState::Loading;
        match event {
            HostEvent::HostReady {
                host,
                authorized,
                can_play,
            } => {
                let current = self
                    .hosted
                    .as_ref()
                    .is_some_and(|hosted| hosted.track.host == host);
                let refusal = if !authorized {
                    Some(UnavailableReason::AccessRevoked)
                } else if !can_play {
                    Some(UnavailableReason::LibraryUnavailable)
                } else {
                    None
                };
                match refusal {
                    Some(reason) => {
                        warn!("Host {host} cannot play: {reason:?}");
                        self.hosts_unable.insert(host, reason);
                        if current {
                            return self.hosted_failed(reason);
                        }
                    }
                    None => {
                        self.hosts_unable.remove(&host);
                        // A host that comes up again lost what it held.
                        if current {
                            self.hand_over_again();
                        }
                    }
                }
            }
            HostEvent::JukeboxStatus {
                host,
                revision,
                current_index,
                current_id,
                playing,
                position_ms,
            } => {
                let Some(hosted) = self.hosted.as_mut().filter(|hosted| {
                    hosted.track.host == host
                        && (hosted.status_revision..=hosted.latest_revision).contains(&revision)
                }) else {
                    return Ok(());
                };
                let position = Duration::from_millis(position_ms);
                match current_index {
                    Some(index)
                        if index == hosted.index
                            && current_id.as_deref() == Some(hosted.track.key.as_str()) =>
                    {
                        let changed = hosted.playing != playing;
                        hosted.report(position, playing);
                        hosted.heard_at = Some(Instant::now());
                        if changed {
                            self.send_track_state(playing, position)?;
                            if !loading {
                                self.state = if playing {
                                    InternalPlaybackState::Playing
                                } else {
                                    InternalPlaybackState::Paused
                                };
                            }
                        }
                    }
                    // The host holds no track, as after a restart, or another one where the
                    // current track should be.
                    None => return self.host_lost_track(),
                    Some(index) if index == hosted.index => {
                        warn!("Host {host} holds {current_id:?} where the current track should be");
                        return self.host_lost_track();
                    }
                    Some(index) if index == hosted.index + 1 => {
                        // As with a track that ends while another loads, the load takes over.
                        if hosted.ended || loading {
                            return Ok(());
                        }
                        return self.host_moved_on(current_id.as_deref(), playing, position);
                    }
                    Some(index) => {
                        debug!("Ignoring a status of host {host} about entry {index}");
                    }
                }
            }
            HostEvent::JukeboxItemFailed {
                host,
                revision,
                index,
                reason,
            } => {
                let Some(hosted) = self
                    .hosted
                    .as_mut()
                    .filter(|hosted| hosted.track.host == host)
                else {
                    return Ok(());
                };
                if hosted.is_current(revision, index) {
                    warn!("Host {host} failed to play its track: {reason:?}");
                    return self.hosted_failed(reason.into());
                }
                // The host failed the track it holds next. That track leaves the host's queue,
                // and is handed over as usual once it is current, failing then if it still does.
                if let HostNext::Queued {
                    item,
                    revision: queued,
                    ..
                } = &hosted.next
                    && index == hosted.index + 1
                    && (*queued..=hosted.latest_revision).contains(&revision)
                {
                    warn!("Host {host} cannot play the track it holds next: {reason:?}");
                    let remove = HostAction::Remove { index };
                    hosted.next = HostNext::Declined { item: item.clone() };
                    self.command_host(vec![remove]);
                }
            }
            HostEvent::JukeboxEnded { host, revision } => {
                let Some(hosted) = self.hosted.as_mut().filter(|hosted| {
                    hosted.track.host == host
                        && (hosted.since_revision..=hosted.latest_revision).contains(&revision)
                        && !hosted.ended
                }) else {
                    return Ok(());
                };
                // A track the user paused just as it ended leaves the next one paused.
                let play = hosted.playing;
                let position = hosted.position();
                hosted.report(position, false);
                hosted.ended = true;
                // As with an older track's sink that runs out while a track loads, the end
                // either started this load or was skipped by the listener.
                if loading {
                    return Ok(());
                }
                let (Some(item), Some(index), Some(path)) = (
                    self.current_item.clone(),
                    self.current_playlist_index(),
                    self.current_track_path.clone(),
                ) else {
                    return Ok(());
                };
                self.event_sender
                    .send(PlayerEvent::EndOfTrack {
                        item,
                        index,
                        path,
                        playback_mode: self.playback_mode,
                    })
                    .with_context(|| "Failed to send EndOfTrack event")?;
                if self.state != InternalPlaybackState::Stopped {
                    self.next_with(play)?;
                }
            }
            HostEvent::Unknown => {}
        }
        Ok(())
    }

    // The host cannot play the current track. It is marked unavailable and playback moves on, as
    // after a stream that fails. While another track loads, that track takes over instead.
    fn hosted_failed(&mut self, reason: UnavailableReason) -> Result<()> {
        let (Some(item), Some(failed_index)) =
            (self.current_item.clone(), self.current_playlist_index())
        else {
            return Ok(());
        };
        let Some(hosted) = &mut self.hosted else {
            return Ok(());
        };
        let play = hosted.playing;
        let loading = self.state == InternalPlaybackState::Loading;
        if loading {
            hosted.report(hosted.position(), false);
        } else {
            self.stop()?;
        }
        self.unavailable.insert(item.clone(), reason);
        self.event_sender
            .send(PlayerEvent::Unavailable(item, reason))?;
        if loading {
            return Ok(());
        }
        if let Some(next) = self.find_available_after(failed_index) {
            self.load(Some(next), play, true)?;
        } else {
            self.event_sender.send(PlayerEvent::EndOfPlaylist)?;
        }
        Ok(())
    }
}

fn inherited_block_for_insert(
    playlist: &[PlaylistItem],
    index: usize,
    current_item: Option<&PlayingItem>,
) -> Option<HandoffBlockId> {
    let left = index.checked_sub(1).and_then(|left| playlist.get(left));
    let right = playlist.get(index);
    let between_same_block = left
        .and_then(|item| item.block)
        .filter(|block| right.is_some_and(|item| item.block == Some(*block)));
    between_same_block.or_else(|| {
        left.and_then(|item| item.block).filter(|block| {
            !playlist[index..]
                .iter()
                .any(|item| item.block == Some(*block))
                && current_item.is_some_and(|current| {
                    playlist
                        .iter()
                        .any(|entry| entry.block == Some(*block) && &entry.item == current)
                })
        })
    })
}

#[cfg(test)]
mod stream_open_tests {
    use super::*;
    use crate::player::{AudioRequest, LibraryPlaybackSource, RemoteAudioSource};
    use futures::future::BoxFuture;
    use std::collections::{HashMap, HashSet};
    use std::io::{Read, Seek, SeekFrom};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{Receiver, channel};
    const TEST_DEADLINE: Duration = Duration::from_secs(60);
    use tokio::runtime::Handle;

    fn opendal_fsio(config: fsio::OpenDalConfig) -> Result<Arc<FsIo>> {
        Ok(Arc::new(FsIo::new_opendal(config, Handle::current())?))
    }

    async fn decode_opendal_asset(fsio: Arc<FsIo>) -> Result<()> {
        let source = open_local_source(fsio, PathBuf::from("startup_0.ogg"), true).await?;
        let duration = source
            .total_duration()
            .context("decoded duration missing")?;
        anyhow::ensure!(duration > Duration::from_secs(1));
        let samples = tokio::task::spawn_blocking(move || -> Result<usize> {
            let mut source = source;
            source
                .try_seek(duration.saturating_sub(Duration::from_millis(500)))
                .map_err(|error| anyhow!("seek failed: {error}"))?;
            let mut samples = 0usize;
            while source.next().is_some() {
                samples += 1;
            }
            Ok(samples)
        })
        .await??;
        anyhow::ensure!(samples > 0, "seek near the end produced no samples");
        Ok(())
    }

    #[tokio::test]
    async fn opendal_track_decodes_seeks_and_finishes_on_current_thread() -> Result<()> {
        use fsio::{OpenDalConfig, test_fixtures::WebDavFixture};

        tokio::time::timeout(TEST_DEADLINE, async {
            let asset = include_bytes!("../../assets/startup_0.ogg").to_vec();
            let fixture = WebDavFixture::start(vec![("startup_0.ogg".into(), asset)]).await?;
            let fsio = opendal_fsio(OpenDalConfig::WebDav {
                endpoint: fixture.endpoint.clone(),
                root: "/dav/library/".into(),
                username: "user".into(),
                password: "pass".into(),
            })?;
            decode_opendal_asset(fsio).await?;
            anyhow::ensure!(fixture.write_requests().is_empty());
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("current-thread OpenDAL decode timed out")??;
        Ok(())
    }

    #[tokio::test]
    async fn opendal_s3_track_decodes_seeks_and_finishes_on_current_thread() -> Result<()> {
        use fsio::{OpenDalConfig, test_fixtures::S3Fixture};

        tokio::time::timeout(TEST_DEADLINE, async {
            let fixture = S3Fixture::start(vec![(
                "startup_0.ogg".into(),
                include_bytes!("../../assets/startup_0.ogg").to_vec(),
            )])
            .await?;
            let fsio = opendal_fsio(OpenDalConfig::S3 {
                endpoint: fixture.endpoint.clone(),
                bucket: "bucket".into(),
                region: "us-east-1".into(),
                root: "/library/".into(),
                access_key_id: "access".into(),
                secret_access_key: "secret".into(),
            })?;
            decode_opendal_asset(fsio).await
        })
        .await
        .context("current-thread OpenDAL S3 decode timed out")??;
        Ok(())
    }

    #[tokio::test]
    async fn rejected_opendal_credentials_are_a_typed_source_failure() -> Result<()> {
        use fsio::{OpenDalConfig, test_fixtures::WebDavFixture};

        tokio::time::timeout(TEST_DEADLINE, async {
            let fixture = WebDavFixture::start(vec![(
                "startup_0.ogg".into(),
                include_bytes!("../../assets/startup_0.ogg").to_vec(),
            )])
            .await?;
            let fsio = opendal_fsio(OpenDalConfig::WebDav {
                endpoint: fixture.endpoint.clone(),
                root: "/dav/library/".into(),
                username: "user".into(),
                password: "wrong".into(),
            })?;
            let error = open_local_source(fsio, PathBuf::from("startup_0.ogg"), true)
                .await
                .expect_err("rejected credentials must fail the source");
            anyhow::ensure!(
                UnavailableReason::classify(&error) == UnavailableReason::AccessRevoked,
                "OpenDAL error was not classified as a typed access failure: {error:#}"
            );
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("current-thread credential failure timed out")??;
        Ok(())
    }
    use tokio::sync::Notify;
    use tokio::task::LocalSet;
    use uuid::Uuid;

    struct FailingRequest;
    impl AudioRequest for FailingRequest {
        fn get(
            &self,
            _: Option<(u64, Option<u64>)>,
        ) -> BoxFuture<'static, Result<reqwest::Response>> {
            Box::pin(async { anyhow::bail!("request failed") })
        }
    }
    struct Resolver(CancellationToken);
    impl PlaybackSourceResolver for Resolver {
        fn resolve(&self, _: i32) -> BoxFuture<'static, Result<RemoteAudioSource>> {
            let cancellation = self.0.clone();
            Box::pin(async move {
                Ok(RemoteAudioSource {
                    request: Arc::new(FailingRequest),
                    url: "http://localhost/audio".into(),
                    extension: "mp3".into(),
                    size: 1,
                    cancellation,
                    failure_reason: Arc::new(Mutex::new(None)),
                })
            })
        }
    }
    #[tokio::test]
    async fn failed_stream_creation_cancels_watcher() {
        let token = CancellationToken::new();
        let resolver: Arc<dyn PlaybackSourceResolver> = Arc::new(Resolver(token.clone()));
        assert!(open_library_stream(&resolver, 1).await.is_err());
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn source_start_timeout_uses_unavailable_network_reason() {
        let error = bound_source_start(Duration::from_millis(1), async {
            std::future::pending::<Result<()>>().await
        })
        .await
        .expect_err("a source that never starts must time out");
        assert_eq!(
            UnavailableReason::classify(&error),
            UnavailableReason::SourceUnreachable
        );
    }

    struct StalledReader {
        release: Arc<Mutex<Receiver<()>>>,
        released: Arc<AtomicBool>,
        probe_finished: Arc<Notify>,
    }

    impl Read for StalledReader {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            if !self.released.swap(true, Ordering::SeqCst) {
                self.release.lock().unwrap().recv().unwrap();
            }
            self.probe_finished.notify_one();
            Ok(0)
        }
    }

    impl Seek for StalledReader {
        fn seek(&mut self, _: SeekFrom) -> std::io::Result<u64> {
            Ok(0)
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocking_decoder_probe_respects_source_start_timeout() -> Result<()> {
        let (release_sender, release_receiver) = channel();
        let probe_finished = Arc::new(Notify::new());

        let reader = StalledReader {
            release: Arc::new(Mutex::new(release_receiver)),
            released: Arc::new(AtomicBool::new(false)),
            probe_finished: probe_finished.clone(),
        };
        let result = tokio::time::timeout(
            TEST_DEADLINE,
            bound_source_start(
                Duration::from_millis(25),
                probe_decoder(reader, Some("ogg".to_owned()), None),
            ),
        )
        .await?;
        let error = match result {
            Ok(_) => panic!("blocking decoder probe must be bounded"),
            Err(error) => error,
        };
        assert_eq!(
            UnavailableReason::classify(&error),
            UnavailableReason::SourceUnreachable
        );
        release_sender.send(()).unwrap();
        tokio::time::timeout(TEST_DEADLINE, probe_finished.notified()).await?;
        Ok(())
    }

    pub(super) struct DelayedResolver {
        ids: Arc<Mutex<Vec<i32>>>,
        started: Arc<Notify>,
        release: Arc<Notify>,
        file_ids: Arc<HashMap<Uuid, i32>>,
        file_path: PathBuf,
        blocked_ids: Arc<HashSet<i32>>,
    }

    impl PlaybackSourceResolver for DelayedResolver {
        fn for_library(
            &self,
            _: String,
        ) -> BoxFuture<'static, Result<Option<LibraryPlaybackSource>>> {
            let ids = self.ids.clone();
            let started = self.started.clone();
            let release = self.release.clone();
            let file_ids = self.file_ids.clone();
            let file_path = self.file_path.clone();
            let blocked_ids = self.blocked_ids.clone();
            Box::pin(async move {
                Ok(Some(LibraryPlaybackSource {
                    fsio: Arc::new(FsIo::new()),
                    resolver: Arc::new(DelayedLibraryResolver {
                        ids,
                        started,
                        release,
                        file_ids,
                        file_path,
                        blocked_ids,
                    }),
                }))
            })
        }

        fn resolve(&self, _: i32) -> BoxFuture<'static, Result<RemoteAudioSource>> {
            Box::pin(async { anyhow::bail!("unexpected remote source resolution") })
        }
    }

    struct DelayedLibraryResolver {
        ids: Arc<Mutex<Vec<i32>>>,
        started: Arc<Notify>,
        release: Arc<Notify>,
        file_ids: Arc<HashMap<Uuid, i32>>,
        file_path: PathBuf,
        blocked_ids: Arc<HashSet<i32>>,
    }

    impl PlaybackSourceResolver for DelayedLibraryResolver {
        fn resolve_id(&self, file_uuid: Uuid) -> BoxFuture<'static, Result<Option<i32>>> {
            let id = self.file_ids.get(&file_uuid).copied();
            Box::pin(async move { Ok(id) })
        }

        fn requires_stream(&self, id: i32) -> BoxFuture<'static, Result<bool>> {
            let ids = self.ids.clone();
            let started = self.started.clone();
            let release = self.release.clone();
            let blocked_ids = self.blocked_ids.clone();
            Box::pin(async move {
                ids.lock().unwrap().push(id);
                if blocked_ids.contains(&id) {
                    started.notify_one();
                    release.notified().await;
                }
                Ok(false)
            })
        }

        fn local_path(&self, _: i32) -> BoxFuture<'static, Result<Option<PathBuf>>> {
            let path = self.file_path.clone();
            Box::pin(async move { Ok(Some(path)) })
        }

        fn resolve(&self, _: i32) -> BoxFuture<'static, Result<RemoteAudioSource>> {
            Box::pin(async { anyhow::bail!("unexpected remote source resolution") })
        }
    }

    pub(super) fn delayed_resolver_fixture(
        file_uuids: &[Uuid],
        ids: Arc<Mutex<Vec<i32>>>,
        started: Arc<Notify>,
        release: Arc<Notify>,
        blocked_ids: &[i32],
    ) -> (Arc<DelayedResolver>, PathBuf) {
        delayed_resolver_fixture_with_samples(file_uuids, ids, started, release, blocked_ids, 2)
    }

    // Every track is the same silent 8 kHz mono WAV file with `samples` samples.
    pub(super) fn delayed_resolver_fixture_with_samples(
        file_uuids: &[Uuid],
        ids: Arc<Mutex<Vec<i32>>>,
        started: Arc<Notify>,
        release: Arc<Notify>,
        blocked_ids: &[i32],
        samples: u32,
    ) -> (Arc<DelayedResolver>, PathBuf) {
        let wav_path = std::env::temp_dir().join(format!("playback-{}.wav", Uuid::new_v4()));
        let data_len = samples * 2;
        let mut wav = Vec::with_capacity(44 + data_len as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&8_000_u32.to_le_bytes());
        wav.extend_from_slice(&16_000_u32.to_le_bytes());
        wav.extend_from_slice(&2_u16.to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.resize(wav.len() + data_len as usize, 0);
        std::fs::write(&wav_path, wav).unwrap();
        let resolver = Arc::new(DelayedResolver {
            ids,
            started,
            release,
            file_ids: Arc::new(
                file_uuids
                    .iter()
                    .enumerate()
                    .map(|(index, file_uuid)| (*file_uuid, index as i32 + 1))
                    .collect(),
            ),
            file_path: wav_path.clone(),
            blocked_ids: Arc::new(blocked_ids.iter().copied().collect()),
        });
        (resolver, wav_path)
    }

    #[tokio::test]
    async fn newest_switch_during_load_replaces_the_pending_track() -> Result<()> {
        let file_uuids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
        let ids = Arc::new(Mutex::new(Vec::new()));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (resolver, wav_path) = delayed_resolver_fixture(
            &file_uuids,
            ids.clone(),
            started.clone(),
            release.clone(),
            &[1],
        );
        let (commands_sender, commands) = mpsc::unbounded_channel();
        let (event_sender, _events) = mpsc::unbounded_channel();
        let mut internal = PlayerInternal::new(
            commands,
            event_sender,
            CancellationToken::new(),
            commands_sender.clone(),
            Arc::new(FsIo::new_noop()),
            PcmTap::new(),
            Arc::new(RwLock::new(Some(resolver))),
        );
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    commands_sender.send(PlayerCommand::AddToPlaylist {
                        tracks: file_uuids
                            .iter()
                            .map(|file_uuid| {
                                (
                                    PlayingItem::LibraryTrack {
                                        library_id,
                                        track: file_uuid.to_string(),
                                    },
                                    PlaybackSource::LibraryStream,
                                )
                            })
                            .collect(),
                        mode: AddMode::AppendToEnd,
                    })?;
                    commands_sender.send(PlayerCommand::Switch(0))?;
                    tokio::time::timeout(TEST_DEADLINE, started.notified()).await?;
                    commands_sender.send(PlayerCommand::Switch(2))?;
                    release.notify_one();

                    tokio::time::timeout(TEST_DEADLINE, async {
                        loop {
                            if ids.lock().unwrap().len() >= 2 {
                                break;
                            }
                            tokio::task::yield_now().await;
                        }
                    })
                    .await?;
                    assert_eq!(&*ids.lock().unwrap(), &[1, 3]);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn clear_replace_during_load_finishes_on_the_new_first_track() -> Result<()> {
        let file_uuids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
        let ids = Arc::new(Mutex::new(Vec::new()));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (resolver, wav_path) = delayed_resolver_fixture(
            &file_uuids,
            ids.clone(),
            started.clone(),
            release.clone(),
            &[1, 2],
        );
        let (commands_sender, commands) = mpsc::unbounded_channel();
        let (event_sender, mut events) = mpsc::unbounded_channel();
        let mut internal = PlayerInternal::new(
            commands,
            event_sender,
            CancellationToken::new(),
            commands_sender.clone(),
            Arc::new(FsIo::new_noop()),
            PcmTap::new(),
            Arc::new(RwLock::new(Some(resolver))),
        );
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let track = |file_uuid: Uuid| {
                        (
                            PlayingItem::LibraryTrack {
                                library_id,
                                track: file_uuid.to_string(),
                            },
                            PlaybackSource::LibraryStream,
                        )
                    };
                    commands_sender.send(PlayerCommand::AddToPlaylist {
                        tracks: vec![track(file_uuids[0])],
                        mode: AddMode::AppendToEnd,
                    })?;
                    commands_sender.send(PlayerCommand::Switch(0))?;
                    tokio::time::timeout(TEST_DEADLINE, started.notified()).await?;
                    commands_sender.send(PlayerCommand::ClearPlaylist)?;
                    commands_sender.send(PlayerCommand::AddToPlaylist {
                        tracks: vec![track(file_uuids[1]), track(file_uuids[2])],
                        mode: AddMode::AppendToEnd,
                    })?;
                    commands_sender.send(PlayerCommand::Switch(0))?;
                    commands_sender.send(PlayerCommand::Play)?;
                    tokio::time::timeout(TEST_DEADLINE, async {
                        loop {
                            if ids.lock().unwrap().len() >= 2 {
                                break;
                            }
                            tokio::task::yield_now().await;
                        }
                    })
                    .await?;
                    release.notify_one();
                    release.notify_one();

                    let expected = PlayingItem::LibraryTrack {
                        library_id,
                        track: file_uuids[1].to_string(),
                    };
                    tokio::time::timeout(TEST_DEADLINE, async {
                        loop {
                            if let Some(PlayerEvent::Playing { item, .. }) = events.recv().await
                                && item == expected
                            {
                                break;
                            }
                        }
                    })
                    .await?;
                    assert_eq!(&*ids.lock().unwrap(), &[1, 2]);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn stop_during_load_drops_the_late_completion() -> Result<()> {
        let file_uuids = [Uuid::new_v4()];
        let ids = Arc::new(Mutex::new(Vec::new()));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (resolver, wav_path) =
            delayed_resolver_fixture(&file_uuids, ids, started.clone(), release.clone(), &[1]);
        let (commands_sender, commands) = mpsc::unbounded_channel();
        let (event_sender, mut events) = mpsc::unbounded_channel();
        let mut internal = PlayerInternal::new(
            commands,
            event_sender,
            CancellationToken::new(),
            commands_sender.clone(),
            Arc::new(FsIo::new_noop()),
            PcmTap::new(),
            Arc::new(RwLock::new(Some(resolver))),
        );
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let expected = PlayingItem::LibraryTrack {
                        library_id,
                        track: file_uuids[0].to_string(),
                    };
                    commands_sender.send(PlayerCommand::AddToPlaylist {
                        tracks: vec![(expected.clone(), PlaybackSource::LibraryStream)],
                        mode: AddMode::AppendToEnd,
                    })?;
                    commands_sender.send(PlayerCommand::Switch(0))?;
                    tokio::time::timeout(TEST_DEADLINE, started.notified()).await?;
                    commands_sender.send(PlayerCommand::Stop)?;
                    release.notify_one();
                    commands_sender.send(PlayerCommand::SetVolume(0.5))?;
                    tokio::time::timeout(TEST_DEADLINE, async {
                        loop {
                            match events.recv().await {
                                Some(PlayerEvent::VolumeUpdate(_)) => break,
                                Some(_) => {}
                                None => panic!("player events closed"),
                            }
                        }
                    })
                    .await?;
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    while let Ok(event) = events.try_recv() {
                        assert!(!matches!(
                            event,
                            PlayerEvent::Loaded { item, .. } | PlayerEvent::Playing { item, .. }
                                if item == expected
                        ));
                    }

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }
}

// The player's state machine: loads, play and pause, seeks, and what happens to the output.
#[cfg(test)]
mod playback_state_tests {
    use super::stream_open_tests::{
        DelayedResolver, delayed_resolver_fixture, delayed_resolver_fixture_with_samples,
    };
    use super::*;
    use crate::player::PlaylistLoad;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::channel;
    use tokio::runtime::Builder;
    use tokio::sync::Notify;
    use tokio::task::LocalSet;
    use uuid::Uuid;

    const TEST_DEADLINE: Duration = Duration::from_secs(60);

    fn library_engine(
        resolver: Arc<DelayedResolver>,
    ) -> (
        PlayerInternal,
        mpsc::UnboundedSender<PlayerCommand>,
        mpsc::UnboundedReceiver<PlayerEvent>,
    ) {
        let (commands_sender, commands) = mpsc::unbounded_channel();
        let (event_sender, events) = mpsc::unbounded_channel();
        let internal = PlayerInternal::new(
            commands,
            event_sender,
            CancellationToken::new(),
            commands_sender.clone(),
            Arc::new(FsIo::new_noop()),
            PcmTap::new(),
            Arc::new(RwLock::new(Some(resolver))),
        );
        (internal, commands_sender, events)
    }

    fn library_item(library_id: Uuid, file_uuid: Uuid) -> PlayingItem {
        PlayingItem::LibraryTrack {
            library_id,
            track: file_uuid.to_string(),
        }
    }

    fn library_tracks(library_id: Uuid, file_uuids: &[Uuid]) -> Vec<(PlayingItem, PlaybackSource)> {
        file_uuids
            .iter()
            .map(|file_uuid| {
                (
                    library_item(library_id, *file_uuid),
                    PlaybackSource::LibraryStream,
                )
            })
            .collect()
    }

    fn add_library_tracks(library_id: Uuid, file_uuids: &[Uuid]) -> PlayerCommand {
        PlayerCommand::AddToPlaylist {
            tracks: library_tracks(library_id, file_uuids),
            mode: AddMode::AppendToEnd,
        }
    }

    fn completed_command(
        commands_sender: &mpsc::UnboundedSender<PlayerCommand>,
        command: PlayerCommand,
    ) -> oneshot::Receiver<std::result::Result<(), PlayerCommandError>> {
        let (response, receiver) = oneshot::channel();
        commands_sender
            .send(PlayerCommand::WithCompletion {
                command: Box::new(command),
                response,
            })
            .expect("player command receiver is alive");
        receiver
    }

    #[test]
    fn handoff_tags_propagate_only_inside_the_block_and_follow_queue_edits() -> Result<()> {
        let (commands_sender, commands) = mpsc::unbounded_channel();
        let (event_sender, _events) = mpsc::unbounded_channel();
        let mut internal = PlayerInternal::new(
            commands,
            event_sender,
            CancellationToken::new(),
            commands_sender,
            Arc::new(FsIo::new_noop()),
            PcmTap::new(),
            Arc::new(RwLock::new(None)),
        );
        let library_id = Uuid::new_v4();
        let block = HandoffBlockId(Uuid::new_v4());
        let ids = (0..6).map(|_| Uuid::new_v4()).collect::<Vec<_>>();
        internal.playlist = ids[..4]
            .iter()
            .enumerate()
            .map(|(index, id)| PlaylistItem {
                item: library_item(library_id, *id),
                source: PlaybackSource::LibraryStream,
                block: (index < 3).then_some(block),
            })
            .collect();
        internal.current_item = Some(library_item(library_id, ids[1]));
        internal.current_track_index = Some(1);
        internal.state = InternalPlaybackState::Paused;

        internal.add_to_playlist_tagged(
            library_tracks(library_id, &[ids[4]]),
            AddMode::PlayNext,
            None,
        );
        assert_eq!(internal.playlist[2].block, Some(block));
        assert_eq!(internal.playlist[4].block, None);

        internal.current_item = Some(library_item(library_id, ids[2]));
        internal.current_track_index = Some(3);
        internal.add_to_playlist_tagged(
            library_tracks(library_id, &[ids[5]]),
            AddMode::PlayNext,
            None,
        );
        assert_eq!(internal.playlist[4].block, Some(block));
        assert_eq!(
            inherited_block_for_insert(&internal.playlist[..5], 5, internal.current_item.as_ref(),),
            Some(block)
        );

        let moved = internal.playlist[0].clone();
        internal.move_playlist_item(0, 5);
        assert_eq!(internal.playlist[5].item, moved.item);
        assert_eq!(internal.playlist[5].block, moved.block);
        internal.remove_from_playlist(5)?;

        internal.clear_playlist()?;
        internal.add_to_playlist_tagged(
            library_tracks(library_id, &[ids[0], ids[1]]),
            AddMode::AppendToEnd,
            None,
        );
        assert!(internal.playlist.iter().all(|item| item.block.is_none()));
        Ok(())
    }

    // Waits for the next event that brings a track up, plays it or pauses it.
    async fn next_track_state(
        events: &mut mpsc::UnboundedReceiver<PlayerEvent>,
    ) -> Result<PlayerEvent> {
        tokio::time::timeout(TEST_DEADLINE, async {
            loop {
                match events.recv().await {
                    Some(
                        event @ (PlayerEvent::Loaded { .. }
                        | PlayerEvent::Playing { .. }
                        | PlayerEvent::Paused { .. }),
                    ) => return Ok(event),
                    Some(_) => {}
                    None => bail!("player events closed"),
                }
            }
        })
        .await?
    }

    // Samples in a five-second track, long enough for positions to mean something.
    const LONG_TRACK_SAMPLES: u32 = 5 * 8_000;

    fn position_of(event: &PlayerEvent) -> Duration {
        match event {
            PlayerEvent::Loaded { position, .. }
            | PlayerEvent::Playing { position, .. }
            | PlayerEvent::Paused { position, .. } => *position,
            _ => panic!("{event:?} has no position"),
        }
    }

    // The output thread may have moved on by a few milliseconds when a position is read.
    fn assert_about(position: Duration, expected: Duration) {
        assert!(
            position + Duration::from_millis(50) >= expected
                && position <= expected + Duration::from_millis(300),
            "{position:?} is not about {expected:?}"
        );
    }

    async fn expect_playing(
        events: &mut mpsc::UnboundedReceiver<PlayerEvent>,
        track: &PlayingItem,
    ) -> Result<Duration> {
        let event = next_track_state(events).await?;
        assert!(
            matches!(&event, PlayerEvent::Playing { item, .. } if item == track),
            "{event:?}"
        );
        Ok(position_of(&event))
    }

    async fn expect_paused(
        events: &mut mpsc::UnboundedReceiver<PlayerEvent>,
        track: &PlayingItem,
    ) -> Result<Duration> {
        let event = next_track_state(events).await?;
        assert!(
            matches!(&event, PlayerEvent::Paused { item, .. } if item == track),
            "{event:?}"
        );
        Ok(position_of(&event))
    }

    // Waits for the next progress report of a loaded track and returns its position.
    async fn next_progress(events: &mut mpsc::UnboundedReceiver<PlayerEvent>) -> Result<Duration> {
        tokio::time::timeout(TEST_DEADLINE, async {
            loop {
                match events.recv().await {
                    Some(PlayerEvent::Progress {
                        position,
                        ready: true,
                        ..
                    }) => return Ok(position),
                    Some(_) => {}
                    None => bail!("player events closed"),
                }
            }
        })
        .await?
    }

    async fn progress_at_or_after(
        events: &mut mpsc::UnboundedReceiver<PlayerEvent>,
        target: Duration,
    ) -> Result<Duration> {
        loop {
            let position = next_progress(events).await?;
            if position >= target {
                return Ok(position);
            }
        }
    }

    async fn wait_for_output_reopen(suspends: &AtomicUsize, resumes: &AtomicUsize) -> Result<()> {
        tokio::time::timeout(TEST_DEADLINE, async {
            while suspends.load(Ordering::SeqCst) == 0 || resumes.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        Ok(())
    }

    // An engine whose tracks are five seconds long and load at once.
    fn long_track_engine(
        file_uuids: &[Uuid],
    ) -> (
        PlayerInternal,
        mpsc::UnboundedSender<PlayerCommand>,
        mpsc::UnboundedReceiver<PlayerEvent>,
        PathBuf,
    ) {
        let (resolver, wav_path) = delayed_resolver_fixture_with_samples(
            file_uuids,
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Notify::new()),
            Arc::new(Notify::new()),
            &[],
            LONG_TRACK_SAMPLES,
        );
        let (internal, commands_sender, events) = library_engine(resolver);
        (internal, commands_sender, events, wav_path)
    }

    // Queues one track and waits until playback has made measurable progress.
    async fn start_playing(
        commands_sender: &mpsc::UnboundedSender<PlayerCommand>,
        events: &mut mpsc::UnboundedReceiver<PlayerEvent>,
        file_uuid: Uuid,
    ) -> Result<PlayingItem> {
        let library_id = Uuid::new_v4();
        let track = library_item(library_id, file_uuid);
        commands_sender.send(add_library_tracks(library_id, &[file_uuid]))?;
        commands_sender.send(PlayerCommand::Switch(0))?;
        expect_playing(events, &track).await?;
        progress_at_or_after(events, Duration::from_millis(300)).await?;
        Ok(track)
    }

    #[tokio::test]
    async fn playlist_edit_uses_current_index_when_the_command_runs() -> Result<()> {
        let file_uuids = (0..8).map(|_| Uuid::new_v4()).collect::<Vec<_>>();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&file_uuids);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let original = library_tracks(library_id, &file_uuids[..5]);
                    commands_sender.send(PlayerCommand::AddToPlaylist {
                        tracks: original,
                        mode: AddMode::AppendToEnd,
                    })?;
                    commands_sender.send(PlayerCommand::Switch(2))?;
                    let target_c = library_item(library_id, file_uuids[2]);
                    expect_playing(&mut events, &target_c).await?;
                    let snapshot_index = 2;

                    commands_sender.send(PlayerCommand::Switch(3))?;
                    let target_d = library_item(library_id, file_uuids[3]);
                    expect_playing(&mut events, &target_d).await?;
                    assert_ne!(snapshot_index, 3);

                    commands_sender.send(PlayerCommand::EditPlaylist(PlaylistEdit {
                        tracks: library_tracks(library_id, &file_uuids[5..8]),
                        placement: PlaylistPlacement::PlayNext,
                        playback_mode: None,
                        block: None,
                        load: Some(PlaylistLoad {
                            start_index: 1,
                            position: Duration::from_secs(1),
                            play: false,
                        }),
                    }))?;
                    let loaded = next_track_state(&mut events).await?;
                    assert!(
                        matches!(
                            loaded,
                        PlayerEvent::Loaded { ref item, index: 5, .. }
                            if *item == library_item(library_id, file_uuids[6])
                        ),
                        "{loaded:?}"
                    );

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn play_next_edit_appends_and_starts_when_stopped_with_a_stale_index() -> Result<()> {
        let file_uuids = (0..4).map(|_| Uuid::new_v4()).collect::<Vec<_>>();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&file_uuids);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    commands_sender.send(PlayerCommand::AddToPlaylist {
                        tracks: library_tracks(library_id, &file_uuids[..3]),
                        mode: AddMode::AppendToEnd,
                    })?;
                    commands_sender.send(PlayerCommand::Switch(1))?;
                    expect_playing(&mut events, &library_item(library_id, file_uuids[1])).await?;
                    commands_sender.send(PlayerCommand::Stop)?;

                    commands_sender.send(PlayerCommand::EditPlaylist(PlaylistEdit {
                        tracks: library_tracks(library_id, &file_uuids[3..]),
                        placement: PlaylistPlacement::PlayNext,
                        playback_mode: None,
                        block: None,
                        load: None,
                    }))?;
                    let started = next_track_state(&mut events).await?;
                    assert!(
                        matches!(started, PlayerEvent::Playing { ref item, index: 3, .. }
                            if *item == library_item(library_id, file_uuids[3])),
                        "{started:?}"
                    );

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    async fn assert_nothing_plays(
        commands_sender: &mpsc::UnboundedSender<PlayerCommand>,
        events: &mut mpsc::UnboundedReceiver<PlayerEvent>,
    ) -> Result<()> {
        commands_sender.send(PlayerCommand::SetVolume(0.5))?;
        tokio::time::timeout(TEST_DEADLINE, async {
            loop {
                match events.recv().await {
                    Some(PlayerEvent::VolumeUpdate(_)) => break,
                    Some(PlayerEvent::Playing { .. }) => panic!("unexpected playback event"),
                    Some(_) => {}
                    None => panic!("player events closed"),
                }
            }
        })
        .await?;
        tokio::time::sleep(Duration::from_millis(150)).await;
        while let Ok(event) = events.try_recv() {
            assert!(!matches!(event, PlayerEvent::Playing { .. }), "{event:?}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn pause_during_load_brings_the_track_up_paused() -> Result<()> {
        let file_uuids = [Uuid::new_v4()];
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (resolver, wav_path) = delayed_resolver_fixture(
            &file_uuids,
            Arc::new(Mutex::new(Vec::new())),
            started.clone(),
            release.clone(),
            &[1],
        );
        let (mut internal, commands_sender, mut events) = library_engine(resolver);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let expected = library_item(library_id, file_uuids[0]);
                    commands_sender.send(add_library_tracks(library_id, &file_uuids))?;
                    commands_sender.send(PlayerCommand::Switch(0))?;
                    tokio::time::timeout(TEST_DEADLINE, started.notified()).await?;
                    commands_sender.send(PlayerCommand::Pause)?;
                    release.notify_one();

                    let event = next_track_state(&mut events).await?;
                    assert!(
                        matches!(&event, PlayerEvent::Loaded { item, .. } if *item == expected),
                        "{event:?}"
                    );
                    assert_nothing_plays(&commands_sender, &mut events).await?;

                    // Play loads the track again to open its output, and the resolver holds it.
                    commands_sender.send(PlayerCommand::Play)?;
                    release.notify_one();
                    let event = next_track_state(&mut events).await?;
                    assert!(
                        matches!(&event, PlayerEvent::Playing { item, .. } if *item == expected),
                        "{event:?}"
                    );

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn pause_during_load_also_pauses_the_queued_track() -> Result<()> {
        let file_uuids = [Uuid::new_v4(), Uuid::new_v4()];
        let ids = Arc::new(Mutex::new(Vec::new()));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (resolver, wav_path) = delayed_resolver_fixture(
            &file_uuids,
            ids.clone(),
            started.clone(),
            release.clone(),
            &[1],
        );
        let (mut internal, commands_sender, mut events) = library_engine(resolver);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let expected = library_item(library_id, file_uuids[1]);
                    commands_sender.send(add_library_tracks(library_id, &file_uuids))?;
                    commands_sender.send(PlayerCommand::Switch(0))?;
                    tokio::time::timeout(TEST_DEADLINE, started.notified()).await?;
                    commands_sender.send(PlayerCommand::Switch(1))?;
                    commands_sender.send(PlayerCommand::Pause)?;
                    release.notify_one();

                    let event = next_track_state(&mut events).await?;
                    assert!(
                        matches!(&event, PlayerEvent::Loaded { item, .. } if *item == expected),
                        "{event:?}"
                    );
                    assert_nothing_plays(&commands_sender, &mut events).await?;
                    assert_eq!(&*ids.lock().unwrap(), &[1, 2]);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn pause_while_switching_tracks_brings_the_new_track_up_paused() -> Result<()> {
        let file_uuids = [Uuid::new_v4(), Uuid::new_v4()];
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (resolver, wav_path) = delayed_resolver_fixture_with_samples(
            &file_uuids,
            Arc::new(Mutex::new(Vec::new())),
            started.clone(),
            release.clone(),
            &[2],
            LONG_TRACK_SAMPLES,
        );
        let (mut internal, commands_sender, mut events) = library_engine(resolver);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let current = library_item(library_id, file_uuids[0]);
                    let expected = library_item(library_id, file_uuids[1]);
                    commands_sender.send(add_library_tracks(library_id, &file_uuids))?;
                    commands_sender.send(PlayerCommand::Switch(0))?;
                    expect_playing(&mut events, &current).await?;

                    commands_sender.send(PlayerCommand::Switch(1))?;
                    tokio::time::timeout(TEST_DEADLINE, started.notified()).await?;
                    commands_sender.send(PlayerCommand::Pause)?;
                    let event = next_track_state(&mut events).await?;
                    assert!(
                        matches!(&event, PlayerEvent::Paused { item, .. } if *item == current),
                        "{event:?}"
                    );
                    release.notify_one();

                    let event = next_track_state(&mut events).await?;
                    assert!(
                        matches!(&event, PlayerEvent::Loaded { item, .. } if *item == expected),
                        "{event:?}"
                    );
                    assert_nothing_plays(&commands_sender, &mut events).await?;

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn play_after_pause_during_load_plays_the_loading_track() -> Result<()> {
        let file_uuids = [Uuid::new_v4(), Uuid::new_v4()];
        let ids = Arc::new(Mutex::new(Vec::new()));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (resolver, wav_path) = delayed_resolver_fixture(
            &file_uuids,
            ids.clone(),
            started.clone(),
            release.clone(),
            &[2],
        );
        let (mut internal, commands_sender, mut events) = library_engine(resolver);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let expected = library_item(library_id, file_uuids[1]);
                    commands_sender.send(add_library_tracks(library_id, &file_uuids))?;
                    commands_sender.send(PlayerCommand::Switch(1))?;
                    tokio::time::timeout(TEST_DEADLINE, started.notified()).await?;
                    commands_sender.send(PlayerCommand::Pause)?;
                    commands_sender.send(PlayerCommand::Play)?;
                    release.notify_one();

                    let event = next_track_state(&mut events).await?;
                    assert!(
                        matches!(&event, PlayerEvent::Playing { item, .. } if *item == expected),
                        "{event:?}"
                    );
                    assert_eq!(&*ids.lock().unwrap(), &[2]);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn play_during_load_keeps_the_requested_track() -> Result<()> {
        let file_uuids = [Uuid::new_v4(), Uuid::new_v4()];
        let ids = Arc::new(Mutex::new(Vec::new()));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (resolver, wav_path) = delayed_resolver_fixture(
            &file_uuids,
            ids.clone(),
            started.clone(),
            release.clone(),
            &[2],
        );
        let (mut internal, commands_sender, mut events) = library_engine(resolver);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let expected = library_item(library_id, file_uuids[1]);
                    commands_sender.send(add_library_tracks(library_id, &file_uuids))?;
                    commands_sender.send(PlayerCommand::Switch(1))?;
                    commands_sender.send(PlayerCommand::Play)?;
                    tokio::time::timeout(TEST_DEADLINE, started.notified()).await?;
                    release.notify_one();

                    let event = next_track_state(&mut events).await?;
                    assert!(
                        matches!(&event, PlayerEvent::Playing { item, .. } if *item == expected),
                        "{event:?}"
                    );
                    assert_eq!(&*ids.lock().unwrap(), &[2]);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn load_without_pause_still_plays() -> Result<()> {
        let file_uuids = [Uuid::new_v4()];
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (resolver, wav_path) = delayed_resolver_fixture(
            &file_uuids,
            Arc::new(Mutex::new(Vec::new())),
            started.clone(),
            release.clone(),
            &[1],
        );
        let (mut internal, commands_sender, mut events) = library_engine(resolver);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let expected = library_item(library_id, file_uuids[0]);
                    commands_sender.send(add_library_tracks(library_id, &file_uuids))?;
                    commands_sender.send(PlayerCommand::Switch(0))?;
                    tokio::time::timeout(TEST_DEADLINE, started.notified()).await?;
                    release.notify_one();

                    let event = next_track_state(&mut events).await?;
                    assert!(
                        matches!(&event, PlayerEvent::Playing { item, .. } if *item == expected),
                        "{event:?}"
                    );

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn old_track_ending_during_a_load_keeps_the_chosen_track() -> Result<()> {
        let file_uuids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
        let ids = Arc::new(Mutex::new(Vec::new()));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (resolver, wav_path) = delayed_resolver_fixture_with_samples(
            &file_uuids,
            ids.clone(),
            started.clone(),
            release.clone(),
            &[3],
            LONG_TRACK_SAMPLES,
        );
        let (mut internal, commands_sender, mut events) = library_engine(resolver);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let current = library_item(library_id, file_uuids[0]);
                    let expected = library_item(library_id, file_uuids[2]);
                    commands_sender.send(add_library_tracks(library_id, &file_uuids))?;
                    commands_sender.send(PlayerCommand::Switch(0))?;
                    expect_playing(&mut events, &current).await?;

                    // The old track plays on while the chosen one loads, and runs out first.
                    commands_sender.send(PlayerCommand::Switch(2))?;
                    tokio::time::timeout(TEST_DEADLINE, started.notified()).await?;
                    commands_sender.send(PlayerCommand::Seek(4.9))?;
                    progress_at_or_after(&mut events, Duration::from_secs(5)).await?;
                    release.notify_one();

                    let event = next_track_state(&mut events).await?;
                    assert!(
                        matches!(&event, PlayerEvent::Playing { item, .. } if *item == expected),
                        "{event:?}"
                    );
                    assert_eq!(&*ids.lock().unwrap(), &[1, 3]);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn track_end_advances_once_while_the_next_track_loads() -> Result<()> {
        let file_uuids = [Uuid::new_v4(), Uuid::new_v4()];
        let ids = Arc::new(Mutex::new(Vec::new()));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (resolver, wav_path) = delayed_resolver_fixture(
            &file_uuids,
            ids.clone(),
            started.clone(),
            release.clone(),
            &[2],
        );
        let (mut internal, commands_sender, mut events) = library_engine(resolver);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let first = library_item(library_id, file_uuids[0]);
                    let second = library_item(library_id, file_uuids[1]);
                    commands_sender.send(add_library_tracks(library_id, &file_uuids))?;
                    commands_sender.send(PlayerCommand::Switch(0))?;
                    tokio::time::timeout(TEST_DEADLINE, started.notified()).await?;
                    release.notify_one();

                    let mut ends = 0;
                    tokio::time::timeout(TEST_DEADLINE, async {
                        loop {
                            match events.recv().await {
                                Some(PlayerEvent::EndOfTrack { item, .. }) if item == first => {
                                    ends += 1
                                }
                                Some(PlayerEvent::Playing { item, .. }) if item == second => break,
                                Some(_) => {}
                                None => panic!("player events closed"),
                            }
                        }
                        Ok::<(), anyhow::Error>(())
                    })
                    .await??;
                    assert_eq!(ends, 1);
                    assert_eq!(&*ids.lock().unwrap(), &[1, 2]);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn restore_position_is_not_applied_to_a_newer_track() -> Result<()> {
        let file_uuids = [Uuid::new_v4(), Uuid::new_v4()];
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (resolver, wav_path) = delayed_resolver_fixture_with_samples(
            &file_uuids,
            Arc::new(Mutex::new(Vec::new())),
            started.clone(),
            release.clone(),
            &[1],
            LONG_TRACK_SAMPLES,
        );
        let (mut internal, commands_sender, mut events) = library_engine(resolver);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let expected = library_item(library_id, file_uuids[1]);
                    commands_sender.send(PlayerCommand::Restore {
                        tracks: library_tracks(library_id, &file_uuids),
                        index: Some(0),
                        position: Duration::from_secs(1),
                        mode: PlaybackMode::Sequential,
                    })?;
                    tokio::time::timeout(TEST_DEADLINE, started.notified()).await?;
                    commands_sender.send(PlayerCommand::Load { index: 1 })?;
                    release.notify_one();

                    let event = next_track_state(&mut events).await?;
                    assert!(
                        matches!(&event, PlayerEvent::Loaded { item, position, .. }
                            if *item == expected && position.is_zero()),
                        "{event:?}"
                    );

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn restore_during_load_keeps_its_position_after_the_hand_off() -> Result<()> {
        let file_uuids = [Uuid::new_v4(), Uuid::new_v4()];
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (resolver, wav_path) = delayed_resolver_fixture_with_samples(
            &file_uuids,
            Arc::new(Mutex::new(Vec::new())),
            started.clone(),
            release.clone(),
            &[1],
            LONG_TRACK_SAMPLES,
        );
        let (mut internal, commands_sender, mut events) = library_engine(resolver);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let expected = library_item(library_id, file_uuids[1]);
                    commands_sender.send(add_library_tracks(library_id, &file_uuids[..1]))?;
                    commands_sender.send(PlayerCommand::Switch(0))?;
                    tokio::time::timeout(TEST_DEADLINE, started.notified()).await?;
                    commands_sender.send(PlayerCommand::Restore {
                        tracks: library_tracks(library_id, &file_uuids[1..]),
                        index: Some(1),
                        position: Duration::from_secs(1),
                        mode: PlaybackMode::Sequential,
                    })?;
                    release.notify_one();

                    let event = next_track_state(&mut events).await?;
                    assert!(
                        matches!(&event, PlayerEvent::Loaded { item, .. } if *item == expected),
                        "{event:?}"
                    );
                    assert_about(position_of(&event), Duration::from_secs(1));

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn interruption_that_ends_with_resume_plays_on_from_the_same_place() -> Result<()> {
        let file_uuid = Uuid::new_v4();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&[file_uuid]);
        let opens = internal.output_opens.clone();
        let resumes = internal.output_resumes.clone();
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let track = start_playing(&commands_sender, &mut events, file_uuid).await?;
                    commands_sender.send(PlayerCommand::Output(OutputEvent::Interrupted))?;
                    let paused_at = expect_paused(&mut events, &track).await?;
                    assert!(paused_at >= Duration::from_millis(300), "{paused_at:?}");

                    // The same output starts again, so the track is not loaded again.
                    commands_sender.send(PlayerCommand::Output(
                        OutputEvent::InterruptionEnded { resume: true },
                    ))?;
                    assert_about(expect_playing(&mut events, &track).await?, paused_at);
                    assert_eq!(opens.load(Ordering::SeqCst), 1);
                    assert_eq!(resumes.load(Ordering::SeqCst), 1);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn interruption_that_ends_without_resume_stays_paused_until_play() -> Result<()> {
        let file_uuid = Uuid::new_v4();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&[file_uuid]);
        let opens = internal.output_opens.clone();
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let track = start_playing(&commands_sender, &mut events, file_uuid).await?;
                    commands_sender.send(PlayerCommand::Output(OutputEvent::Interrupted))?;
                    let paused_at = expect_paused(&mut events, &track).await?;
                    commands_sender.send(PlayerCommand::Output(
                        OutputEvent::InterruptionEnded { resume: false },
                    ))?;
                    assert_nothing_plays(&commands_sender, &mut events).await?;
                    assert_eq!(opens.load(Ordering::SeqCst), 1);

                    commands_sender.send(PlayerCommand::Play)?;
                    assert_about(expect_playing(&mut events, &track).await?, paused_at);
                    assert_eq!(opens.load(Ordering::SeqCst), 1);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn paused_track_starts_its_output_again_on_play_after_an_interruption() -> Result<()> {
        let file_uuid = Uuid::new_v4();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&[file_uuid]);
        let opens = internal.output_opens.clone();
        let resumes = internal.output_resumes.clone();
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let track = start_playing(&commands_sender, &mut events, file_uuid).await?;
                    commands_sender.send(PlayerCommand::Pause)?;
                    let paused_at = expect_paused(&mut events, &track).await?;
                    commands_sender.send(PlayerCommand::Output(OutputEvent::Interrupted))?;
                    commands_sender.send(PlayerCommand::Output(
                        OutputEvent::InterruptionEnded { resume: true },
                    ))?;
                    assert_nothing_plays(&commands_sender, &mut events).await?;
                    assert_eq!(opens.load(Ordering::SeqCst), 1);

                    commands_sender.send(PlayerCommand::Play)?;
                    assert_about(expect_playing(&mut events, &track).await?, paused_at);
                    assert_eq!(opens.load(Ordering::SeqCst), 1);
                    assert_eq!(resumes.load(Ordering::SeqCst), 1);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn device_lost_pauses_and_cancels_the_resume_after_an_interruption() -> Result<()> {
        let file_uuid = Uuid::new_v4();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&[file_uuid]);
        let opens = internal.output_opens.clone();
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let track = start_playing(&commands_sender, &mut events, file_uuid).await?;
                    commands_sender.send(PlayerCommand::Output(OutputEvent::DeviceLost))?;
                    let paused_at = expect_paused(&mut events, &track).await?;
                    commands_sender.send(PlayerCommand::Play)?;
                    assert_about(expect_playing(&mut events, &track).await?, paused_at);

                    commands_sender.send(PlayerCommand::Output(OutputEvent::Interrupted))?;
                    expect_paused(&mut events, &track).await?;
                    commands_sender.send(PlayerCommand::Output(OutputEvent::DeviceLost))?;
                    commands_sender.send(PlayerCommand::Output(
                        OutputEvent::InterruptionEnded { resume: true },
                    ))?;
                    assert_nothing_plays(&commands_sender, &mut events).await?;
                    assert_eq!(opens.load(Ordering::SeqCst), 2);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn device_lost_during_a_load_brings_the_track_up_paused() -> Result<()> {
        let file_uuids = [Uuid::new_v4()];
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (resolver, wav_path) = delayed_resolver_fixture_with_samples(
            &file_uuids,
            Arc::new(Mutex::new(Vec::new())),
            started.clone(),
            release.clone(),
            &[1],
            LONG_TRACK_SAMPLES,
        );
        let (mut internal, commands_sender, mut events) = library_engine(resolver);
        let opens = internal.output_opens.clone();
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let track = library_item(library_id, file_uuids[0]);
                    commands_sender.send(add_library_tracks(library_id, &file_uuids))?;
                    commands_sender.send(PlayerCommand::Switch(0))?;
                    tokio::time::timeout(TEST_DEADLINE, started.notified()).await?;
                    commands_sender.send(PlayerCommand::Output(OutputEvent::DeviceLost))?;
                    release.notify_one();

                    let event = next_track_state(&mut events).await?;
                    assert!(
                        matches!(&event, PlayerEvent::Loaded { item, .. } if *item == track),
                        "{event:?}"
                    );
                    assert_nothing_plays(&commands_sender, &mut events).await?;

                    // Play loads the track again, which the resolver holds, and opens the only
                    // output.
                    assert_eq!(opens.load(Ordering::SeqCst), 0);
                    commands_sender.send(PlayerCommand::Play)?;
                    release.notify_one();
                    expect_playing(&mut events, &track).await?;
                    assert_eq!(opens.load(Ordering::SeqCst), 1);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn device_added_while_playing_moves_playback_in_place() -> Result<()> {
        let file_uuid = Uuid::new_v4();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&[file_uuid]);
        let opens = internal.output_opens.clone();
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let track = start_playing(&commands_sender, &mut events, file_uuid).await?;
                    let playing_at = next_progress(&mut events).await?;
                    commands_sender.send(PlayerCommand::Output(OutputEvent::DeviceAdded))?;
                    assert_about(expect_playing(&mut events, &track).await?, playing_at);
                    assert_eq!(opens.load(Ordering::SeqCst), 2);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn device_added_while_paused_waits_for_play() -> Result<()> {
        let file_uuid = Uuid::new_v4();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&[file_uuid]);
        let opens = internal.output_opens.clone();
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let track = start_playing(&commands_sender, &mut events, file_uuid).await?;
                    commands_sender.send(PlayerCommand::Pause)?;
                    let paused_at = expect_paused(&mut events, &track).await?;
                    commands_sender.send(PlayerCommand::Output(OutputEvent::DeviceAdded))?;
                    assert_nothing_plays(&commands_sender, &mut events).await?;
                    assert_eq!(opens.load(Ordering::SeqCst), 1);

                    commands_sender.send(PlayerCommand::Play)?;
                    assert_about(expect_playing(&mut events, &track).await?, paused_at);
                    assert_eq!(opens.load(Ordering::SeqCst), 2);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn failed_open_keeps_the_track_paused_in_place() -> Result<()> {
        let file_uuid = Uuid::new_v4();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&[file_uuid]);
        let opens = internal.output_opens.clone();
        let failing_opens = internal.failing_opens.clone();
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let track = start_playing(&commands_sender, &mut events, file_uuid).await?;
                    let playing_at = next_progress(&mut events).await?;
                    failing_opens.store(1, Ordering::SeqCst);
                    commands_sender.send(PlayerCommand::Output(OutputEvent::DeviceAdded))?;
                    let paused_at = expect_loaded(&mut events, &track).await?;
                    assert_about(paused_at, playing_at);

                    // The player still answers, and reports no progress without an output.
                    commands_sender.send(PlayerCommand::SetVolume(0.5))?;
                    tokio::time::timeout(TEST_DEADLINE, async {
                        loop {
                            match events.recv().await {
                                Some(PlayerEvent::VolumeUpdate(_)) => break,
                                Some(_) => {}
                                None => panic!("player events closed"),
                            }
                        }
                    })
                    .await?;
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    while let Ok(event) = events.try_recv() {
                        assert!(!matches!(event, PlayerEvent::Progress { .. }), "{event:?}");
                    }

                    commands_sender.send(PlayerCommand::Play)?;
                    assert_about(expect_playing(&mut events, &track).await?, paused_at);
                    assert_eq!(opens.load(Ordering::SeqCst), 3);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn stream_error_while_playing_pauses_in_place() -> Result<()> {
        let file_uuid = Uuid::new_v4();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&[file_uuid]);
        let opens = internal.output_opens.clone();
        let stream_errors = internal.stream_error_sender.clone();
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let track = start_playing(&commands_sender, &mut events, file_uuid).await?;
                    let playing_at = next_progress(&mut events).await?;
                    // A failing stream tends to say so more than once.
                    stream_errors.send("the device is no longer available".into())?;
                    stream_errors.send("the device is no longer available".into())?;
                    let paused_at = expect_paused(&mut events, &track).await?;
                    assert_about(paused_at, playing_at);
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    while let Ok(event) = events.try_recv() {
                        assert!(
                            !matches!(
                                event,
                                PlayerEvent::Playing { .. } | PlayerEvent::Paused { .. }
                            ),
                            "{event:?}"
                        );
                    }
                    assert_eq!(opens.load(Ordering::SeqCst), 1);

                    commands_sender.send(PlayerCommand::Play)?;
                    assert_about(expect_playing(&mut events, &track).await?, paused_at);
                    assert_eq!(opens.load(Ordering::SeqCst), 2);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn seek_while_the_output_is_stale_resumes_there() -> Result<()> {
        let file_uuid = Uuid::new_v4();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&[file_uuid]);
        let opens = internal.output_opens.clone();
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let track = start_playing(&commands_sender, &mut events, file_uuid).await?;
                    commands_sender.send(PlayerCommand::Output(OutputEvent::DeviceLost))?;
                    expect_paused(&mut events, &track).await?;
                    commands_sender.send(PlayerCommand::Seek(2.0))?;
                    assert_about(
                        expect_paused(&mut events, &track).await?,
                        Duration::from_secs(2),
                    );

                    commands_sender.send(PlayerCommand::Play)?;
                    assert_about(
                        expect_playing(&mut events, &track).await?,
                        Duration::from_secs(2),
                    );
                    assert_eq!(opens.load(Ordering::SeqCst), 2);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn seek_during_an_interruption_resumes_there() -> Result<()> {
        let file_uuid = Uuid::new_v4();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&[file_uuid]);
        let opens = internal.output_opens.clone();
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let track = start_playing(&commands_sender, &mut events, file_uuid).await?;
                    commands_sender.send(PlayerCommand::Output(OutputEvent::Interrupted))?;
                    expect_paused(&mut events, &track).await?;
                    commands_sender.send(PlayerCommand::Seek(2.0))?;
                    assert_about(
                        expect_paused(&mut events, &track).await?,
                        Duration::from_secs(2),
                    );

                    commands_sender.send(PlayerCommand::Output(
                        OutputEvent::InterruptionEnded { resume: true },
                    ))?;
                    assert_about(
                        expect_playing(&mut events, &track).await?,
                        Duration::from_secs(2),
                    );
                    assert_eq!(opens.load(Ordering::SeqCst), 1);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn failed_resume_after_an_interruption_opens_a_new_output_in_place() -> Result<()> {
        let file_uuid = Uuid::new_v4();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&[file_uuid]);
        let opens = internal.output_opens.clone();
        let resumes = internal.output_resumes.clone();
        let failing_resumes = internal.failing_resumes.clone();
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let track = start_playing(&commands_sender, &mut events, file_uuid).await?;
                    commands_sender.send(PlayerCommand::Output(OutputEvent::Interrupted))?;
                    let paused_at = expect_paused(&mut events, &track).await?;
                    failing_resumes.store(1, Ordering::SeqCst);

                    commands_sender.send(PlayerCommand::Output(
                        OutputEvent::InterruptionEnded { resume: true },
                    ))?;
                    assert_about(expect_playing(&mut events, &track).await?, paused_at);
                    assert_eq!(resumes.load(Ordering::SeqCst), 1);
                    assert_eq!(opens.load(Ordering::SeqCst), 2);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn interruption_during_a_paused_load_pauses_the_old_track_for_good() -> Result<()> {
        let file_uuids = [Uuid::new_v4(), Uuid::new_v4()];
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (resolver, wav_path) = delayed_resolver_fixture_with_samples(
            &file_uuids,
            Arc::new(Mutex::new(Vec::new())),
            started.clone(),
            release.clone(),
            &[2],
            LONG_TRACK_SAMPLES,
        );
        let (mut internal, commands_sender, mut events) = library_engine(resolver);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let current = library_item(library_id, file_uuids[0]);
                    let expected = library_item(library_id, file_uuids[1]);
                    commands_sender.send(add_library_tracks(library_id, &file_uuids))?;
                    commands_sender.send(PlayerCommand::Switch(0))?;
                    expect_playing(&mut events, &current).await?;

                    // The old track plays on while the next one loads paused.
                    commands_sender.send(PlayerCommand::Load { index: 1 })?;
                    tokio::time::timeout(TEST_DEADLINE, started.notified()).await?;
                    commands_sender.send(PlayerCommand::Output(OutputEvent::Interrupted))?;
                    expect_paused(&mut events, &current).await?;

                    // Nobody wanted playback, so nothing plays after the interruption.
                    commands_sender.send(PlayerCommand::Output(
                        OutputEvent::InterruptionEnded { resume: true },
                    ))?;
                    assert_nothing_plays(&commands_sender, &mut events).await?;
                    release.notify_one();
                    expect_loaded(&mut events, &expected).await?;
                    assert_nothing_plays(&commands_sender, &mut events).await?;

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn load_during_an_interruption_opens_no_output_until_play() -> Result<()> {
        let file_uuid = Uuid::new_v4();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&[file_uuid]);
        let opens = internal.output_opens.clone();
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let track = library_item(library_id, file_uuid);
                    commands_sender.send(PlayerCommand::Output(OutputEvent::Interrupted))?;
                    commands_sender.send(add_library_tracks(library_id, &[file_uuid]))?;
                    commands_sender.send(PlayerCommand::Load { index: 0 })?;
                    let event = next_track_state(&mut events).await?;
                    assert!(
                        matches!(&event, PlayerEvent::Loaded { item, .. } if *item == track),
                        "{event:?}"
                    );
                    assert_eq!(opens.load(Ordering::SeqCst), 0);

                    commands_sender.send(PlayerCommand::Output(
                        OutputEvent::InterruptionEnded { resume: false },
                    ))?;
                    assert_nothing_plays(&commands_sender, &mut events).await?;
                    commands_sender.send(PlayerCommand::Play)?;
                    expect_playing(&mut events, &track).await?;
                    assert_eq!(opens.load(Ordering::SeqCst), 1);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn play_during_an_interruption_cancels_its_resume() -> Result<()> {
        let file_uuid = Uuid::new_v4();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&[file_uuid]);
        let opens = internal.output_opens.clone();
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let track = start_playing(&commands_sender, &mut events, file_uuid).await?;
                    commands_sender.send(PlayerCommand::Output(OutputEvent::Interrupted))?;
                    let paused_at = expect_paused(&mut events, &track).await?;

                    // Play before the end of the interruption came.
                    commands_sender.send(PlayerCommand::Play)?;
                    assert_about(expect_playing(&mut events, &track).await?, paused_at);
                    assert_eq!(opens.load(Ordering::SeqCst), 1);

                    // The track comes up paused again without a Pause, and a late end of the
                    // interruption must not start it.
                    commands_sender.send(PlayerCommand::Load { index: 0 })?;
                    expect_loaded(&mut events, &track).await?;
                    commands_sender.send(PlayerCommand::Output(
                        OutputEvent::InterruptionEnded { resume: true },
                    ))?;
                    assert_nothing_plays(&commands_sender, &mut events).await?;

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn a_track_that_starts_playing_cancels_the_resume_after_an_interruption() -> Result<()> {
        let file_uuids = [Uuid::new_v4(), Uuid::new_v4()];
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&file_uuids);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let first = library_item(library_id, file_uuids[0]);
                    let second = library_item(library_id, file_uuids[1]);
                    commands_sender.send(add_library_tracks(library_id, &file_uuids))?;
                    commands_sender.send(PlayerCommand::Switch(0))?;
                    expect_playing(&mut events, &first).await?;
                    commands_sender.send(PlayerCommand::Output(OutputEvent::Interrupted))?;
                    expect_paused(&mut events, &first).await?;

                    // Another track starts playing before the end of the interruption came.
                    commands_sender.send(PlayerCommand::Switch(1))?;
                    expect_playing(&mut events, &second).await?;
                    commands_sender.send(PlayerCommand::Load { index: 0 })?;
                    expect_loaded(&mut events, &first).await?;
                    commands_sender.send(PlayerCommand::Output(
                        OutputEvent::InterruptionEnded { resume: true },
                    ))?;
                    assert_nothing_plays(&commands_sender, &mut events).await?;

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    fn restore(
        library_id: Uuid,
        file_uuids: &[Uuid],
        index: usize,
        seconds: f64,
        mode: PlaybackMode,
    ) -> PlayerCommand {
        PlayerCommand::Restore {
            tracks: library_tracks(library_id, file_uuids),
            index: Some(index),
            position: Duration::from_secs_f64(seconds),
            mode,
        }
    }

    async fn expect_loaded(
        events: &mut mpsc::UnboundedReceiver<PlayerEvent>,
        track: &PlayingItem,
    ) -> Result<Duration> {
        let event = next_track_state(events).await?;
        assert!(
            matches!(&event, PlayerEvent::Loaded { item, .. } if item == track),
            "{event:?}"
        );
        Ok(position_of(&event))
    }

    #[tokio::test]
    async fn paused_load_opens_no_output_until_play() -> Result<()> {
        let file_uuid = Uuid::new_v4();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&[file_uuid]);
        let opens = internal.output_opens.clone();
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let track = library_item(library_id, file_uuid);
                    commands_sender.send(restore(
                        library_id,
                        &[file_uuid],
                        0,
                        1.5,
                        PlaybackMode::Sequential,
                    ))?;
                    assert_about(
                        expect_loaded(&mut events, &track).await?,
                        Duration::from_millis(1500),
                    );
                    assert_nothing_plays(&commands_sender, &mut events).await?;
                    assert_eq!(opens.load(Ordering::SeqCst), 0);

                    commands_sender.send(PlayerCommand::Play)?;
                    assert_about(
                        expect_playing(&mut events, &track).await?,
                        Duration::from_millis(1500),
                    );
                    assert_eq!(opens.load(Ordering::SeqCst), 1);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn seek_before_the_first_play_is_where_playback_starts() -> Result<()> {
        let file_uuid = Uuid::new_v4();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&[file_uuid]);
        let opens = internal.output_opens.clone();
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let track = library_item(library_id, file_uuid);
                    commands_sender.send(restore(
                        library_id,
                        &[file_uuid],
                        0,
                        1.0,
                        PlaybackMode::Sequential,
                    ))?;
                    expect_loaded(&mut events, &track).await?;
                    commands_sender.send(PlayerCommand::Seek(3.0))?;
                    assert_about(
                        expect_paused(&mut events, &track).await?,
                        Duration::from_secs(3),
                    );
                    assert_eq!(opens.load(Ordering::SeqCst), 0);

                    commands_sender.send(PlayerCommand::Play)?;
                    assert_about(
                        expect_playing(&mut events, &track).await?,
                        Duration::from_secs(3),
                    );
                    assert_eq!(opens.load(Ordering::SeqCst), 1);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn paused_track_reports_that_it_is_ready_where_it_stands() -> Result<()> {
        let file_uuid = Uuid::new_v4();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&[file_uuid]);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let track = library_item(library_id, file_uuid);
                    commands_sender.send(restore(
                        library_id,
                        &[file_uuid],
                        0,
                        2.0,
                        PlaybackMode::Sequential,
                    ))?;
                    expect_loaded(&mut events, &track).await?;
                    // Changing the mode reports progress, which must keep the controls usable.
                    commands_sender
                        .send(PlayerCommand::SetPlaybackMode(PlaybackMode::RepeatAll))?;
                    assert_about(next_progress(&mut events).await?, Duration::from_secs(2));

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn another_paused_load_opens_nothing_and_next_opens_one_output() -> Result<()> {
        let file_uuids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&file_uuids);
        let opens = internal.output_opens.clone();
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    commands_sender.send(restore(
                        library_id,
                        &file_uuids,
                        0,
                        1.0,
                        PlaybackMode::Sequential,
                    ))?;
                    expect_loaded(&mut events, &library_item(library_id, file_uuids[0])).await?;
                    commands_sender.send(PlayerCommand::Load { index: 1 })?;
                    expect_loaded(&mut events, &library_item(library_id, file_uuids[1])).await?;
                    assert_eq!(opens.load(Ordering::SeqCst), 0);

                    commands_sender.send(PlayerCommand::Next)?;
                    expect_playing(&mut events, &library_item(library_id, file_uuids[2])).await?;
                    assert_eq!(opens.load(Ordering::SeqCst), 1);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn previous_restarts_a_paused_track_that_stands_past_three_seconds() -> Result<()> {
        let file_uuids = [Uuid::new_v4(), Uuid::new_v4()];
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&file_uuids);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let track = library_item(library_id, file_uuids[1]);
                    commands_sender.send(PlayerCommand::SetAdaptiveSwitchingEnabled(true))?;
                    commands_sender.send(restore(
                        library_id,
                        &file_uuids,
                        1,
                        4.0,
                        PlaybackMode::Sequential,
                    ))?;
                    expect_loaded(&mut events, &track).await?;
                    commands_sender.send(PlayerCommand::Previous)?;
                    assert_about(expect_playing(&mut events, &track).await?, Duration::ZERO);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn mixing_with_other_apps_is_off_until_asked_and_follows_the_command() -> Result<()> {
        let file_uuid = Uuid::new_v4();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&[file_uuid]);
        assert!(!internal.mix_with_others);

        let cancellation = internal.cancellation_token.clone();
        commands_sender.send(PlayerCommand::SetMixWithOthers(true))?;
        commands_sender.send(PlayerCommand::SetVolume(0.5))?;
        let local = LocalSet::new();
        let internal = local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move {
                    internal.run().await?;
                    Ok::<_, anyhow::Error>(internal)
                });
                tokio::time::timeout(TEST_DEADLINE, async {
                    loop {
                        match events.recv().await {
                            Some(PlayerEvent::VolumeUpdate(_)) => break,
                            Some(_) => {}
                            None => panic!("player events closed"),
                        }
                    }
                })
                .await?;
                cancellation.cancel();
                runner.await?
            })
            .await?;
        assert!(internal.mix_with_others);

        let mut internal = internal;
        internal.set_mix_with_others(false)?;
        assert!(!internal.mix_with_others);
        let _ = std::fs::remove_file(wav_path);
        Ok(())
    }

    #[tokio::test]
    async fn changing_mixing_while_playing_restarts_the_output_where_it_plays() -> Result<()> {
        let file_uuid = Uuid::new_v4();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&[file_uuid]);
        let opens = internal.output_opens.clone();
        let suspends = internal.output_suspends.clone();
        let resumes = internal.output_resumes.clone();
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    start_playing(&commands_sender, &mut events, file_uuid).await?;
                    commands_sender.send(PlayerCommand::SetMixWithOthers(true))?;
                    wait_for_output_reopen(&suspends, &resumes).await?;
                    let before = next_progress(&mut events).await?;
                    assert_eq!(suspends.load(Ordering::SeqCst), 1);
                    assert_eq!(resumes.load(Ordering::SeqCst), 1);

                    // The same output plays on.
                    let later =
                        progress_at_or_after(&mut events, before + Duration::from_millis(300))
                            .await?;
                    assert!(later >= before + Duration::from_millis(300), "{later:?}");
                    assert_eq!(opens.load(Ordering::SeqCst), 1);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn volume_set_before_the_first_play_applies() -> Result<()> {
        let file_uuid = Uuid::new_v4();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&[file_uuid]);
        let script = async {
            let library_id = Uuid::new_v4();
            let track = library_item(library_id, file_uuid);
            commands_sender.send(add_library_tracks(library_id, &[file_uuid]))?;
            commands_sender.send(PlayerCommand::Load { index: 0 })?;
            expect_loaded(&mut events, &track).await?;
            commands_sender.send(PlayerCommand::SetVolume(0.25))?;
            commands_sender.send(PlayerCommand::Play)?;
            expect_playing(&mut events, &track).await?;
            Ok::<(), anyhow::Error>(())
        };
        // The test runs the player itself, so that it can look at the sink afterwards.
        tokio::select! {
            result = internal.run() => bail!("the player stopped: {result:?}"),
            result = script => result?,
        }
        assert_eq!(internal.sink.as_ref().map(Sink::volume), Some(0.25));
        let _ = std::fs::remove_file(wav_path);
        Ok(())
    }

    #[tokio::test]
    async fn pause_stops_the_output_and_play_starts_it_again() -> Result<()> {
        let file_uuid = Uuid::new_v4();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&[file_uuid]);
        let opens = internal.output_opens.clone();
        let suspends = internal.output_suspends.clone();
        let resumes = internal.output_resumes.clone();
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let track = start_playing(&commands_sender, &mut events, file_uuid).await?;
                    assert_eq!(suspends.load(Ordering::SeqCst), 0);
                    commands_sender.send(PlayerCommand::Pause)?;
                    let paused_at = expect_paused(&mut events, &track).await?;
                    assert_eq!(suspends.load(Ordering::SeqCst), 1);
                    assert_eq!(resumes.load(Ordering::SeqCst), 0);

                    // The same output plays on from where the track was paused, and says so.
                    commands_sender.send(PlayerCommand::Play)?;
                    assert_about(expect_playing(&mut events, &track).await?, paused_at);
                    assert_eq!(resumes.load(Ordering::SeqCst), 1);
                    assert_eq!(opens.load(Ordering::SeqCst), 1);
                    assert_about(next_progress(&mut events).await?, paused_at);
                    let later =
                        progress_at_or_after(&mut events, paused_at + Duration::from_millis(300))
                            .await?;
                    assert!(later >= paused_at + Duration::from_millis(300), "{later:?}");

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn seek_while_paused_waits_for_play_to_run_the_output() -> Result<()> {
        let file_uuid = Uuid::new_v4();
        // The player runs on a thread of its own, so that a seek that waits on a stopped output
        // fails the test on a timeout instead of hanging it.
        let (setup_sender, setup) = channel();
        std::thread::spawn(move || {
            let (mut internal, commands_sender, events, wav_path) = long_track_engine(&[file_uuid]);
            let hooks = (
                internal.cancellation_token.clone(),
                internal.output_opens.clone(),
                internal.output_suspends.clone(),
                internal.output_resumes.clone(),
            );
            setup_sender
                .send((commands_sender, events, wav_path, hooks))
                .unwrap();
            let runtime = Builder::new_current_thread().enable_all().build().unwrap();
            let _ = runtime.block_on(internal.run());
        });
        let (commands_sender, mut events, wav_path, (cancel, opens, suspends, resumes)) =
            setup.recv()?;

        let track = start_playing(&commands_sender, &mut events, file_uuid).await?;
        commands_sender.send(PlayerCommand::Pause)?;
        expect_paused(&mut events, &track).await?;
        commands_sender.send(PlayerCommand::Seek(3.0))?;
        assert_about(
            expect_paused(&mut events, &track).await?,
            Duration::from_secs(3),
        );
        // The stopped output stays stopped, and the track reports the place it will play from.
        assert_about(next_progress(&mut events).await?, Duration::from_secs(3));
        assert_eq!(resumes.load(Ordering::SeqCst), 0);
        assert_eq!(suspends.load(Ordering::SeqCst), 1);
        assert_nothing_plays(&commands_sender, &mut events).await?;

        commands_sender.send(PlayerCommand::Play)?;
        assert_about(
            expect_playing(&mut events, &track).await?,
            Duration::from_secs(3),
        );
        assert_about(next_progress(&mut events).await?, Duration::from_secs(3));
        assert_eq!(resumes.load(Ordering::SeqCst), 1);
        assert_eq!(opens.load(Ordering::SeqCst), 1);

        cancel.cancel();
        let _ = std::fs::remove_file(wav_path);
        Ok(())
    }

    #[tokio::test]
    async fn failed_resume_opens_a_new_output_where_the_track_was_paused() -> Result<()> {
        let file_uuid = Uuid::new_v4();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&[file_uuid]);
        let opens = internal.output_opens.clone();
        let resumes = internal.output_resumes.clone();
        let failing_resumes = internal.failing_resumes.clone();
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let track = start_playing(&commands_sender, &mut events, file_uuid).await?;
                    commands_sender.send(PlayerCommand::Pause)?;
                    let paused_at = expect_paused(&mut events, &track).await?;
                    failing_resumes.store(1, Ordering::SeqCst);

                    commands_sender.send(PlayerCommand::Play)?;
                    assert_about(expect_playing(&mut events, &track).await?, paused_at);
                    assert_eq!(resumes.load(Ordering::SeqCst), 1);
                    assert_eq!(opens.load(Ordering::SeqCst), 2);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn pause_and_play_during_a_switch_stop_and_start_the_old_output() -> Result<()> {
        let file_uuids = [Uuid::new_v4(), Uuid::new_v4()];
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (resolver, wav_path) = delayed_resolver_fixture_with_samples(
            &file_uuids,
            Arc::new(Mutex::new(Vec::new())),
            started.clone(),
            release.clone(),
            &[2],
            LONG_TRACK_SAMPLES,
        );
        let (mut internal, commands_sender, mut events) = library_engine(resolver);
        let opens = internal.output_opens.clone();
        let suspends = internal.output_suspends.clone();
        let resumes = internal.output_resumes.clone();
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let current = library_item(library_id, file_uuids[0]);
                    let expected = library_item(library_id, file_uuids[1]);
                    commands_sender.send(add_library_tracks(library_id, &file_uuids))?;
                    commands_sender.send(PlayerCommand::Switch(0))?;
                    expect_playing(&mut events, &current).await?;

                    // The old track keeps its output while the new one loads.
                    commands_sender.send(PlayerCommand::Switch(1))?;
                    tokio::time::timeout(TEST_DEADLINE, started.notified()).await?;
                    commands_sender.send(PlayerCommand::Pause)?;
                    expect_paused(&mut events, &current).await?;
                    assert_eq!(suspends.load(Ordering::SeqCst), 1);
                    commands_sender.send(PlayerCommand::Play)?;
                    expect_playing(&mut events, &current).await?;
                    assert_eq!(resumes.load(Ordering::SeqCst), 1);

                    release.notify_one();
                    expect_playing(&mut events, &expected).await?;
                    assert_eq!(opens.load(Ordering::SeqCst), 2);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn failed_resume_during_a_switch_leaves_playing_to_the_new_track() -> Result<()> {
        let file_uuids = [Uuid::new_v4(), Uuid::new_v4()];
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (resolver, wav_path) = delayed_resolver_fixture_with_samples(
            &file_uuids,
            Arc::new(Mutex::new(Vec::new())),
            started.clone(),
            release.clone(),
            &[2],
            LONG_TRACK_SAMPLES,
        );
        let (mut internal, commands_sender, mut events) = library_engine(resolver);
        let opens = internal.output_opens.clone();
        let failing_resumes = internal.failing_resumes.clone();
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let current = library_item(library_id, file_uuids[0]);
                    let expected = library_item(library_id, file_uuids[1]);
                    commands_sender.send(add_library_tracks(library_id, &file_uuids))?;
                    commands_sender.send(PlayerCommand::Switch(0))?;
                    expect_playing(&mut events, &current).await?;

                    commands_sender.send(PlayerCommand::Switch(1))?;
                    tokio::time::timeout(TEST_DEADLINE, started.notified()).await?;
                    commands_sender.send(PlayerCommand::Pause)?;
                    expect_paused(&mut events, &current).await?;
                    failing_resumes.store(1, Ordering::SeqCst);
                    commands_sender.send(PlayerCommand::Play)?;
                    assert_nothing_plays(&commands_sender, &mut events).await?;

                    release.notify_one();
                    expect_playing(&mut events, &expected).await?;
                    assert_eq!(opens.load(Ordering::SeqCst), 2);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn stop_stops_the_output() -> Result<()> {
        let file_uuid = Uuid::new_v4();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&[file_uuid]);
        let suspends = internal.output_suspends.clone();
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    start_playing(&commands_sender, &mut events, file_uuid).await?;
                    commands_sender.send(PlayerCommand::Stop)?;
                    tokio::time::timeout(TEST_DEADLINE, async {
                        while !matches!(events.recv().await, Some(PlayerEvent::Stopped) | None) {}
                    })
                    .await?;
                    assert_eq!(suspends.load(Ordering::SeqCst), 1);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    // A shuffled order in which every track moves one place on, so that no index maps to
    // itself and an index mapped once too often leads to another track.
    struct ShiftedOrder;

    impl PlaybackStrategy for ShiftedOrder {
        fn next(&self, current_index: usize, playlist_len: usize) -> Option<usize> {
            (current_index + 1 < playlist_len).then_some(current_index + 1)
        }

        fn previous(&self, current_index: usize, _playlist_len: usize) -> Option<usize> {
            current_index.checked_sub(1)
        }

        fn on_playlist_end(&self, _playlist_len: usize) -> Option<usize> {
            Some(0)
        }

        fn get_mapped_track_index(&self, index: usize, playlist_len: usize) -> usize {
            (index + 1) % playlist_len
        }

        fn on_playlist_updated(&mut self, _playlist_len: usize, _reason: UpdateReason) {}
    }

    // Runs the player until its next track event, and returns that event. The player keeps its
    // state, so a test can change it before running it again.
    async fn run_until_track_state(
        internal: &mut PlayerInternal,
        events: &mut mpsc::UnboundedReceiver<PlayerEvent>,
    ) -> Result<PlayerEvent> {
        tokio::select! {
            result = internal.run() => bail!("the player stopped: {result:?}"),
            event = next_track_state(events) => event,
        }
    }

    #[tokio::test]
    async fn play_output_open_failure_is_typed_and_pause_completes() -> Result<()> {
        let file_uuid = Uuid::new_v4();
        let (mut internal, commands_sender, _events, wav_path) = long_track_engine(&[file_uuid]);
        internal.failing_opens.store(1, Ordering::SeqCst);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                let library_id = Uuid::new_v4();
                commands_sender.send(add_library_tracks(library_id, &[file_uuid]))?;
                let play = completed_command(&commands_sender, PlayerCommand::Play);
                assert_eq!(
                    tokio::time::timeout(TEST_DEADLINE, play).await??,
                    Err(PlayerCommandError::OutputUnavailable)
                );
                let pause = completed_command(&commands_sender, PlayerCommand::Pause);
                assert_eq!(tokio::time::timeout(TEST_DEADLINE, pause).await??, Ok(()));
                runner.abort();
                let _ = std::fs::remove_file(wav_path);
                Ok::<(), anyhow::Error>(())
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn play_next_edit_to_idle_queue_reports_output_failure() -> Result<()> {
        let file_uuid = Uuid::new_v4();
        let (mut internal, commands_sender, _events, wav_path) = long_track_engine(&[file_uuid]);
        internal.failing_opens.store(1, Ordering::SeqCst);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                let library_id = Uuid::new_v4();
                let edit = PlaylistEdit {
                    tracks: library_tracks(library_id, &[file_uuid]),
                    placement: PlaylistPlacement::PlayNext,
                    playback_mode: None,
                    load: None,
                    block: None,
                };
                let completion =
                    completed_command(&commands_sender, PlayerCommand::EditPlaylist(edit));
                assert_eq!(
                    tokio::time::timeout(TEST_DEADLINE, completion).await??,
                    Err(PlayerCommandError::OutputUnavailable)
                );
                runner.abort();
                let _ = std::fs::remove_file(wav_path);
                Ok::<(), anyhow::Error>(())
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn pause_completes_while_play_waits_for_stalled_source() -> Result<()> {
        let file_uuids = [Uuid::new_v4()];
        let ids = Arc::new(Mutex::new(Vec::new()));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (resolver, wav_path) =
            delayed_resolver_fixture(&file_uuids, ids, started.clone(), release, &[1]);
        let (mut internal, commands_sender, _events) = library_engine(resolver);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                let library_id = Uuid::new_v4();
                commands_sender.send(add_library_tracks(library_id, &file_uuids))?;
                let play = completed_command(&commands_sender, PlayerCommand::Play);
                tokio::time::timeout(TEST_DEADLINE, started.notified()).await?;
                let pause = completed_command(&commands_sender, PlayerCommand::Pause);
                assert_eq!(tokio::time::timeout(TEST_DEADLINE, pause).await??, Ok(()));
                assert_eq!(
                    tokio::time::timeout(TEST_DEADLINE, play).await??,
                    Err(PlayerCommandError::SourceUnavailable)
                );
                runner.abort();
                let _ = std::fs::remove_file(wav_path);
                Ok::<(), anyhow::Error>(())
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn seek_during_load_waits_for_application_or_returns_typed_failure() -> Result<()> {
        let file_uuids = [Uuid::new_v4()];
        let ids = Arc::new(Mutex::new(Vec::new()));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (resolver, wav_path) =
            delayed_resolver_fixture(&file_uuids, ids, started.clone(), release.clone(), &[1]);
        let (mut internal, commands_sender, _events) = library_engine(resolver);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                let library_id = Uuid::new_v4();
                commands_sender.send(add_library_tracks(library_id, &file_uuids))?;
                let play = completed_command(&commands_sender, PlayerCommand::Play);
                tokio::time::timeout(TEST_DEADLINE, started.notified()).await?;
                let seek = completed_command(&commands_sender, PlayerCommand::Seek(3.0));
                assert_eq!(
                    tokio::time::timeout(TEST_DEADLINE, seek).await??,
                    Err(PlayerCommandError::CannotApply)
                );
                release.notify_one();
                assert_eq!(tokio::time::timeout(TEST_DEADLINE, play).await??, Ok(()));
                runner.abort();
                let _ = std::fs::remove_file(wav_path);
                Ok::<(), anyhow::Error>(())
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn replaced_load_completes_as_superseded() -> Result<()> {
        let file_uuids = [Uuid::new_v4(), Uuid::new_v4()];
        let ids = Arc::new(Mutex::new(Vec::new()));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (resolver, wav_path) =
            delayed_resolver_fixture(&file_uuids, ids, started.clone(), release.clone(), &[1]);
        let (mut internal, commands_sender, mut events) = library_engine(resolver);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                let library_id = Uuid::new_v4();
                commands_sender.send(add_library_tracks(library_id, &file_uuids))?;
                let old_load =
                    completed_command(&commands_sender, PlayerCommand::Load { index: 0 });
                tokio::time::timeout(TEST_DEADLINE, started.notified()).await?;
                let new_load =
                    completed_command(&commands_sender, PlayerCommand::Load { index: 1 });
                assert_eq!(
                    tokio::time::timeout(TEST_DEADLINE, old_load).await??,
                    Err(PlayerCommandError::Superseded)
                );
                release.notify_one();
                assert_eq!(
                    tokio::time::timeout(TEST_DEADLINE, new_load).await??,
                    Ok(())
                );
                tokio::time::timeout(TEST_DEADLINE, async {
                    while !matches!(
                        events.recv().await,
                        Some(PlayerEvent::Loaded { index: 1, .. })
                    ) {}
                })
                .await?;
                runner.abort();
                let _ = std::fs::remove_file(wav_path);
                Ok::<(), anyhow::Error>(())
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn play_after_a_shuffled_restore_plays_the_restored_track() -> Result<()> {
        let file_uuids: Vec<Uuid> = (0..4).map(|_| Uuid::new_v4()).collect();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&file_uuids);
        let library_id = Uuid::new_v4();
        let track = library_item(library_id, file_uuids[2]);
        commands_sender.send(restore(
            library_id,
            &file_uuids,
            2,
            1.0,
            PlaybackMode::Shuffle,
        ))?;
        let event = run_until_track_state(&mut internal, &mut events).await?;
        assert!(
            matches!(&event, PlayerEvent::Loaded { item, index: 2, .. } if *item == track),
            "{event:?}"
        );

        // Restore asks for a playlist position, which must not be mapped by the shuffled order.
        internal.playback_strategy = Box::new(ShiftedOrder);
        commands_sender.send(PlayerCommand::Play)?;
        let event = run_until_track_state(&mut internal, &mut events).await?;
        assert!(
            matches!(&event, PlayerEvent::Playing { item, index: 2, .. } if *item == track),
            "{event:?}"
        );
        assert_about(position_of(&event), Duration::from_secs(1));
        let _ = std::fs::remove_file(wav_path);
        Ok(())
    }

    #[tokio::test]
    async fn load_at_position_uses_the_playlist_index_in_shuffle_mode() -> Result<()> {
        let file_uuids: Vec<Uuid> = (0..4).map(|_| Uuid::new_v4()).collect();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&file_uuids);
        let library_id = Uuid::new_v4();
        let expected = library_item(library_id, file_uuids[2]);
        internal.playback_mode = PlaybackMode::Shuffle;
        internal.playback_strategy = Box::new(ShiftedOrder);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                commands_sender.send(add_library_tracks(library_id, &file_uuids))?;
                tokio::time::timeout(TEST_DEADLINE, async {
                    while !matches!(events.recv().await, Some(PlayerEvent::PlaylistUpdated(_))) {}
                })
                .await?;

                commands_sender.send(PlayerCommand::LoadAtPosition {
                    index: 2,
                    position: Duration::from_secs(1),
                    play: false,
                })?;
                let event = next_track_state(&mut events).await?;
                assert!(
                    matches!(&event, PlayerEvent::Loaded { item, index: 2, .. } if *item == expected),
                    "{event:?}"
                );

                runner.abort();
                let _ = std::fs::remove_file(wav_path);
                Ok::<(), anyhow::Error>(())
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn play_after_an_earlier_track_was_removed_plays_the_same_track() -> Result<()> {
        let file_uuids = [
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        ];
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&file_uuids);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let track = library_item(library_id, file_uuids[2]);
                    commands_sender.send(add_library_tracks(library_id, &file_uuids))?;
                    commands_sender.send(PlayerCommand::Switch(2))?;
                    expect_playing(&mut events, &track).await?;
                    progress_at_or_after(&mut events, Duration::from_millis(400)).await?;
                    commands_sender.send(PlayerCommand::Output(OutputEvent::DeviceLost))?;
                    let paused_at = expect_paused(&mut events, &track).await?;

                    // The track moves up one place, and the next play opens a new output for it.
                    commands_sender.send(PlayerCommand::RemoveFromPlaylist { index: 0 })?;
                    commands_sender.send(PlayerCommand::Play)?;
                    let event = next_track_state(&mut events).await?;
                    assert!(
                        matches!(&event, PlayerEvent::Playing { item, index: 1, .. } if *item == track),
                        "{event:?}"
                    );
                    assert_about(position_of(&event), paused_at);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn play_after_the_track_was_removed_stops() -> Result<()> {
        let file_uuids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&file_uuids);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let track = library_item(library_id, file_uuids[1]);
                    commands_sender.send(add_library_tracks(library_id, &file_uuids))?;
                    commands_sender.send(PlayerCommand::Switch(1))?;
                    expect_playing(&mut events, &track).await?;
                    commands_sender.send(PlayerCommand::Output(OutputEvent::DeviceLost))?;
                    expect_paused(&mut events, &track).await?;

                    commands_sender.send(PlayerCommand::RemoveFromPlaylist { index: 1 })?;
                    commands_sender.send(PlayerCommand::Play)?;
                    tokio::time::timeout(TEST_DEADLINE, async {
                        loop {
                            match events.recv().await {
                                Some(PlayerEvent::Stopped) => return Ok(()),
                                Some(
                                    event @ (PlayerEvent::Playing { .. }
                                    | PlayerEvent::Loaded { .. }),
                                ) => bail!("{event:?}"),
                                Some(_) => {}
                                None => bail!("player events closed"),
                            }
                        }
                    })
                    .await??;
                    assert_nothing_plays(&commands_sender, &mut events).await?;

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn track_whose_first_output_fails_to_open_is_ready_to_play() -> Result<()> {
        let file_uuid = Uuid::new_v4();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&[file_uuid]);
        let opens = internal.output_opens.clone();
        internal.failing_opens.store(1, Ordering::SeqCst);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let track = library_item(library_id, file_uuid);
                    commands_sender.send(add_library_tracks(library_id, &[file_uuid]))?;
                    commands_sender.send(PlayerCommand::Switch(0))?;
                    // `Loaded` is what marks a track ready in the player's status, which lets the
                    // UI offer Play.
                    assert_about(expect_loaded(&mut events, &track).await?, Duration::ZERO);
                    assert_nothing_plays(&commands_sender, &mut events).await?;

                    commands_sender.send(PlayerCommand::Play)?;
                    expect_playing(&mut events, &track).await?;
                    assert_eq!(opens.load(Ordering::SeqCst), 2);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn seek_while_a_new_output_opens_is_where_playback_goes_on() -> Result<()> {
        let file_uuids = [Uuid::new_v4()];
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (resolver, wav_path) = delayed_resolver_fixture_with_samples(
            &file_uuids,
            Arc::new(Mutex::new(Vec::new())),
            started.clone(),
            release.clone(),
            &[1],
            LONG_TRACK_SAMPLES,
        );
        let (mut internal, commands_sender, mut events) = library_engine(resolver);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let track = library_item(library_id, file_uuids[0]);
                    commands_sender.send(add_library_tracks(library_id, &file_uuids))?;
                    commands_sender.send(PlayerCommand::Switch(0))?;
                    tokio::time::timeout(TEST_DEADLINE, started.notified()).await?;
                    release.notify_one();
                    expect_playing(&mut events, &track).await?;
                    commands_sender.send(PlayerCommand::Output(OutputEvent::DeviceLost))?;
                    expect_paused(&mut events, &track).await?;

                    // Play loads the track again for a new output, and the resolver holds it.
                    commands_sender.send(PlayerCommand::Play)?;
                    tokio::time::timeout(TEST_DEADLINE, started.notified()).await?;
                    commands_sender.send(PlayerCommand::Seek(3.0))?;
                    release.notify_one();
                    assert_about(
                        expect_playing(&mut events, &track).await?,
                        Duration::from_secs(3),
                    );

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn changing_mixing_during_a_switch_stops_the_old_output_first() -> Result<()> {
        let file_uuids = [Uuid::new_v4(), Uuid::new_v4()];
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (resolver, wav_path) = delayed_resolver_fixture_with_samples(
            &file_uuids,
            Arc::new(Mutex::new(Vec::new())),
            started.clone(),
            release.clone(),
            &[2],
            LONG_TRACK_SAMPLES,
        );
        let (mut internal, commands_sender, mut events) = library_engine(resolver);
        let suspends = internal.output_suspends.clone();
        let resumes = internal.output_resumes.clone();
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let current = library_item(library_id, file_uuids[0]);
                    let expected = library_item(library_id, file_uuids[1]);
                    commands_sender.send(add_library_tracks(library_id, &file_uuids))?;
                    commands_sender.send(PlayerCommand::Switch(0))?;
                    expect_playing(&mut events, &current).await?;

                    // The old track keeps its output while the new one loads. The session only
                    // changes while no output runs, and the old track plays on afterwards.
                    commands_sender.send(PlayerCommand::Switch(1))?;
                    tokio::time::timeout(TEST_DEADLINE, started.notified()).await?;
                    commands_sender.send(PlayerCommand::SetMixWithOthers(true))?;
                    wait_for_output_reopen(&suspends, &resumes).await?;
                    let before = next_progress(&mut events).await?;
                    assert_eq!(suspends.load(Ordering::SeqCst), 1);
                    assert_eq!(resumes.load(Ordering::SeqCst), 1);
                    let later =
                        progress_at_or_after(&mut events, before + Duration::from_millis(300))
                            .await?;
                    assert!(later >= before + Duration::from_millis(300), "{later:?}");

                    release.notify_one();
                    expect_playing(&mut events, &expected).await?;

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    #[test]
    fn a_seek_before_the_start_goes_to_the_start_and_one_to_no_place_is_dropped() {
        assert_eq!(seek_position(2.5), Some(Duration::from_millis(2500)));
        assert_eq!(seek_position(-3.0), Some(Duration::ZERO));
        assert_eq!(seek_position(f64::NEG_INFINITY), Some(Duration::ZERO));
        assert_eq!(seek_position(f64::NAN), None);
        assert_eq!(seek_position(f64::INFINITY), None);
    }

    #[tokio::test]
    async fn seek_before_the_start_plays_from_the_start() -> Result<()> {
        let file_uuid = Uuid::new_v4();
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&[file_uuid]);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let track = start_playing(&commands_sender, &mut events, file_uuid).await?;
                    commands_sender.send(PlayerCommand::Seek(-3.0))?;
                    assert_about(expect_playing(&mut events, &track).await?, Duration::ZERO);

                    // A seek to no place is dropped, and the player still answers.
                    commands_sender.send(PlayerCommand::Seek(f64::NAN))?;
                    commands_sender.send(PlayerCommand::SetVolume(0.5))?;
                    tokio::time::timeout(TEST_DEADLINE, async {
                        loop {
                            match events.recv().await {
                                Some(PlayerEvent::VolumeUpdate(_)) => return Ok(()),
                                Some(event @ PlayerEvent::Playing { .. }) => bail!("{event:?}"),
                                Some(_) => {}
                                None => bail!("player events closed"),
                            }
                        }
                    })
                    .await??;

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await?;
        Ok(())
    }

    fn load_at_position(index: usize, seconds: f64, play: bool) -> PlayerCommand {
        PlayerCommand::LoadAtPosition {
            index,
            position: Duration::from_secs_f64(seconds),
            play,
        }
    }

    // Loads the second of two tracks at 2 s and checks that it comes up there, playing or
    // paused. A paused one starts playing there too, on an output that the play opens for the
    // same track.
    async fn check_load_at_position(play: bool) -> Result<()> {
        let file_uuids = [Uuid::new_v4(), Uuid::new_v4()];
        let (mut internal, commands_sender, mut events, wav_path) = long_track_engine(&file_uuids);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let track = library_item(library_id, file_uuids[1]);
                    commands_sender.send(add_library_tracks(library_id, &file_uuids))?;
                    commands_sender.send(load_at_position(1, 2.0, play))?;
                    let position = if play {
                        expect_playing(&mut events, &track).await?
                    } else {
                        expect_loaded(&mut events, &track).await?
                    };
                    assert_about(position, Duration::from_secs(2));

                    if !play {
                        commands_sender.send(PlayerCommand::Play)?;
                        assert_about(
                            expect_playing(&mut events, &track).await?,
                            Duration::from_secs(2),
                        );
                    }

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await
    }

    #[tokio::test]
    async fn load_at_a_position_comes_up_playing_there() -> Result<()> {
        check_load_at_position(true).await
    }

    #[tokio::test]
    async fn load_at_a_position_comes_up_paused_there() -> Result<()> {
        check_load_at_position(false).await
    }

    // As `check_load_at_position`, while the first track is still loading. The newer request
    // wins, with its own position.
    async fn check_load_at_position_during_a_load(play: bool) -> Result<()> {
        let file_uuids = [Uuid::new_v4(), Uuid::new_v4()];
        let ids = Arc::new(Mutex::new(Vec::new()));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (resolver, wav_path) = delayed_resolver_fixture_with_samples(
            &file_uuids,
            ids.clone(),
            started.clone(),
            release.clone(),
            &[1],
            LONG_TRACK_SAMPLES,
        );
        let (mut internal, commands_sender, mut events) = library_engine(resolver);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runner = tokio::task::spawn_local(async move { internal.run().await });
                async {
                    let library_id = Uuid::new_v4();
                    let track = library_item(library_id, file_uuids[1]);
                    commands_sender.send(add_library_tracks(library_id, &file_uuids))?;
                    commands_sender.send(PlayerCommand::Switch(0))?;
                    tokio::time::timeout(TEST_DEADLINE, started.notified()).await?;
                    commands_sender.send(load_at_position(1, 2.0, play))?;
                    release.notify_one();

                    let position = if play {
                        expect_playing(&mut events, &track).await?
                    } else {
                        expect_loaded(&mut events, &track).await?
                    };
                    assert_about(position, Duration::from_secs(2));
                    assert_eq!(&*ids.lock().unwrap(), &[1, 2]);

                    runner.abort();
                    let _ = std::fs::remove_file(wav_path);
                    Ok::<(), anyhow::Error>(())
                }
                .await
            })
            .await
    }

    #[tokio::test]
    async fn load_at_a_position_during_a_load_comes_up_playing_there() -> Result<()> {
        check_load_at_position_during_a_load(true).await
    }

    #[tokio::test]
    async fn load_at_a_position_during_a_load_comes_up_paused_there() -> Result<()> {
        check_load_at_position_during_a_load(false).await
    }

    // Hosted tracks, which a platform host plays. The fake host records the batches it is sent
    // and applies them to a model of a host's queue, which checks them against the host
    // contract: every index the player sends names an entry, an `add` carries one id, and only
    // the entry after the current one is ever removed. The tests report the host's events,
    // written out or taken from the model.
    mod hosted {
        use super::*;
        use crate::hosted::HostFailure;
        use crate::player::{
            AudioRequest, LibraryPlaybackSource, Playable, PlaybackState, Player, RemoteAudioSource,
        };
        use futures::future::BoxFuture;
        use simple_channel::SimpleReceiver;
        use std::sync::atomic::{AtomicBool, AtomicU64};

        const HOST: &str = "appleMusic";
        // Keys that start with "other-" belong to this host.
        const OTHER: &str = "otherHost";

        // A host's queue, as the host contract describes it.
        #[derive(Default)]
        struct Model {
            queue: Vec<String>,
            current: Option<usize>,
            playing: bool,
            position_ms: u64,
            revision: u64,
            // What the player sent that the contract does not allow.
            errors: Vec<String>,
        }

        impl Model {
            fn apply(&mut self, command: &HostCommand) {
                if command.revision <= self.revision {
                    self.errors.push(format!(
                        "revision {} after {}",
                        command.revision, self.revision
                    ));
                }
                self.revision = command.revision;
                for action in &command.actions {
                    if let Err(error) = self.act(action) {
                        self.errors.push(format!(
                            "{action:?} on {:?} at {:?}: {error}",
                            self.queue, self.current
                        ));
                    }
                }
            }

            fn act(&mut self, action: &HostAction) -> std::result::Result<(), &'static str> {
                match action {
                    HostAction::Set { ids } => {
                        if ids.is_empty() || ids.len() > 2 {
                            return Err("a set holds the current track and at most one more");
                        }
                        self.queue = ids.clone();
                        self.current = Some(0);
                        self.position_ms = 0;
                    }
                    HostAction::Add { ids } => {
                        if ids.len() != 1 {
                            return Err("an add carries one id");
                        }
                        if self.current.is_none() {
                            return Err("an add to a host that holds nothing");
                        }
                        self.queue.extend(ids.iter().cloned());
                    }
                    HostAction::Remove { index } => {
                        let index = *index as usize;
                        // A host never removes the entry it plays.
                        if Some(index) == self.current {
                            return Ok(());
                        }
                        if self.current.map(|current| current + 1) != Some(index)
                            || index + 1 != self.queue.len()
                        {
                            return Err("only the entry after the current one is removed");
                        }
                        self.queue.pop();
                    }
                    HostAction::Skip { index, offset_ms } => {
                        let index = *index as usize;
                        if index >= self.queue.len() {
                            return Err("a skip past the end of the queue");
                        }
                        self.current = Some(index);
                        self.position_ms = *offset_ms;
                    }
                    HostAction::Start => self.playing = true,
                    HostAction::Stop => self.playing = false,
                    HostAction::Clear => {
                        self.queue.clear();
                        self.current = None;
                        self.playing = false;
                    }
                    HostAction::SetGain { .. } => {}
                }
                Ok(())
            }

            fn status(&self, host: &str) -> HostEvent {
                HostEvent::JukeboxStatus {
                    host: host.to_owned(),
                    revision: self.revision,
                    current_index: self.current.map(|current| u32::try_from(current).unwrap()),
                    current_id: self.current.map(|current| self.queue[current].clone()),
                    playing: self.playing,
                    position_ms: self.position_ms,
                }
            }
        }

        // A host that records the batches it is sent, and applies them to a model of its queue.
        struct FakeHost {
            sent: mpsc::UnboundedSender<HostCommand>,
            received: tokio::sync::Mutex<mpsc::UnboundedReceiver<HostCommand>>,
            models: Mutex<HashMap<String, Model>>,
        }

        impl HostBridge for FakeHost {
            fn send(&self, command: HostCommand) {
                self.models
                    .lock()
                    .unwrap()
                    .entry(command.host.clone())
                    .or_default()
                    .apply(&command);
                let _ = self.sent.send(command);
            }
        }

        impl FakeHost {
            fn new() -> Arc<Self> {
                let (sent, received) = mpsc::unbounded_channel();
                Arc::new(Self {
                    sent,
                    received: tokio::sync::Mutex::new(received),
                    models: Mutex::default(),
                })
            }

            // Waits until the host was sent at least `count` batches since the last take, and
            // returns all of them.
            async fn take(&self, count: usize) -> Result<Vec<HostCommand>> {
                let mut received = self.received.lock().await;
                let mut batches = Vec::new();
                let waited = tokio::time::timeout(TEST_DEADLINE, async {
                    while batches.len() < count {
                        match received.recv().await {
                            Some(batch) => batches.push(batch),
                            None => break,
                        }
                    }
                })
                .await;
                anyhow::ensure!(
                    waited.is_ok() && batches.len() >= count,
                    "the host was sent only {batches:?}"
                );
                while let Ok(batch) = received.try_recv() {
                    batches.push(batch);
                }
                Ok(batches)
            }

            fn sent_nothing(&self) -> bool {
                self.received
                    .try_lock()
                    .is_ok_and(|received| received.is_empty())
            }

            fn model<T>(&self, f: impl FnOnce(&mut Model) -> T) -> T {
                f(self
                    .models
                    .lock()
                    .unwrap()
                    .entry(HOST.to_owned())
                    .or_default())
            }

            // What the main host reports as it stands.
            fn status(&self) -> HostEvent {
                self.model(|model| model.status(HOST))
            }

            // The main host moves on to its next entry by itself, and reports so.
            fn move_on(&self) -> HostEvent {
                self.model(|model| {
                    let next = model.current.map_or(0, |current| current + 1);
                    assert!(
                        next < model.queue.len(),
                        "the host holds nothing after {:?} in {:?}",
                        model.current,
                        model.queue
                    );
                    model.current = Some(next);
                    model.position_ms = 0;
                    model.status(HOST)
                })
            }

            // The main host's last entry finishes, and it reports the end of its queue.
            fn end(&self) -> HostEvent {
                self.model(|model| {
                    assert!(
                        model
                            .current
                            .is_some_and(|current| current + 1 == model.queue.len()),
                        "the host holds more after {:?} in {:?}",
                        model.current,
                        model.queue
                    );
                    model.playing = false;
                    HostEvent::JukeboxEnded {
                        host: HOST.to_owned(),
                        revision: model.revision,
                    }
                })
            }

            // The main host restarts, holding nothing, and says that it is ready.
            fn restart(&self) -> HostEvent {
                self.model(|model| {
                    model.queue.clear();
                    model.current = None;
                    model.playing = false;
                });
                ready(HOST, true, true)
            }

            // The ids in the main host's queue, and the id of its current entry.
            fn queue(&self) -> (Vec<String>, Option<String>) {
                self.model(|model| {
                    (
                        model.queue.clone(),
                        model.current.map(|current| model.queue[current].clone()),
                    )
                })
            }

            // Fails if the player sent anything that the host contract does not allow.
            fn check(&self) -> Result<()> {
                let models = self.models.lock().unwrap();
                let errors = models
                    .iter()
                    .flat_map(|(host, model)| {
                        model
                            .errors
                            .iter()
                            .map(move |error| format!("{host}: {error}"))
                    })
                    .collect::<Vec<_>>();
                anyhow::ensure!(errors.is_empty(), "{errors:#?}");
                Ok(())
            }
        }

        // A request that is never made: the streams in these tests are only looked at.
        struct NoRequest;

        impl AudioRequest for NoRequest {
            fn get(
                &self,
                _: Option<(u64, Option<u64>)>,
            ) -> BoxFuture<'static, Result<reqwest::Response>> {
                Box::pin(async { anyhow::bail!("no request in these tests") })
            }
        }

        // How the resolver answers about keys that start with "slow-": after `delay_ms`, and
        // whether it answered, or the question was dropped first, which `on_drop` is told.
        #[derive(Default)]
        struct Slow {
            delay_ms: AtomicU64,
            answered: AtomicBool,
            dropped: AtomicBool,
            on_drop: Notify,
        }

        // Marks a slow question as dropped, answered or not.
        struct Dropped(Arc<Slow>);

        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.dropped.store(true, Ordering::SeqCst);
                self.0.on_drop.notify_one();
            }
        }

        // Tracks of `hosted_library` are hosted, and the others are local tracks of `local`. Of
        // the hosted tracks, keys that start with "other-" belong to another host, "stream-" are
        // streamed, "error-" cannot be resolved, and "slow-" take a while.
        struct MixedResolver {
            local: Arc<DelayedResolver>,
            hosted_library: Uuid,
            slow: Arc<Slow>,
            // The cancellation of the streams the resolver handed out, which it sends on.
            streams: mpsc::UnboundedSender<CancellationToken>,
            // Told when the resolver fails on a key.
            failed: Arc<Notify>,
        }

        impl PlaybackSourceResolver for MixedResolver {
            fn resolve_track(
                &self,
                library_id: String,
                track: String,
            ) -> BoxFuture<'static, Result<LibraryTrackKey>> {
                if library_id == self.hosted_library.to_string() {
                    return Box::pin(async move { Ok(LibraryTrackKey::Service(track)) });
                }
                self.local.resolve_track(library_id, track)
            }

            fn for_library(
                &self,
                library_id: String,
            ) -> BoxFuture<'static, Result<Option<LibraryPlaybackSource>>> {
                self.local.for_library(library_id)
            }

            fn resolve(&self, id: i32) -> BoxFuture<'static, Result<RemoteAudioSource>> {
                self.local.resolve(id)
            }

            fn resolve_service(
                &self,
                item: ServiceItem,
            ) -> BoxFuture<'static, Result<ServiceTarget>> {
                let key = item.track;
                if key.starts_with("error-") {
                    let failed = self.failed.clone();
                    return Box::pin(async move {
                        failed.notify_one();
                        anyhow::bail!("no track {key}")
                    });
                }
                if key.starts_with("stream-") {
                    let cancellation = CancellationToken::new();
                    let _ = self.streams.send(cancellation.clone());
                    return Box::pin(async move {
                        Ok(ServiceTarget::Stream(RemoteAudioSource {
                            request: Arc::new(NoRequest),
                            url: "https://stream.invalid/".to_owned(),
                            extension: "mp3".to_owned(),
                            size: 0,
                            cancellation,
                            failure_reason: Arc::new(Mutex::new(None)),
                        }))
                    });
                }
                let host = if key.starts_with("other-") {
                    OTHER
                } else {
                    HOST
                };
                let slow = key.starts_with("slow-").then(|| self.slow.clone());
                Box::pin(async move {
                    if let Some(slow) = slow {
                        let _dropped = Dropped(slow.clone());
                        let delay = Duration::from_millis(slow.delay_ms.load(Ordering::SeqCst));
                        tokio::time::sleep(delay).await;
                        slow.answered.store(true, Ordering::SeqCst);
                    }
                    Ok(ServiceTarget::Hosted(HostedTrack {
                        host: host.to_owned(),
                        key,
                    }))
                })
            }
        }

        struct Fixture {
            host: Arc<FakeHost>,
            commands: mpsc::UnboundedSender<PlayerCommand>,
            events: mpsc::UnboundedReceiver<PlayerEvent>,
            local_library: Uuid,
            local_uuids: Vec<Uuid>,
            hosted_library: Uuid,
            started: Arc<Notify>,
            release: Arc<Notify>,
            slow: Arc<Slow>,
            streams: mpsc::UnboundedReceiver<CancellationToken>,
            failed: Arc<Notify>,
            wav_path: PathBuf,
        }

        impl Fixture {
            fn local(&self, index: usize) -> PlayingItem {
                library_item(self.local_library, self.local_uuids[index])
            }

            fn hosted(&self, key: &str) -> PlayingItem {
                PlayingItem::LibraryTrack {
                    library_id: self.hosted_library,
                    track: key.to_owned(),
                }
            }

            fn queue(&self, items: &[PlayingItem]) -> Result<()> {
                self.send(PlayerCommand::AddToPlaylist {
                    tracks: items
                        .iter()
                        .map(|item| (item.clone(), PlaybackSource::LibraryStream))
                        .collect(),
                    mode: AddMode::AppendToEnd,
                })
            }

            fn send(&self, command: PlayerCommand) -> Result<()> {
                self.commands.send(command)?;
                Ok(())
            }

            fn report(&self, event: HostEvent) -> Result<()> {
                self.send(PlayerCommand::Hosted(event))
            }

            // Waits until the player has handled every command sent so far, and returns what it
            // reported meanwhile, which callers check rather than drop. It does not wait for the
            // lookups of the next track that those commands started.
            async fn settle(&mut self) -> Result<Vec<PlayerEvent>> {
                let (response, _) = oneshot::channel();
                self.send(PlayerCommand::StatusAfterPending(response))?;
                until(&mut self.events, |event| {
                    matches!(event, PlayerEvent::StatusAfterPending(_))
                })
                .await
            }

            // Drops the events so far, and fails on any that marks a track unavailable.
            fn assert_all_available(&mut self) {
                while let Ok(event) = self.events.try_recv() {
                    assert!(!matches!(event, PlayerEvent::Unavailable(..)), "{event:?}");
                }
            }
        }

        // An engine whose three local tracks have `samples` samples each, and whose local track
        // ids in `blocked` load only once released. A fake host plays the hosted ones.
        fn hosted_engine(samples: u32, blocked: &[i32]) -> (PlayerInternal, Fixture) {
            let local_uuids = (0..3).map(|_| Uuid::new_v4()).collect::<Vec<_>>();
            let started = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let (local, wav_path) = delayed_resolver_fixture_with_samples(
                &local_uuids,
                Arc::new(Mutex::new(Vec::new())),
                started.clone(),
                release.clone(),
                blocked,
                samples,
            );
            let hosted_library = Uuid::new_v4();
            let slow = Arc::new(Slow::default());
            let (stream_sender, streams) = mpsc::unbounded_channel();
            let failed = Arc::new(Notify::new());
            let host = FakeHost::new();
            let (commands_sender, commands) = mpsc::unbounded_channel();
            let (event_sender, events) = mpsc::unbounded_channel();
            let internal = PlayerInternal::new(
                commands,
                event_sender,
                CancellationToken::new(),
                commands_sender.clone(),
                Arc::new(FsIo::new_noop()),
                PcmTap::new(),
                Arc::new(RwLock::new(Some(Arc::new(MixedResolver {
                    local,
                    hosted_library,
                    slow: slow.clone(),
                    streams: stream_sender,
                    failed: failed.clone(),
                })))),
            )
            .with_host_bridge(Arc::new(RwLock::new(Some(host.clone()))));
            let fixture = Fixture {
                host,
                commands: commands_sender,
                events,
                local_library: Uuid::new_v4(),
                local_uuids,
                hosted_library,
                started,
                release,
                slow,
                streams,
                failed,
                wav_path,
            };
            (internal, fixture)
        }

        // Runs `test` against a running engine, and checks what the host was sent.
        async fn with_engine(
            internal: PlayerInternal,
            mut f: Fixture,
            test: impl AsyncFnOnce(&mut Fixture) -> Result<()>,
        ) -> Result<()> {
            let mut internal = internal;
            LocalSet::new()
                .run_until(async move {
                    let runner = tokio::task::spawn_local(async move { internal.run().await });
                    let result = test(&mut f).await;
                    runner.abort();
                    let _ = std::fs::remove_file(&f.wav_path);
                    result.and_then(|()| f.host.check())
                })
                .await
        }

        fn batch(host: &str, revision: u64, actions: Vec<HostAction>) -> HostCommand {
            HostCommand {
                host: host.to_owned(),
                revision,
                actions,
            }
        }

        // A batch of one action for the main host.
        fn on(revision: u64, action: HostAction) -> HostCommand {
            batch(HOST, revision, vec![action])
        }

        fn hand_over(
            host: &str,
            keys: &[&str],
            revision: u64,
            offset_ms: u64,
            gain: f32,
            play: bool,
        ) -> HostCommand {
            batch(
                host,
                revision,
                vec![
                    HostAction::Set {
                        ids: keys.iter().map(|key| (*key).to_owned()).collect(),
                    },
                    HostAction::Skip {
                        index: 0,
                        offset_ms,
                    },
                    HostAction::SetGain { gain },
                    if play {
                        HostAction::Start
                    } else {
                        HostAction::Stop
                    },
                ],
            )
        }

        // A track handed to the main host to play from its start, at full volume.
        fn handed_over(key: &str, revision: u64) -> HostCommand {
            hand_over(HOST, &[key], revision, 0, 1.0, true)
        }

        // What the main host is sent when its tracks stop being current.
        fn released(revision: u64) -> HostCommand {
            batch(HOST, revision, vec![HostAction::Stop, HostAction::Clear])
        }

        // The main host gets `key` as the track after its current one.
        fn added(revision: u64, key: &str) -> HostCommand {
            on(
                revision,
                HostAction::Add {
                    ids: vec![key.to_owned()],
                },
            )
        }

        // The main host loses the entry at `index`.
        fn removed(revision: u64, index: u32) -> HostCommand {
            on(revision, HostAction::Remove { index })
        }

        // The main host skips to the entry at `index`, and plays it from its start.
        fn skipped_to(revision: u64, index: u32) -> HostCommand {
            batch(
                HOST,
                revision,
                vec![
                    HostAction::Skip {
                        index,
                        offset_ms: 0,
                    },
                    HostAction::Start,
                ],
            )
        }

        // A status of the main host about track 1440 at index 0.
        fn status(revision: u64, playing: bool, position_ms: u64) -> HostEvent {
            status_of(revision, "1440", playing, position_ms)
        }

        // A status of the main host about track `id` at index 0.
        fn status_of(revision: u64, id: &str, playing: bool, position_ms: u64) -> HostEvent {
            HostEvent::JukeboxStatus {
                host: HOST.to_owned(),
                revision,
                current_index: Some(0),
                current_id: Some(id.to_owned()),
                playing,
                position_ms,
            }
        }

        fn lost(revision: u64) -> HostEvent {
            HostEvent::JukeboxStatus {
                host: HOST.to_owned(),
                revision,
                current_index: None,
                current_id: None,
                playing: false,
                position_ms: 0,
            }
        }

        fn ended(revision: u64) -> HostEvent {
            HostEvent::JukeboxEnded {
                host: HOST.to_owned(),
                revision,
            }
        }

        fn failed_at(revision: u64, index: u32, reason: HostFailure) -> HostEvent {
            HostEvent::JukeboxItemFailed {
                host: HOST.to_owned(),
                revision,
                index,
                reason,
            }
        }

        fn failed(revision: u64, reason: HostFailure) -> HostEvent {
            failed_at(revision, 0, reason)
        }

        fn ready(host: &str, authorized: bool, can_play: bool) -> HostEvent {
            HostEvent::HostReady {
                host: host.to_owned(),
                authorized,
                can_play,
            }
        }

        // Waits for the next event that `wanted` picks, and returns it with the events before it.
        async fn until(
            events: &mut mpsc::UnboundedReceiver<PlayerEvent>,
            wanted: impl Fn(&PlayerEvent) -> bool,
        ) -> Result<Vec<PlayerEvent>> {
            let mut seen = Vec::new();
            tokio::time::timeout(TEST_DEADLINE, async {
                loop {
                    match events.recv().await {
                        Some(event) if wanted(&event) => return Ok(()),
                        Some(event) => seen.push(event),
                        None => bail!("player events closed"),
                    }
                }
            })
            .await??;
            Ok(seen)
        }

        // Waits for the next event that `wanted` picks.
        async fn wait_for(
            events: &mut mpsc::UnboundedReceiver<PlayerEvent>,
            wanted: impl Fn(&PlayerEvent) -> bool,
        ) -> Result<PlayerEvent> {
            tokio::time::timeout(TEST_DEADLINE, async {
                loop {
                    match events.recv().await {
                        Some(event) if wanted(&event) => return Ok(event),
                        Some(_) => {}
                        None => bail!("player events closed"),
                    }
                }
            })
            .await?
        }

        async fn expect_unavailable(
            events: &mut mpsc::UnboundedReceiver<PlayerEvent>,
            track: &PlayingItem,
        ) -> Result<UnavailableReason> {
            match wait_for(
                events,
                |event| matches!(event, PlayerEvent::Unavailable(item, _) if item == track),
            )
            .await?
            {
                PlayerEvent::Unavailable(_, reason) => Ok(reason),
                _ => unreachable!(),
            }
        }

        // Waits until `track` counts as played through.
        async fn expect_ended(
            events: &mut mpsc::UnboundedReceiver<PlayerEvent>,
            track: &PlayingItem,
        ) -> Result<()> {
            wait_for(
                events,
                |event| matches!(event, PlayerEvent::EndOfTrack { item, .. } if item == track),
            )
            .await?;
            Ok(())
        }

        // How many times `seen` counts `track` as played through.
        fn ends(seen: &[PlayerEvent], track: &PlayingItem) -> usize {
            seen.iter()
                .filter(
                    |event| matches!(event, PlayerEvent::EndOfTrack { item, .. } if item == track),
                )
                .count()
        }

        fn any(seen: &[PlayerEvent], wanted: impl Fn(&PlayerEvent) -> bool) -> bool {
            seen.iter().any(wanted)
        }

        // Starts the first queued track and checks that the host got it in one batch, and then
        // `next` as the track after it, when one is given.
        async fn start_first(
            f: &mut Fixture,
            items: &[PlayingItem],
            next: Option<&str>,
        ) -> Result<()> {
            let PlayingItem::LibraryTrack { track: key, .. } = &items[0] else {
                bail!("{:?} is not hosted", items[0]);
            };
            f.queue(items)?;
            f.send(PlayerCommand::Switch(0))?;
            expect_playing(&mut f.events, &items[0]).await?;
            let mut expected = vec![handed_over(key, 1)];
            expected.extend(next.map(|next| added(2, next)));
            assert_eq!(f.host.take(expected.len()).await?, expected);
            Ok(())
        }

        #[tokio::test]
        async fn a_hosted_track_between_local_ones_plays_on_its_host() -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            let opens = internal.output_opens.clone();
            with_engine(internal, f, async |f| {
                let (first, hosted, last) = (f.local(0), f.hosted("1440"), f.local(1));
                f.queue(&[first.clone(), hosted.clone(), last.clone()])?;
                f.send(PlayerCommand::Switch(0))?;
                expect_playing(&mut f.events, &first).await?;

                // The local track plays to its end, and the host takes over.
                let event = wait_for(
                    &mut f.events,
                    |event| matches!(event, PlayerEvent::Playing { item, .. } if *item == hosted),
                )
                .await?;
                assert!(
                    matches!(&event, PlayerEvent::Playing { path, position, .. }
                        if path.as_os_str() == "hosted://appleMusic/1440" && position.is_zero()),
                    "{event:?}"
                );
                // The local track after it is not the host's to play.
                assert_eq!(f.host.take(1).await?, vec![handed_over("1440", 1)]);

                f.report(status(1, true, 1_000))?;
                f.report(ended(1))?;
                expect_ended(&mut f.events, &hosted).await?;
                expect_playing(&mut f.events, &last).await?;
                assert_eq!(f.host.take(1).await?, vec![released(2)]);
                // Only the local tracks opened an output.
                assert_eq!(opens.load(Ordering::SeqCst), 2);
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn pause_play_seek_volume_and_stop_of_a_hosted_track_go_to_its_host() -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            let opens = internal.output_opens.clone();
            with_engine(internal, f, async |f| {
                let hosted = f.hosted("1440");
                start_first(f, std::slice::from_ref(&hosted), None).await?;

                f.send(PlayerCommand::Pause)?;
                expect_paused(&mut f.events, &hosted).await?;
                assert_eq!(f.host.take(1).await?, vec![on(2, HostAction::Stop)]);

                f.send(PlayerCommand::Play)?;
                expect_playing(&mut f.events, &hosted).await?;
                assert_eq!(f.host.take(1).await?, vec![on(3, HostAction::Start)]);

                f.send(PlayerCommand::Seek(42.5))?;
                assert_eq!(
                    expect_playing(&mut f.events, &hosted).await?,
                    Duration::from_millis(42_500)
                );
                let skip = HostAction::Skip {
                    index: 0,
                    offset_ms: 42_500,
                };
                assert_eq!(f.host.take(1).await?, vec![on(4, skip)]);

                f.send(PlayerCommand::SetVolume(0.5))?;
                let gain = HostAction::SetGain { gain: 0.5 };
                assert_eq!(f.host.take(1).await?, vec![on(5, gain)]);

                f.send(PlayerCommand::Stop)?;
                wait_for(&mut f.events, |event| matches!(event, PlayerEvent::Stopped)).await?;
                assert_eq!(f.host.take(1).await?, vec![released(6)]);
                assert_eq!(opens.load(Ordering::SeqCst), 0);
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn the_hand_over_carries_the_volume() -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let hosted = f.hosted("1440");
                f.send(PlayerCommand::SetVolume(0.6))?;
                f.queue(std::slice::from_ref(&hosted))?;
                f.send(PlayerCommand::Switch(0))?;
                expect_playing(&mut f.events, &hosted).await?;
                assert_eq!(
                    f.host.take(1).await?,
                    vec![hand_over(HOST, &["1440"], 1, 0, 0.6, true)]
                );
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn next_and_previous_move_between_hosted_and_local_tracks() -> Result<()> {
            let (internal, f) = hosted_engine(LONG_TRACK_SAMPLES, &[]);
            let suspends = internal.output_suspends.clone();
            with_engine(internal, f, async |f| {
                let (hosted, track) = (f.hosted("1440"), f.local(0));
                start_first(f, &[hosted.clone(), track.clone()], None).await?;

                f.send(PlayerCommand::Next)?;
                expect_playing(&mut f.events, &track).await?;
                assert_eq!(f.host.take(1).await?, vec![released(2)]);

                // The local output stops before the host takes over again.
                f.send(PlayerCommand::Previous)?;
                expect_playing(&mut f.events, &hosted).await?;
                assert_eq!(f.host.take(1).await?, vec![handed_over("1440", 3)]);
                assert_eq!(suspends.load(Ordering::SeqCst), 1);
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn a_hosted_track_of_another_host_takes_over_from_the_first() -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let (first, second) = (f.hosted("1440"), f.hosted("other-7"));
                f.queue(&[first.clone(), second.clone()])?;
                f.send(PlayerCommand::Switch(0))?;
                wait_for(
                    &mut f.events,
                    |event| matches!(event, PlayerEvent::HostedChanged(Some(host)) if host == HOST),
                )
                .await?;
                expect_playing(&mut f.events, &first).await?;
                // The track after it is not this host's to play.
                assert_eq!(f.host.take(1).await?, vec![handed_over("1440", 1)]);

                f.send(PlayerCommand::Next)?;
                wait_for(&mut f.events, |event| {
                    matches!(event, PlayerEvent::HostedChanged(Some(host)) if host == OTHER)
                })
                .await?;
                expect_playing(&mut f.events, &second).await?;
                assert_eq!(
                    f.host.take(2).await?,
                    vec![released(2), hand_over(OTHER, &["other-7"], 3, 0, 1.0, true)]
                );
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn a_restored_hosted_track_waits_as_a_local_one_does_until_play() -> Result<()> {
            let (mut internal, mut f) = hosted_engine(2, &[]);
            let hosted = f.hosted("1440");
            f.send(PlayerCommand::Restore {
                tracks: vec![(hosted.clone(), PlaybackSource::LibraryStream)],
                index: Some(0),
                position: Duration::from_millis(1_500),
                mode: PlaybackMode::Sequential,
            })?;
            let event = run_until_track_state(&mut internal, &mut f.events).await?;
            assert!(
                matches!(&event, PlayerEvent::Loaded { item, position, .. }
                    if *item == hosted && *position == Duration::from_millis(1_500)),
                "{event:?}"
            );
            // Nothing goes to the host, and the player stands as after a local paused load.
            assert!(f.host.sent_nothing());
            assert!(internal.hosted.is_none());
            assert_eq!(internal.state, InternalPlaybackState::Stopped);
            assert!(internal.output_stale);
            assert_eq!(
                internal.restore_position,
                Some(Duration::from_millis(1_500))
            );
            assert_eq!(internal.interrupted_item(), InterruptedItem::Nothing);

            // A seek only moves the place it plays from.
            f.send(PlayerCommand::Seek(3.0))?;
            let event = run_until_track_state(&mut internal, &mut f.events).await?;
            assert!(
                matches!(&event, PlayerEvent::Paused { position, .. }
                    if *position == Duration::from_secs(3)),
                "{event:?}"
            );
            assert!(f.host.sent_nothing());

            f.send(PlayerCommand::Play)?;
            let event = run_until_track_state(&mut internal, &mut f.events).await?;
            assert!(
                matches!(&event, PlayerEvent::Playing { item, position, .. }
                    if *item == hosted && *position == Duration::from_secs(3)),
                "{event:?}"
            );
            assert_eq!(
                f.host.take(1).await?,
                vec![hand_over(HOST, &["1440"], 1, 3_000, 1.0, true)]
            );
            assert_eq!(internal.output_opens.load(Ordering::SeqCst), 0);
            let _ = std::fs::remove_file(&f.wav_path);
            f.host.check()
        }

        #[tokio::test]
        async fn an_end_while_paused_brings_the_next_track_up_paused() -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let (first, second) = (f.hosted("1440"), f.hosted("other-7"));
                start_first(f, &[first.clone(), second.clone()], None).await?;
                f.send(PlayerCommand::Pause)?;
                expect_paused(&mut f.events, &first).await?;
                assert_eq!(f.host.take(1).await?, vec![on(2, HostAction::Stop)]);

                // The host ended the track before it saw the pause.
                f.report(ended(1))?;
                expect_ended(&mut f.events, &first).await?;
                expect_loaded(&mut f.events, &second).await?;
                assert_eq!(f.host.take(1).await?, vec![released(3)]);

                f.send(PlayerCommand::Play)?;
                expect_playing(&mut f.events, &second).await?;
                assert_eq!(
                    f.host.take(1).await?,
                    vec![hand_over(OTHER, &["other-7"], 4, 0, 1.0, true)]
                );
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn the_end_of_a_hosted_last_track_ends_the_playlist() -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let hosted = f.hosted("1440");
                start_first(f, std::slice::from_ref(&hosted), None).await?;
                f.report(ended(1))?;
                wait_for(&mut f.events, |event| {
                    matches!(event, PlayerEvent::EndOfPlaylist)
                })
                .await?;
                wait_for(&mut f.events, |event| {
                    matches!(event, PlayerEvent::HostedChanged(None))
                })
                .await?;
                wait_for(&mut f.events, |event| matches!(event, PlayerEvent::Stopped)).await?;
                assert_eq!(f.host.take(1).await?, vec![released(2)]);
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn pause_during_a_load_stops_the_host_and_the_new_track_comes_up_paused() -> Result<()>
        {
            let (internal, f) = hosted_engine(LONG_TRACK_SAMPLES, &[1]);
            let opens = internal.output_opens.clone();
            with_engine(internal, f, async |f| {
                let (hosted, track) = (f.hosted("1440"), f.local(0));
                start_first(f, &[hosted.clone(), track.clone()], None).await?;
                f.send(PlayerCommand::Switch(1))?;
                tokio::time::timeout(TEST_DEADLINE, f.started.notified()).await?;

                f.send(PlayerCommand::Pause)?;
                expect_paused(&mut f.events, &hosted).await?;
                assert_eq!(f.host.take(1).await?, vec![on(2, HostAction::Stop)]);

                f.release.notify_one();
                expect_loaded(&mut f.events, &track).await?;
                assert_eq!(f.host.take(1).await?, vec![released(3)]);
                assert_eq!(opens.load(Ordering::SeqCst), 0);
                Ok(())
            })
            .await
        }

        // Runs `event` while the second track loads behind a hosted one, and returns what the
        // player reported until the second track plays.
        async fn during_a_load(f: &mut Fixture, event: HostEvent) -> Result<Vec<PlayerEvent>> {
            let (hosted, track) = (f.hosted("1440"), f.local(0));
            start_first(f, &[hosted, track.clone(), f.local(1)], None).await?;
            f.report(status(1, true, 0))?;
            f.send(PlayerCommand::Switch(1))?;
            tokio::time::timeout(TEST_DEADLINE, f.started.notified()).await?;
            // The event is queued before the load can complete.
            f.report(event)?;
            f.release.notify_one();
            let seen = until(
                &mut f.events,
                |event| matches!(event, PlayerEvent::Playing { item, .. } if *item == track),
            )
            .await?;
            assert_eq!(f.host.take(1).await?, vec![released(2)]);
            Ok(seen)
        }

        #[tokio::test]
        async fn an_end_during_a_load_leaves_the_queue_to_the_load() -> Result<()> {
            let (internal, f) = hosted_engine(LONG_TRACK_SAMPLES, &[1]);
            with_engine(internal, f, async |f| {
                let hosted = f.hosted("1440");
                let seen = during_a_load(f, ended(1)).await?;
                assert_eq!(ends(&seen, &hosted), 0, "{seen:?}");
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn a_failure_during_a_load_marks_the_track_and_leaves_the_queue_to_the_load()
        -> Result<()> {
            let (internal, f) = hosted_engine(LONG_TRACK_SAMPLES, &[1]);
            with_engine(internal, f, async |f| {
                let hosted = f.hosted("1440");
                let seen = during_a_load(f, failed(1, HostFailure::Network)).await?;
                assert!(
                    any(&seen, |event| matches!(
                        event,
                        PlayerEvent::Unavailable(item, UnavailableReason::SourceUnreachable)
                            if *item == hosted
                    )),
                    "{seen:?}"
                );
                assert!(
                    !any(&seen, |event| matches!(event, PlayerEvent::Stopped)),
                    "{seen:?}"
                );
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn a_track_the_host_fails_is_unavailable_and_the_next_one_plays() -> Result<()> {
            let (internal, f) = hosted_engine(LONG_TRACK_SAMPLES, &[]);
            with_engine(internal, f, async |f| {
                let (hosted, track) = (f.hosted("1440"), f.local(0));
                start_first(f, &[hosted.clone(), track.clone()], None).await?;
                f.report(failed(1, HostFailure::NotAuthorized))?;
                assert_eq!(
                    expect_unavailable(&mut f.events, &hosted).await?,
                    UnavailableReason::AccessRevoked
                );
                expect_playing(&mut f.events, &track).await?;
                assert_eq!(f.host.take(1).await?, vec![released(2)]);
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn an_unplayable_track_is_gone() -> Result<()> {
            let (internal, f) = hosted_engine(LONG_TRACK_SAMPLES, &[]);
            with_engine(internal, f, async |f| {
                let (hosted, track) = (f.hosted("1440"), f.local(0));
                start_first(f, &[hosted.clone(), track.clone()], None).await?;
                f.report(failed(1, HostFailure::Unplayable))?;
                assert_eq!(
                    expect_unavailable(&mut f.events, &hosted).await?,
                    UnavailableReason::TrackGone
                );
                expect_playing(&mut f.events, &track).await?;
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn a_host_that_cannot_play_fails_its_tracks_until_it_can() -> Result<()> {
            let (internal, f) = hosted_engine(LONG_TRACK_SAMPLES, &[]);
            with_engine(internal, f, async |f| {
                let (first, second, track) = (f.hosted("1440"), f.hosted("1441"), f.local(0));
                let queue = [first.clone(), second.clone(), track.clone()];
                start_first(f, &queue, Some("1441")).await?;

                // The track it holds next fails at once, without going to the host.
                f.report(ready(HOST, true, false))?;
                assert_eq!(
                    expect_unavailable(&mut f.events, &first).await?,
                    UnavailableReason::LibraryUnavailable
                );
                assert_eq!(
                    expect_unavailable(&mut f.events, &second).await?,
                    UnavailableReason::LibraryUnavailable
                );
                expect_playing(&mut f.events, &track).await?;
                assert_eq!(f.host.take(1).await?, vec![released(3)]);

                f.report(ready(HOST, true, true))?;
                f.send(PlayerCommand::Switch(1))?;
                expect_playing(&mut f.events, &second).await?;
                assert_eq!(f.host.take(1).await?, vec![handed_over("1441", 4)]);
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn a_host_without_authorization_fails_its_tracks_as_access_revoked() -> Result<()> {
            let (internal, f) = hosted_engine(LONG_TRACK_SAMPLES, &[]);
            with_engine(internal, f, async |f| {
                let (first, second, track) = (f.hosted("1440"), f.hosted("1441"), f.local(0));
                let queue = [first.clone(), second.clone(), track.clone()];
                start_first(f, &queue, Some("1441")).await?;
                f.report(ready(HOST, false, true))?;
                assert_eq!(
                    expect_unavailable(&mut f.events, &first).await?,
                    UnavailableReason::AccessRevoked
                );
                assert_eq!(
                    expect_unavailable(&mut f.events, &second).await?,
                    UnavailableReason::AccessRevoked
                );
                expect_playing(&mut f.events, &track).await?;
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn without_a_host_bridge_a_hosted_track_is_unavailable() -> Result<()> {
            let (mut internal, f) = hosted_engine(LONG_TRACK_SAMPLES, &[]);
            internal.host_bridge = Arc::new(RwLock::new(None));
            with_engine(internal, f, async |f| {
                let (hosted, track) = (f.hosted("1440"), f.local(0));
                f.queue(&[hosted.clone(), track.clone()])?;
                f.send(PlayerCommand::Switch(0))?;
                assert_eq!(
                    expect_unavailable(&mut f.events, &hosted).await?,
                    UnavailableReason::LibraryUnavailable
                );
                expect_playing(&mut f.events, &track).await?;
                assert!(f.host.sent_nothing());
                Ok(())
            })
            .await
        }

        #[tokio::test(start_paused = true)]
        async fn a_track_whose_host_never_reports_is_unreachable() -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let (first, second) = (f.hosted("1440"), f.hosted("1441"));
                start_first(f, &[first.clone(), second.clone()], Some("1441")).await?;
                tokio::time::sleep(Duration::from_secs(9)).await;
                f.assert_all_available();

                tokio::time::sleep(Duration::from_secs(2)).await;
                assert_eq!(
                    expect_unavailable(&mut f.events, &first).await?,
                    UnavailableReason::SourceUnreachable
                );
                expect_playing(&mut f.events, &second).await?;
                assert_eq!(
                    f.host.take(2).await?,
                    vec![released(3), handed_over("1441", 4)]
                );
                Ok(())
            })
            .await
        }

        #[tokio::test(start_paused = true)]
        async fn a_paused_track_waits_for_its_host_without_a_deadline() -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let hosted = f.hosted("1440");
                start_first(f, std::slice::from_ref(&hosted), None).await?;
                f.send(PlayerCommand::Pause)?;
                expect_paused(&mut f.events, &hosted).await?;
                tokio::time::sleep(Duration::from_secs(60)).await;
                f.assert_all_available();

                // The host has as long again once it is told to play.
                f.send(PlayerCommand::Play)?;
                expect_playing(&mut f.events, &hosted).await?;
                tokio::time::sleep(Duration::from_secs(9)).await;
                f.assert_all_available();
                tokio::time::sleep(Duration::from_secs(2)).await;
                assert_eq!(
                    expect_unavailable(&mut f.events, &hosted).await?,
                    UnavailableReason::SourceUnreachable
                );
                Ok(())
            })
            .await
        }

        #[tokio::test(start_paused = true)]
        async fn a_host_that_goes_quiet_while_playing_fails_its_track() -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let (first, second) = (f.hosted("1440"), f.hosted("1441"));
                start_first(f, &[first.clone(), second.clone()], Some("1441")).await?;
                f.report(status(1, true, 0))?;
                f.settle().await?;
                tokio::time::sleep(Duration::from_secs(14)).await;
                f.assert_all_available();

                tokio::time::sleep(Duration::from_secs(2)).await;
                assert_eq!(
                    expect_unavailable(&mut f.events, &first).await?,
                    UnavailableReason::SourceUnreachable
                );
                expect_playing(&mut f.events, &second).await?;
                Ok(())
            })
            .await
        }

        #[tokio::test(start_paused = true)]
        async fn a_host_that_comes_up_again_gets_its_tracks_again_where_they_stood() -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let (first, second) = (f.hosted("1440"), f.hosted("1441"));
                let queue = [first.clone(), second.clone(), f.hosted("1442")];
                start_first(f, &queue, Some("1441")).await?;
                f.report(status(2, true, 5_000))?;
                f.settle().await?;
                tokio::time::advance(Duration::from_secs(2)).await;
                f.report(f.host.restart())?;
                assert_eq!(
                    f.host.take(1).await?,
                    vec![hand_over(HOST, &["1440", "1441"], 3, 7_000, 1.0, true)]
                );

                // The queue starts over, so the host moves on to index 1 again.
                f.report(f.host.move_on())?;
                expect_ended(&mut f.events, &first).await?;
                expect_playing(&mut f.events, &second).await?;
                assert_eq!(f.host.take(1).await?, vec![added(4, "1442")]);

                // A paused track goes back paused.
                f.send(PlayerCommand::Pause)?;
                let paused_at = expect_paused(&mut f.events, &second).await?;
                assert_eq!(f.host.take(1).await?, vec![on(5, HostAction::Stop)]);
                f.report(f.host.restart())?;
                let offset = u64::try_from(paused_at.as_millis())?;
                assert_eq!(
                    f.host.take(1).await?,
                    vec![hand_over(HOST, &["1441", "1442"], 6, offset, 1.0, false)]
                );
                let seen = f.settle().await?;
                assert!(
                    !any(&seen, |event| matches!(event, PlayerEvent::Unavailable(..))),
                    "{seen:?}"
                );
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn a_host_that_loses_its_track_gets_it_again_once() -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let hosted = f.hosted("1440");
                start_first(f, std::slice::from_ref(&hosted), None).await?;
                f.send(PlayerCommand::Pause)?;
                expect_paused(&mut f.events, &hosted).await?;
                assert_eq!(f.host.take(1).await?, vec![on(2, HostAction::Stop)]);

                f.report(lost(2))?;
                assert_eq!(
                    f.host.take(1).await?,
                    vec![hand_over(HOST, &["1440"], 3, 0, 1.0, false)]
                );
                f.report(lost(3))?;
                assert_eq!(
                    expect_unavailable(&mut f.events, &hosted).await?,
                    UnavailableReason::SourceUnreachable
                );
                assert_eq!(f.host.take(1).await?, vec![released(4)]);
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn a_host_that_plays_another_track_in_its_place_gets_it_again_once() -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let hosted = f.hosted("1440");
                start_first(f, std::slice::from_ref(&hosted), None).await?;
                f.send(PlayerCommand::Pause)?;
                let paused_at = expect_paused(&mut f.events, &hosted).await?;
                assert_eq!(f.host.take(1).await?, vec![on(2, HostAction::Stop)]);

                f.report(status_of(2, "9999", false, 0))?;
                let offset = u64::try_from(paused_at.as_millis())?;
                assert_eq!(
                    f.host.take(1).await?,
                    vec![hand_over(HOST, &["1440"], 3, offset, 1.0, false)]
                );
                f.report(status_of(3, "9999", false, 0))?;
                assert_eq!(
                    expect_unavailable(&mut f.events, &hosted).await?,
                    UnavailableReason::SourceUnreachable
                );
                assert_eq!(f.host.take(1).await?, vec![released(4)]);
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn events_about_an_older_track_or_from_another_host_change_nothing() -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let hosted = f.hosted("1440");
                start_first(f, std::slice::from_ref(&hosted), None).await?;
                f.send(PlayerCommand::Switch(0))?;
                expect_playing(&mut f.events, &hosted).await?;
                assert_eq!(f.host.take(1).await?, vec![handed_over("1440", 2)]);

                let other = OTHER.to_owned();
                for event in [
                    ended(1),
                    failed(1, HostFailure::Unplayable),
                    status(1, false, 5_000),
                    lost(1),
                    HostEvent::JukeboxEnded {
                        host: other.clone(),
                        revision: 2,
                    },
                    HostEvent::JukeboxItemFailed {
                        host: other.clone(),
                        revision: 2,
                        index: 0,
                        reason: HostFailure::Network,
                    },
                    HostEvent::JukeboxStatus {
                        host: other.clone(),
                        revision: 2,
                        current_index: None,
                        current_id: None,
                        playing: false,
                        position_ms: 5_000,
                    },
                    ready(&other, false, false),
                    HostEvent::Unknown,
                ] {
                    f.report(event)?;
                }
                for event in f.settle().await? {
                    match event {
                        PlayerEvent::Progress { position, .. } => {
                            assert!(position < Duration::from_secs(1), "{position:?}");
                        }
                        PlayerEvent::PlaylistUpdated(_) | PlayerEvent::RealtimeFFT(_) => {}
                        event => panic!("{event:?}"),
                    }
                }
                assert!(f.host.sent_nothing());

                // An event about the latest batch still counts.
                f.report(status(2, false, 1_000))?;
                assert_eq!(
                    expect_paused(&mut f.events, &hosted).await?,
                    Duration::from_secs(1)
                );
                Ok(())
            })
            .await
        }

        #[tokio::test(start_paused = true)]
        async fn a_status_from_before_a_pause_or_a_seek_is_dropped() -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let hosted = f.hosted("1440");
                start_first(f, std::slice::from_ref(&hosted), None).await?;
                f.send(PlayerCommand::Pause)?;
                let paused_at = expect_paused(&mut f.events, &hosted).await?;
                assert_eq!(f.host.take(1).await?, vec![on(2, HostAction::Stop)]);

                // The host still played when it reported.
                f.report(status(1, true, 3_000))?;
                f.settle().await?;
                while f.events.try_recv().is_ok() {}
                assert_eq!(next_progress(&mut f.events).await?, paused_at);

                f.send(PlayerCommand::Seek(30.0))?;
                expect_paused(&mut f.events, &hosted).await?;
                f.report(status(2, false, 600))?;
                f.settle().await?;
                while f.events.try_recv().is_ok() {}
                assert_eq!(next_progress(&mut f.events).await?, Duration::from_secs(30));
                f.assert_all_available();
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn repeat_one_hands_the_same_track_to_its_host_again() -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let (hosted, track) = (f.hosted("1440"), f.local(0));
                f.send(PlayerCommand::SetPlaybackMode(PlaybackMode::RepeatOne))?;
                start_first(f, &[hosted.clone(), track], None).await?;
                f.report(status(1, true, 0))?;
                f.report(ended(1))?;
                expect_ended(&mut f.events, &hosted).await?;
                expect_playing(&mut f.events, &hosted).await?;
                assert_eq!(f.host.take(1).await?, vec![handed_over("1440", 2)]);
                Ok(())
            })
            .await
        }

        #[tokio::test(start_paused = true)]
        async fn a_hosted_track_moves_on_from_its_report_while_it_plays_only() -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let hosted = f.hosted("1440");
                start_first(f, std::slice::from_ref(&hosted), None).await?;
                f.report(status(1, true, 10_000))?;
                f.settle().await?;
                tokio::time::advance(Duration::from_millis(400)).await;

                f.send(PlayerCommand::Pause)?;
                let paused_at = expect_paused(&mut f.events, &hosted).await?;
                assert_eq!(paused_at, Duration::from_millis(10_400));
                tokio::time::sleep(Duration::from_secs(1)).await;
                while f.events.try_recv().is_ok() {}
                assert_eq!(next_progress(&mut f.events).await?, paused_at);

                // A report moves a paused track too.
                f.report(status(2, false, 20_000))?;
                f.settle().await?;
                while f.events.try_recv().is_ok() {}
                assert_eq!(next_progress(&mut f.events).await?, Duration::from_secs(20));
                Ok(())
            })
            .await
        }

        // The host holds the next track, so it moves on to it without a gap.

        #[tokio::test]
        async fn consecutive_hosted_tracks_are_handed_over_once() -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            let opens = internal.output_opens.clone();
            with_engine(internal, f, async |f| {
                let (first, second, third) = (f.hosted("1440"), f.hosted("1441"), f.hosted("1442"));
                let queue = [first.clone(), second.clone(), third.clone()];
                start_first(f, &queue, Some("1441")).await?;
                f.report(f.host.status())?;

                // The host moves on by itself. The first track played through, and the second
                // is current without being handed over again.
                f.report(f.host.move_on())?;
                expect_ended(&mut f.events, &first).await?;
                let event = next_track_state(&mut f.events).await?;
                assert!(
                    matches!(&event, PlayerEvent::Playing { item, index: 1, path, position, .. }
                        if *item == second
                            && path.as_os_str() == "hosted://appleMusic/1441"
                            && position.is_zero()),
                    "{event:?}"
                );
                // The host gets the track after it, and nothing else.
                assert_eq!(f.host.take(1).await?, vec![added(3, "1442")]);

                // A report about the entry the second track stands at only says where it is.
                f.report(f.host.status())?;
                let seen = f.settle().await?;
                assert!(
                    !any(&seen, |event| matches!(
                        event,
                        PlayerEvent::EndOfTrack { .. } | PlayerEvent::Playing { .. }
                    )),
                    "{seen:?}"
                );
                f.report(f.host.move_on())?;
                expect_ended(&mut f.events, &second).await?;
                expect_playing(&mut f.events, &third).await?;

                // Nothing follows the last track, so the host's queue ends with it.
                f.report(f.host.end())?;
                expect_ended(&mut f.events, &third).await?;
                wait_for(&mut f.events, |event| {
                    matches!(event, PlayerEvent::EndOfPlaylist)
                })
                .await?;
                assert_eq!(f.host.take(1).await?, vec![released(4)]);
                assert_eq!(opens.load(Ordering::SeqCst), 0);
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn a_queue_edit_changes_the_next_track_in_place() -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let first = f.hosted("1440");
                let queue = [first.clone(), f.hosted("1441"), f.hosted("1442")];
                start_first(f, &queue, Some("1441")).await?;

                // Moving the third track before the second makes it the next one.
                f.send(PlayerCommand::MovePlayListItem {
                    old_index: 2,
                    new_index: 1,
                })?;
                assert_eq!(f.host.take(2).await?, vec![removed(3, 1), added(4, "1442")]);

                // An edit that leaves the next track as it is changes nothing for the host.
                f.queue(&[f.hosted("1443"), f.local(0)])?;
                let mut seen = f.settle().await?;
                assert!(f.host.sent_nothing());

                // Removing the next track makes the one after it next.
                f.send(PlayerCommand::RemoveFromPlaylist { index: 1 })?;
                assert_eq!(f.host.take(2).await?, vec![removed(5, 1), added(6, "1441")]);

                // So does a track to play next.
                f.send(PlayerCommand::AddToPlaylist {
                    tracks: vec![(f.hosted("1444"), PlaybackSource::LibraryStream)],
                    mode: AddMode::PlayNext,
                })?;
                assert_eq!(f.host.take(2).await?, vec![removed(7, 1), added(8, "1444")]);

                // The first track played on throughout.
                seen.extend(f.settle().await?);
                assert!(
                    !any(&seen, |event| matches!(
                        event,
                        PlayerEvent::Playing { .. }
                            | PlayerEvent::Paused { .. }
                            | PlayerEvent::Loaded { .. }
                            | PlayerEvent::Stopped
                            | PlayerEvent::EndOfTrack { .. }
                    )),
                    "{seen:?}"
                );
                assert_eq!(
                    f.host.queue(),
                    (
                        vec!["1440".to_owned(), "1444".to_owned()],
                        Some("1440".to_owned())
                    )
                );
                f.report(f.host.move_on())?;
                expect_ended(&mut f.events, &first).await?;
                let next = f.hosted("1444");
                expect_playing(&mut f.events, &next).await?;
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn no_track_is_handed_over_ahead_under_repeat_one_or_before_a_local_one() -> Result<()>
        {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let (first, second, track) = (f.hosted("1440"), f.hosted("1441"), f.local(0));
                f.send(PlayerCommand::SetPlaybackMode(PlaybackMode::RepeatOne))?;
                start_first(f, &[first, second.clone(), track.clone()], None).await?;

                // Leaving repeat-one gives the host the next track, and going back takes it
                // away again.
                f.send(PlayerCommand::SetPlaybackMode(PlaybackMode::Sequential))?;
                assert_eq!(f.host.take(1).await?, vec![added(2, "1441")]);
                f.send(PlayerCommand::SetPlaybackMode(PlaybackMode::RepeatOne))?;
                assert_eq!(f.host.take(1).await?, vec![removed(3, 1)]);

                // Before a local track the host holds nothing next.
                f.send(PlayerCommand::Switch(1))?;
                expect_playing(&mut f.events, &second).await?;
                assert_eq!(f.host.take(1).await?, vec![handed_over("1441", 4)]);
                f.send(PlayerCommand::SetPlaybackMode(PlaybackMode::Sequential))?;
                f.settle().await?;
                assert!(f.host.sent_nothing());

                // Its end is the end of the host's queue, and the local track plays.
                f.report(f.host.end())?;
                expect_playing(&mut f.events, &track).await?;
                assert_eq!(f.host.take(1).await?, vec![released(5)]);
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn next_skips_to_the_track_the_host_holds_and_previous_hands_over_anew() -> Result<()>
        {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let (first, second) = (f.hosted("1440"), f.hosted("1441"));
                let queue = [first.clone(), second.clone(), f.hosted("1442")];
                start_first(f, &queue, Some("1441")).await?;
                f.report(status(2, true, 4_000))?;

                // Next goes to the track the host holds next with a skip. A track the user skips
                // does not count as played through.
                f.send(PlayerCommand::Next)?;
                let seen = f.settle().await?;
                assert!(
                    any(
                        &seen,
                        |event| matches!(event, PlayerEvent::Playing { item, position, .. }
                        if *item == second && position.is_zero())
                    ),
                    "{seen:?}"
                );
                assert_eq!(ends(&seen, &first), 0, "{seen:?}");
                assert_eq!(
                    f.host.take(2).await?,
                    vec![skipped_to(3, 1), added(4, "1442")]
                );

                // A seek moves the current track only, at its own index.
                f.send(PlayerCommand::Seek(30.0))?;
                assert_eq!(
                    expect_playing(&mut f.events, &second).await?,
                    Duration::from_secs(30)
                );
                let skip = HostAction::Skip {
                    index: 1,
                    offset_ms: 30_000,
                };
                assert_eq!(f.host.take(1).await?, vec![on(5, skip)]);

                // Previous hands the earlier track over anew, and the track after it follows.
                f.send(PlayerCommand::Previous)?;
                expect_playing(&mut f.events, &first).await?;
                assert_eq!(
                    f.host.take(2).await?,
                    vec![handed_over("1440", 6), added(7, "1441")]
                );
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn a_move_on_to_a_track_taken_out_meanwhile_loads_what_comes_next() -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let (first, track) = (f.hosted("1440"), f.local(0));
                start_first(
                    f,
                    &[first.clone(), f.hosted("1441"), track.clone()],
                    Some("1441"),
                )
                .await?;
                f.report(f.host.status())?;

                // The host moves on to the second track, and its report is still on its way when
                // the second track leaves the queue. The host plays on, as it never removes the
                // entry it plays.
                let moved = f.host.move_on();
                f.send(PlayerCommand::RemoveFromPlaylist { index: 1 })?;
                assert_eq!(f.host.take(1).await?, vec![removed(3, 1)]);
                assert_eq!(f.host.queue().1.as_deref(), Some("1441"));

                // The first track counts as played through once, and what comes next plays.
                f.report(moved)?;
                let seen = until(
                    &mut f.events,
                    |event| matches!(event, PlayerEvent::Playing { item, .. } if *item == track),
                )
                .await?;
                assert_eq!(ends(&seen, &first), 1, "{seen:?}");
                assert!(
                    !any(&seen, |event| matches!(
                        event,
                        PlayerEvent::Unavailable(..) | PlayerEvent::Playing { .. }
                    )),
                    "{seen:?}"
                );
                assert_eq!(f.host.take(1).await?, vec![released(4)]);
                let seen = f.settle().await?;
                assert_eq!(ends(&seen, &first), 0, "{seen:?}");
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn a_move_on_that_crosses_a_new_next_track_hands_that_one_over() -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let (first, second, third) = (f.hosted("1440"), f.hosted("1441"), f.hosted("1442"));
                start_first(
                    f,
                    &[first.clone(), second.clone(), third.clone()],
                    Some("1441"),
                )
                .await?;
                f.report(f.host.status())?;

                // The host moves on to the second track as the third becomes the next one. It
                // keeps the second, and reports it at index 1, where the player keeps the third.
                let moved = f.host.move_on();
                f.send(PlayerCommand::MovePlayListItem {
                    old_index: 2,
                    new_index: 1,
                })?;
                assert_eq!(f.host.take(2).await?, vec![removed(3, 1), added(4, "1442")]);
                f.report(f.host.status())?;

                let seen = until(
                    &mut f.events,
                    |event| matches!(event, PlayerEvent::Playing { item, .. } if *item == third),
                )
                .await?;
                assert_eq!(ends(&seen, &first), 1, "{seen:?}");
                assert!(
                    !any(&seen, |event| matches!(event, PlayerEvent::Playing { .. })),
                    "{seen:?}"
                );
                assert_eq!(
                    f.host.take(2).await?,
                    vec![handed_over("1442", 5), added(6, "1441")]
                );

                // The report that the host sent as it moved on comes late, and changes nothing.
                f.report(moved)?;
                let seen = f.settle().await?;
                assert!(
                    !any(&seen, |event| matches!(
                        event,
                        PlayerEvent::EndOfTrack { .. } | PlayerEvent::Playing { .. }
                    )),
                    "{seen:?}"
                );
                assert_eq!(f.host.queue().1.as_deref(), Some("1442"));
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn next_as_the_host_moves_on_starts_the_next_track_again() -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let (first, second) = (f.hosted("1440"), f.hosted("1441"));
                start_first(
                    f,
                    &[first.clone(), second.clone(), f.hosted("1442")],
                    Some("1441"),
                )
                .await?;
                f.report(f.host.status())?;

                let moved = f.host.move_on();
                f.send(PlayerCommand::Next)?;
                let mut seen = f.settle().await?;
                assert_eq!(
                    f.host.take(2).await?,
                    vec![skipped_to(3, 1), added(4, "1442")]
                );
                // The host plays the second track from its start again, and the report from
                // before the skip is stale.
                f.report(moved)?;
                f.report(f.host.status())?;
                seen.extend(f.settle().await?);
                let playing = seen
                    .iter()
                    .filter(|event| matches!(event, PlayerEvent::Playing { .. }))
                    .collect::<Vec<_>>();
                assert!(
                    matches!(playing[..], [PlayerEvent::Playing { item, position, .. }]
                        if *item == second && position.is_zero()),
                    "{seen:?}"
                );
                assert_eq!(ends(&seen, &first), 0, "{seen:?}");
                assert_eq!(f.host.queue().1.as_deref(), Some("1441"));
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn a_move_on_that_crosses_a_pause_brings_the_next_track_up_paused() -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let (first, second) = (f.hosted("1440"), f.hosted("1441"));
                start_first(
                    f,
                    &[first.clone(), second.clone(), f.hosted("1442")],
                    Some("1441"),
                )
                .await?;

                // The host moved on before it saw the pause, and its report from before the
                // pause is stale. It reports again once it applied the pause.
                let moved = f.host.move_on();
                f.send(PlayerCommand::Pause)?;
                expect_paused(&mut f.events, &first).await?;
                assert_eq!(f.host.take(1).await?, vec![on(3, HostAction::Stop)]);
                f.report(moved)?;
                f.report(f.host.status())?;
                expect_ended(&mut f.events, &first).await?;
                assert_eq!(expect_paused(&mut f.events, &second).await?, Duration::ZERO);
                assert_eq!(f.host.take(1).await?, vec![added(4, "1442")]);

                f.send(PlayerCommand::Play)?;
                expect_playing(&mut f.events, &second).await?;
                assert_eq!(f.host.take(1).await?, vec![on(5, HostAction::Start)]);
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn volume_changes_do_not_make_the_reports_of_the_host_stale() -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let (first, second) = (f.hosted("1440"), f.hosted("1441"));
                start_first(
                    f,
                    &[first.clone(), second.clone(), f.hosted("1442")],
                    Some("1441"),
                )
                .await?;
                let moved = f.host.move_on();
                for volume in [0.5, 0.4, 0.3] {
                    f.send(PlayerCommand::SetVolume(volume))?;
                }
                assert_eq!(
                    f.host.take(3).await?,
                    vec![
                        on(3, HostAction::SetGain { gain: 0.5 }),
                        on(4, HostAction::SetGain { gain: 0.4 }),
                        on(5, HostAction::SetGain { gain: 0.3 }),
                    ]
                );
                f.report(moved)?;
                expect_ended(&mut f.events, &first).await?;
                expect_playing(&mut f.events, &second).await?;
                assert_eq!(f.host.take(1).await?, vec![added(6, "1442")]);
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn a_next_track_the_host_fails_leaves_its_queue_and_is_handed_over_as_usual()
        -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let (first, second) = (f.hosted("1440"), f.hosted("1441"));
                start_first(
                    f,
                    &[first.clone(), second.clone(), f.hosted("1442")],
                    Some("1441"),
                )
                .await?;

                // A failure from before the host held the second track is about nothing it holds.
                f.report(failed_at(1, 1, HostFailure::Network))?;
                let mut seen = f.settle().await?;
                assert!(f.host.sent_nothing());

                f.report(failed_at(2, 1, HostFailure::Network))?;
                assert_eq!(f.host.take(1).await?, vec![removed(3, 1)]);
                seen.extend(f.settle().await?);
                assert!(f.host.sent_nothing());
                assert!(
                    !any(&seen, |event| matches!(event, PlayerEvent::Unavailable(..))),
                    "{seen:?}"
                );

                // At the end of the current track the next one is handed over as usual.
                f.report(f.host.end())?;
                expect_ended(&mut f.events, &first).await?;
                expect_playing(&mut f.events, &second).await?;
                assert_eq!(
                    f.host.take(2).await?,
                    vec![handed_over("1441", 4), added(5, "1442")]
                );
                Ok(())
            })
            .await
        }

        #[tokio::test(start_paused = true)]
        async fn the_next_track_is_looked_up_for_five_seconds_without_holding_up_the_current_one()
        -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let first = f.hosted("1440");
                f.slow.delay_ms.store(6_000, Ordering::SeqCst);
                let begun = Instant::now();
                f.queue(&[first.clone(), f.hosted("slow-1")])?;
                f.send(PlayerCommand::Switch(0))?;
                expect_playing(&mut f.events, &first).await?;
                assert_eq!(f.host.take(1).await?, vec![handed_over("1440", 1)]);
                assert!(begun.elapsed() < Duration::from_secs(1));
                f.report(status(1, true, 0))?;

                // An answer that would take six seconds is not waited for.
                tokio::time::sleep(Duration::from_secs(7)).await;
                assert!(f.host.sent_nothing());
                assert!(f.slow.dropped.load(Ordering::SeqCst));
                assert!(!f.slow.answered.load(Ordering::SeqCst));

                // One in four seconds is.
                f.slow.delay_ms.store(4_000, Ordering::SeqCst);
                f.report(status(1, true, 7_000))?;
                f.send(PlayerCommand::SetPlaybackMode(PlaybackMode::RepeatOne))?;
                f.send(PlayerCommand::SetPlaybackMode(PlaybackMode::Sequential))?;
                assert_eq!(f.host.take(1).await?, vec![added(2, "slow-1")]);
                assert!(f.slow.answered.load(Ordering::SeqCst));
                Ok(())
            })
            .await
        }

        #[tokio::test(start_paused = true)]
        async fn an_answer_to_an_older_question_about_the_next_track_is_dropped() -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                f.slow.delay_ms.store(4_000, Ordering::SeqCst);
                let queue = [f.hosted("1440"), f.hosted("1441"), f.hosted("slow-1")];
                start_first(f, &queue, Some("1441")).await?;
                f.report(status(2, true, 0))?;

                // The slow track becomes the next one, and an answer to the question about the
                // track before it comes late.
                f.send(PlayerCommand::MovePlayListItem {
                    old_index: 2,
                    new_index: 1,
                })?;
                assert_eq!(f.host.take(1).await?, vec![removed(3, 1)]);
                f.send(PlayerCommand::HostNextResolved {
                    request: 1,
                    track: Some(HostedTrack {
                        host: HOST.to_owned(),
                        key: "1441".to_owned(),
                    }),
                })?;
                f.settle().await?;
                assert!(f.host.sent_nothing());

                assert_eq!(f.host.take(1).await?, vec![added(4, "slow-1")]);
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn a_next_track_the_resolver_fails_on_is_not_handed_over_ahead() -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let (first, broken) = (f.hosted("1440"), f.hosted("error-1"));
                f.queue(&[first.clone(), broken.clone()])?;
                f.send(PlayerCommand::Switch(0))?;
                expect_playing(&mut f.events, &first).await?;
                assert_eq!(f.host.take(1).await?, vec![handed_over("1440", 1)]);
                tokio::time::timeout(TEST_DEADLINE, f.failed.notified()).await?;
                f.settle().await?;
                assert!(f.host.sent_nothing());

                // Once current, it fails as a load does.
                f.report(ended(1))?;
                expect_ended(&mut f.events, &first).await?;
                expect_unavailable(&mut f.events, &broken).await?;
                wait_for(&mut f.events, |event| {
                    matches!(event, PlayerEvent::EndOfPlaylist)
                })
                .await?;
                assert_eq!(f.host.take(1).await?, vec![released(2)]);
                Ok(())
            })
            .await
        }

        #[tokio::test(start_paused = true)]
        async fn a_change_of_mode_stops_the_lookup_of_a_track_that_no_longer_comes_next()
        -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let first = f.hosted("1440");
                f.slow.delay_ms.store(4_000, Ordering::SeqCst);
                f.queue(&[first.clone(), f.hosted("slow-1")])?;
                f.send(PlayerCommand::Switch(0))?;
                expect_playing(&mut f.events, &first).await?;
                assert_eq!(f.host.take(1).await?, vec![handed_over("1440", 1)]);
                f.report(status(1, true, 0))?;
                tokio::time::sleep(Duration::from_secs(1)).await;

                f.send(PlayerCommand::SetPlaybackMode(PlaybackMode::RepeatOne))?;
                f.settle().await?;
                tokio::time::timeout(TEST_DEADLINE, f.slow.on_drop.notified()).await?;
                tokio::time::sleep(Duration::from_secs(5)).await;
                assert!(!f.slow.answered.load(Ordering::SeqCst));
                assert!(f.host.sent_nothing());
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn shuffle_keeps_the_track_the_host_holds_next_where_next_goes() -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let keys = ["1440", "1441", "1442", "1443", "1444"];
                let queue = keys.map(|key| f.hosted(key));
                start_first(f, &queue, Some("1441")).await?;
                f.report(f.host.status())?;

                // The shuffled order may bring another track next, which the host gets in place.
                f.send(PlayerCommand::SetPlaybackMode(PlaybackMode::Shuffle))?;
                f.settle().await?;
                let mut batches = f.host.take(0).await?;
                if batches.iter().any(|batch| {
                    batch
                        .actions
                        .iter()
                        .any(|action| matches!(action, HostAction::Remove { .. }))
                }) {
                    batches.extend(f.host.take(1).await?);
                }
                assert!(
                    batches
                        .iter()
                        .flat_map(|batch| &batch.actions)
                        .all(|action| matches!(
                            action,
                            HostAction::Remove { index: 1 } | HostAction::Add { .. }
                        )),
                    "{batches:?}"
                );
                let (held, current) = f.host.queue();
                assert_eq!(current.as_deref(), Some("1440"));
                assert_eq!(held.len(), 2, "{held:?}");

                // Next goes where the host holds it.
                f.send(PlayerCommand::Next)?;
                let seen = f.settle().await?;
                let next = f.hosted(&held[1]);
                assert!(
                    any(
                        &seen,
                        |event| matches!(event, PlayerEvent::Playing { item, .. } if *item == next)
                    ),
                    "{seen:?}"
                );
                let batches = f.host.take(1).await?;
                assert_eq!(batches[0].actions, skipped_to(0, 1).actions);
                assert_eq!(f.host.queue().1, Some(held[1].clone()));
                Ok(())
            })
            .await
        }

        #[tokio::test]
        async fn a_stream_found_for_the_next_track_is_let_go() -> Result<()> {
            let (internal, f) = hosted_engine(2, &[]);
            with_engine(internal, f, async |f| {
                let first = f.hosted("1440");
                f.queue(&[first.clone(), f.hosted("stream-1")])?;
                f.send(PlayerCommand::Switch(0))?;
                expect_playing(&mut f.events, &first).await?;
                assert_eq!(f.host.take(1).await?, vec![handed_over("1440", 1)]);
                let stream = tokio::time::timeout(TEST_DEADLINE, f.streams.recv())
                    .await?
                    .context("the resolver handed out no stream")?;
                tokio::time::timeout(TEST_DEADLINE, stream.cancelled()).await?;
                f.settle().await?;
                assert!(f.host.sent_nothing());
                Ok(())
            })
            .await
        }

        // A player whose resolver hosts the tracks of the returned library on `HOST`, and whose
        // local track is `local_uuid`. Its status is followed from the start.
        fn hosted_player(
            local_uuid: Uuid,
        ) -> (
            Player,
            SimpleReceiver<PlayerStatus>,
            Arc<FakeHost>,
            Uuid,
            PathBuf,
        ) {
            let (local, wav_path) = delayed_resolver_fixture_with_samples(
                &[local_uuid],
                Arc::new(Mutex::new(Vec::new())),
                Arc::new(Notify::new()),
                Arc::new(Notify::new()),
                &[],
                LONG_TRACK_SAMPLES,
            );
            let hosted_library = Uuid::new_v4();
            let host = FakeHost::new();
            let player = Player::new(Arc::new(FsIo::new_noop()), None);
            let statuses = player.subscribe_status();
            player.set_source_resolver(Arc::new(MixedResolver {
                local,
                hosted_library,
                slow: Arc::default(),
                streams: mpsc::unbounded_channel().0,
                failed: Arc::default(),
            }));
            player.set_host_bridge(host.clone());
            (player, statuses, host, hosted_library, wav_path)
        }

        async fn status_until(
            statuses: &SimpleReceiver<PlayerStatus>,
            wanted: impl Fn(&PlayerStatus) -> bool,
        ) -> Result<PlayerStatus> {
            tokio::time::timeout(TEST_DEADLINE, async {
                loop {
                    match statuses.recv().await {
                        Ok(status) if wanted(&status) => return Ok(status),
                        Ok(_) => {}
                        Err(error) => bail!("the player status closed: {error:?}"),
                    }
                }
            })
            .await?
        }

        #[tokio::test]
        async fn the_status_names_the_host_while_a_hosted_track_is_current() -> Result<()> {
            let local_uuid = Uuid::new_v4();
            let (player, statuses, host, hosted_library, wav_path) = hosted_player(local_uuid);
            let hosted = PlayingItem::LibraryTrack {
                library_id: hosted_library,
                track: "1440".to_owned(),
            };
            let track = library_item(Uuid::new_v4(), local_uuid);
            player.add_to_playlist_with_sources(
                vec![
                    (hosted.clone(), PlaybackSource::LibraryStream),
                    (track.clone(), PlaybackSource::LibraryStream),
                ],
                AddMode::AppendToEnd,
            );
            player.switch(0);
            let status = status_until(&statuses, |status| {
                status.item.as_ref() == Some(&hosted) && status.state == PlaybackState::Playing
            })
            .await?;
            assert_eq!(status.hosted_by.as_deref(), Some(HOST));
            assert_eq!(host.take(1).await?, vec![handed_over("1440", 1)]);

            player.deliver_host_event(ended(1));
            let status = status_until(&statuses, |status| {
                status.item.as_ref() == Some(&track) && status.state == PlaybackState::Playing
            })
            .await?;
            assert_eq!(status.hosted_by, None);

            player.terminate();
            let _ = std::fs::remove_file(wav_path);
            host.check()
        }

        #[tokio::test]
        async fn a_track_the_host_moved_on_from_counts_as_played_through() -> Result<()> {
            let (player, statuses, host, hosted_library, wav_path) = hosted_player(Uuid::new_v4());
            let played_through = player.subscribe_played_through();
            let hosted = |key: &str| PlayingItem::LibraryTrack {
                library_id: hosted_library,
                track: key.to_owned(),
            };
            player.add_to_playlist_with_sources(
                vec![
                    (hosted("1440"), PlaybackSource::LibraryStream),
                    (hosted("1441"), PlaybackSource::LibraryStream),
                ],
                AddMode::AppendToEnd,
            );
            player.switch(0);
            status_until(&statuses, |status| {
                status.item.as_ref() == Some(&hosted("1440"))
                    && status.state == PlaybackState::Playing
            })
            .await?;
            assert_eq!(
                host.take(2).await?,
                vec![handed_over("1440", 1), added(2, "1441")]
            );

            player.deliver_host_event(host.move_on());
            let played = tokio::time::timeout(TEST_DEADLINE, played_through.recv()).await?;
            assert_eq!(played.ok(), Some(hosted("1440")));
            let status = status_until(&statuses, |status| {
                status.item.as_ref() == Some(&hosted("1441"))
            })
            .await?;
            assert_eq!(status.index, Some(1));
            assert_eq!(status.state, PlaybackState::Playing);
            assert_eq!(status.hosted_by.as_deref(), Some(HOST));
            assert!(host.sent_nothing());

            player.terminate();
            let _ = std::fs::remove_file(wav_path);
            host.check()
        }
    }
}
