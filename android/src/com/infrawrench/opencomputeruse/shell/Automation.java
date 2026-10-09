package com.infrawrench.opencomputeruse.shell;

import android.accessibilityservice.AccessibilityServiceInfo;
import android.app.UiAutomation;
import android.content.Context;
import android.graphics.Rect;
import android.os.Build;
import android.os.Bundle;
import android.os.HandlerThread;
import android.os.Looper;
import android.util.SparseArray;
import android.view.accessibility.AccessibilityNodeInfo;
import android.view.accessibility.AccessibilityWindowInfo;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

import java.lang.reflect.Constructor;
import java.util.ArrayList;
import java.util.Collections;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

/**
 * The accessibility side, through a UiAutomation connected from the shell
 * the way the uiautomator command connects: trees of any display's windows,
 * actions on their nodes, and text set into the focused field.
 *
 * Only one UiAutomation may be connected on a device at a time, so while
 * this one is, `uiautomator dump` fails (and the other way round).
 */
final class Automation {
    /** UiAutomation.FLAG_DONT_SUPPRESS_ACCESSIBILITY_SERVICES (hidden). */
    private static final int DONT_SUPPRESS_SERVICES = 1;
    private static final int MAX_NODES = 3000;

    private UiAutomation automation;
    /** Per display, the nodes of the last tree by their path ("0.2.1"). */
    private final Map<Integer, Map<String, AccessibilityNodeInfo>> lastNodes = new HashMap<>();

    private UiAutomation connect() throws Exception {
        if (automation != null) {
            return automation;
        }
        HandlerThread thread = new HandlerThread("ocu-automation");
        thread.start();
        Class<?> connClass = Class.forName("android.app.UiAutomationConnection");
        Constructor<?> connCtor = connClass.getDeclaredConstructor();
        connCtor.setAccessible(true);
        Object connection = connCtor.newInstance();
        UiAutomation ua = null;
        for (Constructor<?> c : UiAutomation.class.getDeclaredConstructors()) {
            Class<?>[] p = c.getParameterTypes();
            if (p.length != 2 || !p[1].isInstance(connection)) {
                continue;
            }
            c.setAccessible(true);
            if (p[0] == Looper.class) {
                ua = (UiAutomation) c.newInstance(thread.getLooper(), connection);
                break;
            }
            if (p[0] == Context.class) {
                ua = (UiAutomation) c.newInstance(ShellContext.get(), connection);
                break;
            }
        }
        if (ua == null) {
            throw new IllegalStateException("no UiAutomation constructor this helper knows");
        }
        UiAutomation.class.getDeclaredMethod("connect", int.class).invoke(ua, DONT_SUPPRESS_SERVICES);
        AccessibilityServiceInfo info = ua.getServiceInfo();
        if (info == null) {
            throw new IllegalStateException("UiAutomation did not connect");
        }
        info.flags |= AccessibilityServiceInfo.FLAG_RETRIEVE_INTERACTIVE_WINDOWS
                | AccessibilityServiceInfo.FLAG_INCLUDE_NOT_IMPORTANT_VIEWS
                | AccessibilityServiceInfo.FLAG_REPORT_VIEW_IDS;
        ua.setServiceInfo(info);
        automation = ua;
        settle(ua);
        return ua;
    }

    /**
     * The window list fills in over a moment after connecting: wait until
     * it stops growing (or a few seconds pass) before the first read.
     */
    private static void settle(UiAutomation ua) throws InterruptedException {
        long deadline = System.currentTimeMillis() + 3000;
        int last = -1;
        int stable = 0;
        while (System.currentTimeMillis() < deadline && stable < 2) {
            SparseArray<List<AccessibilityWindowInfo>> all = ua.getWindowsOnAllDisplays();
            int n = 0;
            for (int i = 0; i < all.size(); i++) {
                n += all.valueAt(i).size();
            }
            stable = n == last && n > 0 ? stable + 1 : 0;
            last = n;
            Thread.sleep(150);
        }
    }

    /** Forgets the nodes read from a display that is going away. */
    void forget(int display) {
        lastNodes.remove(display);
    }

    void disconnect() {
        if (automation != null) {
            try {
                UiAutomation.class.getDeclaredMethod("disconnect").invoke(automation);
            } catch (Throwable ignored) {
            }
            automation = null;
        }
    }

    /**
     * Drops the connection's cached windows and nodes. The cache is meant to
     * follow accessibility events, but it falls behind, most of all on
     * virtual displays, so every read starts from the live UI.
     */
    private static void clearCache(UiAutomation ua) {
        try {
            if (Build.VERSION.SDK_INT >= 34) {
                ua.clearCache();
                return;
            }
            Class<?> c = Class.forName("android.view.accessibility.AccessibilityInteractionClient");
            Object client = c.getMethod("getInstance").invoke(null);
            c.getMethod("clearCache").invoke(client);
        } catch (Throwable ignored) {
            // Reads may then be stale, but they still work.
        }
    }

    /** The display's windows, top first. */
    private List<AccessibilityWindowInfo> windows(int display) throws Exception {
        UiAutomation ua = connect();
        clearCache(ua);
        SparseArray<List<AccessibilityWindowInfo>> all = ua.getWindowsOnAllDisplays();
        List<AccessibilityWindowInfo> list = all.get(display);
        // Window info can lag a display that just got its first window.
        for (int i = 0; list == null && i < 6; i++) {
            Thread.sleep(150);
            list = ua.getWindowsOnAllDisplays().get(display);
        }
        List<AccessibilityWindowInfo> out = list == null ? new ArrayList<>() : new ArrayList<>(list);
        Collections.sort(out, (a, b) -> Integer.compare(b.getLayer(), a.getLayer()));
        return out;
    }

    private static String windowType(int type) {
        switch (type) {
            case AccessibilityWindowInfo.TYPE_APPLICATION: return "application";
            case AccessibilityWindowInfo.TYPE_INPUT_METHOD: return "input_method";
            case AccessibilityWindowInfo.TYPE_SYSTEM: return "system";
            case AccessibilityWindowInfo.TYPE_ACCESSIBILITY_OVERLAY: return "accessibility_overlay";
            case AccessibilityWindowInfo.TYPE_SPLIT_SCREEN_DIVIDER: return "split_screen_divider";
            case AccessibilityWindowInfo.TYPE_MAGNIFICATION_OVERLAY: return "magnification_overlay";
            default: return "unknown";
        }
    }

    JSONObject tree(int display) throws Exception {
        Map<String, AccessibilityNodeInfo> nodes = new HashMap<>();
        int[] count = {0};
        JSONArray wins = new JSONArray();
        List<AccessibilityWindowInfo> list = windows(display);
        for (int i = 0; i < list.size(); i++) {
            AccessibilityWindowInfo w = list.get(i);
            JSONObject wj = new JSONObject();
            CharSequence title = w.getTitle();
            if (title != null && title.length() > 0) {
                wj.put("title", title.toString());
            }
            wj.put("type", windowType(w.getType()));
            wj.put("layer", w.getLayer());
            Rect r = new Rect();
            w.getBoundsInScreen(r);
            wj.put("bounds", bounds(r));
            AccessibilityNodeInfo root = w.getRoot();
            if (root != null) {
                wj.put("root", node(root, String.valueOf(i), nodes, count));
            }
            wins.put(wj);
        }
        lastNodes.put(display, nodes);
        JSONObject out = new JSONObject();
        out.put("windows", wins);
        if (count[0] >= MAX_NODES) {
            out.put("truncated", true);
        }
        return out;
    }

    private static JSONArray bounds(Rect r) {
        return new JSONArray().put(r.left).put(r.top).put(r.right).put(r.bottom);
    }

    private static void putText(JSONObject o, String key, CharSequence v) throws JSONException {
        if (v != null && v.length() > 0) {
            o.put(key, v.toString());
        }
    }

    private static void putFlag(JSONObject o, String key, boolean v) throws JSONException {
        if (v) {
            o.put(key, true);
        }
    }

    private JSONObject node(AccessibilityNodeInfo n, String path,
            Map<String, AccessibilityNodeInfo> nodes, int[] count) throws JSONException {
        count[0]++;
        nodes.put(path, n);
        JSONObject o = new JSONObject();
        putText(o, "class", n.getClassName());
        // A field showing its hint reports the hint as its text.
        if (!n.isShowingHintText()) {
            putText(o, "text", n.getText());
        }
        putText(o, "desc", n.getContentDescription());
        putText(o, "hint", n.getHintText());
        putText(o, "res", n.getViewIdResourceName());
        Rect r = new Rect();
        n.getBoundsInScreen(r);
        o.put("bounds", bounds(r));
        putFlag(o, "clickable", n.isClickable());
        putFlag(o, "longClickable", n.isLongClickable());
        putFlag(o, "scrollable", n.isScrollable());
        putFlag(o, "editable", n.isEditable());
        putFlag(o, "checkable", n.isCheckable());
        putFlag(o, "checked", n.isChecked());
        putFlag(o, "enabled", n.isEnabled());
        putFlag(o, "focused", n.isFocused());
        putFlag(o, "focusable", n.isFocusable());
        putFlag(o, "selected", n.isSelected());
        putFlag(o, "password", n.isPassword());
        putFlag(o, "visible", n.isVisibleToUser());
        JSONArray children = new JSONArray();
        for (int i = 0; i < n.getChildCount() && count[0] < MAX_NODES; i++) {
            AccessibilityNodeInfo c = n.getChild(i);
            if (c == null) {
                continue;
            }
            // Indexes are positions in the reply's children, so a missing
            // child does not shift its siblings' paths.
            children.put(node(c, path + "." + children.length(), nodes, count));
        }
        if (children.length() > 0) {
            o.put("children", children);
        }
        return o;
    }

    boolean action(int display, JSONArray path, String action, String text) throws Exception {
        Map<String, AccessibilityNodeInfo> nodes = lastNodes.get(display);
        if (nodes == null) {
            throw new IllegalStateException("read the tree of display " + display + " first");
        }
        StringBuilder key = new StringBuilder();
        for (int i = 0; i < path.length(); i++) {
            if (i > 0) {
                key.append('.');
            }
            key.append(path.getInt(i));
        }
        AccessibilityNodeInfo n = nodes.get(key.toString());
        if (n == null) {
            throw new IllegalArgumentException("no node at path " + path + " in the last tree");
        }
        if (!n.refresh()) {
            throw new IllegalStateException("that element is gone; read the tree again");
        }
        switch (action) {
            case "click":
                return n.performAction(AccessibilityNodeInfo.ACTION_CLICK);
            case "long_click":
                return n.performAction(AccessibilityNodeInfo.ACTION_LONG_CLICK);
            case "focus":
                return n.performAction(AccessibilityNodeInfo.ACTION_FOCUS)
                        || n.performAction(AccessibilityNodeInfo.ACTION_ACCESSIBILITY_FOCUS)
                        && n.performAction(AccessibilityNodeInfo.ACTION_CLICK);
            case "set_text": {
                Bundle args = new Bundle();
                args.putCharSequence(
                        AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE,
                        text == null ? "" : text);
                return n.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, args);
            }
            case "scroll_forward":
                return n.performAction(AccessibilityNodeInfo.ACTION_SCROLL_FORWARD);
            case "scroll_backward":
                return n.performAction(AccessibilityNodeInfo.ACTION_SCROLL_BACKWARD);
            case "expand":
                return n.performAction(AccessibilityNodeInfo.ACTION_EXPAND);
            case "collapse":
                return n.performAction(AccessibilityNodeInfo.ACTION_COLLAPSE);
            case "select":
                return n.performAction(AccessibilityNodeInfo.ACTION_SELECT);
            case "clear_focus":
                return n.performAction(AccessibilityNodeInfo.ACTION_CLEAR_FOCUS);
            case "dismiss":
                return n.performAction(AccessibilityNodeInfo.ACTION_DISMISS);
            default:
                throw new IllegalArgumentException("unknown action \"" + action + "\"");
        }
    }

    /** The display's input-focused node, if any window has one. */
    private AccessibilityNodeInfo focused(int display) throws Exception {
        for (AccessibilityWindowInfo w : windows(display)) {
            AccessibilityNodeInfo root = w.getRoot();
            if (root == null) {
                continue;
            }
            AccessibilityNodeInfo f = root.findFocus(AccessibilityNodeInfo.FOCUS_INPUT);
            if (f != null) {
                return f;
            }
        }
        return null;
    }

    /**
     * Inserts text at the focused editable field's selection. Returns false
     * when no such field exists, or when it is a password field holding
     * text the helper cannot read back (key presses then do it).
     */
    boolean insertText(int display, String text) throws Exception {
        AccessibilityNodeInfo f = focused(display);
        if (f == null || !f.isEditable()) {
            return false;
        }
        CharSequence cur = f.isShowingHintText() ? null : f.getText();
        String current = cur == null ? "" : cur.toString();
        if (f.isPassword() && !current.isEmpty()) {
            return false;
        }
        int start = f.getTextSelectionStart();
        int end = f.getTextSelectionEnd();
        if (start < 0 || end < 0 || start > current.length() || end > current.length()) {
            start = end = current.length();
        }
        if (start > end) {
            int t = start;
            start = end;
            end = t;
        }
        String next = current.substring(0, start) + text + current.substring(end);
        Bundle args = new Bundle();
        args.putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, next);
        if (!f.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, args)) {
            return false;
        }
        int caret = start + text.length();
        Bundle sel = new Bundle();
        sel.putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_START_INT, caret);
        sel.putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_END_INT, caret);
        f.performAction(AccessibilityNodeInfo.ACTION_SET_SELECTION, sel);
        return true;
    }

    /** The package of the display's top application window, or null. */
    String focusedApp(int display) throws Exception {
        String fallback = null;
        for (AccessibilityWindowInfo w : windows(display)) {
            AccessibilityNodeInfo root = w.getRoot();
            if (root == null || root.getPackageName() == null) {
                continue;
            }
            if (w.getType() == AccessibilityWindowInfo.TYPE_APPLICATION) {
                return root.getPackageName().toString();
            }
            if (fallback == null) {
                fallback = root.getPackageName().toString();
            }
        }
        return fallback;
    }
}
