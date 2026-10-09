package com.infrawrench.opencomputeruse.host;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;

import org.json.JSONArray;
import org.json.JSONObject;

/**
 * Setting the phone up from a computer, over adb. Only the shell may send
 * these (the receiver needs android.permission.DUMP, which apps can't
 * hold). The answer is the broadcast's result data, which `am broadcast`
 * prints:
 *
 *   adb shell am broadcast -n com.infrawrench.opencomputeruse/.host.SetupReceiver --es cmd serve [--ei port 8642]
 *   adb shell am broadcast -n …/.host.SetupReceiver --es cmd stop
 *   adb shell am broadcast -n …/.host.SetupReceiver --es cmd generate --es name "'Work laptop'" [--es url http://100.x.y.z:8642]
 *   adb shell am broadcast -n …/.host.SetupReceiver --es cmd regenerate --es id ID [--es url URL]
 *   adb shell am broadcast -n …/.host.SetupReceiver --es cmd remove --es id ID
 *   adb shell am broadcast -n …/.host.SetupReceiver --es cmd status
 *
 * generate and regenerate answer with the key, the hosts-file entry and the
 * agent prompt, as JSON; it is the only time the key is shown. The device
 * runs `adb shell`'s line through its own shell, so a value with spaces
 * needs quotes inside the quotes, as above.
 */
public final class SetupReceiver extends BroadcastReceiver {
    @Override
    public void onReceive(Context context, Intent intent) {
        JSONObject out = new JSONObject();
        int code = 0;
        try {
            String cmd = intent.getStringExtra("cmd");
            if (cmd == null) {
                cmd = "status";
            }
            switch (cmd) {
                case "serve": {
                    int port = intent.getIntExtra("port", ServerControl.port(context));
                    ServerControl.set(context, true, port);
                    out.put("serving", true);
                    out.put("port", port);
                    break;
                }
                case "stop":
                    ServerControl.set(context, false, null);
                    out.put("serving", false);
                    break;
                case "generate":
                case "regenerate": {
                    String url = intent.getStringExtra("url");
                    Host.Issued i = cmd.equals("generate")
                            ? Host.generate(context, intent.getStringExtra("name"),
                                    url == null ? Host.suggestedUrl(ServerControl.port(context)) : url)
                            : Host.regenerate(context, intent.getStringExtra("id"), url);
                    out.put("id", i.device.id);
                    out.put("name", i.device.name);
                    out.put("url", i.device.url);
                    out.put("key", i.key);
                    out.put("host_entry", i.hostEntry);
                    out.put("prompt", i.prompt);
                    break;
                }
                case "remove": {
                    Keys.Device d = Keys.remove(context, intent.getStringExtra("id"));
                    out.put("removed", d.id);
                    break;
                }
                case "status": {
                    out.put("serving", ServerControl.enabled(context));
                    out.put("listening", HttpServer.isRunning());
                    out.put("port", ServerControl.port(context));
                    out.put("accessibility", HostAccessibilityService.get() != null);
                    JSONArray devs = new JSONArray();
                    for (Keys.Device d : Keys.load(context)) {
                        devs.put(new JSONObject().put("id", d.id).put("name", d.name)
                                .put("url", d.url));
                    }
                    out.put("devices", devs);
                    JSONArray addrs = new JSONArray();
                    for (Host.Address a : Host.addresses()) {
                        addrs.put(a.ip + " (" + a.network + ")");
                    }
                    out.put("addresses", addrs);
                    break;
                }
                default:
                    throw new IllegalArgumentException("unknown cmd \"" + cmd
                            + "\"; use serve, stop, generate, regenerate, remove or status");
            }
        } catch (Exception e) {
            code = 1;
            out = new JSONObject();
            try {
                out.put("error", e.getMessage() == null ? e.toString() : e.getMessage());
            } catch (Exception ignored) {
            }
        }
        setResult(code, Json.compact(out), null);
    }
}
