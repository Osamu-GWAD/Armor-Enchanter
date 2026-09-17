use std::io::{self, Write};

use tracing_appender::non_blocking::{NonBlocking, NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::prelude::*;

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
        .with(tracing_subscriber::fmt::layer().with_writer(writer))
        .with(tracing_subscriber::filter::LevelFilter::INFO);
    tracing::subscriber::set_global_default(subscriber).expect("Failed to set tracing subscriber");
    guard
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

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
