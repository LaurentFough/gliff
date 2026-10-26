//! The settings window: the options every machine starts with, and, one
//! machine at a time, what that machine changes. Every change is saved as it
//! is made. A machine's row shows an option it follows with the word
//! "Default", and one it changes with a reset button that hands it back.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::recent::Config;
use crate::settings::{Field, Resolved, Screen, Settings, Video};

/// What the window needs from the app.
pub struct Env {
    pub config_path: PathBuf,
    /// What the command line sets for every machine.
    pub cli: Settings,
    /// Called after a change is saved, for the app to apply it.
    pub changed: Rc<dyn Fn()>,
}

#[derive(Debug, Clone, PartialEq)]
enum Scope {
    Defaults,
    Machine(String),
}

/// Brings a row's widget in line with the resolved options; the flag says to
/// drop an edit that is not applied yet.
type SyncRow = Box<dyn Fn(&Resolved, bool)>;

/// One option's row: how to bring it in line with the saved values.
struct RowRef {
    field: Field,
    widget: gtk::Widget,
    group: adw::PreferencesGroup,
    /// Title and subtitle, lowercased, for the search.
    text: String,
    /// "Default" or "Built-in" while the option follows the layer below.
    tag: gtk::Label,
    reset: gtk::Button,
    sync: SyncRow,
}

struct View {
    env: Rc<Env>,
    scope: RefCell<Scope>,
    /// The sidebar rows, with the scope each selects.
    sidebar: RefCell<Vec<(Scope, adw::ActionRow)>>,
    list: gtk::ListBox,
    title: adw::WindowTitle,
    search: gtk::SearchEntry,
    scroller: gtk::ScrolledWindow,
    reset_all: gtk::Button,
    rows: RefCell<Vec<RowRef>>,
    /// Set while a row is brought in line, so that does not count as an edit.
    syncing: Cell<bool>,
}

/// Open the window on `machine`, else the first machine, else the defaults.
pub fn present(parent: &impl IsA<gtk::Widget>, env: Rc<Env>, machine: Option<String>) {
    let (view, dialog) = build(env, machine);
    dialog.present(Some(parent));
    view.search.grab_focus();
}

/// The dialog, and the view it owns.
fn build(env: Rc<Env>, machine: Option<String>) -> (Rc<View>, adw::Dialog) {
    let config = Config::load(&env.config_path);
    let scope = match machine {
        Some(m) if config.machines.contains(&m) => Scope::Machine(m),
        _ => config
            .machines
            .first()
            .cloned()
            .map_or(Scope::Defaults, Scope::Machine),
    };

    let title = adw::WindowTitle::new("Settings", "");
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Find a setting")
        .hexpand(true)
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();
    let reset_all = gtk::Button::with_label("Reset to defaults");

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .css_classes(["navigation-sidebar"])
        .build();
    let view = Rc::new(View {
        env,
        list: list.clone(),
        scope: RefCell::new(scope.clone()),
        sidebar: RefCell::new(Vec::new()),
        title,
        search: search.clone(),
        scroller: scroller.clone(),
        reset_all: reset_all.clone(),
        rows: RefCell::new(Vec::new()),
        syncing: Cell::new(false),
    });

    // Sidebar: a row per machine, then, under a divider, the defaults they
    // follow. `by_index` says what each list row selects.
    list.set_header_func(|row, before| {
        let divider = (row.widget_name() == "defaults" && before.is_some()).then(|| {
            let line = gtk::Separator::new(gtk::Orientation::Horizontal);
            line.set_margin_top(6);
            line.set_margin_bottom(6);
            line
        });
        row.set_header(divider.as_ref());
    });
    let mut by_index: Vec<Scope> = Vec::new();
    let machine_scopes = config.machines.iter().cloned().map(Scope::Machine);
    for sc in machine_scopes.chain([Scope::Defaults]) {
        let row = adw::ActionRow::builder()
            .title(scope_title(&sc))
            .tooltip_text(scope_title(&sc))
            .use_markup(false)
            .activatable(true)
            .title_lines(1)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name(match sc {
            Scope::Defaults => "preferences-system-symbolic",
            Scope::Machine(_) => "computer-symbolic",
        }));
        if sc == Scope::Defaults {
            row.set_widget_name("defaults");
        }
        list.append(&row);
        by_index.push(sc.clone());
        view.sidebar.borrow_mut().push((sc, row));
    }
    let sidebar_scroller = gtk::ScrolledWindow::builder()
        .child(&list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();
    let sidebar_header = adw::HeaderBar::builder()
        .show_end_title_buttons(false)
        .title_widget(&adw::WindowTitle::new("Gliff", "Settings"))
        .build();
    let sidebar_view = adw::ToolbarView::new();
    sidebar_view.add_top_bar(&sidebar_header);
    sidebar_view.set_content(Some(&sidebar_scroller));
    let sidebar_page = adw::NavigationPage::builder()
        .title("Settings")
        .child(&sidebar_view)
        .build();

    // Content: the options of the chosen scope, a search, a footer.
    let content_header = adw::HeaderBar::builder().title_widget(&search).build();
    let footer = gtk::Box::builder()
        .spacing(12)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(16)
        .margin_end(16)
        .build();
    let hint = gtk::Label::builder()
        .label("Changes apply as you make them")
        .css_classes(["dim-label", "caption"])
        .hexpand(true)
        .xalign(0.0)
        .build();
    footer.append(&hint);
    footer.append(&reset_all);
    let content_view = adw::ToolbarView::new();
    content_view.add_top_bar(&content_header);
    content_view.set_content(Some(&scroller));
    content_view.add_bottom_bar(&footer);
    let content_page = adw::NavigationPage::builder()
        .title("Options")
        .child(&content_view)
        .build();

    let split = adw::NavigationSplitView::builder()
        .sidebar(&sidebar_page)
        .content(&content_page)
        .min_sidebar_width(200.0)
        .max_sidebar_width(260.0)
        .build();
    let dialog = adw::Dialog::builder()
        .title("Settings")
        .content_width(820)
        .content_height(620)
        .child(&split)
        .build();

    {
        let view = Rc::downgrade(&view);
        list.connect_row_selected(move |_, row| {
            let Some(row) = row else {
                return;
            };
            if let (Some(view), Some(scope)) = (view.upgrade(), by_index.get(row.index() as usize))
            {
                view.select(scope.clone());
            }
        });
    }
    {
        let view = Rc::downgrade(&view);
        search.connect_search_changed(move |_| {
            if let Some(view) = view.upgrade() {
                view.filter();
            }
        });
    }
    {
        let view = Rc::downgrade(&view);
        reset_all.connect_clicked(move |_| {
            if let Some(view) = view.upgrade() {
                view.reset_everything();
            }
        });
    }
    // The handlers hold the view weakly; the dialog owns it, so it is freed
    // with the dialog instead of staying allocated for each time Settings
    // is opened.
    {
        let owner = view.clone();
        dialog.connect_closed(move |_| {
            let _ = &owner;
        });
    }
    view.select_in_list(&scope);
    view.refresh(true);
    (view, dialog)
}

fn scope_title(scope: &Scope) -> String {
    match scope {
        Scope::Defaults => "Defaults".to_string(),
        Scope::Machine(m) => m.clone(),
    }
}

impl View {
    /// The settings the scope changes, and the options it runs with.
    fn layer(&self, config: &Config) -> (Settings, Resolved) {
        match &*self.scope.borrow() {
            Scope::Defaults => (config.defaults.clone(), config.defaults.resolve()),
            Scope::Machine(m) => (
                config.machine(m).cloned().unwrap_or_default(),
                config.settings_for(&self.env.cli, m).resolve(),
            ),
        }
    }

    /// Select `scope`'s row in the sidebar, which opens it.
    fn select_in_list(&self, scope: &Scope) {
        let row = self
            .sidebar
            .borrow()
            .iter()
            .find(|(s, _)| s == scope)
            .map(|(_, r)| r.clone());
        if let Some(row) = row {
            self.list.select_row(Some(&row));
        }
    }

    fn select(self: &Rc<Self>, scope: Scope) {
        *self.scope.borrow_mut() = scope.clone();
        self.search.set_text("");
        self.title.set_title(&scope_title(&scope));
        self.build_page();
        self.refresh(true);
    }

    /// Save a change to the scope's settings and have the app apply it.
    fn edit(self: &Rc<Self>, change: impl Fn(&mut Settings)) {
        self.edit_with(false, change);
    }

    /// `edit`, saying whether the rows drop edits that are not applied yet:
    /// a reset does, since it puts the saved value back.
    fn edit_with(self: &Rc<Self>, discard_edits: bool, change: impl Fn(&mut Settings)) {
        if self.syncing.get() {
            return;
        }
        let mut config = Config::load(&self.env.config_path);
        match self.scope.borrow().clone() {
            Scope::Defaults => {
                change(&mut config.defaults);
                config.defaults.tidy();
            }
            Scope::Machine(m) => {
                let mut o = config.machine(&m).cloned().unwrap_or_default();
                change(&mut o);
                config.set_machine(&m, o);
            }
        }
        self.save(&config, discard_edits);
    }

    fn clear(self: &Rc<Self>, field: Field) {
        self.edit_with(true, |s| s.clear(field));
    }

    fn reset_everything(self: &Rc<Self>) {
        let mut config = Config::load(&self.env.config_path);
        match self.scope.borrow().clone() {
            Scope::Defaults => config.defaults = Settings::default(),
            Scope::Machine(m) => config.set_machine(&m, Settings::default()),
        }
        self.save(&config, true);
    }

    fn save(self: &Rc<Self>, config: &Config, discard_edits: bool) {
        if let Err(e) = config.save(&self.env.config_path) {
            tracing::warn!(path = %self.env.config_path.display(), error = %e, "cannot save config");
        }
        (self.env.changed)();
        self.refresh(discard_edits);
    }

    /// Bring every row, and the sidebar, in line with the saved settings.
    fn refresh(&self, discard_edits: bool) {
        let config = Config::load(&self.env.config_path);
        let (layer, resolved) = self.layer(&config);
        let defaults_scope = *self.scope.borrow() == Scope::Defaults;
        self.syncing.set(true);
        for row in self.rows.borrow().iter() {
            let set = layer.is_set(row.field);
            row.reset.set_visible(set);
            row.tag.set_visible(!set);
            row.tag.set_label(if defaults_scope {
                "Built-in"
            } else {
                "Default"
            });
            (row.sync)(&resolved, discard_edits);
        }
        self.syncing.set(false);
        self.reset_all.set_sensitive(!layer.is_empty());
        self.reset_all.set_label(if defaults_scope {
            "Reset all to built-in"
        } else {
            "Reset to defaults"
        });
        for (scope, row) in self.sidebar.borrow().iter() {
            row.set_subtitle(&match scope {
                Scope::Defaults => "For all machines".to_string(),
                Scope::Machine(m) => match config.machine(m) {
                    None => "Follows the defaults".to_string(),
                    Some(o) => {
                        let n = Field::ALL.iter().filter(|f| o.is_set(**f)).count();
                        format!("{n} custom {}", if n == 1 { "setting" } else { "settings" })
                    }
                },
            });
        }
    }

    /// Hide the rows, and the groups left empty, that the search rules out.
    fn filter(&self) {
        let needle = self.search.text().to_lowercase();
        let needle = needle.trim();
        let rows = self.rows.borrow();
        let mut shown: Vec<&adw::PreferencesGroup> = Vec::new();
        for row in rows.iter() {
            let hit = needle.is_empty() || row.text.contains(needle);
            row.widget.set_visible(hit);
            if hit && !shown.contains(&&row.group) {
                shown.push(&row.group);
            }
        }
        for row in rows.iter() {
            row.group.set_visible(shown.contains(&&row.group));
        }
    }

    fn build_page(self: &Rc<Self>) {
        self.rows.borrow_mut().clear();
        let page = adw::PreferencesPage::new();

        // A machine says what it follows and how to change that.
        if matches!(*self.scope.borrow(), Scope::Machine(_)) {
            let banner = group("", None);
            let row = adw::ActionRow::builder()
                .title("Following the defaults")
                .subtitle("An option you do not change here uses the defaults for all machines")
                .use_markup(false)
                .build();
            let link = gtk::Button::builder()
                .label("Change defaults")
                .valign(gtk::Align::Center)
                .build();
            {
                let view = self.clone();
                link.connect_clicked(move |_| view.select_in_list(&Scope::Defaults));
            }
            row.add_suffix(&link);
            banner.add(&row);
            page.add(&banner);
        }

        let display = group("Display", Some("A change reconnects the machine"));
        self.combo(
            &display,
            Field::Screen,
            "How it is shown",
            "Mirror fits the machine's focused screen into the window; own screen gives it a private screen sized to the window",
            &["Mirror", "Own screen"],
            |r| match r.screen {
                Screen::Mirror => 0,
                Screen::Own => 1,
            },
            |s, i| s.screen = Some([Screen::Mirror, Screen::Own][i as usize]),
        );
        page.add(&display);

        let sharing = group("Clipboard", None);
        self.switch(
            &sharing,
            Field::Clipboard,
            "Share the clipboard",
            "Turn it off for a machine you do not trust: it can read what you copy while it is shown",
            |r| r.clipboard,
            |s, v| s.clipboard = Some(v),
        );
        page.add(&sharing);

        let video = group("Video", Some("A change reconnects the machine"));
        self.combo(
            &video,
            Field::Decode,
            "Decode on this machine",
            "The GPU when it works, else the CPU. Choose CPU to rule out a driver problem",
            &["Automatic", "GPU", "CPU"],
            |r| video_index(r.decode),
            |s, i| s.decode = Some(video_at(i)),
        );
        page.add(&video);

        let connection = group("Connection", Some("A change reconnects the machine"));
        self.entry(
            &connection,
            Field::ServerBin,
            "Server command",
            "What ssh runs on the machine, such as /opt/gliff/gliff-server",
            |r| r.server_bin.clone(),
            |s, v| s.server_bin = Some(v),
        );
        page.add(&connection);

        self.scroller.set_child(Some(&page));
        self.filter();
    }

    fn add_row(
        self: &Rc<Self>,
        group: &adw::PreferencesGroup,
        field: Field,
        row: &(impl IsA<adw::PreferencesRow> + IsA<gtk::Widget>),
        title: &str,
        subtitle: &str,
        sync: SyncRow,
    ) {
        let tag = gtk::Label::builder()
            .css_classes(["dim-label", "caption"])
            .valign(gtk::Align::Center)
            .build();
        let reset = gtk::Button::builder()
            .icon_name("edit-undo-symbolic")
            .tooltip_text("Back to the default")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build();
        {
            let view = Rc::downgrade(self);
            reset.connect_clicked(move |_| {
                if let Some(view) = view.upgrade() {
                    view.clear(field);
                }
            });
        }
        let suffix = gtk::Box::builder().spacing(6).build();
        suffix.append(&tag);
        suffix.append(&reset);
        // The reset button sits at the row's far end, after any control.
        if let Some(r) = row.dynamic_cast_ref::<adw::EntryRow>() {
            r.add_suffix(&suffix);
        } else if let Some(r) = row.dynamic_cast_ref::<adw::ActionRow>() {
            r.add_suffix(&suffix);
        }
        row.set_use_markup(false);
        row.set_title(title);
        group.add(row);
        self.rows.borrow_mut().push(RowRef {
            field,
            widget: row.clone().upcast::<gtk::Widget>(),
            group: group.clone(),
            text: format!("{title} {subtitle}").to_lowercase(),
            tag,
            reset,
            sync,
        });
    }

    fn switch(
        self: &Rc<Self>,
        group: &adw::PreferencesGroup,
        field: Field,
        title: &str,
        subtitle: &str,
        get: fn(&Resolved) -> bool,
        set: fn(&mut Settings, bool),
    ) {
        let row = adw::SwitchRow::builder().subtitle(subtitle).build();
        {
            let view = Rc::downgrade(self);
            row.connect_active_notify(move |r| {
                let on = r.is_active();
                if let Some(view) = view.upgrade() {
                    view.edit(|s| set(s, on));
                }
            });
        }
        let weak = row.downgrade();
        self.add_row(
            group,
            field,
            &row,
            title,
            subtitle,
            Box::new(move |r, _| {
                if let Some(row) = weak.upgrade() {
                    row.set_active(get(r));
                }
            }),
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn combo(
        self: &Rc<Self>,
        group: &adw::PreferencesGroup,
        field: Field,
        title: &str,
        subtitle: &str,
        choices: &[&str],
        get: fn(&Resolved) -> u32,
        set: fn(&mut Settings, u32),
    ) {
        let row = adw::ComboRow::builder()
            .subtitle(subtitle)
            .model(&gtk::StringList::new(choices))
            .build();
        {
            let view = Rc::downgrade(self);
            row.connect_selected_notify(move |r| {
                let at = r.selected();
                if let Some(view) = view.upgrade() {
                    view.edit(|s| set(s, at));
                }
            });
        }
        row.set_factory(Some(&whole_value_factory()));
        let weak = row.downgrade();
        self.add_row(
            group,
            field,
            &row,
            title,
            subtitle,
            Box::new(move |r, _| {
                if let Some(row) = weak.upgrade() {
                    row.set_selected(get(r));
                }
            }),
        );
    }

    fn entry(
        self: &Rc<Self>,
        group: &adw::PreferencesGroup,
        field: Field,
        title: &str,
        subtitle: &str,
        get: fn(&Resolved) -> String,
        set: fn(&mut Settings, String),
    ) {
        let row = adw::EntryRow::builder()
            .show_apply_button(true)
            .tooltip_text(subtitle)
            .build();
        let dirty = Rc::new(Cell::new(false));
        {
            let (view, dirty) = (Rc::downgrade(self), dirty.clone());
            row.connect_changed(move |_| {
                if view.upgrade().is_some_and(|v| !v.syncing.get()) {
                    dirty.set(true);
                }
            });
        }
        {
            let (view, dirty) = (Rc::downgrade(self), dirty.clone());
            row.connect_apply(move |r| {
                let Some(view) = view.upgrade() else {
                    return;
                };
                dirty.set(false);
                let text = r.text().trim().to_string();
                if text.is_empty() {
                    view.clear(field);
                } else {
                    view.edit(|s| set(s, text.clone()));
                }
            });
        }
        let weak = row.downgrade();
        self.add_row(
            group,
            field,
            &row,
            title,
            subtitle,
            Box::new(move |r, discard| {
                if let Some(row) = weak.upgrade() {
                    // Typing is saved by the apply button; a change to
                    // another option must not wipe what is still being typed.
                    if discard || !dirty.get() {
                        row.set_text(&get(r));
                        dirty.set(false);
                    }
                }
            }),
        );
    }
}

/// Shows a combo row's chosen value whole. The row's own factory ellipsizes
/// it to what a long subtitle leaves, so "Own screen" would show as "Own ...";
/// this one does not, and the subtitle wraps instead.
fn whole_value_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
            item.set_child(Some(&gtk::Label::builder().xalign(0.0).build()));
        }
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let text = item.item().and_downcast::<gtk::StringObject>();
        if let (Some(label), Some(text)) = (item.child().and_downcast::<gtk::Label>(), text) {
            label.set_label(&text.string());
        }
    });
    factory
}

fn group(title: &str, description: Option<&str>) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::builder().title(title).build();
    if let Some(d) = description {
        g.set_description(Some(d));
    }
    g
}

fn video_index(v: Video) -> u32 {
    match v {
        Video::Auto => 0,
        Video::Gpu => 1,
        Video::Cpu => 2,
    }
}

fn video_at(i: u32) -> Video {
    [Video::Auto, Video::Gpu, Video::Cpu][i as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_env(name: &str) -> Rc<Env> {
        let dir = std::env::temp_dir().join(format!("gliff-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Rc::new(Env {
            config_path: dir.join("config.toml"),
            cli: Settings::default(),
            changed: Rc::new(|| {}),
        })
    }

    #[test]
    #[ignore = "requires an isolated Wayland session; run by scripts/e2e.sh"]
    fn the_window_keeps_a_typed_command_and_is_freed_with_its_dialog() {
        adw::init().unwrap();
        let env = temp_env("settings-ui");
        let (view, dialog) = build(env.clone(), None);

        // A server command typed but not applied survives an edit to another
        // option, and a reset puts the saved value back.
        let (entry, switch) = {
            let rows = view.rows.borrow();
            let entry = rows
                .iter()
                .find_map(|r| r.widget.clone().downcast::<adw::EntryRow>().ok())
                .expect("a text option");
            let switch = rows
                .iter()
                .find_map(|r| r.widget.clone().downcast::<adw::SwitchRow>().ok())
                .expect("a switch option");
            (entry, switch)
        };
        let saved = entry.text().to_string();
        entry.set_text("typed but not applied");
        switch.set_active(!switch.is_active());
        assert!(
            Config::load(&env.config_path).defaults != Settings::default(),
            "the switch was not saved"
        );
        assert_eq!(
            entry.text(),
            "typed but not applied",
            "another edit wiped the typed command"
        );
        view.refresh(true);
        assert_eq!(entry.text(), saved, "a reset kept the typed command");

        // Closing the window frees the view: nothing else keeps it alive.
        let weak = Rc::downgrade(&view);
        drop((view, entry, switch));
        drop(dialog);
        let context = gtk::glib::MainContext::default();
        while context.iteration(false) {}
        assert!(weak.upgrade().is_none(), "the settings view was leaked");
        let _ = std::fs::remove_dir_all(env.config_path.parent().unwrap());
    }
}
