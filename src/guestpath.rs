//! Absolute guest paths for the directory tools, on Linux and Windows guests.
//!
//! A Linux path starts with `/`. A Windows path is a drive path such as `C:\work\app`, with `\` or `/` between
//! components. Both refuse `..`. A Windows path also refuses what Windows would rewrite or resolve elsewhere: a
//! drive-relative path such as `C:work`, UNC and device paths, a `:` after the drive, and a component ending in a dot
//! or a space, which Windows drops, so `C:\Windows.` would name `C:\Windows`.

use anyhow::{Result, bail};

use crate::agent::GuestOs;

/// A parsed absolute guest path: its root (`/`, or a drive such as `C:\`) and its components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestPath {
    os: GuestOs,
    /// The drive letter, upper case, on Windows.
    drive: Option<char>,
    parts: Vec<String>,
}

impl GuestPath {
    /// Parses `path` under the rules of `os`.
    pub fn parse(os: GuestOs, path: &str) -> Result<Self> {
        match os {
            GuestOs::Linux => parse_linux(path),
            GuestOs::Windows => parse_windows(path),
        }
    }

    /// The path's components below the root.
    pub fn parts(&self) -> &[String] {
        &self.parts
    }

    /// The separator between components.
    pub fn separator(&self) -> char {
        match self.os {
            GuestOs::Linux => '/',
            GuestOs::Windows => '\\',
        }
    }

    /// The root: `/`, or the drive root such as `C:\`.
    pub fn root(&self) -> String {
        match self.drive {
            Some(d) => format!("{d}:\\"),
            None => "/".into(),
        }
    }

    /// The path made of the root and the first `n` components.
    pub fn prefix(&self, n: usize) -> String {
        let sep = self.separator().to_string();
        format!("{}{}", self.root(), self.parts[..n].join(&sep))
    }

    /// The whole path, normalized: one separator between components, none at the end.
    pub fn normalized(&self) -> String {
        self.prefix(self.parts.len())
    }
}

fn parse_linux(path: &str) -> Result<GuestPath> {
    if !path.starts_with('/') {
        bail!("{path}: guest_path must be absolute");
    }
    let parts = components(path, &['/'], path)?;
    Ok(GuestPath {
        os: GuestOs::Linux,
        drive: None,
        parts,
    })
}

fn parse_windows(path: &str) -> Result<GuestPath> {
    let is_sep = |c: char| c == '\\' || c == '/';
    let mut chars = path.chars();
    let (first, second, third) = (chars.next(), chars.next(), chars.next());
    if first.is_some_and(is_sep) && second.is_some_and(is_sep) {
        bail!(
            "{path}: UNC and device paths such as \\\\server\\share are not supported; name a drive path"
        );
    }
    let drive = match (first, second, third) {
        (Some(d), Some(':'), Some(s)) if d.is_ascii_alphabetic() && is_sep(s) => {
            d.to_ascii_uppercase()
        }
        (Some(d), Some(':'), _) if d.is_ascii_alphabetic() => {
            bail!(
                "{path}: guest_path must be absolute, such as {d}:\\work; without a separator after {d}: a path is \
                 relative to the drive's current directory"
            )
        }
        _ => bail!("{path}: guest_path must be an absolute drive path, such as C:\\work"),
    };
    let parts = components(&path[3..], &['\\', '/'], path)?;
    for part in &parts {
        if part.contains(':') {
            bail!("{path}: a ':' is allowed only after the drive letter");
        }
        if part.ends_with('.') || part.ends_with(' ') {
            bail!(
                "{path}: '{part}' ends in a dot or a space, which Windows drops; name the path without it"
            );
        }
    }
    Ok(GuestPath {
        os: GuestOs::Windows,
        drive: Some(drive),
        parts,
    })
}

/// Splits `rest` at `seps`, dropping empty and `.` components and refusing `..`.
fn components(rest: &str, seps: &[char], path: &str) -> Result<Vec<String>> {
    let parts: Vec<String> = rest
        .split(seps)
        .filter(|p| !p.is_empty() && *p != ".")
        .map(String::from)
        .collect();
    if parts.iter().any(|p| p == "..") {
        bail!("{path}: guest_path may not contain '..'");
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(path: &str) -> Result<String> {
        GuestPath::parse(GuestOs::Windows, path).map(|p| p.normalized())
    }

    fn refused(os: GuestOs, path: &str) -> String {
        match GuestPath::parse(os, path) {
            Ok(p) => panic!("{path} accepted as {}", p.normalized()),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn windows_drive_paths_are_normalized_to_backslashes() {
        assert_eq!(win(r"C:\work\app").unwrap(), r"C:\work\app");
        assert_eq!(win("c:/work/app/").unwrap(), r"C:\work\app");
        assert_eq!(win(r"d:\work//app\.\src").unwrap(), r"D:\work\app\src");
        assert_eq!(win(r"C:\").unwrap(), r"C:\");
        let p = GuestPath::parse(GuestOs::Windows, r"C:\work").unwrap();
        assert_eq!((p.prefix(0), p.root()), (r"C:\".into(), r"C:\".into()));
    }

    #[test]
    fn windows_paths_that_windows_would_rewrite_are_refused() {
        for (path, why) in [
            ("work", "absolute drive path"),
            ("C:", "relative to the drive"),
            ("C:work", "relative to the drive"),
            (r"\work", "absolute drive path"),
            ("1:\\x", "absolute drive path"),
            ("\u{e9}:\\x", "absolute drive path"),
            (r"\\server\share\x", "UNC"),
            (r"\\?\C:\x", "UNC"),
            (r"\\.\pipe\x", "UNC"),
            ("//server/share", "UNC"),
            (r"C:\a\..\b", ".."),
            (r"C:\a\b:stream", "':'"),
            (r"C:\Windows.", "dot or a space"),
            ("C:\\Windows \\x", "dot or a space"),
            (r"C:\a\...", "dot or a space"),
        ] {
            let err = refused(GuestOs::Windows, path);
            assert!(err.contains(why), "{path}: {err}");
        }
    }

    #[test]
    fn linux_paths_keep_their_rules() {
        let p = GuestPath::parse(GuestOs::Linux, "/srv//app/./src/").unwrap();
        assert_eq!(p.normalized(), "/srv/app/src");
        assert_eq!(p.prefix(2), "/srv/app");
        assert!(refused(GuestOs::Linux, "relative").contains("absolute"));
        assert!(refused(GuestOs::Linux, "/a/../b").contains(".."));
        // A backslash is an ordinary character in a Linux name.
        let p = GuestPath::parse(GuestOs::Linux, r"/a\b").unwrap();
        assert_eq!(p.parts(), [r"a\b".to_string()]);
    }
}
