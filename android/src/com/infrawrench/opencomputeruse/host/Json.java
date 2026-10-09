package com.infrawrench.opencomputeruse.host;

import org.json.JSONArray;
import org.json.JSONObject;

import java.util.Iterator;

/**
 * JSON text the way the desktop writes it (serde_json): `"key": value`, two
 * space indents when pretty, slashes left alone (org.json escapes them),
 * and whole floats with a ".0" (a frame's 0.0, not 0).
 */
final class Json {
    private Json() {
    }

    static String compact(Object v) {
        StringBuilder sb = new StringBuilder();
        write(sb, v, -1, 0);
        return sb.toString();
    }

    static String pretty(Object v) {
        StringBuilder sb = new StringBuilder();
        write(sb, v, 2, 0);
        return sb.toString();
    }

    private static void newline(StringBuilder sb, int indent, int depth) {
        if (indent < 0) {
            return;
        }
        sb.append('\n');
        for (int i = 0; i < indent * depth; i++) {
            sb.append(' ');
        }
    }

    private static void write(StringBuilder sb, Object v, int indent, int depth) {
        if (v == null || v == JSONObject.NULL) {
            sb.append("null");
        } else if (v instanceof JSONObject) {
            JSONObject o = (JSONObject) v;
            if (o.length() == 0) {
                sb.append("{}");
                return;
            }
            sb.append('{');
            Iterator<String> keys = o.keys();
            boolean first = true;
            while (keys.hasNext()) {
                String k = keys.next();
                if (!first) {
                    sb.append(',');
                }
                first = false;
                newline(sb, indent, depth + 1);
                string(sb, k);
                sb.append(indent < 0 ? ":" : ": ");
                write(sb, o.opt(k), indent, depth + 1);
            }
            newline(sb, indent, depth);
            sb.append('}');
        } else if (v instanceof JSONArray) {
            JSONArray a = (JSONArray) v;
            if (a.length() == 0) {
                sb.append("[]");
                return;
            }
            sb.append('[');
            for (int i = 0; i < a.length(); i++) {
                if (i > 0) {
                    sb.append(',');
                }
                newline(sb, indent, depth + 1);
                write(sb, a.opt(i), indent, depth + 1);
            }
            newline(sb, indent, depth);
            sb.append(']');
        } else if (v instanceof String) {
            string(sb, (String) v);
        } else if (v instanceof Double || v instanceof Float) {
            double d = ((Number) v).doubleValue();
            if (Double.isNaN(d) || Double.isInfinite(d)) {
                sb.append("null");
            } else if (d == Math.rint(d) && Math.abs(d) < 1e15) {
                sb.append((long) d).append(".0");
            } else {
                sb.append(d);
            }
        } else if (v instanceof Number || v instanceof Boolean) {
            sb.append(v);
        } else {
            string(sb, v.toString());
        }
    }

    private static void string(StringBuilder sb, String s) {
        sb.append('"');
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            switch (c) {
                case '"': sb.append("\\\""); break;
                case '\\': sb.append("\\\\"); break;
                case '\n': sb.append("\\n"); break;
                case '\r': sb.append("\\r"); break;
                case '\t': sb.append("\\t"); break;
                case '\b': sb.append("\\b"); break;
                case '\f': sb.append("\\f"); break;
                default:
                    if (c < 0x20) {
                        sb.append(String.format("\\u%04x", (int) c));
                    } else {
                        sb.append(c);
                    }
            }
        }
        sb.append('"');
    }
}
