//! Test-only capture of the `tracing` events a closure emits on the current
//! thread.
//!
//! A per-test scoped dispatcher (`tracing::subscriber::with_default`) is not
//! reliable under the parallel test harness: call-site interest is cached
//! process-wide, so a call site first registered by another thread while no
//! capturing dispatcher existed can stay cached as "never" and silently drop
//! the events a test is waiting for. One global subscriber, installed once for
//! the whole test binary, keeps every WARN and ERROR call site enabled; it
//! records events only while a capture is active on the emitting thread. Lower
//! levels stay disabled so the rest of the suite does not pay for them.

use std::cell::RefCell;
use std::sync::Once;

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};

/// One captured event: its level and its fields as `(name, rendered value)`.
#[derive(Clone, Debug)]
pub struct CapturedEvent {
    pub level: Level,
    pub fields: Vec<(String, String)>,
}

impl CapturedEvent {
    /// The rendered `message` field, if the event has one.
    #[cfg_attr(not(unix), allow(dead_code))] // only the Unix-only tag-writer tests read it
    pub fn message(&self) -> Option<&str> {
        self.fields
            .iter()
            .find(|(name, _)| name == "message")
            .map(|(_, value)| value.as_str())
    }
}

thread_local! {
    static ACTIVE: RefCell<Option<Vec<CapturedEvent>>> = const { RefCell::new(None) };
}

struct ThreadCapture;

impl<S: Subscriber> Layer<S> for ThreadCapture {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        ACTIVE.with(|active| {
            if let Some(events) = active.borrow_mut().as_mut() {
                let mut visitor = FieldVisitor(Vec::new());
                event.record(&mut visitor);
                events.push(CapturedEvent {
                    level: *event.metadata().level(),
                    fields: visitor.0,
                });
            }
        });
    }
}

struct FieldVisitor(Vec<(String, String)>);

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.push((field.name().to_owned(), format!("{value:?}")));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.push((field.name().to_owned(), value.to_owned()));
    }
}

/// Clears this thread's capture even if `body` panics.
struct ActiveCapture;

impl Drop for ActiveCapture {
    fn drop(&mut self) {
        ACTIVE.with(|active| active.borrow_mut().take());
    }
}

/// Run `body` and return every WARN or ERROR `tracing` event it emitted on
/// this thread.
pub fn capture_events(body: impl FnOnce()) -> Vec<CapturedEvent> {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        tracing::subscriber::set_global_default(
            tracing_subscriber::registry().with(ThreadCapture.with_filter(LevelFilter::WARN)),
        )
        .expect("no other global tracing subscriber is installed in tests");
    });

    ACTIVE.with(|active| *active.borrow_mut() = Some(Vec::new()));
    let active = ActiveCapture;
    body();
    let events = ACTIVE.with(|active| active.borrow_mut().take().unwrap_or_default());
    drop(active);
    events
}
