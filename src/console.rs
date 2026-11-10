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
use virt::domain::Domain;
use virt::stream::Stream;
use virt::sys;

use crate::hypervisor::Hypervisor;

const POLL: Duration = Duration::from_millis(30);

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
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "wait_for uses it from run 1 unit 4")
    )]
    fn scan(&self, since: Option<u64>) -> ScanResult {
        let from = since.unwrap_or(self.wait_cursor).min(self.end());
        let dropped = self.start.saturating_sub(from);
        let from = from.max(self.start);
        let skip = usize::try_from(from - self.start).expect("the buffer fits in memory");
        ScanResult {
            data: self.data.iter().skip(skip).copied().collect(),
            from,
            dropped,
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

struct Session {
    stream: Arc<Stream>,
    buffer: Arc<Mutex<Buffer>>,
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    device: Option<String>,
    generation: u64,
    /// The running domain's ID when the session opened; a stop and start gives the domain a new one.
    domain_id: Option<u32>,
}

/// A session's output from an offset, read for a wait without marking it delivered.
#[expect(dead_code, reason = "wait_for uses it from run 1 unit 4")]
pub struct ScanResult {
    pub data: Vec<u8>,
    /// Offset of `data[0]`.
    pub from: u64,
    /// Bytes from the requested offset that were already dropped from the scrollback.
    pub dropped: u64,
    pub closed: Option<String>,
}

/// A scan with the session it came from.
#[expect(dead_code, reason = "wait_for uses it from run 1 unit 4")]
pub struct SessionScan {
    pub scan: ScanResult,
    pub generation: u64,
    pub domain_id: Option<u32>,
}

/// The first generation of a server process started at `now_secs`, so two processes rarely share numbers.
fn first_generation(now_secs: u64) -> u64 {
    now_secs.saturating_mul(1000)
}

/// Checks that an offset belongs to the session with `current` generation. Offset 0 and an omitted offset need no
/// generation; any other offset must name the generation of the read that returned it.
fn check_generation(since: Option<u64>, given: Option<u64>, current: u64) -> Result<()> {
    match (since, given) {
        (_, Some(g)) if g != current => bail!(
            "offset from an earlier console session (generation {g}; the open session is generation {current}); \
             read from since 0 with generation {current}"
        ),
        (Some(s), None) if s > 0 => bail!(
            "since {s} needs the generation of the read that returned it; the open session is generation {current}"
        ),
        _ => Ok(()),
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
        unsafe { sys::virStreamAbort(self.stream.as_ptr()) };
    }
}

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

pub struct Consoles {
    sessions: Mutex<HashMap<String, Session>>,
    /// The last generation handed out.
    generation: AtomicU64,
}

impl Default for Consoles {
    fn default() -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        Self {
            sessions: Mutex::default(),
            generation: AtomicU64::new(first_generation(now)),
        }
    }
}

impl Consoles {
    pub fn open(
        &self,
        hv: &Hypervisor,
        vm: &str,
        dom: &Domain,
        device: Option<&str>,
        force: bool,
    ) -> Result<String> {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(s) = sessions.get(vm) {
            // A session counts as live while its reader runs and has not recorded an end. With
            // `force`, even a live one is replaced: after a power-off its stream may never end.
            let live = s.buffer.lock().unwrap().closed.is_none()
                && s.reader.as_ref().is_some_and(|h| !h.is_finished());
            if live && !force {
                return Ok(format!(
                    "console for '{vm}' is already open (device: {}); pass force: true to reopen it",
                    s.device.as_deref().unwrap_or("default")
                ));
            }
            sessions.remove(vm);
        }

        let stream = hv.with_conn(|conn| Ok(Stream::new(conn, sys::VIR_STREAM_NONBLOCK)?))?;
        let flags = if force {
            sys::VIR_DOMAIN_CONSOLE_FORCE
        } else {
            sys::VIR_DOMAIN_CONSOLE_SAFE
        };
        dom.open_console(device, &stream, flags).map_err(|e| {
            anyhow!(
                "cannot open console: {} (it may be in use, e.g. by virt-manager; retry with force=true to take it over)",
                e.message()
            )
        })?;

        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let stream = Arc::new(stream);
        let buffer = Arc::new(Mutex::new(Buffer::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let cap = hv.config.limits.console_buffer_bytes;
        let reader = {
            let (stream, buffer, stop) = (stream.clone(), buffer.clone(), stop.clone());
            std::thread::Builder::new()
                .name(format!("console-{vm}"))
                .spawn(move || read_loop(&stream, &buffer, &stop, cap))?
        };
        sessions.insert(
            vm.to_string(),
            Session {
                stream,
                buffer,
                stop,
                reader: Some(reader),
                device: device.map(str::to_string),
                generation,
                domain_id: dom.get_id(),
            },
        );
        Ok(format!(
            "console for '{vm}' opened (generation {generation}); output is buffered from now on"
        ))
    }

    /// Returns buffered output from absolute offset `since` (None = only the
    /// latest `max_bytes` bytes). A non-zero `wait` first waits up to that long
    /// for output that no earlier read returned, then until it pauses for
    /// `SETTLE`; a zero `wait` returns at once. `since` above 0 must come with
    /// the `generation` of the read that returned it.
    pub fn read(
        &self,
        vm: &str,
        since: Option<u64>,
        generation: Option<u64>,
        max_bytes: usize,
        wait: Duration,
    ) -> Result<ReadResult> {
        let (buffer, current, _) = self.session(vm)?;
        check_generation(since, generation, current)?;
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
    /// becomes the wait cursor, so a wait without `since` looks at the answer only.
    pub fn write(&self, vm: &str, data: &[u8]) -> Result<(u64, u64)> {
        let (stream, offset, generation) = {
            let sessions = self.sessions.lock().unwrap();
            let s = sessions
                .get(vm)
                .ok_or_else(|| anyhow!("no console open for '{vm}'; call console_open first"))?;
            let mut b = s.buffer.lock().unwrap();
            if let Some(reason) = &b.closed {
                bail!("console for '{vm}' is closed: {reason}");
            }
            (s.stream.clone(), b.begin_write(), s.generation)
        };
        let mut sent = 0;
        let deadline = Instant::now() + Duration::from_secs(10);
        while sent < data.len() {
            let rest = &data[sent..];
            let n = unsafe {
                sys::virStreamSend(stream.as_ptr(), rest.as_ptr() as *const _, rest.len())
            };
            match n {
                n if n >= 0 => sent += n as usize,
                -2 if Instant::now() < deadline => std::thread::sleep(POLL),
                -2 => bail!("timed out writing to console"),
                _ => bail!(
                    "console write failed: {}",
                    virt::error::Error::last_error().message()
                ),
            }
        }
        Ok((offset, generation))
    }

    /// The output from `since` (default: the wait cursor) to the end, for a wait. Marks nothing delivered, so
    /// `console_read` sees the same output afterwards.
    #[expect(dead_code, reason = "wait_for uses it from run 1 unit 4")]
    pub fn scan(
        &self,
        vm: &str,
        since: Option<u64>,
        generation: Option<u64>,
    ) -> Result<SessionScan> {
        let (buffer, current, domain_id) = self.session(vm)?;
        check_generation(since, generation, current)?;
        let scan = buffer.lock().unwrap().scan(since);
        Ok(SessionScan {
            scan,
            generation: current,
            domain_id,
        })
    }

    pub fn is_open(&self, vm: &str) -> bool {
        self.sessions
            .lock()
            .unwrap()
            .get(vm)
            .is_some_and(|s| s.buffer.lock().unwrap().closed.is_none())
    }

    pub fn close(&self, vm: &str) -> bool {
        self.sessions.lock().unwrap().remove(vm).is_some()
    }

    fn session(&self, vm: &str) -> Result<(Arc<Mutex<Buffer>>, u64, Option<u32>)> {
        self.sessions
            .lock()
            .unwrap()
            .get(vm)
            .map(|s| (s.buffer.clone(), s.generation, s.domain_id))
            .ok_or_else(|| anyhow!("no console open for '{vm}'; call console_open first"))
    }
}

fn read_loop(stream: &Stream, buffer: &Mutex<Buffer>, stop: &AtomicBool, cap: usize) {
    let mut chunk = vec![0u8; 16 * 1024];
    let reason = loop {
        if stop.load(Ordering::Relaxed) {
            break "closed by client".to_string();
        }
        let n = unsafe {
            sys::virStreamRecv(stream.as_ptr(), chunk.as_mut_ptr() as *mut _, chunk.len())
        };
        match n {
            0 => break "console stream ended (VM shut down or console taken over)".to_string(),
            -2 => std::thread::sleep(POLL),
            n if n > 0 => {
                let mut b = buffer.lock().unwrap();
                b.data.extend(&chunk[..n as usize]);
                let excess = b.data.len().saturating_sub(cap);
                if excess > 0 {
                    b.data.drain(..excess);
                    b.start += excess as u64;
                }
            }
            _ => {
                break format!(
                    "console error: {}",
                    virt::error::Error::last_error().message()
                );
            }
        }
    };
    buffer.lock().unwrap().closed = Some(reason);
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
        assert!(check_generation(None, None, 7).is_ok());
        assert!(
            check_generation(Some(0), None, 7).is_ok(),
            "since 0 needs none"
        );
        assert!(check_generation(Some(40), Some(7), 7).is_ok());
        let stale = check_generation(Some(40), Some(6), 7)
            .unwrap_err()
            .to_string();
        assert!(stale.contains("earlier console session"), "{stale}");
        let missing = check_generation(Some(40), None, 7).unwrap_err().to_string();
        assert!(missing.contains("needs the generation"), "{missing}");
        assert!(
            check_generation(Some(0), Some(6), 7).is_err(),
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
    fn scan_reports_bytes_dropped_from_scrollback() {
        let b = buffer(b"Password: ", 13);
        let s = b.scan(Some(5));
        assert_eq!((s.from, s.dropped), (13, 8));
        assert_eq!(s.data, b"Password: ");
    }

    #[test]
    fn slice_without_since_returns_the_tail() {
        let b = buffer(b"login: root\r\nPassword: ", 0);
        let r = b.slice(None, 10);
        assert_eq!(r.data, b"Password: ");
        assert_eq!(r.next, 23);
    }
}
