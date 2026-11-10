# Install, update and remove the OCR models

The `screen_text` condition of `wait_for` reads text from a VM's screen with OCR (optical character recognition).
It needs two model files from the [`ocrs`](https://github.com/robertknight/ocrs) project. The server never
downloads them, so you install them, update them and remove them yourself. Without them, `screen_text` is refused,
and `screen_settled`, screenshots and the mouse still work.

## Where the models run

The models run on the host, inside the MCP server process, on the CPU. The server uses pure-Rust inference (`ocrs`
0.14 on `rten` 0.28): there is no GPU, Python or separate service, and OCR makes no network calls. Nothing runs in
the guest. The server reads frames from libvirt screenshots.

The first `screen_text` wait loads both files, and the process keeps them in memory until it exits. If a load fails,
the server does not keep the failure, and the next `screen_text` wait tries again. Each server process loads its own
copy, so several Claude Code sessions each hold one. OCR runs in a pool of 4 threads, which leaves the host's other
cores to its VMs.

## Install the models

You need `curl` and `sha256sum` on the host.

1. Choose a directory owned by the account that runs the server, such as `~/.local/share/virtmanager-mcp/ocr`.

2. Run the fetch script from a checkout of this repository:

   ```sh
   scripts/fetch-ocr-models.sh ~/.local/share/virtmanager-mcp/ocr
   ```

   The script creates the directory and downloads `text-detection.onnx` (about 2.5 MB) and `text-recognition.onnx`
   (about 9.7 MB) from `https://ocrs-models.s3-accelerate.amazonaws.com`. It prints `downloaded, sum matches` for
   each file.

3. Add the directory to `~/.config/virtmanager-mcp/config.toml` as an absolute path:

   ```toml
   [display]
   ocr_models = "/home/me/.local/share/virtmanager-mcp/ocr"
   ```

4. Restart the MCP server, or start a new Claude Code session.

When `ocr_models` is set, the server refuses to start unless the path is absolute and the directory holds both
files. The error names the missing file.

### How the script checks the files

The `ocrs` project publishes no checksums. The script therefore pins the SHA-256 sums recorded in
[`2026-11-10-ocr-probe.md`](../research/2026-11-10-ocr-probe.md). It downloads each file to a temporary name in the
target directory. It moves the file to its final name only when the sum matches, so a file under that name always has
the pinned sum.

If the script exits 1, it names the failed download or the mismatched sum, and it leaves no file under the final
name. A mismatch means the upstream file changed. Do not work around it: report it on the project's issue tracker.
Exit status 2 means the script was called without a directory.

## Update the models

The pinned sums decide which models you get, so the models change only when a commit to this repository changes the
pins in `scripts/fetch-ocr-models.sh`.

1. Pull the repository.
2. Run the script into the same directory.
3. Restart the MCP server.

The script keeps a file whose sum still matches and replaces one whose sum does not. A running server keeps the
models it already loaded until it restarts.

## Remove the models

Nothing else keeps a copy or a cache of the models.

1. Remove the `[display]` table's `ocr_models` line from the config.
2. Restart the MCP server.
3. Delete the directory, for example `rm -r ~/.local/share/virtmanager-mcp/ocr`.

If you delete the directory but leave `ocr_models` set, the server refuses to start.
