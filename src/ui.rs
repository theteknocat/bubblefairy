//! The window: chat list on the left, conversation on the right, composer at the bottom.
//! All state lives on the GTK thread; the backend (`mac.rs`) is reached only through `Cmd` / `Event`.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::{bubble, viewer};
use crate::mac::{Cmd, Config, Event};

const COMPOSE_PAD_X: i32 = 4;
const COMPOSE_PAD_Y: i32 = 9;
const OBJ: char = '\u{FFFC}'; // what imsg puts in `text` for attachment-only messages

pub struct Store {
    pub config_file: PathBuf,
    pub known_host: PathBuf,
    pub image_cache: PathBuf,
    pub public_key: String,
}

#[derive(Default)]
struct State {
    cfg: Config,
    chats: Vec<Value>,
    names: HashMap<String, String>,
    hist: HashMap<i64, Vec<Value>>,
    preview: HashMap<i64, Value>,
    preview_asked: HashSet<i64>,
    sel: Option<i64>,
    textures: HashMap<String, gdk::Texture>,
    waiting: HashMap<String, Vec<bubble::Bubble>>,
    /// photo widgets of the open chat in display order (the viewer cycles through these)
    photos: Vec<bubble::Bubble>,
    host_key: String,
    connected: bool,
    chats_loaded: bool,
    /// chats with a history request in flight (header spinner)
    loading: HashSet<i64>,
    older_loading: bool,
    /// chats whose whole history is already loaded
    no_more: HashSet<i64>,
}

pub struct Ui {
    app: gtk::Application,
    win: gtk::ApplicationWindow,
    store: Store,
    cmd: mpsc::UnboundedSender<Cmd>,
    viewer: Rc<viewer::Viewer>,
    st: RefCell<State>,
    // widgets
    chat_list: gtk::ListBox,
    msgs: gtk::Box,
    scroller: gtk::ScrolledWindow,
    stack: gtk::Stack,
    head_avatar: gtk::Box,
    head_title: gtk::Label,
    head_sub: gtk::Label,
    compose: gtk::TextView,
    compose_hint: gtk::Label,
    send_btn: gtk::Button,
    side_stack: gtk::Stack,
    head_spin: gtk::Spinner,
    status: gtk::Button,
    status_text: gtk::Label,
    banner: gtk::Revealer,
    banner_text: gtk::Label,
    // flags poked from signal handlers, so kept out of the RefCell
    stick_bottom: Cell<bool>,
    /// when not pinned to the bottom, keep this distance from the bottom while content changes above
    from_bottom: Cell<f64>,
    last_upper: Cell<f64>,
    selecting: Cell<bool>,
    sel_gen: Cell<u32>,
    banner_offline: Cell<bool>,
    chats_queued: Cell<bool>,
}

impl Ui {
    fn root_hover_for_dev(&self) {
        self.viewer.root.add_css_class("hover"); // show the controls in headless screenshots
    }

    fn send(&self, c: Cmd) {
        let _ = self.cmd.send(c);
    }
}

pub fn build(app: &gtk::Application, store: Store, cfg: Config, cmd: mpsc::UnboundedSender<Cmd>, mut ev: mpsc::UnboundedReceiver<Event>) {
    let win = gtk::ApplicationWindow::builder().application(app).title("BubbleFairy").default_width(1120).default_height(740).build();
    win.add_css_class("bf");

    // header: status pill, title, settings
    let head = gtk::HeaderBar::new();
    head.add_css_class("bf-head");
    let title = gtk::Label::new(None);
    title.set_markup("<span foreground='#7b5cff'>Bubble</span><span foreground='#ff5fa8'>Fairy</span> ✨");
    title.add_css_class("app-title");
    head.set_title_widget(Some(&title));
    let status_text = gtk::Label::new(Some("Starting"));
    let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    dot.add_css_class("dot");
    dot.set_valign(gtk::Align::Center);
    let pill = gtk::Box::new(gtk::Orientation::Horizontal, 7);
    pill.append(&dot);
    pill.append(&status_text);
    let status = gtk::Button::builder().child(&pill).css_classes(["status", "connecting"]).build();
    head.pack_start(&status);
    let settings_btn = gtk::Button::from_icon_name("emblem-system-symbolic");
    settings_btn.set_tooltip_text(Some("Settings"));
    settings_btn.add_css_class("head-btn");
    head.pack_end(&settings_btn);
    win.set_titlebar(Some(&head));

    // sidebar
    let chat_list = gtk::ListBox::new();
    chat_list.add_css_class("chat-list");
    let side_scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&chat_list).build();
    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar.add_css_class("sidebar");
    let side_title = gtk::Label::new(Some("Conversations"));
    side_title.add_css_class("side-title");
    side_title.set_xalign(0.0);
    sidebar.append(&side_title);
    let side_loading = gtk::Box::new(gtk::Orientation::Vertical, 10);
    side_loading.set_valign(gtk::Align::Center);
    side_loading.set_halign(gtk::Align::Center);
    let sp = gtk::Spinner::new();
    sp.start();
    sp.set_size_request(28, 28);
    side_loading.append(&sp);
    let sl = gtk::Label::new(Some("Loading chats…"));
    sl.add_css_class("loading-text");
    side_loading.append(&sl);
    let side_stack = gtk::Stack::new();
    side_stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    side_stack.add_named(&side_loading, Some("loading"));
    side_stack.add_named(&side_scroll, Some("list"));
    side_stack.set_vexpand(true);
    sidebar.append(&side_stack);

    // conversation
    let head_avatar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let head_title = gtk::Label::new(None);
    head_title.add_css_class("conv-title");
    head_title.set_xalign(0.0);
    head_title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let head_sub = gtk::Label::new(None);
    head_sub.add_css_class("conv-sub");
    head_sub.set_xalign(0.0);
    head_sub.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let titles = gtk::Box::new(gtk::Orientation::Vertical, 1);
    titles.set_valign(gtk::Align::Center);
    titles.set_hexpand(true);
    let head_spin = gtk::Spinner::new();
    head_spin.set_valign(gtk::Align::Center);
    head_spin.set_size_request(22, 22);
    titles.append(&head_title);
    titles.append(&head_sub);
    let conv_head = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    conv_head.add_css_class("conv-head");
    conv_head.append(&head_avatar);
    conv_head.append(&titles);
    conv_head.append(&head_spin);

    let msgs = gtk::Box::new(gtk::Orientation::Vertical, 0);
    msgs.add_css_class("msgs");
    msgs.set_valign(gtk::Align::End);
    // natural height: pictures can shrink to nothing, so their minimum height would understate the content
    let viewport = gtk::Viewport::builder().child(&msgs).vscroll_policy(gtk::ScrollablePolicy::Natural).build();
    let scroller = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&viewport).build();

    let compose = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .hexpand(true)
        .accepts_tab(false)
        .top_margin(COMPOSE_PAD_Y)
        .bottom_margin(COMPOSE_PAD_Y)
        .left_margin(COMPOSE_PAD_X)
        .right_margin(COMPOSE_PAD_X)
        .build();
    compose.add_css_class("compose");
    let compose_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(160)
        .child(&compose)
        .hexpand(true)
        .build();
    // A ScrolledWindow's own minimum height (its scrollbar) is taller than one line of text, so the
    // field only scrolls once the text outgrows the max height; until then it hugs the text.
    compose_scroll.set_vscrollbar_policy(gtk::PolicyType::External);
    let (cs, cv) = (compose_scroll.clone(), compose.clone());
    compose.buffer().connect_changed(move |_| {
        let (cs, cv) = (cs.clone(), cv.clone());
        glib::idle_add_local_once(move || {
            let (_, natural, _, _) = cv.measure(gtk::Orientation::Vertical, cv.width());
            let tall = natural > cs.max_content_height();
            cs.set_vscrollbar_policy(if tall { gtk::PolicyType::Automatic } else { gtk::PolicyType::External });
        });
    });
    let compose_hint = gtk::Label::new(None);
    compose_hint.add_css_class("compose-hint");
    // sits exactly where the first line of text starts, so the caret and the hint line up
    compose_hint.set_halign(gtk::Align::Start);
    compose_hint.set_valign(gtk::Align::Start);
    compose_hint.set_margin_top(COMPOSE_PAD_Y);
    compose_hint.set_margin_start(COMPOSE_PAD_X);
    compose_hint.set_can_target(false);
    let compose_box = gtk::Overlay::new();
    compose_box.set_child(Some(&compose_scroll));
    compose_box.add_overlay(&compose_hint);
    compose_box.set_hexpand(true);
    let hint = compose_hint.clone();
    compose.buffer().connect_changed(move |b| hint.set_visible(b.char_count() == 0));
    compose_box.set_valign(gtk::Align::Center);
    let emoji = gtk::MenuButton::new();
    let smile = gtk::Image::from_icon_name("face-smile-symbolic");
    smile.set_pixel_size(20);
    emoji.set_child(Some(&smile));
    emoji.set_tooltip_text(Some("Emoji"));
    emoji.add_css_class("emoji-btn");
    emoji.set_valign(gtk::Align::End);
    let chooser = gtk::EmojiChooser::new();
    emoji.set_popover(Some(&chooser));
    let buf = compose.buffer();
    chooser.connect_emoji_picked(move |_, e| buf.insert_at_cursor(e));
    let send_btn = gtk::Button::from_icon_name("go-up-symbolic");
    send_btn.add_css_class("send-btn");
    send_btn.set_sensitive(false); // enabled once there's something to send
    let sb = send_btn.clone();
    compose.buffer().connect_changed(move |b| sb.set_sensitive(b.char_count() > 0));
    send_btn.set_tooltip_text(Some("Send (Enter)"));
    send_btn.set_valign(gtk::Align::End);
    let composer = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    composer.add_css_class("composer");
    composer.append(&compose_box);
    composer.append(&emoji);
    composer.append(&send_btn);

    let conv = gtk::Box::new(gtk::Orientation::Vertical, 0);
    conv.append(&conv_head);
    conv.append(&scroller);
    conv.append(&composer);

    let placeholder = gtk::Label::new(Some("Pick a conversation"));
    placeholder.add_css_class("placeholder");
    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.add_named(&placeholder, Some("empty"));
    stack.add_named(&conv, Some("conv"));
    let conv_panel = gtk::Box::new(gtk::Orientation::Vertical, 0);
    conv_panel.add_css_class("conv");
    // the photo viewer covers the whole panel (header and composer too)
    let viewer = viewer::Viewer::new();
    let conv_overlay = gtk::Overlay::new();
    conv_overlay.set_child(Some(&stack));
    conv_overlay.add_overlay(&viewer.root);
    conv_overlay.set_vexpand(true);
    conv_panel.set_overflow(gtk::Overflow::Hidden); // keeps the viewer inside the panel's rounded corners
    conv_panel.append(&conv_overlay);
    stack.set_vexpand(true);
    let _ = &stack;

    // warnings banner above everything
    let banner_text = gtk::Label::new(None);
    banner_text.set_wrap(true);
    banner_text.set_xalign(0.0);
    banner_text.set_hexpand(true);
    let banner_close = gtk::Button::from_icon_name("window-close-symbolic");
    banner_close.add_css_class("flat");
    banner_close.set_valign(gtk::Align::Center);
    let banner_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    banner_box.add_css_class("banner");
    banner_box.append(&banner_text);
    banner_box.append(&banner_close);
    let banner = gtk::Revealer::builder().child(&banner_box).transition_type(gtk::RevealerTransitionType::SlideDown).build();

    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_start_child(Some(&sidebar));
    paned.set_end_child(Some(&conv_panel));
    paned.set_position(340);
    paned.set_shrink_start_child(false);
    paned.set_resize_start_child(false);
    paned.set_vexpand(true);
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&banner);
    root.append(&paned);
    win.set_child(Some(&root));

    let ui = Rc::new(Ui {
        app: app.clone(),
        win: win.clone(),
        store,
        cmd,
        viewer,
        st: RefCell::new(State { cfg, ..Default::default() }),
        chat_list,
        msgs,
        scroller,
        stack,
        head_avatar,
        head_title,
        head_sub,
        compose,
        compose_hint,
        send_btn,
        side_stack,
        head_spin,
        status,
        status_text,
        banner,
        banner_text,
        stick_bottom: Cell::new(true),
        from_bottom: Cell::new(0.0),
        last_upper: Cell::new(0.0),
        selecting: Cell::new(false),
        sel_gen: Cell::new(0),
        banner_offline: Cell::new(false),
        chats_queued: Cell::new(false),
    });

    // wiring
    let u = ui.clone();
    banner_close.connect_clicked(move |_| u.banner.set_reveal_child(false));
    let u = ui.clone();
    settings_btn.connect_clicked(move |_| settings(&u));
    let u = ui.clone();
    ui.status.connect_clicked(move |_| settings(&u));
    let u = ui.clone();
    ui.send_btn.connect_clicked(move |_| send_now(&u));
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let u = ui.clone();
    keys.connect_key_pressed(move |_, key, _, mods| {
        let enter = matches!(key, gdk::Key::Return | gdk::Key::KP_Enter);
        if enter && !mods.contains(gdk::ModifierType::SHIFT_MASK) {
            send_now(&u);
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    ui.compose.add_controller(keys);

    let u = ui.clone();
    ui.chat_list.connect_row_selected(move |_, row| {
        if u.selecting.get() {
            return;
        }
        let Some(row) = row else { return };
        let id = u.st.borrow().chats.get(row.index() as usize).and_then(|c| c["id"].as_i64());
        if let Some(id) = id {
            select(&u, id);
        }
    });

    // keep the conversation pinned to the newest message unless the user scrolled up
    let adj = ui.scroller.vadjustment();
    let u = ui.clone();
    adj.connect_changed(move |a| {
        u.last_upper.set(a.upper());
        // "changed" fires while GTK is laying out the viewport; a value set now moves the adjustment but
        // the viewport misses the redraw (the old "last bubble cut off" bug), so reposition right after
        let (a, u) = (a.clone(), u.clone());
        glib::idle_add_local_once(move || {
            if u.stick_bottom.get() {
                a.set_value(a.upper() - a.page_size());
            } else {
                a.set_value(a.upper() - u.from_bottom.get());
            }
        });
    });
    let u = ui.clone();
    adj.connect_value_changed(move |a| {
        // GTK moves the value while it resizes the content; only react to scrolls at a settled size,
        // otherwise a half-laid-out frame would unpin us from the bottom
        if a.upper() != u.last_upper.get() {
            return;
        }
        u.stick_bottom.set(a.value() >= a.upper() - a.page_size() - 40.0);
        u.from_bottom.set(a.upper() - a.value());
        if a.value() < 400.0 && a.upper() > a.page_size() {
            load_older(&u);
        }
    });

    let open = gio::SimpleAction::new("open-chat", Some(glib::VariantTy::INT64));
    let u = ui.clone();
    open.connect_activate(move |_, p| {
        u.win.present();
        if let Some(id) = p.and_then(|p| p.get::<i64>()) {
            select_in_list(&u, id);
            select(&u, id);
        }
    });
    app.add_action(&open);

    let u = ui.clone();
    glib::spawn_future_local(async move {
        while let Some(e) = ev.recv().await {
            on_event(&u, e);
        }
    });

    // Follow the system light/dark setting (GTK fills this from the desktop's portal).
    let gs = gtk::Settings::default().expect("settings");
    let a = app.clone();
    let apply = move |s: &gtk::Settings| {
        let dark = s.gtk_interface_color_scheme() == gtk::InterfaceColorScheme::Dark;
        for w in a.windows() {
            if dark { w.add_css_class("dark") } else { w.remove_css_class("dark") }
        }
    };
    apply(&gs);
    let a2 = apply.clone();
    gs.connect_gtk_interface_color_scheme_notify(move |s| a2(s));
    let a4 = apply.clone();
    app.connect_window_added(move |_, _| a4(&gtk::Settings::default().unwrap()));

    win.present();
    if let Ok(t) = std::env::var("BF_DRAFT") {
        ui.compose.buffer().set_text(&t.replace("\\n", "\n")); // dev aid: prefill the composer
    }
    if std::env::var_os("BF_VIEW").is_some() {
        // dev aid: open the viewer on the newest loaded photo shortly before BF_SHOT fires
        let ms = std::env::var("BF_SHOT_MS").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(2500);
        let u = ui.clone();
        glib::timeout_add_local_once(Duration::from_millis(ms.saturating_sub(900)), move || {
            let items: Vec<bubble::Bubble> = u.st.borrow().photos.iter().filter(|b| b.texture().is_some()).cloned().collect();
            if let Some(last) = items.last().cloned() {
                u.root_hover_for_dev();
                u.viewer.open(items, &last);
            }
        });
    }
    if let Some(out) = std::env::var_os("BF_SHOT") {
        // dev aid: save the window to a PNG and quit (used with BF_DEMO in a headless compositor)
        let w = win.clone();
        let ms = std::env::var("BF_SHOT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(2500);
        glib::timeout_add_local_once(Duration::from_millis(ms), move || {
            let p = gtk::WidgetPaintable::new(Some(&w));
            let (width, height) = (w.width() as f64, w.height() as f64);
            let snap = gtk::Snapshot::new();
            p.snapshot(&snap, width, height);
            if let (Some(node), Some(r)) = (snap.to_node(), w.renderer()) {
                let _ = r.render_texture(&node, None).save_to_png(&out);
            }
            w.application().unwrap().quit();
        });
    }
    if ui.st.borrow().cfg.host.is_empty() {
        settings(&ui);
    }
}

// ---------- events from the Mac ----------

fn on_event(u: &Rc<Ui>, e: Event) {
    match e {
        Event::Connecting => set_status(u, "connecting", "Connecting", None),
        Event::Connected => {
            u.st.borrow_mut().connected = true;
            set_status(u, "online", "Connected", None);
            if u.banner_offline.get() {
                u.banner.set_reveal_child(false);
            }
            u.send(Cmd::Chats);
            let sel = u.st.borrow().sel;
            if let Some(id) = sel {
                request_history(u, id);
            }
        }
        Event::Offline(why) => {
            u.st.borrow_mut().connected = false;
            set_status(u, "offline", "Offline", Some(&why));
            show_banner(u, &format!("Can't reach your Mac: {why}"), true);
        }
        Event::Warn(w) => show_banner(u, &cap(&w), false),
        Event::HostKey(k) => u.st.borrow_mut().host_key = k,
        Event::Chats(chats) => {
            let ask: Vec<i64> = {
                let mut st = u.st.borrow_mut();
                for c in &chats {
                    if c["is_group"].as_bool() != Some(true)
                        && let (Some(h), Some(n)) = (c["participants"][0].as_str(), c["contact_name"].as_str())
                        && !n.is_empty()
                    {
                        st.names.insert(h.into(), n.into());
                    }
                }
                st.chats = chats;
                st.chats_loaded = true;
                let st = &mut *st;
                let ids: Vec<i64> = st.chats.iter().filter_map(|c| c["id"].as_i64()).collect();
                let ask: Vec<i64> =
                    ids.into_iter().filter(|id| !st.preview.contains_key(id) && !st.hist.contains_key(id)).take(60).collect();
                st.preview_asked.extend(ask.iter().copied());
                ask
            };
            for id in ask {
                u.send(Cmd::Preview(id));
            }
            u.side_stack.set_visible_child_name("list");
            render_chats(u);
            let first = {
                let st = u.st.borrow();
                // BF_CHAT=<id>: dev aid to open a given chat first
                let want = std::env::var("BF_CHAT").ok().and_then(|v| v.parse::<i64>().ok());
                if st.sel.is_none() { want.or_else(|| st.chats.first().and_then(|c| c["id"].as_i64())) } else { None }
            };
            if let Some(id) = first {
                select_in_list(u, id);
                select(u, id);
            }
        }
        Event::Preview(id, m) => {
            learn_names(u, std::slice::from_ref(&m));
            u.st.borrow_mut().preview.insert(id, m);
            render_chats(u);
        }
        Event::History(id, msgs) => {
            learn_names(u, &msgs);
            let is_sel = {
                let mut st = u.st.borrow_mut();
                if let Some(last) = msgs.last() {
                    st.preview.insert(id, last.clone());
                }
                st.hist.insert(id, msgs);
                st.loading.remove(&id);
                st.sel == Some(id)
            };
            if is_sel {
                render_conv(u);
            }
        }
        Event::Older(id, older) => {
            learn_names(u, &older);
            let is_sel = {
                let mut st = u.st.borrow_mut();
                st.older_loading = false;
                let h = st.hist.entry(id).or_default();
                let have: HashSet<String> = h.iter().filter_map(|m| m["guid"].as_str().map(String::from)).collect();
                let fresh: Vec<Value> = older.into_iter().filter(|m| m["guid"].as_str().is_some_and(|g| !have.contains(g))).collect();
                if fresh.is_empty() {
                    st.no_more.insert(id);
                } else {
                    let rest = std::mem::take(h);
                    *h = fresh.into_iter().chain(rest).collect();
                }
                st.sel == Some(id)
            };
            if is_sel {
                render_conv(u);
            }
        }
        Event::Message(m) => on_message(u, m),
        Event::Resync => {
            u.send(Cmd::Chats);
            let sel = u.st.borrow().sel;
            if let Some(id) = sel {
                request_history(u, id);
            }
        }
        Event::SendSlow => show_banner(
            u,
            "Still sending… If this is your first message from BubbleFairy, check your Mac: it may be asking whether to allow control of Messages.",
            false,
        ),
        Event::Sent { text, err, .. } => {
            if let Some(err) = err {
                show_banner(u, &format!("Message not sent: {err}"), false);
                let buf = u.compose.buffer();
                if buf.char_count() == 0 {
                    buf.set_text(&text); // nothing typed is lost
                }
            } else if !u.banner_offline.get() {
                u.banner.set_reveal_child(false);
            }
        }
        Event::Image { path, bytes } => {
            let tex = bytes.and_then(|b| {
                let _ = std::fs::create_dir_all(&u.store.image_cache);
                let _ = std::fs::write(cache_path(u, &path), &b);
                texture(&b)
            });
            let pics = u.st.borrow_mut().waiting.remove(&path).unwrap_or_default();
            if let Some(tex) = tex {
                for p in pics {
                    show_texture(&p, &tex);
                }
                u.st.borrow_mut().textures.insert(path, tex);
            }
        }
    }
}

fn on_message(u: &Rc<Ui>, m: Value) {
    learn_names(u, std::slice::from_ref(&m));
    let Some(cid) = m["chat_id"].as_i64() else { return };
    let mutation = m["type"].as_str().is_some();
    let (is_sel, known_chat) = {
        let mut st = u.st.borrow_mut();
        if let Some(h) = st.hist.get_mut(&cid) {
            match h.iter_mut().find(|x| x["guid"] == m["guid"]) {
                Some(old) => *old = m.clone(),
                None if !mutation => h.push(m.clone()),
                None => {}
            }
        }
        if !mutation {
            st.preview.insert(cid, m.clone());
        }
        (st.sel == Some(cid), st.chats.iter().any(|c| c["id"].as_i64() == Some(cid)))
    };
    if is_sel {
        render_conv(u);
    }
    if mutation {
        return;
    }
    // chat order and unread counts come from the Mac; coalesce bursts into one reload
    if !u.chats_queued.replace(true) {
        let u2 = u.clone();
        glib::timeout_add_local_once(Duration::from_millis(700), move || {
            u2.chats_queued.set(false);
            u2.send(Cmd::Chats);
        });
    }
    if !known_chat {
        return;
    }
    render_chats(u);
    if m["is_from_me"].as_bool() != Some(true) && (!u.win.is_active() || !is_sel) {
        notify(u, cid, &m);
    }
}

fn learn_names(u: &Ui, msgs: &[Value]) {
    let mut st = u.st.borrow_mut();
    for m in msgs {
        if let (Some(h), Some(n)) = (m["sender"].as_str(), m["sender_name"].as_str())
            && !n.is_empty()
        {
            st.names.insert(h.into(), n.into());
        }
    }
}

fn notify(u: &Ui, cid: i64, m: &Value) {
    let st = u.st.borrow();
    let Some(chat) = st.chats.iter().find(|c| c["id"].as_i64() == Some(cid)) else { return };
    let mut title = chat_title(chat, &st.names);
    if chat["is_group"].as_bool() == Some(true) {
        title = format!("{} in {title}", sender_name(m, &st.names));
    }
    let n = gio::Notification::new(&title);
    n.set_body(Some(&summary(m)));
    n.set_default_action_and_target_value("app.open-chat", Some(&cid.to_variant()));
    u.app.send_notification(Some(&format!("chat-{cid}")), &n);
}

// ---------- status and banner ----------

fn set_status(u: &Ui, kind: &str, text: &str, tip: Option<&str>) {
    for k in ["online", "connecting", "offline"] {
        u.status.remove_css_class(k);
    }
    u.status.add_css_class(kind);
    u.status_text.set_text(text);
    u.status.set_tooltip_text(Some(tip.unwrap_or("Connection to your Mac. Click for settings.")));
}

fn show_banner(u: &Ui, text: &str, offline: bool) {
    u.banner_text.set_text(text);
    u.banner_offline.set(offline);
    u.banner.set_reveal_child(true);
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

// ---------- selecting and sending ----------

fn select_in_list(u: &Ui, id: i64) {
    let idx = u.st.borrow().chats.iter().position(|c| c["id"].as_i64() == Some(id));
    if let Some(row) = idx.and_then(|i| u.chat_list.row_at_index(i as i32)) {
        u.selecting.set(true);
        u.chat_list.select_row(Some(&row));
        u.selecting.set(false);
    }
}

fn select(u: &Rc<Ui>, id: i64) {
    u.viewer.close_now();
    u.st.borrow_mut().sel = Some(id);
    u.stick_bottom.set(true);
    render_conv(u); // cached view right away, fresh history shortly
    // debounce: holding an arrow key shouldn't fire one history call per row
    let g = u.sel_gen.get().wrapping_add(1);
    u.sel_gen.set(g);
    let u2 = u.clone();
    glib::timeout_add_local_once(Duration::from_millis(150), move || {
        if u2.sel_gen.get() == g && u2.st.borrow().connected {
            request_history(&u2, id);
        }
    });
    u.compose.grab_focus();
}

fn request_history(u: &Rc<Ui>, id: i64) {
    u.st.borrow_mut().loading.insert(id);
    u.send(Cmd::History(id));
    update_spinner(u);
}

fn update_spinner(u: &Ui) {
    let on = {
        let st = u.st.borrow();
        st.sel.is_some_and(|id| st.loading.contains(&id))
    };
    u.head_spin.set_spinning(on);
    u.head_spin.set_visible(on);
}

/// Scrolled near the top: fetch the next page of older messages.
fn load_older(u: &Rc<Ui>) {
    let req = {
        let mut st = u.st.borrow_mut();
        let Some(id) = st.sel else { return };
        if st.older_loading || st.no_more.contains(&id) || !st.connected {
            return;
        }
        let Some(before) = st.hist.get(&id).and_then(|h| h.first()).and_then(|m| m["created_at"].as_str()).map(String::from) else {
            return;
        };
        st.older_loading = true;
        (id, before)
    };
    u.send(Cmd::Older { id: req.0, before: req.1 });
    let u2 = u.clone();
    glib::idle_add_local_once(move || render_conv(&u2)); // shows the "Loading more" row; not inside a scroll signal
}

fn send_now(u: &Ui) {
    let buf = u.compose.buffer();
    let text = buf.text(&buf.start_iter(), &buf.end_iter(), false).trim().to_string();
    let Some(chat_id) = u.st.borrow().sel else { return };
    if text.is_empty() {
        return;
    }
    buf.set_text(""); // also disables the send button (buffer changed)
    u.stick_bottom.set(true);
    u.send(Cmd::Send { chat_id, text }); // the message itself comes back through the live stream
}

// ---------- chat list ----------

fn render_chats(u: &Rc<Ui>) {
    let rows: Vec<(String, String, String, bool, i64)> = {
        let st = u.st.borrow();
        st.chats
            .iter()
            .map(|c| {
                let id = c["id"].as_i64().unwrap_or(0);
                let p = st.preview.get(&id);
                let when = p.and_then(|m| m["created_at"].as_str()).or(c["last_message_at"].as_str()).and_then(local).map(|d| short_time(&d));
                let text = p.map(|m| {
                    let s = summary(m);
                    if m["is_from_me"].as_bool() == Some(true) { format!("You: {s}") } else { s }
                });
                let unread = c["unread_count"].as_i64().unwrap_or(0) > 0;
                (chat_title(c, &st.names), text.unwrap_or_default(), when.unwrap_or_default(), unread, id)
            })
            .collect()
    };
    let sel = u.st.borrow().sel;
    u.selecting.set(true);
    u.chat_list.remove_all();
    for (title, preview, when, unread, id) in rows {
        let name = gtk::Label::new(Some(&title));
        name.add_css_class("chat-name");
        name.set_xalign(0.0);
        name.set_hexpand(true);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let time = gtk::Label::new(Some(&when));
        time.add_css_class("chat-time");
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        top.append(&name);
        top.append(&time);
        let prev = gtk::Label::new(Some(&preview));
        prev.add_css_class("chat-preview");
        prev.set_xalign(0.0);
        prev.set_ellipsize(gtk::pango::EllipsizeMode::End);
        prev.set_single_line_mode(true);
        let texts = gtk::Box::new(gtk::Orientation::Vertical, 2);
        texts.set_hexpand(true);
        texts.set_valign(gtk::Align::Center);
        texts.append(&top);
        texts.append(&prev);
        let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 11);
        row_box.append(&avatar(&title, false));
        row_box.append(&texts);
        let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        dot.set_valign(gtk::Align::Center);
        dot.add_css_class(if unread { "unread-dot" } else { "read-dot" });
        row_box.append(&dot);
        let row = gtk::ListBoxRow::new();
        row.add_css_class("chat-row");
        if unread {
            row.add_css_class("unread");
        }
        row.set_child(Some(&row_box));
        u.chat_list.append(&row);
        if Some(id) == sel {
            u.chat_list.select_row(Some(&row));
        }
    }
    u.selecting.set(false);
}

// ---------- conversation ----------

fn render_conv(u: &Rc<Ui>) {
    update_spinner(u);
    let (chat, msgs, names, older) = {
        let st = u.st.borrow();
        let Some(id) = st.sel else { return };
        let Some(chat) = st.chats.iter().find(|c| c["id"].as_i64() == Some(id)).cloned() else { return };
        (chat, st.hist.get(&id).cloned().unwrap_or_default(), st.names.clone(), st.older_loading)
    };
    u.stack.set_visible_child_name("conv");
    let title = chat_title(&chat, &names);
    let chat_svc = chat["service"].as_str().unwrap_or("iMessage").to_string();
    let sms = chat_svc != "iMessage";
    u.compose_hint.set_text(&if sms { format!("Text Message · {chat_svc}") } else { "iMessage".into() });
    let group = chat["is_group"].as_bool() == Some(true);
    u.head_title.set_text(&title);
    let mut sub = chat["service"].as_str().unwrap_or("").to_string();
    if group {
        let people: Vec<String> = chat["participants"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p.as_str())
            .map(|p| names.get(p).cloned().unwrap_or_else(|| p.to_string()))
            .collect();
        sub = format!("{sub} · {}", people.join(", "));
    } else if let Some(h) = chat["participants"][0].as_str()
        && h != title
    {
        sub = format!("{sub} · {h}");
    }
    u.head_sub.set_text(&sub);
    while let Some(c) = u.head_avatar.first_child() {
        u.head_avatar.remove(&c);
    }
    u.head_avatar.append(&avatar(&title, false));
    for c in ["sms", "imessage"] {
        u.send_btn.remove_css_class(c);
    }
    u.send_btn.add_css_class(if sms { "sms" } else { "imessage" });

    // the scroll position is kept by the adjustment handlers in build() (anchored to the bottom)
    u.st.borrow_mut().photos.clear();
    while let Some(c) = u.msgs.first_child() {
        u.msgs.remove(&c);
    }
    if older {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.set_halign(gtk::Align::Center);
        row.add_css_class("loading-more");
        let sp = gtk::Spinner::new();
        sp.start();
        row.append(&sp);
        row.append(&gtk::Label::new(Some("Loading more messages…")));
        u.msgs.append(&row);
    }
    let times: Vec<Option<i64>> = msgs.iter().map(|m| m["created_at"].as_str().and_then(local).map(|d| d.to_unix())).collect();
    let who = |m: &Value| (m["is_from_me"].as_bool() == Some(true), m["sender"].as_str().unwrap_or("").to_string());
    for (i, m) in msgs.iter().enumerate() {
        if m["is_reaction"].as_bool() == Some(true) {
            continue;
        }
        let me = m["is_from_me"].as_bool() == Some(true);
        let gap = |a: usize, b: usize| match (times[a], times[b]) {
            (Some(x), Some(y)) => (y - x).abs(),
            _ => 0,
        };
        let sep = i == 0 || gap(i - 1, i) > 30 * 60 || day(times[i - 1]) != day(times[i]);
        if sep && let Some(dt) = m["created_at"].as_str().and_then(local) {
            let l = gtk::Label::new(Some(&sep_time(&dt)));
            l.add_css_class("time-sep");
            u.msgs.append(&l);
        }
        // a "run" = same sender, close together: shares one name label and one avatar
        let run_prev = !sep && i > 0 && who(&msgs[i - 1]) == who(m) && gap(i - 1, i) < 180;
        let run_next = i + 1 < msgs.len() && who(&msgs[i + 1]) == who(m) && gap(i, i + 1) < 180 && day(times[i]) == day(times[i + 1]);
        // bubbles only look continuous with bubbles: photos (shown above a message's bubble) break the chain
        let join_prev = run_prev && has_bubble(&msgs[i - 1]) && !has_photos(m);
        let join_next = run_next && has_bubble(&msgs[i + 1]) && !has_photos(&msgs[i + 1]);

        if group && !me && !run_prev {
            let n = gtk::Label::new(Some(&sender_name(m, &names)));
            n.add_css_class("sender-name");
            n.set_xalign(0.0);
            u.msgs.append(&n);
        }
        let svc = m["service"].as_str().unwrap_or(&chat_svc);
        let parts = message_parts(u, m, me, svc != "iMessage", join_prev, join_next, !run_next);
        let n = parts.len();
        for (k, part) in parts.into_iter().enumerate() {
            let last = k + 1 == n;
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            line.add_css_class("msg-line");
            if !last || join_next {
                line.add_css_class("tight");
            }
            line.set_halign(if me { gtk::Align::End } else { gtk::Align::Start });
            if group && !me {
                if last && !run_next {
                    let a = avatar(&sender_name(m, &names), true);
                    a.set_valign(gtk::Align::End);
                    line.append(&a);
                } else {
                    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                    spacer.add_css_class("avatar-spacer");
                    line.append(&spacer);
                }
            }
            line.append(&part);
            u.msgs.append(&line);
        }
    }
}

/// Photos to show for a message. RCS keeps a converted copy next to a missing original, and some rows
/// carry the same photo twice in different folders, so keep one present file per name stem.
fn photo_paths(m: &Value) -> Vec<String> {
    let mut seen = HashSet::new();
    m["attachments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| a["missing"].as_bool() != Some(true))
        .filter_map(|a| {
            let path = a["original_path"].as_str().filter(|p| !p.is_empty())?;
            if !is_image(a["mime_type"].as_str().unwrap_or(""), path) {
                return None;
            }
            let name = path.rsplit('/').next().unwrap_or(path);
            seen.insert(name.split('.').next().unwrap_or(name).to_string()).then(|| path.to_string())
        })
        .collect()
}

fn has_photos(m: &Value) -> bool {
    !photo_paths(m).is_empty()
}

/// Whether a message draws a bubble: text, "unsent", or an attachment row (a file, or a photo the
/// Mac never downloaded). Photo-only messages don't.
fn has_bubble(m: &Value) -> bool {
    if m["unsent_at"].as_str().is_some() || m["type"].as_str() == Some("unsent") || !clean_text(m).is_empty() {
        return true;
    }
    let atts = m["attachments"].as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    let any_present = atts.iter().any(|a| a["missing"].as_bool() != Some(true));
    // same filter as message_parts: missing duplicates hide when a present copy exists
    atts.iter().filter(|a| !any_present || a["missing"].as_bool() != Some(true)).any(|a| {
        let path = a["original_path"].as_str().unwrap_or("");
        a["missing"].as_bool() == Some(true) || path.is_empty() || !is_image(a["mime_type"].as_str().unwrap_or(""), path)
    })
}

/// One message as display rows: its photos (as cards, outside any bubble), then a bubble with the
/// text and any other attachments. Only the bubble of the last message in a run gets a tail.
/// `end_of_run`: this message is the sender's last in the run, so its last item (bubble or photo)
/// carries the tail. Nothing else does.
fn message_parts(u: &Rc<Ui>, m: &Value, me: bool, sms: bool, join_prev: bool, join_next: bool, end_of_run: bool) -> Vec<gtk::Widget> {
    let look = |join_prev| bubble::Look { me, sms, tail: end_of_run, join_prev, join_next };
    if m["unsent_at"].as_str().is_some() || m["type"].as_str() == Some("unsent") {
        let b = bubble::Bubble::new(look(join_prev));
        let l = gtk::Label::new(Some("Message unsent"));
        l.add_css_class("unsent");
        b.add(&l);
        return vec![b.upcast()];
    }
    let atts: Vec<&Value> = m["attachments"].as_array().into_iter().flatten().collect();
    // RCS/SMS photos often come as a converted copy plus a missing original; show only what's there
    let any_present = atts.iter().any(|a| a["missing"].as_bool() != Some(true));
    let photos = photo_paths(m);
    let mut others = Vec::new();
    for a in atts.into_iter().filter(|a| !any_present || a["missing"].as_bool() != Some(true)) {
        let path = a["original_path"].as_str().unwrap_or("");
        let mime = a["mime_type"].as_str().unwrap_or("");
        if is_image(mime, path) && !path.is_empty() && a["missing"].as_bool() != Some(true) {
            continue; // shown as a photo card (photo_paths)
        } else {
            let name = a["transfer_name"].as_str().or(a["filename"].as_str()).unwrap_or("Attachment");
            let (icon, label) = if a["missing"].as_bool() == Some(true) && is_image(mime, name) {
                ("📷", "Photo not downloaded on your Mac".to_string())
            } else {
                ("📎", name.to_string())
            };
            let l = gtk::Label::new(None);
            l.set_markup(&format!("<span size='170%'>{icon}</span>  {}", glib::markup_escape_text(&label)));
            l.add_css_class("file");
            l.set_xalign(0.0);
            l.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            l.set_max_width_chars(40);
            others.push(l);
        }
    }
    let mut out: Vec<gtk::Widget> = Vec::new();
    let text = clean_text(m);
    let has_bubble = !text.is_empty() || !others.is_empty();
    if !photos.is_empty() {
        out.push(photo_stack(u, &photos, bubble::Look { me, sms, tail: end_of_run && !has_bubble, ..Default::default() }));
    }
    if has_bubble {
        let b = bubble::Bubble::new(look(join_prev && photos.is_empty()));
        for l in others {
            b.add(&l);
        }
        if !text.is_empty() {
            let l = gtk::Label::new(Some(&text));
            l.set_wrap(true);
            l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            l.set_xalign(0.0);
            l.set_max_width_chars(60);
            l.set_selectable(true);
            l.set_can_focus(false);
            l.add_css_class("text");
            b.add(&l);
        }
        if m["edited_at"].as_str().is_some() {
            let l = gtk::Label::new(Some("Edited"));
            l.add_css_class("edited");
            l.set_xalign(if me { 1.0 } else { 0.0 });
            b.add(&l);
        }
        out.push(b.upcast());
    }
    out
}

const CARD: i32 = 210; // stacked photo card size
const CARD_STEP: f64 = 26.0; // how far each card under the top one peeks out
const CARD_SPREAD: f64 = 150.0; // ...and how far on hover
const CARDS_SHOWN: usize = 4;

/// One photo on its own, or several from the same message fanned out like cards: the first on top,
/// the rest peeking out underneath, away from the side the message sits on. Hovering spreads them.
/// `look.tail` puts the tail on the top card.
fn photo_stack(u: &Rc<Ui>, paths: &[String], look: bubble::Look) -> gtk::Widget {
    let me = look.me;
    if paths.len() == 1 {
        return image(u, &paths[0], None, look).upcast();
    }
    let shown = paths.len().min(CARDS_SHOWN);
    let side = if me { gtk::Align::End } else { gtk::Align::Start };
    let base = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let o = gtk::Overlay::new();
    o.set_child(Some(&base));
    o.set_halign(side);
    // created in message order (that's the viewer's order); photos past CARDS_SHOWN aren't drawn
    // in the stack but stay reachable in the viewer
    let all: Vec<bubble::Bubble> = paths
        .iter()
        .enumerate()
        .map(|(i, path)| image(u, path, Some(CARD), bubble::Look { tail: look.tail && i == 0, ..look }))
        .collect();
    let mut cards = Vec::new();
    for i in (0..shown).rev() {
        let p = all[i].clone();
        p.set_halign(side);
        p.set_valign(gtk::Align::Start);
        o.add_overlay(&p); // added last = drawn on top, so the first photo ends up on top
        cards.push((i, p));
    }
    // lay the cards out for a given spacing; the base box gives the overlay its size
    let place = move |step: f64| {
        let card_w = CARD + bubble::TAIL as i32;
        base.set_size_request(card_w + (step * (shown as f64 - 1.0)) as i32, CARD);
        for (i, p) in &cards {
            let off = (step * *i as f64) as i32;
            if me { p.set_margin_end(off) } else { p.set_margin_start(off) }
        }
    };
    place(CARD_STEP);
    let step = Rc::new(Cell::new(CARD_STEP));
    let hover = gtk::EventControllerMotion::new();
    let spread = move |o: &gtk::Overlay, to: f64| {
        let (step, place) = (step.clone(), place.clone());
        let from = step.get();
        let began = Cell::new(None::<i64>);
        o.add_tick_callback(move |_, clock| {
            let t0 = *began.get().get_or_insert(clock.frame_time());
            began.set(Some(t0));
            let p = ((clock.frame_time() - t0) as f64 / 180_000.0).min(1.0);
            let e = 1.0 - (1.0 - p).powi(3);
            step.set(from + (to - from) * e);
            place(step.get());
            if p >= 1.0 { glib::ControlFlow::Break } else { glib::ControlFlow::Continue }
        });
    };
    let (sp, o2) = (spread.clone(), o.clone());
    hover.connect_enter(move |_, _, _| sp(&o2, CARD_SPREAD));
    let o2 = o.clone();
    hover.connect_leave(move |_| spread(&o2, CARD_STEP));
    o.add_controller(hover);
    if paths.len() > shown {
        let more = gtk::Label::new(Some(&format!("+{}", paths.len() - shown)));
        more.add_css_class("stack-count");
        more.set_halign(side);
        more.set_valign(gtk::Align::End);
        let inset = 10 + bubble::TAIL as i32;
        if me { more.set_margin_end(inset) } else { more.set_margin_start(inset) }
        more.set_margin_bottom(10);
        o.add_overlay(&more);
    }
    o.upcast()
}

/// A photo card. `card` = fixed square size (stacks); otherwise it takes the photo's own shape.
/// Clicking opens the viewer.
fn image(u: &Rc<Ui>, path: &str, card: Option<i32>, look: bubble::Look) -> bubble::Bubble {
    let (w, h) = card.map(|c| (c, c)).unwrap_or((240, 180));
    let p = bubble::Bubble::photo(look, w, h);
    if card.is_some() {
        p.add_css_class("stacked");
    }
    p.set_cursor_from_name(Some("pointer"));
    let click = gtk::GestureClick::new();
    let (u2, p2) = (u.clone(), p.clone());
    click.connect_released(move |g, _, _, _| {
        g.set_state(gtk::EventSequenceState::Claimed);
        let items: Vec<bubble::Bubble> = u2.st.borrow().photos.iter().filter(|b| b.texture().is_some()).cloned().collect();
        u2.viewer.open(items, &p2);
    });
    p.add_controller(click);
    u.st.borrow_mut().photos.push(p.clone());

    let in_memory = u.st.borrow().textures.get(path).cloned(); // borrow ends here; or_else below borrows mutably
    let cached = in_memory.or_else(|| {
        let tex = texture(&std::fs::read(cache_path(u, path)).ok()?)?;
        u.st.borrow_mut().textures.insert(path.to_string(), tex.clone());
        Some(tex)
    });
    match cached {
        Some(t) => show_texture(&p, &t),
        None => {
            let first = {
                let mut st = u.st.borrow_mut();
                let w = st.waiting.entry(path.to_string()).or_default();
                w.push(p.clone());
                w.len() == 1
            };
            if first {
                u.send(Cmd::Image { path: path.to_string() });
            }
        }
    }
    p
}

fn texture(bytes: &[u8]) -> Option<gdk::Texture> {
    // the Mac already turned the photo upright (see mac::fetch_image)
    gdk::Texture::from_bytes(&glib::Bytes::from(bytes)).ok()
}

fn show_texture(p: &bubble::Bubble, t: &gdk::Texture) {
    if !p.has_css_class("stacked") {
        // a single photo takes its own shape; stacked cards stay square and crop to fill
        let (w, h) = (t.width().max(1) as f64, t.height().max(1) as f64);
        let s = (300.0 / w).min(340.0 / h).min(1.0);
        p.set_photo_size((w * s) as i32, (h * s) as i32);
    }
    p.set_texture(Some(t));
}

fn cache_path(u: &Ui, path: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    u.store.image_cache.join(format!("{:016x}.jpg", h.finish()))
}

// ---------- settings ----------

fn settings(u: &Rc<Ui>) {
    let w = gtk::Window::builder().transient_for(&u.win).modal(true).title("Settings").default_width(560).build();
    w.add_css_class("bf-settings");
    let grid = gtk::Grid::builder().row_spacing(10).column_spacing(12).margin_top(18).margin_bottom(18).margin_start(18).margin_end(18).build();
    let cfg = u.st.borrow().cfg.clone();
    let field = |row: i32, label: &str, value: &str, hint: &str| {
        let l = gtk::Label::new(Some(label));
        l.set_xalign(1.0);
        let e = gtk::Entry::builder().text(value).placeholder_text(hint).hexpand(true).build();
        grid.attach(&l, 0, row, 1, 1);
        grid.attach(&e, 1, row, 1, 1);
        e
    };
    let intro = gtk::Label::new(Some("BubbleFairy connects to your Mac over SSH (Remote Login) and uses imsg there to read and send messages."));
    intro.set_wrap(true);
    intro.set_xalign(0.0);
    intro.add_css_class("dim");
    grid.attach(&intro, 0, 0, 2, 1);
    let host = field(1, "Mac", &cfg.host, "e.g. my-mac.local");
    let user = field(2, "Mac user", &cfg.user, "your short user name on the Mac");
    let imsg = field(3, "imsg path", &cfg.imsg, "/opt/homebrew/bin/imsg");

    let key_title = gtk::Label::new(Some("BubbleFairy's key"));
    key_title.set_xalign(1.0);
    key_title.set_valign(gtk::Align::Start);
    let key = gtk::Label::new(Some(&u.store.public_key));
    key.set_wrap(true);
    key.set_wrap_mode(gtk::pango::WrapMode::Char);
    key.set_selectable(true);
    key.set_xalign(0.0);
    key.add_css_class("monospace");
    let how = gtk::Label::new(Some(
        "Let your Mac accept this key: copy the command below, open Terminal on the Mac, paste it and press Return.",
    ));
    how.set_wrap(true);
    how.set_xalign(0.0);
    how.add_css_class("dim");
    let copy = gtk::Button::with_label("Copy command for the Mac");
    copy.set_halign(gtk::Align::Start);
    let line = format!(
        "mkdir -p ~/.ssh && chmod 700 ~/.ssh && echo {} >> ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys",
        crate::mac::sh_quote(&u.store.public_key)
    );
    let win = u.win.clone();
    copy.connect_clicked(move |b| {
        win.clipboard().set_text(&line);
        b.set_label("Copied ✓");
    });
    let kbox = gtk::Box::new(gtk::Orientation::Vertical, 8);
    kbox.append(&key);
    kbox.append(&how);
    kbox.append(&copy);
    grid.attach(&key_title, 0, 4, 1, 1);
    grid.attach(&kbox, 1, 4, 1, 1);

    let hk = u.st.borrow().host_key.clone();
    let hk = if hk.is_empty() { std::fs::read_to_string(&u.store.known_host).unwrap_or_default() } else { hk };
    if !hk.trim().is_empty() {
        let l = gtk::Label::new(Some("Mac identity"));
        l.set_xalign(1.0);
        let fp = gtk::Label::new(Some(&crate::mac::fingerprint(hk.trim())));
        fp.set_selectable(true);
        fp.set_xalign(0.0);
        fp.add_css_class("monospace");
        fp.set_tooltip_text(Some("Saved on first connection. BubbleFairy refuses to connect if it changes."));
        let forget = gtk::Button::with_label("Forget");
        forget.set_tooltip_text(Some("Only if you replaced or reinstalled the Mac"));
        let kh = u.store.known_host.clone();
        forget.connect_clicked(move |b| {
            let _ = std::fs::remove_file(&kh);
            b.set_label("Forgotten");
            b.set_sensitive(false);
        });
        let hb = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        hb.append(&fp);
        hb.append(&forget);
        grid.attach(&l, 0, 5, 1, 1);
        grid.attach(&hb, 1, 5, 1, 1);
    }

    let save = gtk::Button::with_label("Save and connect");
    save.add_css_class("suggested-action");
    let cancel = gtk::Button::with_label("Cancel");
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.append(&cancel);
    buttons.append(&save);
    grid.attach(&buttons, 0, 6, 2, 1);
    w.set_child(Some(&grid));

    let w2 = w.clone();
    cancel.connect_clicked(move |_| w2.close());
    let (u2, w2) = (u.clone(), w.clone());
    save.connect_clicked(move |_| {
        let mut imsg_path = imsg.text().trim().to_string();
        if imsg_path.is_empty() {
            imsg_path = crate::DEFAULT_IMSG.into();
        }
        let cfg = Config { host: host.text().trim().to_string(), user: user.text().trim().to_string(), imsg: imsg_path };
        crate::save_config(&u2.store.config_file, &cfg);
        u2.st.borrow_mut().cfg = cfg.clone();
        u2.send(Cmd::Configure(cfg));
        w2.close();
    });
    w.present();
}

// ---------- helpers ----------

/// Some RCS attachments come without a MIME type, so fall back to the file extension.
fn is_image(mime: &str, path: &str) -> bool {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    mime.starts_with("image/") || (mime.is_empty() && matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "heic" | "heif" | "gif" | "webp"))
}

fn chat_title(c: &Value, names: &HashMap<String, String>) -> String {
    for k in ["display_name", "contact_name"] {
        if let Some(s) = c[k].as_str().filter(|s| !s.is_empty()) {
            return s.to_string();
        }
    }
    let people: Vec<String> = c["participants"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p.as_str())
        .map(|p| names.get(p).cloned().unwrap_or_else(|| p.to_string()))
        .collect();
    if !people.is_empty() {
        return people.join(", ");
    }
    c["identifier"].as_str().unwrap_or("?").to_string()
}

fn sender_name(m: &Value, names: &HashMap<String, String>) -> String {
    if let Some(n) = m["sender_name"].as_str().filter(|s| !s.is_empty()) {
        return n.to_string();
    }
    let h = m["sender"].as_str().unwrap_or("?");
    names.get(h).cloned().unwrap_or_else(|| h.to_string())
}

fn clean_text(m: &Value) -> String {
    m["text"].as_str().unwrap_or("").replace(OBJ, "").trim().to_string()
}

/// One-line description of a message, for the chat list and notifications.
fn summary(m: &Value) -> String {
    let t = clean_text(m);
    if !t.is_empty() {
        return t.lines().next().unwrap_or("").to_string();
    }
    let atts = m["attachments"].as_array().map(|a| a.len()).unwrap_or(0);
    let image = m["attachments"][0]["mime_type"].as_str().is_some_and(|s| s.starts_with("image/"));
    match (atts, image) {
        (0, _) if m["unsent_at"].as_str().is_some() => "Message unsent".into(),
        (0, _) => String::new(),
        (1, true) => "📷 Photo".into(),
        (n, true) => format!("📷 {n} photos"),
        _ => "📎 Attachment".into(),
    }
}

pub fn initials(name: &str) -> String {
    let first = name.chars().next().unwrap_or('?');
    if !first.is_alphabetic() {
        return "#".into();
    }
    name.split_whitespace().filter_map(|w| w.chars().find(|c| c.is_alphanumeric())).take(2).flat_map(char::to_uppercase).collect()
}

fn avatar(name: &str, small: bool) -> gtk::Label {
    let b = gtk::Label::new(Some(&initials(name)));
    b.set_halign(gtk::Align::Center);
    b.add_css_class("avatar");
    let hue = name.bytes().fold(0u32, |a, c| a.wrapping_mul(31).wrapping_add(c as u32)) % 8;
    b.add_css_class(&format!("av{hue}"));
    if small {
        b.add_css_class("small");
    }
    b.set_valign(gtk::Align::Center);
    b
}

fn local(iso: &str) -> Option<glib::DateTime> {
    glib::DateTime::from_iso8601(iso, None).ok()?.to_local().ok()
}

fn day(unix: Option<i64>) -> Option<(i32, i32, i32)> {
    glib::DateTime::from_unix_local(unix?).ok().map(|d| d.ymd())
}

fn short_time(d: &glib::DateTime) -> String {
    let today = glib::DateTime::now_local().map(|n| n.ymd()).ok();
    let fmt = if today == Some(d.ymd()) { "%H:%M" } else { "%b %-d, %Y" };
    d.format(fmt).map(|s| s.to_string()).unwrap_or_default()
}

fn sep_time(d: &glib::DateTime) -> String {
    let today = glib::DateTime::now_local().map(|n| n.ymd()).ok();
    let fmt = if today == Some(d.ymd()) { "Today  %H:%M" } else { "%b %-d, %Y  %H:%M" };
    d.format(fmt).map(|s| s.to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn titles_and_initials() {
        let names = HashMap::from([("+1".to_string(), "Ann Lee".to_string())]);
        assert_eq!(chat_title(&json!({"display_name": "", "participants": ["+1", "+2"]}), &names), "Ann Lee, +2");
        assert_eq!(chat_title(&json!({"display_name": "", "contact_name": "Rob"}), &names), "Rob");
        assert_eq!(initials("Ann Lee"), "AL");
        assert_eq!(initials("+1 555"), "#");
    }

    #[test]
    fn summaries() {
        assert_eq!(summary(&json!({"text": "\u{FFFC}", "attachments": [{"mime_type": "image/heic"}]})), "📷 Photo");
        assert_eq!(summary(&json!({"text": "hi\nthere", "attachments": []})), "hi");
    }
}

