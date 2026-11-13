//! Live tool output on the host.
//!
//! A session that streams its model's commands hands their bytes, as kilnd
//! delivers them, to an optional [`LiveOutput`] sink. The sink is host-only:
//! nothing it sees is journaled, audited or shown to the model, and it never
//! changes a result. [`TerminalLive`] is the CLI's `--live` printer.
//!
//! The session never calls the sink itself. A bounded queue and a writer
//! thread of their own stand between them, so a sink that blocks, such as a
//! printer whose pipe nobody reads, never holds up a command's stream, its
//! cutoff, a cancellation, a deadline or cleanup. When the queue is full,
//! the bytes that do not fit are reported to the sink as a gap instead.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::api::LiveStream;

/// Receives live tool output on the host. Bytes are untrusted sandbox output.
///
/// `call_id` is the host's audit call ID (`call-<turn>-<index>`), never a
/// provider's. Calls for one tool call arrive in order: `started`, then
/// chunks and gaps, then `finished`, which also comes when the call is
/// dropped part-way through.
pub trait LiveOutput: Send + Sync {
    fn started(&self, call_id: &str, execution_id: &str);
    fn chunk(&self, call_id: &str, stream: LiveStream, data: &[u8]);
    fn gap(&self, call_id: &str, stream: LiveStream, dropped: u64, known: bool);
    /// The call's stream ended; `stopped` is `Some("output_limit")` when
    /// the harness cut it off.
    fn finished(&self, call_id: &str, stopped: Option<&str>);
}

/// At most this many bytes of chunk data wait for a slow sink.
pub(crate) const LIVE_QUEUE_BYTES: usize = 1 << 20;

/// At most this many chunks and gaps wait for a slow sink. Each call's
/// `started` and `finished`, and the gaps that report what a full queue
/// left out, are queued beyond it.
pub(crate) const LIVE_QUEUE_EVENTS: usize = 4096;

/// One sink call waiting in a [`LivePump`]'s queue.
#[derive(Debug)]
enum Event {
    Started {
        call_id: String,
        execution_id: String,
    },
    Chunk {
        call_id: String,
        stream: LiveStream,
        data: Vec<u8>,
    },
    Gap {
        call_id: String,
        stream: LiveStream,
        dropped: u64,
        known: bool,
    },
    Finished {
        call_id: String,
        stopped: Option<String>,
    },
}

#[derive(Default)]
struct Queue {
    events: VecDeque<Event>,
    /// Chunk bytes in `events`.
    data_bytes: usize,
    /// Per call and stream, bytes left out of the queue that the sink has
    /// not yet been told of, and whether that count is exact.
    pending_gaps: BTreeMap<(String, LiveStream), (u64, bool)>,
    /// The writer has taken an event and is handing it to the sink.
    in_flight: bool,
    /// The pump was dropped: the writer exits once the queue is empty.
    closed: bool,
}

impl Queue {
    fn has_room(&self) -> bool {
        self.events.len() < LIVE_QUEUE_EVENTS
    }

    /// Report `dropped` more bytes of `stream` as not shown live.
    fn coalesce(&mut self, call_id: &str, stream: LiveStream, dropped: u64, known: bool) {
        let gap = self
            .pending_gaps
            .entry((call_id.to_string(), stream))
            .or_insert((0, true));
        gap.0 = gap.0.saturating_add(dropped);
        gap.1 &= known;
    }

    /// Queue the pending gap of `stream`, if any.
    fn flush_gap(&mut self, call_id: &str, stream: LiveStream) {
        let key = (call_id.to_string(), stream);
        if let Some((dropped, known)) = self.pending_gaps.remove(&key) {
            self.events.push_back(Event::Gap {
                call_id: key.0,
                stream,
                dropped,
                known,
            });
        }
    }
}

struct PumpShared {
    sink: Arc<dyn LiveOutput>,
    queue: Mutex<Queue>,
    /// Signalled when an event is queued or the pump closes.
    work: Condvar,
    /// Signalled when the writer finishes handing an event to the sink.
    idle: Condvar,
}

impl PumpShared {
    fn queue(&self) -> MutexGuard<'_, Queue> {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The writer thread: hand each queued event to the sink, in order,
    /// never holding the lock while the sink runs.
    fn write(&self) {
        loop {
            let event = {
                let mut queue = self.queue();
                loop {
                    if let Some(event) = queue.events.pop_front() {
                        if let Event::Chunk { data, .. } = &event {
                            queue.data_bytes = queue.data_bytes.saturating_sub(data.len());
                        }
                        queue.in_flight = true;
                        break event;
                    }
                    if queue.closed {
                        return;
                    }
                    queue = self
                        .work
                        .wait(queue)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            };
            match &event {
                Event::Started {
                    call_id,
                    execution_id,
                } => self.sink.started(call_id, execution_id),
                Event::Chunk {
                    call_id,
                    stream,
                    data,
                } => self.sink.chunk(call_id, *stream, data),
                Event::Gap {
                    call_id,
                    stream,
                    dropped,
                    known,
                } => self.sink.gap(call_id, *stream, *dropped, *known),
                Event::Finished { call_id, stopped } => {
                    self.sink.finished(call_id, stopped.as_deref());
                }
            }
            self.queue().in_flight = false;
            self.idle.notify_all();
        }
    }
}

/// The only caller of a session's host sink: a [`LiveOutput`] whose calls
/// only queue, handed to the sink in order by a thread of its own.
///
/// Its calls never wait for the sink or for I/O. At most
/// [`LIVE_QUEUE_BYTES`] of chunk data and [`LIVE_QUEUE_EVENTS`] chunks and
/// gaps wait; a chunk that does not fit is left out and its bytes are added
/// to a pending gap for its call and stream, which is queued before that
/// stream's next chunk that fits, or before the call's `finished`. So the
/// sink still sees, per call and in order, `started`, chunks and gaps, and
/// `finished`, with every byte it was not shown reported in a gap. A sink
/// that panics stops the writer; calls go on not blocking.
pub(crate) struct LivePump {
    shared: Arc<PumpShared>,
}

impl LivePump {
    /// A pump to `sink`, with its writer thread started.
    pub(crate) fn new(sink: Arc<dyn LiveOutput>) -> Arc<Self> {
        let shared = Arc::new(PumpShared {
            sink,
            queue: Mutex::new(Queue::default()),
            work: Condvar::new(),
            idle: Condvar::new(),
        });
        let writer = Arc::clone(&shared);
        // Without a writer nothing reaches the sink, and the bounded queue
        // still never blocks a caller.
        let _ = std::thread::Builder::new()
            .name("kiln-harness-live".to_string())
            .spawn(move || writer.write());
        Arc::new(Self { shared })
    }

    /// Wait up to `wait` for queued events to reach the sink; true if they
    /// all did. A `wait` too long to be a point in time, such as
    /// `Duration::MAX`, waits without a deadline.
    pub(crate) fn drain(&self, wait: Duration) -> bool {
        let until = Instant::now().checked_add(wait);
        let mut queue = self.shared.queue();
        loop {
            if queue.events.is_empty() && !queue.in_flight {
                return true;
            }
            let Some(until) = until else {
                queue = self
                    .shared
                    .idle
                    .wait(queue)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                continue;
            };
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            queue = self
                .shared
                .idle
                .wait_timeout(queue, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    /// Queue `event` (or not, in `update`) and wake the writer.
    fn push(&self, update: impl FnOnce(&mut Queue)) {
        update(&mut self.shared.queue());
        self.shared.work.notify_one();
    }
}

impl Drop for LivePump {
    fn drop(&mut self) {
        self.shared.queue().closed = true;
        self.shared.work.notify_all();
    }
}

impl LiveOutput for LivePump {
    fn started(&self, call_id: &str, execution_id: &str) {
        self.push(|queue| {
            queue.events.push_back(Event::Started {
                call_id: call_id.to_string(),
                execution_id: execution_id.to_string(),
            });
        });
    }

    fn chunk(&self, call_id: &str, stream: LiveStream, data: &[u8]) {
        self.push(|queue| {
            // The chunk is queued only with room for it and, first, for the
            // gap still pending before it; otherwise it joins that gap.
            let pending = queue
                .pending_gaps
                .contains_key(&(call_id.to_string(), stream));
            let fits = queue.events.len() + usize::from(pending) < LIVE_QUEUE_EVENTS
                && queue.data_bytes.saturating_add(data.len()) <= LIVE_QUEUE_BYTES;
            if fits {
                queue.flush_gap(call_id, stream);
                queue.data_bytes += data.len();
                queue.events.push_back(Event::Chunk {
                    call_id: call_id.to_string(),
                    stream,
                    data: data.to_vec(),
                });
            } else {
                let length = u64::try_from(data.len()).unwrap_or(u64::MAX);
                queue.coalesce(call_id, stream, length, true);
            }
        });
    }

    fn gap(&self, call_id: &str, stream: LiveStream, dropped: u64, known: bool) {
        self.push(|queue| {
            let key = (call_id.to_string(), stream);
            if queue.pending_gaps.contains_key(&key) || !queue.has_room() {
                queue.coalesce(call_id, stream, dropped, known);
            } else {
                queue.events.push_back(Event::Gap {
                    call_id: key.0,
                    stream,
                    dropped,
                    known,
                });
            }
        });
    }

    fn finished(&self, call_id: &str, stopped: Option<&str>) {
        self.push(|queue| {
            queue.flush_gap(call_id, LiveStream::Stdout);
            queue.flush_gap(call_id, LiveStream::Stderr);
            queue.events.push_back(Event::Finished {
                call_id: call_id.to_string(),
                stopped: stopped.map(str::to_string),
            });
        });
    }
}

/// A partial line longer than this is printed as a line of its own, so one
/// newline-free flood cannot grow the buffer.
const MAX_PENDING_LINE: usize = 4 << 10;

/// Prints live output to a terminal (the CLI uses stderr), one line at a
/// time, each prefixed `[<call-id> out] ` or `[<call-id> err] `. Every byte
/// that is not printable ASCII, a newline or a tab is shown as `\xNN`, so
/// no escape sequence, carriage return, bell or other control byte from a
/// sandbox reaches the terminal, and neither does UTF-8: a multi-byte
/// character is its escaped bytes, however the chunks split it.
pub struct TerminalLive<W: Write + Send> {
    inner: Mutex<Printer<W>>,
}

struct Printer<W> {
    out: W,
    /// Bytes of an unfinished line, per call and stream.
    pending: BTreeMap<(String, LiveStream), Vec<u8>>,
}

impl TerminalLive<std::io::Stderr> {
    /// A printer to this process's stderr.
    #[must_use]
    pub fn stderr() -> Self {
        Self::new(std::io::stderr())
    }
}

impl<W: Write + Send> TerminalLive<W> {
    #[must_use]
    pub fn new(out: W) -> Self {
        Self {
            inner: Mutex::new(Printer {
                out,
                pending: BTreeMap::new(),
            }),
        }
    }

    /// The writer, for tests.
    ///
    /// # Panics
    ///
    /// Never: a poisoned lock is recovered.
    pub fn into_inner(self) -> W {
        self.inner
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .out
    }

    fn printer(&self) -> std::sync::MutexGuard<'_, Printer<W>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl<W: Write> Printer<W> {
    /// Print `line` (without its newline) under the call's prefix. Write
    /// errors are ignored: live output never affects a session.
    fn line(&mut self, call_id: &str, stream: LiveStream, line: &[u8]) {
        let _ = writeln!(
            self.out,
            "[{call_id} {}] {}",
            short(stream),
            sanitize_live(line)
        );
    }

    fn flush_pending(&mut self, call_id: &str, stream: LiveStream) {
        if let Some(pending) = self.pending.remove(&(call_id.to_string(), stream))
            && !pending.is_empty()
        {
            self.line(call_id, stream, &pending);
        }
    }
}

fn short(stream: LiveStream) -> &'static str {
    match stream {
        LiveStream::Stdout => "out",
        LiveStream::Stderr => "err",
    }
}

impl<W: Write + Send> LiveOutput for TerminalLive<W> {
    fn started(&self, _call_id: &str, _execution_id: &str) {}

    fn chunk(&self, call_id: &str, stream: LiveStream, data: &[u8]) {
        let mut printer = self.printer();
        let key = (call_id.to_string(), stream);
        let mut pending = printer.pending.remove(&key).unwrap_or_default();
        for &byte in data {
            if byte == b'\n' {
                printer.line(call_id, stream, &pending);
                pending.clear();
            } else {
                pending.push(byte);
                if pending.len() >= MAX_PENDING_LINE {
                    printer.line(call_id, stream, &pending);
                    pending.clear();
                }
            }
        }
        if !pending.is_empty() {
            printer.pending.insert(key, pending);
        }
        let _ = printer.out.flush();
    }

    fn gap(&self, call_id: &str, stream: LiveStream, dropped: u64, known: bool) {
        let mut printer = self.printer();
        printer.flush_pending(call_id, stream);
        let at_least = if known { "" } else { ">= " };
        let _ = writeln!(
            printer.out,
            "[{call_id} {}: {at_least}{dropped} bytes not shown live]",
            short(stream)
        );
        let _ = printer.out.flush();
    }

    fn finished(&self, call_id: &str, stopped: Option<&str>) {
        let mut printer = self.printer();
        printer.flush_pending(call_id, LiveStream::Stdout);
        printer.flush_pending(call_id, LiveStream::Stderr);
        if let Some(stopped) = stopped {
            let _ = writeln!(
                printer.out,
                "[{call_id} stopped: {}]",
                sanitize_live(stopped.as_bytes())
            );
        }
        let _ = printer.out.flush();
    }
}

/// `bytes` made safe for a terminal: printable ASCII, newline and tab as
/// they are, and every other byte (escape, carriage return, bell, NUL, DEL,
/// and each byte of UTF-8) as `\xNN`.
#[must_use]
pub fn sanitize_live(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut safe = String::with_capacity(bytes.len());
    for &byte in bytes {
        if matches!(byte, b' '..=b'~' | b'\n' | b'\t') {
            safe.push(char::from(byte));
        } else {
            let _ = write!(safe, "\\x{byte:02x}");
        }
    }
    safe
}

#[cfg(test)]
mod pump_tests {
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    use super::{LIVE_QUEUE_BYTES, LIVE_QUEUE_EVENTS, LiveOutput, LivePump};
    use crate::api::LiveStream;

    /// Blocks in every call until released, then records it.
    #[derive(Default)]
    struct Gated {
        open: (Mutex<bool>, Condvar),
        entered: std::sync::atomic::AtomicBool,
        heard: Mutex<Vec<String>>,
    }

    impl Gated {
        /// Wait until the writer is stuck in its first call.
        fn until_entered(&self) {
            let by = Instant::now() + Duration::from_secs(5);
            while !self.entered.load(std::sync::atomic::Ordering::Acquire) {
                assert!(Instant::now() < by, "the writer took the first event");
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        fn wait(&self, event: String) {
            self.entered
                .store(true, std::sync::atomic::Ordering::Release);
            let mut open = self.open.0.lock().expect("gate");
            while !*open {
                open = self.open.1.wait(open).expect("gate");
            }
            self.heard.lock().expect("heard").push(event);
        }

        fn release(&self) {
            *self.open.0.lock().expect("gate") = true;
            self.open.1.notify_all();
        }
    }

    impl LiveOutput for Gated {
        fn started(&self, call_id: &str, _execution_id: &str) {
            self.wait(format!("{call_id} started"));
        }
        fn chunk(&self, call_id: &str, stream: LiveStream, data: &[u8]) {
            self.wait(format!("{call_id} {} {}", stream.as_str(), data.len()));
        }
        fn gap(&self, call_id: &str, stream: LiveStream, dropped: u64, known: bool) {
            self.wait(format!(
                "{call_id} {} gap {dropped} {known}",
                stream.as_str()
            ));
        }
        fn finished(&self, call_id: &str, stopped: Option<&str>) {
            self.wait(format!("{call_id} finished {stopped:?}"));
        }
    }

    #[test]
    fn a_full_queue_reports_what_it_left_out_as_one_gap() {
        let sink = Arc::new(Gated::default());
        let pump = LivePump::new(Arc::clone(&sink) as Arc<dyn LiveOutput>);
        pump.started("c", "ex-1");
        sink.until_entered();
        let begun = Instant::now();
        // Twice as many one-byte chunks as the queue holds, then a kilnd
        // gap whose count is a lower bound, then the end.
        for _ in 0..2 * LIVE_QUEUE_EVENTS {
            pump.chunk("c", LiveStream::Stdout, b"y");
        }
        pump.gap("c", LiveStream::Stdout, 10, false);
        pump.chunk("c", LiveStream::Stderr, b"e");
        pump.finished("c", Some("output_limit"));
        // None of that waited for the sink.
        assert!(begun.elapsed() < Duration::from_secs(2));
        assert!(!pump.drain(Duration::from_millis(10)));

        sink.release();
        assert!(pump.drain(Duration::from_secs(5)));
        let heard = sink.heard.lock().expect("heard").clone();
        assert_eq!(heard.first().map(String::as_str), Some("c started"));
        let shown = heard.iter().filter(|event| *event == "c stdout 1").count();
        // `started` was with the sink: the queue held exactly its cap.
        assert_eq!(shown, LIVE_QUEUE_EVENTS);
        // Every byte not shown, the kilnd gap merged in, as one inexact gap
        // before the end; the stderr chunk met a full queue too.
        let left_out = 2 * LIVE_QUEUE_EVENTS - shown + 10;
        assert_eq!(
            heard[shown + 1..],
            [
                format!("c stdout gap {left_out} false"),
                "c stderr gap 1 true".to_string(),
                "c finished Some(\"output_limit\")".to_string(),
            ],
            "{:?}",
            &heard[heard.len().saturating_sub(4)..]
        );
    }

    #[test]
    fn chunk_bytes_are_bounded_and_a_later_chunk_that_fits_follows_its_gap() {
        let sink = Arc::new(Gated::default());
        let pump = LivePump::new(Arc::clone(&sink) as Arc<dyn LiveOutput>);
        let big = vec![b'y'; LIVE_QUEUE_BYTES / 2];
        pump.started("c", "ex-1");
        sink.until_entered();
        for _ in 0..4 {
            pump.chunk("c", LiveStream::Stdout, &big);
        }
        sink.release();
        assert!(pump.drain(Duration::from_secs(5)));
        // Room again: the pending gap goes first, then the chunk.
        pump.chunk("c", LiveStream::Stdout, b"tail");
        pump.finished("c", None);
        assert!(pump.drain(Duration::from_secs(5)));
        // `started` was with the sink while two halves filled the queue;
        // the other two waited as one gap.
        let heard = sink.heard.lock().expect("heard").clone();
        let half = LIVE_QUEUE_BYTES / 2;
        assert_eq!(
            heard[..3],
            [
                "c started".to_string(),
                format!("c stdout {half}"),
                format!("c stdout {half}")
            ]
        );
        assert_eq!(
            heard[3..],
            [
                format!("c stdout gap {LIVE_QUEUE_BYTES} true"),
                "c stdout 4".to_string(),
                "c finished None".to_string(),
            ]
        );
    }

    struct Panics;

    impl LiveOutput for Panics {
        fn started(&self, _call_id: &str, _execution_id: &str) {}
        fn chunk(&self, _call_id: &str, _stream: LiveStream, _data: &[u8]) {
            panic!("a broken sink");
        }
        fn gap(&self, _call_id: &str, _stream: LiveStream, _dropped: u64, _known: bool) {}
        fn finished(&self, _call_id: &str, _stopped: Option<&str>) {}
    }

    #[test]
    fn a_sink_that_panics_never_blocks_its_callers() {
        let pump = LivePump::new(Arc::new(Panics));
        let begun = Instant::now();
        let chunk = vec![b'y'; 64 << 10];
        pump.started("c", "ex-1");
        for _ in 0..1024 {
            pump.chunk("c", LiveStream::Stdout, &chunk);
        }
        pump.finished("c", None);
        assert!(begun.elapsed() < Duration::from_secs(2));
        assert!(!pump.drain(Duration::from_millis(50)));
        // Queued chunk data stayed bounded.
        assert!(pump.shared.queue().data_bytes <= LIVE_QUEUE_BYTES);
    }
}

#[cfg(test)]
mod tests {
    use super::{LiveOutput, TerminalLive, sanitize_live};
    use crate::api::LiveStream;

    fn printed(feed: impl FnOnce(&TerminalLive<Vec<u8>>)) -> String {
        let live = TerminalLive::new(Vec::new());
        feed(&live);
        String::from_utf8(live.into_inner()).expect("the printer writes ASCII only")
    }

    #[test]
    fn control_bytes_and_utf8_are_escaped() {
        assert_eq!(sanitize_live(b"plain text\tok"), "plain text\tok");
        assert_eq!(sanitize_live(b"\x1b[31mred\x1b[0m"), "\\x1b[31mred\\x1b[0m");
        assert_eq!(sanitize_live(b"over\rwrite"), "over\\x0dwrite");
        assert_eq!(
            sanitize_live(b"nul\0bell\x07del\x7f"),
            "nul\\x00bell\\x07del\\x7f"
        );
        assert_eq!(sanitize_live("é".as_bytes()), "\\xc3\\xa9");
        assert_eq!(sanitize_live(b"bad \xff\xfe"), "bad \\xff\\xfe");
        // An OSC sequence that would set the terminal title.
        assert_eq!(sanitize_live(b"\x1b]0;t\x07"), "\\x1b]0;t\\x07");
        assert!(
            sanitize_live(&(0..=255).collect::<Vec<u8>>())
                .bytes()
                .all(|byte| matches!(byte, b' '..=b'~' | b'\n' | b'\t'))
        );
    }

    #[test]
    fn lines_are_prefixed_and_split_chunks_join() {
        let text = printed(|live| {
            live.started("call-1-1", "ex-1");
            live.chunk("call-1-1", LiveStream::Stdout, b"first li");
            live.chunk("call-1-1", LiveStream::Stderr, b"warn\x1b[0m\n");
            live.chunk("call-1-1", LiveStream::Stdout, b"ne\nsecond\r\n\xc3");
            live.chunk("call-1-1", LiveStream::Stdout, b"\xa9 tail");
            live.finished("call-1-1", None);
        });
        assert_eq!(
            text,
            "[call-1-1 err] warn\\x1b[0m\n\
             [call-1-1 out] first line\n\
             [call-1-1 out] second\\x0d\n\
             [call-1-1 out] \\xc3\\xa9 tail\n"
        );
    }

    #[test]
    fn gaps_and_cutoffs_are_reported() {
        let text = printed(|live| {
            live.chunk("call-2-1", LiveStream::Stdout, b"part");
            live.gap("call-2-1", LiveStream::Stdout, 4096, true);
            live.gap("call-2-1", LiveStream::Stderr, 7, false);
            live.finished("call-2-1", Some("output_limit"));
        });
        assert_eq!(
            text,
            "[call-2-1 out] part\n\
             [call-2-1 out: 4096 bytes not shown live]\n\
             [call-2-1 err: >= 7 bytes not shown live]\n\
             [call-2-1 stopped: output_limit]\n"
        );
    }

    #[test]
    fn a_line_without_newlines_is_printed_in_bounded_pieces() {
        let text = printed(|live| {
            live.chunk("call-1-1", LiveStream::Stdout, &vec![b'y'; 10 << 10]);
            live.finished("call-1-1", None);
        });
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(
            lines
                .iter()
                .all(|line| line.starts_with("[call-1-1 out] y"))
        );
        assert_eq!(
            lines
                .iter()
                .map(|line| line.len() - "[call-1-1 out] ".len())
                .sum::<usize>(),
            10 << 10
        );
    }
}
