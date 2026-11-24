# Guest setup

## Guest agent channel (both OSes)

The VM needs a guest agent channel. virt-manager adds one automatically for new VMs; check the XML
(*Details → Overview → XML*) for:

```xml
<channel type="unix">
  <target type="virtio" name="org.qemu.guest_agent.0"/>
</channel>
```

If it is missing, add it (*Add Hardware → Channel → org.qemu.guest_agent.0*) and power-cycle the VM.

## Linux guest

```sh
sudo dnf install qemu-guest-agent     # or apt / pacman
sudo systemctl enable --now qemu-guest-agent
```

> **RHEL / CentOS / Rocky / Alma:** the packaged agent blocks `guest-exec` and file
> operations by default. Unblock both, because file writes also use `guest-exec`: remove them
> from `BLOCK_RPCS` (or the `--block-rpcs` / `FILTER_RPC_ARGS` setting) in
> `/etc/sysconfig/qemu-ga` and restart the agent.

Serial console (for `console_*`): the default virt-manager VM has a `pty` serial console. Enable a login
on it with:

```sh
sudo systemctl enable --now serial-getty@ttyS0.service
```

To also see kernel/boot messages there, add `console=ttyS0` to the kernel command line.

## Windows guest

1. Install the [virtio-win drivers](https://github.com/virtio-win/virtio-win-pkg-scripts)
   (`virtio-win-guest-tools.exe`). It installs the VirtIO serial driver and the **QEMU Guest Agent** service.
2. Make sure the *QEMU Guest Agent* service is running (`services.msc`).

`exec` defaults to PowerShell on Windows (`shell: "cmd"` is also available). `list_dir`/`stat_path`
need PowerShell. Windows does not offer a serial console login by default, so use `exec`
or `screenshot`/`send_keys`/`type_text` instead of `console_*`.

A PowerShell command's exit code is the one passed to `exit`, 1 after an uncaught error or when the last command is a
failing native program or cmdlet, and 0 otherwise. `$false` is not a failure, so a bare `Test-Path` exits 0 either way;
write `exit [int](-not (Test-Path C:\ready))` for a `wait_for` `command` condition.

Jobs run a PowerShell script as `SYSTEM` in session 0, without a desktop, so they cannot open windows the screen
shows. They keep their files in `C:\ProgramData\virtmanager-mcp\jobs\<id>\`; `job_start` refuses to run when that
directory or its parent is a junction or symbolic link, is owned by anyone but `SYSTEM` or `Administrators`, or grants
access to anyone else. A job's exit code follows `powershell -File`: the code passed to `exit`, 1 after an uncaught
error, and 0 otherwise, even after a failing native program, so end a job with `exit $LASTEXITCODE` to pass on a
program's code. `job_stop` ends the job's process tree at once with `taskkill /T /F`, with or without `force`, and the
job then reads "ended without status"; processes outside that tree keep running, such as those the job started
through WMI, or whose parent has already exited. The job's output waits in memory, up to 64 MiB, while a reader such
as the agent holds the log file open; output beyond that is dropped, and a line in the log says how many bytes.

Windows support was tested on an offline, English Windows 11 Pro guest. Reading a job's state depends on the guest
agent's English error texts for a missing or locked file, so on a guest in another display language `job_read`,
`job_list` and the job conditions may fail.

Restarting the `QEMU-GA` service sometimes leaves the agent unreachable until the VM is powered off and on: the new
agent runs, but libvirt reports its channel disconnected. This was seen with `virtio-win` 0.1.302. Windows also
ignores the ACPI power button while a user is logged on, so a graceful `stop_vm` relies on the agent there.

## Remote desktop (RDP)

libvirt on the host offers no RDP display for a VM, so RDP comes from the guest. Enable it inside the guest, then
connect a client on the host to the guest's own address, which `vm_addresses` reports, on TCP port 3389. The VM needs
a network the host reaches, such as libvirt's `default` network. A VNC or SPICE display (`create_vm` and
`set_display`) needs none of this. The server does not change RDP in a guest; the steps below are for you, or for an
agent that runs them with `exec`.

### Windows

Only the Pro, Enterprise and Education editions and Windows Server host RDP; Home does not
([Microsoft Learn][ms-rdp]). The guest agent runs as `SYSTEM`, so `exec` can make these changes. Run them in
PowerShell, the default shell:
The guest agent runs as `SYSTEM`, so `exec` can make these changes. Run them in PowerShell, the default shell:

```powershell
Set-ItemProperty -Path 'HKLM:\System\CurrentControlSet\Control\Terminal Server' -Name fDenyTSConnections -Value 0
Enable-NetFirewallRule -DisplayGroup 'Remote Desktop'
Start-Service TermService
```

The registry value `fDenyTSConnections` set to 0 allows connections, the firewall rule group opens port 3389, and
`TermService` is the service that listens. Network Level Authentication (NLA) makes a client sign in before the
session starts. The test guest had it on; this value sets it explicitly, and 0 allows clients without NLA:

```powershell
Set-ItemProperty -Path 'HKLM:\System\CurrentControlSet\Control\Terminal Server\WinStations\RDP-Tcp' -Name UserAuthentication -Value 1
```

Administrators and members of the `Remote Desktop Users` group can sign in, and the account needs a password. To turn
RDP off again, set `fDenyTSConnections` back to 1, run `Disable-NetFirewallRule -DisplayGroup 'Remote Desktop'` and
`Stop-Service TermService -Force`. The test bed runs these steps on a Windows 11 Pro guest
(`tests/testbed/cases/win-rdp.json`): port 3389 accepts a connection from the host afterwards, and does not accept
one once RDP is off again.

### GNOME

GNOME Remote Desktop shares the session of a logged-in user over RDP. It needs a TLS key and certificate, and RDP
credentials of its own that differ from the login password. Run these as the desktop user in the guest, in a
graphical session, following GNOME Remote Desktop's configuration guide (`docs/configuration.md` in its repository).
These steps are not covered by the test bed.

```sh
dir=~/.local/share/gnome-remote-desktop
mkdir -p "$dir"
openssl req -new -newkey rsa:4096 -days 720 -nodes -x509 -subj /CN=gnome-guest -out "$dir/tls.crt" -keyout "$dir/tls.key"
grdctl rdp set-tls-cert "$dir/tls.crt"
grdctl rdp set-tls-key "$dir/tls.key"
grdctl rdp set-credentials          # reads the username and password from standard input
grdctl rdp disable-view-only        # allows keyboard and mouse input
grdctl rdp enable
systemctl --user enable --now gnome-remote-desktop.service
```

`grdctl status` shows the result. Locking the screen closes this kind of connection. The guest agent runs as root,
so `exec` can run these commands only as the desktop user, with that user's `XDG_RUNTIME_DIR` and session bus; a
terminal in the session is simpler. GNOME Remote Desktop also has a headless mode and a system service mode for
guests that nobody is logged in to, described in the same guide.

[ms-rdp]: https://learn.microsoft.com/en-us/windows-server/remote/remote-desktop-services/remotepc/remote-desktop-allow-access
