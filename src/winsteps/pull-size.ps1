# Prints the length of the file $argv[0], or "none" when it does not exist. The guest agent cannot open an archive
# that tar.exe is still writing, but its directory entry shows the length as it grows.
# Exit status: 0 printed, 1 the query failed.

$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
try {
  $file = [IO.FileInfo]::new($argv[0])
  if ($file.Exists) { [Console]::Out.Write($file.Length) } else { [Console]::Out.Write('none') }
  exit 0
} catch {
  [Console]::Error.Write($_.Exception.Message)
  exit 1
}
