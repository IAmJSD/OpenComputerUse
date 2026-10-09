package com.infrawrench.opencomputeruse.host;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.os.Build;
import android.os.IBinder;
import android.util.Log;

import com.infrawrench.opencomputeruse.R;

/**
 * Keeps the HTTP server running while serving is on, as a foreground
 * service with an ongoing notification saying so.
 */
public final class ServerService extends Service {
    private static final String CHANNEL = "server";
    private static final int NOTIFICATION = 1;

    @Override
    public void onCreate() {
        super.onCreate();
        NotificationManager nm = getSystemService(NotificationManager.class);
        NotificationChannel ch = new NotificationChannel(CHANNEL, "Serving",
                NotificationManager.IMPORTANCE_LOW);
        ch.setDescription("Shown while other devices can operate this phone's apps.");
        nm.createNotificationChannel(ch);
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        int port = ServerControl.port(this);
        PendingIntent open = PendingIntent.getActivity(this, 0,
                new Intent(this, MainActivity.class), PendingIntent.FLAG_IMMUTABLE);
        Notification n = new Notification.Builder(this, CHANNEL)
                .setSmallIcon(R.drawable.ic_notification)
                .setContentTitle("Serving computer use on port " + port)
                .setContentText(HostAccessibilityService.get() == null
                        ? "Turn on the accessibility service so devices can drive apps"
                        : "Devices with a key can operate this phone's apps")
                .setContentIntent(open)
                .setOngoing(true)
                .build();
        if (Build.VERSION.SDK_INT >= 34) {
            startForeground(NOTIFICATION, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE);
        } else {
            startForeground(NOTIFICATION, n);
        }
        try {
            HttpServer.start(this, port);
        } catch (Exception e) {
            Log.w("OcuHttp", "can't listen on port " + port + ": " + e);
        }
        return START_STICKY;
    }

    @Override
    public void onDestroy() {
        if (!ServerControl.enabled(this)) {
            HttpServer.stop();
        }
        super.onDestroy();
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }
}
