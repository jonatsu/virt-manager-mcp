# Ends a Windows job's process tree: run as powershell -EncodedCommand with $id (a UUID the server checked) defined as a
# quoted literal before this text. Ends the tree only when the recorded process ID still has the recorded start time,
# so a reused ID is never hit. A console process has no equivalent of SIGTERM, so the tree always ends at once.
# Exit status: 0 ended, 3 not started, 4 not running, 5 taskkill failed, 6 no such job.

$ProgressPreference = 'SilentlyContinue'
$dir = "C:\ProgramData\virtmanager-mcp\jobs\$id"
if (-not (Test-Path -LiteralPath "$dir\cmd.ps1")) {
  'no such job'
  exit 6
}
$recorded = $null
try { $recorded = (Get-Content -LiteralPath "$dir\pid" -Raw -ErrorAction Stop).Trim().Split(' ') } catch {}
if (-not $recorded -or $recorded.Count -lt 2) {
  'not started'
  exit 3
}
$process = Get-Process -Id ([int]$recorded[0]) -ErrorAction SilentlyContinue
if ((Test-Path -LiteralPath "$dir\exit") -or -not $process -or
  $process.StartTime.ToUniversalTime().Ticks -ne [long]$recorded[1]) {
  'not running'
  exit 4
}
taskkill.exe /T /F /PID $recorded[0] *> $null
if ($LASTEXITCODE -ne 0) {
  "taskkill failed with $LASTEXITCODE"
  exit 5
}
'ended'
