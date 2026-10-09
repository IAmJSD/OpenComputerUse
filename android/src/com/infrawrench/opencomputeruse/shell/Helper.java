package com.infrawrench.opencomputeruse.shell;

import android.graphics.Bitmap;
import android.graphics.BitmapFactory;
import android.os.Build;
import android.util.Base64;

import org.json.JSONObject;

import java.io.BufferedReader;
import java.io.ByteArrayOutputStream;
import java.io.FileOutputStream;
import java.io.FileDescriptor;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.io.PrintStream;
import java.nio.charset.StandardCharsets;

/**
 * The desktop's helper on an Android device, run as the shell user:
 *
 *   adb push OpenComputerUse.apk /data/local/tmp/ocu-android.apk
 *   adb shell -T CLASSPATH=/data/local/tmp/ocu-android.apk \
 *       app_process / com.infrawrench.opencomputeruse.shell.Helper
 *
 * Requests are JSON objects, one per line on stdin, each with an integer
 * "id" and a "cmd". Each reply is one line on stdout starting "OCU ", so
 * anything else the runtime prints can be skipped:
 * {"id":N,"ok":true,...} or {"id":N,"ok":false,"error":"..."}.
 * Requests run one at a time. On end of input the helper releases its
 * displays and exits.
 */
public final class Helper {
    public static final String VERSION = "1";

    private final Displays displays = new Displays();
    private final Automation automation = new Automation();
    private Input input;

    public static void main(String[] args) throws Exception {
        // Replies own stdout; nothing else may write to it.
        PrintStream out = new PrintStream(new FileOutputStream(FileDescriptor.out), false,
                "UTF-8");
        System.setOut(System.err);
        Helper helper = new Helper();
        try {
            BufferedReader in = new BufferedReader(
                    new InputStreamReader(System.in, StandardCharsets.UTF_8));
            String line;
            while ((line = in.readLine()) != null) {
                line = line.trim();
                if (line.isEmpty()) {
                    continue;
                }
                String reply = helper.handle(line);
                out.print("OCU ");
                out.print(reply);
                out.print('\n');
                out.flush();
            }
        } catch (Throwable t) {
            System.err.println("ocu helper: " + t);
        } finally {
            helper.shutdown();
        }
        System.exit(0);
    }

    private void shutdown() {
        try {
            displays.releaseAll();
        } catch (Throwable ignored) {
        }
        automation.disconnect();
    }

    private Input input() throws Exception {
        if (input == null) {
            input = new Input();
        }
        return input;
    }

    private String handle(String line) {
        long id = 0;
        try {
            JSONObject req = new JSONObject(line);
            id = req.optLong("id", 0);
            JSONObject reply = run(req);
            reply.put("id", id);
            reply.put("ok", true);
            return reply.toString();
        } catch (Throwable t) {
            Throwable cause = t;
            while (cause instanceof java.lang.reflect.InvocationTargetException
                    && cause.getCause() != null) {
                cause = cause.getCause();
            }
            String msg = cause.getMessage();
            JSONObject err = new JSONObject();
            try {
                err.put("id", id);
                err.put("ok", false);
                err.put("error", cause.getClass().getSimpleName()
                        + (msg == null ? "" : ": " + msg));
            } catch (Exception ignored) {
            }
            return err.toString();
        }
    }

    private JSONObject run(JSONObject req) throws Exception {
        String cmd = req.getString("cmd");
        int display = req.optInt("display", 0);
        JSONObject r = new JSONObject();
        switch (cmd) {
            case "hello":
                r.put("version", VERSION);
                r.put("sdk", Build.VERSION.SDK_INT);
                return r;
            case "create_display": {
                int id = displays.create(req.getInt("width"), req.getInt("height"),
                        req.getInt("dpi"));
                r.put("display", id);
                String ime = req.optString("ime", "hide");
                int policy = "local".equals(ime) ? Displays.IME_LOCAL
                        : "fallback".equals(ime) ? Displays.IME_FALLBACK : Displays.IME_HIDE;
                r.put("ime_policy_set", Displays.setImePolicy(id, policy));
                return r;
            }
            case "release_display": {
                int id = req.getInt("display");
                displays.release(id);
                automation.forget(id);
                return r;
            }
            case "screenshot": {
                int[] size = new int[2];
                float scale = (float) req.optDouble("scale", 1);
                byte[] png = displays.owns(display)
                        ? displays.screenshot(display, scale, size)
                        : screencap(display, scale, size);
                r.put("width", size[0]);
                r.put("height", size[1]);
                r.put("png", Base64.encodeToString(png, Base64.NO_WRAP));
                return r;
            }
            case "tree":
                return automation.tree(display);
            case "action":
                r.put("performed", automation.action(display, req.getJSONArray("path"),
                        req.getString("action"), req.optString("text", null)));
                return r;
            case "tap":
                input().tap(display, (float) req.getDouble("x"), (float) req.getDouble("y"));
                return r;
            case "long_press":
                input().longPress(display, (float) req.getDouble("x"), (float) req.getDouble("y"),
                        req.optLong("duration_ms", 600));
                return r;
            case "swipe":
                input().swipe(display, (float) req.getDouble("x1"), (float) req.getDouble("y1"),
                        (float) req.getDouble("x2"), (float) req.getDouble("y2"),
                        req.optLong("duration_ms", 300));
                return r;
            case "key":
                input().key(display, req.getInt("keycode"), req.optInt("meta", 0),
                        req.optBoolean("longpress", false));
                return r;
            case "text": {
                String text = req.getString("text");
                if (automation.insertText(display, text)) {
                    r.put("method", "set_text");
                } else if (input().typeKeys(display, text)) {
                    r.put("method", "keys");
                } else {
                    throw new IllegalStateException(
                            "no focused text field, and the text has characters no key types");
                }
                return r;
            }
            case "focused_app":
                r.put("package", automation.focusedApp(display));
                return r;
            default:
                throw new IllegalArgumentException("unknown cmd \"" + cmd + "\"");
        }
    }

    /**
     * The device's own screen (display 0), through the screencap command,
     * which the shell user may run. Other displays this helper did not make
     * cannot be captured.
     */
    private static byte[] screencap(int display, float scale, int[] size) throws Exception {
        if (display != 0) {
            throw new IllegalArgumentException("display " + display
                    + " was not made by this helper; only display 0 and the helper's own"
                    + " displays can be captured");
        }
        Process p = new ProcessBuilder("screencap", "-p").start();
        ByteArrayOutputStream buf = new ByteArrayOutputStream();
        try (InputStream in = p.getInputStream()) {
            byte[] chunk = new byte[65536];
            int n;
            while ((n = in.read(chunk)) > 0) {
                buf.write(chunk, 0, n);
            }
        }
        p.waitFor();
        byte[] png = buf.toByteArray();
        if (png.length < 24 || png[1] != 'P' || png[2] != 'N' || png[3] != 'G') {
            throw new IllegalStateException("screencap gave no picture");
        }
        if (scale > 0 && scale < 1) {
            Bitmap full = BitmapFactory.decodeByteArray(png, 0, png.length);
            int sw = Math.max(1, Math.round(full.getWidth() * scale));
            int sh = Math.max(1, Math.round(full.getHeight() * scale));
            Bitmap small = Bitmap.createScaledBitmap(full, sw, sh, true);
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            small.compress(Bitmap.CompressFormat.PNG, 100, out);
            size[0] = sw;
            size[1] = sh;
            return out.toByteArray();
        }
        // PNG's IHDR: width and height, big-endian, at bytes 16 and 20.
        size[0] = ((png[16] & 0xff) << 24) | ((png[17] & 0xff) << 16)
                | ((png[18] & 0xff) << 8) | (png[19] & 0xff);
        size[1] = ((png[20] & 0xff) << 24) | ((png[21] & 0xff) << 16)
                | ((png[22] & 0xff) << 8) | (png[23] & 0xff);
        return png;
    }
}
