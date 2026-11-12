//! Downloads the `ocrs` models when the server starts with `VIRTMANAGER_OCR_DOWNLOAD=1` and `display.ocr_models` is
//! unset. The models go to a fixed per-user directory, and the download runs on its own thread after startup, so a
//! slow or failed download never delays or stops the server. `screen_text` reads the state through [`Download`].
//!
//! Each file must match the SHA-256 sum that `scripts/fetch-ocr-models.sh` pins, because upstream publishes none. A
//! file arrives under a temporary name in the directory and is renamed into place only once its sum matches, so
//! several server processes starting at once never leave a partial file under a model's name.

use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

use crate::config::{OCR_DETECTION_MODEL, OCR_RECOGNITION_MODEL};

/// The environment variable that turns the download on. Only the value `1` does.
pub const ENV: &str = "VIRTMANAGER_OCR_DOWNLOAD";

const BASE_URL: &str = "https://ocrs-models.s3-accelerate.amazonaws.com";

/// A model file and the SHA-256 sum it must have.
#[derive(Clone, Copy)]
pub struct Model {
    pub name: &'static str,
    pub sha256: &'static str,
}

/// The models `screen_text` needs, with the sums `scripts/fetch-ocr-models.sh` pins.
pub const MODELS: [Model; 2] = [
    Model {
        name: OCR_DETECTION_MODEL,
        sha256: "a917b23dbd9524b465df7e922641b3ff2981623df4ded5a0234004ef2fee7cfe",
    },
    Model {
        name: OCR_RECOGNITION_MODEL,
        sha256: "86c145c2edb96c8caed5b1ebb8f44d706408922211451c309c625157dd6061c5",
    },
];

/// Whether `value`, the environment variable's value, turns the download on.
fn enabled(value: Option<&OsString>) -> bool {
    value.is_some_and(|v| v == "1")
}

/// The directory to download the models to, from the environment variable's `value`, whether `display.ocr_models`
/// is `configured`, and `XDG_DATA_HOME` and `HOME`. `None` means no download: the variable is not `1`, or the config
/// names a directory, which wins.
pub fn download_dir(
    value: Option<OsString>,
    configured: bool,
    xdg_data_home: Option<OsString>,
    home: Option<OsString>,
) -> Result<Option<PathBuf>> {
    if !enabled(value.as_ref()) {
        return Ok(None);
    }
    if configured {
        tracing::info!("display.ocr_models is set, so {ENV} is ignored");
        return Ok(None);
    }
    default_dir(xdg_data_home, home).map(Some)
}

/// The directory the models are downloaded to: `$XDG_DATA_HOME/virtmanager-mcp/ocr-models`, or
/// `~/.local/share/virtmanager-mcp/ocr-models` when `XDG_DATA_HOME` is unset, empty or relative, as the XDG base
/// directory specification says.
fn default_dir(xdg_data_home: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    let data = match xdg_data_home.map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir,
        _ => match home.map(PathBuf::from) {
            Some(home) if home.is_absolute() => home.join(".local/share"),
            _ => bail!("{ENV}: neither XDG_DATA_HOME nor HOME names an absolute directory"),
        },
    };
    Ok(data.join("virtmanager-mcp/ocr-models"))
}

/// Where a download stands.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Running,
    Ready,
    Failed(String),
}

/// The state of the model download that `screen_text` waits on.
pub struct Download {
    dir: PathBuf,
    models: Vec<Model>,
    status: Mutex<Status>,
}

impl Download {
    /// A download of [`MODELS`] into `dir` that has not finished yet.
    pub fn new(dir: PathBuf) -> Arc<Self> {
        Self::of(dir, MODELS.to_vec())
    }

    fn of(dir: PathBuf, models: Vec<Model>) -> Arc<Self> {
        Arc::new(Self {
            dir,
            models,
            status: Mutex::new(Status::Running),
        })
    }

    /// Starts the download on its own thread, which the process does not wait for at exit. curl's own time limits
    /// bound it. A thread that cannot start is recorded as a failed download.
    pub fn spawn(self: &Arc<Self>) {
        let this = self.clone();
        let started = std::thread::Builder::new()
            .name("ocr-download".into())
            .spawn(move || this.run(&curl));
        if let Err(e) = started {
            self.record(Err(anyhow!(e).context("starting the download thread")));
        }
    }

    /// Makes sure the models are in the directory, fetching each with `fetch`, and records the outcome.
    fn run(&self, fetch: &dyn Fn(&str, &Path) -> Result<()>) {
        self.record(ensure(&self.dir, &self.models, fetch));
    }

    fn record(&self, outcome: Result<()>) {
        let status = match outcome {
            Ok(()) => {
                tracing::info!("the OCR models are ready in {}", self.dir.display());
                Status::Ready
            }
            Err(e) => {
                let error = format!("{e:#}");
                tracing::warn!("downloading the OCR models failed: {error}");
                Status::Failed(error)
            }
        };
        *self.status.lock().unwrap_or_else(PoisonError::into_inner) = status;
    }

    /// Succeeds once the models are in place, and otherwise says why `screen_text` cannot run yet. After a failed
    /// download it looks at the directory again, so models installed there with the fetch script count at once.
    pub fn ready(&self) -> Result<()> {
        let mut status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        match &*status {
            Status::Ready => Ok(()),
            Status::Running => Err(anyhow!(
                "the OCR models are still downloading into {}; try screen_text again shortly",
                self.dir.display()
            )),
            Status::Failed(_) if all_present(&self.dir, &self.models) => {
                tracing::info!("the OCR models are now in {}", self.dir.display());
                *status = Status::Ready;
                Ok(())
            }
            Status::Failed(error) => Err(anyhow!(
                "downloading the OCR models into {dir} failed: {error}. Restart the server to try again, or run \
                 scripts/fetch-ocr-models.sh {dir}",
                dir = self.dir.display()
            )),
        }
    }
}

/// Whether every one of `models` is in `dir` with its sum.
fn all_present(dir: &Path, models: &[Model]) -> bool {
    models
        .iter()
        .all(|m| sha256_of(&dir.join(m.name)).is_ok_and(|sum| sum == m.sha256))
}

/// How old a temporary download file must be before a starting server removes it: past curl's `--max-time` and
/// `--connect-timeout`, so no running download still writes it.
const STALE_TEMP: Duration = Duration::from_secs(15 * 60);

/// Removes the temporary files of `models` in `dir` that are older than [`STALE_TEMP`]. A server that exited
/// mid-download leaves one behind, and curl may still finish writing it after the server is gone.
fn remove_stale_temps(dir: &Path, models: &[Model]) -> Result<()> {
    let entries = fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("listing {}", dir.display()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !models
            .iter()
            .any(|m| name.starts_with(&format!(".{}.", m.name)))
        {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let stale = meta
            .modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > STALE_TEMP);
        if meta.is_file() && stale {
            match fs::remove_file(entry.path()) {
                Ok(()) => tracing::info!("removed the stale download {}", entry.path().display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(anyhow!(e).context(format!("removing {}", entry.path().display())));
                }
            }
        }
    }
    Ok(())
}

/// Makes sure each of `models` is in `dir` with its sum. It keeps a file whose sum matches, and fetches a missing or
/// wrong one to a temporary name that it renames into place, over the wrong one, once the sum matches. The wrong file
/// stays until then, because another server may be renaming a good copy over it at the same time.
fn ensure(dir: &Path, models: &[Model], fetch: &dyn Fn(&str, &Path) -> Result<()>) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    remove_stale_temps(dir, models)?;
    for model in models {
        let dest = dir.join(model.name);
        match sha256_of(&dest) {
            Ok(sum) if sum == model.sha256 => continue,
            Ok(sum) => {
                tracing::warn!(
                    "{} has sum {sum}, not {}; downloading it again",
                    dest.display(),
                    model.sha256
                );
            }
            Err(e) if is_not_found(&e) => {}
            Err(e) => return Err(e.context(format!("reading {}", dest.display()))),
        }
        let tmp = dir.join(format!(".{}.{}", model.name, uuid::Uuid::new_v4().simple()));
        let placed = place(model, &tmp, &dest, fetch);
        if placed.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        placed?;
        tracing::info!("downloaded {}", dest.display());
    }
    Ok(())
}

/// Fetches `model` to `tmp`, checks its sum, and renames it to `dest` with mode 644.
fn place(
    model: &Model,
    tmp: &Path,
    dest: &Path,
    fetch: &dyn Fn(&str, &Path) -> Result<()>,
) -> Result<()> {
    let url = format!("{BASE_URL}/{}", model.name);
    fetch(&url, tmp).with_context(|| format!("downloading {url}"))?;
    let sum = sha256_of(tmp).with_context(|| format!("reading {}", tmp.display()))?;
    if sum != model.sha256 {
        bail!(
            "{url} has sum {sum}, expected {}; the upstream file changed, so report it on the project's issue tracker",
            model.sha256
        );
    }
    fs::set_permissions(tmp, fs::Permissions::from_mode(0o644))
        .with_context(|| format!("setting the mode of {}", tmp.display()))?;
    fs::rename(tmp, dest).with_context(|| format!("moving {} into place", tmp.display()))
}

fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

/// The SHA-256 sum of the file at `path`, in lower-case hex.
fn sha256_of(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Downloads `url` to `dest` with curl, over HTTPS only, within 10 minutes.
fn curl(url: &str, dest: &Path) -> Result<()> {
    let out = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--proto",
            "=https",
            "--connect-timeout",
            "30",
            "--max-time",
            "600",
            "--output",
        ])
        .arg(dest)
        .arg("--")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => anyhow!("curl is not installed"),
            _ => anyhow!(e).context("running curl"),
        })?;
    if !out.status.success() {
        bail!(
            "curl failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "virtmanager-mcp-ocrfetch-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// Models named `a.onnx`, `b.onnx` and so on, whose sums are those of `contents`.
    fn models(contents: &[&str]) -> Vec<Model> {
        let names = ["a.onnx", "b.onnx"];
        contents
            .iter()
            .zip(names)
            .map(|(text, name)| {
                let sum: String = Sha256::digest(text.as_bytes())
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                Model {
                    name,
                    sha256: String::leak(sum),
                }
            })
            .collect()
    }

    /// A fetch that writes the text of the model the URL names, from `texts` by file name.
    fn serving<'a>(
        texts: &'a [(&'a str, &'a str)],
        calls: &'a Cell<usize>,
    ) -> impl Fn(&str, &Path) -> Result<()> {
        move |url, dest| {
            calls.set(calls.get() + 1);
            let name = url.rsplit('/').next().unwrap();
            let text = texts
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, t)| *t)
                .ok_or_else(|| anyhow!("404 for {name}"))?;
            fs::write(dest, text)?;
            Ok(())
        }
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn only_the_value_1_turns_the_download_on_and_a_configured_directory_wins() {
        let dir = |value: Option<&str>, configured| {
            download_dir(
                value.map(OsString::from),
                configured,
                None,
                Some("/home/me".into()),
            )
            .unwrap()
        };
        assert_eq!(
            dir(Some("1"), false).unwrap(),
            Path::new("/home/me/.local/share/virtmanager-mcp/ocr-models")
        );
        assert_eq!(dir(Some("1"), true), None);
        for off in [
            None,
            Some(""),
            Some("0"),
            Some("true"),
            Some("yes"),
            Some(" 1"),
        ] {
            assert_eq!(dir(off, false), None, "{off:?}");
        }
        // Without a download, a missing HOME is no error.
        assert_eq!(download_dir(None, false, None, None).unwrap(), None);
    }

    #[test]
    fn stale_temporary_downloads_are_removed_and_fresh_ones_kept() {
        let dir = test_dir("stale");
        fs::create_dir_all(&dir).unwrap();
        let models = models(&["detect"]);
        let old = std::time::SystemTime::now() - STALE_TEMP - Duration::from_secs(60);
        for name in [".a.onnx.old", ".other.old"] {
            fs::File::create(dir.join(name))
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        fs::write(dir.join(".a.onnx.fresh"), "partial").unwrap();
        let calls = Cell::new(0);
        ensure(&dir, &models, &serving(&[("a.onnx", "detect")], &calls)).unwrap();
        assert_eq!(names_in(&dir), [".a.onnx.fresh", ".other.old", "a.onnx"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_directory_follows_xdg_data_home_then_home() {
        let dir = |xdg: Option<&str>, home: Option<&str>| {
            default_dir(xdg.map(OsString::from), home.map(OsString::from))
        };
        assert_eq!(
            dir(Some("/data"), Some("/home/me")).unwrap(),
            Path::new("/data/virtmanager-mcp/ocr-models")
        );
        for xdg in [None, Some(""), Some("relative")] {
            assert_eq!(
                dir(xdg, Some("/home/me")).unwrap(),
                Path::new("/home/me/.local/share/virtmanager-mcp/ocr-models"),
                "{xdg:?}"
            );
        }
        for home in [None, Some(""), Some("home")] {
            assert!(dir(None, home).unwrap_err().to_string().contains(ENV));
        }
    }

    #[test]
    fn missing_models_are_downloaded_with_mode_644_and_no_leftovers() {
        let dir = test_dir("missing");
        let models = models(&["detect", "recognise"]);
        let calls = Cell::new(0);
        ensure(
            &dir,
            &models,
            &serving(&[("a.onnx", "detect"), ("b.onnx", "recognise")], &calls),
        )
        .unwrap();
        assert_eq!(calls.get(), 2);
        assert_eq!(names_in(&dir), ["a.onnx", "b.onnx"]);
        assert_eq!(fs::read_to_string(dir.join("b.onnx")).unwrap(), "recognise");
        let mode = fs::metadata(dir.join("a.onnx"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o644);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_model_whose_sum_matches_is_kept_and_a_wrong_one_replaced() {
        let dir = test_dir("present");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.onnx"), "detect").unwrap();
        fs::write(dir.join("b.onnx"), "stale").unwrap();
        let models = models(&["detect", "recognise"]);
        let calls = Cell::new(0);
        ensure(&dir, &models, &serving(&[("b.onnx", "recognise")], &calls)).unwrap();
        assert_eq!(calls.get(), 1, "only the wrong file is fetched");
        assert_eq!(fs::read_to_string(dir.join("b.onnx")).unwrap(), "recognise");
        assert_eq!(names_in(&dir), ["a.onnx", "b.onnx"]);
        let calls = Cell::new(0);
        ensure(&dir, &models, &serving(&[], &calls)).unwrap();
        assert_eq!(calls.get(), 0, "a complete directory fetches nothing");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_download_with_the_wrong_sum_or_a_failed_fetch_leaves_no_file() {
        let dir = test_dir("bad");
        let models = models(&["detect"]);
        let calls = Cell::new(0);
        let err = ensure(&dir, &models, &serving(&[("a.onnx", "tampered")], &calls)).unwrap_err();
        assert!(format!("{err:#}").contains("expected"), "{err:#}");
        assert!(names_in(&dir).is_empty(), "{:?}", names_in(&dir));
        let err = ensure(&dir, &models, &serving(&[], &calls)).unwrap_err();
        assert!(format!("{err:#}").contains("404 for a.onnx"), "{err:#}");
        assert!(names_in(&dir).is_empty(), "{:?}", names_in(&dir));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn screen_text_learns_whether_the_download_is_running_ready_or_failed() {
        let dir = test_dir("status");
        let download = Download::of(dir.clone(), models(&["detect"]));
        assert!(
            download
                .ready()
                .unwrap_err()
                .to_string()
                .contains("still downloading")
        );
        let calls = Cell::new(0);
        download.run(&serving(&[], &calls));
        let err = download.ready().unwrap_err().to_string();
        let script = format!("scripts/fetch-ocr-models.sh {}", dir.display());
        assert!(
            err.contains("404 for a.onnx")
                && err.contains("Restart the server")
                && err.contains(&script),
            "{err}"
        );
        download.run(&serving(&[("a.onnx", "detect")], &calls));
        download.ready().unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn models_installed_after_a_failed_download_count_without_a_restart() {
        let dir = test_dir("late");
        let download = Download::of(dir.clone(), models(&["detect"]));
        download.run(&serving(&[], &Cell::new(0)));
        assert!(download.ready().is_err());
        fs::write(dir.join("a.onnx"), "wrong").unwrap();
        assert!(download.ready().is_err(), "a wrong file does not count");
        fs::write(dir.join("a.onnx"), "detect").unwrap();
        download.ready().unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_pinned_sums_match_the_fetch_script() {
        let script = include_str!("../scripts/fetch-ocr-models.sh");
        let pin = |var: &str| {
            script
                .lines()
                .find_map(|l| l.strip_prefix(&format!("readonly {var}=")))
                .unwrap_or_else(|| panic!("{var} is not in the script"))
        };
        assert_eq!(MODELS[0].name, "text-detection.onnx");
        assert_eq!(MODELS[0].sha256, pin("DETECTION_SHA256"));
        assert_eq!(MODELS[1].name, "text-recognition.onnx");
        assert_eq!(MODELS[1].sha256, pin("RECOGNITION_SHA256"));
        assert_eq!(BASE_URL, pin("BASE_URL"));
    }
}
