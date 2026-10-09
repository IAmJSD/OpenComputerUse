# OpenComputerUse for Android

One APK, two jobs:

- **The shell helper** (`src/.../shell/`). The desktop pushes the APK to
  `/data/local/tmp` and runs `shell.Helper` there with `app_process` as adb's
  shell user, to give phone sessions a private virtual display. Nothing is
  installed for this.
- **The app** (`src/.../host/`). It makes the phone a host by itself: other
  devices drive its apps over HTTP, as they drive a desktop's "Other devices"
  server, with the same API, keys and skill. An accessibility service reads
  the screen and taps, swipes and types. A foreground service serves.

## Build

```sh
android/build.sh            # → dist/OpenComputerUse.apk
```

It needs a JDK and an Android SDK with build-tools and a platform of API 30 or
newer. No Gradle is involved and nothing is downloaded. It signs with
`$OCU_ANDROID_KEYSTORE` (plus `$OCU_ANDROID_KEYSTORE_PASSWORD` and
`$OCU_ANDROID_KEY_ALIAS`) when set; otherwise it uses a debug key it makes in
`android/.keystore`. Keep one key across releases, or Android won't install an
update over the old app.

## Set up a phone

On the phone: install the APK, open OpenComputerUse, and turn it on in
Accessibility settings. Then turn on **Serve over HTTP** and tap
**Generate key…** for each device that will drive it. The key shows once,
with the prompt to paste into that device's agent.

From a computer over adb:

```sh
adb install -r dist/OpenComputerUse.apk
adb shell settings put secure enabled_accessibility_services \
    com.infrawrench.opencomputeruse/.host.HostAccessibilityService
adb shell settings put secure accessibility_enabled 1
R=com.infrawrench.opencomputeruse/.host.SetupReceiver
adb shell am broadcast -n $R --es cmd serve            # [--ei port 8642]
adb shell am broadcast -n $R --es cmd generate --es name "'Work laptop'" \
    --es url http://100.101.102.103:8642               # the key, entry and prompt, once
adb shell am broadcast -n $R --es cmd status
```

The receiver takes `serve`, `stop`, `generate`, `regenerate` (`--es id`),
`remove` (`--es id`) and `status`. Only the shell can send to it, because it
requires `android.permission.DUMP`. Note the nested quotes: the device's shell
re-splits the line.

## What a driving device gets

It gets the desktop's base tools: `start_session` (a package, an activity or
the app's name), `screenshot`, `get_ui_tree`, `click`, `drag`, `scroll`,
`type_text`, `press_key`, `set_value`, `element_action`, `wait` and the rest.

- Coordinates are points (dp), and screenshots are scaled to match.
- `press_key` takes the phone's buttons: home, back, recents, notifications,
  power, volume_up and volume_down.
- Typing goes into the focused field through accessibility, so any text works.

Limits:

- **This app's own screen.** It manages keys, so it is never reachable over
  HTTP. Starting a session on it fails, and it is left out of trees. While it
  is in front, actions other than the phone's buttons are refused. Its window
  is secure, so screenshots of it come out black.
- **Sensitive windows.** Android 14 and later hide some windows from
  accessibility services that aren't assistive tools, such as permission
  prompts. Those windows appear in screenshots and take taps at coordinates,
  but their elements aren't in the tree.
- **No TLS.** Serve over Tailscale or another network you trust.
