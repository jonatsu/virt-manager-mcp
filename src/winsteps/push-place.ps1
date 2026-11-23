# Places a pushed tree in a Windows guest: compiles the push helper, push-helper.cs, which the push wrote into its
# protected archive directory, and runs it. $argv holds the helper's path, the target, the unpacked tree in
# %SystemRoot%\Temp, the names in the target's parent for a copy and for the old tree, and 1 when the push may replace
# the target. The helper does every step in the target's parent on open handles; its header explains why.
# Exit status: the helper's, which push.rs maps to its messages, or 23 when the helper cannot be compiled.

$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
# The server reads stderr as UTF-8, while [Console]::Error writes in the console's code page. AutoFlush keeps a
# message written just before exit.
$stderr = New-Object IO.StreamWriter ([Console]::OpenStandardError()), (New-Object Text.UTF8Encoding $false)
$stderr.AutoFlush = $true
$helper, $target, $staging, $copy, $old, $replace = $argv
try {
  Add-Type -Path $helper
} catch {
  $stderr.Write("compiling the push helper failed: $($_.Exception.Message)")
  exit 23
}
exit [VmmPush]::Place($target, $staging, $copy, $old, $replace -eq '1', $stderr)
