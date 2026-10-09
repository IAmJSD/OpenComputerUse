package com.infrawrench.opencomputeruse.host;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;

/** Serves again after a reboot, when serving was on. */
public final class BootReceiver extends BroadcastReceiver {
    @Override
    public void onReceive(Context context, Intent intent) {
        if (Intent.ACTION_BOOT_COMPLETED.equals(intent.getAction())
                && ServerControl.enabled(context)) {
            ServerControl.ensure(context);
        }
    }
}
