# Settings and the client UI

How the clients' per-machine options work, and how to add one. Read this
before adding a flag that changes how a session behaves.

Both clients use the same model and the same file: the Linux (GTK) client
and the macOS client read and write `$XDG_CONFIG_HOME/gliff/config.toml`
(else `~/.config/gliff/config.toml`), so `[defaults]` and
`[overrides."user@host"]` mean the same thing on either.

## The model

`crates/gliff-settings/src/model.rs` holds every option (the Linux client
re-exports it as `crate::settings`). `Settings` is one layer
where each option is an `Option`, so a layer states only what it changes.
`Resolved` is the stack with every value filled in; a session runs with it.

Layers, last wins:

1. built-in values (`Settings::resolve`)
2. `[defaults]` in `~/.config/gliff/config.toml`
3. the command line (`cli_settings` in `linux.rs`; `settings::cli_layer`
   in `gliff-macos`)
4. `[overrides."user@host"]`

`crates/gliff-settings/src/store.rs` (`Store`) reads and writes the file for
the macOS client. It owns `machines`, `[defaults]`, `[overrides]` and the
top-level `muted` (the sound switch: the Linux header's speaker button and the
Mac's Machine ▸ Play Sound, written only when sound is off), and writes every
other key back untouched, so the Linux client's `open` and `shown` survive a
Mac save. The Linux client keeps its own `recent::Config` for the same file.

The command line sits below the override on purpose: a choice made in the
window beats the flags. A flag that names something concrete (`--output`)
is the exception and is handled where the server arguments are built
(`server_args`).

## What the UI offers

Linux:

- **Settings window** (`settings_ui.rs`): header gear or Ctrl+,. Machines
  first, then Defaults. Every change is saved at once; a row shows
  "Default"/"Built-in" while it follows the layer below and a reset button
  once it is set. The search box filters rows by title and subtitle.
- **Tab menu** (`install_tab_menu` in `linux.rs`): the everyday choices only.
  It saves through `edit_machine`, the same path as the window.

macOS (`crates/gliff-macos/src/settings.rs`, UI in `native/Gliff.swift`):

- **Settings window** (`SettingsWindow`): Settings… (⌘,) in the picker or the
  viewer, or the picker's Settings… button. All Machines (the defaults) and
  each machine, with a search box, the same "Default"/"Built-in" origin and
  a Reset button per row. It offers the screen, the clipboard and the server
  command; the decode pipeline is Linux-only (the Mac viewer decodes on the
  CPU) and is kept in the file but not shown.
- **Machine menu** in the viewer: Mirror Screen / Private Screen, Share
  Clipboard, Reconnect — the Mac's equivalent of the tab menu.
- **Picker**: its screen menu follows the machine's saved screen. A screen
  chosen there is saved as that machine's override on Connect; a choice
  saved by older versions in `UserDefaults` (`screen.<host>:<port>`) moves
  to the config the next time you connect to that machine.

The Swift windows never touch the file: the client sends them
`settings_state` / `machine_state` / `machine_settings` and applies their
`settings_edit` / `setting` events, so both Macs and Linux keep one writer
per process.

Keep the menu short. An option belongs in it only if people change it
for one machine regularly; everything else lives in the window alone.

## Adding an option

1. `crates/gliff-settings/src/model.rs`: add the field to `Settings` (with
   `skip_serializing_if = "Option::is_none"`), `Field`, `Field::ALL`,
   `is_set`, `clear`, `over`, `resolve` and `Resolved`, and its key in
   `Field::name` (`lib.rs`). Add a test for the built-in value and one for
   layering if it is not trivial.
2. `settings_ui.rs`: add a row in `build_page` with `switch`, `combo` or
   `entry`, in the group it belongs to. The subtitle says what it does and
   when to change it; the group description says "A change reconnects the
   machine" when it does.
3. `linux.rs`:
   - if a flag already exists, map it in `cli_settings`;
   - if the server needs it, add the argument in `server_args`;
   - if the client reads it, read `session.opts` (set when the session
     opens), not the `Cli`.
4. Decide how a change applies, in `apply_settings`. An option that can
   change in a running session (the clipboard) is excluded from the
   `live_only` comparison and applied in place. Anything else reconnects
   the machine, which is the default.
5. Only if it is an everyday choice, add it to `install_tab_menu`.
6. macOS, if the Mac should offer it: add it to `OFFERED`, `layer_json` and
   `set_field` in `crates/gliff-macos/src/settings.rs`, decide in `change`
   whether it applies live, read it from the session's `Resolved` in
   `client.rs`, and add a row in `SettingsWindow` (`native/Gliff.swift`).
7. Update the README's flag list if there is a flag.

A setting that needs a host feature must not be offered while the feature is
absent from `main`. Land the feature first, then its row.

## Layout and wording

- One page per scope (a machine, or the defaults), with the same rows in
  both. Groups: Display, Clipboard, Video, Connection; put a new option in
  the nearest one before creating a group.
- Titles are short noun phrases or "Share the clipboard"-style verbs;
  subtitles are one sentence.
- Libadwaita widgets only (`SwitchRow`, `ComboRow`, `EntryRow`,
  `SpinRow`), so the window follows the Omarchy theme with no CSS.
