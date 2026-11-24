//! The seal mark: the `sealing` attribute of a VM's owner element, which says that a seal is copying the VM's disk
//! into a template, and how a process tells a live mark from a stale one.
//!
//! A seal holds an advisory lock on `~/.cache/virtmanager-mcp/seal-<template>.lock` for as long as it runs. A process
//! of the same account, with the same `HOME`, sees that lock, so a mark is live while the lock is held and stale when
//! it is free. Any other process cannot see the lock and honours the mark for 24 hours. Nothing writes a mark yet;
//! `manage::set_mark` is the writer the seal will use.

use std::ffi::OsString;
use std::fmt;
use std::fs::{File, TryLockError};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

/// How long a process that cannot see the seal lock honours a mark.
pub const FOREIGN_MARK_SECS: u64 = 24 * 60 * 60;

/// The version tag of the encoded value, so a later layout cannot be read as this one.
const VERSION: &str = "v1";

/// A seal's claim on a VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    /// The template the seal makes.
    pub template: String,
    /// The seal's ID, which the template's record repeats.
    pub seal_id: String,
    /// When the seal began, in seconds since the epoch.
    pub started: u64,
    /// The uid of the process that holds the seal lock.
    pub uid: u32,
    /// The seal lock's path as that process computed it.
    pub lock_path: PathBuf,
}

/// What `Mark::live` needs to know about the process and its seal locks, apart so tests need no files.
pub trait LockProbe {
    /// This process's uid.
    fn uid(&self) -> u32;
    /// Where this process puts the seal lock of `template`, or `None` when it cannot, for a name that is no safe file
    /// name or a `HOME` that is not an absolute path.
    fn lock_path(&self, template: &str) -> Option<PathBuf>;
    /// Whether another open of the lock file at `path` holds the lock now.
    ///
    /// # Errors
    ///
    /// Fails when the answer cannot be had, which the caller reads as a live mark.
    fn held(&self, path: &Path) -> Result<bool>;
}

impl Mark {
    /// Whether the mark still claims the VM at `now`. One with this process's uid and lock path is live while the
    /// lock is held. Any other mark is live until it is 24 hours old, because this process cannot see its lock.
    ///
    /// # Errors
    ///
    /// Fails when the probe cannot tell whether the lock is held.
    pub fn live(&self, now: u64, probe: &impl LockProbe) -> Result<bool> {
        if self.uid == probe.uid()
            && probe.lock_path(&self.template).as_deref() == Some(self.lock_path.as_path())
        {
            return probe.held(&self.lock_path);
        }
        Ok(now < self.started.saturating_add(FOREIGN_MARK_SECS))
    }

    /// The value of the `sealing` attribute: the version, then the template, seal ID, start time, uid and lock path,
    /// separated by `:`. Each text field escapes every byte but letters, digits and `-._~` as `%XX`, so no field can
    /// hold a separator, a quote or markup, whatever bytes a path has.
    pub fn encode(&self) -> String {
        format!(
            "{VERSION}:{}:{}:{}:{}:{}",
            escape(self.template.as_bytes()),
            escape(self.seal_id.as_bytes()),
            self.started,
            self.uid,
            escape(self.lock_path.as_os_str().as_bytes())
        )
    }

    /// Reads a value `encode` wrote.
    ///
    /// # Errors
    ///
    /// Fails for any other value: another version, a wrong number of fields, a number that does not parse, or an
    /// escape that is cut off or not hexadecimal.
    pub fn decode(value: &str) -> Result<Self> {
        let fields: Vec<&str> = value.split(':').collect();
        let [version, template, seal_id, started, uid, path] = fields.as_slice() else {
            bail!("expected 6 fields separated by ':', found {}", fields.len());
        };
        if *version != VERSION {
            bail!("unknown version '{version}'");
        }
        Ok(Self {
            template: String::from_utf8(unescape(template)?).context("the template name")?,
            seal_id: String::from_utf8(unescape(seal_id)?).context("the seal ID")?,
            started: started.parse().context("the start time")?,
            uid: uid.parse().context("the uid")?,
            lock_path: PathBuf::from(OsString::from_vec(unescape(path)?)),
        })
    }
}

fn escape(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn unescape(text: &str) -> Result<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text
                .get(i + 1..i + 3)
                .filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
                .ok_or_else(|| anyhow!("a '%' escape is cut off or not hexadecimal"))?;
            out.push(u8::from_str_radix(hex, 16)?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Ok(out)
}

/// The seal lock of `template`: `<home>/.cache/virtmanager-mcp/seal-<template>.lock`. `None` when `home` is not an
/// absolute path, or the name is empty, starts with a dot or holds a byte outside letters, digits and `-._`, so the
/// name cannot step out of the directory.
pub fn lock_path(home: Option<OsString>, template: &str) -> Option<PathBuf> {
    let safe = !template.is_empty()
        && !template.starts_with('.')
        && template
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'));
    let home = home.map(PathBuf::from).filter(|p| p.is_absolute())?;
    safe.then(|| {
        home.join(".cache")
            .join("virtmanager-mcp")
            .join(format!("seal-{template}.lock"))
    })
}

/// The probe of this host: this process's uid, `HOME`, and `flock`.
pub struct HostProbe;

impl LockProbe for HostProbe {
    fn uid(&self) -> u32 {
        rustix::process::getuid().as_raw()
    }

    fn lock_path(&self, template: &str) -> Option<PathBuf> {
        lock_path(std::env::var_os("HOME"), template)
    }

    /// Takes the lock shared and at once, without waiting, and lets go again: a holder keeps it exclusive, so the
    /// attempt fails exactly while a seal holds it. A missing file means no seal ever held it, or none now.
    fn held(&self, path: &Path) -> Result<bool> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => {
                return Err(e).with_context(|| format!("opening the seal lock {}", path.display()));
            }
        };
        match file.try_lock_shared() {
            Ok(()) => {
                let _ = file.unlock();
                Ok(false)
            }
            Err(TryLockError::WouldBlock) => Ok(true),
            Err(TryLockError::Error(e)) => {
                Err(e).with_context(|| format!("probing the seal lock {}", path.display()))
            }
        }
    }
}

/// The refusal for a VM under a live seal, which a caller can tell from any other failure by downcasting.
#[derive(Debug)]
pub struct Sealed {
    pub vm: String,
    pub template: String,
    pub seal_id: String,
}

impl fmt::Display for Sealed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "VM '{}' is being sealed into template '{}' (seal {}); try again once the seal has finished",
            self.vm, self.template, self.seal_id
        )
    }
}

impl std::error::Error for Sealed {}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark() -> Mark {
        Mark {
            template: "t-base".into(),
            seal_id: "0b9f6c2e".into(),
            started: 1_000_000,
            uid: 1000,
            lock_path: PathBuf::from("/home/me/.cache/virtmanager-mcp/seal-t-base.lock"),
        }
    }

    /// A probe with fixed answers and no files.
    struct Fixed {
        uid: u32,
        home: Option<&'static str>,
        held: Result<bool, &'static str>,
    }

    impl Fixed {
        fn mine(held: bool) -> Self {
            Self {
                uid: 1000,
                home: Some("/home/me"),
                held: Ok(held),
            }
        }
    }

    impl LockProbe for Fixed {
        fn uid(&self) -> u32 {
            self.uid
        }
        fn lock_path(&self, template: &str) -> Option<PathBuf> {
            lock_path(self.home.map(OsString::from), template)
        }
        fn held(&self, _path: &Path) -> Result<bool> {
            self.held.map_err(|e| anyhow!(e))
        }
    }

    #[test]
    fn a_mark_of_this_process_is_live_while_its_lock_is_held_and_stale_when_it_is_free() {
        let m = mark();
        // The age does not matter for such a mark: the lock says it all.
        let day = FOREIGN_MARK_SECS;
        assert!(m.live(m.started + 5 * day, &Fixed::mine(true)).unwrap());
        assert!(!m.live(m.started, &Fixed::mine(false)).unwrap());
    }

    #[test]
    fn a_mark_the_process_cannot_see_the_lock_of_is_live_for_24_hours() {
        let m = mark();
        let hour = 3600;
        let other_uid = Fixed {
            uid: 1001,
            ..Fixed::mine(false)
        };
        let other_home = Fixed {
            home: Some("/home/you"),
            ..Fixed::mine(false)
        };
        let no_home = Fixed {
            home: None,
            ..Fixed::mine(false)
        };
        for probe in [&other_uid, &other_home, &no_home] {
            // A free lock is no evidence here: the lock this probe would look at is not the seal's.
            assert!(m.live(m.started + 23 * hour, probe).unwrap());
            assert!(m.live(m.started + 24 * hour - 1, probe).unwrap());
            assert!(!m.live(m.started + 24 * hour, probe).unwrap());
            assert!(!m.live(m.started + 25 * hour, probe).unwrap());
        }
    }

    #[test]
    fn a_mark_from_the_future_is_live() {
        let m = mark();
        let probe = Fixed {
            uid: 1001,
            ..Fixed::mine(false)
        };
        assert!(m.live(m.started - 10, &probe).unwrap());
        let late = Mark {
            started: u64::MAX,
            ..m
        };
        assert!(late.live(0, &probe).unwrap());
    }

    #[test]
    fn a_probe_that_cannot_tell_fails_instead_of_calling_the_mark_stale() {
        let probe = Fixed {
            held: Err("permission denied"),
            ..Fixed::mine(false)
        };
        let e = mark().live(mark().started, &probe).unwrap_err();
        assert!(e.to_string().contains("permission denied"), "{e}");
    }

    #[test]
    fn the_encoding_round_trips_whatever_the_fields_hold() {
        let nasty = [
            "",
            "plain",
            "a:b",
            "100%",
            "%41",
            "it's",
            "say \"hi\"",
            "<vmm:owner a='b'/>",
            "a&b;c",
            "line\nbreak\ttab",
            "ünïcödé 日本",
            ":::",
        ];
        for text in nasty {
            let m = Mark {
                template: text.into(),
                seal_id: text.into(),
                lock_path: PathBuf::from(format!("/x/{text}")),
                ..mark()
            };
            let value = m.encode();
            assert_eq!(value.split(':').count(), 6, "{value}");
            assert!(
                value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b":%-._~".contains(&b)),
                "{value}"
            );
            assert_eq!(Mark::decode(&value).unwrap(), m, "{value}");
        }
    }

    #[test]
    fn a_path_that_is_not_utf8_survives_the_encoding() {
        let raw = OsString::from_vec(b"/home/\xff\xfe/seal.lock".to_vec());
        let m = Mark {
            lock_path: PathBuf::from(raw),
            ..mark()
        };
        assert_eq!(Mark::decode(&m.encode()).unwrap(), m);
    }

    #[test]
    fn a_value_that_encode_did_not_write_is_refused() {
        let good = mark().encode();
        let bad = [
            String::new(),
            "v1".into(),
            good.replacen("v1", "v2", 1),
            format!("{good}:extra"),
            good.replace("1000000", "soon"),
            good.replace(":1000:", ":-1:"),
            "v1:a:b:1:2:%4".into(),
            "v1:a:b:1:2:%zz".into(),
            "v1:%FF:b:1:2:/p".into(),
        ];
        for value in bad {
            assert!(Mark::decode(&value).is_err(), "{value:?} was accepted");
        }
    }

    #[test]
    fn the_seal_lock_sits_beside_the_management_lock_and_takes_only_safe_names() {
        assert_eq!(
            lock_path(Some("/home/me".into()), "t-base").unwrap(),
            Path::new("/home/me/.cache/virtmanager-mcp/seal-t-base.lock")
        );
        for name in ["", ".hidden", "a/b", "..", "a b", "a\0b", "../x"] {
            assert_eq!(lock_path(Some("/home/me".into()), name), None, "{name:?}");
        }
        for home in [None, Some(OsString::new()), Some("home/me".into())] {
            assert_eq!(lock_path(home, "t-base"), None);
        }
    }

    #[test]
    fn the_host_probe_sees_a_lock_held_through_another_open_of_the_file() {
        let dir = std::env::temp_dir().join(format!("virtmanager-mcp-seal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("seal-t-x.lock");
        // No file: no seal holds it.
        assert!(!HostProbe.held(&path).unwrap());
        let holder = File::create(&path).unwrap();
        assert!(!HostProbe.held(&path).unwrap());
        holder.lock().unwrap();
        assert!(HostProbe.held(&path).unwrap());
        // The probe lets go again, so it does not keep a seal from taking the lock.
        holder.unlock().unwrap();
        assert!(!HostProbe.held(&path).unwrap());
        holder.try_lock().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_refusal_names_the_vm_the_template_and_the_seal() {
        let text = Sealed {
            vm: "t-a".into(),
            template: "t-base".into(),
            seal_id: "0b9f".into(),
        }
        .to_string();
        assert!(
            text.contains("'t-a'") && text.contains("'t-base'") && text.contains("0b9f"),
            "{text}"
        );
    }
}
