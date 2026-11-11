//! Background jobs in Linux and Windows guests: the launchers, the job file layout, and how a job's state is read.
//!
//! On Linux a job lives in `/var/lib/virtmanager-mcp/jobs/<id>/` in the guest: `cmd` holds the command, `out` its
//! merged stdout and stderr, `pid` the wrapper's PID and start time, and `exit` the exit code once it ends. On Windows
//! it lives in `C:\ProgramData\virtmanager-mcp\jobs\<id>\`: `cmd.ps1` holds the PowerShell command, `out` the merged
//! output, `pid` the wrapper's process ID and start time, `exit` the exit code, and `lock` a file the wrapper holds
//! open while it runs. The guest-side scripts are fixed texts in `src/jobs/`; IDs reach them only as arguments or
//! quoted literals, and commands only as stdin.

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use uuid::Uuid;

use crate::agent::{Agent, ExecSpec, GuestOs, powershell, ps_quote};

const LAUNCHER: &str = include_str!("jobs/launcher.sh");
const WRAPPER: &str = include_str!("jobs/wrapper.sh");
const STOP: &str = include_str!("jobs/stop.sh");
const LIST: &str = include_str!("jobs/list.sh");
const WIN_LAUNCHER: &str = include_str!("jobs/launcher.ps1");
const WIN_WRAPPER: &str = include_str!("jobs/wrapper.ps1");
const WIN_STOP: &str = include_str!("jobs/stop.ps1");
const WIN_LIST: &str = include_str!("jobs/list.ps1");

/// Where jobs live in a Linux guest.
pub const JOBS_DIR: &str = "/var/lib/virtmanager-mcp/jobs";
/// Where jobs live in a Windows guest.
pub const WIN_JOBS_DIR: &str = r"C:\ProgramData\virtmanager-mcp\jobs";
/// Time the launcher and the stop and list helpers get.
const HELPER_TIMEOUT: Duration = Duration::from_secs(15);
/// Most output a helper may return.
const HELPER_OUTPUT: usize = 256 * 1024;
/// How often, and how long apart, a Windows read retries the log while the wrapper is appending to it.
const BUSY_TRIES: u32 = 10;
const BUSY_PAUSE: Duration = Duration::from_millis(50);

/// A job ID: a UUID v4 in its hyphenated lower-case form, so it can never become another path.
pub fn parse_id(id: &str) -> Result<Uuid> {
    let parsed = Uuid::try_parse(id).map_err(|_| anyhow!("'{id}' is not a job ID"))?;
    if parsed.get_version_num() != 4 || parsed.hyphenated().to_string() != id {
        bail!("'{id}' is not a job ID");
    }
    Ok(parsed)
}

fn job_dir(os: GuestOs, id: &Uuid) -> String {
    match os {
        GuestOs::Linux => format!("{JOBS_DIR}/{}", id.hyphenated()),
        GuestOs::Windows => format!(r"{WIN_JOBS_DIR}\{}", id.hyphenated()),
    }
}

/// A path inside job `id`'s directory.
fn job_file(os: GuestOs, id: &Uuid, name: &str) -> String {
    match os {
        GuestOs::Linux => format!("{}/{name}", job_dir(os, id)),
        GuestOs::Windows => format!(r"{}\{name}", job_dir(os, id)),
    }
}

/// The `guest-exec` call that starts job `id` running `command`. The command travels on stdin. On Windows the ID and
/// the wrapper's text are set as quoted PowerShell literals before the fixed launcher text.
fn launch_spec(os: GuestOs, id: &Uuid, command: &str) -> ExecSpec {
    let id = id.hyphenated().to_string();
    let mut spec = match os {
        GuestOs::Linux => ExecSpec::new("/bin/sh", ["-c", LAUNCHER, "launcher", &id, WRAPPER]),
        GuestOs::Windows => powershell(&format!(
            "$id = {}\n$wrapper = {}\n{WIN_LAUNCHER}",
            ps_quote(&id),
            ps_quote(WIN_WRAPPER)
        )),
    };
    spec.stdin = Some(command.as_bytes().to_vec());
    spec
}

/// Starts `command` as a new job and returns its ID and how it started.
pub fn start(agent: &Agent, os: GuestOs, command: &str) -> Result<(Uuid, String)> {
    let id = Uuid::new_v4();
    let out = agent.exec(
        &launch_spec(os, &id, command),
        HELPER_TIMEOUT,
        HELPER_OUTPUT,
    )?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    match out.exit_code {
        Some(0) => Ok((id, text)),
        _ if out.timed_out => bail!("the job launcher did not finish in time"),
        _ => bail!(
            "the job did not start: {}",
            if text.is_empty() {
                String::from_utf8_lossy(&out.stderr).trim().to_string()
            } else {
                text
            }
        ),
    }
}

/// The `guest-exec` call that stops job `id`: on Linux it signals the process group with `number`, on Windows it ends
/// the process tree.
fn stop_spec(os: GuestOs, id: &Uuid, number: &str) -> ExecSpec {
    match os {
        GuestOs::Linux => ExecSpec::new("/bin/sh", ["-c", STOP, "stop", &job_dir(os, id), number]),
        GuestOs::Windows => powershell(&format!(
            "$id = {}\n{WIN_STOP}",
            ps_quote(&id.hyphenated().to_string())
        )),
    }
}

/// Stops a running job. On Linux it signals the process group: TERM, or KILL with `force`. On Windows a console
/// process has no equivalent of TERM, so it always ends the process tree, and the job reads "ended without status".
pub fn stop(agent: &Agent, os: GuestOs, id: &Uuid, force: bool) -> Result<String> {
    let (name, number) = if force { ("KILL", "9") } else { ("TERM", "15") };
    let out = agent.exec(&stop_spec(os, id, number), HELPER_TIMEOUT, HELPER_OUTPUT)?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    match out.exit_code {
        Some(0) => Ok(match os {
            GuestOs::Linux => format!("sent {name} to job {id}"),
            GuestOs::Windows => format!(
                "ended job {id}'s process tree with taskkill /T /F; on Windows, force makes no difference"
            ),
        }),
        Some(3 | 4) => bail!("job {id} is {text}"),
        Some(6) => bail!("no job {id} in this guest"),
        _ => bail!("could not stop job {id}: {text}"),
    }
}

/// One line of the job list.
#[derive(Debug, PartialEq, Serialize)]
pub struct Listed {
    pub job: String,
    /// When the job was started, in seconds since the epoch.
    pub started: u64,
    pub state: &'static str,
    pub exit_code: Option<i64>,
}

pub fn list(agent: &Agent, os: GuestOs) -> Result<Vec<Listed>> {
    let spec = match os {
        GuestOs::Linux => ExecSpec::new("/bin/sh", ["-c", LIST, "list"]),
        GuestOs::Windows => powershell(WIN_LIST),
    };
    let out = agent.exec(&spec, HELPER_TIMEOUT, HELPER_OUTPUT)?;
    if out.exit_code != Some(0) {
        bail!(
            "could not list jobs: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(parse_list(&String::from_utf8_lossy(&out.stdout)))
}

/// Parses the list helper's lines, skipping any whose ID is not a job ID.
fn parse_list(text: &str) -> Vec<Listed> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let job = f.next().filter(|id| parse_id(id).is_ok())?.to_string();
            let started = f.next()?.parse().ok()?;
            let (state, exit_code) = match f.next()? {
                "exited" => ("exited", f.next().and_then(|c| c.parse().ok())),
                "starting" => ("starting", None),
                "running" => ("running", None),
                _ => ("ended without status", None),
            };
            Some(Listed {
                job,
                started,
                state,
                exit_code,
            })
        })
        .collect()
}

/// What `job_read` reports about a job.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum State {
    /// The wrapper has not recorded its PID yet.
    Starting,
    Running,
    Exited(i64),
    /// The process is gone without an exit code, as after a reboot or a kill.
    EndedWithoutStatus,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Starting => "starting",
            State::Running => "running",
            State::Exited(_) => "exited",
            State::EndedWithoutStatus => "ended without status",
        }
    }

    pub fn exit_code(self) -> Option<i64> {
        match self {
            State::Exited(code) => Some(code),
            _ => None,
        }
    }
}

/// The start time in a `/proc/<pid>/stat` line: field 22, counted after the last `)` because the process name may
/// hold spaces and parentheses.
pub fn stat_start_time(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// A job's state from its `exit` file, its `pid` file ("PID STARTTIME BOOTID"), the current `/proc/<pid>/stat`, and
/// the current boot ID, each `None` when missing. A job runs only while its PID has the recorded start time in the
/// recorded boot, so neither a reused PID nor a reboot passes for it.
pub fn state(
    exit: Option<&str>,
    pid: Option<&str>,
    stat: Option<&str>,
    boot: Option<&str>,
) -> State {
    if let Some(code) = exit_code(exit) {
        return State::Exited(code);
    }
    let Some(pid) = pid else {
        return State::Starting;
    };
    let mut fields = pid.split_whitespace().skip(1);
    let (recorded_start, recorded_boot) = (fields.next(), fields.next());
    let same_boot = recorded_boot.is_some() && recorded_boot == boot.map(str::trim);
    match stat.and_then(stat_start_time) {
        Some(now) if same_boot && Some(now.to_string().as_str()) == recorded_start => {
            State::Running
        }
        _ => State::EndedWithoutStatus,
    }
}

fn exit_code(exit: Option<&str>) -> Option<i64> {
    exit.and_then(|e| e.trim().parse().ok())
}

/// A state of "ended without status" read again after the `exit` file was checked once more: a job that ended
/// between the reads has written its code by then.
fn recheck(state: State, late_exit: Option<&str>) -> State {
    match (state, exit_code(late_exit)) {
        (State::EndedWithoutStatus, Some(code)) => State::Exited(code),
        _ => state,
    }
}

/// How many of `data`'s bytes to return when the read was cut at `max_bytes`: an incomplete UTF-8 sequence at the end
/// is left for the next read, unless that would leave nothing.
fn whole_chars(data: &[u8], cut: bool) -> usize {
    if !cut {
        return data.len();
    }
    // The start of the last character is at most 3 bytes before the end.
    let tail = data.len().saturating_sub(4);
    let Some(start) = (tail..data.len()).rev().find(|&i| data[i] & 0xC0 != 0x80) else {
        return data.len();
    };
    let need = match data[start] {
        b if b >= 0xF0 => 4,
        b if b >= 0xE0 => 3,
        b if b >= 0xC0 => 2,
        _ => 1,
    };
    if start + need > data.len() && start > 0 {
        start
    } else {
        data.len()
    }
}

/// Output of a job from `since`, with the offset to read from next.
pub struct Read {
    pub state: State,
    pub data: Vec<u8>,
    pub next: u64,
    /// The read stopped at `max_bytes`, so more output may follow; `data` can be a few bytes shorter, when a UTF-8
    /// character was cut.
    pub cut: bool,
}

/// Cuts a read that reached `max_bytes` back to whole UTF-8 characters, and says whether it was cut.
pub fn trim_read(data: &mut Vec<u8>, max_bytes: usize) -> bool {
    let cut = data.len() >= max_bytes;
    data.truncate(whole_chars(data, cut));
    cut
}

/// Whether a Windows job's wrapper holds its lock file, as the guest agent finds when it tries to open it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lock {
    /// The agent cannot open the file: the wrapper holds it, so the job runs.
    Held,
    /// The agent opened the file: no wrapper holds it, after an exit, a kill or a reboot.
    Free,
    Missing,
}

/// A Windows job's state from its `exit` file, whether its `pid` file exists, and its lock. The wrapper takes the lock
/// before it writes `pid`, and writes `exit` before it lets go, so a recorded job whose lock is free has ended.
pub fn windows_state(exit: Option<&str>, pid: bool, lock: Lock) -> State {
    if let Some(code) = exit_code(exit) {
        return State::Exited(code);
    }
    if !pid {
        return State::Starting;
    }
    match lock {
        Lock::Held => State::Running,
        Lock::Free | Lock::Missing => State::EndedWithoutStatus,
    }
}

/// Reads job `id`'s output from `since`, at most `max_bytes`, and its state, through the agent's file calls.
/// Returns `None` when the guest has no such job.
pub fn read(
    agent: &Agent,
    os: GuestOs,
    id: &Uuid,
    since: u64,
    max_bytes: usize,
) -> Result<Option<Read>> {
    let file = |name: &str| job_file(os, id, name);
    let command = match os {
        GuestOs::Linux => "cmd",
        GuestOs::Windows => "cmd.ps1",
    };
    if small_file(agent, &file(command))?.is_none() {
        return Ok(None);
    }
    // The state comes first: output read after an exit code is complete.
    let exit = small_file(agent, &file("exit"))?;
    let pid = small_file(agent, &file("pid"))?;
    let mut state = match os {
        GuestOs::Linux => {
            let stat = match pid.as_deref().and_then(|p| p.split_whitespace().next()) {
                Some(p) if p.bytes().all(|b| b.is_ascii_digit()) => {
                    small_file(agent, &format!("/proc/{p}/stat"))?
                }
                _ => None,
            };
            let boot = small_file(agent, "/proc/sys/kernel/random/boot_id")?;
            state(
                exit.as_deref(),
                pid.as_deref(),
                stat.as_deref(),
                boot.as_deref(),
            )
        }
        GuestOs::Windows => {
            windows_state(exit.as_deref(), pid.is_some(), lock(agent, &file("lock"))?)
        }
    };
    if state == State::EndedWithoutStatus {
        state = recheck(state, small_file(agent, &file("exit"))?.as_deref());
    }
    let mut data = read_log(agent, &file("out"), since, max_bytes)?;
    let cut = trim_read(&mut data, max_bytes);
    Ok(Some(Read {
        state,
        next: since + data.len() as u64,
        data,
        cut,
    }))
}

/// Output from `since` in the log at `path`. A Windows wrapper holds the log for a moment while it appends a chunk, and
/// the agent cannot open it then; after a few tries the read returns nothing, and the next read gets the output.
fn read_log(agent: &Agent, path: &str, since: u64, max_bytes: usize) -> Result<Vec<u8>> {
    for _ in 1..BUSY_TRIES {
        match agent.read_file(path, since, max_bytes) {
            Ok((data, _)) => return Ok(data),
            Err(e) if is_missing(&e) => return Ok(Vec::new()),
            Err(e) if is_busy(&e) => std::thread::sleep(BUSY_PAUSE),
            Err(e) => return Err(e),
        }
    }
    match agent.read_file(path, since, max_bytes) {
        Ok((data, _)) => Ok(data),
        Err(e) if is_missing(&e) || is_busy(&e) => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Whether a Windows wrapper holds the lock file at `path`.
fn lock(agent: &Agent, path: &str) -> Result<Lock> {
    match agent.read_file(path, 0, 1) {
        Ok(_) => Ok(Lock::Free),
        Err(e) if is_busy(&e) => Ok(Lock::Held),
        Err(e) if is_missing(&e) => Ok(Lock::Missing),
        Err(e) => Err(e),
    }
}

/// A small file's text, or `None` when it does not exist.
fn small_file(agent: &Agent, path: &str) -> Result<Option<String>> {
    match agent.read_file(path, 0, 4096) {
        Ok((data, _)) => Ok(Some(String::from_utf8_lossy(&data).into_owned())),
        Err(e) if is_missing(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Whether a guest file call failed because the file or its directory does not exist. Linux's text is the same for
/// both; Windows' differ, and follow the guest's display language, which the test bed has as English.
fn is_missing(e: &anyhow::Error) -> bool {
    let text = format!("{e:#}");
    [
        "No such file or directory",
        "The system cannot find the file specified",
        "The system cannot find the path specified",
    ]
    .iter()
    .any(|missing| text.contains(missing))
}

/// Whether a Windows guest file call failed because another process holds the file open.
fn is_busy(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("being used by another process")
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as B64;
    use serde_json::{Value, json};

    use super::*;
    use crate::agent::Channel;

    #[test]
    fn only_lower_case_hyphenated_v4_ids_pass() {
        let id = Uuid::new_v4().hyphenated().to_string();
        assert!(parse_id(&id).is_ok());
        for bad in [
            id.to_uppercase(),
            id.replace('-', ""),
            "../../etc".into(),
            format!("{id}/x"),
            "6ba7b810-9dad-11d1-80b4-00c04fd430c8".into(), // version 1
            String::new(),
        ] {
            assert!(parse_id(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_command_never_appears_in_the_script_text() {
        let id = Uuid::new_v4();
        let command = "echo \"$HOME\" '${VAR}' `id`; rm -rf /tmp/x\nprintf '%s' $(date)";
        let spec = launch_spec(GuestOs::Linux, &id, command);
        assert_eq!(spec.stdin.as_deref(), Some(command.as_bytes()));
        assert_eq!(spec.path, "/bin/sh");
        assert_eq!(
            spec.args,
            [
                "-c",
                LAUNCHER,
                "launcher",
                &id.hyphenated().to_string(),
                WRAPPER
            ]
        );
        for arg in &spec.args {
            assert!(!arg.contains("/tmp/x"), "{arg}");
        }
    }

    #[test]
    fn the_start_time_is_read_after_the_last_parenthesis() {
        let stat = "4242 (my (odd) name) S 1 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 200";
        assert_eq!(stat_start_time(stat), Some(987654));
        assert_eq!(stat_start_time("garbage"), None);
        assert_eq!(stat_start_time("1 (x) S 1"), None);
    }

    #[test]
    fn the_state_follows_the_files() {
        let stat = "77 (sh) S 1 77 77 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 5555 0 0";
        let pid = Some("77 5555 b-1\n");
        let boot = Some("b-1\n");
        assert_eq!(
            state(Some("143\n"), pid, Some(stat), boot),
            State::Exited(143)
        );
        assert_eq!(state(None, None, None, boot), State::Starting);
        assert_eq!(state(None, pid, Some(stat), boot), State::Running);
        assert_eq!(
            state(None, Some("77 1234 b-1\n"), Some(stat), boot),
            State::EndedWithoutStatus,
            "a reused PID"
        );
        assert_eq!(
            state(None, pid, Some(stat), Some("b-2\n")),
            State::EndedWithoutStatus,
            "the same PID and start time in a later boot"
        );
        assert_eq!(state(None, pid, None, boot), State::EndedWithoutStatus);
    }

    #[test]
    fn an_exit_written_between_the_reads_wins_over_ended_without_status() {
        assert_eq!(
            recheck(State::EndedWithoutStatus, Some("0\n")),
            State::Exited(0)
        );
        assert_eq!(
            recheck(State::EndedWithoutStatus, None),
            State::EndedWithoutStatus
        );
        assert_eq!(recheck(State::Running, Some("0\n")), State::Running);
    }

    #[test]
    fn a_cut_read_leaves_an_incomplete_character_for_the_next() {
        let text = "ab€".as_bytes(); // € is three bytes
        assert_eq!(whole_chars(&text[..3], true), 2);
        assert_eq!(whole_chars(&text[..4], true), 2);
        assert_eq!(whole_chars(text, true), 5);
        assert_eq!(whole_chars(&text[..4], false), 4, "the end of the output");
        assert_eq!(whole_chars(&[0xE2], true), 1, "never an empty read");
        assert_eq!(whole_chars(b"abc", true), 3);
    }

    #[test]
    fn only_a_missing_file_counts_as_missing() {
        assert!(is_missing(&anyhow!(
            "guest agent 'guest-file-open' error: failed to open file: No such file or directory"
        )));
        assert!(!is_missing(&anyhow!(
            "guest agent 'guest-file-open' failed: Guest agent is not responding"
        )));
    }

    #[test]
    fn the_list_skips_lines_without_a_job_id() {
        let id = Uuid::new_v4().hyphenated().to_string();
        let text = format!(
            "{id} 1700000000 exited 0\n{id} 1700000001 running\nlost+found 1 running\n{id} 1700000002 ended\n"
        );
        let listed = parse_list(&text);
        assert_eq!(listed.len(), 3);
        assert_eq!((listed[0].state, listed[0].exit_code), ("exited", Some(0)));
        assert_eq!(listed[1].state, "running");
        assert_eq!(listed[2].state, "ended without status");
    }

    /// What a scripted Windows guest answers when a file is opened.
    #[derive(Clone)]
    enum Guarded {
        Present(&'static [u8]),
        /// Another process holds the file open, as the wrapper holds its lock.
        Held,
        Missing,
    }

    /// A Windows guest that serves files by path and answers every `guest-exec` with one exit code and output. A path
    /// with several answers gives them in turn and then keeps the last; a path with none is missing.
    struct WinGuest {
        files: RefCell<HashMap<String, Vec<Guarded>>>,
        exit_code: i64,
        stdout: &'static str,
        execs: RefCell<Vec<Value>>,
        handles: RefCell<Vec<Vec<u8>>>,
    }

    impl WinGuest {
        fn new(exit_code: i64, stdout: &'static str) -> Self {
            Self {
                files: RefCell::default(),
                exit_code,
                stdout,
                execs: RefCell::default(),
                handles: RefCell::default(),
            }
        }

        fn file(self, id: &Uuid, name: &str, answers: &[Guarded]) -> Self {
            self.files
                .borrow_mut()
                .insert(job_file(GuestOs::Windows, id, name), answers.to_vec());
            self
        }
    }

    impl Channel for WinGuest {
        fn command(&self, request: &str, _: i32) -> std::result::Result<String, String> {
            let req: Value = serde_json::from_str(request).unwrap();
            let args = &req["arguments"];
            let ret = match req["execute"].as_str().unwrap() {
                "guest-exec" => {
                    self.execs.borrow_mut().push(args.clone());
                    json!({ "pid": 1 })
                }
                "guest-exec-status" => json!({
                    "exited": true,
                    "exitcode": self.exit_code,
                    "out-data": B64.encode(self.stdout),
                }),
                "guest-file-open" => {
                    let path = args["path"].as_str().unwrap();
                    let missing = format!(
                        "unable to execute QEMU agent command 'guest-file-open': failed to open file '{path}': The \
                         system cannot find the file specified."
                    );
                    let mut files = self.files.borrow_mut();
                    let answers = files.get_mut(path).ok_or_else(|| missing.clone())?;
                    let answer = if answers.len() > 1 {
                        answers.remove(0)
                    } else {
                        answers[0].clone()
                    };
                    match answer {
                        Guarded::Missing => return Err(missing),
                        Guarded::Held => {
                            return Err(format!(
                                "unable to execute QEMU agent command 'guest-file-open': failed to open file \
                                 '{path}': The process cannot access the file because it is being used by another \
                                 process."
                            ));
                        }
                        Guarded::Present(data) => {
                            self.handles.borrow_mut().push(data.to_vec());
                            json!(self.handles.borrow().len() - 1)
                        }
                    }
                }
                "guest-file-seek" => {
                    let handle = args["handle"].as_u64().unwrap() as usize;
                    let offset = args["offset"].as_u64().unwrap() as usize;
                    let mut handles = self.handles.borrow_mut();
                    let data = &mut handles[handle];
                    *data = data.split_off(offset.min(data.len()));
                    json!({ "position": offset, "eof": false })
                }
                "guest-file-read" => {
                    let handle = args["handle"].as_u64().unwrap() as usize;
                    let data = std::mem::take(&mut self.handles.borrow_mut()[handle]);
                    json!({ "count": data.len(), "buf-b64": B64.encode(&data), "eof": true })
                }
                _ => Value::Null,
            };
            Ok(json!({ "return": ret }).to_string())
        }
    }

    fn win_read(guest: &WinGuest, id: &Uuid, since: u64) -> Option<Read> {
        read(&Agent::new(guest, 10), GuestOs::Windows, id, since, 1 << 20).unwrap()
    }

    /// The script a Windows `ExecSpec` carries in its `-EncodedCommand` argument.
    fn encoded_script(spec: &Value) -> String {
        let bytes = B64
            .decode(
                spec["arg"]
                    .as_array()
                    .unwrap()
                    .last()
                    .unwrap()
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
        let utf16: Vec<u16> = bytes
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16(&utf16).unwrap()
    }

    #[test]
    fn a_windows_job_runs_while_its_wrapper_holds_the_lock() {
        let id = Uuid::new_v4();
        let guest = WinGuest::new(0, "")
            .file(&id, "cmd.ps1", &[Guarded::Present(b"\xEF\xBB\xBFping")])
            .file(
                &id,
                "pid",
                &[Guarded::Present(b"4242 639299647847946337\r\n")],
            )
            .file(&id, "lock", &[Guarded::Held])
            .file(&id, "out", &[Guarded::Present(b"step 1\r\nstep 2\r\n")]);
        let r = win_read(&guest, &id, 8).unwrap();
        assert_eq!(r.state, State::Running);
        assert_eq!(r.data, b"step 2\r\n");
        assert_eq!(r.next, 16);
    }

    #[test]
    fn a_windows_job_reads_as_its_files_say() {
        let id = Uuid::new_v4();
        let cmd = [Guarded::Present(b"exit 7")];
        let pid = [Guarded::Present(b"4242 1\r\n")];
        let free = [Guarded::Present(b"")];

        let starting = WinGuest::new(0, "").file(&id, "cmd.ps1", &cmd);
        assert_eq!(win_read(&starting, &id, 0).unwrap().state, State::Starting);

        let exited = WinGuest::new(0, "")
            .file(&id, "cmd.ps1", &cmd)
            .file(&id, "pid", &pid)
            .file(&id, "lock", &free)
            .file(&id, "exit", &[Guarded::Present(b"7\r\n")]);
        assert_eq!(win_read(&exited, &id, 0).unwrap().state, State::Exited(7));

        // A free lock without an exit code: the wrapper was killed or the guest rebooted.
        let ended = WinGuest::new(0, "")
            .file(&id, "cmd.ps1", &cmd)
            .file(&id, "pid", &pid)
            .file(&id, "lock", &free);
        assert_eq!(
            win_read(&ended, &id, 0).unwrap().state,
            State::EndedWithoutStatus
        );

        let no_job = WinGuest::new(0, "");
        assert!(win_read(&no_job, &id, 0).is_none());
    }

    #[test]
    fn a_windows_exit_code_written_after_the_first_check_wins() {
        let id = Uuid::new_v4();
        // The job ends between the first read of `exit` and the lock check: the lock is free, and the re-read of
        // `exit` finds the code.
        let guest = WinGuest::new(0, "")
            .file(&id, "cmd.ps1", &[Guarded::Present(b"exit 3")])
            .file(&id, "pid", &[Guarded::Present(b"4242 1\r\n")])
            .file(&id, "lock", &[Guarded::Present(b"")])
            .file(&id, "exit", &[Guarded::Missing, Guarded::Present(b"3\r\n")]);
        assert_eq!(win_read(&guest, &id, 0).unwrap().state, State::Exited(3));
    }

    #[test]
    fn a_log_the_wrapper_keeps_holding_reads_as_no_new_output() {
        let id = Uuid::new_v4();
        let guest = WinGuest::new(0, "")
            .file(&id, "cmd.ps1", &[Guarded::Present(b"ping")])
            .file(&id, "pid", &[Guarded::Present(b"4242 1\r\n")])
            .file(&id, "lock", &[Guarded::Held])
            .file(&id, "out", &[Guarded::Held]);
        let r = win_read(&guest, &id, 5).unwrap();
        assert_eq!((r.state, r.data.len(), r.next), (State::Running, 0, 5));

        let once = WinGuest::new(0, "")
            .file(&id, "cmd.ps1", &[Guarded::Present(b"ping")])
            .file(&id, "pid", &[Guarded::Present(b"4242 1\r\n")])
            .file(&id, "lock", &[Guarded::Held])
            .file(&id, "out", &[Guarded::Held, Guarded::Present(b"hello")]);
        assert_eq!(win_read(&once, &id, 0).unwrap().data, b"hello");
    }

    #[test]
    fn the_windows_launcher_gets_the_command_on_stdin_and_the_id_as_a_literal() {
        let command = "Write-Output 'päivää'; Remove-Item C:\\x -Recurse\nexit 5";
        let guest = WinGuest::new(0, "started with Win32_Process.Create\r\n");
        let (started, how) = start(&Agent::new(&guest, 10), GuestOs::Windows, command).unwrap();
        assert_eq!(how, "started with Win32_Process.Create");
        let execs = guest.execs.borrow();
        let spec = &execs[0];
        assert_eq!(spec["path"], "powershell.exe");
        assert_eq!(
            B64.decode(spec["input-data"].as_str().unwrap()).unwrap(),
            command.as_bytes()
        );
        let script = encoded_script(spec);
        let expected_head = format!(
            "$id = '{}'\n$wrapper = {}\n",
            started.hyphenated(),
            ps_quote(WIN_WRAPPER)
        );
        assert!(script.starts_with(&expected_head), "{script}");
        assert!(script.ends_with(WIN_LAUNCHER));
        assert!(!script.contains("Remove-Item C:\\x"));
    }

    #[test]
    fn a_refused_windows_launch_names_the_reason() {
        let guest = WinGuest::new(
            3,
            "refused: C:\\ProgramData\\virtmanager-mcp\\jobs is a junction or symbolic link\r\n",
        );
        let error = start(&Agent::new(&guest, 10), GuestOs::Windows, "ping")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("the job did not start: refused: C:\\ProgramData\\virtmanager-mcp\\jobs is a junction"),
            "{error}"
        );
    }

    #[test]
    fn a_windows_stop_ends_the_tree_and_says_force_changes_nothing() {
        let id = Uuid::new_v4();
        let guest = WinGuest::new(0, "ended\r\n");
        let message = stop(&Agent::new(&guest, 10), GuestOs::Windows, &id, false).unwrap();
        assert!(message.contains("taskkill /T /F"), "{message}");
        assert!(message.contains("force makes no difference"), "{message}");
        let script = encoded_script(&guest.execs.borrow()[0]);
        assert!(script.starts_with(&format!("$id = '{}'\n", id.hyphenated())));
        assert!(script.ends_with(WIN_STOP));

        let reused = WinGuest::new(4, "not running\r\n");
        let error = stop(&Agent::new(&reused, 10), GuestOs::Windows, &id, true)
            .unwrap_err()
            .to_string();
        assert_eq!(error, format!("job {id} is not running"));
    }

    #[test]
    fn the_windows_list_reads_crlf_lines() {
        let id = Uuid::new_v4().hyphenated().to_string();
        let guest = WinGuest::new(
            0,
            Box::leak(
                format!("{id} 1794368699 exited 7\r\n{id} 1794368703 ended\r\n").into_boxed_str(),
            ),
        );
        let listed = list(&Agent::new(&guest, 10), GuestOs::Windows).unwrap();
        assert_eq!((listed[0].state, listed[0].exit_code), ("exited", Some(7)));
        assert_eq!(listed[1].state, "ended without status");
        assert!(encoded_script(&guest.execs.borrow()[0]).ends_with(WIN_LIST));
    }

    #[test]
    fn the_windows_launch_fits_a_command_line() {
        // Windows limits a command line to 32,767 characters, and the launcher carries the wrapper inside its
        // -EncodedCommand, so a growing script would make every job_start fail in the guest. The margin leaves room for
        // the program name and the separators.
        let spec = launch_spec(GuestOs::Windows, &Uuid::new_v4(), "'x'");
        let length = spec.path.len() + spec.args.iter().map(|a| a.len() + 3).sum::<usize>();
        assert!(length < 30_000, "{length} characters");
    }

    #[test]
    fn the_windows_scripts_hold_their_checks() {
        for check in [
            "[IO.FileAttributes]::ReparsePoint",
            "is not owned by SYSTEM or Administrators",
            "AreAccessRulesProtected",
            "grants access to",
            "[IO.Directory]::CreateDirectory($path, $acl)",
            "[Console]::OpenStandardInput()",
            "Win32_Process -MethodName Create",
        ] {
            assert!(WIN_LAUNCHER.contains(check), "{check}");
        }
        assert!(WIN_STOP.contains("StartTime.ToUniversalTime().Ticks -ne [long]$recorded[1]"));
        assert!(WIN_STOP.contains("taskkill.exe /T /F /PID"));
        // The exit code is written before the lock is let go, so a free lock without one means the job was ended.
        let exit = WIN_WRAPPER.find("\"$dir\\exit\"").unwrap();
        let unlock = WIN_WRAPPER.find("$lock.Close()").unwrap();
        assert!(exit < unlock);
        for script in [WIN_LAUNCHER, WIN_WRAPPER, WIN_STOP, WIN_LIST] {
            assert!(!script.contains("Invoke-Expression"), "{script}");
        }
    }

    #[test]
    fn windows_missing_and_busy_errors_are_recognised() {
        for text in [
            "failed to open file 'C:\\x\\exit': The system cannot find the file specified.",
            "failed to open file 'C:\\nojob\\out': The system cannot find the path specified.",
        ] {
            assert!(is_missing(&anyhow!("{text}")), "{text}");
            assert!(!is_busy(&anyhow!("{text}")), "{text}");
        }
        let busy = anyhow!(
            "failed to open file 'C:\\x\\lock': The process cannot access the file because it is being used by \
             another process."
        );
        assert!(is_busy(&busy) && !is_missing(&busy));
    }

    #[test]
    fn the_launcher_falls_back_to_setsid_and_discards_a_job_it_cannot_start() {
        let systemd = LAUNCHER.find("systemd-run --quiet").unwrap();
        let setsid = LAUNCHER
            .find("setsid /bin/sh \"${dir}/run\" \"${dir}\"")
            .unwrap();
        let failed = LAUNCHER.find("failed: neither").unwrap();
        assert!(systemd < setsid && setsid < failed);
        // The fallback runs only when systemd is absent or systemd-run fails, and a failed start leaves no directory.
        assert!(LAUNCHER.contains("if [ -d /run/systemd/system ] && command -v systemd-run"));
        assert!(LAUNCHER[setsid..failed].contains("discard"));
    }

    #[test]
    fn the_guest_scripts_name_no_bash_features() {
        for script in [LAUNCHER, WRAPPER, STOP, LIST] {
            assert!(script.starts_with("# shellcheck shell=sh"));
            assert!(!script.contains("[["), "{script}");
        }
    }
}
