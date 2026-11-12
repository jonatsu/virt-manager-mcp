# Deletes a push's own .vmm- entries, named in $argv as drive paths. Remove-Item -Recurse -Force removes read-only
# entries, and removes junctions without following them. Each path gets the \\?\ prefix, without which Remove-Item
# cannot delete a tree holding paths longer than 260 characters. A missing entry is no error.
# Exit status: 0 all gone, 1 an entry could not be deleted.

$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
$failed = $false
foreach ($path in $argv) {
  $long = '\\?\' + $path
  try {
    if (Test-Path -LiteralPath $long) { Remove-Item -LiteralPath $long -Recurse -Force }
  } catch {
    [Console]::Error.WriteLine("${path}: $($_.Exception.Message)")
    $failed = $true
  }
}
if ($failed) { exit 1 }
exit 0
