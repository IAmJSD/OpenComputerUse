#!/usr/bin/env python3
"""Drives xcalc on a private Xvfb display through the MCP server, and watches
it through the live view, then reads Chromium's and GTK's accessibility
trees (a11y_checks.py)."""
import base64, json, os, struct, subprocess, sys

from a11y_checks import check

exe = sys.argv[1] if len(sys.argv) > 1 else "target/debug/opencomputeruse"
TOKEN = "smoke-test-token-0123456789"
p = subprocess.Popen([exe, "mcp", "--view-listen", "127.0.0.1:7912", "--view-token", TOKEN], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
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
    if r["isError"]:
        raise SystemExit(f"{name} failed: {text}")
    return text
call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}})
tool("permissions")
s = json.loads(tool("start_session", app="xcalc"))["session_id"]
tool("type_text", session_id=s, text="12*12=")
tool("screenshot", session_id=s)
try:
    import websocket
except ImportError:
    websocket = None
if websocket:
    ws = websocket.create_connection(f"ws://127.0.0.1:7912/sessions/{s}/view?token={TOKEN}", timeout=20)
    assert json.loads(ws.recv())["type"] == "hello"
    frame = ws.recv()
    assert struct.unpack("<BBH", frame[:4]) == (1, 1, 1) and frame[20:22] == b"\xff\xd8"
    ws.send(json.dumps({"type": "key", "action": "down", "code": "Escape", "key": "Escape"}))
    ws.send(json.dumps({"type": "key", "action": "up", "code": "Escape", "key": "Escape"}))
    ws.send(json.dumps({"type": "text", "text": "7"}))
    ws.settimeout(5)
    assert isinstance(ws.recv(), bytes), "no picture after typing through the view"
    ws.close()
    print("--- live view: a keyframe, and a picture after typing")
tool("end_session", session_id=s)
browser = os.environ.get("OCU_SMOKE_BROWSER") or next(
    b for b in ["chromium", "google-chrome", "chromium-browser"]
    if subprocess.run(["which", b], capture_output=True).returncode == 0)
check(tool, "x11", browser)
print("all good")
