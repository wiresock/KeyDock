# Changelog

## 0.3.12 - 2026-11-21

Initial public release. See [release notes](docs/releases/0.3.12.md) for installation and compatibility.

Fixes from the final review of 0.3.11.

### Fixed

- The result of a Save/Apply is shown only in the Settings window that
  submitted it: when that window was closed and Settings opened again
  before the apply finished, the new window no longer shows the old result.
  A newer save, also one made while paused, replaces a pending one.
- A failure of the first check of the default input no longer stays as a
  warning when the check after all layout changes finds (or sets and reads
  back) the right layout; `--once` then exits 0. The first failure stays in
  the log; a refused switch of the active window and a default input that
  cannot be set are still reported.
- Saving Settings no longer sets up a mode you did not configure. A mode
  missing from config.toml (for example no "External keyboard connected"
  profile) used to start with every loaded layout ticked, and any save,
  also one that only changed an option, wrote it as a profile; that made
  every loaded layout managed, so the other mode could remove a layout such
  as French that KeyDock was never told about. Such a mode now starts with
  nothing ticked and stays absent until you tick its layouts. First run
  still starts both modes from the loaded layouts. A profile written this
  way by an earlier version stays as it is; remove it from config.toml or
  untick the layout in it.

## 0.3.11 - 2026-11-21

Fixes from the release audit of 0.3.10 (KD-01, KD-02).

### Fixed

- `--once` with `language_order = "restore"`: when the restore it started
  has ended, the enabled layouts are read once more (read only) and
  compared with the profile it applied. A layout that something else
  removed meanwhile, another managed layout, or a list that cannot be read
  makes the exit code 2 instead of 0; the restore checks only its own
  snapshot (KD-01).
- Settings no longer keeps "Saved, but applying failed" after a Save/Apply
  that succeeded on a later attempt: while a keyboard is still being set up
  or a retry is scheduled it says so, then shows the real result; only
  retries that are used up are shown as a failure (KD-02).
- The `activate` layout stays the default input for new windows also when
  the same change removes a layout: Windows 10 resets the default input
  when a layout is removed after KeyDock switched, so KeyDock checks it once
  all changes are done and sets it again (found in the 0.3.11 release
  validation).

## 0.3.10 - 2026-11-21

Fixes from the eighth engineering review of 0.3.9 (S8-01, S8-02).

### Fixed

- A damaged or unreadable record of older restores is no longer deleted
  because its file time looks older than the last start of Windows: that
  estimate follows the adjustable clock, and a file time proves nothing
  about the process. Such a record keeps blocking until it can be used; to
  recover, restart Windows and delete it (S8-01).
- A record cut off by an interrupted write no longer parses: a versioned
  header without any process, and a last row without its line end (whose
  start time may be partial and was taken for a reused process ID), keep
  blocking like other damaged records (S8-02).

## 0.3.9 - 2026-11-21

Fixes from the seventh engineering review of 0.3.8 (S7-01 to S7-04).

### Fixed

- After an upgrade from 0.3.7, whose record of older restores listed every
  orphaned PowerShell it found, each recorded process still running is
  attributed again by its command line: one that is not a KeyDock restore
  is no longer waited for, one whose command line cannot be read keeps
  blocking until it can, and a confirmed one blocks until it has ended. The
  record now has a versioned format that says which processes were
  confirmed (S7-01).
- An existing empty record (an interrupted 0.3.7 write can leave one) no
  longer counts as "no previous writer": like an unreadable record it
  blocks, is read again, and is discarded by KeyDock only when it was last
  written before Windows last started; otherwise it is deleted by hand once
  no older restore can be running (S7-03).
- An older KeyDock's restore is recognized by the exact PowerShell switches
  and script of 0.3.2 or 0.3.3, for processes found anew and recorded ones
  alike; the setter in a comment, in quoted data or in another script no
  longer counts (S7-02).
- Tests: every test that runs the restore worker or holds the test writer
  lock takes the same guard, so the default parallel run no longer races on
  them (S7-04).

## 0.3.8 - 2026-11-21

Fixes from the sixth engineering review of 0.3.7 (S6-01 to S6-06).

### Fixed

- An unrelated PowerShell process no longer blocks layout changes: only
  one whose command line is KeyDock's restore script counts as an older
  KeyDock's restore (0.3.2/0.3.3 upgrade) and is waited for until it has
  exited. One whose command line cannot be read is waited for at most 60 s,
  others are ignored, and a parent that cannot be opened no longer counts as
  ended (S6-01).
- The record of older restores KeyDock waits for no longer loses them: an
  unreadable record, a process that cannot be opened, queried or waited
  for, keep blocking and are checked again; only an exit, a missing
  process or an ID that now belongs to another process ends the wait. The
  record is replaced atomically, only when the set changes, and a failure
  to update it is reported and retried (S6-02).
- Every language-order restore runs in a fresh job: Windows can refuse a
  new process in a job that was just terminated, even when it is empty, so
  a second restore right after another could fail with "access denied"
  (S6-03).
- A keyboard that KeyDock gave up waiting for stays given up on while it is
  present and incomplete, also when docking and undocking make it matter
  again; a readable sample or its removal clears that (S6-04).
- "Waiting for a previous restore" is shown as the state of the
  evaluation and goes away with the next evaluation that runs (also an
  empty plan or no matching profile); it no longer stays as a restore
  result. A restore that was interrupted is still reported (S6-05).
- Tests: the restore worker's test (and every test) takes the writer lock
  under a name of its own with the production descriptor, never the real
  per-user lock (S6-06).

## 0.3.7 - 2026-11-21

Fixes from the fifth engineering review of 0.3.6 (F5-01 to F5-06).

### Fixed

- A previous KeyDock's restore no longer stops counting after 60 s: only
  proof that it has stopped (its job is gone or empty, each orphaned
  PowerShell of an older KeyDock has exited) lets a layout change start. A
  job that cannot be opened or queried, and an orphaned process that is
  still running, keep blocking. After 60 s the tray reports it (with the
  process IDs) and checks every 30 s; `--once` exits with 1 after its 75 s
  wait without changing anything. The orphaned processes are recorded, so
  a restarted KeyDock keeps waiting for them; they are still never
  terminated, and Exit does not wait for them. The check is repeated under
  the writer lock (F5-01).
- `--once` checks the configuration that the evaluation itself reads: a
  `config.toml` that becomes unusable while it waits for a previous restore
  is exit code 1, not the older configuration. The tray keeps its last
  usable configuration as before; a missing file stays "not configured"
  (F5-04).
- Every successful apply now rests on a read of the enabled layouts after
  the session work, also when the plan was empty or a planned removal had
  already happened: a missing profile layout or another managed layout
  that appeared meanwhile is reported and retried instead of being taken
  for success (F5-02).
- After KeyDock gives up waiting for incomplete keyboards, those are not
  waited for again while they stay incomplete, and a keyboard that arrives
  later gets its own wait (up to 15 s, sampled every second); one waiting
  episode still ends after 60 s (F5-03).
- The outcome of a background language-order restore is kept beside the
  warning about what the layout change did to the language list (for
  example the first language removed), instead of replacing it (F5-06).
- Tests: the orphan test attempts termination only on its own fixture,
  through the detector's handle; the inheritance test compares kernel
  objects from the parent (the handle is duplicated out of each child, so
  an invalid or reused value is safe) with a positive control; the lock
  tests use test names with the production descriptor and never take the
  real per-user lock; in an AppContainer they check that KeyDock fails
  closed and report it instead of the ordinary-Windows checks; fixtures are
  cleaned up on failure too (F5-05).

## 0.3.6 - 2026-11-20

Fixes from the fourth engineering review of 0.3.5 (FP-01 to FP-08).

### Changed

- `--once` handed to a running KeyDock now answers only 4 (queued) or 5
  (paused). Whether `config.toml` can be used and whether the session may
  change layouts are decided by the queued evaluation, which reads both
  afresh, so a configuration repaired since the last evaluation is applied
  (it used to be refused with 1, and a remote-session answer of 3 could be
  stale) (FP-07).

### Fixed

- `--once` no longer changes anything while a language-order restore of a
  previous KeyDock may still be running: the check is at the point where
  every layout change starts, for the tray and `--once` alike. `--once`
  waits up to 75 s for it and otherwise exits with 1. A restore job that
  exists but cannot be opened counts as still running (bounded to 60 s,
  then a warning), not as absent; and KeyDock's own restore job counts until
  it is empty (FP-01).
- The writer lock's failure to open is now explained in the log (the
  token's account, whether it is deny-only, integrity level, elevation,
  restrictions, and the lock's security descriptor). The reported denial
  was reproduced with a token that carries the user only as a deny-only
  group; KeyDock keeps failing closed there rather than opening the lock to
  all administrators (FP-02).
- With `language_order = "restore"`, only the layouts KeyDock attempted to
  change can explain a change of the language list; a change observed for
  another input method no longer counts as KeyDock's side effect (FP-03).
- The final check after a layout change fails when a managed layout that is
  not part of the profile became enabled meanwhile, instead of reporting
  success (FP-04).
- A retry that does not reach a stage (for example an empty plan, which
  checks neither the language order nor unloading) keeps that stage's
  earlier warning instead of clearing it (FP-05).
- An incomplete keyboard is sampled again every second on its own schedule,
  so earlier failures cannot use up the retries it needs; keyboards that
  keep appearing incomplete one after the other stop holding back the
  profile after 60 s in all (FP-06).
- Tests: the orphan-detection test finds exactly its own fixture process
  (a harmless helper of the test binary in a job the test owns), never
  terminates anything it detected, and cleans up only its own job; the
  handle-inheritance test checks object identity with a sentinel and a
  positive control and reports a helper's exit code and stderr (FP-08).

## 0.3.5 - 2026-11-20

Fixes from the third engineering review of 0.3.4 (TP-01 to TP-10).

### Changed

- **Restoring the language order is now opt-in** (TP-01). A user's move of
  the very language whose layouts KeyDock changed could pass as Windows'
  side effect and be undone. The default is now `language_order =
  "report"`: KeyDock reports a changed order and never runs
  `Set-WinUserLanguageList`. Configured layout changes can still change the
  list in either mode. With `language_order = "restore"`, a language whose
  layouts changed may only have moved to the end of the list; anything else
  is treated as the user's change and not undone. **Migration:** a
  `config.toml` without `language_order` used to restore and now reports;
  add `language_order = "restore"` to keep the 0.3.4 behaviour. Files that
  set the key are unchanged; a file saved with the default does not
  contain the key.
- `--once` exit codes: 1 also for a `config.toml` that cannot be used (it
  was 0, "nothing to do"); when KeyDock is already running the request is
  handed over and confirmed by the running instance: 4 = queued (applied in
  the background), 5 = the running instance is paused, 3 = it is in a
  non-console session (TP-10).

### Fixed

- The per-user writer lock no longer lets KeyDock write when it cannot
  establish ownership (no SID, the lock cannot be created or opened, the
  wait fails): it now changes nothing and reports why. The lock has a
  per-user security descriptor usable by elevated and normal processes and
  is opened with minimal rights; an abandoned lock (its previous owner died
  mid-change) is detected and logged, and the restore checks the whole
  language state afresh under the newly taken lock (TP-02).
- The restore's PowerShell is created inside its job (no moment outside
  it), inherits only its three standard handles, and does not start when
  KeyDock is exiting, also when the cancel arrives while it waits for the
  writer lock. A new instance changes nothing until a previous instance's
  restore is confirmed ended, and after an upgrade from 0.3.3 it waits (up
  to 60 s) for an orphaned PowerShell restore instead of overlapping it. The
  restore marker is per session (TP-03).
- Layout changes now read the enabled layouts back after the session work
  also when only installing; a failing install or removal is observed
  afterwards, so partial changes are reported; and what changed is told
  apart from what was attempted and from what is unknown. The language list
  is compared whenever a change was attempted (TP-04).
- The tray, Settings and `--once` keep each stage's problem (enabled
  layouts, switching to the `activate` layout, unloading, language order)
  until that stage is re-checked or a new apply supersedes it; a successful
  restore and check no longer hide a failed switch (TP-05). A restore whose
  completion message is lost is still picked up, and its end always checks
  the layouts.
- Timing edited in `config.toml` (`debounce_ms`, `resume_delay_ms`,
  `removal_delay_ms`) reaches the event scheduler on the next reload;
  deadlines already recorded keep their delay (TP-06).
- Each incomplete keyboard gets its own 15 s settling budget, so one that
  never becomes readable no longer makes a newly arriving one skip waiting
  (TP-07).
- Marking a serial-less keyboard "This port only" now keeps the mark of
  every other connected keyboard the model-wide rule covered, also one with
  a serial number or Bluetooth, and names any it cannot keep (TP-08).
- A Settings save that is cancelled at a repeated question, or fails at the
  final replace, keeps the previous `config.toml.bak` (TP-09).
- A re-apply handed over by `--once` is recorded durably, also when it
  arrives while the tray instance is busy (TP-10). The
  evaluation timer's fallback can no longer re-post itself in a loop.

## 0.3.4 - 2026-11-20

Fixes from the second engineering review of 0.3.3 (KD-01 to KD-12).

### Changed

- **Language-order restore only when nothing else changed** (KD-01). The
  restore writes the whole Windows language list, so KeyDock now snapshots
  every language setting (`HKCU\Control Panel\International\User Profile`)
  before a layout change, and restores the order only if every difference
  afterwards comes from the layouts it added or removed. Right before the
  restore the settings must be unchanged, the script checks the list once
  more immediately before it writes, and the result is read back and
  compared. A conflict, a failure, a timeout or a result that does not match
  is shown in the tray and makes `--once` exit with code 2, instead of only
  being logged. A layout change is not made when the snapshot cannot be
  read. Removing the last layout of the first language is reported (Windows
  then removes the language, which can change the display language). New
  option `language_order = "report"`: never write the list, only report.
  Existing configurations keep the default `"restore"`; a file saved with the
  default does not contain the new key, so it still loads in 0.3.3.
- `--once` exit codes: 0 done, 1 error, 2 applied but something needs
  attention, 3 nothing changed (Remote Desktop or other non-console session).
- Save/Apply, Re-apply and resuming from Paused wait for a running removal
  hold-off or resume delay (the keyboards are still settling) and for a
  running language-order restore; Settings shows the result when it is known.

### Fixed

- The PowerShell process of a restore could outlive KeyDock (Exit, sign-out,
  an installer closing or terminating KeyDock) and write an old snapshot
  later, also over a new instance's change (KD-02). It now runs suspended
  until it is in a kill-on-close job object, so it ends with KeyDock in any
  case; Exit waits a bounded time and then stops it, waits are bounded, and a
  new instance stops a restore left running by an old one and reports one
  that was interrupted.
- Layouts could be removed without proof that the profile's layouts were
  enabled: when the enabled list could not be read, KeyDock fell back to the
  loaded input locales, and a remove-only change was not checked (KD-03). An
  unreadable or invalid enabled list now stops the change; the list is read
  again right before every removal and read back after it.
- Activating a layout variant could switch to another layout of the same
  language, and failures there were only logged; after giving up on a
  mismatch KeyDock reported success (KD-04). The HKL must now match exactly,
  the default input language is read back, the switch is requested from the
  window with the keyboard focus, and each problem is shown in the tray, in
  Settings and by `--once`. A mismatch that remains is shown until it is
  resolved, and the next event checks the layouts again.
- Scheduling: an event that arrived while KeyDock was busy lost its type and
  hold-off, a later arrival shortened the delay after resume, the restore's
  completion could shorten a retry back-off, and a stale timer message could
  run an evaluation early (KD-05). Each delay now has its own deadline, the
  earliest evaluation is the latest of them, and early timers re-arm.
- Settings could recreate `config.toml` after it was deleted, and could
  replace a version that changed while its question was open (KD-06). It now
  asks in both cases, keeps exactly the version you agreed to replace as
  `config.toml.bak`, and compares again right before the replace. Temporary
  files are unique per write, KeyDock's own writers are serialised, and a
  configuration that exists but cannot be read is an error, not "not
  configured".
- A keyboard interface whose identity could not be read completely (after
  wake or a dock reset) could select the wrong profile (KD-07). KeyDock now
  keeps the current layouts and samples again for up to 15 s when the missing
  data could change the profile.
- Settings now says when a mark covers every keyboard of a model (USB
  keyboards without a serial number), and **This port only** marks just one
  unit on its current port, so two identical keyboards can be told apart
  (KD-08). Existing rules are kept.
- `--once` without a running tray instance ignored the Remote Desktop rule;
  the rule was also checked against a configuration that could be out of
  date (KD-09). Tray and `--once` now share one check on the configuration
  as it is on disk, and a per-user lock lets only one session of a user
  change layouts at a time.
- Repairing a stale "Start with Windows" path removed a "turned off" choice
  made in Windows' Startup apps, and saving Settings could undo a change made
  there while the window was open (KD-10). Only ticking the box turns the
  entry on.
- `installer\build.ps1 -KeyDockExe <path> -Architecture x64|arm64` failed
  under StrictMode (KD-11).
- `--remember-builtin` returned 0 when `state.toml` could not be written
  (KD-12).
- The message loop treated a `GetMessageW` failure (-1) as a message.

## 0.3.3 - 2026-11-20

### Fixed

- First run no longer records the keyboards that happen to be connected as
  built-in (`state.toml`). Set up while docked, that made the external
  keyboard "built-in" so the docked profile never applied, and
  `--list-keyboards` and `--dry-run` wrote the file too. Built-in keyboards
  are now recognised by the existing heuristics or marked in the settings
  window; on first run the window says so. `state.toml` from older versions
  is still read, and keyboards it lists are shown as "Built-in (remembered)".
- The language-order restore (`Set-WinUserLanguageList`, which writes the
  whole language list as it read it) could undo a layout change made while
  it ran, for example on a quick undock and redock, and KeyDock then
  considered the profile applied. Layout changes now wait until the restore
  has finished, the restore is stopped after 30 s, and afterwards KeyDock
  checks the layouts and applies the profile again if they no longer match.
- A keyboard, power or session event that arrived while KeyDock was busy
  outside the debounce timer (at startup, Save/Apply, Re-apply, Paused) was
  dropped. Such events now always lead to a re-evaluation.
- A failed keyboard enumeration or layout change was not retried; KeyDock
  stayed in the old mode until the next device event. It now retries after
  1, 2, 5, 10 and 30 seconds.
- A `config.toml` that could not be read made KeyDock behave as if it was not
  configured, with no visible sign, and the settings window then offered to
  start over and overwrote the file. KeyDock now warns in the notification
  area (tooltip and a balloon, even with notifications off), the settings
  window shows the error and opens on start, and saving asks first and keeps
  the old file as `config.toml.bak`.
- `config.toml` and `state.toml` are written atomically (temporary file and
  rename). The "ask first and keep config.toml.bak" check covers any file
  KeyDock cannot use: a syntax error, another encoding than UTF-8, or a file
  that cannot be read.
- Opening Settings (tray double-click, or starting `keydock.exe` again) while
  one of its message boxes was open created a second settings window and
  crashed KeyDock. The existing window is now brought to the front.
- When applying a profile failed part-way, KeyDock still considered the
  previous profile applied, so returning to it was skipped and the layouts of
  both stayed installed. A failed apply now leaves no profile marked as
  applied, so the next evaluation applies again.
- The language-order restore no longer starts PowerShell with
  `-ExecutionPolicy Bypass` (not needed for `-Command`), and its script uses
  only cmdlets, so it also works under Constrained Language Mode (WDAC,
  AppLocker).
- Layouts are planned against the layouts enabled for the user
  (`EnumEnabledLayoutOrTip`) instead of the input locales loaded in the
  session, so a layout that an application loaded, or that is left over in
  the session, no longer counts as installed. After installing, KeyDock checks
  that every layout of the profile is enabled before it removes any.
- Removed layouts that stay loaded are no longer added and removed once more
  (that could reorder the language list again and leave the layout installed);
  they are unloaded at the next change if possible.
- Marks and "Only these keyboards" entries for USB keyboards without a serial
  number are stored by VID:PID: Windows gives such devices a new container ID
  on every port or dock, so the old entries stopped matching there. Older
  container entries are still recognised, and **Automatic** removes them.
- After a keyboard is removed, KeyDock waits until the keyboards have been
  unchanged for 2.5 s (`removal_delay_ms`) before switching, so a dock or hub
  that drops its devices for a moment no longer causes two switches.
- In a Remote Desktop session KeyDock no longer applies the remote computer's
  keyboards to the session (`remote_sessions = true` restores the old
  behaviour), and it re-checks when a session connects to the console or
  over Remote Desktop.
- `--once` no longer changes layouts in parallel with a running KeyDock; it
  asks the running instance to re-apply.
- The log is rotated at 1 MiB also while KeyDock runs, not only at startup.
- A panic is written to the log before the process ends.
- A keyboard interface whose device cannot be read (one that is being
  removed) no longer counts as an external keyboard.
- Starting `keydock.exe` again now brings the running instance's settings
  window to the front instead of behind other windows.
- "Start with Windows" shows the entry as off when it is turned off in Task
  Manager or Settings > Apps > Startup; ticking it turns it on again, and
  leaving it unticked no longer deletes such an entry.
- The settings window shows `config.toml` as it is on disk when it opens, and
  saving asks before it replaces changes made to the file meanwhile (keeping
  `config.toml.bak`).
- Internal: the scheduling (debounce, missed events, queued modes, retries,
  hold-off) is a tested library type (`keydock::schedule`).
- The MSI was tested on Windows 10 22H2: install, upgrade from 0.3.2 while
  KeyDock runs (it exits cleanly), repair, refused downgrade, uninstall
  (removes only a Start with Windows entry that starts the installed copy).
- The notification-area icons were loaded again on every update and never
  released (one icon handle leaked per keyboard event); they are now loaded
  once.

## 0.3.2 - 2026-11-20

### Fixed

- Switching layouts could change the order of the user's language list, e.g.
  move English (United States) behind Russian, which changes the Windows
  display language at the next sign-in when no display-language override is
  set. KeyDock now records the language order before it adds or removes
  layouts, logs it before and after, warns when it changed (and whether the
  first language changed), and restores it with `Set-WinUserLanguageList` in
  a hidden background PowerShell process. Only the order is restored.

## 0.3.1 - 2026-11-20

### Fixed

- A removed layout could stay in the language switcher until the next sign-in
  although it was gone from the language list: Windows cannot unload a layout
  that a thread still uses, and only the foreground window was switched (and
  only when the profile had a "Switch to" layout). Before removing layouts,
  KeyDock now moves its own process, the default input language and every
  window whose thread uses a layout being removed to a remaining layout, waits
  for that, then unloads leftovers from the session. Layouts that are still
  loaded afterwards are logged as a warning with the processes that use them.
- KeyDock no longer leaves a layout loaded that it loaded only to find it was
  not the requested one.

### Added

- **Apply** button in the settings window: writes and applies the
  configuration without closing the window. **Save** now also closes it.

## 0.3.0 - 2026-11-19

### Added

- WiX Toolset 6 installer (`installer/`): per-machine MSI for x64 and ARM64,
  Program Files install, Start menu shortcut, optional launch at the end,
  major upgrades, clean close of a running KeyDock during upgrade and
  uninstall, `installer/build.ps1`.
- Devices that also expose a mouse interface (same physical device) are marked
  "also mouse" in the settings window and in `--list-keyboards`. They are not
  ignored automatically.
- `--remove-autostart` (used by the uninstaller).

### Fixed

- The built-in PS/2 keyboard (`ACPI\MSFT0001`) was shown as "PCI to ISA Bridge":
  the product-name lookup climbed to the parent bridge. Parents are now only
  examined for HID/USB/Bluetooth devices and only within the same device
  container.
- "Start with Windows": an entry pointing to another copy is shown as enabled
  and is moved to the running copy on save; an entry pointing to a missing file
  is repaired at startup.
- Clean exit on `WM_CLOSE`, `WM_QUERYENDSESSION`/`WM_ENDSESSION` (installer,
  Restart Manager, sign-out): Settings closed, tray icon removed,
  notifications unregistered, exit code 0, even while busy.

### Changed

- Wider "Type" column in the keyboard list; `--list-keyboards` shows the
  product name.

## 0.2.0 - 2026-11-19

### Added

- Settings window (native Win32, per-monitor DPI aware): live keyboard list
  grouped by physical device with Built-in / External / Ignore / Automatic
  overrides, "Laptop only" and "External keyboard connected" panels with layout
  checklists and the layout to switch to, layout picker, options. Saving writes
  `config.toml` and applies it immediately; a mode without layouts is refused.
- Opens on first run, from the tray (double-click or **Settings...**), with
  `--settings`, or when `keydock.exe` is started a second time.
- **Start with Windows** via the `HKCU\...\Run` key, starting with
  `--background` straight into the notification area.
- Tray icon shows docked / laptop / paused; the tooltip shows mode and profile.
  Optional balloon notification on profile changes (`notify`, off by default).
- Embedded application icon, version resource and application manifest.
- Configuration: `builtin` and `external` lists for manual classification,
  `notify`, and `when` may be an array of matchers (any matches).
- Device product names (bus-reported description) in listings.

### Changed

- A missing configuration file is no longer replaced by a commented default;
  KeyDock changes nothing until settings are saved. **Open config file**
  creates a minimal file if needed.
- The configuration file is rewritten by the settings window; comments are not
  preserved. Profiles not managed by the window are kept.
- README is now GUI-first.

## 0.1.0

- First prototype: background tray process that installs and removes keyboard
  layouts with `InstallLayoutOrTip` when an external keyboard is connected or
  disconnected, TOML profiles, `--list-keyboards`, `--once`, `--dry-run`,
  `--remember-builtin`.
