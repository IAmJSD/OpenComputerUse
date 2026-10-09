package com.infrawrench.opencomputeruse.host;

import android.content.Context;
import android.os.Build;
import android.provider.Settings;

import java.net.Inet4Address;
import java.net.InetAddress;
import java.net.NetworkInterface;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

/**
 * This phone as a host: its name, the addresses other devices reach it at,
 * and what a device gets when it is given a key.
 */
final class Host {
    private Host() {
    }

    /** What this phone is called: "Astrid's Pixel", or its model. */
    static String name(Context c) {
        String n = Settings.Global.getString(c.getContentResolver(), Settings.Global.DEVICE_NAME);
        if (n == null || n.trim().isEmpty()) {
            n = Build.MODEL;
        }
        return n.trim();
    }

    /** An address and the network it's on ("Tailscale", "wlan0"). */
    static final class Address {
        final String ip;
        final String network;
        final boolean tailscale;

        Address(String ip, String network, boolean tailscale) {
            this.ip = ip;
            this.network = network;
            this.tailscale = tailscale;
        }
    }

    /** The phone's IPv4 addresses, Tailscale's (100.64.0.0/10) first. */
    static List<Address> addresses() {
        List<Address> out = new ArrayList<>();
        try {
            for (NetworkInterface ni : Collections.list(NetworkInterface.getNetworkInterfaces())) {
                if (!ni.isUp() || ni.isLoopback()) {
                    continue;
                }
                for (InetAddress a : Collections.list(ni.getInetAddresses())) {
                    if (!(a instanceof Inet4Address)) {
                        continue;
                    }
                    byte[] b = a.getAddress();
                    boolean ts = (b[0] & 0xff) == 100 && (b[1] & 0xc0) == 64;
                    Address addr = new Address(a.getHostAddress(),
                            ts ? "Tailscale" : ni.getName(), ts);
                    if (ts) {
                        out.add(0, addr);
                    } else {
                        out.add(addr);
                    }
                }
            }
        } catch (Exception ignored) {
        }
        return out;
    }

    /** The URL to suggest: Tailscale's address, else the first one. */
    static String suggestedUrl(int port) {
        List<Address> a = addresses();
        String ip = a.isEmpty() ? "PHONE-ADDRESS" : a.get(0).ip;
        return "http://" + ip + ":" + port;
    }

    /** What a device sets up with a new key: shown once. */
    static final class Issued {
        final Keys.Device device;
        final String key;
        final String hostEntry;
        final String skill;
        final String prompt;

        Issued(Context c, Keys.Issued i) {
            device = i.device;
            key = i.key;
            hostEntry = Skill.hostEntry(Skill.slug(name(c)), i.device.url, i.key);
            skill = Skill.renderGeneric(c);
            prompt = Skill.agentPrompt(name(c), skill, hostEntry);
        }
    }

    static Issued generate(Context c, String deviceName, String url) throws Exception {
        return new Issued(c, Keys.add(c, deviceName, url));
    }

    static Issued regenerate(Context c, String id, String url) throws Exception {
        return new Issued(c, Keys.regenerate(c, id, url));
    }
}
