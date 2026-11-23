//! The per-VM event log: what happened to each visible VM, who caused it, and which records an agent has yet to be
//! told about. Plain data only: the callbacks and the worker that feed it live elsewhere, and nothing here calls
//! libvirt. Times are epoch seconds, passed in by the caller so tests can drive them.
#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the event worker and the event tools of later units use it"
    )
)]

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use serde::Serialize;
use serde_json::{Value, json};
use virt::sys;

/// How long after a tool returns its expectation still claims a matching record.
pub const EXPECTATION_WINDOW_SECS: u64 = 5;

/// A second `reboot` event within this many seconds of the first is the same reboot: libvirt sends a guest reboot
/// twice, a few milliseconds apart.
const REBOOT_REPEAT_SECS: u64 = 1;

/// What a record says happened.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Started,
    Paused,
    Resumed,
    Shutdown,
    Stopped,
    Crashed,
    Pmsuspended,
    Rebooted,
    Watchdog,
    AgentConnected,
    AgentDisconnected,
    Gap,
}

/// Who caused a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// A tool call of this server, by the tool's name.
    Tool(&'static str),
    /// The server's expiry sweep.
    Expiry,
    /// Anything else: the guest, another program, or a tool whose window had closed.
    Outside,
}

impl Cause {
    fn name(self) -> &'static str {
        match self {
            Self::Tool(name) => name,
            Self::Expiry => "expiry",
            Self::Outside => "outside",
        }
    }
}

/// One libvirt event as a callback copied it.
#[derive(Debug, Clone)]
pub struct Raw {
    pub uuid: String,
    pub name: String,
    pub source: Source,
    pub time: u64,
}

/// The libvirt event ID and its numbers.
#[derive(Debug, Clone, Copy)]
pub enum Source {
    Lifecycle { event: i32, detail: i32 },
    Reboot,
    Watchdog { action: i32 },
    Agent { state: i32, reason: i32 },
}

/// A record in a VM's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub seq: u64,
    pub time: u64,
    pub kind: Kind,
    pub detail: String,
    pub cause: Cause,
    /// For a crash: the VM was also stopped, as under the `destroy` crash action.
    pub stopped: bool,
    /// For a gap: the times between which events may be missing.
    pub gap: Option<(u64, u64)>,
}

impl Record {
    /// Whether an agent is told about this record in the next reply that names the VM.
    pub fn has_notice(&self) -> bool {
        if matches!(self.cause, Cause::Tool(_)) {
            return false;
        }
        match self.kind {
            Kind::Stopped | Kind::Crashed | Kind::Rebooted | Kind::Watchdog | Kind::Gap => true,
            Kind::Paused => matches!(self.detail.as_str(), "ioerror" | "watchdog" | "api_error"),
            _ => false,
        }
    }

    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "seq": self.seq,
            "time": format_utc(self.time),
            "kind": self.kind,
            "detail": self.detail,
            "cause": self.cause.name(),
        });
        if self.kind == Kind::Crashed {
            v["stopped"] = json!(self.stopped);
        }
        if let Some((from, to)) = self.gap {
            v["from"] = json!(format_utc(from));
            v["to"] = json!(format_utc(to));
        }
        v
    }
}

/// What libvirt's numbers mean for the log.
#[derive(Debug, PartialEq, Eq)]
enum Mapped {
    Record(Kind, String),
    /// A stop because the guest crashed: merged into the crash it follows.
    CrashStop,
    /// The VM's definition was removed.
    Undefined,
    /// Nothing to record, such as a definition change.
    Nothing,
}

fn lifecycle_detail(event: u32, detail: u32) -> &'static str {
    match (event, detail) {
        (sys::VIR_DOMAIN_EVENT_STARTED, sys::VIR_DOMAIN_EVENT_STARTED_BOOTED) => "booted",
        (sys::VIR_DOMAIN_EVENT_STARTED, sys::VIR_DOMAIN_EVENT_STARTED_MIGRATED) => "migrated",
        (sys::VIR_DOMAIN_EVENT_STARTED, sys::VIR_DOMAIN_EVENT_STARTED_RESTORED) => "restored",
        (sys::VIR_DOMAIN_EVENT_STARTED, sys::VIR_DOMAIN_EVENT_STARTED_FROM_SNAPSHOT) => {
            "from_snapshot"
        }
        (sys::VIR_DOMAIN_EVENT_STARTED, sys::VIR_DOMAIN_EVENT_STARTED_WAKEUP) => "wakeup",
        (sys::VIR_DOMAIN_EVENT_SUSPENDED, sys::VIR_DOMAIN_EVENT_SUSPENDED_PAUSED) => "paused",
        (sys::VIR_DOMAIN_EVENT_SUSPENDED, sys::VIR_DOMAIN_EVENT_SUSPENDED_MIGRATED) => "migrated",
        (sys::VIR_DOMAIN_EVENT_SUSPENDED, sys::VIR_DOMAIN_EVENT_SUSPENDED_IOERROR) => "ioerror",
        (sys::VIR_DOMAIN_EVENT_SUSPENDED, sys::VIR_DOMAIN_EVENT_SUSPENDED_WATCHDOG) => "watchdog",
        (sys::VIR_DOMAIN_EVENT_SUSPENDED, sys::VIR_DOMAIN_EVENT_SUSPENDED_RESTORED) => "restored",
        (sys::VIR_DOMAIN_EVENT_SUSPENDED, sys::VIR_DOMAIN_EVENT_SUSPENDED_FROM_SNAPSHOT) => {
            "from_snapshot"
        }
        (sys::VIR_DOMAIN_EVENT_SUSPENDED, sys::VIR_DOMAIN_EVENT_SUSPENDED_API_ERROR) => "api_error",
        (sys::VIR_DOMAIN_EVENT_SUSPENDED, sys::VIR_DOMAIN_EVENT_SUSPENDED_POSTCOPY) => "postcopy",
        (sys::VIR_DOMAIN_EVENT_SUSPENDED, sys::VIR_DOMAIN_EVENT_SUSPENDED_POSTCOPY_FAILED) => {
            "postcopy_failed"
        }
        (sys::VIR_DOMAIN_EVENT_RESUMED, sys::VIR_DOMAIN_EVENT_RESUMED_UNPAUSED) => "unpaused",
        (sys::VIR_DOMAIN_EVENT_RESUMED, sys::VIR_DOMAIN_EVENT_RESUMED_MIGRATED) => "migrated",
        (sys::VIR_DOMAIN_EVENT_RESUMED, sys::VIR_DOMAIN_EVENT_RESUMED_FROM_SNAPSHOT) => {
            "from_snapshot"
        }
        (sys::VIR_DOMAIN_EVENT_RESUMED, sys::VIR_DOMAIN_EVENT_RESUMED_POSTCOPY) => "postcopy",
        (sys::VIR_DOMAIN_EVENT_RESUMED, sys::VIR_DOMAIN_EVENT_RESUMED_POSTCOPY_FAILED) => {
            "postcopy_failed"
        }
        (sys::VIR_DOMAIN_EVENT_STOPPED, sys::VIR_DOMAIN_EVENT_STOPPED_SHUTDOWN) => "shutdown",
        (sys::VIR_DOMAIN_EVENT_STOPPED, sys::VIR_DOMAIN_EVENT_STOPPED_DESTROYED) => "destroyed",
        (sys::VIR_DOMAIN_EVENT_STOPPED, sys::VIR_DOMAIN_EVENT_STOPPED_MIGRATED) => "migrated",
        (sys::VIR_DOMAIN_EVENT_STOPPED, sys::VIR_DOMAIN_EVENT_STOPPED_SAVED) => "saved",
        (sys::VIR_DOMAIN_EVENT_STOPPED, sys::VIR_DOMAIN_EVENT_STOPPED_FAILED) => "failed",
        (sys::VIR_DOMAIN_EVENT_STOPPED, sys::VIR_DOMAIN_EVENT_STOPPED_FROM_SNAPSHOT) => {
            "from_snapshot"
        }
        (sys::VIR_DOMAIN_EVENT_SHUTDOWN, sys::VIR_DOMAIN_EVENT_SHUTDOWN_FINISHED) => "finished",
        (sys::VIR_DOMAIN_EVENT_SHUTDOWN, sys::VIR_DOMAIN_EVENT_SHUTDOWN_GUEST) => "guest",
        (sys::VIR_DOMAIN_EVENT_SHUTDOWN, sys::VIR_DOMAIN_EVENT_SHUTDOWN_HOST) => "host",
        (sys::VIR_DOMAIN_EVENT_PMSUSPENDED, sys::VIR_DOMAIN_EVENT_PMSUSPENDED_MEMORY) => "memory",
        (sys::VIR_DOMAIN_EVENT_PMSUSPENDED, sys::VIR_DOMAIN_EVENT_PMSUSPENDED_DISK) => "disk",
        (sys::VIR_DOMAIN_EVENT_CRASHED, sys::VIR_DOMAIN_EVENT_CRASHED_PANICKED) => "panicked",
        (sys::VIR_DOMAIN_EVENT_CRASHED, sys::VIR_DOMAIN_EVENT_CRASHED_CRASHLOADED) => "crashloaded",
        _ => "unknown",
    }
}

fn watchdog_action(action: u32) -> &'static str {
    match action {
        sys::VIR_DOMAIN_EVENT_WATCHDOG_NONE => "none",
        sys::VIR_DOMAIN_EVENT_WATCHDOG_PAUSE => "pause",
        sys::VIR_DOMAIN_EVENT_WATCHDOG_RESET => "reset",
        sys::VIR_DOMAIN_EVENT_WATCHDOG_POWEROFF => "poweroff",
        sys::VIR_DOMAIN_EVENT_WATCHDOG_SHUTDOWN => "shutdown",
        sys::VIR_DOMAIN_EVENT_WATCHDOG_DEBUG => "debug",
        sys::VIR_DOMAIN_EVENT_WATCHDOG_INJECTNMI => "inject_nmi",
        _ => "unknown",
    }
}

fn agent_reason(reason: u32) -> &'static str {
    match reason {
        sys::VIR_CONNECT_DOMAIN_EVENT_AGENT_LIFECYCLE_REASON_DOMAIN_STARTED => "domain_started",
        sys::VIR_CONNECT_DOMAIN_EVENT_AGENT_LIFECYCLE_REASON_CHANNEL => "channel",
        _ => "unknown",
    }
}

/// Maps libvirt's numbers to a record, by the design's table and the probe's observed sequences.
fn map(source: Source) -> Mapped {
    // libvirt's numbers are small and never negative; a negative one maps to nothing known.
    let n = |v: i32| u32::try_from(v).unwrap_or(u32::MAX);
    match source {
        Source::Lifecycle { event, detail } => {
            let (event, detail) = (n(event), n(detail));
            let text = lifecycle_detail(event, detail).to_string();
            let kind = match event {
                sys::VIR_DOMAIN_EVENT_STARTED => Kind::Started,
                sys::VIR_DOMAIN_EVENT_SUSPENDED => Kind::Paused,
                sys::VIR_DOMAIN_EVENT_RESUMED => Kind::Resumed,
                sys::VIR_DOMAIN_EVENT_SHUTDOWN => Kind::Shutdown,
                sys::VIR_DOMAIN_EVENT_STOPPED
                    if detail == sys::VIR_DOMAIN_EVENT_STOPPED_CRASHED =>
                {
                    return Mapped::CrashStop;
                }
                sys::VIR_DOMAIN_EVENT_STOPPED => Kind::Stopped,
                sys::VIR_DOMAIN_EVENT_CRASHED => Kind::Crashed,
                sys::VIR_DOMAIN_EVENT_PMSUSPENDED => Kind::Pmsuspended,
                sys::VIR_DOMAIN_EVENT_UNDEFINED => return Mapped::Undefined,
                _ => return Mapped::Nothing,
            };
            Mapped::Record(kind, text)
        }
        Source::Reboot => Mapped::Record(Kind::Rebooted, String::new()),
        Source::Watchdog { action } => {
            Mapped::Record(Kind::Watchdog, watchdog_action(n(action)).into())
        }
        Source::Agent { state, reason } => {
            let kind = if n(state) == sys::VIR_CONNECT_DOMAIN_EVENT_AGENT_LIFECYCLE_STATE_CONNECTED
            {
                Kind::AgentConnected
            } else {
                Kind::AgentDisconnected
            };
            Mapped::Record(kind, agent_reason(n(reason)).into())
        }
    }
}

/// A tool's claim on the records its action is about to produce.
struct Expectation {
    id: u64,
    uuid: String,
    cause: Cause,
    /// Kinds not yet claimed; each is claimed once.
    kinds: Vec<Kind>,
    /// Set when the tool returns; the expectation lapses after this time.
    until: Option<u64>,
}

/// Identifies an expectation for `Events::finish`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpectationId(u64);

struct VmLog {
    name: String,
    records: VecDeque<Record>,
    dropped: u64,
    /// The highest `seq` an agent has been shown, by a notice, an unfiltered `vm_events` or a wait.
    seen: u64,
}

/// What `Events::read` returns.
#[derive(Debug, PartialEq, Eq)]
pub struct Read {
    pub records: Vec<Record>,
    /// The `since` for the following call.
    pub next: u64,
    /// How many of the VM's records `max_events` has dropped.
    pub dropped: u64,
}

struct Inner {
    max: usize,
    last_seq: u64,
    logs: HashMap<String, VmLog>,
    expectations: Vec<Expectation>,
    last_expectation: u64,
}

/// Every visible VM's event log, shared by the event worker and the tools.
pub struct Events {
    inner: Mutex<Inner>,
}

impl Events {
    /// A log that keeps at most `max_events` records for each VM.
    pub fn new(max_events: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                max: max_events.max(1),
                last_seq: 0,
                logs: HashMap::new(),
                expectations: Vec::new(),
                last_expectation: 0,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // Every state of the log is valid, so a panic elsewhere while it was held leaves nothing to repair.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records that a tool is about to cause `kinds` on the VM. The claim stays open until `finish`, then for
    /// `EXPECTATION_WINDOW_SECS` more.
    pub fn expect(&self, uuid: &str, cause: Cause, kinds: &[Kind]) -> ExpectationId {
        let mut inner = self.lock();
        inner.last_expectation += 1;
        let id = inner.last_expectation;
        inner.expectations.push(Expectation {
            id,
            uuid: uuid.to_string(),
            cause,
            kinds: kinds.to_vec(),
            until: None,
        });
        ExpectationId(id)
    }

    /// Marks the tool as returned at `now`, which starts its expectation's closing window.
    pub fn finish(&self, id: ExpectationId, now: u64) {
        let mut inner = self.lock();
        if let Some(e) = inner.expectations.iter_mut().find(|e| e.id == id.0) {
            e.until = Some(now + EXPECTATION_WINDOW_SECS);
        }
    }

    /// Adds a libvirt event to its VM's log, and returns the record's `seq` when it made or changed one.
    pub fn append(&self, raw: Raw) -> Option<u64> {
        let mut inner = self.lock();
        let time = raw.time;
        inner
            .expectations
            .retain(|e| e.until.is_none_or(|until| time <= until));
        let (kind, detail) = match map(raw.source) {
            Mapped::Nothing => return None,
            Mapped::Undefined => {
                inner.logs.remove(&raw.uuid);
                return None;
            }
            Mapped::CrashStop => {
                // Under `destroy`, libvirt follows the crash with a stop; the crash record already says it all.
                let log = inner.log(&raw.uuid, &raw.name);
                if let Some(last) = log.records.back_mut()
                    && last.kind == Kind::Crashed
                    && !last.stopped
                {
                    last.stopped = true;
                    return Some(last.seq);
                }
                (Kind::Crashed, "crashed".to_string())
            }
            Mapped::Record(kind, detail) => (kind, detail),
        };
        if kind == Kind::Rebooted
            && inner
                .logs
                .get(&raw.uuid)
                .and_then(|l| l.records.back())
                .is_some_and(|last| {
                    last.kind == Kind::Rebooted
                        && time.saturating_sub(last.time) <= REBOOT_REPEAT_SECS
                })
        {
            return None;
        }
        let cause = inner.claim(&raw.uuid, kind);
        let stopped = kind == Kind::Crashed && detail == "crashed";
        inner.last_seq += 1;
        let record = Record {
            seq: inner.last_seq,
            time,
            kind,
            detail,
            cause,
            stopped,
            gap: None,
        };
        Some(inner.push(&raw.uuid, &raw.name, record))
    }

    /// Appends a `gap` record to every log: events from `from` to `to` may be missing.
    pub fn record_gap(&self, from: u64, to: u64) {
        let mut inner = self.lock();
        let uuids: Vec<(String, String)> = inner
            .logs
            .iter()
            .map(|(u, l)| (u.clone(), l.name.clone()))
            .collect();
        for (uuid, name) in uuids {
            inner.last_seq += 1;
            let record = Record {
                seq: inner.last_seq,
                time: to,
                kind: Kind::Gap,
                detail: String::new(),
                cause: Cause::Outside,
                stopped: false,
                gap: Some((from, to)),
            };
            inner.push(&uuid, &name, record);
        }
    }

    /// The VM's records after `since`, oldest first, of `kinds` when given. Without `kinds`, the VM's seen-cursor
    /// moves past what is returned.
    pub fn read(&self, uuid: &str, since: u64, kinds: Option<&[Kind]>) -> Read {
        let mut inner = self.lock();
        let Some(log) = inner.logs.get_mut(uuid) else {
            return Read {
                records: Vec::new(),
                next: since,
                dropped: 0,
            };
        };
        let records: Vec<Record> = log
            .records
            .iter()
            .filter(|r| r.seq > since && kinds.is_none_or(|k| k.contains(&r.kind)))
            .cloned()
            .collect();
        let next = records.last().map_or(since, |r| r.seq);
        if kinds.is_none() {
            log.seen = log.seen.max(next);
        }
        Read {
            records,
            next,
            dropped: log.dropped,
        }
    }

    /// The highest `seq` in the VM's log, or 0: the `since` that matches only what arrives from now on.
    pub fn head(&self, uuid: &str) -> u64 {
        let inner = self.lock();
        inner
            .logs
            .get(uuid)
            .and_then(|l| l.records.back())
            .map_or(0, |r| r.seq)
    }

    /// The first record after `since` of one of `kinds`, which moves the seen-cursor past it.
    pub fn take_match(&self, uuid: &str, since: u64, kinds: &[Kind]) -> Option<Record> {
        let mut inner = self.lock();
        let log = inner.logs.get_mut(uuid)?;
        let found = log
            .records
            .iter()
            .find(|r| r.seq > since && (kinds.is_empty() || kinds.contains(&r.kind)))
            .cloned()?;
        log.seen = log.seen.max(found.seq);
        Some(found)
    }

    /// The VM's records with a notice that no agent has been shown yet; the seen-cursor moves past them, so each is
    /// announced once per server process.
    pub fn take_notices(&self, uuid: &str) -> (String, Vec<Record>) {
        let mut inner = self.lock();
        let Some(log) = inner.logs.get_mut(uuid) else {
            return (String::new(), Vec::new());
        };
        let seen = log.seen;
        let notices: Vec<Record> = log
            .records
            .iter()
            .filter(|r| r.seq > seen && r.has_notice())
            .cloned()
            .collect();
        if let Some(last) = notices.last() {
            log.seen = last.seq;
        }
        (log.name.clone(), notices)
    }

    /// Drops everything kept for a deleted VM.
    pub fn drop_vm(&self, uuid: &str) {
        let mut inner = self.lock();
        inner.logs.remove(uuid);
        inner.expectations.retain(|e| e.uuid != uuid);
    }
}

impl Inner {
    fn log(&mut self, uuid: &str, name: &str) -> &mut VmLog {
        let log = self.logs.entry(uuid.to_string()).or_insert_with(|| VmLog {
            name: String::new(),
            records: VecDeque::new(),
            dropped: 0,
            seen: 0,
        });
        if !name.is_empty() {
            log.name = name.to_string();
        }
        log
    }

    fn push(&mut self, uuid: &str, name: &str, record: Record) -> u64 {
        let max = self.max;
        let seq = record.seq;
        let log = self.log(uuid, name);
        log.records.push_back(record);
        while log.records.len() > max {
            log.records.pop_front();
            log.dropped += 1;
        }
        seq
    }

    /// The cause of a new record of `kind`: the first open expectation on the VM that still claims the kind, which
    /// then claims it no more, or `Outside`.
    fn claim(&mut self, uuid: &str, kind: Kind) -> Cause {
        for e in self.expectations.iter_mut().filter(|e| e.uuid == uuid) {
            if let Some(at) = e.kinds.iter().position(|k| *k == kind) {
                e.kinds.remove(at);
                return e.cause;
            }
        }
        Cause::Outside
    }
}

/// The notice text for a VM's unannounced records, such as
/// `VM 't-deb' crashed (panicked) at 14:02:11Z; see vm_events since 41`. `holds_memory` adds that a crashed VM stays
/// in place until a forced stop, for a VM still in the crashed state.
pub fn notice_text(name: &str, records: &[Record], holds_memory: bool) -> Option<String> {
    let first = records.first()?;
    let events: Vec<String> = records
        .iter()
        .map(|r| {
            let at = format_utc(r.time);
            let clock = &at[11..];
            let what = match r.kind {
                Kind::Crashed if r.stopped => format!("crashed ({}) and stopped", r.detail),
                Kind::Crashed => format!("crashed ({})", r.detail),
                Kind::Stopped => format!("stopped ({})", r.detail),
                Kind::Rebooted => "rebooted".to_string(),
                Kind::Paused => format!("paused ({})", r.detail),
                Kind::Watchdog => format!("set off its watchdog ({})", r.detail),
                Kind::Gap => {
                    let (from, to) = r.gap.unwrap_or((r.time, r.time));
                    return format!(
                        "events may be missing from {} to {}",
                        &format_utc(from)[11..],
                        &format_utc(to)[11..]
                    );
                }
                _ => format!("{:?} ({})", r.kind, r.detail),
            };
            let by = if r.cause == Cause::Expiry {
                " by its expiry"
            } else {
                ""
            };
            format!("{what}{by} at {clock}")
        })
        .collect();
    let mut text = format!("VM '{name}' {}", events.join(", "));
    if holds_memory {
        text.push_str("; it holds its memory until stop_vm with force: true");
    }
    text.push_str(&format!("; see vm_events since {}", first.seq - 1));
    Some(text)
}

/// Epoch seconds as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn format_utc(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    // Howard Hinnant's civil-from-days, for days since 1970-01-01.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "00000000-0000-0000-0000-000000000001";

    fn lifecycle(event: u32, detail: u32, time: u64) -> Raw {
        Raw {
            uuid: UUID.into(),
            name: "t-deb".into(),
            source: Source::Lifecycle {
                event: event as i32,
                detail: detail as i32,
            },
            time,
        }
    }

    fn crashed(time: u64) -> Raw {
        lifecycle(
            sys::VIR_DOMAIN_EVENT_CRASHED,
            sys::VIR_DOMAIN_EVENT_CRASHED_PANICKED,
            time,
        )
    }

    fn crash_stop(time: u64) -> Raw {
        lifecycle(
            sys::VIR_DOMAIN_EVENT_STOPPED,
            sys::VIR_DOMAIN_EVENT_STOPPED_CRASHED,
            time,
        )
    }

    fn destroyed(time: u64) -> Raw {
        lifecycle(
            sys::VIR_DOMAIN_EVENT_STOPPED,
            sys::VIR_DOMAIN_EVENT_STOPPED_DESTROYED,
            time,
        )
    }

    fn reboot(time: u64) -> Raw {
        Raw {
            uuid: UUID.into(),
            name: "t-deb".into(),
            source: Source::Reboot,
            time,
        }
    }

    fn kinds(events: &Events) -> Vec<(Kind, String, Cause)> {
        events
            .read(UUID, 0, None)
            .records
            .into_iter()
            .map(|r| (r.kind, r.detail, r.cause))
            .collect()
    }

    #[test]
    fn the_kind_table_maps_the_probes_sequences() {
        let cases = [
            (
                sys::VIR_DOMAIN_EVENT_STARTED,
                sys::VIR_DOMAIN_EVENT_STARTED_BOOTED,
                Kind::Started,
                "booted",
            ),
            (
                sys::VIR_DOMAIN_EVENT_STARTED,
                sys::VIR_DOMAIN_EVENT_STARTED_FROM_SNAPSHOT,
                Kind::Started,
                "from_snapshot",
            ),
            (
                sys::VIR_DOMAIN_EVENT_SUSPENDED,
                sys::VIR_DOMAIN_EVENT_SUSPENDED_PAUSED,
                Kind::Paused,
                "paused",
            ),
            (
                sys::VIR_DOMAIN_EVENT_SUSPENDED,
                sys::VIR_DOMAIN_EVENT_SUSPENDED_FROM_SNAPSHOT,
                Kind::Paused,
                "from_snapshot",
            ),
            (
                sys::VIR_DOMAIN_EVENT_SUSPENDED,
                sys::VIR_DOMAIN_EVENT_SUSPENDED_IOERROR,
                Kind::Paused,
                "ioerror",
            ),
            (
                sys::VIR_DOMAIN_EVENT_RESUMED,
                sys::VIR_DOMAIN_EVENT_RESUMED_UNPAUSED,
                Kind::Resumed,
                "unpaused",
            ),
            (
                sys::VIR_DOMAIN_EVENT_SHUTDOWN,
                sys::VIR_DOMAIN_EVENT_SHUTDOWN_GUEST,
                Kind::Shutdown,
                "guest",
            ),
            (
                sys::VIR_DOMAIN_EVENT_SHUTDOWN,
                sys::VIR_DOMAIN_EVENT_SHUTDOWN_HOST,
                Kind::Shutdown,
                "host",
            ),
            (
                sys::VIR_DOMAIN_EVENT_STOPPED,
                sys::VIR_DOMAIN_EVENT_STOPPED_SHUTDOWN,
                Kind::Stopped,
                "shutdown",
            ),
            (
                sys::VIR_DOMAIN_EVENT_STOPPED,
                sys::VIR_DOMAIN_EVENT_STOPPED_DESTROYED,
                Kind::Stopped,
                "destroyed",
            ),
            (
                sys::VIR_DOMAIN_EVENT_STOPPED,
                sys::VIR_DOMAIN_EVENT_STOPPED_FROM_SNAPSHOT,
                Kind::Stopped,
                "from_snapshot",
            ),
            (
                sys::VIR_DOMAIN_EVENT_CRASHED,
                sys::VIR_DOMAIN_EVENT_CRASHED_PANICKED,
                Kind::Crashed,
                "panicked",
            ),
            (
                sys::VIR_DOMAIN_EVENT_PMSUSPENDED,
                sys::VIR_DOMAIN_EVENT_PMSUSPENDED_MEMORY,
                Kind::Pmsuspended,
                "memory",
            ),
        ];
        for (event, detail, kind, text) in cases {
            let source = Source::Lifecycle {
                event: event as i32,
                detail: detail as i32,
            };
            assert_eq!(
                map(source),
                Mapped::Record(kind, text.into()),
                "{event}/{detail}"
            );
        }
        let defined = Source::Lifecycle {
            event: sys::VIR_DOMAIN_EVENT_DEFINED as i32,
            detail: sys::VIR_DOMAIN_EVENT_DEFINED_UPDATED as i32,
        };
        assert_eq!(map(defined), Mapped::Nothing);
        let agent = Source::Agent {
            state: sys::VIR_CONNECT_DOMAIN_EVENT_AGENT_LIFECYCLE_STATE_DISCONNECTED as i32,
            reason: sys::VIR_CONNECT_DOMAIN_EVENT_AGENT_LIFECYCLE_REASON_DOMAIN_STARTED as i32,
        };
        assert_eq!(
            map(agent),
            Mapped::Record(Kind::AgentDisconnected, "domain_started".into())
        );
        let watchdog = Source::Watchdog {
            action: sys::VIR_DOMAIN_EVENT_WATCHDOG_RESET as i32,
        };
        assert_eq!(
            map(watchdog),
            Mapped::Record(Kind::Watchdog, "reset".into())
        );
        assert_eq!(
            map(Source::Lifecycle {
                event: -1,
                detail: 0
            }),
            Mapped::Nothing
        );
    }

    #[test]
    fn a_crash_stop_merges_into_its_crash_or_stands_alone() {
        let events = Events::new(16);
        let seq = events.append(crashed(10)).unwrap();
        assert_eq!(events.append(crash_stop(11)), Some(seq));
        let read = events.read(UUID, 0, None);
        assert_eq!(read.records.len(), 1);
        assert!(read.records[0].stopped);
        assert_eq!(read.records[0].to_json()["stopped"], json!(true));

        let alone = Events::new(16);
        alone.append(crash_stop(11)).unwrap();
        let record = &alone.read(UUID, 0, None).records[0];
        assert_eq!(
            (record.kind, record.detail.as_str(), record.stopped),
            (Kind::Crashed, "crashed", true)
        );
    }

    #[test]
    fn the_bound_drops_the_oldest_and_counts_them() {
        let events = Events::new(3);
        for t in 0..5 {
            events.append(destroyed(t));
        }
        let read = events.read(UUID, 0, None);
        assert_eq!(
            read.records.iter().map(|r| r.seq).collect::<Vec<_>>(),
            [3, 4, 5]
        );
        assert_eq!(read.dropped, 2);
    }

    #[test]
    fn an_outside_crash_never_uses_up_a_stop_expectation() {
        let events = Events::new(16);
        let id = events.expect(
            UUID,
            Cause::Tool("stop_vm"),
            &[Kind::Shutdown, Kind::Stopped],
        );
        events.append(crashed(10));
        events.append(destroyed(11));
        events.finish(id, 11);
        let causes: Vec<_> = kinds(&events).into_iter().map(|(k, _, c)| (k, c)).collect();
        assert_eq!(
            causes,
            [
                (Kind::Crashed, Cause::Outside),
                (Kind::Stopped, Cause::Tool("stop_vm"))
            ]
        );
    }

    #[test]
    fn an_expectation_claims_each_kind_once_and_lapses_after_its_window() {
        let events = Events::new(16);
        let id = events.expect(UUID, Cause::Tool("stop_vm"), &[Kind::Stopped]);
        events.finish(id, 100);
        events.append(destroyed(100 + EXPECTATION_WINDOW_SECS));
        events.append(destroyed(100 + EXPECTATION_WINDOW_SECS));
        let late = events.expect(UUID, Cause::Tool("stop_vm"), &[Kind::Stopped]);
        events.finish(late, 200);
        events.append(destroyed(200 + EXPECTATION_WINDOW_SECS + 1));
        let causes: Vec<_> = kinds(&events).into_iter().map(|(_, _, c)| c).collect();
        assert_eq!(
            causes,
            [Cause::Tool("stop_vm"), Cause::Outside, Cause::Outside]
        );
    }

    #[test]
    fn the_expiry_cause_is_kept_and_announced() {
        let events = Events::new(16);
        events.expect(UUID, Cause::Expiry, &[Kind::Stopped]);
        events.append(destroyed(10));
        let (name, notices) = events.take_notices(UUID);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].to_json()["cause"], json!("expiry"));
        let text = notice_text(&name, &notices, false).unwrap();
        assert!(text.contains("stopped (destroyed) by its expiry"), "{text}");
    }

    #[test]
    fn each_record_is_announced_once_and_a_tools_record_never() {
        let events = Events::new(16);
        let id = events.expect(UUID, Cause::Tool("stop_vm"), &[Kind::Stopped]);
        events.append(destroyed(10));
        events.finish(id, 10);
        assert!(events.take_notices(UUID).1.is_empty());

        events.append(crashed(1_795_438_931));
        let (name, notices) = events.take_notices(UUID);
        let text = notice_text(&name, &notices, true).unwrap();
        assert_eq!(
            text,
            "VM 't-deb' crashed (panicked) at 13:02:11Z; it holds its memory until stop_vm with force: true; see \
             vm_events since 1"
        );
        assert!(events.take_notices(UUID).1.is_empty());
    }

    #[test]
    fn a_filtered_read_advances_nothing() {
        let events = Events::new(16);
        events.append(crashed(10));
        let read = events.read(UUID, 0, Some(&[Kind::Started]));
        assert!(read.records.is_empty());
        assert_eq!(events.take_notices(UUID).1.len(), 1);

        events.append(destroyed(11));
        assert_eq!(events.read(UUID, 0, None).next, 2);
        assert!(events.take_notices(UUID).1.is_empty());
    }

    #[test]
    fn a_wait_match_advances_the_cursor_over_its_record() {
        let events = Events::new(16);
        let since = events.head(UUID);
        events.append(crashed(10));
        let found = events.take_match(UUID, since, &[Kind::Crashed]).unwrap();
        assert_eq!(found.kind, Kind::Crashed);
        assert!(events.take_notices(UUID).1.is_empty());
        assert_eq!(events.take_match(UUID, found.seq, &[Kind::Crashed]), None);
    }

    #[test]
    fn a_repeated_reboot_is_one_record() {
        let events = Events::new(16);
        assert!(events.append(reboot(10)).is_some());
        assert!(events.append(reboot(10)).is_none());
        assert!(events.append(reboot(15)).is_some());
        assert_eq!(events.read(UUID, 0, None).records.len(), 2);
    }

    #[test]
    fn two_threads_taking_notices_share_each_record_once() {
        let events = std::sync::Arc::new(Events::new(1_000));
        for t in 0..200 {
            events.append(reboot(t * 10));
        }
        let take = |events: std::sync::Arc<Events>| {
            std::thread::spawn(move || {
                let mut got = Vec::new();
                for _ in 0..50 {
                    got.extend(events.take_notices(UUID).1.into_iter().map(|r| r.seq));
                }
                got
            })
        };
        let (a, b) = (take(events.clone()), take(events.clone()));
        let mut all = a.join().unwrap();
        all.extend(b.join().unwrap());
        all.sort_unstable();
        assert_eq!(all, (1..=200).collect::<Vec<u64>>());
    }

    #[test]
    fn a_gap_reaches_every_log_and_is_announced() {
        let events = Events::new(16);
        events.append(destroyed(10));
        events.take_notices(UUID);
        events.record_gap(20, 30);
        let (name, notices) = events.take_notices(UUID);
        assert_eq!(notices[0].gap, Some((20, 30)));
        let text = notice_text(&name, &notices, false).unwrap();
        assert!(
            text.contains("events may be missing from 00:00:20Z to 00:00:30Z"),
            "{text}"
        );
    }

    #[test]
    fn a_dropped_vm_keeps_nothing() {
        let events = Events::new(16);
        events.expect(UUID, Cause::Tool("stop_vm"), &[Kind::Stopped]);
        events.append(crashed(10));
        events.drop_vm(UUID);
        assert_eq!(events.read(UUID, 0, None).records, Vec::new());
        assert!(events.take_notices(UUID).1.is_empty());
        events.append(destroyed(11));
        assert_eq!(events.read(UUID, 0, None).records[0].cause, Cause::Outside);

        let undefined = Events::new(16);
        undefined.append(crashed(10));
        undefined.append(lifecycle(
            sys::VIR_DOMAIN_EVENT_UNDEFINED,
            sys::VIR_DOMAIN_EVENT_UNDEFINED_REMOVED,
            11,
        ));
        assert!(undefined.read(UUID, 0, None).records.is_empty());
    }

    #[test]
    fn utc_formatting() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_utc(1_795_438_931), "2026-11-23T13:02:11Z");
        assert_eq!(format_utc(4_107_542_399), "2100-02-28T23:59:59Z");
    }
}
