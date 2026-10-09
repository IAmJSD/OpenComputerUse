package com.infrawrench.opencomputeruse.host;

import android.content.Context;

/** This APK's version, which build.sh takes from the workspace's Cargo.toml. */
final class Version {
    private Version() {
    }

    static String name(Context c) {
        try {
            return c.getPackageManager().getPackageInfo(c.getPackageName(), 0).versionName;
        } catch (Exception e) {
            return "0";
        }
    }
}
