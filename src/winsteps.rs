//! The PowerShell scripts that run in Windows guests. Each lives as a `.ps1` file beside the module that runs it, in
//! `src/jobs/` or `src/winsteps/`, so a parse test can find every one of them.

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::ErrorKind;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// Prints each parse error of the file named by `VMM_PS_FILE` and exits with their count.
    const PARSE: &str = r#"$tokens = $null
$errors = $null
[void][System.Management.Automation.Language.Parser]::ParseFile($env:VMM_PS_FILE, [ref]$tokens, [ref]$errors)
foreach ($e in $errors) { '{0}:{1}: {2}' -f $e.Extent.StartLineNumber, $e.Extent.StartColumnNumber, $e.Message }
exit $errors.Count"#;

    /// The parse errors `pwsh` reports for `path`, or `None` when `pwsh` is not installed.
    fn parse_errors(path: &Path) -> Option<String> {
        let out = match Command::new("pwsh")
            .args(["-NoProfile", "-NonInteractive", "-Command", PARSE])
            .env("VMM_PS_FILE", path)
            .output()
        {
            Ok(out) => out,
            Err(e) if e.kind() == ErrorKind::NotFound => return None,
            Err(e) => panic!("running pwsh: {e}"),
        };
        if out.status.success() {
            return Some(String::new());
        }
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Some(if text.is_empty() {
            format!(
                "pwsh failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )
        } else {
            text
        })
    }

    /// Every `.ps1` file under `src/jobs/` and `src/winsteps/`.
    fn scripts() -> Vec<PathBuf> {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut found = Vec::new();
        for dir in ["jobs", "winsteps"] {
            let Ok(entries) = fs::read_dir(src.join(dir)) else {
                continue;
            };
            for entry in entries {
                let path = entry.unwrap().path();
                if path.extension().is_some_and(|e| e == "ps1") {
                    found.push(path);
                }
            }
        }
        found.sort();
        found
    }

    /// Checks the syntax of every Windows script with `pwsh`'s parser. PowerShell 7 on Linux is not the guests'
    /// Windows PowerShell 5.1, so this proves syntax only; the test bed runs the scripts on a Windows guest. Without
    /// `pwsh` the test is skipped, unless `VIRTMANAGER_REQUIRE_PWSH=1`, which CI sets.
    #[test]
    fn every_windows_script_parses() {
        let broken =
            std::env::temp_dir().join(format!("virtmanager-mcp-broken-{}.ps1", std::process::id()));
        fs::write(&broken, "if ($x) {\n  'unclosed'\n").unwrap();
        let Some(found) = parse_errors(&broken) else {
            fs::remove_file(&broken).unwrap();
            assert!(
                std::env::var("VIRTMANAGER_REQUIRE_PWSH").as_deref() != Ok("1"),
                "VIRTMANAGER_REQUIRE_PWSH=1, but pwsh is not installed"
            );
            eprintln!("skipped: pwsh is not installed, so the Windows scripts were not parsed");
            return;
        };
        fs::remove_file(&broken).unwrap();
        assert!(!found.is_empty(), "the parser accepted an unclosed brace");
        let scripts = scripts();
        assert!(
            scripts.len() >= 4,
            "only {} scripts found: {scripts:?}",
            scripts.len()
        );
        for path in scripts {
            let errors = parse_errors(&path).expect("pwsh ran a moment ago");
            assert!(errors.is_empty(), "{}:\n{errors}", path.display());
        }
    }
}
