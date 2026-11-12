# Creates the directory $argv[0], a new .vmm- name beside a push's target, for tar.exe to unpack into.
# Exit status: 0 created, 1 failed.

$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
try {
  [void][IO.Directory]::CreateDirectory($argv[0])
  exit 0
} catch {
  [Console]::Error.Write($_.Exception.Message)
  exit 1
}
