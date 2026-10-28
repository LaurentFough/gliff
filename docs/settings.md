# Settings and the client UI

How the client's per-machine options work, and how to add one. Read this
before adding a flag that changes how a session behaves.

## The model

`crates/gliff/src/settings.rs` holds every option. `Settings` is one layer
where each option is an `Option`, so a layer states only what it changes.
`Resolved` is the stack with every value filled in; a session runs with it.

Layers, last wins:

1. built-in values (`Settings::resolve`)
2. `[defaults]` in `~/.config/gliff/config.toml`
3. the command line (`cli_settings` in `main.rs`)
4. `[overrides."user@host"]`

The command line sits below the override on purpose: a choice made in the
window beats the flags. A flag that names something concrete (`--output`)
is the exception and is handled where the server arguments are built
(`server_args`).

## What the UI offers

- **Settings window** (`settings_ui.rs`): header gear or Ctrl+,. Machines
  first, then Defaults. Every change is saved at once; a row shows
  "Default"/"Built-in" while it follows the layer below and a reset button
  once it is set. The search box filters rows by title and subtitle.
- **Tab menu** (`install_tab_menu` in `main.rs`): the everyday choices only.
  It saves through `edit_machine`, the same path as the window.

Keep the menu short. An option belongs in it only if people change it
for one machine regularly; everything else lives in the window alone.

## Adding an option

1. `settings.rs`: add the field to `Settings` (with
   `skip_serializing_if = "Option::is_none"`), `Field`, `Field::ALL`,
   `is_set`, `clear`, `over`, `resolve` and `Resolved`. Add a test for the
   built-in value and one for layering if it is not trivial.
2. `settings_ui.rs`: add a row in `build_page` with `switch`, `combo` or
   `entry`, in the group it belongs to. The subtitle says what it does and
   when to change it; the group description says "A change reconnects the
   machine" when it does.
3. `main.rs`:
   - if a flag already exists, map it in `cli_settings`;
   - if the server needs it, add the argument in `server_args`;
   - if the client reads it, read `session.opts` (set when the session
     opens), not the `Cli`.
4. Decide how a change applies, in `apply_settings`. An option that can
   change in a running session (the clipboard) is excluded from the
   `live_only` comparison and applied in place. Anything else reconnects
   the machine, which is the default.
5. Only if it is an everyday choice, add it to `install_tab_menu`.
6. Update the README's flag list if there is a flag.

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
