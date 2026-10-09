package com.infrawrench.opencomputeruse.host;

import android.content.Context;
import android.util.Base64;

import org.json.JSONArray;
import org.json.JSONObject;

/**
 * MCP over HTTP, and tool results as MCP shapes them: the same answers the
 * desktop's server gives (src/mcp.rs), for the phone's tools.
 */
final class Mcp {
    private Mcp() {
    }

    private static final String[] PROTOCOL_VERSIONS = {"2025-06-18", "2025-03-26", "2024-11-05"};

    static final String INSTRUCTIONS = "This server operates apps on an Android phone, through its OpenComputerUse app. "
            + "Start a session with an app (start_session), then drive it with the session id: "
            + "screenshot and get_ui_tree to look; click (a tap), type_text, press_key (home, back and the phone's other buttons), "
            + "scroll, drag (a swipe), set_value and element_action to act. "
            + "Actions return a fresh screenshot by default (screenshot: false skips it; ui_tree: true adds the accessibility tree). "
            + "Coordinates are points, the grid of the screenshots. "
            + "Prefer element ids from the tree (click with element: \"e12\") over coordinates. "
            + "End sessions with end_session when done.";

    /** A tool call's result: text, and an image when there is one. */
    static JSONObject callTool(Context ctx, Tools.Client client, String name, JSONObject args) {
        JSONObject r = new JSONObject();
        try {
            try {
                Tools.Output out = Tools.call(ctx, client, name, args);
                JSONArray content = new JSONArray();
                content.put(new JSONObject().put("type", "text").put("text", out.text));
                if (out.image != null) {
                    content.put(new JSONObject()
                            .put("type", "image")
                            .put("data", Base64.encodeToString(out.image.png, Base64.NO_WRAP))
                            .put("mimeType", "image/png"));
                }
                r.put("content", content);
                r.put("isError", false);
            } catch (Throwable t) {
                String msg = t instanceof ToolError ? t.getMessage()
                        : t.getClass().getSimpleName()
                                + (t.getMessage() == null ? "" : ": " + t.getMessage());
                JSONArray content = new JSONArray();
                content.put(new JSONObject().put("type", "text").put("text", "Error: " + msg));
                r.put("content", content);
                r.put("isError", true);
            }
        } catch (Exception ignored) {
        }
        return r;
    }

    /** One JSON-RPC message or a batch; null when nothing needs sending back. */
    static Object dispatchValue(Context ctx, Tools.Client client, Object msg) {
        if (msg instanceof JSONArray) {
            JSONArray in = (JSONArray) msg;
            JSONArray out = new JSONArray();
            for (int i = 0; i < in.length(); i++) {
                JSONObject reply = dispatch(ctx, client, in.optJSONObject(i));
                if (reply != null) {
                    out.put(reply);
                }
            }
            return out.length() == 0 ? null : out;
        }
        return dispatch(ctx, client, msg instanceof JSONObject ? (JSONObject) msg : null);
    }

    private static JSONObject dispatch(Context ctx, Tools.Client client, JSONObject msg) {
        if (msg == null || !msg.has("id")) {
            // Notifications (and responses) get no reply.
            return null;
        }
        Object id = msg.opt("id");
        String method = msg.optString("method", "");
        JSONObject params = msg.optJSONObject("params");
        if (params == null) {
            params = new JSONObject();
        }
        JSONObject reply = new JSONObject();
        try {
            reply.put("jsonrpc", "2.0");
            reply.put("id", id);
            switch (method) {
                case "initialize": {
                    String asked = params.optString("protocolVersion", PROTOCOL_VERSIONS[0]);
                    String version = PROTOCOL_VERSIONS[0];
                    for (String v : PROTOCOL_VERSIONS) {
                        if (v.equals(asked)) {
                            version = v;
                        }
                    }
                    JSONObject result = new JSONObject();
                    result.put("protocolVersion", version);
                    result.put("capabilities", new JSONObject().put("tools",
                            new JSONObject().put("listChanged", true)));
                    result.put("serverInfo", new JSONObject().put("name", "opencomputeruse")
                            .put("version", Version.name(ctx)));
                    result.put("instructions", INSTRUCTIONS);
                    reply.put("result", result);
                    break;
                }
                case "ping":
                    reply.put("result", new JSONObject());
                    break;
                case "tools/list":
                    reply.put("result", new JSONObject().put("tools", Tools.list()));
                    break;
                case "tools/call": {
                    String name = params.optString("name", "");
                    JSONObject args = params.optJSONObject("arguments");
                    reply.put("result", callTool(ctx, client, name, args == null ? new JSONObject() : args));
                    break;
                }
                case "resources/list":
                    reply.put("result", new JSONObject().put("resources", new JSONArray()));
                    break;
                case "prompts/list":
                    reply.put("result", new JSONObject().put("prompts", new JSONArray()));
                    break;
                default:
                    reply.put("error", new JSONObject().put("code", -32601)
                            .put("message", "method not found: " + method));
            }
        } catch (Exception ignored) {
        }
        return reply;
    }
}
