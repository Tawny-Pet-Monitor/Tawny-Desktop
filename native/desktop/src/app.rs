//! The window's behaviour: what each key does, and what is kept. The flows
//! are the web app's (app.js): channels, the name editor, the row menu,
//! delete, picking a job, first-run setup, the theme, the user's name, their
//! relays, and the walkthrough.

use std::cell::RefCell;
use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};

use crate::store::{self, Role, Store};
use crate::{AppWindow, Channel as UiChannel, Live, Theme};

/// The most channels kept (as the web app).
const MAX_CHANNELS: usize = 12;
/// The most relays a user lists.
const MAX_RELAYS: usize = 5;

/// What the window is doing, beyond what it shows.
#[derive(Default)]
struct State {
    /// The channel the role screen, menu or call is about.
    channel: Option<String>,
    /// The channel the name editor renames (`None`: a new one).
    editing: Option<String>,
}

/// Where a job starts once it is picked: the engine plugs in here.
pub trait Sessions {
    /// Starts this device's job on a channel.
    fn start(&self, ui: &AppWindow, store: &mut Store, channel: &store::Channel, role: Role);
    /// The user's name changed: the call hears of it.
    fn renamed(&self, name: &str);
}

/// Wires every key in the window to the store and the sessions.
pub fn attach(ui: &AppWindow, store: &Rc<RefCell<Store>>, sessions: &Rc<dyn Sessions>) {
    let state = Rc::new(RefCell::new(State::default()));
    {
        let st = store.borrow();
        ui.global::<Theme>()
            .set_mode(st.data.settings.theme.clone().into());
        ui.set_my_name(st.data.settings.my_name.clone().into());
        ui.set_relays(st.data.settings.relays.join("\n").into());
        ui.set_relays_only(st.data.settings.relays_only);
        let started = !st.data.channels.is_empty() || st.data.settings.imported;
        ui.set_screen(if started { "channels" } else { "welcome" }.into());
    }
    refresh(ui, &store.borrow());

    let w = ui.as_weak();
    let st = store.clone();
    ui.on_welcome_skip(move || {
        with(&w, |ui| {
            onboarded(&st);
            ui.set_screen("channels".into());
        });
    });
    let w = ui.as_weak();
    ui.on_welcome_go(move || with(&w, |ui| ui.set_screen("setup-role".into())));
    wire_first_run(ui, store, &state, sessions);
    wire_list(ui, store, &state, sessions);
    wire_editor(ui, store, &state, sessions);
    wire_menu(ui, store, &state, sessions);
    wire_roles(ui, store, &state, sessions);
    wire_settings(ui, store, sessions);
}

fn wire_first_run(
    ui: &AppWindow,
    store: &Rc<RefCell<Store>>,
    state: &Rc<RefCell<State>>,
    sessions: &Rc<dyn Sessions>,
) {
    // First run: the Monitor takes the first channel (or a new one).
    let (w, st, ss, sx) = (ui.as_weak(), store.clone(), state.clone(), sessions.clone());
    ui.on_pick_monitor(move || {
        with(&w, |ui| {
            let ch = {
                let mut s = st.borrow_mut();
                let ch = match s.data.channels.first() {
                    Some(c) => c.clone(),
                    None => s.add_channel("Pet camera"),
                };
                s.set_role(&ch.id, Role::Station);
                s.data.settings.imported = true;
                ch
            };
            save(ui, &st);
            refresh(ui, &st.borrow());
            ss.borrow_mut().channel = Some(ch.id.clone());
            sx.start(ui, &mut st.borrow_mut(), &ch, Role::Station);
        });
    });
    // First run, the Viewer: it joins by the Monitor's code.
    let (w, st) = (ui.as_weak(), store.clone());
    ui.on_pick_viewer(move || {
        with(&w, |ui| {
            onboarded(&st);
            ui.set_screen("channels".into());
            ui.set_sheet("scanner".into());
        });
    });
}

fn wire_list(
    ui: &AppWindow,
    store: &Rc<RefCell<Store>>,
    state: &Rc<RefCell<State>>,
    sessions: &Rc<dyn Sessions>,
) {
    // The list.
    let (w, st, ss, sx) = (ui.as_weak(), store.clone(), state.clone(), sessions.clone());
    ui.on_channel_open(move |id| {
        with(&w, |ui| open_role(ui, &st, &ss, sx.as_ref(), &id, false));
    });
    let (w, ss) = (ui.as_weak(), state.clone());
    ui.on_channel_more(move |id| {
        with(&w, |ui| {
            ss.borrow_mut().channel = Some(id.to_string());
            ui.set_sheet("row-menu".into());
        });
    });
    let (w, st, ss) = (ui.as_weak(), store.clone(), state.clone());
    ui.on_channel_add(move || {
        with(&w, |ui| {
            if st.borrow().data.channels.len() >= MAX_CHANNELS {
                ui.set_toast(
                    crate::tr::t("Tawny keeps up to 12 channels. Delete one to add another.")
                        .into(),
                );
                return;
            }
            ss.borrow_mut().editing = None;
            ui.set_editor_renaming(false);
            ui.set_editor_name(SharedString::new());
            ui.set_sheet("editor".into());
        });
    });
    let w = ui.as_weak();
    ui.on_set_up(move || with(&w, |ui| ui.set_screen("setup-role".into())));
    let w = ui.as_weak();
    ui.on_scan(move || with(&w, |ui| ui.set_sheet("scanner".into())));
    let w = ui.as_weak();
    ui.on_support(move || with(&w, |ui| ui.set_sheet("tip".into())));
    let w = ui.as_weak();
    ui.on_open_servers(move || with(&w, |ui| ui.set_sheet("servers".into())));
}

fn wire_editor(
    ui: &AppWindow,
    store: &Rc<RefCell<Store>>,
    state: &Rc<RefCell<State>>,
    sessions: &Rc<dyn Sessions>,
) {
    // The name editor.
    let (w, st, ss, sx) = (ui.as_weak(), store.clone(), state.clone(), sessions.clone());
    ui.on_editor_save(move |name| {
        with(&w, |ui| {
            let name = name.trim();
            if name.is_empty() {
                ui.set_toast(crate::tr::t("Give this channel a name.").into());
                return;
            }
            let editing = ss.borrow_mut().editing.take();
            let fresh = match editing {
                Some(id) => {
                    st.borrow_mut().rename(&id, name);
                    None
                }
                None => Some(st.borrow_mut().add_channel(name)),
            };
            save(ui, &st);
            ui.set_sheet(SharedString::new());
            refresh(ui, &st.borrow());
            // A channel just made is one about to be used: on to its job.
            if let Some(ch) = fresh {
                open_role(ui, &st, &ss, sx.as_ref(), &ch.id, false);
            }
        });
    });
}

fn wire_menu(
    ui: &AppWindow,
    store: &Rc<RefCell<Store>>,
    state: &Rc<RefCell<State>>,
    sessions: &Rc<dyn Sessions>,
) {
    // The row menu.
    let (w, st, ss) = (ui.as_weak(), store.clone(), state.clone());
    ui.on_menu_rename(move || {
        with(&w, |ui| {
            let id = ss.borrow().channel.clone();
            let Some(ch) = id.and_then(|id| st.borrow().channel(&id).cloned()) else {
                return;
            };
            ss.borrow_mut().editing = Some(ch.id.clone());
            ui.set_editor_renaming(true);
            ui.set_editor_name(ch.name.into());
            ui.set_sheet("editor".into());
        });
    });
    let (w, st, ss, sx) = (ui.as_weak(), store.clone(), state.clone(), sessions.clone());
    ui.on_menu_switch_role(move || {
        with(&w, |ui| {
            ui.set_sheet(SharedString::new());
            let id = ss.borrow().channel.clone();
            if let Some(id) = id {
                open_role(ui, &st, &ss, sx.as_ref(), &id, true);
            }
        });
    });
    let w = ui.as_weak();
    ui.on_menu_delete(move || with(&w, |ui| ui.set_sheet("del-confirm".into())));
    let (w, st, ss) = (ui.as_weak(), store.clone(), state.clone());
    ui.on_delete_confirmed(move || {
        with(&w, |ui| {
            if let Some(id) = ss.borrow_mut().channel.take() {
                st.borrow_mut().delete(&id);
                save(ui, &st);
            }
            ui.set_sheet(SharedString::new());
            refresh(ui, &st.borrow());
        });
    });
}

fn wire_roles(
    ui: &AppWindow,
    store: &Rc<RefCell<Store>>,
    state: &Rc<RefCell<State>>,
    sessions: &Rc<dyn Sessions>,
) {
    // The role screen.
    for role in [Role::Station, Role::Viewer] {
        let (w, st, ss, sx) = (ui.as_weak(), store.clone(), state.clone(), sessions.clone());
        let pick = move || {
            with(&w, |ui| {
                let id = ss.borrow().channel.clone();
                let Some(id) = id else { return };
                st.borrow_mut().set_role(&id, role);
                save(ui, &st);
                refresh(ui, &st.borrow());
                let ch = st.borrow().channel(&id).cloned();
                if let Some(ch) = ch {
                    sx.start(ui, &mut st.borrow_mut(), &ch, role);
                }
            });
        };
        match role {
            Role::Station => ui.on_role_monitor(pick),
            Role::Viewer => ui.on_role_viewer(pick),
        }
    }
}

fn wire_settings(ui: &AppWindow, store: &Rc<RefCell<Store>>, sessions: &Rc<dyn Sessions>) {
    // Settings.
    let (w, st) = (ui.as_weak(), store.clone());
    ui.on_theme_changed(move |mode| {
        with(&w, |ui| {
            st.borrow_mut().data.settings.theme = mode.to_string();
            save(ui, &st);
        });
    });
    let (w, st, sx) = (ui.as_weak(), store.clone(), sessions.clone());
    ui.on_my_name_save(move |name| {
        with(&w, |ui| {
            let name = store::clip(name.trim(), store::MY_NAME_MAX);
            ui.set_my_name(name.clone().into());
            st.borrow_mut().data.settings.my_name.clone_from(&name);
            save(ui, &st);
            ui.set_sheet(SharedString::new());
            sx.renamed(&name);
        });
    });
    let (w, st) = (ui.as_weak(), store.clone());
    ui.on_servers_save(move |text, only| {
        with(&w, |ui| match parse_relays(&text) {
            Ok(relays) => {
                ui.set_servers_note(SharedString::new());
                let was_only = st.borrow().data.settings.relays_only;
                {
                    let mut s = st.borrow_mut();
                    s.data.settings.relays = relays;
                    // "Only mine" is switched off here at once, but on only
                    // after the relays pass a test (the only sheet).
                    if !only {
                        s.data.settings.relays_only = false;
                    }
                }
                save(ui, &st);
                if only && !was_only {
                    ui.set_relays_only(false);
                    ui.set_sheet("only".into());
                } else {
                    ui.set_sheet(SharedString::new());
                }
            }
            Err(why) => ui.set_servers_note(why.into()),
        });
    });
    let (w, st) = (ui.as_weak(), store.clone());
    ui.global::<Live>().on_coach_done(move || {
        with(&w, |ui| {
            let role = ui.global::<Live>().get_role().to_string();
            let mut s = st.borrow_mut();
            if !s.data.settings.coach_seen.contains(&role) {
                s.data.settings.coach_seen.push(role);
            }
            drop(s);
            save(ui, &st);
        });
    });
}

/// Opens a channel: straight into its remembered job, or the role screen.
/// `switch` (the row menu's "Switch role") flips a remembered job instead.
fn open_role(
    ui: &AppWindow,
    store: &Rc<RefCell<Store>>,
    state: &Rc<RefCell<State>>,
    sessions: &dyn Sessions,
    id: &str,
    switch: bool,
) {
    let Some(ch) = store.borrow().channel(id).cloned() else {
        return;
    };
    state.borrow_mut().channel = Some(ch.id.clone());
    match (ch.role, switch) {
        (Some(role), false) => sessions.start(ui, &mut store.borrow_mut(), &ch, role),
        (Some(role), true) => {
            let other = match role {
                Role::Station => Role::Viewer,
                Role::Viewer => Role::Station,
            };
            store.borrow_mut().set_role(&ch.id, other);
            save(ui, store);
            refresh(ui, &store.borrow());
            sessions.start(ui, &mut store.borrow_mut(), &ch, other);
        }
        (None, _) => {
            ui.set_role_name(shown_name(&ch.name).into());
            ui.set_screen("role".into());
        }
    }
}

/// Puts the channels in the list.
pub fn refresh(ui: &AppWindow, store: &Store) {
    let rows: Vec<UiChannel> = store
        .data
        .channels
        .iter()
        .map(|c| UiChannel {
            id: c.id.clone().into(),
            name: shown_name(&c.name).into(),
            role: c.role.map_or("", Role::as_str).into(),
            key: c.key.chars().take(6).collect::<String>().into(),
        })
        .collect();
    ui.set_channels(ModelRc::new(VecModel::from(rows)));
}

/// The name a channel goes by: the stock names read as "Pet camera".
#[must_use]
pub fn shown_name(name: &str) -> String {
    let n = name.trim();
    let low = n.to_lowercase();
    if n.is_empty() || low == "pet camera" || low == "cámara de mascota" {
        crate::tr::t("Pet camera").into()
    } else {
        n.into()
    }
}

/// The user's relay list, one per line: `wss://` (or `ws://` on a private
/// address is the engine's call), at most five, no repeats.
fn parse_relays(text: &str) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let ok = line.starts_with("wss://") || line.starts_with("ws://");
        if !ok || line.contains(char::is_whitespace) {
            return Err(if crate::tr::spanish() {
                format!("“{line}” no es una dirección de relé (wss://…).")
            } else {
                format!("“{line}” is not a relay address (wss://…).")
            });
        }
        let line = line.trim_end_matches('/').to_owned();
        if !out.contains(&line) {
            out.push(line);
        }
    }
    if out.len() > MAX_RELAYS {
        return Err(crate::tr::t("List up to 5 relays.").into());
    }
    Ok(out)
}

fn onboarded(store: &Rc<RefCell<Store>>) {
    store.borrow_mut().data.settings.imported = true;
    // Best effort: a failed save only means the welcome shows again.
    let _ = store.borrow_mut().save();
}

/// Saves, and says so when it can't.
fn save(ui: &AppWindow, store: &Rc<RefCell<Store>>) {
    if let Err(e) = store.borrow_mut().save() {
        eprintln!("tawny: saving: {e:#}");
        ui.set_toast(crate::tr::t("Couldn’t save that. Is the disk full?").into());
    }
}

fn with(w: &Weak<AppWindow>, f: impl FnOnce(&AppWindow)) {
    if let Some(ui) = w.upgrade() {
        f(&ui);
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_relays, shown_name};

    #[test]
    fn relays_are_checked_and_tidied() {
        let got =
            parse_relays(" wss://a.example/ \n\nwss://b.example\nwss://a.example").expect("ok");
        assert_eq!(got, ["wss://a.example", "wss://b.example"]);
        assert!(parse_relays("https://a.example").is_err());
        assert!(parse_relays("wss://x.example\n").is_ok());
        let six = (0..6).fold(String::new(), |acc, i| {
            acc + &format!("wss://r{i}.example\n")
        });
        assert!(parse_relays(&six).is_err());
    }

    #[test]
    fn stock_names_read_as_pet_camera() {
        assert_eq!(shown_name("  "), "Pet camera");
        assert_eq!(shown_name("pet camera"), "Pet camera");
        assert_eq!(shown_name("Biscuit"), "Biscuit");
    }
}
