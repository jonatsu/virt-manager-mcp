"""Drive an MCP stdio server through a scripted list of tool calls.

Usage: python3 -I mcp_call.py <calls.json> -- <server command...>
calls.json is a list of {"tool": name, "args": {...}}. Special tools: "__list__" prints every
advertised tool with its input properties, "__sleep__" pauses for args.secs seconds, and
"__time__" prints a monotonic timestamp, to time the calls between two of them.
"""

import base64
import json
import pathlib
import subprocess
import sys
import time

MAX_TEXT = 500000


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


def main() -> int:
    sep = sys.argv.index("--")
    calls = json.load(open(sys.argv[1]))
    proc = subprocess.Popen(sys.argv[sep + 1 :], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    send(proc, {"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "trial-driver", "version": "0"}}})
    receive(proc, 0)
    send(proc, {"jsonrpc": "2.0", "method": "notifications/initialized"})
    for n, call in enumerate(calls, start=1):
        if call["tool"] == "__sleep__":
            time.sleep(call["args"]["secs"])
            continue
        if call["tool"] == "__time__":
            print(f"@ {time.monotonic():.1f}s")
            continue
        if call["tool"] == "__list__":
            send(proc, {"jsonrpc": "2.0", "id": n, "method": "tools/list"})
            for tool in receive(proc, n)["result"]["tools"]:
                props = sorted(tool.get("inputSchema", {}).get("properties", {}))
                print(f"- {tool['name']}({', '.join(props)})")
            continue
        send(proc, {"jsonrpc": "2.0", "id": n, "method": "tools/call",
                    "params": {"name": call["tool"], "arguments": call.get("args", {})}})
        reply = receive(proc, n)
        body = show(reply["result"]) if "result" in reply else f"RPC error: {reply.get('error')}"
        print(f"### {call['tool']} {json.dumps(call.get('args', {}))}\n{body}\n")
    proc.stdin.close()
    proc.wait(timeout=30)
    return 0


if __name__ == "__main__":
    sys.exit(main())
