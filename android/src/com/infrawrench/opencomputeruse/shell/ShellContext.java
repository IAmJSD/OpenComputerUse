package com.infrawrench.opencomputeruse.shell;

import android.content.AttributionSource;
import android.content.Context;
import android.content.ContextWrapper;
import android.os.Looper;

import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;

/**
 * A Context for a process started by app_process as the shell user, which
 * has none of its own. It wraps the system context of a hand-made
 * ActivityThread and answers as the "com.android.shell" package, whose
 * permissions (trusted displays, input injection) the shell user holds.
 */
final class ShellContext extends ContextWrapper {
    static final String PACKAGE = "com.android.shell";
    static final int SHELL_UID = 2000;

    private static ShellContext instance;

    static synchronized ShellContext get() {
        if (instance == null) {
            instance = new ShellContext(systemContext());
        }
        return instance;
    }

    private ShellContext(Context base) {
        super(base);
    }

    @Override
    public String getPackageName() {
        return PACKAGE;
    }

    @Override
    public String getOpPackageName() {
        return PACKAGE;
    }

    @Override
    public AttributionSource getAttributionSource() {
        return new AttributionSource.Builder(SHELL_UID).setPackageName(PACKAGE).build();
    }

    @Override
    public Context getApplicationContext() {
        return this;
    }

    /**
     * An ActivityThread made by hand and installed as the current one, then
     * its system context. Called once, before anything else needs a Context.
     */
    private static Context systemContext() {
        try {
            if (Looper.getMainLooper() == null) {
                Looper.prepareMainLooper();
            }
            Class<?> at = Class.forName("android.app.ActivityThread");
            Constructor<?> ctor = at.getDeclaredConstructor();
            ctor.setAccessible(true);
            Object thread = ctor.newInstance();
            Field current = at.getDeclaredField("sCurrentActivityThread");
            current.setAccessible(true);
            current.set(null, thread);
            Field system = at.getDeclaredField("mSystemThread");
            system.setAccessible(true);
            system.setBoolean(thread, true);
            // Android 12+ dereferences the thread's configuration controller
            // when making contexts; a bare thread has none.
            try {
                Class<?> cc = Class.forName("android.app.ConfigurationController");
                Class<?> internal = Class.forName("android.app.ActivityThreadInternal");
                Constructor<?> ccCtor = cc.getDeclaredConstructor(internal);
                ccCtor.setAccessible(true);
                Field f = at.getDeclaredField("mConfigurationController");
                f.setAccessible(true);
                f.set(thread, ccCtor.newInstance(thread));
            } catch (Throwable ignored) {
                // Older releases have no controller to fill in.
            }
            Method get = at.getDeclaredMethod("getSystemContext");
            get.setAccessible(true);
            return (Context) get.invoke(thread);
        } catch (Exception e) {
            throw new RuntimeException("no system context: " + e, e);
        }
    }
}
