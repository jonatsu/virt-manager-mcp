# Serial console "stall": root cause

Date: 2026-11-09. Subject: `console_read` in `src/console.rs` at commit `b4a074d`, before the fix in `acf1538`.
Test bed: `trial-deb13` on `qemu:///system`, libvirt 12.0.0, driven by `tests/testbed/mcp_call.py`.

The 2026-11-09 trial reported that console reads stall after the first burst of output: the echo of `root` arrived,
and the `Password:` prompt never did. The console stream never stalled. The reads returned before the guest
answered.

## Evidence

1. **Input reaches the guest.** After `console_write("root")`, the guest agent listed `/bin/login --` and
   `/usr/sbin/unix_chkpwd root nullok` on `ttyS0`. `login` had accepted the name and was checking a password.
2. **Rereads from offset 0 never wait.** `console_read` returned as soon as any byte existed after `since`. With
   `since: 0`, the first prompt always exists, so `wait_ms` was ignored. A run with the libvirt client log enabled
   (`LIBVIRT_LOG_FILTERS="1:rpc 1:event 1:remote"`) spans 18:51:10.538 to 18:51:10.604, 66 ms for the whole
   sequence. Each read had returned within milliseconds of the write before it.
3. **The stream and the event loop work.** In the same log, the event-loop thread runs `virNetClientIncomingEvent`
   and queues each incoming stream packet with `virNetClientStreamQueuePacket`. The reader thread then receives it
   with `virNetClientStreamRecvPacket`. Thread states sampled mid-run show the reader thread in its 30 ms sleep and
   the `libvirt-events` thread in `poll`, with nothing blocked.
4. **A login works without code changes.** With a pause of several seconds before each read, one read from offset 0
   returned `login: root`, `Password:`, the motd and `root@trial-deb13:~#`. A command typed there printed its
   result, and `tty` reported `/dev/ttyS0`.

The earlier hypothesis, that the reader must use `virStreamEventAddCallback` as `virsh console` does, is refuted:
polling `virStreamRecv` on a non-blocking stream delivers every byte.

## Fix

A waiting read now waits for output that no earlier read returned, then until that output pauses for 500 ms,
whatever `since` is. A zero `wait_ms` still returns at once. `tests/testbed/cases/console-stall.json`, unchanged
from the trial, then shows the `Password:` prompt, the motd and a root shell prompt.
