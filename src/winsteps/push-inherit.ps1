# Gives the unpacked tree $argv[0] back the access list its parent passes on, once tar.exe has finished: turns
# inheritance back on for the staging directory, removes the two rules push-mkdir.ps1 set, and lets Windows propagate
# the result to every entry inside. SetAccessControl writes only the access list, leaving owner and group alone, and
# the propagation does not pass through a junction. It applies the list on every run, so a second run repairs a
# propagation that a first one left unfinished.
# Exit status: 0 done, 1 failed.

$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
$path = $argv[0]
try {
  $security = [IO.Directory]::GetAccessControl($path, 'Access')
  $security.SetAccessRuleProtection($false, $false)
  foreach ($rule in @($security.GetAccessRules($true, $false, [Security.Principal.SecurityIdentifier]))) {
    [void]$security.RemoveAccessRuleSpecific($rule)
  }
  [IO.Directory]::SetAccessControl($path, $security)
  exit 0
} catch {
  [Console]::Error.Write($_.Exception.Message)
  exit 1
}
