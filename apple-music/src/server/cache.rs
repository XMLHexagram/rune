//! What the server keeps between calls, and how sign-ins and age retire it.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use subsonic::{SubsonicAlbum, SubsonicSong};

use super::map::{Artworks, SongRef};
use super::Outcome;
use crate::models::LibraryPlaylist;

/// How long the song union and the album list are kept, so changes made in the Apple
/// Music app show up.
pub(super) const TTL: Duration = Duration::from_secs(10 * 60);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Default)]
pub(super) struct Caches {
    /// Counts sign-ins with new tokens; reads that started before one are not kept.
    session: AtomicU64,
    pub(super) songs: Slot<SongUnion>,
    pub(super) albums: Slot<Vec<SubsonicAlbum>>,
    /// Apple rating by song id, 0 for a song known to be unrated.
    ratings: Mutex<HashMap<String, i8>>,
    /// Playlists created here that Apple may not list yet.
    created: Mutex<Vec<LibraryPlaylist>>,
    pub(super) artworks: Artworks,
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

    pub(super) fn ratings(&self) -> MutexGuard<'_, HashMap<String, i8>> {
        lock(&self.ratings)
    }

    /// Changes the ratings, unless a sign-in happened since `session`.
    pub(super) fn update_ratings(
        &self,
        session: u64,
        update: impl FnOnce(&mut HashMap<String, i8>),
    ) {
        let mut ratings = lock(&self.ratings);
        if self.session() == session {
            update(&mut ratings);
        }
    }

    pub(super) fn created(&self) -> MutexGuard<'_, Vec<LibraryPlaylist>> {
        lock(&self.created)
    }

    pub(super) fn remember_created(&self, session: u64, playlist: LibraryPlaylist) {
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
        }
    }
}

impl<T: Clone> Slot<T> {
    /// The kept value when it is current and younger than `ttl` at `now`, else one
    /// built by `build`. A failed build keeps nothing.
    pub(super) async fn get<F, Fut>(&self, now: Instant, ttl: Duration, build: F) -> Outcome<Arc<T>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Outcome<T>>,
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

    fn fresh(&self, now: Instant, ttl: Duration) -> Option<Arc<T>> {
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

/// The user's songs in order, deduplicated by id and by catalog id.
#[derive(Clone, Default)]
pub(super) struct SongUnion {
    pub(super) songs: Vec<SubsonicSong>,
    index: HashMap<String, usize>,
    /// Catalog ids of the library songs, so a catalog copy of one is not listed twice.
    library_catalog_ids: HashSet<String>,
}

impl SongUnion {
    /// Whether `id`, or the library song a catalog id stands for, is listed.
    pub(super) fn lists(&self, id: &str) -> bool {
        self.index.contains_key(id)
            || matches!(SongRef::parse(id), Ok(SongRef::Catalog(catalog)) if self.library_catalog_ids.contains(catalog))
    }

    pub(super) fn get(&self, id: &str) -> Option<&SubsonicSong> {
        self.index.get(id).map(|&at| &self.songs[at])
    }

    pub(super) fn push(&mut self, song: SubsonicSong) {
        if !self.lists(&song.id) {
            self.index.insert(song.id.clone(), self.songs.len());
            self.songs.push(song);
        }
    }

    /// Records the catalog id of a library song.
    pub(super) fn add_library_catalog_id(&mut self, catalog_id: &str) {
        self.library_catalog_ids.insert(catalog_id.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;

    use super::*;

    async fn build(slot: &Slot<u32>, now: Instant, builds: &AtomicU32) -> u32 {
        let value = slot
            .get(now, TTL, || async {
                Ok(builds.fetch_add(1, Ordering::SeqCst) + 1)
            })
            .await;
        *value.ok().unwrap()
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

    #[test]
    fn the_union_skips_catalog_copies_of_library_songs() {
        let song = |id: &str| SubsonicSong {
            id: id.into(),
            ..SubsonicSong::default()
        };
        let mut union = SongUnion::default();
        union.push(song("i.a"));
        union.add_library_catalog_id("100");
        union.push(song("i.a"));
        union.push(song("catalog:song:100"));
        union.push(song("catalog:song:200"));
        let ids: Vec<&str> = union.songs.iter().map(|song| song.id.as_str()).collect();
        assert_eq!(ids, ["i.a", "catalog:song:200"]);
        assert!(union.lists("catalog:song:100"));
        assert!(union.get("catalog:song:100").is_none());
    }
}
