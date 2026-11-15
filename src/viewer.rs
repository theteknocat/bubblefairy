//! Full-size photo viewer covering the conversation panel. Opens by zooming the clicked photo up from
//! its spot in the chat while a black backdrop fades in, and closes the same way in reverse.
//! - The photo fills the panel on its longest side. The chat's 1200 px copy shows at once; when the
//!   original is bigger, the full version is fetched and swapped in (small spinner at its top-left).
//! - Prev/next (buttons, ← →, mouse wheel) walk every loaded photo in the chat and stop at the ends.
//!   Near the earliest photo it asks the chat to load older messages so more photos can follow.
//! - Controls overlay the photo and fade out after a few seconds without mouse movement.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene};

use crate::bubble::{self, Bubble};

const OPEN_MS: f64 = 240.0;
const HIDE_AFTER: Duration = Duration::from_millis(2500);
const PRELOAD_WITHIN: usize = 2; // start loading older messages this many photos from the start

/// What the viewer needs from the chat.
pub struct Hooks {
    /// fetch the big version of this photo; the answer arrives via `full_ready` / `full_failed`
    pub fetch_full: Box<dyn Fn(&str)>,
    /// whether older messages may still exist
    pub has_older: Box<dyn Fn() -> bool>,
    pub load_older: Box<dyn Fn()>,
}

pub struct Viewer {
    pub root: gtk::Overlay,
    canvas: Canvas,
    prev: gtk::Button,
    next: gtk::Button,
    older_spin: gtk::Spinner,
    /// (path, widget) for every photo in the chat, in display order
    items: RefCell<Vec<(String, Bubble)>>,
    idx: Cell<usize>,
    full: RefCell<HashMap<String, gdk::Texture>>,
    hooks: RefCell<Option<Hooks>>,
    hide_gen: Rc<Cell<u32>>,
}

impl Viewer {
    pub fn new() -> Rc<Self> {
        let canvas = Canvas::default();
        canvas.set_hexpand(true);
        canvas.set_vexpand(true);
        let root = gtk::Overlay::new();
        root.set_child(Some(&canvas));
        root.add_css_class("viewer");
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
        let older_spin = gtk::Spinner::new();
        older_spin.add_css_class("viewer-older");
        older_spin.set_halign(gtk::Align::Start);
        older_spin.set_valign(gtk::Align::Center);
        older_spin.set_size_request(28, 28);
        older_spin.set_tooltip_text(Some("Loading earlier messages…"));
        older_spin.set_visible(false);
        root.add_overlay(&older_spin);

        let v = Rc::new(Self {
            root,
            canvas,
            prev,
            next,
            older_spin,
            items: RefCell::default(),
            idx: Cell::new(0),
            full: RefCell::default(),
            hooks: RefCell::default(),
            hide_gen: Rc::default(),
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
        self.canvas.imp().from.set(from);
        self.canvas.imp().closing.set(true);
        self.canvas.set_loading(false);
        self.older_spin.set_visible(false);
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
        let older = hooks.as_ref().is_some_and(|h| (h.has_older)());
        self.prev.set_visible(i > 0);
        self.next.set_visible(i + 1 < n);
        // at the very start with more history to come: a spinner where "previous" would be
        let waiting = i == 0 && older;
        self.older_spin.set_visible(waiting);
        self.older_spin.set_spinning(waiting);
        if i < PRELOAD_WITHIN && older && let Some(h) = hooks.as_ref() {
            (h.load_older)(); // no-op while a load is already running
        }
    }

    fn show_controls(&self) {
        self.root.add_css_class("controls");
        let g = self.hide_gen.get().wrapping_add(1);
        self.hide_gen.set(g);
        let (root, gen_now) = (self.root.clone(), self.hide_gen.clone());
        glib::timeout_add_local_once(HIDE_AFTER, move || {
            if gen_now.get() == g {
                root.remove_css_class("controls"); // no movement since this timer started
            }
        });
    }

    /// Where the photo sits in the chat, in the viewer's coordinates (the viewer covers its parent).
    fn source_rect(&self, b: &Bubble) -> graphene::Rect {
        let parent = self.root.parent().unwrap_or_else(|| self.root.clone().upcast());
        let body = b.body();
        b.compute_point(&parent, &graphene::Point::new(body.x(), body.y()))
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
        let Some(t) = self.imp().tex.borrow().clone() else { return graphene::Rect::new(0.0, 0.0, w, h) };
        let (tw, th) = (t.width().max(1) as f32, t.height().max(1) as f32);
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
            bubble::draw_cover(s, &tex, &r);
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
