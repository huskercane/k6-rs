//! Bounded console delivery. The CLI shares one writer across all VUs and
//! lifecycle contexts; producers never wait for terminal/file I/O.

use std::fs::File;
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub const CONSOLE_CAPACITY: usize = 1024;
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024;
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy)]
pub enum ConsoleLevel {
    Log,
    Warn,
    Error,
}

impl ConsoleLevel {
    fn prefix(self) -> &'static str {
        match self {
            Self::Log => "",
            Self::Warn => "WARN: ",
            Self::Error => "ERROR: ",
        }
    }
}

struct Record {
    level: ConsoleLevel,
    message: Box<str>,
}

enum Event {
    Record(Record),
    Wake,
}

#[derive(Default)]
struct State {
    closed: AtomicBool,
    dropped: AtomicU64,
    truncated: AtomicU64,
    write_errors: AtomicU64,
}

struct Queue {
    tx: SyncSender<Event>,
    state: Arc<State>,
}

/// A cloneable console destination. The default preserves direct terminal
/// output for standalone library contexts. The CLI always supplies a queued
/// sink from `ConsoleWriter`, including for setup/teardown.
#[derive(Clone, Default)]
pub struct ConsoleSink {
    queue: Option<Arc<Queue>>,
}

impl ConsoleSink {
    pub fn emit(&self, level: ConsoleLevel, mut message: String) {
        let truncated = message.len() > MAX_MESSAGE_BYTES;
        if truncated {
            let mut end = MAX_MESSAGE_BYTES;
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
        }
        // Shrink the allocation too: truncating a String alone retains the
        // original capacity and would defeat the queue's byte bound.
        let record = Record {
            level,
            message: message.into_boxed_str(),
        };
        if let Some(queue) = &self.queue {
            if truncated {
                queue.state.truncated.fetch_add(1, Ordering::Relaxed);
            }
            if queue.state.closed.load(Ordering::Acquire)
                || queue.tx.try_send(Event::Record(record)).is_err()
            {
                queue.state.dropped.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            let _ = write_terminal(&record);
        }
    }
}

fn write_record(writer: &mut impl Write, record: &Record) -> io::Result<()> {
    writeln!(writer, "{}{}", record.level.prefix(), record.message)
}

fn write_terminal(record: &Record) -> io::Result<()> {
    match record.level {
        ConsoleLevel::Log => write_record(&mut io::stdout().lock(), record),
        ConsoleLevel::Warn | ConsoleLevel::Error => write_record(&mut io::stderr().lock(), record),
    }
}

#[derive(Debug, Default)]
pub struct ConsoleStats {
    pub dropped: u64,
    pub truncated: u64,
    pub write_errors: u64,
    pub drain_timed_out: bool,
}

impl std::fmt::Display for ConsoleStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "console output: {} dropped, {} truncated, {} write errors{}",
            self.dropped,
            self.truncated,
            self.write_errors,
            if self.drain_timed_out {
                "; drain timed out (messages may remain unwritten)"
            } else {
                ""
            }
        )
    }
}

impl ConsoleStats {
    pub fn has_loss(&self) -> bool {
        self.dropped != 0 || self.truncated != 0 || self.write_errors != 0 || self.drain_timed_out
    }
}

/// Owns the writer thread. Finish after VUs and lifecycle contexts stop; Drop
/// also attempts a bounded drain on early-return paths. A blocked OS write is
/// detached after the deadline rather than hanging test shutdown indefinitely.
pub struct ConsoleWriter {
    sink: ConsoleSink,
    worker: Option<JoinHandle<()>>,
    done: Receiver<()>,
}

impl ConsoleWriter {
    pub fn start(mut file: Option<File>) -> io::Result<Self> {
        Self::with_writer(CONSOLE_CAPACITY, move |record| match file.as_mut() {
            Some(file) => write_record(file, record),
            None => write_terminal(record),
        })
    }

    fn with_writer(
        capacity: usize,
        mut write: impl FnMut(&Record) -> io::Result<()> + Send + 'static,
    ) -> io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(capacity);
        let (done_tx, done) = mpsc::channel();
        let state = Arc::new(State::default());
        let worker_state = state.clone();
        let worker = thread::Builder::new()
            .name("k6-console".into())
            .spawn(move || {
                loop {
                    let event = if worker_state.closed.load(Ordering::Acquire) {
                        rx.try_recv().ok()
                    } else {
                        rx.recv().ok()
                    };
                    match event {
                        Some(Event::Record(record)) => {
                            if write(&record).is_err() {
                                worker_state.write_errors.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Some(Event::Wake) => {}
                        None => break,
                    }
                }
                let _ = done_tx.send(());
            })?;
        Ok(Self {
            sink: ConsoleSink {
                queue: Some(Arc::new(Queue { tx, state })),
            },
            worker: Some(worker),
            done,
        })
    }

    pub fn sink(&self) -> ConsoleSink {
        self.sink.clone()
    }

    pub fn finish(mut self) -> ConsoleStats {
        self.shutdown(DRAIN_TIMEOUT)
    }

    fn shutdown(&mut self, timeout: Duration) -> ConsoleStats {
        let queue = self.sink.queue.as_ref().expect("writer always has a queue");
        queue.state.closed.store(true, Ordering::Release);
        let mut drain_timed_out = false;
        if let Some(worker) = self.worker.take() {
            // Wake an idle receiver even if another context still owns a sink.
            // A full queue already has work to wake/drain the receiver.
            let _ = queue.tx.try_send(Event::Wake);
            if self.done.recv_timeout(timeout).is_ok() {
                if worker.join().is_err() {
                    queue.state.write_errors.fetch_add(1, Ordering::Relaxed);
                }
            } else {
                drain_timed_out = true;
            }
        }
        ConsoleStats {
            dropped: queue.state.dropped.load(Ordering::Relaxed),
            truncated: queue.state.truncated.load(Ordering::Relaxed),
            write_errors: queue.state.write_errors.load(Ordering::Relaxed),
            drain_timed_out,
        }
    }
}

impl Drop for ConsoleWriter {
    fn drop(&mut self) {
        if self.worker.is_some() {
            let stats = self.shutdown(DRAIN_TIMEOUT);
            if stats.has_loss() {
                eprintln!("  warning: {stats}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn slow_writer_drops_overflow_without_blocking_producers() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut first = true;
        let writer = ConsoleWriter::with_writer(2, move |_| {
            if first {
                first = false;
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }
            Ok(())
        })
        .unwrap();
        let sink = writer.sink();
        sink.emit(ConsoleLevel::Log, "first".into());
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        for _ in 0..3 {
            sink.emit(ConsoleLevel::Log, "queued".into());
        }
        release_tx.send(()).unwrap();
        let stats = writer.finish();
        assert_eq!(stats.dropped, 1);
        assert!(!stats.drain_timed_out);
        // A surviving sink cannot enqueue into a finished writer.
        sink.emit(ConsoleLevel::Log, "late".into());
        assert_eq!(
            sink.queue
                .as_ref()
                .unwrap()
                .state
                .dropped
                .load(Ordering::Relaxed),
            2
        );
    }

    #[test]
    fn truncates_utf8_and_drains_in_order_with_live_sink() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let captured = output.clone();
        let writer = ConsoleWriter::with_writer(8, move |record| {
            write_record(&mut *captured.lock().unwrap(), record)
        })
        .unwrap();
        let sink = writer.sink();
        sink.emit(ConsoleLevel::Log, "hello".into());
        sink.emit(ConsoleLevel::Warn, "界".repeat(MAX_MESSAGE_BYTES));
        sink.emit(ConsoleLevel::Error, "last".into());
        let stats = writer.finish();
        assert_eq!(stats.truncated, 1);
        assert_eq!(stats.dropped, 0);
        let text = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert_eq!(
            text,
            format!(
                "hello\nWARN: {}\nERROR: last\n",
                "界".repeat(MAX_MESSAGE_BYTES / 3)
            )
        );
    }

    #[test]
    fn counts_writer_errors() {
        let writer = ConsoleWriter::with_writer(8, |_| Err(io::Error::other("disk full"))).unwrap();
        writer.sink().emit(ConsoleLevel::Log, "message".into());
        assert_eq!(writer.finish().write_errors, 1);
    }

    #[test]
    fn blocked_writer_has_bounded_shutdown() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut writer = ConsoleWriter::with_writer(1, move |_| {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        })
        .unwrap();
        writer.sink().emit(ConsoleLevel::Log, "message".into());
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(writer.shutdown(Duration::from_millis(10)).drain_timed_out);
        release_tx.send(()).unwrap();
        writer.done.recv_timeout(Duration::from_secs(2)).unwrap();
    }
}
