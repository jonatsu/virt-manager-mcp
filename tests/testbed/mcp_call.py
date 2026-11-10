"""Drive an MCP stdio server through a scripted list of tool calls.

Usage: python3 -I mcp_call.py [--check] <calls.json> -- <server command...>
calls.json is a list of {"tool": name, "args": {...}}. Special tools: "__list__" prints every
advertised tool with its input properties, "__sleep__" pauses for args.secs seconds, and
"__time__" prints a monotonic timestamp, to time the calls between two of them.

A call may carry "save": {"name": "field"}, which stores that field of the reply's first text block, read as
JSON, under the name. A later string argument holding "${name}" gets the stored value. Saved values persist in
vars.json in the current directory, so a later run, with a new server process, can use them.

A call may also carry "expect", which --check enforces on its printed reply. Its "error" (true or false; false
when not given) says whether the reply is a tool error (isError); each text in "contains" (a text or a list) must
appear in the reply, and none in "absent"; and the Python regex "matches", with DOTALL, must match somewhere in it.
With --check, every tool call is checked, with or without "expect", so a call that should be refused says
"error": true. A failed check prints a line starting "CHECK FAILED", and the run exits 1 after the last call.
On "__list__", "contains", "absent" and "matches" apply to the printed list.
"""

import base64
import json
import pathlib
import re
import subprocess
import sys
import time

MAX_TEXT = 500000
VARS = pathlib.Path("vars.json")


def substitute(value, saved: dict):
    """Replaces each ${name} in the strings of `value` with its saved value."""
    if isinstance(value, str):
        for name, text in saved.items():
            value = value.replace("${" + name + "}", str(text))
        return value
    if isinstance(value, list):
        return [substitute(v, saved) for v in value]
    if isinstance(value, dict):
        return {k: substitute(v, saved) for k, v in value.items()}
    return value


def save(result: dict, wanted: dict, saved: dict) -> None:
    """Stores the fields `wanted` names from the first text block of `result`."""
    texts = [item["text"] for item in result.get("content", []) if item.get("type") == "text"]
    reply = json.loads(texts[0]) if texts else {}
    for name, field in wanted.items():
        saved[name] = reply[field]
    VARS.write_text(json.dumps(saved))


def send(proc: subprocess.Popen, msg: dict) -> None:
    proc.stdin.write(json.dumps(msg) + "\n")
    proc.stdin.flush()


def receive(proc: subprocess.Popen, msg_id: int) -> dict:
    while True:
        line = proc.stdout.readline()
        if not line:
            raise RuntimeError("server closed stdout")
        msg = json.loads(line)
        if msg.get("id") == msg_id:
            return msg


def show(result: dict) -> str:
    parts = []
    for item in result.get("content", []):
        if item.get("type") == "text":
            parts.append(item["text"][:MAX_TEXT])
        elif item.get("type") == "image":
            path = f"shot-{len(list(pathlib.Path('.').glob('shot-*.png')))}.png"
            pathlib.Path(path).write_bytes(base64.b64decode(item["data"]))
            parts.append(f"<image saved to {path}>")
        else:
            parts.append(f"<{item.get('type')}>")
    flag = " [isError]" if result.get("isError") else ""
    return "\n".join(parts) + flag


def texts(value) -> list:
    return [value] if isinstance(value, str) else list(value)


def check(expect: dict, body: str, is_error: bool | None) -> list:
    """The ways `body` misses `expect`; `is_error` is None for a reply that is not a tool result."""
    problems = []
    if is_error is not None and is_error != expect.get("error", False):
        problems.append(f"isError is {is_error}, expected {expect.get('error', False)}")
    for text in texts(expect.get("contains", [])):
        if text not in body:
            problems.append(f"missing {text!r}")
    for text in texts(expect.get("absent", [])):
        if text in body:
            problems.append(f"unexpected {text!r}")
    if "matches" in expect and not re.search(expect["matches"], body, re.S):
        problems.append(f"no match for /{expect['matches']}/")
    return problems


def main() -> int:
    checking = "--check" in sys.argv[: sys.argv.index("--")]
    args = [a for a in sys.argv[1:] if a != "--check"]
    sep = args.index("--")
    calls = json.load(open(args[0]))
    proc = subprocess.Popen(args[sep + 1 :], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    failures = 0

    def verify(n: int, call: dict, body: str, is_error: bool | None) -> None:
        nonlocal failures
        if not checking:
            return
        for problem in check(call.get("expect", {}), body, is_error):
            failures += 1
            print(f"CHECK FAILED: call {n} ({call['tool']}): {problem}")
    send(proc, {"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "trial-driver", "version": "0"}}})
    receive(proc, 0)
    send(proc, {"jsonrpc": "2.0", "method": "notifications/initialized"})
    saved = json.loads(VARS.read_text()) if VARS.exists() else {}
    for n, call in enumerate(calls, start=1):
        call = {**call, "args": substitute(call.get("args", {}), saved)}
        if call["tool"] == "__sleep__":
            time.sleep(call["args"]["secs"])
            continue
        if call["tool"] == "__time__":
            print(f"@ {time.monotonic():.1f}s")
            continue
        if call["tool"] == "__list__":
            send(proc, {"jsonrpc": "2.0", "id": n, "method": "tools/list"})
            lines = []
            for tool in receive(proc, n)["result"]["tools"]:
                props = sorted(tool.get("inputSchema", {}).get("properties", {}))
                lines.append(f"- {tool['name']}({', '.join(props)})")
            print("\n".join(lines))
            verify(n, call, "\n".join(lines), None)
            continue
        send(proc, {"jsonrpc": "2.0", "id": n, "method": "tools/call",
                    "params": {"name": call["tool"], "arguments": call.get("args", {})}})
        reply = receive(proc, n)
        body = show(reply["result"]) if "result" in reply else f"RPC error: {reply.get('error')}"
        print(f"### {call['tool']} {json.dumps(call.get('args', {}))}\n{body}\n")
        if "result" in reply:
            verify(n, call, body, bool(reply["result"].get("isError")))
        elif checking:
            failures += 1
            print(f"CHECK FAILED: call {n} ({call['tool']}): {body}")
        if "save" in call and "result" in reply and not reply["result"].get("isError"):
            save(reply["result"], call["save"], saved)
    proc.stdin.close()
    proc.wait(timeout=30)
    if checking:
        verdict = "FAILED" if failures else "PASSED"
        print(f"CHECK {verdict}: {args[0]}, {failures} failed check(s)")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
