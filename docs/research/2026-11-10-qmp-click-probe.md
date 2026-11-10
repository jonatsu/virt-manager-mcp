# A QEMU absolute-pointer click through input-send-event (2026-11-10)

The `mouse` tool sends QMP `input-send-event` commands through `virDomainQemuMonitorCommand`. This note records which
command shape makes the host's QEMU register a click, and the axis mapping that hit the intended pixel.

## Setup

The probe ran on `t-inst` on `qemu:///system`: the Debian 13.7 netinst graphical installer, with a USB tablet, SPICE
and virtio video, at a 1280x800 framebuffer. The domain XML is the one `virt-install --print-xml` gave for the install
phase, kept as `tests/testbed/t-inst.xml`. Commands went through `virsh qemu-monitor-command` in QMP mode, and
`virsh screenshot` captured the screen before and after each attempt.

The target was the installer's "Continue" button at pixel (1218, 774). The mapping `v * 32767 / (dim - 1)`, rounded
down, gives the axis values (31204, 31741).

## Observations

- One `input-send-event` whose `events` array held both `abs` moves, a left `btn` press and a left `btn` release
  returned `{"return":{}}`. The screenshot changed by 34 pixels, which is the pointer drawn at its new place, and the
  installer stayed on the same screen. No click registered.
- Three separate commands registered the click: the two `abs` moves; 50 ms; the left press; 50 ms; the left release.
  After about 4 s the installer showed "Select your location", the screen after "Continue".

## Conclusion

The likely cause is that QEMU delivers the events of one `input-send-event` command in a single input sync, so the
guest never sees the button held. That cause is inferred from the behaviour and was not traced in QEMU's source. In
either case, a click needs the move, the press and the release as
separate commands with a short pause between them. `src/mouse.rs` builds its sequences that way, with 50 ms pauses,
and its axis test asserts the (1218, 774) to (31204, 31741) mapping above.
