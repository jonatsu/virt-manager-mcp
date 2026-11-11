# Runs one Windows job: powershell -File DIR\run.ps1 DIR, where DIR\run.ps1 holds this text. Holds DIR\lock open
# without sharing while it runs, records "PID TICKS" (its process ID and start time in UTC ticks) in DIR\pid, runs
# DIR\cmd.ps1 with stdout and stderr merged into DIR\out, and writes the exit code to DIR\exit before it lets go of
# the lock. The guest agent cannot open a file another process holds open for writing, so the log is opened only while
# a chunk is appended.

param($dir)
$ProgressPreference = 'SilentlyContinue'

# Opens $path for $mode without sharing a write, retrying for up to 10 s while a reader such as the guest agent
# holds it.
function Open-Retrying($path, $mode, $access, $share) {
  for ($try = 0; ; $try++) {
    try { return [IO.File]::Open($path, $mode, $access, $share) }
    catch [IO.IOException] {
      if ($try -ge 200) { throw }
      Start-Sleep -Milliseconds 50
    }
  }
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
$stream = $child.StandardOutput.BaseStream
$buffer = New-Object byte[] 65536
while (($count = $stream.Read($buffer, 0, $buffer.Length)) -gt 0) {
  $log = Open-Retrying "$dir\out" 'Append' 'Write' 'Read'
  $log.Write($buffer, 0, $count)
  $log.Close()
}
$child.WaitForExit()
"$($child.ExitCode)" | Set-Content -LiteralPath "$dir\exit.tmp" -Encoding ascii
Move-Item -LiteralPath "$dir\exit.tmp" -Destination "$dir\exit" -Force
$lock.Close()
