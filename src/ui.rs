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
const COMPOSE_MAX: i32 = 160; // composer stops growing here and scrolls
const COMPOSE_PAD_Y: i32 = 9;
const OBJ: char = '\u{FFFC}'; // what imsg puts in `text` for attachment-only messages

pub struct Store {
    pub config_file: PathBuf,
    pub known_host: PathBuf,
    pub image_cache: PathBuf,
    /// newest message seen in BubbleFairy per chat (`chat_id=message_id` lines)
    pub seen_file: PathBuf,
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
    photos: Vec<(String, bubble::Bubble)>,
    /// full-size viewer fetches in flight
    waiting_full: HashSet<String>,
    host_key: String,
    connected: bool,
    chats_loaded: bool,
    /// chats with a history request in flight (header spinner)
    loading: HashSet<i64>,
    older_loading: bool,
    /// link previews by message row: None = asked for, or the Mac had none
    links: HashMap<i64, Option<crate::mac::LinkPreview>>,
    /// files waiting in the composer
    outgoing: Vec<PathBuf>,
    /// chat id -> newest message id seen in BubbleFairy; hides the unread dot the Mac can't clear
    seen: HashMap<i64, i64>,
    /// address a new conversation's first message went to; its chat opens once it shows up
    new_pending: Option<String>,
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
    attach_bar: gtk::Box,
    side_stack: gtk::Stack,
    head_spin: gtk::Spinner,
    head_titles: gtk::Box,
    to_row: gtk::Box,
    to_entry: gtk::Entry,
    suggest: gtk::ListBox,
    suggest_scroll: gtk::ScrolledWindow,
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
    /// open chat is a group (them-side rows carry an avatar column)
    group: Cell<bool>,
    /// re-lay photo stacks when the chat width changes (rebuilt with the conversation)
    stacks: RefCell<Vec<Rc<dyn Fn()>>>,
    render_queued: Cell<bool>,
    /// writing a new message (To field showing, no chat selected)
    new_mode: Cell<bool>,
}

impl Ui {
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
    side_title.set_hexpand(true);
    let new_btn = gtk::Button::from_icon_name("document-edit-symbolic");
    new_btn.set_tooltip_text(Some("New message (Ctrl+N)"));
    new_btn.add_css_class("head-btn");
    new_btn.add_css_class("new-btn");
    new_btn.set_valign(gtk::Align::Center);
    new_btn.set_action_name(Some("app.new-message"));
    let side_head = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    side_head.append(&side_title);
    side_head.append(&new_btn);
    sidebar.append(&side_head);
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
    // new message: the header becomes a To field, the message area a list of people to pick
    let to_label = gtk::Label::new(Some("To:"));
    to_label.add_css_class("to-label");
    let to_entry = gtk::Entry::builder().placeholder_text("Name, phone number or email").hexpand(true).build();
    to_entry.add_css_class("to-entry");
    let to_row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    to_row.set_hexpand(true);
    to_row.append(&to_label);
    to_row.append(&to_entry);
    to_row.set_visible(false);
    conv_head.append(&to_row);
    let suggest = gtk::ListBox::new();
    suggest.add_css_class("chat-list");
    suggest.add_css_class("suggest");
    let suggest_scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&suggest).build();
    suggest_scroll.set_visible(false);

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
    let compose_scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&compose).hexpand(true).build();
    // The field's height is worked out here from the text itself (Pango, at the field's width) rather
    // than taken from the TextView, whose reported height runs a line tall while new lines are still
    // being laid out. Past COMPOSE_MAX it stops growing and scrolls.
    let fits = Rc::new(Cell::new(true)); // all the text fits without scrolling
    let (cs, cv, fits2) = (compose_scroll.clone(), compose.clone(), fits.clone());
    let fit = Rc::new(move || {
        let w = cv.width() - cv.left_margin() - cv.right_margin();
        if w <= 0 {
            return;
        }
        let buf = cv.buffer();
        let layout = cv.create_pango_layout(Some(&buf.text(&buf.start_iter(), &buf.end_iter(), false)));
        layout.set_width(w * gtk::pango::SCALE);
        layout.set_wrap(gtk::pango::WrapMode::WordChar);
        let need = layout.pixel_size().1 + cv.top_margin() + cv.bottom_margin();
        let shown = need.min(COMPOSE_MAX);
        fits2.set(need <= COMPOSE_MAX);
        if cs.min_content_height() != shown {
            cs.set_min_content_height(-1);
            cs.set_max_content_height(shown);
            cs.set_min_content_height(shown);
        }
        // External: no scrollbar (its own minimum height is taller than one line) until it's needed
        cs.set_vscrollbar_policy(if need > COMPOSE_MAX { gtk::PolicyType::Automatic } else { gtk::PolicyType::External });
        if need > COMPOSE_MAX {
            let (cv, cs) = (cv.clone(), cs.clone());
            glib::idle_add_local_once(move || {
                // after the new size applies; at the end, go all the way down so the bottom padding shows
                let buf = cv.buffer();
                if buf.iter_at_mark(&buf.get_insert()).is_end() {
                    let a = cs.vadjustment();
                    a.set_value(a.upper() - a.page_size());
                } else {
                    cv.scroll_mark_onscreen(&buf.get_insert());
                }
            });
        }
    });
    let f = fit.clone();
    compose.buffer().connect_changed(move |_| f());
    // A key press that adds a line makes the TextView scroll to the cursor while the field is still
    // the old height; once it grows, that scroll would linger and push the text up. While everything
    // fits, stay at the top.
    compose_scroll.vadjustment().connect_value_changed(move |a| {
        if fits.get() && a.value() != 0.0 {
            a.set_value(0.0);
        }
    });
    let f = fit.clone();
    compose_scroll.hadjustment().connect_page_size_notify(move |_| f()); // width changed: rewrap
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
    send_btn.set_tooltip_text(Some("Send (Enter)"));
    send_btn.set_valign(gtk::Align::End);
    let composer = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    composer.add_css_class("composer");
    let attach_btn = gtk::Button::from_icon_name("mail-attachment-symbolic");
    attach_btn.add_css_class("head-btn");
    attach_btn.add_css_class("attach-btn");
    attach_btn.set_tooltip_text(Some("Attach files (or drop or paste them)"));
    attach_btn.set_valign(gtk::Align::End);
    composer.append(&attach_btn);
    composer.append(&compose_box);
    composer.append(&emoji);
    composer.append(&send_btn);

    let conv = gtk::Box::new(gtk::Orientation::Vertical, 0);
    conv.append(&conv_head);
    conv.append(&suggest_scroll);
    conv.append(&scroller);
    // files waiting to go, above the composer
    let attach_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    attach_bar.add_css_class("attach-bar");
    attach_bar.set_visible(false);
    conv.append(&attach_bar);
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

    let seen = load_seen(&store.seen_file);
    let ui = Rc::new(Ui {
        app: app.clone(),
        win: win.clone(),
        store,
        cmd,
        viewer,
        st: RefCell::new(State { cfg, seen, ..Default::default() }),
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
        attach_bar,
        side_stack,
        head_spin,
        head_titles: titles,
        to_row,
        to_entry,
        suggest,
        suggest_scroll,
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
        group: Cell::new(false),
        stacks: RefCell::default(),
        render_queued: Cell::new(false),
        new_mode: Cell::new(false),
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

    let u = ui.clone();
    ui.compose.buffer().connect_changed(move |_| update_send(&u));
    let u = ui.clone();
    attach_btn.connect_clicked(move |_| pick_files(&u));
    let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
    let u = ui.clone();
    drop.connect_drop(move |_, v, _, _| {
        let Ok(list) = v.get::<gdk::FileList>() else { return false };
        add_files(&u, list.files().iter().filter_map(|f| f.path()).collect());
        true
    });
    conv.add_controller(drop);
    let u = ui.clone();
    ui.compose.connect_paste_clipboard(move |tv| {
        // pasted files or images become attachments; text pastes as usual
        let cb = tv.clipboard();
        let f = cb.formats();
        if f.contains_type(gdk::FileList::static_type()) {
            tv.stop_signal_emission_by_name("paste-clipboard");
            let u = u.clone();
            glib::spawn_future_local(async move {
                if let Ok(v) = cb.read_value_future(gdk::FileList::static_type(), glib::Priority::DEFAULT).await
                    && let Ok(list) = v.get::<gdk::FileList>()
                {
                    add_files(&u, list.files().iter().filter_map(|f| f.path()).collect());
                }
            });
        } else if f.contains_type(gdk::Texture::static_type()) && !f.contain_mime_type("text/plain") {
            tv.stop_signal_emission_by_name("paste-clipboard");
            let u = u.clone();
            glib::spawn_future_local(async move {
                let Ok(Some(t)) = cb.read_texture_future().await else { return };
                let dir = outgoing_dir(&u);
                let _ = std::fs::create_dir_all(&dir);
                let path = dir.join(format!("Pasted image {}.png", glib::DateTime::now_local().map(|d| d.format("%Y-%m-%d %H.%M.%S").unwrap_or_default()).unwrap_or_default()));
                if t.save_to_png(&path).is_ok() {
                    add_files(&u, vec![path]);
                }
            });
        }
    });

    let new_msg = gio::SimpleAction::new("new-message", None);
    let u = ui.clone();
    new_msg.connect_activate(move |_, _| start_new(&u));
    app.add_action(&new_msg);
    app.set_accels_for_action("app.new-message", &["<Control>n"]);
    let u = ui.clone();
    ui.to_entry.connect_changed(move |_| render_suggest(&u));
    let u = ui.clone();
    ui.to_entry.connect_activate(move |e| {
        // Enter: open the top suggestion, or move on to the message if it's a new address
        match ui_first_suggestion(&u) {
            Some(id) if !e.text().trim().is_empty() => open_chat(&u, id),
            _ if is_addr(&e.text()) => {
                u.compose.grab_focus();
            }
            _ => {}
        }
    });
    let u = ui.clone();
    ui.suggest.connect_row_activated(move |_, row| {
        if let Ok(id) = row.widget_name().parse::<i64>() {
            open_chat(&u, id);
        }
    });
    let esc = gtk::EventControllerKey::new();
    let u = ui.clone();
    esc.connect_key_pressed(move |_, key, _, _| {
        if key == gdk::Key::Escape && u.new_mode.get() {
            end_new(&u);
            u.stack.set_visible_child_name("empty");
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    ui.to_entry.add_controller(esc);

    // coming back to the window counts as seeing the open chat
    let u = ui.clone();
    ui.win.connect_is_active_notify(move |_| mark_seen(&u));
    // read state changes (chats read on other devices) don't come through the live stream
    let w = Rc::downgrade(&ui);
    glib::timeout_add_seconds_local(60, move || {
        let Some(u) = w.upgrade() else { return glib::ControlFlow::Break };
        if u.st.borrow().connected {
            u.send(Cmd::Chats);
        }
        glib::ControlFlow::Continue
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

    // viewer keys: at window level so they work whatever has focus
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let u = ui.clone();
    keys.connect_key_pressed(move |_, key, _, _| {
        if u.viewer.is_open() && u.viewer.key(key) { glib::Propagation::Stop } else { glib::Propagation::Proceed }
    });
    win.add_controller(keys);
    let w = Rc::downgrade(&ui);
    ui.viewer.set_hooks(viewer::Hooks { fetch_full: Box::new(move |p| if let Some(u) = w.upgrade() { fetch_full(&u, p) }) });
    let u = ui.clone();
    ui.scroller.hadjustment().connect_page_size_notify(move |_| {
        let fs: Vec<_> = u.stacks.borrow().clone();
        for f in fs {
            f();
        }
    });

    win.present();
    if let Ok(t) = std::env::var("BF_TYPE") {
        // dev aid: type this into the composer one character every 25 ms (`\\n` = Shift+Enter)
        let chars: Vec<char> = t.replace("\\n", "\n").chars().collect();
        let (i, c) = (Cell::new(0), ui.compose.clone());
        glib::timeout_add_local(Duration::from_millis(25), move || {
            let Some(ch) = chars.get(i.get()) else { return glib::ControlFlow::Break };
            c.buffer().insert_at_cursor(&ch.to_string());
            i.set(i.get() + 1);
            glib::ControlFlow::Continue
        });
    }
    if let Ok(t) = std::env::var("BF_DRAFT") {
        ui.compose.buffer().set_text(&t.replace("\\n", "\n")); // dev aid: prefill the composer
    }
    if let Some(n) = std::env::var("BF_TOP").ok().and_then(|v| v.parse::<u32>().ok()) {
        // dev aid: scroll to the top n times, 1.5 s apart (each loads an older page)
        let (u, left) = (ui.clone(), Cell::new(n));
        glib::timeout_add_local(Duration::from_millis(1500), move || {
            u.stick_bottom.set(false);
            u.scroller.vadjustment().set_value(0.0);
            left.set(left.get().saturating_sub(1));
            if left.get() == 0 { glib::ControlFlow::Break } else { glib::ControlFlow::Continue }
        });
    }
    if let Some(ms) = std::env::var("BF_LINK").ok().and_then(|v| v.parse::<u64>().ok()) {
        // dev aid: every `ms`, scroll the open chat to its first link card with a picture
        let u = ui.clone();
        glib::timeout_add_local(Duration::from_millis(ms), move || {
            fn find(w: &gtk::Widget) -> Option<gtk::Widget> {
                if w.has_css_class("linkcard") && w.has_css_class("fixed") {
                    return Some(w.clone());
                }
                let mut c = w.first_child();
                while let Some(x) = c {
                    if let Some(f) = find(&x) {
                        return Some(f);
                    }
                    c = x.next_sibling();
                }
                None
            }
            if let Some(card) = find(u.msgs.upcast_ref())
                && let Some(p) = card.compute_point(&u.msgs, &gtk::graphene::Point::new(0.0, 0.0))
            {
                u.stick_bottom.set(false);
                u.scroller.vadjustment().set_value(p.y() as f64 - 200.0);
            }
            glib::ControlFlow::Continue
        });
    }
    if let Ok(t) = std::env::var("BF_ATTACH") {
        // dev aid: put these files (colon-separated) in the composer
        let u = ui.clone();
        glib::timeout_add_local_once(Duration::from_millis(1500), move || add_files(&u, t.split(':').map(PathBuf::from).collect()));
    }
    if let Ok(t) = std::env::var("BF_NEW") {
        // dev aid: open the new-message view with this typed in the To field
        let u = ui.clone();
        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
            start_new(&u);
            u.to_entry.set_text(&t);
        });
    }
    if std::env::var_os("BF_VIEW").is_some() {
        // dev aid: open the viewer on the newest loaded photo shortly before BF_SHOT fires
        // BF_VIEW=<ms> sets when it opens; otherwise 0.9 s before the shot
        let ms = std::env::var("BF_SHOT_MS").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(2500);
        let at = std::env::var("BF_VIEW").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(ms.saturating_sub(900));
        let u = ui.clone();
        glib::timeout_add_local_once(Duration::from_millis(at), move || {
            let items = u.st.borrow().photos.clone();
            if let Some((last, _)) = items.iter().rev().find(|(_, b)| b.texture().is_some()).cloned() {
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
                if st.sel.is_none() && !u.new_mode.get() {
                    want.or_else(|| st.chats.first().and_then(|c| c["id"].as_i64()))
                } else {
                    None
                }
            };
            // a new conversation's chat has appeared: open it
            let new_chat = {
                let st = u.st.borrow();
                st.new_pending.as_deref().and_then(|a| find_direct_chat(&st.chats, a))
            };
            if let Some(id) = new_chat {
                u.st.borrow_mut().new_pending = None;
                open_chat(u, id);
            }
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
        Event::Sent { text, files, err } => {
            if let Some(err) = err {
                u.st.borrow_mut().new_pending = None;
                show_banner(u, &format!("Message not sent: {err}"), false);
                let buf = u.compose.buffer();
                if buf.char_count() == 0 {
                    buf.set_text(&text); // nothing typed is lost
                }
                add_files(u, files); // nor any file that didn't go
            } else if !u.banner_offline.get() {
                u.banner.set_reveal_child(false);
            }
        }
        Event::Link(row, p) => {
            u.st.borrow_mut().links.insert(row, p.clone());
            let shown = {
                let st = u.st.borrow();
                p.is_some() && st.sel.and_then(|id| st.hist.get(&id)).is_some_and(|h| h.iter().any(|m| preview_row(m) == Some(row)))
            };
            // previews arrive in bursts when a chat opens: one re-render for the lot
            if shown && !u.render_queued.replace(true) {
                let u2 = u.clone();
                glib::idle_add_local_once(move || {
                    u2.render_queued.set(false);
                    render_conv(&u2);
                });
            }
        }
        Event::Image { path, full, bytes } => {
            let tex = bytes.and_then(|b| {
                let _ = std::fs::create_dir_all(&u.store.image_cache);
                let _ = std::fs::write(cache_path(u, &path, full), &b);
                texture(&b)
            });
            if full {
                u.st.borrow_mut().waiting_full.remove(&path);
                match tex {
                    Some(t) => u.viewer.full_ready(&path, t),
                    None => u.viewer.full_failed(&path),
                }
                return;
            }
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
    end_new(u);
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

fn send_now(u: &Rc<Ui>) {
    let buf = u.compose.buffer();
    let text = buf.text(&buf.start_iter(), &buf.end_iter(), false).trim().to_string();
    if text.is_empty() && u.st.borrow().outgoing.is_empty() {
        return;
    }
    if u.new_mode.get() {
        send_new(u, &text);
        return;
    }
    let Some(chat_id) = u.st.borrow().sel else { return };
    let to = {
        let st = u.st.borrow();
        st.chats
            .iter()
            .find(|c| c["id"].as_i64() == Some(chat_id))
            .filter(|c| c["is_group"].as_bool() != Some(true))
            .and_then(|c| direct_addr(c).map(String::from))
    };
    let files = take_files(u);
    buf.set_text(""); // also disables the send button (buffer changed)
    u.stick_bottom.set(true);
    u.send(Cmd::Send { chat_id, to, text, files }); // the messages come back through the live stream
}

// ---------- attachments ----------

fn update_send(u: &Ui) {
    let has = u.compose.buffer().char_count() > 0 || !u.st.borrow().outgoing.is_empty();
    u.send_btn.set_sensitive(has);
}

fn outgoing_dir(u: &Ui) -> PathBuf {
    u.store.image_cache.with_file_name("outgoing")
}

fn pick_files(u: &Rc<Ui>) {
    let d = gtk::FileDialog::builder().title("Attach files").modal(true).build();
    let u = u.clone();
    glib::spawn_future_local(async move {
        if let Ok(list) = d.open_multiple_future(Some(&u.win)).await {
            let files = (0..list.n_items()).filter_map(|i| list.item(i).and_downcast::<gio::File>()?.path()).collect();
            add_files(&u, files);
        }
    });
}

fn add_files(u: &Rc<Ui>, files: Vec<PathBuf>) {
    if files.is_empty() {
        return;
    }
    {
        let mut st = u.st.borrow_mut();
        for f in files {
            if f.is_file() && !st.outgoing.contains(&f) {
                st.outgoing.push(f);
            }
        }
    }
    render_attach(u);
    u.compose.grab_focus();
}

fn take_files(u: &Rc<Ui>) -> Vec<PathBuf> {
    let files = std::mem::take(&mut u.st.borrow_mut().outgoing);
    render_attach(u);
    files
}

/// The waiting files as removable cards: a thumbnail for images, else an icon and the name.
fn render_attach(u: &Rc<Ui>) {
    while let Some(c) = u.attach_bar.first_child() {
        u.attach_bar.remove(&c);
    }
    let files = u.st.borrow().outgoing.clone();
    u.attach_bar.set_visible(!files.is_empty());
    for f in files {
        let name = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let card = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        card.add_css_class("attach-chip");
        card.set_tooltip_text(Some(&name));
        if let Some(t) = is_image("", &name).then(|| gdk::Texture::from_filename(&f).ok()).flatten() {
            // ponytail: whole image decoded for a 64 px thumbnail; fine for a few files
            let pic = gtk::Image::from_paintable(Some(&t));
            pic.set_pixel_size(64);
            pic.add_css_class("attach-thumb");
            card.append(&pic);
        } else {
            let icon = gtk::Image::from_icon_name("text-x-generic-symbolic");
            icon.set_pixel_size(24);
            card.append(&icon);
            let l = gtk::Label::new(Some(&name));
            l.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            l.set_max_width_chars(18);
            card.append(&l);
        }
        let x = gtk::Button::from_icon_name("window-close-symbolic");
        x.add_css_class("attach-remove");
        x.set_tooltip_text(Some("Remove"));
        x.set_halign(gtk::Align::End);
        x.set_valign(gtk::Align::Start);
        let o = gtk::Overlay::new();
        o.set_child(Some(&card));
        o.add_overlay(&x);
        let (u2, f2) = (u.clone(), f.clone());
        x.connect_clicked(move |_| {
            u2.st.borrow_mut().outgoing.retain(|p| *p != f2);
            render_attach(&u2);
        });
        u.attach_bar.append(&o);
    }
    update_send(u);
}

// ---------- new conversation ----------

fn start_new(u: &Rc<Ui>) {
    u.viewer.close_now();
    u.st.borrow_mut().sel = None;
    u.selecting.set(true);
    u.chat_list.unselect_all();
    u.selecting.set(false);
    u.new_mode.set(true);
    for w in [u.head_avatar.upcast_ref::<gtk::Widget>(), u.head_titles.upcast_ref(), u.head_spin.upcast_ref(), u.scroller.upcast_ref()] {
        w.set_visible(false);
    }
    u.to_row.set_visible(true);
    u.suggest_scroll.set_visible(true);
    u.to_entry.set_text("");
    u.compose_hint.set_text("Message");
    u.send_btn.remove_css_class("sms");
    u.send_btn.add_css_class("imessage");
    u.stack.set_visible_child_name("conv");
    render_suggest(u);
    u.to_entry.grab_focus();
}

fn end_new(u: &Ui) {
    if !u.new_mode.replace(false) {
        return;
    }
    u.head_avatar.set_visible(true);
    u.head_titles.set_visible(true);
    u.scroller.set_visible(true);
    u.to_row.set_visible(false);
    u.suggest_scroll.set_visible(false);
    update_spinner(u);
}

fn open_chat(u: &Rc<Ui>, id: i64) {
    select_in_list(u, id);
    select(u, id);
}

/// 1:1 chats matching what's typed in the To field: (chat id, name, address).
fn suggestions(u: &Ui) -> Vec<(i64, String, String)> {
    let q = u.to_entry.text().trim().to_lowercase();
    let st = u.st.borrow();
    st.chats
        .iter()
        .filter(|c| c["is_group"].as_bool() != Some(true))
        .filter_map(|c| Some((c["id"].as_i64()?, chat_title(c, &st.names), direct_addr(c)?.to_string())))
        .filter(|(_, name, addr)| q.is_empty() || name.to_lowercase().contains(&q) || addr.to_lowercase().contains(&q) || same_addr(&q, addr))
        .take(8)
        .collect()
}

fn ui_first_suggestion(u: &Ui) -> Option<i64> {
    suggestions(u).first().map(|s| s.0)
}

fn render_suggest(u: &Ui) {
    while let Some(c) = u.suggest.first_child() {
        u.suggest.remove(&c);
    }
    let typed = u.to_entry.text().trim().to_string();
    let hint = if is_addr(&typed) {
        format!("New conversation with {typed}. Write your message below.")
    } else {
        "No matching conversations. Type a phone number or email to start a new one.".into()
    };
    let none = gtk::Label::new(Some(&hint));
    none.add_css_class("dim");
    none.set_wrap(true);
    none.set_margin_top(24);
    u.suggest.set_placeholder(Some(&none));
    for (id, name, addr) in suggestions(u) {
        let row = gtk::ListBoxRow::new();
        row.add_css_class("chat-row");
        row.set_widget_name(&id.to_string());
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        line.append(&avatar(&name, false));
        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_valign(gtk::Align::Center);
        let n = gtk::Label::new(Some(&name));
        n.add_css_class("chat-name");
        n.set_xalign(0.0);
        let a = gtk::Label::new(Some(&addr));
        a.add_css_class("chat-preview");
        a.set_xalign(0.0);
        text.append(&n);
        if addr != name {
            text.append(&a);
        }
        line.append(&text);
        row.set_child(Some(&line));
        u.suggest.append(&row);
    }
}

/// Send the first message of a new conversation (or to an existing one, if the To field names it).
fn send_new(u: &Rc<Ui>, text: &str) {
    let typed = u.to_entry.text().trim().to_string();
    let existing = {
        let st = u.st.borrow();
        find_direct_chat(&st.chats, &typed).or_else(|| {
            st.chats
                .iter()
                .filter(|c| c["is_group"].as_bool() != Some(true))
                .find(|c| chat_title(c, &st.names).eq_ignore_ascii_case(&typed))
                .and_then(|c| c["id"].as_i64())
        })
    };
    if let Some(id) = existing {
        open_chat(u, id);
        send_now(u);
        return;
    }
    if !is_addr(&typed) {
        show_banner(u, "Who's it to? Pick a conversation or type a phone number or email.", false);
        u.to_entry.grab_focus();
        return;
    }
    let to = clean_addr(&typed);
    let files = take_files(u);
    u.compose.buffer().set_text("");
    u.st.borrow_mut().new_pending = Some(to.clone());
    u.send(Cmd::Send { chat_id: 0, to: Some(to), text: text.to_string(), files });
    // the chat opens when the Mac's chat list includes it (Event::Chats)
}

/// The 1:1 chat with this address, if there is one.
fn find_direct_chat(chats: &[Value], addr: &str) -> Option<i64> {
    chats
        .iter()
        .filter(|c| c["is_group"].as_bool() != Some(true))
        .find(|c| direct_addr(c).is_some_and(|i| same_addr(addr, i)))
        .and_then(|c| c["id"].as_i64())
}

/// The other person's address in a 1:1 chat.
fn direct_addr(c: &Value) -> Option<&str> {
    c["identifier"].as_str().or(c["participants"][0].as_str())
}

/// A phone number or email someone could be messaged at.
fn is_addr(s: &str) -> bool {
    let s = s.trim();
    if let Some((user, host)) = s.split_once('@') {
        return !user.is_empty() && host.contains('.') && !s.contains(' ');
    }
    let digits = s.chars().filter(char::is_ascii_digit).count();
    digits >= 5 && s.chars().all(|c| c.is_ascii_digit() || " +-().".contains(c))
}

fn clean_addr(s: &str) -> String {
    let s = s.trim();
    if s.contains('@') { s.to_lowercase() } else { s.chars().filter(|c| c.is_ascii_digit() || *c == '+').collect() }
}

/// Typed address vs a chat identifier: emails ignore case; phone numbers compare their last digits,
/// so "403 555 1234" matches "+14035551234".
fn same_addr(typed: &str, ident: &str) -> bool {
    if typed.contains('@') || ident.contains('@') {
        return typed.trim().eq_ignore_ascii_case(ident.trim());
    }
    let d = |s: &str| s.chars().filter(char::is_ascii_digit).collect::<String>();
    let (a, b) = (d(typed), d(ident));
    let n = a.len().min(b.len());
    n >= 5 && (a.len() == b.len() || n >= 7) && a.ends_with(&b[b.len() - n..]) && b.ends_with(&a[a.len() - n..])
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
                // read here but not on the Mac (imsg can't mark read): trust what we've shown
                let seen_here = p.and_then(|m| m["id"].as_i64()).is_some_and(|last| st.seen.get(&id).is_some_and(|s| *s >= last));
                let unread = c["unread_count"].as_i64().unwrap_or(0) > 0 && !seen_here;
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
    let (chat, msgs, names, older, preview_svc) = {
        let st = u.st.borrow();
        let Some(id) = st.sel else { return };
        let Some(chat) = st.chats.iter().find(|c| c["id"].as_i64() == Some(id)).cloned() else { return };
        let preview_svc = st.preview.get(&id).and_then(|m| m["service"].as_str()).map(String::from);
        (chat, st.hist.get(&id).cloned().unwrap_or_default(), st.names.clone(), st.older_loading, preview_svc)
    };
    u.stack.set_visible_child_name("conv");
    let title = chat_title(&chat, &names);
    let chat_svc = chat["service"].as_str().unwrap_or("iMessage").to_string();
    // chat.db's chat service can be stale (see mac::handle Send); the latest message tells what's in use
    let now_svc = msgs
        .iter()
        .rev()
        .find_map(|m| m["service"].as_str().map(String::from))
        .or(preview_svc)
        .unwrap_or_else(|| chat_svc.clone());
    let sms = now_svc != "iMessage";
    u.compose_hint.set_text(&if sms { format!("Text Message · {now_svc}") } else { "iMessage".into() });
    let group = chat["is_group"].as_bool() == Some(true);
    u.head_title.set_text(&title);
    let mut sub = now_svc.clone();
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
    u.stacks.borrow_mut().clear();
    u.group.set(group);
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
    let photos = u.st.borrow().photos.clone(); // the viewer's hooks borrow State
    u.viewer.refresh(photos);
    mark_seen(u);
}

/// The open chat's newest message has been seen (window focused): remember it, clear its dot.
fn mark_seen(u: &Rc<Ui>) {
    if !u.win.is_active() {
        return;
    }
    let changed = {
        let mut st = u.st.borrow_mut();
        let Some(id) = st.sel else { return };
        let Some(last) = st.hist.get(&id).and_then(|h| h.iter().filter_map(|m| m["id"].as_i64()).max()) else { return };
        let old = st.seen.insert(id, last);
        old.is_none_or(|o| o < last)
    };
    if changed {
        let st = u.st.borrow();
        let text: String = st.seen.iter().map(|(c, m)| format!("{c}={m}\n")).collect();
        let _ = std::fs::create_dir_all(u.store.seen_file.parent().unwrap());
        let _ = std::fs::write(&u.store.seen_file, text);
        drop(st);
        render_chats(u);
    }
}

fn load_seen(path: &std::path::Path) -> HashMap<i64, i64> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('='))
        .filter_map(|(c, m)| Some((c.parse().ok()?, m.parse().ok()?)))
        .collect()
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
        if path.ends_with(".pluginPayloadAttachment") {
            continue; // ponytail: link-preview data; drawn as a preview card in Phase 1.1
        }
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
    let link = preview_row(m).and_then(|r| link_for(u, r));
    let mut text = clean_text(m);
    if let Some(l) = &link {
        text = without_url(&text, &l.url); // the card shows the link; any other words follow it
    }
    let has_bubble = !text.is_empty() || !others.is_empty();
    if !photos.is_empty() {
        let tail = end_of_run && !has_bubble && link.is_none();
        out.push(photo_stack(u, &photos, bubble::Look { me, sms, tail, ..Default::default() }));
    }
    if let Some(l) = &link {
        let card = bubble::Look { me, sms, tail: end_of_run && !has_bubble, join_prev: join_prev && photos.is_empty(), join_next: has_bubble };
        out.push(link_card(u, l, card));
    }
    if has_bubble {
        let b = bubble::Bubble::new(look((join_prev && photos.is_empty()) || link.is_some()));
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

/// `text` with the link removed (it's shown as the preview card instead).
fn without_url(text: &str, url: &str) -> String {
    let t = text.replace(url, "").replace(url.trim_end_matches('/'), "");
    let t = t.trim();
    if t.chars().all(|c| c == '/') { String::new() } else { t.to_string() }
}

/// The row holding Apple's preview for this message: folded in by imsg (`url_preview`), or the
/// message itself when it is the preview row.
fn preview_row(m: &Value) -> Option<i64> {
    m["url_preview"]["id"].as_i64().or_else(|| {
        (m["balloon_bundle_id"].as_str() == Some("com.apple.messages.URLBalloonProvider")).then(|| m["id"].as_i64()).flatten()
    })
}

/// The preview if we have it; otherwise ask the Mac once (the card appears when it arrives).
fn link_for(u: &Ui, row: i64) -> Option<crate::mac::LinkPreview> {
    if let Some(p) = u.st.borrow().links.get(&row) {
        return p.clone();
    }
    u.st.borrow_mut().links.insert(row, None);
    u.send(Cmd::Link(row));
    None
}

const LINK_W: i32 = 300;

/// A shared link as Apple saved it, as one bubble: the preview image (if any) across the top, then
/// the title and site. Clicking opens it in the browser.
fn link_card(u: &Rc<Ui>, l: &crate::mac::LinkPreview, look: bubble::Look) -> gtk::Widget {
    let img_h = if l.image.is_some() { LINK_W * 52 / 100 } else { 0 };
    let b = bubble::Bubble::card(look, LINK_W, img_h);
    if let Some(img) = &l.image {
        b.add_css_class("fixed"); // keeps its card size instead of taking the photo's shape
        load_texture(u, &b, img);
    }
    if !l.title.is_empty() {
        let t = gtk::Label::new(Some(&l.title));
        t.add_css_class("link-title");
        t.set_xalign(0.0);
        t.set_wrap(true);
        t.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        t.set_lines(2);
        t.set_ellipsize(gtk::pango::EllipsizeMode::End);
        t.set_max_width_chars(34);
        b.add(&t);
    }
    // ponytail: the site's icon (l.icon) isn't drawn; a link glyph stands in
    let site = gtk::Label::new(Some(&format!("🔗 {}", if l.site.is_empty() { &l.url } else { &l.site })));
    site.add_css_class("link-site");
    site.set_xalign(0.0);
    site.set_ellipsize(gtk::pango::EllipsizeMode::End);
    site.set_max_width_chars(34);
    b.add(&site);
    b.set_tooltip_text(Some(&l.url));
    b.set_cursor_from_name(Some("pointer"));
    let click = gtk::GestureClick::new();
    let (u2, url) = (u.clone(), l.url.clone());
    click.connect_released(move |g, _, _, _| {
        g.set_state(gtk::EventSequenceState::Claimed);
        gtk::UriLauncher::new(&url).launch(Some(&u2.win), gio::Cancellable::NONE, |_| {});
    });
    b.add_controller(click);
    b.upcast()
}

const CARD: i32 = 210; // stacked photo card size
const CARD_STEP: f64 = 26.0; // how far each card under the top one peeks out
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
    // the widest spacing that still fits the chat; resting never exceeds it, hovering uses all of it
    let room = {
        let u = Rc::downgrade(u);
        move || {
            let w = u.upgrade().map_or(0.0, |u| stack_room(&u));
            // never further apart than side by side
            if w <= 0.0 { CARD_STEP } else { ((w - (CARD as f64 + bubble::TAIL as f64)) / (shown as f64 - 1.0)).clamp(0.0, CARD as f64 + 12.0) }
        }
    };
    let step = Rc::new(Cell::new(CARD_STEP.min(room())));
    let hovered = Rc::new(Cell::new(false));
    place(step.get());
    let (st2, pl2, rm2, hv2) = (step.clone(), place.clone(), room.clone(), hovered.clone());
    u.stacks.borrow_mut().push(Rc::new(move || {
        let s = if hv2.get() { rm2() } else { CARD_STEP.min(rm2()) };
        st2.set(s);
        pl2(s);
    }));
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
    let (sp, o2, rm, hv) = (spread.clone(), o.clone(), room.clone(), hovered.clone());
    hover.connect_enter(move |_, _, _| {
        hv.set(true);
        sp(&o2, rm());
    });
    let o2 = o.clone();
    hover.connect_leave(move |_| {
        hovered.set(false);
        spread(&o2, CARD_STEP.min(room()));
    });
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
    let (u2, path2) = (u.clone(), path.to_string());
    click.connect_released(move |g, _, _, _| {
        g.set_state(gtk::EventSequenceState::Claimed);
        let items = u2.st.borrow().photos.clone();
        u2.viewer.open(items, &path2);
    });
    p.add_controller(click);
    u.st.borrow_mut().photos.push((path.to_string(), p.clone()));
    load_texture(u, &p, path);
    p
}

/// Give `p` the chat copy of the image at `path`: from memory, the disk cache, or the Mac.
fn load_texture(u: &Rc<Ui>, p: &bubble::Bubble, path: &str) {
    let in_memory = u.st.borrow().textures.get(path).cloned(); // borrow ends here; or_else below borrows mutably
    let cached = in_memory.or_else(|| {
        let tex = texture(&std::fs::read(cache_path(u, path, false)).ok()?)?;
        u.st.borrow_mut().textures.insert(path.to_string(), tex.clone());
        Some(tex)
    });
    match cached {
        Some(t) => show_texture(p, &t),
        None => {
            let first = {
                let mut st = u.st.borrow_mut();
                let w = st.waiting.entry(path.to_string()).or_default();
                w.push(p.clone());
                w.len() == 1
            };
            if first {
                u.send(Cmd::Image { path: path.to_string(), full: false });
            }
        }
    }
}

fn texture(bytes: &[u8]) -> Option<gdk::Texture> {
    // the Mac already turned the photo upright (see mac::fetch_image)
    gdk::Texture::from_bytes(&glib::Bytes::from(bytes)).ok()
}

fn show_texture(p: &bubble::Bubble, t: &gdk::Texture) {
    if !p.has_css_class("stacked") && !p.has_css_class("fixed") {
        // a single photo takes its own shape; stacked cards stay square and crop to fill
        let (w, h) = (t.width().max(1) as f64, t.height().max(1) as f64);
        let s = (300.0 / w).min(340.0 / h).min(1.0);
        p.set_photo_size((w * s) as i32, (h * s) as i32);
    }
    p.set_texture(Some(t));
}

/// `full`: the viewer's big copy (mac::FULL_PX) rather than the chat's.
// ponytail: the cache never shrinks; prune by age if it gets big
fn cache_path(u: &Ui, path: &str, full: bool) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    u.store.image_cache.join(format!("{:016x}{}.jpg", h.finish(), if full { "-full" } else { "" }))
}

/// The viewer wants the big version of a photo: disk cache, else ask the Mac (once).
fn fetch_full(u: &Rc<Ui>, path: &str) {
    if let Some(t) = std::fs::read(cache_path(u, path, true)).ok().and_then(|b| texture(&b)) {
        u.viewer.full_ready(path, t);
    } else if u.st.borrow_mut().waiting_full.insert(path.to_string()) {
        u.send(Cmd::Image { path: path.to_string(), full: true });
    }
}

/// Width a photo stack may use in the open chat.
fn stack_room(u: &Ui) -> f64 {
    // ponytail: mirrors .msgs padding (18 px a side) and the avatar column (28 + 4) in style.css
    let w = u.scroller.hadjustment().page_size() - 36.0;
    if u.group.get() { w - 32.0 } else { w }
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

    #[test]
    fn url_removed_from_text() {
        assert_eq!(without_url("https://a.com/x", "https://a.com/x"), "");
        assert_eq!(without_url("look https://a.com/ nice", "https://a.com/"), "look  nice");
        assert_eq!(without_url("https://a.com/", "https://a.com"), "");
        assert_eq!(without_url("hello", "https://a.com"), "hello");
    }

    #[test]
    fn addresses() {
        assert!(is_addr("403 555-1234") && is_addr("a@b.ca") && is_addr("562528"));
        assert!(!is_addr("Sue") && !is_addr("12") && !is_addr("a@b"));
        assert!(same_addr("403 555 1234", "+14035551234"));
        assert!(same_addr("Sue@iCloud.com", "sue@icloud.com"));
        assert!(!same_addr("562528", "+14035562528"));
        assert!(!same_addr("4035551234", "+14035551235"));
        assert_eq!(clean_addr(" +1 (403) 555-1234 "), "+14035551234");
    }
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

