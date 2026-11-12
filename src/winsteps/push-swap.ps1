# Moves the unpacked tree into a push's target in a Windows guest. $argv holds the target, the unpacked tree, the
# name for the old tree, and 1 when the push may replace the target. An existing target moves aside first; when the
# new tree cannot move in, the old one moves back. [IO.Directory]::Move refuses an existing destination, so a target
# that appears meanwhile is never moved into.
# Exit status: 0 swapped; 11 the target exists and may not be replaced, or appeared; 12 the target is not a directory
# or is a link; 14 the old tree could not move aside; 15 the new tree could not move in and the old one is back;
# 16 the new tree could not move in; 18 neither the new tree nor the old one could move in; 20 a move was refused
# because a file is in use or access was denied, and nothing changed.

$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
$target, $new, $old, $replace = $argv

# Writes the cause of a failed move to stderr and tells whether the guest refused it because a file is in use or
# access was denied, which may pass once a scanner lets go of a file.
function Test-Busy($record) {
  $e = $record.Exception
  while ($e.InnerException) { $e = $e.InnerException }
  [Console]::Error.WriteLine($e.Message)
  # Renaming a directory that holds an open file fails with an IOException of ERROR_ACCESS_DENIED (5); 32 is
  # ERROR_SHARING_VIOLATION.
  ($e -is [UnauthorizedAccessException]) -or (($e -is [IO.IOException]) -and (($e.HResult -band 0xFFFF) -in 5, 32))
}

if (Test-Path -LiteralPath $target) {
  if ($replace -ne '1') { exit 11 }
  $item = Get-Item -LiteralPath $target -Force
  if (-not $item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { exit 12 }
  try {
    [IO.Directory]::Move($target, $old)
  } catch {
    if (Test-Busy $_) { exit 20 }
    exit 14
  }
  try {
    [IO.Directory]::Move($new, $target)
  } catch {
    $busy = Test-Busy $_
    try {
      [IO.Directory]::Move($old, $target)
    } catch {
      [void](Test-Busy $_)
      exit 18
    }
    if ($busy) { exit 20 }
    exit 15
  }
} else {
  try {
    [IO.Directory]::Move($new, $target)
  } catch {
    $busy = Test-Busy $_
    if (Test-Path -LiteralPath $target) { exit 11 }
    if ($busy) { exit 20 }
    exit 16
  }
}
exit 0
