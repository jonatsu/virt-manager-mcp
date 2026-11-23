# Prints the length of the file $argv[0], or "none" when it does not exist. The guest agent cannot open an archive
# that tar.exe is still writing, but its directory entry shows the length as it grows.
# Exit status: 0 printed, 1 the query failed.

$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
# The server reads stderr as UTF-8, while [Console]::Error writes in the console's code page. AutoFlush keeps a
# message written just before exit.
$stderr = New-Object IO.StreamWriter ([Console]::OpenStandardError()), (New-Object Text.UTF8Encoding $false)
$stderr.AutoFlush = $true
try {
  $file = [IO.FileInfo]::new($argv[0])
  if ($file.Exists) { [Console]::Out.Write($file.Length) } else { [Console]::Out.Write('none') }
  exit 0
} catch {
  $stderr.Write($_.Exception.Message)
  exit 1
}
