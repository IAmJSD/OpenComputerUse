package com.infrawrench.opencomputeruse.shell;

import android.os.Build;
import android.os.SystemClock;
import android.view.InputDevice;
import android.view.InputEvent;
import android.view.KeyCharacterMap;
import android.view.KeyEvent;
import android.view.MotionEvent;

import java.lang.reflect.Method;

/**
 * Touches and keys injected straight into a display, through the input
 * manager the shell user may inject into.
 */
final class Input {
    private static final int INJECT_WAIT_FOR_RESULT = 1;

    private final Object manager;
    private final Method inject;
    private final Method setDisplayId;

    Input() throws Exception {
        Object im;
        if (Build.VERSION.SDK_INT >= 34) {
            Class<?> global = Class.forName("android.hardware.input.InputManagerGlobal");
            im = global.getDeclaredMethod("getInstance").invoke(null);
        } else {
            Class<?> c = android.hardware.input.InputManager.class;
            Method get = c.getDeclaredMethod("getInstance");
            get.setAccessible(true);
            im = get.invoke(null);
        }
        manager = im;
        inject = im.getClass().getMethod("injectInputEvent", InputEvent.class, int.class);
        setDisplayId = InputEvent.class.getMethod("setDisplayId", int.class);
    }

    private void send(InputEvent event, int display) throws Exception {
        setDisplayId.invoke(event, display);
        Object ok = inject.invoke(manager, event, INJECT_WAIT_FOR_RESULT);
        if (Boolean.FALSE.equals(ok)) {
            throw new IllegalStateException("the system refused the input event");
        }
    }

    private void touch(int display, long down, int action, float x, float y) throws Exception {
        MotionEvent e = MotionEvent.obtain(down, SystemClock.uptimeMillis(), action, x, y, 0);
        e.setSource(InputDevice.SOURCE_TOUCHSCREEN);
        try {
            send(e, display);
        } finally {
            e.recycle();
        }
    }

    void tap(int display, float x, float y) throws Exception {
        long down = SystemClock.uptimeMillis();
        touch(display, down, MotionEvent.ACTION_DOWN, x, y);
        touch(display, down, MotionEvent.ACTION_UP, x, y);
    }

    void longPress(int display, float x, float y, long ms) throws Exception {
        long down = SystemClock.uptimeMillis();
        touch(display, down, MotionEvent.ACTION_DOWN, x, y);
        Thread.sleep(ms);
        touch(display, down, MotionEvent.ACTION_UP, x, y);
    }

    void swipe(int display, float x1, float y1, float x2, float y2, long ms) throws Exception {
        long down = SystemClock.uptimeMillis();
        touch(display, down, MotionEvent.ACTION_DOWN, x1, y1);
        long start = SystemClock.uptimeMillis();
        long end = start + Math.max(ms, 1);
        long now;
        while ((now = SystemClock.uptimeMillis()) < end) {
            float t = (now - start) / (float) (end - start);
            touch(display, down, MotionEvent.ACTION_MOVE, x1 + (x2 - x1) * t, y1 + (y2 - y1) * t);
            Thread.sleep(12);
        }
        touch(display, down, MotionEvent.ACTION_MOVE, x2, y2);
        touch(display, down, MotionEvent.ACTION_UP, x2, y2);
    }

    void key(int display, int code, int meta, boolean longPress) throws Exception {
        long down = SystemClock.uptimeMillis();
        sendKey(display, new KeyEvent(down, down, KeyEvent.ACTION_DOWN, code, 0, meta,
                KeyCharacterMap.VIRTUAL_KEYBOARD, 0, 0, InputDevice.SOURCE_KEYBOARD));
        if (longPress) {
            Thread.sleep(500);
            sendKey(display, new KeyEvent(down, SystemClock.uptimeMillis(), KeyEvent.ACTION_DOWN,
                    code, 1, meta, KeyCharacterMap.VIRTUAL_KEYBOARD, 0,
                    KeyEvent.FLAG_LONG_PRESS, InputDevice.SOURCE_KEYBOARD));
        }
        sendKey(display, new KeyEvent(down, SystemClock.uptimeMillis(), KeyEvent.ACTION_UP, code,
                0, meta, KeyCharacterMap.VIRTUAL_KEYBOARD, 0, 0, InputDevice.SOURCE_KEYBOARD));
    }

    private void sendKey(int display, KeyEvent e) throws Exception {
        send(e, display);
    }

    /** Types text as key presses; false when a character has no key. */
    boolean typeKeys(int display, String text) throws Exception {
        KeyCharacterMap map = KeyCharacterMap.load(KeyCharacterMap.VIRTUAL_KEYBOARD);
        KeyEvent[] events = map.getEvents(text.toCharArray());
        if (events == null) {
            return false;
        }
        for (KeyEvent e : events) {
            sendKey(display, KeyEvent.changeTimeRepeat(e, SystemClock.uptimeMillis(), 0));
        }
        return true;
    }
}
