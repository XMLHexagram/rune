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

/// Parses what a host reported. Bad JSON and types this version does not know are logged and
/// ignored.
pub(crate) fn parse_host_event(payload: &str) -> Option<HostEvent> {
    HostEvent::parse(payload)
}

pub(crate) async fn deliver_host_event(player: &Mutex<dyn Playable>, payload: &str) {
    if let Some(event) = parse_host_event(payload) {
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

    use playback::hosted::HostAction;

    #[test]
    fn batches_reach_the_sink_as_json() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let sink = seen.clone();
        let bridge = JsonHostBridge::new(move |payload| sink.lock().unwrap().push(payload));
        bridge.send(HostCommand {
            host: "appleMusic".into(),
            revision: 2,
            actions: vec![HostAction::Stop, HostAction::Clear],
        });
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            [
                r#"{"host":"appleMusic","revision":2,"actions":[{"action":"stop"},{"action":"clear"}]}"#
            ]
        );
    }

    #[test]
    fn host_events_parse_and_bad_ones_are_ignored() {
        assert_eq!(
            parse_host_event(
                r#"{"type":"hostReady","host":"appleMusic","authorized":false,"canPlay":false}"#
            ),
            Some(HostEvent::HostReady {
                host: "appleMusic".into(),
                authorized: false,
                can_play: false,
            })
        );
        assert_eq!(parse_host_event("not json"), None);
        assert_eq!(parse_host_event(r#"{"type":"somethingNew"}"#), None);
    }
}
