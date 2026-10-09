package com.infrawrench.opencomputeruse.shell;

import android.graphics.Bitmap;
import android.graphics.PixelFormat;
import android.hardware.display.DisplayManager;
import android.hardware.display.VirtualDisplay;
import android.media.Image;
import android.media.ImageReader;
import android.os.Build;
import android.os.Handler;
import android.os.HandlerThread;
import android.os.IBinder;

import java.io.ByteArrayOutputStream;
import java.lang.reflect.Constructor;
import java.lang.reflect.Method;
import java.nio.ByteBuffer;
import java.util.HashMap;
import java.util.Map;

/**
 * Virtual displays the shell user owns, each drawn into an ImageReader so
 * its picture can be read back. Apps started on one (am start --display N)
 * never appear on the device's own screen.
 */
final class Displays {
    // DisplayManager.VIRTUAL_DISPLAY_FLAG_*, several of them hidden.
    private static final int PUBLIC = 1;
    private static final int OWN_CONTENT_ONLY = 1 << 3;
    private static final int SUPPORTS_TOUCH = 1 << 6;
    private static final int ROTATES_WITH_CONTENT = 1 << 7;
    private static final int DESTROY_CONTENT_ON_REMOVAL = 1 << 8;
    private static final int TRUSTED = 1 << 10;
    private static final int OWN_DISPLAY_GROUP = 1 << 11;
    private static final int ALWAYS_UNLOCKED = 1 << 12;
    private static final int TOUCH_FEEDBACK_DISABLED = 1 << 13;
    private static final int OWN_FOCUS = 1 << 14;
    private static final int DEVICE_DISPLAY_GROUP = 1 << 15;

    // WindowManager.DISPLAY_IME_POLICY_*.
    static final int IME_LOCAL = 0;
    static final int IME_FALLBACK = 1;
    static final int IME_HIDE = 2;

    private static final class Entry {
        final VirtualDisplay display;
        final ImageReader reader;
        final Object lock = new Object();
        /** The newest frame, held until a newer one replaces it. */
        Image last;

        Entry(VirtualDisplay display, ImageReader reader) {
            this.display = display;
            this.reader = reader;
        }
    }

    private final Map<Integer, Entry> displays = new HashMap<>();
    private final Handler frames;
    private DisplayManager manager;

    Displays() {
        HandlerThread t = new HandlerThread("ocu-frames");
        t.start();
        frames = new Handler(t.getLooper());
    }

    private DisplayManager manager() throws Exception {
        if (manager == null) {
            Constructor<DisplayManager> ctor =
                    DisplayManager.class.getDeclaredConstructor(android.content.Context.class);
            ctor.setAccessible(true);
            manager = ctor.newInstance(ShellContext.get());
        }
        return manager;
    }

    boolean owns(int id) {
        return displays.containsKey(id);
    }

    /** Returns the new display's id. */
    int create(int width, int height, int dpi) throws Exception {
        int flags = PUBLIC | OWN_CONTENT_ONLY | SUPPORTS_TOUCH | ROTATES_WITH_CONTENT
                | DESTROY_CONTENT_ON_REMOVAL;
        if (Build.VERSION.SDK_INT >= 33) {
            flags |= TRUSTED | OWN_DISPLAY_GROUP | ALWAYS_UNLOCKED | TOUCH_FEEDBACK_DISABLED;
        }
        if (Build.VERSION.SDK_INT >= 34) {
            flags |= OWN_FOCUS | DEVICE_DISPLAY_GROUP;
        }
        ImageReader reader = ImageReader.newInstance(width, height, PixelFormat.RGBA_8888, 3);
        VirtualDisplay vd;
        try {
            vd = manager().createVirtualDisplay("opencomputeruse", width, height, dpi,
                    reader.getSurface(), flags);
        } catch (Exception e) {
            reader.close();
            throw e;
        }
        if (vd == null) {
            reader.close();
            throw new IllegalStateException("the system refused the virtual display");
        }
        final Entry entry = new Entry(vd, reader);
        reader.setOnImageAvailableListener(r -> {
            Image img;
            try {
                img = r.acquireLatestImage();
            } catch (IllegalStateException e) {
                return; // every buffer is held; the next frame will do
            }
            if (img == null) {
                return;
            }
            synchronized (entry.lock) {
                if (entry.last != null) {
                    entry.last.close();
                }
                entry.last = img;
            }
        }, frames);
        int id = vd.getDisplay().getDisplayId();
        displays.put(id, entry);
        return id;
    }

    /**
     * Where the keyboard goes for the display: IME_HIDE keeps it off the
     * device's own screen (text arrives through accessibility instead).
     * Returns whether the system took the policy.
     */
    static boolean setImePolicy(int display, int policy) {
        try {
            Class<?> sm = Class.forName("android.os.ServiceManager");
            IBinder binder = (IBinder) sm.getMethod("getService", String.class).invoke(null, "window");
            Class<?> stub = Class.forName("android.view.IWindowManager$Stub");
            Object wm = stub.getMethod("asInterface", IBinder.class).invoke(null, binder);
            Method set = wm.getClass().getMethod("setDisplayImePolicy", int.class, int.class);
            set.invoke(wm, display, policy);
            return true;
        } catch (Throwable e) {
            return false;
        }
    }

    void release(int id) {
        Entry e = displays.remove(id);
        if (e == null) {
            throw new IllegalArgumentException("display " + id + " was not made here");
        }
        close(e);
    }

    void releaseAll() {
        for (Entry e : displays.values()) {
            close(e);
        }
        displays.clear();
    }

    private static void close(Entry e) {
        e.display.release();
        synchronized (e.lock) {
            if (e.last != null) {
                e.last.close();
                e.last = null;
            }
        }
        e.reader.close();
    }

    /**
     * The display's newest frame as a PNG, with its size. A scale below 1
     * shrinks it first, which also makes the PNG much quicker to encode.
     */
    byte[] screenshot(int id, float scale, int[] size) {
        Entry e = displays.get(id);
        if (e == null) {
            throw new IllegalArgumentException("display " + id + " was not made here");
        }
        Bitmap bmp;
        synchronized (e.lock) {
            if (e.last == null) {
                throw new IllegalStateException("display " + id + " has not drawn anything yet");
            }
            Image img = e.last;
            int w = img.getWidth();
            int h = img.getHeight();
            Image.Plane plane = img.getPlanes()[0];
            ByteBuffer buf = plane.getBuffer();
            int pixelStride = plane.getPixelStride();
            int rowStride = plane.getRowStride();
            // Rows may be padded: copy into a bitmap wide enough for the
            // stride, then crop.
            int padded = rowStride / pixelStride;
            Bitmap full = Bitmap.createBitmap(padded, h, Bitmap.Config.ARGB_8888);
            buf.rewind();
            full.copyPixelsFromBuffer(buf);
            bmp = padded == w ? full : Bitmap.createBitmap(full, 0, 0, w, h);
        }
        if (scale > 0 && scale < 1) {
            int sw = Math.max(1, Math.round(bmp.getWidth() * scale));
            int sh = Math.max(1, Math.round(bmp.getHeight() * scale));
            bmp = Bitmap.createScaledBitmap(bmp, sw, sh, true);
        }
        size[0] = bmp.getWidth();
        size[1] = bmp.getHeight();
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        bmp.compress(Bitmap.CompressFormat.PNG, 100, out);
        return out.toByteArray();
    }
}
