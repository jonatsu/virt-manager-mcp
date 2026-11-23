# Deletes the pull's archive $argv[0], a .vmm- name in the guest's temporary directory. A missing archive is no error.
# Exit status: 0 gone, 1 still there.

$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
# The server reads stderr as UTF-8, while [Console]::Error writes in the console's code page. AutoFlush keeps a
# message written just before exit.
$stderr = New-Object IO.StreamWriter ([Console]::OpenStandardError()), (New-Object Text.UTF8Encoding $false)
$stderr.AutoFlush = $true
try {
  if (Test-Path -LiteralPath $argv[0]) { Remove-Item -LiteralPath $argv[0] -Force }
  exit 0
} catch {
  $stderr.Write($_.Exception.Message)
  exit 1
}
