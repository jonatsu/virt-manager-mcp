//! `wait_for`: waiting for the first of several conditions on one VM.
//!
//! The loop polls every 30 ms. Console conditions scan the session buffer on the loop's own thread. Each agent and
//! command condition runs its probes on a thread of its own, at most one at a time and about once a second, so a hung
//! guest agent delays neither the console checks nor the end of the call. The loop reaches the world only through
//! [`Env`] and [`Guest`], so tests drive it with a fake clock and fake probes.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use regex::bytes::Regex;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use virt::domain::Domain;

use crate::agent::{Agent, ExecSpec, GuestOs};
use crate::ansi;
use crate::config::Config;
use crate::console::{Consoles, SessionScan};

/// How often the loop polls.
const POLL: Duration = Duration::from_millis(30);
/// How often an agent or command condition starts a probe.
const PROBE_EVERY: Duration = Duration::from_secs(1);
/// How often the loop checks that the VM still runs.
const VM_CHECK_EVERY: Duration = Duration::from_millis(500);
/// How often a progress notification goes out.
const PROGRESS_EVERY: Duration = Duration::from_secs(5);
/// Most bytes of matched text or command output a result carries.
const MAX_TEXT: usize = 4096;
/// Most conditions one call may name.
const MAX_CONDITIONS: usize = 16;

/// One thing to wait for.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Condition {
    /// The console session's output matches `pattern`, a regular expression (Rust `regex` syntax).
    Console {
        pattern: String,
        /// Offset to match from, such as `next` from an earlier result. Omit to start at the session's wait cursor:
        /// where the last `console_write` began, or just past the last match.
        since: Option<u64>,
        /// The `generation` returned with `since`. Needed whenever `since` is above 0.
        generation: Option<u64>,
        /// Match against output with ANSI escape sequences removed. Default true.
        strip_ansi: Option<bool>,
    },
    /// The QEMU guest agent answers.
    Agent,
    /// A shell command run through the guest agent exits 0. It runs once, and runs again after a non-zero exit; it
    /// is never killed. Linux guests only. Refused in read-only mode.
    Command { command: String },
}

impl Condition {
    #[cfg(test)]
    fn kind(&self) -> &'static str {
        match self {
            Condition::Console { .. } => "console",
            Condition::Agent => "agent",
            Condition::Command { .. } => "command",
        }
    }
}

/// A condition that passed validation, with its pattern compiled.
pub enum Check {
    Console {
        regex: Regex,
        since: Option<u64>,
        generation: Option<u64>,
        strip_ansi: bool,
    },
    Agent,
    Command(String),
}

/// Refuses a list of conditions the server will not wait for, before any libvirt call: an empty or overlong list, a
/// pattern that does not compile, an offset without its generation, and a command in read-only mode.
pub fn validate_conditions(config: &Config, conditions: &[Condition]) -> Result<Vec<Check>> {
    if conditions.is_empty() {
        bail!("name at least one condition");
    }
    if conditions.len() > MAX_CONDITIONS {
        bail!("name at most {MAX_CONDITIONS} conditions");
    }
    conditions
        .iter()
        .enumerate()
        .map(|(i, c)| {
            Ok(match c {
                Condition::Console {
                    pattern,
                    since,
                    generation,
                    strip_ansi,
                } => {
                    let regex = Regex::new(pattern)
                        .map_err(|e| anyhow!("condition {i}: bad pattern: {e}"))?;
                    if since.is_some_and(|s| s > 0) && generation.is_none() {
                        bail!(
                            "condition {i}: since {} needs the generation returned with it",
                            since.unwrap_or_default()
                        );
                    }
                    Check::Console {
                        regex,
                        since: *since,
                        generation: *generation,
                        strip_ansi: strip_ansi.unwrap_or(true),
                    }
                }
                Condition::Agent => Check::Agent,
                Condition::Command { command } => {
                    if config.read_only {
                        bail!("condition {i}: command conditions are disabled in read-only mode");
                    }
                    if command.trim().is_empty() {
                        bail!("condition {i}: command is empty");
                    }
                    Check::Command(command.clone())
                }
            })
        })
        .collect()
}

/// Everything the loop needs from the server and the clock.
pub trait Env {
    fn now(&self) -> Instant;
    fn sleep(&self, d: Duration);
    /// The VM's console output from `since`, as [`Consoles::scan`] returns it.
    fn scan(&self, since: Option<u64>, generation: Option<u64>) -> Result<SessionScan>;
    /// Moves the wait cursor past a match, as [`Consoles::advance_cursor`] does.
    fn advance_cursor(&self, generation: u64, to: u64);
    /// The running domain's ID, or `None` once it stopped.
    fn domain_id(&self) -> Option<u32>;
    fn cancelled(&self) -> bool;
    /// Called every 5 s with the time waited so far.
    fn progress(&self, waited: Duration);
}

/// A process the `command` probe started.
pub enum ExecState {
    Running,
    Exited { code: Option<i64>, output: Vec<u8> },
}

/// The guest agent calls the probes make, each bounded by `timeout_secs`.
pub trait Guest: Send + Sync + 'static {
    fn ping(&self, timeout_secs: i32) -> bool;
    fn os(&self, timeout_secs: i32) -> Result<GuestOs>;
    fn start(&self, command: &str, timeout_secs: i32) -> Result<i64>;
    fn status(&self, pid: i64, timeout_secs: i32) -> Result<ExecState>;
}

/// The guest agent of a real domain.
pub struct LiveGuest(pub Domain);

impl Guest for LiveGuest {
    fn ping(&self, timeout_secs: i32) -> bool {
        Agent::new(&self.0, timeout_secs).ping()
    }

    fn os(&self, timeout_secs: i32) -> Result<GuestOs> {
        Agent::new(&self.0, timeout_secs).os()
    }

    fn start(&self, command: &str, timeout_secs: i32) -> Result<i64> {
        Agent::new(&self.0, timeout_secs).start(&ExecSpec::new("/bin/sh", ["-c", command]))
    }

    fn status(&self, pid: i64, timeout_secs: i32) -> Result<ExecState> {
        Ok(
            match Agent::new(&self.0, timeout_secs).status(pid, MAX_TEXT)? {
                None => ExecState::Running,
                Some(o) => ExecState::Exited {
                    code: o.exit_code,
                    output: o.stdout,
                },
            },
        )
    }
}

/// The real environment of a wait on `vm`.
pub struct LiveEnv<'a> {
    pub consoles: &'a Consoles,
    pub vm: &'a str,
    pub dom: Domain,
    pub cancelled: Box<dyn Fn() -> bool + 'a>,
    pub progress: Box<dyn Fn(Duration) + 'a>,
}

impl Env for LiveEnv<'_> {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }

    fn scan(&self, since: Option<u64>, generation: Option<u64>) -> Result<SessionScan> {
        self.consoles.scan(self.vm, since, generation)
    }

    fn advance_cursor(&self, generation: u64, to: u64) {
        self.consoles.advance_cursor(self.vm, generation, to);
    }

    fn domain_id(&self) -> Option<u32> {
        self.dom.get_id()
    }

    fn cancelled(&self) -> bool {
        (self.cancelled)()
    }

    fn progress(&self, waited: Duration) {
        (self.progress)(waited);
    }
}

/// How a wait ended.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Matched {
        index: usize,
        kind: &'static str,
        text: String,
        /// For a console condition: the raw offset just past the match, and its generation.
        next: Option<(u64, u64)>,
    },
    Closed {
        index: Option<usize>,
        reason: String,
    },
    TimedOut {
        /// Each console condition's resume point: the start of its last unfinished line, and the generation.
        resume: Vec<(usize, u64, u64)>,
        /// Each command condition's process still running, or the exit code of its last run.
        commands: Vec<(usize, Option<i64>, Option<i64>)>,
    },
    Cancelled,
}

impl Outcome {
    pub fn to_json(&self, waited: Duration) -> Value {
        let waited_ms = u64::try_from(waited.as_millis()).unwrap_or(u64::MAX);
        match self {
            Outcome::Matched {
                index,
                kind,
                text,
                next,
            } => {
                let mut v = json!({
                    "result": "matched", "index": index, "kind": kind, "text": text, "waited_ms": waited_ms,
                });
                if let Some((next, generation)) = next {
                    v["next"] = json!(next);
                    v["generation"] = json!(generation);
                }
                v
            }
            Outcome::Closed { index, reason } => {
                json!({ "result": "closed", "index": index, "reason": reason, "waited_ms": waited_ms })
            }
            Outcome::TimedOut { resume, commands } => json!({
                "result": "timed_out",
                "waited_ms": waited_ms,
                "resume": resume.iter().map(|(index, since, generation)| {
                    json!({ "index": index, "since": since, "generation": generation })
                }).collect::<Vec<_>>(),
                "commands": commands.iter().map(|(index, pid, code)| {
                    json!({ "index": index, "running_pid": pid, "last_exit_code": code })
                }).collect::<Vec<_>>(),
            }),
            Outcome::Cancelled => json!({ "result": "cancelled", "waited_ms": waited_ms }),
        }
    }
}

/// Waits up to `timeout` for the first of `checks` to hold.
pub fn run(
    env: &dyn Env,
    guest: Arc<dyn Guest>,
    checks: Vec<Check>,
    timeout: Duration,
) -> Result<Outcome> {
    let start = env.now();
    let deadline = start + timeout;
    let mut states = checks
        .into_iter()
        .map(|c| State::new(c, env))
        .collect::<Result<Vec<_>>>()?;
    let mut next_vm_check = start;
    let mut next_progress = start + PROGRESS_EVERY;
    let mut domain_id = None;
    loop {
        let now = env.now();
        if env.cancelled() {
            return Ok(Outcome::Cancelled);
        }
        if now >= next_vm_check {
            domain_id = env.domain_id();
            if domain_id.is_none() {
                return Ok(Outcome::Closed {
                    index: None,
                    reason: "the VM is not running".into(),
                });
            }
            next_vm_check = now + VM_CHECK_EVERY;
        }
        let mut closed = None;
        for (index, state) in states.iter_mut().enumerate() {
            match state.poll(env, &guest, now, deadline, domain_id)? {
                Poll::Pending => {}
                Poll::Hit { text, next } => {
                    if let Some((to, generation)) = next {
                        env.advance_cursor(generation, to);
                    }
                    return Ok(Outcome::Matched {
                        index,
                        kind: state.kind(),
                        text,
                        next,
                    });
                }
                Poll::Closed(reason) => {
                    closed.get_or_insert((index, reason));
                }
            }
        }
        if let Some((index, reason)) = closed {
            return Ok(Outcome::Closed {
                index: Some(index),
                reason,
            });
        }
        if now >= deadline {
            return Ok(timed_out(&states));
        }
        if now >= next_progress {
            env.progress(now - start);
            next_progress = now + PROGRESS_EVERY;
        }
        env.sleep(POLL.min(deadline - now));
    }
}

fn timed_out(states: &[State]) -> Outcome {
    let mut resume = Vec::new();
    let mut commands = Vec::new();
    for (index, state) in states.iter().enumerate() {
        match state {
            State::Console(c) => resume.push((index, c.resume, c.generation)),
            State::Command(c) => commands.push((index, c.pid, c.last_code)),
            State::Agent(_) => {}
        }
    }
    Outcome::TimedOut { resume, commands }
}

enum Poll {
    Pending,
    Hit {
        text: String,
        next: Option<(u64, u64)>,
    },
    Closed(String),
}

enum State {
    Console(ConsoleState),
    Agent(Probe<bool>),
    Command(CommandState),
}

impl State {
    fn new(check: Check, env: &dyn Env) -> Result<Self> {
        Ok(match check {
            Check::Console {
                regex,
                since,
                generation,
                strip_ansi,
            } => {
                // The first scan pins the start and the generation, so a write during the wait cannot move them.
                let first = env.scan(since, generation)?;
                State::Console(ConsoleState {
                    regex,
                    strip_ansi,
                    from: first.scan.from,
                    generation: first.generation,
                    domain_id: first.domain_id,
                    resume: first.scan.from,
                    seen_end: None,
                })
            }
            Check::Agent => State::Agent(Probe::default()),
            Check::Command(command) => State::Command(CommandState {
                command: Arc::from(command),
                os_checked: false,
                pid: None,
                last_code: None,
                probe: Probe::default(),
            }),
        })
    }

    fn kind(&self) -> &'static str {
        match self {
            State::Console(_) => "console",
            State::Agent(_) => "agent",
            State::Command(_) => "command",
        }
    }

    fn poll(
        &mut self,
        env: &dyn Env,
        guest: &Arc<dyn Guest>,
        now: Instant,
        deadline: Instant,
        domain_id: Option<u32>,
    ) -> Result<Poll> {
        match self {
            State::Console(c) => Ok(c.poll(env, domain_id)),
            State::Agent(p) => {
                if p.take() == Some(true) {
                    return Ok(Poll::Hit {
                        text: "the guest agent answered".into(),
                        next: None,
                    });
                }
                p.start_if_due(now, deadline, guest, |g, t| g.ping(t));
                Ok(Poll::Pending)
            }
            State::Command(c) => c.poll(now, deadline, guest),
        }
    }
}

struct ConsoleState {
    regex: Regex,
    strip_ansi: bool,
    /// Where matching starts, pinned by the first scan.
    from: u64,
    generation: u64,
    domain_id: Option<u32>,
    /// The start of the last unfinished line seen.
    resume: u64,
    /// The buffer end at the last scan, to skip matching when nothing arrived.
    seen_end: Option<u64>,
}

impl ConsoleState {
    fn poll(&mut self, env: &dyn Env, domain_id: Option<u32>) -> Poll {
        if domain_id.is_some() && self.domain_id != domain_id {
            return Poll::Closed(
                "the VM restarted after console_open; open the console again".into(),
            );
        }
        let scan = match env.scan(Some(self.from), Some(self.generation)) {
            Ok(s) => s,
            Err(e) => return Poll::Closed(format!("the console session ended: {e:#}")),
        };
        if scan.scan.dropped > 0 {
            // The scrollback dropped the oldest output; match from what it still holds.
            self.from = scan.scan.from;
        }
        let data = &scan.scan.data;
        let end = scan.scan.from + data.len() as u64;
        if self.seen_end != Some(end) {
            self.seen_end = Some(end);
            self.resume = scan.scan.from
                + data
                    .iter()
                    .rposition(|&b| b == b'\n')
                    .map_or(0, |p| p as u64 + 1);
            if let Some((next, text)) = find(&self.regex, self.strip_ansi, scan.scan.from, data) {
                return Poll::Hit {
                    text,
                    next: Some((next, self.generation)),
                };
            }
        }
        match scan.scan.closed {
            Some(reason) => Poll::Closed(format!("the console session closed: {reason}")),
            None => Poll::Pending,
        }
    }
}

/// Finds `regex` in `data`, which starts at raw offset `from`. Returns the raw offset just past the match and the
/// matched text.
fn find(regex: &Regex, strip_ansi: bool, from: u64, data: &[u8]) -> Option<(u64, String)> {
    let (m, end) = if strip_ansi {
        let stripped = ansi::strip(data);
        let m = regex.find(&stripped.text)?;
        let end = stripped.raw_end(m.end());
        (text(m.as_bytes()), end)
    } else {
        let m = regex.find(data)?;
        (text(m.as_bytes()), m.end())
    };
    Some((from + end as u64, m))
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_TEXT)]).into_owned()
}

/// At most one probe in flight, started about once a second.
struct Probe<T> {
    inflight: Option<Receiver<T>>,
    next_at: Option<Instant>,
}

impl<T> Default for Probe<T> {
    fn default() -> Self {
        Self {
            inflight: None,
            next_at: None,
        }
    }
}

impl<T: Send + 'static> Probe<T> {
    /// The finished probe's result, if one finished.
    fn take(&mut self) -> Option<T> {
        let rx = self.inflight.as_ref()?;
        match rx.try_recv() {
            Ok(v) => {
                self.inflight = None;
                Some(v)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.inflight = None;
                None
            }
        }
    }

    /// Starts `f` on its own thread when no probe is in flight and the last one started a second ago or more. Each
    /// agent call gets the time left, between 1 and 2 s.
    fn start_if_due(
        &mut self,
        now: Instant,
        deadline: Instant,
        guest: &Arc<dyn Guest>,
        f: impl FnOnce(&dyn Guest, i32) -> T + Send + 'static,
    ) {
        if self.inflight.is_some() || self.next_at.is_some_and(|at| now < at) || now >= deadline {
            return;
        }
        let timeout_secs = if deadline - now >= Duration::from_secs(2) {
            2
        } else {
            1
        };
        let (tx, rx) = mpsc::channel();
        let guest = guest.clone();
        let spawned = std::thread::Builder::new()
            .name("wait-probe".into())
            .spawn(move || {
                let _ = tx.send(f(guest.as_ref(), timeout_secs));
            });
        if spawned.is_ok() {
            self.inflight = Some(rx);
        }
        self.next_at = Some(now + PROBE_EVERY);
    }
}

struct CommandState {
    command: Arc<str>,
    os_checked: bool,
    /// The process started and not yet seen to exit.
    pid: Option<i64>,
    last_code: Option<i64>,
    probe: Probe<CommandStep>,
}

/// What one `command` probe found.
enum CommandStep {
    Windows,
    /// The agent did not answer, or refused the call.
    Unavailable,
    Running(i64),
    Exited {
        code: Option<i64>,
        output: Vec<u8>,
    },
}

impl CommandState {
    fn poll(&mut self, now: Instant, deadline: Instant, guest: &Arc<dyn Guest>) -> Result<Poll> {
        match self.probe.take() {
            Some(CommandStep::Windows) => bail!(
                "command conditions are not available for Windows guests yet; they come with Windows job support \
                 (live-interaction run 3)"
            ),
            Some(CommandStep::Unavailable) => {
                // A failed status call may mean the process is gone, as after an agent restart; start it again.
                self.pid = None;
            }
            Some(CommandStep::Running(pid)) => {
                self.os_checked = true;
                self.pid = Some(pid);
            }
            Some(CommandStep::Exited { code, output }) => {
                self.os_checked = true;
                self.pid = None;
                self.last_code = code;
                if code == Some(0) {
                    return Ok(Poll::Hit {
                        text: text(&output),
                        next: None,
                    });
                }
            }
            None => {}
        }
        let (command, os_checked, pid) = (self.command.clone(), self.os_checked, self.pid);
        self.probe.start_if_due(now, deadline, guest, move |g, t| {
            command_step(g, &command, os_checked, pid, t)
        });
        Ok(Poll::Pending)
    }
}

/// One `command` probe: checks the OS once, starts the command when no run is under way, and asks for its status.
fn command_step(
    guest: &dyn Guest,
    command: &str,
    os_checked: bool,
    pid: Option<i64>,
    timeout_secs: i32,
) -> CommandStep {
    if !os_checked {
        match guest.os(timeout_secs) {
            Ok(GuestOs::Windows) => return CommandStep::Windows,
            Ok(GuestOs::Linux) => {}
            Err(_) => return CommandStep::Unavailable,
        }
    }
    let pid = match pid {
        Some(pid) => pid,
        None => match guest.start(command, timeout_secs) {
            Ok(pid) => pid,
            Err(_) => return CommandStep::Unavailable,
        },
    };
    match guest.status(pid, timeout_secs) {
        Ok(ExecState::Running) => CommandStep::Running(pid),
        Ok(ExecState::Exited { code, output }) => CommandStep::Exited { code, output },
        Err(_) => CommandStep::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::console::ScanResult;

    /// A fake console and clock: output chunks appear at set times, and each sleep moves the clock.
    struct FakeEnv {
        clock: Mutex<Instant>,
        start: Instant,
        /// (time since start, bytes) in time order.
        output: Vec<(Duration, &'static [u8])>,
        cursor: Mutex<u64>,
        generation: u64,
        session_domain: Option<u32>,
        domain: AtomicU32,
        closed_at: Option<Duration>,
    }

    impl FakeEnv {
        fn new(output: Vec<(Duration, &'static [u8])>) -> Self {
            let now = Instant::now();
            Self {
                clock: Mutex::new(now),
                start: now,
                output,
                cursor: Mutex::new(0),
                generation: 7,
                session_domain: Some(1),
                domain: AtomicU32::new(1),
                closed_at: None,
            }
        }

        fn elapsed(&self) -> Duration {
            *self.clock.lock().unwrap() - self.start
        }

        fn buffer(&self) -> Vec<u8> {
            let t = self.elapsed();
            self.output
                .iter()
                .filter(|(at, _)| *at <= t)
                .flat_map(|(_, b)| b.iter().copied())
                .collect()
        }
    }

    impl Env for FakeEnv {
        fn now(&self) -> Instant {
            *self.clock.lock().unwrap()
        }

        fn sleep(&self, d: Duration) {
            *self.clock.lock().unwrap() += d;
            // Lets probe threads, which return at once, finish within one poll.
            std::thread::sleep(Duration::from_millis(1));
        }

        fn scan(&self, since: Option<u64>, generation: Option<u64>) -> Result<SessionScan> {
            if generation.is_some_and(|g| g != self.generation) {
                bail!("offset from an earlier console session");
            }
            let data = self.buffer();
            let from = since
                .unwrap_or(*self.cursor.lock().unwrap())
                .min(data.len() as u64);
            let closed = self
                .closed_at
                .filter(|at| *at <= self.elapsed())
                .map(|_| "stream ended".to_string());
            Ok(SessionScan {
                scan: ScanResult {
                    data: data[from as usize..].to_vec(),
                    from,
                    dropped: 0,
                    closed,
                },
                generation: self.generation,
                domain_id: self.session_domain,
            })
        }

        fn advance_cursor(&self, generation: u64, to: u64) {
            assert_eq!(generation, self.generation);
            let mut c = self.cursor.lock().unwrap();
            *c = (*c).max(to);
        }

        fn domain_id(&self) -> Option<u32> {
            match self.domain.load(Ordering::Relaxed) {
                0 => None,
                id => Some(id),
            }
        }

        fn cancelled(&self) -> bool {
            false
        }

        fn progress(&self, _: Duration) {}
    }

    /// A guest whose agent answers after `ping_after` probes and whose command exits with each code in turn.
    struct FakeGuest {
        os: GuestOs,
        pings: AtomicU32,
        ping_after: u32,
        codes: Mutex<Vec<i64>>,
        starts: AtomicU32,
        /// When set, every call blocks until the sender is dropped.
        hang: Option<Mutex<Receiver<()>>>,
    }

    impl FakeGuest {
        fn new() -> Self {
            Self {
                os: GuestOs::Linux,
                pings: AtomicU32::new(0),
                ping_after: 0,
                codes: Mutex::new(vec![]),
                starts: AtomicU32::new(0),
                hang: None,
            }
        }

        fn block(&self) {
            if let Some(rx) = &self.hang {
                let _ = rx.lock().unwrap().recv();
            }
        }
    }

    impl Guest for FakeGuest {
        fn ping(&self, _: i32) -> bool {
            self.block();
            self.pings.fetch_add(1, Ordering::Relaxed) >= self.ping_after
        }

        fn os(&self, _: i32) -> Result<GuestOs> {
            self.block();
            Ok(self.os)
        }

        fn start(&self, _: &str, _: i32) -> Result<i64> {
            self.block();
            Ok(100 + i64::from(self.starts.fetch_add(1, Ordering::Relaxed)))
        }

        fn status(&self, _: i64, _: i32) -> Result<ExecState> {
            let mut codes = self.codes.lock().unwrap();
            Ok(if codes.is_empty() {
                ExecState::Running
            } else {
                ExecState::Exited {
                    code: Some(codes.remove(0)),
                    output: b"ready\n".to_vec(),
                }
            })
        }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn console(pattern: &str) -> Condition {
        Condition::Console {
            pattern: pattern.into(),
            since: None,
            generation: None,
            strip_ansi: None,
        }
    }

    fn wait(env: &FakeEnv, guest: FakeGuest, conditions: &[Condition], secs: u64) -> Outcome {
        let checks = validate_conditions(&Config::default(), conditions).unwrap();
        run(env, Arc::new(guest), checks, Duration::from_secs(secs)).unwrap()
    }

    fn matched(o: &Outcome) -> (usize, &str, Option<u64>) {
        match o {
            Outcome::Matched {
                index, text, next, ..
            } => (*index, text.as_str(), next.map(|n| n.0)),
            other => panic!("expected a match, got {other:?}"),
        }
    }

    #[test]
    fn a_pattern_split_by_an_escape_sequence_across_scans_maps_to_raw_offsets() {
        let env = FakeEnv::new(vec![
            (ms(0), b"\x1b[1mlog"),
            (ms(100), b"\x1b[0"),
            (ms(200), b"min: \xc3"),
            (ms(300), b"\xa4"),
        ]);
        let o = wait(&env, FakeGuest::new(), &[console("login: ä")], 5);
        // Raw: ESC[1m (4) "log" (3) ESC[0m (4) "in: " (4) then the two bytes of 'ä'.
        assert_eq!(matched(&o), (0, "login: ä", Some(17)));
        assert!(env.elapsed() >= ms(300));
    }

    #[test]
    fn a_timeout_returns_the_start_of_the_last_unfinished_line() {
        let env = FakeEnv::new(vec![(ms(0), b"booting\r\nlog"), (ms(2500), b"in: ")]);
        let o = wait(&env, FakeGuest::new(), &[console("login:")], 1);
        let Outcome::TimedOut { resume, .. } = o else {
            panic!("expected a timeout, got {o:?}")
        };
        assert_eq!(resume, vec![(0, 9, 7)]);
        // The next call resumes there and matches the prompt split across the two calls.
        let again = Condition::Console {
            pattern: "login:".into(),
            since: Some(9),
            generation: Some(7),
            strip_ansi: None,
        };
        let o = wait(&env, FakeGuest::new(), &[again], 5);
        assert_eq!(matched(&o), (0, "login:", Some(15)));
    }

    #[test]
    fn the_lowest_index_wins_and_a_repeated_wait_does_not_rematch() {
        let env = FakeEnv::new(vec![(ms(0), b"$ "), (ms(1000), b"ok\n$ ")]);
        let conds = [console("never"), console(r"\$ "), console(r"\$")];
        let o = wait(&env, FakeGuest::new(), &conds, 5);
        assert_eq!(matched(&o), (1, "$ ", Some(2)));
        // The cursor moved past the first prompt, so the same wait finds the second one.
        let o = wait(&env, FakeGuest::new(), &[console(r"\$ ")], 5);
        assert_eq!(matched(&o), (0, "$ ", Some(7)));
        assert!(env.elapsed() >= ms(1000));
    }

    #[test]
    fn a_closed_session_ends_the_wait_after_its_last_output() {
        let mut env = FakeEnv::new(vec![(ms(0), b"bye\n")]);
        env.closed_at = Some(ms(200));
        let o = wait(&env, FakeGuest::new(), &[console("login:")], 5);
        assert!(
            matches!(o, Outcome::Closed { index: Some(0), ref reason } if reason.contains("stream ended")),
            "{o:?}"
        );
        // Output that arrived before the close still matches.
        let o = wait(&env, FakeGuest::new(), &[console("bye")], 5);
        assert_eq!(matched(&o).0, 0);
    }

    #[test]
    fn a_restarted_or_stopped_vm_ends_the_wait() {
        let env = FakeEnv::new(vec![]);
        env.domain.store(2, Ordering::Relaxed);
        let o = wait(&env, FakeGuest::new(), &[console("login:")], 5);
        assert!(
            matches!(o, Outcome::Closed { index: Some(0), ref reason } if reason.contains("restarted")),
            "{o:?}"
        );
        env.domain.store(0, Ordering::Relaxed);
        let o = wait(&env, FakeGuest::new(), &[Condition::Agent], 5);
        assert!(matches!(o, Outcome::Closed { index: None, .. }), "{o:?}");
    }

    #[test]
    fn a_hung_agent_does_not_stretch_the_call() {
        let (tx, rx) = mpsc::channel();
        let guest = FakeGuest {
            hang: Some(Mutex::new(rx)),
            ..FakeGuest::new()
        };
        let env = FakeEnv::new(vec![(ms(1500), b"Kernel panic - not syncing\n")]);
        let conds = [
            console("login:"),
            console("Kernel panic"),
            Condition::Command {
                command: "false".into(),
            },
        ];
        let o = wait(&env, guest, &conds, 3);
        assert_eq!(matched(&o).0, 1);
        assert!(env.elapsed() <= ms(1500) + POLL, "{:?}", env.elapsed());
        drop(tx);
    }

    #[test]
    fn a_hung_probe_leaves_the_time_bound_intact() {
        let (tx, rx) = mpsc::channel();
        let guest = FakeGuest {
            hang: Some(Mutex::new(rx)),
            ..FakeGuest::new()
        };
        let env = FakeEnv::new(vec![]);
        let o = wait(&env, guest, &[Condition::Agent], 4);
        let Outcome::TimedOut { resume, commands } = o else {
            panic!("expected a timeout, got {o:?}")
        };
        assert!(resume.is_empty() && commands.is_empty());
        assert!(env.elapsed() >= Duration::from_secs(4));
        assert!(env.elapsed() <= Duration::from_secs(4) + POLL);
        drop(tx);
    }

    #[test]
    fn the_agent_condition_holds_once_the_agent_answers() {
        let guest = FakeGuest {
            ping_after: 2,
            ..FakeGuest::new()
        };
        let env = FakeEnv::new(vec![]);
        let o = wait(&env, guest, &[Condition::Agent], 10);
        assert_eq!(matched(&o).0, 0);
        // Probes start about once a second: the third one answers.
        assert!(
            env.elapsed() >= Duration::from_secs(2),
            "{:?}",
            env.elapsed()
        );
    }

    #[test]
    fn a_command_runs_again_after_a_failure_until_it_exits_zero() {
        let guest = Arc::new(FakeGuest {
            codes: Mutex::new(vec![1, 0]),
            ..FakeGuest::new()
        });
        let env = FakeEnv::new(vec![]);
        let checks = validate_conditions(
            &Config::default(),
            &[Condition::Command {
                command: "test -e /tmp/ready".into(),
            }],
        )
        .unwrap();
        let o = run(&env, guest.clone(), checks, Duration::from_secs(10)).unwrap();
        assert_eq!(matched(&o), (0, "ready\n", None));
        assert_eq!(guest.starts.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_running_command_is_reported_and_not_killed_at_the_timeout() {
        let env = FakeEnv::new(vec![]);
        let o = wait(
            &env,
            FakeGuest::new(),
            &[Condition::Command {
                command: "sleep 100".into(),
            }],
            2,
        );
        assert_eq!(
            o,
            Outcome::TimedOut {
                resume: vec![],
                commands: vec![(0, Some(100), None)]
            }
        );
    }

    #[test]
    fn a_command_on_a_windows_guest_is_refused() {
        let guest = FakeGuest {
            os: GuestOs::Windows,
            ..FakeGuest::new()
        };
        let env = FakeEnv::new(vec![]);
        let checks = validate_conditions(
            &Config::default(),
            &[Condition::Command {
                command: "true".into(),
            }],
        )
        .unwrap();
        let err = run(&env, Arc::new(guest), checks, Duration::from_secs(5))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Windows"), "{err}");
    }

    #[test]
    fn validation_refuses_bad_conditions() {
        let refused = |config: &Config, c: Condition| {
            validate_conditions(config, &[c]).err().unwrap().to_string()
        };
        let config = Config::default();
        assert!(refused(&config, console("(")).contains("bad pattern"));
        let stale = Condition::Console {
            pattern: "x".into(),
            since: Some(5),
            generation: None,
            strip_ansi: None,
        };
        assert!(refused(&config, stale).contains("generation"));
        let read_only = Config {
            read_only: true,
            ..Config::default()
        };
        let cmd = Condition::Command {
            command: "true".into(),
        };
        assert!(refused(&read_only, cmd.clone()).contains("read-only"));
        assert!(validate_conditions(&config, &[cmd]).is_ok());
        assert!(validate_conditions(&read_only, &[Condition::Agent]).is_ok());
        assert!(validate_conditions(&config, &[]).is_err());
    }

    #[test]
    fn conditions_parse_from_tagged_json() {
        let c: Vec<Condition> = serde_json::from_value(json!([
            {"kind": "console", "pattern": "login:", "since": 0},
            {"kind": "agent"},
            {"kind": "command", "command": "true"},
        ]))
        .unwrap();
        assert_eq!(
            c.iter().map(Condition::kind).collect::<Vec<_>>(),
            ["console", "agent", "command"]
        );
        assert!(
            serde_json::from_value::<Condition>(json!({"kind": "console", "patern": "x"})).is_err()
        );
    }
}
