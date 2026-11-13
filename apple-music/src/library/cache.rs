//! What the library keeps between calls, and how sign-ins and age retire it.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use super::types::{
    Album, CatalogAlbumDetail, CatalogArtistDetail, CatalogPlaylistDetail, Playlist,
    RecommendationGroup, SongList, TrackKey,
};

/// How long the song list, the album list and each rating are kept, so changes made in
/// the Apple Music app or on another device show up.
pub(super) const TTL: Duration = Duration::from_secs(10 * 60);

/// How long a catalog album, artist or playlist is kept.
pub(super) const DETAIL_TTL: Duration = Duration::from_secs(30);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Default)]
pub(super) struct Caches {
    /// Counts sign-ins with new tokens; reads that started before one are not kept.
    session: AtomicU64,
    pub(super) songs: Slot<SongList>,
    pub(super) albums: Slot<Vec<Album>>,
    /// Apple rating by song, 0 for a song known to be unrated.
    ratings: Mutex<HashMap<TrackKey, Rated>>,
    /// Playlists created here that Apple may not list yet.
    created: Mutex<Vec<Playlist>>,
    /// The user's recommendation groups, without their membership marks.
    pub(super) recommendations: Slot<Vec<RecommendationGroup>>,
    /// Catalog items added to the library here, which Apple may list only a while later.
    added: Mutex<Added>,
    /// The library's artists, by name key.
    pub(super) artists: Slot<HashSet<String>>,
    /// The catalog ids of the songs of the catalog albums added during this sign-in, by
    /// album.
    album_tracks: Mutex<HashMap<String, Vec<String>>>,
    pub(super) album_details: Details<CatalogAlbumDetail>,
    pub(super) playlist_details: Details<CatalogPlaylistDetail>,
    pub(super) artist_details: Details<CatalogArtistDetail>,
}

/// Catalog ids added to the library during this sign-in, by kind.
#[derive(Default)]
pub(super) struct Added {
    pub(super) songs: HashSet<String>,
    pub(super) albums: HashSet<String>,
    pub(super) playlists: HashSet<String>,
}

/// Catalog pages kept for [`DETAIL_TTL`], by catalog id, each with the sign-in it was read
/// under.
pub(super) struct Details<T>(Mutex<HashMap<String, KeptDetail<T>>>);

/// The sign-in, the time of the read and the page.
type KeptDetail<T> = (u64, Instant, Arc<T>);

impl<T> Default for Details<T> {
    fn default() -> Self {
        Self(Mutex::default())
    }
}

impl<T> Details<T> {
    /// The page kept under `id` when it was read under `session` and is younger than
    /// [`DETAIL_TTL`] at `now`.
    pub(super) fn fresh(&self, id: &str, session: u64, now: Instant) -> Option<Arc<T>> {
        let kept = lock(&self.0);
        let (kept_session, at, value) = kept.get(id)?;
        (*kept_session == session && now.saturating_duration_since(*at) < DETAIL_TTL)
            .then(|| value.clone())
    }

    fn clear(&self) {
        lock(&self.0).clear();
    }
}

#[derive(Clone, Copy)]
struct Rated {
    value: i8,
    at: Instant,
}

impl Rated {
    pub(super) fn fresh(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.at) < TTL
    }
}

impl Caches {
    pub(super) fn session(&self) -> u64 {
        self.session.load(Ordering::Acquire)
    }

    /// Forgets everything of the previous sign-in, including reads still in flight.
    pub(super) fn start_session(&self) {
        let mut ratings = lock(&self.ratings);
        let mut created = lock(&self.created);
        self.session.fetch_add(1, Ordering::AcqRel);
        self.songs.invalidate();
        self.albums.invalidate();
        self.recommendations.invalidate();
        self.artists.invalidate();
        ratings.clear();
        created.clear();
        drop((ratings, created));
        *lock(&self.added) = Added::default();
        lock(&self.album_tracks).clear();
        // Cleared after the new session began, so a page being kept is checked against it.
        self.album_details.clear();
        self.playlist_details.clear();
        self.artist_details.clear();
    }

    pub(super) fn added(&self) -> MutexGuard<'_, Added> {
        lock(&self.added)
    }

    pub(super) fn album_tracks(&self) -> MutexGuard<'_, HashMap<String, Vec<String>>> {
        lock(&self.album_tracks)
    }

    /// Records the songs of a catalog album, unless a sign-in happened since `session`.
    pub(super) fn remember_album_tracks(&self, session: u64, album: &str, tracks: Vec<String>) {
        let mut album_tracks = lock(&self.album_tracks);
        if self.session() == session {
            album_tracks.insert(album.to_owned(), tracks);
        }
    }

    /// Records catalog items added to the library, unless a sign-in happened since
    /// `session`.
    pub(super) fn remember_added(&self, session: u64, update: impl FnOnce(&mut Added)) {
        let mut added = lock(&self.added);
        if self.session() == session {
            update(&mut added);
        }
    }

    /// Keeps a catalog page read at `now`, unless a sign-in happened since `session`.
    /// Pages that have grown old are dropped meanwhile.
    pub(super) fn keep_detail<T>(
        &self,
        session: u64,
        details: fn(&Caches) -> &Details<T>,
        id: &str,
        now: Instant,
        value: Arc<T>,
    ) {
        let mut kept = lock(&details(self).0);
        if self.session() != session {
            return;
        }
        kept.retain(|_, (kept_session, at, _)| {
            *kept_session == session && now.saturating_duration_since(*at) < DETAIL_TTL
        });
        kept.insert(id.to_owned(), (session, now, value));
    }

    /// The kept rating of each of `keys` that has one younger than [`TTL`] at `now`.
    pub(super) fn rated(&self, keys: &[TrackKey], now: Instant) -> HashMap<TrackKey, i8> {
        let ratings = lock(&self.ratings);
        keys.iter()
            .filter_map(|key| {
                let rated = ratings.get(key).filter(|rated| rated.fresh(now))?;
                Some((key.clone(), rated.value))
            })
            .collect()
    }

    /// Keeps ratings that were read, unless a sign-in happened since `session`. A rating
    /// kept meanwhile is newer than the read, so it stays.
    pub(super) fn merge_read_ratings(
        &self,
        session: u64,
        now: Instant,
        read: &HashMap<TrackKey, i8>,
    ) {
        let mut ratings = lock(&self.ratings);
        if self.session() != session {
            return;
        }
        for (key, value) in read {
            match ratings.get(key) {
                Some(kept) if kept.fresh(now) => {}
                _ => {
                    ratings.insert(
                        key.clone(),
                        Rated {
                            value: *value,
                            at: now,
                        },
                    );
                }
            }
        }
    }

    /// Keeps a rating that was set, unless a sign-in happened since `session`.
    pub(super) fn set_rating(&self, session: u64, now: Instant, key: &TrackKey, value: i8) {
        let mut ratings = lock(&self.ratings);
        if self.session() == session {
            ratings.insert(key.clone(), Rated { value, at: now });
        }
    }

    pub(super) fn created(&self) -> MutexGuard<'_, Vec<Playlist>> {
        lock(&self.created)
    }

    pub(super) fn remember_created(&self, session: u64, playlist: Playlist) {
        let mut created = lock(&self.created);
        if self.session() == session {
            created.push(playlist);
        }
    }
}

/// A value built on demand and kept for [`TTL`]. One build runs at a time, and the
/// kept value stays readable while it does.
pub(super) struct Slot<T> {
    /// Bumped to retire the kept value and any build in flight.
    epoch: AtomicU64,
    value: Mutex<Option<Cached<T>>>,
    building: tokio::sync::Mutex<()>,
    /// Held while a patch is applied, so patches wait for each other.
    patching: Mutex<()>,
    /// Whether a refresh in the background is running.
    refreshing: AtomicBool,
}

struct Cached<T> {
    epoch: u64,
    built: Instant,
    value: Arc<T>,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Self {
            epoch: AtomicU64::new(0),
            value: Mutex::new(None),
            building: tokio::sync::Mutex::new(()),
            patching: Mutex::new(()),
            refreshing: AtomicBool::new(false),
        }
    }
}

impl<T: Clone> Slot<T> {
    /// The kept value when it is current and younger than `ttl` at `now`, else one
    /// built by `build`. A failed build keeps nothing.
    pub(super) async fn get<E, F, Fut>(
        &self,
        now: Instant,
        ttl: Duration,
        build: F,
    ) -> Result<Arc<T>, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        if let Some(value) = self.fresh(now, ttl) {
            return Ok(value);
        }
        let _building = self.building.lock().await;
        if let Some(value) = self.fresh(now, ttl) {
            return Ok(value);
        }
        let epoch = self.epoch.load(Ordering::Acquire);
        let value = Arc::new(build().await?);
        *lock(&self.value) = Some(Cached {
            epoch,
            built: now,
            value: value.clone(),
        });
        Ok(value)
    }

    pub(super) fn fresh(&self, now: Instant, ttl: Duration) -> Option<Arc<T>> {
        let value = lock(&self.value);
        let cached = value.as_ref()?;
        (cached.epoch == self.epoch.load(Ordering::Acquire)
            && now.saturating_duration_since(cached.built) < ttl)
            .then(|| cached.value.clone())
    }

    /// The kept value of the current epoch, however old.
    pub(super) fn peek(&self) -> Option<Arc<T>> {
        let value = lock(&self.value);
        let cached = value.as_ref()?;
        (cached.epoch == self.epoch.load(Ordering::Acquire)).then(|| cached.value.clone())
    }

    /// Claims the right to refresh in the background; false when one is running.
    pub(super) fn claim_refresh(&self) -> bool {
        !self.refreshing.swap(true, Ordering::AcqRel)
    }

    pub(super) fn end_refresh(&self) {
        self.refreshing.store(false, Ordering::Release);
    }

    pub(super) fn invalidate(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
    }

    /// Changes the kept value in place, keeping its age. Patches wait for each other. A
    /// build in flight may not see the change, so it is retired instead.
    pub(super) fn patch(&self, apply: impl FnOnce(&mut T)) {
        let _patching = lock(&self.patching);
        let Ok(_building) = self.building.try_lock() else {
            self.invalidate();
            return;
        };
        let mut value = lock(&self.value);
        let epoch = self.epoch.load(Ordering::Acquire);
        if let Some(cached) = value.as_mut().filter(|cached| cached.epoch == epoch) {
            apply(Arc::make_mut(&mut cached.value));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::atomic::AtomicU32;

    use super::*;

    async fn build(slot: &Slot<u32>, now: Instant, builds: &AtomicU32) -> u32 {
        let value = slot
            .get::<Infallible, _, _>(now, TTL, || async {
                Ok(builds.fetch_add(1, Ordering::SeqCst) + 1)
            })
            .await;
        *value.unwrap()
    }

    #[tokio::test]
    async fn kept_values_expire_after_the_ttl() {
        let slot = Slot::default();
        let builds = AtomicU32::new(0);
        let start = Instant::now();

        assert_eq!(build(&slot, start, &builds).await, 1);
        assert_eq!(build(&slot, start + TTL / 2, &builds).await, 1);
        assert_eq!(build(&slot, start + TTL, &builds).await, 2);
        // The rebuilt value counts its age from its own build.
        assert_eq!(build(&slot, start + TTL + TTL / 2, &builds).await, 2);
        assert_eq!(builds.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn invalidation_and_patches() {
        let slot = Slot::default();
        let builds = AtomicU32::new(0);
        let start = Instant::now();

        assert_eq!(build(&slot, start, &builds).await, 1);
        slot.patch(|value| *value += 10);
        assert_eq!(build(&slot, start, &builds).await, 11);
        assert_eq!(slot.peek().as_deref(), Some(&11));

        slot.invalidate();
        assert_eq!(slot.peek(), None);
        assert_eq!(build(&slot, start, &builds).await, 2);

        // A patch while a build runs retires that build instead.
        let building = slot.building.try_lock().unwrap();
        slot.patch(|value| *value += 10);
        drop(building);
        assert_eq!(slot.peek(), None);
    }

    #[tokio::test]
    async fn patches_made_at_once_all_apply() {
        for _ in 0..200 {
            let slot = Arc::new(Slot::default());
            let builds = AtomicU32::new(0);
            let start = Instant::now();
            build(&slot, start, &builds).await;
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let threads: Vec<_> = (0..2)
                .map(|_| {
                    let (slot, barrier) = (slot.clone(), barrier.clone());
                    std::thread::spawn(move || {
                        barrier.wait();
                        slot.patch(|value| *value += 10);
                    })
                })
                .collect();
            for thread in threads {
                thread.join().unwrap();
            }
            assert_eq!(slot.peek().as_deref(), Some(&21));
        }
    }
}
