#!/usr/bin/env python3
"""Drives the MCP server over stdio the way a client would: start TextEdit,
look, type, press a toolbar button by element id, end the session.

    python3 scripts/mcp_smoke.py [path/to/opencomputeruse] [app]
"""
import base64, json, subprocess, sys, time

exe = sys.argv[1] if len(sys.argv) > 1 else "target/debug/opencomputeruse"
app = sys.argv[2] if len(sys.argv) > 2 else "TextEdit"
proc = subprocess.Popen([exe, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
next_id = 0

def call(method, params=None):
    global next_id
    next_id += 1
    proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}}) + "\n")
    proc.stdin.flush()
    reply = json.loads(proc.stdout.readline())
    if "error" in reply:
        raise SystemExit(f"{method}: {reply['error']}")
    return reply["result"]

def tool(name, **args):
    t = time.time()
    r = call("tools/call", {"name": name, "arguments": args})
    texts = [c["text"] for c in r["content"] if c["type"] == "text"]
    images = [c for c in r["content"] if c["type"] == "image"]
    for i, img in enumerate(images):
        path = f"/tmp/ocu-mcp-{name}-{next_id}.png"
        open(path, "wb").write(base64.b64decode(img["data"]))
        texts.append(f"[image → {path}]")
    print(f"--- {name} ({time.time() - t:.2f}s){' ERROR' if r.get('isError') else ''}")
    print("\n".join(texts)[:1500])
    return r, "\n".join(texts)

print(call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "smoke", "version": "0"}})["serverInfo"])
proc.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
print(len(call("tools/list")["tools"]), "tools")
tool("permissions")
_, text = tool("start_session", app=app, new_instance=True)
session = json.loads(text)["session_id"]
tool("type_text", session_id=session, text="Typed by opencomputeruse in the background.\n")
_, tree = tool("get_ui_tree", session_id=session)
bold = next((l.split("]")[0].strip(" [") for l in tree.splitlines() if 'desc="align centre"' in l), None)
if bold:
    tool("click", session_id=session, element=bold, ui_tree=False)
tool("press_key", session_id=session, keys="cmd+a", screenshot=False)
tool("list_sessions")
time.sleep(1)
tool("end_session", session_id=session)
proc.stdin.close()
proc.wait(timeout=10)
