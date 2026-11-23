# Creates the directory $argv[0], a new .vmm- name beside a push's target, for tar.exe to unpack into. Its access list
# is protected from inheritance and grants full control, inherited by everything inside, only to SYSTEM (S-1-5-18) and
# Administrators (S-1-5-32-544), so no other guest user can plant a junction in it while tar.exe unpacks. SIDs, not
# names, because group names are localized. CreateDirectory accepts a path that already exists, so the script reads
# the directory back: a directory that another user created first, after seeing the archive's name, has that user as
# its owner, who could change its access list later.
# Exit status: 0 created and checked, 1 failed to create or read back, 2 the directory is not one this script made.

$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
# The server reads stderr as UTF-8, while [Console]::Error writes in the console's code page. AutoFlush keeps a
# message written just before exit.
$stderr = New-Object IO.StreamWriter ([Console]::OpenStandardError()), (New-Object Text.UTF8Encoding $false)
$stderr.AutoFlush = $true
$path = $argv[0]
$trusted = @('S-1-5-18', 'S-1-5-32-544')
$sid = [Security.Principal.SecurityIdentifier]
$full = [Security.AccessControl.FileSystemRights]::FullControl
$inherit = [Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
$none = [Security.AccessControl.PropagationFlags]::None
$allow = [Security.AccessControl.AccessControlType]::Allow
try {
  $security = New-Object Security.AccessControl.DirectorySecurity
  $security.SetAccessRuleProtection($true, $false)
  foreach ($id in $trusted) {
    $rule = New-Object Security.AccessControl.FileSystemAccessRule ((New-Object $sid $id), $full, $inherit, $none, $allow)
    $security.AddAccessRule($rule)
  }
  [void][IO.Directory]::CreateDirectory($path, $security)
} catch {
  $stderr.Write($_.Exception.Message)
  exit 1
}

function Refuse($why) {
  $stderr.Write("${path}: $why")
  exit 2
}

try {
  $item = Get-Item -LiteralPath $path -Force
  $read = [IO.Directory]::GetAccessControl($path, 'Access, Owner')
} catch {
  $stderr.Write("${path}: cannot read it back: $($_.Exception.Message)")
  exit 1
}
if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { Refuse 'it is a junction or link' }
if ($trusted -notcontains $read.GetOwner($sid).Value) { Refuse "its owner is $($read.GetOwner($sid).Value)" }
if (-not $read.AreAccessRulesProtected) { Refuse 'its access list inherits from its parent' }
$rules = @($read.GetAccessRules($true, $true, $sid))
$granted = @($rules | Where-Object {
    -not $_.IsInherited -and $_.AccessControlType -eq $allow -and $_.FileSystemRights -eq $full -and
    $_.InheritanceFlags -eq $inherit -and $_.PropagationFlags -eq $none
  } | ForEach-Object { $_.IdentityReference.Value } | Sort-Object -Unique)
if ($rules.Count -ne 2 -or $granted.Count -ne 2 -or (Compare-Object $granted $trusted)) {
  Refuse "its access list is not the one the push set ($($rules.Count) rules)"
}
exit 0
