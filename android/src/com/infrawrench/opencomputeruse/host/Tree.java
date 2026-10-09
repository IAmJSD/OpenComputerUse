package com.infrawrench.opencomputeruse.host;

import android.graphics.Rect;
import android.view.accessibility.AccessibilityNodeInfo;
import android.view.accessibility.AccessibilityWindowInfo;

import java.util.ArrayList;
import java.util.List;
import java.util.Locale;

/**
 * The phone's windows as the desktop's accessibility tree text: one line
 * per element, `[e12] Button "Save" @(x,y wxh) actions=press`, with the
 * same rules as the desktop's Android backend (crates/ocu-mobile's
 * android/tree.rs): layout wrappers with nothing to say or do give way to
 * their children, roles are short class names, a field's text is its value,
 * and frames are in points.
 */
final class Tree {
    /** An element of the last tree read, by its id's number. */
    static final class Element {
        /** In points, relative to the screen. */
        double x, y, width, height;
        AccessibilityNodeInfo node;
        boolean editable;
        int valueLen;

        double cx() {
            return x + width / 2;
        }

        double cy() {
            return y + height / 2;
        }
    }

    /** A node of the rendered tree. */
    static final class UiNode {
        String id;
        String role;
        String name;
        String value;
        String description;
        boolean hasFrame;
        double x, y, width, height;
        List<String> actions = new ArrayList<>();
        boolean enabled = true;
        boolean focused;
        List<UiNode> children = new ArrayList<>();

        String render() {
            StringBuilder sb = new StringBuilder();
            render(sb, 0);
            return sb.toString();
        }

        private void render(StringBuilder out, int depth) {
            for (int i = 0; i < depth * 2; i++) {
                out.append(' ');
            }
            out.append('[').append(id).append("] ").append(role);
            if (name != null && !name.isEmpty()) {
                out.append(" \"").append(clip(name, 80)).append('"');
            }
            if (value != null && !value.isEmpty()) {
                out.append(" value=\"").append(clip(value, 120)).append('"');
            }
            if (description != null && !description.isEmpty() && !description.equals(name)) {
                out.append(" desc=\"").append(clip(description, 80)).append('"');
            }
            if (hasFrame) {
                out.append(String.format(Locale.ROOT, " @(%.0f,%.0f %.0fx%.0f)", x, y, width, height));
            }
            if (!actions.isEmpty()) {
                out.append(" actions=").append(String.join(",", actions));
            }
            if (!enabled) {
                out.append(" disabled");
            }
            if (focused) {
                out.append(" focused");
            }
            out.append('\n');
            for (UiNode c : children) {
                c.render(out, depth + 1);
            }
        }
    }

    static String clip(String s, int max) {
        String t = s.replace("\n", "\\n").replace("\"", "\\\"");
        if (t.codePointCount(0, t.length()) <= max) {
            return t;
        }
        return t.substring(0, t.offsetByCodePoints(0, max)) + "…";
    }

    private final List<Element> elements = new ArrayList<>();
    private final double scale;
    private final int maxDepth;
    private int budget;

    private Tree(double scale, int maxDepth, int maxNodes) {
        this.scale = scale;
        this.maxDepth = maxDepth;
        this.budget = Math.max(1, maxNodes);
    }

    /** What a read produces: the tree, and its elements by id number. */
    static final class Read {
        final UiNode root;
        final List<Element> elements;

        Read(UiNode root, List<Element> elements) {
            this.root = root;
            this.elements = elements;
        }
    }

    /** Reads `windows`, leaving out those of the package `hidden`. */
    static Read read(List<AccessibilityWindowInfo> windows, String title, double scale,
            int maxDepth, int maxNodes, String hidden) {
        Tree t = new Tree(scale, maxDepth, maxNodes);
        UiNode root = new UiNode();
        root.id = t.add(new Element());
        root.role = "Screen";
        root.name = title;
        t.budget--;
        for (AccessibilityWindowInfo w : windows) {
            // With the cache cleared, fetch each window's nodes in bulk
            // rather than one call per child.
            AccessibilityNodeInfo r = android.os.Build.VERSION.SDK_INT >= 33
                    ? w.getRoot(AccessibilityNodeInfo.FLAG_PREFETCH_DESCENDANTS_HYBRID)
                    : w.getRoot();
            if (r != null && hidden.contentEquals(str(r.getPackageName()))) {
                continue;
            }
            if (r != null) {
                root.children.addAll(t.convert(r, 1));
            }
        }
        return new Read(root, t.elements);
    }

    private String add(Element e) {
        elements.add(e);
        return "e" + (elements.size() - 1);
    }

    private static String str(CharSequence c) {
        return c == null ? "" : c.toString();
    }

    private static boolean interactive(AccessibilityNodeInfo n) {
        return n.isClickable() || n.isLongClickable() || n.isScrollable() || n.isEditable()
                || n.isCheckable();
    }

    /** "com.android.settings:id/search_bar" → "search_bar". */
    private static String shortRes(String res) {
        int i = res.lastIndexOf('/');
        return i < 0 ? res : res.substring(i + 1);
    }

    private static String role(String cls) {
        int i = Math.max(cls.lastIndexOf('.'), cls.lastIndexOf('$'));
        String s = i < 0 ? cls : cls.substring(i + 1);
        return s.isEmpty() ? "View" : s;
    }

    private static String firstNonEmpty(String... values) {
        for (String v : values) {
            if (v != null && !v.isEmpty()) {
                return v;
            }
        }
        return null;
    }

    private List<UiNode> convert(AccessibilityNodeInfo n, int depth) {
        List<UiNode> out = new ArrayList<>();
        // What isn't on screen can't be acted on.
        if (!n.isVisibleToUser()) {
            return out;
        }
        String text = n.isShowingHintText() ? "" : str(n.getText());
        String desc = str(n.getContentDescription());
        String hint = str(n.getHintText());
        String res = str(n.getViewIdResourceName());
        boolean hollow = !interactive(n) && text.isEmpty() && desc.isEmpty() && hint.isEmpty()
                && !n.isFocused();
        if (hollow) {
            for (int i = 0; i < n.getChildCount(); i++) {
                AccessibilityNodeInfo c = n.getChild(i);
                if (c != null) {
                    out.addAll(convert(c, depth));
                }
            }
            return out;
        }
        if (budget == 0 || depth > maxDepth) {
            return out;
        }
        budget--;
        Rect r = new Rect();
        n.getBoundsInScreen(r);
        Element e = new Element();
        e.x = r.left / scale;
        e.y = r.top / scale;
        e.width = Math.max(0, (r.right - r.left) / scale);
        e.height = Math.max(0, (r.bottom - r.top) / scale);
        e.node = n;
        e.editable = n.isEditable();
        e.valueLen = n.isEditable() ? text.codePointCount(0, text.length()) : 0;
        UiNode u = new UiNode();
        u.id = add(e);
        u.role = role(str(n.getClassName()));
        if (n.isClickable() || n.isCheckable()) {
            u.actions.add("press");
        }
        if (n.isLongClickable()) {
            u.actions.add("longpress");
        }
        if (n.isEditable() || (n.isFocusable() && !n.isClickable())) {
            u.actions.add("focus");
        }
        if (n.isScrollable()) {
            u.actions.add("scrollforward");
            u.actions.add("scrollbackward");
        }
        if (n.isEditable()) {
            u.name = firstNonEmpty(desc, hint, shortRes(res));
            if (n.isPassword() && !text.isEmpty()) {
                StringBuilder dots = new StringBuilder();
                for (int i = 0; i < Math.min(12, text.length()); i++) {
                    dots.append('•');
                }
                u.value = dots.toString();
            } else {
                u.value = text.isEmpty() ? null : text;
            }
        } else if (n.isCheckable()) {
            u.name = firstNonEmpty(text, desc);
            u.value = n.isChecked() ? "on" : "off";
        } else {
            u.name = firstNonEmpty(text, desc);
        }
        if (!text.isEmpty() && !desc.isEmpty() && !n.isEditable()) {
            u.description = desc;
        } else if (u.name == null && !res.isEmpty()) {
            u.description = "id:" + shortRes(res);
        }
        u.hasFrame = true;
        u.x = e.x;
        u.y = e.y;
        u.width = e.width;
        u.height = e.height;
        u.enabled = n.isEnabled();
        u.focused = n.isFocused();
        for (int i = 0; i < n.getChildCount(); i++) {
            AccessibilityNodeInfo c = n.getChild(i);
            if (c != null) {
                u.children.addAll(convert(c, depth + 1));
            }
        }
        out.add(u);
        return out;
    }
}
