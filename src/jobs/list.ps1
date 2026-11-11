# Lists the jobs in a Windows guest, one per line: ID STARTED STATE [EXIT_CODE], where STARTED is the command file's
# modification time in seconds since the epoch and STATE is exited, starting, running or ended. A job runs while its
# wrapper holds DIR\lock open without sharing.

$ProgressPreference = 'SilentlyContinue'
$jobs = 'C:\ProgramData\virtmanager-mcp\jobs'
if (-not (Test-Path -LiteralPath $jobs)) { exit 0 }
$epoch = New-Object DateTime 1970, 1, 1, 0, 0, 0, ([DateTimeKind]::Utc)

function Get-Code($dir) {
  try { (Get-Content -LiteralPath "$dir\exit" -Raw -ErrorAction Stop).Trim() } catch { $null }
}

function Test-Held($dir) {
  try {
    [IO.File]::Open("$dir\lock", 'Open', 'Read', 'None').Close()
    $false
  } catch [IO.FileNotFoundException] { $false } catch { $true }
}

foreach ($job in Get-ChildItem -LiteralPath $jobs -Directory) {
  $dir = $job.FullName
  if (-not (Test-Path -LiteralPath "$dir\cmd.ps1")) { continue }
  $started = [long]((Get-Item -LiteralPath "$dir\cmd.ps1").LastWriteTimeUtc - $epoch).TotalSeconds
  $code = Get-Code $dir
  if ($null -ne $code) { "$($job.Name) $started exited $code"; continue }
  if (-not (Test-Path -LiteralPath "$dir\pid")) { "$($job.Name) $started starting"; continue }
  if (Test-Held $dir) { "$($job.Name) $started running"; continue }
  # The job ended between the first check and now.
  $code = Get-Code $dir
  if ($null -ne $code) { "$($job.Name) $started exited $code" } else { "$($job.Name) $started ended" }
}
