# Console output stalls after an unpaced 4000-byte write (2026-11-10)

`tests/testbed/run-all.sh` runs `owrt-pacing.json` with `--check`, and the case fails most of the time. This note
records what the failing runs showed. The cause is not known yet; roadmap follow-up 14 tracks it.

## What the case does

On the serial-only OpenWrt guest `t-owrt`, the case writes 4000 bytes into `cat > /tmp/paced` with pacing (16-byte
chunks, 20 ms apart), ends it with Ctrl-D, then writes the same 4000 bytes unpaced into `cat > /tmp/unpaced`, ends it
with Ctrl-D, and runs `wc -c` and `md5sum` on both files. Call 14, a `console_read`, expects both files to hold 4000
bytes with MD5 `3f59f6f6f02f857841c375830e5edeb1`.

## Observations

- Call 14 failed in 5 of 7 runs during the `run-all.sh` work, and in 7 of 8 runs of a comparison:
  - the build at `f438cf6`, before the console-lock commits `13434e4` and `1316415`, failed 4 of 4;
  - the build at `b8e89cd` failed 3 of 4.
- Every failure is the same:
  - After the unpaced write, each later `console_write` still replies "sent N bytes".
  - The session's buffer stops growing at about offset 38500. Its last output is the shell's echo of
    `cat > /tmp/unpaced`, and nothing arrives for 25 s or more.
- A probe opened a fresh session with `console_open` and `force: true`. That session works normally, and it showed
  what the guest received:
  - The unpaced bytes reached the guest.
  - The Ctrl-D after them was lost, so `cat` also took in the `wc` line.
  - `/tmp/unpaced` held 4064 bytes, with MD5 `4bc8831eabf4dc665cd99a7060b0a167`.
  - `/tmp/paced` held 4000 bytes with the expected MD5.

  The guest's echo of the unpaced bytes never reached the old session.
- Run 1 recorded one passing run of this case, at `ff94648`. Given the rates above, that pass was probably luck rather
  than a sign that a later change broke the case.

## Open questions

- Why does the old session receive nothing after the burst? The server polls a non-blocking libvirt stream with no
  stream event callback (`read_loop` and `send_all` in `src/console.rs`). The stall may come from that loop or from
  libvirt's stream flow control. None of this is verified.
- Why is the Ctrl-D lost? An unpaced write can overrun the guest's serial input. Losing input is what pacing exists to
  prevent, so the lost Ctrl-D alone may be expected. The stalled output is not.
