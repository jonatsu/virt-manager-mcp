//! The PowerShell scripts that run in Windows guests. Each lives as a `.ps1` file beside the module that runs it, in
//! `src/jobs/` or `src/winsteps/`, so a parse test can find every one of them.

use crate::agent::{ExecSpec, powershell, ps_quote};

/// Checks a directory pull's source and prints the temporary directory and the path of `tar.exe`.
pub const PULL_PREPARE: &str = include_str!("winsteps/pull-prepare.ps1");
/// Prints the length of a file that `tar.exe` may still be writing.
pub const PULL_SIZE: &str = include_str!("winsteps/pull-size.ps1");
/// Deletes a pull's archive.
pub const PULL_REMOVE: &str = include_str!("winsteps/pull-remove.ps1");
/// Packs a pull whose path is not ASCII, which `guest-exec` cannot pass to `tar.exe`.
pub const PULL_PACK: &str = include_str!("winsteps/pull-pack.ps1");
/// Checks a directory push's target, and prints the staging root and the path of `tar.exe`.
pub const PUSH_PROBE: &str = include_str!("winsteps/push-probe.ps1");
/// Creates a directory, for a push's archive or for the push to unpack into, that only `SYSTEM` and
/// `Administrators` can write.
pub const PUSH_MKDIR: &str = include_str!("winsteps/push-mkdir.ps1");
/// Compiles and runs the push helper, which places the unpacked tree in its target.
pub const PUSH_PLACE: &str = include_str!("winsteps/push-place.ps1");
/// The C# source of the push helper, which a push writes into its protected archive directory for `PUSH_PLACE`.
pub const PUSH_HELPER: &str = include_str!("winsteps/push-helper.cs");
/// Deletes a push's temporary entries.
pub const PUSH_CLEANUP: &str = include_str!("winsteps/push-cleanup.ps1");

/// Runs the constant script `body` with `args` defined before it as `$argv`, an array of quoted literals, so no
/// argument is ever parsed as PowerShell code.
pub fn script(body: &str, args: &[&str]) -> ExecSpec {
    let quoted: Vec<String> = args.iter().map(|a| ps_quote(a)).collect();
    powershell(&format!("$argv = @({})\n{body}", quoted.join(", ")))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::ErrorKind;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::*;

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

    /// The script text a PowerShell `ExecSpec` carries in its `-EncodedCommand` argument.
    fn decoded(spec: &ExecSpec) -> String {
        use base64::Engine;
        let encoded = spec.args.last().unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        let units: Vec<u16> = bytes
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16(&units).unwrap()
    }

    #[test]
    fn arguments_reach_a_script_as_quoted_literals() {
        let spec = script(PULL_SIZE, &[r"C:\it's $(evil)\a.tar", "two"]);
        assert_eq!(spec.path, "powershell.exe");
        let text = decoded(&spec);
        assert!(
            text.starts_with("$argv = @('C:\\it''s $(evil)\\a.tar', 'two')\n"),
            "{text}"
        );
        assert!(text.ends_with(PULL_SIZE));
    }

    /// Compiles the push helper with `pwsh`'s `Add-Type` and reads the parameter types of the method that
    /// `push-place.ps1` calls. pre-commit also builds the helper as C# 5, which Windows PowerShell 5.1 needs. Without
    /// `pwsh` the test is skipped, unless `VIRTMANAGER_REQUIRE_PWSH=1`.
    #[test]
    fn the_push_helper_compiles_with_the_method_the_place_script_calls() {
        const CHECK: &str = r#"Add-Type -Path $env:VMM_CS_FILE
$m = [VmmPush].GetMethod('Place')
if ($null -eq $m) { 'no Place method'; exit 1 }
($m.GetParameters() | ForEach-Object { $_.ParameterType.Name }) -join ','
[VmmPush].GetFields() | Where-Object IsLiteral | Sort-Object { $_.GetValue($null) } |
  ForEach-Object { '{0}={1}' -f $_.Name, $_.GetValue($null) }"#;
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/winsteps/push-helper.cs");
        let out = match Command::new("pwsh")
            .args(["-NoProfile", "-NonInteractive", "-Command", CHECK])
            .env("VMM_CS_FILE", &path)
            .output()
        {
            Ok(out) => out,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                assert!(
                    std::env::var("VIRTMANAGER_REQUIRE_PWSH").as_deref() != Ok("1"),
                    "VIRTMANAGER_REQUIRE_PWSH=1, but pwsh is not installed"
                );
                eprintln!("skipped: pwsh is not installed, so the push helper was not compiled");
                return;
            }
            Err(e) => panic!("running pwsh: {e}"),
        };
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mut lines = stdout.lines();
        assert_eq!(
            lines.next(),
            Some("String,String,String,String,Boolean,TextWriter")
        );
        // Every exit status the helper returns is the one push.rs maps; 23 comes from the script and 14 to 16 share
        // push.rs's fallback message.
        use crate::push::place_status as s;
        let expected = [
            ("Done", 0),
            ("Exists", s::EXISTS),
            ("NotADirectory", s::NOT_A_DIRECTORY),
            ("NoParents", s::NO_PARENTS),
            ("OldStuck", 14),
            ("NewStuckOldBack", 15),
            ("NewStuck", 16),
            ("BothStuck", s::BOTH_STUCK),
            ("Busy", s::BUSY),
            ("Redirected", s::REDIRECTED),
            ("NotInherited", s::NOT_INHERITED),
            ("ReservedName", s::RESERVED_NAME),
            ("NotCopied", s::NOT_COPIED),
            ("InheritStuck", s::INHERIT_STUCK),
            ("OldLeft", s::OLD_LEFT),
        ]
        .map(|(name, code)| format!("{name}={code}"));
        assert_eq!(lines.collect::<Vec<_>>(), expected);
        assert!(
            !expected
                .iter()
                .any(|e| e.ends_with(&format!("={}", s::NOT_COMPILED)))
        );
        assert!(PUSH_PLACE.contains(
            "[VmmPush]::Place($target, $staging, $copy, $old, $replace -eq '1', $stderr)"
        ));
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
            scripts.len() >= 10,
            "only {} scripts found: {scripts:?}",
            scripts.len()
        );
        for path in scripts {
            let errors = parse_errors(&path).expect("pwsh ran a moment ago");
            assert!(errors.is_empty(), "{}:\n{errors}", path.display());
        }
    }
}
