# Deletes the pull's archive $argv[0], a .vmm- name in the guest's temporary directory. A missing archive is no error.
# Exit status: 0 gone, 1 still there.

$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
try {
  if (Test-Path -LiteralPath $argv[0]) { Remove-Item -LiteralPath $argv[0] -Force }
  exit 0
} catch {
  [Console]::Error.Write($_.Exception.Message)
  exit 1
}
