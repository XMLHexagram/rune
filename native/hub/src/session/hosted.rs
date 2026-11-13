//! Carries messages between the player and the platform host on this device, which lives in the
//! app (Dart). Nothing here leaves the device.

use log::{debug, warn};
use playback::{
    hosted::{HostBridge, HostCommand, HostEvent},
    player::Playable,
};
use rinf::{DartSignal, RustSignal};
use tokio::sync::Mutex;

use crate::messages::{HostedPlayerCommand, HostedPlayerEvent};
use crate::node::NodeSlot;

/// Serialises batches of commands and hands them to `sink`, which must not block.
pub(crate) struct JsonHostBridge {
    sink: Box<dyn Fn(String) + Send + Sync>,
}

impl JsonHostBridge {
    pub(crate) fn new(sink: impl Fn(String) + Send + Sync + 'static) -> Self {
        Self {
            sink: Box::new(sink),
        }
    }

    /// Sends commands to the app on this device.
    pub(crate) fn to_app() -> Self {
        Self::new(|payload| HostedPlayerCommand { payload }.send_signal_to_dart())
    }
}

impl HostBridge for JsonHostBridge {
    fn send(&self, command: HostCommand) {
        match serde_json::to_string(&command) {
            Ok(payload) => (self.sink)(payload),
            Err(error) => warn!("Could not encode a host command: {error}"),
        }
    }
}

/// Hands what a host reported to the player. Bad JSON and types this version does not know are
/// logged and dropped.
async fn deliver_host_event(player: &Mutex<dyn Playable>, payload: &str) {
    if let Some(event) = HostEvent::parse(payload) {
        player.lock().await.deliver_host_event(event);
    }
}

/// Passes the app's host events to the local session's player.
pub async fn receive_hosted_player_events(node: &'static NodeSlot) {
    let receiver = HostedPlayerEvent::get_dart_signal_receiver();
    while let Some(signal) = receiver.recv().await {
        let session = match node.current().await.and_then(|node| node.session()) {
            Ok(session) => session,
            Err(error) => {
                debug!("Dropping a host event before the node is ready: {error:#}");
                continue;
            }
        };
        deliver_host_event(&session.player, &signal.message.payload).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex as StdMutex};

    use playback::hosted::{HostAction, HostFailure};

    fn parsed(payloads: &[&str]) -> Vec<HostEvent> {
        payloads
            .iter()
            .filter_map(|payload| HostEvent::parse(payload))
            .collect()
    }

    #[test]
    fn batches_reach_the_sink_as_json() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let sink = seen.clone();
        let bridge = JsonHostBridge::new(move |payload| sink.lock().unwrap().push(payload));
        bridge.send(HostCommand {
            host: "appleMusic".into(),
            revision: 1,
            actions: vec![
                HostAction::Set {
                    ids: vec!["i.abc123".into(), "catalog:1000".into()],
                },
                HostAction::Skip {
                    index: 0,
                    offset_ms: 1500,
                },
                HostAction::SetGain { gain: 0.5 },
                HostAction::Start,
            ],
        });
        bridge.send(HostCommand {
            host: "appleMusic".into(),
            revision: 2,
            actions: vec![
                HostAction::Remove { index: 1 },
                HostAction::Add {
                    ids: vec!["i.def456".into()],
                },
            ],
        });
        bridge.send(HostCommand {
            host: "appleMusic".into(),
            revision: 3,
            actions: vec![HostAction::Stop, HostAction::Clear],
        });
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            [
                r#"{"host":"appleMusic","revision":1,"actions":[{"action":"set","ids":["i.abc123","catalog:1000"]},{"action":"skip","index":0,"offsetMs":1500},{"action":"setGain","gain":0.5},{"action":"start"}]}"#,
                r#"{"host":"appleMusic","revision":2,"actions":[{"action":"remove","index":1},{"action":"add","ids":["i.def456"]}]}"#,
                r#"{"host":"appleMusic","revision":3,"actions":[{"action":"stop"},{"action":"clear"}]}"#,
            ]
        );
    }

    #[test]
    fn host_events_parse() {
        assert_eq!(
            parsed(&[
                r#"{"type":"hostReady","host":"appleMusic","authorized":false,"canPlay":false}"#,
                r#"{"type":"jukeboxStatus","host":"appleMusic","revision":4,"currentIndex":1,"currentId":"catalog:1000","playing":true,"positionMs":2500.4}"#,
                r#"{"type":"jukeboxItemFailed","host":"appleMusic","revision":4,"index":1,"reason":"network"}"#,
                r#"{"type":"jukeboxEnded","host":"appleMusic","revision":5}"#,
            ]),
            [
                HostEvent::HostReady {
                    host: "appleMusic".into(),
                    authorized: false,
                    can_play: false,
                },
                HostEvent::JukeboxStatus {
                    host: "appleMusic".into(),
                    revision: 4,
                    current_index: Some(1),
                    current_id: Some("catalog:1000".into()),
                    playing: true,
                    position_ms: 2500,
                },
                HostEvent::JukeboxItemFailed {
                    host: "appleMusic".into(),
                    revision: 4,
                    index: 1,
                    reason: HostFailure::Network,
                },
                HostEvent::JukeboxEnded {
                    host: "appleMusic".into(),
                    revision: 5,
                },
            ]
        );
    }

    #[test]
    fn bad_and_unknown_events_are_dropped() {
        assert_eq!(
            parsed(&[
                "not json",
                "[1]",
                r#"{"type":"somethingNew","host":"appleMusic"}"#,
                r#"{"host":"appleMusic","revision":1}"#,
                r#"{"type":"jukeboxEnded","host":"appleMusic"}"#,
                r#"{"type":"jukeboxStatus","host":"appleMusic","revision":1,"currentIndex":0,"playing":"yes","positionMs":0}"#,
            ]),
            []
        );
    }
}
