# Prepares a directory pull in a Windows guest. Run as powershell -EncodedCommand, with $argv defined before this text
# as an array of quoted literals: $argv[0] is the directory to pull.
# Prints the temporary directory for the archive and the path of tar.exe, one per line.
# Exit status: 0 ready, 10 no tar.exe, 11 not a directory, 13 no temporary directory.

$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
$tar = Join-Path $env:SystemRoot 'System32\tar.exe'
if (-not (Test-Path -LiteralPath $tar -PathType Leaf)) { exit 10 }
if (-not (Test-Path -LiteralPath $argv[0] -PathType Container)) { exit 11 }
$temp = Join-Path $env:SystemRoot 'Temp'
if (-not (Test-Path -LiteralPath $temp -PathType Container)) { exit 13 }
[Console]::Out.Write("$temp`n$tar")
exit 0
