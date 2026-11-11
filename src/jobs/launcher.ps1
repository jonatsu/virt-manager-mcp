# Starts a job in a Windows guest. Run through guest-exec as powershell -EncodedCommand, with $id (a UUID the server
# checked) and $wrapper (the fixed text of wrapper.ps1) defined as quoted literals before this text. Standard input
# carries the job's command as UTF-8 bytes, which this script stores as data and never evaluates. Prints one line
# saying how the job started or why it was refused.
# Exit status: 0 started, 3 refused, 4 failed to start.

$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
$base = 'C:\ProgramData\virtmanager-mcp'
$jobs = "$base\jobs"
$sys = New-Object Security.Principal.SecurityIdentifier 'S-1-5-18'
$adm = New-Object Security.Principal.SecurityIdentifier 'S-1-5-32-544'

function Refuse($why) {
  "refused: $why"
  exit 3
}

# Each level must be owned by SYSTEM or Administrators, inherit nothing, and grant access to those two only, so no
# other user can swap or reach a path under it. A missing level is created in that form in one step.
function Assert-Protected($path) {
  if (-not (Test-Path -LiteralPath $path)) {
    $acl = New-Object Security.AccessControl.DirectorySecurity
    $acl.SetOwner($adm)
    $acl.SetAccessRuleProtection($true, $false)
    foreach ($sid in $sys, $adm) {
      $rule = New-Object Security.AccessControl.FileSystemAccessRule($sid, 'FullControl',
        'ContainerInherit,ObjectInherit', 'None', 'Allow')
      $acl.AddAccessRule($rule)
    }
    try { [IO.Directory]::CreateDirectory($path, $acl) | Out-Null } catch { Refuse "cannot create $path" }
  }
  $item = Get-Item -LiteralPath $path -Force
  if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { Refuse "$path is a junction or symbolic link" }
  if (-not $item.PSIsContainer) { Refuse "$path is not a directory" }
  $acl = Get-Acl -LiteralPath $path
  $owner = $acl.GetOwner([Security.Principal.SecurityIdentifier])
  if ($owner -ne $sys -and $owner -ne $adm) { Refuse "$path is not owned by SYSTEM or Administrators" }
  if (-not $acl.AreAccessRulesProtected) { Refuse "$path inherits access rules" }
  foreach ($rule in $acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])) {
    if ($rule.IdentityReference -ne $sys -and $rule.IdentityReference -ne $adm) {
      $who = $rule.IdentityReference
      try { $who = $who.Translate([Security.Principal.NTAccount]) } catch {}
      Refuse "the access list of $path grants access to $who"
    }
  }
}

Assert-Protected $base
Assert-Protected $jobs
$dir = "$jobs\$id"
if (Test-Path -LiteralPath $dir) { Refuse "$dir already exists" }
# The job directory inherits the protected access list of the jobs directory.
try { [IO.Directory]::CreateDirectory($dir) | Out-Null } catch { Refuse "cannot create $dir" }

# From here on, a job that does not start leaves no directory behind, so it never lists as starting.
try {
  $in = [Console]::OpenStandardInput()
  $command = New-Object IO.MemoryStream
  $in.CopyTo($command)
  # A byte-order mark makes PowerShell 5.1 read the command as UTF-8.
  $bom = [byte[]](0xEF, 0xBB, 0xBF)
  [IO.File]::WriteAllBytes("$dir\cmd.ps1", $bom + $command.ToArray())
  [IO.File]::WriteAllText("$dir\run.ps1", $wrapper, (New-Object Text.UTF8Encoding $true))
  # The WMI service starts the wrapper, so it is no child of the guest agent.
  $line = "powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -File " +
    "`"$dir\run.ps1`" `"$dir`""
  $result = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = $line }
  if ($result.ReturnValue -ne 0) { throw "Win32_Process.Create returned $($result.ReturnValue)" }
} catch {
  Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
  "failed: $_"
  exit 4
}
'started with Win32_Process.Create'
exit 0
