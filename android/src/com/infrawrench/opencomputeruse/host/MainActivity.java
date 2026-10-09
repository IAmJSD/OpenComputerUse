package com.infrawrench.opencomputeruse.host;

import android.Manifest;
import android.app.Activity;
import android.app.AlertDialog;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.graphics.Typeface;
import android.os.Build;
import android.os.Bundle;
import android.provider.Settings;
import android.text.InputType;
import android.util.TypedValue;
import android.view.Gravity;
import android.view.View;
import android.widget.Button;
import android.widget.EditText;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.Switch;
import android.widget.TextView;
import android.widget.Toast;

import java.util.List;

/**
 * The app's one screen: whether the accessibility service is on, serving
 * over HTTP, the phone's addresses, and the devices allowed in with their
 * keys (made here, shown once).
 */
public final class MainActivity extends Activity {
    private LinearLayout root;

    @Override
    protected void onCreate(Bundle saved) {
        super.onCreate(saved);
        // Keys show here; keep them out of screenshots, the phone's own
        // computer use among them.
        getWindow().addFlags(android.view.WindowManager.LayoutParams.FLAG_SECURE);
        ScrollView scroll = new ScrollView(this);
        root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        int pad = dp(20);
        root.setPadding(pad, pad, pad, pad);
        scroll.addView(root);
        // Below the status bar and above the navigation bar.
        scroll.setFitsSystemWindows(true);
        setContentView(scroll);
        if (Build.VERSION.SDK_INT >= 33
                && checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS)
                        != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(new String[] {Manifest.permission.POST_NOTIFICATIONS}, 1);
        }
    }

    @Override
    protected void onResume() {
        super.onResume();
        render();
    }

    private int dp(int v) {
        return Math.round(TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_DIP, v,
                getResources().getDisplayMetrics()));
    }

    private TextView text(String s, float sp, boolean bold) {
        TextView t = new TextView(this);
        t.setText(s);
        t.setTextSize(TypedValue.COMPLEX_UNIT_SP, sp);
        if (bold) {
            t.setTypeface(Typeface.DEFAULT_BOLD);
        }
        t.setPadding(0, dp(4), 0, dp(4));
        return t;
    }

    private void heading(String s) {
        TextView t = text(s, 18, true);
        t.setPadding(0, dp(20), 0, dp(6));
        root.addView(t);
    }

    private Button button(String label, View.OnClickListener l) {
        Button b = new Button(this);
        b.setText(label);
        b.setAllCaps(false);
        b.setOnClickListener(l);
        return b;
    }

    private void render() {
        root.removeAllViews();
        root.addView(text("OpenComputerUse", 24, true));
        root.addView(text("Lets your other devices operate this phone's apps, the way they drive "
                + "a computer running OpenComputerUse: give each one a key below.", 14, false));

        heading("Accessibility");
        boolean on = HostAccessibilityService.get() != null;
        root.addView(text(on
                ? "On: OpenComputerUse can read the screen and tap for the devices you allow."
                : "Off. Turn on OpenComputerUse in Accessibility settings so devices can see and "
                        + "operate apps.", 14, false));
        if (!on) {
            root.addView(button("Open Accessibility settings",
                    v -> startActivity(new Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS))));
        }

        heading("Serve over HTTP");
        LinearLayout row = new LinearLayout(this);
        row.setOrientation(LinearLayout.HORIZONTAL);
        row.setGravity(Gravity.CENTER_VERTICAL);
        Switch serve = new Switch(this);
        serve.setText("Serve on port ");
        serve.setChecked(ServerControl.enabled(this));
        EditText port = new EditText(this);
        port.setInputType(InputType.TYPE_CLASS_NUMBER);
        port.setText(String.valueOf(ServerControl.port(this)));
        port.setEms(4);
        port.setContentDescription("Port");
        serve.setOnCheckedChangeListener((b, checked) -> {
            try {
                ServerControl.set(this, checked, Integer.parseInt(port.getText().toString().trim()));
            } catch (Exception e) {
                toast(e.getMessage() == null ? "use a port from 1024 to 65535" : e.getMessage());
                b.setChecked(false);
            }
            root.postDelayed(this::render, 300);
        });
        row.addView(serve);
        row.addView(port);
        root.addView(row);
        StringBuilder where = new StringBuilder();
        List<Host.Address> addrs = Host.addresses();
        if (addrs.isEmpty()) {
            where.append("No network.");
        }
        for (Host.Address a : addrs) {
            where.append(a.ip).append("  ").append(a.network).append('\n');
        }
        root.addView(text("This phone's addresses:\n" + where.toString().trim(), 14, false));
        root.addView(text("There's no TLS, so serve over Tailscale or another network you trust.",
                12, false));

        heading("Devices");
        root.addView(button("Generate key…", v -> askForDevice()));
        List<Keys.Device> devices = Keys.load(this);
        if (devices.isEmpty()) {
            root.addView(text("No devices yet.", 14, false));
        }
        for (Keys.Device d : devices) {
            LinearLayout item = new LinearLayout(this);
            item.setOrientation(LinearLayout.VERTICAL);
            item.setPadding(0, dp(8), 0, dp(8));
            item.addView(text(d.name, 16, true));
            item.addView(text(d.url, 13, false));
            LinearLayout actions = new LinearLayout(this);
            actions.addView(button("Regenerate key", v -> confirm(
                    "Regenerate " + d.name + "'s key?",
                    "Its old key stops working and its sessions end.",
                    () -> {
                        try {
                            showIssued(Host.regenerate(this, d.id, null));
                        } catch (Exception e) {
                            toast(e.getMessage());
                        }
                    })));
            actions.addView(button("Remove", v -> confirm("Remove " + d.name + "?",
                    "Its key stops working and its sessions end.",
                    () -> {
                        try {
                            Keys.remove(this, d.id);
                        } catch (Exception e) {
                            toast(e.getMessage());
                        }
                        render();
                    })));
            item.addView(actions);
            root.addView(item);
        }
    }

    private void toast(String s) {
        Toast.makeText(this, s, Toast.LENGTH_LONG).show();
    }

    private void confirm(String title, String message, Runnable yes) {
        new AlertDialog.Builder(this)
                .setTitle(title)
                .setMessage(message)
                .setPositiveButton("OK", (d, w) -> yes.run())
                .setNegativeButton("Cancel", null)
                .show();
    }

    private void askForDevice() {
        LinearLayout form = new LinearLayout(this);
        form.setOrientation(LinearLayout.VERTICAL);
        form.setPadding(dp(20), dp(8), dp(20), 0);
        form.addView(text("The device that will connect", 13, false));
        EditText name = new EditText(this);
        name.setHint("Work laptop");
        name.setContentDescription("Device name");
        form.addView(name);
        form.addView(text("The URL it reaches this phone at", 13, false));
        EditText url = new EditText(this);
        url.setText(Host.suggestedUrl(ServerControl.port(this)));
        url.setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_URI);
        url.setContentDescription("URL");
        form.addView(url);
        new AlertDialog.Builder(this)
                .setTitle("Generate key")
                .setView(form)
                .setPositiveButton("Generate", (d, w) -> {
                    try {
                        showIssued(Host.generate(this, name.getText().toString(),
                                url.getText().toString()));
                    } catch (Exception e) {
                        toast(e.getMessage());
                    }
                })
                .setNegativeButton("Cancel", null)
                .show();
    }

    private void copy(String label, String s) {
        ClipboardManager cm = getSystemService(ClipboardManager.class);
        cm.setPrimaryClip(ClipData.newPlainText(label, s));
        toast("Copied " + label);
    }

    /** The key, shown once: as a prompt for an agent, and as the entry. */
    private void showIssued(Host.Issued i) {
        render();
        LinearLayout body = new LinearLayout(this);
        body.setOrientation(LinearLayout.VERTICAL);
        body.setPadding(dp(20), dp(8), dp(20), 0);
        body.addView(text("This is the only time the key is shown. Paste the prompt into the "
                + "agent on " + i.device.name + " (Claude Code, Codex, OpenCode, …): it adds this "
                + "phone to its hosts file and installs the skill.", 14, false));
        TextView entry = text("hosts.yaml entry:\n" + i.hostEntry, 12, false);
        entry.setTypeface(Typeface.MONOSPACE);
        entry.setTextIsSelectable(true);
        body.addView(entry);
        body.addView(button("Copy prompt for an agent", v -> copy("the prompt", i.prompt)));
        body.addView(button("Copy hosts.yaml entry", v -> copy("the hosts entry", i.hostEntry)));
        body.addView(button("Copy skill", v -> copy("the skill", i.skill)));
        ScrollView s = new ScrollView(this);
        s.addView(body);
        new AlertDialog.Builder(this)
                .setTitle("Key for " + i.device.name)
                .setView(s)
                .setPositiveButton("Done", null)
                .setCancelable(false)
                .show();
    }
}
