package com.infrawrench.opencomputeruse.host;

import android.accessibilityservice.AccessibilityService;
import android.view.accessibility.AccessibilityEvent;

/**
 * How the app sees and drives other apps: window trees, gestures, global
 * actions (home, back) and screenshots. The user turns it on in Settings ›
 * Accessibility. While it is bound, the system also lets this app start
 * activities from the background, which is how sessions open apps.
 */
public final class HostAccessibilityService extends AccessibilityService {
    private static volatile HostAccessibilityService instance;

    /** The running service, or null when it is off. */
    static HostAccessibilityService get() {
        return instance;
    }

    @Override
    protected void onServiceConnected() {
        instance = this;
        // The server may have been waiting for this (after an update or a
        // reboot), so make sure it runs if it should.
        ServerControl.ensure(this);
    }

    @Override
    public boolean onUnbind(android.content.Intent intent) {
        instance = null;
        return super.onUnbind(intent);
    }

    @Override
    public void onDestroy() {
        instance = null;
        super.onDestroy();
    }

    @Override
    public void onAccessibilityEvent(AccessibilityEvent event) {
        // Trees are read on demand; events aren't needed.
    }

    @Override
    public void onInterrupt() {
    }
}
