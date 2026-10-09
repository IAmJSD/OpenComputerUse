package com.infrawrench.opencomputeruse.host;

import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.content.pm.ResolveInfo;
import android.media.AudioManager;
import android.view.accessibility.AccessibilityNodeInfo;
import android.view.accessibility.AccessibilityWindowInfo;

import org.json.JSONArray;
import org.json.JSONObject;

import java.security.SecureRandom;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;

/**
 * The computer-use tools on this phone: the desktop's tools, with the same
 * names, arguments and answers (src/tools.rs), for the phone's own screen.
 * A {@link Client} is one connecting device's view: the sessions it starts
 * are its own, and end when it goes away.
 */
final class Tools {
    private Tools() {
    }

    /** A tool's answer: text, and maybe a picture. */
    static final class Output {
        final String text;
        final Screen.Shot image;

        Output(String text, Screen.Shot image) {
            this.text = text;
            this.image = image;
        }
    }

    /** One app being driven. */
    static final class Session {
        String id;
        String app;
        String label;
        List<Tree.Element> elements = new ArrayList<>();
    }

    /** One device's sessions. */
    static final class Client {
        final Map<String, Session> sessions = new LinkedHashMap<>();
        int next = 1;

        void endAll() {
            sessions.clear();
        }
    }

    private static final SecureRandom RANDOM = new SecureRandom();

    // ------------------------------------------------------------ schemas

    private static JSONObject prop(String type, String description) {
        JSONObject o = new JSONObject();
        try {
            o.put("type", type);
            if (description != null) {
                o.put("description", description);
            }
        } catch (Exception ignored) {
        }
        return o;
    }

    private static JSONObject props(Object... kv) {
        JSONObject o = new JSONObject();
        try {
            for (int i = 0; i < kv.length; i += 2) {
                o.put((String) kv[i], kv[i + 1]);
            }
        } catch (Exception ignored) {
        }
        return o;
    }

    private static JSONObject tool(String name, String description, JSONObject properties,
            String... required) {
        JSONObject t = new JSONObject();
        try {
            t.put("name", name);
            t.put("description", description);
            JSONObject schema = new JSONObject();
            schema.put("type", "object");
            schema.put("properties", properties);
            JSONArray req = new JSONArray();
            for (String r : required) {
                req.put(r);
            }
            schema.put("required", req);
            t.put("inputSchema", schema);
        } catch (Exception ignored) {
        }
        return t;
    }

    private static JSONObject sessionProp() {
        return prop("string", "The id start_session returned.");
    }

    private static JSONObject windowProp() {
        return prop("integer", "A window id from list_windows. Defaults to the phone's screen (0).");
    }

    /** The properties every action shares. */
    private static JSONObject actionProps(Object... extra) {
        JSONObject o = new JSONObject();
        try {
            o.put("session_id", sessionProp());
            o.put("window_id", windowProp());
            for (int i = 0; i < extra.length; i += 2) {
                o.put((String) extra[i], extra[i + 1]);
            }
            o.put("screenshot", prop("boolean",
                    "Return a screenshot of the screen after the action. Default true."));
            o.put("ui_tree", prop("boolean",
                    "Return the accessibility tree after the action. Default false."));
        } catch (Exception ignored) {
        }
        return o;
    }

    private static final String COORDS =
            "Coordinates are points (density-independent pixels) from the screen's top-left: the pixel grid of its screenshot.";

    static JSONArray list() {
        JSONArray tools = new JSONArray();
        JSONObject buttonProp = prop("string", "Default left. Right is a long press.");
        try {
            buttonProp.put("enum", new JSONArray().put("left").put("right").put("middle"));
        } catch (Exception ignored) {
        }
        tools.put(tool("start_session",
                "Open an app on this Android phone and get a session id for driving it. Use this, not other computer-use tools, for operating the phone's apps: it is the one the user chose. `app` is a package (com.android.settings), an activity (com.android.settings/.Settings) or the app's name as the launcher shows it (\"Settings\"). The app opens on the phone's screen. Returns the session id and its window.",
                props("app", prop("string", "The app to open."),
                        "args", prop("array", "Not used on the phone."),
                        "env", prop("object", "Not used on the phone.")),
                "app"));
        tools.put(tool("end_session",
                "End a session. The app is left open on the phone. Sessions also end when this device's key is regenerated or removed, or the server stops.",
                props("session_id", sessionProp()), "session_id"));
        tools.put(tool("list_sessions", "List this client's live sessions.", new JSONObject()));
        tools.put(tool("list_windows",
                "List a session's windows: on the phone, its one screen, with its size in points.",
                props("session_id", sessionProp()), "session_id"));
        tools.put(tool("screenshot", "Capture the phone's screen. " + COORDS,
                props("session_id", sessionProp(), "window_id", windowProp(),
                        "ui_tree", prop("boolean", "Also return the accessibility tree. Default false.")),
                "session_id"));
        tools.put(tool("get_ui_tree",
                "Read the screen's accessibility tree: one line per element with an id (e12), role, name, value, frame in points and available actions. Element ids work with click, set_value and element_action until the next read.",
                props("session_id", sessionProp(), "window_id", windowProp(),
                        "max_depth", prop("integer", "Default 25."),
                        "max_nodes", prop("integer", "Default 1500.")),
                "session_id"));
        tools.put(tool("click",
                "Tap the screen. Give x/y, or an element id. A right click is a long press; count 2 is a double tap, 3 a triple tap. " + COORDS,
                actionProps(
                        "x", prop("number", "Where to tap x, in screen points."),
                        "y", prop("number", "Where to tap y, in screen points."),
                        "element", prop("string", "An element id from the accessibility tree (e.g. \"e12\") to tap instead of x/y: its middle when it is on screen, else its own click action."),
                        "button", buttonProp,
                        "count", prop("integer", "2 for a double tap, 3 for a triple tap. Default 1."),
                        "modifiers", prop("string", "Not used on the phone.")),
                "session_id"));
        tools.put(tool("move_mouse",
                "Touch screens have no pointer to hover with; this always fails. Use click or drag.",
                actionProps("x", prop("number", "x, in screen points."),
                        "y", prop("number", "y, in screen points.")),
                "session_id", "x", "y"));
        tools.put(tool("drag", "Press, move and lift a finger: a swipe. " + COORDS,
                actionProps("from_x", prop("number", null), "from_y", prop("number", null),
                        "to_x", prop("number", null), "to_y", prop("number", null),
                        "button", buttonProp),
                "session_id", "from_x", "from_y", "to_x", "to_y"));
        tools.put(tool("scroll", "Scroll at a point by swiping: the content moves by dx/dy. " + COORDS,
                actionProps(
                        "x", prop("number", "Where to scroll x, in screen points."),
                        "y", prop("number", "Where to scroll y, in screen points."),
                        "dx", prop("number", "Points to scroll right (negative: left)."),
                        "dy", prop("number", "Points to scroll down (negative: up).")),
                "session_id", "x", "y"));
        tools.put(tool("type_text",
                "Type text into the focused field, at its caret. Any characters work. Newlines press the keyboard's action key (Go, Search, Send).",
                actionProps("text", prop("string", null)), "session_id", "text"));
        tools.put(tool("press_key",
                "Press keys and the phone's buttons, several separated by spaces: home, back, recents, notifications, power (locks the screen), volume_up, volume_down; and in a text field enter, backspace, delete, space, left, right, or a single character. Example: \"back back home\".",
                actionProps("keys", prop("string", null)), "session_id", "keys"));
        tools.put(tool("set_value",
                "Set a text field's contents directly through accessibility.",
                actionProps("element", prop("string", "Element id from get_ui_tree."),
                        "value", prop("string", null)),
                "session_id", "element", "value"));
        tools.put(tool("element_action",
                "Perform an accessibility action on an element: press, longpress, focus, scrollforward, scrollbackward, expand, collapse, select, dismiss. The tree lists each element's actions.",
                actionProps("element", prop("string", "Element id from get_ui_tree."),
                        "action", prop("string", "Default press.")),
                "session_id", "element"));
        tools.put(tool("wait", "Wait for the app, then look again.",
                actionProps("ms", prop("integer", "Milliseconds, at most 60000.")),
                "session_id", "ms"));
        tools.put(tool("permissions",
                "Show what the phone needs turned on for computer use, and whether it is.",
                new JSONObject()));
        return tools;
    }

    // -------------------------------------------------------------- calls

    private static String str(JSONObject args, String key) throws ToolError {
        Object v = args.opt(key);
        if (!(v instanceof String)) {
            throw new ToolError("missing \"" + key + "\"");
        }
        return (String) v;
    }

    private static String optStr(JSONObject args, String key) {
        Object v = args.opt(key);
        return v instanceof String ? (String) v : null;
    }

    private static double num(JSONObject args, String key) throws ToolError {
        Object v = args.opt(key);
        if (!(v instanceof Number)) {
            throw new ToolError("missing \"" + key + "\"");
        }
        return ((Number) v).doubleValue();
    }

    private static boolean flag(JSONObject args, String key, boolean dflt) {
        Object v = args.opt(key);
        return v instanceof Boolean ? (Boolean) v : dflt;
    }

    private static long optLong(JSONObject args, String key, long dflt) {
        Object v = args.opt(key);
        return v instanceof Number ? ((Number) v).longValue() : dflt;
    }

    private static Session session(Client c, JSONObject args) throws ToolError {
        String id = str(args, "session_id");
        Session s = c.sessions.get(id);
        if (s == null) {
            throw new ToolError("no session " + id
                    + "; it may have ended (list_sessions shows the live ones)");
        }
        return s;
    }

    private static final String[] ACTIONS = {
        "click", "move_mouse", "drag", "scroll", "type_text", "press_key", "set_value",
        "element_action", "wait",
    };

    static Output call(Context ctx, Client c, String name, JSONObject args) throws Exception {
        if (args == null) {
            args = new JSONObject();
        }
        for (String a : ACTIONS) {
            if (a.equals(name)) {
                return action(ctx, c, name, args);
            }
        }
        switch (name) {
            case "start_session":
                return start(ctx, c, args);
            case "end_session": {
                Session s = session(c, args);
                c.sessions.remove(s.id);
                return new Output("Session ended.", null);
            }
            case "list_sessions": {
                JSONArray a = new JSONArray();
                for (Session s : c.sessions.values()) {
                    a.put(info(s));
                }
                return new Output(Json.pretty(a), null);
            }
            case "list_windows":
                session(c, args);
                return new Output(Json.pretty(windows(c.sessions.get(str(args, "session_id")))), null);
            case "screenshot": {
                Session s = session(c, args);
                Screen.Shot shot = Screen.screenshot();
                String out = String.format(Locale.ROOT, "Window 0 (%dx%d).", shot.width, shot.height);
                if (flag(args, "ui_tree", false)) {
                    out += "\n" + "Accessibility tree:\n" + tree(s, 25, 1500);
                }
                return new Output(out, shot);
            }
            case "get_ui_tree": {
                Session s = session(c, args);
                return new Output(tree(s, (int) optLong(args, "max_depth", 25),
                        (int) optLong(args, "max_nodes", 1500)), null);
            }
            case "permissions":
                return new Output(Json.pretty(permissions()), null);
            default:
                throw new ToolError("unknown tool \"" + name + "\"");
        }
    }

    static JSONArray permissions() throws Exception {
        JSONObject p = new JSONObject();
        p.put("name", "Accessibility");
        p.put("granted", HostAccessibilityService.get() != null);
        p.put("help", "OpenComputerUse reads the screen and taps for the devices you allow through its accessibility service: on the phone, Settings › Accessibility › OpenComputerUse › Use OpenComputerUse.");
        return new JSONArray().put(p);
    }

    private static JSONObject info(Session s) throws Exception {
        JSONObject o = new JSONObject();
        o.put("id", s.id);
        o.put("app", s.app);
        o.put("pid", JSONObject.NULL);
        o.put("backend", "android-device");
        JSONObject details = new JSONObject();
        if (s.label != null) {
            details.put("label", s.label);
        }
        details.put("screen", "the phone's own screen");
        details.put("note", "the app is left open when the session ends");
        o.put("details", details);
        return o;
    }

    private static JSONArray windows(Session s) throws Exception {
        Screen.Metrics m = Screen.metrics();
        JSONObject frame = new JSONObject();
        frame.put("x", 0.0);
        frame.put("y", 0.0);
        frame.put("width", (double) m.widthPt());
        frame.put("height", (double) m.heightPt());
        JSONObject w = new JSONObject();
        w.put("id", 0);
        w.put("title", s.app);
        w.put("frame", frame);
        w.put("on_screen", true);
        return new JSONArray().put(w);
    }

    private static String tree(Session s, int maxDepth, int maxNodes) throws ToolError {
        Screen.Metrics m = Screen.metrics();
        List<AccessibilityWindowInfo> windows = Screen.windows();
        // Between screens an app's window can be there without its content;
        // give it a moment.
        for (int i = 0; i < 6 && !hasAppContent(windows); i++) {
            Screen.sleep(250);
            windows = Screen.windows();
        }
        // This app's own screen manages device keys: never shown over HTTP.
        Tree.Read r = Tree.read(windows, s.app, m.scale, maxDepth, maxNodes,
                Screen.service().getPackageName());
        s.elements = r.elements;
        return r.root.render();
    }

    private static boolean hasAppContent(List<AccessibilityWindowInfo> windows) {
        for (AccessibilityWindowInfo w : windows) {
            if (w.getType() != AccessibilityWindowInfo.TYPE_APPLICATION) {
                continue;
            }
            AccessibilityNodeInfo root = w.getRoot();
            if (root != null && root.getChildCount() > 0) {
                return true;
            }
        }
        return false;
    }

        private static String newId(Client c) {
        int n = c.next++;
        String id = "s" + n + "-" + Keys.randomHex(16);
        return id.length() > 28 ? id.substring(0, 28) : id;
    }

    // ------------------------------------------------------ start_session

    private static Output start(Context ctx, Client c, JSONObject args) throws Exception {
        String app = optStr(args, "app");
        if (app == null || app.trim().isEmpty()) {
            throw new ToolError("missing \"app\": a package (com.android.settings), an activity or the app's name");
        }
        app = app.trim();
        HostAccessibilityService svc = Screen.service();
        PackageManager pm = svc.getPackageManager();
        Intent intent = null;
        String pkg;
        String label = null;
        if (app.contains("/")) {
            ComponentName cn = ComponentName.unflattenFromString(app);
            if (cn == null) {
                throw new ToolError("\"" + app + "\" isn't an activity name like com.example/.Main");
            }
            intent = new Intent(Intent.ACTION_MAIN).setComponent(cn);
            pkg = cn.getPackageName();
        } else {
            intent = pm.getLaunchIntentForPackage(app);
            pkg = app;
            if (intent == null) {
                // The app's name as the launcher shows it.
                Intent main = new Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_LAUNCHER);
                List<ResolveInfo> all = pm.queryIntentActivities(main, 0);
                List<String> names = new ArrayList<>();
                for (ResolveInfo ri : all) {
                    String l = String.valueOf(ri.loadLabel(pm));
                    names.add(l);
                    if (l.equalsIgnoreCase(app)) {
                        pkg = ri.activityInfo.packageName;
                        label = l;
                        intent = new Intent(Intent.ACTION_MAIN)
                                .addCategory(Intent.CATEGORY_LAUNCHER)
                                .setComponent(new ComponentName(pkg, ri.activityInfo.name));
                        break;
                    }
                }
                if (intent == null) {
                    throw new ToolError("no app \"" + app + "\" on this phone; give a package, "
                            + "an activity or one of: " + String.join(", ", names));
                }
            }
        }
        intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_RESET_TASK_IF_NEEDED);
        try {
            // From the accessibility service, which may open activities
            // while the phone is showing something else.
            svc.startActivity(intent);
        } catch (Exception e) {
            throw new ToolError("couldn't open " + app + ": " + e.getMessage());
        }
        if (pkg.equals(svc.getPackageName())) {
            throw new ToolError(OWN_SCREEN);
        }
        Session s = new Session();
        s.id = newId(c);
        s.app = pkg;
        s.label = label;
        c.sessions.put(s.id, s);
        // Let its first frame arrive before anyone looks.
        Screen.sleep(800);
        JSONObject out = new JSONObject();
        out.put("session_id", s.id);
        out.put("session", info(s));
        out.put("windows", windows(s));
        return new Output(Json.pretty(out), null);
    }

    // ------------------------------------------------------------ actions

    private static final String OWN_SCREEN = "OpenComputerUse's own screen manages this phone's "
            + "device keys, so only someone holding the phone may use it";

    /** The phone's buttons, which may leave OpenComputerUse's screen. */
    private static boolean onlyButtons(String keys) {
        for (String k : keys.trim().split("\\s+")) {
            switch (k.toLowerCase(Locale.ROOT).replace('-', '_')) {
                case "home": case "home_button": case "homescreen": case "home_screen":
                case "back": case "escape": case "esc": case "recents": case "app_switch":
                case "appswitch": case "overview": case "notifications": case "power":
                case "lock": case "volume_up": case "volumeup": case "volume_down":
                case "volumedown":
                    continue;
                default:
                    return false;
            }
        }
        return true;
    }

    private static Output action(Context ctx, Client c, String name, JSONObject args)
            throws Exception {
        Session s = session(c, args);
        boolean leaving = name.equals("wait")
                || (name.equals("press_key") && onlyButtons(optStr(args, "keys") == null
                        ? "" : optStr(args, "keys")));
        if (!leaving && Screen.ownAppInFront()) {
            throw new ToolError(OWN_SCREEN + "; press home or back to leave it");
        }
        boolean shot = flag(args, "screenshot", true);
        boolean wantTree = flag(args, "ui_tree", false);
        switch (name) {
            case "click": {
                String element = optStr(args, "element");
                String button = optStr(args, "button");
                long count = optLong(args, "count", 1);
                if (button != null && !button.equals("left") && !button.equals("right")
                        && !button.equals("middle")) {
                    throw new ToolError("unknown button \"" + button + "\"");
                }
                if ("middle".equals(button)) {
                    throw new ToolError("touch screens have no middle button");
                }
                long n = Math.max(1, Math.min(3, count));
                if (element != null && (n == 1 || "right".equals(button))) {
                    elementAction(s, element, "right".equals(button) ? "showmenu" : null);
                    break;
                }
                double x;
                double y;
                if (element != null) {
                    // Several taps land where the element shows.
                    Tree.Element e = element(s, element);
                    double[] spot = e.node != null && e.node.refresh() ? visibleCentre(e.node) : null;
                    x = spot != null ? spot[0] : e.cx();
                    y = spot != null ? spot[1] : e.cy();
                } else {
                    x = num(args, "x");
                    y = num(args, "y");
                }
                if ("right".equals(button)) {
                    Screen.longPress(x, y);
                } else {
                    for (int i = 0; i < n; i++) {
                        if (i > 0) {
                            Screen.sleep(80);
                        }
                        Screen.tap(x, y);
                    }
                }
                break;
            }
            case "move_mouse":
                num(args, "x");
                num(args, "y");
                throw new ToolError("touch screens have no pointer to hover with; click or drag instead");
            case "drag":
                Screen.swipe(num(args, "from_x"), num(args, "from_y"), num(args, "to_x"),
                        num(args, "to_y"), 600);
                break;
            case "scroll": {
                double x = num(args, "x");
                double y = num(args, "y");
                double dx = args.optDouble("dx", 0);
                double dy = args.optDouble("dy", 0);
                if (Double.isNaN(dx)) {
                    dx = 0;
                }
                if (Double.isNaN(dy)) {
                    dy = 0;
                }
                Screen.Metrics m = Screen.metrics();
                // Content moves the opposite way to the finger.
                double tx = Math.max(1, Math.min(m.widthPt() - 1, x - dx));
                double ty = Math.max(1, Math.min(m.heightPt() - 1, y - dy));
                Screen.swipe(x, y, tx, ty, 350);
                break;
            }
            case "type_text":
                typeText(str(args, "text"));
                break;
            case "press_key":
                pressKeys(ctx, str(args, "keys"));
                break;
            case "set_value": {
                Tree.Element e = element(s, str(args, "element"));
                String value = str(args, "value");
                android.os.Bundle b = new android.os.Bundle();
                b.putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, value);
                e.node.refresh();
                if (!e.node.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, b)) {
                    // Focus it, clear it, and type.
                    Screen.tap(e.cx(), e.cy());
                    Screen.sleep(300);
                    AccessibilityNodeInfo f = Screen.focusedField();
                    if (f == null || !f.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, b)) {
                        throw new ToolError("that element takes no text");
                    }
                }
                break;
            }
            case "element_action": {
                String element = str(args, "element");
                String a = optStr(args, "action");
                elementAction(s, element, a);
                break;
            }
            case "wait":
                Screen.sleep(Math.min(60_000, Math.max(0, optLong(args, "ms", 1000))));
                break;
            default:
                throw new ToolError("unknown tool \"" + name + "\"");
        }
        if ((shot || wantTree) && !"wait".equals(name)) {
            Screen.sleep(350);
        }
        // Looking is best effort: the action happened either way.
        Screen.Shot picture = null;
        if (shot) {
            try {
                picture = Screen.screenshot();
            } catch (ToolError ignored) {
            }
        }
        StringBuilder out = new StringBuilder("Done: " + name + ".");
        if (picture != null) {
            out.append(String.format(Locale.ROOT, " Screenshot of window 0 (%dx%d).",
                    picture.width, picture.height));
        }
        if (wantTree) {
            try {
                String t = tree(s, 25, 1500);
                out.append("\nAccessibility tree:\n").append(t);
            } catch (ToolError ignored) {
            }
        }
        return new Output(out.toString(), picture);
    }

    private static Tree.Element element(Session s, String id) throws ToolError {
        String t = id.trim();
        if (t.startsWith("e")) {
            try {
                int n = Integer.parseInt(t.substring(1));
                if (n > 0 && n < s.elements.size()) {
                    return s.elements.get(n);
                }
            } catch (NumberFormatException ignored) {
            }
        }
        throw new ToolError("no element " + id + " in the last tree; read the tree again (get_ui_tree)");
    }

    private static void elementAction(Session s, String id, String action) throws ToolError {
        Tree.Element e = element(s, id);
        String a = action == null ? "press" : action.toLowerCase(Locale.ROOT);
        int nodeAction;
        switch (a) {
            case "press": case "click": case "tap": case "pick": case "confirm":
                nodeAction = AccessibilityNodeInfo.ACTION_CLICK;
                break;
            case "longpress": case "long_press": case "showmenu":
                nodeAction = AccessibilityNodeInfo.ACTION_LONG_CLICK;
                break;
            case "focus":
                nodeAction = AccessibilityNodeInfo.ACTION_FOCUS;
                break;
            case "scrollforward": case "scroll_forward": case "increment":
                nodeAction = AccessibilityNodeInfo.ACTION_SCROLL_FORWARD;
                break;
            case "scrollbackward": case "scroll_backward": case "decrement":
                nodeAction = AccessibilityNodeInfo.ACTION_SCROLL_BACKWARD;
                break;
            case "expand":
                nodeAction = AccessibilityNodeInfo.ACTION_EXPAND;
                break;
            case "collapse":
                nodeAction = AccessibilityNodeInfo.ACTION_COLLAPSE;
                break;
            case "select":
                nodeAction = AccessibilityNodeInfo.ACTION_SELECT;
                break;
            case "dismiss": case "cancel":
                nodeAction = AccessibilityNodeInfo.ACTION_DISMISS;
                break;
            default:
                throw new ToolError("unknown action \"" + a + "\"; Android elements take press, "
                        + "longpress, focus, scrollforward, scrollbackward, expand, collapse, "
                        + "select, dismiss");
        }
        boolean live = e.node != null && e.node.refresh();
        // A press or long press touches the element where it shows: some
        // views (Compose, custom ones) accept the accessibility click and do
        // nothing. The phone's screen is in front anyway.
        if (live && (nodeAction == AccessibilityNodeInfo.ACTION_CLICK
                || nodeAction == AccessibilityNodeInfo.ACTION_LONG_CLICK)) {
            double[] spot = visibleCentre(e.node);
            if (spot != null) {
                if (nodeAction == AccessibilityNodeInfo.ACTION_CLICK) {
                    Screen.tap(spot[0], spot[1]);
                } else {
                    Screen.longPress(spot[0], spot[1]);
                }
                return;
            }
        }
        boolean done = false;
        if (live) {
            done = e.node.performAction(nodeAction);
            if (!done && nodeAction == AccessibilityNodeInfo.ACTION_FOCUS) {
                done = e.node.performAction(AccessibilityNodeInfo.ACTION_ACCESSIBILITY_FOCUS)
                        && e.node.performAction(AccessibilityNodeInfo.ACTION_CLICK);
            }
        }
        if (done) {
            return;
        }
        // When the node refused (or is gone), touch it.
        switch (nodeAction) {
            case AccessibilityNodeInfo.ACTION_CLICK:
            case AccessibilityNodeInfo.ACTION_FOCUS:
            case AccessibilityNodeInfo.ACTION_SELECT:
                Screen.tap(e.cx(), e.cy());
                return;
            case AccessibilityNodeInfo.ACTION_LONG_CLICK:
                Screen.longPress(e.cx(), e.cy());
                return;
            case AccessibilityNodeInfo.ACTION_SCROLL_FORWARD:
                Screen.swipe(e.cx(), e.y + e.height * 0.75, e.cx(), e.y + e.height * 0.25, 400);
                return;
            case AccessibilityNodeInfo.ACTION_SCROLL_BACKWARD:
                Screen.swipe(e.cx(), e.y + e.height * 0.25, e.cx(), e.y + e.height * 0.75, 400);
                return;
            default:
                throw new ToolError("the element refused \"" + a + "\"");
        }
    }

    /** The centre of the part of a node on screen, in points, or null. */
    private static double[] visibleCentre(AccessibilityNodeInfo n) throws ToolError {
        if (!n.isVisibleToUser()) {
            return null;
        }
        android.graphics.Rect r = new android.graphics.Rect();
        n.getBoundsInScreen(r);
        Screen.Metrics m = Screen.metrics();
        if (!r.intersect(0, 0, m.widthPx, m.heightPx) || r.isEmpty()) {
            return null;
        }
        return new double[] {r.exactCenterX() / m.scale, r.exactCenterY() / m.scale};
    }

    private static void typeText(String text) throws ToolError {
        String[] lines = text.split("\n", -1);
        for (int i = 0; i < lines.length; i++) {
            if (i > 0) {
                Screen.imeEnter();
            }
            if (!lines[i].isEmpty()) {
                Screen.insert(lines[i]);
            }
        }
    }

    private static void pressKeys(Context ctx, String keys) throws ToolError {
        String[] parts = keys.trim().split("\\s+");
        if (keys.trim().isEmpty()) {
            throw new ToolError("no keys given");
        }
        HostAccessibilityService svc = Screen.service();
        for (String part : parts) {
            String k = part.toLowerCase(Locale.ROOT).replace('-', '_');
            switch (k) {
                case "home": case "home_button": case "homescreen": case "home_screen":
                    global(svc, android.accessibilityservice.AccessibilityService.GLOBAL_ACTION_HOME);
                    break;
                case "back": case "escape": case "esc":
                    global(svc, android.accessibilityservice.AccessibilityService.GLOBAL_ACTION_BACK);
                    break;
                case "recents": case "app_switch": case "appswitch": case "overview":
                    global(svc, android.accessibilityservice.AccessibilityService.GLOBAL_ACTION_RECENTS);
                    break;
                case "notifications":
                    global(svc, android.accessibilityservice.AccessibilityService.GLOBAL_ACTION_NOTIFICATIONS);
                    break;
                case "power": case "lock":
                    global(svc, android.accessibilityservice.AccessibilityService.GLOBAL_ACTION_LOCK_SCREEN);
                    break;
                case "volume_up": case "volumeup":
                case "volume_down": case "volumedown": {
                    AudioManager am = svc.getSystemService(AudioManager.class);
                    am.adjustStreamVolume(AudioManager.STREAM_MUSIC,
                            k.contains("up") ? AudioManager.ADJUST_RAISE : AudioManager.ADJUST_LOWER,
                            AudioManager.FLAG_SHOW_UI);
                    break;
                }
                case "enter": case "return": case "ret":
                    Screen.imeEnter();
                    break;
                case "backspace": case "back_space":
                    Screen.delete(false);
                    break;
                case "delete": case "del": case "forwarddelete":
                    Screen.delete(true);
                    break;
                case "space": case "spacebar":
                    Screen.insert(" ");
                    break;
                case "left": case "arrowleft": case "leftarrow":
                    Screen.moveCaret(-1);
                    break;
                case "right": case "arrowright": case "rightarrow":
                    Screen.moveCaret(1);
                    break;
                default:
                    if (part.contains("+") && part.length() > 1) {
                        String[] chord = part.split("\\+");
                        if (chord.length == 2 && chord[0].equalsIgnoreCase("shift")
                                && chord[1].length() == 1) {
                            Screen.insert(chord[1].toUpperCase(Locale.ROOT));
                            break;
                        }
                        throw new ToolError("\"" + part + "\": the phone takes no keyboard shortcuts; "
                                + "use the app's buttons or element_action");
                    }
                    if (part.codePointCount(0, part.length()) == 1) {
                        Screen.insert(part);
                        break;
                    }
                    throw new ToolError("unknown key \"" + part + "\"; the phone takes home, back, "
                            + "recents, notifications, power, volume_up, volume_down, and in a text "
                            + "field enter, backspace, delete, space, left, right or one character");
            }
        }
    }

    private static void global(HostAccessibilityService svc, int action) throws ToolError {
        if (!svc.performGlobalAction(action)) {
            throw new ToolError("Android refused the button");
        }
        // Home, back and recents animate; let them land before looking.
        Screen.sleep(450);
    }
}
