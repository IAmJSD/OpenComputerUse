package com.infrawrench.opencomputeruse.host;

import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.util.Log;

/**
 * Whether the phone serves over HTTP, and on which port: kept in the app's
 * preferences, so the server comes back after a reboot or an update.
 */
final class ServerControl {
    static final int DEFAULT_PORT = 8642;
    private static final String PREFS = "server";

    private ServerControl() {
    }

    private static SharedPreferences prefs(Context c) {
        return c.getApplicationContext().getSharedPreferences(PREFS, Context.MODE_PRIVATE);
    }

    static boolean enabled(Context c) {
        return prefs(c).getBoolean("enabled", false);
    }

    static int port(Context c) {
        return prefs(c).getInt("port", DEFAULT_PORT);
    }

    /** Turns serving on or off (and sets the port when given). */
    static void set(Context c, boolean on, Integer port) {
        SharedPreferences.Editor e = prefs(c).edit().putBoolean("enabled", on);
        if (port != null) {
            if (port < 1024 || port > 65535) {
                throw new IllegalArgumentException("use a port from 1024 to 65535");
            }
            e.putInt("port", port);
        }
        e.apply();
        ensure(c);
    }

    /**
     * Makes the server match the settings. The foreground service keeps the
     * app alive and says so in a notification; when Android won't start one
     * from where this is called (a broadcast in the background), the server
     * still runs, kept alive by the bound accessibility service.
     */
    static void ensure(Context c) {
        Context app = c.getApplicationContext();
        if (!enabled(app)) {
            app.stopService(new Intent(app, ServerService.class));
            HttpServer.stop();
            return;
        }
        try {
            app.startForegroundService(new Intent(app, ServerService.class));
        } catch (Exception e) {
            Log.w("OcuHttp", "no foreground service (" + e + "); serving without one");
            try {
                HttpServer.start(app, port(app));
            } catch (Exception e2) {
                Log.w("OcuHttp", "can't serve: " + e2);
            }
        }
    }
}
