use rinf::{DartSignal, RustSignal};
use serde::{Deserialize, Serialize};

/// A command for the platform host on this device, as the JSON of a `HostCommand`. Never sent to
/// other devices.
#[derive(Clone, Deserialize, Serialize, RustSignal)]
pub struct HostedPlayerCommand {
    pub payload: String,
}

/// What the platform host on this device reports, as the JSON of a `HostEvent`.
#[derive(Serialize, Deserialize, DartSignal)]
pub struct HostedPlayerEvent {
    pub payload: String,
}
