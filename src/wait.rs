//! `wait_for`: waiting for the first of several conditions on one VM.
//!
//! The loop polls every 30 ms. Console conditions scan the session buffer on the loop's own thread. Each agent and
//! command condition runs its probes on a thread of its own, at most one at a time and about once a second, so a hung
//! guest agent delays neither the console checks nor the end of the call. A wait with screen conditions captures
//! their display head the same way, on one probe thread every 500 ms, and every screen condition reads that
//! capture. The loop reaches the world only through [`Env`] and [`Guest`], so tests drive it with a fake clock and
//! fake probes.

use std::cell::OnceCell;
use std::fmt;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use image::RgbImage;
use regex::Regex as TextRegex;
use regex::bytes::Regex;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use virt::domain::Domain;

use crate::agent::{Agent, ExecSpec, GuestOs};
use crate::ansi;
use crate::config::Config;
use crate::console::{Consoles, SessionScan};
use crate::display::{self, CheckedRegion, Region};
use crate::jobs;
use uuid::Uuid;

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
/// Most bytes of a job's output one `job_output` probe reads.
const MAX_JOB_READ: usize = 1024 * 1024;
/// How often a wait with screen conditions captures a frame; the interval `max_changed_pixels`'s default was
/// calibrated on.
const FRAME_EVERY: Duration = Duration::from_millis(500);
/// Most pixels two consecutive frames of a settled screen may differ in, by default.
const DEFAULT_MAX_CHANGED_PIXELS: u32 = 600;
/// How long the screen must hold still, by default, in seconds.
const DEFAULT_SETTLE_SECS: f64 = 3.0;
/// The shortest `settle_secs`: one frame interval.
const MIN_SETTLE_SECS: f64 = 0.5;
/// The time past the call's timeout that a screenshot attachment may take, within the call's bound.
const ATTACH_MARGIN: Duration = Duration::from_secs(2);
/// Width that an attached screenshot is downscaled to, as `screenshot` does by default.
const ATTACH_WIDTH: u32 = 1280;

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
    /// A shell command run through the guest agent exits 0. It runs again after a non-zero exit, and may also run
    /// again when a slow agent's reply to the start is lost; it is never killed. Linux guests only. Refused in
    /// read-only mode.
    Command { command: String },
    /// A job has exited, or has ended without writing its status. Linux guests only.
    JobDone {
        /// The job ID that job_start returned.
        job: String,
    },
    /// A job's output from `since` matches `pattern`, a regular expression (Rust `regex` syntax). Linux guests only.
    JobOutput {
        /// The job ID that job_start returned.
        job: String,
        pattern: String,
        /// Byte offset in the job's output to match from, such as `next` from an earlier result. Default 0.
        since: Option<u64>,
    },
    /// The screen has stopped changing: over `settle_secs`, no two consecutive frames, captured every 500 ms,
    /// differ in more than `max_changed_pixels` pixels, compared exactly. A change of frame size counts as a change.
    /// All screen conditions of one wait name the same display head. Refused in read-only mode.
    ScreenSettled {
        /// Most pixels two consecutive frames may differ in while the screen counts as still. Default 600, which
        /// lets a blinking text cursor through.
        max_changed_pixels: Option<u32>,
        /// Seconds the screen must hold still; at least 0.5 and at most the call's timeout less 0.5. Default 3.
        settle_secs: Option<f64>,
        /// Compare only this part of the screen, in pixels of a full-screen image from `screenshot` or a wait's
        /// attachment, with that image's size.
        region: Option<Region>,
        /// Display head index (default 0).
        screen: Option<u32>,
    },
    /// Text read from the screen by OCR matches `pattern`, a regular expression (Rust `regex` syntax), on the
    /// recognised lines joined by newlines. OCR runs on frames captured every 500 ms, only when the screen, or the
    /// region, changed. All screen conditions of one wait name the same display head. Needs `display.ocr_models`;
    /// refused in read-only mode.
    ScreenText {
        pattern: String,
        /// Read only this part of the screen, in pixels of a full-screen image from `screenshot` or a wait's
        /// attachment, with that image's size.
        region: Option<Region>,
        /// Display head index (default 0).
        screen: Option<u32>,
    },
}

impl Condition {
    #[cfg(test)]
    fn kind(&self) -> &'static str {
        match self {
            Condition::Console { .. } => "console",
            Condition::Agent => "agent",
            Condition::Command { .. } => "command",
            Condition::JobDone { .. } => "job_done",
            Condition::JobOutput { .. } => "job_output",
            Condition::ScreenSettled { .. } => "screen_settled",
            Condition::ScreenText { .. } => "screen_text",
        }
    }

    /// The display head a screen condition reads, or `None` for the other kinds.
    fn screen(&self) -> Option<u32> {
        match self {
            Condition::ScreenSettled { screen, .. } | Condition::ScreenText { screen, .. } => {
                Some(screen.unwrap_or(0))
            }
            Condition::Console { .. }
            | Condition::Agent
            | Condition::Command { .. }
            | Condition::JobDone { .. }
            | Condition::JobOutput { .. } => None,
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
    JobDone(Uuid),
    JobOutput {
        job: Uuid,
        regex: Regex,
        since: u64,
    },
    ScreenSettled {
        max_changed_pixels: u32,
        settle: Duration,
        region: Option<CheckedRegion>,
        screen: u32,
    },
    ScreenText {
        regex: TextRegex,
        region: Option<CheckedRegion>,
        screen: u32,
    },
}

/// Refuses a list of conditions the server will not wait for, before any libvirt call: an empty or overlong list, a
/// pattern that does not compile, an offset without its generation, a command or screen condition in read-only
/// mode, `screen_text` without OCR models, screen conditions on different display heads, and a bad region or
/// `settle_secs` for a call of `timeout`.
pub fn validate_conditions(
    config: &Config,
    timeout: Duration,
    conditions: &[Condition],
) -> Result<Vec<Check>> {
    if conditions.is_empty() {
        bail!("name at least one condition");
    }
    if conditions.len() > MAX_CONDITIONS {
        bail!("name at most {MAX_CONDITIONS} conditions");
    }
    // One capture per poll serves every screen condition, so they all read one head.
    let mut heads = conditions
        .iter()
        .enumerate()
        .filter_map(|(i, c)| Some((i, c.screen()?)));
    if let Some((first, head)) = heads.next()
        && let Some((i, other)) = heads.find(|&(_, h)| h != head)
    {
        bail!(
            "condition {i}: all screen conditions of one wait must name the same display head; condition {first} \
             names screen {head} and condition {i} screen {other}"
        );
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
                Condition::JobDone { job } => {
                    Check::JobDone(jobs::parse_id(job).map_err(|e| anyhow!("condition {i}: {e}"))?)
                }
                Condition::JobOutput {
                    job,
                    pattern,
                    since,
                } => Check::JobOutput {
                    job: jobs::parse_id(job).map_err(|e| anyhow!("condition {i}: {e}"))?,
                    regex: Regex::new(pattern)
                        .map_err(|e| anyhow!("condition {i}: bad pattern: {e}"))?,
                    since: since.unwrap_or(0),
                },
                Condition::ScreenSettled {
                    max_changed_pixels,
                    settle_secs,
                    region,
                    screen,
                } => {
                    if config.read_only {
                        bail!("condition {i}: screen conditions are disabled in read-only mode");
                    }
                    Check::ScreenSettled {
                        max_changed_pixels: max_changed_pixels
                            .unwrap_or(DEFAULT_MAX_CHANGED_PIXELS),
                        settle: settle_duration(*settle_secs, timeout)
                            .map_err(|e| anyhow!("condition {i}: {e}"))?,
                        region: region
                            .map(|r| r.check())
                            .transpose()
                            .map_err(|e| anyhow!("condition {i}: {e:#}"))?,
                        screen: screen.unwrap_or(0),
                    }
                }
                Condition::ScreenText {
                    pattern,
                    region,
                    screen,
                } => {
                    if config.read_only {
                        bail!("condition {i}: screen conditions are disabled in read-only mode");
                    }
                    if config.display.ocr_models.is_none() {
                        bail!("condition {i}: {NO_OCR_MODELS}");
                    }
                    Check::ScreenText {
                        regex: TextRegex::new(pattern)
                            .map_err(|e| anyhow!("condition {i}: bad pattern: {e}"))?,
                        region: region
                            .map(|r| r.check())
                            .transpose()
                            .map_err(|e| anyhow!("condition {i}: {e:#}"))?,
                        screen: screen.unwrap_or(0),
                    }
                }
            })
        })
        .collect()
}

/// Why `screen_text` is refused on a server without OCR models.
pub const NO_OCR_MODELS: &str = "screen_text needs the OCR models: set display.ocr_models to the directory that \
     scripts/fetch-ocr-models.sh fills";

/// Checks `settle_secs`: at least one frame interval, and at most the call's `timeout` less one frame interval,
/// because every frame of the wait starts before its end. A zero timeout is left to [`run`], whose refusal says that
/// screen conditions need at least 1 s.
fn settle_duration(settle_secs: Option<f64>, timeout: Duration) -> Result<Duration> {
    let secs = settle_secs.unwrap_or(DEFAULT_SETTLE_SECS);
    if secs.is_nan() || secs < MIN_SETTLE_SECS {
        bail!("settle_secs must be at least {MIN_SETTLE_SECS}, got {secs}");
    }
    let most = timeout.saturating_sub(FRAME_EVERY).as_secs_f64();
    if !timeout.is_zero() && secs > most {
        bail!(
            "settle_secs {secs} is above {most}, the call's timeout of {} s less one {} s frame interval",
            timeout.as_secs(),
            FRAME_EVERY.as_secs_f64()
        );
    }
    Duration::try_from_secs_f64(secs).map_err(|e| anyhow!("settle_secs {secs}: {e}"))
}

/// Everything the loop needs from the server and the clock.
pub trait Env {
    fn now(&self) -> Instant;
    fn sleep(&self, d: Duration);
    /// The VM's console output from `since`, as [`Consoles::scan`] returns it.
    fn scan(&self, since: Option<u64>, generation: Option<u64>) -> Result<SessionScan>;
    /// Moves the wait cursor past a match, as [`Consoles::advance_cursor`] does.
    fn advance_cursor(&self, generation: u64, to: u64);
    fn cancelled(&self) -> bool;
    /// Called every 5 s with the time waited so far.
    fn progress(&self, waited: Duration);
    /// The recogniser `screen_text` reads frames with, loaded once and shared, or `None` without OCR models.
    fn recogniser(&self) -> Option<Arc<dyn Recognise>>;
}

/// Reads the text lines of a frame. Probe threads share one behind an `Arc`, so tests use a fake.
pub trait Recognise: Send + Sync + 'static {
    fn read_lines(&self, frame: &RgbImage) -> Result<Vec<String>>;
}

impl Recognise for display::Recognizer {
    fn read_lines(&self, frame: &RgbImage) -> Result<Vec<String>> {
        display::Recognizer::read_lines(self, frame)
    }
}

/// A process the `command` probe started.
pub enum ExecState {
    Running,
    Exited {
        code: Option<i64>,
        output: Vec<u8>,
    },
    /// The agent does not know the PID, as after an agent restart.
    Gone,
}

/// The calls the probes make, each on a thread of its own. Agent calls are bounded by `timeout_secs`.
pub trait Guest: Send + Sync + 'static {
    /// The running domain's current ID, `None` once it stopped, or an error when libvirt cannot tell.
    fn domain_id(&self) -> Result<Option<u32>>;
    /// The domain's ID as the lookup at the start of the call saw it; makes no call.
    fn initial_id(&self) -> Option<u32>;
    fn ping(&self, timeout_secs: i32) -> bool;
    fn os(&self, timeout_secs: i32) -> Result<GuestOs>;
    fn start(&self, command: &str, timeout_secs: i32) -> Result<i64>;
    fn status(&self, pid: i64, timeout_secs: i32) -> Result<ExecState>;
    /// A job's state and up to `max_bytes` of its output from `since`, or `None` when the guest has no such job.
    fn job_read(
        &self,
        job: &Uuid,
        since: u64,
        max_bytes: usize,
        timeout_secs: i32,
    ) -> Result<Option<jobs::Read>>;
    /// Display head `screen` at full resolution. libvirt bounds the call by no timeout, so the loop never waits on
    /// one.
    fn capture(&self, screen: u32) -> Result<RgbImage>;
}

/// The guest agent of a real domain.
pub struct LiveGuest(pub Domain);

impl Guest for LiveGuest {
    fn initial_id(&self) -> Option<u32> {
        self.0.get_id()
    }

    fn domain_id(&self) -> Result<Option<u32>> {
        // A `Domain` caches its ID at lookup, so only a fresh lookup sees a stop or a restart.
        let conn = self.0.get_connect()?;
        let fresh = match Domain::lookup_by_uuid_string(&conn, &self.0.get_uuid_string()?) {
            Ok(d) => d,
            // A transient domain that stopped, or one deleted during the wait, is gone.
            Err(e) if matches!(e.code(), virt::error::ErrorNumber::NoDomain) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(if fresh.is_active()? {
            fresh.get_id()
        } else {
            None
        })
    }

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
        match Agent::new(&self.0, timeout_secs).status(pid, MAX_TEXT) {
            Ok(None) => Ok(ExecState::Running),
            Ok(Some(o)) => Ok(ExecState::Exited {
                code: o.exit_code,
                output: o.stdout,
            }),
            // qemu-ga's answer for a PID it does not track.
            Err(e) if format!("{e:#}").contains("Invalid parameter") => Ok(ExecState::Gone),
            Err(e) => Err(e),
        }
    }

    fn job_read(
        &self,
        job: &Uuid,
        since: u64,
        max_bytes: usize,
        timeout_secs: i32,
    ) -> Result<Option<jobs::Read>> {
        jobs::read(&Agent::new(&self.0, timeout_secs), job, since, max_bytes)
    }

    fn capture(&self, screen: u32) -> Result<RgbImage> {
        display::capture(&self.0, screen)
    }
}

/// The real environment of a wait on `vm`.
pub struct LiveEnv<'a> {
    pub consoles: &'a Consoles,
    pub vm: &'a str,
    pub cancelled: Box<dyn Fn() -> bool + 'a>,
    pub progress: Box<dyn Fn(Duration) + 'a>,
    pub recogniser: Option<Arc<dyn Recognise>>,
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

    fn cancelled(&self) -> bool {
        (self.cancelled)()
    }

    fn progress(&self, waited: Duration) {
        (self.progress)(waited);
    }

    fn recogniser(&self) -> Option<Arc<dyn Recognise>> {
        self.recogniser.clone()
    }
}

/// How a wait ended.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Matched {
        index: usize,
        kind: &'static str,
        text: String,
        /// For a console or job_output condition: the offset just past the match.
        next: Option<u64>,
        /// For a console condition: the session's generation, to pass with `next`.
        generation: Option<u64>,
        /// For a screen condition: the frame it matched on.
        frame: Option<ScreenFrame>,
    },
    Closed {
        index: Option<usize>,
        reason: String,
    },
    TimedOut {
        /// Each console and job_output condition's resume point: the start of its last unfinished line, and for a
        /// console condition the generation.
        resume: Vec<(usize, u64, Option<u64>)>,
        /// Each command condition's process still running, or the exit code of its last run.
        commands: Vec<(usize, Option<i64>, Option<i64>)>,
        /// Each screen_text condition's last OCR text, cut to the result's text limit, or `None` before any read.
        texts: Vec<(usize, Option<String>)>,
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
                generation,
                frame,
            } => {
                let mut v = json!({
                    "result": "matched", "index": index, "kind": kind, "text": text, "waited_ms": waited_ms,
                });
                if let Some(next) = next {
                    v["next"] = json!(next);
                }
                if let Some(generation) = generation {
                    v["generation"] = json!(generation);
                }
                if let Some(frame) = frame {
                    v["screen"] = json!(frame.screen);
                }
                v
            }
            Outcome::Closed { index, reason } => {
                json!({ "result": "closed", "index": index, "reason": reason, "waited_ms": waited_ms })
            }
            Outcome::TimedOut {
                resume,
                commands,
                texts,
            } => {
                let mut v = json!({
                    "result": "timed_out",
                    "waited_ms": waited_ms,
                    "resume": resume.iter().map(|(index, since, generation)| {
                        let mut point = json!({ "index": index, "since": since });
                        if let Some(generation) = generation {
                            point["generation"] = json!(generation);
                        }
                        point
                    }).collect::<Vec<_>>(),
                    "commands": commands.iter().map(|(index, pid, code)| {
                        json!({ "index": index, "running_pid": pid, "last_exit_code": code })
                    }).collect::<Vec<_>>(),
                });
                // Only a wait with screen_text conditions reports their text, so other results keep their shape.
                if !texts.is_empty() {
                    v["screen_text"] = texts
                        .iter()
                        .map(|(index, text)| json!({ "index": index, "last_text": text }))
                        .collect();
                }
                v
            }
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
    if timeout.is_zero() && checks.iter().any(|c| !matches!(c, Check::Console { .. })) {
        bail!(
            "timeout_secs 0 checks console conditions once; agent, command, job and screen conditions need at \
             least 1 s"
        );
    }
    let start = env.now();
    let deadline = start + timeout;
    // Validation made every screen condition name the same head.
    let mut frames = checks.iter().find_map(Check::screen).map(FrameSource::new);
    let mut states = checks
        .into_iter()
        .map(|c| State::new(c, env))
        .collect::<Result<Vec<_>>>()?;
    let mut vm = Probe::<Result<Option<u32>>>::default();
    // When the VM check in flight started.
    let mut vm_started = None;
    // A failed capture and the time it was seen. A capture fails when the VM stops during it, so the failure ends the
    // wait only once a VM check started after it finds the VM still there; a stopped VM ends it as closed.
    let mut capture_failure: Option<(anyhow::Error, Instant)> = None;
    let mut next_progress = start + PROGRESS_EVERY;
    // Seeded from the call's own lookup, so a restart before the call is caught on the first poll.
    let mut domain_id = guest.initial_id();
    loop {
        let now = env.now();
        if env.cancelled() {
            return Ok(Outcome::Cancelled);
        }
        let checked = vm.take().map(|id| {
            let started: Instant = vm_started.take().expect("a finished VM check had a start");
            (id, started)
        });
        let stopped = match &checked {
            Some((Ok(None), _)) => true,
            Some((Ok(id), _)) => {
                domain_id = *id;
                false
            }
            Some((Err(_), _)) | None => false,
        };
        // A check that started after the failure and did not find the VM stopped confirms the failure.
        let failure_confirmed = !stopped
            && matches!((&checked, &capture_failure), (Some((_, started)), Some((_, at))) if started >= at);
        // A new frame, when one arrived since the last poll; every screen condition reads the same one.
        let frame = match frames.as_mut().filter(|_| !stopped) {
            Some(frames) => match frames.poll(now, deadline, &guest) {
                Some(Ok(frame)) => Some(frame),
                Some(Err(e)) => {
                    capture_failure = Some((e, now));
                    vm.due_now();
                    None
                }
                None => None,
            },
            None => None,
        };
        if !stopped && vm.start_if_due(now, deadline, VM_CHECK_EVERY, &guest, |g, _| g.domain_id())
        {
            vm_started = Some(now);
        }
        // One scan serves every console condition, so all of them see the same output in a poll.
        let scan = console_scan(env, &states);
        // Stripped once, on first use, for every console condition that strips escape sequences.
        let stripped = OnceCell::new();
        let mut closed = None;
        for (index, state) in states.iter_mut().enumerate() {
            // After a stop, console conditions still get one look at the output stored before it.
            if stopped && !matches!(state, State::Console(_)) {
                continue;
            }
            let looks = Looks {
                scan: scan.as_ref(),
                stripped: &stripped,
                frame: frame.as_ref(),
                domain_id,
            };
            match state.poll(&looks, &guest, now, deadline)? {
                Poll::Pending => {}
                Poll::Hit {
                    text,
                    next,
                    generation,
                    frame,
                } => {
                    if let (Some(to), Some(generation)) = (next, generation) {
                        env.advance_cursor(generation, to);
                    }
                    return Ok(Outcome::Matched {
                        index,
                        kind: state.kind(),
                        text,
                        next,
                        generation,
                        frame,
                    });
                }
                Poll::Closed(reason) => {
                    closed.get_or_insert((index, reason));
                }
            }
        }
        if stopped {
            return Ok(Outcome::Closed {
                index: None,
                reason: "the VM is not running".into(),
            });
        }
        if let Some((index, reason)) = closed {
            return Ok(Outcome::Closed {
                index: Some(index),
                reason,
            });
        }
        if failure_confirmed && let Some((e, _)) = capture_failure {
            return Err(e);
        }
        if now >= deadline {
            return at_deadline(&mut states, capture_failure.map(|(e, _)| e));
        }
        if now >= next_progress {
            env.progress(now - start);
            next_progress = now + PROGRESS_EVERY;
        }
        env.sleep(POLL.min(deadline - now));
    }
}

/// How a wait ends at its deadline: a match from a probe that finished after the last poll, as [`timed_out`] finds
/// it, wins as a match in the same poll does; otherwise a capture failure no VM check has settled is returned, and
/// without one the wait times out.
fn at_deadline(states: &mut [State], capture_failure: Option<anyhow::Error>) -> Result<Outcome> {
    let outcome = timed_out(states)?;
    match (capture_failure, &outcome) {
        (Some(e), Outcome::TimedOut { .. }) => Err(e),
        _ => Ok(outcome),
    }
}

fn timed_out(states: &mut [State]) -> Result<Outcome> {
    let mut resume = Vec::new();
    let mut commands = Vec::new();
    let mut texts = Vec::new();
    for (index, state) in states.iter_mut().enumerate() {
        match state {
            State::Console(c) => resume.push((index, c.resume, Some(c.generation))),
            State::Job(j) => {
                // As for commands below, a read that finished after the last poll still counts.
                if let Some(Poll::Hit { text, next, .. }) = j.absorb()? {
                    return Ok(Outcome::Matched {
                        index,
                        kind: j.kind(),
                        text,
                        next,
                        generation: None,
                        frame: None,
                    });
                }
                if j.output.is_some() {
                    resume.push((index, j.from, None));
                }
            }
            State::Command(c) => {
                // A probe that finished after the last poll counts: a success still matches, and a new run is reported.
                if let Some(Poll::Hit { text, .. }) = c.absorb()? {
                    return Ok(Outcome::Matched {
                        index,
                        kind: "command",
                        text,
                        next: None,
                        generation: None,
                        frame: None,
                    });
                }
                commands.push((index, c.pid, c.last_code));
            }
            State::Text(t) => {
                // A read that finished after the last poll counts too; one still running is abandoned.
                if let Some(Poll::Hit { text, frame, .. }) = t.absorb()? {
                    return Ok(Outcome::Matched {
                        index,
                        kind: "screen_text",
                        text,
                        next: None,
                        generation: None,
                        frame,
                    });
                }
                texts.push((index, t.last_text.as_deref().map(cut_text)));
            }
            State::Agent(_) | State::Settled(_) => {}
        }
    }
    Ok(Outcome::TimedOut {
        resume,
        commands,
        texts,
    })
}

/// A screenshot attached to a match, or why the match has none.
#[derive(Debug, PartialEq)]
pub enum Attachment {
    Image {
        png: Vec<u8>,
        width: u32,
        height: u32,
        screen: u32,
    },
    Failed(String),
}

impl Attachment {
    /// Adds the image's size and display head, or the note, to a result's JSON.
    pub fn describe(&self, result: &mut Value) {
        match self {
            Attachment::Image {
                width,
                height,
                screen,
                ..
            } => {
                result["screenshot"] =
                    json!({ "width": width, "height": height, "screen": screen });
            }
            Attachment::Failed(note) => result["screenshot_note"] = json!(note),
        }
    }
}

/// The screenshot a match attaches, downscaled to [`ATTACH_WIDTH`] pixels wide: the frame a screen condition matched
/// on, or for any other match a capture of display head 0 made now. The capture and the encoding run on a probe
/// thread and are given up 2 s after the match or after `deadline`, the end of the wait, whichever is earlier, so the
/// call still returns within its timeout plus 2 s. Returns `None` unless `outcome` is a match.
pub fn attach(
    env: &dyn Env,
    guest: &Arc<dyn Guest>,
    outcome: &Outcome,
    deadline: Instant,
) -> Option<Attachment> {
    let Outcome::Matched { frame, .. } = outcome else {
        return None;
    };
    let (screen, matched) = match frame {
        Some(frame) => (frame.screen, Some(frame.image.clone())),
        None => (0, None),
    };
    let take = move |g: &dyn Guest| {
        let image = match matched {
            Some(image) => image,
            None => Arc::new(
                g.capture(screen)
                    .with_context(|| format!("capturing display head {screen}"))?,
            ),
        };
        display::encode_png(&image, None, Some(ATTACH_WIDTH)).context("encoding the screenshot")
    };
    Some(match within_margin(env, guest, deadline, take) {
        Ok((png, width, height)) => Attachment::Image {
            png,
            width,
            height,
            screen,
        },
        Err(e) => Attachment::Failed(format!("no screenshot attached: {e:#}")),
    })
}

/// Runs `f` on a probe thread, waiting until 2 s past now or past `deadline`, whichever is earlier.
fn within_margin<T: Send + 'static>(
    env: &dyn Env,
    guest: &Arc<dyn Guest>,
    deadline: Instant,
    f: impl FnOnce(&dyn Guest) -> Result<T> + Send + 'static,
) -> Result<T> {
    let now = env.now();
    let limit = now.min(deadline) + ATTACH_MARGIN;
    let mut probe = Probe::default();
    if !probe.start_if_due(now, limit, Duration::ZERO, guest, move |g, _| f(g)) {
        bail!("could not start taking the screenshot");
    }
    loop {
        if let Some(result) = probe.take() {
            return result;
        }
        let now = env.now();
        if now >= limit {
            bail!(
                "taking the screenshot did not finish within {} s",
                ATTACH_MARGIN.as_secs()
            );
        }
        env.sleep(POLL.min(limit - now));
    }
}

/// The session's output from the earliest start of any console condition, or `None` without console conditions.
fn console_scan(env: &dyn Env, states: &[State]) -> Option<Result<SessionScan>> {
    let consoles = states.iter().filter_map(|s| match s {
        State::Console(c) => Some(c),
        _ => None,
    });
    let (from, generation) = consoles.fold(None, |acc: Option<(u64, u64)>, c| {
        Some(acc.map_or((c.from, c.generation), |(f, g)| (f.min(c.from), g)))
    })?;
    Some(env.scan(Some(from), Some(generation)))
}

enum Poll {
    Pending,
    Hit {
        text: String,
        next: Option<u64>,
        generation: Option<u64>,
        frame: Option<ScreenFrame>,
    },
    Closed(String),
}

impl Poll {
    fn hit(text: String) -> Self {
        Poll::Hit {
            text,
            next: None,
            generation: None,
            frame: None,
        }
    }
}

/// What one poll observed, shared by every condition: the console scan, its stripped text, the new frame, and the
/// domain's current ID.
struct Looks<'a> {
    scan: Option<&'a Result<SessionScan>>,
    stripped: &'a OnceCell<ansi::Stripped>,
    frame: Option<&'a Captured>,
    domain_id: Option<u32>,
}

/// A frame of the display head that a wait's screen conditions read, with the time its capture started.
struct Captured {
    image: Arc<RgbImage>,
    at: Instant,
}

/// A frame a screen condition matched on, kept for the screenshot attachment.
#[derive(Clone, PartialEq)]
pub struct ScreenFrame {
    pub screen: u32,
    pub image: Arc<RgbImage>,
}

impl fmt::Debug for ScreenFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (w, h) = self.image.dimensions();
        write!(
            f,
            "ScreenFrame {{ screen: {}, size: {w}x{h} }}",
            self.screen
        )
    }
}

/// Captures the display head of a wait's screen conditions on a probe thread, every [`FRAME_EVERY`].
struct FrameSource {
    screen: u32,
    probe: Probe<Result<RgbImage>>,
    /// When the capture in flight started.
    started: Option<Instant>,
    /// Set once a capture failed; no capture starts after it.
    failed: bool,
}

impl FrameSource {
    fn new(screen: u32) -> Self {
        Self {
            screen,
            probe: Probe::default(),
            started: None,
            failed: false,
        }
    }

    /// The capture that finished since the last poll, if any, and starts the next one when it is due. After a
    /// failed capture it starts none. One still in flight at the deadline is left to finish on its own thread, and
    /// its frame is dropped.
    fn poll(
        &mut self,
        now: Instant,
        deadline: Instant,
        guest: &Arc<dyn Guest>,
    ) -> Option<Result<Captured>> {
        let frame = self.probe.take().map(|image| {
            let at = self.started.take().expect("a finished capture had a start");
            let image = image.with_context(|| format!("capturing display head {}", self.screen))?;
            Ok(Captured {
                image: Arc::new(image),
                at,
            })
        });
        self.failed |= matches!(frame, Some(Err(_)));
        if self.failed {
            return frame;
        }
        let screen = self.screen;
        if self
            .probe
            .start_if_due(now, deadline, FRAME_EVERY, guest, move |g, _| {
                g.capture(screen)
            })
        {
            self.started = Some(now);
        }
        frame
    }
}

enum State {
    Console(ConsoleState),
    Agent(Probe<bool>),
    Command(CommandState),
    Job(JobState),
    Settled(SettleState),
    Text(TextState),
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
            Check::JobDone(job) => State::Job(JobState::new(job, None, 0)),
            Check::JobOutput { job, regex, since } => {
                State::Job(JobState::new(job, Some(regex), since))
            }
            Check::ScreenSettled {
                max_changed_pixels,
                settle,
                region,
                screen,
            } => State::Settled(SettleState {
                screen,
                settle: Settle::new(max_changed_pixels, settle, region),
            }),
            Check::ScreenText {
                regex,
                region,
                screen,
            } => State::Text(TextState::new(
                regex,
                region,
                screen,
                env.recogniser().ok_or_else(|| anyhow!(NO_OCR_MODELS))?,
            )),
        })
    }

    fn kind(&self) -> &'static str {
        match self {
            State::Console(_) => "console",
            State::Agent(_) => "agent",
            State::Command(_) => "command",
            State::Job(j) => j.kind(),
            State::Settled(_) => "screen_settled",
            State::Text(_) => "screen_text",
        }
    }

    fn poll(
        &mut self,
        looks: &Looks<'_>,
        guest: &Arc<dyn Guest>,
        now: Instant,
        deadline: Instant,
    ) -> Result<Poll> {
        match self {
            State::Console(c) => Ok(match looks.scan {
                Some(scan) => c.poll(scan, looks.stripped, looks.domain_id),
                None => Poll::Pending,
            }),
            State::Settled(s) => match looks.frame {
                Some(frame) => s.poll(frame),
                None => Ok(Poll::Pending),
            },
            State::Text(t) => t.poll(looks.frame, now, deadline, guest),
            State::Agent(p) => {
                if p.take() == Some(true) {
                    return Ok(Poll::hit("the guest agent answered".into()));
                }
                p.start_if_due(now, deadline, PROBE_EVERY, guest, |g, t| g.ping(t));
                Ok(Poll::Pending)
            }
            State::Command(c) => c.poll(now, deadline, guest),
            State::Job(j) => j.poll(now, deadline, guest),
        }
    }
}

impl Check {
    /// Whether the check needs the OCR recogniser.
    pub fn reads_text(&self) -> bool {
        matches!(self, Check::ScreenText { .. })
    }

    /// The display head a screen check reads, or `None` for the other kinds.
    fn screen(&self) -> Option<u32> {
        match self {
            Check::ScreenSettled { screen, .. } | Check::ScreenText { screen, .. } => Some(*screen),
            Check::Console { .. }
            | Check::Agent
            | Check::Command(_)
            | Check::JobDone(_)
            | Check::JobOutput { .. } => None,
        }
    }
}

/// A `screen_settled` condition.
struct SettleState {
    screen: u32,
    settle: Settle,
}

impl SettleState {
    fn poll(&mut self, frame: &Captured) -> Result<Poll> {
        let Some(held) = self.settle.push(frame.at, &frame.image)? else {
            return Ok(Poll::Pending);
        };
        Ok(Poll::Hit {
            text: format!(
                "the screen held still for {:.1} s, with at most {} pixels changed between frames",
                held.as_secs_f64(),
                self.settle.max_changed_pixels
            ),
            next: None,
            generation: None,
            frame: Some(ScreenFrame {
                screen: self.screen,
                image: frame.image.clone(),
            }),
        })
    }
}

/// A `screen_text` condition. It runs the recogniser on its own probe thread, one read at a time, and only on a frame
/// whose hash, on the region when one is given, differs from the one it last read. A read still running at the
/// deadline, or when the wait is cancelled, is abandoned and its result dropped; `ocrs` cannot stop a run.
struct TextState {
    regex: TextRegex,
    region: Option<CheckedRegion>,
    screen: u32,
    recogniser: Arc<dyn Recognise>,
    /// The hash of the frame, or region, last handed to the recogniser.
    read_hash: Option<u64>,
    probe: Probe<Result<String>>,
    /// The frame the read in flight reads, for the attachment.
    reading: Option<Arc<RgbImage>>,
    /// The text of the last finished read.
    last_text: Option<String>,
}

impl TextState {
    fn new(
        regex: TextRegex,
        region: Option<CheckedRegion>,
        screen: u32,
        recogniser: Arc<dyn Recognise>,
    ) -> Self {
        Self {
            regex,
            region,
            screen,
            recogniser,
            read_hash: None,
            probe: Probe::default(),
            reading: None,
            last_text: None,
        }
    }

    /// Takes in a finished read. Returns a hit when its text matches.
    fn absorb(&mut self) -> Result<Option<Poll>> {
        let Some(read) = self.probe.take() else {
            return Ok(None);
        };
        let frame = self.reading.take().expect("a finished read had a frame");
        let text = read.context("reading text from the screen")?;
        let hit = self.regex.find(&text).map(|m| Poll::Hit {
            text: cut_text(m.as_str()),
            next: None,
            generation: None,
            frame: Some(ScreenFrame {
                screen: self.screen,
                image: frame,
            }),
        });
        self.last_text = Some(text);
        Ok(hit)
    }

    fn poll(
        &mut self,
        frame: Option<&Captured>,
        now: Instant,
        deadline: Instant,
        guest: &Arc<dyn Guest>,
    ) -> Result<Poll> {
        if let Some(hit) = self.absorb()? {
            return Ok(hit);
        }
        // A frame that arrives during a read is skipped; the next one starts a read if it still differs.
        let Some(frame) = frame.filter(|_| !self.probe.busy()) else {
            return Ok(Poll::Pending);
        };
        let area = self
            .region
            .map(|r| r.on_frame(frame.image.width(), frame.image.height()))
            .transpose()?;
        let hash = display::area_hash(&frame.image, area);
        if self.read_hash == Some(hash) {
            return Ok(Poll::Pending);
        }
        let (recogniser, image) = (self.recogniser.clone(), frame.image.clone());
        let started = self
            .probe
            .start_if_due(now, deadline, Duration::ZERO, guest, move |_, _| {
                let lines = recogniser.read_lines(&display::area_image(&image, area))?;
                Ok(lines.join("\n"))
            });
        if started {
            self.read_hash = Some(hash);
            self.reading = Some(frame.image.clone());
        }
        Ok(Poll::Pending)
    }
}

/// `text` cut to the result's text limit, at a character boundary.
fn cut_text(text: &str) -> String {
    text[..text.floor_char_boundary(MAX_TEXT)].to_owned()
}

/// Whether a sequence of frames has settled: it keeps the previous frame and the time of the frame that last changed
/// beyond the threshold.
struct Settle {
    max_changed_pixels: u32,
    settle: Duration,
    region: Option<CheckedRegion>,
    previous: Option<Arc<RgbImage>>,
    /// The capture time of the first frame, or of the last frame that differed from the one before it in more than
    /// `max_changed_pixels` pixels or in size.
    still_since: Option<Instant>,
}

impl Settle {
    fn new(max_changed_pixels: u32, settle: Duration, region: Option<CheckedRegion>) -> Self {
        Self {
            max_changed_pixels,
            settle,
            region,
            previous: None,
            still_since: None,
        }
    }

    /// Takes in the frame captured at `at`, later than the one before. Returns how long the screen has held still
    /// once that is at least `settle`.
    fn push(&mut self, at: Instant, frame: &Arc<RgbImage>) -> Result<Option<Duration>> {
        let changed = match &self.previous {
            None => true,
            Some(previous) => {
                let area = self
                    .region
                    .map(|r| r.on_frame(frame.width(), frame.height()))
                    .transpose()?;
                display::changed_pixels(previous, frame, area)
                    .is_none_or(|n| n > u64::from(self.max_changed_pixels))
            }
        };
        self.previous = Some(frame.clone());
        if changed {
            self.still_since = Some(at);
        }
        let held = at - self.still_since.expect("set by the first frame");
        Ok((held >= self.settle).then_some(held))
    }
}

/// A `job_done` condition, or with `output` a `job_output` condition. Each probe reads the job's files once.
struct JobState {
    job: Uuid,
    output: Option<Regex>,
    /// Where the next read of the job's output starts: the start of the last unfinished line read so far.
    from: u64,
    os_checked: bool,
    probe: Probe<JobStep>,
}

/// What one job probe found.
enum JobStep {
    Windows,
    Missing,
    /// The agent did not answer; the next probe tries again.
    Unavailable,
    Read {
        state: jobs::State,
        data: Vec<u8>,
        from: u64,
        cut: bool,
    },
}

impl JobState {
    fn new(job: Uuid, output: Option<Regex>, since: u64) -> Self {
        Self {
            job,
            output,
            from: since,
            os_checked: false,
            probe: Probe::default(),
        }
    }

    fn kind(&self) -> &'static str {
        if self.output.is_some() {
            "job_output"
        } else {
            "job_done"
        }
    }

    /// Takes in a finished probe's result. Returns a hit when the condition holds.
    fn absorb(&mut self) -> Result<Option<Poll>> {
        let Some(step) = self.probe.take() else {
            return Ok(None);
        };
        let (state, data, from, cut) = match step {
            JobStep::Windows => bail!(
                "job conditions are not available for Windows guests yet; they come with Windows job support \
                 (live-interaction run 3)"
            ),
            JobStep::Missing => bail!("no job {} in this guest", self.job),
            JobStep::Unavailable => return Ok(None),
            JobStep::Read {
                state,
                data,
                from,
                cut,
            } => (state, data, from, cut),
        };
        self.os_checked = true;
        let Some(regex) = &self.output else {
            return Ok(match state {
                jobs::State::Exited(code) => Some(Poll::hit(format!("exited with code {code}"))),
                jobs::State::EndedWithoutStatus => Some(Poll::hit("ended without status".into())),
                jobs::State::Starting | jobs::State::Running => None,
            });
        };
        if let Some(m) = regex.find(&data) {
            return Ok(Some(Poll::Hit {
                text: text(m.as_bytes()),
                next: Some(from + m.end() as u64),
                generation: None,
                frame: None,
            }));
        }
        // Read on from the last unfinished line, so a match split across reads is found. A cut read with no line end
        // moves past it, so one long line cannot hold the reads in place.
        self.from = match data.iter().rposition(|&b| b == b'\n') {
            Some(p) => from + p as u64 + 1,
            None if cut => from + data.len() as u64,
            None => from,
        };
        Ok(None)
    }

    fn poll(&mut self, now: Instant, deadline: Instant, guest: &Arc<dyn Guest>) -> Result<Poll> {
        if let Some(hit) = self.absorb()? {
            return Ok(hit);
        }
        let (job, from, os_checked) = (self.job, self.from, self.os_checked);
        // job_done needs only the state; reading one byte keeps its probe cheap.
        let max = if self.output.is_some() {
            MAX_JOB_READ
        } else {
            1
        };
        self.probe
            .start_if_due(now, deadline, PROBE_EVERY, guest, move |g, t| {
                job_step(g, &job, from, max, os_checked, t)
            });
        Ok(Poll::Pending)
    }
}

/// One job probe: checks the OS once, then reads the job's state and output from `from`.
fn job_step(
    guest: &dyn Guest,
    job: &Uuid,
    from: u64,
    max: usize,
    os_checked: bool,
    timeout_secs: i32,
) -> JobStep {
    if !os_checked {
        match guest.os(timeout_secs) {
            Ok(GuestOs::Windows) => return JobStep::Windows,
            Ok(GuestOs::Linux) => {}
            Err(_) => return JobStep::Unavailable,
        }
    }
    match guest.job_read(job, from, max, timeout_secs) {
        Ok(Some(r)) => JobStep::Read {
            state: r.state,
            data: r.data,
            from,
            cut: r.cut,
        },
        Ok(None) => JobStep::Missing,
        Err(_) => JobStep::Unavailable,
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
    /// Matches this condition against `scan`, a scan that starts at or before `self.from`. `stripped` holds the
    /// whole scan with escape sequences stripped, made on first use and shared by the poll's console conditions.
    fn poll(
        &mut self,
        scan: &Result<SessionScan>,
        stripped: &OnceCell<ansi::Stripped>,
        domain_id: Option<u32>,
    ) -> Poll {
        // The scan names the pinned generation, so a console reopened during the wait fails it too.
        let scan = match scan {
            Ok(s) => s,
            Err(e) => {
                return Poll::Closed(format!("the console session ended or was reopened: {e:#}"));
            }
        };
        // Output stored before a restart still counts, so match first.
        let restarted = domain_id.is_some() && self.domain_id != domain_id;
        if let Some(hit) = self.find_new(scan, stripped) {
            return hit;
        }
        if restarted {
            return Poll::Closed(
                "the VM restarted after console_open; open the console again".into(),
            );
        }
        match &scan.scan.closed {
            Some(reason) => Poll::Closed(format!("the console session closed: {reason}")),
            None => Poll::Pending,
        }
    }

    /// Matches the output that arrived since the last scan, and moves the resume point.
    fn find_new(
        &mut self,
        scan: &SessionScan,
        stripped: &OnceCell<ansi::Stripped>,
    ) -> Option<Poll> {
        // When the scrollback dropped output, match from what it still holds.
        self.from = self.from.max(scan.scan.from);
        let skip = usize::try_from(self.from - scan.scan.from).unwrap_or(usize::MAX);
        let data = scan.scan.data.get(skip..).unwrap_or_default();
        let end = self.from + data.len() as u64;
        if self.seen_end != Some(end) {
            self.seen_end = Some(end);
            self.resume = self.from
                + data
                    .iter()
                    .rposition(|&b| b == b'\n')
                    .map_or(0, |p| p as u64 + 1);
            let found = if self.strip_ansi {
                let stripped = stripped.get_or_init(|| ansi::strip(&scan.scan.data));
                find_stripped(&self.regex, stripped, scan.scan.from, self.from)
            } else {
                find(&self.regex, self.from, data)
            };
            if let Some((next, text)) = found {
                return Some(Poll::Hit {
                    text,
                    next: Some(next),
                    generation: Some(self.generation),
                    frame: None,
                });
            }
        }
        None
    }
}

/// Finds `regex` in raw `data`, which starts at raw offset `from`. Returns the raw offset just past the match and the
/// matched text.
fn find(regex: &Regex, from: u64, data: &[u8]) -> Option<(u64, String)> {
    let m = regex.find(data)?;
    Some((from + m.end() as u64, text(m.as_bytes())))
}

/// Finds `regex` in the stripped text of a scan that starts at raw offset `scan_from`, from the first kept byte at or
/// after raw offset `from`. Returns the raw offset just past the match and the matched text.
fn find_stripped(
    regex: &Regex,
    stripped: &ansi::Stripped,
    scan_from: u64,
    from: u64,
) -> Option<(u64, String)> {
    let skip = usize::try_from(from - scan_from).unwrap_or(usize::MAX);
    let start = stripped.origin.partition_point(|&o| o < skip);
    let m = regex.find(&stripped.text[start..])?;
    let end = if m.end() == 0 {
        from
    } else {
        scan_from + stripped.raw_end(start + m.end()) as u64
    };
    Some((end, text(m.as_bytes())))
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
    /// Makes the next probe due at once; it starts once none is in flight.
    fn due_now(&mut self) {
        self.next_at = None;
    }

    /// Whether a probe is in flight.
    fn busy(&self) -> bool {
        self.inflight.is_some()
    }

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

    /// Starts `f` on its own thread when no probe is in flight and the last one started `every` ago or more, and
    /// returns whether it started. Each agent call gets the time left, between 1 and 2 s.
    fn start_if_due(
        &mut self,
        now: Instant,
        deadline: Instant,
        every: Duration,
        guest: &Arc<dyn Guest>,
        f: impl FnOnce(&dyn Guest, i32) -> T + Send + 'static,
    ) -> bool {
        if self.inflight.is_some() || self.next_at.is_some_and(|at| now < at) || now >= deadline {
            return false;
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
        self.next_at = Some(now + every);
        if spawned.is_ok() {
            self.inflight = Some(rx);
            true
        } else {
            false
        }
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
    /// The agent did not answer the OS check or the start; nothing was started.
    Unavailable,
    /// A run is under way, or its status could not be read this time; it is polled again, never started twice.
    Running(i64),
    Exited {
        code: Option<i64>,
        output: Vec<u8>,
    },
    /// The agent no longer knows the run's PID, so the next probe starts the command again.
    Gone,
}

impl CommandState {
    /// Takes in a finished probe's result. Returns a hit when the command exited 0.
    fn absorb(&mut self) -> Result<Option<Poll>> {
        match self.probe.take() {
            Some(CommandStep::Windows) => bail!(
                "command conditions are not available for Windows guests yet; they come with Windows job support \
                 (live-interaction run 3)"
            ),
            Some(CommandStep::Unavailable) | None => {}
            Some(CommandStep::Gone) => self.pid = None,
            Some(CommandStep::Running(pid)) => {
                self.os_checked = true;
                self.pid = Some(pid);
            }
            Some(CommandStep::Exited { code, output }) => {
                self.os_checked = true;
                self.pid = None;
                self.last_code = code;
                if code == Some(0) {
                    return Ok(Some(Poll::hit(text(&output))));
                }
            }
        }
        Ok(None)
    }

    fn poll(&mut self, now: Instant, deadline: Instant, guest: &Arc<dyn Guest>) -> Result<Poll> {
        if let Some(hit) = self.absorb()? {
            return Ok(hit);
        }
        let (command, os_checked, pid) = (self.command.clone(), self.os_checked, self.pid);
        self.probe
            .start_if_due(now, deadline, PROBE_EVERY, guest, move |g, t| {
                command_step(g, &command, os_checked, pid, t)
            });
        Ok(Poll::Pending)
    }
}

/// One `command` probe: checks the OS once, starts the command when no run is under way, and asks for its status. A
/// start whose reply is lost can still leave a run the probe does not know about; nothing else starts a second run.
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
        Ok(ExecState::Running) | Err(_) => CommandStep::Running(pid),
        Ok(ExecState::Exited { code, output }) => CommandStep::Exited { code, output },
        Ok(ExecState::Gone) => CommandStep::Gone,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::console::ScanResult;

    #[test]
    fn a_stray_osc_start_does_not_hide_a_later_match() {
        let regex = Regex::new("login: ").unwrap();
        let data = b"\x1b]noise\nDebian login: ";
        assert_eq!(
            find_stripped(&regex, &ansi::strip(data), 100, 100),
            Some((100 + data.len() as u64, "login: ".to_string()))
        );
    }

    #[test]
    fn one_stripped_scan_serves_every_start() {
        // Each start is a sequence boundary, as a condition's start is: a write offset or the end of a match.
        let data = b"\x1b[1mboot\x1b[0m ok\r\nlogin: \x1b]0;t\x07root\r\n# \x1b[32mdone\x1b[0m\n";
        let shared = ansi::strip(data);
        let regex = Regex::new("o+").unwrap();
        for start in [0, 4, 8, 12, 13, 16, 17, 24, 30, 36, 38, 43, 47, 51] {
            let span = ansi::strip(&data[start..]);
            let alone = regex.find(&span.text).map(|m| {
                (
                    40 + (start + span.raw_end(m.end())) as u64,
                    text(m.as_bytes()),
                )
            });
            assert_eq!(
                find_stripped(&regex, &shared, 40, 40 + start as u64),
                alone,
                "start {start}"
            );
        }
    }

    /// A fake console and clock: output chunks appear at set times, and each sleep moves the clock.
    struct FakeEnv {
        clock: Mutex<Instant>,
        start: Instant,
        /// (time since start, bytes) in time order.
        output: Vec<(Duration, &'static [u8])>,
        cursor: Mutex<u64>,
        generation: u64,
        session_domain: Option<u32>,
        closed_at: Option<Duration>,
        /// When set, the console is reopened at this time and takes the next generation.
        reopened_at: Option<Duration>,
        cancel_at: Option<Duration>,
        progress: Mutex<Vec<Duration>>,
        /// When set, each sleep also lasts as long in real time, for probes that do real work, such as encoding.
        real_sleep: bool,
        recogniser: Option<Arc<dyn Recognise>>,
        /// When set, called in each sleep with the number of sleeps before it, after the clock moved, so a test can
        /// hold the loop until its probes have delivered.
        on_sleep: Option<Box<dyn Fn(u32) + Send + Sync>>,
        sleeps: AtomicU32,
    }

    /// Counts the probe threads that have exited. A thread-local's destructor runs when its thread exits, after the
    /// probe's closure has sent its result, so once a count is reached those results wait in their channels.
    #[derive(Clone, Default)]
    struct Exits(Arc<(Mutex<u32>, std::sync::Condvar)>);

    struct ExitGuard(Exits);

    impl Drop for ExitGuard {
        fn drop(&mut self) {
            let (count, changed) = &*(self.0).0;
            *count.lock().unwrap() += 1;
            changed.notify_all();
        }
    }

    thread_local! {
        static ON_EXIT: std::cell::RefCell<Option<ExitGuard>> = const { std::cell::RefCell::new(None) };
    }

    impl Exits {
        /// Counts the current thread when it exits.
        fn on_exit(&self) {
            ON_EXIT.with(|guard| *guard.borrow_mut() = Some(ExitGuard(self.clone())));
        }

        /// Waits until `n` counted threads have exited.
        fn wait_for(&self, n: u32) {
            let (count, changed) = &*self.0;
            let (count, timeout) = changed
                .wait_timeout_while(count.lock().unwrap(), Duration::from_secs(10), |k| *k < n)
                .unwrap();
            assert!(!timeout.timed_out(), "only {} of {n} probes exited", *count);
        }
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
                closed_at: None,
                reopened_at: None,
                cancel_at: None,
                progress: Mutex::default(),
                real_sleep: false,
                recogniser: None,
                on_sleep: None,
                sleeps: AtomicU32::new(0),
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
            if let Some(hook) = &self.on_sleep {
                hook(self.sleeps.fetch_add(1, Ordering::SeqCst));
            }
            // Lets probe threads, which return at once, finish within one poll.
            std::thread::sleep(if self.real_sleep {
                d
            } else {
                Duration::from_millis(1)
            });
        }

        fn scan(&self, since: Option<u64>, generation: Option<u64>) -> Result<SessionScan> {
            let current = if self.reopened_at.is_some_and(|at| at <= self.elapsed()) {
                self.generation + 1
            } else {
                self.generation
            };
            if generation.is_some_and(|g| g != current) {
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
                    closed,
                },
                generation: current,
                domain_id: self.session_domain,
            })
        }

        fn advance_cursor(&self, generation: u64, to: u64) {
            assert_eq!(generation, self.generation);
            let mut c = self.cursor.lock().unwrap();
            *c = (*c).max(to);
        }

        fn cancelled(&self) -> bool {
            self.cancel_at.is_some_and(|at| self.elapsed() >= at)
        }

        fn progress(&self, waited: Duration) {
            self.progress.lock().unwrap().push(waited);
        }

        fn recogniser(&self) -> Option<Arc<dyn Recognise>> {
            self.recogniser.clone()
        }
    }

    /// A guest whose agent answers after `ping_after` probes and whose command exits with each code in turn.
    struct FakeGuest {
        os: GuestOs,
        /// The running domain's ID; 0 means stopped.
        domain: AtomicU32,
        pings: AtomicU32,
        ping_after: u32,
        codes: Mutex<Vec<i64>>,
        starts: AtomicU32,
        /// Status calls that fail, as a timed-out agent call does, before any succeeds.
        status_errors: AtomicU32,
        /// When set, every agent call blocks until the sender is dropped.
        hang: Option<Mutex<Receiver<()>>>,
        /// The job's state and whole output as each read sees it; the last entry repeats. Empty means no job.
        job: Mutex<Vec<(jobs::State, &'static [u8])>>,
        /// The `since` of each job read.
        job_reads: Mutex<Vec<u64>>,
        /// The frame each capture returns in turn; the last repeats. Empty means every capture fails, as on a VM
        /// without graphics.
        frames: Mutex<Vec<RgbImage>>,
        /// The display head of each capture.
        captures: Mutex<Vec<u32>>,
        /// When set, every capture blocks until the sender is dropped.
        capture_hang: Option<Mutex<Receiver<()>>>,
        /// When set, the capture with this index stops the domain and fails, as a capture does when the VM stops
        /// during it. It first waits until `stop_after_checks` VM checks have read the domain.
        stops_at_capture: Option<usize>,
        stop_after_checks: u32,
        /// The VM checks that have read the domain; `initial_id` is not one.
        checks: AtomicU32,
        /// When set, the VM check with this index reads the domain, then blocks until the sender is dropped before
        /// returning what it read.
        hold_check: Option<(u32, Mutex<Receiver<()>>)>,
        /// When set, a VM check of a stopped domain fails, as when libvirt cannot tell.
        check_fails_once_stopped: bool,
        /// When set, counts each capture's probe thread when it exits.
        exits: Option<Exits>,
    }

    impl FakeGuest {
        fn new() -> Self {
            Self {
                os: GuestOs::Linux,
                domain: AtomicU32::new(1),
                pings: AtomicU32::new(0),
                ping_after: 0,
                codes: Mutex::new(vec![]),
                starts: AtomicU32::new(0),
                status_errors: AtomicU32::new(0),
                hang: None,
                job: Mutex::new(vec![]),
                job_reads: Mutex::new(vec![]),
                frames: Mutex::new(vec![]),
                captures: Mutex::new(vec![]),
                capture_hang: None,
                stops_at_capture: None,
                stop_after_checks: 0,
                checks: AtomicU32::new(0),
                hold_check: None,
                check_fails_once_stopped: false,
                exits: None,
            }
        }

        /// The domain's ID as `domain_id` reports it, without counting a check.
        fn read_domain(&self) -> Option<u32> {
            match self.domain.load(Ordering::SeqCst) {
                0 => None,
                id => Some(id),
            }
        }

        /// A guest whose captures return `frames` in turn.
        fn showing(frames: Vec<RgbImage>) -> Self {
            Self {
                frames: Mutex::new(frames),
                ..Self::new()
            }
        }

        fn block(&self) {
            if let Some(rx) = &self.hang {
                let _ = rx.lock().unwrap().recv();
            }
        }
    }

    impl Guest for FakeGuest {
        fn initial_id(&self) -> Option<u32> {
            self.read_domain()
        }

        fn domain_id(&self) -> Result<Option<u32>> {
            let id = self.read_domain();
            let index = self.checks.fetch_add(1, Ordering::SeqCst);
            if let Some((held, rx)) = &self.hold_check
                && *held == index
            {
                let _ = rx.lock().unwrap().recv();
            }
            if id.is_none() && self.check_fails_once_stopped {
                bail!("cannot reach libvirt");
            }
            Ok(id)
        }

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
            let failing = self.status_errors.load(Ordering::Relaxed);
            if failing > 0 {
                self.status_errors.store(failing - 1, Ordering::Relaxed);
                bail!("guest agent 'guest-exec-status' failed: timed out");
            }
            let mut codes = self.codes.lock().unwrap();
            Ok(match codes.first().copied() {
                None => ExecState::Running,
                // -1 stands for an agent that no longer knows the PID.
                Some(-1) => {
                    codes.remove(0);
                    ExecState::Gone
                }
                Some(code) => {
                    codes.remove(0);
                    ExecState::Exited {
                        code: Some(code),
                        output: b"ready\n".to_vec(),
                    }
                }
            })
        }

        fn job_read(
            &self,
            _: &Uuid,
            since: u64,
            max_bytes: usize,
            _: i32,
        ) -> Result<Option<jobs::Read>> {
            self.job_reads.lock().unwrap().push(since);
            let mut job = self.job.lock().unwrap();
            let Some(&(state, output)) = job.first() else {
                return Ok(None);
            };
            if job.len() > 1 {
                job.remove(0);
            }
            let from = (since as usize).min(output.len());
            // Trimmed like the real read, so a cut UTF-8 character shortens it.
            let mut data = output[from..(from + max_bytes).min(output.len())].to_vec();
            let cut = jobs::trim_read(&mut data, max_bytes);
            Ok(Some(jobs::Read {
                state,
                next: since + data.len() as u64,
                data,
                cut,
            }))
        }

        fn capture(&self, screen: u32) -> Result<RgbImage> {
            if let Some(exits) = &self.exits {
                exits.on_exit();
            }
            if let Some(rx) = &self.capture_hang {
                let _ = rx.lock().unwrap().recv();
            }
            let mut captures = self.captures.lock().unwrap();
            captures.push(screen);
            if self.stops_at_capture == Some(captures.len() - 1) {
                let waited = Instant::now();
                while self.checks.load(Ordering::SeqCst) < self.stop_after_checks {
                    assert!(
                        waited.elapsed() < Duration::from_secs(10),
                        "no VM check read the domain"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
                self.domain.store(0, Ordering::SeqCst);
                bail!("screenshot failed: Requested operation is not valid: domain is not running");
            }
            drop(captures);
            let mut frames = self.frames.lock().unwrap();
            let Some(frame) = frames.first().cloned() else {
                bail!(
                    "screenshot failed: this domain has no graphics (does the VM have a graphical display?)"
                );
            };
            if frames.len() > 1 {
                frames.remove(0);
            }
            Ok(frame)
        }
    }

    fn job(id: &Uuid) -> String {
        id.hyphenated().to_string()
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
        let checks = validate_conditions(&Config::default(), ANY_TIMEOUT, conditions).unwrap();
        run(env, Arc::new(guest), checks, Duration::from_secs(secs)).unwrap()
    }

    /// A timeout long enough for any condition's validation.
    const ANY_TIMEOUT: Duration = Duration::from_secs(60);

    /// A `width`x`height` frame of one grey `level`.
    fn solid(width: u32, height: u32, level: u8) -> RgbImage {
        RgbImage::from_pixel(width, height, image::Rgb([level; 3]))
    }

    /// A 100x10 black frame whose first `changed` pixels, in row order, are white.
    fn with_changed(changed: u32) -> RgbImage {
        RgbImage::from_fn(100, 10, |x, y| {
            image::Rgb([if y * 100 + x < changed { 255 } else { 0 }; 3])
        })
    }

    fn settled(max_changed_pixels: Option<u32>, settle_secs: Option<f64>) -> Condition {
        Condition::ScreenSettled {
            max_changed_pixels,
            settle_secs,
            region: None,
            screen: None,
        }
    }

    /// Validates `conditions` for a call of `secs` and runs them on `guest`.
    fn wait_on(
        env: &FakeEnv,
        guest: &Arc<FakeGuest>,
        conditions: &[Condition],
        secs: u64,
    ) -> Result<Outcome> {
        let timeout = Duration::from_secs(secs);
        let checks = validate_conditions(&Config::default(), timeout, conditions)?;
        let guest: Arc<dyn Guest> = guest.clone();
        run(env, guest, checks, timeout)
    }

    /// Feeds `frames`, captured at the given milliseconds, to a settle check, and returns the first time it held.
    fn first_hold(
        mut settle: Settle,
        frames: impl IntoIterator<Item = (u64, RgbImage)>,
    ) -> Option<u64> {
        let origin = Instant::now();
        frames.into_iter().find_map(|(at, frame)| {
            settle
                .push(origin + ms(at), &Arc::new(frame))
                .unwrap()
                .map(|_| at)
        })
    }

    #[test]
    fn a_still_screen_settles_after_settle_secs() {
        let still = (0..10).map(|k| (k * 500, solid(4, 4, 7)));
        let settle = Settle::new(600, Duration::from_secs(3), None);
        assert_eq!(first_hold(settle, still), Some(3000));
    }

    #[test]
    fn a_change_just_before_settle_secs_starts_the_count_again() {
        // The frame at 2.5 s differs in every pixel from the one before, so the screen holds from 2.5 s.
        let frames = (0..14).map(|k| (k * 500, solid(4, 4, if k < 5 { 7 } else { 8 })));
        let settle = Settle::new(0, Duration::from_secs(3), None);
        assert_eq!(first_hold(settle, frames), Some(5500));
    }

    #[test]
    fn a_change_of_frame_size_counts_as_a_change() {
        // Same colour, one column wider from 2.5 s, with a threshold that would let any pixel change through.
        let frames = (0..14).map(|k| (k * 500, solid(if k < 5 { 4 } else { 5 }, 4, 7)));
        let settle = Settle::new(u32::MAX, Duration::from_secs(3), None);
        assert_eq!(first_hold(settle, frames), Some(5500));
    }

    #[test]
    fn the_threshold_allows_up_to_max_changed_pixels() {
        // Frames alternate between black and one with `changed` white pixels.
        let alternating = |changed| {
            (0..10).map(move |k| {
                let frame = if k % 2 == 0 {
                    with_changed(0)
                } else {
                    with_changed(changed)
                };
                (k * 500, frame)
            })
        };
        let hold = |changed| {
            first_hold(
                Settle::new(600, Duration::from_secs(3), None),
                alternating(changed),
            )
        };
        assert_eq!(hold(599), Some(3000), "just below");
        assert_eq!(hold(600), Some(3000), "at");
        assert_eq!(hold(601), None, "just above");
    }

    #[test]
    fn a_region_compares_only_its_part_of_the_screen() {
        // The changing pixels are in the first six rows; the region is the bottom four rows of a 200x20 image the
        // agent saw, which maps onto rows 6 to 9 of the 100x10 frame.
        let region = Region {
            x: 0,
            y: 12,
            width: 200,
            height: 8,
            image_width: Some(200),
            image_height: Some(20),
        }
        .check()
        .unwrap();
        let frames = (0..10).map(|k| (k * 500, with_changed(if k % 2 == 0 { 0 } else { 600 })));
        let settle = Settle::new(0, Duration::from_secs(3), Some(region));
        assert_eq!(first_hold(settle, frames.clone()), Some(3000));
        let whole = Settle::new(0, Duration::from_secs(3), None);
        assert_eq!(first_hold(whole, frames), None);
    }

    #[test]
    fn screen_settled_matches_on_the_polled_frames() {
        // Frames start about every 510 ms (17 polls); the sixth one, at 2.55 s, changes in all its 16 pixels.
        let mut frames = vec![solid(4, 4, 1); 5];
        frames.push(solid(4, 4, 2));
        let guest = Arc::new(FakeGuest::showing(frames));
        let env = FakeEnv::new(vec![]);
        let o = wait_on(
            &env,
            &guest,
            &[console("never"), settled(Some(0), None)],
            20,
        )
        .unwrap();
        let Outcome::Matched {
            index, kind, frame, ..
        } = &o
        else {
            panic!("expected a match, got {o:?}")
        };
        assert_eq!((*index, *kind), (1, "screen_settled"));
        assert_eq!(frame.as_ref().unwrap().screen, 0);
        // It holds from 2.55 s, so it matches at the frame that starts 3 s after that.
        let waited = env.elapsed();
        assert!(
            waited >= ms(5550) && waited < ms(6200),
            "matched after {waited:?}"
        );
        let json = o.to_json(waited);
        assert_eq!(json["screen"], 0, "{json}");
        assert!(guest.captures.lock().unwrap().iter().all(|&s| s == 0));
    }

    #[test]
    fn screen_settled_captures_the_head_it_names() {
        let guest = Arc::new(FakeGuest::showing(vec![solid(4, 4, 1)]));
        let env = FakeEnv::new(vec![]);
        let on_head_one = Condition::ScreenSettled {
            max_changed_pixels: None,
            settle_secs: Some(0.5),
            region: None,
            screen: Some(1),
        };
        let o = wait_on(&env, &guest, &[on_head_one], 5).unwrap();
        assert_eq!(matched(&o).0, 0);
        let captures = guest.captures.lock().unwrap().clone();
        assert!(
            !captures.is_empty() && captures.iter().all(|&s| s == 1),
            "{captures:?}"
        );
    }

    #[test]
    fn a_failed_capture_ends_the_wait_with_its_cause() {
        let guest = Arc::new(FakeGuest::new());
        let env = FakeEnv::new(vec![]);
        let err = wait_on(&env, &guest, &[settled(None, None)], 10).unwrap_err();
        let err = format!("{err:#}");
        assert!(
            err.starts_with("capturing display head 0: ") && err.contains("no graphics"),
            "{err}"
        );
        assert!(env.elapsed() < Duration::from_secs(1));
    }

    /// A guest whose second capture, about 510 ms in, waits until the VM check started with it has read the domain
    /// as running, then stops the domain and fails. That check is held until `release` is dropped.
    fn stopping_during_a_held_check(release: Receiver<()>) -> FakeGuest {
        FakeGuest {
            stops_at_capture: Some(1),
            stop_after_checks: 2,
            hold_check: Some((1, Mutex::new(release))),
            ..FakeGuest::showing(vec![solid(4, 4, 1)])
        }
    }

    /// Runs `conditions` for up to `secs` on its own thread, so the test can act on the fake clock meanwhile.
    fn wait_in_background(
        env: &Arc<FakeEnv>,
        guest: &Arc<FakeGuest>,
        conditions: Vec<Condition>,
        secs: u64,
    ) -> std::thread::JoinHandle<Result<Outcome>> {
        let (env, guest) = (env.clone(), guest.clone());
        std::thread::spawn(move || wait_on(&env, &guest, &conditions, secs))
    }

    /// Waits until the failing capture has stopped the domain, then lets the fake clock run `polls` polls further. It
    /// returns at once when `waiter` ends first, so the caller reports the wait's own result.
    fn after_the_stop(
        env: &FakeEnv,
        guest: &FakeGuest,
        waiter: &std::thread::JoinHandle<Result<Outcome>>,
        polls: u32,
    ) {
        let started = Instant::now();
        while guest.read_domain().is_some() {
            if waiter.is_finished() {
                return;
            }
            assert!(started.elapsed() < Duration::from_secs(10), "no stop");
            std::thread::sleep(Duration::from_millis(1));
        }
        let stopped_at = env.elapsed();
        while env.elapsed() < stopped_at + POLL * polls {
            if waiter.is_finished() {
                return;
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the clock stalled"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn a_capture_that_fails_as_the_vm_stops_ends_the_wait_as_closed() {
        // The VM check that started with the failing capture read the VM as running before the stop. Its answer is
        // held until 20 polls after the stop, so it arrives after the failure, and must be ignored: only a check
        // started after the failure, which finds the VM stopped, decides.
        let (release, held) = mpsc::channel();
        let guest = Arc::new(stopping_during_a_held_check(held));
        let env = Arc::new(FakeEnv::new(vec![]));
        // A long timeout, so the fake deadline cannot end the wait while the test holds the check.
        let waiter = wait_in_background(&env, &guest, vec![settled(None, None)], 60);
        after_the_stop(&env, &guest, &waiter, 20);
        drop(release);
        let o = waiter.join().unwrap().unwrap();
        assert!(
            matches!(o, Outcome::Closed { index: None, ref reason } if reason == "the VM is not running"),
            "{o:?}"
        );
        assert_eq!(
            guest.captures.lock().unwrap().len(),
            2,
            "no capture after the failure"
        );
        assert_eq!(
            guest.checks.load(Ordering::SeqCst),
            3,
            "the held check, then one started after the failure"
        );
    }

    #[test]
    fn a_vm_check_that_fails_after_a_capture_failure_returns_the_capture_error() {
        // A check started after the failure cannot tell whether the VM runs, so the capture's own error is returned.
        let guest = Arc::new(FakeGuest {
            stops_at_capture: Some(1),
            check_fails_once_stopped: true,
            ..FakeGuest::showing(vec![solid(4, 4, 1)])
        });
        let env = FakeEnv::new(vec![]);
        let err = format!(
            "{:#}",
            wait_on(&env, &guest, &[settled(None, None)], 10).unwrap_err()
        );
        assert!(
            err.starts_with("capturing display head 0: ") && err.contains("domain is not running"),
            "{err}"
        );
        assert!(env.elapsed() < ms(1500), "{:?}", env.elapsed());
    }

    #[test]
    fn at_the_deadline_a_late_match_wins_over_a_capture_failure() {
        // A command condition whose run exited after the last poll, with `code`.
        let finished = |code| {
            let (tx, rx) = mpsc::channel();
            tx.send(CommandStep::Exited {
                code: Some(code),
                output: b"ready\n".to_vec(),
            })
            .unwrap();
            State::Command(CommandState {
                command: Arc::from("test -e /tmp/ready"),
                os_checked: true,
                pid: Some(100),
                last_code: None,
                probe: Probe {
                    inflight: Some(rx),
                    next_at: None,
                },
            })
        };
        let failure = || Some(anyhow!("capturing display head 0: domain is not running"));
        let o = at_deadline(&mut [finished(0)], failure()).unwrap();
        assert_eq!(matched(&o), (0, "ready\n", None));
        // A run that exited non-zero is no match, so the failure stands.
        let err = at_deadline(&mut [finished(1)], failure()).unwrap_err();
        assert!(err.to_string().contains("domain is not running"), "{err}");
        // Without a failure the wait times out as before.
        let o = at_deadline(&mut [finished(1)], None).unwrap();
        assert_eq!(
            o,
            Outcome::TimedOut {
                resume: vec![],
                commands: vec![(0, None, Some(1))],
                texts: vec![],
            }
        );
    }

    #[test]
    fn a_vm_check_hung_across_a_capture_failure_leaves_the_error_for_the_deadline() {
        let (release, held) = mpsc::channel();
        let guest = Arc::new(stopping_during_a_held_check(held));
        let env = FakeEnv::new(vec![]);
        let err = format!(
            "{:#}",
            wait_on(&env, &guest, &[settled(None, None)], 4).unwrap_err()
        );
        assert!(
            err.starts_with("capturing display head 0: ") && err.contains("domain is not running"),
            "{err}"
        );
        assert!(env.elapsed() >= Duration::from_secs(4));
        assert!(
            env.elapsed() <= Duration::from_secs(4) + POLL,
            "{:?}",
            env.elapsed()
        );
        drop(release);
    }

    #[test]
    fn a_console_match_in_the_poll_of_a_capture_failure_wins() {
        // The first capture starts at 0 and fails; its result arrives in the poll at 30 ms, with the output.
        let guest = Arc::new(FakeGuest::new());
        let env = FakeEnv::new(vec![(POLL, b"login: ")]);
        let o = wait_on(&env, &guest, &[settled(None, None), console("login:")], 10).unwrap();
        assert_eq!(matched(&o), (1, "login:", Some(6)));
        assert_eq!(env.elapsed(), POLL);
    }

    #[test]
    fn a_hung_capture_at_the_deadline_times_out_within_its_bound() {
        let (tx, rx) = mpsc::channel();
        let guest = Arc::new(FakeGuest {
            capture_hang: Some(Mutex::new(rx)),
            ..FakeGuest::showing(vec![solid(4, 4, 1)])
        });
        let env = FakeEnv::new(vec![]);
        let o = wait_on(&env, &guest, &[settled(None, None)], 4).unwrap();
        assert!(matches!(o, Outcome::TimedOut { .. }), "{o:?}");
        assert!(env.elapsed() >= Duration::from_secs(4));
        assert!(env.elapsed() <= Duration::from_secs(4) + POLL);
        drop(tx);
    }

    #[test]
    fn a_hung_capture_does_not_delay_console_checks() {
        let (tx, rx) = mpsc::channel();
        let guest = Arc::new(FakeGuest {
            capture_hang: Some(Mutex::new(rx)),
            ..FakeGuest::showing(vec![solid(4, 4, 1)])
        });
        let env = FakeEnv::new(vec![(ms(1500), b"login: ")]);
        let o = wait_on(&env, &guest, &[settled(None, None), console("login:")], 10).unwrap();
        assert_eq!(matched(&o).0, 1);
        assert!(env.elapsed() <= ms(1500) + POLL, "{:?}", env.elapsed());
        drop(tx);
    }

    #[test]
    fn screen_settled_validates_its_settle_time_region_and_head() {
        let refused = |timeout: u64, conditions: &[Condition]| {
            let err =
                validate_conditions(&Config::default(), Duration::from_secs(timeout), conditions)
                    .err()
                    .unwrap();
            format!("{err:#}")
        };
        let err = refused(10, &[settled(None, Some(0.49))]);
        assert!(err.contains("settle_secs must be at least 0.5"), "{err}");
        // Every frame starts before the call's end, so settle_secs equal to the timeout could never hold.
        let err = refused(10, &[settled(None, Some(10.0))]);
        assert_eq!(
            err,
            "condition 0: settle_secs 10 is above 9.5, the call's timeout of 10 s less one 0.5 s frame interval"
        );
        assert!(refused(10, &[settled(None, Some(9.51))]).contains("above 9.5"));
        // The default of 3 s is above a 3 s timeout too.
        assert!(refused(3, &[settled(None, None)]).contains("above 2.5"));
        assert!(
            validate_conditions(
                &Config::default(),
                Duration::from_secs(1),
                &[settled(None, Some(0.5))]
            )
            .is_ok()
        );
        for ok in [Some(0.5), Some(9.5), None] {
            assert!(
                validate_conditions(
                    &Config::default(),
                    Duration::from_secs(10),
                    &[settled(None, ok)]
                )
                .is_ok(),
                "{ok:?}"
            );
        }
        let outside = Condition::ScreenSettled {
            max_changed_pixels: None,
            settle_secs: None,
            region: Some(Region {
                x: 90,
                y: 0,
                width: 20,
                height: 10,
                image_width: Some(100),
                image_height: Some(100),
            }),
            screen: None,
        };
        let err = refused(10, &[outside]);
        assert!(
            err.starts_with("condition 0: the region") && err.contains("does not fit"),
            "{err}"
        );
        let head = |screen| Condition::ScreenSettled {
            max_changed_pixels: None,
            settle_secs: None,
            region: None,
            screen,
        };
        let err = refused(10, &[head(None), console("x"), head(Some(1))]);
        assert!(err.contains("must name the same display head"), "{err}");
        assert!(
            validate_conditions(
                &Config::default(),
                ANY_TIMEOUT,
                &[head(None), head(Some(0))]
            )
            .is_ok()
        );
    }

    #[test]
    fn a_zero_timeout_refuses_screen_conditions() {
        let checks =
            validate_conditions(&Config::default(), Duration::ZERO, &[settled(None, None)])
                .unwrap();
        let env = FakeEnv::new(vec![]);
        let err = run(&env, Arc::new(FakeGuest::new()), checks, Duration::ZERO)
            .unwrap_err()
            .to_string();
        assert!(err.contains("screen conditions need at least 1 s"), "{err}");
    }

    #[test]
    fn read_only_mode_refuses_screen_settled() {
        let read_only = Config {
            read_only: true,
            ..Config::default()
        };
        let err = validate_conditions(&read_only, ANY_TIMEOUT, &[settled(None, None)])
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("disabled in read-only mode"), "{err}");
        assert!(
            validate_conditions(&Config::default(), ANY_TIMEOUT, &[settled(None, None)]).is_ok()
        );
    }

    /// Runs `conditions` on `guest` for up to 10 s, then takes the attachment as `wait_for` does.
    fn wait_and_attach(
        env: &FakeEnv,
        guest: &Arc<FakeGuest>,
        conditions: &[Condition],
    ) -> (Outcome, Option<Attachment>) {
        let deadline = env.now() + Duration::from_secs(10);
        let o = wait_on(env, guest, conditions, 10).unwrap();
        let guest: Arc<dyn Guest> = guest.clone();
        let attachment = attach(env, &guest, &o, deadline);
        (o, attachment)
    }

    #[test]
    fn a_console_match_attaches_a_downscaled_screenshot_of_head_0() {
        // The clock runs in real time, because the encoding on the probe thread takes real time, which under a loaded
        // test run can exceed 2 s of the usual fast clock. A thin frame keeps that time short.
        let guest = Arc::new(FakeGuest::showing(vec![solid(1600, 90, 3)]));
        let env = FakeEnv {
            real_sleep: true,
            ..FakeEnv::new(vec![(ms(200), b"login: ")])
        };
        let (o, attachment) = wait_and_attach(&env, &guest, &[console("login:")]);
        assert_eq!(matched(&o).0, 0);
        let Some(Attachment::Image {
            png,
            width,
            height,
            screen,
        }) = &attachment
        else {
            panic!("expected an image, got {attachment:?}")
        };
        assert_eq!((*width, *height, *screen), (1280, 72, 0));
        let decoded = image::load_from_memory(png).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (1280, 72));
        assert_eq!(*guest.captures.lock().unwrap(), [0]);
        let mut json = o.to_json(Duration::ZERO);
        attachment.unwrap().describe(&mut json);
        assert_eq!(
            json["screenshot"],
            json!({ "width": 1280, "height": 72, "screen": 0 })
        );
    }

    #[test]
    fn a_screen_match_attaches_the_frame_it_matched_on() {
        let guest = Arc::new(FakeGuest::showing(vec![solid(8, 6, 5)]));
        let env = FakeEnv::new(vec![]);
        let on_head_two = Condition::ScreenSettled {
            max_changed_pixels: None,
            settle_secs: Some(0.5),
            region: None,
            screen: Some(2),
        };
        let (o, attachment) = wait_and_attach(&env, &guest, &[on_head_two]);
        assert_eq!(matched(&o).0, 0);
        let captures = guest.captures.lock().unwrap().len();
        assert!(
            matches!(
                attachment,
                Some(Attachment::Image {
                    width: 8,
                    height: 6,
                    screen: 2,
                    ..
                })
            ),
            "{attachment:?}"
        );
        // The attachment reused the matched frame rather than capturing again.
        assert_eq!(captures, 2, "two frames, half a second apart");
    }

    #[test]
    fn a_failed_attachment_capture_keeps_the_match_and_adds_a_note() {
        let guest = Arc::new(FakeGuest::new());
        let env = FakeEnv::new(vec![(ms(0), b"login: ")]);
        let (o, attachment) = wait_and_attach(&env, &guest, &[console("login:")]);
        assert_eq!(matched(&o).0, 0);
        let mut json = o.to_json(Duration::ZERO);
        attachment.unwrap().describe(&mut json);
        assert_eq!(json["result"], "matched");
        let note = json["screenshot_note"].as_str().unwrap();
        assert!(
            note.starts_with("no screenshot attached: capturing display head 0: ")
                && note.contains("no graphics"),
            "{note}"
        );
        assert!(json.get("screenshot").is_none());
    }

    #[test]
    fn a_hung_attachment_capture_is_given_up_after_2_s() {
        let (tx, rx) = mpsc::channel();
        let guest = Arc::new(FakeGuest {
            capture_hang: Some(Mutex::new(rx)),
            ..FakeGuest::showing(vec![solid(4, 4, 1)])
        });
        let env = FakeEnv::new(vec![(ms(100), b"login: ")]);
        let (o, attachment) = wait_and_attach(&env, &guest, &[console("login:")]);
        assert_eq!(matched(&o).0, 0);
        let matched_at = ms(100) + POLL;
        assert!(
            env.elapsed() <= matched_at + ATTACH_MARGIN + POLL,
            "{:?}",
            env.elapsed()
        );
        let Some(Attachment::Failed(note)) = attachment else {
            panic!("expected a note, got {attachment:?}")
        };
        assert_eq!(
            note,
            "no screenshot attached: taking the screenshot did not finish within 2 s"
        );
        drop(tx);
    }

    #[test]
    fn an_attachment_capture_ends_with_the_call_margin_after_the_deadline() {
        // A match found at the deadline itself leaves only the 2 s margin.
        let (tx, rx) = mpsc::channel();
        let guest: Arc<dyn Guest> = Arc::new(FakeGuest {
            capture_hang: Some(Mutex::new(rx)),
            ..FakeGuest::new()
        });
        let env = FakeEnv::new(vec![]);
        let deadline = env.now();
        env.sleep(Duration::from_secs(1));
        let o = Outcome::Matched {
            index: 0,
            kind: "agent",
            text: String::new(),
            next: None,
            generation: None,
            frame: None,
        };
        let attachment = attach(&env, &guest, &o, deadline);
        assert!(matches!(attachment, Some(Attachment::Failed(_))));
        assert!(env.elapsed() <= ATTACH_MARGIN + POLL, "{:?}", env.elapsed());
        drop(tx);
    }

    /// A recogniser that returns each text in turn, split into lines; the last repeats.
    struct FakeRecogniser {
        texts: Mutex<Vec<&'static str>>,
        /// The size of each image read.
        reads: Mutex<Vec<(u32, u32)>>,
        /// When set, every read blocks until the sender is dropped.
        hang: Option<Mutex<Receiver<()>>>,
        /// When set, every read fails.
        fails: bool,
        /// When set, counts each read's probe thread when it exits.
        exits: Option<Exits>,
    }

    impl FakeRecogniser {
        fn reading(texts: Vec<&'static str>) -> Self {
            Self {
                texts: Mutex::new(texts),
                reads: Mutex::default(),
                hang: None,
                fails: false,
                exits: None,
            }
        }
    }

    impl Recognise for FakeRecogniser {
        fn read_lines(&self, frame: &RgbImage) -> Result<Vec<String>> {
            if let Some(exits) = &self.exits {
                exits.on_exit();
            }
            if let Some(rx) = &self.hang {
                let _ = rx.lock().unwrap().recv();
            }
            self.reads.lock().unwrap().push(frame.dimensions());
            if self.fails {
                bail!("the detection model failed");
            }
            let mut texts = self.texts.lock().unwrap();
            let text = texts.first().copied().unwrap_or_default();
            if texts.len() > 1 {
                texts.remove(0);
            }
            Ok(text.split('\n').map(String::from).collect())
        }
    }

    /// A config with OCR models set, as `Config::validate` leaves it.
    fn with_models() -> Config {
        let mut config = Config::default();
        config.display.ocr_models = Some("/srv/ocr-models".into());
        config
    }

    fn screen_text(pattern: &str) -> Condition {
        Condition::ScreenText {
            pattern: pattern.into(),
            region: None,
            screen: None,
        }
    }

    /// A fake environment whose recogniser is `recogniser`.
    fn reading_env(
        output: Vec<(Duration, &'static [u8])>,
        recogniser: &Arc<FakeRecogniser>,
    ) -> FakeEnv {
        let recogniser: Arc<dyn Recognise> = recogniser.clone();
        FakeEnv {
            recogniser: Some(recogniser),
            ..FakeEnv::new(output)
        }
    }

    /// Validates `conditions` with OCR models for a call of `secs` and runs them on `guest`.
    fn wait_reading(
        env: &FakeEnv,
        guest: &Arc<FakeGuest>,
        conditions: &[Condition],
        secs: u64,
    ) -> Result<Outcome> {
        let timeout = Duration::from_secs(secs);
        let checks = validate_conditions(&with_models(), timeout, conditions)?;
        let guest: Arc<dyn Guest> = guest.clone();
        run(env, guest, checks, timeout)
    }

    #[test]
    fn screen_text_matches_the_recognised_lines_joined_by_newlines() {
        let recogniser = Arc::new(FakeRecogniser::reading(vec![
            "Debian\nSelect a language\nEnglish",
        ]));
        let guest = Arc::new(FakeGuest::showing(vec![solid(8, 6, 1)]));
        let env = reading_env(vec![], &recogniser);
        let o = wait_reading(&env, &guest, &[screen_text(r"language\nEng")], 10).unwrap();
        let Outcome::Matched {
            index,
            kind,
            text,
            frame,
            ..
        } = &o
        else {
            panic!("expected a match, got {o:?}")
        };
        assert_eq!(
            (*index, *kind, text.as_str()),
            (0, "screen_text", "language\nEng")
        );
        assert_eq!(frame.as_ref().unwrap().screen, 0);
        assert_eq!(*recogniser.reads.lock().unwrap(), [(8, 6)]);
    }

    #[test]
    fn the_recogniser_runs_only_when_the_frame_changes() {
        let recogniser = Arc::new(FakeRecogniser::reading(vec!["nothing here"]));
        // Three frames of one screen, then another that stays.
        let frames = [1, 1, 1, 2].map(|level| solid(8, 6, level)).to_vec();
        let guest = Arc::new(FakeGuest::showing(frames));
        let env = reading_env(vec![], &recogniser);
        let o = wait_reading(&env, &guest, &[screen_text("never")], 5).unwrap();
        assert!(matches!(o, Outcome::TimedOut { .. }), "{o:?}");
        assert!(guest.captures.lock().unwrap().len() >= 9);
        assert_eq!(recogniser.reads.lock().unwrap().len(), 2);
    }

    #[test]
    fn the_recogniser_reads_only_the_region_and_only_when_it_changes() {
        let recogniser = Arc::new(FakeRecogniser::reading(vec!["nothing here"]));
        // The first six rows change from frame to frame; the region, the bottom four rows, never does.
        let frames = (0..10)
            .map(|k| with_changed(if k % 2 == 0 { 0 } else { 600 }))
            .collect();
        let guest = Arc::new(FakeGuest::showing(frames));
        let env = reading_env(vec![], &recogniser);
        let bottom = Condition::ScreenText {
            pattern: "never".into(),
            region: Some(Region {
                x: 0,
                y: 12,
                width: 200,
                height: 8,
                image_width: Some(200),
                image_height: Some(20),
            }),
            screen: None,
        };
        let o = wait_reading(&env, &guest, &[bottom], 5).unwrap();
        assert!(matches!(o, Outcome::TimedOut { .. }), "{o:?}");
        // The region maps onto the 100x10 frame as its bottom 100x4, read at full resolution.
        assert_eq!(*recogniser.reads.lock().unwrap(), [(100, 4)]);
    }

    #[test]
    fn a_time_out_returns_the_last_ocr_text_cut_to_the_limit() {
        let recogniser = Arc::new(FakeRecogniser::reading(vec!["Hello\nWorld"]));
        let guest = Arc::new(FakeGuest::showing(vec![solid(4, 4, 1)]));
        let env = reading_env(vec![], &recogniser);
        let o = wait_reading(&env, &guest, &[console("never"), screen_text("Goodbye")], 3).unwrap();
        let Outcome::TimedOut { texts, .. } = &o else {
            panic!("expected a timeout, got {o:?}")
        };
        assert_eq!(texts, &[(1, Some("Hello\nWorld".to_string()))]);
        let json = o.to_json(Duration::ZERO);
        assert_eq!(
            json["screen_text"],
            json!([{ "index": 1, "last_text": "Hello\nWorld" }])
        );

        // One ASCII byte, then two-byte characters: the limit falls inside one and the cut stops before it.
        let long: &'static str = Box::leak(format!("a{}", "é".repeat(3000)).into_boxed_str());
        let recogniser = Arc::new(FakeRecogniser::reading(vec![long]));
        let env = reading_env(vec![], &recogniser);
        let o = wait_reading(&env, &guest, &[screen_text("never")], 3).unwrap();
        let Outcome::TimedOut { texts, .. } = &o else {
            panic!("expected a timeout, got {o:?}")
        };
        let cut = texts[0].1.as_deref().unwrap();
        assert_eq!(cut.len(), MAX_TEXT - 1);
        assert!(long.starts_with(cut));
        // A wait without screen_text conditions keeps its former result.
        let o = wait(
            &FakeEnv::new(vec![]),
            FakeGuest::new(),
            &[console("never")],
            1,
        );
        assert!(o.to_json(Duration::ZERO).get("screen_text").is_none());
    }

    #[test]
    fn a_blocking_recogniser_at_the_deadline_times_out_within_its_bound() {
        let (tx, rx) = mpsc::channel();
        let recogniser = Arc::new(FakeRecogniser {
            hang: Some(Mutex::new(rx)),
            ..FakeRecogniser::reading(vec!["Select a language"])
        });
        let guest = Arc::new(FakeGuest::showing(vec![solid(4, 4, 1)]));
        let env = reading_env(vec![], &recogniser);
        let o = wait_reading(&env, &guest, &[screen_text("language")], 4).unwrap();
        let Outcome::TimedOut { texts, .. } = &o else {
            panic!("expected a timeout, got {o:?}")
        };
        assert_eq!(texts, &[(0, None)], "no read finished");
        assert!(env.elapsed() >= Duration::from_secs(4));
        assert!(
            env.elapsed() <= Duration::from_secs(4) + POLL,
            "{:?}",
            env.elapsed()
        );
        drop(tx);
    }

    #[test]
    fn a_cancelled_wait_drops_a_running_read() {
        let (tx, rx) = mpsc::channel();
        let recogniser = Arc::new(FakeRecogniser {
            hang: Some(Mutex::new(rx)),
            ..FakeRecogniser::reading(vec!["Select a language"])
        });
        let guest = Arc::new(FakeGuest::showing(vec![solid(4, 4, 1)]));
        let env = FakeEnv {
            cancel_at: Some(ms(700)),
            ..reading_env(vec![], &recogniser)
        };
        let o = wait_reading(&env, &guest, &[screen_text("language")], 30).unwrap();
        assert_eq!(o, Outcome::Cancelled);
        assert!(env.elapsed() < ms(800), "{:?}", env.elapsed());
        drop(tx);
    }

    /// A `screen_text` state whose read of `frame` finished with `read` after the last poll.
    fn finished_read(pattern: &str, read: &str) -> State {
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(read.to_string())).unwrap();
        let mut state = TextState::new(
            TextRegex::new(pattern).unwrap(),
            None,
            0,
            Arc::new(FakeRecogniser::reading(vec![])),
        );
        state.probe = Probe {
            inflight: Some(rx),
            next_at: None,
        };
        state.reading = Some(Arc::new(solid(4, 4, 1)));
        State::Text(state)
    }

    #[test]
    fn a_read_that_finished_after_the_last_poll_still_counts() {
        let o = at_deadline(&mut [finished_read("language", "Select a language")], None).unwrap();
        assert_eq!(matched(&o), (0, "language", None));
        let o = at_deadline(&mut [finished_read("Goodbye", "Select a language")], None).unwrap();
        assert_eq!(
            o,
            Outcome::TimedOut {
                resume: vec![],
                commands: vec![],
                texts: vec![(0, Some("Select a language".into()))],
            }
        );
    }

    #[test]
    fn the_lowest_index_wins_between_screen_text_and_a_console_in_one_poll() {
        // The capture starts at 0, its frame arrives at 30 ms and starts a read, whose text arrives at 60 ms with the
        // console's output. Each sleep waits for its poll's probe thread to exit, so its result is there at the next.
        for (conditions, winner) in [
            (
                [screen_text("language"), console("login:")],
                (0, "screen_text"),
            ),
            ([console("login:"), screen_text("language")], (0, "console")),
        ] {
            let exits = Exits::default();
            let recogniser = Arc::new(FakeRecogniser {
                exits: Some(exits.clone()),
                ..FakeRecogniser::reading(vec!["Select a language"])
            });
            let guest = Arc::new(FakeGuest {
                exits: Some(exits.clone()),
                ..FakeGuest::showing(vec![solid(4, 4, 1)])
            });
            let env = FakeEnv {
                on_sleep: Some(Box::new(move |sleeps| match sleeps {
                    0 => exits.wait_for(1),
                    1 => exits.wait_for(2),
                    _ => {}
                })),
                ..reading_env(vec![(POLL * 2, b"login: ")], &recogniser)
            };
            let o = wait_reading(&env, &guest, &conditions, 10).unwrap();
            let Outcome::Matched { index, kind, .. } = o else {
                panic!("expected a match, got {o:?}")
            };
            assert_eq!((index, kind), winner);
            assert_eq!(env.elapsed(), POLL * 2, "both held in the same poll");
        }
    }

    #[test]
    fn a_failed_read_ends_the_wait_with_its_cause() {
        let recogniser = Arc::new(FakeRecogniser {
            fails: true,
            ..FakeRecogniser::reading(vec![])
        });
        let guest = Arc::new(FakeGuest::showing(vec![solid(4, 4, 1)]));
        let env = reading_env(vec![], &recogniser);
        let err = format!(
            "{:#}",
            wait_reading(&env, &guest, &[screen_text("x")], 5).unwrap_err()
        );
        assert_eq!(
            err,
            "reading text from the screen: the detection model failed"
        );
    }

    #[test]
    fn screen_text_is_refused_in_read_only_mode_and_without_models() {
        let refused = |config: &Config, c: Condition| {
            format!(
                "{:#}",
                validate_conditions(config, ANY_TIMEOUT, &[c])
                    .err()
                    .unwrap()
            )
        };
        let read_only = Config {
            read_only: true,
            ..with_models()
        };
        assert!(refused(&read_only, screen_text("x")).contains("disabled in read-only mode"));
        let err = refused(&Config::default(), screen_text("x"));
        assert!(
            err.contains("display.ocr_models") && err.contains("scripts/fetch-ocr-models.sh"),
            "{err}"
        );
        assert!(refused(&with_models(), screen_text("(")).contains("bad pattern"));
        assert!(validate_conditions(&with_models(), ANY_TIMEOUT, &[screen_text("x")]).is_ok());
        // A server that validated the models but has no recogniser refuses the same way.
        let checks = validate_conditions(&with_models(), ANY_TIMEOUT, &[screen_text("x")]).unwrap();
        let err = run(
            &FakeEnv::new(vec![]),
            Arc::new(FakeGuest::new()),
            checks,
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(err.to_string().contains("display.ocr_models"), "{err}");
        // screen_text and screen_settled share the display head rule.
        let on = |screen| Condition::ScreenText {
            pattern: "x".into(),
            region: None,
            screen: Some(screen),
        };
        let err = validate_conditions(&with_models(), ANY_TIMEOUT, &[on(1), settled(None, None)])
            .err()
            .unwrap();
        assert!(err.to_string().contains("same display head"), "{err}");
        assert!(
            validate_conditions(&with_models(), ANY_TIMEOUT, &[on(0), settled(None, None)]).is_ok()
        );
    }

    /// The probe's installer language screen, read through the fake frame source by the real recogniser. Run with
    /// `VIRTMANAGER_OCR_MODELS=<dir> RTEN_NUM_THREADS=4 cargo test --release -- --ignored`: a debug build's OCR does
    /// not finish one read within the wait's 60 s.
    #[test]
    #[ignore = "needs the OCR models; set VIRTMANAGER_OCR_MODELS"]
    fn screen_text_reads_the_installer_language_screen() {
        let dir = std::env::var_os("VIRTMANAGER_OCR_MODELS").expect(
            "set VIRTMANAGER_OCR_MODELS to the directory scripts/fetch-ocr-models.sh filled",
        );
        let loading = std::time::Instant::now();
        let recogniser: Arc<dyn Recognise> =
            Arc::new(display::Recognizer::load(std::path::Path::new(&dir)).unwrap());
        println!("models loaded in {} ms", loading.elapsed().as_millis());
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("docs/research/2026-11-10-ocr-probe/deb-installer-language.png");
        let screen = image::open(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
            .into_rgb8();
        let guest = Arc::new(FakeGuest::showing(vec![screen]));
        // OCR takes real time, so the clock runs in real time.
        let env = FakeEnv {
            real_sleep: true,
            recogniser: Some(recogniser),
            ..FakeEnv::new(vec![])
        };
        let o = wait_reading(&env, &guest, &[screen_text("Select a language")], 60).unwrap();
        println!("matched after {} ms", env.elapsed().as_millis());
        assert_eq!(matched(&o), (0, "Select a language", None));
    }

    #[test]
    fn only_a_match_gets_an_attachment() {
        let guest = Arc::new(FakeGuest::showing(vec![solid(4, 4, 1)]));
        let env = FakeEnv::new(vec![]);
        let deadline = env.now();
        let guest: Arc<dyn Guest> = guest;
        for o in [
            Outcome::Cancelled,
            Outcome::TimedOut {
                resume: vec![],
                commands: vec![],
                texts: vec![],
            },
        ] {
            assert_eq!(attach(&env, &guest, &o, deadline), None);
        }
    }

    fn matched(o: &Outcome) -> (usize, &str, Option<u64>) {
        match o {
            Outcome::Matched {
                index, text, next, ..
            } => (*index, text.as_str(), *next),
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
        assert_eq!(resume, vec![(0, 9, Some(7))]);
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
        let restarted = FakeGuest::new();
        restarted.domain.store(2, Ordering::Relaxed);
        let o = wait(&env, restarted, &[console("login:")], 5);
        assert!(
            matches!(o, Outcome::Closed { index: Some(0), ref reason } if reason.contains("restarted")),
            "{o:?}"
        );
        // A stop during the wait is seen by the next VM check, within about half a second.
        let guest = Arc::new(FakeGuest {
            ping_after: u32::MAX,
            ..FakeGuest::new()
        });
        let stopper = guest.clone();
        let checks =
            validate_conditions(&Config::default(), ANY_TIMEOUT, &[Condition::Agent]).unwrap();
        let waiter = std::thread::spawn(move || {
            let env = FakeEnv::new(vec![]);
            let o = run(&env, guest, checks, Duration::from_secs(30)).unwrap();
            (o, env.elapsed())
        });
        std::thread::sleep(ms(50));
        stopper.domain.store(0, Ordering::Relaxed);
        let (o, waited) = waiter.join().unwrap();
        assert!(matches!(o, Outcome::Closed { index: None, .. }), "{o:?}");
        assert!(waited < Duration::from_secs(30), "{waited:?}");
    }

    #[test]
    fn output_stored_before_a_stop_or_restart_still_matches() {
        // The stop and the last line reach the wait in the same poll.
        let env = FakeEnv::new(vec![(ms(10), b"reboot: Power down\n")]);
        let stopped = FakeGuest::new();
        stopped.domain.store(0, Ordering::Relaxed);
        let o = wait(&env, stopped, &[console("Power down")], 5);
        assert_eq!(matched(&o).1, "Power down");
        // The same holds for a restart.
        let env = FakeEnv::new(vec![(ms(0), b"bye\n")]);
        let restarted = FakeGuest::new();
        restarted.domain.store(2, Ordering::Relaxed);
        let o = wait(&env, restarted, &[console("bye")], 5);
        assert_eq!(matched(&o).1, "bye");
    }

    #[test]
    fn a_console_reopened_during_the_wait_ends_it() {
        let mut env = FakeEnv::new(vec![]);
        env.reopened_at = Some(ms(100));
        let o = wait(&env, FakeGuest::new(), &[console("login:")], 5);
        assert!(
            matches!(o, Outcome::Closed { index: Some(0), ref reason } if reason.contains("reopened")),
            "{o:?}"
        );
        assert!(env.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_slow_status_call_never_starts_a_second_run() {
        let guest = Arc::new(FakeGuest {
            status_errors: AtomicU32::new(3),
            codes: Mutex::new(vec![0]),
            ..FakeGuest::new()
        });
        let env = FakeEnv::new(vec![]);
        let checks = validate_conditions(
            &Config::default(),
            ANY_TIMEOUT,
            &[Condition::Command {
                command: "true".into(),
            }],
        )
        .unwrap();
        let o = run(&env, guest.clone(), checks, Duration::from_secs(10)).unwrap();
        assert_eq!(matched(&o).0, 0);
        assert_eq!(guest.starts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn job_output_reads_on_from_the_last_unfinished_line() {
        let id = Uuid::new_v4();
        let guest = Arc::new(FakeGuest {
            job: Mutex::new(vec![
                (jobs::State::Running, b"step 1\nstep 3 is"),
                (jobs::State::Running, b"step 1\nstep 3 is done\n"),
            ]),
            ..FakeGuest::new()
        });
        let env = FakeEnv::new(vec![]);
        let cond = Condition::JobOutput {
            job: job(&id),
            pattern: "step 3 is done".into(),
            since: None,
        };
        let checks = validate_conditions(&Config::default(), ANY_TIMEOUT, &[cond]).unwrap();
        let o = run(&env, guest.clone(), checks, Duration::from_secs(10)).unwrap();
        assert_eq!(matched(&o), (0, "step 3 is done", Some(21)));
        assert_eq!(*guest.job_reads.lock().unwrap(), [0, 7]);
    }

    #[test]
    fn a_long_line_cut_inside_a_character_does_not_hold_the_reads() {
        // One ASCII byte, then two-byte characters: a 1 MiB read ends inside a character and is trimmed by a byte.
        let mut output = b"a".to_vec();
        output.extend("é".repeat(MAX_JOB_READ / 2 + 1000).bytes());
        output.extend(b"\nstep 3\n");
        let output: &'static [u8] = Box::leak(output.into_boxed_slice());
        let guest = Arc::new(FakeGuest {
            job: Mutex::new(vec![(jobs::State::Running, output)]),
            ..FakeGuest::new()
        });
        let env = FakeEnv::new(vec![]);
        let cond = Condition::JobOutput {
            job: job(&Uuid::new_v4()),
            pattern: "step 3".into(),
            since: None,
        };
        let checks = validate_conditions(&Config::default(), ANY_TIMEOUT, &[cond]).unwrap();
        let o = run(&env, guest.clone(), checks, Duration::from_secs(10)).unwrap();
        assert_eq!(matched(&o).0, 0);
        let reads = guest.job_reads.lock().unwrap().clone();
        assert_eq!(
            reads,
            [0, MAX_JOB_READ as u64 - 1],
            "the second read starts past the cut"
        );
        let json = o.to_json(Duration::ZERO);
        assert!(
            json.get("generation").is_none() && json["next"].is_u64(),
            "{json}"
        );
    }

    #[test]
    fn job_conditions_refuse_a_zero_timeout_and_a_windows_guest() {
        let env = FakeEnv::new(vec![]);
        let done = Condition::JobDone {
            job: job(&Uuid::new_v4()),
        };
        let checks =
            validate_conditions(&Config::default(), ANY_TIMEOUT, std::slice::from_ref(&done))
                .unwrap();
        let err = run(&env, Arc::new(FakeGuest::new()), checks, Duration::ZERO)
            .unwrap_err()
            .to_string();
        assert!(err.contains("at least 1 s"), "{err}");
        let windows = FakeGuest {
            os: GuestOs::Windows,
            ..FakeGuest::new()
        };
        let checks = validate_conditions(&Config::default(), ANY_TIMEOUT, &[done]).unwrap();
        let err = run(&env, Arc::new(windows), checks, Duration::from_secs(5))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Windows"), "{err}");
    }

    #[test]
    fn a_job_output_timeout_returns_a_resume_point_without_a_generation() {
        let id = Uuid::new_v4();
        let guest = Arc::new(FakeGuest {
            job: Mutex::new(vec![(jobs::State::Running, b"line\npart")]),
            ..FakeGuest::new()
        });
        let env = FakeEnv::new(vec![]);
        let cond = Condition::JobOutput {
            job: job(&id),
            pattern: "never".into(),
            since: Some(0),
        };
        let checks = validate_conditions(&Config::default(), ANY_TIMEOUT, &[cond]).unwrap();
        let o = run(&env, guest, checks, Duration::from_secs(3)).unwrap();
        let Outcome::TimedOut { resume, .. } = &o else {
            panic!("expected a timeout, got {o:?}")
        };
        assert_eq!(resume, &[(0, 5, None)]);
        let json = o.to_json(Duration::from_secs(3));
        assert!(json["resume"][0].get("generation").is_none(), "{json}");
    }

    #[test]
    fn job_done_reports_the_exit_code_and_refuses_an_unknown_job() {
        let id = Uuid::new_v4();
        let guest = Arc::new(FakeGuest {
            job: Mutex::new(vec![
                (jobs::State::Running, b""),
                (jobs::State::Exited(3), b""),
            ]),
            ..FakeGuest::new()
        });
        let env = FakeEnv::new(vec![]);
        let done = Condition::JobDone { job: job(&id) };
        let checks =
            validate_conditions(&Config::default(), ANY_TIMEOUT, std::slice::from_ref(&done))
                .unwrap();
        let o = run(&env, guest, checks, Duration::from_secs(10)).unwrap();
        assert_eq!(matched(&o), (0, "exited with code 3", None));
        let checks = validate_conditions(&Config::default(), ANY_TIMEOUT, &[done]).unwrap();
        let err = run(
            &env,
            Arc::new(FakeGuest::new()),
            checks,
            Duration::from_secs(5),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no job"), "{err}");
    }

    #[test]
    fn job_conditions_pass_read_only_mode_and_need_a_job_id() {
        let read_only = Config {
            read_only: true,
            ..Config::default()
        };
        let id = job(&Uuid::new_v4());
        let conds = [
            Condition::JobDone { job: id.clone() },
            Condition::JobOutput {
                job: id,
                pattern: "x".into(),
                since: None,
            },
        ];
        assert!(validate_conditions(&read_only, ANY_TIMEOUT, &conds).is_ok());
        let bad = Condition::JobDone {
            job: "../etc".into(),
        };
        assert!(validate_conditions(&read_only, ANY_TIMEOUT, &[bad]).is_err());
    }

    #[test]
    fn a_pid_the_agent_forgot_starts_the_command_again() {
        let guest = Arc::new(FakeGuest {
            codes: Mutex::new(vec![-1, 0]),
            ..FakeGuest::new()
        });
        let env = FakeEnv::new(vec![]);
        let checks = validate_conditions(
            &Config::default(),
            ANY_TIMEOUT,
            &[Condition::Command {
                command: "true".into(),
            }],
        )
        .unwrap();
        let o = run(&env, guest.clone(), checks, Duration::from_secs(10)).unwrap();
        assert_eq!(matched(&o).0, 0);
        assert_eq!(guest.starts.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_zero_timeout_refuses_probe_conditions_and_checks_the_console_once() {
        let env = FakeEnv::new(vec![(ms(0), b"login: ")]);
        let checks =
            validate_conditions(&Config::default(), ANY_TIMEOUT, &[Condition::Agent]).unwrap();
        let err = run(&env, Arc::new(FakeGuest::new()), checks, Duration::ZERO)
            .unwrap_err()
            .to_string();
        assert!(err.contains("at least 1 s"), "{err}");
        let o = wait(&env, FakeGuest::new(), &[console("login:")], 0);
        assert_eq!(matched(&o).0, 0);
    }

    #[test]
    fn a_cancelled_wait_stops_polling() {
        let mut env = FakeEnv::new(vec![]);
        env.cancel_at = Some(ms(700));
        let o = wait(&env, FakeGuest::new(), &[console("never")], 30);
        assert_eq!(o, Outcome::Cancelled);
        assert!(env.elapsed() < ms(800), "{:?}", env.elapsed());
    }

    #[test]
    fn progress_goes_out_every_five_seconds() {
        let env = FakeEnv::new(vec![]);
        let o = wait(&env, FakeGuest::new(), &[console("never")], 12);
        assert!(matches!(o, Outcome::TimedOut { .. }));
        let sent = env.progress.lock().unwrap().clone();
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert!(sent[0] >= Duration::from_secs(5) && sent[0] < Duration::from_secs(6));
        assert!(sent[1] >= Duration::from_secs(10) && sent[1] < Duration::from_secs(11));
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
        let Outcome::TimedOut {
            resume, commands, ..
        } = o
        else {
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
            ANY_TIMEOUT,
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
                commands: vec![(0, Some(100), None)],
                texts: vec![],
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
            ANY_TIMEOUT,
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
            validate_conditions(config, ANY_TIMEOUT, &[c])
                .err()
                .unwrap()
                .to_string()
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
        assert!(validate_conditions(&config, ANY_TIMEOUT, &[cmd]).is_ok());
        assert!(validate_conditions(&read_only, ANY_TIMEOUT, &[Condition::Agent]).is_ok());
        assert!(validate_conditions(&config, ANY_TIMEOUT, &[]).is_err());
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
