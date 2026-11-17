//! Full-size photo viewer, a panel of its own covering the sidebar and the chat. Opens by zooming the clicked photo up from
//! its spot in the chat while a black backdrop fades in, and closes the same way in reverse.
//! - The photo fills the panel on its longest side. The chat's 1200 px copy shows at once; when the
//!   original is bigger, the full version is fetched and swapped in (small spinner at its top-left).
//! - Prev/next (buttons, ← →, mouse wheel) walk the photos loaded in the chat so far and stop at
//!   the ends; older ones appear once the chat is scrolled back to them.
//! - Controls overlay the photo and fade out after a few seconds without mouse movement.
//! - A video (a preview with the `video` class) is copied from the Mac with a progress bar, then plays in place of
//!   the photo with play/pause, a seek bar, times and volume (see player.rs). Space or a click on it toggles play.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene};

use crate::bubble::{self, Bubble};
use crate::player::{self, Player};
use std::path::{Path, PathBuf};

const OPEN_MS: f64 = 240.0;
const HIDE_AFTER: Duration = Duration::from_millis(2500);

/// What the viewer needs from the chat.
pub struct Hooks {
    /// fetch the big version of this photo; the answer arrives via `full_ready` / `full_failed`
    pub fetch_full: Box<dyn Fn(&str)>,
    /// copy this video from the Mac (or find the copy we have); the answer arrives via `video_progress` / `video_ready` / `video_failed`
    pub fetch_video: Box<dyn Fn(&str)>,
    /// open a local file in another app (when the video can't be played here)
    pub open_external: Box<dyn Fn(&Path)>,
}

pub struct Viewer {
    pub root: gtk::Overlay,
    canvas: Canvas,
    prev: gtk::Button,
    next: gtk::Button,
    /// (path, widget) for every photo in the chat, in display order
    items: RefCell<Vec<(String, Bubble)>>,
    idx: Cell<usize>,
    full: RefCell<HashMap<String, gdk::Texture>>,
    hooks: RefCell<Option<Hooks>>,
    hide_gen: Rc<Cell<u32>>,
    me: RefCell<std::rc::Weak<Viewer>>,
    // video
    player: RefCell<Option<Rc<Player>>>,
    /// the video whose copy or playback is under way
    want_video: RefCell<Option<String>>,
    /// copies we've got (Mac path -> local file)
    local: RefCell<HashMap<String, PathBuf>>,
    bar: gtk::Box,
    play_btn: gtk::Button,
    time_l: gtk::Label,
    len_l: gtk::Label,
    seek: gtk::Scale,
    mute_btn: gtk::Button,
    vol: gtk::Scale,
    busy: gtk::Box,
    busy_label: gtk::Label,
    busy_bar: gtk::ProgressBar,
    busy_btn: gtk::Button,
    /// the seek bar is being set by us, not dragged
    syncing: Cell<bool>,
    dragging: Cell<bool>,
    muted: Cell<bool>,
    volume: Cell<f64>,
    poll_gen: Cell<u32>,
    /// last pointer position over the viewer (controls stay while it's on one)
    ptr: Cell<(f64, f64)>,
    close: gtk::Button,
}

fn clock(secs: f64) -> String {
    let t = secs.max(0.0).round() as i64;
    let (h, m, s) = (t / 3600, t / 60 % 60, t % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

fn icon_btn(icon: &str, tip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.add_css_class("viewer-ctl");
    b.set_valign(gtk::Align::Center);
    b.set_tooltip_text(Some(tip));
    b
}

impl Viewer {
    pub fn new() -> Rc<Self> {
        let canvas = Canvas::default();
        canvas.set_hexpand(true);
        canvas.set_vexpand(true);
        let root = gtk::Overlay::new();
        root.set_child(Some(&canvas));
        root.add_css_class("viewer");
        root.set_overflow(gtk::Overflow::Hidden); // rounded corners clip the photo and backdrop
        root.set_visible(false);
        let nav = |icon: &str, align: gtk::Align, tip: &str| {
            let b = gtk::Button::from_icon_name(icon);
            b.add_css_class("viewer-nav");
            b.set_halign(align);
            b.set_valign(gtk::Align::Center);
            b.set_tooltip_text(Some(tip));
            root.add_overlay(&b);
            b
        };
        let prev = nav("go-previous-symbolic", gtk::Align::Start, "Previous photo (←)");
        let next = nav("go-next-symbolic", gtk::Align::End, "Next photo (→)");
        let close = nav("window-close-symbolic", gtk::Align::End, "Close (Esc)");
        close.set_valign(gtk::Align::Start);
        close.add_css_class("viewer-close");

        // video controls along the bottom, and the copy's progress in the middle (both hidden until a video is open)
        let play_btn = icon_btn("bf-play-symbolic", "Play / pause (Space)");
        let time_l = gtk::Label::new(Some("0:00"));
        let len_l = gtk::Label::new(Some("0:00"));
        for l in [&time_l, &len_l] {
            l.add_css_class("viewer-time");
        }
        let seek = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.001);
        seek.set_draw_value(false);
        seek.set_hexpand(true);
        seek.add_css_class("viewer-scale");
        let mute_btn = icon_btn("bf-volume-symbolic", "Mute");
        let vol = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.01);
        vol.set_draw_value(false);
        vol.set_value(1.0);
        vol.set_size_request(80, -1);
        vol.add_css_class("viewer-scale");
        vol.set_tooltip_text(Some("Volume"));
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        bar.add_css_class("viewer-bar");
        bar.set_halign(gtk::Align::Fill);
        bar.set_valign(gtk::Align::End);
        bar.set_visible(false);
        for w in [play_btn.upcast_ref::<gtk::Widget>(), time_l.upcast_ref(), seek.upcast_ref(), len_l.upcast_ref(), mute_btn.upcast_ref(), vol.upcast_ref()] {
            bar.append(w);
        }
        root.add_overlay(&bar);
        let busy_label = gtk::Label::new(Some("Copying from the Mac…"));
        busy_label.add_css_class("viewer-time");
        let busy_bar = gtk::ProgressBar::new();
        busy_bar.add_css_class("viewer-progress");
        let busy_btn = gtk::Button::with_label("Open in another app");
        busy_btn.add_css_class("viewer-ctl-text");
        busy_btn.set_visible(false);
        let busy = gtk::Box::new(gtk::Orientation::Vertical, 10);
        busy.add_css_class("viewer-busy");
        busy.set_halign(gtk::Align::Center);
        busy.set_valign(gtk::Align::End);
        busy.set_size_request(300, -1);
        busy.set_visible(false);
        busy.append(&busy_label);
        busy.append(&busy_bar);
        busy.append(&busy_btn);
        root.add_overlay(&busy);

        let v = Rc::new(Self {
            root,
            canvas,
            prev,
            next,
            items: RefCell::default(),
            idx: Cell::new(0),
            full: RefCell::default(),
            hooks: RefCell::default(),
            hide_gen: Rc::default(),
            me: RefCell::default(),
            player: RefCell::default(),
            want_video: RefCell::default(),
            local: RefCell::default(),
            bar,
            play_btn,
            time_l,
            len_l,
            seek,
            mute_btn,
            vol,
            busy,
            busy_label,
            busy_bar,
            busy_btn,
            syncing: Cell::new(false),
            dragging: Cell::new(false),
            muted: Cell::new(false),
            volume: Cell::new(1.0),
            poll_gen: Cell::new(0),
            ptr: Cell::new((f64::NAN, f64::NAN)),
            close: close.clone(),
        });
        *v.me.borrow_mut() = Rc::downgrade(&v);
        let w = v.clone();
        v.play_btn.connect_clicked(move |_| w.toggle_play());
        let w = v.clone();
        v.seek.connect_value_changed(move |s| {
            if w.syncing.get() {
                return;
            }
            if let Some(p) = w.player.borrow().as_ref()
                && let (_, Some(len)) = p.times()
            {
                p.seek(s.value() * len);
            }
        });
        let press = gtk::GestureClick::new();
        press.set_propagation_phase(gtk::PropagationPhase::Capture);
        let w = v.clone();
        press.connect_pressed(move |_, _, _, _| w.dragging.set(true));
        let w = v.clone();
        press.connect_released(move |_, _, _, _| w.dragging.set(false));
        v.seek.add_controller(press);
        let w = v.clone();
        v.mute_btn.connect_clicked(move |_| {
            w.muted.set(!w.muted.get());
            w.apply_sound();
        });
        let w = v.clone();
        v.vol.connect_value_changed(move |s| {
            w.volume.set(s.value());
            w.muted.set(false);
            w.apply_sound();
        });
        let w = v.clone();
        v.busy_btn.connect_clicked(move |_| {
            let want = w.want_video.borrow().clone();
            let local = want.and_then(|p| w.local.borrow().get(&p).cloned());
            if let (Some(l), Some(h)) = (local, w.hooks.borrow().as_ref()) {
                (h.open_external)(&l);
            }
        });
        let w = v.clone();
        v.prev.connect_clicked(move |_| w.step(-1));
        let w = v.clone();
        v.next.connect_clicked(move |_| w.step(1));
        let w = v.clone();
        close.connect_clicked(move |_| w.close());

        // only real pointer movement reveals the controls (GTK also sends motion when content
        // changes under a still pointer, so compare positions); they fade after a pause
        let motion = gtk::EventControllerMotion::new();
        let last = Cell::new((f64::NAN, f64::NAN));
        let w = v.clone();
        motion.connect_motion(move |_, x, y| {
            w.ptr.set((x, y));
            if last.get() != (x, y) {
                last.set((x, y));
                w.show_controls();
            }
        });
        let w = v.clone();
        motion.connect_leave(move |_| w.root.remove_css_class("controls"));
        v.root.add_controller(motion);

        // one photo per wheel notch (DISCRETE turns smooth touchpad scrolling into whole steps)
        let wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL | gtk::EventControllerScrollFlags::DISCRETE);
        let w = v.clone();
        wheel.connect_scroll(move |_, _, dy| {
            if dy > 0.0 {
                w.step(1);
            } else if dy < 0.0 {
                w.step(-1);
            }
            glib::Propagation::Stop
        });
        v.root.add_controller(wheel);

        let click = gtk::GestureClick::new();
        let w = v.clone();
        click.connect_released(move |_, _, x, y| {
            if !w.canvas.photo_rect().contains_point(&graphene::Point::new(x as f32, y as f32)) {
                w.close(); // a click on the backdrop closes, like Quick Look
            } else if w.player.borrow().is_some() {
                w.toggle_play(); // a click on the playing video pauses it
            }
        });
        v.canvas.add_controller(click);
        v
    }

    pub fn set_hooks(&self, h: Hooks) {
        *self.hooks.borrow_mut() = Some(h);
    }

    pub fn is_open(&self) -> bool {
        self.root.is_visible()
    }

    /// Keys while open (the window forwards them): Esc closes, ← → step. Returns true if handled.
    pub fn key(&self, key: gdk::Key) -> bool {
        match key {
            gdk::Key::Escape => self.close(),
            gdk::Key::space if self.player.borrow().is_some() => self.toggle_play(),
            gdk::Key::Left => self.step(-1),
            gdk::Key::Right => self.step(1),
            _ => return false,
        }
        true
    }

    /// Open on the photo at `path` (it must have its texture), with `items` to walk through.
    pub fn open(&self, items: Vec<(String, Bubble)>, path: &str) {
        let Some(i) = items.iter().position(|(p, b)| p == path && b.texture().is_some()) else { return };
        let from = self.source_rect(&items[i].1);
        *self.items.borrow_mut() = items;
        self.idx.set(i);
        self.canvas.imp().from.set(from);
        self.canvas.imp().t.set(0.0);
        self.root.set_visible(true);
        self.show_current();
        self.show_controls();
        self.canvas.animate(1.0, || {});
    }

    /// The chat re-rendered (new or older messages): take the new widgets, staying on the same photo.
    pub fn refresh(&self, items: Vec<(String, Bubble)>) {
        if !self.is_open() {
            return;
        }
        let cur = self.current_path();
        let i = cur.and_then(|c| items.iter().position(|(p, _)| *p == c)).unwrap_or(0);
        *self.items.borrow_mut() = items;
        self.idx.set(i);
        self.show_current();
    }

    pub fn full_ready(&self, path: &str, tex: gdk::Texture) {
        self.full.borrow_mut().insert(path.to_string(), tex.clone());
        if self.is_open() && self.current_path().as_deref() == Some(path) {
            self.canvas.imp().tex.replace(Some(tex));
            self.canvas.set_loading(false);
        }
    }

    pub fn full_failed(&self, path: &str) {
        if self.current_path().as_deref() == Some(path) {
            self.canvas.set_loading(false);
        }
    }

    pub fn close(&self) {
        if !self.is_open() || self.canvas.imp().closing.get() {
            return;
        }
        let cur = self.items.borrow().get(self.idx.get()).map(|(_, b)| b.clone());
        let from = match cur {
            Some(b) if b.is_mapped() => self.source_rect(&b),
            _ => {
                // the photo isn't on screen any more: shrink towards the middle instead
                let (w, h) = (self.root.width() as f32, self.root.height() as f32);
                graphene::Rect::new(w / 2.0 - 40.0, h / 2.0 - 40.0, 80.0, 80.0)
            }
        };
        self.stop_video();
        self.canvas.imp().from.set(from);
        self.canvas.imp().closing.set(true);
        self.canvas.set_loading(false);
        self.root.remove_css_class("controls");
        let root = self.root.clone();
        let canvas = self.canvas.clone();
        self.canvas.animate(0.0, move || {
            canvas.imp().closing.set(false);
            root.set_visible(false);
        });
    }

    /// Hide immediately (switching chats).
    pub fn close_now(&self) {
        self.stop_video();
        self.root.set_visible(false);
        self.canvas.imp().closing.set(false);
        self.canvas.set_loading(false);
        self.items.borrow_mut().clear();
    }

    fn current_path(&self) -> Option<String> {
        self.items.borrow().get(self.idx.get()).map(|(p, _)| p.clone())
    }

    /// Move by one photo, stopping at the ends; skips photos whose chat copy hasn't arrived yet.
    fn step(&self, d: isize) {
        if !self.is_open() || self.canvas.imp().closing.get() {
            return;
        }
        let found = {
            let items = self.items.borrow();
            let mut j = self.idx.get() as isize + d;
            while (0..items.len() as isize).contains(&j) && items[j as usize].1.texture().is_none() {
                j += d;
            }
            (0..items.len() as isize).contains(&j).then_some(j as usize)
        };
        if let Some(j) = found {
            self.idx.set(j);
            self.show_current();
        }
    }

    /// Show the photo at `idx`: the big version if we have it, else the chat copy plus a fetch.
    fn show_current(&self) {
        let Some((path, b)) = self.items.borrow().get(self.idx.get()).cloned() else { return };
        let hooks = self.hooks.borrow();
        let full = self.full.borrow().get(&path).cloned();
        if let Some(t) = full {
            self.canvas.imp().tex.replace(Some(t));
            self.canvas.set_loading(false);
        } else if let Some(t) = b.texture() {
            // the chat copy is capped at CHAT_PX; only a copy that hit the cap has a bigger original
            let bigger = t.width().max(t.height()) as u32 >= crate::mac::CHAT_PX - 10;
            self.canvas.imp().tex.replace(Some(t));
            self.canvas.set_loading(bigger);
            if bigger && let Some(h) = hooks.as_ref() {
                (h.fetch_full)(&path);
            }
        }
        self.canvas.queue_draw();

        let (i, n) = (self.idx.get(), self.items.borrow().len());
        self.prev.set_visible(i > 0);
        self.next.set_visible(i + 1 < n);
        self.sync_video(&path, &b);
    }

    // ---- video ----

    /// The photo on show is a video: copy it from the Mac and play it (once); anything else stops the player.
    fn sync_video(&self, path: &str, b: &Bubble) {
        if !b.has_css_class("video") {
            self.stop_video();
            return;
        }
        if self.want_video.borrow().as_deref() == Some(path) {
            return; // already on it (the chat re-rendered under us)
        }
        self.stop_video();
        *self.want_video.borrow_mut() = Some(path.to_string());
        let local = self.local.borrow().get(path).filter(|p| p.exists()).cloned();
        match local {
            Some(l) => self.start_player(path, &l),
            None => {
                self.busy_label.set_text("Copying from the Mac…");
                self.busy_bar.set_fraction(0.0);
                self.busy_bar.set_visible(true);
                self.busy_btn.set_visible(false);
                self.busy.set_visible(true);
                if let Some(h) = self.hooks.borrow().as_ref() {
                    (h.fetch_video)(path);
                }
            }
        }
    }

    /// Stop and forget the playing video (and any copy in progress).
    fn stop_video(&self) {
        self.poll_gen.set(self.poll_gen.get().wrapping_add(1));
        self.player.borrow_mut().take();
        self.want_video.borrow_mut().take();
        self.canvas.imp().video.take();
        self.bar.set_visible(false);
        self.busy.set_visible(false);
        self.canvas.queue_draw();
    }

    /// How far the copy has got.
    pub fn video_progress(&self, path: &str, got: usize, total: i64) {
        if self.want_video.borrow().as_deref() != Some(path) {
            return;
        }
        let mb = |b: f64| b / 1024.0 / 1024.0;
        if total > 0 {
            self.busy_bar.set_fraction((got as f64 / total as f64).clamp(0.0, 1.0));
            self.busy_label.set_text(&format!("Copying from the Mac… {:.1} of {:.1} MB", mb(got as f64), mb(total as f64)));
        } else {
            self.busy_bar.pulse();
            self.busy_label.set_text(&format!("Copying from the Mac… {:.1} MB", mb(got as f64)));
        }
    }

    /// The copy is done.
    pub fn video_ready(&self, path: &str, local: PathBuf) {
        self.local.borrow_mut().insert(path.to_string(), local.clone());
        if self.want_video.borrow().as_deref() == Some(path) {
            self.start_player(path, &local);
        }
    }

    pub fn video_failed(&self, path: &str, why: &str) {
        if self.want_video.borrow().as_deref() == Some(path) {
            self.busy_bar.set_visible(false);
            self.busy_btn.set_visible(false);
            self.busy_label.set_text(&format!("Couldn't copy the video: {why}"));
            self.busy.set_visible(true);
        }
    }

    fn start_player(&self, path: &str, local: &Path) {
        let me = self.me.borrow().clone();
        let msg = move |m: player::Msg| {
            let Some(v) = me.upgrade() else { return };
            match m {
                player::Msg::Ended => {
                    if let Some(p) = v.player.borrow().as_ref() {
                        p.pause();
                        p.seek(0.0);
                    }
                    v.update_times();
                    v.show_controls();
                }
                player::Msg::Failed(e) => {
                    v.player.borrow_mut().take();
                    v.canvas.imp().video.take();
                    v.bar.set_visible(false);
                    v.busy_bar.set_visible(false);
                    v.busy_label.set_text(&format!("This video can't be played here: {e}"));
                    v.busy_btn.set_visible(true);
                    v.busy.set_visible(true);
                    v.canvas.queue_draw();
                }
            }
        };
        match Player::new(local, msg) {
            Ok(p) => {
                let canvas = self.canvas.clone();
                p.paintable.connect_invalidate_contents(glib::clone!(
                    #[weak]
                    canvas,
                    move |_| canvas.queue_draw()
                ));
                let canvas = self.canvas.clone();
                p.paintable.connect_invalidate_size(glib::clone!(
                    #[weak]
                    canvas,
                    move |_| canvas.queue_draw()
                ));
                self.canvas.imp().video.replace(Some(p.paintable.clone()));
                p.set_volume(self.volume.get());
                p.set_muted(self.muted.get());
                p.play();
                *self.player.borrow_mut() = Some(Rc::new(p));
                self.busy.set_visible(false);
                self.bar.set_visible(true);
                self.apply_sound();
                self.update_times();
                self.show_controls();
                let g = self.poll_gen.get().wrapping_add(1);
                self.poll_gen.set(g);
                let me = self.me.borrow().clone();
                glib::timeout_add_local(Duration::from_millis(250), move || {
                    let Some(v) = me.upgrade() else { return glib::ControlFlow::Break };
                    if v.poll_gen.get() != g {
                        return glib::ControlFlow::Break;
                    }
                    v.update_times();
                    glib::ControlFlow::Continue
                });
            }
            Err(e) => {
                self.busy_bar.set_visible(false);
                self.busy_label.set_text(&format!("This video can't be played here: {e}"));
                self.busy_btn.set_visible(true);
                self.busy.set_visible(true);
            }
        }
        let _ = path;
    }

    fn toggle_play(&self) {
        if let Some(p) = self.player.borrow().as_ref() {
            if p.is_playing() { p.pause() } else { p.play() }
        }
        self.update_times();
        self.show_controls();
    }

    fn apply_sound(&self) {
        if let Some(p) = self.player.borrow().as_ref() {
            p.set_muted(self.muted.get());
            p.set_volume(self.volume.get());
        }
        let muted = self.muted.get() || self.volume.get() == 0.0;
        let img = gtk::Image::from_icon_name(if muted { "bf-muted-symbolic" } else { "bf-volume-symbolic" });
        self.mute_btn.set_child(Some(&img));
    }

    /// The times, the seek bar and the play/pause icon follow the player.
    fn update_times(&self) {
        let Some(p) = self.player.borrow().clone() else { return };
        let (pos, len) = p.times();
        self.time_l.set_text(&clock(pos));
        if let Some(l) = len {
            self.len_l.set_text(&clock(l));
            if !self.dragging.get() && l > 0.0 {
                self.syncing.set(true);
                self.seek.set_value((pos / l).clamp(0.0, 1.0));
                self.syncing.set(false);
            }
        }
        let icon = gtk::Image::from_icon_name(if p.is_playing() { "bf-pause-symbolic" } else { "bf-play-symbolic" });
        self.play_btn.set_child(Some(&icon));
    }

    fn show_controls(&self) {
        self.root.add_css_class("controls");
        let g = self.hide_gen.get().wrapping_add(1);
        self.hide_gen.set(g);
        let me = self.me.borrow().clone();
        glib::timeout_add_local_once(HIDE_AFTER, move || {
            let Some(v) = me.upgrade() else { return };
            if v.hide_gen.get() != g {
                return; // moved since this timer started
            }
            if v.pointer_on_control() {
                v.show_controls(); // never hide while the pointer rests on a control
            } else {
                v.root.remove_css_class("controls");
            }
        });
    }

    fn pointer_on_control(&self) -> bool {
        let (x, y) = self.ptr.get();
        let pt = graphene::Point::new(x as f32, y as f32);
        [self.prev.upcast_ref::<gtk::Widget>(), self.next.upcast_ref(), self.close.upcast_ref(), self.bar.upcast_ref(), self.busy.upcast_ref()]
            .into_iter()
            .any(|w| w.is_visible() && w.compute_bounds(&self.root).is_some_and(|r| r.contains_point(&pt)))
    }

    /// Where the photo sits in the chat (or the info panel), in the viewer's coordinates.
    fn source_rect(&self, b: &Bubble) -> graphene::Rect {
        let body = b.body();
        b.compute_point(&self.canvas, &graphene::Point::new(body.x(), body.y()))
            .map(|p| graphene::Rect::new(p.x(), p.y(), body.width(), body.height()))
            .unwrap_or_else(|| graphene::Rect::new(0.0, 0.0, 1.0, 1.0))
    }
}

// ---- the drawing surface ----

glib::wrapper! {
    pub struct Canvas(ObjectSubclass<imp::Canvas>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for Canvas {
    fn default() -> Self {
        glib::Object::new()
    }
}

fn ease(t: f64) -> f64 {
    1.0 - (1.0 - t).powi(3) // ease-out cubic
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

impl Canvas {
    /// Where the photo sits when fully open: longest side to the edges, keeping its shape.
    fn fit_rect(&self) -> graphene::Rect {
        let (w, h) = (self.width() as f32, self.height() as f32);
        let (tw, th) = match self.imp().video.borrow().as_ref().filter(|v| v.intrinsic_width() > 0) {
            Some(v) => (v.intrinsic_width() as f32, v.intrinsic_height().max(1) as f32),
            None => {
                let Some(t) = self.imp().tex.borrow().clone() else { return graphene::Rect::new(0.0, 0.0, w, h) };
                (t.width().max(1) as f32, t.height().max(1) as f32)
            }
        };
        let k = (w / tw).min(h / th).max(0.01);
        let (dw, dh) = (tw * k, th * k);
        graphene::Rect::new((w - dw) / 2.0, (h - dh) / 2.0, dw, dh)
    }

    fn photo_rect(&self) -> graphene::Rect {
        let t = ease(self.imp().t.get()) as f32;
        let (a, b) = (self.imp().from.get(), self.fit_rect());
        graphene::Rect::new(lerp(a.x(), b.x(), t), lerp(a.y(), b.y(), t), lerp(a.width(), b.width(), t), lerp(a.height(), b.height(), t))
    }

    /// Show or hide the "fetching the big version" spinner; it turns on the frame clock while shown.
    fn set_loading(&self, on: bool) {
        let was = self.imp().loading.replace(on);
        self.queue_draw();
        if on && !was {
            self.add_tick_callback(|w, _| {
                w.queue_draw();
                if w.imp().loading.get() { glib::ControlFlow::Continue } else { glib::ControlFlow::Break }
            });
        }
    }

    /// Animate `t` (0 = in the chat, 1 = open) towards `to`, then run `done`.
    fn animate(&self, to: f64, done: impl FnOnce() + 'static) {
        let start = self.imp().t.get();
        let began = Cell::new(None::<i64>);
        let done = RefCell::new(Some(done));
        self.add_tick_callback(move |w, clock| {
            let now = clock.frame_time();
            let t0 = *began.get().get_or_insert(now);
            began.set(Some(t0));
            let p = (((now - t0) as f64 / 1000.0) / OPEN_MS).min(1.0);
            w.imp().t.set(start + (to - start) * p);
            w.queue_draw();
            if p >= 1.0 {
                if let Some(f) = done.borrow_mut().take() {
                    f();
                }
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    }
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Canvas {
        pub tex: RefCell<Option<gdk::Texture>>,
        pub from: Cell<graphene::Rect>,
        pub t: Cell<f64>,
        pub closing: Cell<bool>,
        pub loading: Cell<bool>,
        /// a playing video's frames, drawn instead of `tex` once the first one has arrived
        pub video: RefCell<Option<gdk::Paintable>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Canvas {
        const NAME: &'static str = "BfViewerCanvas";
        type Type = super::Canvas;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Canvas {}

    impl WidgetImpl for Canvas {
        fn snapshot(&self, s: &gtk::Snapshot) {
            let w = self.obj();
            let (width, height) = (w.width() as f32, w.height() as f32);
            let t = ease(self.t.get()) as f32;
            s.append_color(&gdk::RGBA::new(0.0, 0.0, 0.0, t), &graphene::Rect::new(0.0, 0.0, width, height));
            let Some(tex) = self.tex.borrow().clone() else { return };
            let r = w.photo_rect();
            // corners go from the chat's rounded card to square as it opens
            s.push_rounded_clip(&gtk::gsk::RoundedRect::from_rect(r, lerp(18.0, 0.0, t)));
            match self.video.borrow().as_ref().filter(|v| v.intrinsic_width() > 0) {
                Some(video) => {
                    s.save();
                    s.translate(&graphene::Point::new(r.x(), r.y()));
                    video.snapshot(s, r.width() as f64, r.height() as f64);
                    s.restore();
                }
                None => bubble::draw_cover(s, &tex, &r),
            }
            s.pop();

            if self.loading.get() && t > 0.9 {
                // small spinner near the photo's top-left, kept inside the panel
                let (cx, cy) = ((r.x().max(0.0) + 30.0) as f64, (r.y().max(0.0) + 30.0) as f64);
                let angle = w.frame_clock().map_or(0.0, |c| c.frame_time() as f64 / 1_000_000.0 * 6.0);
                let cr = s.append_cairo(&graphene::Rect::new(cx as f32 - 16.0, cy as f32 - 16.0, 32.0, 32.0));
                cr.arc(cx, cy, 14.0, 0.0, std::f64::consts::TAU);
                cr.set_source_rgba(0.0, 0.0, 0.0, 0.55);
                let _ = cr.fill();
                cr.set_line_width(2.5);
                cr.set_line_cap(gtk::cairo::LineCap::Round);
                cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
                cr.arc(cx, cy, 8.0, angle, angle + 4.4);
                let _ = cr.stroke();
            }
        }
    }
}
