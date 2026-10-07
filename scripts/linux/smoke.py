#!/usr/bin/env python3
"""Drives xcalc on a private Xvfb display through the MCP server."""
import base64, json, subprocess, sys

exe = sys.argv[1] if len(sys.argv) > 1 else "target/debug/opencomputeruse"
p = subprocess.Popen([exe, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
n = 0
def call(method, params):
    global n
    n += 1
    p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": n, "method": method, "params": params}) + "\n")
    p.stdin.flush()
    return json.loads(p.stdout.readline())["result"]
def tool(name, **args):
    r = call("tools/call", {"name": name, "arguments": args})
    text = "\n".join(c["text"] for c in r["content"] if c["type"] == "text")
    for c in r["content"]:
        if c["type"] == "image":
            path = f"/tmp/{name}-{n}.png"
            open(path, "wb").write(base64.b64decode(c["data"]))
            text += f"\n[image {path}, {len(c['data'])} b64 chars]"
    print(f"--- {name}{' ERROR' if r['isError'] else ''}\n{text[:800]}")
    return text
call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}})
tool("permissions")
s = json.loads(tool("start_session", app="xcalc"))["session_id"]
tool("type_text", session_id=s, text="12*12=")
tool("screenshot", session_id=s)
tool("get_ui_tree", session_id=s)
tool("end_session", session_id=s)
