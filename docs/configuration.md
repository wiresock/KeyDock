# Configuration reference

## How it works

* A hidden window registers for keyboard device-interface notifications
  (`RegisterDeviceNotificationW` with `GUID_DEVINTERFACE_KEYBOARD`), resume from
  sleep (`WM_POWERBROADCAST` / `PBT_APMRESUMEAUTOMATIC`) and session unlock or
  (re)connect (`WTSRegisterSessionNotification`).
* Every event pushes out a short timer (trailing debounce, 500 ms by
  default). When it fires, KeyDock enumerates the keyboards that are present
  right now (`CM_Get_Device_Interface_ListW`) and evaluates the configuration
  from scratch, so late or out-of-order events cannot leave it in a wrong
  state. Each event is recorded with its type and time as it arrives, also
  while KeyDock is busy changing layouts, and the evaluation runs at the
  latest of several independent deadlines: the debounce; 2 s after resume
  or unlock (`resume_delay_ms`); after a keyboard disappears, until the
  keyboards have been unchanged for 2.5 s (`removal_delay_ms`), so a dock or
  hub that drops its devices for a moment does not cause two switches; and
  after a failure, the retry back-off (1, 2, 5, 10 and 30 seconds, then
  KeyDock waits for the next event). A later, shorter request never shortens
  one of these. **Re-apply now**, Save/Apply and resuming from **Paused**
  skip the debounce and the back-off, but wait for a running removal
  hold-off or resume delay (the keyboards are still settling).
* If a keyboard interface is present but part of its identity cannot be read
  yet (typically right after wake or a dock reset), and that missing data
  could change which profile applies, KeyDock keeps the current layouts and
  samples again every second (up to 15 s for each interface, and at most
  60 s in all, however many keyboards keep appearing incomplete one after
  the other; this does not use up the failure retries); otherwise, or after
  that, it leaves that interface out. An interface left out is not waited
  for again while it stays present and incomplete (also when docking and
  undocking makes it matter again), but a keyboard that arrives later gets
  its own wait.
* Layouts are added and removed with the documented `InstallLayoutOrTip`
  function from `input.dll`, and compared with the layouts enabled for the
  user (`EnumEnabledLayoutOrTip`, the list `InstallLayoutOrTip` edits; a layout
  that is only loaded in the session, for example by an application, does not
  count). If that list cannot be read reliably, KeyDock changes nothing and
  tries again. New layouts are always installed first and confirmed enabled;
  right before removing anything KeyDock reads the list again and removes
  nothing unless every layout of the profile is still enabled; afterwards it
  reads the list back to confirm. So you are never left without your
  layouts, and a change that did not take is reported and retried. Layouts
  that are already in the desired state are not touched; nothing happens
  when the selected profile has not changed.
* The `activate` layout becomes the default input language
  (`SPI_SETDEFAULTINPUTLANG`, read back to confirm), and the window with the
  keyboard focus is asked to switch to it (`WM_INPUTLANGCHANGEREQUEST`). The
  layout must resolve to exactly its input locale (HKL); KeyDock never
  switches to another layout of the same language instead. The request to the
  window is best effort: an application may ignore it, and Windows refuses it
  for an elevated window, which is reported.
* Removing a layout from the user's list does not unload it from the running
  session while any thread still has it active (`UnloadKeyboardLayout` fails
  for an input locale that is in use), and such a layout stays in the language
  switcher until the next sign-in. So before removing layouts KeyDock checks
  its own thread, the default input language and the thread of every top-level
  window (`GetKeyboardLayout`). Anything that uses a layout being removed is
  moved to a remaining layout first (the profile's `activate` layout, otherwise
  the first remaining one), even when the profile does not switch layouts:
  its own process with `ActivateKeyboardLayout`, other applications with a
  posted `WM_INPUTLANGCHANGEREQUEST`, the default with `SPI_SETDEFAULTINPUTLANG`.
  KeyDock waits up to 1.5 s for that, removes the layouts and unloads any that
  are still loaded (`UnloadKeyboardLayout`). Whatever is still loaded after
  that is logged as a warning, together with the processes that still use it,
  and reported in the tray; it is unloaded at the next change if possible, and
  not loaded again after the next sign-in because it is no longer enabled.
* Adding and removing layouts can make Windows move a language to the end of
  the user's language list (`InstallLayoutOrTip` rewrites the whole
  configuration). Without a display-language override, the first language in
  that list is the Windows display language, so this could switch Windows to
  another language. By default (`language_order = "report"`, since 0.3.5)
  KeyDock only reports such a change in the tray and the log; it never
  writes the language list itself. Note that this does not stop the layout
  changes you configured from changing the list: `InstallLayoutOrTip` can
  still move a language, add one, or remove one whose last layout is
  removed. To avoid any change to the list, keep all layouts in every
  profile and switch with `activate` only.

  With `language_order = "restore"` (opt-in) KeyDock puts the order back
  with the documented `Set-WinUserLanguageList` cmdlet, which writes the
  *whole* list (input methods, spelling, handwriting and more), and only
  when it can tell that nothing else changed:
  * before changing layouts it takes a snapshot of everything under
    `HKCU\Control Panel\International\User Profile` (if that cannot be read,
    it changes nothing and tries again);
  * afterwards every difference must be explained by the layouts it added or
    removed, and a language whose layouts changed may only have moved to the
    end of the list (what Windows does); anything else (you reordered
    languages, changed an input method or another language setting
    meanwhile) is a conflict, and the order is not touched but reported;
  * right before PowerShell starts the settings must still be exactly as
    after the change, and the script itself checks the languages, their order
    and their input methods once more immediately before it writes;
  * afterwards KeyDock reads everything back: the order must be the one
    asked for and nothing else may have changed.
  The result (restored, not restored because of a conflict, failed, timed
  out) is shown in the tray and returned by `--once`. The script uses only
  cmdlets, so it also works under Constrained Language Mode (WDAC/AppLocker),
  and needs no execution-policy override. Two things remain possible with
  the opt-in, which is why it is not the default: if you move the very
  language whose layouts KeyDock is changing to the end of the list at the
  same moment, that looks exactly like Windows' side effect and is undone;
  and a change made by another program in the fraction of a second between
  the script's last check and its write can be lost (Windows has no way to
  update this list conditionally).

  **Migration from 0.3.4:** a `config.toml` without `language_order` used
  to restore; it now only reports. Add `language_order = "restore"` to keep
  the old behaviour. A file that already says `"restore"` or `"report"` is
  unchanged.
* PowerShell is created inside a job object that ends it when KeyDock ends
  in any way (Exit, sign-out, an installer closing or terminating KeyDock,
  a crash), and inherits nothing but its three standard handles; it is
  stopped after 30 s. While it runs KeyDock makes no layout changes (events
  are handled afterwards) and then checks that the layouts still match the
  profile. **Exit** waits up to 3 s for a running restore (sign-out 1.5 s),
  then cancels it and waits up to 6 s more for it to end; a restore that is
  waiting for the writer lock when KeyDock exits does not start. A restore
  that did not finish is reported at the next start. A new KeyDock stops one
  that an old instance left running and changes nothing until it is
  confirmed gone; a restore job that exists but cannot be opened or queried
  counts as still running. After an upgrade from 0.3.2 or 0.3.3, whose
  restore ran outside any job, a new KeyDock looks for PowerShell processes
  started in the two minutes before it whose parent is known to have ended
  (a parent it cannot open does not count), and waits for those whose
  command line is exactly a restore of KeyDock 0.3.2 or 0.3.3 (that
  version's PowerShell switches and its whole script; a fragment of it in
  a comment or in another script does not count), rather than killing
  processes it cannot prove are its own. It records them
  (`langorder-restore.<session>.legacy-writers`), so a restarted KeyDock
  keeps waiting. Only proof ends this: the job is gone or empty, each such
  process has exited or its ID now belongs to another process; a process
  that cannot be checked, or a record that cannot be used, keeps blocking
  and is checked again. Time never does: after 60 s the tray reports the
  restore as needing attention (with the process IDs) and keeps checking
  every 30 s, and applies the profile as soon as it has ended; `--once`
  waits up to 75 s and otherwise exits with code 1, having changed nothing.
  A PowerShell process found anew whose command line cannot be read is
  waited for at most 60 s (it is probably not KeyDock's) and one with any
  other command line is ignored. Exiting KeyDock does not wait for such a
  process. These checks cover this session; a process an older KeyDock left
  in another session of yours is not seen (the per-user lock below only
  excludes KeyDock 0.3.4 and later).
* A record written by KeyDock 0.3.7 lists every orphaned PowerShell process
  it found, KeyDock's or not: each one still running is checked by its
  command line again. One that is not a KeyDock restore is no longer waited
  for; one whose command line cannot be read keeps blocking (and is checked
  again) until it can be read or the process has ended. A record that
  exists but cannot be used keeps blocking as well: unreadable, not a
  KeyDock record, or cut off as an interrupted write of 0.3.7 can leave it
  (empty, a last line without its line end, or a header without any
  process). KeyDock reads it again on every check and uses it as soon as it
  is complete, but never deletes it by itself: neither the file's time nor
  the clock proves that the process it named has ended. To recover, restart
  Windows (that ends every process an older KeyDock left), then delete
  `%LOCALAPPDATA%\KeyDock\langorder-restore.<session>.legacy-writers`;
  KeyDock continues at its next check.
  A changed display language
  takes effect at the next sign-in either way. To pin the display language
  regardless of the order, use `Set-WinUILanguageOverride` (or **Settings >
  Time & language > Language & region > Windows display language**).
* Removing the last layout of the *first* language removes that language
  from the list (and without an override, Windows then uses another display
  language from the next sign-in); KeyDock reports this.
* Remote Desktop and other software keyboards (`ROOT\...`, `TERMINPUT_BUS`,
  `RDP_KBD`) are ignored.
* In a Remote Desktop or other non-console session KeyDock changes nothing
  (the tooltip says "remote session: inactive", `--once` exits with code 3):
  the keyboards it sees are those of the remote computer, not the one you
  type on, while the layouts are settings of your account that also apply
  when you are back at the console. The check uses the configuration as it
  is on disk at that moment, so editing `remote_sessions` takes effect at the
  next evaluation. It checks again when the session is back on the console.
  Set `remote_sessions = true` to change layouts in remote sessions too. If
  you are signed in more than once with KeyDock running in each session, only
  one of them changes your layouts at a time (a per-user lock, usable by both
  elevated and normal processes of your account); the other retries. If
  KeyDock cannot establish that lock at all, it changes nothing and reports
  it, rather than writing without it. This includes a process whose token
  carries your account only as a deny-only group (for example a filtered or
  restricted token, or an AppContainer); the log then names the token's
  account, integrity level and restrictions and the lock's security
  descriptor. The lock is taken before KeyDock reads the layouts it is
  about to change, and the previous-restore checks above are repeated while
  it is held. Between applying layouts and restoring the order the lock is
  released for a moment; the restore takes it again and checks the whole
  language state afresh before writing.

## Files

| What | Where |
|------|-------|
| Configuration | `%APPDATA%\KeyDock\config.toml` (written by the settings window; until it exists KeyDock changes nothing and opens Settings on start) |
| Built-in keyboards | `%APPDATA%\KeyDock\state.toml` (optional; written only by `--remember-builtin`, and on first run by versions up to 0.3.2) |
| Log | `%LOCALAPPDATA%\KeyDock\keydock.log` (rotated to `keydock.log.old` at 1 MiB) |
| Restore in progress | `%LOCALAPPDATA%\KeyDock\langorder-restore.<session>.pending` (exists only while a language-order restore runs in that session, or after one was interrupted) |
| Older restores waited for | `%LOCALAPPDATA%\KeyDock\langorder-restore.<session>.legacy-writers` (first line `keydock-legacy-writers 2`, then process ID, start time and `confirmed` or `unverified` for each older KeyDock's restore still running; exists only while there is one; a file without that first line was written by 0.3.7) |

## Configuration

Everything the settings window does is stored in a plain TOML file, which you
can also edit by hand. The configuration is re-read on every evaluation; use
**Re-apply now** in the tray menu (or reconnect the keyboard) after editing.

If the file cannot be read (for example after a typo, or because it is
locked), KeyDock keeps the last configuration that worked (at startup: none,
so it changes nothing), shows a warning in the notification area, and the
settings window names the error. Saving from the settings window then asks
before it replaces the file and keeps the old one as `config.toml.bak`.
KeyDock writes `config.toml` and `state.toml` through a temporary file of its
own, so an interrupted save never leaves a truncated file.

```toml
# Delay after the last device event before re-evaluating (ms).
debounce_ms = 500
# Delay after resume from sleep or session unlock (ms).
resume_delay_ms = 2000
# After a keyboard is removed, how long the keyboards must stay unchanged
# before KeyDock switches (ms).
removal_delay_ms = 2500
# Also change layouts in Remote Desktop sessions.
remote_sessions = false
# When a layout change reorders the language list: "report" (default) only
# reports it; "restore" (opt-in) puts the order back when nothing else changed.
language_order = "report"
# Notification-area icon.
tray = true
# Balloon notification when the selected profile changes.
notify = false
# Count ACPI (PS/2) keyboards and keyboards in the local-machine container as built-in.
builtin_heuristics = true

# Manual classification (the Built-in / External / Ignore buttons).
builtin = [ { instance_id = "ACPI\\MSFT0001\\4&1A2B3C4D&0" } ]
external = [ { container_id = "{8C1C3E4A-0000-0000-0000-000000000000}" } ]
# Keyboards to disregard entirely (e.g. a YubiKey, which is also a "keyboard").
ignore = [ { vid = "1050" } ]

# Profiles are evaluated in file order; the first one whose `when` matches any
# present keyboard wins. `when` is one matcher table or an array of them (OR).
[[profile]]
name = "Docked, US keyboard"
layouts = ["0419:00000419", "0409:00000409"]
activate = "0409:00000409"
when = [ { vid = "03F0", pid = "2041" }, { vid = "03F0", pid = "3341" } ]

# The profile managed by the settings window.
[[profile]]
name = "External keyboard"
layouts = ["0419:00000419", "0409:00000409"]
activate = "0409:00000409"
when = { any_external = true }

# Used when no profile matches (laptop alone).
[default]
layouts = ["0419:00000419", "0409:0000040A"]
activate = "0409:0000040A"
```

(The IDs above are examples; use the settings window or `--list-keyboards` to
see yours.)

### What the settings window manages

* The **Laptop only** panel is `[default]`; the **External keyboard connected**
  panel is the `[[profile]]` named `"External keyboard"`. Its `when` is
  `{ any_external = true }` or one matcher per ticked device.
* The device buttons edit the `builtin`, `external` and `ignore` lists
  (**Automatic** removes the device from all three; a broader rule you wrote
  yourself, such as a VID-only or `name` rule, is kept, and the status line
  says so).
* A device is stored by its `container_id` when that stays the same on every
  port (Bluetooth, USB devices with a serial number), by `vid` and `pid` for
  USB devices without a serial number (Windows gives those a new container ID
  on every port or dock, so the rule then covers every keyboard of that
  model; the window says so), and by `instance_id` for internal devices. With
  **This port only** ticked, a USB device without a serial number is stored by
  its container ID on its current port instead; if a model-wide rule covered
  it, every other connected keyboard that rule covered gets a rule of its own
  (its port, or its stable container ID for a unit with a serial number), so
  it keeps its mark; the status line names any keyboard whose mark could not
  be kept, and keyboards of that model that are not connected are no longer
  covered. Entries written by older versions, and rules you wrote yourself,
  are kept and still recognised.
* Saving keeps every other profile and every other entry in those lists. If the
  `"External keyboard"` profile already exists it is replaced in place;
  otherwise it is appended after your own profiles, so hand-written profiles
  keep priority. Profiles that are not managed by the window are listed under
  the panels.
* The file is rewritten on save, so **comments and formatting are not kept**.
  The window shows the file as it is when it opens; if the file is changed or
  deleted meanwhile (for example in a text editor or by a sync client),
  saving asks before it replaces those changes (or creates the file again)
  and keeps the version you agreed to replace as `config.toml.bak`. If the
  file changes again while that question is open, KeyDock asks again. The
  file is compared once more right before it is replaced; an editor that
  saves in the same instant can still lose its change (Windows has no
  conditional file replace that every program honours). The previous
  `config.toml.bak` is only replaced when the save goes through: a save you
  cancel, or one that fails, leaves it as it was.
* If a mode is not configured yet, both checklists start with the layouts that
  are currently loaded.

### Layouts

Each layout is `"<LANGID>:<KLID>"` in hexadecimal, the format `InstallLayoutOrTip`
uses. The language identifier and the keyboard layout identifier are separate:
`"0C0A:0000040A"` is the Spanish layout under the Spanish (Spain) language,
`"0409:0000040A"` would be the Spanish layout under English (US).

| Entry | Meaning |
|-------|---------|
| `0409:00000409` | English (United States) - US |
| `0419:00000419` | Russian - Russian |
| `0C0A:0000040A` | Spanish (Spain) - Spanish |
| `0407:00000407` | German (Germany) - German |
| `040C:0000040C` | French (France) - French |

KLIDs are the key names under
`HKLM\SYSTEM\CurrentControlSet\Control\Keyboard Layouts`. Text services (IMEs
identified by GUIDs) are not supported yet.

**Managed layouts.** KeyDock only ever installs or removes layouts that appear
in at least one profile or in `[default]`. Anything else you have installed is
left alone.

### Matching keyboards (`when`, `builtin`, `external`, `ignore`)

All keys given in one matcher table must match the same keyboard; a `when`
array matches if any of its tables does. `any_external` is only allowed in
`when`.

| Key | Matches |
|-----|---------|
| `vid`, `pid` | USB vendor/product ID (4 hex digits). Matched in USB form (`VID_046D&PID_C52B`) and in Bluetooth HID form (`VID&0002046D_PID&C52B`, BLE `VID&02046D_PID&C52B`), so the same rule covers a keyboard used over a cable and over Bluetooth if it reports the same IDs. `pid` requires `vid`. |
| `container_id` | The physical device container, e.g. `"{8C1C3E4A-...}"`. All functions of one physical device share it. |
| `name` | Case-insensitive substring of the device name. |
| `instance_id` | Case-insensitive substring of the device instance ID. |
| `any_external` | `true`: any keyboard that is not built in. |

**Built-in or external?** A keyboard is classified in this order: virtual
(Remote Desktop, software keyboards) -> `ignore` -> `external` -> `builtin` ->
`state.toml` -> heuristics -> otherwise external.

KeyDock does not record the keyboards present on first run: on a laptop
that is set up while docked, that would make the external keyboard
"built-in". Mark a keyboard **Built-in** in the settings window instead (this
writes the `builtin` list), or run `keydock.exe --remember-builtin` with only
the built-in keyboard connected to record it in `state.toml`. Versions up to
0.3.2 wrote `state.toml` on first run; keyboards it lists are shown as
"Built-in (remembered)" in the settings window. If that includes an external
keyboard, mark it **External** or delete `state.toml`. With `builtin_heuristics = true` (the default), ACPI-enumerated
(PS/2) keyboards and devices in the local-machine container
`{00000000-0000-0000-FFFF-FFFFFFFFFFFF}` also count as built-in.

## Finding device IDs

The settings window shows the devices. For full details run

```
keydock.exe --list-keyboards
```

writes every present keyboard interface with its name, instance ID, VID/PID,
container ID, hardware IDs, bus-reported product name and classification
(built-in / external / virtual / ignored, plus "(also exposes a mouse
interface)" where that applies), the currently loaded input locales, the layouts enabled for the user and the profile that would be
selected, to the log file. If started from a console it prints the same output
there. Because `keydock.exe` is a GUI-subsystem program, `cmd.exe` does not wait
for it; use `start /wait keydock.exe --list-keyboards` in `cmd`, or
`.\keydock.exe --list-keyboards | Out-Host` in PowerShell.

A typical laptop shows several keyboard interfaces (the built-in keyboard, plus
"keyboard" collections of touchpads, hot-key controllers, receivers, and so on).
Use the entry that appears and disappears with your external keyboard.

## Command line

The command-line switches of the first version still work.

| Option | Effect |
|--------|--------|
| *(none)* | Run in the notification area (single instance). Opens Settings on first run, or in the already running instance. |
| `--background` | Run in the notification area without opening Settings (used by **Start with Windows**). |
| `--settings` | Open Settings (in the running instance if there is one). |
| `--list-keyboards` | List keyboards (see above) and exit. |
| `--once` | Evaluate and apply once (waiting up to 75 s for a language-order restore of a previous KeyDock), then exit. If KeyDock is already running, hands the request to it instead (only one process changes layouts); the running instance queues it in memory (several requests are combined) and evaluates it in the background with `config.toml` and the session policy as they are then. Exit code 0: done or nothing to do; 1: error, including a `config.toml` that cannot be used (also one that became unusable while `--once` waited) and a previous restore that is still running after 75 s; 2: applied, but something needs attention (see the log), including layouts that no longer match the profile after a language-order restore (they are read once more when the restore has ended, without changing anything); 3: nothing changed because this is a Remote Desktop or other non-console session; 4: handed to the running KeyDock, which evaluates it in the background (its tray icon and log show the result, also a `config.toml` that cannot be used or a remote session); 5: the running KeyDock is paused, nothing applied. |
| `--dry-run` | Log what would change but change nothing (combine with `--once` or the background mode). |
| `--remember-builtin` | Record the keyboards present now as built-in, then exit (exit code 1 if `state.toml` cannot be written). A running KeyDock uses the new list after it is restarted. |
| `--remove-autostart` | Remove the **Start with Windows** entry if it starts this copy (or a file that no longer exists), then exit. Used by the uninstaller. |
| `--config <path>` | Use another configuration file. |
| `--help` | Show help. |

## Autostart

Tick **Start with Windows** in the settings window and click **Save** or **Apply**. KeyDock
then writes the value `KeyDock` = `"<path to keydock.exe>" --background` under
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, so it starts at sign-in
straight into the notification area. Unticking removes the value. This needs no
administrator rights and, unlike a scheduled task, has no "AC power only" or
"stop after 3 days" defaults. If you move `keydock.exe`, open Settings and save
again so the path is updated. Windows' **Settings > Apps > Startup** page (or
Task Manager) can also turn the entry off; the checkbox then shows it as off.
Only you change that choice: ticking the box and saving turns the entry on
again, while saving with the box untouched, or KeyDock repairing a stale path
(below), keeps an entry that is turned off there off. If the entry is turned
on or off in Windows while the settings window is open, saving keeps that
newer choice and the box shows it.

There is one entry per user. The checkbox is ticked when the entry starts any
copy of KeyDock; saving with it ticked points the entry to the copy you are
running (the status line says so when it currently starts another copy). When
KeyDock starts and the entry points to a file that no longer exists (for
example a portable copy you replaced with the installed one), it moves the
entry to itself. An entry for another copy that still exists is left alone.

A Task Scheduler logon task still works if you prefer it (do not use both):

```powershell
$exe      = "$env:LOCALAPPDATA\Programs\KeyDock\keydock.exe"
$action   = New-ScheduledTaskAction -Execute $exe -Argument "--background"
$trigger  = New-ScheduledTaskTrigger -AtLogOn -User "$env:USERDOMAIN\$env:USERNAME"
$settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) `
              -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
Register-ScheduledTask -TaskName "KeyDock" -Action $action -Trigger $trigger -Settings $settings
```

## Installer

`installer/` contains a [WiX Toolset](https://wixtoolset.org/) 6 project that
builds a per-machine MSI for x64 and for ARM64:

* installs `keydock.exe` to `C:\Program Files\KeyDock` and adds a **KeyDock**
  Start menu shortcut; the last page offers **Launch KeyDock**;
* no license page (the wizard is WixUI_InstallDir without it);
* upgrades replace older versions (one UpgradeCode for both architectures),
  downgrades are refused;
* a running KeyDock is closed before files are replaced or removed (WiX
  `CloseApplication`: KeyDock exits cleanly on `WM_CLOSE` and on
  `WM_QUERYENDSESSION`/`WM_ENDSESSION`, which is also what Windows Restart
  Manager sends), without a reboot prompt;
* settings in `%APPDATA%\KeyDock` are not touched. On uninstall (not on an
  upgrade) the installed `keydock.exe --remove-autostart` removes the
  **Start with Windows** entry of the user who uninstalls, if it points to the
  installed copy. Entries of other users stay and are harmless: Windows skips a
  missing program.

Build on Windows with the .NET SDK (6 or newer) and Rust installed; WiX is
restored from NuGet:

```powershell
.\installer\build.ps1                     # x64 and ARM64
.\installer\build.ps1 -Architecture x64
.\installer\build.ps1 -Architecture arm64 -KeyDockExe .\\out\\keydock-aarch64.exe
```

The packages are written to `installer\out\KeyDock-<version>-x64.msi` and
`KeyDock-<version>-arm64.msi`. The version comes from `Cargo.toml`. Without the
script: `dotnet build installer\KeyDock.wixproj -c Release -p:Platform=x64`
(or `ARM64`) after `cargo build --release --target <triple>`.

## Building

Requires Rust 1.88 or newer.

On Windows:

```
cargo build --release --target x86_64-pc-windows-msvc
cargo build --release --target aarch64-pc-windows-msvc
```

Cross-compiling from Linux with [cargo-xwin](https://github.com/rust-cross/cargo-xwin)
(needs `clang` and `lld`):

```
rustup target add x86_64-pc-windows-msvc aarch64-pc-windows-msvc
cargo xwin build --release --target x86_64-pc-windows-msvc --target aarch64-pc-windows-msvc
```

`build.rs` embeds the icons from `assets/`, a version resource and the
application manifest (per-monitor DPI awareness v2, Common Controls 6) with
[embed-resource](https://crates.io/crates/embed-resource); no Windows SDK
headers are needed. The icons are generated by `tools/make_icons.py` (Pillow).
When cross-compiling, the build script may print a harmless "Compiler family
detection failed" warning from the `cc` crate.

The C runtime is linked statically (`.cargo/config.toml`), so no Visual C++
Redistributable is needed.

The platform-independent logic (configuration, ID matching, profile selection,
layout planning, KLID to HKL computation, device grouping and naming, the
settings model, autostart entry parsing) is in the library crate and its unit
tests run on any OS:

```
cargo test
```

## Known limitations

* The `activate` layout is requested for the window with the keyboard focus
  only (and becomes the default input for new windows; KeyDock checks that
  again after removing layouts, which can make Windows reset it). Other windows keep their current layout until they are switched or
  you use the normal layout hotkey; new windows use the default input
  language. Windows that use a layout being removed are all asked to switch
  (see How it works). Some applications (elevated windows, some games and
  console hosts) may ignore the request; a removed layout they keep using
  stays in the language switcher of this session, and the tray and log name
  the process.
* With the opt-in `language_order = "restore"`, restoring the order can undo
  a move of the very language whose layouts KeyDock is changing to the end of
  the list (it looks exactly like Windows' side effect), and can still lose a
  change another program makes
  to the language settings in the instant between the restore's last check
  and its write (see How it works; `language_order = "report"` avoids any
  such write).
* Removing the last layout of a language removes the language from the
  Windows language list (seen on Windows 10 22H2); adding a layout of it again
  adds the language at the end of the list.
* Bluetooth keyboards that only go to sleep may stay "present" to Windows, so
  KeyDock only reacts when Windows actually removes the device.
* Some Bluetooth keyboards report a different PID over Bluetooth than over USB;
  use separate rules or `name` / `container_id` in that case.
* Internal keyboards that are not ACPI-enumerated and not in the local-machine
  container must be marked **Built-in** once (or recorded with
  `--remember-builtin`).
* KeyDock runs per user session and only changes the current user's layouts.
  Layouts on the sign-in screen are not affected.
* A rule for a USB keyboard without a serial number matches every keyboard of
  that model unless it was marked with **This port only** (see "What the
  settings window manages"). A port rule stops matching when the keyboard is
  plugged into another port. Keyboards whose firmware reports a fixed
  container ID are treated like other serial-less keyboards; that case has
  not been tested with such hardware.
* Text-service (IME) profiles identified by GUIDs are not supported.
* The settings window manages only `[default]` and one external profile; use
  the file for more profiles. Saving drops comments.
