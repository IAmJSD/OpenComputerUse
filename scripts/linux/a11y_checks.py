"""The accessibility checks both Linux smoke tests run: a Chromium page and a
GTK window, found in the tree by name and driven by element id. Pressing an
element uses its own action, and a double click lands at its centre, which
checks that frames match the screenshot's coordinates.

`tool(name, **args)` calls an MCP tool and returns its text, raising if
the call fails.
"""
import json, os, re, time

HERE = os.path.dirname(os.path.abspath(__file__))
PAGE = ("data:text/html,<title>A11y</title>"
        "<button id=b onclick=\"n++;document.title='pressed:'+n+':'+q.value\">Go</button>"
        "<input id=q aria-label=Query><script>n=0</script>")


def wait_for(what, cond, wait=30.0):
    deadline = time.time() + wait
    while True:
        value = cond()
        if value:
            print(f"ok: {what}")
            return value
        if time.time() > deadline:
            raise SystemExit(f"FAILED: {what} (last: {value!r})")
        time.sleep(0.5)


def element(tree, role, name):
    m = re.search(rf'\[(e\d+)\] {role} "{re.escape(name)}"', tree)
    return m and m.group(1)


def title(tool, s):
    return json.loads(tool("list_windows", session_id=s))[0]["title"]


def tree_with(tool, s, role, name):
    """Reads the tree until it has the element, which an app exports a
    moment after its window appears. Returns the tree and the element."""
    last = {}

    def read():
        try:
            last["tree"] = tool("get_ui_tree", session_id=s)
        except SystemExit as e:
            last["tree"] = str(e)
            return None
        return element(last["tree"], role, name)

    deadline = time.time() + 40
    while time.time() < deadline:
        if read():
            return last["tree"], element(last["tree"], role, name)
        time.sleep(1)
    raise SystemExit(f"FAILED: no {role} \"{name}\" in the tree:\n{last.get('tree')}")


def check(tool, display, browser):
    # Chromium: its web content, not just its own controls.
    out = tool("start_session", app=browser, display=display,
               args=[f"--user-data-dir=/tmp/ocu-a11y-{display}", "--no-first-run", "--no-sandbox",
                     "--no-default-browser-check", "--app=" + PAGE])
    s = json.loads(out)["session_id"]
    tree, button = tree_with(tool, s, "Button", "Go")
    field = element(tree, "TextField", "Query")
    assert field, tree
    tool("set_value", session_id=s, element=field, value="hello", screenshot=False)
    wait_for(f"{display}: set_value on a web page's field",
             lambda: 'value="hello"' in tool("get_ui_tree", session_id=s))
    _, button = tree_with(tool, s, "Button", "Go")
    tool("click", session_id=s, element=button, screenshot=False)
    wait_for(f"{display}: a click by element id", lambda: title(tool, s) == "pressed:1:hello")
    tool("click", session_id=s, element=button, count=2, screenshot=False)
    wait_for(f"{display}: clicks at an element's centre", lambda: title(tool, s) == "pressed:3:hello")
    small = tool("get_ui_tree", session_id=s, max_nodes=3)
    assert 0 < small.count("[e") <= 3, small
    print(f"ok: {display}: max_nodes")
    shot = tool("screenshot", session_id=s, ui_tree=True)
    assert 'Button "Go"' in shot, shot
    print(f"ok: {display}: a screenshot with its tree")
    tool("end_session", session_id=s)

    # GTK: pressing Greet puts the field's text in the title.
    for gtk in ("3", "4"):
        out = tool("start_session", app="python3", display=display,
                   args=[os.path.join(HERE, "a11y_app.py"), gtk])
        s = json.loads(out)["session_id"]
        tree, greet = tree_with(tool, s, "Button", "Greet")
        name = element(tree, "TextField", "Name")
        assert name, tree
        tool("set_value", session_id=s, element=name, value="Ada", screenshot=False)
        tool("element_action", session_id=s, element=greet, action="press", screenshot=False)
        wait_for(f"{display}: GTK {gtk}: set_value and press", lambda: title(tool, s) == "greeted:Ada")
        tree, greet = tree_with(tool, s, "Button", "Greet")
        tool("set_value", session_id=s, element=element(tree, "TextField", "Name"), value="Bo",
             screenshot=False)
        tool("click", session_id=s, element=greet, count=2, screenshot=False)
        wait_for(f"{display}: GTK {gtk}: clicks at an element's centre",
                 lambda: title(tool, s) == "greeted:Bo")
        tool("end_session", session_id=s)
