# Deletes a push's own .vmm- entries, named in $argv. Remove-Item -Recurse -Force removes read-only entries, and
# removes junctions without following them. A missing entry is no error.
# Exit status: 0 all gone, 1 an entry could not be deleted.

$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
$failed = $false
foreach ($path in $argv) {
  try {
    if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Recurse -Force }
  } catch {
    [Console]::Error.WriteLine("${path}: $($_.Exception.Message)")
    $failed = $true
  }
}
if ($failed) { exit 1 }
exit 0
