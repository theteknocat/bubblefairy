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

use crate::mac::{Cmd, Config, Event, Kind, MediaItem};
use crate::theme::{Adjust, LEVEL_MAX, TINT_MAX};
use crate::viewer::is_gif;
use crate::{bubble, viewer};

const SIDEBAR_MIN: i32 = 260; // paned positions and widths below include the panels' side margins
const CHAT_MIN: i32 = 440;
/// Chats asked for at first, and added each time the sidebar is scrolled to its end.
const CHATS_PAGE: usize = 100;
/// how many older messages one request brings (`Cmd::Older`)
const HISTORY_OLDER: usize = 100;
const COLLAPSED_W: i32 = 112;
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
    /// the local copy of the chat list and contacts (see snapshot.rs)
    pub chats_cache: PathBuf,
    pub contacts_cache: PathBuf,
    /// newest message seen in BubbleFairy per chat (`chat_id=message_id` lines)
    pub seen_file: PathBuf,
    /// failed messages that were sent again, one message id per line
    pub resent_file: PathBuf,
    /// pinned chats, one chat identifier per line, in pin order (our own: Messages' pins aren't readable)
    pub pins_file: PathBuf,
    /// muted chats (no notification, no sound), one chat identifier per line (our own: Messages' list is hashed)
    pub muted_file: PathBuf,
    /// previews we made ourselves for plain links (see webpreview.rs), one JSON line per link
    pub web_file: PathBuf,
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
    /// a picture sent again: what the chat already shows of it, and the file name Messages will give it. Not drawn;
    /// it only lets the new message show that picture without fetching it back from the Mac
    Seed(gdk::Texture, String),
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

/// A chat opened on one of its search hits.
struct FindView {
    chat: i64,
    /// which hit (0 = the newest)
    idx: usize,
    /// messages newer than the ones loaded exist (scrolling down fetches them)
    more: bool,
    loading: bool,
    /// scroll to this message once drawn
    goto: Option<String>,
}

#[derive(Default)]
struct State {
    /// preview pictures the Mac couldn't give us: drawn as if the preview had none
    bad_images: HashSet<String>,
    /// the info panel's links so far (the Links tab is rebuilt from these when its width changes)
    info_photos: Vec<MediaItem>,
    info_links: Vec<MediaItem>,
    /// links to YouTube, for the Videos tab
    info_youtube: Vec<MediaItem>,
    /// per tab of the panel (photos, videos, files, links): how many tiles it may show, how many it has drawn, and
    /// whether scrolling to the end should bring more
    info_shown: [usize; 4],
    info_drawn: [usize; 4],
    info_more: [bool; 4],
    /// the chat the panel last showed: another one starts on the first tab
    info_last: Option<i64>,
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
    /// GIF bubbles waiting for the original file (Mac path), and the players of those already moving
    gif_wait: HashMap<String, Vec<glib::WeakRef<bubble::Bubble>>>,
    gifs: HashMap<String, Rc<crate::player::Player>>,
    /// messages we've already set a "look for its link preview" timer for
    preview_watch: HashSet<String>,
    /// how much the window was widened to fit the info panel (and is narrowed by when it closes)
    /// the sidebar width the user has chosen (0: the default); the info panel may squeeze it meanwhile
    side_pref: i32,
    /// the chat the info panel shows (None: panel closed)
    info_chat: Option<i64>,
    /// per chat and kind: where the next page starts, and whether there is one
    media: HashMap<(i64, Kind), Paging>,
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
    /// the chat opened by clicking a search result (it follows later searches; the chat that was open before a search
    /// stays as it was until another is clicked)
    search_pick: Option<i64>,
    /// chats a search put in the list that weren't there (dropped again when the search is cleared)
    search_added: HashSet<i64>,
    /// the open chat was reached from a search and isn't among the loaded chats: kept (row hidden) until scrolling
    /// loads it into the list, like Messages
    off_list: HashSet<i64>,
    textures: HashMap<String, gdk::Texture>,
    /// Mac path -> where to put the copy once it has arrived (a download button was pressed)
    save_to: HashMap<String, Vec<(PathBuf, SaveDone)>>,
    /// message search: the hits (newest first) per chat for the words typed in the sidebar search
    find_hits: HashMap<i64, Vec<Value>>,
    /// how many messages match in each chat (the sidebar's "N matches")
    find_counts: HashMap<i64, usize>,
    /// our own previews for plain links; None = the page gave nothing
    web_links: HashMap<String, Option<crate::mac::LinkPreview>>,
    web_asked: HashSet<String>,
    /// the chat open on a search hit: a window of its history around the hit, not its newest messages
    find_view: Option<FindView>,
    waiting: HashMap<String, Vec<bubble::Bubble>>,
    /// notifications held until their picture has been fetched (key = the picture's Mac path)
    notify_wait: HashMap<String, Vec<(i64, Value)>>,
    /// notifications held until Messages has downloaded the message's file (key = message guid)
    notify_media: HashMap<String, (i64, Value)>,
    /// messages already being re-checked for their files
    media_watch: HashSet<String>,
    /// pictures to ask the Mac for, oldest first; sent newest-first once the chat has been built
    fetch_q: Vec<(String, bool)>,
    /// photo widgets of the open chat in display order (the viewer cycles through these)
    photos: Vec<crate::viewer::Item>,
    /// full-size viewer fetches in flight
    waiting_full: HashSet<String>,
    host_key: String,
    connected: bool,
    chats_loaded: bool,
    /// failed messages that were sent again (their "!" is drawn as an outline, like Messages does)
    resent: HashSet<i64>,
    /// resend token -> the failed message being sent again
    resend_of: HashMap<u64, i64>,
    /// sends (by token) that have taken long enough to say so under their bubbles
    slow: HashSet<u64>,
    /// failed messages being sent again right now (drawn as a ghost with a spinner, in place)
    resending: HashSet<i64>,
    /// the delivery / read receipt of each chat's newest sent message
    receipts: HashMap<i64, crate::mac::Receipt>,
    /// unsent text and files of the chats not on show, by chat id
    drafts: HashMap<i64, (String, Vec<PathBuf>)>,
    /// how many chats are asked for (imsg has no offset); more are asked for as the sidebar is scrolled down
    chat_limit: usize,
    /// the last answer filled the limit, so there may be older chats
    more_chats: bool,
    /// asking for more chats
    more_loading: bool,
    /// chats with a history request in flight (header spinner)
    loading: HashSet<i64>,
    older_loading: bool,
    /// the Mac's contact photos (asked for once per connection) and the decoded pictures
    contacts: Option<crate::mac::Contacts>,
    /// `contacts` came from the local copy and the Mac hasn't sent its own yet
    contacts_cached: bool,
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
    /// the chat list as imsg gave it, before one person's chats were merged into one entry (`merged_chats`)
    raw_chats: Vec<Value>,
    /// chat id -> the id of the merged conversation it belongs to (only for merged ones)
    alias: HashMap<i64, i64>,
    /// the merge last sent to the backend (`Cmd::Merge`)
    merge_sig: Vec<(i64, Vec<i64>)>,
    /// identifiers (`pin_key`) of the muted chats
    muted: Vec<String>,
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
    /// the tray icon while it runs, and what keeps the app alive with no window showing
    tray: RefCell<Option<ksni::Handle<crate::tray::Tray>>>,
    hold: RefCell<Option<gio::ApplicationHoldGuard>>,
    // widgets
    chat_list: gtk::ListBox,
    /// "Retrieving more chats…" under the list while older chats are asked for
    side_more: gtk::Label,
    search: gtk::SearchEntry,
    web: webkit6::soup::Session,
    side_none: gtk::Label,
    /// the empty search result: a spinner over "Searching…" while the messages are being searched, else the text alone
    side_none_box: gtk::Box,
    side_none_spin: gtk::Spinner,
    /// the unread count after the app's title in the header, and the spacer that balances it
    title_badge: (gtk::Label, gtk::Box),
    /// header search: the magnifier button, the field that grows out of it over the title, the layer they share, and the
    /// settings button the field stops short of
    search_btn: gtk::Button,
    search_wrap: gtk::Overlay,
    head_overlay: gtk::Overlay,
    settings_btn: gtk::Button,
    /// 0 = just the button, 1 = the field out over the title; moves toward `search_open`
    search_p: Cell<f64>,
    search_open: Cell<bool>,
    search_tick: Cell<bool>,
    /// on the search field's right end while a message search runs
    search_spin: gtk::Spinner,
    /// bumped by each keystroke / search request: late answers and stale timers are dropped
    msearch_gen: Cell<u64>,
    /// bumped when a jump to a search hit starts, and when the user scrolls: the jump stops holding its place
    goto_gen: Cell<u64>,
    /// rows' texts are showing search results (so they must be put back when the search ends)
    styled: Cell<bool>,
    /// above the messages while a chat is open on a search hit
    find_bar: gtk::Box,
    find_label: gtk::Label,
    find_older: gtk::Button,
    find_newer: gtk::Button,
    /// the pinned chats above the list
    tiles: crate::tilegrid::TileGrid,
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
    /// "Retrieving messages…" in the middle of the empty message area while a chat's first messages load
    msgs_loading: gtk::Box,
    head_titles: gtk::Box,
    to_row: gtk::Box,
    to_entry: gtk::Entry,
    suggest: gtk::ListBox,
    suggest_scroll: gtk::ScrolledWindow,
    status: gtk::Button,
    /// what the pill says (it shows only a light): the tooltip and the popover title
    status_title: RefCell<String>,
    /// a short-lived message over the top of the window (`show_toast`)
    toast: gtk::Revealer,
    toast_text: gtk::Label,
    toast_gen: Cell<u32>,
    /// what's wrong with the connection right now, by kind (`set_issue`); listed when the status pill is clicked
    issues: RefCell<Vec<(&'static str, String)>>,
    /// unix seconds: when the Mac connection came up (None while it's down), and when we last heard from it
    since: Cell<Option<i64>>,
    heard: Cell<Option<i64>>,
    /// the status popover while it's open, redrawn when the status changes (`fill_status_pop`)
    status_pop: RefCell<Option<gtk::Popover>>,
    /// the reason that goes with the status (why it's offline, say); the pill itself shows only a light
    status_tip: RefCell<String>,
    // flags poked from signal handlers, so kept out of the RefCell
    stick_bottom: Rc<Cell<bool>>,
    /// when not pinned to the bottom, keep this distance from the bottom while content changes above
    selecting: Cell<bool>,
    /// a `render_chats_soon` is waiting
    list_queued: Cell<bool>,
    /// a save of the local copy is waiting for its timer
    snap_queued: Cell<bool>,
    /// every time shown in a label, to be rewritten when the day changes (`track_time`)
    time_labels: RefCell<Vec<TimeLabel>>,
    /// the day the tracked times were last written for
    time_day: Cell<Option<(i32, i32, i32)>>,
    /// the open chat as `render_conv` last drew it: what it was drawn from, and each message's widgets (`redraw_message`)
    conv_ctx: RefCell<Option<Rc<ConvCtx>>>,
    msg_widgets: RefCell<Vec<Vec<gtk::Widget>>>,
    sel_gen: Cell<u32>,
    /// has this run connected to the Mac at least once (tells a lost connection from one that never came up)
    ever_connected: Cell<bool>,
    /// failed connection attempts in a row
    fails: Cell<u32>,
    chats_queued: Cell<bool>,
    title_queued: Cell<bool>,
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

/// "Bubble" in blue and "Fairy" in pink.
fn wordmark() -> gtk::Box {
    let name = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    for (text, class) in [("Bubble", "brand-b"), ("Fairy", "brand-f")] {
        let l = gtk::Label::new(Some(text));
        l.add_css_class("app-title");
        l.add_css_class(class);
        name.append(&l);
    }
    name
}

/// The header's name: the bubble-and-wand mark, then the wordmark.
pub(crate) fn brand() -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let icon = gtk::Image::from_icon_name("bf-mark");
    icon.set_pixel_size(32);
    b.append(&icon);
    b.append(&wordmark());
    b
}

pub fn build(
    app: &gtk::Application,
    store: Store,
    cfg: Config,
    cmd: mpsc::UnboundedSender<Cmd>,
    mut ev: mpsc::UnboundedReceiver<Event>,
) {
    let win = gtk::ApplicationWindow::builder()
        .application(app)
        .title("BubbleFairy")
        .default_width(1120)
        .default_height(740)
        .build();
    win.add_css_class("bf");

    // header: status pill, title, settings
    let head = gtk::HeaderBar::new();
    head.add_css_class("bf-head");
    let title = brand();
    // the unread count sits in a round badge after the title; a spacer of the same width before it keeps the title
    // centred (they share a width)
    let title_badge = gtk::Label::new(None);
    title_badge.add_css_class("title-badge");
    title_badge.set_visible(false);
    title_badge.set_valign(gtk::Align::Center);
    let title_gap = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    title_gap.set_visible(false);
    let badge_size = gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal);
    badge_size.add_widget(&title_badge);
    badge_size.add_widget(&title_gap);
    let title_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    title_box.append(&title_gap);
    title_box.append(&title);
    title_box.append(&title_badge);
    head.set_title_widget(Some(&title_box));
    let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    dot.add_css_class("dot");
    dot.set_valign(gtk::Align::Center);
    let pill = gtk::Box::new(gtk::Orientation::Horizontal, 7);
    pill.set_halign(gtk::Align::Center);
    pill.append(&dot);
    let status = gtk::Button::builder()
        .child(&pill)
        .css_classes(["status", "connecting"])
        .build();
    status.set_tooltip_text(Some("Starting"));
    status.set_valign(gtk::Align::Center);
    head.pack_start(&status);
    let settings_btn = gtk::Button::from_icon_name("bf-settings-symbolic");
    settings_btn.set_tooltip_text(Some("Settings"));
    settings_btn.add_css_class("head-btn");
    head.pack_end(&settings_btn);
    let search_btn = gtk::Button::from_icon_name("bf-search-symbolic");
    search_btn.set_tooltip_text(Some("Search (Ctrl+F)"));
    search_btn.add_css_class("head-btn");
    head.pack_start(&search_btn);
    // the field lives in a layer over the whole header, placed over the title between the status pill and the settings
    // button (a header bar centres its title and can't give it that space)
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search conversations")
        .build();
    search.add_css_class("head-search");
    search.set_valign(gtk::Align::Center);
    let search_spin = gtk::Spinner::new();
    search_spin.set_halign(gtk::Align::End);
    search_spin.set_valign(gtk::Align::Center);
    search_spin.set_margin_end(38);
    search_spin.set_can_target(false);
    search_spin.set_visible(false);
    // always there while the field is out, empty or not (the field's own clear icon is hidden in the CSS)
    let search_close = gtk::Button::from_icon_name("bf-close-symbolic");
    search_close.set_tooltip_text(Some("Close search (Esc)"));
    search_close.add_css_class("head-btn");
    search_close.add_css_class("search-close");
    search_close.set_halign(gtk::Align::End);
    search_close.set_valign(gtk::Align::Center);
    search_close.set_margin_end(3);
    let search_wrap = gtk::Overlay::new();
    search_wrap.set_child(Some(&search));
    search_wrap.add_overlay(&search_spin);
    search_wrap.add_overlay(&search_close);
    search_wrap.set_halign(gtk::Align::Start);
    search_wrap.set_valign(gtk::Align::Center);
    search_wrap.set_visible(false);
    search_wrap.set_opacity(0.0);
    let head_overlay = gtk::Overlay::new();
    head_overlay.set_child(Some(&head));
    head_overlay.add_overlay(&search_wrap);
    let search_close_btn = search_close.clone();
    win.set_titlebar(Some(&head_overlay));

    // sidebar
    let chat_list = gtk::ListBox::new();
    chat_list.add_css_class("chat-list");
    let tiles = crate::tilegrid::TileGrid::new();
    tiles.add_css_class("tiles");
    tiles.set_visible(false);
    let side_inner = gtk::Box::new(gtk::Orientation::Vertical, 0);
    side_inner.append(&tiles);
    side_inner.append(&chat_list);
    let side_more = gtk::Label::new(Some("Retrieving more chats…"));
    side_more.add_css_class("loading-text");
    side_more.set_margin_top(12);
    side_more.set_margin_bottom(16);
    side_more.set_visible(false);
    side_inner.append(&side_more);
    // the list's own placeholder never showed for a filtered list
    let side_none = gtk::Label::new(Some("No conversations found"));
    side_none.add_css_class("loading-text");
    side_none.set_wrap(true);
    side_none.set_justify(gtk::Justification::Center);
    let side_none_spin = gtk::Spinner::new();
    side_none_spin.set_size_request(24, 24);
    side_none_spin.set_visible(false);
    let side_none_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    side_none_box.set_halign(gtk::Align::Center);
    side_none_box.set_margin_top(24);
    side_none_box.append(&side_none_spin);
    side_none_box.append(&side_none);
    side_none_box.set_visible(false);
    side_inner.append(&side_none_box);
    let side_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&side_inner)
        .build();
    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar.add_css_class("sidebar");
    let side_title = gtk::Label::new(Some("Conversations"));
    side_title.add_css_class("side-title");
    side_title.set_xalign(0.0);
    side_title.set_valign(gtk::Align::Center);
    let new_btn = gtk::Button::from_icon_name("bf-compose-symbolic");
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
    let sl = gtk::Label::new(Some("Retrieving chats…"));
    sl.add_css_class("loading-text");
    side_loading.append(&sl);
    let side_stack = gtk::Stack::new();
    side_stack.set_transition_type(gtk::StackTransitionType::None); // a fade froze half way while the rows were built
    side_stack.add_named(&side_loading, Some("loading"));
    side_stack.add_named(&side_scroll, Some("list"));
    side_stack.set_vexpand(true);
    side_stack.set_hhomogeneous(false); // else the "Retrieving chats…" text keeps the collapsed sidebar wide
    sidebar.append(&side_stack);

    // conversation
    let head_avatar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let head_title = gtk::Label::new(None);
    head_title.add_css_class("conv-title");
    head_title.set_xalign(0.0);
    head_title.set_selectable(true);
    head_title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let head_sub = gtk::Label::new(None);
    head_sub.add_css_class("conv-sub");
    head_sub.set_xalign(0.0);
    head_sub.set_selectable(true);
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
    let to_entry = gtk::Entry::builder()
        .placeholder_text("Name, phone number or email")
        .hexpand(true)
        .build();
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
    let suggest_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&suggest)
        .build();
    suggest_scroll.set_visible(false);

    let msgs = gtk::Box::new(gtk::Orientation::Vertical, 0);
    msgs.add_css_class("msgs");
    msgs.set_valign(gtk::Align::End);
    // anchored to the bottom (see chatscroll.rs); `stick` is true while the view follows the newest message
    let stick = Rc::new(Cell::new(true));
    let anchored = crate::chatscroll::ChatScroll::new(&msgs, stick.clone());
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&anchored)
        .build();

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
        .child(&compose)
        .hexpand(true)
        .build();
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
        let layout =
            cv.create_pango_layout(Some(&buf.text(&buf.start_iter(), &buf.end_iter(), false)));
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
        cs.set_vscrollbar_policy(if need > COMPOSE_MAX {
            gtk::PolicyType::Automatic
        } else {
            gtk::PolicyType::External
        });
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
    compose_scroll
        .vadjustment()
        .connect_value_changed(move |a| {
            if fits.get() && a.value() != 0.0 {
                a.set_value(0.0);
            }
        });
    let f = fit.clone();
    compose_scroll
        .hadjustment()
        .connect_page_size_notify(move |_| f()); // width changed: rewrap
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
    compose
        .buffer()
        .connect_changed(move |b| hint.set_visible(b.char_count() == 0));
    compose_box.set_valign(gtk::Align::Center);
    let emoji = gtk::MenuButton::new();
    let smile = gtk::Image::from_icon_name("bf-smile-symbolic");
    smile.set_pixel_size(20);
    emoji.set_child(Some(&smile));
    emoji.set_tooltip_text(Some("Emoji"));
    emoji.add_css_class("emoji-btn");
    emoji.set_valign(gtk::Align::End);
    let chooser = gtk::EmojiChooser::new();
    emoji.set_popover(Some(&chooser));
    let buf = compose.buffer();
    chooser.connect_emoji_picked(move |_, e| buf.insert_at_cursor(e));
    let send_btn = gtk::Button::from_icon_name("bf-up-symbolic");
    send_btn.add_css_class("send-btn");
    send_btn.set_sensitive(false); // enabled once there's something to send
    send_btn.set_tooltip_text(Some("Send (Enter)"));
    send_btn.set_valign(gtk::Align::End);
    let composer = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    composer.add_css_class("composer");
    let attach_btn = gtk::Button::from_icon_name("bf-attach-symbolic");
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
    // while a chat is open on a search hit: which hit, step through them, back to the latest messages
    let find_label = gtk::Label::new(None);
    find_label.set_xalign(0.0);
    find_label.set_hexpand(true);
    find_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let find_older = gtk::Button::from_icon_name("bf-left-symbolic");
    find_older.set_tooltip_text(Some("Older match"));
    let find_newer = gtk::Button::from_icon_name("bf-right-symbolic");
    find_newer.set_tooltip_text(Some("Newer match"));
    let find_latest = gtk::Button::with_label("Latest");
    find_latest.set_tooltip_text(Some("Back to the newest messages"));
    let find_close = gtk::Button::from_icon_name("bf-close-symbolic");
    find_close.set_tooltip_text(Some("Close search"));
    let find_bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    find_bar.add_css_class("find-bar");
    for w in [
        find_label.upcast_ref::<gtk::Widget>(),
        find_older.upcast_ref(),
        find_newer.upcast_ref(),
        find_latest.upcast_ref(),
        find_close.upcast_ref(),
    ] {
        find_bar.append(w);
    }
    for b in [&find_older, &find_newer, &find_latest, &find_close] {
        b.add_css_class("flat");
    }
    find_bar.set_visible(false);
    conv.append(&find_bar);
    conv.append(&suggest_scroll);
    // a round button over the bottom of the messages that jumps back down once you're a panel's height or more up
    let to_bottom = gtk::Button::from_icon_name("bf-down-symbolic");
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
    let ml = gtk::Label::new(Some("Retrieving messages…"));
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
    scroller
        .vadjustment()
        .connect_value_changed(show_btn.clone());
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

    // a short message over the top of the window, for what went wrong with something just done
    let toast_text = gtk::Label::new(None);
    toast_text.set_wrap(true);
    toast_text.set_max_width_chars(60);
    let toast_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    toast_box.add_css_class("toast");
    toast_box.append(&toast_text);
    let toast = gtk::Revealer::builder()
        .child(&toast_box)
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Start)
        .build();

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
    stage.add_overlay(&toast);
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

    let mut cfg = cfg;
    if std::env::var_os("BF_LINKS").is_some() {
        cfg.link_previews = true; // dev aid
    }
    let web_links = load_web_links(&store.web_file);
    let web = webkit6::soup::Session::new();
    webkit6::soup::prelude::SessionExt::set_timeout(&web, 10);
    webkit6::soup::prelude::SessionExt::set_user_agent(
        &web,
        "Mozilla/5.0 (compatible; facebookexternalhit/1.1; BubbleFairy)",
    ); // the form sites expect from link previewers
    let seen = load_seen(&store.seen_file);
    let resent: HashSet<i64> = std::fs::read_to_string(&store.resent_file)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.parse().ok())
        .collect();
    let pins: Vec<String> = std::fs::read_to_string(&store.pins_file)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    let muted: Vec<String> = std::fs::read_to_string(&store.muted_file)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    // dev: BF_PINS=<key>,<key> pins those chats (demo chats' keys are their participants)
    let pins = std::env::var("BF_PINS")
        .map(|v| v.split(';').map(String::from).collect())
        .unwrap_or(pins);
    let ui = Rc::new(Ui {
        app: app.clone(),
        win: win.clone(),
        store,
        cmd,
        viewer,
        web,
        st: RefCell::new(State {
            cfg,
            seen,
            pins,
            muted,
            chat_limit: CHATS_PAGE,
            resent,
            web_links,
            ..Default::default()
        }),
        chat_list,
        side_more,
        search,
        side_none,
        side_none_box,
        side_none_spin,
        title_badge: (title_badge, title_gap),
        tray: RefCell::new(None),
        hold: RefCell::new(None),
        search_btn,
        search_wrap,
        head_overlay,
        settings_btn: settings_btn.clone(),
        search_p: Cell::new(0.0),
        search_open: Cell::new(false),
        search_tick: Cell::new(false),
        search_spin,
        msearch_gen: Cell::new(0),
        goto_gen: Cell::new(0),
        styled: Cell::new(false),
        find_bar,
        find_label,
        find_older,
        find_newer,
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
        status_title: RefCell::new("Starting".into()),
        toast,
        toast_text,
        stick_bottom: stick.clone(),
        selecting: Cell::new(false),
        list_queued: Cell::new(false),
        snap_queued: Cell::new(false),
        time_labels: RefCell::default(),
        time_day: Cell::new(glib::DateTime::now_local().map(|n| n.ymd()).ok()),
        conv_ctx: RefCell::default(),
        msg_widgets: RefCell::default(),
        sel_gen: Cell::new(0),
        toast_gen: Cell::new(0),
        issues: RefCell::new(Vec::new()),
        since: Cell::new(None),
        heard: Cell::new(None),
        status_pop: RefCell::new(None),
        status_tip: RefCell::new(String::new()),
        ever_connected: Cell::new(false),
        fails: Cell::new(0),
        chats_queued: Cell::new(false),
        title_queued: Cell::new(false),
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
    settings_btn.connect_clicked(move |_| settings(&u));
    let u = ui.clone();
    ui.status.connect_clicked(move |_| status_clicked(&u));
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

    // search: rows that don't match are filtered out; pinned chats' tiles give way to their (otherwise hidden) rows
    let w = Rc::downgrade(&ui);
    ui.chat_list
        .set_filter_func(move |row| w.upgrade().is_none_or(|u| row_matches(&u, row)));
    let u = ui.clone();
    ui.search
        .connect_search_changed(move |_| search_changed(&u));
    for (b, step) in [(&ui.find_older, 1_i64), (&ui.find_newer, -1)] {
        let u = ui.clone();
        b.connect_clicked(move |_| step_find(&u, step));
    }
    for b in [&find_latest, &find_close] {
        let u = ui.clone();
        b.connect_clicked(move |_| end_find(&u));
    }
    let search_action = gio::SimpleAction::new("search", None);
    let u = ui.clone();
    search_action.connect_activate(move |_, _| open_search(&u));
    let u = ui.clone();
    ui.search_btn.connect_clicked(move |_| open_search(&u));
    let u = ui.clone();
    ui.search.connect_stop_search(move |_| close_search(&u)); // Esc
    let u = ui.clone();
    search_close_btn.connect_clicked(move |_| close_search(&u));
    // the magnifier inside the field sits where the header button was: pressing it again closes the field too
    let g = gtk::GestureClick::new();
    g.set_propagation_phase(gtk::PropagationPhase::Capture);
    let u = ui.clone();
    g.connect_pressed(move |g, _, x, _| {
        if x < 34.0 {
            g.set_state(gtk::EventSequenceState::Claimed);
            close_search(&u);
        }
    });
    ui.search.add_controller(g);
    // focus leaves an empty field: back to the button (not when the whole window lost the focus)
    let fc = gtk::EventControllerFocus::new();
    let u = ui.clone();
    fc.connect_leave(move |_| {
        if u.search.text().is_empty() && u.search_open.get() && u.win.is_active() {
            search_to(&u, false);
        }
    });
    ui.search.add_controller(fc);
    app.add_action(&search_action);
    app.set_accels_for_action("app.search", &["<Control>f"]);

    if let Ok(q) = std::env::var("BF_SEARCH") {
        let u = ui.clone();
        glib::timeout_add_local_once(Duration::from_millis(5000), move || {
            if q.is_empty() {
                open_search(&u)
            } else {
                u.search.set_text(&q)
            }
        }); // dev aid (after the connection is up; empty = just open the field)
    }
    if let Ok(id) = std::env::var("BF_FIND_OPEN").map(|v| v.parse::<i64>().unwrap_or(1)) {
        // dev aid: open this chat from the search results, 9 s in
        let u = ui.clone();
        glib::timeout_add_local_once(Duration::from_millis(9000), move || select(&u, id));
    }

    if let Ok(name) = std::env::var("BF_SOUND") {
        // dev aid: play this sound (a bundled name or file:<path>) 2 s in
        glib::timeout_add_local_once(Duration::from_millis(2000), move || {
            crate::sound::play(&name)
        });
    }
    if let Ok(ms) = std::env::var("BF_SIDE_END").map(|v| v.parse::<u64>().unwrap_or(9000)) {
        // dev aid: scroll the sidebar to its end after this many ms
        let u = ui.clone();
        glib::timeout_add_local_once(Duration::from_millis(ms), move || {
            if let Some(a) = side_adjustment(&u) {
                a.set_value(a.upper());
            }
        });
    }
    // dev aid BF_TILE_DRAG=<ms>: drag the first tile two slots to the right at that time, hold, let go 3 s later
    if let Some(ms) = std::env::var("BF_TILE_DRAG")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        let u = ui.clone();
        glib::timeout_add_local_once(Duration::from_millis(ms), move || u.tiles.dev_drag(0, 2.0));
    }
    // pinned tiles dragged into a new order are saved that way
    let u = ui.clone();
    ui.tiles
        .connect_reordered(move |order| reorder_pins(&u, order));
    // scrolled to the end of the list: ask for older chats
    if let Some(adj) = side_adjustment(&ui) {
        let u = ui.clone();
        adj.connect_value_changed(move |a| {
            // halfway through the newest page of chats (about CHATS_PAGE rows) counts as near the end
            let loaded = u.st.try_borrow().map_or(0, |st| st.chats.len()).max(1);
            let page_h = a.upper() * (CHATS_PAGE as f64 / loaded as f64).min(1.0);
            if a.upper() > a.page_size()
                && a.value() + a.page_size() >= a.upper() - (page_h / 2.0).max(300.0)
            {
                load_more_chats(&u);
            }
        });
    }

    let u = ui.clone();
    ui.chat_list.connect_row_selected(move |_, row| {
        if u.selecting.get() {
            return;
        }
        let Some(row) = row else { return };
        let id =
            u.st.borrow()
                .chats
                .get(row.index() as usize)
                .and_then(|c| c["id"].as_i64());
        if let Some(id) = id {
            select(&u, id);
            // the click itself gives the row the focus once the selection is done: take it back for the composer
            let u2 = u.clone();
            glib::idle_add_local_once(move || {
                u2.compose.grab_focus();
            });
        }
    });

    // the user's own scrolling ends a jump to a search hit (see show_goto)
    let wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    wheel.set_propagation_phase(gtk::PropagationPhase::Capture);
    let u = ui.clone();
    wheel.connect_scroll(move |_, _, _| {
        u.goto_gen.set(u.goto_gen.get().wrapping_add(1));
        glib::Propagation::Proceed
    });
    ui.scroller.add_controller(wheel);

    // scrolled near the top: fetch older messages (the view itself stays put, see chatscroll.rs)
    let u = ui.clone();
    ui.scroller.vadjustment().connect_value_changed(move |a| {
        // like Messages: older messages come once the view is halfway through the newest page loaded above
        let loaded =
            u.st.try_borrow()
                .ok()
                .and_then(|st| Some(st.hist.get(&st.sel?)?.len()))
                .unwrap_or(0)
                .max(1);
        let page_h = a.upper() * (HISTORY_OLDER as f64 / loaded as f64).min(1.0);
        if a.value() < (page_h / 2.0).max(400.0) && a.upper() > a.page_size() {
            load_older(&u);
        }
        if a.upper() - a.value() - a.page_size() < 400.0 && a.upper() > a.page_size() {
            load_newer(&u);
        }
    });

    let u = ui.clone();
    ui.compose
        .buffer()
        .connect_changed(move |_| update_send(&u));
    let u = ui.clone();
    attach_btn.connect_clicked(move |_| pick_files(&u));
    let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
    let u = ui.clone();
    drop.connect_drop(move |_, v, _, _| {
        let Ok(list) = v.get::<gdk::FileList>() else {
            return false;
        };
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
                if let Ok(v) = cb
                    .read_value_future(gdk::FileList::static_type(), glib::Priority::DEFAULT)
                    .await
                    && let Ok(list) = v.get::<gdk::FileList>()
                {
                    add_files(&u, list.files().iter().filter_map(|f| f.path()).collect());
                }
            });
        } else if f.contains_type(gdk::Texture::static_type()) && !f.contain_mime_type("text/plain")
        {
            tv.stop_signal_emission_by_name("paste-clipboard");
            let u = u.clone();
            glib::spawn_future_local(async move {
                let Ok(Some(t)) = cb.read_texture_future().await else {
                    return;
                };
                let dir = outgoing_dir(&u);
                let _ = std::fs::create_dir_all(&dir);
                let path = dir.join(format!(
                    "Pasted image {}.png",
                    glib::DateTime::now_local()
                        .map(|d| d.format("%Y-%m-%d %H.%M.%S").unwrap_or_default())
                        .unwrap_or_default()
                ));
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
        let Some(u) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
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
    // dev aid BF_PROF=1: report when the GTK thread was busy for 250 ms or more, and what the app holds
    if std::env::var_os("BF_PROF").is_some() {
        let last = Cell::new(std::time::Instant::now());
        glib::timeout_add_local(Duration::from_millis(50), move || {
            let late = last
                .replace(std::time::Instant::now())
                .elapsed()
                .as_millis();
            if late > 300 {
                eprintln!("prof: GTK thread stalled {} ms", late - 50);
            }
            glib::ControlFlow::Continue
        });
    }
    ui.win.connect_is_active_notify(move |_| {
        // back from a suspend that spanned midnight: the timer is late
        if u.time_day.get() != glib::DateTime::now_local().map(|n| n.ymd()).ok() {
            refresh_times(&u);
        }
        mark_seen(&u);
        if u.win.is_active() && u.st.borrow().connected {
            u.send(Cmd::ContactsCheck); // photos changed on the Mac while the window was away
        }
    });
    // ... and while it is in front
    let w = Rc::downgrade(&ui);
    glib::timeout_add_seconds_local(60, move || {
        let Some(u) = w.upgrade() else {
            return glib::ControlFlow::Break;
        };
        if u.win.is_active() && u.st.borrow().connected {
            u.send(Cmd::ContactsCheck);
        }
        glib::ControlFlow::Continue
    });
    schedule_midnight(&ui);
    // receipts (delivered, read) don't come through the live stream either: look again while a chat is open
    let w = Rc::downgrade(&ui);
    glib::timeout_add_seconds_local(10, move || {
        let Some(u) = w.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let sel = u.st.borrow().sel.filter(|_| u.st.borrow().connected);
        if let Some(id) = sel.filter(|_| u.win.is_active()) {
            u.send(Cmd::Receipts(id));
        }
        glib::ControlFlow::Continue
    });
    // read state changes (chats read on other devices) don't come through the live stream
    let w = Rc::downgrade(&ui);
    glib::timeout_add_seconds_local(60, move || {
        let Some(u) = w.upgrade() else {
            return glib::ControlFlow::Break;
        };
        if u.st.borrow().connected {
            u.send(Cmd::Chats(u.st.borrow().chat_limit));
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
            let t0 = std::time::Instant::now();
            let kind = format!("{:?}", std::mem::discriminant(&e));
            on_event(&u, e);
            if std::env::var_os("BF_PROF").is_some() && t0.elapsed().as_millis() > 40 {
                eprintln!("prof: event {kind} took {} ms", t0.elapsed().as_millis());
            }
        }
    });

    // Follow the system light/dark setting (GTK fills this from the desktop's portal).
    let gs = gtk::Settings::default().expect("settings");
    let a = app.clone();
    let apply = move |s: &gtk::Settings| {
        let dark = s.gtk_interface_color_scheme() == gtk::InterfaceColorScheme::Dark;
        for w in a.windows() {
            if dark {
                w.add_css_class("dark")
            } else {
                w.remove_css_class("dark")
            }
        }
    };
    // the saved choice, if any; "system" leaves GTK (and the BF_DARK / BF_LIGHT switches) alone
    let scheme = ui.st.borrow().cfg.color_scheme;
    if scheme != 0 {
        set_scheme(scheme);
    }
    apply(&gs);
    let a2 = apply.clone();
    gs.connect_gtk_interface_color_scheme_notify(move |s| a2(s));
    let a4 = apply.clone();
    app.connect_window_added(move |_, _| {
        if let Some(settings) = gtk::Settings::default() {
            a4(&settings);
        }
    });

    // a click outside the info panel closes it (the "i" button itself toggles; popovers and the viewer keep it)
    let click = gtk::GestureClick::new();
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    let u = ui.clone();
    click.connect_pressed(move |_, _, x, y| {
        if !u.info.revealer.reveals_child() || u.viewer.is_open() {
            return;
        }
        let Some(hit) = u.win.pick(x, y, gtk::PickFlags::DEFAULT) else {
            return;
        };
        let inside = |w: &gtk::Widget| hit == *w || hit.is_ancestor(w);
        if !inside(u.info.panel.upcast_ref())
            && !inside(u.info.btn.upcast_ref())
            && hit.ancestor(gtk::Popover::static_type()).is_none()
        {
            close_info(&u);
        }
    });
    ui.win.add_controller(click);
    // viewer keys: at window level so they work whatever has focus
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let u = ui.clone();
    keys.connect_key_pressed(move |_, key, _, _| {
        if u.viewer.is_open() && u.viewer.key(key) {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    win.add_controller(keys);
    // Esc closes the search bar over a chat (in the bubble phase: a popover or the viewer that wants it gets it first)
    let esc_find = gtk::EventControllerKey::new();
    let u = ui.clone();
    esc_find.connect_key_pressed(move |_, key, _, _| {
        if key == gdk::Key::Escape && u.st.borrow().find_view.is_some() && !u.viewer.is_open() {
            end_find(&u);
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    win.add_controller(esc_find);
    let w = Rc::downgrade(&ui);
    ui.viewer.set_hooks(viewer::Hooks {
        fetch_full: Box::new(move |p| {
            if let Some(u) = w.upgrade() {
                fetch_full(&u, p)
            }
        }),
        fetch_video: Box::new({
            let w = Rc::downgrade(&ui);
            move |p| {
                if let Some(u) = w.upgrade() {
                    fetch_video(&u, p)
                }
            }
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
            move |p| {
                if let Some(u) = w.upgrade() {
                    open_file(&u, p)
                }
            }
        }),
    });
    let u = ui.clone();
    ui.scroller
        .hadjustment()
        .connect_page_size_notify(move |_| {
            let fs: Vec<_> = u.stacks.borrow().clone();
            for f in fs {
                f();
            }
        });

    // first run: only the setup wizard shows; the main window follows when it closes
    let first_run = ui.st.borrow().cfg.host.is_empty()
        || std::env::var_os("BF_SETUP").is_some()
        || std::env::var_os("BF_SETUP_TEST").is_some();
    let login = std::env::args().any(|a| a == "--tray");
    start_tray(&ui, login); // a login start stays hidden if a tray turns up
    if !first_run {
        load_snapshot(&ui);
    }
    if !first_run && !login {
        win.present();
    }
    // with the tray running, closing the window only hides it
    let u = ui.clone();
    win.connect_close_request(move |w| {
        if u.tray.borrow().is_none() {
            return glib::Propagation::Proceed;
        }
        w.set_visible(false);
        tray_hint(&u);
        glib::Propagation::Stop
    });
    // Ctrl+Q quits for real, even with the tray keeping the app alive
    let quit = gio::SimpleAction::new("quit", None);
    let a = ui.app.clone();
    quit.connect_activate(move |_, _| a.quit());
    ui.app.add_action(&quit);
    ui.app.set_accels_for_action("app.quit", &["<Control>q"]);
    if let Ok(t) = std::env::var("BF_TYPE") {
        // dev aid: type this into the composer one character every 25 ms (`\\n` = Shift+Enter)
        let chars: Vec<char> = t.replace("\\n", "\n").chars().collect();
        let (i, c) = (Cell::new(0), ui.compose.clone());
        glib::timeout_add_local(Duration::from_millis(25), move || {
            let Some(ch) = chars.get(i.get()) else {
                return glib::ControlFlow::Break;
            };
            c.buffer().insert_at_cursor(&ch.to_string());
            i.set(i.get() + 1);
            glib::ControlFlow::Continue
        });
    }
    if let Ok(t) = std::env::var("BF_DRAFT") {
        ui.compose.buffer().set_text(&t.replace("\\n", "\n")); // dev aid: prefill the composer
    }
    if let Some(n) = std::env::var("BF_TOP")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
    {
        // dev aid: scroll to the top n times, 1.5 s apart (each loads an older page)
        let (u, left) = (ui.clone(), Cell::new(n));
        glib::timeout_add_local(Duration::from_millis(1500), move || {
            u.stick_bottom.set(false);
            u.scroller.vadjustment().set_value(0.0);
            left.set(left.get().saturating_sub(1));
            if left.get() == 0 {
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }
    if let Some(ms) = std::env::var("BF_LINK")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        // dev aid: every `ms`, scroll the open chat to its first link card with a picture
        let u = ui.clone();
        glib::timeout_add_local(Duration::from_millis(ms), move || {
            // BF_LINK_TITLE=<text> picks the card whose title has that text; else the first with a big picture
            fn has_text(w: &gtk::Widget, needle: &str) -> bool {
                if w.downcast_ref::<gtk::Label>()
                    .is_some_and(|l| l.text().contains(needle))
                {
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
                if w.has_css_class("linkcard")
                    && title.map_or(w.has_css_class("fixed"), |t| has_text(w, t))
                {
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
        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
            add_files(&u, t.split(':').map(PathBuf::from).collect())
        });
    }
    if let Ok(t) = std::env::var("BF_NEW") {
        // dev aid: open the new-message view with this typed in the To field
        let u = ui.clone();
        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
            start_new(&u);
            u.to_entry.set_text(&t);
        });
    }
    // dev aid: BF_CARD=<ms> opens the header avatar's popover for the open chat (the contact card, or the people list
    // of a big group); with BF_POPSHOT=<png> it is drawn to a file
    if let Some(ms) = std::env::var("BF_CARD")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        let u = ui.clone();
        glib::timeout_add_local_once(Duration::from_millis(ms), move || {
            let chat = {
                let st = u.st.borrow();
                st.chats
                    .iter()
                    .find(|c| c["id"].as_i64() == st.sel)
                    .cloned()
            };
            let Some(c) = chat else { return };
            let people: Vec<String> = c["participants"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p.as_str().map(String::from))
                .collect();
            let avatar = u.head_avatar.clone().upcast();
            if c["is_group"].as_bool() == Some(true) && people.len() > BIG_GROUP {
                show_people(&u, &avatar, &people);
            } else {
                let handle = people.first().cloned().unwrap_or_default();
                let name = c["contact_name"].as_str().unwrap_or(&handle).to_string();
                show_card(&u, &avatar, &handle, &name, Rc::new(|_| {}));
            }
            popshot_soon(&u);
        });
    }
    // dev aid: BF_ISSUE=<ms> flags a connection problem then and opens the pill's popover; BF_TOAST=<ms> shows a toast
    for (var, toast) in [("BF_ISSUE", false), ("BF_TOAST", true)] {
        if let Some(ms) = std::env::var(var).ok().and_then(|v| v.parse::<u64>().ok()) {
            let u = ui.clone();
            glib::timeout_add_local_once(Duration::from_millis(ms), move || {
                if toast {
                    show_toast(&u, "Message not sent: the Mac didn't answer");
                } else {
                    set_issue(&u, "live", Some("Live updates unavailable: demo".into()));
                    status_clicked(&u);
                    popshot_soon(&u);
                }
            });
        }
    }
    if let Some(ms) = std::env::var("BF_INFO")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        // dev aid: open the info panel after <ms> (e.g. with BF_CHAT, in a headless compositor)
        let u = ui.clone();
        glib::timeout_add_local_once(Duration::from_millis(ms), move || {
            open_info(&u);
            // BF_TAB=videos|files|links: show that tab
            let n = match std::env::var("BF_TAB").as_deref() {
                Ok("videos") => 1,
                Ok("files") => 2,
                Ok("links") => 3,
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
    if let Some(ms) = std::env::var("BF_SETTINGS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        // dev aid: open Settings after <ms>; BF_SETTINGS_TAB=<section> opens on it, BF_SETTINGS_SHOT=<png> saves the dialog 1.5 s later
        let u = ui.clone();
        glib::timeout_add_local_once(Duration::from_millis(ms), move || settings(&u));
    }
    if let Some(ms) = std::env::var("BF_SIDE")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        // dev aid: press the sidebar button after <ms> (narrow window)
        let u = ui.clone();
        glib::timeout_add_local_once(Duration::from_millis(ms), move || {
            u.side_toggle.emit_clicked()
        });
    }
    if let Ok(id) = std::env::var("BF_YT") {
        // dev aid: open the viewer on this YouTube video id
        let u = ui.clone();
        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
            u.viewer.open_youtube(&id)
        });
    }
    if std::env::var_os("BF_VIEW").is_some() {
        // dev aid: open the viewer on the newest loaded photo shortly before BF_SHOT fires
        // BF_VIEW=<ms> sets when it opens; otherwise 0.9 s before the shot
        let ms = std::env::var("BF_SHOT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(2500);
        let at = std::env::var("BF_VIEW")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(ms.saturating_sub(900));
        let u = ui.clone();
        glib::timeout_add_local_once(Duration::from_millis(at), move || {
            let items = u.st.borrow().photos.clone();
            if let Some((last, _)) = items
                .iter()
                .rev()
                .find(|(_, w)| {
                    w.downcast_ref::<bubble::Bubble>()
                        .is_some_and(|b| b.texture().is_some())
                })
                .cloned()
            {
                u.viewer.open(items, &last);
            }
        });
    }
    if let Some(out) = std::env::var_os("BF_SHOT") {
        // dev aid: save the window to a PNG and quit (used with BF_DEMO in a headless compositor)
        let w = win.clone();
        let ms = std::env::var("BF_SHOT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2500);
        glib::timeout_add_local_once(Duration::from_millis(ms), move || {
            let p = gtk::WidgetPaintable::new(Some(&w));
            let (width, height) = (w.width() as f64, w.height() as f64);
            let snap = gtk::Snapshot::new();
            p.snapshot(&snap, width, height);
            if let (Some(node), Some(r)) = (snap.to_node(), w.renderer()) {
                let _ = r.render_texture(&node, None).save_to_png(&out);
            }
            if let Some(app) = w.application() {
                app.quit();
            }
        });
    }
    if first_run {
        start_setup(&ui, false);
    }
}

/// The first-run wizard (also reachable from Settings). `BF_SETUP_TEST=1` is the dev profile: a throwaway key and a
/// separate folder on the Mac, and nothing saved.
fn start_setup(u: &Rc<Ui>, over_main: bool) {
    let test = std::env::var_os("BF_SETUP_TEST").is_some();
    let data = glib::user_data_dir().join("bubblefairy");
    let (key_path, public_key) = if test {
        let p = data.join("setup-test/id_ed25519");
        let k = crate::mac::ensure_key(&p).unwrap_or_default();
        (p, k)
    } else {
        (data.join("id_ed25519"), u.store.public_key.clone())
    };
    let u2 = u.clone();
    let done = Rc::new(
        move |host: String, user: String, imsg: String, host_key: String| {
            let mut cfg = u2.st.borrow().cfg.clone();
            (cfg.host, cfg.user, cfg.imsg) = (host, user, imsg);
            if !host_key.is_empty() {
                let _ = std::fs::write(&u2.store.known_host, host_key); // the fingerprint the person accepted
            }
            crate::save_config(&u2.store.config_file, &cfg);
            u2.st.borrow_mut().cfg = cfg.clone();
            clear_snapshot(&u2); // it may belong to another Mac
            set_status(&u2, "connecting", "Connecting", None);
            u2.send(Cmd::Configure(cfg));
        },
    );
    let u3 = u.clone();
    let closed = Rc::new(move || u3.win.present());
    let u4 = u.clone();
    let set_skip_intro = Rc::new(move |skip: bool| {
        let mut cfg = u4.st.borrow().cfg.clone();
        cfg.skip_intro = skip;
        crate::save_config(&u4.store.config_file, &cfg);
        u4.st.borrow_mut().cfg = cfg;
    });
    let skip_intro = u.st.borrow().cfg.skip_intro;
    let app = u.win.application().expect("application");
    let parent = over_main.then(|| u.win.clone().upcast::<gtk::Window>());
    crate::setup::open_wizard(
        &app,
        parent.as_ref(),
        crate::setup::Ctx {
            public_key,
            key_path,
            default_user: crate::setup::default_user(),
            test,
            done,
            closed,
            skip_intro,
            set_skip_intro,
        },
    );
}

// ---------- events from the Mac ----------

fn on_event(u: &Rc<Ui>, e: Event) {
    if !matches!(
        e,
        Event::Connecting | Event::Offline(_) | Event::Issue(..) | Event::ChatsFailed
    ) {
        u.heard.set(Some(now_unix()));
    }
    match e {
        Event::Connecting => set_status(u, "connecting", "Connecting", None),
        Event::Contacts(mut c) => {
            // the Mac's own list (not the local copy being shown): keep it for the next start
            let raw = std::mem::take(&mut c.raw);
            if !raw.is_empty() && keeps_copy(u) {
                let (path, who) = (u.store.contacts_cache.clone(), account(u));
                bg(
                    move || crate::snapshot::save_contacts(&path, &who, &raw),
                    |_| {},
                );
            }
            {
                let mut st = u.st.borrow_mut();
                st.contacts_cached = false;
                // a photo added, changed or removed since is picked up, the others stay
                let old = st.contacts.take();
                st.avatar_tex.retain(|key, t| {
                    t.is_some() && old.as_ref().is_some_and(|o| o.photo(key) == c.photo(key))
                });
                st.contacts = Some(c);
                st.avatar_asked.clear();
                fill_names(&mut st);
            }
            // now that people are known, one person's chats can be merged
            let raw = u.st.borrow().raw_chats.clone();
            if !raw.is_empty() {
                let groups = set_chats(&mut u.st.borrow_mut(), raw);
                after_merge(u, groups);
                render_chats(u);
            }
            prefetch_avatars(u);
            refresh_identities(u);
            if u.st.borrow().sel.is_some() {
                render_conv(u);
            }
            // a search typed before the people were known can now look for their names too
            if u.search.text().trim().chars().count() >= 3 {
                search_changed(u);
            }
        }
        Event::Connected => {
            {
                let st = u.st.borrow();
                if st.contacts.is_none() || st.contacts_cached {
                    u.send(Cmd::Contacts);
                }
            }
            u.st.borrow_mut().connected = true;
            u.since.set(Some(now_unix()));
            u.ever_connected.set(true);
            u.fails.set(0);
            set_issue(u, "offline", None);
            set_status(u, "online", "Connected", None);
            u.send(Cmd::Chats(u.st.borrow().chat_limit));
            let sel = u.st.borrow().sel;
            if let Some(id) = sel {
                request_history(u, id);
            }
        }
        Event::Offline(why) => {
            u.st.borrow_mut().connected = false;
            u.since.set(None); // before the early returns, so the popover stops saying "connected"
            // not set up yet: nothing has failed, so no alarm (the setup wizard is on its way, or Settings)
            if u.st.borrow().cfg.host.is_empty() {
                set_status(
                    u,
                    "offline",
                    "Not set up",
                    Some("Set up your Mac from Settings."),
                );
                return;
            }
            // a first failed try is only a try: the pill stays orange and the app tries again by itself. Only a second one
            // in a row is a problem worth the person's attention.
            let fails = u.fails.get() + 1;
            u.fails.set(fails);
            if fails < 2 {
                set_status(
                    u,
                    "connecting",
                    if u.ever_connected.get() {
                        "Reconnecting"
                    } else {
                        "Connecting"
                    },
                    Some(&why),
                );
                return;
            }
            set_status(u, "offline", "Offline", Some(&why));
            let text = if u.ever_connected.get() {
                format!(
                    "Lost the connection to your Mac ({why}). BubbleFairy will reconnect on its own."
                )
            } else {
                format!("Couldn't connect to your Mac yet ({why}). BubbleFairy will keep trying.")
            };
            set_issue(u, "offline", Some(text));
        }
        Event::Issue(kind, why) => set_issue(u, kind, why.map(|w| cap(&w))),
        Event::ChatsFailed => set_issue(
            u,
            "chats",
            Some("Couldn't read the chat list from the Mac.".into()),
        ),
        Event::HostKey(k) => u.st.borrow_mut().host_key = k,
        Event::Chats(chats) => {
            set_issue(u, "chats", None);
            let groups;
            let ask: Vec<i64> = {
                let mut st = u.st.borrow_mut();
                for c in &chats {
                    if c["is_group"].as_bool() != Some(true)
                        && let (Some(h), Some(n)) =
                            (c["participants"][0].as_str(), c["contact_name"].as_str())
                        && !n.is_empty()
                    {
                        st.names.insert(h.into(), n.into());
                    }
                }
                let asked = chats.len();
                groups = set_chats(&mut st, chats);
                st.chats_loaded = true;
                st.more_chats = asked >= st.chat_limit;
                st.more_loading = false;
                fill_names(&mut st);
                let st = &mut *st;
                let ids: Vec<i64> = st.chats.iter().filter_map(|c| c["id"].as_i64()).collect();
                let ask: Vec<i64> = ids
                    .into_iter()
                    .filter(|id| !st.preview.contains_key(id) && !st.hist.contains_key(id))
                    .take(150)
                    .collect();
                st.preview_asked.extend(ask.iter().copied());
                ask
            };
            for id in ask {
                u.send(Cmd::Preview(id));
            }
            after_merge(u, groups);
            prefetch_avatars(u);
            u.side_more.set_visible(false);
            let prof = std::time::Instant::now();
            render_chats(u); // before the fade starts: building the rows takes long enough to freeze it half way
            if std::env::var_os("BF_PROF").is_some() {
                let rss = std::fs::read_to_string("/proc/self/statm")
                    .ok()
                    .and_then(|t| t.split(' ').nth(1)?.parse::<u64>().ok())
                    .unwrap_or(0)
                    * 4
                    / 1024;
                eprintln!(
                    "prof: {} chats, render_chats {} ms, previews asked {}, memory {} MB",
                    u.st.borrow().chats.len(),
                    prof.elapsed().as_millis(),
                    u.st.borrow().preview_asked.len(),
                    rss
                );
            }
            u.side_stack.set_visible_child_name("list");
            snapshot_soon(u);
            prof_list(u, "Event::Chats rendered");
            let first = {
                let st = u.st.borrow();
                // BF_CHAT=<id>: dev aid to open a given chat first
                let want = std::env::var("BF_CHAT")
                    .ok()
                    .and_then(|v| v.parse::<i64>().ok());
                if st.sel.is_none() && !u.new_mode.get() {
                    want.or_else(|| st.chats.first().and_then(|c| c["id"].as_i64()))
                } else {
                    None
                }
            };
            // a new conversation's chat has appeared: open it
            let new_chat = {
                let st = u.st.borrow();
                st.new_pending
                    .as_deref()
                    .and_then(|a| find_direct_chat(&st.chats, a))
            };
            if let Some(id) = new_chat {
                {
                    let mut st = u.st.borrow_mut();
                    st.new_pending = None;
                    st.pending
                        .iter_mut()
                        .filter(|p| p.chat_id == 0)
                        .for_each(|p| p.chat_id = id);
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
            let Some(id) = chat["id"].as_i64() else {
                return;
            };
            let id = u.st.borrow().alias.get(&id).copied().unwrap_or(id); // one of a merged person's chats
            if !u
                .st
                .borrow()
                .chats
                .iter()
                .any(|c| c["id"].as_i64() == Some(id))
            {
                insert_chat(&mut u.st.borrow_mut().chats, chat);
                render_chats(u);
                u.send(Cmd::Preview(id));
            }
            // still in the new-message view with that address: show the conversation, like Messages does
            if u.new_mode.get() && same_addr(u.to_entry.text().trim(), &addr) {
                open_chat(u, id);
            }
        }
        Event::FoundNamed { seq, chats } => {
            if seq != u.msearch_gen.get() {
                return; // typed on since
            }
            let mut added = Vec::new();
            for chat in chats {
                let Some(id) = chat["id"].as_i64() else {
                    continue;
                };
                let id = u.st.borrow().alias.get(&id).copied().unwrap_or(id); // one of a merged person's chats
                if !u
                    .st
                    .borrow()
                    .chats
                    .iter()
                    .any(|c| c["id"].as_i64() == Some(id))
                {
                    let mut st = u.st.borrow_mut();
                    insert_chat(&mut st.chats, chat);
                    st.search_added.insert(id);
                    added.push(id);
                }
            }
            if !added.is_empty() {
                fill_names(&mut u.st.borrow_mut()); // the new chats' people
                render_chats(u);
                for id in added {
                    u.send(Cmd::Preview(id));
                }
                apply_search(u);
            }
        }
        Event::Preview(id, m) => {
            learn_names(u, std::slice::from_ref(&m));
            u.st.borrow_mut().preview.insert(id, m);
            update_row(u, id);
        }
        Event::Receipt(chat, r) => on_receipt(u, chat, r),
        Event::History(id, msgs) => {
            learn_names(u, &msgs);
            let is_sel = {
                let mut st = u.st.borrow_mut();
                // (a group event isn't a message: the list's preview and date are those of the last real one)
                if let Some(last) = msgs
                    .iter()
                    .rev()
                    .find(|m| m["item_type"].as_i64().unwrap_or(0) == 0)
                {
                    st.preview.insert(id, last.clone());
                }
                // a chat open on a search hit shows a window of its history, not its newest messages
                if st.find_view.as_ref().is_none_or(|f| f.chat != id) {
                    st.hist.insert(id, msgs);
                }
                st.loading.remove(&id);
                st.sel == Some(id)
            };
            if is_sel {
                render_conv(u);
                u.send(Cmd::Receipts(id)); // "Delivered" / "Read": receipts come after the message
            }
        }
        Event::Older(id, older) => {
            learn_names(u, &older);
            let is_sel = {
                let mut st = u.st.borrow_mut();
                st.older_loading = false;
                let h = st.hist.entry(id).or_default();
                let have: HashSet<String> = h
                    .iter()
                    .filter_map(|m| m["guid"].as_str().map(String::from))
                    .collect();
                let fresh: Vec<Value> = older
                    .into_iter()
                    .filter(|m| m["guid"].as_str().is_some_and(|g| !have.contains(g)))
                    .collect();
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
        Event::Found { seq, hits, counts } => on_found(u, seq, hits, counts),
        Event::ChatFound { chat, q, hits } => {
            // the chat's own list of matches (the sidebar's came from the newest 100 across all chats): the real
            // count, and every match to step through. The hit on show stays the current one.
            if u.search.text().trim() != q || hits.is_empty() {
                return;
            }
            let (showing, opened) = {
                let st = u.st.borrow();
                (
                    st.find_view.as_ref().is_some_and(|f| f.chat == chat),
                    st.sel == Some(chat),
                )
            };
            {
                let mut st = u.st.borrow_mut();
                let cur = st
                    .find_view
                    .as_ref()
                    .filter(|f| f.chat == chat)
                    .and_then(|f| st.find_hits.get(&chat)?.get(f.idx))
                    .map(|h| h["guid"].clone());
                let idx = cur
                    .and_then(|g| hits.iter().position(|h| h["guid"] == g))
                    .unwrap_or(0);
                st.find_counts.entry(chat).or_insert(hits.len());
                st.find_hits.insert(chat, hits);
                if let Some(f) = st.find_view.as_mut().filter(|f| f.chat == chat) {
                    f.idx = idx;
                }
            }
            if showing {
                update_find_bar(u);
            } else if opened {
                begin_find(u, chat, 0); // a chat whose matches were all older than the sidebar's 100
            }
            style_rows(u);
        }
        Event::Window { id, msgs, more } => {
            let ok = {
                let mut st = u.st.borrow_mut();
                let ok = st.find_view.as_ref().is_some_and(|f| f.chat == id);
                if ok {
                    st.hist.insert(id, msgs.clone());
                    st.no_more.remove(&id);
                    st.older_loading = false;
                    if let Some(f) = st.find_view.as_mut() {
                        f.more = more;
                        f.loading = false;
                    }
                }
                ok
            };
            if ok {
                learn_names(u, &msgs);
                if u.st.borrow().sel == Some(id) {
                    render_conv(u);
                }
            }
        }
        Event::Newer { id, msgs, more } => {
            let ok = {
                let mut st = u.st.borrow_mut();
                let ok = st.find_view.as_ref().is_some_and(|f| f.chat == id);
                if ok {
                    let h = st.hist.entry(id).or_default();
                    let have: HashSet<String> = h
                        .iter()
                        .filter_map(|m| m["guid"].as_str().map(String::from))
                        .collect();
                    h.extend(
                        msgs.iter()
                            .filter(|m| m["guid"].as_str().is_some_and(|g| !have.contains(g)))
                            .cloned(),
                    );
                    if let Some(f) = st.find_view.as_mut() {
                        f.more = more;
                        f.loading = false;
                    }
                }
                ok
            };
            if ok {
                learn_names(u, &msgs);
                if u.st.borrow().sel == Some(id) {
                    if let Some(sc) = u
                        .msgs
                        .parent()
                        .and_downcast::<crate::chatscroll::ChatScroll>()
                    {
                        sc.hold(); // messages appended below: the view stays where it is
                    }
                    render_conv(u);
                }
            }
        }
        Event::Message(m) => on_message(u, m),
        Event::Resync => {
            u.send(Cmd::Chats(u.st.borrow().chat_limit));
            let sel = u.st.borrow().sel;
            if let Some(id) = sel {
                request_history(u, id);
            }
        }
        Event::SendSlow(token) => {
            u.st.borrow_mut().slow.insert(token);
            redraw_pending(u);
        }
        Event::Sent {
            text,
            files,
            err,
            token,
        } if u.st.borrow().resend_of.contains_key(&token) => {
            let mid = u.st.borrow_mut().resend_of.remove(&token);
            u.st.borrow_mut().slow.remove(&token); // both outcomes below redraw the chat
            let _ = (text, files);
            if let Some(mid) = mid {
                u.st.borrow_mut().resending.remove(&mid);
            }
            match (err, mid) {
                (Some(err), _) => {
                    drop_pending(u, token);
                    render_conv(u); // the message is a failed one again
                    show_toast(u, &format!("Message not sent: {err}"));
                }
                (None, Some(mid)) => {
                    // sent again: the failed one stays (it is still in the Mac's database, with its error), its
                    // "!" turns to an outline
                    let all: String = {
                        let mut st = u.st.borrow_mut();
                        st.resent.insert(mid);
                        st.resent.iter().map(|i| format!("{i}\n")).collect()
                    };
                    crate::write_state(&u.store.resent_file, all);
                    render_conv(u);
                    // the seed for a picture isn't needed once the new message has come (or never does)
                    let u2 = u.clone();
                    glib::timeout_add_local_once(Duration::from_secs(20), move || {
                        drop_pending(&u2, token)
                    });
                }
                (None, None) => {}
            }
        }
        Event::Sent {
            text,
            files,
            err,
            token,
        } => {
            if let Some(err) = err {
                u.st.borrow_mut().new_pending = None;
                drop_pending(u, token);
                show_toast(u, &format!("Message not sent: {err}"));
                let buf = u.compose.buffer();
                if buf.char_count() == 0 {
                    buf.set_text(&text); // nothing typed is lost
                }
                add_files(u, files); // nor any file that didn't go
            } else {
                if u.st.borrow_mut().slow.remove(&token) {
                    redraw_pending(u); // the "still sending" note is done with
                }
                // normally the real messages replace these within a second; if they never show, don't leave spinners
                let u2 = u.clone();
                glib::timeout_add_local_once(Duration::from_secs(20), move || {
                    drop_pending(&u2, token)
                });
            }
        }
        Event::Recent(id, msgs) => {
            let fresh = msgs.clone();
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
            for m in &fresh {
                release_held(u, m);
            }
            if is_sel {
                render_conv(u);
            }
        }
        Event::Media {
            chat_id,
            kind,
            offset,
            items,
            next,
            more,
        } => on_media(u, chat_id, kind, offset, items, next, more),
        Event::Map(query, data) => {
            let view = data.and_then(|d| {
                let _ = std::fs::create_dir_all(&u.store.image_cache);
                let _ = std::fs::write(map_cache_path(u, &query), map_cache_bytes(&d));
                map_view(&d)
            });
            u.st.borrow_mut().maps.insert(query.clone(), view.clone());
            let slots =
                u.st.borrow_mut()
                    .waiting_map
                    .remove(&query)
                    .unwrap_or_default();
            for (frame, loc) in slots.iter().filter_map(|(w, l)| Some((w.upgrade()?, l))) {
                fill_map(&frame, loc, view.as_ref());
            }
        }
        Event::Downloaded { path, dest, err } => {
            let chip =
                u.st.borrow_mut()
                    .downloading
                    .remove(&path)
                    .and_then(|w| w.upgrade());
            if let Some(c) = chip {
                c.set_opacity(1.0);
                c.set_tooltip_text(Some("Copy from the Mac and open"));
            }
            let gif = u.st.borrow_mut().gif_wait.remove(&path);
            let gif_only = gif.is_some();
            if let Some(waiting) = gif {
                if err.is_none() && whole_gif(&dest) {
                    for b in waiting.iter().filter_map(|w| w.upgrade()) {
                        play_gif(u, &b, &path, &dest);
                    }
                    u.viewer.video_ready(&path, dest.clone());
                } else {
                    let _ = std::fs::remove_file(&dest); // the still picture stays
                }
            }
            let targets = u.st.borrow_mut().save_to.remove(&path);
            if let Some(targets) = targets {
                match err {
                    Some(e) => {
                        show_toast(u, &format!("Couldn't copy the file from the Mac: {e}"));
                        for (_, done) in targets {
                            done(false);
                        }
                    }
                    None => {
                        for (t, done) in targets {
                            copy_to(u, dest.clone(), t, done);
                        }
                    }
                }
            } else if u.st.borrow_mut().video_wait.remove(&path) {
                match err {
                    Some(e) => u.viewer.video_failed(&path, &e),
                    None => u.viewer.video_ready(&path, dest),
                }
            } else if !gif_only {
                match err {
                    Some(e) => show_toast(u, &format!("Couldn't copy the file from the Mac: {e}")),
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
                let slots =
                    u.st.borrow_mut()
                        .link_waiters
                        .remove(&row)
                        .unwrap_or_default();
                for slot in slots.iter().filter_map(|w| w.upgrade()) {
                    clear(&slot);
                    slot.append(&link_tile(u, &l.url, Some(l)));
                }
            }
            let shown = {
                let st = u.st.borrow();
                p.is_some()
                    && st
                        .sel
                        .and_then(|id| st.hist.get(&id))
                        .is_some_and(|h| h.iter().any(|m| preview_row(m) == Some(row)))
            };
            if shown {
                // just that message, where it stands; the whole chat only if that can't be done
                let at = u
                    .conv_ctx
                    .borrow()
                    .as_ref()
                    .and_then(|c| c.msgs.iter().position(|m| preview_row(m) == Some(row)));
                if !at.is_some_and(|i| redraw_message(u, i)) && !u.render_queued.replace(true) {
                    let u2 = u.clone();
                    glib::idle_add_local_once(move || {
                        u2.render_queued.set(false);
                        render_conv(&u2);
                    });
                }
            }
        }
        Event::Image { path, full, bytes } => {
            let Some(b) = bytes else {
                return image_done(u, path, full, None);
            };
            // written to the cache and decoded off the GTK thread (a photo costs 5-10 ms to decode)
            let (file, dir, u2) = (
                cache_path(u, &path, full),
                u.store.image_cache.clone(),
                u.clone(),
            );
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
    let held =
        u.st.borrow_mut()
            .notify_wait
            .remove(&path)
            .unwrap_or_default();
    for (cid, m) in held {
        show_notification(
            u,
            cid,
            &m,
            tex.is_some().then(|| cache_path(u, &path, false)),
        );
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
                u2.st.borrow_mut().info_drawn = [0; 4]; // from scratch
                for tab in [Kind::Photos, Kind::Videos, Kind::Files, Kind::Links] {
                    update_tab(&u2, tab);
                }
            });
        }
    }
}

/// The receipt of a chat's newest sent message arrived: show it under that message.
fn on_receipt(u: &Rc<Ui>, chat: i64, r: Option<crate::mac::Receipt>) {
    let old = {
        let mut st = u.st.borrow_mut();
        let old = match &r {
            Some(r) => st.receipts.insert(chat, r.clone()),
            None => st.receipts.remove(&chat),
        };
        if old == r {
            return;
        }
        old
    };
    if u.st.borrow().sel != Some(chat) {
        return;
    }
    // the message that has the line now, and the one that had it before (a newer message was sent)
    let mut stale = false;
    for msg in [r.as_ref().map(|r| r.msg), old.as_ref().map(|r| r.msg)]
        .into_iter()
        .flatten()
        .collect::<HashSet<_>>()
    {
        let at = u
            .conv_ctx
            .borrow()
            .as_ref()
            .and_then(|c| c.msgs.iter().position(|m| m["id"].as_i64() == Some(msg)));
        stale |= !at.is_some_and(|i| redraw_message(u, i));
    }
    if stale {
        render_conv(u);
    }
}

/// "Delivered": the line under my newest message that isn't read yet ("Read ..." is a `TimeKind::Read` label). A
/// message delivered while the recipient's Focus silenced notifications says "Delivered" too (what Messages on the
/// phone shows); the tooltip tells.
fn receipt_text(r: &crate::mac::Receipt) -> Option<(String, Option<&'static str>)> {
    let tip = r
        .silenced
        .then_some("Delivered quietly: their Focus silenced notifications when it arrived");
    r.delivered.then(|| ("Delivered".to_string(), tip))
}

/// Forget a send's waiting bubbles (it failed, or never showed up).
fn drop_pending(u: &Rc<Ui>, token: u64) {
    let had = {
        let mut st = u.st.borrow_mut();
        let before = st.pending.len();
        st.pending.retain(|p| p.token != token);
        st.slow.remove(&token);
        before != st.pending.len()
    };
    if had {
        redraw_pending(u);
    }
}

/// Draws the waiting bubbles (and their "still sending" notes) again.
fn redraw_pending(u: &Rc<Ui>) {
    if u.st.borrow().sel.is_some() {
        render_conv(u);
    }
    show_new_ghosts(u);
}

/// My own message came back through the live stream: its waiting bubble is replaced by the real one.
fn settle_pending(st: &mut State, cid: i64, m: &Value) {
    let text = clean_text(m);
    let has_files = m["attachments"].as_array().is_some_and(|a| !a.is_empty());
    let name = |p: &PathBuf| {
        p.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let at = st.pending.iter().position(|p| {
        // chat id 0: the first message to a new address, whose chat we don't know yet
        (p.chat_id == cid || p.chat_id == 0)
            && match &p.item {
                PendingItem::Text(t) => !text.is_empty() && *t == text,
                PendingItem::File(f) => {
                    has_files
                        && m["attachments"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .any(|a| a["transfer_name"].as_str().is_none_or(|n| n == name(f)))
                }
                PendingItem::Seed(_, n) => {
                    has_files
                        && m["attachments"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .any(|a| a["transfer_name"].as_str().is_none_or(|t| t == n))
                }
            }
    });
    if let Some(i) = at {
        let gone = st.pending.remove(i);
        if let PendingItem::Seed(t, n) = &gone.item {
            // the new message shows the picture we already had, with no fetch from the Mac
            let att = m["attachments"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|a| a["transfer_name"].as_str().is_none_or(|x| x == n));
            if let Some(path) = att
                .and_then(|a| a["original_path"].as_str())
                .filter(|p| !p.is_empty())
            {
                st.textures
                    .entry(path.to_string())
                    .or_insert_with(|| t.clone());
            }
        }
        if let PendingItem::File(f) = gone.item {
            // the real photo shows the local picture at once instead of waiting for the fetch back from the Mac
            let att = m["attachments"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|a| a["transfer_name"].as_str().is_none_or(|n| n == name(&f)));
            if let (Some(path), Ok(t)) = (
                att.and_then(|a| a["original_path"].as_str())
                    .filter(|p| !p.is_empty()),
                gdk::Texture::from_filename(&f),
            ) {
                st.textures.entry(path.to_string()).or_insert(t);
            }
        }
    }
}

fn on_message(u: &Rc<Ui>, m: Value) {
    learn_names(u, std::slice::from_ref(&m));
    let Some(cid) = m["chat_id"].as_i64() else {
        return;
    };
    let mutation = m["type"].as_str().is_some();
    if !mutation && m["is_from_me"].as_bool() == Some(true) {
        settle_pending(&mut u.st.borrow_mut(), cid, &m);
        show_new_ghosts(u);
    }
    let mut fresh = false; // a message we hadn't got yet
    // a search window that doesn't reach the newest messages yet: a new one isn't next to what's loaded
    let gap =
        u.st.borrow()
            .find_view
            .as_ref()
            .is_some_and(|f| f.chat == cid && f.more);
    let (is_sel, known_chat) = {
        let mut st = u.st.borrow_mut();
        if let Some(h) = st.hist.get_mut(&cid) {
            match h.iter_mut().find(|x| x["guid"] == m["guid"]) {
                Some(old) => *old = m.clone(),
                None if !mutation && !gap => {
                    h.push(m.clone());
                    fresh = true;
                }
                None => {}
            }
        }
        if !mutation && !is_group_event(&m) {
            st.preview.insert(cid, m.clone());
        }
        (
            st.sel == Some(cid),
            st.chats.iter().any(|c| c["id"].as_i64() == Some(cid)),
        )
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
    if m["is_reaction"].as_bool() == Some(true) {
        u.send(Cmd::Recent(cid)); // the target message's reaction list comes with its history row
    }
    watch_for_preview(u, cid, &m);
    watch_for_media(u, cid, &m);
    release_held(u, &m);
    // chat order and unread counts come from the Mac; coalesce bursts into one reload
    if !u.chats_queued.replace(true) {
        let u2 = u.clone();
        glib::timeout_add_local_once(Duration::from_millis(700), move || {
            u2.chats_queued.set(false);
            u2.send(Cmd::Chats(u2.st.borrow().chat_limit));
        });
    }
    if !known_chat {
        return;
    }
    render_chats(u);
    // someone joining, leaving or renaming a group arrives as a message with no text: no chime, no notification
    if m["is_from_me"].as_bool() != Some(true) && !is_group_event(&m) {
        notify(u, cid, &m);
    }
}

/// A group event row (`item_type` other than 0), not a message someone wrote.
fn is_group_event(m: &Value) -> bool {
    m["item_type"].as_i64().unwrap_or(0) != 0
}

/// A message with a link arrives before Messages has made its preview (and, for one we sent, before it
/// has pulled the link out of the text). Look again a few times so the card replaces the plain text.
fn watch_for_preview(u: &Rc<Ui>, cid: i64, m: &Value) {
    let Some(guid) = m["guid"].as_str() else {
        return;
    };
    if preview_row(m).is_some()
        || crate::mac::urls_in(&clean_text(m)).is_empty()
        || !u.st.borrow_mut().preview_watch.insert(guid.to_string())
    {
        return;
    }
    for secs in [3, 8, 20] {
        let (u2, guid) = (u.clone(), guid.to_string());
        glib::timeout_add_local_once(Duration::from_secs(secs), move || {
            let (done, connected) = {
                let st = u2.st.borrow();
                let msg = st
                    .hist
                    .get(&cid)
                    .and_then(|h| h.iter().find(|x| x["guid"].as_str() == Some(&guid)));
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
        for h in c["participants"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p.as_str())
        {
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

fn notify(u: &Rc<Ui>, cid: i64, m: &Value) {
    {
        // a muted chat makes no sound and shows nothing
        let st = u.st.borrow();
        if st
            .chats
            .iter()
            .find(|c| c["id"].as_i64() == Some(cid))
            .is_some_and(|c| is_muted(&st.muted, c))
        {
            return;
        }
    }
    let (show, sound, choice) = {
        let c = &u.st.borrow().cfg;
        (c.notify, c.notify_sound, c.sound.clone())
    };
    // window in front: the in-app counterpart of the send swish, not the chosen notification sound
    let choice = if u.win.is_active() {
        "received".to_string()
    } else {
        choice
    };
    if sound {
        // not while the desktop is in do-not-disturb
        glib::spawn_future_local(async move {
            if !crate::sound::desktop_inhibited().await {
                crate::sound::play_arrival(&choice);
            }
        });
    }
    // window in front: the sound is all you get; the desktop notification is for when you're elsewhere
    if !show || u.win.is_active() {
        return;
    }
    deliver(u, cid, m);
}

/// A message's file is on the Mac only once Messages has downloaded it, which can lag the message row.
fn awaiting_media(m: &Value) -> bool {
    let atts = m["attachments"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    !atts.is_empty() && atts.iter().all(|a| a["missing"].as_bool() == Some(true))
}

/// Ask again a few times for a message whose pictures or files aren't on the Mac yet, so they appear when they land.
fn watch_for_media(u: &Rc<Ui>, cid: i64, m: &Value) {
    let Some(guid) = m["guid"].as_str() else {
        return;
    };
    if !awaiting_media(m) || !u.st.borrow_mut().media_watch.insert(guid.to_string()) {
        return;
    }
    for secs in [2, 5, 10, 20, 40] {
        let (u2, guid) = (u.clone(), guid.to_string());
        glib::timeout_add_local_once(Duration::from_secs(secs), move || {
            let (done, connected) = {
                let st = u2.st.borrow();
                let msg = st
                    .hist
                    .get(&cid)
                    .and_then(|h| h.iter().find(|x| x["guid"].as_str() == Some(&guid)));
                (msg.is_none_or(|x| !awaiting_media(x)), st.connected)
            };
            if !done && connected {
                u2.send(Cmd::Recent(cid));
            }
        });
    }
}

/// A held notification can go once its message has its file (or has waited long enough).
fn release_held(u: &Rc<Ui>, m: &Value) {
    let Some(guid) = m["guid"].as_str() else {
        return;
    };
    if awaiting_media(m) {
        return;
    }
    let held = u.st.borrow_mut().notify_media.remove(guid);
    if let Some((cid, _)) = held {
        deliver(u, cid, m);
    }
}

fn deliver(u: &Rc<Ui>, cid: i64, m: &Value) {
    let text_level = u.st.borrow().cfg.notify_text;
    // the first picture goes in the notification, but only when the text is shown too (it would give the message away)
    let pic = (text_level == 0)
        .then(|| {
            m["attachments"].as_array().into_iter().flatten().find(|a| {
                a["mime_type"]
                    .as_str()
                    .is_some_and(|t| t.starts_with("image/"))
            })
        })
        .flatten()
        .and_then(|a| a["original_path"].as_str().filter(|p| !p.is_empty()))
        .map(String::from);
    let Some(path) = pic else {
        return show_notification(u, cid, m, None);
    };
    if let (true, Some(guid)) = (awaiting_media(m), m["guid"].as_str()) {
        // Messages hasn't downloaded the picture yet: wait for it (up to 40 s) instead of showing "Photo"
        u.st.borrow_mut()
            .notify_media
            .insert(guid.to_string(), (cid, m.clone()));
        let (u2, guid, m) = (u.clone(), guid.to_string(), m.clone());
        glib::timeout_add_local_once(Duration::from_secs(40), move || {
            if u2.st.borrow_mut().notify_media.remove(&guid).is_some() {
                show_notification(&u2, cid, &m, None);
            }
        });
        return;
    }
    let file = cache_path(u, &path, false);
    if file.exists() {
        return show_notification(u, cid, m, Some(file));
    }
    // not fetched yet: ask the Mac and notify when it arrives (`image_done`), with or without the picture
    let first = {
        let mut st = u.st.borrow_mut();
        let w = st.notify_wait.entry(path.clone()).or_default();
        w.push((cid, m.clone()));
        w.len() == 1
    };
    if first {
        u.send(Cmd::Image { path, full: false });
    }
}

fn show_notification(u: &Ui, cid: i64, m: &Value, picture: Option<PathBuf>) {
    if u.win.is_active() {
        return; // the message may have been held while you came to the window
    }
    let (text_level, st) = (u.st.borrow().cfg.notify_text, u.st.borrow());
    let Some(chat) = st.chats.iter().find(|c| c["id"].as_i64() == Some(cid)) else {
        return;
    };
    let mut title = chat_title(chat, &st.names);
    if chat["is_group"].as_bool() == Some(true) {
        title = format!("{} in {title}", sender_name(m, &st.names));
    }
    let (title, body) = match text_level {
        0 => (title, summary(m)),
        1 => (title, "New message".to_string()),
        _ => ("BubbleFairy".to_string(), "New message".to_string()),
    };
    let n = gio::Notification::new(&title);
    n.set_body(Some(&body));
    n.set_default_action_and_target_value("app.open-chat", Some(&cid.to_variant()));
    // the picture if there is one, else our own icon (the desktop may only show it when the app is installed)
    match picture.and_then(|f| std::fs::read(f).ok()) {
        Some(b) => n.set_icon(&gio::BytesIcon::new(&glib::Bytes::from_owned(b))),
        None => n.set_icon(&gio::ThemedIcon::new(crate::APP_ID)),
    }
    // one per message: re-using an id per chat made the desktop update the old one quietly instead of popping up
    u.app.send_notification(
        Some(&format!(
            "chat-{cid}-{}",
            m["id"].as_i64().unwrap_or_default()
        )),
        &n,
    );
}

// ---------- status and toast ----------

fn set_status(u: &Rc<Ui>, kind: &str, text: &str, tip: Option<&str>) {
    // connected, but something needs a look: the pill says so and lists it when clicked
    let (kind, text, tip) = if kind == "online" && !u.issues.borrow().is_empty() {
        ("issue", "Needs attention", None)
    } else {
        (kind, text, tip)
    };
    for k in ["online", "connecting", "offline", "issue"] {
        u.status.remove_css_class(k);
    }
    u.status.add_css_class(kind);
    *u.status_title.borrow_mut() = text.to_string();
    // the pill is only a light: its words are the tooltip, the accessible name and the popover's title
    let said = tip.map_or_else(|| text.to_string(), |t| format!("{text}: {t}"));
    u.status.set_tooltip_text(Some(&said));
    u.status
        .update_property(&[gtk::accessible::Property::Label(&said)]);
    *u.status_tip.borrow_mut() = tip.unwrap_or_default().to_string();
    refresh_status_pop(u);
}

/// Sets (`Some`) or clears (`None`) one thing that's wrong with the connection. Setting what's already set
/// changes nothing on screen, so a warning repeated on every reconnect doesn't flash.
fn set_issue(u: &Rc<Ui>, kind: &'static str, why: Option<String>) {
    let changed = {
        let mut v = u.issues.borrow_mut();
        let was = v.iter().position(|(k, _)| *k == kind);
        match (was, why) {
            (Some(i), Some(w)) if v[i].1 != w => {
                v[i].1 = w;
                true
            }
            (None, Some(w)) => {
                v.push((kind, w));
                true
            }
            (Some(i), None) => {
                v.remove(i);
                true
            }
            _ => false,
        }
    };
    if changed && (u.status.has_css_class("online") || u.status.has_css_class("issue")) {
        set_status(u, "online", "Connected", None);
    }
    if changed {
        refresh_status_pop(u);
    }
}

fn now_unix() -> i64 {
    glib::real_time() / 1_000_000
}

/// `just now`, `3 min ago`, `2 h ago`, `4 d ago`.
fn ago(secs: i64) -> String {
    match secs {
        ..10 => "just now".into(),
        10..60 => format!("{secs} s ago"),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86400 => format!("{} h ago", secs / 3600),
        _ => format!("{} d ago", secs / 86400),
    }
}

/// `45 s`, `12 min`, `2 h 14 min`, `3 d 4 h`.
fn lasted(secs: i64) -> String {
    match secs {
        ..60 => format!("{} s", secs.max(0)),
        60..3600 => format!("{} min", secs / 60),
        3600..86400 => format!("{} h {} min", secs / 3600, secs % 3600 / 60),
        _ => format!("{} d {} h", secs / 86400, secs % 86400 / 3600),
    }
}

/// A coloured dot for the status popover (`ok`, `warn` or `bad`).
fn pop_dot(kind: &str) -> gtk::Box {
    let d = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    d.add_css_class("pop-dot");
    d.add_css_class(kind);
    d.set_valign(gtk::Align::Center);
    d
}

/// The status pill: a popover with what's wrong (if anything) and how the connection is doing.
fn status_clicked(u: &Rc<Ui>) {
    let pop = gtk::Popover::new();
    pop.add_css_class("issues");
    pop.set_position(gtk::PositionType::Bottom);
    fill_status_pop(u, &pop);
    pop.set_parent(&u.status);
    *u.status_pop.borrow_mut() = Some(pop.clone());
    let u2 = u.clone();
    pop.connect_closed(move |_| *u2.status_pop.borrow_mut() = None);
    unparent_on_close(&pop);
    // the times move on even when nothing else changes
    let (u3, weak) = (u.clone(), pop.downgrade());
    glib::timeout_add_local(Duration::from_secs(5), move || match weak.upgrade() {
        Some(p) if p.is_visible() => {
            fill_status_pop(&u3, &p);
            glib::ControlFlow::Continue
        }
        _ => glib::ControlFlow::Break,
    });
    pop.popup();
}

/// Dev aid: with `BF_POPSHOT=<png>` set, draws the open popover to that file shortly after, then quits.
fn popshot_soon(u: &Rc<Ui>) {
    let Some(path) = std::env::var_os("BF_POPSHOT") else {
        return;
    };
    let ms = std::env::var("BF_POPSHOT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(600);
    let u = u.clone();
    glib::timeout_add_local_once(Duration::from_millis(ms), move || {
        // the status popover hangs off the pill, the others off the window
        let mut pop = u.status_pop.borrow().clone();
        let mut child = u.win.first_child();
        while let (None, Some(w)) = (&pop, &child) {
            pop = w.clone().downcast::<gtk::Popover>().ok();
            child = w.next_sibling();
        }
        if let Some(pop) = pop {
            save_widget_png(&pop, std::path::Path::new(&path));
        }
        u.win.close();
    });
}

/// Dev aid: draws a widget (a popover, which a window screenshot leaves out) to a PNG.
fn save_widget_png(w: &impl IsA<gtk::Widget>, path: &std::path::Path) {
    let paintable = gtk::WidgetPaintable::new(Some(w));
    let (pw, ph) = (paintable.intrinsic_width(), paintable.intrinsic_height());
    let snap = gtk::Snapshot::new();
    paintable.snapshot(&snap, f64::from(pw), f64::from(ph));
    if let (Some(node), Some(r)) = (snap.to_node(), w.native().and_then(|n| n.renderer())) {
        let _ = r.render_texture(&node, None).save_to_png(path);
    }
}

/// Width of the status popover's content, in pixels.
const STATUS_POP_W: i32 = 270;

/// Redraws the status popover if it's open (the status changed under it).
fn refresh_status_pop(u: &Rc<Ui>) {
    let pop = u.status_pop.borrow().clone();
    if let Some(p) = pop {
        fill_status_pop(u, &p);
    }
}

/// Builds the status popover's content from the current state.
fn fill_status_pop(u: &Rc<Ui>, pop: &gtk::Popover) {
    let issues = u.issues.borrow().clone();
    let since = u.since.get();
    let col = gtk::Box::new(gtk::Orientation::Vertical, 6);
    col.set_size_request(STATUS_POP_W, -1); // a fixed width, so a change of state doesn't make it jump about
    col.set_margin_start(8);
    col.set_margin_end(8);
    col.set_margin_top(4);
    col.set_margin_bottom(4);
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let kind = ["online", "offline"]
        .iter()
        .find(|k| u.status.has_css_class(k))
        .map_or("warn", |k| if *k == "online" { "ok" } else { "bad" });
    head.append(&pop_dot(kind));
    let t = gtk::Label::new(Some(&u.status_title.borrow()));
    t.add_css_class("pop-title");
    t.set_hexpand(true);
    t.set_xalign(0.0);
    head.append(&t);
    col.append(&head);
    let tip = u.status_tip.borrow().clone();
    if !tip.is_empty() && issues.is_empty() {
        let l = gtk::Label::new(Some(&tip));
        l.add_css_class("dim");
        l.set_wrap(true);
        l.set_max_width_chars(36);
        l.set_xalign(0.0);
        col.append(&l);
    }
    for (k, why) in &issues {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.append(&pop_dot(if *k == "offline" { "bad" } else { "warn" }));
        let l = gtk::Label::new(Some(why));
        l.set_wrap(true);
        l.set_max_width_chars(36);
        l.set_xalign(0.0);
        row.append(&l);
        col.append(&row);
    }
    if let Some(since) = since {
        let now = now_unix();
        let (host, user) = {
            let st = u.st.borrow();
            (st.cfg.host.clone(), st.cfg.user.clone())
        };
        let grid = gtk::Grid::new();
        grid.set_row_spacing(3);
        grid.set_column_spacing(16);
        let mut rows = vec![
            ("Mac", host),
            ("Signed in as", user),
            (
                "Connected",
                format!(
                    "{} · {}",
                    rel_time(since, TimeKind::Sep),
                    lasted(now - since)
                ),
            ),
        ];
        if let Some(h) = u.heard.get() {
            rows.push(("Last heard from", ago(now - h)));
        }
        for (i, (k, v)) in rows.iter().filter(|(_, v)| !v.is_empty()).enumerate() {
            let key = gtk::Label::new(Some(k));
            key.add_css_class("dim");
            key.set_xalign(0.0);
            let val = gtk::Label::new(Some(v));
            val.set_xalign(0.0);
            val.set_hexpand(true);
            val.set_ellipsize(gtk::pango::EllipsizeMode::End);
            val.set_tooltip_text(Some(v));
            grid.attach(&key, 0, i as i32, 1, 1);
            grid.attach(&val, 1, i as i32, 1, 1);
        }
        col.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        col.append(&grid);
        let refresh = gtk::Button::from_icon_name("bf-refresh-symbolic");
        refresh.add_css_class("head-btn");
        refresh.add_css_class("pop-refresh");
        refresh.set_tooltip_text(Some("Refresh now"));
        let (u2, p2) = (u.clone(), pop.clone());
        refresh.connect_clicked(move |_| {
            p2.popdown();
            let limit = u2.st.borrow().chat_limit;
            u2.send(Cmd::Chats(limit));
            refresh_chat(&u2);
        });
        head.append(&refresh);
    }
    pop.set_child(Some(&col));
}

/// A short message over the top of the window; it goes by itself.
fn show_toast(u: &Rc<Ui>, text: &str) {
    u.toast_text.set_text(text);
    u.toast.set_reveal_child(true);
    let gen_ = u.toast_gen.get().wrapping_add(1);
    u.toast_gen.set(gen_);
    let u2 = u.clone();
    glib::timeout_add_local_once(Duration::from_secs(8), move || {
        if u2.toast_gen.get() == gen_ {
            u2.toast.set_reveal_child(false);
        }
    });
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

// ---------- selecting and sending ----------

fn select_in_list(u: &Ui, id: i64) {
    let idx =
        u.st.borrow()
            .chats
            .iter()
            .position(|c| c["id"].as_i64() == Some(id));
    if let Some(row) = idx.and_then(|i| u.chat_list.row_at_index(i as i32)) {
        u.selecting.set(true);
        u.chat_list.select_row(Some(&row));
        u.selecting.set(false);
    }
}

/// Highlight the pinned tile of this chat (if it has one), clear the others.
fn mark_tile(u: &Ui, id: i64) {
    for t in u.tiles.tiles() {
        if t.widget_name() == id.to_string() {
            t.add_css_class("selected")
        } else {
            t.remove_css_class("selected")
        }
    }
}

/// Leaving a chat for another: its unsent text and waiting files are kept for when you come back, and the other
/// chat's are put in the composer. (Coming from the new-message view, what was typed there goes with you.)
fn swap_draft(u: &Rc<Ui>, from: Option<i64>, to: i64) {
    let Some(from) = from.filter(|f| *f != to) else {
        return;
    };
    let buf = u.compose.buffer();
    let text = buf
        .text(&buf.start_iter(), &buf.end_iter(), false)
        .to_string();
    let draft = {
        let mut st = u.st.borrow_mut();
        let files = std::mem::take(&mut st.outgoing);
        if text.is_empty() && files.is_empty() {
            st.drafts.remove(&from);
        } else {
            st.drafts.insert(from, (text, files));
        }
        st.drafts.get(&to).cloned()
    };
    let (text, files) = draft.unwrap_or_default();
    buf.set_text(&text);
    u.st.borrow_mut().outgoing = files;
    render_attach(u);
    update_send(u);
}

fn select(u: &Rc<Ui>, id: i64) {
    let from = u.st.borrow().sel;
    swap_draft(u, from, id);
    fold_side(u);
    end_new(u);
    u.viewer.close_now();
    u.st.borrow_mut().gifs.clear(); // the last chat's players go with its bubbles
    u.st.borrow_mut().sel = Some(id);
    let picked = !u.search.text().trim().is_empty();
    u.st.borrow_mut().search_pick = picked.then_some(id);
    select_in_list(u, id);
    mark_tile(u, id);
    u.stick_bottom.set(true);
    // picked from a message search: open on its newest hit instead of the newest messages
    let searching = !u.search.text().trim().is_empty();
    let on_hit = searching && u.st.borrow().find_hits.contains_key(&id);
    // matches only older than the 100 newest: the chat's own list comes first (`Event::ChatFound`)
    let counted = searching && !on_hit && u.st.borrow().find_counts.contains_key(&id);
    if !on_hit {
        u.st.borrow_mut().find_view = None;
        update_find_bar(u);
    }
    render_conv(u); // cached view right away, fresh history shortly
    if counted {
        u.send(Cmd::ChatHits {
            chat: id,
            q: u.search.text().trim().to_string(),
        });
    }
    if on_hit {
        begin_find(u, id, 0);
        u.send(Cmd::ChatHits {
            chat: id,
            q: u.search.text().trim().to_string(),
        });
        u.compose.grab_focus();
        return;
    }
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
        st.sel.is_some_and(|id| {
            st.loading.contains(&id) && st.hist.get(&id).is_none_or(|h| h.is_empty())
        })
    };
    u.msgs_loading.set_visible(on);
}

/// Scrolled near the top: fetch the next page of older messages.
fn load_older(u: &Rc<Ui>) {
    let req = {
        let mut st = u.st.borrow_mut();
        let Some(id) = st.sel else { return };
        if st.older_loading || st.no_more.contains(&id) || !st.connected {
            return;
        }
        let Some(before) = st
            .hist
            .get(&id)
            .and_then(|h| h.first())
            .and_then(|m| m["created_at"].as_str())
            .map(String::from)
        else {
            return;
        };
        st.older_loading = true;
        (id, before)
    };
    u.send(Cmd::Older {
        id: req.0,
        before: req.1,
    });
    let u2 = u.clone();
    glib::idle_add_local_once(move || render_conv(&u2)); // shows the "Loading more" row; not inside a scroll signal
}

/// Whom a message to this chat is sent to directly: the other person of a 1:1 chat (None for a group).
fn direct_to(u: &Ui, chat_id: i64) -> Option<String> {
    let st = u.st.borrow();
    st.chats
        .iter()
        .find(|c| c["id"].as_i64() == Some(chat_id))
        .filter(|c| c["is_group"].as_bool() != Some(true))
        .and_then(|c| direct_addr(c).map(String::from))
}

/// "Try Again" on a failed message: send its text and its attachments (already on the Mac) once more. The failed
/// message stays where it is, as a ghost with a spinner while that goes on; the new one appears at the end when the
/// Mac has it (as in Messages, the failed one stays in the chat).
fn retry_failed(u: &Rc<Ui>, m: &Value) {
    let (Some(chat_id), Some(mid)) = (m["chat_id"].as_i64(), m["id"].as_i64()) else {
        return;
    };
    let text = clean_text(m);
    let mac_files: Vec<String> = m["attachments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a["original_path"].as_str())
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect();
    if text.is_empty() && mac_files.is_empty() {
        return;
    }
    u.stick_bottom.set(true);
    let token = u.send_token.get() + 1;
    u.send_token.set(token);
    {
        let mut st = u.st.borrow_mut();
        st.resend_of.insert(token, mid);
        st.resending.insert(mid);
        // pictures we already show: the new message will show them too, nothing is fetched back
        for a in m["attachments"].as_array().into_iter().flatten() {
            let tex = a["original_path"]
                .as_str()
                .and_then(|p| st.textures.get(p))
                .cloned();
            if let (Some(t), Some(n)) = (tex, a["transfer_name"].as_str()) {
                st.pending.push(Pending {
                    token,
                    chat_id,
                    item: PendingItem::Seed(t, n.to_string()),
                });
            }
        }
    }
    u.send(Cmd::Resend {
        chat_id,
        to: direct_to(u, chat_id),
        text,
        mac_files,
        token,
    });
    render_conv(u);
}

/// Called when a save has finished (true) or failed (false).
type SaveDone = Rc<dyn Fn(bool)>;

/// Something the download button can save: a file on the Mac (`pics`: it goes to the pictures folder).
#[derive(Clone)]
struct Save {
    path: String,
    name: String,
    size: i64,
    pics: bool,
}

impl Save {
    fn new(path: &str, name: &str, size: i64, pics: bool) -> Save {
        Save {
            path: path.into(),
            name: safe_name(name, "file"),
            size,
            pics,
        }
    }
}

/// The name a photo is saved under: its attachment's own name, else the file name on the Mac.
fn save_name(m: &Value, path: &str) -> String {
    let from_att = m["attachments"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["original_path"].as_str() == Some(path))
        .and_then(|a| a["transfer_name"].as_str().filter(|n| !n.is_empty()));
    from_att
        .unwrap_or_else(|| path.rsplit('/').next().unwrap_or("photo"))
        .to_string()
}

/// The file name at the end of a picture's path or web address.
fn web_name(path: &str) -> String {
    let path = if path.starts_with("web:") {
        path.split(['?', '#']).next().unwrap_or("")
    } else {
        path
    };
    match path.rsplit('/').next() {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => "photo".to_string(),
    }
}

/// The small download button beside a photo, video or file.
fn save_button(u: &Rc<Ui>, items: Vec<Save>) -> gtk::Button {
    let b = gtk::Button::from_icon_name("bf-download-symbolic");
    b.add_css_class("save-btn");
    b.set_valign(gtk::Align::Center);
    b.set_tooltip_text(Some(if items.len() > 1 {
        "Save these photos"
    } else {
        "Save"
    }));
    let u = u.clone();
    b.connect_clicked(move |b| {
        let b = b.clone();
        save_items(
            &u,
            items.clone(),
            false,
            Rc::new(move |ok| {
                if ok {
                    show_saved(&b);
                }
            }),
        );
    });
    b
}

/// Finished: the save button is a green check for 3 s (and can't be clicked) before it turns back into the save icon.
fn show_saved(b: &gtk::Button) {
    b.set_icon_name("bf-check-symbolic");
    b.add_css_class("done");
    b.set_sensitive(false);
    let b = b.clone();
    glib::timeout_add_local_once(Duration::from_secs(3), move || {
        b.set_icon_name("bf-download-symbolic");
        b.remove_css_class("done");
        b.set_sensitive(true);
    });
}

/// The save button in the same row as `w` (a photo): a right-click "Save as…" has no button of its own to confirm on.
fn save_button_near(w: &gtk::Widget) -> Option<gtk::Button> {
    fn find(w: &gtk::Widget) -> Option<gtk::Button> {
        if let Some(b) = w
            .downcast_ref::<gtk::Button>()
            .filter(|b| b.has_css_class("save-btn"))
        {
            return Some(b.clone());
        }
        let mut c = w.first_child();
        while let Some(x) = c {
            if let Some(b) = find(&x) {
                return Some(b);
            }
            c = x.next_sibling();
        }
        None
    }
    let mut at = w.parent();
    while let Some(p) = at {
        if p.has_css_class("msg-line") {
            return find(&p);
        }
        at = p.parent();
    }
    None
}

/// A name that isn't taken in `dir`: "photo.jpg", "photo (2).jpg", ...
fn unique(dir: &std::path::Path, name: &str) -> PathBuf {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    let mut p = dir.join(name);
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{stem} ({n}){ext}"));
        n += 1;
    }
    p
}

/// Save to the default folder set in Settings, or ask where (a save dialog; for several photos, a folder).
fn save_items(u: &Rc<Ui>, items: Vec<Save>, ask: bool, done: SaveDone) {
    let Some(first) = items.first() else { return };
    // with several items, `done` fires once all of them have been saved
    let left = Rc::new(Cell::new(items.len()));
    let all_ok = Rc::new(Cell::new(true));
    let done: SaveDone = if items.len() == 1 {
        done
    } else {
        Rc::new(move |ok| {
            all_ok.set(all_ok.get() && ok);
            left.set(left.get() - 1);
            if left.get() == 0 {
                done(all_ok.get());
            }
        })
    };
    let dir = {
        let st = u.st.borrow();
        if ask {
            String::new()
        } else if first.pics {
            st.cfg.dir_images.clone()
        } else {
            st.cfg.dir_files.clone()
        }
    };
    // a default folder that has gone (deleted, access revoked): say so and ask where instead
    let usable = |d: &str| {
        gio::File::for_path(d)
            .query_info(
                "standard::type,access::can-write",
                gio::FileQueryInfoFlags::NONE,
                gio::Cancellable::NONE,
            )
            .is_ok_and(|i| {
                i.file_type() == gio::FileType::Directory && i.boolean("access::can-write")
            })
    };
    let dir = if !dir.is_empty() && !usable(&dir) {
        let msg = "The default download folder isn't available any more. Choose where to save, or set it again in Settings.";
        show_toast(u, msg);
        String::new()
    } else {
        dir
    };
    if !dir.is_empty() {
        for it in &items {
            queue_save(
                u,
                it,
                unique(std::path::Path::new(&dir), &it.name),
                done.clone(),
            );
        }
        return;
    }
    let d = gtk::FileDialog::builder().modal(true).build();
    let u2 = u.clone();
    if let [one] = items.as_slice() {
        d.set_title("Save");
        d.set_initial_name(Some(&one.name));
        let one = one.clone();
        d.save(Some(&u.win), gio::Cancellable::NONE, move |r| {
            if let Some(p) = r.ok().and_then(|f| f.path()) {
                queue_save(&u2, &one, p, done);
            }
        });
    } else {
        d.set_title("Save photos to");
        d.select_folder(Some(&u.win), gio::Cancellable::NONE, move |r| {
            if let Some(dir) = r.ok().and_then(|f| f.path()) {
                for it in &items {
                    queue_save(&u2, it, unique(&dir, &it.name), done.clone());
                }
            }
        });
    }
}

/// Put the Mac's file at `target`: from our copy if there is one, else fetch it first.
fn queue_save(u: &Rc<Ui>, it: &Save, target: PathBuf, done: SaveDone) {
    // a picture off the web is already here, in the image cache
    let src = if it.path.starts_with("web:") {
        cache_path(u, &it.path, false)
    } else {
        file_dest(u, &it.path, &it.name)
    };
    if src.exists() || it.path.starts_with("web:") {
        copy_to(u, src, target, done);
    } else {
        let copying = {
            let mut st = u.st.borrow_mut();
            st.save_to
                .entry(it.path.clone())
                .or_default()
                .push((target, done));
            st.gif_wait.contains_key(&it.path) // already on its way for the chat
        };
        if !copying {
            u.send(Cmd::Download {
                path: it.path.clone(),
                dest: src,
                size: it.size,
            });
        }
    }
}

fn copy_to(u: &Rc<Ui>, src: PathBuf, target: PathBuf, done: SaveDone) {
    let u = u.clone();
    glib::spawn_future_local(async move {
        let (copy, _) = gio::File::for_path(&src).copy_future(
            &gio::File::for_path(&target),
            gio::FileCopyFlags::OVERWRITE,
            glib::Priority::DEFAULT,
        );
        match copy.await {
            Ok(()) => done(true),
            Err(e) => {
                let name = target
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                show_toast(&u, &format!("Couldn't save {name}: {}", e.message()));
                done(false);
            }
        }
    });
}

/// Reactions (tapbacks) on a message: a small pill of the distinct emoji, with a count when there are more
/// reactions than emoji; the tooltip says who reacted with what.
fn reactions_pill(m: &Value) -> Option<gtk::Box> {
    let rs = m["reactions"].as_array().filter(|r| !r.is_empty())?;
    let emoji = |r: &Value| match (
        r["emoji"].as_str().filter(|e| !e.is_empty()),
        r["type"].as_str(),
    ) {
        (Some(e), _) => e.to_string(),
        (None, Some("love")) => "❤️".into(),
        (None, Some("like")) => "👍".into(),
        (None, Some("dislike")) => "👎".into(),
        (None, Some("laugh")) => "😂".into(),
        (None, Some("emphasis")) => "‼️".into(),
        (None, Some("question")) => "❓".into(),
        _ => "•".into(),
    };
    let mut distinct: Vec<String> = Vec::new();
    for r in rs {
        let e = emoji(r);
        if !distinct.contains(&e) {
            distinct.push(e);
        }
    }
    let text = distinct
        .iter()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join("");
    let label = if rs.len() > distinct.len().min(3) {
        format!("{text} {}", rs.len())
    } else {
        text
    };
    let l = gtk::Label::new(Some(&label));
    let pill = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    pill.add_css_class("reactions");
    if distinct.len() == 1 && rs.len() == 1 {
        pill.add_css_class("one"); // a single reaction is a circle
    }
    l.set_hexpand(true);
    pill.append(&l);
    let tip: Vec<String> = rs
        .iter()
        .map(|r| {
            let who = if r["is_from_me"].as_bool() == Some(true) {
                "You"
            } else {
                r["sender_name"]
                    .as_str()
                    .or(r["sender"].as_str())
                    .unwrap_or("Someone")
            };
            format!("{who}: {}", emoji(r))
        })
        .collect();
    pill.set_tooltip_text(Some(&tip.join("\n")));
    Some(pill)
}

/// Where the first match of the words sits, as a y in `to`: the line of the label it is in (a long message is
/// taller than the view, so its top says nothing about where the word is).
fn match_y(ws: &[gtk::Widget], words: &[String], to: &gtk::Widget) -> Option<f32> {
    fn find(w: &gtk::Widget, words: &[String], to: &gtk::Widget) -> Option<f32> {
        if let Some(l) = w.downcast_ref::<gtk::Label>() {
            let text = l.text().to_ascii_lowercase();
            if let Some(at) = words
                .iter()
                .filter(|x| x.is_ascii() && !x.is_empty())
                .filter_map(|x| text.find(x.as_str()))
                .min()
            {
                let line = l.layout().index_to_pos(at as i32).y() as f32 / gtk::pango::SCALE as f32
                    + l.layout_offsets().1 as f32;
                return l
                    .compute_point(to, &gtk::graphene::Point::new(0.0, line))
                    .map(|p| p.y());
            }
        }
        let mut c = w.first_child();
        while let Some(x) = c {
            if let Some(y) = find(&x, words, to) {
                return Some(y);
            }
            c = x.next_sibling();
        }
        None
    }
    ws.iter().find_map(|w| find(w, words, to))
}

fn mark_labels(w: &gtk::Widget, words: &[String], strong: bool) {
    // Collected first: highlighting may move a label into an overlay, which would disturb a walk of the tree.
    fn walk(w: &gtk::Widget, out: &mut Vec<gtk::Label>) {
        if let Some(l) = w.downcast_ref::<gtk::Label>() {
            out.push(l.clone());
        }
        let mut c = w.first_child();
        while let Some(x) = c {
            walk(&x, out);
            c = x.next_sibling();
        }
    }
    let mut labels = Vec::new();
    walk(w, &mut labels);
    for l in labels {
        highlight(&l, words, strong);
    }
}

/// The words to mark in this message (only while its chat is open on a search hit), and whether this message is
/// the hit just jumped to.
fn find_words(u: &Ui, m: &Value) -> (Vec<String>, bool) {
    let st = u.st.borrow();
    match &st.find_view {
        Some(f) if m["chat_id"].as_i64() == Some(f.chat) => {
            let current = st
                .find_hits
                .get(&f.chat)
                .and_then(|h| h.get(f.idx))
                .is_some_and(|h| h["guid"] == m["guid"]);
            (search_words(u.search.text().trim()), current)
        }
        _ => (Vec::new(), false),
    }
}

/// The red "!" beside a failed message: a popup says so and offers to try again.
fn failed_button(u: &Rc<Ui>, m: &Value) -> gtk::Button {
    let kind = {
        let atts: Vec<&str> = m["attachments"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| a["mime_type"].as_str())
            .collect();
        if atts.iter().any(|t| t.starts_with("image/")) {
            "photo"
        } else if atts.iter().any(|t| t.starts_with("video/")) {
            "video"
        } else if !atts.is_empty() {
            "file"
        } else {
            "message"
        }
    };
    let b = gtk::Button::with_label("!");
    b.add_css_class("failed-btn");
    if m["id"]
        .as_i64()
        .is_some_and(|id| u.st.borrow().resent.contains(&id))
    {
        b.add_css_class("retried"); // sent again since: an outline, as Messages draws it
    }
    b.set_valign(gtk::Align::Center);
    b.set_tooltip_text(Some("Not delivered"));
    let (u, m, b2) = (u.clone(), m.clone(), b.clone());
    b.connect_clicked(move |_| {
        let note = gtk::Label::new(Some(&format!("This {kind} was not delivered.")));
        let again = gtk::Button::with_label("Try Again");
        again.add_css_class("suggested-action");
        let col = gtk::Box::new(gtk::Orientation::Vertical, 10);
        col.set_margin_top(6);
        col.set_margin_bottom(6);
        col.set_margin_start(8);
        col.set_margin_end(8);
        let hint = gtk::Label::new(Some("Trying again sends a new message. This one stays here and stays marked as not delivered on your Apple devices until you delete it there."));
        hint.add_css_class("dim-label");
        hint.set_wrap(true);
        hint.set_max_width_chars(34);
        hint.set_xalign(0.0);
        col.append(&note);
        col.append(&hint);
        col.append(&again);
        let pop = gtk::Popover::new();
        pop.set_child(Some(&col));
        pop.set_parent(&b2);
        let (u2, m2, p2) = (u.clone(), m.clone(), pop.clone());
        again.connect_clicked(move |_| {
            p2.popdown();
            retry_failed(&u2, &m2);
        });
        unparent_on_close(&pop);
        pop.popup();
    });
    b
}

/// The little swish when you send, while the window is in front (it's your own action, so no notification).
fn sent_sound(u: &Ui) {
    if u.st.borrow().cfg.notify_sound && u.win.is_active() {
        crate::sound::play("sent");
    }
}

fn send_now(u: &Rc<Ui>) {
    let buf = u.compose.buffer();
    let text = buf
        .text(&buf.start_iter(), &buf.end_iter(), false)
        .trim()
        .to_string();
    if text.is_empty() && u.st.borrow().outgoing.is_empty() {
        return;
    }
    if u.new_mode.get() {
        sent_sound(u);
        send_new(u, &text);
        return;
    }
    let Some(chat_id) = u.st.borrow().sel else {
        return;
    };
    sent_sound(u);
    let to = direct_to(u, chat_id);
    let files = take_files(u);
    buf.set_text(""); // also disables the send button (buffer changed)
    u.stick_bottom.set(true);
    let token = u.send_token.get() + 1;
    u.send_token.set(token);
    {
        // a faded bubble per message until the real one comes back through the live stream
        let mut st = u.st.borrow_mut();
        for f in &files {
            st.pending.push(Pending {
                token,
                chat_id,
                item: PendingItem::File(f.clone()),
            });
        }
        if !text.is_empty() {
            st.pending.push(Pending {
                token,
                chat_id,
                item: PendingItem::Text(text.clone()),
            });
        }
    }
    u.send(Cmd::Send {
        chat_id,
        to,
        text,
        files,
        token,
    });
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
    let d = gtk::FileDialog::builder()
        .title("Attach files")
        .modal(true)
        .build();
    let u = u.clone();
    glib::spawn_future_local(async move {
        if let Ok(list) = d.open_multiple_future(Some(&u.win)).await {
            let files = (0..list.n_items())
                .filter_map(|i| list.item(i).and_downcast::<gio::File>()?.path())
                .collect();
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
        let name = f
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let card = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        card.add_css_class("attach-chip");
        card.set_tooltip_text(Some(&name));
        if let Some(t) = is_image("", &name)
            .then(|| gdk::Texture::from_filename(&f).ok())
            .flatten()
        {
            // ponytail: whole image decoded for a 64 px thumbnail; fine for a few files
            let pic = gtk::Image::from_paintable(Some(&t));
            pic.set_pixel_size(64);
            pic.add_css_class("attach-thumb");
            card.append(&pic);
        } else {
            let icon = gtk::Image::from_icon_name("bf-document-symbolic");
            icon.set_pixel_size(24);
            card.append(&icon);
            let l = gtk::Label::new(Some(&name));
            l.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            l.set_max_width_chars(18);
            card.append(&l);
        }
        let x = gtk::Button::from_icon_name("bf-close-symbolic");
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
    for w in [
        u.head_avatar.upcast_ref::<gtk::Widget>(),
        u.head_titles.upcast_ref(),
        u.scroller.upcast_ref(),
    ] {
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
        .filter_map(|c| {
            Some((
                c["id"].as_i64()?,
                chat_title(c, &st.names),
                direct_addr(c)?.to_string(),
            ))
        })
        .filter(|(_, name, addr)| {
            q.is_empty()
                || name.to_lowercase().contains(&q)
                || addr.to_lowercase().contains(&q)
                || same_addr(&q, addr)
        })
        .take(8)
        .map(|(id, n, a)| (Some(id), n, a))
        .collect::<Vec<_>>()
        .into_iter()
        .chain(
            match (&st.contacts, q.is_empty()) {
                (Some(c), false) => c.search(&q), // ponytail: nothing listed until you type; a 360-row list helps nobody
                _ => vec![],
            }
            .into_iter()
            .filter(|(_, a)| {
                !st.chats
                    .iter()
                    .any(|c| direct_addr(c).is_some_and(|d| same_addr(a, d)))
            })
            .take(8)
            .map(|(n, a)| (None, n.to_string(), a.to_string())),
        )
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
            let label =
                u.st.borrow()
                    .contacts
                    .as_ref()
                    .and_then(|c| c.label(&addr).map(str::to_string));
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
/// What makes two 1:1 chats one conversation: the same address-book entry, or (for someone who isn't a contact, or
/// before the contacts are read) the same number or email.
fn merge_key(handle: &str, contacts: Option<&crate::mac::Contacts>) -> Option<String> {
    match contacts.and_then(|k| k.person_id(handle)) {
        Some(p) => Some(format!("person:{p}")),
        None => crate::mac::addr_key(handle).map(|a| format!("addr:{a}")),
    }
}

/// One person's chats (a phone number and an email, iMessage and SMS ones) are one conversation, as in Messages:
/// the list shows a single entry standing for all of them. It is the newest chat's entry with the id of the oldest
/// (an id that doesn't change), the unread counts added up, and `merged` naming every chat. Group chats stay as they
/// are. The contacts tell which numbers and emails are one person; without them the same address still merges. Returns the list and the merges as
/// (id, all ids newest first).
fn merged_chats(
    raw: &[Value],
    contacts: Option<&crate::mac::Contacts>,
) -> (Vec<Value>, Vec<(i64, Vec<i64>)>) {
    let mut slot_of: HashMap<String, usize> = HashMap::new(); // person (or bare address) -> position in `out`
    let mut out: Vec<Value> = Vec::new();
    let mut members: Vec<Vec<usize>> = Vec::new(); // raw positions behind each entry of `out`
    for (i, c) in raw.iter().enumerate() {
        let person = if c["is_group"].as_bool() == Some(true) {
            None
        } else {
            c["participants"][0]
                .as_str()
                .and_then(|h| merge_key(h, contacts))
        };
        match person.as_ref().and_then(|p| slot_of.get(p).copied()) {
            Some(slot) => members[slot].push(i),
            None => {
                if let Some(p) = person {
                    slot_of.insert(p, out.len());
                }
                out.push(c.clone());
                members.push(vec![i]);
            }
        }
    }
    let mut groups = Vec::new();
    for (slot, ms) in members.iter().enumerate().filter(|(_, m)| m.len() > 1) {
        let ids: Vec<i64> = ms.iter().filter_map(|&i| raw[i]["id"].as_i64()).collect();
        let Some(&canon) = ids.iter().min() else {
            continue;
        };
        let unread: i64 = ms
            .iter()
            .map(|&i| raw[i]["unread_count"].as_i64().unwrap_or(0))
            .sum();
        let c = &mut out[slot];
        c["id"] = Value::from(canon);
        c["unread_count"] = Value::from(unread);
        c["merged"] = Value::from(ids.clone());
        // pins and mutes are kept by chat identifier; any of the chats' identifiers counts (which of them are loaded
        // changes as older chats load, so none can stand for the whole)
        c["merged_keys"] = Value::from(ms.iter().map(|&i| pin_key(&raw[i])).collect::<Vec<_>>());
        groups.push((canon, ids));
    }
    (out, groups)
}

/// Order a group's people for showing: those with a contact name first, the people you wrote to most recently ahead of
/// the others (numbers and emails last, in the order they came). Messages does something like this (a group of Jo, Rob
/// and Drayven reads "Jo, Rob & Drayven": the people you message most).
fn rank_members(
    handles: &[String],
    known: impl Fn(&str) -> bool,
    when: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    let mut v: Vec<(bool, Option<String>, usize, &String)> = handles
        .iter()
        .enumerate()
        .map(|(i, h)| (known(h), when(h), i, h))
        .collect();
    // known first; then newest first (those with no conversation after the others); ties in the original order
    v.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| {
                if a.0 {
                    b.1.cmp(&a.1)
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .then(a.2.cmp(&b.2))
    });
    v.into_iter().map(|(_, _, _, h)| h.clone()).collect()
}

/// Put every group's `participants` in the order `rank_members` gives, from when you last wrote to each person.
fn rank_group_members(st: &State, raw: &mut [Value]) {
    let Some(k) = st.contacts.as_ref() else {
        return;
    };
    let mut by_addr: HashMap<String, String> = HashMap::new();
    let mut by_person: HashMap<usize, String> = HashMap::new();
    for c in raw.iter().filter(|c| c["is_group"].as_bool() != Some(true)) {
        let (Some(h), Some(t)) = (c["participants"][0].as_str(), c["last_message_at"].as_str())
        else {
            continue;
        };
        if let Some(a) = crate::mac::addr_key(h) {
            let e = by_addr.entry(a).or_default();
            if t > e.as_str() {
                *e = t.to_string();
            }
        }
        if let Some(p) = k.person_id(h) {
            let e = by_person.entry(p).or_default();
            if t > e.as_str() {
                *e = t.to_string();
            }
        }
    }
    let known =
        |h: &str| st.names.contains_key(h) || k.person(h).is_some_and(|p| !p.name.is_empty());
    let when = |h: &str| {
        k.person_id(h)
            .and_then(|p| by_person.get(&p))
            .or_else(|| crate::mac::addr_key(h).and_then(|a| by_addr.get(&a)))
            .cloned()
    };
    for c in raw
        .iter_mut()
        .filter(|c| c["is_group"].as_bool() == Some(true))
    {
        let handles: Vec<String> = c["participants"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p.as_str().map(String::from))
            .collect();
        if handles.len() > 1 {
            c["participants"] = Value::from(rank_members(&handles, known, when));
        }
    }
}

/// Replace the chat list with `raw` (as imsg gave it), merged by person. Returns the merges.
fn raw_ids_of(raw: &[Value]) -> HashSet<i64> {
    raw.iter().filter_map(|c| c["id"].as_i64()).collect()
}

fn set_chats(st: &mut State, raw: Vec<Value>) -> Vec<(i64, Vec<i64>)> {
    let mut raw: Vec<Value> = raw
        .into_iter()
        .filter(|c| c["hidden"].as_bool() != Some(true))
        .collect(); // (chats without a message)
    rank_group_members(st, &mut raw);
    let (merged, groups) = merged_chats(&raw, st.contacts.as_ref());
    let alias: HashMap<i64, i64> = groups
        .iter()
        .flat_map(|(c, ids)| ids.iter().map(move |i| (*i, *c)))
        .collect();
    if let Some(c) = st.sel.and_then(|s| alias.get(&s)) {
        st.sel = Some(*c); // the chat on show is now part of a merged one
    }
    // the chat on show may be older than the list's 100 (found by address): keep it
    // (and the ones a search added, while the results are showing)
    let keep: Vec<Value> = st
        .chats
        .iter()
        .filter(|c| {
            c["id"].as_i64().is_some_and(|id| {
                (st.sel == Some(id) || st.search_added.contains(&id))
                    && !merged.iter().any(|m| m["id"].as_i64() == Some(id))
            })
        })
        .cloned()
        .collect();
    st.raw_chats = raw;
    st.chats = merged;
    for c in keep {
        insert_chat(&mut st.chats, c);
    }
    let (sel, loaded): (Option<i64>, HashSet<i64>) = (
        st.sel,
        st.chats.iter().filter_map(|c| c["id"].as_i64()).collect(),
    );
    let listed: HashSet<i64> = raw_ids_of(&st.raw_chats);
    st.off_list
        .retain(|id| Some(*id) == sel && loaded.contains(id) && !listed.contains(id));
    st.alias = alias;
    groups
}

/// A new merge: the backend is told, and what was loaded for a conversation whose chats changed is read again.
fn after_merge(u: &Rc<Ui>, groups: Vec<(i64, Vec<i64>)>) {
    let changed = {
        let mut st = u.st.borrow_mut();
        if st.merge_sig == groups {
            return;
        }
        let old = std::mem::replace(&mut st.merge_sig, groups.clone());
        let changed: Vec<i64> = groups
            .iter()
            .filter(|g| !old.contains(g))
            .map(|g| g.0)
            .collect();
        for c in &changed {
            st.hist.remove(c);
            st.preview.remove(c);
            st.preview_asked.remove(c);
        }
        changed
    };
    u.send(Cmd::Merge(groups));
    for c in &changed {
        u.send(Cmd::Preview(*c));
    }
    if u.st.borrow().sel.is_some_and(|s| changed.contains(&s)) {
        refresh_chat(u);
    }
}

/// The tiles were dragged into a new order: keep it as the pin order (one entry per tile; pins of chats that aren't
/// loaded stay after them).
fn reorder_pins(u: &Rc<Ui>, order: Vec<gtk::Widget>) {
    {
        let mut st = u.st.borrow_mut();
        let mut mine: Vec<String> = Vec::new();
        let mut used: Vec<String> = Vec::new();
        for t in &order {
            let Ok(id) = t.widget_name().parse::<i64>() else {
                continue;
            };
            let Some(c) = st.chats.iter().find(|c| c["id"].as_i64() == Some(id)) else {
                continue;
            };
            let keys = keys_of(c);
            // the key it is saved under now (or its first)
            let pick = keys
                .iter()
                .filter_map(|k| st.pins.iter().position(|p| p == k).map(|i| (i, k)))
                .min()
                .map(|(_, k)| k.clone())
                .or_else(|| keys.first().cloned());
            used.extend(keys);
            mine.extend(pick);
        }
        let rest: Vec<String> = st
            .pins
            .iter()
            .filter(|p| !used.contains(p))
            .cloned()
            .collect();
        mine.extend(rest);
        st.pins = mine;
        if std::env::var_os("BF_PINS").is_none() {
            // (dev runs with invented pins must not overwrite the real file)
            let _ = std::fs::write(
                &u.store.pins_file,
                st.pins.iter().map(|p| format!("{p}\n")).collect::<String>(),
            );
        }
    }
    // the list's hidden rows follow the order of `chats`, not the pins: nothing else to redraw
}

fn insert_chat(chats: &mut Vec<Value>, chat: Value) {
    let at = chats
        .iter()
        .position(|c| c["last_message_at"].as_str() < chat["last_message_at"].as_str())
        .unwrap_or(chats.len());
    chats.insert(at, chat);
}

/// The typed address first, then the other numbers and emails of the contact it belongs to: Messages keeps one
/// conversation per person, whichever of their addresses is used.
fn person_addrs(u: &Ui, typed: &str) -> Vec<String> {
    let mut v = vec![clean_addr(typed)];
    if let Some(p) =
        u.st.borrow()
            .contacts
            .as_ref()
            .and_then(|c| c.person(typed))
    {
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
    let Some(adj) = side_adjustment(u) else {
        return;
    };
    u.side_anim.set(true);
    let (u, from, began) = (u.clone(), adj.value(), Cell::new(None::<i64>));
    glib::timeout_add_local_once(Duration::from_millis(150), move || {
        // already in view where the list was scrolled to (list rebuilds since may have reset it): leave it there
        let idx =
            u.st.borrow()
                .chats
                .iter()
                .position(|c| c["id"].as_i64() == Some(id));
        if let Some(b) = idx
            .and_then(|i| u.chat_list.row_at_index(i as i32))
            .and_then(|r| r.compute_bounds(&u.tiles.parent()?))
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
            let idx =
                u.st.borrow()
                    .chats
                    .iter()
                    .position(|c| c["id"].as_i64() == Some(id));
            if let Some(b) = idx
                .and_then(|i| u.chat_list.row_at_index(i as i32))
                .and_then(|r| r.compute_bounds(&u.tiles.parent()?))
                && b.height() > 1.0
            {
                to.set(Some(
                    (b.y() as f64 - 6.0).clamp(0.0, (adj.upper() - adj.page_size()).max(0.0)),
                ));
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
        show_toast(
            u,
            "Who's it to? Pick a conversation or type a phone number or email.",
        );
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
            st.pending.push(Pending {
                token,
                chat_id: 0,
                item: PendingItem::File(f.clone()),
            });
        }
        if !text.is_empty() {
            st.pending.push(Pending {
                token,
                chat_id: 0,
                item: PendingItem::Text(text.to_string()),
            });
        }
    }
    u.send(Cmd::Send {
        chat_id: 0,
        to: Some(to),
        text: text.to_string(),
        files,
        token,
    });
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
    digits >= 5
        && s.chars()
            .all(|c| c.is_ascii_digit() || " +-().".contains(c))
}

fn clean_addr(s: &str) -> String {
    let s = s.trim();
    if s.contains('@') {
        s.to_lowercase()
    } else {
        s.chars()
            .filter(|c| c.is_ascii_digit() || *c == '+')
            .collect()
    }
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
    n >= 5
        && (a.len() == b.len() || n >= 7)
        && a.ends_with(&b[b.len() - n..])
        && b.ends_with(&a[a.len() - n..])
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
fn row_info(st: &State, c: &Value) -> (String, String, Option<i64>, bool) {
    let id = c["id"].as_i64().unwrap_or(0);
    let p = st.preview.get(&id);
    let when = p
        .and_then(|m| m["created_at"].as_str())
        .or(c["last_message_at"].as_str())
        .and_then(local)
        .map(|d| d.to_unix());
    let text = p.map(|m| {
        let s = summary(m);
        if m["is_from_me"].as_bool() == Some(true) {
            format!("You: {s}")
        } else {
            s
        }
    });
    // read here but not on the Mac (imsg can't mark read): trust what we've shown
    let seen_here = p
        .and_then(|m| m["id"].as_i64())
        .is_some_and(|last| st.seen.get(&id).is_some_and(|s| *s >= last));
    let unread = c["unread_count"].as_i64().unwrap_or(0) > 0 && !seen_here;
    (
        chat_title(c, &st.names),
        text.unwrap_or_default(),
        when,
        unread,
    )
}

/// `update_title` once, after the events that are waiting: it counts every chat, so doing it per preview is O(n²).
fn update_title_soon(u: &Rc<Ui>) {
    if !u.title_queued.replace(true) {
        let u2 = u.clone();
        glib::idle_add_local_once(move || {
            u2.title_queued.set(false);
            update_title(&u2);
        });
    }
}

/// Put the icon in the tray (once: a second item would be a second icon) and keep the app alive while it's there.
/// `hidden`: the app was started at login, so if there is no tray after all the window opens minimized.
fn start_tray(u: &Rc<Ui>, hidden: bool) {
    let Some(rt) = crate::setup::RT.get() else {
        return;
    };
    if u.tray.borrow().is_some() {
        return;
    }
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    // started at login: the tray host may not be up yet, so keep trying for ~10 s before giving up
    let tries = if hidden { 20 } else { 1 };
    rt.spawn(async move {
        let mut r = crate::tray::start(tx.clone()).await;
        for _ in 1..tries {
            if r.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            r = crate::tray::start(tx.clone()).await;
        }
        let _ = done_tx.send(r);
    });
    let u = u.clone();
    glib::MainContext::default().spawn_local(async move {
        let handle = match done_rx.await {
            Ok(Ok(h)) => h,
            r => {
                eprintln!("no tray icon: {:?}", r.err());
                if hidden {
                    u.win.present();
                    present_minimized(&u.win);
                }
                return;
            }
        };
        if u.tray.borrow().is_some() {
            return; // turned on twice while the first was starting
        }
        *u.tray.borrow_mut() = Some(handle);
        *u.hold.borrow_mut() = Some(u.app.hold());
        update_title(&u); // the tray starts with no unread dot
        while let Some(e) = rx.recv().await {
            match e {
                crate::tray::Event::Show => u.win.present(),
                crate::tray::Event::Toggle if u.win.is_visible() && u.win.is_active() => {
                    u.win.set_visible(false)
                }
                crate::tray::Event::Toggle => u.win.present(),
                crate::tray::Event::Quit => u.app.quit(),
            }
        }
    });
}

/// Minimize a window that was just presented (Wayland has no "start minimized", so it shows for a moment first).
fn present_minimized(win: &gtk::ApplicationWindow) {
    let w = win.clone();
    glib::timeout_add_local_once(Duration::from_millis(300), move || w.minimize());
}

/// The first time the window is closed into the tray, say where the app went (a notification: the desktop
/// puts it by the tray and fades it); remembered in a file so it only happens once.
fn tray_hint(u: &Rc<Ui>) {
    let flag = u.store.seen_file.with_file_name("tray-hint");
    if flag.exists() {
        return;
    }
    // Sent straight to the notification service (the Flatpak already may talk to it) because GNotification can't carry
    // KDE's `x-kde-user-action-feedback` hint, which Plasma shows even in do-not-disturb (it's the answer to the user's own
    // action, like Spectacle's "screenshot saved").
    glib::spawn_future_local(async move {
        let Ok(bus) = gio::bus_get_future(gio::BusType::Session).await else {
            return;
        };
        let hints = HashMap::from([
            ("desktop-entry".to_string(), crate::APP_ID.to_variant()),
            ("x-kde-user-action-feedback".to_string(), true.to_variant()),
        ]);
        let args = (
            "BubbleFairy",
            0u32,
            crate::APP_ID,
            "BubbleFairy is still running",
            "It's in the system tray, so messages keep coming. Right-click the icon and choose Quit to close it.",
            Vec::<String>::new(),
            hints,
            -1i32,
        )
            .to_variant();
        let sent = bus
            .call_future(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications",
                "Notify",
                Some(&args),
                None,
                gio::DBusCallFlags::NONE,
                2000,
            )
            .await;
        if sent.is_ok() {
            let _ = std::fs::write(flag, "");
        }
    });
}

/// The window's title (what a taskbar or window list shows) carries the number of unread conversations.
fn update_title(u: &Ui) {
    let n = {
        let st = u.st.borrow();
        st.chats.iter().filter(|c| row_info(&st, c).3).count()
    };
    u.win.set_title(Some(&if n > 0 {
        format!("({n}) BubbleFairy")
    } else {
        "BubbleFairy".into()
    }));
    let (badge, gap) = &u.title_badge;
    badge.set_text(&if n > 99 {
        "99+".to_string()
    } else {
        n.to_string()
    });
    badge.set_visible(n > 0);
    gap.set_visible(n > 0);
    if let (Some(h), Some(rt)) = (u.tray.borrow().clone(), crate::setup::RT.get()) {
        crate::tray::UNREAD.store(n > 0, std::sync::atomic::Ordering::Relaxed);
        rt.spawn(async move { h.update(|_| {}).await }); // makes the icon re-read the flag
    }
}

/// The account the local copy belongs to.
fn account(u: &Ui) -> String {
    let st = u.st.borrow();
    format!("{}@{}", st.cfg.user, st.cfg.host)
}

/// Whether the local copy (see snapshot.rs) is in use: switched on in Settings, and not the demo.
fn keeps_copy(u: &Ui) -> bool {
    let st = u.st.borrow();
    st.cfg.local_cache && !st.cfg.host.is_empty() && st.cfg.host != "demo"
}

fn clear_snapshot(u: &Ui) {
    crate::snapshot::clear(&[&u.store.chats_cache, &u.store.contacts_cache]);
}

/// Save the chat list and the previews of its rows a few seconds after the last change (one timer at a time).
fn snapshot_soon(u: &Rc<Ui>) {
    if !keeps_copy(u) || u.snap_queued.replace(true) {
        return;
    }
    let u = u.clone();
    glib::timeout_add_local_once(Duration::from_secs(3), move || {
        u.snap_queued.set(false);
        if !keeps_copy(&u) {
            return;
        }
        let (chats, previews, keep) = {
            let st = u.st.borrow();
            if !st.chats_loaded {
                return;
            }
            let chats: Vec<Value> = st.raw_chats.iter().take(CHATS_PAGE).cloned().collect();
            // previews are filed under a merged chat's id as well as the chats' own
            let keep: HashSet<i64> = chats
                .iter()
                .chain(&st.chats)
                .filter_map(|c| c["id"].as_i64())
                .collect();
            let previews: HashMap<i64, Value> = st
                .preview
                .iter()
                .filter(|(id, _)| keep.contains(id))
                .map(|(id, m)| (*id, m.clone()))
                .collect();
            (chats, previews, keep)
        };
        let (path, who) = (u.store.chats_cache.clone(), account(&u));
        bg(
            move || crate::snapshot::save_chats(&path, &who, &chats, &previews, &keep),
            |_| {},
        );
    });
}

/// Show the saved chat list and contacts right away, by handing them to the same code the Mac's answers go through.
/// Whatever the Mac sends later replaces them; if its answer got here first, the saved part is skipped.
fn load_snapshot(u: &Rc<Ui>) {
    if !keeps_copy(u) {
        return;
    }
    let (chats_path, contacts_path, who) = (
        u.store.chats_cache.clone(),
        u.store.contacts_cache.clone(),
        account(u),
    );
    let u = u.clone();
    bg(
        move || crate::snapshot::load(&chats_path, &contacts_path, &who),
        move |(chats, contacts)| {
            if let Some(c) = contacts
                && u.st.borrow().contacts.is_none()
            {
                on_event(&u, Event::Contacts(c));
                u.st.borrow_mut().contacts_cached = true;
            }
            if let Some(s) = chats
                && !u.st.borrow().chats_loaded
            {
                {
                    let mut st = u.st.borrow_mut();
                    for (id, m) in s.previews {
                        st.preview.entry(id).or_insert(m);
                    }
                }
                on_event(&u, Event::Chats(s.chats));
            }
        },
    );
}

/// A chat's last message arrived: change its row's texts in place. Rebuilding the whole list for each of the
/// ~60 previews that arrive at start-up cost 25+ ms apiece. Anything the row doesn't hold yet (a new chat, a
/// pinned chat's tile) goes through a rebuild instead.
fn update_row(u: &Rc<Ui>, id: i64) {
    snapshot_soon(u);
    let parts = u.row_parts.borrow().get(&id).cloned();
    let info = {
        let st = u.st.borrow();
        st.chats
            .iter()
            .find(|c| c["id"].as_i64() == Some(id))
            .map(|c| (row_info(&st, c), pin_pos(&st.pins, c).is_some()))
    };
    // a chat that isn't in the list any more (one a search had added): nothing to update, and no rebuild for it
    if info.is_none() {
        return;
    }
    let (Some(parts), Some(((_, text, when, unread), pinned))) = (parts, info) else {
        return render_chats_soon(u);
    };
    if pinned {
        return render_chats_soon(u); // its tile shows the unread dot
    }
    if u.styled.get() {
        // a search is on: the row shows its match, not the newest message
        let words = search_words(u.search.text().trim());
        let st = u.st.borrow();
        style_row(
            &st,
            &words,
            true,
            id,
            &parts,
            st.chats.iter().find(|c| c["id"].as_i64() == Some(id)),
        );
    } else {
        parts.prev.set_text(&text);
    }
    parts.time_unix.set(when);
    if let Some(t) = when {
        parts.time.set_text(&rel_time(t, TimeKind::Row)); // only this row's: rewriting every tracked time per preview was O(n) each
    }
    for (c, on) in [("unread-dot", unread), ("read-dot", !unread)] {
        if on {
            parts.dot.add_css_class(c)
        } else {
            parts.dot.remove_css_class(c)
        }
    }
    if unread {
        parts.row.add_css_class("unread")
    } else {
        parts.row.remove_css_class("unread")
    }
    update_title_soon(u);
}

/// Names and photos have arrived (the Contacts are read after the chat list is shown): change what the rows and
/// tiles show in place. No rebuild of the list, so nothing hitches and the scroll position stays.
fn refresh_identities(u: &Rc<Ui>) {
    let parts: Vec<(i64, RowParts)> = u
        .row_parts
        .borrow()
        .iter()
        .map(|(k, v)| (*k, v.clone()))
        .collect();
    let info: HashMap<i64, (Value, String)> = {
        let st = u.st.borrow();
        st.chats
            .iter()
            .filter_map(|c| Some((c["id"].as_i64()?, (c.clone(), chat_title(c, &st.names)))))
            .collect()
    };
    for (id, p) in &parts {
        let Some((chat, title)) = info.get(id) else {
            continue;
        };
        if p.name.text() != *title {
            p.name.set_text(title);
        }
        if p.short.text() != short_title(chat, title) {
            p.short.set_text(&short_title(chat, title));
        }
        put_avatar(&p.stack, &chat_avatar(u, chat, title));
    }
    // the collapsed sidebar's tooltips read the titles kept in `rows`
    for (row, _, t) in u.rows.borrow_mut().iter_mut() {
        if let Some((_, title)) = parts
            .iter()
            .find(|(_, p)| p.row == *row)
            .and_then(|(id, _)| info.get(id))
        {
            *t = title.clone();
            if u.collapsed.get() {
                row.set_tooltip_text(Some(title));
            }
        }
    }
    rebuild_tiles(u);
    style_rows(u);
    if !u.search.text().trim().is_empty() {
        u.chat_list.invalidate_filter(); // a name that has just arrived may match
    }
}

/// A pinned chat's tile: position in the pin list, chat id, title, unread flag, the chat itself.
type PinnedTile = (usize, i64, String, bool, Value);

/// The pinned tiles are few: redraw just them (names and photos), not the list.
fn rebuild_tiles(u: &Rc<Ui>) {
    let (pinned, sel): (Vec<PinnedTile>, Option<i64>) = {
        let st = u.st.borrow();
        let mut v = Vec::new();
        for c in &st.chats {
            if let Some(i) = pin_pos(&st.pins, c) {
                let (title, _, _, unread) = row_info(&st, c);
                v.push((i, c["id"].as_i64().unwrap_or(0), title, unread, c.clone()));
            }
        }
        v.sort_by_key(|p| p.0);
        (v, st.sel)
    };
    render_tiles(u, pinned, sel);
}

/// The widgets of a list row that change when its chat's last message does.
#[derive(Clone)]
struct RowParts {
    row: gtk::ListBoxRow,
    time_unix: Rc<Cell<Option<i64>>>,
    time: gtk::Label,
    name: gtk::Label,
    /// avatar on top, the short name under it (shown only in the avatars-and-names sidebar)
    stack: gtk::Box,
    short: gtk::Label,
    prev: gtk::Label,
    /// "N matches" in the preview line (wide sidebar) and the number over the avatar's corner (avatars-only sidebar)
    pill: gtk::Label,
    badge: gtk::Label,
    dot: gtk::Box,
}

fn render_chats(u: &Rc<Ui>) {
    let t0 = std::time::Instant::now();
    render_chats_inner(u);
    if std::env::var_os("BF_PROF").is_some() {
        eprintln!(
            "prof: render_chats {} ms ({} rows)",
            t0.elapsed().as_millis(),
            u.row_parts.borrow().len()
        );
    }
}

fn render_chats_inner(u: &Rc<Ui>) {
    let rows: Vec<(String, String, Option<i64>, bool, i64, Value)> = {
        let st = u.st.borrow();
        st.chats
            .iter()
            .map(|c| {
                let (title, text, when, unread) = row_info(&st, c);
                (
                    title,
                    text,
                    when,
                    unread,
                    c["id"].as_i64().unwrap_or(0),
                    c.clone(),
                )
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
    let (pins, muted) = {
        let st = u.st.borrow();
        (st.pins.clone(), st.muted.clone())
    };
    let mut pinned: Vec<PinnedTile> = Vec::new();
    for (title, preview, when, unread, id, chat) in rows {
        let keys = keys_of(&chat);
        let pin_at = pin_pos(&pins, &chat);
        let name = gtk::Label::new(Some(&title));
        name.add_css_class("chat-name");
        name.set_xalign(0.0);
        name.set_hexpand(true);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let time = gtk::Label::new(None);
        time.add_css_class("chat-time");
        let time_unix = track_time(u, &time, when, TimeKind::Row, None);
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        top.append(&name);
        let is_muted = is_muted(&muted, &chat);
        if is_muted {
            let m = gtk::Label::new(Some("🔕"));
            m.add_css_class("chat-time");
            m.set_tooltip_text(Some("Muted"));
            top.append(&m);
        }
        top.append(&time);
        let prev = gtk::Label::new(Some(&preview));
        prev.add_css_class("chat-preview");
        prev.set_xalign(0.0);
        prev.set_ellipsize(gtk::pango::EllipsizeMode::End);
        prev.set_single_line_mode(true);
        prev.set_hexpand(true);
        let pill = gtk::Label::new(None);
        pill.add_css_class("count-pill");
        pill.set_valign(gtk::Align::Center);
        pill.set_visible(false);
        let prev_line = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        prev_line.append(&pill);
        prev_line.append(&prev);
        let texts = gtk::Box::new(gtk::Orientation::Vertical, 2);
        texts.set_hexpand(true);
        texts.add_css_class("chat-texts");
        texts.set_valign(gtk::Align::Center);
        texts.append(&top);
        texts.append(&prev_line);
        // The avatar is the row's only measured content, so the row can be as narrow as a circle round it;
        // the texts sit beside it at a width of their own and are cropped by the row as the sidebar narrows.
        let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        dot.set_valign(gtk::Align::Center);
        dot.add_css_class("row-dot");
        dot.add_css_class(if unread { "unread-dot" } else { "read-dot" });
        let slot = gtk::Box::new(gtk::Orientation::Horizontal, 11);
        slot.append(&texts);
        slot.append(&dot);
        slot.add_css_class("row-slot");
        slot.set_halign(gtk::Align::Start);
        slot.set_valign(gtk::Align::Center);
        slot.set_margin_start(AV_PAD + AVATAR + 11);
        slot.set_size_request(u.text_w.get() + 21, -1);
        let stack = gtk::Box::new(gtk::Orientation::Vertical, 4);
        stack.set_size_request(stack_w(u.collapsed.get()), -1);
        stack.set_valign(gtk::Align::Center);
        stack.set_halign(gtk::Align::Start); // not centred in a wide row: the texts sit beside it
        put_avatar(&stack, &chat_avatar(u, &chat, &title));
        let short = gtk::Label::new(Some(&short_title(&chat, &title)));
        short.add_css_class("tile-name");
        short.set_ellipsize(gtk::pango::EllipsizeMode::End);
        short.set_width_chars(1);
        short.set_max_width_chars(9);
        short.set_visible(u.collapsed.get());
        stack.append(&short);
        let row_box = gtk::Overlay::new();
        row_box.set_child(Some(&stack));
        row_box.add_overlay(&slot);
        row_box.set_clip_overlay(&slot, true);
        let badge = gtk::Label::new(None);
        badge.add_css_class("count-pill");
        badge.add_css_class("count-badge");
        badge.set_halign(gtk::Align::End);
        badge.set_valign(gtk::Align::Start);
        badge.set_visible(false);
        row_box.add_overlay(&badge);
        let row = gtk::ListBoxRow::new();
        if u.collapsed.get() {
            row.set_tooltip_text(Some(&title));
        }
        new_rows.push((row.clone(), slot, title.clone()));
        u.row_parts.borrow_mut().insert(
            id,
            RowParts {
                row: row.clone(),
                time_unix,
                time: time.clone(),
                name: name.clone(),
                stack: stack.clone(),
                short: short.clone(),
                prev: prev.clone(),
                pill: pill.clone(),
                badge: badge.clone(),
                dot: dot.clone(),
            },
        );
        row.add_css_class("chat-row");
        if unread {
            row.add_css_class("unread");
        }
        row.set_child(Some(&row_box));
        pin_menu(u, row.upcast_ref(), &keys, pin_at.is_some(), is_muted);
        if let Some(i) = pin_at {
            // hidden, not left out: row indexes must keep matching `State.chats`
            row.set_visible(!u.search.text().trim().is_empty());
            pinned.push((i, id, title.clone(), unread, chat.clone()));
        } else if u.st.borrow().off_list.contains(&id) {
            row.set_visible(!u.search.text().trim().is_empty()); // open, but not in the loaded list
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
    update_title(u);
    style_rows(u);
    if let (Some(adj), true) = (side_adj, keep > 0.0 && !u.side_anim.get()) {
        // the new rows have no size until the next layout: set the value again each frame until it holds
        let (n, u) = (Cell::new(0), u.clone());
        let list = u.chat_list.clone();
        list.add_tick_callback(move |_, _| {
            n.set(n.get() + 1);
            if !u.side_anim.get() {
                adj.set_value(keep);
            }
            if n.get() < 10 && adj.upper() - adj.page_size() < keep - 0.5 {
                glib::ControlFlow::Continue
            } else {
                glib::ControlFlow::Break
            }
        });
    }
}

/// Does this list row's chat match what is typed in the search box (name, or part of a number or address)?
fn row_matches(u: &Ui, row: &gtk::ListBoxRow) -> bool {
    let q = u.search.text().trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    let Ok(st) = u.st.try_borrow() else {
        return true;
    };
    let Some(c) = usize::try_from(row.index())
        .ok()
        .and_then(|i| st.chats.get(i))
    else {
        return true;
    };
    if c["id"]
        .as_i64()
        .is_some_and(|id| st.find_counts.contains_key(&id) || st.find_hits.contains_key(&id))
    {
        return true; // a message in it matches
    }
    let digits: String = q.chars().filter(char::is_ascii_digit).collect();
    chat_title(c, &st.names).to_lowercase().contains(&q)
        || c["participants"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p.as_str())
            .any(|h| {
                let name = st
                    .names
                    .get(h)
                    .map(|n| n.to_lowercase())
                    .unwrap_or_default();
                let plain: String = h.chars().filter(char::is_ascii_digit).collect();
                name.contains(&q)
                    || h.to_lowercase().contains(&q)
                    || (digits.len() >= 3
                        && q.chars()
                            .all(|ch| ch.is_ascii_digit() || " -()+".contains(ch))
                        && plain.contains(&digits))
            })
}

const SEARCH_MS: f64 = 200.0;

/// Put the field over the title: from the magnifier button's left edge to the settings button. `p` is how far it has
/// grown (0 = the button's own size and invisible, 1 = full width), so the button fades out as the field fades in.
fn search_place(u: &Ui, p: f64) {
    let (Some(b), Some(s)) = (
        u.search_btn.compute_bounds(&u.head_overlay),
        u.settings_btn.compute_bounds(&u.head_overlay),
    ) else {
        return;
    };
    let (x, w0) = (b.x() as f64, b.width() as f64);
    let full = (s.x() as f64 - 10.0 - x).max(w0);
    let e = 1.0 - (1.0 - p).powi(3);
    u.search_wrap.set_margin_start(x.round() as i32);
    u.search_wrap
        .set_width_request((w0 + (full - w0) * e).round() as i32);
    u.search_wrap.set_opacity(e);
    u.search_btn.set_opacity(1.0 - e);
    u.search_btn.set_can_target(p < 0.5);
    // the title gives way to the field (it would show through its translucent fill)
    if let Some(t) = u
        .head_overlay
        .child()
        .and_then(|h| h.downcast::<gtk::HeaderBar>().ok())
        .and_then(|h| h.title_widget())
    {
        t.set_opacity(1.0 - e);
    }
}

/// Open or fold the header search field, with a short fade-and-grow.
fn search_to(u: &Rc<Ui>, open: bool) {
    u.search_open.set(open);
    if open {
        u.search_wrap.set_visible(true);
    }
    if u.search_tick.replace(true) {
        return; // already moving: it turns round
    }
    let (u2, last) = (u.clone(), Cell::new(0_i64));
    u.head_overlay.add_tick_callback(move |_, clock| {
        let now = clock.frame_time();
        let dt = if last.get() == 0 {
            16_000.0
        } else {
            (now - last.get()) as f64
        };
        last.set(now);
        let target = if u2.search_open.get() { 1.0 } else { 0.0 };
        let p = u2.search_p.get();
        let p = if p < target {
            (p + dt / 1000.0 / SEARCH_MS).min(target)
        } else {
            (p - dt / 1000.0 / SEARCH_MS).max(target)
        };
        u2.search_p.set(p);
        search_place(&u2, p);
        if p == target {
            u2.search_tick.set(false);
            if !u2.search_open.get() {
                u2.search_wrap.set_visible(false);
            }
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });
}

fn open_search(u: &Rc<Ui>) {
    search_to(u, true);
    u.search.grab_focus();
}

/// Esc: the text goes and the field folds back into the button.
fn close_search(u: &Rc<Ui>) {
    u.search.set_text("");
    search_to(u, false);
    if u.st.borrow().sel.is_some() {
        u.compose.grab_focus();
    }
}

/// What is typed in the search box has changed.
fn apply_search(u: &Rc<Ui>) {
    let t0 = std::time::Instant::now();
    apply_search_inner(u);
    if std::env::var_os("BF_PROF").is_some() {
        eprintln!("prof: apply_search {} ms", t0.elapsed().as_millis());
    }
}

fn apply_search_inner(u: &Rc<Ui>) {
    let searching = !u.search.text().trim().is_empty();
    u.chat_list.invalidate_filter();
    if !searching {
        u.chat_list.queue_resize(); // rows the filter had hidden come back: make sure they are laid out and drawn again
        u.chat_list.queue_draw();
    }
    // pinned chats are in the list only as hidden rows: while searching they show there, and their tiles give way
    let pinned: Vec<bool> = {
        let st = u.st.borrow();
        st.chats
            .iter()
            .map(|c| {
                pin_pos(&st.pins, c).is_some()
                    || c["id"].as_i64().is_some_and(|id| st.off_list.contains(&id))
            })
            .collect()
    };
    for (i, p) in pinned.into_iter().enumerate() {
        if p && let Some(row) = u.chat_list.row_at_index(i as i32) {
            row.set_visible(searching);
        }
    }
    u.tiles
        .set_visible(!searching && u.tiles.first_child().is_some());
    if !searching {
        u.tiles.queue_resize(); // shown again: measure it afresh (GTK keeps the height it was asked for while hidden)
    }
    let any = (0..u.st.borrow().chats.len()).any(|i| {
        u.chat_list
            .row_at_index(i as i32)
            .is_some_and(|r| row_matches(u, &r))
    });
    // a message search can take several seconds: say so instead of "nothing found"
    let waiting = u.search_spin.is_visible();
    u.side_none.set_text(if waiting {
        "Searching…"
    } else {
        "No conversations found"
    });
    u.side_none_spin.set_visible(waiting);
    u.side_none_spin.set_spinning(waiting);
    u.side_none_box.set_visible(searching && !any);
    style_rows(u);
}

/// Words of a search, lower case.
/// Lower case for matching: A-Z and accented letters alike, but only where the lower-case letter takes as many bytes
/// as the original, so byte offsets found in the result are valid in `s`.
fn fold(s: &str) -> String {
    s.chars()
        .map(|c| {
            let mut l = c.to_lowercase();
            match (l.next(), l.next()) {
                (Some(x), None) if x.len_utf8() == c.len_utf8() => x,
                _ => c,
            }
        })
        .collect()
}

fn search_words(q: &str) -> Vec<String> {
    q.split_whitespace().map(str::to_lowercase).collect()
}

/// Mark every occurrence of the words in the label's text: bold dark text on a pink ground, or on a vivid
/// pink one (`strong`) for the match you have just jumped to. Both are dark-on-light, so they read on any bubble.
/// Whole words get rounded pills (consecutive matched words share one) drawn under the label by `pilllabel`; a
/// label that can't sit on that layer gets plain rectangles over just the matched letters.
fn highlight(l: &gtk::Label, words: &[String], strong: bool) {
    let (r, g, b) = if strong {
        (0xffff_u16, 0x7e7e_u16, 0xb9b9_u16)
    } else {
        (0xffff, 0xb8b8, 0xdcdc)
    };
    let rgb = (r as f64 / 65535.0, g as f64 / 65535.0, b as f64 / 65535.0);
    if words.is_empty() {
        l.set_attributes(None);
        crate::pilllabel::set_pills(l, Vec::new(), rgb);
        return;
    }
    let text = l.text().to_string();
    let lower = fold(&text); // same byte offsets as `text`
    let spans = word_spans(&text, &lower, words);
    let pilled = crate::pilllabel::set_pills(l, spans.clone(), rgb);
    let list = gtk::pango::AttrList::new();
    let plain: Vec<(usize, usize)> = if pilled {
        spans
    } else {
        words
            .iter()
            .filter(|w| !w.is_empty())
            .flat_map(|w| {
                lower
                    .match_indices(w.as_str())
                    .map(|(at, _)| (at, at + w.len()))
                    .collect::<Vec<_>>()
            })
            .collect()
    };
    for (s, e) in plain {
        let (start, end) = (s as u32, e as u32);
        if !pilled {
            let mut bg = gtk::pango::AttrColor::new_background(r, g, b);
            bg.set_start_index(start);
            bg.set_end_index(end);
            list.insert(bg);
        }
        let mut fg = gtk::pango::AttrColor::new_foreground(0x2a2a, 0x2424, 0x4242);
        fg.set_start_index(start);
        fg.set_end_index(end);
        list.insert(fg);
        let mut bold = gtk::pango::AttrInt::new_weight(gtk::pango::Weight::Bold);
        bold.set_start_index(start);
        bold.set_end_index(end);
        list.insert(bold);
    }
    l.set_attributes(Some(&list));
}

/// Where the search words are, grown to whole words, with words that follow one another (only blanks between)
/// joined into one span: "hi abas" is one pill, not two.
fn word_spans(text: &str, lower: &str, words: &[String]) -> Vec<(usize, usize)> {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for w in words.iter().filter(|w| !w.is_empty()) {
        for (at, _) in lower.match_indices(w.as_str()) {
            let (mut s, mut e) = (at, at + w.len());
            while let Some(c) = text[..s]
                .chars()
                .next_back()
                .filter(|c| c.is_alphanumeric())
            {
                s -= c.len_utf8();
            }
            while let Some(c) = text[e..].chars().next().filter(|c| c.is_alphanumeric()) {
                e += c.len_utf8();
            }
            spans.push((s, e));
        }
    }
    spans.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::new();
    for (s, e) in spans {
        match out.last_mut() {
            Some(last) if s <= last.1 || text[last.1..s].chars().all(char::is_whitespace) => {
                last.1 = last.1.max(e)
            }
            _ => out.push((s, e)),
        }
    }
    out
}

/// The part of a message around its first match, for a row's preview line.
fn snippet(text: &str, words: &[String]) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let lower = fold(&flat);
    let at = words
        .iter()
        .filter_map(|w| lower.find(w.as_str()))
        .min()
        .unwrap_or(0);
    let start = flat[..at].char_indices().rev().nth(6).map_or(0, |(i, _)| i);
    format!("{}{}", if start > 0 { "…" } else { "" }, &flat[start..])
}

/// While searching: highlight the words in the titles, and show the newest matching message (and how many
/// match) as the preview of a chat with hits. When the search ends, the previews are put back.
fn style_rows(u: &Rc<Ui>) {
    let q = u.search.text().trim().to_string();
    let searching = !q.is_empty();
    if !searching && !u.styled.get() {
        return;
    }
    u.styled.set(searching);
    let words = search_words(&q);
    let parts: Vec<(i64, RowParts)> = u
        .row_parts
        .borrow()
        .iter()
        .map(|(k, v)| (*k, v.clone()))
        .collect();
    let st = u.st.borrow();
    let by_id: HashMap<i64, &Value> = st
        .chats
        .iter()
        .filter_map(|c| Some((c["id"].as_i64()?, c)))
        .collect(); // not a scan per row
    for (id, p) in parts {
        style_row(&st, &words, searching, id, &p, by_id.get(&id).copied());
    }
}

/// One row's name and preview while a search is on (or back to plain when it isn't), and its match count: a pill
/// beside the preview in the wide sidebar, a badge on the avatar's corner in the avatars-only one.
fn style_row(
    st: &State,
    words: &[String],
    searching: bool,
    id: i64,
    p: &RowParts,
    chat: Option<&Value>,
) {
    highlight(&p.name, if searching { words } else { &[] }, false);
    let hits = if searching {
        st.find_hits.get(&id)
    } else {
        None
    };
    let count = if searching {
        st.find_counts.get(&id).copied().or(hits.map(Vec::len))
    } else {
        None
    };
    let text = match hits.and_then(|h| h.first()) {
        Some(m) => snippet(m["text"].as_str().unwrap_or(""), words),
        None => chat.map(|c| row_info(st, c).1).unwrap_or_default(),
    };
    p.prev.set_text(&text);
    highlight(&p.prev, if hits.is_some() { words } else { &[] }, false);
    let n = count.unwrap_or(0);
    p.pill.set_text(&format!("{n} matches"));
    p.pill.set_visible(n > 1);
    p.badge.set_text(&if n > 99 {
        "99+".to_string()
    } else {
        n.to_string()
    });
    p.badge.set_visible(n > 0);
}

/// The search box changed: filter the list by name at once, and (after a pause) look through the messages.
/// Dev aid (BF_PROF): what the sidebar list looks like right now, with the time since the first call.
fn prof_list(u: &Ui, tag: &str) {
    if std::env::var_os("BF_PROF").is_none() {
        return;
    }
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let t = START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs_f32();
    let (mut rows, mut shown, mut mapped) = (0, 0, 0);
    let mut c = u.chat_list.first_child();
    while let Some(w) = c {
        rows += 1;
        shown += i32::from(w.is_child_visible() && w.is_visible());
        mapped += i32::from(w.is_mapped());
        c = w.next_sibling();
    }
    if let Some(r) = u.chat_list.row_at_index(5) {
        eprintln!(
            "prof[{t:.1}s]: {tag}: row 5 is {}x{} px, opacity {:.1}, child_visible {}, visible {}, drawable {}, name label \"{}\"",
            r.width(),
            r.height(),
            r.opacity(),
            r.is_child_visible(),
            r.is_visible(),
            r.is_drawable(),
            u.row_parts
                .borrow()
                .values()
                .next()
                .map(|p| p.name.text().to_string())
                .unwrap_or_default()
        );
    }
    eprintln!(
        "prof[{t:.1}s]: {tag}: list {}x{} px, rows {rows}, visible {shown}, mapped {mapped}, side stack \"{}\", scroller value {:.0}",
        u.chat_list.width(),
        u.chat_list.height(),
        u.side_stack.visible_child_name().unwrap_or_default(),
        side_adjustment(u).map_or(0.0, |a| a.value())
    );
}

fn search_changed(u: &Rc<Ui>) {
    let t0 = std::time::Instant::now();
    search_changed_inner(u);
    if u.search.text().is_empty() && std::env::var_os("BF_PROF").is_some() {
        prof_list(u, "search cleared");
        for secs in [1, 5, 15, 30, 45] {
            let u2 = u.clone();
            glib::timeout_add_local_once(Duration::from_secs(secs), move || {
                prof_list(&u2, &format!("{secs}s after clear"))
            });
        }
    }
    if std::env::var_os("BF_PROF").is_some() {
        eprintln!(
            "prof: search_changed {} ms ({} chats, {} rows)",
            t0.elapsed().as_millis(),
            u.st.borrow().chats.len(),
            u.row_parts.borrow().len()
        );
    }
}

fn search_changed_inner(u: &Rc<Ui>) {
    let q = u.search.text().trim().to_string();
    if !q.is_empty() && !u.search_open.get() {
        search_to(u, true); // the text was set from elsewhere
    }

    let gen_ = u.msearch_gen.get().wrapping_add(1);
    u.msearch_gen.set(gen_);
    if q.chars().count() < 2 {
        u.search_spin.set_spinning(false);
        u.search_spin.set_visible(false);
        let had = !u.st.borrow().find_counts.is_empty();
        u.st.borrow_mut().find_hits.clear();
        u.st.borrow_mut().find_counts.clear();
        if q.is_empty() && u.st.borrow().find_view.is_some() {
            end_find(u);
        }
        if had {
            u.chat_list.invalidate_filter();
        }
        if q.is_empty() {
            exit_search(u);
        }
    } else {
        u.search_spin.set_visible(true);
        u.search_spin.set_spinning(true);
        let u2 = u.clone();
        glib::timeout_add_local_once(Duration::from_millis(300), move || {
            if u2.msearch_gen.get() == gen_ && u2.st.borrow().connected {
                let q = u2.search.text().trim().to_string();
                // chats older than the loaded ones, found by name: a group's name, or a contact whose name matches
                if q.chars().count() >= 3 {
                    let addrs = u2
                        .st
                        .borrow()
                        .contacts
                        .as_ref()
                        .map(|k| k.search(&q).iter().map(|(_, a)| a.to_string()).collect())
                        .unwrap_or_default();
                    u2.send(Cmd::FindNamed {
                        q: q.clone(),
                        addrs,
                        seq: gen_,
                    });
                }
                u2.send(Cmd::Search { q, seq: gen_ });
            }
        });
    }
    apply_search(u);
}

/// The answer to a message search.
fn on_found(u: &Rc<Ui>, seq: u64, hits: Vec<Value>, counts: HashMap<i64, usize>) {
    if seq != u.msearch_gen.get() {
        return; // typed on since
    }
    u.search_spin.set_spinning(false);
    u.search_spin.set_visible(false);
    let mut by_chat: HashMap<i64, Vec<Value>> = HashMap::new();
    for h in hits {
        if let Some(c) = h["chat_id"].as_i64() {
            by_chat.entry(c).or_default().push(h);
        }
    }
    // hits in chats older than the loaded list: fetch those chats so they get rows
    let missing: Vec<i64> = {
        let st = u.st.borrow();
        let known = |id: i64| {
            let id = st.alias.get(&id).copied().unwrap_or(id);
            st.chats.iter().any(|c| c["id"].as_i64() == Some(id))
        };
        by_chat
            .keys()
            .chain(counts.keys())
            .copied()
            .filter(|id| !known(*id))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    };
    {
        let mut st = u.st.borrow_mut();
        st.find_hits = by_chat;
        st.find_counts = counts;
    }
    if !missing.is_empty() {
        u.send(Cmd::FindChats { ids: missing, seq });
    }
    apply_search(u);
    // the chat on show was picked from earlier results: if it has hits for these words, open it on the first one
    // (also after the bar had closed); if not, close the bar. The chat itself stays open either way. One that was
    // open before the search is left as it is.
    let showing = {
        let st = u.st.borrow();
        st.sel.filter(|c| st.search_pick == Some(*c))
    };
    if let Some(chat) = showing {
        if u.st.borrow().find_hits.contains_key(&chat) {
            begin_find(u, chat, 0);
            u.send(Cmd::ChatHits {
                chat,
                q: u.search.text().trim().to_string(),
            });
        } else if u.st.borrow().find_view.is_some() {
            end_find(u);
        }
    }
}

/// The search box was emptied: the chat on show stays (picked from the results or not, and in the list however old);
/// the chats the search added to the list go.
fn exit_search(u: &Rc<Ui>) {
    u.st.borrow_mut().search_pick = None;
    let added = std::mem::take(&mut u.st.borrow_mut().search_added);
    let (removed, hide) = {
        let mut st = u.st.borrow_mut();
        let (sel, before) = (st.sel, st.chats.len());
        st.chats.retain(|c| {
            c["id"]
                .as_i64()
                .is_none_or(|id| !added.contains(&id) || Some(id) == sel)
        });
        let hide = sel.filter(|id| added.contains(id));
        if let Some(id) = hide {
            st.off_list.insert(id); // like Messages: you stay in it, and it shows in the list once scrolling loads it
        }
        (st.chats.len() != before, hide)
    };
    // rebuilding every row is the slow part: only when the search had put chats in the list
    if removed {
        let t = std::time::Instant::now();
        render_chats(u);
        if std::env::var_os("BF_PROF").is_some() {
            eprintln!(
                "prof: exit_search rebuilt the list in {} ms",
                t.elapsed().as_millis()
            );
        }
    } else if let Some(i) = hide.and_then(|id| {
        u.st.borrow()
            .chats
            .iter()
            .position(|c| c["id"].as_i64() == Some(id))
    }) && let Some(row) = u.chat_list.row_at_index(i as i32)
    {
        row.set_visible(false);
    }
    if let Some(id) = u.st.borrow().sel {
        select_in_list(u, id);
    }
}

/// Open the chat on its `idx`-th hit (0 = newest): a window of messages around it, scrolled to it.
fn begin_find(u: &Rc<Ui>, chat: i64, idx: usize) {
    let (guid, row, loaded) = {
        let st = u.st.borrow();
        let Some(hit) = st.find_hits.get(&chat).and_then(|h| h.get(idx)) else {
            return;
        };
        let guid = hit["guid"].as_str().unwrap_or("").to_string();
        let loaded = st
            .hist
            .get(&chat)
            .is_some_and(|h| h.iter().any(|m| m["guid"].as_str() == Some(&guid)));
        (guid, hit["id"].as_i64().unwrap_or(0), loaded)
    };
    let more =
        u.st.borrow()
            .find_view
            .as_ref()
            .filter(|f| f.chat == chat)
            .is_some_and(|f| f.more);
    u.st.borrow_mut().find_view = Some(FindView {
        chat,
        idx,
        more: if loaded { more } else { true },
        loading: !loaded,
        goto: Some(guid),
    });
    u.stick_bottom.set(false);
    if loaded {
        render_conv(u);
    } else {
        u.send(Cmd::Window { id: chat, row });
    }
    update_find_bar(u);
}

/// The ‹ › buttons: to the next older (+1) or newer (-1) hit of this chat.
fn step_find(u: &Rc<Ui>, by: i64) {
    let (chat, idx, n) = {
        let st = u.st.borrow();
        let Some(f) = &st.find_view else { return };
        (f.chat, f.idx, st.find_hits.get(&f.chat).map_or(0, Vec::len))
    };
    let to = idx as i64 + by;
    if to >= 0 && (to as usize) < n {
        begin_find(u, chat, to as usize);
    }
}

/// Leave the search window: the chat shows its newest messages again.
fn end_find(u: &Rc<Ui>) {
    let chat = u.st.borrow_mut().find_view.take().map(|f| f.chat);
    u.find_bar.set_visible(false);
    if let Some(chat) = chat {
        u.stick_bottom.set(true);
        u.st.borrow_mut().no_more.remove(&chat);
        request_history(u, chat);
        render_conv(u);
    }
}

fn update_find_bar(u: &Ui) {
    let st = u.st.borrow();
    let Some(f) = st.find_view.as_ref().filter(|f| Some(f.chat) == st.sel) else {
        u.find_bar.set_visible(false);
        return;
    };
    let n = st.find_hits.get(&f.chat).map_or(0, Vec::len);
    u.find_label.set_text(&format!(
        "“{}” · {} of {}{}",
        u.search.text().trim(),
        f.idx + 1,
        n,
        if f.loading { " · loading…" } else { "" }
    ));
    u.find_older.set_sensitive(f.idx + 1 < n);
    u.find_newer.set_sensitive(f.idx > 0);
    u.find_bar.set_visible(true);
}

/// Scrolled to the bottom of a search window: fetch the next page of newer messages.
fn load_newer(u: &Rc<Ui>) {
    let req = {
        let mut st = u.st.borrow_mut();
        let Some(chat) = st
            .find_view
            .as_ref()
            .filter(|f| f.more && !f.loading)
            .map(|f| f.chat)
        else {
            return;
        };
        if !st.connected {
            return;
        }
        let Some((last, iso)) = st
            .hist
            .get(&chat)
            .and_then(|h| h.last())
            .and_then(|m| Some((m["id"].as_i64()?, m["created_at"].as_str()?.to_string())))
        else {
            return;
        };
        if let Some(f) = st.find_view.as_mut() {
            f.loading = true;
        }
        (chat, last, iso)
    };
    u.send(Cmd::Newer {
        id: req.0,
        iso: req.2,
    });
}

/// After a redraw: scroll to the message a search hit points at (its words are drawn in the strong colour).
fn show_goto(u: &Rc<Ui>) {
    let guid =
        u.st.borrow_mut()
            .find_view
            .as_mut()
            .and_then(|f| f.goto.take());
    let Some(guid) = guid else { return };
    // the target's widget is looked up again every frame: the chat is redrawn by other events (previews, file
    // info, contacts) while we wait, and the widgets we saw first are gone by then
    let target = move |u: &Ui| {
        let st = u.st.borrow();
        let i = st
            .sel
            .and_then(|c| st.hist.get(&c))
            .and_then(|h| h.iter().position(|m| m["guid"].as_str() == Some(&guid)))?;
        u.msg_widgets.borrow().get(i).cloned()
    };
    // Keep the message a third of the way down for a moment: pictures and link cards below it get their size
    // late, and the view is anchored to the bottom, so without this the target would drift out of sight.
    let my = u.goto_gen.get().wrapping_add(1);
    u.goto_gen.set(my);
    let (u2, started) = (u.clone(), std::time::Instant::now());
    u.msgs.add_tick_callback(move |_, _| {
        if u2.goto_gen.get() != my || started.elapsed() > Duration::from_millis(1800) {
            return glib::ControlFlow::Break;
        }
        let adj = u2.scroller.vadjustment();
        if let Some(ws) = target(&u2)
            && let Some(w) = ws.first()
            && w.height() > 0
            && adj.upper() > adj.page_size()
            && let Some(y) = match_y(
                &ws,
                &search_words(u2.search.text().trim()),
                u2.msgs.upcast_ref(),
            )
            .or_else(|| {
                w.compute_point(&u2.msgs, &gtk::graphene::Point::new(0.0, 0.0))
                    .map(|p| p.y())
            })
        {
            let want = (y as f64 - adj.page_size() / 3.0).max(0.0);
            if (adj.value() - want).abs() > 1.0 {
                u2.stick_bottom.set(false);
                adj.set_value(want);
            }
        }
        glib::ControlFlow::Continue
    });
}

/// imsg's chat list has no offset, so older chats mean asking for a longer list; the answer replaces the list.
fn load_more_chats(u: &Rc<Ui>) {
    // while searching the list is a short, filtered one that is always "at its end": asking for more would chain
    // (each answer rebuilds every row) and the results come from the search anyway
    if !u.search.text().trim().is_empty() {
        return;
    }
    let n = {
        let mut st = u.st.borrow_mut();
        if !st.chats_loaded || !st.more_chats || st.more_loading || !st.connected {
            return;
        }
        st.more_loading = true;
        st.chat_limit += CHATS_PAGE;
        st.chat_limit
    };
    u.side_more.set_visible(true);
    u.send(Cmd::Chats(n));
    // no answer (an error): don't stay stuck on it
    let u2 = u.clone();
    glib::timeout_add_local_once(Duration::from_secs(30), move || {
        let mut st = u2.st.borrow_mut();
        if st.more_loading {
            st.more_loading = false;
            drop(st);
            u2.side_more.set_visible(false);
        }
    });
}

/// A chat's stable identity for pinning (its row id changes between runs of the Mac's database).
fn pin_key(c: &Value) -> String {
    c["identifier"]
        .as_str()
        .or(c["guid"].as_str())
        .map(String::from)
        .unwrap_or_else(|| {
            c["participants"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p.as_str())
                .collect::<Vec<_>>()
                .join(",")
        })
}

/// The keys a chat can be pinned or muted under: its own, or one for each chat behind a merged conversation.
fn keys_of(c: &Value) -> Vec<String> {
    match c["merged_keys"].as_array() {
        Some(a) => a
            .iter()
            .filter_map(|k| k.as_str().map(String::from))
            .collect(),
        None => vec![pin_key(c)],
    }
}

/// Where this chat is in the pin order (None: not pinned).
fn pin_pos(pins: &[String], c: &Value) -> Option<usize> {
    keys_of(c)
        .iter()
        .filter_map(|k| pins.iter().position(|p| p == k))
        .min()
}

fn is_muted(muted: &[String], c: &Value) -> bool {
    keys_of(c).iter().any(|k| muted.contains(k))
}

/// Right-click menu on a row or tile: Pin / Unpin.
fn pin_menu(u: &Rc<Ui>, w: &gtk::Widget, keys: &[String], pinned: bool, muted: bool) {
    let g = gtk::GestureClick::new();
    g.set_button(3);
    // weak: a strong reference to the widget from its own gesture kept every rebuilt row alive for good
    let (u, w2, key) = (u.clone(), w.downgrade(), keys.to_vec());
    g.connect_pressed(move |g, _, x, y| {
        let Some(w2) = w2.upgrade() else { return };
        g.set_state(gtk::EventSequenceState::Claimed);
        let b = gtk::Button::with_label(if pinned { "Unpin" } else { "Pin to top" });
        b.add_css_class("flat");
        let mb = gtk::Button::with_label(if muted { "Unmute" } else { "Mute" });
        mb.add_css_class("flat");
        let menu = gtk::Box::new(gtk::Orientation::Vertical, 0);
        menu.append(&b);
        menu.append(&mb);
        let pop = gtk::Popover::new();
        pop.set_child(Some(&menu));
        pop.set_has_arrow(false);
        pop.set_parent(&w2);
        pop.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        let (u2, key2, p2) = (u.clone(), key.clone(), pop.clone());
        b.connect_clicked(move |_| {
            p2.popdown();
            toggle_pin(&u2, &key2);
        });
        let (u3, key3, p3) = (u.clone(), key.clone(), pop.clone());
        mb.connect_clicked(move |_| {
            p3.popdown();
            toggle_mute(&u3, &key3);
        });
        unparent_on_close(&pop);
        pop.popup();
    });
    w.add_controller(g);
}

/// Mute or unmute a chat: `keys` are the identifiers it goes by (several for a merged conversation).
fn toggle_mute(u: &Rc<Ui>, keys: &[String]) {
    {
        let mut st = u.st.borrow_mut();
        if keys.iter().any(|k| st.muted.contains(k)) {
            st.muted.retain(|p| !keys.contains(p));
        } else if let Some(k) = keys.first() {
            st.muted.push(k.clone());
        }
        crate::write_state(
            &u.store.muted_file,
            st.muted
                .iter()
                .map(|p| format!("{p}\n"))
                .collect::<String>(),
        );
    }
    render_chats(u);
}

/// Pin or unpin a chat: `keys` are the identifiers it goes by (several for a merged conversation).
fn toggle_pin(u: &Rc<Ui>, keys: &[String]) {
    {
        let mut st = u.st.borrow_mut();
        if keys.iter().any(|k| st.pins.contains(k)) {
            st.pins.retain(|p| !keys.contains(p));
        } else if let Some(k) = keys.first() {
            st.pins.push(k.clone());
        }
        crate::write_state(
            &u.store.pins_file,
            st.pins.iter().map(|p| format!("{p}\n")).collect::<String>(),
        );
    }
    render_chats(u);
}

/// Room on each side of a list avatar, so it sits centred in the avatars-only sidebar (and the texts start after it).
const AV_PAD: i32 = 8;

/// Width of a row's avatar column: just the avatar and its room, or (avatars-only sidebar, margins 4 + padding 4) the
/// whole row, so the names under the avatars get all the width there is. The avatar's x is 24 either way.
fn stack_w(collapsed: bool) -> i32 {
    if collapsed {
        COLLAPSED_W - 20 - 16
    } else {
        AV_PAD * 2 + AVATAR
    }
}

/// The avatar goes first in a row's stack (replacing the one there).
fn put_avatar(stack: &gtk::Box, av: &gtk::Widget) {
    if let Some(old) = stack
        .first_child()
        .filter(|c| !c.has_css_class("tile-name"))
    {
        stack.remove(&old);
    }
    av.set_halign(gtk::Align::Center); // centred in the stack, whose width is `stack_w`
    av.set_valign(gtk::Align::Center);
    stack.prepend(av);
}

/// What shows under an avatar: a person is just their first name (the list has room for the whole name); a group, a
/// number or an email stays whole.
fn short_title(chat: &Value, title: &str) -> String {
    if chat["is_group"].as_bool() == Some(true)
        || title.chars().any(|c| c.is_ascii_digit() || c == '@')
    {
        title.to_string()
    } else {
        first_name(title)
    }
}

/// The pinned chats as avatar tiles above the list (names hide when the sidebar is avatars-only).
fn render_tiles(u: &Rc<Ui>, pinned: Vec<PinnedTile>, sel: Option<i64>) {
    u.tiles.remove_all();
    u.tiles
        .set_visible(!pinned.is_empty() && u.search.text().trim().is_empty());
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
        // under a pinned avatar a person is just their first name (the list has room for the whole name); a group, a
        // number or an email stays whole
        let shown = short_title(&chat, &title);
        let name = gtk::Label::new(Some(&shown));
        name.add_css_class("tile-name");
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        name.set_width_chars(1);
        name.set_max_width_chars(9);
        let tile = gtk::Box::new(gtk::Orientation::Vertical, 4);
        tile.add_css_class("tile");
        tile.set_widget_name(&id.to_string());
        tile.set_hexpand(true);
        tile.set_tooltip_text(Some(&title));
        tile.append(&holder);
        tile.append(&name);
        let keys = keys_of(&chat);
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
        let tile_muted = is_muted(&u.st.borrow().muted, &chat);
        pin_menu(u, tile.upcast_ref(), &keys, true, tile_muted);
        u.tiles.append(&tile);
    }
}

fn side_adjustment(u: &Ui) -> Option<gtk::Adjustment> {
    u.chat_list
        .ancestor(gtk::ScrolledWindow::static_type())
        .and_downcast::<gtk::ScrolledWindow>()
        .map(|sw| sw.vadjustment())
}

// ---------- conversation ----------

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

/// Draw message `i` of the open chat again where it stands (its link preview arrived, say) instead of rebuilding
/// the whole conversation: that took ~150 ms to lay out again for 150 messages. False when it can't (nothing
/// drawn, the view changed since, or the message has pictures, which the viewer's list is kept in step with).
fn redraw_message(u: &Rc<Ui>, i: usize) -> bool {
    let Some(ctx) = u.conv_ctx.borrow().clone() else {
        return false;
    };
    let mut parts = u.msg_widgets.borrow_mut();
    let Some(old) = parts.get(i).filter(|o| !o.is_empty()) else {
        return false;
    };
    let first = old[0].clone();
    // (a link message's own attachment is just its preview's payload: no picture for the viewer's list)
    // (a link to a picture may be drawn as that picture, which is in the viewer's list too)
    let text = clean_text(&ctx.msgs[i]);
    let known_picture = text.split_whitespace().last().is_some_and(|url| {
        let st = u.st.borrow();
        let known = st.web_links.get(url).and_then(Option::as_ref);
        known.and_then(direct_image).is_some()
    });
    let pictures = known_picture
        || crate::webpreview::is_image_url(text.trim_end())
        || preview_row(&ctx.msgs[i]).is_none()
            && ctx.msgs[i]["attachments"]
                .as_array()
                .is_some_and(|a| !a.is_empty());
    if first.parent().as_ref() != Some(u.msgs.upcast_ref()) || pictures {
        return false;
    }
    let old = std::mem::take(&mut parts[i]);
    let mut prev = first.prev_sibling();
    let fresh = conv_widgets(u, &ctx, i);
    for w in &fresh {
        u.msgs.insert_child_after(w, prev.as_ref());
        prev = Some(w.clone());
    }
    for w in &old {
        u.msgs.remove(w);
    }
    parts[i] = fresh;
    true
}

/// The widgets of message `i` (time separator, sender name, one line per part), top to bottom.
/// How far a reaction pill rises above its bubble's top edge.
const REACT_UP: i32 = 22; // the pill is 28 px tall: a little of it lies over the bubble's corner

fn conv_widgets(u: &Rc<Ui>, c: &ConvCtx, i: usize) -> Vec<gtk::Widget> {
    let (msgs, times, names, group) = (&c.msgs, &c.times, &c.names, c.group);
    let mut out: Vec<gtk::Widget> = Vec::new();
    let m = &msgs[i];
    if m["is_reaction"].as_bool() == Some(true) {
        return out;
    }
    // a group event (joined, left, renamed) is a line of its own, not a message; one we can't word is left out
    let event = group_event(m, names);
    if m["item_type"].as_i64().unwrap_or(0) != 0
        && event.is_none()
        && clean_text(m).is_empty()
        && m["attachments"].as_array().is_none_or(|a| a.is_empty())
    {
        return out;
    }
    let who = |m: &Value| {
        (
            m["is_from_me"].as_bool() == Some(true),
            m["sender"].as_str().unwrap_or("").to_string(),
        )
    };
    let me = m["is_from_me"].as_bool() == Some(true);
    let gap = |a: usize, b: usize| match (times[a], times[b]) {
        (Some(x), Some(y)) => (y - x).abs(),
        _ => 0,
    };
    let sep = i == 0 || gap(i - 1, i) > 60 * 60 || day(times[i - 1]) != day(times[i]);
    // the service (iMessage, RCS, Text Message) is named at the start of the chat and wherever it changes
    let service_of = |m: &Value| m["service"].as_str().unwrap_or(&c.chat_svc).to_string();
    let svc = service_of(m);
    let svc_changed = i == 0 || service_of(&msgs[i - 1]) != svc;
    let svc_name = match svc.as_str() {
        "iMessage" => "iMessage",
        "RCS" => "RCS",
        "SMS" => "Text Message",
        other => other,
    };
    let dt = m["created_at"].as_str().and_then(local);
    let show_time = sep && dt.is_some();
    if svc_changed || show_time {
        let l = gtk::Label::new(None);
        if show_time {
            track_time(
                u,
                &l,
                dt.as_ref().map(|d| d.to_unix()),
                TimeKind::Sep,
                svc_changed.then(|| svc_name.to_string()),
            );
        } else {
            l.set_markup(&format!("<b>{}</b>", glib::markup_escape_text(svc_name)));
        }
        l.set_justify(gtk::Justification::Center);
        l.add_css_class("time-sep");
        out.push(l.upcast());
    }
    if let Some(text) = event {
        let l = gtk::Label::new(Some(&text));
        l.add_css_class("group-event");
        l.set_justify(gtk::Justification::Center);
        l.set_wrap(true);
        l.set_halign(gtk::Align::Center);
        out.push(l.upcast());
        return out;
    }
    // a "run" = same sender, close together: shares one name label and one avatar
    let run_prev = !sep && i > 0 && who(&msgs[i - 1]) == who(m) && gap(i - 1, i) < 300;
    let run_next = i + 1 < msgs.len()
        && who(&msgs[i + 1]) == who(m)
        && gap(i, i + 1) < 300
        && day(times[i]) == day(times[i + 1]);
    // my messages still on their way continue my last run: no tail on the one above them
    let run_next = run_next || (i + 1 == msgs.len() && me && c.pending_tail);
    // big emoji have no bubble and don't count: the bubble before them ends with a tail as well
    let tail = !run_next || msgs.get(i + 1).is_some_and(lone_emoji);
    let mut name_w = 0; // width of the name above the message (0: none), which a reaction pill must clear
    if group && !me && !run_prev {
        let n = gtk::Label::new(Some(&sender_name(m, names)));
        n.add_css_class("sender-name");
        n.set_xalign(0.0);
        name_w = n.preferred_size().1.width();
        out.push(n.upcast());
    }
    // hovering a message shows its full date and time (the separators only show them for a group of messages)
    let when = dt
        .and_then(|d| d.format("%A, %B %-d, %Y at %H:%M").ok())
        .map(|g| g.to_string());
    let failed_msg = me && m["error"].as_i64().unwrap_or(0) != 0;
    let resending = failed_msg
        && m["id"]
            .as_i64()
            .is_some_and(|id| u.st.borrow().resending.contains(&id));
    let (parts, saves) = message_parts(u, m, me, svc != "iMessage", tail);
    // a chat open on a search hit: mark the words wherever they show (text, link and file cards)
    let (words, current) = find_words(u, m);
    if !words.is_empty() {
        for p in &parts {
            mark_labels(p, &words, current);
        }
    }
    // a reply: the message it answers in a small outlined bubble above the first reply of a run (like Messages),
    // then a bar down to the replies; further replies to the same message just continue the bar
    if let Some(g) = m["reply_to_guid"].as_str().filter(|g| !g.is_empty()) {
        let prev_same = msgs[..i]
            .iter()
            .rev()
            .find(|x| x["is_reaction"].as_bool() != Some(true))
            .is_some_and(|x| x["reply_to_guid"].as_str() == Some(g));
        let side = if me {
            gtk::Align::End
        } else {
            gtk::Align::Start
        };
        let indent = |line: &gtk::Box| {
            if group && !me {
                let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                spacer.add_css_class("avatar-spacer");
                line.append(&spacer);
            }
        };
        if !prev_same {
            let said = match msgs.iter().find(|x| x["guid"].as_str() == Some(g)) {
                Some(o) => {
                    let who = if o["is_from_me"].as_bool() == Some(true) {
                        None
                    } else {
                        Some(sender_name(o, names))
                    };
                    let t = clip_words(&summary(o), 160);
                    match who.filter(|_| group) {
                        Some(w) => format!("{w}: {t}"),
                        None => t,
                    }
                }
                None => "Earlier message".to_string(),
            };
            let t = gtk::Label::new(Some(&said));
            t.add_css_class("reply-text");
            t.set_wrap(true);
            t.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            t.set_xalign(0.0);
            t.set_max_width_chars(34);
            let q = gtk::Box::new(gtk::Orientation::Vertical, 0);
            q.add_css_class("reply-bubble");
            q.append(&t);
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            line.add_css_class("msg-line");
            line.add_css_class("tight");
            line.set_halign(side);
            indent(&line);
            line.append(&q);
            out.push(line.upcast());
        }
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        bar.add_css_class("thread-bar");
        bar.set_halign(side);
        bar.set_size_request(3, 16);
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        line.add_css_class("tight");
        line.set_halign(side);
        indent(&line);
        line.append(&bar);
        out.push(line.upcast());
    }
    let n = parts.len();
    for (k, part) in parts.into_iter().enumerate() {
        let last = k + 1 == n;
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        line.add_css_class("msg-line");
        if !last || run_next {
            line.add_css_class("tight"); // same sender close together: a smaller gap
        }
        // the other side speaks (no name label to space it): more room than within a run
        let turn = k == 0 && !run_prev && !sep && i > 0 && name_w == 0;
        if turn {
            line.add_css_class("turn");
        }
        line.set_halign(if me {
            gtk::Align::End
        } else {
            gtk::Align::Start
        });
        line.set_tooltip_text(when.as_deref());
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
        let save = saves
            .iter()
            .find(|(i, _)| *i == k)
            .map(|(_, s)| save_button(u, s.clone()));
        if me && let Some(b) = &save {
            line.append(b); // beside the bubble, on the side away from the edge
        }
        if resending {
            // being sent again: a ghost with a spinner, in place
            let spin = gtk::Spinner::new();
            spin.start();
            spin.set_valign(gtk::Align::Center);
            spin.set_margin_end(8);
            spin.set_tooltip_text(Some("Sending again…"));
            line.append(&spin);
            part.add_css_class("pending");
        }
        match reactions_pill(m).filter(|_| k == 0) {
            Some(pill) => {
                // the pill sits over the bubble's top corner (the side away from its tail), half of it above the
                // bubble. Under a name it rises beside the name (the bubble is made wide enough for both, so
                // the name stays as close as ever); otherwise the message gets that much room above it.
                let ov = gtk::Overlay::new();
                ov.set_halign(part.halign());
                if name_w > 0 && !me {
                    part.set_size_request(name_w + pill.preferred_size().1.width() + 24, -1);
                } else {
                    // room for the pill above the bubble: the line's own gap and the one above (tight: only 2 px) are already there
                    line.set_margin_top(if turn { REACT_UP - 8 } else { REACT_UP - 2 });
                }
                ov.set_child(Some(&part));
                pill.add_css_class("rise"); // moved up by REACT_UP in CSS (a negative margin stretched it)
                pill.set_valign(gtk::Align::Start);
                pill.set_halign(if me {
                    gtk::Align::Start
                } else {
                    gtk::Align::End
                });
                ov.add_overlay(&pill);
                line.append(&ov);
            }
            None => line.append(&part),
        }
        if !me && let Some(b) = &save {
            line.append(b);
        }
        // a failed message moves in from the edge for a red "!" that offers to send it again
        if last && failed_msg && !resending {
            line.append(&failed_button(u, m));
        }
        out.push(line.upcast());
    }
    // the message that was answered: how many replies it has (among the loaded ones); a click goes to the first
    if let Some(g) = m["guid"].as_str() {
        let first = msgs
            .iter()
            .position(|x| x["reply_to_guid"].as_str() == Some(g));
        let n = msgs
            .iter()
            .filter(|x| x["reply_to_guid"].as_str() == Some(g))
            .count();
        if let Some(first) = first {
            let l = gtk::Label::new(Some(&format!(
                "{n} {}",
                if n == 1 { "Reply" } else { "Replies" }
            )));
            l.add_css_class("reply-count");
            l.set_halign(if me {
                gtk::Align::End
            } else {
                gtk::Align::Start
            });
            l.set_cursor_from_name(Some("pointer"));
            let click = gtk::GestureClick::new();
            let u2 = u.clone();
            click.connect_released(move |_, _, _, _| {
                let w = u2.msg_widgets.borrow().get(first).and_then(|ws| {
                    ws.iter()
                        .find(|w| w.has_css_class("msg-line") || w.has_css_class("tight"))
                        .or(ws.first())
                        .cloned()
                });
                let adj = u2.scroller.vadjustment();
                if let Some(y) = w
                    .and_then(|w| w.compute_point(&u2.msgs, &gtk::graphene::Point::new(0.0, 0.0)))
                    .map(|p| p.y())
                {
                    u2.stick_bottom.set(false);
                    adj.set_value((y as f64 - adj.page_size() / 3.0).max(0.0));
                }
            });
            l.add_controller(click);
            out.push(l.upcast());
        }
    }
    // a received message the sender dictated through Siri says so, small, under the message (Messages shows it
    // only on received ones)
    if !me && m["siri"].as_bool() == Some(true) {
        let l = gtk::Label::new(Some("Sent with Siri"));
        l.add_css_class("receipt");
        l.set_halign(gtk::Align::Start);
        l.set_margin_start(if group { 62 } else { 14 });
        out.push(l.upcast());
    }
    // under a message that failed (any of mine), or my newest one: not delivered / delivered / read
    let failed = failed_msg && !resending; // (while it is sent again: no "Not delivered" and no "!")
    let receipt = if me && !failed_msg {
        m["chat_id"].as_i64().and_then(|chat| {
            u.st.borrow()
                .receipts
                .get(&chat)
                .filter(|r| Some(r.msg) == m["id"].as_i64())
                .cloned()
        })
    } else {
        None
    };
    let read_at = receipt.as_ref().and_then(|r| r.read_at);
    let line = if failed {
        Some(("Not delivered".to_string(), None))
    } else if read_at.is_some() {
        Some((String::new(), None)) // written by `track_time` below
    } else {
        receipt.as_ref().and_then(receipt_text)
    };
    if let Some((text, tip)) = line {
        let l = gtk::Label::new(Some(&text));
        l.add_css_class("receipt");
        if failed {
            l.add_css_class("failed");
        }
        if read_at.is_some() {
            track_time(u, &l, read_at, TimeKind::Read, None);
        }
        l.set_tooltip_text(tip);
        l.set_halign(gtk::Align::End);
        out.push(l.upcast());
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
        let Some(chat) = st
            .chats
            .iter()
            .find(|c| c["id"].as_i64() == Some(id))
            .cloned()
        else {
            return;
        };
        let preview_svc = st
            .preview
            .get(&id)
            .and_then(|m| m["service"].as_str())
            .map(String::from);
        (
            chat,
            st.hist.get(&id).cloned().unwrap_or_default(),
            st.names.clone(),
            st.older_loading,
            preview_svc,
        )
    };
    u.stack.set_visible_child_name("conv");
    let title = chat_title(&chat, &names);
    let chat_svc = chat["service"].as_str().unwrap_or("iMessage").to_string();
    // chat.db's chat service can be stale (see mac::handle Send); the latest message tells what's in use
    let now_svc = msgs
        .iter()
        .rev()
        .filter(|m| m["error"].as_i64().unwrap_or(0) == 0) // a failed send says nothing about what works now
        .find_map(|m| m["service"].as_str().map(String::from))
        .or(preview_svc)
        .unwrap_or_else(|| chat_svc.clone());
    let sms = now_svc != "iMessage";
    u.compose_hint.set_text(&if sms {
        format!("Text Message · {now_svc}")
    } else {
        "iMessage".into()
    });
    let group = chat["is_group"].as_bool() == Some(true);
    let people_n = chat["participants"].as_array().map_or(0, Vec::len);
    let big = group && people_n > BIG_GROUP;
    // a big group: its name (or "Chat with N People"), not every name; the avatar opens the list
    let named = chat["display_name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);
    let head = match (&named, big) {
        (Some(n), _) => n.clone(),
        (None, true) => big_group_title(&chat, &names),
        (None, false) => title.clone(),
    };
    u.head_title.set_text(&head);
    // only the kind of service under the name: the people and numbers are in the title, the card and the info panel
    let sub = match now_svc.as_str() {
        "SMS" | "RCS" => format!("Text Message \u{b7} {now_svc}"),
        other => other.to_string(),
    };
    u.head_sub.set_text(&sub);
    set_head_avatar(u, &chat, &title);
    // dev aid BF_PEOPLE=<ms>: open the people list of a big group, click the first person 0.6 s later and report
    if big
        && let Some(ms) = std::env::var("BF_PEOPLE")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
    {
        static ONCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !ONCE.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let (u2, chat2) = (u.clone(), chat.clone());
            glib::timeout_add_local_once(Duration::from_millis(ms), move || {
                let members: Vec<String> = chat2["participants"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|p| p.as_str().map(String::from))
                    .collect();
                let w = u2
                    .head_avatar
                    .first_child()
                    .and_then(|c| c.first_child())
                    .unwrap_or_else(|| u2.head_avatar.clone().upcast());
                show_people(&u2, &w, &members);
            });
        }
    }
    for c in ["sms", "imessage"] {
        u.send_btn.remove_css_class(c);
    }
    u.send_btn
        .add_css_class(if sms { "sms" } else { "imessage" });

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
        st.pending
            .iter()
            .filter(|p| Some(p.chat_id) == st.sel && !matches!(p.item, PendingItem::Seed(..)))
            .cloned()
            .collect()
    };
    let times: Vec<Option<i64>> = msgs
        .iter()
        .map(|m| {
            m["created_at"]
                .as_str()
                .and_then(local)
                .map(|d| d.to_unix())
        })
        .collect();
    let ctx = Rc::new(ConvCtx {
        msgs,
        times,
        names,
        group,
        chat_svc,
        pending_tail: pending
            .first()
            .is_some_and(|p| !matches!(&p.item, PendingItem::Text(t) if is_jumbo(t))),
    });
    // All at once: with the pictures decoded on another thread this takes ~45 ms for 150 messages. Building the
    // newest part first and the rest afterwards was tried; both parts showed up together anyway.
    let mut drawn = Vec::with_capacity(ctx.msgs.len());
    for i in 0..ctx.msgs.len() {
        let ws = conv_widgets(u, &ctx, i);
        for w in &ws {
            u.msgs.append(w);
        }
        drawn.push(ws);
    }
    *u.msg_widgets.borrow_mut() = drawn;
    *u.conv_ctx.borrow_mut() = Some(ctx.clone());
    append_pending(u, &pending, sms);
    let photos = u.st.borrow().photos.clone(); // the viewer's hooks borrow State
    if !(u.st.borrow().viewer_panel && u.viewer.is_open()) {
        u.st.borrow_mut().viewer_panel = false;
        u.viewer.refresh(photos);
    }
    update_find_bar(u);
    show_goto(u);
    mark_seen(u);
}

/// The open chat's newest message has been seen (window focused): remember it, clear its dot.
fn mark_seen(u: &Rc<Ui>) {
    if !u.win.is_active() {
        return;
    }
    let (changed, id) = {
        let mut st = u.st.borrow_mut();
        let Some(id) = st.sel else { return };
        let Some(last) = st
            .hist
            .get(&id)
            .and_then(|h| h.iter().filter_map(|m| m["id"].as_i64()).max())
        else {
            return;
        };
        let old = st.seen.insert(id, last);
        (old.is_none_or(|o| o < last), id)
    };
    if changed {
        let st = u.st.borrow();
        let text: String = st.seen.iter().map(|(c, m)| format!("{c}={m}\n")).collect();
        crate::write_state(&u.store.seen_file, text);
        drop(st);
        update_row(u, id); // its dot only: a rebuild of the whole list right after drawing the chat was a second stall
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
        let tail = pending
            .get(i + 1)
            .is_none_or(|n| matches!(&n.item, PendingItem::Text(t) if is_jumbo(t)));
        line.append(&pending_bubble(p, sms, tail));
        u.msgs.append(&line);
        // a send that's taking long says so under its last bubble
        if pending.get(i + 1).is_none_or(|n| n.token != p.token)
            && u.st.borrow().slow.contains(&p.token)
        {
            let note = gtk::Label::new(Some("Still sending…"));
            note.add_css_class("receipt"); // looks like "Read" and "Not delivered"
            note.set_halign(gtk::Align::End);
            u.msgs.append(&note);
        }
    }
}

/// In the new-message view: the first message to a new address, shown as a ghost until its chat
/// appears (pending items with chat id 0); with none, the people suggestions.
fn show_new_ghosts(u: &Ui) {
    if !u.new_mode.get() {
        return;
    }
    let ghosts: Vec<Pending> =
        u.st.borrow()
            .pending
            .iter()
            .filter(|p| p.chat_id == 0)
            .cloned()
            .collect();
    while let Some(c) = u.msgs.first_child() {
        u.msgs.remove(&c);
    }
    append_pending(u, &ghosts, false);
    u.scroller.set_visible(!ghosts.is_empty());
    u.suggest_scroll.set_visible(ghosts.is_empty());
}

/// A message on its way: faded, drawn like the real one will be (blue or green, tail on the last).
fn pending_bubble(p: &Pending, sms: bool, last: bool) -> gtk::Widget {
    let look = bubble::Look {
        me: true,
        sms,
        tail: last,
    };
    // shortcut: decoded here on the GTK thread, once per redraw; move it to `bg` with a texture kept in the pending item (docs/ponytail-audit-2026-11-22-3.md, finding 1)
    // decoded once, and only when it's a picture we can read (anything else shows as a paperclip line)
    let photo = match &p.item {
        PendingItem::File(f) if is_image("", &f.to_string_lossy()) => {
            gdk::Texture::from_filename(f).ok()
        }
        _ => None,
    };
    let w: gtk::Widget = match (&p.item, photo) {
        (PendingItem::Text(t), _) if is_jumbo(t) => jumbo(t, true).upcast(),
        (_, Some(t)) => {
            let b = bubble::Bubble::photo(look, 160, 120);
            show_texture(&b, &t);
            b.upcast()
        }
        (item, None) => {
            let b = bubble::Bubble::new(look);
            let text = match item {
                PendingItem::Seed(_, n) => format!("📎 {n}"),
                PendingItem::Text(t) => t.clone(),
                PendingItem::File(f) => format!(
                    "📎 {}",
                    f.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                ),
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
            seen.insert(name.split('.').next().unwrap_or(name).to_string())
                .then(|| path.to_string())
        })
        .collect()
}

/// One message as display rows: its photos (as cards, outside any bubble), then a bubble with the
/// text and any other attachments. Only the bubble of the last message in a run gets a tail.
/// `end_of_run`: this message is the sender's last in the run, so its last item (bubble or photo)
/// carries the tail. Nothing else does.
fn message_parts(
    u: &Rc<Ui>,
    m: &Value,
    me: bool,
    sms: bool,
    end_of_run: bool,
) -> (Vec<gtk::Widget>, Vec<(usize, Vec<Save>)>) {
    let look = bubble::Look {
        me,
        sms,
        tail: end_of_run,
    };
    if m["unsent_at"].as_str().is_some() || m["type"].as_str() == Some("unsent") {
        let b = bubble::Bubble::new(look);
        let l = gtk::Label::new(Some("Message unsent"));
        l.add_css_class("unsent");
        b.add(&l);
        return (vec![b.upcast()], Vec::new());
    }
    let atts: Vec<&Value> = m["attachments"].as_array().into_iter().flatten().collect();
    // RCS/SMS photos often come as a converted copy plus a missing original; show only what's there
    let any_present = atts.iter().any(|a| a["missing"].as_bool() != Some(true));
    let photos = photo_paths(m);
    let mut others = Vec::new(); // a photo that isn't on the Mac
    let mut files: Vec<FileRef> = Vec::new();
    for a in atts
        .into_iter()
        .filter(|a| !any_present || a["missing"].as_bool() != Some(true))
    {
        let path = a["original_path"].as_str().unwrap_or("");
        if path.ends_with(".pluginPayloadAttachment") {
            continue; // ponytail: link-preview data; drawn as a preview card in Phase 1.1
        }
        let mime = a["mime_type"].as_str().unwrap_or("");
        if is_image(mime, path) && !path.is_empty() && a["missing"].as_bool() != Some(true) {
            continue; // shown as a photo card (photo_paths)
        } else {
            let name = a["transfer_name"]
                .as_str()
                .or(a["filename"].as_str())
                .unwrap_or("Attachment");
            let missing = a["missing"].as_bool() == Some(true);
            if missing && is_image(mime, name) {
                let l = gtk::Label::new(None);
                l.set_markup(&format!(
                    "<span size='170%'>📷</span>  {}",
                    glib::markup_escape_text("Photo not downloaded on your Mac")
                ));
                l.add_css_class("file");
                l.set_xalign(0.0);
                others.push(l);
            } else {
                let size = a["total_bytes"].as_i64().unwrap_or(0);
                files.push(FileRef {
                    path: path.to_string(),
                    name: name.to_string(),
                    kind: file_kind(mime, name),
                    size,
                    missing,
                });
            }
        }
    }
    let mut out: Vec<gtk::Widget> = Vec::new();
    let mut saves: Vec<(usize, Vec<Save>)> = Vec::new();
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
    let look = bubble::Look {
        tail: end_of_run && plain.is_none(),
        ..look
    };
    if !photos.is_empty() {
        let tail =
            end_of_run && !has_bubble && link.is_none() && plain.is_none() && files.is_empty();
        saves.push((
            out.len(),
            photos
                .iter()
                .map(|p| Save::new(p, &save_name(m, p), 0, true))
                .collect(),
        ));
        out.push(photo_stack(u, &photos, bubble::Look { me, sms, tail }));
    }
    for (i, f) in files.iter().enumerate() {
        let look = bubble::Look {
            me,
            sms,
            tail: end_of_run
                && !has_bubble
                && link.is_none()
                && plain.is_none()
                && i + 1 == files.len(),
        };
        // a video on the Mac is a big first-frame picture like a photo; anything else (or no frame) is a card
        if !f.missing && !f.path.is_empty() {
            saves.push((out.len(), vec![Save::new(&f.path, &f.name, f.size, false)]));
        }
        let framed = f.kind == FileKind::Video
            && !f.missing
            && !f.path.is_empty()
            && !u.st.borrow().bad_images.contains(&f.path);
        out.push(if framed {
            video_preview(u, f, look)
        } else {
            file_card(u, f, look)
        });
    }
    if let Some(l) = &link {
        let look = bubble::Look {
            me,
            sms,
            tail: end_of_run && !has_bubble,
        };
        // a link that looks to lead to a picture is looked at ourselves too (only with our previews switched on): by
        // its ending, or by Messages having a picture for it but no real title
        let may_be_picture = crate::webpreview::is_image_url(&l.url)
            || l.image.is_some() && crate::webpreview::titled_like_a_file(&l.url, &l.title);
        let ours = may_be_picture.then(|| web_preview(u, &l.url)).flatten();
        match ours.as_ref().and_then(|lp| shown_picture(u, lp)) {
            Some(key) => {
                saves.push((out.len(), vec![Save::new(key, &web_name(key), 0, true)]));
                out.push(image(u, key, None, look).upcast());
            }
            None => out.push(link_card(u, l, look)),
        }
    }
    if lone_emoji(m) {
        let col = jumbo(&text, me);
        if m["edited_at"].as_str().is_some() {
            col.append(&edited_label(me));
        }
        out.push(col.upcast());
    } else if has_bubble {
        let b = bubble::Bubble::new(look);
        for l in others {
            b.add(&l);
        }
        if !text.is_empty() {
            let l = gtk::Label::new(None);
            l.set_markup(&linkify(&text));
            l.set_wrap(true);
            // WordChar (break inside a word if it has to) takes 3-4 times as long to lay out as Word, and 150 of them
            // were most of the ~150 ms the first frame took: only a text with a very long word (a link) needs it
            let long_word = text.split_whitespace().any(|w| w.chars().count() > 30);
            l.set_wrap_mode(if long_word {
                gtk::pango::WrapMode::WordChar
            } else {
                gtk::pango::WrapMode::Word
            });
            l.set_xalign(0.0);
            l.set_max_width_chars(60);
            l.set_selectable(true);
            l.set_can_focus(false);
            l.add_css_class("text");
            b.add(&l);
        }
        if m["edited_at"].as_str().is_some() {
            b.add(&edited_label(me));
        }
        out.push(b.upcast());
    }
    if let Some(url) = plain {
        let look = bubble::Look {
            me,
            sms,
            tail: end_of_run,
        };
        let lp = web_preview(u, &url);
        let picture = lp.as_ref().and_then(|lp| shown_picture(u, lp));
        // a picture link whose picture couldn't be read is a plain card again
        let page = lp.as_ref().filter(|lp| direct_image(lp).is_none());
        out.push(match (page, picture) {
            (_, Some(key)) => {
                saves.push((out.len(), vec![Save::new(key, &web_name(key), 0, true)]));
                image(u, key, None, look).upcast()
            }
            (Some(lp), None) => link_card(u, lp, look),
            (None, None) => plain_link_card(u, &url, look),
        });
    }
    (out, saves)
}

fn edited_label(me: bool) -> gtk::Label {
    let l = gtk::Label::new(Some("Edited"));
    l.add_css_class("edited");
    l.set_xalign(if me { 1.0 } else { 0.0 });
    l
}

/// A message that is drawn as big emoji: nothing but one to three of them.
fn lone_emoji(m: &Value) -> bool {
    m["attachments"].as_array().is_none_or(Vec::is_empty)
        && m["unsent_at"].as_str().is_none()
        && m["type"].as_str() != Some("unsent")
        && is_jumbo(&clean_text(m))
}

/// One to three emoji and nothing else: shown big, without a bubble.
fn is_jumbo(text: &str) -> bool {
    emoji_count(text).is_some_and(|n| (1..=3).contains(&n))
}

/// How many emoji `text` is made of, if it holds nothing but emoji and spaces. Joined families, skin tones, flags
/// and keycaps each count as one.
// shortcut: goes by Unicode blocks, so a plain symbol such as ★ counts too; use the Emoji_Presentation property if
// that bothers anyone
fn emoji_count(text: &str) -> Option<usize> {
    let flag = |c: char| ('\u{1F1E6}'..='\u{1F1FF}').contains(&c);
    let picture = |c: char| {
        matches!(c, '\u{1F000}'..='\u{1FAFF}' | '\u{2300}'..='\u{23FF}' | '\u{25A0}'..='\u{27BF}'
            | '\u{2900}'..='\u{2BFF}')
    };
    // symbols that are only an emoji when U+FE0F follows
    let dressed = |c: char| {
        matches!(
            c,
            '©' | '®' | '‼' | '⁉' | '™' | 'ℹ' | '\u{2194}'..='\u{21AA}' | '〰' | '〽' | '㊗' | '㊙'
        )
    };
    let mut chars = text.chars().peekable();
    let (mut n, mut joined) = (0, false);
    while let Some(c) = chars.next() {
        if c.is_whitespace() && !joined {
            continue;
        }
        if c.is_ascii_digit() || c == '#' || c == '*' {
            chars.next_if_eq(&'\u{FE0F}');
            chars.next_if_eq(&'\u{20E3}')?; // a keycap, or else it's ordinary text
        } else if flag(c) {
            chars.next_if(|&d| flag(d));
        } else if !picture(c) && !(dressed(c) && chars.peek() == Some(&'\u{FE0F}')) {
            return None;
        }
        // variation selector, skin tone and flag tags belong to the emoji before them
        while chars
            .next_if(|&d| matches!(d, '\u{FE0F}' | '\u{1F3FB}'..='\u{1F3FF}' | '\u{E0020}'..='\u{E007F}'))
            .is_some()
        {}
        if !joined {
            n += 1;
        }
        joined = chars.next_if_eq(&'\u{200D}').is_some();
    }
    Some(n)
}

/// The big emoji themselves, on the sender's side, lined up with the bubbles' bodies.
fn jumbo(text: &str, me: bool) -> gtk::Box {
    let col = gtk::Box::new(gtk::Orientation::Vertical, 2);
    col.set_halign(if me {
        gtk::Align::End
    } else {
        gtk::Align::Start
    });
    if me {
        col.set_margin_end(bubble::TAIL as i32);
    } else {
        col.set_margin_start(bubble::TAIL as i32);
    }
    let l = gtk::Label::new(Some(text.trim()));
    l.add_css_class("jumbo");
    l.set_selectable(true);
    l.set_can_focus(false);
    col.append(&l);
    col
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

// ---------- our own previews for plain links ----------

fn load_web_links(path: &std::path::Path) -> HashMap<String, Option<crate::mac::LinkPreview>> {
    let mut out = HashMap::new();
    for l in std::fs::read_to_string(path).unwrap_or_default().lines() {
        let Ok(v) = serde_json::from_str::<Value>(l) else {
            continue;
        };
        let Some(url) = v["url"].as_str() else {
            continue;
        };
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        let size = |k: &str| Some((v[k][0].as_i64()? as i32, v[k][1].as_i64()? as i32));
        let lp = (!v["failed"].as_bool().unwrap_or(false)).then(|| crate::mac::LinkPreview {
            url: url.to_string(),
            alt_url: None,
            title: s("title"),
            site: s("site"),
            image: v["image"].as_str().map(String::from),
            image_size: size("size"),
            icon: v["icon"].as_str().map(String::from),
            icon_size: size("icon_size"),
        });
        out.insert(url.to_string(), lp);
    }
    out
}

fn save_web_links(
    path: &std::path::Path,
    links: &HashMap<String, Option<crate::mac::LinkPreview>>,
) {
    let lines: Vec<String> = links
        .iter()
        .map(|(url, lp)| match lp {
            Some(l) => serde_json::json!({"url": url, "title": l.title, "site": l.site, "image": l.image, "size": l.image_size.map(|(w, h)| [w, h]), "icon": l.icon, "icon_size": l.icon_size.map(|(w, h)| [w, h])}).to_string(),
            None => serde_json::json!({"url": url, "failed": true}).to_string(),
        })
        .collect();
    crate::write_state(path, lines.join("\n") + "\n");
}

/// Our own preview for a link Messages made none for, if that is switched on in Settings: the saved one, else
/// (asked once) a fetch in the background that redraws the chat when it's done. None = draw the plain card.
fn web_preview(u: &Rc<Ui>, url: &str) -> Option<crate::mac::LinkPreview> {
    if !u.st.borrow().cfg.link_previews {
        return None;
    }
    let known = u.st.borrow().web_links.get(url).cloned();
    match known {
        // the picture was cleared out of the cache since: fetch it all again
        Some(Some(lp))
            if lp
                .image
                .as_ref()
                .is_some_and(|k| !cache_path(u, k, false).exists()) =>
        {
            u.st.borrow_mut().web_links.remove(url);
            request_web_preview(u, url);
            Some(crate::mac::LinkPreview {
                image: None,
                image_size: None,
                ..lp
            })
        }
        // a picture's address saved as a failure may date from before pictures were shown: ask again (once a run)
        Some(None) if crate::webpreview::is_image_url(url) => {
            request_web_preview(u, url);
            None
        }
        Some(lp) => lp,
        None => {
            request_web_preview(u, url);
            None
        }
    }
}

fn request_web_preview(u: &Rc<Ui>, url: &str) {
    // addresses on the local network are never fetched
    if !crate::webpreview::public_url(url) || !u.st.borrow_mut().web_asked.insert(url.to_string()) {
        return;
    }
    let (u2, url) = (u.clone(), url.to_string());
    glib::spawn_future_local(async move {
        let lp = fetch_web_preview(&u2, &url).await;
        let all = {
            let mut st = u2.st.borrow_mut();
            st.web_links.insert(url, lp);
            st.web_links.clone()
        };
        save_web_links(&u2.store.web_file, &all);
        queue_conv_render(&u2);
    });
}

/// GET a public web address: the body and the address it came from after any redirects. Redirects are followed here,
/// not by libsoup, so that every address on the way gets the public-address check (a public page could otherwise send
/// us to a local one).
async fn web_get(
    session: &webkit6::soup::Session,
    url: &str,
    max: usize,
    html: bool,
) -> Option<(Vec<u8>, String)> {
    use webkit6::soup::{MessageFlags, Status, prelude::*};
    let mut url = url.to_string();
    for _ in 0..5 {
        if !crate::webpreview::public_url(&url) {
            return None;
        }
        let msg = webkit6::soup::Message::new("GET", &url).ok()?;
        msg.add_flags(MessageFlags::NO_REDIRECT);
        // the headers arrive first; the body is read below in pieces, so a huge one never lands in memory
        let stream = session
            .send_future(&msg, glib::Priority::DEFAULT)
            .await
            .ok()?;
        if matches!(
            msg.status(),
            Status::MovedPermanently
                | Status::Found
                | Status::SeeOther
                | Status::TemporaryRedirect
                | Status::PermanentRedirect
        ) {
            let to = msg.response_headers()?.one("Location")?;
            url = crate::webpreview::resolve(&url, to.trim());
            continue;
        }
        let headers = msg.response_headers()?;
        let is_html = headers
            .content_type()
            .is_some_and(|(t, _)| t.contains("html"));
        if msg.status() != Status::Ok || is_html != html || headers.content_length() > max as i64 {
            return None;
        }
        // a server that never stops sending is cut off at `max` (the idle timeout alone would never fire)
        let mut body = Vec::new();
        loop {
            let chunk = stream
                .read_bytes_future(64 * 1024, glib::Priority::DEFAULT)
                .await
                .ok()?;
            if chunk.is_empty() {
                return Some((body, url));
            }
            body.extend_from_slice(&chunk);
            if body.len() > max {
                return None;
            }
        }
    }
    None
}

async fn fetch_web_preview(u: &Rc<Ui>, url: &str) -> Option<crate::mac::LinkPreview> {
    // relative picture and icon addresses are relative to the page we ended on, not the link as sent
    let Some((body, reached)) = web_get(&u.web, url, 3_000_000, true).await else {
        // not a page: perhaps the link is a picture itself
        let (key, size) = fetch_web_picture(u, Some(url.to_string()), 15_000_000).await?;
        return Some(crate::mac::LinkPreview {
            url: url.to_string(),
            image: Some(key),
            image_size: Some(size),
            ..Default::default()
        });
    };
    let page = crate::webpreview::parse(&String::from_utf8_lossy(&body), &reached);
    if page.title.is_empty() && page.image.is_none() {
        return None;
    }
    let image = fetch_web_picture(u, page.image, 8_000_000).await;
    let icon = fetch_web_picture(u, page.icon, 1_000_000).await;
    let (image, image_size) = image.unzip();
    let (icon, icon_size) = icon.unzip();
    Some(crate::mac::LinkPreview {
        url: url.to_string(),
        alt_url: None,
        title: page.title,
        site: page.site,
        image,
        image_size,
        icon,
        icon_size,
    })
}

/// Fetch a page's picture or icon into the image cache: its cache key and size, or None when it isn't a public
/// address, can't be read, or isn't a format we can size (and so draw).
async fn fetch_web_picture(
    u: &Rc<Ui>,
    url: Option<String>,
    max: usize,
) -> Option<(String, (i32, i32))> {
    let url = url.filter(|i| crate::webpreview::public_url(i))?;
    let (bytes, _) = web_get(&u.web, &url, max, false).await?;
    let size = crate::webpreview::image_size(&bytes)?;
    let key = format!("web:{url}");
    let _ = std::fs::create_dir_all(&u.store.image_cache);
    std::fs::write(cache_path(u, &key, false), bytes).ok()?;
    Some((key, size))
}

/// The picture a link leads straight to, if that's what our own look at it found: its key in the image cache.
fn direct_image(lp: &crate::mac::LinkPreview) -> Option<&str> {
    lp.image
        .as_deref()
        .filter(|k| k.strip_prefix("web:") == Some(lp.url.as_str()))
}

/// `direct_image`, unless that picture turned out to be unreadable.
fn shown_picture<'a>(u: &Ui, lp: &'a crate::mac::LinkPreview) -> Option<&'a str> {
    direct_image(lp).filter(|k| !u.st.borrow().bad_images.contains(*k))
}

/// A link with no preview from Messages, drawn like a card whose picture is just a small icon: the
/// host as its title and a link glyph on the right.
fn plain_link_card(u: &Rc<Ui>, url: &str, look: bubble::Look) -> gtk::Widget {
    let b = bubble::Bubble::card(look, LINK_W, 0);
    let host = url
        .split('/')
        .nth(2)
        .unwrap_or(url)
        .trim_start_matches("www.")
        .to_string();
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
    if u.st.borrow().cfg.link_previews {
        b.set_tooltip_text(Some(url));
    } else {
        b.set_tooltip_text(Some(&format!("{url}\nTo see a preview, turn on \"Make previews for links Messages didn't\" in Settings > Privacy")));
    }
    b.set_cursor_from_name(Some("pointer"));
    let key = chat_youtube(u, url, b.upcast_ref());
    let (u2, url, b2) = (
        u.clone(),
        url.to_string(),
        b.clone().upcast::<gtk::Widget>(),
    );
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
    let host = |u: &str| {
        u.split('/')
            .nth(2)
            .unwrap_or("")
            .trim_start_matches("www.")
            .to_lowercase()
    };
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
    if t.chars().all(|c| c == '/') {
        String::new()
    } else {
        t.to_string()
    }
}

/// The row holding Apple's preview for this message: folded in by imsg (`url_preview`), or the
/// message itself when it is the preview row.
fn preview_row(m: &Value) -> Option<i64> {
    m["url_preview"]["id"].as_i64().or_else(|| {
        (m["balloon_bundle_id"].as_str() == Some("com.apple.messages.URLBalloonProvider"))
            .then(|| m["id"].as_i64())
            .flatten()
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

/// The width of every preview in the chat (photos, videos, link and file cards), like Messages: a portrait
/// and a landscape photo are equally wide, the height follows the picture.
const MEDIA_W: i32 = 300;
const LINK_W: i32 = MEDIA_W;
const SMALL_W: i32 = 200; // a small photo is shown at least this wide (up to twice its size)

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
    let odd =
        |s: Option<(i32, i32)>| s.is_some_and(|(w, h)| w > 0 && h > 0 && (w > 3 * h || h > 3 * w));
    match (&l.image, &l.icon) {
        (Some(i), Some(c)) => Some(
            if odd(l.image_size) || area(l.icon_size) > area(l.image_size) {
                c
            } else {
                i
            },
        ),
        (i, c) => i.as_ref().or(c.as_ref()),
    }
}

/// `link_card` at a given width (the info panel's cards follow the panel).
fn link_card_w(
    u: &Rc<Ui>,
    l: &crate::mac::LinkPreview,
    look: bubble::Look,
    link_w: i32,
) -> gtk::Widget {
    let l = &usable(u, l);
    // a real preview picture goes across the top; a small one (the site's logo) sits beside the text
    let big = l.image.is_some()
        && l.image_size
            .is_none_or(|(w, h)| crate::mac::is_preview_size(w, h));
    let small = if big { None } else { small_picture(l) };
    // the preview shows whole, in its own proportions (portrait ones too); only a freak shape is capped
    let img_h = match l.image_size {
        _ if !big => 0,
        Some((w, h)) if w > 0 => {
            (link_w as f64 * h as f64 / w as f64).clamp(60.0, 2.0 * link_w as f64) as i32
        }
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
    let site = gtk::Label::new(Some(&format!(
        "🔗 {}",
        if l.site.is_empty() { &l.url } else { &l.site }
    )));
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
    // a YouTube card with a picture is two targets: the picture plays here, the text under it opens the browser
    let split = big && crate::mac::youtube_id(&l.url).is_some();
    if split {
        b.set_has_tooltip(true);
        let url = l.url.clone();
        b.connect_query_tooltip(move |b, _, y, _, tip| {
            let h = b.card_image_height() as i32;
            let on_picture = y < h;
            if on_picture {
                tip.set_text(Some("Play here"));
            } else {
                tip.set_text(Some(&format!("Open on YouTube\n{url}")));
            }
            // the tip is asked for again once the pointer leaves this half
            let (top, height) = if on_picture {
                (0, h)
            } else {
                (h, b.height() - h)
            };
            tip.set_tip_area(&gdk::Rectangle::new(0, top, b.width(), height));
            true
        });
    } else {
        b.set_tooltip_text(Some(&l.url));
    }
    b.set_cursor_from_name(Some("pointer"));
    let click = gtk::GestureClick::new();
    let key = chat_youtube(u, &l.url, b.upcast_ref());
    let (u2, url) = (u.clone(), l.url.clone());
    // weakly: a strong reference from its own handler would keep every redrawn card alive
    let weak = b.downgrade();
    click.connect_released(move |g, _, _, y| {
        g.set_state(gtk::EventSequenceState::Claimed);
        let Some(b) = weak.upgrade() else { return };
        let h = b.card_image_height();
        if split && h > 0.0 && y >= f64::from(h) {
            open_browser(&u2, &url);
        } else {
            open_link(&u2, &url, b.upcast_ref(), key.as_deref());
        }
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
    let side = if me {
        gtk::Align::End
    } else {
        gtk::Align::Start
    };
    let base = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let o = gtk::Overlay::new();
    o.set_child(Some(&base));
    o.set_halign(side);
    // created in message order (that's the viewer's order); photos past CARDS_SHOWN aren't drawn
    // in the stack but stay reachable in the viewer
    let all: Vec<bubble::Bubble> = paths
        .iter()
        .enumerate()
        .map(|(i, path)| {
            image(
                u,
                path,
                Some(CARD),
                bubble::Look {
                    tail: look.tail && i == 0,
                    ..look
                },
            )
        })
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
            if me {
                p.set_margin_end(off)
            } else {
                p.set_margin_start(off)
            }
        }
    };
    // the widest spacing that still fits the chat; resting never exceeds it, hovering uses all of it
    let room = {
        let u = Rc::downgrade(u);
        move || {
            let w = u.upgrade().map_or(0.0, |u| stack_room(&u));
            // never further apart than side by side
            if w <= 0.0 {
                CARD_STEP
            } else {
                ((w - (CARD as f64 + bubble::TAIL as f64)) / (shown as f64 - 1.0))
                    .clamp(0.0, CARD as f64 + 12.0)
            }
        }
    };
    let step = Rc::new(Cell::new(CARD_STEP.min(room())));
    let hovered = Rc::new(Cell::new(false));
    place(step.get());
    let (st2, pl2, rm2, hv2) = (step.clone(), place.clone(), room.clone(), hovered.clone());
    u.stacks.borrow_mut().push(Rc::new(move || {
        let s = if hv2.get() {
            rm2()
        } else {
            CARD_STEP.min(rm2())
        };
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
            if p >= 1.0 {
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
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
        if me {
            more.set_margin_end(inset)
        } else {
            more.set_margin_start(inset)
        }
        more.set_margin_bottom(10);
        o.add_overlay(&more);
    }
    o.upcast()
}

/// A photo card. `card` = fixed square size (stacks); otherwise it takes the photo's own shape.
/// Clicking opens the viewer.
fn image(u: &Rc<Ui>, path: &str, card: Option<i32>, look: bubble::Look) -> bubble::Bubble {
    let (w, h) = card.map(|c| (c, c)).unwrap_or((MEDIA_W, MEDIA_W * 3 / 4));
    let p = bubble::Bubble::photo(look, w, h);
    if card.is_some() {
        p.add_css_class("stacked");
    }
    p.set_cursor_from_name(Some("pointer"));
    if let Some(url) = path.strip_prefix("web:") {
        p.set_tooltip_text(Some(url));
    }
    let click = gtk::GestureClick::new();
    let (u2, path2, p2) = (
        u.clone(),
        path.to_string(),
        p.clone().upcast::<gtk::Widget>(),
    );
    click.connect_released(move |g, _, _, _| {
        g.set_state(gtk::EventSequenceState::Claimed);
        open_media(&u2, &path2, &p2);
    });
    p.add_controller(click);
    // right-click: copy the picture or save it
    let menu = gtk::GestureClick::builder().button(3).build();
    let (u2, path2, p2) = (u.clone(), path.to_string(), p.clone());
    menu.connect_pressed(move |g, _, x, y| {
        g.set_state(gtk::EventSequenceState::Claimed);
        let copy = gtk::Button::with_label("Copy image");
        let save = gtk::Button::with_label("Save as…");
        for b in [&copy, &save] {
            b.add_css_class("flat");
        }
        let col = gtk::Box::new(gtk::Orientation::Vertical, 2);
        col.append(&copy);
        col.append(&save);
        if let Some(url) = path2.strip_prefix("web:") {
            let open = gtk::Button::with_label("Open in browser");
            open.add_css_class("flat");
            let (u3, url) = (u2.clone(), url.to_string());
            open.connect_clicked(move |_| open_browser(&u3, &url));
            col.append(&open);
        }
        let pop = gtk::Popover::new();
        pop.set_child(Some(&col));
        pop.set_parent(&p2);
        pop.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        let (u3, path3, p3) = (u2.clone(), path2.clone(), pop.clone());
        copy.connect_clicked(move |_| {
            p3.popdown();
            let tex = u3.st.borrow().textures.get(&path3).cloned();
            if let Some(t) = tex {
                u3.win.clipboard().set_texture(&t);
            }
        });
        let (u3, path3, p3, photo) = (u2.clone(), path2.clone(), pop.clone(), p2.clone());
        save.connect_clicked(move |_| {
            p3.popdown();
            let name = web_name(&path3);
            let near = save_button_near(photo.upcast_ref());
            save_items(
                &u3,
                vec![Save::new(&path3, &name, 0, true)],
                true,
                Rc::new(move |ok| {
                    if let (true, Some(b)) = (ok, &near) {
                        show_saved(b);
                    }
                }),
            );
        });
        unparent_on_close(&pop);
        pop.popup();
    });
    p.add_controller(menu);
    u.st.borrow_mut()
        .photos
        .push((path.to_string(), p.clone().upcast()));
    load_texture(u, &p, path);
    if is_gif(path) {
        animate_gif(u, &p, path);
    }
    p
}

/// A GIF ends with a `;` byte; a copy cut off on the way almost never does.
fn whole_gif(file: &std::path::Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let mut last = [0u8];
    std::fs::File::open(file).is_ok_and(|mut f| {
        f.seek(SeekFrom::End(-1)).is_ok() && f.read_exact(&mut last).is_ok() && last == *b";"
    })
}

/// Make a GIF's bubble move: the still copy the Mac converts shows until the original file is here.
// shortcut: plays whenever it's in the loaded chat, scrolled out of view too; pause by scroll position if it costs CPU
fn animate_gif(u: &Rc<Ui>, p: &bubble::Bubble, path: &str) {
    let playing = u.st.borrow().gifs.get(path).cloned();
    if let Some(player) = playing {
        p.animate(player);
        return;
    }
    let dest = gif_dest(u, path);
    if whole_gif(&dest) {
        play_gif(u, p, path, &dest);
    } else {
        queue_gif(u, path, dest, Some(p));
    }
}

fn gif_dest(u: &Ui, path: &str) -> PathBuf {
    if path.starts_with("web:") {
        return cache_path(u, path, false); // fetched whole already
    }
    file_dest(u, path, path.rsplit('/').next().unwrap_or("image.gif"))
}

/// Ask the Mac for a GIF's original file, once, however many are waiting for it.
fn queue_gif(u: &Ui, path: &str, dest: PathBuf, bubble: Option<&bubble::Bubble>) {
    if path.starts_with("web:") {
        return; // not on the Mac; if our copy isn't whole, the still picture stays
    }
    let send = {
        let mut st = u.st.borrow_mut();
        let busy = st.save_to.contains_key(path) || st.gif_wait.contains_key(path);
        let waiting = st.gif_wait.entry(path.to_string()).or_default();
        waiting.extend(bubble.map(|b| b.downgrade()));
        !busy
    };
    if send {
        u.send(Cmd::Download {
            path: path.to_string(),
            dest,
            size: 0,
        });
    }
}

fn play_gif(u: &Rc<Ui>, p: &bubble::Bubble, path: &str, file: &std::path::Path) {
    let known = u.st.borrow().gifs.get(path).cloned();
    let player = match known {
        Some(player) => player,
        None => {
            let Ok(player) = crate::player::Player::looping(file) else {
                return; // the still picture stays
            };
            let player = Rc::new(player);
            u.st.borrow_mut()
                .gifs
                .insert(path.to_string(), player.clone());
            player
        }
    };
    p.animate(player);
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
                bg(
                    move || decode_file(&f3),
                    move |tex| match tex {
                        Some(_) => image_done(&u2, path2, false, tex),
                        // a picture off the web isn't the Mac's to give again
                        None if path2.starts_with("web:") => image_done(&u2, path2, false, None),
                        None => {
                            // unreadable copy: forget it and fetch it again
                            let _ = std::fs::remove_file(&f2);
                            let pics = u2
                                .st
                                .borrow_mut()
                                .waiting
                                .remove(&path2)
                                .unwrap_or_default();
                            for p in pics {
                                load_texture_in(&u2, &p, &path2, bulk);
                            }
                        }
                    },
                );
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
fn bg<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(T) + 'static,
) {
    use std::sync::{Arc, Mutex, OnceLock, mpsc};
    static POOL: OnceLock<mpsc::Sender<Job>> = OnceLock::new();
    let tx = POOL.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        for _ in 0..2 {
            let rx = rx.clone();
            std::thread::spawn(move || {
                loop {
                    // a poisoned lock means a job panicked: crash so the bug shows (see mac.rs)
                    #[allow(clippy::unwrap_used)]
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
        && let Some(slot) = p
            .first_child()
            .and_then(|inner| inner.first_child())
            .and_downcast::<gtk::Box>()
    {
        return slot;
    }
    p.clone().upcast()
}

fn show_texture(p: &bubble::Bubble, t: &gdk::Texture) {
    stop_spin(p);
    if !p.has_css_class("stacked") && !p.has_css_class("fixed") {
        // stacked cards stay square and crop to fill
        let (w, h) = (t.width().max(1) as f64, t.height().max(1) as f64);
        let s = photo_scale(w, h);
        p.set_photo_size((w * s) as i32, (h * s) as i32);
    }
    p.set_texture(Some(t));
}

/// How much a single photo of `w` x `h` pixels is scaled in the chat: as wide as every other preview, but a small
/// one only to between `SMALL_W` and that, and never past twice its size; a very tall one is narrowed instead.
fn photo_scale(w: f64, h: f64) -> f64 {
    let wide = w.clamp(f64::from(SMALL_W), f64::from(MEDIA_W));
    (wide / w).min(2.0).min(2.2 * f64::from(MEDIA_W) / h)
}

/// `full`: the viewer's big copy (mac::FULL_PX) rather than the chat's.
fn cache_path(u: &Ui, path: &str, full: bool) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    // "-2": the Mac's copies from before it stopped enlarging small pictures are left to age out
    let made = if path.starts_with("web:") { "" } else { "-2" };
    u.store.image_cache.join(format!(
        "{:016x}{}{made}.jpg",
        h.finish(),
        if full { "-full" } else { "" }
    ))
}

/// The viewer wants the big version of a photo: disk cache, else ask the Mac (once).
fn fetch_full(u: &Rc<Ui>, path: &str) {
    if !u.st.borrow_mut().waiting_full.insert(path.to_string()) {
        return; // already on its way
    }
    // a picture off the web is cached whole: that copy is the big one
    let web = path.starts_with("web:");
    let file = cache_path(u, path, !web);
    if file.exists() {
        let (u2, path2) = (u.clone(), path.to_string());
        bg(
            move || decode_file(&file),
            move |tex| match tex {
                Some(_) => image_done(&u2, path2, true, tex),
                None => {
                    let _ = std::fs::remove_file(cache_path(&u2, &path2, true));
                    u2.st.borrow_mut().waiting_full.remove(&path2);
                    fetch_full(&u2, &path2);
                }
            },
        );
    } else {
        u.send(Cmd::Image {
            path: path.to_string(),
            full: true,
        });
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
    switcher: gtk::Box,
    photos: gtk::FlowBox,
    videos: gtk::FlowBox,
    files: gtk::FlowBox,
    links: gtk::FlowBox,
    stack: gtk::Stack,
    scroll: gtk::ScrolledWindow,
}

/// Where the loading of one kind of item has got to.
#[derive(Clone, Copy)]
struct Paging {
    offset: usize,
    /// the Mac has older ones
    more: bool,
    loading: bool,
}

const TILES: usize = 24; // shown at first, and added each time the end is scrolled to
const COLS: usize = 4;
const INFO_KINDS: [Kind; 5] = [
    Kind::Photos,
    Kind::Videos,
    Kind::Files,
    Kind::Links,
    Kind::YouTube,
];

/// What a tab (named by its first kind) is filled from.
fn sources(tab: Kind) -> &'static [Kind] {
    match tab {
        Kind::Photos => &[Kind::Photos],
        Kind::Videos | Kind::YouTube => &[Kind::Videos, Kind::YouTube],
        Kind::Files => &[Kind::Files],
        Kind::Links => &[Kind::Links],
    }
}

/// How many tiles a tab shows now, and whether older pages are needed to make up the number. `ready` items are in
/// their final order; `more`: the Mac has older ones. A part-filled row only ever shows at the very end.
fn tiles_to_show(ready: usize, allowed: usize, more: bool) -> (usize, bool) {
    if ready >= allowed {
        (allowed, false)
    } else if more {
        (ready - ready % COLS, true)
    } else {
        (ready, false)
    }
}

fn kind_idx(k: Kind) -> usize {
    match k {
        Kind::Photos => 0,
        Kind::Videos | Kind::YouTube => 1,
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
    let refresh = gtk::Button::from_icon_name("bf-refresh-symbolic");
    refresh.add_css_class("head-btn");
    refresh.set_valign(gtk::Align::Center);
    refresh.set_tooltip_text(Some("Reload this conversation from the Mac"));
    let close = gtk::Button::from_icon_name("bf-close-symbolic");
    close.add_css_class("head-btn");
    close.set_valign(gtk::Align::Center);
    let [photos, videos, files, links] = [(); 4].map(|_| {
        let f = gtk::FlowBox::new();
        f.set_selection_mode(gtk::SelectionMode::None);
        f.set_min_children_per_line(1);
        f.set_max_children_per_line(COLS as u32);
        f.set_row_spacing(4);
        f.set_column_spacing(4);
        f.set_homogeneous(true);
        f
    });
    let stack = gtk::Stack::new();
    stack.set_vhomogeneous(false); // else a short tab scrolls as far as the tallest one
    for (grid, name, title) in [
        (&photos, "photos", "Photos"),
        (&videos, "videos", "Videos"),
        (&files, "files", "Files"),
        (&links, "links", "Links"),
    ] {
        // in a box of its own, so a short tab's tiles stay at the top
        let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        page.append(grid);
        stack.add_titled(&page, Some(name), title);
    }
    let switcher = tab_switcher(
        &stack,
        &[
            ("photos", "Photos"),
            ("videos", "Videos"),
            ("files", "Files"),
            ("links", "Links"),
        ],
    );
    // one row: the tabs, and the close button
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    head.set_margin_top(12);
    head.set_margin_bottom(8);
    head.set_margin_start(14);
    head.set_margin_end(14);
    head.append(&switcher);
    head.append(&close);
    // only the tab's content scrolls
    stack.set_margin_start(14);
    stack.set_margin_end(14);
    stack.set_margin_bottom(14);
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&stack)
        .build();
    let panel = gtk::Box::new(gtk::Orientation::Vertical, 0);
    panel.append(&head);
    panel.append(&scroll);
    panel.set_size_request(INFO_W, -1); // the CSS margin counts inside it
    panel.set_hexpand(false);
    panel.set_overflow(gtk::Overflow::Hidden); // the rounded corners clip it
    panel.add_css_class("info-panel");
    let revealer = gtk::Revealer::builder()
        .child(&panel)
        .transition_type(gtk::RevealerTransitionType::SlideLeft)
        .reveal_child(false)
        .build();
    // expand flags pass up from the children: without this, the closed panel still takes a share of the width
    revealer.set_hexpand(false);
    Info {
        revealer,
        panel,
        btn,
        refresh,
        close,
        switcher,
        photos,
        videos,
        files,
        links,
        stack,
        scroll,
    }
}

const THUMB: i32 = 92;
/// The panel's width: four tiles (each cell measures THUMB + 6) with their gaps and the margins. It slides over the
/// chat, and over the sidebar if it has to: even the narrowest window is wider.
const INFO_W: i32 = COLS as i32 * (THUMB + 6) + (COLS as i32 - 1) * 4 + 28 + 14;

/// Share the window between sidebar, chat and info panel.
/// - Too narrow for the full sidebar beside the chat (`SIDEBAR_MIN` + `CHAT_MIN`): the sidebar shows avatars
///   only (`COLLAPSED_W`), or fills the window when expanded with its caret.
fn relayout(u: &Rc<Ui>, total: i32) {
    if total <= 0 {
        return;
    }
    if u.search_p.get() > 0.0 {
        search_place(u, u.search_p.get());
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
        if u.side_open.get() {
            total - 8
        } else {
            COLLAPSED_W
        }
    } else {
        pref.clamp(SIDEBAR_MIN, total - CHAT_MIN)
    };
    // crossing the line slides the sidebar to its new width (not on the first layout)
    let animate = u.animate.replace(false) || (crossed && u.last_pos.get() != 0);
    move_side(u, side, animate);
    // sidebar: avatars only, or with names
    let collapsed = narrow && !u.side_open.get();
    u.side_toggle.set_visible(narrow);
    u.side_toggle.set_tooltip_text(Some(if collapsed {
        "Show conversation names"
    } else {
        "Hide conversation names"
    }));
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
        u.text_target
            .set((inner - 32 - AV_PAD - AVATAR - 11 - 11 - 10).max(40));
    }
    if u.slide_to.get() == NO_SLIDE {
        settle(u);
    }
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
    for p in u.row_parts.borrow().values() {
        p.short.set_visible(collapsed);
        p.stack.set_size_request(stack_w(collapsed), -1);
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
        let Some(u) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
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
    // near the end of a tab (or a tab that doesn't fill the panel) brings more; after the layout that told us
    let queued = Rc::new(Cell::new(false));
    let w = Rc::downgrade(u);
    let near_end = Rc::new(move || {
        if !queued.replace(true) {
            let (w, queued) = (w.clone(), queued.clone());
            glib::idle_add_local_once(move || {
                queued.set(false);
                if let Some(u) = w.upgrade() {
                    more_if_near_end(&u);
                }
            });
        }
    });
    let adj = u.info.scroll.vadjustment();
    let f = near_end.clone();
    adj.connect_value_changed(move |_| f());
    let f = near_end.clone();
    adj.connect_changed(move |_| f());
    u.info
        .stack
        .connect_visible_child_name_notify(move |_| near_end());
}

fn more_if_near_end(u: &Rc<Ui>) {
    let adj = u.info.scroll.vadjustment();
    // once scrolling, two rows ahead of the end; before that only if the panel isn't filled
    let ahead = if adj.value() > 0.0 { 200.0 } else { 0.0 };
    // (a page size of 0: not laid out yet)
    if adj.page_size() <= 0.0 || adj.upper() - adj.value() - adj.page_size() > ahead {
        return;
    }
    let tab = match u.info.stack.visible_child_name().as_deref() {
        Some("videos") => Kind::Videos,
        Some("files") => Kind::Files,
        Some("links") => Kind::Links,
        _ => Kind::Photos,
    };
    {
        let mut st = u.st.borrow_mut();
        if st.info_chat.is_none() || !st.info_more[kind_idx(tab)] {
            return;
        }
        st.info_more[kind_idx(tab)] = false;
        st.info_shown[kind_idx(tab)] += TILES;
    }
    update_tab(u, tab);
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
        (None, None) => open_browser(u, url),
    }
}

fn open_browser(u: &Ui, url: &str) {
    gtk::UriLauncher::new(url).launch(Some(&u.win), gio::Cancellable::NONE, |_| {});
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
    let mut items = if in_panel {
        info_sequence(u, from.is_ancestor(&u.info.videos))
    } else {
        u.st.borrow().photos.clone()
    };
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
    for flow in [if videos {
        &u.info.videos
    } else {
        &u.info.photos
    }] {
        let mut c = flow.first_child();
        while let Some(fc) = c {
            if let Some(w) = fc
                .downcast_ref::<gtk::FlowBoxChild>()
                .and_then(|f| f.child())
                && let Some((date, key)) = w.widget_name().split_once('|')
                && let Ok(date) = date.parse::<i64>()
            {
                // a video's picture is the tile's inner bubble; a YouTube tile is its own widget
                let item = if key.starts_with("yt:") {
                    w.clone()
                } else {
                    w.clone()
                        .downcast::<gtk::Overlay>()
                        .ok()
                        .and_then(|o| o.child())
                        .unwrap_or(w.clone())
                };
                v.push((date, (key.to_string(), item)));
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
        st.info_photos.clear();
        st.info_links.clear();
        st.info_youtube.clear();
        st.info_shown = [TILES; 4];
        st.info_drawn = [0; 4];
        st.info_more = [false; 4];
        st.info_files.clear();
        st.info_videos.clear();
        st.link_waiters.clear();
        st.media.retain(|k, _| k.0 != id);
    }
    u.info.photos.remove_all();
    u.info.videos.remove_all();
    u.info.files.remove_all();
    u.info.links.remove_all();
    if u.st.borrow_mut().info_last.replace(id) != Some(id) {
        u.info.stack.set_visible_child_name("photos");
        u.info.scroll.vadjustment().set_value(0.0);
    }
    for kind in INFO_KINDS {
        let waiting = Paging {
            offset: 0,
            more: true,
            loading: false,
        };
        u.st.borrow_mut().media.insert((id, kind), waiting);
        fetch_media(u, kind);
    }
}

/// Ask the Mac for the next page of one kind of item, unless it's on its way or there is none.
fn fetch_media(u: &Rc<Ui>, kind: Kind) {
    let (chat_id, offset) = {
        let mut st = u.st.borrow_mut();
        let Some(chat) = st.info_chat else { return };
        let Some(p) = st.media.get_mut(&(chat, kind)) else {
            return;
        };
        if p.loading || !p.more {
            return;
        }
        p.loading = true;
        (chat, p.offset)
    };
    u.send(Cmd::Media {
        chat_id,
        kind,
        offset,
    });
}

fn fmt_date(unix: i64) -> String {
    glib::DateTime::from_unix_local(unix)
        .ok()
        .and_then(|d| d.format("%b %-d, %Y").ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
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
fn on_media(
    u: &Rc<Ui>,
    chat_id: i64,
    kind: Kind,
    offset: usize,
    items: Vec<MediaItem>,
    next: usize,
    more: bool,
) {
    {
        let mut st = u.st.borrow_mut();
        // for another chat (the panel moved on) or an out-of-date page: drop it
        let wanted = st
            .media
            .get(&(chat_id, kind))
            .is_some_and(|p| p.offset == offset);
        if st.info_chat != Some(chat_id) || !wanted {
            return;
        }
        let paging = Paging {
            offset: next,
            more,
            loading: false,
        };
        st.media.insert((chat_id, kind), paging);
        match kind {
            Kind::Photos => st.info_photos.extend(items),
            Kind::Videos => st.info_videos.extend(items),
            Kind::Files => st.info_files.extend(items),
            Kind::Links => st.info_links.extend(items),
            Kind::YouTube => st.info_youtube.extend(items),
        }
    }
    update_tab(u, kind);
}

/// Redraw the tab that `kind` belongs to, and ask for older pages if it has fewer tiles than it may show.
fn update_tab(u: &Rc<Ui>, kind: Kind) {
    let tab = match kind {
        Kind::YouTube => Kind::Videos,
        k => k,
    };
    // the tab's items newest first, how far back all its sources are loaded, and the sources that have older ones
    let (items, allowed, older) = {
        let st = u.st.borrow();
        let Some(chat) = st.info_chat else { return };
        let list = |k: Kind| match k {
            Kind::Photos => &st.info_photos,
            Kind::Videos => &st.info_videos,
            Kind::Files => &st.info_files,
            Kind::Links => &st.info_links,
            Kind::YouTube => &st.info_youtube,
        };
        let mut items: Vec<MediaItem> = sources(tab)
            .iter()
            .flat_map(|&k| list(k).iter().cloned())
            // YouTube links are in the Videos tab, not with the other links
            .filter(|i| tab != Kind::Links || crate::mac::youtube_id(&i.url).is_none())
            .collect();
        items.sort_by_key(|i| std::cmp::Reverse(i.date));
        let older: Vec<(Kind, i64)> = sources(tab)
            .iter()
            .filter(|&&k| st.media.get(&(chat, k)).is_some_and(|p| p.more))
            .map(|&k| (k, list(k).last().map_or(i64::MAX, |i| i.date)))
            .collect();
        (items, st.info_shown[kind_idx(tab)], older)
    };
    // an item older than what a source is loaded to isn't in its final place yet: a newer one may still come
    let horizon = older.iter().map(|&(_, d)| d).max();
    let ready = items
        .iter()
        .filter(|i| horizon.is_none_or(|h| i.date >= h))
        .count();
    let (n, short) = tiles_to_show(ready, allowed, horizon.is_some());
    if short {
        for &(k, date) in &older {
            if Some(date) == horizon {
                fetch_media(u, k);
            }
        }
    }
    let (grid, none) = match tab {
        Kind::Photos => (&u.info.photos, "No photos in this conversation"),
        Kind::Videos | Kind::YouTube => (&u.info.videos, "No videos in this conversation"),
        Kind::Files => (&u.info.files, "No files in this conversation"),
        Kind::Links => (&u.info.links, "No links in this conversation"),
    };
    // only the new tiles are added: rebuilding hundreds on every page would stutter the scrolling
    let drawn = {
        let mut st = u.st.borrow_mut();
        st.info_more[kind_idx(tab)] = !short && (items.len() > n || horizon.is_some());
        std::mem::replace(&mut st.info_drawn[kind_idx(tab)], n)
    };
    if drawn == 0 {
        grid.remove_all();
    }
    for it in items.iter().take(n).skip(drawn) {
        match tab {
            Kind::Photos => grid.append(&photo_tile(u, it)),
            Kind::Links => grid.append(&link_slot(u, it)),
            _ if it.url.is_empty() => grid.append(&file_tile(u, it)),
            _ => {
                let slot = link_slot(u, it);
                if let Some(id) = crate::mac::youtube_id(&it.url) {
                    let key = crate::viewer::yt_key(&id, it.row as usize);
                    slot.set_widget_name(&format!("{}|{key}", it.date));
                }
                grid.append(&slot);
            }
        }
    }
    if items.is_empty() && horizon.is_none() {
        let l = gtk::Label::new(Some(none));
        l.add_css_class("dim");
        grid.append(&l);
    }
}

/// A photo as a square tile; one that isn't on the Mac says so.
fn photo_tile(u: &Rc<Ui>, it: &MediaItem) -> gtk::Widget {
    if it.missing {
        // Messages hasn't downloaded it from iCloud, or it's gone: nothing to fetch
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
        return tile.upcast();
    }
    let b = bubble::Bubble::tile(THUMB);
    b.add_css_class("fixed");
    b.set_cursor_from_name(Some("pointer"));
    let (u2, path) = (u.clone(), it.path.clone());
    let weak = b.downgrade();
    let g = gtk::GestureClick::new();
    g.connect_released(move |g, _, _, _| {
        g.set_state(gtk::EventSequenceState::Claimed);
        if let Some(b) = weak.upgrade() {
            open_media(&u2, &path, b.upcast_ref());
        }
    });
    b.add_controller(g);
    b.set_widget_name(&format!("{}|{}", it.date, it.path)); // read back by `info_sequence`
    load_texture_in(u, &b, &it.path, true);
    b.upcast()
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
    caption.set_max_width_chars(1); // a long name must not widen the tile
    caption.set_xalign(0.0);
    frame.add_overlay(&caption);
    let detail: Vec<String> = [
        kind.label().to_string(),
        fmt_size(it.size),
        fmt_date(it.date),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect();
    if it.missing {
        frame.add_css_class("file-missing");
        frame.set_tooltip_text(Some(&format!(
            "{}\n{}\nMessages hasn't downloaded this file to your Mac",
            it.name,
            detail.join(" · ")
        )));
    } else if kind == FileKind::Video && thumb {
        frame.set_tooltip_text(Some(&format!(
            "{}\n{}\nClick to play",
            it.name,
            detail.join(" · ")
        )));
        frame.set_cursor_from_name(Some("pointer"));
        let b = frame
            .child()
            .and_downcast::<bubble::Bubble>()
            .expect("video tile picture");
        b.add_css_class("video");
        u.st.borrow_mut()
            .video_meta
            .insert(it.path.clone(), (it.name.clone(), it.size));
        frame.set_widget_name(&format!("{}|{}", it.date, it.path)); // read back by `info_sequence`
        let (u2, path) = (u.clone(), it.path.clone());
        let g = gtk::GestureClick::new();
        g.connect_released(move |_, _, _, _| open_media(&u2, &path, b.upcast_ref()));
        frame.add_controller(g);
    } else {
        frame.set_tooltip_text(Some(&format!(
            "{}\n{}\nClick to copy from the Mac and open",
            it.name,
            detail.join(" · ")
        )));
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
    } else if mime.starts_with("video/")
        || matches!(
            ext,
            "mov" | "mp4" | "m4v" | "3gp" | "avi" | "mkv" | "mpg" | "mpeg"
        )
    {
        FileKind::Video
    } else if mime.starts_with("audio/")
        || matches!(
            ext,
            "mp3" | "m4a" | "wav" | "caf" | "aac" | "amr" | "aiff" | "flac"
        )
    {
        FileKind::Audio
    } else if mime == "text/vcard" || ext == "vcf" {
        FileKind::Contact
    } else if mime.contains("spreadsheet")
        || mime.contains("excel")
        || matches!(ext, "xls" | "xlsx" | "csv" | "numbers" | "ods")
    {
        FileKind::Sheet
    } else if mime.contains("zip")
        || mime.contains("compressed")
        || matches!(ext, "zip" | "gz" | "tar" | "7z" | "rar")
    {
        FileKind::Archive
    } else if mime.contains("word")
        || mime.starts_with("text/")
        || matches!(ext, "doc" | "docx" | "txt" | "rtf" | "pages" | "odt" | "md")
    {
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
    let head = head
        .rfind(' ')
        .filter(|i| *i > head.len() / 2)
        .map_or(head.as_str(), |i| &head[..i])
        .trim_end_matches([' ', ',', '-', '–', '|', ':']);
    let tail = tail
        .find(' ')
        .filter(|i| *i < tail.len() / 2)
        .map_or(tail.as_str(), |i| &tail[i + 1..]);
    format!("{head}…{tail}")
}

/// "2:16", "1:02:03"
fn fmt_duration(secs: f64) -> String {
    let t = secs.round() as i64;
    let (h, m, s) = (t / 3600, t / 60 % 60, t % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// A file of a message, as the chat card needs it.
struct FileRef {
    path: String,
    name: String,
    kind: FileKind,
    size: i64,
    missing: bool,
}

const FILE_W: i32 = MEDIA_W;

/// The extra facts for a file, asked of the Mac once (the chat redraws when they arrive).
fn file_info_for(u: &Ui, f: &FileRef) -> Option<crate::mac::FileInfo> {
    if f.missing
        || f.path.is_empty()
        || !matches!(
            f.kind,
            FileKind::Video | FileKind::Audio | FileKind::Contact
        )
    {
        return None;
    }
    if let Some(i) = u.st.borrow().file_info.get(&f.path) {
        return i.clone();
    }
    u.st.borrow_mut().file_info.insert(f.path.clone(), None);
    u.send(Cmd::FileInfo {
        path: f.path.clone(),
        contact: f.kind == FileKind::Contact,
    });
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

    let thumb = f.kind.thumb()
        && !f.missing
        && !f.path.is_empty()
        && !u.st.borrow().bad_images.contains(&f.path);
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
        let item = crate::mac::MediaItem {
            path: f.path.clone(),
            name: f.name.clone(),
            ..Default::default()
        };
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
    let p = bubble::Bubble::photo(look, MEDIA_W, MEDIA_W * 3 / 4);
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
    let tip = [
        f.name.clone(),
        [f.kind.label().to_string(), fmt_size(f.size)]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" · "),
        "Click to play".into(),
    ];
    p.set_tooltip_text(Some(&tip.join("\n")));
    p.set_cursor_from_name(Some("pointer"));
    p.add_css_class("video");
    {
        let mut st = u.st.borrow_mut();
        st.video_meta
            .insert(f.path.clone(), (f.name.clone(), f.size));
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
    gtk::FileLauncher::new(Some(&gio::File::for_path(path))).launch(
        Some(&u.win),
        gio::Cancellable::NONE,
        |_| {},
    );
}

/// Copy a chat file from the Mac (once; kept under the cache folder) and open it with the default app.
// the copies are pruned at startup by `prune_old_dirs` in main.rs (30 days)
fn file_dest(u: &Ui, path: &str, name: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    files_dir(u)
        .join(format!("{:016x}", h.finish()))
        .join(safe_name(name, "file"))
}

/// A file name from a sender, made safe to use under a folder we choose: no `/` or control characters, and never
/// empty, `.` or `..` (those would point at the folder itself or above it); `fallback` stands in for those.
fn safe_name(name: &str, fallback: &str) -> String {
    let name: String = name
        .chars()
        .map(|c| if c == '/' || c.is_control() { '_' } else { c })
        .collect();
    match name.trim() {
        "" | "." | ".." => fallback.to_string(),
        _ => name,
    }
}

/// The viewer wants a video: use our copy, or copy it from the Mac (progress comes back as events).
fn fetch_video(u: &Rc<Ui>, path: &str) {
    if is_gif(path) {
        let dest = gif_dest(u, path);
        if whole_gif(&dest) {
            u.viewer.video_ready(path, dest);
        } else {
            queue_gif(u, path, dest, None);
        }
        return;
    }
    let (name, size) =
        u.st.borrow()
            .video_meta
            .get(path)
            .cloned()
            .unwrap_or_default();
    let dest = file_dest(u, path, &name);
    if dest.exists()
        && std::fs::metadata(&dest).is_ok_and(|m| size <= 0 || m.len() as i64 >= size * 98 / 100)
    {
        u.viewer.video_ready(path, dest);
    } else if u.st.borrow_mut().video_wait.insert(path.to_string()) {
        u.send(Cmd::Download {
            path: path.to_string(),
            dest,
            size,
        });
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
    u.st.borrow_mut()
        .downloading
        .insert(it.path.clone(), chip.downgrade());
    u.send(Cmd::Download {
        path: it.path.clone(),
        dest,
        size: it.size,
    });
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
        Some(i)
            if l.image_size
                .is_none_or(|(w, h)| crate::mac::is_preview_size(w, h)) =>
        {
            Some((i, true))
        }
        _ => small_picture(l).map(|i| (i, false)), // whole on the white tile
    });
    if let Some((img, big)) = image {
        let b = if big {
            bubble::Bubble::tile(THUMB)
        } else {
            bubble::Bubble::thumb(THUMB)
        };
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
    let host = url
        .split('/')
        .nth(2)
        .unwrap_or(url)
        .trim_start_matches("www.")
        .to_string();
    let caption = gtk::Label::new(Some(
        l.map(|l| l.site.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(&host),
    ));
    caption.add_css_class("tile-caption");
    caption.set_halign(gtk::Align::Fill);
    caption.set_valign(gtk::Align::End);
    caption.set_ellipsize(gtk::pango::EllipsizeMode::End);
    caption.set_max_width_chars(1); // a long name must not widen the tile
    caption.set_xalign(0.0);
    frame.add_overlay(&caption);
    frame.set_tooltip_text(Some(
        &match l.map(|l| l.title.as_str()).filter(|t| !t.is_empty()) {
            Some(t) => format!("{t}\n{url}"),
            None => url.to_string(),
        },
    ));
    frame.set_cursor_from_name(Some("pointer"));
    let (u2, url, f2) = (u.clone(), url.to_string(), frame.clone());
    let g = gtk::GestureClick::new();
    g.connect_released(move |_, _, _, _| {
        // a YouTube tile's slot (its parent) is named "<date>|<key>": the key in the panel's sequence
        let slot = f2.parent();
        let key = slot
            .as_ref()
            .map(|p| p.widget_name().to_string())
            .and_then(|n| n.split_once('|').map(|x| x.1.to_string()));
        open_link(
            &u2,
            &url,
            slot.as_ref().unwrap_or(f2.upcast_ref()),
            key.as_deref(),
        );
    });
    frame.add_controller(g);
    frame.upcast()
}

/// A link as a tile in a slot, with Messages' preview picture where it made one (asked for from the Mac).
fn link_slot(u: &Rc<Ui>, it: &MediaItem) -> gtk::Box {
    let slot = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let cached = if it.preview && it.row != 0 {
        u.st.borrow().links.get(&it.row).cloned()
    } else {
        None
    };
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
                    st.link_waiters
                        .entry(it.row)
                        .or_default()
                        .push(slot.downgrade());
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

// ---------- settings ----------

/// Our own tabs for a Stack (StackSwitcher's buttons measured far wider than their labels): a rounded track with
/// pill buttons, the open one filled. Used by the info panel and Settings.
fn tab_switcher(stack: &gtk::Stack, items: &[(&'static str, &str)]) -> gtk::Box {
    let switcher = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    switcher.add_css_class("bf-tabs");
    switcher.set_homogeneous(true); // equal widths, filling the row
    switcher.set_hexpand(true);
    let mut group: Option<gtk::ToggleButton> = None;
    let mut tabs = Vec::new();
    for (i, (name, label)) in items.iter().enumerate() {
        // the label, with an invisible bold copy in the same spot: the button is always as wide as its bold form, so
        // the active tab (bold) never changes the widths
        let t = gtk::ToggleButton::new();
        let both = gtk::Grid::new();
        both.set_valign(gtk::Align::Center);
        let shown = gtk::Label::new(Some(label));
        shown.set_hexpand(true);
        let twin = gtk::Label::builder()
            .accessible_role(gtk::AccessibleRole::Presentation)
            .opacity(0.0)
            .can_target(false)
            .hexpand(true)
            .build();
        twin.set_markup(&format!("<b>{}</b>", glib::markup_escape_text(label)));
        both.attach(&shown, 0, 0, 1, 1);
        both.attach(&twin, 0, 0, 1, 1);
        t.set_child(Some(&both));
        t.update_property(&[gtk::accessible::Property::Label(label)]);
        t.add_css_class("info-tab");
        t.set_hexpand(true);
        t.set_group(group.as_ref());
        t.set_active(i == 0);
        let (stack, name) = (stack.clone(), *name);
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
    switcher
}

/// Settings > About: name, version, licence and where to find the project.
fn about_tab() -> gtk::Frame {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 6);
    page.set_halign(gtk::Align::Center);
    for m in ["margin_top", "margin_bottom", "margin_start", "margin_end"] {
        page.set_property(m, 18);
    }
    let icon = gtk::Image::from_icon_name("bf-mark");
    icon.set_pixel_size(96);
    page.append(&icon);
    let name = wordmark();
    name.set_halign(gtk::Align::Center);
    page.append(&name);
    let line = |text: &str, dim: bool| {
        let l = gtk::Label::new(Some(text));
        l.set_wrap(true);
        l.set_justify(gtk::Justification::Center);
        l.set_max_width_chars(48);
        if dim {
            l.add_css_class("dim");
        }
        l
    };
    page.append(&line(
        &format!("Version {}", env!("CARGO_PKG_VERSION")),
        false,
    ));
    page.append(&line(
        "A Linux front end for Messages on your Mac. The Mac does the actual messaging.",
        true,
    ));
    page.append(&line(
        &format!(
            "© 2026 Peter Epp. Released under the {} licence.",
            env!("CARGO_PKG_LICENSE")
        ),
        true,
    ));
    let links = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    links.set_halign(gtk::Align::Center);
    links.set_margin_top(8);
    let repo = env!("CARGO_PKG_REPOSITORY");
    for (label, url) in [
        ("Project page", repo.to_string()),
        ("Report a problem", format!("{repo}/issues")),
        (
            "Known limitations",
            format!("{repo}/blob/main/docs/LIMITATIONS.md"),
        ),
    ] {
        links.append(&gtk::LinkButton::with_label(&url, label));
    }
    page.append(&links);
    let frame = gtk::Frame::new(None);
    frame.set_child(Some(&page));
    frame
}

/// Settings' second tab: where BubbleFairy connects, read only. Changing any of it is what the setup wizard is for.
fn mac_tab(u: &Rc<Ui>, win: &gtk::Window) -> gtk::Box {
    let cfg = u.st.borrow().cfg.clone();
    let page = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let mut in_card = None;
    if cfg.host.is_empty() {
        let l = gtk::Label::new(Some("Not set up yet."));
        l.set_xalign(0.0);
        page.append(&l);
    } else {
        let card = gtk::Box::new(gtk::Orientation::Vertical, 12);
        for m in ["margin_top", "margin_bottom", "margin_start", "margin_end"] {
            card.set_property(m, 14);
        }
        let grid = gtk::Grid::builder()
            .row_spacing(10)
            .column_spacing(14)
            .build();
        let hk = u.st.borrow().host_key.clone();
        let hk = if hk.is_empty() {
            std::fs::read_to_string(&u.store.known_host).unwrap_or_default()
        } else {
            hk
        };
        let ours = cfg.imsg.contains("/BubbleFairy");
        let mut rows = vec![
            ("Mac", cfg.host.clone()),
            ("Mac user", cfg.user.clone()),
            ("imsg", cfg.imsg.clone()),
        ];
        if ours {
            rows.push(("imsg version", crate::setup::IMSG_VERSION.to_string()));
        }
        if !hk.trim().is_empty() {
            rows.push(("Mac identity", crate::mac::fingerprint(hk.trim())));
        }
        for (i, (k, v)) in rows.iter().enumerate() {
            let l = gtk::Label::new(Some(k));
            l.set_xalign(1.0);
            l.set_yalign(0.0);
            l.add_css_class("dim");
            let val = gtk::Label::new(Some(v));
            val.set_xalign(0.0);
            val.set_wrap(true);
            val.set_wrap_mode(gtk::pango::WrapMode::Char);
            val.set_selectable(true);
            val.set_max_width_chars(44);
            val.set_hexpand(true);
            if *k == "Mac identity" || *k == "imsg" {
                val.add_css_class("monospace");
            }
            grid.attach(&l, 0, i as i32, 1, 1);
            grid.attach(&val, 1, i as i32, 1, 1);
        }
        card.append(&grid);
        let note = gtk::Label::new(Some(
            "The Mac identity is how BubbleFairy recognises your Mac. It is saved on the first connection, and BubbleFairy refuses to connect if it ever changes. To use a different Mac, or to fix the connection, run the setup again.",
        ));
        note.set_wrap(true);
        note.set_max_width_chars(58);
        note.set_xalign(0.0);
        note.add_css_class("dim");
        card.append(&note);
        let frame = gtk::Frame::new(None);
        frame.set_child(Some(&card));
        page.append(&frame);
        in_card = Some(card);
    }
    let redo = gtk::Button::with_label("Set up again…");
    redo.add_css_class("wiz-next");
    redo.set_halign(gtk::Align::Center);
    redo.set_margin_top(14);
    let (u2, w2) = (u.clone(), win.clone());
    redo.connect_clicked(move |_| {
        w2.close();
        start_setup(&u2, true);
    });
    // inside the box with the connection details when there are some, else just below the message
    in_card.as_ref().unwrap_or(&page).append(&redo);
    page
}

fn settings(u: &Rc<Ui>) {
    let w = gtk::Window::builder()
        .transient_for(&u.win)
        .modal(true)
        .title("Settings")
        .default_width(700)
        .build();
    w.set_application(u.win.application().as_ref()); // so the light/dark class reaches it, as for the other windows
    w.add_css_class("bf");
    w.add_css_class("bf-settings");
    let head = gtk::HeaderBar::new();
    head.add_css_class("bf-head");
    head.set_title_widget(Some(&brand()));
    w.set_titlebar(Some(&head));
    let cfg = u.st.borrow().cfg.clone();
    let (previews_row, previews, ..) = switch_row(
        "Show map previews on contact cards",
        "Uses OpenStreetMap: a contact's postal address is sent there to find it on the map. When off, cards show a plain button that opens the address in your maps app.",
    );
    previews.set_active(cfg.map_previews);
    let (links_row, links, ..) = switch_row(
        "Make previews for links Messages didn't",
        "A link with no preview shows its title and picture, read from the page itself, and a link straight to a picture shows that picture. This computer fetches the page, so the linked site sees your internet address. Off: such links stay a plain card with the site's name.",
    );
    links.set_active(cfg.link_previews);
    let pbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
    add_row(&pbox, &previews_row);
    add_row(&pbox, &links_row);
    let appearance = appearance_frame(u, &w);
    let privacy_frame = titled_frame("bf-privacy-symbolic", "Privacy");
    privacy_frame.set_child(Some(&pbox));

    // default download folders: empty = the download buttons ask every time
    let dl = gtk::Box::new(gtk::Orientation::Vertical, 0);
    // The file chooser hands a Flatpak app a portal path (/run/user/1000/doc/<id>/Pictures); that is what
    // saving needs, but the field shows the real folder, which the portal keeps in an attribute.
    let shown = |path: &str| {
        let info = gio::File::for_path(path)
            .query_info(
                "xattr::document-portal.host-path",
                gio::FileQueryInfoFlags::NONE,
                gio::Cancellable::NONE,
            )
            .ok();
        info.and_then(|i| i.attribute_string("xattr::document-portal.host-path"))
            .map(|s| s.to_string())
            .unwrap_or_else(|| path.to_string())
    };
    let folder_row = |label: &str, value: &str, set: fn(&mut Config, String)| {
        let l = gtk::Label::new(Some(label));
        l.set_xalign(0.0);
        l.set_width_chars(8);
        let path = Rc::new(RefCell::new(value.to_string()));
        let e = gtk::Entry::builder()
            .text(if value.is_empty() {
                String::new()
            } else {
                shown(value)
            })
            .editable(false)
            .can_focus(false)
            .hexpand(true)
            .placeholder_text("Ask every time")
            .build();
        let choose = gtk::Button::with_label("Choose…");
        let clear = gtk::Button::with_label("Clear");
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        for w in [
            l.upcast_ref::<gtk::Widget>(),
            e.upcast_ref(),
            choose.upcast_ref(),
            clear.upcast_ref(),
        ] {
            row.append(w);
        }
        let (e2, p2, win, label) = (e.clone(), path.clone(), u.win.clone(), label.to_lowercase());
        choose.connect_clicked(move |_| {
            let d = gtk::FileDialog::builder()
                .title(format!("Default folder for {label}"))
                .modal(true)
                .build();
            if !p2.borrow().is_empty() {
                d.set_initial_folder(Some(&gio::File::for_path(p2.borrow().as_str())));
            }
            let (e3, p3) = (e2.clone(), p2.clone());
            d.select_folder(Some(&win), gio::Cancellable::NONE, move |r| {
                if let Some(p) = r.ok().and_then(|f| f.path()) {
                    let p = p.to_string_lossy().to_string();
                    let text = shown(&p);
                    *p3.borrow_mut() = p;
                    e3.set_text(&text);
                }
            });
        });
        let e2 = e.clone();
        let p2 = path.clone();
        clear.connect_clicked(move |_| {
            p2.borrow_mut().clear();
            e2.set_text("");
        });
        let u = u.clone();
        e.connect_changed(move |_| change_setting(&u, |c| set(c, path.borrow().clone())));
        row
    };
    let images_row = folder_row("Pictures", &cfg.dir_images, |c, v| c.dir_images = v);
    let files_row = folder_row("Files", &cfg.dir_files, |c, v| c.dir_files = v);
    let dl_help = help_label(
        "The download button next to a picture or file saves it to the folder set here. With no folder set, it opens a save dialog every time so you choose where it goes.",
    );
    add_row(&dl, &images_row);
    add_row(&dl, &files_row);
    let dl_frame = titled_frame("bf-download-symbolic", "Default download folders");
    dl_frame.set_child(Some(&dl));

    // notifications: whether, with a sound (which), and what they show
    let nb = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let (notify_on_row, notify_on, ..) = switch_row("Show a notification for a new message", "");
    notify_on.set_active(cfg.notify);
    let (sound_on_row, sound_on, ..) = switch_row("Play a sound", "");
    sound_on.set_active(cfg.notify_sound);
    // the sound: the bundled ones, then "Custom file…" (which shows the chosen file's name once there is one)
    let custom: Rc<RefCell<String>> = Rc::new(RefCell::new(
        cfg.sound.strip_prefix("file:").unwrap_or("").to_string(),
    ));
    let sounds = gtk::StringList::new(&crate::sound::BUNDLED.map(|(_, n)| n));
    let custom_label = |c: &str| {
        if c.is_empty() {
            "Custom file…".to_string()
        } else {
            format!(
                "Custom: {}",
                std::path::Path::new(c)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default()
            )
        }
    };
    sounds.append(&custom_label(&custom.borrow()));
    let picker = gtk::DropDown::new(Some(sounds.clone()), gtk::Expression::NONE);
    picker.set_hexpand(true);
    let current = crate::sound::BUNDLED
        .iter()
        .position(|(n, _)| *n == cfg.sound)
        .unwrap_or(if custom.borrow().is_empty() {
            0
        } else {
            crate::sound::BUNDLED.len()
        });
    picker.set_selected(current as u32);
    let chosen: Rc<RefCell<String>> = Rc::new(RefCell::new(cfg.sound.clone()));
    let listen = gtk::Button::from_icon_name("bf-listen-symbolic");
    listen.set_tooltip_text(Some("Listen"));
    let (chosen2, listen2) = (chosen.clone(), listen.clone());
    listen2.connect_clicked(move |_| crate::sound::play(&chosen2.borrow()));
    let last_ok = Rc::new(Cell::new(current as u32));
    {
        let u = u.clone();
        let (win, custom, chosen, sounds, last_ok) = (
            w.clone(),
            custom.clone(),
            chosen.clone(),
            sounds.clone(),
            last_ok.clone(),
        );
        picker.connect_selected_notify(move |p| {
            let i = p.selected() as usize;
            if i < crate::sound::BUNDLED.len() {
                *chosen.borrow_mut() = crate::sound::BUNDLED[i].0.to_string();
                last_ok.set(i as u32);
                crate::sound::play(&chosen.borrow()); // hear it as you choose it
            } else if !custom.borrow().is_empty() && last_ok.get() as usize == i {
                *chosen.borrow_mut() = format!("file:{}", custom.borrow());
            } else {
                // "Custom file…": ask for the file; cancelling puts the previous choice back
                let filter = gtk::FileFilter::new();
                filter.set_name(Some("Sounds"));
                filter.add_mime_type("audio/*");
                let filters = gio::ListStore::new::<gtk::FileFilter>();
                filters.append(&filter);
                let d = gtk::FileDialog::builder()
                    .title("Notification sound")
                    .modal(true)
                    .filters(&filters)
                    .build();
                let u = u.clone();
                let (p2, custom, chosen, sounds, last_ok) = (
                    p.clone(),
                    custom.clone(),
                    chosen.clone(),
                    sounds.clone(),
                    last_ok.clone(),
                );
                d.open(Some(&win), gio::Cancellable::NONE, move |r| {
                    match r.ok().and_then(|f| f.path()) {
                        Some(path) => {
                            let path = path.to_string_lossy().to_string();
                            *custom.borrow_mut() = path.clone();
                            sounds.splice(
                                crate::sound::BUNDLED.len() as u32,
                                1,
                                &[&custom_label(&path)],
                            );
                            *chosen.borrow_mut() = format!("file:{path}");
                            change_setting(&u, |c| c.sound = format!("file:{path}"));
                            last_ok.set(crate::sound::BUNDLED.len() as u32);
                            p2.set_selected(crate::sound::BUNDLED.len() as u32);
                            crate::sound::play(&chosen.borrow());
                        }
                        None => p2.set_selected(last_ok.get()),
                    }
                });
            }
        });
    }
    let sound_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let sound_label = gtk::Label::new(Some("Sound"));
    sound_label.set_width_chars(8);
    sound_label.set_xalign(0.0);
    sound_row.append(&sound_label);
    sound_row.append(&picker);
    sound_row.append(&listen);
    let text_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let text_label = gtk::Label::new(Some("Shows"));
    text_label.set_width_chars(8);
    text_label.set_xalign(0.0);
    let text_pick = gtk::DropDown::from_strings(&[
        "Sender and message",
        "Sender only",
        "Nothing about the message",
    ]);
    text_pick.set_hexpand(true);
    text_pick.set_selected(u32::from(cfg.notify_text.min(2)));
    text_row.append(&text_label);
    text_row.append(&text_pick);
    let notify_help = help_label(
        "Notifications follow your desktop's own settings (do not disturb, per-app controls). The sound is played by BubbleFairy itself, and stays quiet while the desktop is in do-not-disturb. A message in the chat you are looking at makes neither.",
    );
    for row in [
        notify_on_row.upcast_ref::<gtk::Widget>(),
        sound_on_row.upcast_ref(),
        sound_row.upcast_ref(),
        text_row.upcast_ref(),
    ] {
        add_row(&nb, row);
    }
    let notify_frame = titled_frame("bf-notifications-symbolic", "Notifications");
    notify_frame.set_child(Some(&nb));

    let (auto_on_row, auto_on, auto_label, tray_help) =
        switch_row("Start hidden in the tray when I log in", "");
    auto_on.set_active(cfg.autostart);
    let th = tray_help.clone();
    let say = move |found: bool| {
        auto_label.set_text(if found {
            "Start hidden in the tray when I log in"
        } else {
            "Start minimized when I log in"
        });
        th.set_visible(true);
        th.set_text(if found {
            "BubbleFairy keeps an icon in the system tray. Closing the window keeps it running there, so notifications keep coming, and the icon shows a dot while a chat is unread. Quit from the icon's menu or with Ctrl+Q."
        } else {
            "I couldn't find a system tray on this desktop (KDE Plasma has one, GNOME needs the AppIndicator extension), so closing the window quits the app. Starting at login opens the window minimized instead, though you may see it for a moment first."
        });
    };
    // filled in straight away from the tray we have (or haven't), so the window is sized to the real text
    say(u.tray.borrow().is_some());
    glib::MainContext::default().spawn_local(async move { say(crate::tray::host_present().await) });
    let bg = gtk::Box::new(gtk::Orientation::Vertical, 0);
    add_row(&bg, &auto_on_row);
    let bg_frame = titled_frame("bf-background-symbolic", "Background");
    bg_frame.set_child(Some(&bg));

    // a local copy of the chat list and contacts, so the next start shows them while the Mac is being reached
    let (cache_on_row, cache_on, ..) = switch_row(
        "Keep a local copy of my chats and contacts for a faster start",
        "The chat list, the last message of each chat and your contacts (names, numbers and photos) are saved on this computer. The next start shows them straight away and swaps in what the Mac sends as soon as it answers. Turning this off deletes the copy.",
    );
    cache_on.set_active(cfg.local_cache);
    let cache_clear = gtk::Button::with_label("Clear saved copy");
    cache_clear.set_halign(gtk::Align::Start);
    let u2 = u.clone();
    cache_clear.connect_clicked(move |_| clear_snapshot(&u2));
    let cache_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    add_row(&cache_box, &cache_on_row);
    let clear_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    clear_row.append(&cache_clear);
    add_row(&cache_box, &clear_row);
    let cache_frame = titled_frame("bf-bolt-symbolic", "Faster start");
    cache_frame.set_child(Some(&cache_box));

    // every change applies at once
    let check = |b: &gtk::Switch, set: fn(&mut Config, bool)| {
        let u = u.clone();
        b.connect_active_notify(move |b| change_setting(&u, |c| set(c, b.is_active())));
    };
    check(&previews, |c, on| c.map_previews = on);
    check(&links, |c, on| c.link_previews = on);
    check(&notify_on, |c, on| c.notify = on);
    check(&sound_on, |c, on| c.notify_sound = on);
    check(&cache_on, |c, on| c.local_cache = on);
    let u2 = u.clone();
    cache_on.connect_active_notify(move |b| {
        if !b.is_active() {
            clear_snapshot(&u2);
        }
    });
    let u2 = u.clone();
    auto_on.connect_active_notify(move |b| change_autostart(&u2, b));
    let (u2, chosen2) = (u.clone(), chosen.clone());
    // connected after the picker's own handler, so `chosen` is already the new one
    picker.connect_selected_notify(move |_| {
        change_setting(&u2, |c| c.sound = chosen2.borrow().clone());
    });
    let u2 = u.clone();
    text_pick.connect_selected_notify(move |p| {
        change_setting(&u2, |c| c.notify_text = p.selected() as u8);
    });

    // the window is as tall as its tallest part, never taller than the screen (a section then scrolls)
    let max_h = u
        .win
        .surface()
        .and_then(|s| gdk::Display::default()?.monitor_at_surface(&s))
        .map_or(600, |m| m.geometry().height() * 80 / 100);
    let pages = gtk::Stack::new();
    pages.set_hexpand(true);
    let nav = gtk::ListBox::new();
    nav.add_css_class("chat-list");
    nav.add_css_class("settings-nav");
    let add = |name: &str, icon: &str, title: &str, content: &gtk::Widget| {
        pages.add_named(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .propagate_natural_height(true)
                .max_content_height(max_h)
                .child(content)
                .build(),
            Some(name),
        );
        let (head, label) = icon_label(icon, title);
        label.add_css_class("chat-name");
        let row = gtk::ListBoxRow::new();
        row.add_css_class("chat-row");
        row.set_widget_name(name);
        row.set_child(Some(&head));
        nav.append(&row);
    };
    // the same edges for every section, level with the sidebar panel's
    let section = |parts: &[&gtk::Widget]| -> gtk::Widget {
        let b = gtk::Box::new(gtk::Orientation::Vertical, 12);
        b.set_valign(gtk::Align::Start);
        b.set_margin_top(2);
        b.set_margin_bottom(14);
        b.set_margin_end(14);
        for p in parts {
            b.append(*p);
        }
        b.upcast()
    };
    add(
        "appearance",
        "bf-appearance-symbolic",
        "Appearance",
        &section(&[appearance.upcast_ref()]),
    );
    add(
        "notifications",
        "bf-notifications-symbolic",
        "Notifications",
        &section(&[notify_frame.upcast_ref(), notify_help.upcast_ref()]),
    );
    add(
        "downloads",
        "bf-download-symbolic",
        "Downloads",
        &section(&[dl_frame.upcast_ref(), dl_help.upcast_ref()]),
    );
    add(
        "general",
        "bf-general-symbolic",
        "General",
        &section(&[bg_frame.upcast_ref(), cache_frame.upcast_ref()]),
    );
    add(
        "privacy",
        "bf-privacy-symbolic",
        "Privacy",
        &section(&[privacy_frame.upcast_ref()]),
    );
    add(
        "mac",
        "bf-computer-symbolic",
        "Mac connection",
        &section(&[mac_tab(u, &w).upcast_ref()]),
    );
    add(
        "about",
        "bf-info-symbolic",
        "About",
        &section(&[about_tab().upcast_ref()]),
    );
    let stack = pages.clone();
    nav.connect_row_selected(move |_, row| {
        if let Some(row) = row {
            stack.set_visible_child_name(&row.widget_name());
        }
    });
    // BF_SETTINGS_TAB picks the section to open on
    let wanted = std::env::var("BF_SETTINGS_TAB").unwrap_or_default();
    let first = std::iter::successors(nav.row_at_index(0), |r| nav.row_at_index(r.index() + 1))
        .find(|r| r.widget_name() == wanted.as_str());
    nav.select_row(first.or_else(|| nav.row_at_index(0)).as_ref());
    let side = gtk::Box::new(gtk::Orientation::Vertical, 0);
    side.add_css_class("sidebar");
    side.set_size_request(170, -1);
    side.append(&nav);
    let root = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    root.append(&side);
    root.append(&pages);
    w.set_child(Some(&root));
    w.present();
    if let Some(out) = std::env::var_os("BF_SETTINGS_SHOT") {
        let w = w.clone();
        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
            let p = gtk::WidgetPaintable::new(Some(&w));
            let snap = gtk::Snapshot::new();
            p.snapshot(&snap, w.width() as f64, w.height() as f64);
            if let (Some(node), Some(r)) = (snap.to_node(), w.renderer()) {
                let _ = r.render_texture(&node, None).save_to_png(&out);
            }
            if let Some(app) = w.application() {
                app.quit();
            }
        });
    }
}

/// Recolour the app: the CSS, then everything that draws itself.
fn apply_theme(u: &Ui, light: Adjust, dark: Adjust) {
    fn redraw(w: &gtk::Widget) {
        w.queue_draw();
        let mut c = w.first_child();
        while let Some(child) = c {
            redraw(&child);
            c = child.next_sibling();
        }
    }
    crate::theme::apply(light, dark);
    redraw(u.win.upcast_ref());
}

/// Light, dark (`Config.color_scheme`) or whatever the system says.
fn set_scheme(mode: u8) {
    let Some(s) = gtk::Settings::default() else {
        return;
    };
    match mode {
        1 => s.set_gtk_interface_color_scheme(gtk::InterfaceColorScheme::Light),
        2 => s.set_gtk_interface_color_scheme(gtk::InterfaceColorScheme::Dark),
        _ => s.reset_property("gtk-interface-color-scheme"), // the next read asks the system again
    }
}

fn is_dark_scheme(s: &gtk::Settings) -> bool {
    s.gtk_interface_color_scheme() == gtk::InterfaceColorScheme::Dark
}

/// A small wrapped explanation under a setting or a Settings box.
fn help_label(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_wrap(true);
    l.set_max_width_chars(58);
    l.set_xalign(0.0);
    l.add_css_class("set-help");
    l
}

/// Add a row to a Settings box, with a divider after the row before it.
fn add_row(list: &gtk::Box, row: &impl IsA<gtk::Widget>) {
    if list.first_child().is_some() {
        let line = gtk::Separator::new(gtk::Orientation::Horizontal);
        line.add_css_class("set-sep");
        list.append(&line);
    }
    row.add_css_class("set-row");
    list.append(row);
}

/// An on/off setting: its name with a slide toggle at the right end, level with it, and what it does
/// underneath if `help` says.
/// Clicking anywhere on the row toggles it. Returns the row, the toggle, the name and the help.
pub(crate) fn switch_row(
    text: &str,
    help: &str,
) -> (gtk::Grid, gtk::Switch, gtk::Label, gtk::Label) {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    let more = help_label(help);
    more.set_visible(!help.is_empty());
    label.set_hexpand(true);
    let switch = gtk::Switch::new();
    switch.set_valign(gtk::Align::Center);
    switch.update_relation(&[gtk::accessible::Relation::LabelledBy(&[label.upcast_ref()])]);
    // the toggle shares the name's line only, so it stays at the top however long the help runs
    let row = gtk::Grid::builder()
        .column_spacing(16)
        .row_spacing(3)
        .build();
    row.add_css_class("toggle-row");
    row.attach(&label, 0, 0, 1, 1);
    row.attach(&switch, 1, 0, 1, 1);
    row.attach(&more, 0, 1, 1, 1);
    let click = gtk::GestureClick::new();
    let (s, r) = (switch.downgrade(), row.downgrade());
    click.connect_released(move |_, _, x, y| {
        let (Some(s), Some(r)) = (s.upgrade(), r.upgrade()) else {
            return;
        };
        // the toggle handles its own clicks
        let on_switch = r
            .pick(x, y, gtk::PickFlags::DEFAULT)
            .is_some_and(|w| w == *s.upcast_ref::<gtk::Widget>() || w.is_ancestor(&s));
        if !on_switch && s.is_sensitive() && r.contains(x, y) {
            s.set_active(!s.is_active());
        }
    });
    row.add_controller(click);
    (row, switch, label, more)
}

/// An icon and a name, side by side.
fn icon_label(icon: &str, text: &str) -> (gtk::Box, gtk::Label) {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    b.append(&gtk::Image::from_icon_name(icon));
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    b.append(&l);
    (b, l)
}

/// A Settings box with its icon and title.
fn titled_frame(icon: &str, title: &str) -> gtk::Frame {
    let frame = gtk::Frame::new(None);
    let (head, _) = icon_label(icon, title);
    head.add_css_class("box-title");
    frame.set_label_widget(Some(&head));
    frame
}

/// Save one change from the Settings window and put it into effect.
fn change_setting(u: &Ui, f: impl FnOnce(&mut Config)) {
    let cfg = {
        let mut st = u.st.borrow_mut();
        f(&mut st.cfg);
        st.cfg.clone()
    };
    crate::save_config(&u.store.config_file, &cfg);
    u.send(Cmd::Configure(cfg)); // the connection is only redone if where we connect to changed
}

/// Make or remove the login entry; if the desktop refuses, put the toggle and the setting back.
fn change_autostart(u: &Rc<Ui>, check: &gtk::Switch) {
    let want = check.is_active();
    if u.st.borrow().cfg.autostart == want {
        return;
    }
    change_setting(u, |c| c.autostart = want);
    let Some(rt) = crate::setup::RT.get() else {
        return;
    };
    let (tx, rx) = tokio::sync::oneshot::channel();
    rt.spawn(async move {
        let _ = tx.send(crate::tray::set_autostart(want).await);
    });
    let (u, check) = (u.clone(), check.clone());
    glib::spawn_future_local(async move {
        if rx.await == Ok(false) {
            // the desktop didn't make (or remove) the login entry: don't pretend it did
            change_setting(&u, |c| c.autostart = !want);
            check.set_active(!want);
            show_toast(
                &u,
                if want {
                    "Couldn't set up starting at login"
                } else {
                    "Couldn't remove the login entry"
                },
            );
        }
    });
}

/// Settings' appearance box: light / dark / system, and the theme sliders for whichever of light and
/// dark is showing. Everything recolours the app and is saved as it changes.
fn appearance_frame(u: &Rc<Ui>, win: &gtk::Window) -> gtk::Frame {
    let (scheme, colours) = {
        let st = u.st.borrow();
        (st.cfg.color_scheme, [st.cfg.theme_light, st.cfg.theme_dark])
    };
    let colours = Rc::new(Cell::new(colours));
    // a slider drag changes the value many times a second: the file is written once it settles
    let save_queued = Rc::new(Cell::new(false));
    let rows = gtk::Box::new(gtk::Orientation::Vertical, 0);

    let schemes = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    schemes.add_css_class("bf-tabs");
    schemes.set_homogeneous(true);
    schemes.set_halign(gtk::Align::Center);
    let mut group: Option<gtk::ToggleButton> = None;
    for (mode, name) in [(0, "System"), (1, "Light"), (2, "Dark")] {
        let t = gtk::ToggleButton::with_label(name);
        t.add_css_class("info-tab");
        t.set_group(group.as_ref());
        t.set_active(scheme == mode);
        let u = u.clone();
        t.connect_toggled(move |t| {
            if t.is_active() {
                set_scheme(mode);
                change_setting(&u, |c| c.color_scheme = mode);
            }
        });
        schemes.append(&t);
        group.get_or_insert(t);
    }
    // in a box of its own: the row's padding would otherwise swell the control's track
    let scheme_row = gtk::Box::new(gtk::Orientation::Vertical, 0);
    scheme_row.append(&schemes);
    add_row(&rows, &scheme_row);

    let Some(settings) = gtk::Settings::default() else {
        return gtk::Frame::new(None);
    };
    type Field = fn(&mut Adjust) -> &mut i32;
    let sliders: [(&str, i32, Field); 4] = [
        ("Tint", TINT_MAX, |a| &mut a.tint),
        ("Saturation", LEVEL_MAX, |a| &mut a.saturation),
        ("Brightness", LEVEL_MAX, |a| &mut a.brightness),
        ("Bubbliness", LEVEL_MAX, |a| &mut a.bubbliness),
    ];
    let scales = sliders.map(|(name, max, field)| {
        let label = gtk::Label::new(Some(name));
        label.set_width_chars(10);
        label.set_xalign(0.0);
        let max = f64::from(max);
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, -max, max, 1.0);
        scale.set_draw_value(false);
        scale.set_hexpand(true);
        scale.update_property(&[gtk::accessible::Property::Label(name)]);
        let (u, colours, settings) = (u.clone(), colours.clone(), settings.clone());
        let save_queued = save_queued.clone();
        scale.connect_value_changed(move |s| {
            let mut c = colours.get();
            let value = s.value().round() as i32;
            let slot = field(&mut c[usize::from(is_dark_scheme(&settings))]);
            if *slot == value {
                return; // the sliders being filled in
            }
            *slot = value;
            colours.set(c);
            {
                let mut st = u.st.borrow_mut();
                (st.cfg.theme_light, st.cfg.theme_dark) = (c[0], c[1]);
            }
            apply_theme(&u, c[0], c[1]);
            if !save_queued.replace(true) {
                let (u, save_queued) = (u.clone(), save_queued.clone());
                glib::timeout_add_local_once(Duration::from_millis(500), move || {
                    save_queued.set(false);
                    crate::save_config(&u.store.config_file, &u.st.borrow().cfg);
                });
            }
        });
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.append(&label);
        row.append(&scale);
        add_row(&rows, &row);
        (scale, field)
    });
    // put the showing scheme's values on the sliders, now and whenever it changes
    let show = {
        let (scales, colours) = (scales.clone(), colours.clone());
        move |s: &gtk::Settings| {
            let dark = is_dark_scheme(s);
            let mut a = colours.get()[usize::from(dark)];
            for (scale, field) in &scales {
                scale.set_value(f64::from(*field(&mut a)));
            }
        }
    };
    show(&settings);
    let watch = Cell::new(Some(
        settings.connect_gtk_interface_color_scheme_notify(show),
    ));

    let reset = gtk::Button::with_label("Restore Defaults");
    reset.set_halign(gtk::Align::Start);
    reset.connect_clicked(move |_| {
        for (s, _) in &scales {
            s.set_value(0.0);
        }
    });
    let reset_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    reset_row.append(&reset);
    add_row(&rows, &reset_row);

    win.connect_close_request(move |_| {
        if let Some(id) = watch.take() {
            settings.disconnect(id);
        }
        glib::Propagation::Proceed
    });
    let frame = titled_frame("bf-appearance-symbolic", "Appearance");
    frame.set_child(Some(&rows));
    frame
}

// ---------- helpers ----------

/// Some RCS attachments come without a MIME type, so fall back to the file extension.
fn is_image(mime: &str, path: &str) -> bool {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    mime.starts_with("image/")
        || (mime.is_empty()
            && matches!(
                ext.as_str(),
                "jpg" | "jpeg" | "png" | "heic" | "heif" | "gif" | "webp"
            ))
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
    // a group without a name: its people by first name, "Jo & Kari", "Jo, Rob & Drayven" (numbers stay whole)
    // (people with a contact name first, then numbers and emails, each in the order they come)
    let handles: Vec<&str> = c["participants"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p.as_str())
        .collect();
    if let [one] = handles.as_slice()
        && let Some(n) = names.get(*one)
    {
        return n.clone(); // one person: the full name
    }
    let mut people: Vec<String> = handles
        .iter()
        .filter_map(|p| names.get(*p).map(|n| first_name(n)))
        .collect();
    people.extend(
        handles
            .iter()
            .filter(|p| !names.contains_key(**p))
            .map(|p| pretty_handle(p)),
    );
    match people.as_slice() {
        [] => {}
        [one] => return one.clone(),
        [init @ .., last] => return format!("{} & {last}", init.join(", ")),
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

/// `s` cut to about `max` characters at a word boundary, with an ellipsis.
fn clip_words(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    let cut = cut
        .rsplit_once(char::is_whitespace)
        .map_or(cut.as_str(), |(a, _)| a);
    format!("{}…", cut.trim_end())
}

/// The line Messages shows for a group event, like "Jo Harris left the conversation." (None for kinds we don't word).
fn group_event(m: &Value, names: &HashMap<String, String>) -> Option<String> {
    let item = m["item_type"].as_i64().filter(|i| *i != 0)?;
    let e = &m["event"];
    let name = |h: &str| names.get(h).cloned().unwrap_or_else(|| pretty_handle(h));
    let me = m["is_from_me"].as_bool() == Some(true);
    let who = e["who"].as_str().filter(|s| !s.is_empty()).map(name);
    let whom = e["whom"].as_str().filter(|s| !s.is_empty()).map(name);
    let actor = if me {
        Some("You".to_string())
    } else {
        who.clone()
    };
    match (item, e["action"].as_i64().unwrap_or(0)) {
        (3, _) => Some(format!(
            "{} left the conversation.",
            if me { "You".to_string() } else { who.or(whom)? }
        )),
        (1, 1) => match (actor, whom) {
            (Some(a), Some(w)) if a != w => Some(format!("{a} removed {w} from the conversation.")),
            (_, Some(w)) => Some(format!("{w} left the conversation.")),
            (Some(a), None) => Some(format!("{a} left the conversation.")),
            _ => None,
        },
        (1, 0) => match (actor, whom) {
            (Some(a), Some(w)) => Some(format!("{a} added {w} to the conversation.")),
            (None, Some(w)) => Some(format!("{w} joined the conversation.")),
            _ => None,
        },
        (2, _) => {
            let a = actor.unwrap_or_else(|| "Someone".to_string());
            match e["title"].as_str().filter(|t| !t.is_empty()) {
                Some(t) => Some(format!("{a} named the conversation \u{201c}{t}\u{201d}.")),
                None => Some(format!("{a} removed the conversation name.")),
            }
        }
        _ => None,
    }
}

/// "Jo Harris" -> "Jo".
fn first_name(name: &str) -> String {
    name.split_whitespace().next().unwrap_or(name).to_string()
}

/// The header title of a group with more than a handful of people and no name: a few first names, then how many
/// more there are. Numbers and emails are never listed (they count among the others); with no names at all it is
/// just the number of people.
fn big_group_title(c: &Value, names: &HashMap<String, String>) -> String {
    let handles: Vec<&str> = c["participants"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p.as_str())
        .collect();
    let known: Vec<String> = handles
        .iter()
        .filter_map(|p| names.get(*p).map(|n| first_name(n)))
        .take(3)
        .collect();
    if known.is_empty() {
        return format!("Chat with {} People", handles.len());
    }
    let others = handles.len() - known.len();
    let others = if others == 1 {
        "1 other".to_string()
    } else {
        format!("{others} others")
    };
    match known.as_slice() {
        [one] => format!("{one} & {others}"),
        _ => format!("{} & {others}", known.join(", ")),
    }
}

/// People with a contact name first, the rest after them, each group in the order it came.
fn known_first(members: &[String], names: &HashMap<String, String>) -> Vec<String> {
    let mut v: Vec<String> = members
        .iter()
        .filter(|m| names.contains_key(*m))
        .cloned()
        .collect();
    v.extend(members.iter().filter(|m| !names.contains_key(*m)).cloned());
    v
}

fn clean_text(m: &Value) -> String {
    m["text"]
        .as_str()
        .unwrap_or("")
        .replace(OBJ, "")
        .trim()
        .to_string()
}

/// One-line description of a message, for the chat list and notifications.
fn summary(m: &Value) -> String {
    let t = clean_text(m);
    if !t.is_empty() {
        return t.lines().next().unwrap_or("").to_string();
    }
    let atts = m["attachments"].as_array().map(|a| a.len()).unwrap_or(0);
    let image = m["attachments"][0]["mime_type"]
        .as_str()
        .is_some_and(|s| s.starts_with("image/"));
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
    name.split_whitespace()
        .filter_map(|w| w.chars().find(|c| c.is_alphanumeric()))
        .take(2)
        .flat_map(char::to_uppercase)
        .collect()
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
    let t = st
        .contacts
        .as_ref()?
        .photo(handle)
        .map(<[u8]>::to_vec)
        .and_then(decode_avatar);
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
            for h in c["participants"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p.as_str())
            {
                let Some(key) = crate::mac::addr_key(h) else {
                    continue;
                };
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
                .map(|(key, bytes)| (key, bytes.and_then(decode_avatar)))
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
                refresh_identities(&u2);
                // the open chat: its header avatar only, unless it is a group (senders' avatars are in the messages)
                let open = {
                    let st = u2.st.borrow();
                    st.sel.and_then(|id| {
                        st.chats
                            .iter()
                            .find(|c| c["id"].as_i64() == Some(id))
                            .cloned()
                    })
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
    let w = initials_avatar(name, if small { 28 } else { 44 });
    if small {
        w.add_css_class("small");
    }
    w
}

/// Initials on a colour, or a generic person for numbers and other names with no letters.
fn initials_avatar(name: &str, d: i32) -> gtk::Widget {
    let hue = name
        .bytes()
        .fold(0u32, |a, c| a.wrapping_mul(31).wrapping_add(c as u32))
        % 8;
    let w: gtk::Widget = if name.chars().next().is_some_and(char::is_alphabetic) {
        let l = gtk::Label::new(Some(&initials(name)));
        l.set_halign(gtk::Align::Center);
        l.upcast()
    } else {
        let img = gtk::Image::from_icon_name("bf-contact-symbolic"); // ours: themes differ
        img.set_pixel_size(d * 3 / 5);
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
        initials_avatar(name, d)
    };
    w.add_css_class("avatar");
    w.add_css_class("mini");
    w.set_size_request(d, d);
    w
}

/// "+3": how many more people than there is room for.
fn more_pill(n: usize, d: i32) -> gtk::Widget {
    let text = format!("+{n}");
    let l = gtk::Label::new(Some(&text));
    l.add_css_class("avatar");
    l.add_css_class("mini");
    l.add_css_class("more");
    l.set_size_request(more_width(n, d), d); // a pill, not a squeezed circle, for two digits or more
    l.upcast()
}

/// How wide the "+N" pill is for a circle size `d`.
fn more_width(n: usize, d: i32) -> i32 {
    d + (n.to_string().len() as i32 - 1) * d / 3
}

/// A group's avatar, like Messages: a disc with the other members' small avatars inside it. With
/// more than four people it shows three and a "+N" circle for the rest.
fn group_avatar(
    u: &Ui,
    title: &str,
    members: &[String],
    names: &HashMap<String, String>,
) -> gtk::Widget {
    let members = &known_first(members, names)[..];
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
        let w = more_width(n - 3, d);
        fixed.put(&more_pill(n - 3, d), (x - (w - d)).max(0) as f64, y as f64); // grows to the left, inside the avatar
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
fn stacked_avatars(
    u: &Ui,
    title: &str,
    members: &[String],
    names: &HashMap<String, String>,
    open_card: OpenCard,
) -> gtk::Widget {
    let members = &known_first(members, names)[..];
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
                    mini_avatar(
                        u,
                        &names
                            .get(&members[i])
                            .cloned()
                            .unwrap_or_else(|| pretty_handle(&members[i])),
                        &members[i],
                        d,
                    )
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
            let mut order: Vec<usize> = (0..ws.len())
                .rev()
                .filter(|i| Some(*i) != hv && Some(*i) != op)
                .collect();
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
            let inside =
                |i: usize| x >= (i as i32 * step) as f64 && x < (i as i32 * step + d) as f64;
            [hover.get(), open.get()]
                .into_iter()
                .flatten()
                .find(|i| inside(*i))
                .or_else(|| (0..slots).find(|i| inside(*i)))
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
            let name = names
                .get(&members[i])
                .cloned()
                .unwrap_or_else(|| pretty_handle(&members[i]));
            let (open, restack) = (open.clone(), restack.clone());
            open_card(
                &ws2[i],
                &members[i],
                &name,
                Rc::new(move |on| {
                    open.set(on.then_some(i));
                    restack();
                }),
            );
        }
    });
    o.add_controller(g);
    o.upcast()
}

/// The avatar for a chat in the list: the member circles for a group, the person's own for a 1:1 chat.
fn chat_avatar(u: &Ui, chat: &Value, title: &str) -> gtk::Widget {
    if chat["is_group"].as_bool() == Some(true) {
        let members: Vec<String> = chat["participants"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p.as_str().map(String::from))
            .collect();
        let names = u.st.borrow().names.clone();
        group_avatar(u, title, &members, &names)
    } else {
        avatar(u, title, direct_addr(chat), false)
    }
}

// ---------- contact card ----------

/// A popover in the app's style (`issues`), hung under `anchor` and parented to the window, plus the height left
/// below the anchor (inside the window and the monitor) for its content.
fn anchored_popover(u: &Rc<Ui>, anchor: &gtk::Widget) -> (gtk::Popover, i32) {
    let pop = gtk::Popover::new();
    pop.add_css_class("issues");
    // On the window, not the header: a popover parented to a Box gets a slot among its children
    // and pushes them over.
    pop.set_parent(&u.win);
    pop.set_position(gtk::PositionType::Bottom);
    let mut room = u.win.height();
    if let Some(r) = anchor.compute_bounds(&u.win) {
        pop.set_pointing_to(Some(&gdk::Rectangle::new(
            r.x() as i32,
            r.y() as i32,
            r.width() as i32,
            r.height() as i32,
        )));
        room -= (r.y() + r.height()) as i32;
    }
    if let Some(mon) = u
        .win
        .surface()
        .and_then(|s| WidgetExt::display(&u.win).monitor_at_surface(&s))
    {
        room = room.min(mon.geometry().height() - 120);
    }
    (pop, room)
}

/// Takes a popover off its parent once it has closed (after the close finishes, not during it).
fn unparent_on_close(pop: &gtk::Popover) {
    pop.connect_closed(|p| {
        let p = p.clone();
        glib::idle_add_local_once(move || p.unparent());
    });
}

/// The conversation header's avatar; each person in it opens their contact card when clicked.
fn header_avatar(u: &Rc<Ui>, chat: &Value, title: &str) -> gtk::Widget {
    let click: OpenCard = {
        let u = u.clone();
        Rc::new(move |w, handle, name, open| show_card(&u, w, handle, name, open))
    };
    if chat["is_group"].as_bool() == Some(true) {
        let members: Vec<String> = chat["participants"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p.as_str().map(String::from))
            .collect();
        let names = u.st.borrow().names.clone();
        if members.len() > BIG_GROUP {
            // too many to show one by one: the list's avatar, which opens the list of people
            let w = group_avatar(u, title, &members, &names);
            w.set_halign(gtk::Align::Start);
            w.set_cursor_from_name(Some("pointer"));
            let (u2, w2) = (u.clone(), w.clone());
            let g = gtk::GestureClick::new();
            g.connect_released(move |_, _, _, _| show_people(&u2, &w2, &members));
            w.add_controller(g);
            return w;
        }
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
    let (pop, room) = anchored_popover(u, w);
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
    pop.connect_closed(move |_| open2(false));
    unparent_on_close(&pop);
    pop.popup();
    open(true);
    body.grab_focus(); // else the first button opens with a focus ring
}

/// More people than this in a group chat and the header shows the list's avatar and "N People" instead of one
/// avatar and one name for each.
const BIG_GROUP: usize = 4;

/// The list of people in a big group, dropped in under its header avatar. Picking a person swaps the list for their
/// contact card, with a back button, so it is one popover of one width whatever the window's size.
fn show_people(u: &Rc<Ui>, w: &gtk::Widget, members: &[String]) {
    let (pop, room) = anchored_popover(u, w);
    let max_h = (room - 30).max(200);
    let names = u.st.borrow().names.clone();

    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::SlideLeftRight);
    stack.set_transition_duration(220);
    stack.set_vhomogeneous(false);
    stack.set_interpolate_size(true);
    // A popover doesn't follow its content's size on its own once it is open, so it is asked to, on every frame of
    // the slide (the stack interpolates its height between the two pages) and once more when that ends.
    // (weak references throughout: a strong one in a handler of something inside the popover keeps it all alive)
    {
        let pop = pop.downgrade();
        stack.connect_transition_running_notify(move |st| {
            let pop = pop.clone();
            if st.is_transition_running() {
                st.add_tick_callback(move |st, _| {
                    if let Some(p) = pop.upgrade() {
                        p.present();
                    }
                    if st.is_transition_running() {
                        glib::ControlFlow::Continue
                    } else {
                        glib::ControlFlow::Break
                    }
                });
            } else if let Some(p) = pop.upgrade() {
                p.present();
            }
        });
    }
    let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
    list.add_css_class("people-list");
    list.set_size_request(340, -1);
    let head = gtk::Label::new(Some(&format!("{} People", members.len())));
    head.add_css_class("card-name");
    head.set_xalign(0.0);
    list.append(&head);
    let people = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let mut rows: Vec<gtk::Button> = Vec::new();
    for h in members {
        let name = names.get(h).cloned().unwrap_or_else(|| pretty_handle(h));
        let row = gtk::Button::new();
        row.add_css_class("flat");
        row.add_css_class("person-row");
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        line.append(&mini_avatar(u, &name, h, 32));
        let col = gtk::Box::new(gtk::Orientation::Vertical, 0);
        col.set_valign(gtk::Align::Center);
        let n = gtk::Label::new(Some(&name));
        n.set_xalign(0.0);
        n.set_ellipsize(gtk::pango::EllipsizeMode::End);
        col.append(&n);
        if pretty_handle(h) != name {
            let a = gtk::Label::new(Some(&pretty_handle(h)));
            a.add_css_class("dim");
            a.set_xalign(0.0);
            a.set_ellipsize(gtk::pango::EllipsizeMode::End);
            col.append(&a);
        }
        line.append(&col);
        row.set_child(Some(&line));
        let (u2, pop2, stack2, list2, h2, name2, count) = (
            u.clone(),
            pop.downgrade(),
            stack.downgrade(),
            list.downgrade(),
            h.clone(),
            name.clone(),
            members.len(),
        );
        row.connect_clicked(move |_| {
            let (Some(pop2), Some(stack2), Some(list2)) =
                (pop2.upgrade(), stack2.upgrade(), list2.upgrade())
            else {
                return;
            };
            if let Some(old) = stack2.child_by_name("card") {
                stack2.remove(&old);
            }
            let page = gtk::Box::new(gtk::Orientation::Vertical, 4);
            // never shorter than the list it slides over from; taller when the card needs it (up to the room there is)
            page.set_size_request(-1, list2.measure(gtk::Orientation::Vertical, -1).1);
            let back = gtk::Button::with_label(&format!("\u{2039} {count} People"));
            back.add_css_class("flat");
            back.set_halign(gtk::Align::Start);
            back.set_margin_top(4); // about as far from the top as its text is from the left
            let stack3 = stack2.downgrade();
            back.connect_clicked(move |_| {
                if let Some(st) = stack3.upgrade() {
                    st.set_visible_child_name("list");
                }
            });
            page.append(&back);
            let scroll = gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .propagate_natural_height(true)
                .propagate_natural_width(true)
                .max_content_height(max_h - 40)
                .child(&card_body(&u2, &pop2, &h2, &name2))
                .build();
            page.append(&scroll);
            stack2.add_named(&page, Some("card"));
            stack2.set_visible_child_name("card");
            back.grab_focus(); // keep the focus inside the popover: the list's button, which had it, is hidden now
        });
        rows.push(row.clone());
        people.append(&row);
    }
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(max_h - 40)
        .propagate_natural_width(true)
        .child(&people)
        .build();
    list.append(&scroll);
    stack.add_named(&list, Some("list"));
    pop.set_child(Some(&stack));
    if std::env::var_os("BF_PEOPLE").is_some() {
        let (stack2, pop2) = (stack.clone(), pop.clone());
        glib::timeout_add_local_once(Duration::from_millis(600), move || {
            eprintln!(
                "people: before click visible={} page={:?} width={}",
                pop2.is_visible(),
                stack2.visible_child_name(),
                stack2.width()
            );
            // BF_PEOPLE_ROW=<n>: which person to open (default the first)
            let n = std::env::var("BF_PEOPLE_ROW")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            if let Some(b) = rows.get(n) {
                b.emit_clicked();
            }
            let (stack3, pop3) = (stack2.clone(), pop2.clone());
            glib::timeout_add_local_once(Duration::from_millis(600), move || {
                eprintln!(
                    "people: after click visible={} page={:?} width={}",
                    pop3.is_visible(),
                    stack3.visible_child_name(),
                    stack3.width()
                );
            });
        });
    }
    unparent_on_close(&pop);
    pop.popup();
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
        match st
            .contacts
            .as_ref()
            .and_then(|c| Some((c, c.person(handle)?)))
        {
            Some((c, p)) => (
                p.name.clone(),
                p.org.clone(),
                p.addrs
                    .iter()
                    .map(|(a, l)| (a.clone(), l.clone(), c.has_imessage(a)))
                    .collect::<Vec<_>>(),
                p.places.clone(),
                p.birthday.clone(),
            ),
            None => (
                name.to_string(),
                String::new(),
                vec![(
                    handle.to_string(),
                    String::new(),
                    st.contacts.as_ref().is_some_and(|c| c.has_imessage(handle)),
                )],
                vec![],
                None,
            ),
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
    n.set_selectable(true);
    names.append(&n);
    // a popover hands its focus to the first selectable text, which then shows itself selected: clear that
    {
        let n = n.clone();
        root.connect_map(move |_| {
            let n = n.clone();
            glib::idle_add_local_once(move || n.select_region(0, 0));
        });
    }
    if !org.is_empty() {
        let o = gtk::Label::new(Some(&org));
        o.add_css_class("dim");
        o.set_xalign(0.0);
        o.set_selectable(true);
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
        v.set_selectable(true);
        // a star on the number or address this chat uses (the one the latest messages went to or came from)
        let used = crate::mac::addr_key(&addr)
            .is_some_and(|k| crate::mac::addr_key(handle) == Some(k))
            || addr.eq_ignore_ascii_case(handle);
        if used {
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            line.set_halign(gtk::Align::Start);
            let star = gtk::Label::new(Some(" ★")); // one space after the number, inline
            star.add_css_class("used-star");
            star.set_tooltip_text(Some("The one used in this conversation"));
            line.append(&v);
            line.append(&star);
            col.append(&line);
        } else {
            col.append(&v);
        }
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
        let copy = card_button("bf-copy-symbolic", "Copy");
        let (win, text) = (u.win.clone(), pretty_handle(&addr));
        copy.connect_clicked(move |b| {
            win.clipboard().set_text(&text);
            b.set_icon_name("bf-check-symbolic"); // a moment of "done"
            let b = b.clone();
            glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                b.set_icon_name("bf-copy-symbolic")
            });
        });
        row.append(&copy);
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
            let mail = card_button("bf-mail-symbolic", "Write an email");
            let (pop2, win) = (pop.downgrade(), u.win.clone());
            mail.connect_clicked(move |_| {
                if let Some(p) = pop2.upgrade() {
                    p.popdown();
                }
                // not gio::AppInfo::launch_default_for_uri: that handed Thunderbird "mailto:///addr"
                gtk::UriLauncher::new(&format!("mailto:{addr}")).launch(
                    Some(&win),
                    gio::Cancellable::NONE,
                    |_| {},
                );
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
        l.set_selectable(true);
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
            let b = card_button(
                "bf-map-pin-symbolic",
                "Show on a map\nTo see a small map here, turn on map previews in Settings > Privacy",
            );
            let win = u.win.clone();
            b.connect_clicked(move |_| {
                let q = crate::mac::url_encode(&query);
                open_map(
                    &win,
                    &format!("geo:0,0?q={q}"),
                    &format!("https://www.openstreetmap.org/search?query={q}"),
                );
            });
            row.append(&b);
        }
        root.append(&row);
    }
    if let Some(b) = birthday {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let bl = gtk::Label::new(Some(&b));
        bl.set_selectable(true);
        row.append(&bl);
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
    let mut b = format!(
        "{} {} {} {} {} {} {}\n",
        d.lat,
        d.lon,
        d.cols,
        d.rows,
        d.pin_x,
        d.pin_y,
        lens.join(" ")
    )
    .into_bytes();
    for t in &d.tiles {
        b.extend_from_slice(t);
    }
    b
}

fn map_view(d: &crate::mac::MapData) -> Option<MapView> {
    let tiles: Vec<gdk::Texture> = d.tiles.iter().filter_map(|t| texture(t)).collect();
    (tiles.len() == d.tiles.len() && !tiles.is_empty()).then(|| MapView {
        lat: d.lat,
        lon: d.lon,
        cols: d.cols,
        pin_x: d.pin_x,
        pin_y: d.pin_y,
        tiles,
    })
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
    map_view(&crate::mac::MapData {
        lat: num(0)?,
        lon: num(1)?,
        cols,
        rows,
        pin_x: num(4)?,
        pin_y: num(5)?,
        tiles,
    })
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
        Some((lat, lon)) => open_map(
            &win,
            &format!("geo:{lat},{lon}"), // no q=: apps search for q, and "lat,lon" matches nothing
            &format!("https://www.openstreetmap.org/?mlat={lat}&mlon={lon}#map=17/{lat}/{lon}"),
        ),
        None => {
            let q = crate::mac::url_encode(&q); // not found (yet): the maps app searches for the address itself
            open_map(
                &win,
                &format!("geo:0,0?q={q}"),
                &format!("https://www.openstreetmap.org/search?query={q}"),
            );
        }
    });
    frame.add_controller(g);

    let known = u.st.borrow().maps.get(query).cloned();
    let known = known.or_else(|| {
        let v = read_map_cache(u, query)?;
        u.st.borrow_mut()
            .maps
            .insert(query.to_string(), Some(v.clone()));
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
    let (ox, oy) = (
        m.pin_x / 2.0 - f64::from(MAP_BOX) / 2.0,
        m.pin_y / 2.0 - f64::from(MAP_BOX) / 2.0,
    );
    let fixed = gtk::Fixed::new();
    for (i, t) in m.tiles.iter().enumerate() {
        let pic = gtk::Picture::for_paintable(t);
        pic.set_can_shrink(true);
        pic.set_size_request(half, half);
        fixed.put(
            &pic,
            (i % m.cols) as f64 * f64::from(half) - ox,
            (i / m.cols) as f64 * f64::from(half) - oy,
        );
    }
    let pin = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    pin.add_css_class("map-pin");
    pin.set_size_request(14, 14);
    fixed.put(
        &pin,
        f64::from(MAP_BOX) / 2.0 - 7.0,
        f64::from(MAP_BOX) / 2.0 - 7.0,
    );
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
    glib::DateTime::from_iso8601(iso, None)
        .ok()?
        .to_local()
        .ok()
}

fn day(unix: Option<i64>) -> Option<(i32, i32, i32)> {
    glib::DateTime::from_unix_local(unix?).ok().map(|d| d.ymd())
}

/// Whole calendar days from `d` to today (0 = today, 1 = yesterday).
fn days_ago(d: &glib::DateTime) -> i64 {
    let midnight = |t: &glib::DateTime| {
        glib::DateTime::new(
            &glib::TimeZone::local(),
            t.year(),
            t.month(),
            t.day_of_month(),
            0,
            0,
            0.0,
        )
        .map(|m| m.to_unix())
        .unwrap_or(0)
    };
    let Ok(now) = glib::DateTime::now_local() else {
        return 0;
    };
    ((midnight(&now) - midnight(d) + 43_200).div_euclid(86_400)).max(0)
}

/// Where a time is shown, which decides how it is written (see `rel_time`).
#[derive(Clone, Copy, PartialEq)]
enum TimeKind {
    /// the chat list: `17:38`, `Yesterday`, `Mon`, `Nov 8`, `Nov 8, 2025`
    Row,
    /// after "Read ": the same
    Read,
    /// between groups of messages: `Today 17:38`, `Yesterday 17:38`, `Mon 17:38`, `Nov 8, 17:38`, `Nov 8, 2025, 17:38`
    Sep,
}

fn rel_time(unix: i64, kind: TimeKind) -> String {
    let Ok(d) = glib::DateTime::from_unix_local(unix) else {
        return String::new();
    };
    let fmt = |f: &str| d.format(f).map(|s| s.to_string()).unwrap_or_default();
    let (days, clock) = (days_ago(&d), fmt("%H:%M"));
    let this_year = glib::DateTime::now_local().is_ok_and(|n| n.year() == d.year());
    match (kind, days) {
        (TimeKind::Sep, 0) => format!("Today {clock}"),
        (TimeKind::Sep, 1) => format!("Yesterday {clock}"),
        (TimeKind::Sep, 2..=6) => format!("{} {clock}", fmt("%a")),
        (TimeKind::Sep, _) if this_year => format!("{}, {clock}", fmt("%a, %b %-d")),
        (TimeKind::Sep, _) => format!("{}, {clock}", fmt("%b %-d, %Y")),
        (_, 0) => clock,
        (_, 1) => "Yesterday".into(),
        (_, 2..=6) => fmt("%a"),
        (_, _) if this_year => fmt("%b %-d"),
        _ => fmt("%b %-d, %Y"),
    }
}

/// A label that shows a time written relative to today. They are kept so they can be rewritten in place when the
/// day changes (`refresh_times`), without rebuilding the list or the chat they sit in.
struct TimeLabel {
    label: glib::WeakRef<gtk::Label>,
    unix: Rc<Cell<Option<i64>>>,
    kind: TimeKind,
    /// a service name shown in bold above a `Sep` time
    svc: Option<String>,
}

impl TimeLabel {
    fn apply(&self) {
        let (Some(l), Some(t)) = (self.label.upgrade(), self.unix.get()) else {
            return;
        };
        let text = rel_time(t, self.kind);
        match (self.kind, &self.svc) {
            (TimeKind::Read, _) => l.set_text(&format!("Read {text}")),
            (_, Some(svc)) => l.set_markup(&format!(
                "<b>{}</b>\n{}",
                glib::markup_escape_text(svc),
                glib::markup_escape_text(&text)
            )),
            _ => l.set_text(&text),
        }
    }
}

/// Show `unix` in `label` and keep it right as days pass. The returned cell changes the time later.
fn track_time(
    u: &Ui,
    label: &gtk::Label,
    unix: Option<i64>,
    kind: TimeKind,
    svc: Option<String>,
) -> Rc<Cell<Option<i64>>> {
    let unix = Rc::new(Cell::new(unix));
    let t = TimeLabel {
        label: label.downgrade(),
        unix: unix.clone(),
        kind,
        svc,
    };
    t.apply();
    let mut all = u.time_labels.borrow_mut();
    if all.len() > 3000 {
        all.retain(|t| t.label.upgrade().is_some()); // the ones of chats drawn since are gone
    }
    all.push(t);
    unix
}

/// The day changed: write every tracked time again.
fn refresh_times(u: &Rc<Ui>) {
    u.time_day
        .set(glib::DateTime::now_local().map(|n| n.ymd()).ok());
    let mut all = u.time_labels.borrow_mut();
    all.retain(|t| t.label.upgrade().is_some());
    for t in all.iter() {
        t.apply();
    }
}

/// Run `refresh_times` just after each midnight. (A suspended computer wakes late: see also the window becoming
/// active again.)
fn schedule_midnight(u: &Rc<Ui>) {
    let Ok(now) = glib::DateTime::now_local() else {
        return;
    };
    let next = glib::DateTime::new(
        &glib::TimeZone::local(),
        now.year(),
        now.month(),
        now.day_of_month(),
        0,
        0,
        0.0,
    )
    .and_then(|m| m.add_days(1))
    .map(|m| m.to_unix())
    .unwrap_or(0);
    let secs = (next - now.to_unix() + 3).clamp(5, 90_000) as u32;
    let u = u.clone();
    glib::timeout_add_seconds_local_once(secs, move || {
        refresh_times(&u);
        schedule_midnight(&u);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_few_lone_emoji_go_big() {
        use super::{emoji_count, is_jumbo};
        assert_eq!(emoji_count("😂"), Some(1));
        assert_eq!(emoji_count("👨\u{200D}👩\u{200D}👧 👍🏽 🇨🇦"), Some(3));
        assert_eq!(emoji_count("❤\u{FE0F}1\u{FE0F}\u{20E3}"), Some(2));
        assert_eq!(emoji_count("😂😂😂😂"), Some(4));
        for text in ["ok 👍", "123", "#", "©", ":)", "é"] {
            assert_eq!(emoji_count(text), None, "{text}");
        }
        assert!(is_jumbo("🎉🎉🎉") && !is_jumbo("🎉🎉🎉🎉") && !is_jumbo("  "));
    }

    #[test]
    fn info_tabs_only_end_on_full_rows() {
        use super::tiles_to_show;
        assert_eq!(tiles_to_show(60, 24, true), (24, false));
        assert_eq!(tiles_to_show(11, 24, true), (8, true)); // wait for older ones
        assert_eq!(tiles_to_show(0, 24, true), (0, true));
        assert_eq!(tiles_to_show(11, 24, false), (11, false)); // the very end may be short
        assert_eq!(tiles_to_show(30, 24, false), (24, false));
    }

    #[test]
    fn small_photos_grow_only_so_far() {
        let shown = |w: f64, h: f64| (w * super::photo_scale(w, h)).round();
        assert_eq!(shown(1200.0, 900.0), 300.0);
        assert_eq!(shown(250.0, 200.0), 250.0);
        assert_eq!(shown(147.0, 110.0), 200.0);
        assert_eq!(shown(60.0, 60.0), 120.0);
        assert_eq!(shown(300.0, 1320.0), 150.0); // very tall: narrowed to fit the height limit
    }

    #[test]
    fn a_cut_off_gif_is_not_played() {
        let f = std::env::temp_dir().join(format!("bf-gif-{}", std::process::id()));
        std::fs::write(&f, b"GIF89a...;").expect("write");
        assert!(super::whole_gif(&f));
        std::fs::write(&f, b"GIF89a..").expect("write");
        assert!(!super::whole_gif(&f));
        let _ = std::fs::remove_file(&f);
        assert!(!super::whole_gif(&f));
        assert!(is_gif("a/b.GIF") && !is_gif("a/gif") && !is_gif("b.png"));
    }

    #[test]
    fn status_times_read_well_at_their_boundaries() {
        for (secs, ago_text, lasted_text) in [
            (-5, "just now", "0 s"), // the clock moved back
            (9, "just now", "9 s"),
            (10, "10 s ago", "10 s"),
            (59, "59 s ago", "59 s"),
            (60, "1 min ago", "1 min"),
            (3599, "59 min ago", "59 min"),
            (3600, "1 h ago", "1 h 0 min"),
            (8040, "2 h ago", "2 h 14 min"),
            (86400, "1 d ago", "1 d 0 h"),
        ] {
            assert_eq!(ago(secs), ago_text, "ago({secs})");
            assert_eq!(lasted(secs), lasted_text, "lasted({secs})");
        }
    }

    #[test]
    fn group_events_are_not_messages() {
        assert!(is_group_event(&json!({"item_type": 3, "text": ""})));
        assert!(!is_group_event(&json!({"item_type": 0, "text": "hi"})));
        assert!(!is_group_event(&json!({"text": "hi"})));
    }

    #[test]
    fn sender_file_names_stay_inside_the_folder() {
        assert_eq!(safe_name("photo.jpg", "file"), "photo.jpg");
        assert_eq!(safe_name("../../.bashrc", "file"), ".._.._.bashrc");
        assert_eq!(safe_name("/etc/passwd", "file"), "_etc_passwd");
        assert_eq!(safe_name("a\nb", "file"), "a_b");
        for odd in ["", ".", "..", "  "] {
            assert_eq!(safe_name(odd, "file"), "file");
        }
    }

    #[test]
    fn matching_ignores_case_for_accents_and_keeps_offsets() {
        assert_eq!(fold("CAFÉ Ünï İ"), "café ünï İ"); // İ would change length, so it stays
        assert_eq!(fold("CAFÉ").len(), "CAFÉ".len());
        let text = format!("{} Café au lait", "long ".repeat(30));
        assert!(snippet(&text, &["café".to_string()]).contains("Café"));
    }

    #[test]
    fn times_are_written_relative_to_today() {
        let now = glib::DateTime::now_local().unwrap().to_unix();
        assert_eq!(rel_time(now, TimeKind::Row).len(), 5); // 17:38
        assert_eq!(rel_time(now - 86_400, TimeKind::Row), "Yesterday");
        assert_eq!(rel_time(now - 86_400, TimeKind::Read), "Yesterday");
        assert!(rel_time(now, TimeKind::Sep).starts_with("Today "));
        assert!(rel_time(now - 86_400, TimeKind::Sep).starts_with("Yesterday "));
        assert_eq!(rel_time(now - 3 * 86_400, TimeKind::Row).len(), 3); // Mon
        assert!(rel_time(now - 800 * 86_400, TimeKind::Row).contains(", 20")); // Nov 8, 2024
    }

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
        assert_eq!(
            linkify("see https://a.com/x?a=1&b=2, ok <b>"),
            "see <a href=\"https://a.com/x?a=1&amp;b=2\">https://a.com/x?a=1&amp;b=2</a>, ok &lt;b&gt;"
        );
        assert_eq!(linkify("no link & more"), "no link &amp; more");
    }

    #[test]
    fn url_removed_from_text() {
        let l = |u: &str| crate::mac::LinkPreview {
            url: u.into(),
            ..Default::default()
        };
        assert_eq!(without_url("https://a.com/x", &l("https://a.com/x")), "");
        assert_eq!(
            without_url("look https://a.com/ nice", &l("https://a.com/")),
            "look nice"
        );
        assert_eq!(without_url("https://a.com/", &l("https://a.com")), "");
        assert_eq!(without_url("hello", &l("https://a.com")), "hello");
        // the typed address carried tracking parameters; the text holds the clean one
        assert_eq!(
            without_url(
                "https://www.cabidor.com/",
                &l("https://www.cabidor.com/?srsltid=AYC")
            ),
            ""
        );
        // a link to some other site stays
        assert_eq!(
            without_url("see https://b.com/y", &l("https://a.com/x")),
            "see https://b.com/y"
        );
    }

    #[test]
    fn addresses() {
        assert!(is_addr("403 555-1234") && is_addr("a@b.ca") && is_addr("562528"));
        assert!(!is_addr("Sue") && !is_addr("12") && !is_addr("a@b"));
        assert!(same_addr("403 555 1234", "+14035551234"));
        assert!(same_addr("Sue@iCloud.com", "sue@icloud.com"));
        assert!(!same_addr("551299", "+14035551299"));
        assert!(!same_addr("4035551234", "+14035551235"));
        assert_eq!(clean_addr(" +1 (403) 555-1234 "), "+14035551234");
    }
    use serde_json::json;

    #[test]
    fn titles_and_initials() {
        let names = HashMap::from([("+1".to_string(), "Ann Lee".to_string())]);
        assert_eq!(
            chat_title(
                &json!({"display_name": "", "participants": ["+1", "+2"]}),
                &names
            ),
            "Ann & +2"
        );
        let names3 = HashMap::from([
            ("a".to_string(), "Jo Harris".to_string()),
            ("b".to_string(), "Rob Epp".to_string()),
            ("c".to_string(), "Drayven".to_string()),
        ]);
        assert_eq!(
            chat_title(
                &json!({"display_name": "", "participants": ["a", "b", "c"]}),
                &names3
            ),
            "Jo, Rob & Drayven"
        );
        assert_eq!(
            chat_title(
                &json!({"display_name": "Book Club", "participants": ["a", "b"]}),
                &names3
            ),
            "Book Club"
        );
        // numbers go last, in titles and avatars
        assert_eq!(
            chat_title(
                &json!({"display_name": "", "participants": ["+9", "a", "b"]}),
                &names3
            ),
            "Jo, Rob & +9"
        );
        assert_eq!(
            known_first(&["+9".to_string(), "a".to_string()], &names3),
            vec!["a".to_string(), "+9".to_string()]
        );
        // ranked: known first, those you wrote to last come first, a number after them all
        let hs: Vec<String> = ["drayven", "+9", "rob", "jo"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let when = |h: &str| match h {
            "jo" => Some("2026-11".to_string()),
            "rob" => Some("2025-01".to_string()),
            "drayven" => Some("2024-02".to_string()),
            _ => None,
        };
        let got = rank_members(&hs, |h| h != "+9", when);
        assert_eq!(got, vec!["jo", "rob", "drayven", "+9"]);
        // a big group: first names, the rest counted; no names, just the count
        let big =
            json!({"display_name": "", "participants": ["+1", "+2", "a", "b", "c", "+3", "+4"]});
        assert_eq!(
            big_group_title(&big, &names3),
            "Jo, Rob, Drayven & 4 others"
        );
        let few = json!({"display_name": "", "participants": ["+1", "+2", "a", "+3", "+4"]});
        assert_eq!(big_group_title(&few, &names3), "Jo & 4 others");
        let none = json!({"display_name": "", "participants": ["+1", "+2", "+3", "+4", "+5"]});
        assert_eq!(big_group_title(&none, &names3), "Chat with 5 People");
        let one_left = json!({"display_name": "", "participants": ["a", "b", "c", "+1", "d"]});
        assert_eq!(
            big_group_title(&one_left, &names3),
            "Jo, Rob, Drayven & 2 others"
        );
        assert_eq!(
            chat_title(&json!({"display_name": "", "contact_name": "Rob"}), &names),
            "Rob"
        );
        assert_eq!(initials("Ann Lee"), "AL");
        assert_eq!(initials("+1 555"), "#");
    }

    #[test]
    fn summaries() {
        assert_eq!(
            summary(&json!({"text": "\u{FFFC}", "attachments": [{"mime_type": "image/heic"}]})),
            "📷 Photo"
        );
        let long = "King Will Silicone Wedding Rings For Men,6.6mm Wide 1.8mm Thick Rubber Wedding Bands,Breathable Design,Comfort Fit Size 7-13";
        let c = clip_middle(long, 70);
        assert!(
            c.starts_with("King Will Silicone")
                && c.ends_with("Fit Size 7-13")
                && c.contains('…')
                && c.chars().count() <= 70,
            "{c}"
        );
        assert_eq!(clip_middle("short title", 70), "short title");
        let f = clip_middle(
            "Packing_list_with_no_spaces_at_all_but_a_really_long_name_for_the_trip_2026.docx",
            48,
        );
        assert!(f.ends_with(".docx") && f.chars().count() <= 48, "{f}");
        assert_eq!(
            summary(&json!({"text": "hi\nthere", "attachments": []})),
            "hi"
        );
    }
}

#[cfg(test)]
mod pill_tests {
    use super::{fold, word_spans};

    #[test]
    fn words_grow_and_neighbours_join() {
        let t = "say hi abas there";
        let w = vec!["hi".to_string(), "aba".to_string()];
        assert_eq!(word_spans(t, &fold(t), &w), vec![(4, 11)]);
        let t = "xx thing, other";
        assert_eq!(
            word_spans(t, &fold(t), &["th".to_string()]),
            vec![(3, 8), (10, 15)]
        );
    }
}

#[cfg(test)]
mod merge_tests {
    use super::merged_chats;
    use serde_json::json;

    #[test]
    fn same_number_merges_without_a_contact_and_keeps_order() {
        let chat = |id: i64, who: &str, group: bool, unread: i64| json!({"id": id, "identifier": who, "participants": [who], "is_group": group, "unread_count": unread});
        // newest first, as imsg lists them: the newest chat is id 9, the oldest id 2
        let raw = vec![
            chat(9, "+1 (555) 010-2000", false, 1),
            chat(5, "+15550109999", false, 0),
            chat(2, "5550102000", false, 2),
            chat(7, "+15550102000", true, 0),
        ];
        let (list, groups) = merged_chats(&raw, None);
        let ids: Vec<i64> = list.iter().map(|c| c["id"].as_i64().unwrap()).collect();
        assert_eq!(
            ids,
            vec![2, 5, 7],
            "entry stands at the newest chat's place, with the oldest id"
        );
        assert_eq!(groups, vec![(2, vec![9, 2])]);
        assert_eq!(list[0]["unread_count"], 3);
    }
}
