#!/usr/bin/env python3
"""Drives Chromium on a Wayland session through the MCP server and the live
view, checking each step through the page's title (which the window title
follows): clicks, typing beyond a US keyboard, keys, scrolling, a view
frame and a person's input, and the sessions API.

    python3 scripts/linux/wayland_smoke.py [target/debug/opencomputeruse]
"""
import base64, json, os, socket, struct, subprocess, sys, time, urllib.request

import websocket  # python3-websocket

exe = sys.argv[1] if len(sys.argv) > 1 else "target/debug/opencomputeruse"
browser = os.environ.get("OCU_SMOKE_BROWSER") or next(
    b for b in ["chromium", "google-chrome", "chromium-browser"]
    if subprocess.run(["which", b], capture_output=True).returncode == 0)
TOKEN = "smoke-test-token-0123456789"
PORT = 7911
BASE = f"http://127.0.0.1:{PORT}"
PAGE = """data:text/html,<html><body style="margin:0;height:5000px;background:%23fff">
<input id=i style="position:fixed;left:20px;top:150px;width:600px;height:100px;font-size:20px">
<script>
const t = s => document.title = s;
document.body.onmousedown = e => { if (e.target.id != 'i') t('click:' + e.button) };
i.oninput = () => t('v:' + i.value);
onscroll = () => t('scroll:' + Math.round(scrollY));
onkeydown = e => { if (e.key == 'F2') t('key:F2') };
</script></body></html>"""

p = subprocess.Popen([exe, "mcp", "--view-listen", f"127.0.0.1:{PORT}", "--view-token", TOKEN],
                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
n = 0
failures = []


def call(method, params):
    global n
    n += 1
    p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": n, "method": method, "params": params}) + "\n")
    p.stdin.flush()
    return json.loads(p.stdout.readline())["result"]


def tool(name, **args):
    r = call("tools/call", {"name": name, "arguments": args})
    text = "\n".join(c["text"] for c in r["content"] if c["type"] == "text")
    images = [c for c in r["content"] if c["type"] == "image"]
    if r["isError"] or name != "list_windows":
        print(f"--- {name}{' ERROR' if r['isError'] else ''}: {text[:200]}")
    if r["isError"]:
        raise SystemExit(f"{name} failed: {text}")
    return text, images


titles = []


def title(s):
    t = json.loads(tool("list_windows", session_id=s)[0])[0]["title"]
    titles.append(t)
    return t


def expect(what, cond, wait=5.0):
    """Waits for cond() (a value), recording a failure if it never comes."""
    deadline = time.time() + wait
    value = None
    while time.time() < deadline:
        value = cond()
        if value:
            print(f"ok: {what}")
            return value
        time.sleep(0.3)
    print(f"FAILED: {what} (last: {value!r}, title: {titles[-1] if titles else None!r})")
    failures.append(what)


def api(method, path, body=None):
    req = urllib.request.Request(BASE + path, method=method,
                                 data=json.dumps(body).encode() if body is not None else None,
                                 headers={"Authorization": f"Bearer {TOKEN}",
                                          "Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=60) as r:
        return r.status, r.read()


call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}})
out, _ = tool("start_session", app=browser, display="wayland",
              # No sandbox: CI's kernels may not allow Chrome's.
              args=["--user-data-dir=/tmp/ocu-smoke-profile", "--no-first-run", "--no-sandbox",
                    "--no-default-browser-check", "--app=" + PAGE])
started = json.loads(out)
s = started["session_id"]
assert started["session"]["backend"] == "linux-wayland", started
assert started.get("view_path") == f"/sessions/{s}/view", started
expect("a window", lambda: json.loads(tool("list_windows", session_id=s)[0]), wait=20)

_, images = tool("screenshot", session_id=s)
png = base64.b64decode(images[0]["data"])
assert png[:4] == b"\x89PNG"
open("/tmp/wayland-1.png", "wb").write(png)

tool("click", session_id=s, x=600, y=400, screenshot=False)
expect("a click reaches the page", lambda: title(s).startswith("click:0") and title(s))
tool("click", session_id=s, x=600, y=400, button="right", screenshot=False)
expect("a right click", lambda: title(s).startswith("click:2") and title(s))
tool("press_key", session_id=s, keys="escape", screenshot=False)

tool("click", session_id=s, x=300, y=260, screenshot=False)
tool("type_text", session_id=s, text="Hi there, ünïcødé €5 ✓", screenshot=False)
expect("typing, US and not", lambda: "Hi there, ünïcødé €5 ✓" in title(s) and title(s))
tool("press_key", session_id=s, keys="ctrl+a backspace", screenshot=False)
tool("type_text", session_id=s, text="again", screenshot=False)
expect("shortcuts and keys", lambda: title(s).startswith("v:again") and title(s))
tool("press_key", session_id=s, keys="f2", screenshot=False)
expect("a function key", lambda: title(s).startswith("key:F2") and title(s))

tool("scroll", session_id=s, x=600, y=400, dy=400, screenshot=False)
expect("scrolling", lambda: title(s).startswith("scroll:") and title(s))

# The live view: a picture, then a person's input.
ws = websocket.create_connection(f"ws://127.0.0.1:{PORT}/sessions/{s}/view?token={TOKEN}", timeout=20)
hello = json.loads(ws.recv())
assert hello["type"] == "hello" and hello["window"], hello
frame = ws.recv()
assert isinstance(frame, bytes), frame
kind, fmt, flags, wid, x, y, w, h, ww, wh = struct.unpack("<BBHIHHHHHH", frame[:20])
assert (kind, fmt, flags & 1) == (1, 1, 1) and frame[20:22] == b"\xff\xd8", frame[:24]
assert (w, h) == (ww, wh) and wid == hello["window"]
print(f"ok: a keyframe from the view ({w}x{h}, {len(frame)} bytes)")

ws.send(json.dumps({"type": "pointer", "action": "down", "x": 300, "y": 260, "button": 0}))
ws.send(json.dumps({"type": "pointer", "action": "up", "x": 300, "y": 260, "button": 0}))
for ch, code in [("x", "KeyX"), ("Y", "KeyY")]:
    if ch.isupper():
        ws.send(json.dumps({"type": "key", "action": "down", "code": "ShiftLeft", "key": "Shift"}))
    ws.send(json.dumps({"type": "key", "action": "down", "code": code, "key": ch}))
    ws.send(json.dumps({"type": "key", "action": "up", "code": code, "key": ch}))
    if ch.isupper():
        ws.send(json.dumps({"type": "key", "action": "up", "code": "ShiftLeft", "key": "Shift"}))
ws.send(json.dumps({"type": "text", "text": "ß"}))
expect("a person types through the view", lambda: title(s).endswith("xYß") and title(s))
ws.send(json.dumps({"type": "key", "action": "down", "code": "Backspace", "key": "Backspace"}))
ws.send(json.dumps({"type": "key", "action": "up", "code": "Backspace", "key": "Backspace"}))
expect("and presses keys", lambda: title(s).endswith("xY") and title(s))
ws.settimeout(5)
deltas = 0
try:
    while deltas == 0:
        m = ws.recv()
        if isinstance(m, bytes) and not (m[2] & 1):
            deltas += 1
except Exception:
    pass
if deltas:
    print("ok: partial frames follow changes")
else:
    print("FAILED: no partial frames")
    failures.append("partial frames")
ws.send(json.dumps({"type": "resize", "width": 1024, "height": 700}))
expect("a resize", lambda: [w for w in json.loads(tool("list_windows", session_id=s)[0])
                            if w["frame"]["width"] == 1024], wait=10)
ws.close()

# The sessions API, and its sessions are the agent's too.
status, body = api("GET", "/sessions")
listed = json.loads(body)["sessions"]
assert status == 200 and any(x["session_id"] == s for x in listed), listed
status, body = api("GET", f"/sessions/{s}/screenshot")
assert status == 200 and body[:4] == b"\x89PNG"
status, body = api("POST", "/sessions", {"app": "foot", "display": "wayland"})
other = json.loads(body)
assert status == 201 and other["view_path"].endswith("/view"), other
o = other["session_id"]
expect("a session the API started", lambda: other["windows"] or json.loads(tool("list_windows", session_id=o)[0]), wait=15)
tool("type_text", session_id=o, text="echo hi\n", screenshot=False)
status, _ = api("DELETE", f"/sessions/{o}")
assert status == 200
print("ok: the sessions API")
try:
    api("GET", "/sessions/nope/view")
except urllib.error.HTTPError as e:
    assert e.code == 404
bad = urllib.request.Request(BASE + "/sessions")
try:
    urllib.request.urlopen(bad)
    failures.append("no token refused")
except urllib.error.HTTPError as e:
    assert e.code == 401

tool("end_session", session_id=s)
p.stdin.close()
p.wait(timeout=20)
if failures:
    raise SystemExit(f"failed: {', '.join(failures)}")
print("all good")
