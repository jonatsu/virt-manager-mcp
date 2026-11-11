# Runs one Windows job: powershell -File DIR\run.ps1 DIR, where DIR\run.ps1 holds this text. Holds DIR\lock open
# without sharing while it runs, records "PID TICKS" (its process ID and start time in UTC ticks) in DIR\pid, runs
# DIR\cmd.ps1 with stdout and stderr merged into DIR\out, and writes the exit code to DIR\exit before it lets go of
# the lock. The guest agent cannot open a file another process holds open for writing, so the log is opened only while
# output is appended, and output waits in memory while a reader such as the agent holds the log.

param($dir)
$ProgressPreference = 'SilentlyContinue'

# How long output is still copied after the job's process exits. A process the job started in the background can keep
# the output pipe open, so the wrapper waits for the job's process rather than for the end of its output.
$drainSeconds = 2
# How long the last output may wait for a reader to let go of the log before the exit code is written without it.
$flushSeconds = 60
# How much output may wait in memory while a reader holds the log. Output beyond it is dropped, and a marker line in the
# log says how much.
$pendingCap = 64MB
$dropped = 0

# Opens $path for $mode without sharing a write, retrying for up to 10 s while another process holds it.
function Open-Retrying($path, $mode, $access, $share) {
  for ($try = 0; ; $try++) {
    try { return [IO.File]::Open($path, $mode, $access, $share) }
    catch [IO.IOException] {
      if ($try -ge 200) { throw }
      Start-Sleep -Milliseconds 50
    }
  }
}

# Appends the waiting output to the log unless a reader holds it, followed by a marker when output was dropped, and
# returns whether nothing is left waiting.
function Write-Pending {
  if ($pending.Length -eq 0 -and $script:dropped -eq 0) { return $true }
  try { $log = [IO.File]::Open("$dir\out", 'Append', 'Write', 'Read') }
  catch [IO.IOException] { return $false }
  try {
    $pending.WriteTo($log)
    if ($script:dropped -gt 0) {
      $marker = [Text.Encoding]::UTF8.GetBytes(
        "`r`n[virtmanager-mcp: dropped $($script:dropped) bytes of output while the log was held open]`r`n")
      $log.Write($marker, 0, $marker.Length)
    }
  } finally { $log.Close() }
  $pending.SetLength(0)
  $script:dropped = 0
  return $true
}

$lock = Open-Retrying "$dir\lock" 'OpenOrCreate' 'ReadWrite' 'None'
$me = Get-Process -Id $PID
"$PID $($me.StartTime.ToUniversalTime().Ticks)" | Set-Content -LiteralPath "$dir\pid.tmp" -Encoding ascii
Move-Item -LiteralPath "$dir\pid.tmp" -Destination "$dir\pid" -Force

# cmd merges stderr into stdout and switches the console to UTF-8, so native programs write UTF-8 too.
$start = New-Object Diagnostics.ProcessStartInfo 'cmd.exe'
$start.Arguments = "/d /c chcp 65001 >nul & powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass " +
  "-File `"$dir\cmd.ps1`" 2>&1"
$start.UseShellExecute = $false
$start.RedirectStandardOutput = $true
$start.CreateNoWindow = $true
[IO.File]::WriteAllBytes("$dir\out", [byte[]]@())
$child = [Diagnostics.Process]::Start($start)
$done = $false
try {
  $stream = $child.StandardOutput.BaseStream
  $buffer = New-Object byte[] 65536
  $pending = New-Object IO.MemoryStream
  $read = $stream.ReadAsync($buffer, 0, $buffer.Length)
  $exitedAt = $null
  while ($true) {
    # A read that fails, as on a broken pipe, ends the output like its end does.
    try { $ready = $read.Wait(200) } catch { break }
    if ($ready) {
      $count = $read.Result
      if ($count -eq 0) { break }
      if ($pending.Length + $count -le $pendingCap) { $pending.Write($buffer, 0, $count) } else { $dropped += $count }
      $read = $stream.ReadAsync($buffer, 0, $buffer.Length)
    }
    [void](Write-Pending)
    if ($child.HasExited) {
      if ($null -eq $exitedAt) { $exitedAt = [DateTime]::UtcNow }
      elseif (([DateTime]::UtcNow - $exitedAt).TotalSeconds -ge $drainSeconds) { break }
    }
  }
  $child.WaitForExit()
  $flushBy = [DateTime]::UtcNow.AddSeconds($flushSeconds)
  while (-not (Write-Pending) -and [DateTime]::UtcNow -lt $flushBy) { Start-Sleep -Milliseconds 100 }
  "$($child.ExitCode)" | Set-Content -LiteralPath "$dir\exit.tmp" -Encoding ascii
  Move-Item -LiteralPath "$dir\exit.tmp" -Destination "$dir\exit" -Force
  $done = $true
} finally {
  # A wrapper that fails leaves no exit code, so the job reads "ended without status", and it ends the job's process
  # tree rather than leave it running out of reach of job_stop.
  if (-not $done -and -not $child.HasExited) { & taskkill.exe /T /F /PID $child.Id 2>&1 | Out-Null }
  $lock.Close()
}
