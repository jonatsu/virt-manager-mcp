//! Background jobs in Linux guests: the launcher, the job file layout, and how a job's state is read.
//!
//! A job lives in `/var/lib/virtmanager-mcp/jobs/<id>/` in the guest: `cmd` holds the command, `out` its merged
//! stdout and stderr, `pid` the wrapper's PID and start time, and `exit` the exit code once it ends. The guest-side
//! scripts are fixed texts in `src/jobs/`; ids and commands reach them only as arguments and stdin.

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use uuid::Uuid;

use crate::agent::{Agent, ExecSpec, GuestOs};

const LAUNCHER: &str = include_str!("jobs/launcher.sh");
const WRAPPER: &str = include_str!("jobs/wrapper.sh");
const STOP: &str = include_str!("jobs/stop.sh");
const LIST: &str = include_str!("jobs/list.sh");

/// Where jobs live in a Linux guest.
pub const JOBS_DIR: &str = "/var/lib/virtmanager-mcp/jobs";
/// Time the launcher and the stop and list helpers get.
const HELPER_TIMEOUT: Duration = Duration::from_secs(15);
/// Most output a helper may return.
const HELPER_OUTPUT: usize = 256 * 1024;

/// A job ID: a UUID v4 in its hyphenated lower-case form, so it can never become another path.
pub fn parse_id(id: &str) -> Result<Uuid> {
    let parsed = Uuid::try_parse(id).map_err(|_| anyhow!("'{id}' is not a job ID"))?;
    if parsed.get_version_num() != 4 || parsed.hyphenated().to_string() != id {
        bail!("'{id}' is not a job ID");
    }
    Ok(parsed)
}

fn job_dir(id: &Uuid) -> String {
    format!("{JOBS_DIR}/{}", id.hyphenated())
}

/// Refuses guests other than Linux; Windows jobs come later.
pub fn require_linux(agent: &Agent) -> Result<()> {
    match agent.os()? {
        GuestOs::Linux => Ok(()),
        GuestOs::Windows => bail!(
            "jobs are not available for Windows guests yet; they come with Windows job support (live-interaction \
             run 3)"
        ),
    }
}

/// The `guest-exec` call that starts job `id` running `command`. The command travels on stdin.
fn launch_spec(id: &Uuid, command: &str) -> ExecSpec {
    let mut spec = ExecSpec::new(
        "/bin/sh",
        [
            "-c",
            LAUNCHER,
            "launcher",
            &id.hyphenated().to_string(),
            WRAPPER,
        ],
    );
    spec.stdin = Some(command.as_bytes().to_vec());
    spec
}

/// Starts `command` as a new job and returns its ID and how it started.
pub fn start(agent: &Agent, command: &str) -> Result<(Uuid, String)> {
    let id = Uuid::new_v4();
    let out = agent.exec(&launch_spec(&id, command), HELPER_TIMEOUT, HELPER_OUTPUT)?;
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

/// Signals a running job's process group: TERM, or KILL with `force`.
pub fn stop(agent: &Agent, id: &Uuid, force: bool) -> Result<String> {
    let sig = if force { "KILL" } else { "TERM" };
    let dir = job_dir(id);
    let spec = ExecSpec::new("/bin/sh", ["-c", STOP, "stop", &dir, sig]);
    let out = agent.exec(&spec, HELPER_TIMEOUT, HELPER_OUTPUT)?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    match out.exit_code {
        Some(0) => Ok(format!("sent {sig} to job {id}")),
        Some(3 | 4) => bail!("job {id} is {text}"),
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

pub fn list(agent: &Agent) -> Result<Vec<Listed>> {
    let spec = ExecSpec::new("/bin/sh", ["-c", LIST, "list"]);
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

/// A job's state from its `exit` file, its `pid` file, and the current `/proc/<pid>/stat`, each `None` when missing.
pub fn state(exit: Option<&str>, pid: Option<&str>, stat: Option<&str>) -> State {
    if let Some(code) = exit.and_then(|e| e.trim().parse().ok()) {
        return State::Exited(code);
    }
    let Some(recorded) = pid.and_then(|p| p.split_whitespace().nth(1)) else {
        return State::Starting;
    };
    match stat.and_then(stat_start_time) {
        Some(now) if now.to_string() == recorded => State::Running,
        _ => State::EndedWithoutStatus,
    }
}

/// Output of a job from `since`, with the offset to read from next.
pub struct Read {
    pub state: State,
    pub data: Vec<u8>,
    pub next: u64,
}

/// Reads job `id`'s output from `since`, at most `max_bytes`, and its state, through the agent's file calls.
pub fn read(agent: &Agent, id: &Uuid, since: u64, max_bytes: usize) -> Result<Read> {
    let dir = job_dir(id);
    if small_file(agent, &format!("{dir}/cmd"))?.is_none() {
        bail!("no job {id} in this guest");
    }
    // The state comes first: output read after an exit code is complete.
    let exit = small_file(agent, &format!("{dir}/exit"))?;
    let pid = small_file(agent, &format!("{dir}/pid"))?;
    let stat = match pid.as_deref().and_then(|p| p.split_whitespace().next()) {
        Some(p) if p.bytes().all(|b| b.is_ascii_digit()) => {
            small_file(agent, &format!("/proc/{p}/stat"))?
        }
        _ => None,
    };
    let state = state(exit.as_deref(), pid.as_deref(), stat.as_deref());
    let data = match agent.read_file(&format!("{dir}/out"), since, max_bytes) {
        Ok((data, _)) => data,
        Err(e) if is_missing(&e) => Vec::new(),
        Err(e) => return Err(e),
    };
    Ok(Read {
        state,
        next: since + data.len() as u64,
        data,
    })
}

/// A small file's text, or `None` when it does not exist.
fn small_file(agent: &Agent, path: &str) -> Result<Option<String>> {
    match agent.read_file(path, 0, 4096) {
        Ok((data, _)) => Ok(Some(String::from_utf8_lossy(&data).into_owned())),
        Err(e) if is_missing(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Whether a guest file call failed because the file does not exist.
fn is_missing(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("No such file or directory")
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let spec = launch_spec(&id, command);
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
            assert!(!arg.contains("rm -rf"), "{arg}");
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
        assert_eq!(
            state(Some("143\n"), Some("77 5555\n"), Some(stat)),
            State::Exited(143)
        );
        assert_eq!(state(None, None, None), State::Starting);
        assert_eq!(state(None, Some("77 5555\n"), Some(stat)), State::Running);
        assert_eq!(
            state(None, Some("77 1234\n"), Some(stat)),
            State::EndedWithoutStatus,
            "a reused PID"
        );
        assert_eq!(
            state(None, Some("77 5555\n"), None),
            State::EndedWithoutStatus
        );
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

    #[test]
    fn the_guest_scripts_name_no_bash_features() {
        for script in [LAUNCHER, WRAPPER, STOP, LIST] {
            assert!(script.starts_with("# shellcheck shell=sh"));
            assert!(!script.contains("[["), "{script}");
        }
    }
}
