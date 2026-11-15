//! Full-size photo viewer that covers the conversation panel. Opens by zooming the clicked photo up
//! from where it sits in the chat while a black backdrop fades in; closes the same way in reverse.
//! Prev/next (buttons on hover, arrow keys) cycle through every loaded photo in the chat.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{glib, graphene};

use crate::bubble::{self, Bubble};

const OPEN_MS: f64 = 240.0;
const MARGIN_X: f32 = 84.0; // room for the prev/next buttons
const MARGIN_Y: f32 = 28.0;

pub struct Viewer {
    pub root: gtk::Overlay,
    canvas: Canvas,
    prev: gtk::Button,
    next: gtk::Button,
    /// the chat's photo widgets in display order, for prev/next and to zoom back into on close
    items: RefCell<Vec<Bubble>>,
    idx: Cell<usize>,
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
        root.set_focusable(true);
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

        let v = Rc::new(Self { root, canvas, prev, next, items: RefCell::default(), idx: Cell::new(0) });
        let w = v.clone();
        v.prev.connect_clicked(move |_| w.step(-1));
        let w = v.clone();
        v.next.connect_clicked(move |_| w.step(1));
        let w = v.clone();
        close.connect_clicked(move |_| w.close());

        // controls show while the pointer is over the viewer
        let motion = gtk::EventControllerMotion::new();
        let r = v.root.clone();
        motion.connect_enter(move |_, _, _| r.add_css_class("hover"));
        let r = v.root.clone();
        motion.connect_leave(move |_| r.remove_css_class("hover"));
        v.root.add_controller(motion);

        let click = gtk::GestureClick::new();
        let w = v.clone();
        click.connect_released(move |_, _, x, y| {
            if !w.canvas.photo_rect().contains_point(&graphene::Point::new(x as f32, y as f32)) {
                w.close(); // a click on the backdrop closes, like Quick Look
            }
        });
        v.canvas.add_controller(click);

        let keys = gtk::EventControllerKey::new();
        let w = v.clone();
        keys.connect_key_pressed(move |_, key, _, _| {
            match key {
                gtk::gdk::Key::Escape => w.close(),
                gtk::gdk::Key::Left => w.step(-1),
                gtk::gdk::Key::Right => w.step(1),
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        v.root.add_controller(keys);
        v
    }

    pub fn is_open(&self) -> bool {
        self.root.is_visible()
    }

    /// Open on `photo` (which must have its texture), cycling through `items`.
    pub fn open(&self, items: Vec<Bubble>, photo: &Bubble) {
        let Some(tex) = photo.texture() else { return };
        let idx = items.iter().position(|b| b == photo).unwrap_or(0);
        let many = items.len() > 1;
        self.prev.set_visible(many);
        self.next.set_visible(many);
        *self.items.borrow_mut() = items;
        self.idx.set(idx);
        let from = self.source_rect(photo);
        self.canvas.imp().tex.replace(Some(tex));
        self.canvas.imp().from.set(from);
        self.canvas.imp().t.set(0.0);
        self.root.set_visible(true);
        self.root.grab_focus();
        self.canvas.animate(1.0, || {});
    }

    pub fn close(&self) {
        if !self.is_open() || self.canvas.imp().closing.get() {
            return;
        }
        let cur = self.items.borrow().get(self.idx.get()).cloned();
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
        let root = self.root.clone();
        let canvas = self.canvas.clone();
        self.canvas.animate(0.0, move || {
            canvas.imp().closing.set(false);
            root.set_visible(false);
            root.remove_css_class("hover");
        });
    }

    /// Hide immediately (switching chats).
    pub fn close_now(&self) {
        self.root.set_visible(false);
        self.canvas.imp().closing.set(false);
        self.items.borrow_mut().clear();
    }

    fn step(&self, d: isize) {
        let items = self.items.borrow();
        let n = items.len() as isize;
        if n < 2 {
            return;
        }
        let i = (self.idx.get() as isize + d).rem_euclid(n) as usize;
        if let Some(t) = items[i].texture() {
            self.idx.set(i);
            self.canvas.imp().tex.replace(Some(t));
            self.canvas.queue_draw();
        }
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
    /// Where the photo sits when fully open: as large as fits, keeping its shape.
    fn fit_rect(&self) -> graphene::Rect {
        let (w, h) = (self.width() as f32, self.height() as f32);
        let Some(t) = self.imp().tex.borrow().clone() else { return graphene::Rect::new(0.0, 0.0, w, h) };
        let (tw, th) = (t.width().max(1) as f32, t.height().max(1) as f32);
        let k = ((w - 2.0 * MARGIN_X) / tw).min((h - 2.0 * MARGIN_Y) / th).max(0.01);
        let (dw, dh) = (tw * k, th * k);
        graphene::Rect::new((w - dw) / 2.0, (h - dh) / 2.0, dw, dh)
    }

    fn photo_rect(&self) -> graphene::Rect {
        let t = ease(self.imp().t.get()) as f32;
        let (a, b) = (self.imp().from.get(), self.fit_rect());
        graphene::Rect::new(lerp(a.x(), b.x(), t), lerp(a.y(), b.y(), t), lerp(a.width(), b.width(), t), lerp(a.height(), b.height(), t))
    }

    /// Animate `t` (0 = in the chat, 1 = open) towards `to`, then run `done`.
    fn animate(&self, to: f64, done: impl FnOnce() + 'static) {
        let start = self.imp().t.get();
        let began = Rc::new(Cell::new(None::<i64>));
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

    pub struct Canvas {
        pub tex: RefCell<Option<gtk::gdk::Texture>>,
        pub from: Cell<graphene::Rect>,
        pub t: Cell<f64>,
        pub closing: Cell<bool>,
    }

    impl Default for Canvas {
        fn default() -> Self {
            Self { tex: RefCell::default(), from: Cell::new(graphene::Rect::zero()), t: Cell::new(0.0), closing: Cell::new(false) }
        }
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
            s.append_color(&gtk::gdk::RGBA::new(0.0, 0.0, 0.0, t), &graphene::Rect::new(0.0, 0.0, width, height));
            let Some(tex) = self.tex.borrow().clone() else { return };
            let r = w.photo_rect();
            // corners go from the chat's rounded card to nearly square as it opens
            let radius = lerp(18.0, 4.0, t);
            s.push_rounded_clip(&gtk::gsk::RoundedRect::from_rect(r, radius));
            bubble::draw_cover(s, &tex, &r);
            s.pop();
        }
    }
}
