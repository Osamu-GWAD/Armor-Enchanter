use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};

use tracing_appender::non_blocking::{NonBlocking, NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::prelude::*;

static SUPPRESSED_CHUNK_WARNINGS: AtomicU64 = AtomicU64::new(0);

pub fn take_suppressed_chunk_warnings() -> u64 {
    SUPPRESSED_CHUNK_WARNINGS.swap(0, Ordering::Relaxed)
}

#[derive(Default)]
struct ChunkWarningSamples(AtomicU64);

impl<S: tracing::Subscriber> tracing_subscriber::layer::Filter<S> for ChunkWarningSamples {
    fn enabled(&self, _: &tracing::Metadata<'_>, _: &tracing_subscriber::layer::Context<'_, S>) -> bool {
        true
    }

    fn event_enabled(&self, event: &tracing::Event<'_>, _: &tracing_subscriber::layer::Context<'_, S>) -> bool {
        if event.metadata().target() != "azalea_world::chunk::partial"
            || *event.metadata().level() != tracing::Level::WARN { return true; }
        #[derive(Default)]
        struct Message { out_of_range: bool }
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    let text = format!("{value:?}");
                    self.out_of_range = text.contains("is not in the render distance")
                        || text.contains("Ignoring chunk since it's not in the view range");
                }
            }
        }
        let mut message = Message::default();
        event.record(&mut message);
        if !message.out_of_range { return true; }
        let count = self.0.fetch_add(1, Ordering::Relaxed);
        let show = count < 4 || count.is_multiple_of(1000);
        if !show { SUPPRESSED_CHUNK_WARNINGS.fetch_add(1, Ordering::Relaxed); }
        show
    }
}

fn background_writer(writer: impl Write + Send + 'static) -> (NonBlocking, WorkerGuard) {
    NonBlockingBuilder::default()
        .buffered_lines_limit(4096)
        // A stalled terminal must never backpressure the game loop. If the
        // bounded queue fills, discard new log lines instead of blocking.
        .lossy(true)
        .thread_name("enchanter-logs")
        .finish(writer)
}

pub fn init() -> WorkerGuard {
    let (writer, guard) = background_writer(io::stdout());
    let subscriber = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_writer(writer).with_filter(ChunkWarningSamples::default()))
        .with(tracing_subscriber::filter::LevelFilter::INFO);
    tracing::subscriber::set_global_default(subscriber).expect("Failed to set tracing subscriber");
    guard
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn chunk_sampling_preserves_unrelated_warnings_and_errors() {
        #[derive(Clone, Default)]
        struct Buffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for Buffer {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> { Ok(()) }
        }
        let output = Buffer::default();
        let sink = output.clone();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer().with_ansi(false).without_time()
                .with_writer(move || sink.clone()).with_filter(ChunkWarningSamples::default()),
        );
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..20 {
                tracing::warn!(target: "azalea_world::chunk::partial", "Ignoring chunk since it's not in the view range");
            }
            tracing::warn!(target: "azalea_world::chunk::partial", "unrelated chunk warning");
            tracing::error!(target: "azalea_world::chunk::partial", "chunk error");
            tracing::warn!("drop warning");
        });
        let text = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
        assert_eq!(text.matches("Ignoring chunk").count(), 4);
        assert!(text.contains("unrelated chunk warning"));
        assert!(text.contains("chunk error"));
        assert!(text.contains("drop warning"));
    }

    struct StalledConsole {
        entered: Option<mpsc::Sender<()>>,
        release: mpsc::Receiver<()>,
    }

    impl Write for StalledConsole {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if let Some(entered) = self.entered.take() {
                entered.send(()).unwrap();
                self.release.recv().unwrap();
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn stalled_console_does_not_block_producer_when_queue_fills() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (mut writer, guard) = background_writer(StalledConsole {
            entered: Some(entered_tx),
            release: release_rx,
        });
        writer.write_all(b"first line\n").unwrap();
        let entered = entered_rx.recv_timeout(Duration::from_secs(5));
        if entered.is_err() {
            let _ = release_tx.send(());
            panic!("logging worker did not start: {entered:?}");
        }
        let dropped = writer.error_counter();
        let (done_tx, done_rx) = mpsc::channel();
        let producer = std::thread::spawn(move || {
            for _ in 0..8192 {
                writer.write_all(b"queued line\n").unwrap();
            }
            done_tx.send(()).unwrap();
        });
        let completed = done_rx.recv_timeout(Duration::from_secs(5));
        let dropped_lines = dropped.dropped_lines();
        // Always unblock the sink before asserting, even if this regresses.
        release_tx.send(()).unwrap();
        producer.join().unwrap();
        drop(guard);
        assert!(completed.is_ok(), "producer waited for the stalled console");
        assert!(dropped_lines > 0, "test must exercise a full log queue");
    }
}
