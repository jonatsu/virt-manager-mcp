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
