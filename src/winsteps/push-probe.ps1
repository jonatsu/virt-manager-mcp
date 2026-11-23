# Checks a directory push's target in a Windows guest. Run as powershell -EncodedCommand, with $argv defined before
# this text as an array of quoted literals: the target, its parent, and 1 when the push may replace the target.
# Prints the staging root, %SystemRoot%\Temp, on one line and the path of tar.exe on the next. The Place step creates
# the target's missing parents, on handles, so this script only reads.
# Exit status: 0 ready, 10 no tar.exe, 11 the target exists and may not be replaced, 12 the target is not a directory
# or is a link, 15 PowerShell runs in a restricted language mode, which cannot load the push helper.

$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
$target, $null, $replace = $argv
$tar = Join-Path $env:SystemRoot 'System32\tar.exe'
if (-not (Test-Path -LiteralPath $tar -PathType Leaf)) { exit 10 }
if ($ExecutionContext.SessionState.LanguageMode -ne 'FullLanguage') { exit 15 }
if (Test-Path -LiteralPath $target) {
  $item = Get-Item -LiteralPath $target -Force
  if (-not $item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { exit 12 }
  if ($replace -ne '1') { exit 11 }
}
[Console]::Out.Write((Join-Path $env:SystemRoot 'Temp') + "`n" + $tar)
exit 0
