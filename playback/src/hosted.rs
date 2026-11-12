//! Tracks that only a platform's own SDK may play. They stay in the player's queue, and while one
//! is current a host on the platform plays it and reports back. The host only ever holds the
//! current track, so the queue, its modes and its edits stay with the player.
//!
//! Hosts may be written in Swift, Kotlin or JavaScript, so the messages are JSON in camelCase.
//! Commands use the action vocabulary of Subsonic's `jukeboxControl`.

use std::fmt;
use std::path::PathBuf;

use log::{debug, warn};
use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize};

use crate::player::UnavailableReason;

/// A track that a platform host plays: the host family, and the id that host understands.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HostedTrack {
    pub host: String,
    pub key: String,
}

impl HostedTrack {
    /// The path the player reports for the track. It names no file.
    pub fn path(&self) -> PathBuf {
        PathBuf::from(format!("hosted://{}/{}", self.host, self.key))
    }
}

/// A batch of actions for a host, to be applied in order. Every batch has a revision of its own,
/// one higher than the batch before, and the host's events carry back the revision of the last
/// batch it applied, so that events from before a newer batch can be told apart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostCommand {
    pub host: String,
    pub revision: u64,
    pub actions: Vec<HostAction>,
}

/// The player hands a track over in one batch: `Set` with its id, `Skip` to index 0 at the
/// position to play from, `SetGain` with the player's volume, and then `Start`, or `Stop` when
/// the host is to hold it paused. When the track stops being current, the host gets `Stop` and
/// `Clear`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum HostAction {
    Set { ids: Vec<String> },
    Add { ids: Vec<String> },
    Remove { index: u32 },
    Clear,
    Skip { index: u32, offset_ms: u64 },
    Start,
    Stop,
    Status,
    SetGain { gain: f32 },
}

/// What a host reports to the player.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum HostEvent {
    HostReady {
        host: String,
        authorized: bool,
        can_play: bool,
    },
    /// `current_index` is none when the host holds no track, as after it restarted.
    JukeboxStatus {
        host: String,
        revision: u64,
        current_index: Option<u32>,
        playing: bool,
        #[serde(deserialize_with = "position_ms")]
        position_ms: u64,
    },
    JukeboxItemFailed {
        host: String,
        revision: u64,
        index: u32,
        reason: HostFailure,
    },
    JukeboxEnded {
        host: String,
        revision: u64,
    },
    /// An event of a type this version does not know.
    #[serde(other)]
    Unknown,
}

impl HostEvent {
    /// Reads an event from a host's JSON. Input that does not parse, and events of types this
    /// version does not know, are logged and dropped.
    pub fn parse(json: &str) -> Option<Self> {
        match serde_json::from_str(json) {
            Ok(Self::Unknown) => {
                debug!("Ignoring a host event of an unknown type: {json}");
                None
            }
            Ok(event) => Some(event),
            Err(error) => {
                warn!("Ignoring a host event that does not parse: {error}");
                None
            }
        }
    }
}

// Hosts written in JavaScript may report a fractional position.
fn position_ms<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    struct Millis;

    impl Visitor<'_> for Millis {
        type Value = u64;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a position in milliseconds")
        }

        fn visit_u64<E: de::Error>(self, value: u64) -> Result<u64, E> {
            Ok(value)
        }

        fn visit_i64<E: de::Error>(self, value: i64) -> Result<u64, E> {
            Ok(u64::try_from(value).unwrap_or(0))
        }

        fn visit_f64<E: de::Error>(self, value: f64) -> Result<u64, E> {
            // A cast saturates, and takes NaN to zero.
            Ok(value.max(0.0).round() as u64)
        }
    }

    deserializer.deserialize_any(Millis)
}

/// Why a host could not play a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HostFailure {
    Unplayable,
    NotAuthorized,
    Network,
    /// A reason this version does not know.
    #[serde(other)]
    Unknown,
}

impl From<HostFailure> for UnavailableReason {
    fn from(failure: HostFailure) -> Self {
        match failure {
            HostFailure::Unplayable => Self::TrackGone,
            HostFailure::NotAuthorized => Self::AccessRevoked,
            HostFailure::Network | HostFailure::Unknown => Self::SourceUnreachable,
        }
    }
}

/// Carries commands to the platform hosts. Sending must not block: the player calls it from its
/// own loop, and the outcome comes back as [`HostEvent`]s.
pub trait HostBridge: Send + Sync {
    fn send(&self, command: HostCommand);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::de::DeserializeOwned;
    use std::fmt::Debug;

    fn assert_json<T>(value: T, json: &str)
    where
        T: Serialize + DeserializeOwned + PartialEq + Debug,
    {
        assert_eq!(serde_json::to_string(&value).unwrap(), json);
        assert_eq!(serde_json::from_str::<T>(json).unwrap(), value);
    }

    #[test]
    fn a_batch_carries_jukebox_actions_in_camel_case() {
        assert_json(
            HostCommand {
                host: "appleMusic".to_owned(),
                revision: 12,
                actions: vec![
                    HostAction::Set {
                        ids: vec!["i.x".to_owned()],
                    },
                    HostAction::Skip {
                        index: 0,
                        offset_ms: 0,
                    },
                    HostAction::SetGain { gain: 0.6 },
                    HostAction::Start,
                ],
            },
            r#"{"host":"appleMusic","revision":12,"actions":[{"action":"set","ids":["i.x"]},{"action":"skip","index":0,"offsetMs":0},{"action":"setGain","gain":0.6},{"action":"start"}]}"#,
        );
        let cases = [
            (
                HostAction::Add {
                    ids: vec!["1440".to_owned(), "1441".to_owned()],
                },
                r#"{"action":"add","ids":["1440","1441"]}"#,
            ),
            (
                HostAction::Remove { index: 2 },
                r#"{"action":"remove","index":2}"#,
            ),
            (HostAction::Clear, r#"{"action":"clear"}"#),
            (
                HostAction::Skip {
                    index: 0,
                    offset_ms: 1500,
                },
                r#"{"action":"skip","index":0,"offsetMs":1500}"#,
            ),
            (HostAction::Stop, r#"{"action":"stop"}"#),
            (HostAction::Status, r#"{"action":"status"}"#),
        ];
        for (action, json) in cases {
            assert_json(action, json);
        }
    }

    #[test]
    fn events_are_tagged_by_type_in_camel_case() {
        assert_json(
            HostEvent::HostReady {
                host: "appleMusic".to_owned(),
                authorized: true,
                can_play: false,
            },
            r#"{"type":"hostReady","host":"appleMusic","authorized":true,"canPlay":false}"#,
        );
        assert_json(
            HostEvent::JukeboxStatus {
                host: "appleMusic".to_owned(),
                revision: 3,
                current_index: Some(0),
                playing: true,
                position_ms: 1500,
            },
            r#"{"type":"jukeboxStatus","host":"appleMusic","revision":3,"currentIndex":0,"playing":true,"positionMs":1500}"#,
        );
        assert_json(
            HostEvent::JukeboxStatus {
                host: "appleMusic".to_owned(),
                revision: 3,
                current_index: None,
                playing: false,
                position_ms: 0,
            },
            r#"{"type":"jukeboxStatus","host":"appleMusic","revision":3,"currentIndex":null,"playing":false,"positionMs":0}"#,
        );
        for (reason, json) in [
            (HostFailure::Unplayable, "unplayable"),
            (HostFailure::NotAuthorized, "notAuthorized"),
            (HostFailure::Network, "network"),
        ] {
            assert_json(
                HostEvent::JukeboxItemFailed {
                    host: "appleMusic".to_owned(),
                    revision: 3,
                    index: 0,
                    reason,
                },
                &format!(
                    r#"{{"type":"jukeboxItemFailed","host":"appleMusic","revision":3,"index":0,"reason":"{json}"}}"#
                ),
            );
        }
        assert_json(
            HostEvent::JukeboxEnded {
                host: "appleMusic".to_owned(),
                revision: 3,
            },
            r#"{"type":"jukeboxEnded","host":"appleMusic","revision":3}"#,
        );
    }

    #[test]
    fn a_status_without_a_current_index_has_none() {
        let event: HostEvent = serde_json::from_str(
            r#"{"type":"jukeboxStatus","host":"appleMusic","revision":1,"playing":false,"positionMs":0}"#,
        )
        .unwrap();
        assert!(matches!(
            event,
            HostEvent::JukeboxStatus {
                current_index: None,
                ..
            }
        ));
    }

    #[test]
    fn a_fractional_position_is_rounded_to_milliseconds() {
        assert_eq!(
            HostEvent::parse(
                r#"{"type":"jukeboxStatus","host":"appleMusic","revision":1,"currentIndex":0,"playing":true,"positionMs":81250.5}"#
            ),
            Some(HostEvent::JukeboxStatus {
                host: "appleMusic".to_owned(),
                revision: 1,
                current_index: Some(0),
                playing: true,
                position_ms: 81251,
            })
        );
        assert!(matches!(
            HostEvent::parse(
                r#"{"type":"jukeboxStatus","host":"appleMusic","revision":1,"currentIndex":0,"playing":true,"positionMs":-3}"#
            ),
            Some(HostEvent::JukeboxStatus { position_ms: 0, .. })
        ));
    }

    #[test]
    fn unknown_types_and_reasons_do_not_break_parsing() {
        assert_eq!(
            serde_json::from_str::<HostEvent>(r#"{"type":"somethingNew","extra":1}"#).unwrap(),
            HostEvent::Unknown
        );
        assert_eq!(HostEvent::parse(r#"{"type":"somethingNew"}"#), None);
        assert_eq!(HostEvent::parse("not json"), None);
        assert_eq!(
            HostEvent::parse(
                r#"{"type":"jukeboxItemFailed","host":"appleMusic","revision":3,"index":0,"reason":"regionLocked"}"#
            ),
            Some(HostEvent::JukeboxItemFailed {
                host: "appleMusic".to_owned(),
                revision: 3,
                index: 0,
                reason: HostFailure::Unknown,
            })
        );
    }

    #[test]
    fn host_failures_map_to_queue_reasons() {
        assert_eq!(
            UnavailableReason::from(HostFailure::Unplayable),
            UnavailableReason::TrackGone
        );
        assert_eq!(
            UnavailableReason::from(HostFailure::NotAuthorized),
            UnavailableReason::AccessRevoked
        );
        assert_eq!(
            UnavailableReason::from(HostFailure::Network),
            UnavailableReason::SourceUnreachable
        );
        assert_eq!(
            UnavailableReason::from(HostFailure::Unknown),
            UnavailableReason::SourceUnreachable
        );
    }
}
