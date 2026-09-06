//! [`AstrsFmtLayer`] — renders `tracing::Event`s through `astrs-log`'s
//! deterministic human/JSON formatters instead of `tracing_subscriber`'s
//! own.

use std::io::Write;
use std::sync::{Arc, Mutex};

use tracing::Subscriber;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

use astrs_log::{LogRecord, format};
use astrs_time::HlcClock;

use crate::subscriber::visitor::FieldVisitor;

/// Which of `astrs-log`'s two deterministic renderings
/// [`AstrsFmtLayer`] uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// `astrs_log::format::human`: aligned, human-readable text.
    Human,
    /// `astrs_log::format::json`: one compact JSON object per line.
    Json,
}

/// A `tracing_subscriber::Layer` that renders every event through
/// [`astrs_log::format`] (blueprint §13: "human/JSON formatting (reuse
/// astrs-log formatters where sensible)"), stamping each line with an
/// HLC timestamp from a shared clock and this process's node id, if
/// configured.
///
/// Every event is rendered independently and written under a lock — this
/// is a straightforward, allocation-per-line formatter, not a batching
/// or buffered one; `astrs-log`'s [`astrs_log::RotatingWriter`] is the
/// place batching/rotation belongs for on-disk logs, and this layer's
/// `Write` target can be one if a caller wants that (see
/// [`AstrsFmtLayer::with_writer`]).
pub struct AstrsFmtLayer {
    clock: Arc<HlcClock>,
    format: LogFormat,
    node: Option<String>,
    writer: Mutex<Box<dyn Write + Send>>,
}

impl AstrsFmtLayer {
    /// Builds a layer writing to standard output.
    #[must_use]
    pub fn new(clock: Arc<HlcClock>, format: LogFormat, node: Option<String>) -> Self {
        Self::with_writer(clock, format, node, Box::new(std::io::stdout()))
    }

    /// Builds a layer writing to an arbitrary destination — tests use
    /// this to capture rendered lines into an in-memory buffer instead
    /// of standard output.
    #[must_use]
    pub fn with_writer(
        clock: Arc<HlcClock>,
        format: LogFormat,
        node: Option<String>,
        writer: Box<dyn Write + Send>,
    ) -> Self {
        Self {
            clock,
            format,
            node,
            writer: Mutex::new(writer),
        }
    }
}

impl<S: Subscriber> Layer<S> for AstrsFmtLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let message = visitor.message.take().unwrap_or_default();

        let hlc = self.clock.now();
        let mut record = LogRecord::from_tracing_event(
            hlc,
            *event.metadata().level(),
            event.metadata().target(),
            message,
        )
        .with_fields(visitor.fields);
        if let Some(node) = &self.node {
            record = record.with_node(node.clone());
        }

        let rendered = match self.format {
            LogFormat::Human => format::human(&record),
            LogFormat::Json => format::json(&record).unwrap_or_else(|_| format::human(&record)),
        };

        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = writeln!(writer, "{rendered}");
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::SubscriberExt;

    /// A `Write` sink that appends into a shared, readable `Vec<u8>` --
    /// simpler than piping through a real file for a unit test.
    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn render_one_event(format: LogFormat, node: Option<&str>) -> String {
        let buffer = SharedBuffer::default();
        let clock = Arc::new(HlcClock::system());
        let layer = AstrsFmtLayer::with_writer(
            Arc::clone(&clock),
            format,
            node.map(str::to_owned),
            Box::new(buffer.clone()),
        );
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            tracing::info!(frame_id = 42, "camera frame captured");
        });
        String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn human_format_reads_like_astrs_logs() {
        let line = render_one_event(LogFormat::Human, Some("camera"));
        assert!(line.contains("INFO"));
        assert!(line.contains("camera"));
        assert!(line.contains("camera frame captured"));
        assert!(line.contains("frame_id=42"));
    }

    #[test]
    fn json_format_is_valid_json_with_the_expected_fields() {
        let line = render_one_event(LogFormat::Json, Some("camera"));
        let value: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(value["level"], "info");
        assert_eq!(value["node"], "camera");
        assert_eq!(value["message"], "camera frame captured");
        assert_eq!(value["fields"]["frame_id"], 42);
    }

    #[test]
    fn no_node_configured_means_no_node_in_the_record() {
        let line = render_one_event(LogFormat::Json, None);
        let value: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert!(value.get("node").is_none());
    }

    #[test]
    fn each_event_produces_exactly_one_line() {
        let buffer = SharedBuffer::default();
        let clock = Arc::new(HlcClock::system());
        let layer =
            AstrsFmtLayer::with_writer(clock, LogFormat::Human, None, Box::new(buffer.clone()));
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            tracing::info!("one");
            tracing::warn!("two");
        });
        let text = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        assert_eq!(text.lines().count(), 2);
    }
}
