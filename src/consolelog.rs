//! Console logs that outlive their VM: every console session the server opens also appends its raw output to a file
//! in the first host pull directory, `console/<vm>/<UTC start>-g<generation>.log`. A file is created new and never
//! overwritten, the server never deletes one, and a file stops growing at its size cap.

use std::ffi::OsStr;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rustix::fs::{Mode, OFlags};

use crate::pull::open_or_create_dir;

/// Where console logs go and how large one may grow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogTarget {
    /// The pull directory the logs live under.
    pub dir: PathBuf,
    /// The most bytes of console output one file holds.
    pub max_bytes: u64,
}

/// The subdirectory of the pull directory that holds the logs.
pub const CONSOLE_DIR: &str = "console";

/// An open log, appended to by the console's reader thread.
pub struct ConsoleLog {
    file: File,
    written: u64,
    max_bytes: u64,
    done: bool,
}

impl ConsoleLog {
    /// Appends `data`, up to the size cap. At the cap it writes a closing line and stops; a failed write stops the
    /// log too, and the console capture goes on without it.
    pub fn append(&mut self, data: &[u8]) {
        if self.done {
            return;
        }
        let room = self.max_bytes.saturating_sub(self.written);
        let take = usize::try_from(room).unwrap_or(usize::MAX).min(data.len());
        let mut result = self.file.write_all(&data[..take]);
        self.written += take as u64;
        if result.is_ok() && take < data.len() {
            self.done = true;
            result = writeln!(
                self.file,
                "\n[virtmanager-mcp: log cut at {} bytes, limits.console_log_bytes]",
                self.max_bytes
            );
        }
        if let Err(e) = result {
            tracing::warn!("console log write failed, the log stops here: {e}");
            self.done = true;
        }
    }
}

/// Creates the log of `vm`'s console session `generation`, started at `now`, and returns its path and the open log.
/// Every directory level is opened without following a symlink, and the file is created exclusively, because agents
/// copy guest files into the pull directory and could otherwise plant a link that redirects the log.
///
/// # Errors
///
/// Fails when the pull directory or a level below it cannot be opened or created, or the file exists.
pub fn create(
    target: &LogTarget,
    vm: &str,
    generation: u64,
    now: SystemTime,
) -> Result<(PathBuf, ConsoleLog)> {
    let pull = rustix::fs::open(
        &target.dir,
        OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .with_context(|| format!("opening pull directory {}", target.dir.display()))?;
    let vm_dir_name = safe_component(vm);
    let console = open_or_create_dir(&pull, OsStr::new(CONSOLE_DIR))
        .with_context(|| format!("{}", target.dir.join(CONSOLE_DIR).display()))?;
    let vm_dir = open_or_create_dir(&console, OsStr::new(&vm_dir_name)).with_context(|| {
        format!(
            "{}",
            target.dir.join(CONSOLE_DIR).join(&vm_dir_name).display()
        )
    })?;
    let name = format!("{}-g{generation}.log", utc_stamp(now));
    let path = target.dir.join(CONSOLE_DIR).join(&vm_dir_name).join(&name);
    let fd = rustix::fs::openat(
        &vm_dir,
        name.as_str(),
        OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::WRONLY | OFlags::CLOEXEC,
        Mode::from(0o644),
    )
    .with_context(|| format!("creating {}", path.display()))?;
    Ok((
        path,
        ConsoleLog {
            file: File::from(fd),
            written: 0,
            max_bytes: target.max_bytes,
            done: false,
        },
    ))
}

/// `name` as one safe path component: characters other than letters, digits, `.`, `_` and `-` become `_`, and a
/// leading `.` gets a `_` before it, so no VM name can make `..` or a hidden name.
fn safe_component(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() || out.starts_with('.') {
        out.insert(0, '_');
    }
    out
}

/// `now` in UTC as `YYYYMMDDTHHMMSSZ`.
fn utc_stamp(now: SystemTime) -> String {
    let secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// The proleptic Gregorian date of `days` after 1970-01-01, by Howard Hinnant's `civil_from_days`.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("vmm-consolelog-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn utc_stamps_match_known_dates() {
        let at = |secs| UNIX_EPOCH + Duration::from_secs(secs);
        assert_eq!(utc_stamp(at(0)), "19700101T000000Z");
        assert_eq!(utc_stamp(at(951_782_400)), "20000229T000000Z");
        assert_eq!(utc_stamp(at(1_795_523_402)), "20261124T123002Z");
    }

    #[test]
    fn vm_names_become_one_safe_component() {
        assert_eq!(safe_component("t-a_1.2"), "t-a_1.2");
        assert_eq!(safe_component(".."), "_..");
        assert_eq!(safe_component("a/b c"), "a_b_c");
        assert_eq!(safe_component(""), "_");
    }

    #[test]
    fn a_log_is_named_by_vm_and_run_and_never_overwritten() {
        let dir = temp("name");
        let target = LogTarget {
            dir: dir.clone(),
            max_bytes: 1024,
        };
        let now = UNIX_EPOCH + Duration::from_secs(1_795_523_402);
        let (path, mut log) = create(&target, "t-a", 7, now).unwrap();
        assert_eq!(path, dir.join("console/t-a/20261124T123002Z-g7.log"));
        log.append(b"boot\n");
        drop(log);
        assert!(create(&target, "t-a", 7, now).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"boot\n");
        assert!(create(&target, "t-a", 8, now).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_planted_symlink_is_not_followed() {
        let dir = temp("link");
        let elsewhere = temp("elsewhere");
        std::os::unix::fs::symlink(&elsewhere, dir.join("console")).unwrap();
        let target = LogTarget {
            dir: dir.clone(),
            max_bytes: 1024,
        };
        assert!(create(&target, "t-a", 1, SystemTime::now()).is_err());
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&elsewhere).unwrap();
    }

    #[test]
    fn a_log_stops_at_its_cap_with_a_closing_line() {
        let dir = temp("cap");
        let target = LogTarget {
            dir: dir.clone(),
            max_bytes: 8,
        };
        let (path, mut log) = create(&target, "t-a", 1, SystemTime::now()).unwrap();
        log.append(b"12345");
        log.append(b"67890");
        log.append(b"more");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("12345678\n[virtmanager-mcp: log cut at 8 bytes"),
            "{text}"
        );
        assert!(!text.contains("more"), "{text}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
