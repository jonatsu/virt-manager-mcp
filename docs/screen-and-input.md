# Screen and mouse

The graphical display lets an agent drive guests that have no guest agent, such as installers and firmware screens.
`screenshot`, `send_keys`, `type_text`, `mouse` and the screen conditions of `wait_for` all use it.

Coordinates are pixels of a full-screen image from `screenshot` without a region, or from a wait's screenshot
attachment. Each call that takes them also takes that image's `image_width` and `image_height`, and refuses a point or
region outside the image. The server maps them onto the screen proportionally, so a resolution change keeps them at
the same relative place. A `region` (`x`, `y`, `width`, `height`) on `screenshot` returns only that part, cropped at
full resolution before downscaling. Its reply gives the region's place in the full-screen image. A region image's
pixels are not click coordinates.

`wait_for` has two screen conditions, which read a frame captured every 500 ms. All screen conditions of one wait use
the same display head.

- `screen_settled` holds once no two consecutive frames differ in more than `max_changed_pixels` pixels (default
  600, which lets a blinking cursor through) for `settle_secs` (default 3, from 0.5 to the timeout less 0.5). It needs
  no OCR models.
- `screen_text` matches a `pattern` against the text that OCR reads from the screen, or from a `region`. A time-out
  returns the last text read as `last_text`, so the agent can loosen its pattern. OCR misreads some text: a console
  `login:` read as `log in:`. A region or a tolerant pattern such as `log ?in:` helps. Each `screen_text` condition
  runs its own OCR, so one pattern with alternatives (`a|b`) costs less than several conditions.

With `screenshot: true`, a match of any condition attaches a PNG downscaled to 1280 pixels wide: the frame a screen
condition matched, or a new capture of display head 0. Some guests drop input sent too early: the Debian installer
ignores keys sent within about 2 s of a new screen. Wait for the screen's text and then for `screen_settled` before
typing.

`screen_text` needs the `ocrs` OCR models, about 12 MiB. Start the server with `VIRTMANAGER_OCR_DOWNLOAD=1` in its
environment, and it downloads them in the background into `~/.local/share/virtmanager-mcp/ocr-models`, or under
`$XDG_DATA_HOME`. It checks each file against a pinned SHA-256 sum, and it skips files already there. Until the
download ends, `screen_text` replies that the models are still downloading. A failed download leaves the server
running, and `screen_text` names the cause. The variable needs `curl` on the host.

To keep the models somewhere else, or on a host without network access, fetch them once and name the directory in
the config:

```sh
scripts/fetch-ocr-models.sh ~/.local/share/virtmanager-mcp/ocr   # checks pinned SHA-256 sums
```

```toml
[display]
ocr_models = "/home/me/.local/share/virtmanager-mcp/ocr" # absolute path
```

When `ocr_models` is set, the server refuses to start unless the directory holds both model files, and it ignores
`VIRTMANAGER_OCR_DOWNLOAD`. With neither, `screen_text` is refused and `screen_settled` still works. OCR runs in a
pool of 4 threads, so it leaves the host's other cores to its VMs.
[`docs/how-to/ocr-models.md`](how-to/ocr-models.md) covers where the models run and how to update and remove
them.

`mouse` moves, clicks, double-clicks, drags or scrolls. It needs a USB tablet (`<input type='tablet' bus='usb'/>`),
which VMs from `create_vm` and `create_vm_from_template` have, and it supports display head 0 only. It sends QEMU
only `input-send-event` commands that it builds itself, through the QEMU monitor. libvirt then marks the domain as
tainted (`custom-monitor`).

In read-only mode, `screenshot` and `mouse` are hidden, and `wait_for` refuses the screen conditions and the
screenshot attachment.
