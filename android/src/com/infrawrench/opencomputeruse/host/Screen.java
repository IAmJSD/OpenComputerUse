package com.infrawrench.opencomputeruse.host;

import android.accessibilityservice.AccessibilityService;
import android.accessibilityservice.GestureDescription;
import android.graphics.Bitmap;
import android.graphics.Path;
import android.hardware.HardwareBuffer;
import android.hardware.display.DisplayManager;
import android.os.Bundle;
import android.util.DisplayMetrics;
import android.view.Display;
import android.view.accessibility.AccessibilityNodeInfo;
import android.view.accessibility.AccessibilityWindowInfo;

import java.io.ByteArrayOutputStream;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * The phone's screen through the accessibility service: its size, pictures
 * of it, its windows, gestures and text entry. Positions are points
 * (density-independent pixels); the screen works in pixels underneath.
 */
final class Screen {
    private Screen() {
    }

    static HostAccessibilityService service() throws ToolError {
        HostAccessibilityService s = HostAccessibilityService.get();
        if (s == null) {
            throw new ToolError("OpenComputerUse's accessibility service is off; turn it on in "
                    + "Settings › Accessibility › OpenComputerUse on the phone");
        }
        return s;
    }

    static final class Metrics {
        final int widthPx;
        final int heightPx;
        /** Pixels per point. */
        final double scale;

        Metrics(int w, int h, double scale) {
            this.widthPx = w;
            this.heightPx = h;
            this.scale = scale;
        }

        double widthPt() {
            return widthPx / scale;
        }

        double heightPt() {
            return heightPx / scale;
        }
    }

    @SuppressWarnings("deprecation")
    static Metrics metrics() throws ToolError {
        HostAccessibilityService s = service();
        DisplayManager dm = s.getSystemService(DisplayManager.class);
        Display d = dm.getDisplay(Display.DEFAULT_DISPLAY);
        DisplayMetrics m = new DisplayMetrics();
        d.getRealMetrics(m);
        double scale = Math.max(0.5, m.densityDpi / 160.0);
        return new Metrics(m.widthPixels, m.heightPixels, scale);
    }

    /** A PNG of the screen, scaled to points. */
    static final class Shot {
        final byte[] png;
        final int width;
        final int height;

        Shot(byte[] png, int width, int height) {
            this.png = png;
            this.width = width;
            this.height = height;
        }
    }

    static Shot screenshot() throws ToolError {
        HostAccessibilityService s = service();
        Metrics m = metrics();
        Bitmap bitmap = null;
        String failure = null;
        // Android allows about three a second; wait out the interval.
        for (int attempt = 0; attempt < 8 && bitmap == null; attempt++) {
            final Bitmap[] got = new Bitmap[1];
            final int[] error = {-1};
            final CountDownLatch done = new CountDownLatch(1);
            s.takeScreenshot(Display.DEFAULT_DISPLAY, s.getMainExecutor(),
                    new AccessibilityService.TakeScreenshotCallback() {
                        @Override
                        public void onSuccess(AccessibilityService.ScreenshotResult r) {
                            try (HardwareBuffer hb = r.getHardwareBuffer()) {
                                Bitmap hw = Bitmap.wrapHardwareBuffer(hb, r.getColorSpace());
                                if (hw != null) {
                                    got[0] = hw.copy(Bitmap.Config.ARGB_8888, false);
                                    hw.recycle();
                                }
                            } catch (Throwable t) {
                                error[0] = -2;
                            }
                            done.countDown();
                        }

                        @Override
                        public void onFailure(int code) {
                            error[0] = code;
                            done.countDown();
                        }
                    });
            try {
                if (!done.await(10, TimeUnit.SECONDS)) {
                    failure = "the screenshot didn't arrive";
                    continue;
                }
            } catch (InterruptedException e) {
                throw new ToolError("interrupted");
            }
            bitmap = got[0];
            if (bitmap == null) {
                failure = screenshotError(error[0]);
                if (error[0] != AccessibilityService.ERROR_TAKE_SCREENSHOT_INTERVAL_TIME_SHORT) {
                    break;
                }
                sleep(350);
            }
        }
        if (bitmap == null) {
            throw new ToolError("couldn't take a screenshot: " + failure);
        }
        int w = (int) Math.max(1, Math.round(bitmap.getWidth() / m.scale));
        int h = (int) Math.max(1, Math.round(bitmap.getHeight() / m.scale));
        Bitmap small = (w == bitmap.getWidth() && h == bitmap.getHeight())
                ? bitmap : Bitmap.createScaledBitmap(bitmap, w, h, true);
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        small.compress(Bitmap.CompressFormat.PNG, 100, out);
        if (small != bitmap) {
            small.recycle();
        }
        bitmap.recycle();
        return new Shot(out.toByteArray(), w, h);
    }

    private static String screenshotError(int code) {
        switch (code) {
            case AccessibilityService.ERROR_TAKE_SCREENSHOT_INTERVAL_TIME_SHORT:
                return "asked too often";
            case AccessibilityService.ERROR_TAKE_SCREENSHOT_NO_ACCESSIBILITY_ACCESS:
                return "the accessibility service may not take screenshots";
            case AccessibilityService.ERROR_TAKE_SCREENSHOT_SECURE_WINDOW:
                return "a secure window is showing (a password or payment screen, or "
                        + "OpenComputerUse's own, which only someone holding the phone may use)";
            case AccessibilityService.ERROR_TAKE_SCREENSHOT_INVALID_DISPLAY:
                return "no such display";
            default:
                return "error " + code;
        }
    }

    /**
     * Drops the service's cached nodes, so a read starts from the live UI.
     * The cache follows accessibility events, but falls behind across
     * screens (a node that was hidden stays hidden).
     */
    static void clearCache(HostAccessibilityService s) {
        try {
            if (android.os.Build.VERSION.SDK_INT >= 34) {
                s.clearCache();
                return;
            }
            Class<?> c = Class.forName("android.view.accessibility.AccessibilityInteractionClient");
            Object client = c.getMethod("getInstance").invoke(null);
            c.getMethod("clearCache").invoke(client);
        } catch (Throwable ignored) {
            // Reads may then be stale, but they still work.
        }
    }

    /** The windows on the phone's screen, top first, read afresh. */
    static List<AccessibilityWindowInfo> windows() throws ToolError {
        HostAccessibilityService s = service();
        clearCache(s);
        List<AccessibilityWindowInfo> list = new ArrayList<>(s.getWindows());
        Collections.sort(list, (a, b) -> Integer.compare(b.getLayer(), a.getLayer()));
        return list;
    }

    /**
     * Whether OpenComputerUse's own screen is in front. It makes and removes
     * device keys, so like the desktop's device management it is never
     * reachable over HTTP: a device with a key can't mint itself another.
     */
    static boolean ownAppInFront() throws ToolError {
        HostAccessibilityService s = service();
        for (AccessibilityWindowInfo w : windows()) {
            if (w.getType() != AccessibilityWindowInfo.TYPE_APPLICATION) {
                continue;
            }
            AccessibilityNodeInfo root = w.getRoot();
            if (root != null && root.getPackageName() != null) {
                return s.getPackageName().contentEquals(root.getPackageName());
            }
        }
        return false;
    }

    // ------------------------------------------------------------ gestures

    private static void gesture(Path path, long durationMs) throws ToolError {
        HostAccessibilityService s = service();
        GestureDescription g = new GestureDescription.Builder()
                .addStroke(new GestureDescription.StrokeDescription(path, 0,
                        Math.max(1, durationMs)))
                .build();
        final boolean[] ok = {false};
        final CountDownLatch done = new CountDownLatch(1);
        boolean sent = s.dispatchGesture(g, new AccessibilityService.GestureResultCallback() {
            @Override
            public void onCompleted(GestureDescription d) {
                ok[0] = true;
                done.countDown();
            }

            @Override
            public void onCancelled(GestureDescription d) {
                done.countDown();
            }
        }, null);
        if (!sent) {
            throw new ToolError("Android refused the gesture");
        }
        try {
            if (!done.await(durationMs + 10_000, TimeUnit.MILLISECONDS)) {
                throw new ToolError("the gesture didn't finish");
            }
        } catch (InterruptedException e) {
            throw new ToolError("interrupted");
        }
        if (!ok[0]) {
            throw new ToolError("the gesture was cancelled (something else touched the screen?)");
        }
    }

    private static float px(double points, Metrics m) {
        return (float) (points * m.scale);
    }

    /** Clamps a point onto the screen, where gestures must start and end. */
    private static float[] onScreen(double x, double y, Metrics m) {
        float fx = Math.max(0, Math.min(m.widthPx - 1, px(x, m)));
        float fy = Math.max(0, Math.min(m.heightPx - 1, px(y, m)));
        return new float[] {fx, fy};
    }

    static void tap(double x, double y) throws ToolError {
        Metrics m = metrics();
        float[] p = onScreen(x, y, m);
        Path path = new Path();
        path.moveTo(p[0], p[1]);
        gesture(path, 50);
    }

    static void longPress(double x, double y) throws ToolError {
        Metrics m = metrics();
        float[] p = onScreen(x, y, m);
        Path path = new Path();
        path.moveTo(p[0], p[1]);
        gesture(path, 800);
    }

    static void swipe(double x1, double y1, double x2, double y2, long ms) throws ToolError {
        Metrics m = metrics();
        float[] a = onScreen(x1, y1, m);
        float[] b = onScreen(x2, y2, m);
        Path path = new Path();
        path.moveTo(a[0], a[1]);
        path.lineTo(b[0], b[1]);
        gesture(path, ms);
    }

    // ------------------------------------------------------------- typing

    /** The field with input focus, or null. */
    static AccessibilityNodeInfo focusedField() throws ToolError {
        AccessibilityNodeInfo f = service().findFocus(AccessibilityNodeInfo.FOCUS_INPUT);
        if (f != null) {
            return f;
        }
        for (AccessibilityWindowInfo w : windows()) {
            AccessibilityNodeInfo root = w.getRoot();
            if (root == null) {
                continue;
            }
            f = root.findFocus(AccessibilityNodeInfo.FOCUS_INPUT);
            if (f != null) {
                return f;
            }
        }
        return null;
    }

    private static AccessibilityNodeInfo editableField() throws ToolError {
        AccessibilityNodeInfo f = focusedField();
        if (f == null || !f.isEditable()) {
            throw new ToolError("no text field has focus; click one (or use set_value on it) first");
        }
        f.refresh();
        return f;
    }

    private static String currentText(AccessibilityNodeInfo f) {
        CharSequence cur = f.isShowingHintText() ? null : f.getText();
        return cur == null ? "" : cur.toString();
    }

    /** The selection as [start, end], within the text. */
    private static int[] selection(AccessibilityNodeInfo f, String text) {
        int start = f.getTextSelectionStart();
        int end = f.getTextSelectionEnd();
        if (start < 0 || end < 0 || start > text.length() || end > text.length()) {
            start = end = text.length();
        }
        if (start > end) {
            int t = start;
            start = end;
            end = t;
        }
        return new int[] {start, end};
    }

    private static void setText(AccessibilityNodeInfo f, String text, int caret) throws ToolError {
        Bundle args = new Bundle();
        args.putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, text);
        if (!f.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, args)) {
            throw new ToolError("the field refused the text");
        }
        Bundle sel = new Bundle();
        sel.putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_START_INT, caret);
        sel.putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_END_INT, caret);
        f.performAction(AccessibilityNodeInfo.ACTION_SET_SELECTION, sel);
    }

    /** Types at the focused field's caret, replacing any selection. */
    static void insert(String text) throws ToolError {
        AccessibilityNodeInfo f = editableField();
        String cur = currentText(f);
        if (f.isPassword() && !cur.isEmpty()) {
            // A password field reads back as dots, so its text can't be
            // rebuilt; replace it with what is typed.
            setText(f, text, text.length());
            return;
        }
        int[] sel = selection(f, cur);
        setText(f, cur.substring(0, sel[0]) + text + cur.substring(sel[1]), sel[0] + text.length());
    }

    /** Backspace (`forward` false) or forward delete at the caret. */
    static void delete(boolean forward) throws ToolError {
        AccessibilityNodeInfo f = editableField();
        String cur = currentText(f);
        int[] sel = selection(f, cur);
        int start = sel[0];
        int end = sel[1];
        if (start == end) {
            if (forward) {
                end = Math.min(cur.length(), end + 1);
            } else {
                start = Math.max(0, start - 1);
            }
        }
        setText(f, cur.substring(0, start) + cur.substring(end), start);
    }

    /** Moves the caret one character left or right. */
    static void moveCaret(int by) throws ToolError {
        AccessibilityNodeInfo f = editableField();
        String cur = currentText(f);
        int[] sel = selection(f, cur);
        int caret = Math.max(0, Math.min(cur.length(), (by < 0 ? sel[0] : sel[1]) + by));
        Bundle args = new Bundle();
        args.putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_START_INT, caret);
        args.putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_END_INT, caret);
        f.performAction(AccessibilityNodeInfo.ACTION_SET_SELECTION, args);
    }

    /** The keyboard's action key (Go, Search, Send, a new line). */
    static void imeEnter() throws ToolError {
        AccessibilityNodeInfo f = editableField();
        if (!f.performAction(AccessibilityNodeInfo.AccessibilityAction.ACTION_IME_ENTER.getId())) {
            // A multi-line field takes a new line instead.
            insert("\n");
        }
    }

    static void sleep(long ms) {
        try {
            Thread.sleep(ms);
        } catch (InterruptedException ignored) {
            Thread.currentThread().interrupt();
        }
    }
}
