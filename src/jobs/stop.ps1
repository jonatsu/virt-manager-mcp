# Ends a Windows job's process tree: run as powershell -EncodedCommand with $id (a UUID the server checked) defined as a
# quoted literal before this text. Ends the tree only when the recorded process ID still has the recorded start time,
# so a reused ID is never hit. A console process has no equivalent of SIGTERM, so the tree always ends at once.
# taskkill can fail while the tree goes anyway: it exits 128 when the root has just exited, which can leave a child
# running, and 255 when the root exits while it walks the tree. So the script lists the tree first, and after a
# failure ends what is left of that list itself, matching each process by ID and start time.
# Exit status: 0 ended, 3 not started, 4 not running, 5 taskkill failed and part of the tree runs on, 6 no such job,
# 7 ended after taskkill failed.

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
# The tree as it stands: the root and every descendant, each with its start time. A child must have started after
# its parent, so a process whose parent ID was reused is not taken for one.
$started = @{}
$children = @{}
foreach ($p in @(Get-CimInstance Win32_Process)) {
  $started[[int]$p.ProcessId] = $p.CreationDate.ToUniversalTime()
  $children[[int]$p.ParentProcessId] += @([int]$p.ProcessId)
}
$tree = @{ ([int]$recorded[0]) = $process.StartTime.ToUniversalTime() }
$queue = New-Object Collections.Generic.Queue[int]
$queue.Enqueue([int]$recorded[0])
while ($queue.Count -gt 0) {
  $parent = $queue.Dequeue()
  foreach ($child in @($children[$parent])) {
    if ($null -eq $child -or $tree.ContainsKey($child) -or $started[$child] -lt $tree[$parent]) { continue }
    $tree[$child] = $started[$child]
    $queue.Enqueue($child)
  }
}

taskkill.exe /T /F /PID $recorded[0] *> $null
$status = $LASTEXITCODE
if ($status -eq 0) {
  'ended'
  exit 0
}

# The processes of the list still running, by ID and a start time within a second of the listed one.
function Alive {
  foreach ($id in @($tree.Keys)) {
    $p = Get-Process -Id $id -ErrorAction SilentlyContinue
    if ($p -and [Math]::Abs(($p.StartTime.ToUniversalTime() - $tree[$id]).TotalSeconds) -lt 1) { $p }
  }
}
$end = (Get-Date).AddSeconds(2)
do {
  $left = @(Alive)
  foreach ($p in $left) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue }
  if ($left.Count -eq 0) { break }
  Start-Sleep -Milliseconds 100
} while ((Get-Date) -lt $end)
if (@(Alive).Count -eq 0) {
  "ended; taskkill exited $status after the tree had gone"
  exit 7
}
"taskkill failed with $status"
exit 5
