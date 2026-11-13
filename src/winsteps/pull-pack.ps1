# Packs the directory $argv[0] into the archive $argv[1] with tar.exe at $argv[2]. A pull runs this script only for a
# directory whose path is not ASCII: the guest agent fails a guest-exec whose arguments are not ASCII, and this script
# receives the path inside -EncodedCommand instead. tar.exe packs "." in the directory set here, so its arguments stay
# ASCII. Its stderr is relayed as plain text, and its own exit status, 0, 1 or 255, is the script's.
# Exit status: tar.exe's, 30 the directory cannot be entered, 31 tar.exe did not run.

$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
$dir, $archive, $tar = $argv
# The server reads stderr as UTF-8, while [Console]::Error writes in the console's code page. Changing
# [Console]::OutputEncoding instead would also change how PowerShell reads tar.exe's own output.
$stderr = New-Object IO.StreamWriter ([Console]::OpenStandardError()), (New-Object Text.UTF8Encoding $false)
$stderr.AutoFlush = $true
# A failed call of a missing program would reach stderr as CLIXML.
if (-not (Test-Path -LiteralPath $tar -PathType Leaf)) {
  $stderr.Write("$tar does not exist")
  exit 31
}
try {
  Set-Location -LiteralPath $dir
} catch {
  $stderr.Write($_.Exception.Message)
  exit 30
}
# A native program's stderr line arrives as an error record, which 'Stop' would turn into a failure of the script.
$ErrorActionPreference = 'Continue'
$global:LASTEXITCODE = $null
& $tar -c -f $archive . 2>&1 | ForEach-Object {
  if ($_ -is [Management.Automation.ErrorRecord]) {
    $stderr.WriteLine($_.Exception.Message)
  } else {
    [Console]::Out.WriteLine($_)
  }
}
if ($null -eq $LASTEXITCODE) { exit 31 }
exit $LASTEXITCODE
