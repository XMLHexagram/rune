//! The visualizer's log messages on the app's Log page, while the user turns that on.
//!
//! A layer of the process's tracing subscriber picks the events of the visualizer's code (info and
//! more serious, which never come every frame) and hands them to a task that writes them to the
//! main database, where the Log page reads them.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use ::database::actions::logging::{LogLevel, insert_log};
use ::database::connection::MainDbConnection;
use log::warn;
use tokio::sync::mpsc;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_log::NormalizeEvent;
use tracing_subscriber::layer::{Context, Layer};

/// The code whose messages count as the visualizer's: the crates and the hub's modules that drive
/// it.
const TARGETS: [&str; 6] = [
    "visualizer",
    "chart_analyzer",
    "flutter_texture",
    "hub::visualizer",
    "hub::handlers::visualizer",
    "hub::flutter_bridge",
];

/// How many messages wait for the database at most; more are dropped.
const QUEUE: usize = 1024;

static ENABLED: AtomicBool = AtomicBool::new(false);

struct Entry {
    level: LogLevel,
    domain: String,
    detail: String,
}

struct Queue {
    sender: mpsc::Sender<Entry>,
    /// Taken by the task that writes, once.
    receiver: Mutex<Option<mpsc::Receiver<Entry>>>,
}

static PENDING: LazyLock<Queue> = LazyLock::new(|| {
    let (sender, receiver) = mpsc::channel(QUEUE);
    Queue {
        sender,
        receiver: Mutex::new(Some(receiver)),
    }
});

/// Write the visualizer's messages to the Log page from now on (`enabled`), or stop. The first
/// time it is turned on, a task starts writing them to `main_db`; call it within the runtime.
pub fn set_enabled(enabled: bool, main_db: Arc<MainDbConnection>) {
    ENABLED.store(enabled, Ordering::Relaxed);
    if !enabled {
        return;
    }
    let receiver = PENDING
        .receiver
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    let Some(mut receiver) = receiver else {
        return;
    };
    tokio::spawn(async move {
        while let Some(entry) = receiver.recv().await {
            if let Err(e) = insert_log(&*main_db, entry.level, entry.domain, entry.detail).await {
                warn!("Failed to write a visualizer message to the log: {e:#}");
            }
        }
    });
}

/// The layer that picks the visualizer's messages, for the process's subscriber.
pub fn layer<S: Subscriber>() -> impl Layer<S> {
    VisualizerLog
}

struct VisualizerLog;

impl<S: Subscriber> Layer<S> for VisualizerLog {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if !ENABLED.load(Ordering::Relaxed) {
            return;
        }
        // Messages of the `log` crate arrive as events of the target "log"; this is their own.
        let normalized = event.normalized_metadata();
        let metadata = normalized.as_ref().unwrap_or_else(|| event.metadata());
        let level = match *metadata.level() {
            Level::ERROR => LogLevel::Error,
            Level::WARN => LogLevel::Warning,
            Level::INFO => LogLevel::Info,
            _ => return,
        };
        let target = metadata.target();
        let ours = TARGETS.iter().any(|prefix| {
            target
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
        });
        if !ours {
            return;
        }
        let mut detail = Detail::default();
        event.record(&mut detail);
        // A full queue (the database is slow, or not open yet) drops the message.
        let _ = PENDING.sender.try_send(Entry {
            level,
            domain: target.to_string(),
            detail: detail.0,
        });
    }
}

/// The message of an event, then its other fields as `name=value`; the fields the `log` crate's
/// bridge adds (`log.target` and the like) are left out.
#[derive(Default)]
struct Detail(String);

impl Visit for Detail {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let name = field.name();
        if name.starts_with("log.") {
            return;
        }
        if name == "message" {
            let fields = std::mem::take(&mut self.0);
            let _ = write!(self.0, "{value:?}");
            if !fields.is_empty() {
                self.0.push_str(&fields);
            }
        } else {
            let _ = write!(self.0, " {name}={value:?}");
        }
    }
}
