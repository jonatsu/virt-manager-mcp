# Install, update and remove the OCR models

The `screen_text` condition of `wait_for` reads text from a VM's screen with OCR (optical character recognition).
It needs two model files from the [`ocrs`](https://github.com/robertknight/ocrs) project, about 12 MiB together.
The server downloads them when its environment holds `VIRTMANAGER_OCR_DOWNLOAD=1`. Otherwise you install them
yourself with a script. Without them, `screen_text` is refused, and `screen_settled`, screenshots and the mouse still
work.

## Where the models run

The models run on the host, inside the MCP server process, on the CPU. The server uses pure-Rust inference (`ocrs`
0.14 on `rten` 0.28): there is no GPU, Python or separate service, and OCR itself makes no network calls. Nothing
runs in the guest. The server reads frames from libvirt screenshots.

The first `screen_text` wait loads both files, and the process keeps them in memory until it exits. If a load fails,
the server does not keep the failure, and the next `screen_text` wait tries again. Each server process loads its own
copy, so several Claude Code sessions each hold one. OCR runs in a pool of 4 threads, which leaves the host's other
cores to its VMs.

## Let the server download the models

You need `curl` on the host.

1. Leave `ocr_models` out of the `[display]` table in `~/.config/virtmanager-mcp/config.toml`.

2. Add the variable to the server's environment. For Claude Code, register the server with it:

   ```sh
   claude mcp add virtmanager -e VIRTMANAGER_OCR_DOWNLOAD=1 -- /path/to/virtmanager-mcp
   ```

3. Restart the MCP server, or start a new Claude Code session.

The server downloads the models into `$XDG_DATA_HOME/virtmanager-mcp/ocr-models`. When `XDG_DATA_HOME` is unset,
that is `~/.local/share/virtmanager-mcp/ocr-models`. The download runs in the background after the server starts,
so the client never waits for it. Until it ends, `screen_text` replies that the models are still downloading. The
server logs "the OCR models are ready" once they are.

Only the value `1` turns the download on. When `ocr_models` is set in the config, the server ignores the variable and
downloads nothing.

### How the server checks the files

The `ocrs` project publishes no checksums. The server therefore pins the SHA-256 sums recorded in
[`2026-11-10-ocr-probe.md`](../research/2026-11-10-ocr-probe.md), the same sums as the fetch script. At every start,
it keeps a file whose sum matches. It downloads a missing file, and a file with another sum, again. Each download
goes to a temporary name in the directory and is renamed into place only when its sum matches. So several servers
that start at once never leave a partial file under a model's name. A server that exits mid-download can leave a
temporary file behind, and the next start removes it once it is 15 minutes old.

If the download fails, the server keeps running, and `screen_text` replies with the cause. Fix the cause and restart
the server to try again. You can also run the fetch script into the download directory: the next `screen_text`
finds the models without a restart. A sum mismatch means the upstream file changed. Do not work around it: report it
on the project's issue tracker.

## Install the models yourself

Use the fetch script on a host without network access for the server, or to keep the models in a directory of your
choice. You need `curl` and `sha256sum` on the host.

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

The script checks the files the same way as the server. If it exits 1, it names the failed download or the
mismatched sum, and it leaves no file under the final name. Exit status 2 means the script was called without a
directory.

## Update the models

The pinned sums decide which models you get, so the models change only when a commit to this repository changes the
pins. A test keeps the server's pins equal to the script's.

- With the download on, install the new server version and restart it. It replaces each file whose sum no longer
  matches.
- With the script, pull the repository, run the script into the same directory, and restart the server. The script
  keeps a file whose sum still matches and replaces one whose sum does not.

A running server keeps the models it already loaded until it restarts.

## Remove the models

Nothing else keeps a copy or a cache of the models.

1. Remove `VIRTMANAGER_OCR_DOWNLOAD` from the server's environment, or the `[display]` table's `ocr_models` line from
   the config.
2. Restart the MCP server.
3. Delete the directory, for example `rm -r ~/.local/share/virtmanager-mcp/ocr-models`.

If you delete the directory but leave `ocr_models` set, the server refuses to start. If you leave the variable set,
the next start downloads the models again.
