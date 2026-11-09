//! QEMU guest agent (qemu-ga) access over the virtio-serial channel, via libvirt.
//!
//! All functions here block; call them from `spawn_blocking`.

use std::io::Read;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::Serialize;
use serde_json::{Value, json};
use virt::domain::Domain;

/// Max bytes per guest-file-read request.
const FILE_CHUNK: usize = 1024 * 1024;
/// Max bytes per guest-file-write request. Larger writes (256 KiB and up) were seen to
/// hang qemu-ga past the agent timeout, while 64 KiB copies several MB/s.
const WRITE_CHUNK: usize = 64 * 1024;

/// Mode for files that `write_from` creates when the caller gives none: what a umask of 022 gives.
pub const DEFAULT_FILE_MODE: u32 = 0o644;

/// Calls `ready` every `every` until it returns true or `wait` has passed. Checks at least once.
pub fn wait_for(wait: Duration, every: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if ready() {
            return true;
        }
        if start.elapsed() >= wait {
            return false;
        }
        std::thread::sleep(every.min(wait.saturating_sub(start.elapsed())));
    }
}

/// Timeout for the small helper commands (`test`, `chmod`) that file writes run.
const HELPER_TIMEOUT: Duration = Duration::from_secs(10);

/// Parses permission bits given as octal digits, such as "644" or "0600". Only the 0o777 bits are
/// accepted: setuid, setgid and sticky bits are refused.
pub fn parse_mode(text: &str) -> Result<u32> {
    let digits = text.trim();
    let valid =
        (1..=4).contains(&digits.len()) && digits.bytes().all(|b| (b'0'..=b'7').contains(&b));
    let mode = u32::from_str_radix(digits, 8)
        .ok()
        .filter(|m| valid && *m <= 0o777);
    mode.ok_or_else(|| {
        anyhow!("invalid mode {text:?}: expected octal permission bits from 0 to 777")
    })
}

/// The mode to set after opening a file: the caller's `mode` if given, else the default for a new
/// file. An existing file keeps its mode.
fn mode_to_set(existed: bool, mode: Option<u32>) -> Option<u32> {
    match (mode, existed) {
        (Some(m), _) => Some(m),
        (None, false) => Some(DEFAULT_FILE_MODE),
        (None, true) => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GuestOs {
    Linux,
    Windows,
}

pub struct Agent<'a> {
    dom: &'a Domain,
    timeout_secs: i32,
}

impl<'a> Agent<'a> {
    pub fn new(dom: &'a Domain, timeout_secs: i32) -> Self {
        Self { dom, timeout_secs }
    }

    /// Sends one agent command and returns its `return` value.
    pub fn call(&self, execute: &str, arguments: Option<Value>) -> Result<Value> {
        let mut req = json!({ "execute": execute });
        if let Some(args) = arguments {
            req["arguments"] = args;
        }
        let reply = self
            .dom
            .qemu_agent_command(&req.to_string(), self.timeout_secs, 0)
            .map_err(|e| anyhow!("guest agent '{execute}' failed: {}", e.message()))?;
        let mut v: Value = serde_json::from_str(&reply).context("invalid guest agent reply")?;
        if let Some(err) = v.get("error") {
            bail!("guest agent '{execute}' error: {err}");
        }
        Ok(v.get_mut("return").map(Value::take).unwrap_or(Value::Null))
    }

    pub fn ping(&self) -> bool {
        self.call("guest-ping", None).is_ok()
    }

    pub fn os(&self) -> Result<GuestOs> {
        let info = self.call("guest-get-osinfo", None)?;
        Ok(match info.get("id").and_then(Value::as_str) {
            Some("mswindows") => GuestOs::Windows,
            _ => GuestOs::Linux,
        })
    }

    // ---- command execution -------------------------------------------------

    pub fn exec(
        &self,
        spec: &ExecSpec,
        timeout: Duration,
        max_output: usize,
    ) -> Result<ExecOutcome> {
        let mut args = json!({
            "path": spec.path,
            "arg": spec.args,
            "capture-output": true,
        });
        if !spec.env.is_empty() {
            args["env"] = json!(spec.env);
        }
        if let Some(input) = &spec.stdin {
            args["input-data"] = json!(B64.encode(input));
        }
        let pid = self
            .call("guest-exec", Some(args))?
            .get("pid")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow!("guest-exec returned no pid"))?;

        let start = Instant::now();
        let mut delay = Duration::from_millis(50);
        loop {
            let st = self.call("guest-exec-status", Some(json!({ "pid": pid })))?;
            if st.get("exited").and_then(Value::as_bool).unwrap_or(false) {
                let stream = |data: &str, trunc: &str| -> Result<(Vec<u8>, bool)> {
                    let mut bytes = match st.get(data).and_then(Value::as_str) {
                        Some(s) => B64.decode(s)?,
                        None => Vec::new(),
                    };
                    let mut truncated = st.get(trunc).and_then(Value::as_bool).unwrap_or(false);
                    if bytes.len() > max_output {
                        bytes.truncate(max_output);
                        truncated = true;
                    }
                    Ok((bytes, truncated))
                };
                let (stdout, stdout_truncated) = stream("out-data", "out-truncated")?;
                let (stderr, stderr_truncated) = stream("err-data", "err-truncated")?;
                return Ok(ExecOutcome {
                    pid,
                    exit_code: st.get("exitcode").and_then(Value::as_i64),
                    signal: st.get("signal").and_then(Value::as_i64),
                    timed_out: false,
                    kill_attempted: false,
                    stdout,
                    stdout_truncated,
                    stderr,
                    stderr_truncated,
                });
            }
            if start.elapsed() >= timeout {
                return Ok(ExecOutcome {
                    pid,
                    timed_out: true,
                    kill_attempted: self.kill(pid).is_ok(),
                    ..Default::default()
                });
            }
            std::thread::sleep(delay.min(timeout.saturating_sub(start.elapsed())));
            delay = (delay * 2).min(Duration::from_millis(500));
        }
    }

    /// Best-effort termination of a timed-out guest-exec process.
    fn kill(&self, pid: i64) -> Result<()> {
        let spec = match self.os()? {
            GuestOs::Linux => ExecSpec::new("kill", ["-KILL", &pid.to_string()]),
            GuestOs::Windows => {
                ExecSpec::new("taskkill.exe", ["/F", "/T", "/PID", &pid.to_string()])
            }
        };
        self.call(
            "guest-exec",
            Some(json!({ "path": spec.path, "arg": spec.args, "capture-output": false })),
        )?;
        Ok(())
    }

    // ---- file access --------------------------------------------------------

    fn open(&self, path: &str, mode: &str) -> Result<i64> {
        self.call(
            "guest-file-open",
            Some(json!({ "path": path, "mode": mode })),
        )?
        .as_i64()
        .ok_or_else(|| anyhow!("guest-file-open returned no handle"))
    }

    fn close(&self, handle: i64) {
        let _ = self.call("guest-file-close", Some(json!({ "handle": handle })));
    }

    /// Whether `path` exists in a Linux guest, following symlinks like `open` does.
    fn exists(&self, path: &str) -> Result<bool> {
        let out = self.exec(&ExecSpec::new("test", ["-e", path]), HELPER_TIMEOUT, 0)?;
        match out.exit_code {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => bail!("cannot check whether {path} exists in the guest"),
        }
    }

    /// Sets permission bits on a Linux guest file.
    fn chmod(&self, path: &str, mode: u32) -> Result<()> {
        let spec = ExecSpec::new("chmod", [format!("{mode:o}"), "--".into(), path.into()]);
        let out = self.exec(&spec, HELPER_TIMEOUT, 4096)?;
        if out.exit_code != Some(0) {
            bail!(
                "chmod {mode:o} {path} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// Reads up to `max_bytes` starting at `offset`. Returns the data and whether EOF was reached.
    pub fn read_file(&self, path: &str, offset: u64, max_bytes: usize) -> Result<(Vec<u8>, bool)> {
        let handle = self.open(path, "rb")?;
        let result = (|| {
            if offset > 0 {
                self.call(
                    "guest-file-seek",
                    Some(json!({ "handle": handle, "offset": offset, "whence": "set" })),
                )?;
            }
            let mut data = Vec::new();
            loop {
                let want = (max_bytes - data.len()).min(FILE_CHUNK);
                if want == 0 {
                    // Probe one byte so the caller learns whether more data exists.
                    let r = self.call(
                        "guest-file-read",
                        Some(json!({ "handle": handle, "count": 1 })),
                    )?;
                    let more = r.get("count").and_then(Value::as_u64).unwrap_or(0) > 0;
                    return Ok((data, !more));
                }
                let r = self.call(
                    "guest-file-read",
                    Some(json!({ "handle": handle, "count": want })),
                )?;
                if let Some(b) = r.get("buf-b64").and_then(Value::as_str) {
                    data.extend(B64.decode(b)?);
                }
                let count = r.get("count").and_then(Value::as_u64).unwrap_or(0);
                if r.get("eof").and_then(Value::as_bool).unwrap_or(false) || count == 0 {
                    return Ok((data, true));
                }
            }
        })();
        self.close(handle);
        result
    }

    pub fn write_file(
        &self,
        path: &str,
        data: &[u8],
        append: bool,
        mode: Option<u32>,
    ) -> Result<u64> {
        self.write_from(path, data, append, mode)
    }

    /// Streams `src` into a guest file in `WRITE_CHUNK` pieces. Returns the number of bytes written.
    ///
    /// The guest agent creates files world-writable (mode 666). On Linux guests a file this call
    /// creates gets `mode`, or `DEFAULT_FILE_MODE`, before any data is written; an existing file
    /// keeps its mode unless `mode` is given. Windows guests have no mode bits and reject `mode`.
    pub fn write_from(
        &self,
        path: &str,
        mut src: impl Read,
        append: bool,
        mode: Option<u32>,
    ) -> Result<u64> {
        let linux = self.os()? == GuestOs::Linux;
        if !linux && mode.is_some() {
            bail!("mode is only supported on Linux guests");
        }
        let new_mode = if linux {
            mode_to_set(self.exists(path)?, mode)
        } else {
            None
        };
        let handle = self.open(path, if append { "ab" } else { "wb" })?;
        let result = (|| {
            if let Some(m) = new_mode {
                self.chmod(path, m)?;
            }
            let mut written = 0u64;
            let mut buf = Vec::with_capacity(WRITE_CHUNK);
            loop {
                buf.clear();
                (&mut src).take(WRITE_CHUNK as u64).read_to_end(&mut buf)?;
                if buf.is_empty() {
                    break;
                }
                let r = self.call(
                    "guest-file-write",
                    Some(json!({ "handle": handle, "buf-b64": B64.encode(&buf) })),
                )?;
                let n = r.get("count").and_then(Value::as_u64).unwrap_or(0);
                written += n;
                if n != buf.len() as u64 {
                    bail!("short write: only {written} bytes written");
                }
            }
            self.call("guest-file-flush", Some(json!({ "handle": handle })))?;
            Ok(written)
        })();
        self.close(handle);
        result
    }
}

#[derive(Debug, Clone)]
pub struct ExecSpec {
    pub path: String,
    pub args: Vec<String>,
    pub env: Vec<String>,
    pub stdin: Option<Vec<u8>>,
}

impl ExecSpec {
    pub fn new<I, S>(path: &str, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            path: path.into(),
            args: args.into_iter().map(Into::into).collect(),
            env: Vec::new(),
            stdin: None,
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct ExecOutcome {
    pub pid: i64,
    pub exit_code: Option<i64>,
    pub signal: Option<i64>,
    pub timed_out: bool,
    pub kill_attempted: bool,
    #[serde(skip)]
    pub stdout: Vec<u8>,
    pub stdout_truncated: bool,
    #[serde(skip)]
    pub stderr: Vec<u8>,
    pub stderr_truncated: bool,
}

/// Wraps a PowerShell script so it can be passed safely as a single argument.
pub fn powershell(script: &str) -> ExecSpec {
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    ExecSpec::new(
        "powershell.exe",
        [
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-ExecutionPolicy".to_string(),
            "Bypass".to_string(),
            "-EncodedCommand".to_string(),
            B64.encode(utf16),
        ],
    )
}

/// Quotes a string as a PowerShell single-quoted literal. PowerShell also treats
/// the Unicode single quotation marks as quote characters, so those are doubled too.
pub fn ps_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps_quote_escapes_all_single_quote_variants() {
        assert_eq!(ps_quote("C:\\a b"), "'C:\\a b'");
        assert_eq!(ps_quote("it's"), "'it''s'");
        assert_eq!(ps_quote("x\u{2019}; rm"), "'x\u{2019}\u{2019}; rm'");
    }

    #[test]
    fn powershell_encodes_utf16le() {
        let spec = powershell("echo é");
        let decoded = B64.decode(spec.args.last().unwrap()).unwrap();
        let utf16: Vec<u16> = decoded
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(String::from_utf16(&utf16).unwrap(), "echo é");
    }
}

#[cfg(test)]
mod mode_tests {
    use super::*;

    #[test]
    fn parse_mode_accepts_octal_permission_bits() {
        assert_eq!(parse_mode("644").unwrap(), 0o644);
        assert_eq!(parse_mode("0600").unwrap(), 0o600);
        assert_eq!(parse_mode(" 755 ").unwrap(), 0o755);
        assert_eq!(parse_mode("0").unwrap(), 0);
    }

    #[test]
    fn parse_mode_refuses_special_bits_and_non_octal() {
        for bad in [
            "4755", "1777", "7777", "8", "64a", "", "0o644", "-644", "06440",
        ] {
            assert!(parse_mode(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn new_file_gets_the_default_mode() {
        assert_eq!(mode_to_set(false, None), Some(DEFAULT_FILE_MODE));
    }

    #[test]
    fn existing_file_keeps_its_mode() {
        assert_eq!(mode_to_set(true, None), None);
    }

    #[test]
    fn explicit_mode_applies_to_new_and_existing_files() {
        assert_eq!(mode_to_set(false, Some(0o600)), Some(0o600));
        assert_eq!(mode_to_set(true, Some(0o640)), Some(0o640));
    }
}

#[cfg(test)]
mod wait_tests {
    use super::*;

    #[test]
    fn wait_for_returns_once_ready() {
        let mut calls = 0;
        let ready = wait_for(Duration::from_secs(5), Duration::ZERO, || {
            calls += 1;
            calls == 3
        });
        assert!(ready);
        assert_eq!(calls, 3);
    }

    #[test]
    fn wait_for_checks_once_with_zero_wait() {
        let mut calls = 0;
        assert!(!wait_for(Duration::ZERO, Duration::from_secs(1), || {
            calls += 1;
            false
        }));
        assert_eq!(calls, 1);
    }

    #[test]
    fn wait_for_gives_up_after_the_wait() {
        let start = Instant::now();
        assert!(!wait_for(
            Duration::from_millis(50),
            Duration::from_millis(10),
            || false
        ));
        let took = start.elapsed();
        assert!(
            took >= Duration::from_millis(50) && took < Duration::from_secs(1),
            "{took:?}"
        );
    }
}
