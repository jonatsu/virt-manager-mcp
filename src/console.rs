//! Serial / virtio console sessions (the equivalent of `virsh console`).
//!
//! Each open session owns a non-blocking libvirt stream. A background thread
//! drains it into a bounded scrollback buffer addressed by absolute byte
//! offsets, so callers can read incrementally with a cursor.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use virt::domain::Domain;
use virt::stream::Stream;
use virt::sys;

use crate::hypervisor::Hypervisor;

const POLL: Duration = Duration::from_millis(30);

/// How long one segment of a write may wait for room on a full stream.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// How long output must pause before a waiting read returns. A guest answers
/// in bursts (echo, then a prompt), so returning on the first byte would split
/// one answer across reads.
const SETTLE: Duration = Duration::from_millis(500);

#[derive(Default)]
struct Buffer {
    data: VecDeque<u8>,
    /// Absolute offset of `data[0]`.
    start: u64,
    /// Set once the stream ended; holds the reason.
    closed: Option<String>,
    /// Highest offset any read has returned. A waiting read waits for output
    /// beyond it, so rereading from an old offset still waits for the guest.
    delivered: u64,
    /// Where a wait without `since` starts: the buffer's end before the last write, or just past the last match.
    wait_cursor: u64,
}

impl Buffer {
    fn end(&self) -> u64 {
        self.start + self.data.len() as u64
    }

    /// Marks the start of a write: the guest's answer to it begins at the returned offset.
    fn begin_write(&mut self) -> u64 {
        self.wait_cursor = self.end();
        self.wait_cursor
    }

    /// Everything from `since` (default: the wait cursor) to the end, without marking it delivered.
    fn scan(&self, since: Option<u64>) -> ScanResult {
        let from = since
            .unwrap_or(self.wait_cursor)
            .min(self.end())
            .max(self.start);
        let skip = usize::try_from(from - self.start).expect("the buffer fits in memory");
        ScanResult {
            data: self.data.iter().skip(skip).copied().collect(),
            from,
            closed: self.closed.clone(),
        }
    }

    /// Output from absolute offset `since` (None = only the latest `max_bytes`).
    fn slice(&self, since: Option<u64>, max_bytes: usize) -> ReadResult {
        let end = self.end();
        let from = match since {
            Some(s) => s.min(end),
            None => end.saturating_sub(max_bytes as u64).max(self.start),
        };
        let dropped = self.start.saturating_sub(from);
        let from = from.max(self.start);
        let skip = (from - self.start) as usize;
        let data: Vec<u8> = self
            .data
            .iter()
            .skip(skip)
            .take(max_bytes)
            .copied()
            .collect();
        ReadResult {
            next: from + data.len() as u64,
            data,
            dropped,
            closed: self.closed.clone(),
            generation: 0,
        }
    }
}

/// Tracks when the buffer end last moved, to tell when a waiting read may return.
#[derive(Default)]
struct Settle {
    last: Option<(u64, Instant)>,
}

impl Settle {
    /// True once output beyond `delivered` exists and `end` has not moved for `SETTLE`.
    fn ready(&mut self, end: u64, delivered: u64, now: Instant) -> bool {
        if end <= delivered {
            return false;
        }
        match self.last {
            Some((seen, at)) if seen == end => now.duration_since(at) >= SETTLE,
            _ => {
                self.last = Some((end, now));
                false
            }
        }
    }
}

/// One end of a console stream: libvirt's in the server, a fake in tests.
pub trait ConsoleStream: Send + Sync {
    /// Reads what the guest has written, without blocking.
    fn recv(&self, buf: &mut [u8]) -> Recv;
    /// Writes toward the guest, without blocking.
    fn send(&self, data: &[u8]) -> Sent;
    /// Releases the stream, so libvirt frees the console for another client.
    fn abort(&self);
}

/// The outcome of one non-blocking read of a console stream.
pub enum Recv {
    Data(usize),
    /// Nothing to read yet.
    Again,
    /// The stream ended: the VM shut down or another client took the console.
    End,
    Error(String),
}

/// The outcome of one non-blocking write to a console stream.
pub enum Sent {
    Bytes(usize),
    /// The stream is full.
    Again,
    Error(String),
}

/// A libvirt console stream.
struct LibvirtStream(Stream);

impl ConsoleStream for LibvirtStream {
    fn recv(&self, buf: &mut [u8]) -> Recv {
        let n =
            unsafe { sys::virStreamRecv(self.0.as_ptr(), buf.as_mut_ptr() as *mut _, buf.len()) };
        match n {
            0 => Recv::End,
            -2 => Recv::Again,
            n if n > 0 => Recv::Data(n as usize),
            _ => Recv::Error(virt::error::Error::last_error().message().to_string()),
        }
    }

    fn send(&self, data: &[u8]) -> Sent {
        let n =
            unsafe { sys::virStreamSend(self.0.as_ptr(), data.as_ptr() as *const _, data.len()) };
        match n {
            n if n >= 0 => Sent::Bytes(n as usize),
            -2 => Sent::Again,
            _ => Sent::Error(virt::error::Error::last_error().message().to_string()),
        }
    }

    fn abort(&self) {
        unsafe { sys::virStreamAbort(self.0.as_ptr()) };
    }
}

/// Why a console could not be opened.
#[derive(Debug, PartialEq, Eq)]
pub enum OpenError {
    /// The VM has no console device, or none with the requested alias; holds libvirt's message.
    NoDevice(String),
    /// Anything else, with libvirt's reason.
    Failed(String),
}

/// Opens a VM's console on a new stream. The force flag is libvirt's: it takes the console over from another
/// client, and is separate from replacing this server's own session.
pub trait ConsoleOpener {
    fn open(&self, device: Option<&str>, force: bool) -> Result<Box<dyn ConsoleStream>, OpenError>;
}

/// Opens consoles through libvirt.
pub struct LibvirtOpener<'a> {
    pub hv: &'a Hypervisor,
    pub dom: &'a Domain,
}

impl ConsoleOpener for LibvirtOpener<'_> {
    fn open(&self, device: Option<&str>, force: bool) -> Result<Box<dyn ConsoleStream>, OpenError> {
        let stream = self
            .hv
            .with_conn(|conn| Ok(Stream::new(conn, sys::VIR_STREAM_NONBLOCK)?))
            .map_err(|e| OpenError::Failed(format!("{e:#}")))?;
        let flags = if force {
            sys::VIR_DOMAIN_CONSOLE_FORCE
        } else {
            sys::VIR_DOMAIN_CONSOLE_SAFE
        };
        self.dom.open_console(device, &stream, flags).map_err(|e| {
            let message = e.message().to_string();
            // libvirt reports a missing console device, or an alias that matches none, this way.
            if message.contains("cannot find character device") {
                OpenError::NoDevice(message)
            } else {
                OpenError::Failed(message)
            }
        })?;
        Ok(Box::new(LibvirtStream(stream)))
    }
}

struct Session {
    stream: Arc<dyn ConsoleStream>,
    buffer: Arc<Mutex<Buffer>>,
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    aborted: bool,
    device: Option<String>,
    generation: u64,
    /// The running domain's ID when the session opened; a stop and start gives the domain a new one.
    domain_id: Option<u32>,
    /// Held for the length of a write, so two writes cannot interleave their bytes.
    writing: Arc<Mutex<()>>,
}

impl Session {
    /// A session counts as live while its reader runs and has not recorded an end.
    fn is_live(&self) -> bool {
        self.buffer.lock().unwrap().closed.is_none()
            && self.reader.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// Stops the reader and releases the stream. Slow, so callers hold no lock.
    fn release(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
        if !std::mem::replace(&mut self.aborted, true) {
            self.stream.abort();
        }
    }

    /// Ends the session and keeps its output as a previous run. A reason the stream already recorded stays;
    /// otherwise the run ended for `why`.
    fn retire(mut self, why: &str) -> Previous {
        self.buffer
            .lock()
            .unwrap()
            .closed
            .get_or_insert_with(|| why.to_string());
        self.release();
        Previous {
            buffer: self.buffer.clone(),
            generation: self.generation,
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.release();
    }
}

/// The output of a VM's earlier run: its buffer, which holds the reason the run ended, and its own generation.
struct Previous {
    buffer: Arc<Mutex<Buffer>>,
    generation: u64,
}

/// A VM's console state: its current session, and the run before it.
#[derive(Default)]
struct Entry {
    current: Option<Session>,
    previous: Option<Previous>,
}

/// A VM as the console tools name it: sessions are keyed by `uuid`, and `name` only appears in messages.
#[derive(Debug, Clone, Copy)]
pub struct VmId<'a> {
    pub uuid: &'a str,
    pub name: &'a str,
}

/// Which run of a VM `console_read` reads.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Run {
    /// The open session, or the one that ended last while the VM kept its run. The default.
    #[default]
    Current,
    /// The run before the current one, or the session that `console_close` or a reset ended.
    Previous,
}

/// A session's output from an offset, read for a wait without marking it delivered.
pub struct ScanResult {
    pub data: Vec<u8>,
    /// Offset of `data[0]`: the requested offset, or the scrollback's start when output from there was dropped.
    pub from: u64,
    pub closed: Option<String>,
}

/// A scan with the session it came from.
pub struct SessionScan {
    pub scan: ScanResult,
    pub generation: u64,
    pub domain_id: Option<u32>,
}

/// How a write spaces its bytes, for receivers that drop input sent back to back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pacing {
    None,
    /// A pause after every byte but the last.
    PerByte(Duration),
    /// Segments of `size` bytes with a pause after every segment but the last.
    Chunks {
        size: usize,
        delay: Duration,
    },
}

impl Pacing {
    /// The segment size for a write of `len` bytes, and the pause between segments.
    fn shape(self, len: usize) -> (usize, Duration) {
        match self {
            Pacing::None => (len.max(1), Duration::ZERO),
            Pacing::PerByte(delay) => (1, delay),
            Pacing::Chunks { size, delay } => (size.max(1), delay),
        }
    }

    /// The segments a write of `len` bytes goes out in, as byte ranges.
    fn segments(self, len: usize) -> impl Iterator<Item = std::ops::Range<usize>> {
        let (size, _) = self.shape(len);
        (0..len)
            .step_by(size)
            .map(move |at| at..at.saturating_add(size).min(len))
    }

    /// Pacing from a write's options, refused when both modes are given, a delay exceeds 1000 ms, a chunk is
    /// empty or lacks its delay, or the pauses for `len` bytes would take longer than `max_wait`.
    pub fn from_options(
        inter_char_delay_ms: Option<u64>,
        chunk_size: Option<usize>,
        chunk_delay_ms: Option<u64>,
        len: usize,
        max_wait: Duration,
    ) -> Result<Self> {
        const MAX_DELAY_MS: u64 = 1000;
        let delay = |ms: u64, name: &str| {
            if ms > MAX_DELAY_MS {
                bail!("{name} must be at most {MAX_DELAY_MS} ms");
            }
            Ok(Duration::from_millis(ms))
        };
        let pacing = match (inter_char_delay_ms, chunk_size, chunk_delay_ms) {
            (None, None, None) => Pacing::None,
            (Some(ms), None, None) => Pacing::PerByte(delay(ms, "inter_char_delay_ms")?),
            (None, Some(0), _) => bail!("chunk_size must be at least 1"),
            (None, Some(size), Some(ms)) => Pacing::Chunks {
                size,
                delay: delay(ms, "chunk_delay_ms")?,
            },
            (None, _, _) => bail!("chunk_size and chunk_delay_ms go together; give both"),
            (Some(_), _, _) => {
                bail!("give either inter_char_delay_ms or chunk_size with chunk_delay_ms, not both")
            }
        };
        let took = pacing.duration(len);
        if took > max_wait {
            bail!(
                "this paced write would take {} s, above limits.max_wait_secs ({} s); send it in parts or pace it \
                 less",
                took.as_secs_f64().ceil(),
                max_wait.as_secs()
            );
        }
        Ok(pacing)
    }

    /// The pauses a write of `len` bytes spends, not counting the time the bytes take to send.
    pub fn duration(self, len: usize) -> Duration {
        let (size, delay) = self.shape(len);
        let pauses = len.div_ceil(size).saturating_sub(1);
        delay.saturating_mul(u32::try_from(pauses).unwrap_or(u32::MAX))
    }
}

/// The first generation of a server process started at `now_secs`, so two processes rarely share numbers.
fn first_generation(now_secs: u64) -> u64 {
    now_secs.saturating_mul(1000)
}

/// Checks that an offset belongs to the run with `current` generation, which `run` names in the message. Offset 0
/// and an omitted offset need no generation; any other offset must name the generation of the read that returned it.
fn check_generation(since: Option<u64>, given: Option<u64>, current: u64, run: Run) -> Result<()> {
    let which = match run {
        Run::Current => "the open session",
        Run::Previous => "the previous run",
    };
    match (since, given) {
        (_, Some(g)) if g != current => bail!(
            "offset from an earlier console session (generation {g}; {which} is generation {current}); \
             read from since 0 with generation {current}"
        ),
        (Some(s), None) if s > 0 => bail!(
            "since {s} needs the generation of the read that returned it; {which} is generation {current}"
        ),
        _ => Ok(()),
    }
}

#[derive(Debug)]
pub struct ReadResult {
    pub data: Vec<u8>,
    /// Offset to pass as `since` on the next read.
    pub next: u64,
    /// Bytes that were requested but already dropped from the scrollback.
    pub dropped: u64,
    pub closed: Option<String>,
    /// The session's generation, to pass with `next`.
    pub generation: u64,
}

/// What `console_close` records as the reason a session ended.
pub const CLOSED_BY_CONSOLE_CLOSE: &str = "closed by console_close";

/// What a session that was still live records when a new run replaces it.
const REPLACED_BY_NEW_RUN: &str = "replaced by a new run";

/// A request to open a VM's console.
pub struct OpenRequest<'a> {
    pub vm: VmId<'a>,
    /// The running domain's ID. A session from a run with another ID is an earlier run's.
    pub domain_id: Option<u32>,
    pub device: Option<&'a str>,
    /// libvirt's force flag: take the console over from another client.
    pub force: bool,
    /// Replace a live session of this server instead of attaching to it. Separate from `force`, so the server can
    /// replace its own session without taking the console from anyone.
    pub replace: bool,
    /// The scrollback limit of the new session.
    pub buffer_bytes: usize,
}

/// How an open ended.
#[derive(Debug, PartialEq, Eq)]
pub enum OpenResult {
    /// A new session, with its generation.
    Opened { generation: u64 },
    /// The VM's live session was kept, because the request did not ask to replace it.
    Attached {
        generation: u64,
        device: Option<String>,
    },
    /// The VM has no console device, or none with the requested alias; holds libvirt's message.
    NoDevice(String),
    /// libvirt refused, or the live session is on another device; holds the reason.
    Failed(String),
}

impl OpenResult {
    /// The reply for `console_open` on the VM `name`, or the error it ends in.
    pub fn into_message(self, name: &str) -> Result<String> {
        match self {
            Self::Opened { generation } => Ok(format!(
                "console for '{name}' opened (generation {generation}); output is buffered from now on"
            )),
            Self::Attached { generation, device } => Ok(format!(
                "console for '{name}' is already open (device: {}, generation {generation}); attached to it. Pass \
                 force: true to open a new session and drop its buffered output",
                device.as_deref().unwrap_or("default")
            )),
            Self::NoDevice(reason) => {
                bail!("VM '{name}' has no console device to open: {reason}")
            }
            Self::Failed(reason) => bail!(
                "cannot open console: {reason} (it may be in use, e.g. by virt-manager; retry with force=true to \
                 take it over)"
            ),
        }
    }
}

pub struct Consoles {
    /// Each VM's current session and previous run, by domain UUID. Held only to check and swap entries: every scan
    /// in a wait takes it, so joining a reader and releasing a stream happen outside it.
    entries: Mutex<HashMap<String, Entry>>,
    /// Serializes opens, closes and forgets, so two opens never both pass the "already open" check and a run that
    /// ends cannot land in the previous-run slot after a newer one, or after its VM was forgotten. Scans and writes
    /// never take it.
    opening: Mutex<()>,
    /// The last generation handed out.
    generation: AtomicU64,
}

impl Default for Consoles {
    fn default() -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        Self {
            entries: Mutex::default(),
            opening: Mutex::default(),
            generation: AtomicU64::new(first_generation(now)),
        }
    }
}

impl Consoles {
    /// Opens the VM's console. A live session of the same run is kept and attached to, unless the request replaces
    /// it. Otherwise the old session goes: it moves to the previous-run slot when it belongs to an earlier run, a
    /// different domain ID, and is dropped when it belongs to this one, so a habitual force after a restart cannot
    /// push the crashed run out of the slot. The move happens before the open, so an open that fails still leaves
    /// the earlier run readable as the previous one.
    pub fn open(&self, opener: &dyn ConsoleOpener, req: OpenRequest) -> Result<OpenResult> {
        let _opening = self.opening.lock().unwrap();
        let (vm, uuid) = (req.vm, req.vm.uuid);
        if !req.replace
            && let Some(s) = self
                .entries
                .lock()
                .unwrap()
                .get(uuid)
                .and_then(|e| e.current.as_ref())
            && s.is_live()
            && s.domain_id == req.domain_id
        {
            return Ok(match (req.device, s.device.as_deref()) {
                (Some(want), have) if have != Some(want) => OpenResult::Failed(format!(
                    "console for '{}' is open on {}, not device {want}; pass force: true to reopen it",
                    vm.name,
                    have.map_or("the default device".to_string(), |d| format!("device {d}"))
                )),
                _ => OpenResult::Attached {
                    generation: s.generation,
                    device: s.device.clone(),
                },
            });
        }
        self.replace_current(uuid, req.domain_id);

        let stream: Arc<dyn ConsoleStream> = match opener.open(req.device, req.force) {
            Ok(stream) => stream.into(),
            Err(OpenError::NoDevice(reason)) => return Ok(OpenResult::NoDevice(reason)),
            Err(OpenError::Failed(reason)) => return Ok(OpenResult::Failed(reason)),
        };
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let buffer = Arc::new(Mutex::new(Buffer::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let (stream, buffer, stop) = (stream.clone(), buffer.clone(), stop.clone());
            let cap = req.buffer_bytes;
            std::thread::Builder::new()
                .name(format!("console-{}", vm.name))
                .spawn(move || read_loop(stream.as_ref(), &buffer, &stop, cap))?
        };
        let session = Session {
            stream,
            buffer,
            stop,
            reader: Some(reader),
            aborted: false,
            device: req.device.map(str::to_string),
            generation,
            domain_id: req.domain_id,
            writing: Arc::default(),
        };
        // Opens are serialized, so nothing should be here; anything that is gets dropped after the lock.
        let raced = self
            .entries
            .lock()
            .unwrap()
            .entry(uuid.to_string())
            .or_default()
            .current
            .replace(session);
        drop(raced);
        Ok(OpenResult::Opened { generation })
    }

    /// Returns buffered output from absolute offset `since` (None = only the
    /// latest `max_bytes` bytes) of the VM's current session or previous run. A
    /// non-zero `wait` first waits up to that long for output that no earlier
    /// read returned, then until it pauses for `SETTLE`; a zero `wait` returns
    /// at once, and so does a read of an ended run. `since` above 0 must come
    /// with the `generation` of the read that returned it.
    pub fn read(
        &self,
        vm: VmId,
        run: Run,
        since: Option<u64>,
        generation: Option<u64>,
        max_bytes: usize,
        wait: Duration,
    ) -> Result<ReadResult> {
        let (buffer, current, _) = match run {
            Run::Current => self.session(vm)?,
            Run::Previous => self.previous(vm)?,
        };
        check_generation(since, generation, current, run)?;
        let deadline = Instant::now() + wait;
        let mut settle = Settle::default();
        loop {
            {
                let mut b = buffer.lock().unwrap();
                let now = Instant::now();
                if wait.is_zero()
                    || b.closed.is_some()
                    || now >= deadline
                    || settle.ready(b.end(), b.delivered, now)
                {
                    let mut r = b.slice(since, max_bytes);
                    b.delivered = b.delivered.max(r.next);
                    r.generation = current;
                    return Ok(r);
                }
            }
            std::thread::sleep(POLL);
        }
    }

    /// Sends `data` and returns the offset the guest's answer starts at, with the session's generation. That offset
    /// becomes the wait cursor, so a wait without `since` looks at the answer only. One write at a time runs per
    /// session, and the whole write ends within `budget`; pauses are scheduled from its start, so they do not drift.
    pub fn write(
        &self,
        vm: VmId,
        data: &[u8],
        pacing: Pacing,
        budget: Duration,
    ) -> Result<(u64, u64)> {
        let start = Instant::now();
        let (stream, buffer, writing, generation) = {
            let entries = self.entries.lock().unwrap();
            let s = entries
                .get(vm.uuid)
                .and_then(|e| e.current.as_ref())
                .ok_or_else(|| no_console(vm))?;
            (
                s.stream.clone(),
                s.buffer.clone(),
                s.writing.clone(),
                s.generation,
            )
        };
        let name = vm.name;
        let _writing = writing.try_lock().map_err(|_| {
            anyhow!(
                "another write to the console of '{name}' is still running; wait for it to finish"
            )
        })?;
        let offset = {
            let mut b = buffer.lock().unwrap();
            if let Some(reason) = &b.closed {
                bail!("console for '{name}' is closed: {reason}");
            }
            b.begin_write()
        };
        let (_, delay) = pacing.shape(data.len());
        let overall = start + budget;
        for (i, segment) in pacing.segments(data.len()).enumerate() {
            let due = start + delay.saturating_mul(u32::try_from(i).unwrap_or(u32::MAX));
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            let deadline = (Instant::now() + SEND_TIMEOUT).min(overall);
            send_all(stream.as_ref(), &data[segment.clone()], deadline).map_err(|(sent, e)| {
                e.context(format!(
                    "{} of {} bytes were sent; any answer starts at offset {offset} (generation {generation})",
                    segment.start + sent,
                    data.len()
                ))
            })?;
        }
        Ok((offset, generation))
    }

    /// The output from `since` (default: the wait cursor) to the end, for a wait. Marks nothing delivered, so
    /// `console_read` sees the same output afterwards. Reads the current session only.
    pub fn scan(
        &self,
        vm: VmId,
        since: Option<u64>,
        generation: Option<u64>,
    ) -> Result<SessionScan> {
        let (buffer, current, domain_id) = self.session(vm)?;
        check_generation(since, generation, current, Run::Current)?;
        let scan = buffer.lock().unwrap().scan(since);
        Ok(SessionScan {
            scan,
            generation: current,
            domain_id,
        })
    }

    /// Moves the wait cursor of session `generation` to `to`, after a wait matched just before it. The cursor never
    /// moves back, so a write made during the wait keeps its answer ahead of the cursor.
    pub fn advance_cursor(&self, uuid: &str, generation: u64, to: u64) {
        let entries = self.entries.lock().unwrap();
        if let Some(s) = entries
            .get(uuid)
            .and_then(|e| e.current.as_ref())
            .filter(|s| s.generation == generation)
        {
            let mut b = s.buffer.lock().unwrap();
            b.wait_cursor = b.wait_cursor.max(to.min(b.end()));
        }
    }

    /// The generation of the VM's current session, if it has one.
    pub fn generation(&self, vm: VmId) -> Option<u64> {
        self.session(vm).ok().map(|(_, generation, _)| generation)
    }

    /// Called once the VM is shut off, with the `generation` of the session it had before the stop. Waits up to
    /// `wait` for that session's reader to record that the stream ended, and records it as ended when it has not,
    /// so a read right after reports the console closed. Its stream may never report the end after a power-off. A
    /// session opened since, for a VM started again meanwhile, is left alone.
    pub fn note_shut_off(&self, vm: VmId, generation: Option<u64>, wait: Duration) {
        if let Ok((buffer, current, _)) = self.session(vm)
            && Some(current) == generation
        {
            await_closed(&buffer, wait, SHUT_OFF);
        }
    }

    /// True when the VM has a current session whose stream has not ended.
    pub fn is_open(&self, uuid: &str) -> bool {
        self.entries
            .lock()
            .unwrap()
            .get(uuid)
            .and_then(|e| e.current.as_ref())
            .is_some_and(|s| s.buffer.lock().unwrap().closed.is_none())
    }

    /// Ends the VM's current session for `why` and moves its output to the previous-run slot, dropping what the
    /// slot held. A reason the stream already recorded stays. False when there was no session.
    pub fn close(&self, uuid: &str, why: &str) -> bool {
        let _opening = self.opening.lock().unwrap();
        let Some(old) = self.take_current(uuid) else {
            return false;
        };
        let previous = old.retire(why);
        self.set_previous(uuid, previous);
        true
    }

    /// Drops everything kept for a VM that no longer exists: its session, its previous run and their output.
    pub fn forget(&self, uuid: &str) {
        // Bound first, so the guard is released before the session's drop joins its reader and aborts its stream.
        let _opening = self.opening.lock().unwrap();
        let gone = self.entries.lock().unwrap().remove(uuid);
        drop(gone);
    }

    /// Takes the VM's current session out, and dismantles it outside the lock: an earlier run's session moves to the
    /// previous-run slot, and one from the same run is dropped with its output.
    fn replace_current(&self, uuid: &str, domain_id: Option<u32>) {
        let Some(old) = self.take_current(uuid) else {
            return;
        };
        if old.domain_id == domain_id {
            drop(old);
        } else {
            let previous = old.retire(REPLACED_BY_NEW_RUN);
            self.set_previous(uuid, previous);
        }
    }

    fn take_current(&self, uuid: &str) -> Option<Session> {
        let mut entries = self.entries.lock().unwrap();
        let old = entries.get_mut(uuid)?.current.take();
        if entries.get(uuid).is_some_and(|e| e.previous.is_none()) {
            entries.remove(uuid);
        }
        old
    }

    fn set_previous(&self, uuid: &str, previous: Previous) {
        let older = self
            .entries
            .lock()
            .unwrap()
            .entry(uuid.to_string())
            .or_default()
            .previous
            .replace(previous);
        drop(older);
    }

    fn session(&self, vm: VmId) -> Result<(Arc<Mutex<Buffer>>, u64, Option<u32>)> {
        self.entries
            .lock()
            .unwrap()
            .get(vm.uuid)
            .and_then(|e| e.current.as_ref())
            .map(|s| (s.buffer.clone(), s.generation, s.domain_id))
            .ok_or_else(|| no_console(vm))
    }

    fn previous(&self, vm: VmId) -> Result<(Arc<Mutex<Buffer>>, u64, Option<u32>)> {
        self.entries
            .lock()
            .unwrap()
            .get(vm.uuid)
            .and_then(|e| e.previous.as_ref())
            .map(|p| (p.buffer.clone(), p.generation, None))
            .ok_or_else(|| anyhow!("no previous console run for '{}'", vm.name))
    }
}

fn no_console(vm: VmId) -> anyhow::Error {
    anyhow!("no console open for '{}'; call console_open first", vm.name)
}

/// The reason a session records when its VM is shut off and the stream has not reported its end.
const SHUT_OFF: &str = "console stream ended (VM shut down)";

/// Waits up to `wait` for `buffer` to record why its stream closed, and records `reason` when it has not.
fn await_closed(buffer: &Mutex<Buffer>, wait: Duration, reason: &str) {
    let deadline = Instant::now() + wait;
    loop {
        {
            let mut b = buffer.lock().unwrap();
            if b.closed.is_some() {
                return;
            }
            if Instant::now() >= deadline {
                b.closed = Some(reason.to_string());
                return;
            }
        }
        std::thread::sleep(POLL);
    }
}

/// Sends all of `data` on a non-blocking stream, waiting for room while the stream is full, until `deadline`. On
/// failure, returns how many bytes of `data` were sent.
fn send_all(
    stream: &dyn ConsoleStream,
    data: &[u8],
    deadline: Instant,
) -> Result<(), (usize, anyhow::Error)> {
    let mut sent = 0;
    while sent < data.len() {
        match stream.send(&data[sent..]) {
            Sent::Bytes(n) => sent += n,
            Sent::Again if Instant::now() < deadline => std::thread::sleep(POLL),
            Sent::Again => return Err((sent, anyhow!("timed out writing to console"))),
            Sent::Error(message) => {
                return Err((sent, anyhow!("console write failed: {message}")));
            }
        }
    }
    Ok(())
}

fn read_loop(stream: &dyn ConsoleStream, buffer: &Mutex<Buffer>, stop: &AtomicBool, cap: usize) {
    let mut chunk = vec![0u8; 16 * 1024];
    let reason = loop {
        if stop.load(Ordering::Relaxed) {
            break "closed by client".to_string();
        }
        match stream.recv(&mut chunk) {
            Recv::End => {
                break "console stream ended (VM shut down or console taken over)".to_string();
            }
            Recv::Again => std::thread::sleep(POLL),
            Recv::Data(n) => {
                let mut b = buffer.lock().unwrap();
                b.data.extend(&chunk[..n]);
                let excess = b.data.len().saturating_sub(cap);
                if excess > 0 {
                    b.data.drain(..excess);
                    b.start += excess as u64;
                }
            }
            Recv::Error(message) => break format!("console error: {message}"),
        }
    };
    // Keep a reason already recorded, such as a shut-off noted before the stream ended.
    buffer.lock().unwrap().closed.get_or_insert(reason);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(text: &[u8], start: u64) -> Buffer {
        Buffer {
            data: text.iter().copied().collect(),
            start,
            ..Buffer::default()
        }
    }

    #[test]
    fn a_shut_off_keeps_the_end_the_reader_records() {
        let b = Arc::new(Mutex::new(Buffer::default()));
        let reader = {
            let b = b.clone();
            std::thread::spawn(move || {
                std::thread::sleep(POLL * 2);
                b.lock()
                    .unwrap()
                    .closed
                    .get_or_insert("console stream ended (from the reader)".into());
            })
        };
        await_closed(&b, Duration::from_secs(5), SHUT_OFF);
        reader.join().unwrap();
        assert_eq!(
            b.lock().unwrap().closed.as_deref(),
            Some("console stream ended (from the reader)")
        );
    }

    #[test]
    fn a_shut_off_records_the_end_when_the_stream_never_reports_it() {
        let b = Mutex::new(Buffer::default());
        let start = Instant::now();
        await_closed(&b, POLL * 3, SHUT_OFF);
        assert!(start.elapsed() >= POLL * 3);
        assert_eq!(b.lock().unwrap().closed.as_deref(), Some(SHUT_OFF));
    }

    #[test]
    fn settle_waits_for_output_beyond_delivered() {
        let t0 = Instant::now();
        let mut s = Settle::default();
        // Everything was already returned: keep waiting, however long it is quiet.
        assert!(!s.ready(22, 22, t0));
        assert!(!s.ready(22, 22, t0 + SETTLE * 4));
    }

    #[test]
    fn settle_returns_once_new_output_pauses() {
        let t0 = Instant::now();
        let mut s = Settle::default();
        assert!(!s.ready(30, 22, t0));
        // More output arrives, which restarts the pause.
        assert!(!s.ready(40, 22, t0 + SETTLE / 2));
        assert!(!s.ready(40, 22, t0 + SETTLE));
        assert!(s.ready(40, 22, t0 + SETTLE / 2 + SETTLE));
    }

    #[test]
    fn slice_rereads_from_an_old_offset() {
        let b = buffer(b"login: root\r\nPassword: ", 0);
        let r = b.slice(Some(0), 1024);
        assert_eq!(r.data, b"login: root\r\nPassword: ");
        assert_eq!((r.next, r.dropped), (23, 0));
    }

    #[test]
    fn slice_reports_bytes_dropped_from_scrollback() {
        let b = buffer(b"Password: ", 13);
        let r = b.slice(Some(5), 4);
        assert_eq!(r.data, b"Pass");
        assert_eq!((r.next, r.dropped), (17, 8));
    }

    #[test]
    fn generations_differ_between_process_starts() {
        assert!(first_generation(1_000_001) > first_generation(1_000_000));
        let consoles = Consoles::default();
        let a = consoles.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let b = consoles.generation.fetch_add(1, Ordering::Relaxed) + 1;
        assert!(b > a, "each new session takes a higher generation");
    }

    #[test]
    fn stale_or_missing_generations_are_refused() {
        assert!(check_generation(None, None, 7, Run::Current).is_ok());
        assert!(
            check_generation(Some(0), None, 7, Run::Current).is_ok(),
            "since 0 needs none"
        );
        assert!(check_generation(Some(40), Some(7), 7, Run::Current).is_ok());
        let stale = check_generation(Some(40), Some(6), 7, Run::Current)
            .unwrap_err()
            .to_string();
        assert!(stale.contains("earlier console session"), "{stale}");
        let missing = check_generation(Some(40), None, 7, Run::Current)
            .unwrap_err()
            .to_string();
        assert!(missing.contains("needs the generation"), "{missing}");
        assert!(
            check_generation(Some(0), Some(6), 7, Run::Current).is_err(),
            "a named stale generation is refused even at 0"
        );
    }

    #[test]
    fn a_write_moves_the_wait_cursor_to_the_end() {
        let mut b = buffer(b"login: ", 0);
        assert_eq!(b.begin_write(), 7);
        b.data.extend(b"root\r\nPassword: ");
        let s = b.scan(None);
        assert_eq!(s.from, 7);
        assert_eq!(s.data, b"root\r\nPassword: ");
    }

    #[test]
    fn scan_marks_nothing_delivered() {
        let b = buffer(b"login: ", 0);
        let s = b.scan(Some(0));
        assert_eq!(s.data, b"login: ");
        assert_eq!(
            b.delivered, 0,
            "a waiting read afterwards still waits for new output"
        );
    }

    #[test]
    fn scan_starts_at_the_scrollback_when_output_was_dropped() {
        let b = buffer(b"Password: ", 13);
        let s = b.scan(Some(5));
        assert_eq!(s.from, 13);
        assert_eq!(s.data, b"Password: ");
    }

    const MAX: Duration = Duration::from_secs(50);

    #[test]
    fn pacing_splits_writes_into_segments() {
        let ms = Duration::from_millis;
        let all = |p: Pacing, len| p.segments(len).collect::<Vec<_>>();
        let one = all(Pacing::None, 5);
        assert_eq!((one.len(), one[0].clone()), (1, 0..5));
        assert_eq!(all(Pacing::PerByte(ms(8)), 3), vec![0..1, 1..2, 2..3]);
        let chunks = Pacing::Chunks {
            size: 4,
            delay: ms(20),
        };
        assert_eq!(all(chunks, 10), vec![0..4, 4..8, 8..10]);
        assert_eq!(chunks.shape(10), (4, ms(20)));
        assert!(all(Pacing::None, 0).is_empty());
    }

    #[test]
    fn pacing_duration_counts_the_pauses_between_segments() {
        let ms = Duration::from_millis;
        assert_eq!(Pacing::PerByte(ms(10)).duration(101), ms(1000));
        assert_eq!(
            Pacing::Chunks {
                size: 4,
                delay: ms(20)
            }
            .duration(10),
            ms(40)
        );
        assert_eq!(Pacing::None.duration(100_000), Duration::ZERO);
    }

    #[test]
    fn pacing_options_are_checked() {
        let opts = |c, s, d, len| Pacing::from_options(c, s, d, len, MAX);
        assert_eq!(opts(None, None, None, 10).unwrap(), Pacing::None);
        assert_eq!(
            opts(Some(8), None, None, 10).unwrap(),
            Pacing::PerByte(Duration::from_millis(8))
        );
        assert!(
            opts(Some(1001), None, None, 10).is_err(),
            "delay above 1000 ms"
        );
        assert!(opts(Some(8), Some(4), Some(20), 10).is_err(), "both modes");
        assert!(
            opts(None, Some(4), None, 10).is_err(),
            "chunk without delay"
        );
        assert!(
            opts(None, None, Some(20), 10).is_err(),
            "delay without chunk"
        );
        assert!(opts(None, Some(0), Some(20), 10).is_err(), "empty chunk");
        // 1000 ms after each of 51 bytes is 50 s of pauses: exactly the cap passes, one more byte does not.
        assert!(opts(Some(1000), None, None, 51).is_ok());
        let err = opts(Some(1000), None, None, 52).unwrap_err().to_string();
        assert!(err.contains("max_wait_secs"), "{err}");
    }

    #[test]
    fn slice_without_since_returns_the_tail() {
        let b = buffer(b"login: root\r\nPassword: ", 0);
        let r = b.slice(None, 10);
        assert_eq!(r.data, b"Password: ");
        assert_eq!(r.next, 23);
    }

    // Sessions, runs and the previous-run slot, driven through a fake opener and streams.

    #[derive(Default)]
    struct FakeInner {
        incoming: Mutex<VecDeque<Vec<u8>>>,
        ended: AtomicBool,
        aborted: AtomicBool,
    }

    #[derive(Clone, Default)]
    struct FakeStream(Arc<FakeInner>);

    impl FakeStream {
        fn say(&self, text: &[u8]) {
            self.0.incoming.lock().unwrap().push_back(text.to_vec());
        }

        fn end(&self) {
            self.0.ended.store(true, Ordering::SeqCst);
        }

        fn aborted(&self) -> bool {
            self.0.aborted.load(Ordering::SeqCst)
        }
    }

    impl ConsoleStream for FakeStream {
        fn recv(&self, buf: &mut [u8]) -> Recv {
            if let Some(chunk) = self.0.incoming.lock().unwrap().pop_front() {
                buf[..chunk.len()].copy_from_slice(&chunk);
                Recv::Data(chunk.len())
            } else if self.0.ended.load(Ordering::SeqCst) {
                Recv::End
            } else {
                Recv::Again
            }
        }

        fn send(&self, data: &[u8]) -> Sent {
            Sent::Bytes(data.len())
        }

        fn abort(&self) {
            self.0.aborted.store(true, Ordering::SeqCst);
        }
    }

    /// Records each open's device and force flag, and hands out the streams it made.
    #[derive(Default)]
    struct FakeOpener {
        calls: Mutex<Vec<(Option<String>, bool)>>,
        streams: Mutex<Vec<FakeStream>>,
        fail: Mutex<Option<OpenError>>,
    }

    impl FakeOpener {
        fn stream(&self, n: usize) -> FakeStream {
            self.streams.lock().unwrap()[n].clone()
        }

        fn flags(&self) -> Vec<bool> {
            self.calls.lock().unwrap().iter().map(|c| c.1).collect()
        }
    }

    impl ConsoleOpener for FakeOpener {
        fn open(
            &self,
            device: Option<&str>,
            force: bool,
        ) -> Result<Box<dyn ConsoleStream>, OpenError> {
            self.calls
                .lock()
                .unwrap()
                .push((device.map(str::to_string), force));
            if let Some(e) = self.fail.lock().unwrap().take() {
                return Err(e);
            }
            let stream = FakeStream::default();
            self.streams.lock().unwrap().push(stream.clone());
            Ok(Box::new(stream))
        }
    }

    const UUID: &str = "6b9f7a52-1c2d-4e55-9a10-0123456789ab";

    fn vm() -> VmId<'static> {
        VmId {
            uuid: UUID,
            name: "t-vm",
        }
    }

    fn open_with(
        c: &Consoles,
        o: &FakeOpener,
        domain_id: u32,
        device: Option<&'static str>,
        force: bool,
        replace: bool,
    ) -> OpenResult {
        c.open(
            o,
            OpenRequest {
                vm: vm(),
                domain_id: Some(domain_id),
                device,
                force,
                replace,
                buffer_bytes: 1024,
            },
        )
        .unwrap()
    }

    fn open(c: &Consoles, o: &FakeOpener, domain_id: u32) -> OpenResult {
        open_with(c, o, domain_id, None, false, false)
    }

    fn generation_of(result: OpenResult) -> u64 {
        match result {
            OpenResult::Opened { generation } | OpenResult::Attached { generation, .. } => {
                generation
            }
            other => panic!("expected an open session, got {other:?}"),
        }
    }

    fn read_all(c: &Consoles, run: Run) -> Result<ReadResult> {
        c.read(vm(), run, Some(0), None, 1024, Duration::ZERO)
    }

    /// Waits until the reader thread has stored `text` as the whole output of `run`.
    fn await_output(c: &Consoles, run: Run, text: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while read_all(c, run).is_ok_and(|r| r.data != text) {
            assert!(
                Instant::now() < deadline,
                "the reader never stored {text:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Waits until the reader thread has recorded that the stream ended.
    fn await_end(c: &Consoles) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while read_all(c, Run::Current).is_ok_and(|r| r.closed.is_none()) {
            assert!(Instant::now() < deadline, "the reader never saw the end");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_new_domain_id_moves_the_old_run_to_the_previous_slot() {
        let (c, o) = (Consoles::default(), FakeOpener::default());
        let first = generation_of(open(&c, &o, 10));
        o.stream(0).say(b"run one\n");
        await_output(&c, Run::Current, b"run one\n");

        let second = generation_of(open(&c, &o, 11));
        assert!(second > first);
        assert!(o.stream(0).aborted(), "the old stream is released");
        let previous = read_all(&c, Run::Previous).unwrap();
        assert_eq!(previous.data, b"run one\n");
        assert_eq!(previous.generation, first);
        assert_eq!(previous.closed.as_deref(), Some("replaced by a new run"));
        let current = read_all(&c, Run::Current).unwrap();
        assert_eq!((current.data.len(), current.generation), (0, second));
        assert_eq!(current.closed, None);
    }

    #[test]
    fn a_force_or_a_reopen_over_a_closed_session_in_the_same_run_drops_it() {
        let (c, o) = (Consoles::default(), FakeOpener::default());
        open(&c, &o, 10);
        o.stream(0).say(b"old\n");
        await_output(&c, Run::Current, b"old\n");
        // A force replaces the live session and drops its output; the run is the same, so nothing moves.
        let forced = open_with(&c, &o, 10, None, true, true);
        assert!(matches!(forced, OpenResult::Opened { .. }));
        assert!(read_all(&c, Run::Previous).is_err());
        assert_eq!(read_all(&c, Run::Current).unwrap().data, b"");

        // A session whose stream ended is replaced by a plain open of the same run, and dropped just the same.
        o.stream(1).say(b"newer\n");
        await_output(&c, Run::Current, b"newer\n");
        o.stream(1).end();
        await_end(&c);
        let reopened = open(&c, &o, 10);
        assert!(matches!(reopened, OpenResult::Opened { .. }));
        assert!(read_all(&c, Run::Previous).is_err());
        assert_eq!(read_all(&c, Run::Current).unwrap().data, b"");
    }

    #[test]
    fn an_open_over_a_live_session_of_the_same_run_attaches() {
        let (c, o) = (Consoles::default(), FakeOpener::default());
        let first = generation_of(open_with(&c, &o, 10, Some("serial0"), false, false));
        o.stream(0).say(b"kept\n");
        await_output(&c, Run::Current, b"kept\n");

        let again = open_with(&c, &o, 10, None, false, false);
        assert_eq!(
            again,
            OpenResult::Attached {
                generation: first,
                device: Some("serial0".into())
            }
        );
        assert_eq!(o.calls.lock().unwrap().len(), 1, "no second open");
        assert_eq!(read_all(&c, Run::Current).unwrap().data, b"kept\n");

        let other = open_with(&c, &o, 10, Some("console1"), false, false);
        let OpenResult::Failed(reason) = other else {
            panic!("a session on another device is not attached to");
        };
        assert!(
            reason.contains("serial0") && reason.contains("console1"),
            "{reason}"
        );
        assert_eq!(o.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_named_device_is_not_attached_to_a_session_on_the_default_one() {
        let (c, o) = (Consoles::default(), FakeOpener::default());
        open_with(&c, &o, 10, None, false, false);
        let OpenResult::Failed(reason) = open_with(&c, &o, 10, Some("console1"), false, false)
        else {
            panic!("a session on the default device is not attached to for console1");
        };
        assert!(
            reason.contains("the default device") && reason.contains("console1"),
            "{reason}"
        );
        assert_eq!(o.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn the_opener_gets_libvirts_force_flag_apart_from_replacing_a_session() {
        let (c, o) = (Consoles::default(), FakeOpener::default());
        open(&c, &o, 10);
        // The server replaces its own live session without taking the console over from anyone.
        open_with(&c, &o, 10, None, false, true);
        // A caller's force does take it over.
        open_with(&c, &o, 10, None, true, true);
        assert_eq!(o.flags(), [false, false, true]);
    }

    #[test]
    fn close_moves_the_session_and_keeps_the_reason_the_stream_recorded() {
        let (c, o) = (Consoles::default(), FakeOpener::default());
        let first = generation_of(open(&c, &o, 10));
        o.stream(0).say(b"before close\n");
        await_output(&c, Run::Current, b"before close\n");
        assert!(c.close(UUID, CLOSED_BY_CONSOLE_CLOSE));
        assert!(!c.is_open(UUID));
        assert!(read_all(&c, Run::Current).is_err());
        let previous = read_all(&c, Run::Previous).unwrap();
        assert_eq!(previous.data, b"before close\n");
        assert_eq!(previous.generation, first);
        assert_eq!(previous.closed.as_deref(), Some("closed by console_close"));
        assert!(
            !c.close(UUID, CLOSED_BY_CONSOLE_CLOSE),
            "nothing left to close"
        );

        // A session whose stream ended on its own keeps that reason when it moves.
        open(&c, &o, 10);
        o.stream(1).end();
        await_end(&c);
        assert!(c.close(UUID, CLOSED_BY_CONSOLE_CLOSE));
        let reason = read_all(&c, Run::Previous).unwrap().closed.unwrap();
        assert!(reason.starts_with("console stream ended"), "{reason}");
    }

    #[test]
    fn a_second_move_drops_the_older_slot_content() {
        let (c, o) = (Consoles::default(), FakeOpener::default());
        open(&c, &o, 10);
        o.stream(0).say(b"run A\n");
        await_output(&c, Run::Current, b"run A\n");
        let second = generation_of(open(&c, &o, 11));
        o.stream(1).say(b"run B\n");
        await_output(&c, Run::Current, b"run B\n");
        open(&c, &o, 12);

        let previous = read_all(&c, Run::Previous).unwrap();
        assert_eq!(previous.data, b"run B\n", "run A is gone");
        assert_eq!(previous.generation, second);
    }

    #[test]
    fn the_previous_run_keeps_its_own_generation_and_offsets() {
        let (c, o) = (Consoles::default(), FakeOpener::default());
        let first = generation_of(open(&c, &o, 10));
        o.stream(0).say(b"abcdef");
        await_output(&c, Run::Current, b"abcdef");
        let second = generation_of(open(&c, &o, 11));
        o.stream(1).say(b"xy");
        await_output(&c, Run::Current, b"xy");

        let read =
            |run, since, generation| c.read(vm(), run, since, generation, 1024, Duration::ZERO);
        let tail = read(Run::Previous, Some(3), Some(first)).unwrap();
        assert_eq!((tail.data.as_slice(), tail.next), (&b"def"[..], 6));
        let current = read(Run::Current, Some(1), Some(second)).unwrap();
        assert_eq!((current.data.as_slice(), current.next), (&b"y"[..], 2));
        let stale = read(Run::Previous, Some(3), Some(second))
            .unwrap_err()
            .to_string();
        assert!(stale.contains("the previous run is generation"), "{stale}");
        let bare = read(Run::Previous, Some(3), None).unwrap_err().to_string();
        assert!(bare.contains("needs the generation"), "{bare}");
    }

    #[test]
    fn an_open_that_fails_still_moves_the_earlier_run() {
        let (c, o) = (Consoles::default(), FakeOpener::default());
        open(&c, &o, 10);
        o.stream(0).say(b"crash output\n");
        await_output(&c, Run::Current, b"crash output\n");

        *o.fail.lock().unwrap() = Some(OpenError::Failed("console in use".into()));
        let failed = open(&c, &o, 11);
        assert_eq!(failed, OpenResult::Failed("console in use".into()));
        assert!(
            read_all(&c, Run::Current).is_err(),
            "no old run reads as current"
        );
        assert_eq!(read_all(&c, Run::Previous).unwrap().data, b"crash output\n");

        *o.fail.lock().unwrap() = Some(OpenError::NoDevice(
            "cannot find character device <null>".into(),
        ));
        let none = open(&c, &o, 12);
        assert!(matches!(none, OpenResult::NoDevice(_)), "{none:?}");
        let message = none.into_message("t-vm").unwrap_err().to_string();
        assert!(message.contains("no console device"), "{message}");
    }

    #[test]
    fn sessions_are_keyed_by_uuid_and_forgotten_with_their_vm() {
        let (c, o) = (Consoles::default(), FakeOpener::default());
        open(&c, &o, 10);
        open(&c, &o, 11);
        let same_name = VmId {
            uuid: "another-uuid",
            name: "t-vm",
        };
        let read = |run| c.read(same_name, run, None, None, 16, Duration::ZERO);
        assert!(
            read(Run::Current).is_err(),
            "a VM that reuses the name does not see the session"
        );
        assert!(read(Run::Previous).is_err());

        c.forget(UUID);
        assert!(read_all(&c, Run::Current).is_err());
        assert!(read_all(&c, Run::Previous).is_err());
        assert!(c.entries.lock().unwrap().is_empty());
        assert!(o.stream(1).aborted());
    }

    #[test]
    fn an_open_result_becomes_a_reply_or_an_error() {
        let attached = OpenResult::Attached {
            generation: 7,
            device: None,
        };
        let text = attached.into_message("t-vm").unwrap();
        assert!(
            text.contains("already open") && text.contains("generation 7"),
            "{text}"
        );
        let opened = OpenResult::Opened { generation: 8 }
            .into_message("t-vm")
            .unwrap();
        assert!(opened.contains("opened (generation 8)"), "{opened}");
        let failed = OpenResult::Failed("busy".into())
            .into_message("t-vm")
            .unwrap_err();
        assert!(
            failed.to_string().contains("cannot open console: busy"),
            "{failed}"
        );
    }

    #[test]
    fn run_accepts_current_and_previous_only() {
        let run = |text: &str| serde_json::from_str::<Run>(text);
        assert_eq!(run("\"previous\"").unwrap(), Run::Previous);
        assert_eq!(run("\"current\"").unwrap(), Run::Current);
        assert!(run("\"older\"").is_err());
        assert_eq!(Run::default(), Run::Current);
    }
}
