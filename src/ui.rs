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
use crate::mac::{Cmd, Config, Event, Kind, MediaItem};

const SIDEBAR_MIN: i32 = 260; // paned positions and widths below include the panels' side margins
const CHAT_MIN: i32 = 440;
const COLLAPSED_W: i32 = 96;
const NO_SLIDE: i32 = i32::MIN;
const AVATAR: i32 = 44; // the chat list's avatar size // the sidebar showing avatars only (side margins included)
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
    /// pinned chats, one chat identifier per line, in pin order (our own: Messages' pins aren't readable)
    pub pins_file: PathBuf,
    pub public_key: String,
}

/// A message that's been handed to the Mac but hasn't come back through the live stream yet.
#[derive(Clone)]
struct Pending {
    token: u64,
    chat_id: i64,
    item: PendingItem,
}

#[derive(Clone)]
enum PendingItem {
    Text(String),
    File(PathBuf),
}

/// The map around an address, ready to show: tiles in a grid and where the address is on it.
#[derive(Clone)]
struct MapView {
    lat: f64,
    lon: f64,
    cols: usize,
    pin_x: f64,
    pin_y: f64,
    tiles: Vec<gdk::Texture>,
}

/// A card's map box waiting for its map, and the place the box opens when clicked.
type MapWait = (glib::WeakRef<gtk::Overlay>, Rc<Cell<Option<(f64, f64)>>>);

#[derive(Default)]
struct State {
    /// preview pictures the Mac couldn't give us: drawn as if the preview had none
    bad_images: HashSet<String>,
    /// the info panel's links so far (the Links tab is rebuilt from these when its width changes)
    info_links: Vec<MediaItem>,
    info_files: Vec<MediaItem>,
    info_videos: Vec<MediaItem>,
    /// what the Mac said about a file (recording length, contact card); `None` while asked or when it couldn't say
    file_info: HashMap<String, Option<crate::mac::FileInfo>>,
    /// slots in the Links tab waiting for a preview card, by message row
    link_waiters: HashMap<i64, Vec<glib::WeakRef<gtk::Box>>>,
    /// files being copied from the Mac, with the chip that was clicked
    downloading: HashMap<String, glib::WeakRef<gtk::Widget>>,
    /// videos the viewer is waiting on (Mac path), and what we need to copy them (name, size)
    video_wait: std::collections::HashSet<String>,
    video_meta: HashMap<String, (String, i64)>,
    /// messages we've already set a "look for its link preview" timer for
    preview_watch: HashSet<String>,
    /// how much the window was widened to fit the info panel (and is narrowed by when it closes)
    /// the sidebar width the user has chosen (0: the default); the info panel may squeeze it meanwhile
    side_pref: i32,
    /// the chat the info panel shows (None: panel closed)
    info_chat: Option<i64>,
    /// per chat and kind: where the next page starts, and whether there is one
    media: HashMap<(i64, Kind), (usize, bool)>,
    /// the info panel's photo thumbnails, for the viewer
    /// the viewer was opened from the info panel (so a conversation redraw mustn't swap its photos)
    viewer_panel: bool,
    /// address text -> its map (None: couldn't be found, don't ask again this session)
    maps: HashMap<String, Option<MapView>>,
    /// card slots waiting for a map
    waiting_map: HashMap<String, Vec<MapWait>>,
    /// messages on their way (shown faded with a spinner until the real one arrives)
    pending: Vec<Pending>,
    cfg: Config,
    chats: Vec<Value>,
    names: HashMap<String, String>,
    hist: HashMap<i64, Vec<Value>>,
    preview: HashMap<i64, Value>,
    preview_asked: HashSet<i64>,
    sel: Option<i64>,
    textures: HashMap<String, gdk::Texture>,
    waiting: HashMap<String, Vec<bubble::Bubble>>,
    /// pictures to ask the Mac for, oldest first; sent newest-first once the chat has been built
    fetch_q: Vec<(String, bool)>,
    /// photo widgets of the open chat in display order (the viewer cycles through these)
    photos: Vec<crate::viewer::Item>,
    /// full-size viewer fetches in flight
    waiting_full: HashSet<String>,
    host_key: String,
    connected: bool,
    chats_loaded: bool,
    /// chats with a history request in flight (header spinner)
    loading: HashSet<i64>,
    older_loading: bool,
    /// the Mac's contact photos (asked for once per connection) and the decoded pictures
    contacts: Option<crate::mac::Contacts>,
    avatar_tex: HashMap<String, Option<gdk::Texture>>,
    /// avatar keys already sent to the background decoder
    avatar_asked: HashSet<String>,
    /// link previews by message row: None = asked for, or the Mac had none
    links: HashMap<i64, Option<crate::mac::LinkPreview>>,
    /// files waiting in the composer
    outgoing: Vec<PathBuf>,
    /// chat id -> newest message id seen in BubbleFairy; hides the unread dot the Mac can't clear
    seen: HashMap<i64, i64>,
    /// identifiers (`pin_key`) of the pinned chats, in pin order
    pins: Vec<String>,
    /// address a new conversation's first message went to; its chat opens once it shows up
    new_pending: Option<String>,
    /// addresses already looked up on the Mac for an older chat (new-message view)
    found_asked: std::collections::HashSet<String>,
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
    /// the pinned chats above the list
    tiles: gtk::FlowBox,
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
    /// spinner and text in the middle of the empty message area while a chat's first messages load
    msgs_loading: gtk::Box,
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
    stick_bottom: Rc<Cell<bool>>,
    /// when not pinned to the bottom, keep this distance from the bottom while content changes above
    selecting: Cell<bool>,
    /// a `render_chats_soon` is waiting
    list_queued: Cell<bool>,
    /// bumped by every `render_conv`: a batch of older messages still being drawn stops when it is stale
    conv_gen: Cell<u32>,
    /// the older part of the open chat is still being built (see `render_conv`)
    staging: Cell<bool>,
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
    /// counts To-field edits: a lookup waits out the typing
    find_gen: Cell<u32>,
    /// the sidebar is gliding to a row (a list rebuild must not put it back)
    side_anim: Cell<bool>,
    send_token: Cell<u64>,
    info: Info,
    paned: gtk::Paned,
    sidebar: gtk::Box,
    side_title: gtk::Label,
    side_toggle: gtk::Button,
    conv_panel: gtk::Box,
    /// the window is too narrow for the full sidebar beside the chat
    narrow: Cell<bool>,
    /// narrow window, and the sidebar is expanded over the chat (forgotten when the window gets wide again)
    side_open: Cell<bool>,
    /// the chat list is drawn avatars-only
    collapsed: Cell<bool>,
    /// the chat rows' text boxes with their chats' names (width and tooltips follow the sidebar's state)
    rows: RefCell<Vec<(gtk::ListBoxRow, gtk::Box, String)>>,
    /// the changing parts of each list row, by chat id (see `update_row`)
    row_parts: RefCell<HashMap<i64, RowParts>>,
    /// width the row texts have now / will have once the sidebar is settled (so text isn't re-truncated mid-slide)
    text_w: Cell<i32>,
    text_target: Cell<i32>,
    /// where relayout last put the paned divider (a different position means the user dragged it)
    last_pos: Cell<i32>,
    /// the next relayout slides the divider instead of jumping
    animate: Cell<bool>,
    /// bumped to cancel a running slide
    slide: Cell<u32>,
    /// where the running slide ends (`NO_SLIDE`: none)
    slide_to: Cell<i32>,
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
    let tiles = gtk::FlowBox::new();
    tiles.add_css_class("tiles");
    tiles.set_selection_mode(gtk::SelectionMode::None);
    tiles.set_homogeneous(true);
    tiles.set_max_children_per_line(3);
    tiles.set_min_children_per_line(1);
    tiles.set_visible(false);
    let side_inner = gtk::Box::new(gtk::Orientation::Vertical, 0);
    side_inner.append(&tiles);
    side_inner.append(&chat_list);
    let side_scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&side_inner).build();
    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar.add_css_class("sidebar");
    let side_title = gtk::Label::new(Some("Conversations"));
    side_title.add_css_class("side-title");
    side_title.set_xalign(0.0);
    side_title.set_valign(gtk::Align::Center);
    let new_btn = gtk::Button::from_icon_name("document-edit-symbolic");
    new_btn.set_tooltip_text(Some("New message (Ctrl+N)"));
    new_btn.add_css_class("head-btn");
    new_btn.add_css_class("new-btn");
    new_btn.set_valign(gtk::Align::Center);
    new_btn.set_action_name(Some("app.new-message"));
    // narrow window: expands the avatars-only sidebar over the chat and back
    let side_toggle = gtk::Button::from_icon_name("bf-sidebar-symbolic");
    side_toggle.add_css_class("head-btn");
    side_toggle.set_halign(gtk::Align::Start); // hexpand must not stretch the circle into a pill
    side_toggle.add_css_class("side-toggle");
    side_toggle.set_valign(gtk::Align::Center);
    side_toggle.set_visible(false);
    let side_head = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    side_head.append(&side_toggle);
    // the title sits in a slot that measures nothing, so the sidebar can narrow past it: it is cropped, not squeezed
    let title_slot = gtk::Overlay::new();
    title_slot.set_child(Some(&gtk::Box::new(gtk::Orientation::Horizontal, 0)));
    title_slot.add_overlay(&side_title);
    title_slot.set_clip_overlay(&side_title, true);
    title_slot.set_hexpand(true);
    side_title.set_halign(gtk::Align::Start);
    side_title.set_margin_start(18);
    side_title.set_margin_end(18);
    side_head.append(&title_slot);
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
    side_stack.set_transition_type(gtk::StackTransitionType::None); // a fade froze half way while the rows were built
    side_stack.add_named(&side_loading, Some("loading"));
    side_stack.add_named(&side_scroll, Some("list"));
    side_stack.set_vexpand(true);
    side_stack.set_hhomogeneous(false); // else the "Loading chats…" text keeps the collapsed sidebar wide
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
    titles.append(&head_title);
    titles.append(&head_sub);
    let conv_head = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    conv_head.add_css_class("conv-head");
    conv_head.append(&head_avatar);
    conv_head.append(&titles);
    let info = build_info();
    conv_head.append(&info.refresh);
    conv_head.append(&info.btn);
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
    // anchored to the bottom (see chatscroll.rs); `stick` is true while the view follows the newest message
    let stick = Rc::new(Cell::new(true));
    let anchored = crate::chatscroll::ChatScroll::new(&msgs, stick.clone());
    let scroller = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&anchored).build();

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
    // a round button over the bottom of the messages that jumps back down once you're a panel's height or more up
    let to_bottom = gtk::Button::from_icon_name("go-down-symbolic");
    to_bottom.add_css_class("to-bottom");
    to_bottom.set_halign(gtk::Align::Center);
    to_bottom.set_valign(gtk::Align::End);
    to_bottom.set_can_target(false);
    to_bottom.set_tooltip_text(Some("Jump to the newest message"));
    let (stick2, anchored2, scroller2) = (stick.clone(), anchored.clone(), scroller.clone());
    to_bottom.connect_clicked(move |_| {
        // glide down (ease-out, 350 ms) towards wherever the bottom is each frame, then stay stuck to it
        let adj = scroller2.vadjustment();
        let from = adj.value();
        let began = Cell::new(None::<i64>);
        let (stick, anchored) = (stick2.clone(), anchored2.clone());
        scroller2.add_tick_callback(move |_, clock| {
            let t0 = began.get().unwrap_or_else(|| {
                began.set(Some(clock.frame_time()));
                clock.frame_time()
            });
            let t = ((clock.frame_time() - t0) as f64 / 350_000.0).min(1.0);
            let e = 1.0 - (1.0 - t).powi(3);
            let bottom = adj.upper() - adj.page_size();
            adj.set_value(from + (bottom - from) * e);
            if t >= 1.0 {
                stick.set(true);
                anchored.queue_allocate();
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    });
    let scroll_over = gtk::Overlay::new();
    scroll_over.set_child(Some(&scroller));
    scroll_over.add_overlay(&to_bottom);
    let msgs_loading = gtk::Box::new(gtk::Orientation::Vertical, 10);
    msgs_loading.set_halign(gtk::Align::Center);
    msgs_loading.set_valign(gtk::Align::Center);
    let sp = gtk::Spinner::new();
    sp.set_size_request(28, 28);
    msgs_loading.append(&sp);
    let ml = gtk::Label::new(Some("Loading messages…"));
    ml.add_css_class("loading-text");
    msgs_loading.append(&ml);
    msgs_loading.set_visible(false);
    scroll_over.add_overlay(&msgs_loading);
    scroll_over.set_vexpand(true);
    scroll_over.set_visible(scroller.property("visible"));
    let so = scroll_over.clone();
    // the suggestions list replaces it in the new-message view. The property, not `is_visible()`: that one also asks
    // the ancestors, so once `scroll_over` was hidden the scroller could never show it again
    scroller.connect_visible_notify(move |s| so.set_visible(s.property("visible")));
    let show_btn = {
        let b = to_bottom.clone();
        move |a: &gtk::Adjustment| {
            let away = a.upper() - a.value() - a.page_size() >= a.page_size().max(1.0);
            if away {
                b.add_css_class("shown");
            } else {
                b.remove_css_class("shown");
            }
            b.set_can_target(away);
        }
    };
    scroller.vadjustment().connect_value_changed(show_btn.clone());
    scroller.vadjustment().connect_changed(show_btn);
    conv.append(&scroll_over);
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
    let viewer = viewer::Viewer::new();
    conv_panel.set_overflow(gtk::Overflow::Hidden); // rounded corners clip the content
    conv_panel.append(&stack);
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
    conv_panel.set_hexpand(true);
    sidebar.set_size_request(COLLAPSED_W - 20, -1); // minus its margins; the window's narrowest
    conv_panel.set_size_request(CHAT_MIN - 20, -1);
    // the info panel slides over the chat's right side
    let right = gtk::Overlay::new();
    right.set_child(Some(&conv_panel));
    info.revealer.set_halign(gtk::Align::End);
    right.add_overlay(&info.revealer);
    paned.set_end_child(Some(&right));
    paned.set_position(340);
    // the sidebar may be squeezed below its content's width (clipped): while it slides between collapsed and
    // expanded the names are there before it has the room; the window's own minimum is set here instead
    paned.set_shrink_start_child(true);
    sidebar.set_overflow(gtk::Overflow::Hidden);
    win.set_size_request(COLLAPSED_W + CHAT_MIN, -1);
    paned.set_resize_start_child(false);
    paned.set_shrink_end_child(true);
    paned.set_vexpand(true);
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&banner);
    // the photo viewer is a panel of its own over the sidebar and the chat, edge to edge with them (the panels'
    // outer margins), so a photo opened from the info panel isn't hidden under anything
    let stage = gtk::Overlay::new();
    stage.set_child(Some(&paned));
    stage.set_vexpand(true);
    viewer.root.set_margin_top(2);
    viewer.root.set_margin_bottom(14);
    viewer.root.set_margin_start(14);
    viewer.root.set_margin_end(14);
    stage.add_overlay(&viewer.root);
    let w = win.clone();
    viewer.root.connect_visible_notify(move |r| {
        // the panels under it lose their shadows and bevels, so they don't add to the viewer's
        if r.is_visible() {
            w.add_css_class("viewing");
        } else {
            w.remove_css_class("viewing");
        }
    });
    root.append(&stage);
    win.set_child(Some(&root));

    let seen = load_seen(&store.seen_file);
    let pins: Vec<String> = std::fs::read_to_string(&store.pins_file).unwrap_or_default().lines().filter(|l| !l.is_empty()).map(String::from).collect();
    // dev: BF_PINS=<key>,<key> pins those chats (demo chats' keys are their participants)
    let pins = std::env::var("BF_PINS").map(|v| v.split(';').map(String::from).collect()).unwrap_or(pins);
    let ui = Rc::new(Ui {
        app: app.clone(),
        win: win.clone(),
        store,
        cmd,
        viewer,
        st: RefCell::new(State { cfg, seen, pins, ..Default::default() }),
        chat_list,
        tiles,
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
        msgs_loading,
        head_titles: titles,
        to_row,
        to_entry,
        suggest,
        suggest_scroll,
        status,
        status_text,
        banner,
        banner_text,
        stick_bottom: stick.clone(),
        selecting: Cell::new(false),
        list_queued: Cell::new(false),
        conv_gen: Cell::new(0),
        staging: Cell::new(false),
        sel_gen: Cell::new(0),
        banner_offline: Cell::new(false),
        chats_queued: Cell::new(false),
        group: Cell::new(false),
        stacks: RefCell::default(),
        render_queued: Cell::new(false),
        new_mode: Cell::new(false),
        find_gen: Cell::new(0),
        side_anim: Cell::new(false),
        send_token: Cell::new(0),
        info,
        paned: paned.clone(),
        sidebar: sidebar.clone(),
        side_title: side_title.clone(),
        side_toggle: side_toggle.clone(),
        conv_panel: conv_panel.clone(),
        narrow: Cell::new(false),
        side_open: Cell::new(false),
        collapsed: Cell::new(false),
        rows: RefCell::default(),
        row_parts: RefCell::default(),
        text_w: Cell::new(200),
        text_target: Cell::new(200),
        last_pos: Cell::new(0),
        animate: Cell::new(false),
        slide: Cell::new(0),
        slide_to: Cell::new(NO_SLIDE),
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
            // the click itself gives the row the focus once the selection is done: take it back for the composer
            let u2 = u.clone();
            glib::idle_add_local_once(move || {
                u2.compose.grab_focus();
            });
        }
    });

    // scrolled near the top: fetch older messages (the view itself stays put, see chatscroll.rs)
    let u = ui.clone();
    ui.scroller.vadjustment().connect_value_changed(move |a| {
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
    ui.to_entry.connect_changed(move |_| {
        render_suggest(&u);
        find_prior_chat(&u);
    });
    let u = ui.clone();
    ui.to_entry.connect_activate(move |e| {
        // Enter: open the top suggestion, or move on to the message if it's a new address
        match ui_first_suggestion(&u) {
            Some((Some(id), _)) if !e.text().trim().is_empty() => open_chat(&u, id),
            Some((None, addr)) if !is_addr(&e.text()) => e.set_text(&addr),
            _ if is_addr(&e.text()) => {
                u.compose.grab_focus();
            }
            _ => {}
        }
    });
    let u = ui.clone();
    ui.suggest.connect_row_activated(move |_, row| {
        let name = row.widget_name();
        if let Some(addr) = name.strip_prefix("a:") {
            u.to_entry.set_text(addr);
            u.compose.grab_focus();
        } else if let Ok(id) = name.parse::<i64>() {
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
    wire_info(&ui);
    // keep the three panels fitting as the window is resized (see relayout)
    let (weak, last_w) = (Rc::downgrade(&ui), Cell::new(0));
    ui.paned.add_tick_callback(move |p, _| {
        let Some(u) = weak.upgrade() else { return glib::ControlFlow::Break };
        let dragged = p.position() != u.last_pos.get();
        if dragged && !u.narrow.get() && !u.info.revealer.reveals_child() {
            u.st.borrow_mut().side_pref = p.position(); // wherever the user has dragged it
        }
        if dragged || p.width() != last_w.replace(p.width()) {
            relayout(&u, p.width());
        }
        glib::ControlFlow::Continue
    });
    let u = ui.clone();
    ui.side_toggle.connect_clicked(move |_| {
        let open = !u.side_open.get();
        u.side_open.set(open);
        if open && u.info.revealer.reveals_child() {
            close_info(&u);
        }
        u.animate.set(true);
        relayout(&u, u.paned.width());
    });

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

    // a click outside the info panel closes it (the "i" button itself toggles; popovers and the viewer keep it)
    let click = gtk::GestureClick::new();
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    let u = ui.clone();
    click.connect_pressed(move |_, _, x, y| {
        if !u.info.revealer.reveals_child() || u.viewer.is_open() {
            return;
        }
        let Some(hit) = u.win.pick(x, y, gtk::PickFlags::DEFAULT) else { return };
        let inside = |w: &gtk::Widget| hit == *w || hit.is_ancestor(w);
        if !inside(u.info.panel.upcast_ref()) && !inside(u.info.btn.upcast_ref()) && hit.ancestor(gtk::Popover::static_type()).is_none() {
            close_info(&u);
        }
    });
    ui.win.add_controller(click);
    // viewer keys: at window level so they work whatever has focus
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let u = ui.clone();
    keys.connect_key_pressed(move |_, key, _, _| {
        if u.viewer.is_open() && u.viewer.key(key) { glib::Propagation::Stop } else { glib::Propagation::Proceed }
    });
    win.add_controller(keys);
    let w = Rc::downgrade(&ui);
    ui.viewer.set_hooks(viewer::Hooks {
        fetch_full: Box::new(move |p| if let Some(u) = w.upgrade() { fetch_full(&u, p) }),
        fetch_video: Box::new({
            let w = Rc::downgrade(&ui);
            move |p| if let Some(u) = w.upgrade() { fetch_video(&u, p) }
        }),
        set_fullscreen: Box::new({
            let w = Rc::downgrade(&ui);
            let was = Rc::new(Cell::new(false)); // the window was already full screen before the viewer asked
            move |on| {
                let Some(u) = w.upgrade() else { return };
                let m = if on { [0; 4] } else { [2, 14, 14, 14] };
                u.viewer.root.set_margin_top(m[0]);
                u.viewer.root.set_margin_end(m[1]);
                u.viewer.root.set_margin_bottom(m[2]);
                u.viewer.root.set_margin_start(m[3]);
                if on {
                    was.set(u.win.is_fullscreen());
                    u.win.fullscreen();
                } else if !was.get() {
                    u.win.unfullscreen();
                }
            }
        }),
        open_external: Box::new({
            let w = Rc::downgrade(&ui);
            move |p| if let Some(u) = w.upgrade() { open_file(&u, p) }
        }),
    });
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
            // BF_LINK_TITLE=<text> picks the card whose title has that text; else the first with a big picture
            fn has_text(w: &gtk::Widget, needle: &str) -> bool {
                if w.downcast_ref::<gtk::Label>().is_some_and(|l| l.text().contains(needle)) {
                    return true;
                }
                let mut c = w.first_child();
                while let Some(x) = c {
                    if has_text(&x, needle) {
                        return true;
                    }
                    c = x.next_sibling();
                }
                false
            }
            fn find(w: &gtk::Widget, title: Option<&str>) -> Option<gtk::Widget> {
                if w.has_css_class("linkcard") && title.map_or(w.has_css_class("fixed"), |t| has_text(w, t)) {
                    return Some(w.clone());
                }
                let mut c = w.first_child();
                while let Some(x) = c {
                    if let Some(f) = find(&x, title) {
                        return Some(f);
                    }
                    c = x.next_sibling();
                }
                None
            }
            let title = std::env::var("BF_LINK_TITLE").ok();
            if let Some(card) = find(u.msgs.upcast_ref(), title.as_deref())
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
    if let Some(ms) = std::env::var("BF_INFO").ok().and_then(|v| v.parse::<u64>().ok()) {
        // dev aid: open the info panel after <ms> (e.g. with BF_CHAT, in a headless compositor)
        let u = ui.clone();
        glib::timeout_add_local_once(Duration::from_millis(ms), move || {
            open_info(&u);
            // BF_TAB=files|links: show that tab
            let n = match std::env::var("BF_TAB").as_deref() {
                Ok("files") => 1,
                Ok("links") => 2,
                _ => 0,
            };
            let mut tab = u.info.switcher.first_child();
            for _ in 0..n {
                tab = tab.and_then(|t| t.next_sibling());
            }
            if let Some(t) = tab.and_downcast::<gtk::ToggleButton>() {
                t.set_active(true);
            }
        });
    }
    if let Some(ms) = std::env::var("BF_SIDE").ok().and_then(|v| v.parse::<u64>().ok()) {
        // dev aid: press the sidebar button after <ms> (narrow window)
        let u = ui.clone();
        glib::timeout_add_local_once(Duration::from_millis(ms), move || u.side_toggle.emit_clicked());
    }
    if let Ok(id) = std::env::var("BF_YT") {
        // dev aid: open the viewer on this YouTube video id
        let u = ui.clone();
        glib::timeout_add_local_once(Duration::from_millis(1500), move || u.viewer.open_youtube(&id));
    }
    if std::env::var_os("BF_VIEW").is_some() {
        // dev aid: open the viewer on the newest loaded photo shortly before BF_SHOT fires
        // BF_VIEW=<ms> sets when it opens; otherwise 0.9 s before the shot
        let ms = std::env::var("BF_SHOT_MS").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(2500);
        let at = std::env::var("BF_VIEW").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(ms.saturating_sub(900));
        let u = ui.clone();
        glib::timeout_add_local_once(Duration::from_millis(at), move || {
            let items = u.st.borrow().photos.clone();
            if let Some((last, _)) = items.iter().rev().find(|(_, w)| w.downcast_ref::<bubble::Bubble>().is_some_and(|b| b.texture().is_some())).cloned() {
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
        Event::Contacts(c) => {
            {
                let mut st = u.st.borrow_mut();
                st.contacts = Some(c);
                st.avatar_tex.retain(|_, t| t.is_some()); // a photo added since is picked up, the others stay
                st.avatar_asked.clear();
                fill_names(&mut st);
            }
            prefetch_avatars(u);
            render_chats(u);
            if u.st.borrow().sel.is_some() {
                render_conv(u);
            }
        }
        Event::Connected => {
            if u.st.borrow().contacts.is_none() {
                u.send(Cmd::Contacts);
            }
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
                // the chat on show may be older than the list's 100 (found by address): keep it
                let keep = st.sel.filter(|id| !chats.iter().any(|c| c["id"].as_i64() == Some(*id))).and_then(|id| st.chats.iter().find(|c| c["id"].as_i64() == Some(id)).cloned());
                st.chats = chats;
                if let Some(c) = keep {
                    insert_chat(&mut st.chats, c);
                }
                st.chats_loaded = true;
                fill_names(&mut st);
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
            prefetch_avatars(u);
            render_chats(u); // before the fade starts: building the rows takes long enough to freeze it half way
            u.side_stack.set_visible_child_name("list");
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
                {
                    let mut st = u.st.borrow_mut();
                    st.new_pending = None;
                    st.pending.iter_mut().filter(|p| p.chat_id == 0).for_each(|p| p.chat_id = id);
                }
                open_chat(u, id);
            }
            if let Some(id) = first {
                select_in_list(u, id);
                select(u, id);
            }
        }
        Event::FoundChat(addr, chat) => {
            let Some(chat) = chat else { return };
            let Some(id) = chat["id"].as_i64() else { return };
            if !u.st.borrow().chats.iter().any(|c| c["id"].as_i64() == Some(id)) {
                insert_chat(&mut u.st.borrow_mut().chats, chat);
                render_chats(u);
                u.send(Cmd::Preview(id));
            }
            // still in the new-message view with that address: show the conversation, like Messages does
            if u.new_mode.get() && same_addr(u.to_entry.text().trim(), &addr) {
                open_chat(u, id);
            }
        }
        Event::Preview(id, m) => {
            learn_names(u, std::slice::from_ref(&m));
            u.st.borrow_mut().preview.insert(id, m);
            update_row(u, id);
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
        Event::Sent { text, files, err, token } => {
            if let Some(err) = err {
                u.st.borrow_mut().new_pending = None;
                drop_pending(u, token);
                show_banner(u, &format!("Message not sent: {err}"), false);
                let buf = u.compose.buffer();
                if buf.char_count() == 0 {
                    buf.set_text(&text); // nothing typed is lost
                }
                add_files(u, files); // nor any file that didn't go
            } else {
                if !u.banner_offline.get() {
                    u.banner.set_reveal_child(false);
                }
                // normally the real messages replace these within a second; if they never show, don't leave spinners
                let u2 = u.clone();
                glib::timeout_add_local_once(Duration::from_secs(20), move || drop_pending(&u2, token));
            }
        }
        Event::Recent(id, msgs) => {
            let is_sel = {
                let mut st = u.st.borrow_mut();
                if let Some(h) = st.hist.get_mut(&id) {
                    for m in msgs {
                        match h.iter_mut().find(|x| x["guid"] == m["guid"]) {
                            Some(old) => *old = m,
                            None => h.push(m), // e.g. Messages replaced the row it sent with another
                        }
                    }
                }
                st.sel == Some(id)
            };
            if is_sel {
                render_conv(u);
            }
        }
        Event::Media { chat_id, kind, offset, items, next, more } => on_media(u, chat_id, kind, offset, items, next, more),
        Event::Map(query, data) => {
            let view = data.and_then(|d| {
                let _ = std::fs::create_dir_all(&u.store.image_cache);
                let _ = std::fs::write(map_cache_path(u, &query), map_cache_bytes(&d));
                map_view(&d)
            });
            u.st.borrow_mut().maps.insert(query.clone(), view.clone());
            let slots = u.st.borrow_mut().waiting_map.remove(&query).unwrap_or_default();
            for (frame, loc) in slots.iter().filter_map(|(w, l)| Some((w.upgrade()?, l))) {
                fill_map(&frame, loc, view.as_ref());
            }
        }
        Event::Downloaded { path, dest, err } => {
            let chip = u.st.borrow_mut().downloading.remove(&path).and_then(|w| w.upgrade());
            if let Some(c) = chip {
                c.set_opacity(1.0);
                c.set_tooltip_text(Some("Copy from the Mac and open"));
            }
            if u.st.borrow_mut().video_wait.remove(&path) {
                match err {
                    Some(e) => u.viewer.video_failed(&path, &e),
                    None => u.viewer.video_ready(&path, dest),
                }
            } else {
                match err {
                    Some(e) => show_banner(u, &format!("Couldn't copy the file from the Mac: {e}"), false),
                    None => open_file(u, &dest),
                }
            }
        }
        Event::DownloadProgress { path, got, total } => u.viewer.video_progress(&path, got, total),
        Event::FileInfo(path, info) => {
            let found = info.is_some();
            u.st.borrow_mut().file_info.insert(path, info);
            if found {
                queue_conv_render(u);
            }
        }
        Event::Link(row, p) => {
            u.st.borrow_mut().links.insert(row, p.clone());
            // Links tab slots waiting for this card
            if let Some(l) = &p {
                let slots = u.st.borrow_mut().link_waiters.remove(&row).unwrap_or_default();
                for slot in slots.iter().filter_map(|w| w.upgrade()) {
                    clear(&slot);
                    slot.append(&link_tile(u, &l.url, Some(l)));
                }
            }
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
            let Some(b) = bytes else { return image_done(u, path, full, None) };
            // written to the cache and decoded off the GTK thread (a photo costs 5-10 ms to decode)
            let (file, dir, u2) = (cache_path(u, &path, full), u.store.image_cache.clone(), u.clone());
            bg(
                move || {
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::fs::write(&file, &b);
                    decode_file(&file)
                },
                move |tex| image_done(&u2, path, full, tex),
            );
        }
    }
}

/// A picture the Mac sent (or that was read back from the cache) has been decoded: show it where it waits.
fn image_done(u: &Rc<Ui>, path: String, full: bool, tex: Option<gdk::Texture>) {
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
    } else {
        // couldn't load: no endless spinner; a link card gives its empty picture area back
        for p in &pics {
            if p.has_css_class("linkcard") {
                p.drop_card_image();
            } else {
                stop_spin(p);
            }
        }
        // and a link preview whose picture is that one is redrawn without it (with the link icon)
        u.st.borrow_mut().bad_images.insert(path);
        if !u.render_queued.replace(true) {
            let u2 = u.clone();
            glib::idle_add_local_once(move || {
                u2.render_queued.set(false);
                render_conv(&u2);
                if !u2.st.borrow().info_links.is_empty() {
                    render_links(&u2);
                }
                if !u2.st.borrow().info_files.is_empty() {
                    render_files(&u2);
                }
                render_videos(&u2);
            });
        }
    }

}

/// Forget a send's waiting bubbles (it failed, or never showed up).
fn drop_pending(u: &Rc<Ui>, token: u64) {
    let (had, sel) = {
        let mut st = u.st.borrow_mut();
        let before = st.pending.len();
        st.pending.retain(|p| p.token != token);
        (before != st.pending.len(), st.sel)
    };
    if had && sel.is_some() {
        render_conv(u);
    }
    if had {
        show_new_ghosts(u);
    }
}

/// My own message came back through the live stream: its waiting bubble is replaced by the real one.
fn settle_pending(st: &mut State, cid: i64, m: &Value) {
    let text = clean_text(m);
    let has_files = m["attachments"].as_array().is_some_and(|a| !a.is_empty());
    let name = |p: &PathBuf| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let at = st.pending.iter().position(|p| {
        // chat id 0: the first message to a new address, whose chat we don't know yet
        (p.chat_id == cid || p.chat_id == 0)
            && match &p.item {
                PendingItem::Text(t) => !text.is_empty() && *t == text,
                PendingItem::File(f) => {
                    has_files
                        && m["attachments"].as_array().into_iter().flatten().any(|a| a["transfer_name"].as_str().is_none_or(|n| n == name(f)))
                }
            }
    });
    if let Some(i) = at {
        let gone = st.pending.remove(i);
        if let PendingItem::File(f) = gone.item {
            // the real photo shows the local picture at once instead of waiting for the fetch back from the Mac
            let att = m["attachments"].as_array().into_iter().flatten().find(|a| a["transfer_name"].as_str().is_none_or(|n| n == name(&f)));
            if let (Some(path), Ok(t)) = (att.and_then(|a| a["original_path"].as_str()).filter(|p| !p.is_empty()), gdk::Texture::from_filename(&f)) {
                st.textures.entry(path.to_string()).or_insert(t);
            }
        }
    }
}

fn on_message(u: &Rc<Ui>, m: Value) {
    learn_names(u, std::slice::from_ref(&m));
    let Some(cid) = m["chat_id"].as_i64() else { return };
    let mutation = m["type"].as_str().is_some();
    if !mutation && m["is_from_me"].as_bool() == Some(true) {
        settle_pending(&mut u.st.borrow_mut(), cid, &m);
        show_new_ghosts(u);
    }
    let mut fresh = false; // a message we hadn't got yet
    let (is_sel, known_chat) = {
        let mut st = u.st.borrow_mut();
        if let Some(h) = st.hist.get_mut(&cid) {
            match h.iter_mut().find(|x| x["guid"] == m["guid"]) {
                Some(old) => *old = m.clone(),
                None if !mutation => {
                    h.push(m.clone());
                    fresh = true;
                }
                None => {}
            }
        }
        if !mutation {
            st.preview.insert(cid, m.clone());
        }
        (st.sel == Some(cid), st.chats.iter().any(|c| c["id"].as_i64() == Some(cid)))
    };
    if is_sel {
        if fresh {
            u.stick_bottom.set(true); // a new message while scrolled up brings you down to it
        }
        render_conv(u);
    }
    if mutation {
        return;
    }
    watch_for_preview(u, cid, &m);
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

/// A message with a link arrives before Messages has made its preview (and, for one we sent, before it
/// has pulled the link out of the text). Look again a few times so the card replaces the plain text.
fn watch_for_preview(u: &Rc<Ui>, cid: i64, m: &Value) {
    let Some(guid) = m["guid"].as_str() else { return };
    if preview_row(m).is_some() || crate::mac::urls_in(&clean_text(m)).is_empty() || !u.st.borrow_mut().preview_watch.insert(guid.to_string()) {
        return;
    }
    for secs in [3, 8, 20] {
        let (u2, guid) = (u.clone(), guid.to_string());
        glib::timeout_add_local_once(Duration::from_secs(secs), move || {
            let (done, connected) = {
                let st = u2.st.borrow();
                let msg = st.hist.get(&cid).and_then(|h| h.iter().find(|x| x["guid"].as_str() == Some(&guid)));
                (msg.is_none_or(|x| preview_row(x).is_some()), st.connected)
            };
            if !done && connected {
                u2.send(Cmd::Recent(cid));
            }
        });
    }
}

/// Names for the people in group chats from the Mac's contacts: imsg only names the other person of a 1:1 chat, and
/// the rest were only learned from messages, so a group's title showed an address until the chat had been opened.
fn fill_names(st: &mut State) {
    let Some(contacts) = &st.contacts else { return };
    let mut found = Vec::new();
    for c in &st.chats {
        for h in c["participants"].as_array().into_iter().flatten().filter_map(|p| p.as_str()) {
            if !st.names.contains_key(h)
                && let Some(p) = contacts.person(h).filter(|p| !p.name.is_empty())
            {
                found.push((h.to_string(), p.name.clone()));
            }
        }
    }
    st.names.extend(found);
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

/// Highlight the pinned tile of this chat (if it has one), clear the others.
fn mark_tile(u: &Ui, id: i64) {
    let mut c = u.tiles.first_child();
    while let Some(w) = c {
        if let Some(t) = w.downcast_ref::<gtk::FlowBoxChild>().and_then(|f| f.child()) {
            if t.widget_name() == id.to_string() { t.add_css_class("selected") } else { t.remove_css_class("selected") }
        }
        c = w.next_sibling();
    }
}

fn select(u: &Rc<Ui>, id: i64) {
    fold_side(u);
    end_new(u);
    u.viewer.close_now();
    u.st.borrow_mut().sel = Some(id);
    select_in_list(u, id);
    mark_tile(u, id);
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
    if u.info.revealer.reveals_child() {
        refresh_info(u);
    }
}

const HISTORY_PAGE: usize = 150;

fn request_history(u: &Rc<Ui>, id: i64) {
    u.st.borrow_mut().loading.insert(id);
    u.send(Cmd::History(id, HISTORY_PAGE));
    update_spinner(u);
}

/// Reload the open chat from the Mac, as many messages as are showing now (more if the user has
/// scrolled back), and forget link previews that came up empty so they're asked for again.
fn refresh_chat(u: &Rc<Ui>) {
    let (id, limit) = {
        let mut st = u.st.borrow_mut();
        let Some(id) = st.sel else { return };
        st.links.retain(|_, v| v.is_some());
        (id, st.hist.get(&id).map_or(0, Vec::len).max(HISTORY_PAGE))
    };
    u.st.borrow_mut().loading.insert(id);
    u.send(Cmd::History(id, limit));
    update_spinner(u);
}

fn update_spinner(u: &Ui) {
    let on = {
        let st = u.st.borrow();
        // only while there is nothing to look at: a reload of messages already showing is silent
        st.sel.is_some_and(|id| st.loading.contains(&id) && st.hist.get(&id).is_none_or(|h| h.is_empty()))
    };
    u.msgs_loading.set_visible(on);
    if let Some(sp) = u.msgs_loading.first_child().and_downcast::<gtk::Spinner>() {
        sp.set_spinning(on);
    }
}

/// Scrolled near the top: fetch the next page of older messages.
fn load_older(u: &Rc<Ui>) {
    let req = {
        let mut st = u.st.borrow_mut();
        let Some(id) = st.sel else { return };
        if st.older_loading || st.no_more.contains(&id) || !st.connected || u.staging.get() {
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
    let token = u.send_token.get() + 1;
    u.send_token.set(token);
    {
        // a faded bubble per message until the real one comes back through the live stream
        let mut st = u.st.borrow_mut();
        for f in &files {
            st.pending.push(Pending { token, chat_id, item: PendingItem::File(f.clone()) });
        }
        if !text.is_empty() {
            st.pending.push(Pending { token, chat_id, item: PendingItem::Text(text.clone()) });
        }
    }
    u.send(Cmd::Send { chat_id, to, text, files, token });
    render_conv(u);
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
    fold_side(u);
    u.viewer.close_now();
    u.st.borrow_mut().sel = None;
    u.selecting.set(true);
    u.chat_list.unselect_all();
    u.selecting.set(false);
    u.new_mode.set(true);
    close_info(u);
    u.info.btn.set_visible(false);
    u.info.refresh.set_visible(false);
    for w in [u.head_avatar.upcast_ref::<gtk::Widget>(), u.head_titles.upcast_ref(), u.scroller.upcast_ref()] {
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
    u.info.btn.set_visible(true);
    u.info.refresh.set_visible(true);
    u.head_titles.set_visible(true);
    u.scroller.set_visible(true);
    u.to_row.set_visible(false);
    u.suggest_scroll.set_visible(false);
    update_spinner(u);
}

/// Open a chat from the new-message view (a suggestion, Enter, a send, a chat just found): the sidebar glides to it.
fn open_chat(u: &Rc<Ui>, id: i64) {
    select_in_list(u, id);
    select(u, id);
    scroll_list_to(u, id);
}

/// What the To field suggests: 1:1 chats first, then address-book contacts without a chat
/// yet, as (chat id if there is one, name, address).
fn suggestions(u: &Ui) -> Vec<(Option<i64>, String, String)> {
    let q = u.to_entry.text().trim().to_lowercase();
    let st = u.st.borrow();
    st.chats
        .iter()
        .filter(|c| c["is_group"].as_bool() != Some(true))
        .filter_map(|c| Some((c["id"].as_i64()?, chat_title(c, &st.names), direct_addr(c)?.to_string())))
        .filter(|(_, name, addr)| q.is_empty() || name.to_lowercase().contains(&q) || addr.to_lowercase().contains(&q) || same_addr(&q, addr))
        .take(8)
        .map(|(id, n, a)| (Some(id), n, a))
        .collect::<Vec<_>>()
        .into_iter()
        .chain(match (&st.contacts, q.is_empty()) {
            (Some(c), false) => c.search(&q), // ponytail: nothing listed until you type; a 360-row list helps nobody
            _ => vec![],
        }
        .into_iter()
        .filter(|(_, a)| !st.chats.iter().any(|c| direct_addr(c).is_some_and(|d| same_addr(a, d))))
        .take(8)
        .map(|(n, a)| (None, n.to_string(), a.to_string())))
        .collect()
}

fn ui_first_suggestion(u: &Ui) -> Option<(Option<i64>, String)> {
    suggestions(u).into_iter().next().map(|s| (s.0, s.2))
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
        row.set_widget_name(&id.map_or_else(|| format!("a:{addr}"), |i| i.to_string()));
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        line.append(&avatar(u, &name, Some(&addr), false));
        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_valign(gtk::Align::Center);
        let n = gtk::Label::new(Some(&name));
        n.add_css_class("chat-name");
        n.set_xalign(0.0);
        let a = gtk::Label::new(Some(&pretty_handle(&addr)));
        a.add_css_class("chat-preview");
        a.set_xalign(0.0);
        text.append(&n);
        if pretty_handle(&addr) != name {
            let line2 = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            line2.append(&a);
            let label = u.st.borrow().contacts.as_ref().and_then(|c| c.label(&addr).map(str::to_string));
            if let Some(l) = label {
                let chip = gtk::Label::new(Some(&l));
                chip.add_css_class("addr-chip");
                chip.set_valign(gtk::Align::Center);
                line2.append(&chip);
            }
            text.append(&line2);
        }
        line.append(&text);
        row.set_child(Some(&line));
        u.suggest.append(&row);
    }
}

/// Send the first message of a new conversation (or to an existing one, if the To field names it).
/// Put `chat` into the list where its last message belongs (newest first).
fn insert_chat(chats: &mut Vec<Value>, chat: Value) {
    let at = chats.iter().position(|c| c["last_message_at"].as_str() < chat["last_message_at"].as_str()).unwrap_or(chats.len());
    chats.insert(at, chat);
}

/// The typed address first, then the other numbers and emails of the contact it belongs to: Messages keeps one
/// conversation per person, whichever of their addresses is used.
fn person_addrs(u: &Ui, typed: &str) -> Vec<String> {
    let mut v = vec![clean_addr(typed)];
    if let Some(p) = u.st.borrow().contacts.as_ref().and_then(|c| c.person(typed)) {
        for a in p.addrs.iter().map(|(a, _)| clean_addr(a)) {
            if is_addr(&a) && !v.contains(&a) {
                v.push(a);
            }
        }
    }
    v
}

/// The newest 1:1 chat in the list with any of these addresses.
fn find_chat_for(chats: &[Value], addrs: &[String]) -> Option<i64> {
    chats
        .iter()
        .filter(|c| c["is_group"].as_bool() != Some(true))
        .filter(|c| direct_addr(c).is_some_and(|d| addrs.iter().any(|a| same_addr(a, d))))
        .max_by_key(|c| c["last_message_at"].as_str().map(String::from))
        .and_then(|c| c["id"].as_i64())
}

/// Glide the sidebar's list so the row of this chat ends up at the top (or as near as the list's end allows). The
/// row is looked up again every frame: the list is rebuilt when previews arrive.
fn scroll_list_to(u: &Rc<Ui>, id: i64) {
    let Some(adj) = side_adjustment(u) else { return };
    u.side_anim.set(true);
    let (u, from, began) = (u.clone(), adj.value(), Cell::new(None::<i64>));
    glib::timeout_add_local_once(Duration::from_millis(150), move || {
        // already in view where the list was scrolled to (list rebuilds since may have reset it): leave it there
        let idx = u.st.borrow().chats.iter().position(|c| c["id"].as_i64() == Some(id));
        if let Some(b) = idx.and_then(|i| u.chat_list.row_at_index(i as i32)).and_then(|r| r.compute_bounds(&u.tiles.parent().unwrap()))
            && b.height() > 1.0
            && b.y() as f64 >= from
            && (b.y() + b.height()) as f64 <= from + adj.page_size()
        {
            adj.set_value(from);
            u.side_anim.set(false);
            return;
        }
        let to = Cell::new(None::<f64>);
        let uu = u.clone();
        u.chat_list.add_tick_callback(move |_, clock| {
            let (u, adj) = (&uu, &adj);
            let idx = u.st.borrow().chats.iter().position(|c| c["id"].as_i64() == Some(id));
            if let Some(b) = idx.and_then(|i| u.chat_list.row_at_index(i as i32)).and_then(|r| r.compute_bounds(&u.tiles.parent().unwrap()))
                && b.height() > 1.0
            {
                to.set(Some((b.y() as f64 - 6.0).clamp(0.0, (adj.upper() - adj.page_size()).max(0.0))));
            }
            let t0 = began.get().unwrap_or_else(|| {
                began.set(Some(clock.frame_time()));
                clock.frame_time()
            });
            let t = ((clock.frame_time() - t0) as f64 / 450_000.0).min(1.0);
            if let Some(to) = to.get() {
                adj.set_value(from + (to - from) * (1.0 - (1.0 - t).powi(3)));
            }
            if t >= 1.0 {
                u.side_anim.set(false);
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    });
}

/// In the new-message view, once a whole address is in the To field: open the conversation we already have with it,
/// or ask the Mac for an older one (after a pause in the typing).
fn find_prior_chat(u: &Rc<Ui>) {
    let g = u.find_gen.get().wrapping_add(1);
    u.find_gen.set(g);
    let typed = u.to_entry.text().trim().to_string();
    if !u.new_mode.get() || !is_addr(&typed) {
        return;
    }
    let u2 = u.clone();
    glib::timeout_add_local_once(Duration::from_millis(500), move || {
        if u2.find_gen.get() != g || !u2.new_mode.get() {
            return;
        }
        let addrs = person_addrs(&u2, &typed);
        let known = find_chat_for(&u2.st.borrow().chats, &addrs);
        if let Some(id) = known {
            open_chat(&u2, id);
            return;
        }
        let key = clean_addr(&typed);
        if u2.st.borrow_mut().found_asked.insert(key) {
            u2.send(Cmd::FindChat(addrs));
        }
    });
}

fn send_new(u: &Rc<Ui>, text: &str) {
    let typed = u.to_entry.text().trim().to_string();
    let existing = {
        let st = u.st.borrow();
        find_chat_for(&st.chats, &person_addrs(u, &typed)).or_else(|| {
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
    let token = u.send_token.get() + 1;
    u.send_token.set(token);
    {
        // ghosts (chat id 0) until the chat appears; Event::Chats gives them its id
        let mut st = u.st.borrow_mut();
        st.new_pending = Some(to.clone());
        for f in &files {
            st.pending.push(Pending { token, chat_id: 0, item: PendingItem::File(f.clone()) });
        }
        if !text.is_empty() {
            st.pending.push(Pending { token, chat_id: 0, item: PendingItem::Text(text.to_string()) });
        }
    }
    u.send(Cmd::Send { chat_id: 0, to: Some(to), text: text.to_string(), files, token });
    show_new_ghosts(u);
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

/// Same as `render_chats`, but a burst of calls (one preview arrives per chat, dozens at start-up) becomes a
/// single rebuild shortly after the last: each rebuild takes ~23 ms of the GTK thread, longer than a frame.
fn render_chats_soon(u: &Rc<Ui>) {
    if u.list_queued.replace(true) {
        return;
    }
    let u = u.clone();
    glib::timeout_add_local_once(Duration::from_millis(120), move || {
        u.list_queued.set(false);
        render_chats(&u);
    });
}

/// What a chat's row shows: title, last message, time, unread.
fn row_info(st: &State, c: &Value) -> (String, String, String, bool) {
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
    (chat_title(c, &st.names), text.unwrap_or_default(), when.unwrap_or_default(), unread)
}

/// A chat's last message arrived: change its row's texts in place. Rebuilding the whole list for each of the
/// ~60 previews that arrive at start-up cost 25+ ms apiece. Anything the row doesn't hold yet (a new chat, a
/// pinned chat's tile) goes through a rebuild instead.
fn update_row(u: &Rc<Ui>, id: i64) {
    let parts = u.row_parts.borrow().get(&id).cloned();
    let info = {
        let st = u.st.borrow();
        st.chats.iter().find(|c| c["id"].as_i64() == Some(id)).map(|c| (row_info(&st, c), pin_key(c)))
    };
    let (Some(parts), Some(((_, text, when, unread), key))) = (parts, info) else { return render_chats_soon(u) };
    if u.st.borrow().pins.contains(&key) {
        return render_chats_soon(u); // its tile shows the unread dot
    }
    parts.prev.set_text(&text);
    parts.time.set_text(&when);
    for (c, on) in [("unread-dot", unread), ("read-dot", !unread)] {
        if on { parts.dot.add_css_class(c) } else { parts.dot.remove_css_class(c) }
    }
    if unread { parts.row.add_css_class("unread") } else { parts.row.remove_css_class("unread") }
}

/// The widgets of a list row that change when its chat's last message does.
#[derive(Clone)]
struct RowParts {
    row: gtk::ListBoxRow,
    time: gtk::Label,
    prev: gtk::Label,
    dot: gtk::Box,
}

fn render_chats(u: &Rc<Ui>) {
    let rows: Vec<(String, String, String, bool, i64, Value)> = {
        let st = u.st.borrow();
        st.chats
            .iter()
            .map(|c| {
                let (title, text, when, unread) = row_info(&st, c);
                (title, text, when, unread, c["id"].as_i64().unwrap_or(0), c.clone())
            })
            .collect()
    };
    u.row_parts.borrow_mut().clear();
    let sel = u.st.borrow().sel;
    // the rebuild empties the list, which would put the sidebar back at the top: remember where it was
    let side_adj = side_adjustment(u);
    let keep = side_adj.as_ref().map_or(0.0, |a| a.value());
    u.selecting.set(true);
    u.chat_list.remove_all();
    let mut new_rows = Vec::new();
    let pins = u.st.borrow().pins.clone();
    let mut pinned: Vec<(usize, i64, String, bool, Value)> = Vec::new();
    for (title, preview, when, unread, id, chat) in rows {
        let key = pin_key(&chat);
        let pin_at = pins.iter().position(|p| *p == key);
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
        texts.add_css_class("chat-texts");
        texts.set_valign(gtk::Align::Center);
        texts.append(&top);
        texts.append(&prev);
        // The avatar is the row's only measured content, so the row can be as narrow as a circle round it;
        // the texts sit beside it at a width of their own and are cropped by the row as the sidebar narrows.
        let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        dot.set_valign(gtk::Align::Center);
        dot.add_css_class("row-dot");
        dot.add_css_class(if unread { "unread-dot" } else { "read-dot" });
        let slot = gtk::Box::new(gtk::Orientation::Horizontal, 11);
        slot.append(&texts);
        slot.append(&dot);
        slot.set_halign(gtk::Align::Start);
        slot.set_valign(gtk::Align::Center);
        slot.set_margin_start(AVATAR + 11);
        slot.set_size_request(u.text_w.get() + 21, -1);
        let avatar = chat_avatar(u, &chat, &title);
        avatar.set_halign(gtk::Align::Start);
        avatar.set_valign(gtk::Align::Center);
        let row_box = gtk::Overlay::new();
        row_box.set_child(Some(&avatar));
        row_box.add_overlay(&slot);
        row_box.set_clip_overlay(&slot, true);
        let row = gtk::ListBoxRow::new();
        if u.collapsed.get() {
            row.set_tooltip_text(Some(&title));
        }
        new_rows.push((row.clone(), slot, title.clone()));
        u.row_parts.borrow_mut().insert(id, RowParts { row: row.clone(), time: time.clone(), prev: prev.clone(), dot: dot.clone() });
        row.add_css_class("chat-row");
        if unread {
            row.add_css_class("unread");
        }
        row.set_child(Some(&row_box));
        pin_menu(u, row.upcast_ref(), &key, pin_at.is_some());
        if let Some(i) = pin_at {
            // hidden, not left out: row indexes must keep matching `State.chats`
            row.set_visible(false);
            pinned.push((i, id, title.clone(), unread, chat.clone()));
        }
        u.chat_list.append(&row);
        if Some(id) == sel {
            u.chat_list.select_row(Some(&row));
        }
    }
    *u.rows.borrow_mut() = new_rows;
    u.selecting.set(false);
    pinned.sort_by_key(|p| p.0);
    render_tiles(u, pinned, sel);
    if let (Some(adj), true) = (side_adj, keep > 0.0 && !u.side_anim.get()) {
        // the new rows have no size until the next layout: set the value again each frame until it holds
        let (n, u) = (Cell::new(0), u.clone());
        let list = u.chat_list.clone();
        list.add_tick_callback(move |_, _| {
            n.set(n.get() + 1);
            if !u.side_anim.get() {
                adj.set_value(keep);
            }
            if n.get() < 10 && adj.upper() - adj.page_size() < keep - 0.5 { glib::ControlFlow::Continue } else { glib::ControlFlow::Break }
        });
    }
}

/// A chat's stable identity for pinning (its row id changes between runs of the Mac's database).
fn pin_key(c: &Value) -> String {
    c["identifier"].as_str().or(c["guid"].as_str()).map(String::from).unwrap_or_else(|| {
        c["participants"].as_array().into_iter().flatten().filter_map(|p| p.as_str()).collect::<Vec<_>>().join(",")
    })
}

/// Right-click menu on a row or tile: Pin / Unpin.
fn pin_menu(u: &Rc<Ui>, w: &gtk::Widget, key: &str, pinned: bool) {
    let g = gtk::GestureClick::new();
    g.set_button(3);
    let (u, w2, key) = (u.clone(), w.clone(), key.to_string());
    g.connect_pressed(move |g, _, x, y| {
        g.set_state(gtk::EventSequenceState::Claimed);
        let b = gtk::Button::with_label(if pinned { "Unpin" } else { "Pin to top" });
        b.add_css_class("flat");
        let pop = gtk::Popover::new();
        pop.set_child(Some(&b));
        pop.set_has_arrow(false);
        pop.set_parent(&w2);
        pop.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        let (u, key, p2) = (u.clone(), key.clone(), pop.clone());
        b.connect_clicked(move |_| {
            p2.popdown();
            toggle_pin(&u, &key);
        });
        pop.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        pop.popup();
    });
    w.add_controller(g);
}

fn toggle_pin(u: &Rc<Ui>, key: &str) {
    {
        let mut st = u.st.borrow_mut();
        match st.pins.iter().position(|p| p == key) {
            Some(i) => {
                st.pins.remove(i);
            }
            None => st.pins.push(key.to_string()),
        }
        let _ = std::fs::create_dir_all(u.store.pins_file.parent().unwrap());
        let _ = std::fs::write(&u.store.pins_file, st.pins.iter().map(|p| format!("{p}\n")).collect::<String>());
    }
    render_chats(u);
}

/// The pinned chats as avatar tiles above the list (names hide when the sidebar is avatars-only).
fn render_tiles(u: &Rc<Ui>, pinned: Vec<(usize, i64, String, bool, Value)>, sel: Option<i64>) {
    u.tiles.remove_all();
    u.tiles.set_visible(!pinned.is_empty());
    for (_, id, title, unread, chat) in pinned {
        let av = chat_avatar(u, &chat, &title);
        av.set_halign(gtk::Align::Center);
        let holder = gtk::Overlay::new();
        holder.set_halign(gtk::Align::Center);
        holder.set_child(Some(&av));
        if unread {
            let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            dot.add_css_class("unread-dot");
            dot.set_halign(gtk::Align::Start);
            dot.set_valign(gtk::Align::Start);
            holder.add_overlay(&dot);
        }
        let name = gtk::Label::new(Some(&title));
        name.add_css_class("tile-name");
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        name.set_width_chars(1);
        name.set_max_width_chars(9);
        name.set_visible(!u.collapsed.get());
        let tile = gtk::Box::new(gtk::Orientation::Vertical, 4);
        tile.add_css_class("tile");
        tile.set_widget_name(&id.to_string());
        tile.set_hexpand(true);
        tile.set_tooltip_text(Some(&title));
        tile.append(&holder);
        tile.append(&name);
        let key = pin_key(&chat);
        if sel == Some(id) {
            tile.add_css_class("selected");
        }
        let g = gtk::GestureClick::new();
        let u2 = u.clone();
        g.connect_released(move |_, _, _, _| {
            select(&u2, id);
            u2.compose.grab_focus();
        });
        tile.add_controller(g);
        pin_menu(u, tile.upcast_ref(), &key, true);
        u.tiles.append(&tile);
        if let Some(f) = tile.parent() {
            f.set_focusable(false); // no focus box round a clicked tile
        }
    }
}

fn side_adjustment(u: &Ui) -> Option<gtk::Adjustment> {
    u.chat_list.ancestor(gtk::ScrolledWindow::static_type()).and_downcast::<gtk::ScrolledWindow>().map(|sw| sw.vadjustment())
}

// ---------- conversation ----------

const FIRST_MIN: usize = 3; // newest messages drawn at once: at least this many, until the window is full
const FIRST_MAX: usize = 40; // and never more than this

/// What drawing one message needs to know about the whole conversation.
struct ConvCtx {
    msgs: Vec<Value>,
    times: Vec<Option<i64>>,
    names: HashMap<String, String>,
    group: bool,
    chat_svc: String,
    /// my messages still on their way follow the last one
    pending_tail: bool,
}

/// Draw message `i` at the front of the message list (after `base`), its photos at the front of the photo list.
/// Returns about how tall it is, for deciding when the window is full.
fn put_front(u: &Rc<Ui>, c: &ConvCtx, i: usize, base: &Option<gtk::Widget>, width: i32, measure: bool) -> f64 {
    let before = u.st.borrow().photos.len();
    let (mut h, mut prev) = (0.0, base.clone());
    for w in conv_widgets(u, c, i) {
        if measure {
            h += w.measure(gtk::Orientation::Vertical, width).1 as f64 + 8.0;
        }
        u.msgs.insert_child_after(&w, prev.as_ref());
        prev = Some(w);
    }
    // conv_widgets pushed this message's photos at the end of the list; it is older than all built so far
    let mut st = u.st.borrow_mut();
    let fresh: Vec<_> = st.photos.drain(before..).collect();
    st.photos.splice(0..0, fresh);
    h
}

/// The widgets of message `i` (time separator, sender name, one line per part), top to bottom.
fn conv_widgets(u: &Rc<Ui>, c: &ConvCtx, i: usize) -> Vec<gtk::Widget> {
    let (msgs, times, names, group) = (&c.msgs, &c.times, &c.names, c.group);
    let mut out: Vec<gtk::Widget> = Vec::new();
    let m = &msgs[i];
    if m["is_reaction"].as_bool() == Some(true) {
        return out;
    }
    let who = |m: &Value| (m["is_from_me"].as_bool() == Some(true), m["sender"].as_str().unwrap_or("").to_string());
    let me = m["is_from_me"].as_bool() == Some(true);
    let gap = |a: usize, b: usize| match (times[a], times[b]) {
        (Some(x), Some(y)) => (y - x).abs(),
        _ => 0,
    };
    let sep = i == 0 || gap(i - 1, i) > 30 * 60 || day(times[i - 1]) != day(times[i]);
    if sep && let Some(dt) = m["created_at"].as_str().and_then(local) {
        let l = gtk::Label::new(Some(&sep_time(&dt)));
        l.add_css_class("time-sep");
        out.push(l.upcast());
    }
    // a "run" = same sender, close together: shares one name label and one avatar
    let run_prev = !sep && i > 0 && who(&msgs[i - 1]) == who(m) && gap(i - 1, i) < 180;
    let run_next = i + 1 < msgs.len() && who(&msgs[i + 1]) == who(m) && gap(i, i + 1) < 180 && day(times[i]) == day(times[i + 1]);
    // my messages still on their way continue my last run: no tail on the one above them
    let run_next = run_next || (i + 1 == msgs.len() && me && c.pending_tail);
    if group && !me && !run_prev {
        let n = gtk::Label::new(Some(&sender_name(m, names)));
        n.add_css_class("sender-name");
        n.set_xalign(0.0);
        out.push(n.upcast());
    }
    let svc = m["service"].as_str().unwrap_or(&c.chat_svc);
    let parts = message_parts(u, m, me, svc != "iMessage", !run_next);
    let n = parts.len();
    for (k, part) in parts.into_iter().enumerate() {
        let last = k + 1 == n;
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        line.add_css_class("msg-line");
        if !last || run_next {
            line.add_css_class("tight"); // same sender close together: a smaller gap
        }
        line.set_halign(if me { gtk::Align::End } else { gtk::Align::Start });
        if group && !me {
            if last && !run_next {
                let a = avatar(u, &sender_name(m, names), m["sender"].as_str(), true);
                a.set_valign(gtk::Align::End);
                line.append(&a);
            } else {
                let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                spacer.add_css_class("avatar-spacer");
                line.append(&spacer);
            }
        }
        line.append(&part);
        out.push(line.upcast());
    }
    out
}

fn set_head_avatar(u: &Rc<Ui>, chat: &Value, title: &str) {
    while let Some(c) = u.head_avatar.first_child() {
        u.head_avatar.remove(&c);
    }
    u.head_avatar.append(&header_avatar(u, chat, title));
}

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
            .map(|p| names.get(p).cloned().unwrap_or_else(|| pretty_handle(p)))
            .collect();
        sub = format!("{sub} · {}", people.join(", "));
    } else if let Some(h) = chat["participants"][0].as_str()
        && pretty_handle(h) != title
    {
        sub = format!("{sub} · {}", pretty_handle(h));
    }
    u.head_sub.set_text(&sub);
    set_head_avatar(u, &chat, &title);
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
    let pending: Vec<Pending> = {
        let st = u.st.borrow();
        st.pending.iter().filter(|p| Some(p.chat_id) == st.sel).cloned().collect()
    };
    let times: Vec<Option<i64>> = msgs.iter().map(|m| m["created_at"].as_str().and_then(local).map(|d| d.to_unix())).collect();
    let ctx = Rc::new(ConvCtx { msgs, times, names, group, chat_svc, pending_tail: !pending.is_empty() });
    // Built newest first, each message put in at the front. At the bottom of the chat (opening it, a new message)
    // that is done in two steps: the messages that fill the window now, all the rest in one go as soon as that
    // has been drawn; they land above the top edge of the view, which is anchored to the bottom, so nothing moves.
    // Away from the bottom (older messages just arrived while scrolling up) everything is built at once, or the
    // view would lose its place while only the newest part existed.
    let stamp = u.conv_gen.get().wrapping_add(1);
    u.conv_gen.set(stamp);
    u.staging.set(false);
    let staged = u.stick_bottom.get();
    // where messages go in: after the "loading more" row when there is one
    let base: Option<gtk::Widget> = if older { u.msgs.first_child() } else { None };
    let width = match u.scroller.hadjustment().page_size() as i32 {
        w if w > 100 => w,
        _ => 600,
    };
    let want = match u.scroller.vadjustment().page_size() {
        h if h > 100.0 => h,
        _ => 700.0,
    } * 1.1;
    let (mut split, mut filled) = (ctx.msgs.len(), 0.0);
    while split > 0 && (!staged || ((filled < want || ctx.msgs.len() - split < FIRST_MIN) && ctx.msgs.len() - split < FIRST_MAX)) {
        split -= 1;
        filled += put_front(u, &ctx, split, &base, width, staged);
    }
    if split > 0 {
        // no loading of older messages until the rest is in: with only the window's worth built, the view is "at
        // the top" and would ask for more again and again
        u.staging.set(true);
        let (u2, ctx2) = (u.clone(), ctx.clone());
        glib::idle_add_local_once(move || {
            if u2.conv_gen.get() != stamp {
                return; // the conversation was drawn again since
            }
            for i in (0..split).rev() {
                put_front(&u2, &ctx2, i, &base, width, false);
            }
            u2.staging.set(false);
            let photos = u2.st.borrow().photos.clone();
            if !(u2.st.borrow().viewer_panel && u2.viewer.is_open()) {
                u2.viewer.refresh(photos);
            }
        });
    }
    append_pending(u, &pending, sms);
    let photos = u.st.borrow().photos.clone(); // the viewer's hooks borrow State
    if !(u.st.borrow().viewer_panel && u.viewer.is_open()) {
        u.st.borrow_mut().viewer_panel = false;
        u.viewer.refresh(photos);
    }
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

/// The faded, spinning bubbles of messages still on their way, at the end of the message list.
fn append_pending(u: &Ui, pending: &[Pending], sms: bool) {
    for (i, p) in pending.iter().enumerate() {
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        line.add_css_class("msg-line");
        if i + 1 < pending.len() {
            line.add_css_class("tight");
        }
        line.set_halign(gtk::Align::End);
        let spin = gtk::Spinner::new();
        spin.start();
        spin.set_valign(gtk::Align::Center);
        spin.set_tooltip_text(Some("Sending…"));
        line.append(&spin);
        line.append(&pending_bubble(p, sms, i + 1 == pending.len()));
        u.msgs.append(&line);
    }
}

/// In the new-message view: the first message to a new address, shown as a ghost until its chat
/// appears (pending items with chat id 0); with none, the people suggestions.
fn show_new_ghosts(u: &Ui) {
    if !u.new_mode.get() {
        return;
    }
    let ghosts: Vec<Pending> = u.st.borrow().pending.iter().filter(|p| p.chat_id == 0).cloned().collect();
    while let Some(c) = u.msgs.first_child() {
        u.msgs.remove(&c);
    }
    append_pending(u, &ghosts, false);
    u.scroller.set_visible(!ghosts.is_empty());
    u.suggest_scroll.set_visible(ghosts.is_empty());
}

/// A message on its way: faded, drawn like the real one will be (blue or green, tail on the last).
fn pending_bubble(p: &Pending, sms: bool, last: bool) -> gtk::Widget {
    let look = bubble::Look { me: true, sms, tail: last };
    let w: gtk::Widget = match &p.item {
        PendingItem::File(f) if is_image("", &f.to_string_lossy()) && gdk::Texture::from_filename(f).is_ok() => {
            let t = gdk::Texture::from_filename(f).expect("checked");
            let b = bubble::Bubble::photo(look, 160, 120);
            show_texture(&b, &t);
            b.upcast()
        }
        item => {
            let b = bubble::Bubble::new(look);
            let text = match item {
                PendingItem::Text(t) => t.clone(),
                PendingItem::File(f) => format!("📎 {}", f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
            };
            let l = gtk::Label::new(Some(&text));
            l.set_wrap(true);
            l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            l.set_xalign(0.0);
            l.set_max_width_chars(60);
            l.add_css_class("text");
            b.add(&l);
            b.upcast()
        }
    };
    w.add_css_class("pending");
    w
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

/// One message as display rows: its photos (as cards, outside any bubble), then a bubble with the
/// text and any other attachments. Only the bubble of the last message in a run gets a tail.
/// `end_of_run`: this message is the sender's last in the run, so its last item (bubble or photo)
/// carries the tail. Nothing else does.
fn message_parts(u: &Rc<Ui>, m: &Value, me: bool, sms: bool, end_of_run: bool) -> Vec<gtk::Widget> {
    let look = bubble::Look { me, sms, tail: end_of_run };
    if m["unsent_at"].as_str().is_some() || m["type"].as_str() == Some("unsent") {
        let b = bubble::Bubble::new(look);
        let l = gtk::Label::new(Some("Message unsent"));
        l.add_css_class("unsent");
        b.add(&l);
        return vec![b.upcast()];
    }
    let atts: Vec<&Value> = m["attachments"].as_array().into_iter().flatten().collect();
    // RCS/SMS photos often come as a converted copy plus a missing original; show only what's there
    let any_present = atts.iter().any(|a| a["missing"].as_bool() != Some(true));
    let photos = photo_paths(m);
    let mut others = Vec::new(); // a photo that isn't on the Mac
    let mut files: Vec<FileRef> = Vec::new();
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
            let missing = a["missing"].as_bool() == Some(true);
            if missing && is_image(mime, name) {
                let l = gtk::Label::new(None);
                l.set_markup(&format!("<span size='170%'>📷</span>  {}", glib::markup_escape_text("Photo not downloaded on your Mac")));
                l.add_css_class("file");
                l.set_xalign(0.0);
                others.push(l);
            } else {
                let size = a["total_bytes"].as_i64().unwrap_or(0);
                files.push(FileRef { path: path.to_string(), name: name.to_string(), kind: file_kind(mime, name), size, missing });
            }
        }
    }
    let mut out: Vec<gtk::Widget> = Vec::new();
    let link = preview_row(m).and_then(|r| link_for(u, r));
    let mut text = clean_text(m);
    // a link Messages made no preview of: like Messages, one that ends the message becomes a small
    // card under the text; with several links, or one in the middle, the text stays as it is (clickable)
    let mut plain: Option<String> = None;
    if let Some(l) = &link {
        text = without_url(&text, l); // the card shows the link; any other words follow it
    } else if preview_row(m).is_none()
        && let Some(url) = crate::mac::single_link(&text)
    {
        text = crate::mac::strip_url(&text, &url);
        plain = Some(url);
    }
    let has_bubble = !text.is_empty() || !others.is_empty();
    let look = bubble::Look { tail: end_of_run && plain.is_none(), ..look };
    if !photos.is_empty() {
        let tail = end_of_run && !has_bubble && link.is_none() && plain.is_none() && files.is_empty();
        out.push(photo_stack(u, &photos, bubble::Look { me, sms, tail }));
    }
    for (i, f) in files.iter().enumerate() {
        let look = bubble::Look { me, sms, tail: end_of_run && !has_bubble && link.is_none() && plain.is_none() && i + 1 == files.len() };
        // a video on the Mac is a big first-frame picture like a photo; anything else (or no frame) is a card
        let framed = f.kind == FileKind::Video && !f.missing && !f.path.is_empty() && !u.st.borrow().bad_images.contains(&f.path);
        out.push(if framed { video_preview(u, f, look) } else { file_card(u, f, look) });
    }
    if let Some(l) = &link {
        out.push(link_card(u, l, bubble::Look { me, sms, tail: end_of_run && !has_bubble }));
    }
    if has_bubble {
        let b = bubble::Bubble::new(look);
        for l in others {
            b.add(&l);
        }
        if !text.is_empty() {
            let l = gtk::Label::new(None);
            l.set_markup(&linkify(&text));
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
    if let Some(url) = plain {
        out.push(plain_link_card(u, &url, bubble::Look { me, sms, tail: end_of_run }));
    }
    out
}

/// Our own stand-in for a link's missing picture: a white chain on a solid slate circle, which reads
/// on the off-white card (Messages shows a Safari compass there).
fn link_glyph() -> gtk::Widget {
    glyph("bf-link-symbolic")
}

/// A solid slate circle with one of our icons in white (links without a picture, files without a preview).
fn glyph(icon: &str) -> gtk::Widget {
    let g = gtk::Box::new(gtk::Orientation::Vertical, 0);
    g.add_css_class("link-glyph");
    g.set_size_request(44, 44);
    g.set_valign(gtk::Align::Center);
    g.set_hexpand(false);
    let icon = gtk::Image::from_icon_name(icon);
    icon.set_pixel_size(24);
    icon.set_halign(gtk::Align::Center);
    icon.set_valign(gtk::Align::Center);
    icon.set_vexpand(true);
    g.append(&icon);
    g.upcast()
}

/// A link with no preview from Messages, drawn like a card whose picture is just a small icon: the
/// host as its title and a link glyph on the right.
fn plain_link_card(u: &Rc<Ui>, url: &str, look: bubble::Look) -> gtk::Widget {
    let b = bubble::Bubble::card(look, LINK_W, 0);
    let host = url.split('/').nth(2).unwrap_or(url).trim_start_matches("www.").to_string();
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let title = gtk::Label::new(Some(&host));
    title.add_css_class("link-title");
    title.set_xalign(0.0);
    title.set_hexpand(true);
    title.set_valign(gtk::Align::Center);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    row.append(&title);
    row.append(&link_glyph());
    b.add(&row);
    b.set_tooltip_text(Some(url));
    b.set_cursor_from_name(Some("pointer"));
    let key = chat_youtube(u, url, b.upcast_ref());
    let (u2, url, b2) = (u.clone(), url.to_string(), b.clone().upcast::<gtk::Widget>());
    let click = gtk::GestureClick::new();
    click.connect_released(move |g, _, _, _| {
        g.set_state(gtk::EventSequenceState::Claimed);
        open_link(&u2, &url, &b2, key.as_deref());
    });
    b.add_controller(click);
    b.upcast()
}

/// Pango markup for message text with each `http(s)://` link clickable (the label opens it itself).
fn linkify(text: &str) -> String {
    let mut out = String::new();
    for word in text.split_inclusive(char::is_whitespace) {
        let w = word.trim_end();
        if let Some(p) = crate::mac::scheme_at(w) {
            let core = w[p..].trim_end_matches(['.', ',', ')', '>', '"', '\'', '!', '?', ';']);
            let esc = glib::markup_escape_text(core);
            out.push_str(&format!(
                "{}<a href=\"{esc}\">{esc}</a>{}{}",
                glib::markup_escape_text(&w[..p]),
                glib::markup_escape_text(&w[p + core.len()..]),
                &word[w.len()..]
            ));
        } else {
            out.push_str(&glib::markup_escape_text(word));
        }
    }
    out
}

/// `text` with the link removed (it's shown as the preview card instead). The text can hold a
/// different form of the address than the preview saved (tracking parameters, a redirect), so any
/// link to the same site goes too.
fn without_url(text: &str, l: &crate::mac::LinkPreview) -> String {
    let host = |u: &str| u.split('/').nth(2).unwrap_or("").trim_start_matches("www.").to_lowercase();
    let want = host(&l.url);
    let mut out = String::new();
    for word in text.split_inclusive(char::is_whitespace) {
        let w = word.trim();
        let is_link = w.starts_with("http") && !want.is_empty() && host(w) == want;
        if !is_link {
            out.push_str(word);
        }
    }
    let t = out.trim();
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
    link_card_w(u, l, look, LINK_W)
}

/// The preview without the pictures that couldn't be fetched from the Mac.
fn usable(u: &Ui, l: &crate::mac::LinkPreview) -> crate::mac::LinkPreview {
    let st = u.st.borrow();
    let mut l = l.clone();
    if l.image.as_ref().is_some_and(|p| st.bad_images.contains(p)) {
        (l.image, l.image_size) = (None, None);
    }
    if l.icon.as_ref().is_some_and(|p| st.bad_images.contains(p)) {
        (l.icon, l.icon_size) = (None, None);
    }
    l
}

/// The small picture for a preview without a big one: the larger of the preview's own picture and the
/// site's icon. A very wide or tall picture (a button graphic: 87x23 on thegentlechef.com) loses to the icon.
fn small_picture(l: &crate::mac::LinkPreview) -> Option<&String> {
    let area = |s: Option<(i32, i32)>| s.map_or(0, |(w, h)| i64::from(w) * i64::from(h));
    let odd = |s: Option<(i32, i32)>| s.is_some_and(|(w, h)| w > 0 && h > 0 && (w > 3 * h || h > 3 * w));
    match (&l.image, &l.icon) {
        (Some(i), Some(c)) => Some(if odd(l.image_size) || area(l.icon_size) > area(l.image_size) { c } else { i }),
        (i, c) => i.as_ref().or(c.as_ref()),
    }
}

/// `link_card` at a given width (the info panel's cards follow the panel).
fn link_card_w(u: &Rc<Ui>, l: &crate::mac::LinkPreview, look: bubble::Look, link_w: i32) -> gtk::Widget {
    let l = &usable(u, l);
    // a real preview picture goes across the top; a small one (the site's logo) sits beside the text
    let big = l.image.is_some() && l.image_size.is_none_or(|(w, h)| crate::mac::is_preview_size(w, h));
    let small = if big { None } else { small_picture(l) };
    // the preview shows whole, in its own proportions (portrait ones too); only a freak shape is capped
    let img_h = match l.image_size {
        _ if !big => 0,
        Some((w, h)) if w > 0 => (link_w as f64 * h as f64 / w as f64).clamp(60.0, 2.0 * link_w as f64) as i32,
        _ => link_w * 52 / 100,
    };
    let b = bubble::Bubble::card(look, link_w, img_h);
    if big && let Some(img) = &l.image {
        b.add_css_class("fixed"); // keeps its card size instead of taking the photo's shape
        load_texture(u, &b, img);
    }
    let texts = gtk::Box::new(gtk::Orientation::Vertical, 2);
    texts.set_hexpand(true);
    texts.set_valign(gtk::Align::Center);
    if !l.title.is_empty() {
        let t = gtk::Label::new(Some(&clip_middle(&l.title, if big { 70 } else { 48 })));
        t.add_css_class("link-title");
        t.set_xalign(0.0);
        t.set_wrap(true);
        t.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        t.set_max_width_chars(if big { 34 } else { 24 });
        texts.append(&t);
    }
    // ponytail: the site's icon (l.icon) isn't drawn; a link glyph stands in
    let site = gtk::Label::new(Some(&format!("🔗 {}", if l.site.is_empty() { &l.url } else { &l.site })));
    site.add_css_class("link-site");
    site.set_xalign(0.0);
    site.set_ellipsize(gtk::pango::EllipsizeMode::End);
    site.set_max_width_chars(if big { 34 } else { 24 });
    texts.append(&site);
    if let Some(img) = small {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.append(&texts);
        let icon = bubble::Bubble::thumb(56);
        icon.add_css_class("fixed");
        load_texture(u, &icon, img);
        row.append(&icon);
        b.add(&row);
    } else if !big {
        // Messages has no picture for it: our own link icon stands in
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.append(&texts);
        row.append(&link_glyph());
        b.add(&row);
    } else {
        b.add(&texts);
    }
    b.set_tooltip_text(Some(&l.url));
    b.set_cursor_from_name(Some("pointer"));
    let click = gtk::GestureClick::new();
    let key = chat_youtube(u, &l.url, b.upcast_ref());
    let (u2, url, b2) = (u.clone(), l.url.clone(), b.clone().upcast::<gtk::Widget>());
    click.connect_released(move |g, _, _, _| {
        g.set_state(gtk::EventSequenceState::Claimed);
        open_link(&u2, &url, &b2, key.as_deref());
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
    let (u2, path2, p2) = (u.clone(), path.to_string(), p.clone().upcast::<gtk::Widget>());
    click.connect_released(move |g, _, _, _| {
        g.set_state(gtk::EventSequenceState::Claimed);
        open_media(&u2, &path2, &p2);
    });
    p.add_controller(click);
    u.st.borrow_mut().photos.push((path.to_string(), p.clone().upcast()));
    load_texture(u, &p, path);
    p
}

/// Give `p` the chat copy of the image at `path`: from memory, the disk cache, or the Mac.
fn load_texture(u: &Rc<Ui>, p: &bubble::Bubble, path: &str) {
    load_texture_in(u, p, path, false)
}

/// `bulk`: one of many at once (the info panel's grid), fetched through the Mac's slower lane.
fn load_texture_in(u: &Rc<Ui>, p: &bubble::Bubble, path: &str, bulk: bool) {
    let in_memory = u.st.borrow().textures.get(path).cloned(); // borrow ends here
    let file = cache_path(u, path, false);
    let cached = match in_memory {
        Some(t) => Some(t),
        None if file.exists() => {
            // on disk: decoded in the background, the bubble stays as it is (no spinner flash) until it is ready
            let first = {
                let mut st = u.st.borrow_mut();
                let w = st.waiting.entry(path.to_string()).or_default();
                w.push(p.clone());
                w.len() == 1
            };
            if first {
                let (u2, path2, f2, f3) = (u.clone(), path.to_string(), file.clone(), file.clone());
                bg(move || decode_file(&f3), move |tex| match tex {
                    Some(_) => image_done(&u2, path2, false, tex),
                    None => {
                        // unreadable copy: forget it and fetch it again
                        let _ = std::fs::remove_file(&f2);
                        let pics = u2.st.borrow_mut().waiting.remove(&path2).unwrap_or_default();
                        for p in pics {
                            load_texture_in(&u2, &p, &path2, bulk);
                        }
                    }
                });
            }
            return;
        }
        None => None,
    };
    match cached {
        Some(t) => show_texture(p, &t),
        None => {
            // a spinner in the empty bubble until the picture arrives
            let sp = gtk::Spinner::new();
            sp.set_spinning(true);
            sp.set_halign(gtk::Align::Center);
            sp.set_valign(gtk::Align::Center);
            sp.set_hexpand(true);
            sp.set_size_request(28, 28);
            if p.has_css_class("linkcard") {
                sp.set_vexpand(true);
                sp.set_margin_bottom(12); // the slot sits a little low; this centres it on the picture
            }
            spin_host(p).append(&sp);
            let first = {
                let mut st = u.st.borrow_mut();
                let w = st.waiting.entry(path.to_string()).or_default();
                w.push(p.clone());
                w.len() == 1
            };
            if first {
                let was_empty = {
                    let mut st = u.st.borrow_mut();
                    st.fetch_q.push((path.to_string(), bulk));
                    st.fetch_q.len() == 1
                };
                if was_empty {
                    let u = u.clone();
                    glib::idle_add_local_once(move || {
                        let q = std::mem::take(&mut u.st.borrow_mut().fetch_q);
                        // the chat is built oldest to newest, so its pictures go newest first;
                        // the info panel's grid is built newest first already
                        let (bulk, chat): (Vec<_>, Vec<_>) = q.into_iter().partition(|(_, b)| *b);
                        for (path, _) in chat.into_iter().rev() {
                            u.send(Cmd::Image { path, full: false });
                        }
                        for (path, _) in bulk {
                            u.send(Cmd::Bulk(path));
                        }
                    });
                }
            }
        }
    }
}

type Job = Box<dyn FnOnce() + Send>;

/// Run `work` on one of two background threads and hand its result to `done` on the GTK thread. Jobs run in the
/// order they were queued (a chat is built newest first, so the pictures you can see come first). Widgets stay on
/// the GTK thread; reading files and decoding pictures don't need to.
fn bg<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static, done: impl FnOnce(T) + 'static) {
    use std::sync::{Arc, Mutex, OnceLock, mpsc};
    static POOL: OnceLock<mpsc::Sender<Job>> = OnceLock::new();
    let tx = POOL.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        for _ in 0..2 {
            let rx = rx.clone();
            std::thread::spawn(move || {
                loop {
                    let job = rx.lock().unwrap().recv(); // the lock is released before the job runs
                    match job {
                        Ok(job) => job(),
                        Err(_) => break,
                    }
                }
            });
        }
        tx
    });
    let (rtx, rrx) = tokio::sync::oneshot::channel();
    let _ = tx.send(Box::new(move || {
        let _ = rtx.send(work());
    }));
    glib::spawn_future_local(async move {
        if let Ok(v) = rrx.await {
            done(v);
        }
    });
}

/// A picture file as a texture. (Scaling while decoding through gdk-pixbuf was tried: it made the GTK thread
/// stall for ~20 ms at a time while it ran in the background, so the Mac sends smaller copies instead.)
fn decode_file(path: &std::path::Path) -> Option<gdk::Texture> {
    gdk::Texture::from_filename(path).ok()
}

fn texture(bytes: &[u8]) -> Option<gdk::Texture> {
    // the Mac already turned the photo upright (see mac::fetch_image)
    gdk::Texture::from_bytes(&glib::Bytes::from(bytes)).ok()
}

/// Remove the loading spinner `load_texture` put in a photo bubble.
fn stop_spin(p: &bubble::Bubble) {
    let host = spin_host(p);
    if let Some(sp) = host.first_child().and_downcast::<gtk::Spinner>() {
        host.remove(&sp);
    }
}

/// Where a bubble's loading spinner lives: a link card has a slot over its picture; others hold it directly.
fn spin_host(p: &bubble::Bubble) -> gtk::Box {
    if p.has_css_class("linkcard")
        && let Some(slot) = p.first_child().and_then(|inner| inner.first_child()).and_downcast::<gtk::Box>()
    {
        return slot;
    }
    p.clone().upcast()
}

fn show_texture(p: &bubble::Bubble, t: &gdk::Texture) {
    stop_spin(p);
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
    if !u.st.borrow_mut().waiting_full.insert(path.to_string()) {
        return; // already on its way
    }
    let file = cache_path(u, path, true);
    if file.exists() {
        let (u2, path2) = (u.clone(), path.to_string());
        bg(move || decode_file(&file), move |tex| match tex {
            Some(_) => image_done(&u2, path2, true, tex),
            None => {
                let _ = std::fs::remove_file(cache_path(&u2, &path2, true));
                u2.st.borrow_mut().waiting_full.remove(&path2);
                fetch_full(&u2, &path2);
            }
        });
    } else {
        u.send(Cmd::Image { path: path.to_string(), full: true });
    }
}

/// Width a photo stack may use in the open chat.
fn stack_room(u: &Ui) -> f64 {
    // ponytail: mirrors .msgs padding (18 px a side) and the avatar column (28 + 4) in style.css
    let w = u.scroller.hadjustment().page_size() - 36.0;
    if u.group.get() { w - 32.0 } else { w }
}

// ---------- conversation info ----------

#[derive(Clone)]
struct Info {
    revealer: gtk::Revealer,
    /// the panel itself (its width is set by `relayout`)
    panel: gtk::Box,
    btn: gtk::Button,
    refresh: gtk::Button,
    close: gtk::Button,
    /// the three tabs side by side, when there's room for them...
    switcher: gtk::Box,
    /// ...else the current tab's name with carets to move between them
    carets: gtk::Box,
    photos: gtk::FlowBox,
    videos: gtk::FlowBox,
    files: gtk::FlowBox,
    links: gtk::FlowBox,
    /// "Show more" under photos, videos, files, links
    more: [gtk::Button; 4],
}

fn kind_idx(k: Kind) -> usize {
    match k {
        Kind::Photos => 0,
        Kind::Videos => 1,
        Kind::Files => 2,
        Kind::Links => 3,
    }
}

/// The panel that slides in over the right side of the conversation: who's in it, and tabs for every
/// photo, file and link in the chat.
fn build_info() -> Info {
    let btn = gtk::Button::from_icon_name("bf-info-symbolic");
    btn.add_css_class("head-btn");
    btn.set_valign(gtk::Align::Center);
    btn.set_tooltip_text(Some("Conversation info"));
    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh.add_css_class("head-btn");
    refresh.set_valign(gtk::Align::Center);
    refresh.set_tooltip_text(Some("Reload this conversation from the Mac"));
    let close = gtk::Button::from_icon_name("window-close-symbolic");
    close.add_css_class("head-btn");
    close.set_valign(gtk::Align::Center);
    let photos = gtk::FlowBox::new();
    photos.set_selection_mode(gtk::SelectionMode::None);
    photos.set_min_children_per_line(1);
    photos.set_max_children_per_line(4); // the panel's width decides how many fit (see relayout)
    photos.set_row_spacing(4);
    photos.set_column_spacing(4);
    photos.set_homogeneous(true);
    let videos = gtk::FlowBox::new();
    videos.set_selection_mode(gtk::SelectionMode::None);
    videos.set_min_children_per_line(1);
    videos.set_max_children_per_line(3);
    videos.set_row_spacing(4);
    videos.set_column_spacing(4);
    videos.set_homogeneous(true);
    let files = gtk::FlowBox::new();
    files.set_selection_mode(gtk::SelectionMode::None);
    files.set_min_children_per_line(1);
    files.set_max_children_per_line(3);
    files.set_row_spacing(4);
    files.set_column_spacing(4);
    files.set_homogeneous(true);
    let links = gtk::FlowBox::new();
    links.set_selection_mode(gtk::SelectionMode::None);
    links.set_min_children_per_line(1);
    links.set_max_children_per_line(3);
    links.set_row_spacing(4);
    links.set_column_spacing(4);
    links.set_homogeneous(true);
    let more = [(); 4].map(|_| gtk::Button::with_label("Show more"));
    for m in &more {
        m.add_css_class("flat");
        m.set_visible(false);
    }
    let tab = |child: &gtk::Widget, more: &gtk::Button| {
        let b = gtk::Box::new(gtk::Orientation::Vertical, 8);
        b.append(child);
        b.append(more);
        b
    };
    let stack = gtk::Stack::new();
    stack.set_vhomogeneous(false); // else a short tab scrolls as far as the tallest one
    stack.add_titled(&tab(photos.upcast_ref(), &more[0]), Some("photos"), "Photos");
    stack.add_titled(&tab(videos.upcast_ref(), &more[1]), Some("videos"), "Videos");
    stack.add_titled(&tab(files.upcast_ref(), &more[2]), Some("files"), "Files");
    stack.add_titled(&tab(links.upcast_ref(), &more[3]), Some("links"), "Links");
    // our own tabs: StackSwitcher's buttons measured far wider than their labels
    let switcher = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    switcher.add_css_class("linked");
    switcher.set_homogeneous(true); // equal widths, filling the row
    switcher.set_hexpand(true);
    let mut group: Option<gtk::ToggleButton> = None;
    let mut tabs = Vec::new();
    for (name, label) in [("photos", "Photos"), ("videos", "Videos"), ("files", "Files"), ("links", "Links")] {
        let t = gtk::ToggleButton::with_label(label);
        t.add_css_class("info-tab");
        t.set_hexpand(true);
        t.set_group(group.as_ref());
        t.set_active(name == "photos");
        let stack = stack.clone();
        t.connect_toggled(move |t| {
            if t.is_active() {
                stack.set_visible_child_name(name);
            }
        });
        switcher.append(&t);
        group.get_or_insert_with(|| t.clone());
        tabs.push((name, t));
    }
    stack.connect_visible_child_name_notify(move |st| {
        if let Some(n) = st.visible_child_name() {
            for (name, t) in &tabs {
                t.set_active(*name == n);
            }
        }
    });
    // narrow panel: "‹ Photos ›". Both carets take the same room even when one is hidden, so the name stays centred.
    let prev = gtk::Button::from_icon_name("pan-start-symbolic");
    let next = gtk::Button::from_icon_name("pan-end-symbolic");
    let name = gtk::Label::new(Some("Photos"));
    name.add_css_class("tab-name");
    name.set_hexpand(true);
    name.set_xalign(0.5);
    let carets = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    for b in [&prev, &next] {
        b.add_css_class("head-btn");
        b.set_valign(gtk::Align::Center);
    }
    carets.append(&prev);
    carets.append(&name);
    carets.append(&next);
    carets.set_hexpand(true);
    carets.set_visible(false);
    const TABS: [&str; 4] = ["photos", "videos", "files", "links"];
    let show = {
        let (stack, prev, next, name) = (stack.clone(), prev.clone(), next.clone(), name.clone());
        move || {
            let i = stack.visible_child_name().and_then(|n| TABS.iter().position(|t| *t == n)).unwrap_or(0);
            let names = ["Photos", "Videos", "Files", "Links"];
            name.set_text(names[i]);
            prev.set_tooltip_text(i.checked_sub(1).map(|j| names[j]));
            next.set_tooltip_text(names.get(i + 1).copied());
            for (b, on) in [(&prev, i > 0), (&next, i + 1 < TABS.len())] {
                b.set_opacity(if on { 1.0 } else { 0.0 });
                b.set_can_target(on);
                b.set_can_focus(on);
            }
        }
    };
    show();
    stack.connect_visible_child_name_notify(move |_| show());
    for (b, step) in [(&prev, -1i32), (&next, 1)] {
        let stack = stack.clone();
        b.connect_clicked(move |_| {
            let i = stack.visible_child_name().and_then(|n| TABS.iter().position(|t| *t == n)).unwrap_or(0) as i32;
            if let Some(t) = TABS.get((i + step).max(0) as usize) {
                stack.set_visible_child_name(t);
            }
        });
    }
    // one row: the tabs, and the close button
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    head.set_margin_top(12);
    head.set_margin_bottom(8);
    head.set_margin_start(14);
    head.set_margin_end(14);
    // an invisible twin of the close button on the left, so the carets' label is centred on the whole row
    let twin = gtk::Button::from_icon_name("window-close-symbolic");
    twin.add_css_class("head-btn");
    twin.set_opacity(0.0);
    twin.set_can_target(false);
    twin.set_can_focus(false);
    twin.set_focusable(false);
    carets.bind_property("visible", &twin, "visible").sync_create().build();
    head.append(&twin);
    head.append(&switcher);
    head.append(&carets);
    head.append(&close);
    // only the tab's content scrolls
    stack.set_margin_start(14);
    stack.set_margin_end(14);
    stack.set_margin_bottom(14);
    let scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&stack).build();
    let panel = gtk::Box::new(gtk::Orientation::Vertical, 0);
    panel.append(&head);
    panel.append(&scroll);
    panel.set_size_request(info_w(4), -1);
    panel.set_hexpand(false);
    panel.set_overflow(gtk::Overflow::Hidden); // the rounded corners clip it
    panel.add_css_class("info-panel");
    let revealer = gtk::Revealer::builder().child(&panel).transition_type(gtk::RevealerTransitionType::SlideLeft).reveal_child(false).build();
    // expand flags pass up from the children: without this, the closed panel still takes a share of the width
    revealer.set_hexpand(false);
    Info { revealer, panel, btn, refresh, close, switcher, carets, photos, videos, files, links, more }
}

const THUMB: i32 = 92;

/// Width the panel takes beside the chat for `n` columns of photos (4 at most), margin included.
fn info_w(n: i32) -> i32 {
    // each grid cell measures THUMB + 6; the header needs room too (measured: three tabs + close = 263, caret row = 223, margins included)
    (n * (THUMB + 6) + (n - 1) * 4 + 28 + 14).max(if n >= 3 { 340 } else { 223 } + 14) // ponytail: 340 = four tabs + close, estimated; measure
}

/// Share the window between sidebar, chat and info panel.
/// - Too narrow for the full sidebar beside the chat (`SIDEBAR_MIN` + `CHAT_MIN`): the sidebar shows avatars
///   only (`COLLAPSED_W`), or fills the window when expanded with its caret.
/// - The info panel is an overlay of constant width (4 photo columns): it covers the chat's right side, and the
///   sidebar too when the chat is narrower than it.
fn relayout(u: &Rc<Ui>, total: i32) {
    if total <= 0 {
        return;
    }
    let narrow = total < SIDEBAR_MIN + CHAT_MIN;
    let crossed = narrow != u.narrow.replace(narrow);
    if crossed {
        u.side_open.set(false); // the expanded state is forgotten whenever the window crosses the line
    }
    let pref = match u.st.borrow().side_pref {
        0 => 340,
        p => p,
    };
    let side = if narrow {
        // expanded: the chat's 6 px margin and the 2 px divider lie beyond the window edge, so the sidebar's own 6 px
        // right margin leaves it 14 px from the edge, the same as on the left
        if u.side_open.get() { total - 8 } else { COLLAPSED_W }
    } else {
        pref.clamp(SIDEBAR_MIN, total - CHAT_MIN)
    };
    // the info panel keeps its full width (4 photo columns) and slides over the chat, and over the sidebar if it has to:
    // it fits even in the narrowest window (INFO_COLS = 4: info_w(4) is 446, the window's minimum 536)
    let cols = 4;
    // crossing the line slides the sidebar to its new width (not on the first layout)
    let animate = u.animate.replace(false) || (crossed && u.last_pos.get() != 0);
    move_side(u, side, animate);
    // sidebar: avatars only, or with names
    let collapsed = narrow && !u.side_open.get();
    u.side_toggle.set_visible(narrow);
    u.side_toggle.set_tooltip_text(Some(if collapsed { "Show conversation names" } else { "Hide conversation names" }));
    if narrow && !collapsed {
        u.conv_panel.add_css_class("away"); // fades while the sidebar pushes it out of the window: no shadow left at the edge
    } else {
        u.conv_panel.remove_css_class("away");
    }
    // the avatars' circles and the names' cropping change with the slide itself; nothing is rebuilt
    if u.collapsed.get() != collapsed {
        show_collapsed(u, collapsed);
    }
    u.side_title.set_margin_start(if narrow { 6 } else { 18 });
    if !collapsed {
        // room the names will have once the sidebar is settled (row width minus avatar, gaps and dot)
        let inner = side - 20;
        u.text_target.set((inner - 32 - AVATAR - 11 - 11 - 10).max(40));
    }
    if u.slide_to.get() == NO_SLIDE {
        settle(u);
    }
    // info panel
    let info = &u.info;
    info.panel.set_size_request(info_w(cols), -1); // the CSS margin counts inside it
    // four tabs in a row fit from 3 photo columns up (measuring StackSwitcher gave nonsense widths)
    let wide = cols >= 3;
    info.switcher.set_visible(wide);
    info.carets.set_visible(!wide);
}

/// Avatars only, or names too: the CSS classes (circle round the avatar, unread tint) and the tooltips.
fn show_collapsed(u: &Rc<Ui>, collapsed: bool) {
    u.collapsed.set(collapsed);
    if collapsed {
        u.sidebar.add_css_class("collapsed");
    } else {
        u.sidebar.remove_css_class("collapsed");
    }
    for (row, _, title) in u.rows.borrow().iter() {
        row.set_tooltip_text(collapsed.then_some(title.as_str()));
    }
    // a pinned tile's name goes away with the rest of the text (hidden, it also takes no room under the avatar)
    let mut c = u.tiles.first_child();
    while let Some(w) = c {
        if let Some(name) = w.downcast_ref::<gtk::FlowBoxChild>().and_then(|f| f.child()).and_then(|t| t.last_child()) {
            name.set_visible(!collapsed);
        }
        c = w.next_sibling();
    }
}

/// The sidebar has stopped moving: truncate the names to their final room.
fn settle(u: &Rc<Ui>) {
    fit_texts(u);
}

/// Give the row texts the width they'll have once settled (truncating with "…" only then, never mid-slide).
fn fit_texts(u: &Rc<Ui>) {
    let w = u.text_target.get();
    if u.text_w.replace(w) != w {
        for (_, slot, _) in u.rows.borrow().iter() {
            slot.set_size_request(w + 21, -1);
        }
    }
}

/// Put the paned divider at `side`, sliding there over 220 ms when `animate`.
fn move_side(u: &Rc<Ui>, side: i32, animate: bool) {
    let running = u.slide_to.get() != NO_SLIDE;
    if running && !animate && u.slide_to.get() == side {
        return; // already on its way there
    }
    let animate = animate || running; // a window being dragged retargets the slide instead of cutting it short
    let id = u.slide.get().wrapping_add(1);
    u.slide.set(id); // cancels a slide in progress
    let from = u.paned.position();
    if !animate || from == side {
        if from != side {
            u.paned.set_position(side);
        }
        u.last_pos.set(side);
        u.slide_to.set(NO_SLIDE);
        return;
    }
    u.slide_to.set(side);
    let start = std::cell::Cell::new(None::<i64>);
    let weak = Rc::downgrade(u);
    u.paned.add_tick_callback(move |p, clock| {
        let Some(u) = weak.upgrade() else { return glib::ControlFlow::Break };
        if u.slide.get() != id {
            return glib::ControlFlow::Break;
        }
        let t0 = start.get().unwrap_or_else(|| {
            start.set(Some(clock.frame_time()));
            clock.frame_time()
        });
        let t = ((clock.frame_time() - t0) as f64 / 220_000.0).min(1.0);
        let e = 1.0 - (1.0 - t).powi(3); // ease out
        let pos = from + ((side - from) as f64 * e).round() as i32;
        p.set_position(pos);
        u.last_pos.set(pos);
        if t >= 1.0 {
            u.slide_to.set(NO_SLIDE);
            settle(&u);
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
    u.last_pos.set(from);
}

/// Picking a chat or starting a message closes the expanded sidebar (narrow window).
fn fold_side(u: &Rc<Ui>) {
    if u.side_open.replace(false) {
        u.animate.set(true);
        relayout(u, u.paned.width());
    }
}

fn wire_info(u: &Rc<Ui>) {
    let u2 = u.clone();
    u.info.refresh.connect_clicked(move |_| refresh_chat(&u2));
    let u2 = u.clone();
    u.info.btn.connect_clicked(move |_| {
        if u2.info.revealer.reveals_child() {
            close_info(&u2);
        } else {
            open_info(&u2);
        }
    });
    let u2 = u.clone();
    u.info.close.connect_clicked(move |_| close_info(&u2));
    for kind in [Kind::Photos, Kind::Videos, Kind::Files, Kind::Links] {
        let u2 = u.clone();
        u.info.more[kind_idx(kind)].connect_clicked(move |b| {
            b.set_visible(false);
            let (chat, off) = {
                let st = u2.st.borrow();
                let Some(chat) = st.info_chat else { return };
                (chat, st.media.get(&(chat, kind)).map_or(0, |m| m.0))
            };
            u2.send(Cmd::Media { chat_id: chat, kind, offset: off });
        });
    }
}

/// The panel slides over the chat from the right; a click anywhere outside it closes it.
fn open_info(u: &Rc<Ui>) {
    u.info.revealer.set_reveal_child(true);
    relayout(u, u.paned.width());
    refresh_info(u);
}

fn close_info(u: &Rc<Ui>) {
    u.info.revealer.set_reveal_child(false);
    u.st.borrow_mut().info_chat = None;
}

/// A clicked link: a YouTube one (with its `key` in the viewer's sequence) plays in the viewer, anything else opens
/// in the browser.
fn open_link(u: &Rc<Ui>, url: &str, from: &gtk::Widget, key: Option<&str>) {
    match (key, crate::mac::youtube_id(url)) {
        (Some(k), _) => open_media(u, k, from),
        (None, Some(id)) => open_media(u, &crate::viewer::yt_key(&id, 0), from), // alone: not in any list
        (None, None) => gtk::UriLauncher::new(url).launch(Some(&u.win), gio::Cancellable::NONE, |_| {}),
    }
}

/// A YouTube link card in the chat joins the viewer's sequence of the chat's pictures and videos; returns its key.
fn chat_youtube(u: &Rc<Ui>, url: &str, card: &gtk::Widget) -> Option<String> {
    let id = crate::mac::youtube_id(url)?;
    let mut st = u.st.borrow_mut();
    let key = crate::viewer::yt_key(&id, st.photos.len());
    st.photos.push((key.clone(), card.clone()));
    Some(key)
}

/// Open the viewer on `key`, walking the chat's pictures and videos if `from` is in the chat, or the info panel's
/// the photos or the videos and YouTube links (newest first) of the tab it is in, if it is in the panel.
fn open_media(u: &Rc<Ui>, key: &str, from: &gtk::Widget) {
    let in_panel = from.is_ancestor(&u.info.panel);
    let mut items = if in_panel { info_sequence(u, from.is_ancestor(&u.info.videos)) } else { u.st.borrow().photos.clone() };
    u.st.borrow_mut().viewer_panel = in_panel;
    if !items.iter().any(|i| i.0 == key) {
        items = vec![(key.to_string(), from.clone())]; // not in the list (yet): just this one
    }
    u.viewer.open(items, key);
}

/// The info panel's sequence for the viewer, newest first: the Photos tab's photos, or the Videos tab's video files and
/// YouTube links together (you chose one kind to look at). The tiles carry "<date>|<key>" as their widget name.
fn info_sequence(u: &Rc<Ui>, videos: bool) -> Vec<crate::viewer::Item> {
    let mut v: Vec<(i64, crate::viewer::Item)> = vec![];
    for flow in [if videos { &u.info.videos } else { &u.info.photos }] {
        let mut c = flow.first_child();
        while let Some(fc) = c {
            if let Some(w) = fc.downcast_ref::<gtk::FlowBoxChild>().and_then(|f| f.child()) {
                if let Some((date, key)) = w.widget_name().split_once('|')
                    && let Ok(date) = date.parse::<i64>()
                {
                    // a video's picture is the tile's inner bubble; a YouTube tile is its own widget
                    let item = if key.starts_with("yt:") { w.clone() } else { w.clone().downcast::<gtk::Overlay>().ok().and_then(|o| o.child()).unwrap_or(w.clone()) };
                    v.push((date, (key.to_string(), item)));
                }
            }
            c = fc.next_sibling();
        }
    }
    v.sort_by_key(|(d, _)| std::cmp::Reverse(*d));
    v.into_iter().map(|(_, i)| i).collect()
}

fn clear(b: &impl IsA<gtk::Widget>) {
    while let Some(c) = b.first_child() {
        c.unparent();
    }
}

/// Fill the panel for the open chat: its people now, its photos, files and links as they arrive.
fn refresh_info(u: &Rc<Ui>) {
    let Some(id) = u.st.borrow().sel else { return };
    {
        let mut st = u.st.borrow_mut();
        st.info_chat = Some(id);
        st.info_links.clear();
        st.info_files.clear();
        st.info_videos.clear();
        st.link_waiters.clear();
        st.media.retain(|k, _| k.0 != id);
    }
    u.info.photos.remove_all();
    u.info.videos.remove_all();
    u.info.files.remove_all();
    u.info.links.remove_all();
    for kind in [Kind::Photos, Kind::Videos, Kind::Files, Kind::Links] {
        u.info.more[kind_idx(kind)].set_visible(false);
        u.st.borrow_mut().media.insert((id, kind), (0, false));
        u.send(Cmd::Media { chat_id: id, kind, offset: 0 });
    }
}

fn fmt_date(unix: i64) -> String {
    glib::DateTime::from_unix_local(unix).ok().and_then(|d| d.format("%b %-d, %Y").ok()).map(|s| s.to_string()).unwrap_or_default()
}

fn fmt_size(b: i64) -> String {
    match b {
        0 => String::new(),
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.0} KB", b as f64 / 1024.0),
        b => format!("{:.1} MB", b as f64 / 1024.0 / 1024.0),
    }
}

/// A page of photos, files or links arrived for the panel.
fn on_media(u: &Rc<Ui>, chat_id: i64, kind: Kind, offset: usize, items: Vec<MediaItem>, next: usize, more: bool) {
    {
        let mut st = u.st.borrow_mut();
        // for another chat (the panel moved on) or an out-of-date page: drop it
        if st.info_chat != Some(chat_id) || st.media.get(&(chat_id, kind)).map(|m| m.0) != Some(offset) {
            return;
        }
        st.media.insert((chat_id, kind), (next, more));
    }
    u.info.more[kind_idx(kind)].set_visible(more);
    let empty = |text: &str| {
        let l = gtk::Label::new(Some(text));
        l.add_css_class("dim");
        l
    };
    match kind {
        Kind::Photos => {
            for it in items {
                if it.missing {
                    // not on the Mac (Messages hasn't downloaded it from iCloud, or it's gone): nothing to fetch
                    let tile = gtk::Box::new(gtk::Orientation::Vertical, 0);
                    tile.add_css_class("info-missing");
                    tile.set_size_request(THUMB, THUMB);
                    let l = gtk::Label::new(Some("Not on your Mac"));
                    l.add_css_class("dim");
                    l.set_wrap(true);
                    l.set_justify(gtk::Justification::Center);
                    l.set_max_width_chars(10);
                    l.set_valign(gtk::Align::Center);
                    l.set_vexpand(true);
                    tile.append(&l);
                    tile.set_tooltip_text(Some("Messages hasn't downloaded this photo to your Mac"));
                    u.info.photos.append(&tile);
                    continue;
                }
                let b = bubble::Bubble::tile(THUMB);
                b.add_css_class("fixed");
                b.set_cursor_from_name(Some("pointer"));
                let (u2, path, b2) = (u.clone(), it.path.clone(), b.clone().upcast::<gtk::Widget>());
                let g = gtk::GestureClick::new();
                g.connect_released(move |g, _, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    open_media(&u2, &path, &b2);
                });
                b.add_controller(g);
                b.set_widget_name(&format!("{}|{}", it.date, it.path)); // read back by `info_sequence`
                load_texture_in(u, &b, &it.path, true);
                u.info.photos.append(&b);
            }
            if offset == 0 && u.info.photos.first_child().is_none() {
                u.info.photos.insert(&empty("No photos in this conversation"), -1);
            }
        }
        Kind::Videos => {
            u.st.borrow_mut().info_videos.extend(items);
            render_videos(u);
        }
        Kind::Files => {
            u.st.borrow_mut().info_files.extend(items);
            render_files(u);
            if offset == 0 && u.st.borrow().info_files.is_empty() {
                u.info.files.insert(&empty("No files in this conversation"), -1);
            }
        }
        Kind::Links => {
            u.st.borrow_mut().info_links.extend(items);
            render_links(u);
            render_videos(u); // YouTube links live there
            if offset == 0 && u.info.links.first_child().is_none() {
                u.info.links.append(&empty("No links in this conversation"));
            }
        }
    }
}

/// Rebuild the Files tab from `info_files`: a tile per file, like the Links tab's.
fn render_files(u: &Rc<Ui>) {
    u.info.files.remove_all();
    let items = u.st.borrow().info_files.clone();
    for it in &items {
        u.info.files.append(&file_tile(u, it));
    }
}

/// A file as a square tile like the links': the first page or frame where the Mac can draw one, else our
/// icon for its kind on off-white, with the file name along the bottom. Clicking copies it from the Mac and opens it.
fn file_tile(u: &Rc<Ui>, it: &MediaItem) -> gtk::Widget {
    let kind = file_kind("", &it.name);
    let thumb = kind.thumb() && !it.missing && !u.st.borrow().bad_images.contains(&it.path);
    let frame = gtk::Overlay::new();
    frame.add_css_class("link-tile");
    frame.set_overflow(gtk::Overflow::Hidden);
    frame.set_size_request(THUMB, THUMB);
    frame.set_hexpand(false);
    frame.set_vexpand(false);
    if thumb {
        let b = bubble::Bubble::tile(THUMB);
        b.add_css_class("fixed");
        load_texture_in(u, &b, &it.path, true);
        frame.set_child(Some(&b));
        if kind == FileKind::Video {
            frame.add_overlay(&play_badge());
        }
    } else {
        frame.add_css_class("plain");
        let base = gtk::Box::new(gtk::Orientation::Vertical, 0);
        base.set_size_request(THUMB, THUMB);
        let g = gtk::Image::from_icon_name(kind.icon());
        g.set_pixel_size(40);
        g.add_css_class("tile-glyph");
        g.set_vexpand(true);
        g.set_valign(gtk::Align::Center);
        g.set_halign(gtk::Align::Center);
        base.append(&g);
        frame.set_child(Some(&base));
    }
    let caption = gtk::Label::new(Some(&it.name));
    caption.add_css_class("tile-caption");
    caption.set_halign(gtk::Align::Fill);
    caption.set_valign(gtk::Align::End);
    caption.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    caption.set_xalign(0.0);
    frame.add_overlay(&caption);
    let detail: Vec<String> = [kind.label().to_string(), fmt_size(it.size), fmt_date(it.date)].into_iter().filter(|s| !s.is_empty()).collect();
    if it.missing {
        frame.add_css_class("file-missing");
        frame.set_tooltip_text(Some(&format!("{}\n{}\nMessages hasn't downloaded this file to your Mac", it.name, detail.join(" · "))));
    } else if kind == FileKind::Video && thumb {
        frame.set_tooltip_text(Some(&format!("{}\n{}\nClick to play", it.name, detail.join(" · "))));
        frame.set_cursor_from_name(Some("pointer"));
        let b = frame.child().and_downcast::<bubble::Bubble>().expect("video tile picture");
        b.add_css_class("video");
        u.st.borrow_mut().video_meta.insert(it.path.clone(), (it.name.clone(), it.size));
        frame.set_widget_name(&format!("{}|{}", it.date, it.path)); // read back by `info_sequence`
        let (u2, path) = (u.clone(), it.path.clone());
        let g = gtk::GestureClick::new();
        g.connect_released(move |_, _, _, _| open_media(&u2, &path, b.upcast_ref()));
        frame.add_controller(g);
    } else {
        frame.set_tooltip_text(Some(&format!("{}\n{}\nClick to copy from the Mac and open", it.name, detail.join(" · "))));
        frame.set_cursor_from_name(Some("pointer"));
        let (u2, it2, f2) = (u.clone(), it.clone(), frame.clone());
        let g = gtk::GestureClick::new();
        g.connect_released(move |_, _, _, _| start_download(&u2, &it2, f2.upcast_ref()));
        frame.add_controller(g);
    }
    frame.upcast()
}

/// A small "play" disc over a video's first frame.
fn play_badge() -> gtk::Widget {
    let l = gtk::Label::new(Some("▶"));
    l.add_css_class("play-badge");
    l.set_halign(gtk::Align::Center);
    l.set_valign(gtk::Align::Center);
    l.set_can_target(false);
    l.upcast()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileKind {
    Pdf,
    Video,
    Audio,
    Contact,
    Doc,
    Sheet,
    Archive,
    Other,
}

impl FileKind {
    fn label(self) -> &'static str {
        match self {
            FileKind::Pdf => "PDF",
            FileKind::Video => "Video",
            FileKind::Audio => "Audio",
            FileKind::Contact => "Contact",
            FileKind::Doc => "Document",
            FileKind::Sheet => "Spreadsheet",
            FileKind::Archive => "Archive",
            FileKind::Other => "File",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            FileKind::Video => "bf-video-symbolic",
            FileKind::Audio => "bf-audio-symbolic",
            FileKind::Contact => "bf-contact-symbolic",
            FileKind::Archive => "bf-archive-symbolic",
            _ => "bf-file-symbolic",
        }
    }

    /// The Mac's Quick Look can draw a picture of these (a PDF's first page, a video's first frame). It only
    /// has a generic note for audio and can't draw a vCard.
    fn thumb(self) -> bool {
        matches!(self, FileKind::Pdf | FileKind::Video)
    }
}

/// What kind of file this is, from its MIME type when the database has one, else its extension.
fn file_kind(mime: &str, name: &str) -> FileKind {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let ext = ext.as_str();
    if mime == "application/pdf" || ext == "pdf" {
        FileKind::Pdf
    } else if mime.starts_with("video/") || matches!(ext, "mov" | "mp4" | "m4v" | "3gp" | "avi" | "mkv" | "mpg" | "mpeg") {
        FileKind::Video
    } else if mime.starts_with("audio/") || matches!(ext, "mp3" | "m4a" | "wav" | "caf" | "aac" | "amr" | "aiff" | "flac") {
        FileKind::Audio
    } else if mime == "text/vcard" || ext == "vcf" {
        FileKind::Contact
    } else if mime.contains("spreadsheet") || mime.contains("excel") || matches!(ext, "xls" | "xlsx" | "csv" | "numbers" | "ods") {
        FileKind::Sheet
    } else if mime.contains("zip") || mime.contains("compressed") || matches!(ext, "zip" | "gz" | "tar" | "7z" | "rar") {
        FileKind::Archive
    } else if mime.contains("word") || mime.starts_with("text/") || matches!(ext, "doc" | "docx" | "txt" | "rtf" | "pages" | "odt" | "md") {
        FileKind::Doc
    } else {
        FileKind::Other
    }
}

/// `s` shortened to about `max` characters by cutting out the middle ("Start of the title … the end of it"), at word
/// boundaries where there are any, so the end (a file's extension, a product's size) stays. Card titles are limited
/// this way instead of with the label's `lines` + ellipsize, because GTK then asks for as much width as the lines
/// need, which let a long title push a card wider than its cap.
fn clip_middle(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.trim().chars().collect();
    if chars.len() <= max {
        return chars.into_iter().collect();
    }
    let keep = max.saturating_sub(1);
    let (head, tail) = (keep * 3 / 5, keep - keep * 3 / 5);
    let head: String = chars[..head].iter().collect();
    let tail: String = chars[chars.len() - tail..].iter().collect();
    // back to a word end on the head side, forward to a word start on the tail side (when that costs little)
    let head = head.rfind(' ').filter(|i| *i > head.len() / 2).map_or(head.as_str(), |i| &head[..i]).trim_end_matches([' ', ',', '-', '–', '|', ':']);
    let tail = tail.find(' ').filter(|i| *i < tail.len() / 2).map_or(tail.as_str(), |i| &tail[i + 1..]);
    format!("{head}…{tail}")
}

/// "2:16", "1:02:03"
fn fmt_duration(secs: f64) -> String {
    let t = secs.round() as i64;
    let (h, m, s) = (t / 3600, t / 60 % 60, t % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// A file of a message, as the chat card needs it.
struct FileRef {
    path: String,
    name: String,
    kind: FileKind,
    size: i64,
    missing: bool,
}

const FILE_W: i32 = 300;

/// The extra facts for a file, asked of the Mac once (the chat redraws when they arrive).
fn file_info_for(u: &Ui, f: &FileRef) -> Option<crate::mac::FileInfo> {
    if f.missing || f.path.is_empty() || !matches!(f.kind, FileKind::Video | FileKind::Audio | FileKind::Contact) {
        return None;
    }
    if let Some(i) = u.st.borrow().file_info.get(&f.path) {
        return i.clone();
    }
    u.st.borrow_mut().file_info.insert(f.path.clone(), None);
    u.send(Cmd::FileInfo { path: f.path.clone(), contact: f.kind == FileKind::Contact });
    None
}

/// A file in a message, drawn like a link card: its name and kind (with length, size, or the contact's
/// details) beside a small picture: the first page or frame where the Mac can draw one, else our icon for
/// its kind. Clicking copies it from the Mac and opens it; one that isn't on the Mac is dimmed.
fn file_card(u: &Rc<Ui>, f: &FileRef, look: bubble::Look) -> gtk::Widget {
    let info = file_info_for(u, f);
    let b = bubble::Bubble::card(look, FILE_W, 0);
    let texts = gtk::Box::new(gtk::Orientation::Vertical, 2);
    texts.set_hexpand(true);
    texts.set_valign(gtk::Align::Center);
    let title = match &info {
        Some(i) if f.kind == FileKind::Contact && !i.name.is_empty() => i.name.clone(),
        _ => f.name.clone(),
    };
    let t = gtk::Label::new(Some(&clip_middle(&title, 48)));
    t.add_css_class("link-title");
    t.set_xalign(0.0);
    t.set_wrap(true);
    t.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    t.set_max_width_chars(24);
    texts.append(&t);
    let mut parts = vec![f.kind.label().to_string()];
    if let Some(i) = &info {
        if let Some(d) = i.duration {
            parts.push(fmt_duration(d));
        }
        for extra in [&i.org, &i.phone] {
            if !extra.is_empty() {
                parts.push(extra.clone());
            }
        }
    }
    let size = fmt_size(f.size);
    if !size.is_empty() && f.kind != FileKind::Contact {
        parts.push(size);
    }
    if f.missing {
        parts.push("Not on your Mac".into());
    }
    let d = gtk::Label::new(Some(&parts.join(" · ")));
    d.add_css_class("link-site");
    d.set_xalign(0.0);
    d.set_ellipsize(gtk::pango::EllipsizeMode::End);
    d.set_max_width_chars(24);
    texts.append(&d);

    let thumb = f.kind.thumb() && !f.missing && !f.path.is_empty() && !u.st.borrow().bad_images.contains(&f.path);
    let tile: gtk::Widget = if thumb {
        let t = bubble::Bubble::tile(56);
        t.add_css_class("fixed");
        load_texture(u, &t, &f.path);
        if f.kind == FileKind::Video {
            let o = gtk::Overlay::new();
            o.set_child(Some(&t));
            o.add_overlay(&play_badge());
            o.set_valign(gtk::Align::Center);
            o.set_hexpand(false);
            o.upcast()
        } else {
            t.upcast()
        }
    } else {
        glyph(f.kind.icon())
    };
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.append(&texts);
    row.append(&tile);
    b.add(&row);
    if f.missing {
        b.add_css_class("file-missing");
        b.set_tooltip_text(Some("Messages hasn't downloaded this file to your Mac"));
    } else if !f.path.is_empty() {
        b.set_tooltip_text(Some("Copy from the Mac and open"));
        b.set_cursor_from_name(Some("pointer"));
        let item = crate::mac::MediaItem { path: f.path.clone(), name: f.name.clone(), ..Default::default() };
        let (u2, b2) = (u.clone(), b.clone());
        let click = gtk::GestureClick::new();
        click.connect_released(move |g, _, _, _| {
            g.set_state(gtk::EventSequenceState::Claimed);
            start_download(&u2, &item, b2.upcast_ref());
        });
        b.add_controller(click);
    }
    b.upcast()
}

/// A video on the Mac drawn like a photo: its first frame at the photo size, a play disc in the middle and the
/// length in a corner. Clicking copies it from the Mac and opens it (playing it inside the app comes later).
fn video_preview(u: &Rc<Ui>, f: &FileRef, look: bubble::Look) -> gtk::Widget {
    let info = file_info_for(u, f);
    let p = bubble::Bubble::photo(look, 240, 180);
    load_texture(u, &p, &f.path); // first, so its spinner is the first child (see `stop_spin`)
    let over = gtk::Box::new(gtk::Orientation::Vertical, 0);
    over.set_hexpand(true);
    over.set_vexpand(true);
    over.set_can_target(false);
    let badge = play_badge();
    badge.add_css_class("big");
    badge.set_vexpand(true);
    over.append(&badge);
    if let Some(d) = info.and_then(|i| i.duration) {
        let pill = gtk::Label::new(Some(&fmt_duration(d)));
        pill.add_css_class("video-pill");
        pill.set_halign(gtk::Align::Start);
        over.append(&pill);
    }
    p.append(&over);
    let tip = [f.name.clone(), [f.kind.label().to_string(), fmt_size(f.size)].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · "), "Click to play".into()];
    p.set_tooltip_text(Some(&tip.join("\n")));
    p.set_cursor_from_name(Some("pointer"));
    p.add_css_class("video");
    {
        let mut st = u.st.borrow_mut();
        st.video_meta.insert(f.path.clone(), (f.name.clone(), f.size));
        st.photos.push((f.path.clone(), p.clone().upcast()));
    }
    let (u2, path, p2) = (u.clone(), f.path.clone(), p.clone().upcast::<gtk::Widget>());
    let click = gtk::GestureClick::new();
    click.connect_released(move |g, _, _, _| {
        g.set_state(gtk::EventSequenceState::Claimed);
        open_media(&u2, &path, &p2);
    });
    p.add_controller(click);
    p.upcast()
}

/// Redraw the open conversation soon (once, however many things ask).
fn queue_conv_render(u: &Rc<Ui>) {
    if !u.render_queued.replace(true) {
        let u2 = u.clone();
        glib::idle_add_local_once(move || {
            u2.render_queued.set(false);
            render_conv(&u2);
        });
    }
}

fn files_dir(u: &Ui) -> PathBuf {
    u.store.image_cache.with_file_name("files")
}

fn open_file(u: &Ui, path: &std::path::Path) {
    gtk::FileLauncher::new(Some(&gio::File::for_path(path))).launch(Some(&u.win), gio::Cancellable::NONE, |_| {});
}

/// Copy a chat file from the Mac (once; kept under the cache folder) and open it with the default app.
// ponytail: the copies are never cleaned up; prune by age like the image cache would need to
fn file_dest(u: &Ui, path: &str, name: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    let name: String = name.chars().map(|c| if c == '/' || c.is_control() { '_' } else { c }).collect();
    files_dir(u).join(format!("{:016x}", h.finish())).join(if name.is_empty() { "file" } else { &name })
}

/// The viewer wants a video: use our copy, or copy it from the Mac (progress comes back as events).
fn fetch_video(u: &Rc<Ui>, path: &str) {
    let (name, size) = u.st.borrow().video_meta.get(path).cloned().unwrap_or_default();
    let dest = file_dest(u, path, &name);
    if dest.exists() && std::fs::metadata(&dest).is_ok_and(|m| size <= 0 || m.len() as i64 >= size * 98 / 100) {
        u.viewer.video_ready(path, dest);
    } else if u.st.borrow_mut().video_wait.insert(path.to_string()) {
        u.send(Cmd::Download { path: path.to_string(), dest, size });
    }
}

fn start_download(u: &Rc<Ui>, it: &MediaItem, chip: &gtk::Widget) {
    let dest = file_dest(u, &it.path, &it.name);
    if dest.exists() {
        open_file(u, &dest);
        return;
    }
    if u.st.borrow().downloading.contains_key(&it.path) {
        return;
    }
    chip.set_opacity(0.5);
    chip.set_tooltip_text(Some("Copying from the Mac…"));
    u.st.borrow_mut().downloading.insert(it.path.clone(), chip.downgrade());
    u.send(Cmd::Download { path: it.path.clone(), dest, size: it.size });
}

/// A link as a square tile like the photos: Messages' preview picture cropped to fit (or a link glyph
/// when there is none), with the site name along the bottom. Clicking opens it in the browser.
fn link_tile(u: &Rc<Ui>, url: &str, l: Option<&crate::mac::LinkPreview>) -> gtk::Widget {
    let l = l.map(|l| usable(u, l));
    let l = l.as_ref();
    let frame = gtk::Overlay::new();
    frame.add_css_class("link-tile");
    frame.set_overflow(gtk::Overflow::Hidden);
    frame.set_size_request(THUMB, THUMB);
    frame.set_hexpand(false);
    frame.set_vexpand(false);
    // a big picture is cropped to fill; a small one is the site's logo, which is shown whole on a white tile
    let image = l.and_then(|l| match &l.image {
        Some(i) if l.image_size.is_none_or(|(w, h)| crate::mac::is_preview_size(w, h)) => Some((i, true)),
        _ => small_picture(l).map(|i| (i, false)), // whole on the white tile
    });
    if let Some((img, big)) = image {
        let b = if big { bubble::Bubble::tile(THUMB) } else { bubble::Bubble::thumb(THUMB) };
        b.add_css_class("fixed");
        load_texture_in(u, &b, img, true);
        frame.set_child(Some(&b));
    } else {
        // nothing to show: the link icon, solid and dark on off-white
        frame.add_css_class("plain");
        let base = gtk::Box::new(gtk::Orientation::Vertical, 0);
        base.set_size_request(THUMB, THUMB);
        let g = gtk::Image::from_icon_name("bf-link-symbolic");
        g.set_pixel_size(40);
        g.add_css_class("tile-glyph");
        g.set_vexpand(true);
        g.set_valign(gtk::Align::Center);
        g.set_halign(gtk::Align::Center);
        base.append(&g);
        frame.set_child(Some(&base));
    }
    let host = url.split('/').nth(2).unwrap_or(url).trim_start_matches("www.").to_string();
    let caption = gtk::Label::new(Some(l.map(|l| l.site.as_str()).filter(|s| !s.is_empty()).unwrap_or(&host)));
    caption.add_css_class("tile-caption");
    caption.set_halign(gtk::Align::Fill);
    caption.set_valign(gtk::Align::End);
    caption.set_ellipsize(gtk::pango::EllipsizeMode::End);
    caption.set_xalign(0.0);
    frame.add_overlay(&caption);
    frame.set_tooltip_text(Some(&match l.map(|l| l.title.as_str()).filter(|t| !t.is_empty()) {
        Some(t) => format!("{t}\n{url}"),
        None => url.to_string(),
    }));
    frame.set_cursor_from_name(Some("pointer"));
    let (u2, url, f2) = (u.clone(), url.to_string(), frame.clone());
    let g = gtk::GestureClick::new();
    g.connect_released(move |_, _, _, _| {
        // a YouTube tile's slot (its parent) is named "<date>|<key>": the key in the panel's sequence
        let slot = f2.parent();
        let key = slot.as_ref().map(|p| p.widget_name().to_string()).and_then(|n| n.split_once('|').map(|x| x.1.to_string()));
        open_link(&u2, &url, slot.as_ref().unwrap_or(f2.upcast_ref()), key.as_deref());
    });
    frame.add_controller(g);
    frame.upcast()
}

/// A link as a tile in a slot, with Messages' preview picture where it made one (asked for from the Mac).
fn link_slot(u: &Rc<Ui>, it: &MediaItem) -> gtk::Box {
    let slot = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let cached = if it.preview && it.row != 0 { u.st.borrow().links.get(&it.row).cloned() } else { None };
    match cached {
        Some(Some(l)) => slot.append(&link_tile(u, &l.url, Some(&l))),
        _ => {
            slot.append(&link_tile(u, &it.url, None));
            if it.preview && it.row != 0 {
                let first = {
                    let mut st = u.st.borrow_mut();
                    let first = !st.links.contains_key(&it.row);
                    if first {
                        st.links.insert(it.row, None);
                    }
                    st.link_waiters.entry(it.row).or_default().push(slot.downgrade());
                    first
                };
                if first {
                    u.send(Cmd::Link(it.row));
                }
            }
        }
    }
    slot
}

/// Rebuild the Links tab from `info_links` (YouTube links go to the Videos tab).
fn render_links(u: &Rc<Ui>) {
    u.info.links.remove_all();
    let items = u.st.borrow().info_links.clone();
    for it in items.iter().filter(|i| crate::mac::youtube_id(&i.url).is_none()) {
        u.info.links.append(&link_slot(u, it));
    }
}

/// Rebuild the Videos tab: video files and YouTube links, newest first.
fn render_videos(u: &Rc<Ui>) {
    u.info.videos.remove_all();
    let (files, links) = {
        let st = u.st.borrow();
        (st.info_videos.clone(), st.info_links.clone())
    };
    let mut all: Vec<MediaItem> = files.into_iter().chain(links.into_iter().filter(|i| crate::mac::youtube_id(&i.url).is_some())).collect();
    all.sort_by_key(|i| std::cmp::Reverse(i.date));
    for it in &all {
        if it.url.is_empty() {
            u.info.videos.append(&file_tile(u, it));
        } else {
            let slot = link_slot(u, it);
            if let Some(id) = crate::mac::youtube_id(&it.url) {
                slot.set_widget_name(&format!("{}|{}", it.date, crate::viewer::yt_key(&id, it.row as usize)));
            }
            u.info.videos.append(&slot);
        }
    }
    if all.is_empty() && !u.info.more[1].is_visible() {
        let l = gtk::Label::new(Some("No videos in this conversation"));
        l.add_css_class("dim");
        u.info.videos.append(&l);
    }
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

    let privacy_title = gtk::Label::new(Some("Privacy"));
    privacy_title.set_xalign(1.0);
    privacy_title.set_valign(gtk::Align::Start);
    let previews = gtk::CheckButton::with_label("Show map previews on contact cards");
    previews.set_active(cfg.map_previews);
    let why = gtk::Label::new(Some(
        "Uses OpenStreetMap: a contact's postal address is sent there to find it on the map. When off, cards show a plain button that opens the address in your maps app.",
    ));
    why.set_wrap(true);
    why.set_xalign(0.0);
    why.add_css_class("dim");
    let pbox = gtk::Box::new(gtk::Orientation::Vertical, 6);
    pbox.append(&previews);
    pbox.append(&why);
    grid.attach(&privacy_title, 0, 6, 1, 1);
    grid.attach(&pbox, 1, 6, 1, 1);

    let save = gtk::Button::with_label("Save and connect");
    save.add_css_class("suggested-action");
    let cancel = gtk::Button::with_label("Cancel");
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.append(&cancel);
    buttons.append(&save);
    grid.attach(&buttons, 0, 7, 2, 1);
    w.set_child(Some(&grid));

    let w2 = w.clone();
    cancel.connect_clicked(move |_| w2.close());
    let (u2, w2) = (u.clone(), w.clone());
    save.connect_clicked(move |_| {
        let mut imsg_path = imsg.text().trim().to_string();
        if imsg_path.is_empty() {
            imsg_path = crate::DEFAULT_IMSG.into();
        }
        let cfg = Config { host: host.text().trim().to_string(), user: user.text().trim().to_string(), imsg: imsg_path, map_previews: previews.is_active() };
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

/// A phone number as people write it: +1 (888) 555-1212 for North American numbers. Emails, short
/// codes and other countries' numbers stay as they are.
pub fn pretty_handle(h: &str) -> String {
    let d: String = h.chars().filter(char::is_ascii_digit).collect();
    let na = match (h.starts_with('+'), d.len()) {
        (true, 11) if d.starts_with('1') => Some(&d[1..]),
        (false, 11) if d.starts_with('1') => Some(&d[1..]),
        (false, 10) => Some(&d[..]),
        _ => None,
    };
    match na {
        Some(n) if !h.contains('@') => format!("+1 ({}) {}-{}", &n[..3], &n[3..6], &n[6..]),
        _ => h.to_string(),
    }
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
        .map(|p| names.get(p).cloned().unwrap_or_else(|| pretty_handle(p)))
        .collect();
    if !people.is_empty() {
        return people.join(", ");
    }
    pretty_handle(c["identifier"].as_str().unwrap_or("?"))
}

fn sender_name(m: &Value, names: &HashMap<String, String>) -> String {
    if let Some(n) = m["sender_name"].as_str().filter(|s| !s.is_empty()) {
        return n.to_string();
    }
    let h = m["sender"].as_str().unwrap_or("?");
    names.get(h).cloned().unwrap_or_else(|| pretty_handle(h))
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

/// The contact's photo for a phone number or email, if the Mac has one.
fn photo_for(u: &Ui, handle: &str) -> Option<gdk::Texture> {
    let key = crate::mac::addr_key(handle)?;
    let mut st = u.st.borrow_mut();
    if let Some(t) = st.avatar_tex.get(&key) {
        return t.clone(); // filled in by `prefetch_avatars` for everyone in the chat list
    }
    if st.avatar_asked.contains(&key) {
        return None; // on its way
    }
    // someone outside the chat list (a suggestion, a card): one small photo, decoded at once
    let t = st.contacts.as_ref()?.photo(handle).map(<[u8]>::to_vec).and_then(decode_avatar);
    st.avatar_tex.insert(key, t.clone());
    t
}

/// Decode the contact photos of everyone in the chat list in the background, scaled down while decoding (they show
/// at 44 px at most), and redraw the avatars once they are in. Decoding them one by one while the list was being
/// built cost most of its time.
fn prefetch_avatars(u: &Rc<Ui>) {
    let todo: Vec<(String, Option<Vec<u8>>)> = {
        let mut st = u.st.borrow_mut();
        let st = &mut *st;
        let Some(contacts) = &st.contacts else { return };
        let mut out = Vec::new();
        for c in &st.chats {
            for h in c["participants"].as_array().into_iter().flatten().filter_map(|p| p.as_str()) {
                let Some(key) = crate::mac::addr_key(h) else { continue };
                if st.avatar_tex.contains_key(&key) || !st.avatar_asked.insert(key.clone()) {
                    continue;
                }
                out.push((key, contacts.photo(h).map(<[u8]>::to_vec)));
            }
        }
        out
    };
    if todo.is_empty() {
        return;
    }
    let u2 = u.clone();
    bg(
        move || {
            todo.into_iter()
                .map(|(key, bytes)| (key, bytes.and_then(|b| decode_avatar(b))))
                .collect::<Vec<_>>()
        },
        move |done| {
            let any = done.iter().any(|(_, t)| t.is_some());
            {
                let mut st = u2.st.borrow_mut();
                for (key, t) in done {
                    st.avatar_tex.insert(key, t);
                }
            }
            if any {
                render_chats_soon(&u2);
                // the open chat: its header avatar only, unless it is a group (senders' avatars are in the messages)
                let open = {
                    let st = u2.st.borrow();
                    st.sel.and_then(|id| st.chats.iter().find(|c| c["id"].as_i64() == Some(id)).cloned())
                };
                if let Some(chat) = open {
                    if chat["is_group"].as_bool() == Some(true) {
                        render_conv(&u2);
                    } else {
                        let title = chat_title(&chat, &u2.st.borrow().names);
                        set_head_avatar(&u2, &chat, &title);
                    }
                }
            }
        },
    );
}

/// A contact photo as a small texture (128 px is enough for a 44 px avatar at 250%).
#[allow(deprecated)] // Texture::for_pixbuf: GTK has no scaled decoding of its own
fn decode_avatar(bytes: Vec<u8>) -> Option<gdk::Texture> {
    use gtk::gdk_pixbuf::Pixbuf;
    let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from_owned(bytes.clone()));
    match Pixbuf::from_stream_at_scale(&stream, 128, 128, true, gio::Cancellable::NONE) {
        Ok(pb) => Some(gdk::Texture::for_pixbuf(&pb)),
        Err(_) => texture(&bytes),
    }
}

/// A round avatar: the contact's photo if there is one, otherwise their initials on a colour.
fn avatar(u: &Ui, name: &str, handle: Option<&str>, small: bool) -> gtk::Widget {
    if let Some(t) = handle.and_then(|h| photo_for(u, h)) {
        let size = if small { 28 } else { 44 };
        let b = bubble::Bubble::disc(size);
        b.set_texture(Some(&t));
        b.add_css_class("avatar");
        b.add_css_class("photo");
        if small {
            b.add_css_class("small");
        }
        b.set_halign(gtk::Align::Center);
        return b.upcast();
    }
    initials_avatar(name, small)
}

/// Initials on a colour, or a generic person for numbers and other names with no letters.
fn initials_avatar(name: &str, small: bool) -> gtk::Widget {
    let hue = name.bytes().fold(0u32, |a, c| a.wrapping_mul(31).wrapping_add(c as u32)) % 8;
    let w: gtk::Widget = if name.chars().next().is_some_and(char::is_alphabetic) {
        let l = gtk::Label::new(Some(&initials(name)));
        l.set_halign(gtk::Align::Center);
        l.upcast()
    } else {
        let img = gtk::Image::from_icon_name("avatar-default-symbolic");
        img.set_pixel_size(if small { 16 } else { 26 });
        img.set_halign(gtk::Align::Center);
        img.set_valign(gtk::Align::Center);
        img.set_hexpand(false); // an avatar never takes spare row width
        let b = gtk::CenterBox::new();
        b.set_halign(gtk::Align::Center);
        b.set_hexpand(false);
        b.set_vexpand(false);
        b.set_center_widget(Some(&img));
        b.upcast()
    };
    w.add_css_class("avatar");
    w.add_css_class(&format!("av{hue}"));
    if small {
        w.add_css_class("small");
    }
    w.set_valign(gtk::Align::Center);
    w
}

/// A small circle for the group avatar: the person's photo or their initials, `d` px across.
fn mini_avatar(u: &Ui, name: &str, handle: &str, d: i32) -> gtk::Widget {
    let w: gtk::Widget = if let Some(t) = photo_for(u, handle) {
        let b = bubble::Bubble::disc(d);
        b.set_texture(Some(&t));
        b.upcast()
    } else {
        let w = initials_avatar(name, true);
        w.remove_css_class("small");
        w
    };
    w.add_css_class("avatar");
    w.add_css_class("mini");
    w.set_size_request(d, d);
    w
}

/// "+3": how many more people than there is room for.
fn more_pill(n: usize, d: i32) -> gtk::Widget {
    let l = gtk::Label::new(Some(&format!("+{n}")));
    l.add_css_class("avatar");
    l.add_css_class("mini");
    l.add_css_class("more");
    l.set_size_request(d, d);
    l.upcast()
}

/// A group's avatar, like Messages: a disc with the other members' small avatars inside it. With
/// more than four people it shows three and a "+N" circle for the rest.
fn group_avatar(u: &Ui, title: &str, members: &[String], names: &HashMap<String, String>) -> gtk::Widget {
    let n = members.len();
    if n < 2 {
        return avatar(u, title, members.first().map(String::as_str), false);
    }
    // ponytail: equal sizes per layout; Messages may size them by member order
    let (d, spots): (i32, &[(i32, i32)]) = match n {
        2 => (26, &[(2, 2), (16, 16)]),
        3 => (22, &[(11, 1), (0, 21), (22, 21)]),
        _ => (19, &[(3, 3), (22, 3), (3, 22), (22, 22)]),
    };
    let fixed = gtk::Fixed::new();
    fixed.set_size_request(44, 44);
    let shown = if n > 4 { 3 } else { n };
    for (h, (x, y)) in members.iter().take(shown).zip(spots) {
        let name = names.get(h).cloned().unwrap_or_else(|| pretty_handle(h));
        fixed.put(&mini_avatar(u, &name, h, d), *x as f64, *y as f64);
    }
    if n > 4 {
        let (x, y) = spots[3];
        fixed.put(&more_pill(n - 3, d), x as f64, y as f64);
    }
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    b.add_css_class("group-avatar");
    b.set_halign(gtk::Align::Center);
    b.set_valign(gtk::Align::Center);
    b.append(&fixed);
    b.upcast()
}

/// For the conversation header: the members' avatars in a row, each peeking out from under the one
/// before it (the first on top), like a stack of photos. Up to four, then a "+N" circle.
/// Called with true while a person's contact card is open, false when it closes.
type CardOpen = Rc<dyn Fn(bool)>;
/// Opens a contact card: (avatar to point at, handle, name, card-open callback).
type OpenCard = Rc<dyn Fn(&gtk::Widget, &str, &str, CardOpen)>;
type Slot = Rc<Cell<Option<usize>>>;

/// `open_card` is called with (avatar widget, handle, name, card-open callback) when a member is clicked.
/// An avatar the pointer is on, or that has its card open, is drawn on top of the stack.
// GTK4's Overlay can't reorder children, so raising one means removing and re-adding it, which would
// fire leave/enter events on it. The avatars therefore don't take events at all: the stack does the
// hit-testing itself, by position.
fn stacked_avatars(u: &Ui, title: &str, members: &[String], names: &HashMap<String, String>, open_card: OpenCard) -> gtk::Widget {
    let n = members.len();
    if n < 2 {
        let w = avatar(u, title, members.first().map(String::as_str), false);
        if let Some(h) = members.first() {
            let (w2, h, t) = (w.clone(), h.clone(), title.to_string());
            let g = gtk::GestureClick::new();
            g.connect_released(move |_, _, _, _| open_card(&w2, &h, &t, Rc::new(|_| {})));
            w.add_controller(g);
            w.set_cursor_from_name(Some("pointer"));
        }
        return w;
    }
    let (d, step) = (36, 22);
    let shown = n.min(4);
    let slots = shown + usize::from(n > 4);
    let base = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    base.set_size_request(d + step * (slots as i32 - 1), d);
    let o = gtk::Overlay::new();
    o.set_child(Some(&base));
    o.set_halign(gtk::Align::Start);
    o.set_valign(gtk::Align::Center);
    o.set_cursor_from_name(Some("pointer"));
    let ws: Rc<Vec<gtk::Widget>> = Rc::new(
        (0..slots)
            .map(|i| {
                let w = if i < shown {
                    mini_avatar(u, &names.get(&members[i]).cloned().unwrap_or_else(|| pretty_handle(&members[i])), &members[i], d)
                } else {
                    more_pill(n - shown, d)
                };
                w.add_css_class("stackav");
                w.set_halign(gtk::Align::Start);
                w.set_valign(gtk::Align::Center);
                w.set_margin_start(i as i32 * step);
                w.set_can_target(false);
                w
            })
            .collect(),
    );
    // added last = drawn on top, so go from the last slot back to the first
    for w in ws.iter().rev() {
        o.add_overlay(w);
    }
    let (hover, open): (Slot, Slot) = (Rc::default(), Rc::default());
    // the usual order (first slot on top), then the slot with its card open, then the hovered one
    let restack: Rc<dyn Fn()> = {
        let (o, ws, hover, open) = (o.downgrade(), ws.clone(), hover.clone(), open.clone());
        Rc::new(move || {
            let Some(o) = o.upgrade() else { return };
            let (hv, op) = (hover.get(), open.get());
            let mut order: Vec<usize> = (0..ws.len()).rev().filter(|i| Some(*i) != hv && Some(*i) != op).collect();
            order.extend(op);
            if hv != op {
                order.extend(hv);
            }
            for i in order {
                o.remove_overlay(&ws[i]);
                o.add_overlay(&ws[i]);
            }
        })
    };
    // which slot is under x: a raised one keeps what it covers, else the topmost (lowest index)
    let hit = {
        let (hover, open) = (hover.clone(), open.clone());
        move |x: f64| -> Option<usize> {
            let inside = |i: usize| x >= (i as i32 * step) as f64 && x < (i as i32 * step + d) as f64;
            [hover.get(), open.get()].into_iter().flatten().find(|i| inside(*i)).or_else(|| (0..slots).find(|i| inside(*i)))
        }
    };
    let m = gtk::EventControllerMotion::new();
    let (hit2, hover2, restack2) = (hit.clone(), hover.clone(), restack.clone());
    m.connect_motion(move |_, x, _| {
        let now = hit2(x);
        if now != hover2.get() {
            hover2.set(now);
            restack2();
        }
    });
    let (hover2, restack2) = (hover.clone(), restack.clone());
    m.connect_leave(move |_| {
        if hover2.replace(None).is_some() {
            restack2();
        }
    });
    o.add_controller(m);
    let g = gtk::GestureClick::new();
    let (members, names, ws2) = (members.to_vec(), names.clone(), ws.clone());
    g.connect_released(move |_, _, x, _| {
        if let Some(i) = hit(x).filter(|i| *i < shown) {
            let name = names.get(&members[i]).cloned().unwrap_or_else(|| pretty_handle(&members[i]));
            let (open, restack) = (open.clone(), restack.clone());
            open_card(&ws2[i], &members[i], &name, Rc::new(move |on| {
                open.set(on.then_some(i));
                restack();
            }));
        }
    });
    o.add_controller(g);
    o.upcast()
}

/// The avatar for a chat in the list: the member circles for a group, the person's own for a 1:1 chat.
fn chat_avatar(u: &Ui, chat: &Value, title: &str) -> gtk::Widget {
    if chat["is_group"].as_bool() == Some(true) {
        let members: Vec<String> = chat["participants"].as_array().into_iter().flatten().filter_map(|p| p.as_str().map(String::from)).collect();
        let names = u.st.borrow().names.clone();
        group_avatar(u, title, &members, &names)
    } else {
        avatar(u, title, direct_addr(chat), false)
    }
}

// ---------- contact card ----------

/// The conversation header's avatar; each person in it opens their contact card when clicked.
fn header_avatar(u: &Rc<Ui>, chat: &Value, title: &str) -> gtk::Widget {
    let click: OpenCard = {
        let u = u.clone();
        Rc::new(move |w, handle, name, open| show_card(&u, w, handle, name, open))
    };
    if chat["is_group"].as_bool() == Some(true) {
        let members: Vec<String> = chat["participants"].as_array().into_iter().flatten().filter_map(|p| p.as_str().map(String::from)).collect();
        let names = u.st.borrow().names.clone();
        stacked_avatars(u, title, &members, &names, click)
    } else {
        let w = avatar(u, title, direct_addr(chat), false);
        if let Some(h) = direct_addr(chat) {
            let (w2, h, t) = (w.clone(), h.to_string(), title.to_string());
            let g = gtk::GestureClick::new();
            g.connect_released(move |_, _, _, _| click(&w2, &h, &t, Rc::new(|_| {})));
            w.add_controller(g);
            w.set_cursor_from_name(Some("pointer"));
        }
        w
    }
}

/// Drop a card in under `w` (a header avatar), with an arrow pointing at it.
fn show_card(u: &Rc<Ui>, w: &gtk::Widget, handle: &str, name: &str, open: CardOpen) {
    let pop = gtk::Popover::new();
    // On the window, not the header: a popover parented to a Box gets a slot among its children
    // and pushes them over.
    pop.set_parent(&u.win);
    pop.set_position(gtk::PositionType::Bottom);
    // room below the avatar, inside the window and the monitor; taller cards scroll
    let mut room = u.win.height();
    if let Some(r) = w.compute_bounds(&u.win) {
        pop.set_pointing_to(Some(&gdk::Rectangle::new(r.x() as i32, r.y() as i32, r.width() as i32, r.height() as i32)));
        room -= (r.y() + r.height()) as i32;
    }
    if let Some(mon) = u.win.surface().and_then(|s| WidgetExt::display(&u.win).monitor_at_surface(&s)) {
        room = room.min(mon.geometry().height() - 120);
    }
    let body = card_body(u, &pop, handle, name);
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .propagate_natural_width(true)
        .max_content_height((room - 30).max(160))
        .child(&body)
        .build();
    pop.set_child(Some(&scroll));
    let open2 = open.clone();
    pop.connect_closed(move |p| {
        open2(false);
        let p = p.clone();
        glib::idle_add_local_once(move || p.unparent());
    });
    pop.popup();
    open(true);
    body.grab_focus(); // else the first button opens with a focus ring
}

/// A round icon button for the contact card.
fn card_button(icon: &str, tip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.add_css_class("card-action");
    b.set_valign(gtk::Align::Center);
    b.set_tooltip_text(Some(tip));
    b
}

/// A person's details from the address book (or just their number when they aren't in it), with
/// buttons to message them or, for an email address, write to it.
fn card_body(u: &Rc<Ui>, pop: &gtk::Popover, handle: &str, name: &str) -> gtk::Box {
    // (value, label, has an iMessage history)
    let (title, org, rows, places, birthday) = {
        let st = u.st.borrow();
        match st.contacts.as_ref().and_then(|c| Some((c, c.person(handle)?))) {
            Some((c, p)) => (
                p.name.clone(),
                p.org.clone(),
                p.addrs.iter().map(|(a, l)| (a.clone(), l.clone(), c.has_imessage(a))).collect::<Vec<_>>(),
                p.places.clone(),
                p.birthday.clone(),
            ),
            None => (name.to_string(), String::new(), vec![(handle.to_string(), String::new(), st.contacts.as_ref().is_some_and(|c| c.has_imessage(handle)))], vec![], None),
        }
    };
    let root = gtk::Box::new(gtk::Orientation::Vertical, 10);
    root.add_css_class("contact-card");
    root.set_size_request(340, -1);
    root.set_focusable(true);
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    head.append(&avatar(u, &title, Some(handle), false));
    let names = gtk::Box::new(gtk::Orientation::Vertical, 2);
    names.set_valign(gtk::Align::Center);
    let n = gtk::Label::new(Some(&title));
    n.add_css_class("card-name");
    n.set_xalign(0.0);
    n.set_wrap(true);
    names.append(&n);
    if !org.is_empty() {
        let o = gtk::Label::new(Some(&org));
        o.add_css_class("dim");
        o.set_xalign(0.0);
        names.append(&o);
    }
    head.append(&names);
    root.append(&head);
    let chip = |text: &str, extra: &str| {
        let c = gtk::Label::new(Some(text));
        c.add_css_class("addr-chip");
        if !extra.is_empty() {
            c.add_css_class(extra);
        }
        c.set_valign(gtk::Align::Center);
        c
    };
    for (addr, label, imsg) in rows {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let col = gtk::Box::new(gtk::Orientation::Vertical, 3);
        col.set_hexpand(true);
        let v = gtk::Label::new(Some(&pretty_handle(&addr)));
        v.set_xalign(0.0);
        v.set_wrap(true);
        v.set_wrap_mode(gtk::pango::WrapMode::Char);
        col.append(&v);
        if !label.is_empty() || imsg {
            let chips = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            if !label.is_empty() {
                chips.append(&chip(&label, ""));
            }
            if imsg {
                chips.append(&chip("iMessage", "imessage"));
            }
            col.append(&chips);
        }
        row.append(&col);
        let msg = card_button("bf-message-symbolic", "Send a message");
        let (u2, pop2, a2) = (u.clone(), pop.downgrade(), addr.clone());
        msg.connect_clicked(move |_| {
            if let Some(p) = pop2.upgrade() {
                p.popdown();
            }
            message_to(&u2, &a2);
        });
        row.append(&msg);
        if addr.contains('@') {
            let mail = card_button("mail-message-new-symbolic", "Write an email");
            let (pop2, win) = (pop.downgrade(), u.win.clone());
            mail.connect_clicked(move |_| {
                if let Some(p) = pop2.upgrade() {
                    p.popdown();
                }
                // not gio::AppInfo::launch_default_for_uri: that handed Thunderbird "mailto:///addr"
                gtk::UriLauncher::new(&format!("mailto:{addr}")).launch(Some(&win), gio::Cancellable::NONE, |_| {});
            });
            row.append(&mail);
        }
        root.append(&row);
    }
    let previews = u.st.borrow().cfg.map_previews;
    for (label, lines) in places {
        let query = lines.join(", ");
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let col = gtk::Box::new(gtk::Orientation::Vertical, 3);
        col.set_hexpand(true);
        let l = gtk::Label::new(Some(&lines.join("\n")));
        l.set_xalign(0.0);
        l.set_wrap(true);
        col.append(&l);
        if !label.is_empty() {
            let chips = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            chips.append(&chip(&label, ""));
            col.append(&chips);
        }
        row.append(&col);
        if previews {
            row.append(&map_box(u, &query)); // always there, so nothing shifts when the map arrives
        } else {
            // no lookups: the maps app searches for the address itself
            let b = card_button("bf-map-pin-symbolic", "Show on a map");
            let win = u.win.clone();
            b.connect_clicked(move |_| {
                let q = crate::mac::url_encode(&query);
                open_map(&win, &format!("geo:0,0?q={q}"), &format!("https://www.openstreetmap.org/search?query={q}"));
            });
            row.append(&b);
        }
        root.append(&row);
    }
    if let Some(b) = birthday {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.append(&gtk::Label::new(Some(&b)));
        row.append(&chip("birthday", ""));
        root.append(&row);
    }
    root
}

// ---------- address maps ----------

fn map_cache_path(u: &Ui, query: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    query.hash(&mut h);
    u.store.image_cache.join(format!("{:016x}.map", h.finish()))
}

/// "lat lon cols rows pin_x pin_y len len...\n" then the PNGs. The file name is a hash, but it still
/// says where a contact lives.
fn map_cache_bytes(d: &crate::mac::MapData) -> Vec<u8> {
    let lens: Vec<String> = d.tiles.iter().map(|t| t.len().to_string()).collect();
    let mut b = format!("{} {} {} {} {} {} {}\n", d.lat, d.lon, d.cols, d.rows, d.pin_x, d.pin_y, lens.join(" ")).into_bytes();
    for t in &d.tiles {
        b.extend_from_slice(t);
    }
    b
}

fn map_view(d: &crate::mac::MapData) -> Option<MapView> {
    let tiles: Vec<gdk::Texture> = d.tiles.iter().filter_map(|t| texture(t)).collect();
    (tiles.len() == d.tiles.len() && !tiles.is_empty()).then(|| MapView { lat: d.lat, lon: d.lon, cols: d.cols, pin_x: d.pin_x, pin_y: d.pin_y, tiles })
}

fn read_map_cache(u: &Ui, query: &str) -> Option<MapView> {
    let b = std::fs::read(map_cache_path(u, query)).ok()?;
    let nl = b.iter().position(|c| *c == b'\n')?;
    let head: Vec<&str> = std::str::from_utf8(&b[..nl]).ok()?.split(' ').collect();
    let num = |i: usize| head.get(i)?.parse::<f64>().ok();
    let (cols, rows) = (num(2)? as usize, num(3)? as usize);
    let mut at = nl + 1;
    let mut tiles = vec![];
    for i in 0..cols * rows {
        let len = num(6 + i)? as usize;
        tiles.push(b.get(at..at + len)?.to_vec());
        at += len;
    }
    map_view(&crate::mac::MapData { lat: num(0)?, lon: num(1)?, cols, rows, pin_x: num(4)?, pin_y: num(5)?, tiles })
}

const MAP_BOX: i32 = 96;

/// A rounded square next to a postal address: a map pin until the map arrives (with a small spinner
/// in the corner while it's being found, or "no preview available" if it can't be), then the map.
/// Clicking opens the address in a maps app, whichever state it's in.
fn map_box(u: &Rc<Ui>, query: &str) -> gtk::Overlay {
    let frame = gtk::Overlay::new();
    frame.add_css_class("map-box");
    frame.set_overflow(gtk::Overflow::Hidden);
    frame.set_size_request(MAP_BOX, MAP_BOX);
    frame.set_valign(gtk::Align::Start);
    // set explicitly: the pin icon below expands, and expand flags would otherwise pass up and
    // stretch the box while it's a placeholder
    frame.set_hexpand(false);
    frame.set_vexpand(false);
    frame.set_halign(gtk::Align::End);
    frame.set_cursor_from_name(Some("pointer"));
    frame.set_tooltip_text(Some("Show on a map"));
    // the box's size comes from this base alone: the map goes in as an overlay, so it can't stretch the box
    let base = gtk::Box::new(gtk::Orientation::Vertical, 0);
    base.set_size_request(MAP_BOX, MAP_BOX);
    // the pin, plus "no preview available" under it if that turns out to be so, centred together
    let group = gtk::Box::new(gtk::Orientation::Vertical, 10);
    group.set_valign(gtk::Align::Center);
    group.set_vexpand(true);
    let pin = gtk::Image::from_icon_name("bf-map-pin-symbolic");
    pin.set_pixel_size(36);
    pin.add_css_class("dim");
    pin.set_halign(gtk::Align::Center);
    group.append(&pin);
    let note = gtk::Label::new(Some("no preview available"));
    note.set_widget_name("map-note");
    note.add_css_class("map-note");
    note.set_wrap(true);
    note.set_justify(gtk::Justification::Center);
    note.set_max_width_chars(12);
    note.set_halign(gtk::Align::Center);
    note.set_visible(false);
    group.append(&note);
    base.append(&group);
    frame.set_child(Some(&base));
    let spin = gtk::Spinner::new();
    spin.set_spinning(true);
    spin.set_widget_name("map-spin");
    spin.set_size_request(12, 12);
    spin.set_halign(gtk::Align::End);
    spin.set_valign(gtk::Align::Start);
    spin.set_margin_top(6);
    spin.set_margin_end(6);
    frame.add_overlay(&spin);
    let attr = gtk::Label::new(Some("© OpenStreetMap"));
    attr.set_widget_name("map-attr");
    attr.add_css_class("map-attr");
    attr.set_halign(gtk::Align::End);
    attr.set_valign(gtk::Align::End);
    attr.set_visible(false);
    frame.add_overlay(&attr);

    let loc: Rc<Cell<Option<(f64, f64)>>> = Rc::default();
    let (win, loc2, q) = (u.win.clone(), loc.clone(), query.to_string());
    let g = gtk::GestureClick::new();
    g.connect_released(move |_, _, _, _| match loc2.get() {
        Some((lat, lon)) => open_map(&win, &format!("geo:{lat},{lon}"), // no q=: apps search for q, and "lat,lon" matches nothing
             &format!("https://www.openstreetmap.org/?mlat={lat}&mlon={lon}#map=17/{lat}/{lon}")),
        None => {
            let q = crate::mac::url_encode(&q); // not found (yet): the maps app searches for the address itself
            open_map(&win, &format!("geo:0,0?q={q}"), &format!("https://www.openstreetmap.org/search?query={q}"));
        }
    });
    frame.add_controller(g);

    let known = u.st.borrow().maps.get(query).cloned();
    let known = known.or_else(|| {
        let v = read_map_cache(u, query)?;
        u.st.borrow_mut().maps.insert(query.to_string(), Some(v.clone()));
        Some(Some(v))
    });
    match known {
        Some(v) => fill_map(&frame, &loc, v.as_ref()),
        None => {
            let first = {
                let mut st = u.st.borrow_mut();
                let w = st.waiting_map.entry(query.to_string()).or_default();
                w.push((frame.downgrade(), loc));
                w.len() == 1
            };
            if first {
                u.send(Cmd::Map(query.to_string()));
            }
        }
    }
    frame
}

/// The map arrived (or couldn't be had): swap it in for the pin and stop the spinner.
fn fill_map(frame: &gtk::Overlay, loc: &Rc<Cell<Option<(f64, f64)>>>, map: Option<&MapView>) {
    fn find(w: &gtk::Widget, name: &str) -> Option<gtk::Widget> {
        let mut c = w.first_child();
        while let Some(ch) = c {
            if ch.widget_name() == name {
                return Some(ch);
            }
            if let Some(f) = find(&ch, name) {
                return Some(f);
            }
            c = ch.next_sibling();
        }
        None
    }
    let part = |name: &str| find(frame.upcast_ref(), name);
    if let Some(w) = part("map-spin") {
        w.set_visible(false);
    }
    let Some(m) = map else {
        if let Some(w) = part("map-note") {
            w.set_visible(true);
        }
        return;
    };
    // the tiles are 256 px; shown at half size, so the 192 px window around the pin fills the box
    let half = 128;
    let (ox, oy) = (m.pin_x / 2.0 - f64::from(MAP_BOX) / 2.0, m.pin_y / 2.0 - f64::from(MAP_BOX) / 2.0);
    let fixed = gtk::Fixed::new();
    for (i, t) in m.tiles.iter().enumerate() {
        let pic = gtk::Picture::for_paintable(t);
        pic.set_can_shrink(true);
        pic.set_size_request(half, half);
        fixed.put(&pic, (i % m.cols) as f64 * f64::from(half) - ox, (i / m.cols) as f64 * f64::from(half) - oy);
    }
    let pin = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    pin.add_css_class("map-pin");
    pin.set_size_request(14, 14);
    fixed.put(&pin, f64::from(MAP_BOX) / 2.0 - 7.0, f64::from(MAP_BOX) / 2.0 - 7.0);
    if let Some(base) = frame.child().and_downcast::<gtk::Box>()
        && let Some(icon) = base.first_child()
    {
        base.remove(&icon);
    }
    frame.add_overlay(&fixed);
    if let Some(w) = part("map-attr") {
        w.set_visible(true);
        frame.remove_overlay(&w); // back on top of the map
        frame.add_overlay(&w);
    }
    loc.set(Some((m.lat, m.lon)));
}

/// The default maps app via a `geo:` link (GNOME Maps, Marble...); the OpenStreetMap site if none takes it.
fn open_map(win: &gtk::ApplicationWindow, geo: &str, web: &str) {
    let (win2, web) = (win.clone(), web.to_string());
    gtk::UriLauncher::new(geo).launch(Some(win), gio::Cancellable::NONE, move |r| {
        if r.is_err() {
            gtk::UriLauncher::new(&web).launch(Some(&win2), gio::Cancellable::NONE, |_| {});
        }
    });
}

/// Open the chat with this number or email, or start a new one to it.
fn message_to(u: &Rc<Ui>, addr: &str) {
    let id = find_direct_chat(&u.st.borrow().chats, addr);
    match id {
        Some(id) => open_chat(u, id),
        None => {
            start_new(u);
            u.to_entry.set_text(addr);
            u.compose.grab_focus();
        }
    }
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
    fn phone_numbers_read_nicely() {
        assert_eq!(pretty_handle("+14035551212"), "+1 (403) 555-1212");
        assert_eq!(pretty_handle("14035551212"), "+1 (403) 555-1212");
        assert_eq!(pretty_handle("4035551212"), "+1 (403) 555-1212");
        assert_eq!(pretty_handle("+1 (403) 555-1212"), "+1 (403) 555-1212");
        assert_eq!(pretty_handle("+447860153932"), "+447860153932"); // not North American: untouched
        assert_eq!(pretty_handle("562528"), "562528");
        assert_eq!(pretty_handle("sue@icloud.com"), "sue@icloud.com");
    }

    #[test]
    fn links_become_markup() {
        assert_eq!(linkify("see https://a.com/x?a=1&b=2, ok <b>"), "see <a href=\"https://a.com/x?a=1&amp;b=2\">https://a.com/x?a=1&amp;b=2</a>, ok &lt;b&gt;");
        assert_eq!(linkify("no link & more"), "no link &amp; more");
    }

    #[test]
    fn url_removed_from_text() {
        let l = |u: &str| crate::mac::LinkPreview { url: u.into(), ..Default::default() };
        assert_eq!(without_url("https://a.com/x", &l("https://a.com/x")), "");
        assert_eq!(without_url("look https://a.com/ nice", &l("https://a.com/")), "look nice");
        assert_eq!(without_url("https://a.com/", &l("https://a.com")), "");
        assert_eq!(without_url("hello", &l("https://a.com")), "hello");
        // the typed address carried tracking parameters; the text holds the clean one
        assert_eq!(without_url("https://www.cabidor.com/", &l("https://www.cabidor.com/?srsltid=AYC")), "");
        // a link to some other site stays
        assert_eq!(without_url("see https://b.com/y", &l("https://a.com/x")), "see https://b.com/y");
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
        let long = "King Will Silicone Wedding Rings For Men,6.6mm Wide 1.8mm Thick Rubber Wedding Bands,Breathable Design,Comfort Fit Size 7-13";
        let c = clip_middle(long, 70);
        assert!(c.starts_with("King Will Silicone") && c.ends_with("Fit Size 7-13") && c.contains('…') && c.chars().count() <= 70, "{c}");
        assert_eq!(clip_middle("short title", 70), "short title");
        let f = clip_middle("Packing_list_with_no_spaces_at_all_but_a_really_long_name_for_the_trip_2026.docx", 48);
        assert!(f.ends_with(".docx") && f.chars().count() <= 48, "{f}");
        assert_eq!(summary(&json!({"text": "hi\nthere", "attachments": []})), "hi");
    }
}

