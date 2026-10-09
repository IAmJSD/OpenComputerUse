package com.infrawrench.opencomputeruse.host;

import android.content.Context;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;

import com.infrawrench.opencomputeruse.R;

/**
 * What a device needs to drive this phone, the same as a desktop gives
 * (src/remote/skill.rs): the generic skill (how to call every tool over
 * HTTP, the same for every host), this phone's entry for the device's
 * hosts file (its URL and the device's key), and a prompt holding both for
 * the device's agent to set up.
 */
final class Skill {
    private Skill() {
    }

    static final String HOSTS_FILE = "~/.config/opencomputeruse/hosts.yaml";
    static final String GENERIC_SKILL_NAME = "opencomputeruse-remote";

    private static final String PROMPT = "Set yourself up to drive apps on %computer%, a phone, through its OpenComputerUse HTTP API. Two steps:\n"
            + "\n"
            + "1. Add the lines between BEGIN HOSTS and END HOSTS to %HOSTS_FILE%, under its `hosts:` line. If the file doesn't exist, create it with `hosts:` as its first line. If it already has an entry with the same name, replace that entry. The entry holds this phone's key, so make the file readable by this user only (chmod 600 on macOS and Linux) and don't repeat the key back to me.\n"
            + "2. Save everything between the BEGIN SKILL and END SKILL lines, exactly as it is, as `%GENERIC_SKILL_NAME%/SKILL.md` in your skills folder, replacing any older copy:\n"
            + "   - Claude Code: ~/.claude/skills/%GENERIC_SKILL_NAME%/SKILL.md\n"
            + "   - Codex: ~/.codex/skills/%GENERIC_SKILL_NAME%/SKILL.md\n"
            + "   - OpenCode: ~/.config/opencode/skills/%GENERIC_SKILL_NAME%/SKILL.md\n"
            + "   - Anything else: wherever you keep skills, or your instructions file if you have no skills.\n"
            + "\n"
            + "Then tell me what you changed, and that a new session picks the skill up.\n"
            + "\n"
            + "-----BEGIN HOSTS-----\n"
            + "%entry%-----END HOSTS-----\n"
            + "\n"
            + "-----BEGIN SKILL-----\n"
            + "%skill%-----END SKILL-----\n";

    /**
     * The skill that works for every host in the hosts file: no key in it.
     * It is res/raw/skill.md, which the desktop writes
     * (`opencomputeruse skill`) and its tests keep current, so a device gets
     * the same skill from a phone as from a computer.
     */
    static String renderGeneric(Context c) {
        try (InputStream in = c.getResources().openRawResource(R.raw.skill)) {
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            byte[] buf = new byte[8192];
            int n;
            while ((n = in.read(buf)) > 0) {
                out.write(buf, 0, n);
            }
            return new String(out.toByteArray(), StandardCharsets.UTF_8);
        } catch (IOException e) {
            throw new IllegalStateException("the skill is missing from the app", e);
        }
    }

    /** What to add under `hosts:` in the driving device's hosts file. */
    static String hostEntry(String slug, String url, String key) {
        return "  " + slug + ":\n    url: \"" + url + "\"\n    key: \"" + key + "\"\n";
    }

    /** A message to paste into the device's agent, which sets it all up. */
    static String agentPrompt(String computer, String skill, String entry) {
        return PROMPT.replace("%computer%", computer)
                .replace("%HOSTS_FILE%", HOSTS_FILE)
                .replace("%GENERIC_SKILL_NAME%", GENERIC_SKILL_NAME)
                .replace("%entry%", entry)
                .replace("%skill%", skill);
    }

    /** "Astrid's Pixel" → "astrid-s-pixel". */
    static String slug(String s) {
        StringBuilder out = new StringBuilder();
        for (char c : s.toCharArray()) {
            if (c < 128 && Character.isLetterOrDigit(c)) {
                out.append(Character.toLowerCase(c));
            } else if (out.length() > 0 && out.charAt(out.length() - 1) != '-') {
                out.append('-');
            }
        }
        String r = out.toString();
        while (r.endsWith("-")) {
            r = r.substring(0, r.length() - 1);
        }
        return r.isEmpty() ? "computer" : r;
    }
}
