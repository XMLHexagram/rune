use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use apple_music::AppleMusicServer;
use database::library_registry::LibraryRecordId;
use subsonic::SubsonicApi;

/// Records whose endpoint uses this scheme are served by a server inside this process.
const IN_PROCESS_SCHEME: &str = "rune-service://";

#[derive(Debug, thiserror::Error)]
pub(crate) enum InProcessError {
    #[error("Unknown in-process service: {0}")]
    UnknownService(String),
    #[error("The library record was removed")]
    Removed,
    #[error("Could not create the in-process service: {0}")]
    Build(String),
}

pub(crate) fn is_in_process_endpoint(endpoint: &str) -> bool {
    endpoint.starts_with(IN_PROCESS_SCHEME)
}

#[derive(Default)]
struct State {
    servers: HashMap<LibraryRecordId, Arc<dyn SubsonicApi>>,
    /// Records removed during this run, so a request that read the registry earlier cannot
    /// create a server under a dead id.
    removed: HashSet<LibraryRecordId>,
}

/// The in-process Subsonic servers of one node, by library record.
///
/// A server keeps its sign-in tokens in memory, so one instance serves the open binding,
/// playback, login and registration for the life of the app run.
#[derive(Default)]
pub(crate) struct InProcessServers {
    state: Mutex<State>,
    #[cfg(test)]
    test_base_url: Mutex<Option<String>>,
}

impl InProcessServers {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A server for `endpoint` that is not kept, for probing before a record exists.
    pub(crate) fn create(&self, endpoint: &str) -> Result<Arc<dyn SubsonicApi>, InProcessError> {
        let name = endpoint.strip_prefix(IN_PROCESS_SCHEME).unwrap_or_default();
        match name {
            "apple-music" => self.apple_music().map(|server| Arc::new(server) as _),
            _ => {
                log::warn!("Unknown in-process service {name:?} in endpoint {endpoint:?}");
                Err(InProcessError::UnknownService(name.to_owned()))
            }
        }
    }

    /// The record's server, created on first use.
    pub(crate) fn get_or_create(
        &self,
        record_id: LibraryRecordId,
        endpoint: &str,
    ) -> Result<Arc<dyn SubsonicApi>, InProcessError> {
        {
            let state = self.state();
            if state.removed.contains(&record_id) {
                return Err(InProcessError::Removed);
            }
            if let Some(server) = state.servers.get(&record_id) {
                return Ok(Arc::clone(server));
            }
        }
        // Built outside the lock; another path may win the slot meanwhile.
        let created = self.create(endpoint)?;
        let mut state = self.state();
        if state.removed.contains(&record_id) {
            return Err(InProcessError::Removed);
        }
        Ok(Arc::clone(
            state.servers.entry(record_id).or_insert(created),
        ))
    }

    /// Keeps `server`, created for a new record, as that record's instance.
    pub(crate) fn insert(&self, record_id: LibraryRecordId, server: Arc<dyn SubsonicApi>) {
        self.state().servers.entry(record_id).or_insert(server);
    }

    /// Drops the instance, so its tokens do not outlive the record.
    pub(crate) fn drop_instance(&self, record_id: LibraryRecordId) {
        self.state().servers.remove(&record_id);
    }

    /// Drops the instance of a record that no longer exists and refuses to create another.
    pub(crate) fn retire(&self, record_id: LibraryRecordId) {
        let mut state = self.state();
        state.servers.remove(&record_id);
        state.removed.insert(record_id);
    }

    #[cfg(test)]
    pub(crate) fn get(&self, record_id: LibraryRecordId) -> Option<Arc<dyn SubsonicApi>> {
        self.state().servers.get(&record_id).cloned()
    }

    #[cfg(test)]
    pub(crate) fn set_test_base_url(&self, url: &str) {
        *self
            .test_base_url
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(url.to_owned());
    }

    #[cfg(not(test))]
    fn apple_music(&self) -> Result<AppleMusicServer, InProcessError> {
        AppleMusicServer::try_new().map_err(|error| InProcessError::Build(error.to_string()))
    }

    #[cfg(test)]
    fn apple_music(&self) -> Result<AppleMusicServer, InProcessError> {
        let base_url = self
            .test_base_url
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        AppleMusicServer::try_with_client(|builder| match base_url {
            Some(url) => builder.base_url(url).storefront("it"),
            None => builder,
        })
        .map_err(|error| InProcessError::Build(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use uuid::Uuid;

    use super::*;

    const APPLE_MUSIC: &str = "rune-service://apple-music";

    fn record() -> LibraryRecordId {
        LibraryRecordId(Uuid::new_v4())
    }

    #[test]
    fn only_the_exact_scheme_and_a_known_name_create_a_server() {
        let servers = InProcessServers::default();
        for endpoint in [
            "apple-music",
            "https://apple-music",
            "rune-service://",
            "rune-service://nothing",
        ] {
            assert!(
                matches!(
                    servers.create(endpoint),
                    Err(InProcessError::UnknownService(_))
                ),
                "{endpoint}"
            );
        }
        assert!(servers.create(APPLE_MUSIC).is_ok());
    }

    #[test]
    fn one_instance_per_record_until_it_is_retired() {
        let servers = InProcessServers::default();
        let id = record();
        let first = servers.get_or_create(id, APPLE_MUSIC).unwrap();
        assert!(Arc::ptr_eq(
            &first,
            &servers.get_or_create(id, APPLE_MUSIC).unwrap()
        ));
        // Dropping the instance, as after a failed removal, lets a later call start afresh.
        servers.drop_instance(id);
        let second = servers.get_or_create(id, APPLE_MUSIC).unwrap();
        assert!(!Arc::ptr_eq(&first, &second));
        servers.retire(id);
        assert!(servers.get(id).is_none());
        assert!(matches!(
            servers.get_or_create(id, APPLE_MUSIC),
            Err(InProcessError::Removed)
        ));
        // Other records are not affected.
        assert!(servers.get_or_create(record(), APPLE_MUSIC).is_ok());
    }
}
