//! What the library keeps between calls, and how sign-ins and age retire it.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use super::types::{Album, Playlist, SongList, TrackKey};

/// How long the song list, the album list and each rating are kept, so changes made in
/// the Apple Music app or on another device show up.
pub(super) const TTL: Duration = Duration::from_secs(10 * 60);

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
        ratings.clear();
        created.clear();
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

    /// Changes the kept value in place, keeping its age. A build in flight may not see
    /// the change, so it is retired instead.
    pub(super) fn patch(&self, apply: impl FnOnce(&mut T)) {
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
}
