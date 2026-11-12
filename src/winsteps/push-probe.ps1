# Checks a directory push's target in a Windows guest. Run as powershell -EncodedCommand, with $argv defined before
# this text as an array of quoted literals: the target, its parent, and 1 when the push may replace the target.
# Prints the path of tar.exe. Creates the target's missing parents when the target does not exist.
# Exit status: 0 ready, 10 no tar.exe, 11 the target exists and may not be replaced, 12 the target is not a directory
# or is a link, 13 the parents could not be created.

$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
$target, $parent, $replace = $argv
$tar = Join-Path $env:SystemRoot 'System32\tar.exe'
if (-not (Test-Path -LiteralPath $tar -PathType Leaf)) { exit 10 }
if (Test-Path -LiteralPath $target) {
  $item = Get-Item -LiteralPath $target -Force
  if (-not $item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { exit 12 }
  if ($replace -ne '1') { exit 11 }
} else {
  try {
    [void][IO.Directory]::CreateDirectory($parent)
  } catch {
    [Console]::Error.Write($_.Exception.Message)
    exit 13
  }
}
[Console]::Out.Write($tar)
exit 0
