# The Windows panel hook

A DLL that apps opencomputeruse starts load into themselves, so their open
and save dialogs are handed to the agent instead of shown on screen. It is
the Windows counterpart of the macOS panel hook in
`packaging/macos/panelhook`, and speaks the same protocol, so the agent
answers the same way whichever platform it is on.

It is off by default. Turn it on with `windows_panel_hook` in `config.json`,
or `OCU_WINDOWS_PANEL_HOOK=1` for a machine with no config to change.

## Why it exists, when it should not be on

Every app's file dialog is already answerable without this, over UI
Automation — see `crates/ocu-windows/src/dialog.rs`. That path types into
the dialog's own file name box and presses its own button. It works for the
shell's dialogs in any app, needs no extra code in any process, and is what
the setting being off means.

The hook earns its keep only where UI Automation cannot reach:

- an app whose window stops pumping messages, so its dialog never answers a
  query made from another thread;
- an app with a file dialog of its own drawing, which is not on the
  accessibility tree;
- an app that must be told the answer without a dialog appearing at all.

Where it does not earn its keep, leaving it off costs nothing but the dialog
appearing. So it stays off unless someone wants it.

## How the code gets in

The app is created suspended and put in its kill-on-close Job Object before
it runs an instruction (`crates/ocu-windows/src/launch.rs`). That is the
window this uses:

1. `VirtualAllocEx` a slot in the target, and `WriteProcessMemory` the hook's
   path into it.
2. `NtQueueApcThread` on the target's primary thread, with `LoadLibraryW` as
   the routine and that slot as its argument.
3. Resume the thread, as the launch code does anyway. The APC runs.

An APC rather than `CreateRemoteThread` because a remote thread only starts
once the loader has initialised, which has not happened in a process that has
never been resumed. Queuing on the primary thread puts the load after the
loader is up and before the app's entry point, which is exactly what is
wanted.

**Nothing in the target is replaced or unmapped.** The hook is added to the
process, not substituted for its image. This is worth stating plainly,
because the obvious alternative is much worse: mapping a section over a
process's image and redirecting its entry point is process hollowing, the
signature endpoint protection watches hardest for, and none of it is needed
to add a library. `crates/ocu-windows/src/inject.rs` does not call
`NtCreateSection`, `NtMapViewOfSection` or `NtUnmapViewOfSection` at all.

One thing is deliberately leaked: the slot holding the path. `LoadLibraryW`
has read it by the time anything is asked, but nothing here can observe
that, and freeing too early is a use-after-free in the target. It is one
path per app started, in a process about to be thrown away.

## What it hooks

Two routes, because Windows has two file dialogs.

**The modern one.** `CoCreateInstance` is intercepted through the exe's
import table. When it is asked for `CLSID_FileOpenDialog` or
`CLSID_FileSaveDialog`, the real object is created and then its vtable is
swapped for a copy with `Show`, `GetResult` and `GetResults` replaced.
`Show` asks the agent and returns `S_OK` without anything appearing; the
shell takes that as the dialog having closed, and the app's next
`GetResult` is answered from `SHCreateItemFromParsingName`.

`IFileOpenDialogVtbl` is `IFileDialogVtbl` plus `GetResults` and
`GetSelectedItems`, so the wrapper's table is the longer one. A save dialog
only has the shorter table, so only its own slots are copied and only they
are ever called.

**The older one.** `GetOpenFileNameW` and `GetSaveFileNameW` from
comdlg32 are wrapped whole, and the caller's buffer is filled in the shell's
own format — one path, or the folder followed by each name, double-null
terminated, when `OFN_ALLOWMULTISELECT` is set.

Only imports are patched. An app that reaches for a dialog through a
delay-loaded import gets its own dialog, which is the fail-open outcome
everywhere else here.

## The protocol

The pipe name arrives in `OCU_PANEL_PIPE`, which `DllMain` reads and then
clears, so nothing the app starts inherits it. One JSON line each way:

```
->  {"pid":N,"kind":"open"|"save","multiple":bool,"folders":bool}
<-  {"paths":["C:\\a.txt"]}      pick these
<-  {"paths":[]}                 cancelled
```

`{"paths":[]}` is a cancel, reported as `HRESULT_FROM_WIN32(ERROR_CANCELLED)`
so the app sees what it sees when a person presses Cancel. No reply at all,
or a pipe that cannot be reached, is not a cancel: the real dialog is shown.

The agent's side is `crates/ocu-windows/src/hook.rs`. One named pipe per
agent, `\\.\pipe\ocu-panel-<pid>`, one dialog at a time per app, and a newer
dialog replaces an older one.

## Failing open

Every path that is not a clear answer shows the app its own dialog:

- no pipe, no answer, or a read that takes past 30 seconds;
- an allocation that fails while answering several paths;
- a `SHCreateItemFromParsingName` that fails, which returns the failure to
  the app rather than a wrong file;
- any COM call that fails.

Nothing in the hook can hang an app: the reply read polls with a deadline
rather than blocking. Nothing in it can crash an app by making it stop:
`GetOpenFileNameW` and friends are only reached through the DLL's own import
table, never the patched one.

## What will still go wrong

**Defender will flag this.** Loading a library into another process is one of
the most heavily observed things Windows does, signed or not. An unsigned
hook is quarantined outright on most non-development machines. The release
build signs it when `WINDOWS_CERT_PFX_BASE64` is set, with a timestamp
because a library loaded into other processes must keep validating, but
signing helps reputation, not detection.

**HVCI may block it.** With Memory Integrity on, which is the default on
Windows 11, the allocator and write calls the injector uses against a
process whose image is not itself signed are refused. On such a machine the
hook silently does not arrive, and the session falls back to UI Automation —
`describe` says so, and the setting is reported unmet.

Both are why this is opt-in and why the fallback exists.

## Building

Local, with mingw: `packaging/windows/panelhook/build.sh`. Releases use MSVC
in `.github/workflows/release.yml`.

The hook calls nothing but Windows. That is checked rather than asserted:
the local build looks at the compiled object's undefined symbols and fails if
anything but a Win32 import or a uuid.lib GUID is left, and the release build
does the same over `dumpbin /dependents`. A C runtime inside someone else's
process is a dependency this does not need to take, and adding one silently
is the kind of thing that only shows up in the field.

## What has not been tested

The Rust half is checked on every build, for the Windows target. The C has
been compiled and linked for Windows, and reviewed, but **it has never been
run.** Before this ships it needs a Windows machine and:

- an app that opens a modern dialog, picked and cancelled, single and
  multiple;
- an app that opens a legacy `GetOpenFileName` dialog, picked and cancelled;
- the agent killed mid-dialog, to confirm the app sees a cancel and not a
  hang;
- the agent not running at all, to confirm the app still shows its own
  dialog and still works;
- a session ending while a dialog is held, to confirm the app is cancelled
  and the process is gone;
- at least one machine with Defender and one with HVCI, to see what actually
  happens.