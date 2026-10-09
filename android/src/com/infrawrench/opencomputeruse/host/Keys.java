package com.infrawrench.opencomputeruse.host;

import android.content.Context;

import org.json.JSONArray;
import org.json.JSONObject;

import java.io.File;
import java.io.FileOutputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.security.MessageDigest;
import java.security.SecureRandom;
import java.util.ArrayList;
import java.util.List;

/**
 * The devices allowed to drive this phone, each with a key: devices.json in
 * the app's private files, the same shape as the desktop's. Only a SHA-256
 * of each key is kept; the key itself is shown once, when it is made.
 */
final class Keys {
    static final class Device {
        String id;
        /** What the device connecting in is called: "Work laptop". */
        String name;
        /** How that device reaches this phone: "http://100.101.102.103:8642". */
        String url;
        String keyHash;
        long created;

        JSONObject toJson() throws Exception {
            JSONObject o = new JSONObject();
            o.put("id", id);
            o.put("name", name);
            o.put("url", url);
            o.put("key_hash", keyHash);
            o.put("created", created);
            return o;
        }
    }

    /** A device and its new key. */
    static final class Issued {
        final Device device;
        final String key;

        Issued(Device device, String key) {
            this.device = device;
            this.key = key;
        }
    }

    private static final SecureRandom RANDOM = new SecureRandom();

    private Keys() {
    }

    static File file(Context c) {
        return new File(c.getFilesDir(), "devices.json");
    }

    static String randomHex(int bytes) {
        byte[] b = new byte[bytes];
        RANDOM.nextBytes(b);
        return hex(b);
    }

    static String hex(byte[] b) {
        StringBuilder sb = new StringBuilder();
        for (byte x : b) {
            sb.append(String.format("%02x", x & 0xff));
        }
        return sb.toString();
    }

    static String hashKey(String key) {
        try {
            MessageDigest md = MessageDigest.getInstance("SHA-256");
            return hex(md.digest(key.getBytes(StandardCharsets.UTF_8)));
        } catch (Exception e) {
            throw new IllegalStateException(e);
        }
    }

    static synchronized List<Device> load(Context c) {
        List<Device> out = new ArrayList<>();
        File f = file(c);
        if (!f.isFile()) {
            return out;
        }
        try {
            JSONObject o = new JSONObject(new String(Files.readAllBytes(f.toPath()),
                    StandardCharsets.UTF_8));
            JSONArray a = o.optJSONArray("devices");
            for (int i = 0; a != null && i < a.length(); i++) {
                JSONObject d = a.getJSONObject(i);
                Device dev = new Device();
                dev.id = d.optString("id");
                dev.name = d.optString("name");
                dev.url = d.optString("url");
                dev.keyHash = d.optString("key_hash");
                dev.created = d.optLong("created");
                out.add(dev);
            }
        } catch (Exception ignored) {
            // A malformed file allows nobody in, as on the desktop.
        }
        return out;
    }

    private static synchronized void save(Context c, List<Device> devices) throws Exception {
        JSONArray a = new JSONArray();
        for (Device d : devices) {
            a.put(d.toJson());
        }
        JSONObject o = new JSONObject();
        o.put("devices", a);
        File f = file(c);
        File tmp = new File(f.getPath() + ".tmp");
        try (FileOutputStream out = new FileOutputStream(tmp)) {
            out.write(Json.pretty(o).getBytes(StandardCharsets.UTF_8));
        }
        if (!tmp.renameTo(f)) {
            throw new IllegalStateException("couldn't save " + f);
        }
    }

    /** The device a key belongs to, or null. */
    static Device authenticate(List<Device> devices, String key) {
        String hash = hashKey(key);
        for (Device d : devices) {
            if (d.keyHash.equals(hash)) {
                return d;
            }
        }
        return null;
    }

    /** "my-phone.ts.net:8642/" → "http://my-phone.ts.net:8642". */
    static String normalizeUrl(String url) {
        String u = url == null ? "" : url.trim();
        while (u.endsWith("/")) {
            u = u.substring(0, u.length() - 1);
        }
        if (u.isEmpty()) {
            throw new IllegalArgumentException("give the URL the device will reach this phone at");
        }
        return u.contains("://") ? u : "http://" + u;
    }

    static synchronized Issued add(Context c, String name, String url) throws Exception {
        String n = name == null ? "" : name.trim();
        if (n.isEmpty()) {
            throw new IllegalArgumentException("name the device that will connect");
        }
        String key = "ocu_" + randomHex(24);
        Device d = new Device();
        d.id = randomHex(4);
        d.name = n;
        d.url = normalizeUrl(url);
        d.keyHash = hashKey(key);
        d.created = System.currentTimeMillis() / 1000;
        List<Device> all = load(c);
        all.add(d);
        save(c, all);
        return new Issued(d, key);
    }

    private static Device find(List<Device> all, String id) {
        for (Device d : all) {
            if (d.id.equals(id) || d.name.equalsIgnoreCase(id)) {
                return d;
            }
        }
        throw new IllegalArgumentException("no device \"" + id + "\"");
    }

    /** A new key for a device, retiring its old one (and its sessions). */
    static synchronized Issued regenerate(Context c, String id, String url) throws Exception {
        List<Device> all = load(c);
        Device d = find(all, id);
        String key = "ocu_" + randomHex(24);
        d.keyHash = hashKey(key);
        if (url != null && !url.trim().isEmpty()) {
            d.url = normalizeUrl(url);
        }
        save(c, all);
        return new Issued(d, key);
    }

    static synchronized Device remove(Context c, String id) throws Exception {
        List<Device> all = load(c);
        Device d = find(all, id);
        all.remove(d);
        save(c, all);
        return d;
    }
}
