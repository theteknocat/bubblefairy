//! A message bubble drawn as one vector shape (body + tail), so the gradient, shadow, highlight and
//! border all follow the tail. GTK CSS has no clip-path or pseudo-elements, hence the custom drawing.
//! Padding and text colour still come from CSS (`.bubble` classes); only the background is drawn here.

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};

/// Width of the gutter on the tail side, kept on every bubble so a run of bubbles lines up.
pub const TAIL: f32 = 8.0;
const R: f32 = 18.0; // corner radius

#[derive(Clone, Copy, Default)]
pub struct Look {
    pub me: bool,
    pub sms: bool,
    pub tail: bool,
}

glib::wrapper! {
    pub struct Bubble(ObjectSubclass<imp::Bubble>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Orientable, gtk::Buildable, gtk::Accessible, gtk::ConstraintTarget;
}

impl Bubble {
    pub fn new(look: Look) -> Self {
        let b: Self = glib::Object::builder().property("orientation", gtk::Orientation::Vertical).property("spacing", 6).build();
        b.imp().look.set(look);
        // inner spacing set here rather than in CSS: CSS padding didn't apply to this subclass
        let inner = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let side = 14 + TAIL as i32;
        inner.set_margin_top(8);
        inner.set_margin_bottom(8);
        inner.set_margin_start(if look.me { 14 } else { side });
        inner.set_margin_end(if look.me { side } else { 14 });
        BoxExt::append(&b, &inner);
        b.add_css_class("bubble");
        b.add_css_class(if look.me { "me" } else { "them" });
        b.add_css_class(if look.sms { "sms" } else { "imessage" });
        b
    }

    /// A photo drawn through the bubble outline: rounded like a bubble, and with a tail (showing a
    /// sliver of the photo) when it ends a run. `w`/`h` are the photo's display size.
    pub fn photo(look: Look, w: i32, h: i32) -> Self {
        let b: Self = glib::Object::new();
        b.imp().look.set(look);
        b.imp().is_photo.set(true);
        b.set_photo_size(w, h);
        b.add_css_class("photo");
        b
    }

    /// A link preview: one bubble, off-white whoever sent it, with the preview image (if `img_h` > 0)
    /// across the top, clipped to the bubble's outline, and the title and site below it.
    /// Set the picture with `set_texture`; content goes in with `add`.
    pub fn card(look: Look, w: i32, img_h: i32) -> Self {
        let b = Self::new(look);
        b.imp().is_card.set(true);
        b.imp().card_h.set(img_h as f32);
        b.set_size_request(w + TAIL as i32, -1);
        if let Some(inner) = b.first_child().and_downcast::<gtk::Box>()
            && img_h > 0
        {
            inner.set_margin_top(img_h + 10);
        }
        b.add_css_class("linkcard");
        b
    }

    /// A small square picture with its own rounded corners and no tail (a link's logo or icon).
    pub fn thumb(size: i32) -> Self {
        let b: Self = glib::Object::new();
        b.imp().is_photo.set(true);
        b.imp().flat.set(true);
        b.set_size_request(size, size);
        b.set_valign(gtk::Align::Center);
        b.add_css_class("photo");
        b
    }

    pub fn set_photo_size(&self, w: i32, h: i32) {
        self.set_size_request(w + TAIL as i32, h);
    }

    pub fn set_texture(&self, t: Option<&gdk::Texture>) {
        self.imp().tex.replace(t.cloned());
        self.queue_draw();
    }

    pub fn texture(&self) -> Option<gdk::Texture> {
        self.imp().tex.borrow().clone()
    }

    /// The photo's area without the tail gutter, in this widget's coordinates.
    pub fn body(&self) -> graphene::Rect {
        let x = if self.imp().look.get().me { 0.0 } else { TAIL };
        graphene::Rect::new(x, 0.0, self.width() as f32 - TAIL, self.height() as f32)
    }

    /// Add content (text, file rows) inside the bubble's padding.
    pub fn add(&self, w: &impl IsA<gtk::Widget>) {
        if let Some(inner) = self.first_child().and_downcast::<gtk::Box>() {
            inner.append(w);
        }
    }
}

/// Draw `t` so it fills `r` (cropping the overflow), like CSS `object-fit: cover`.
pub fn draw_cover(s: &gtk::Snapshot, t: &gdk::Texture, r: &graphene::Rect) {
    let (tw, th) = (t.width().max(1) as f32, t.height().max(1) as f32);
    let k = (r.width() / tw).max(r.height() / th);
    let (dw, dh) = (tw * k, th * k);
    s.push_clip(r);
    s.save();
    s.translate(&graphene::Point::new(r.x() + (r.width() - dw) / 2.0, r.y() + (r.height() - dh) / 2.0));
    t.snapshot(s, dw as f64, dh as f64);
    s.restore();
    s.pop();
}

/// Link-preview cards: soft off-white in light and dark mode alike (dark grey text, set in CSS).
fn card_palette() -> (gdk::RGBA, gdk::RGBA, gdk::RGBA, f32, Option<gdk::RGBA>) {
    (rgba(0xfbfaf6, 1.0), rgba(0xeeece6, 1.0), rgba(0x0a0528, 0.30), 0.7, Some(rgba(0x000000, 0.10)))
}

fn rgba(hex: u32, a: f32) -> gdk::RGBA {
    gdk::RGBA::new(((hex >> 16) & 255) as f32 / 255.0, ((hex >> 8) & 255) as f32 / 255.0, (hex & 255) as f32 / 255.0, a)
}

/// (gradient top, gradient bottom, shadow, highlight alpha, border)
fn palette(look: Look, dark: bool) -> (gdk::RGBA, gdk::RGBA, gdk::RGBA, f32, Option<gdk::RGBA>) {
    match (look.me, look.sms, dark) {
        (true, false, _) => (rgba(0x4ea8ff, 1.0), rgba(0x2166ff, 1.0), rgba(0x2166ff, 0.30), 0.45, None),
        (true, true, _) => (rgba(0x52df8c, 1.0), rgba(0x16ad57, 1.0), rgba(0x16ad57, 0.30), 0.45, None),
        (false, _, false) => (rgba(0xffffff, 1.0), rgba(0xf2f0fa, 1.0), rgba(0x4c3c96, 0.16), 0.6, Some(rgba(0x7a6eb4, 0.20))),
        (false, _, true) => (rgba(0x544d93, 1.0), rgba(0x453f80, 1.0), rgba(0x08041e, 0.40), 0.18, Some(rgba(0xffffff, 0.10))),
    }
}

/// The outline for a bubble whose tail sits on the right (`me`); `them` is the mirror image.
pub fn outline(look: Look, w: f32, h: f32) -> gsk::Path {
    let k = 0.5523; // cubic approximation of a quarter circle
    let x = |v: f32| if look.me { v } else { w - v };
    let (x0, x1, y0, y1) = (0.0, w - TAIL, 0.0, h);
    let max = (h / 2.0).min((x1 - x0) / 2.0);
    let r = R.min(max); // every corner the same, whatever the neighbours
    let (tl, tr, br, bl) = (r, r, r, r);
    let p = gsk::PathBuilder::new();
    p.move_to(x(x0 + tl), y0);
    p.line_to(x(x1 - tr), y0);
    p.cubic_to(x(x1 - tr + tr * k), y0, x(x1), y0 + tr - tr * k, x(x1), y0 + tr);
    if look.tail {
        let start = (y1 - 16.0).max(y0 + tr);
        p.line_to(x(x1), start);
        // outer edge flicks out to the tip, the underside curls back into the bottom edge
        p.cubic_to(x(x1), y1 - 6.0, x(x1 + 2.5), y1 - 1.5, x(x1 + TAIL), y1);
        p.cubic_to(x(x1 + 2.0), y1 + 0.4, x(x1 - 4.0), y1 - 1.0, x(x1 - 9.0), y1);
    } else {
        p.line_to(x(x1), y1 - br);
        p.cubic_to(x(x1), y1 - br + br * k, x(x1 - br + br * k), y1, x(x1 - br), y1);
    }
    p.line_to(x(x0 + bl), y1);
    p.cubic_to(x(x0 + bl - bl * k), y1, x(x0), y1 - bl + bl * k, x(x0), y1 - bl);
    p.line_to(x(x0), y0 + tl);
    p.cubic_to(x(x0), y0 + tl - tl * k, x(x0 + tl - tl * k), y0, x(x0 + tl), y0);
    p.close();
    p.to_path()
}

mod imp {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Default)]
    pub struct Bubble {
        pub look: Cell<Look>,
        pub is_photo: Cell<bool>,
        pub is_card: Cell<bool>,
        pub flat: Cell<bool>,
        pub card_h: Cell<f32>,
        pub tex: RefCell<Option<gdk::Texture>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Bubble {
        const NAME: &'static str = "BfBubble";
        type Type = super::Bubble;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for Bubble {}
    impl BoxImpl for Bubble {}

    impl WidgetImpl for Bubble {
        fn snapshot(&self, s: &gtk::Snapshot) {
            let w = self.obj();
            let (width, height) = (w.width() as f32, w.height() as f32);
            if self.flat.get() && width > 0.0 && height > 0.0 {
                let bounds = graphene::Rect::new(0.0, 0.0, width, height);
                let b = gsk::PathBuilder::new();
                b.add_rounded_rect(&gsk::RoundedRect::from_rect(bounds, 12.0));
                let path = b.to_path();
                s.push_fill(&path, gsk::FillRule::Winding);
                s.append_color(&rgba(0xffffff, 1.0), &bounds); // a white tile, so a transparent logo reads
                if let Some(t) = self.tex.borrow().as_ref() {
                    // the whole logo, never cropped, with a little margin
                    let (tw, th) = (t.width().max(1) as f32, t.height().max(1) as f32);
                    let k = ((width - 8.0) / tw).min((height - 8.0) / th);
                    s.save();
                    s.translate(&graphene::Point::new((width - tw * k) / 2.0, (height - th * k) / 2.0));
                    t.snapshot(s, (tw * k) as f64, (th * k) as f64);
                    s.restore();
                }
                s.pop();
                s.append_stroke(&path, &gsk::Stroke::new(1.0), &rgba(0x000000, 0.12));
                return;
            }
            if self.is_photo.get() && width > TAIL && height > 0.0 {
                let look = self.look.get();
                let path = outline(look, width, height);
                let bounds = graphene::Rect::new(0.0, 0.0, width, height);
                s.push_shadow(&[gsk::Shadow::new(rgba(0x0a0528, 0.35), 0.0, 4.0, 14.0)]);
                s.push_fill(&path, gsk::FillRule::Winding);
                match self.tex.borrow().as_ref() {
                    // cover the whole widget, gutter included, so the tail shows a sliver of the photo
                    Some(t) => draw_cover(s, t, &bounds),
                    None => s.append_color(&rgba(0x8a7cc8, 0.22), &bounds),
                }
                s.pop();
                s.pop();
                s.push_fill(&path, gsk::FillRule::Winding);
                s.save();
                s.translate(&graphene::Point::new(0.0, 1.2));
                s.append_stroke(&path, &gsk::Stroke::new(1.5), &gdk::RGBA::new(1.0, 1.0, 1.0, 0.30));
                s.restore();
                s.pop();
                s.append_stroke(&path, &gsk::Stroke::new(1.0), &gdk::RGBA::new(1.0, 1.0, 1.0, 0.22));
                return;
            }
            if width > TAIL && height > 0.0 {
                let look = self.look.get();
                let dark = w.root().is_some_and(|r| r.has_css_class("dark"));
                let (top, bottom, shadow, hi, border) = if self.is_card.get() { card_palette() } else { palette(look, dark) };
                let path = outline(look, width, height);
                let bounds = graphene::Rect::new(0.0, 0.0, width, height);

                s.push_shadow(&[gsk::Shadow::new(shadow, 0.0, 3.0, 10.0)]);
                s.push_fill(&path, gsk::FillRule::Winding);
                s.append_linear_gradient(
                    &bounds,
                    &graphene::Point::new(0.0, 0.0),
                    &graphene::Point::new(0.0, height),
                    &[gsk::ColorStop::new(0.0, top), gsk::ColorStop::new(1.0, bottom)],
                );
                s.pop();
                s.pop();

                if self.is_card.get() && self.card_h.get() > 0.0 {
                    // the preview image across the top, shaped by the bubble's outline
                    let x = if look.me { 0.0 } else { TAIL };
                    let r = graphene::Rect::new(x, 0.0, width - TAIL, self.card_h.get());
                    s.push_fill(&path, gsk::FillRule::Winding);
                    match self.tex.borrow().as_ref() {
                        Some(t) => draw_cover(s, t, &r),
                        None => s.append_color(&rgba(0x8a7cc8, 0.22), &r),
                    }
                    s.pop();
                }

                // glassy rim: the outline nudged down 1px and clipped to the shape lights the top edge
                s.push_fill(&path, gsk::FillRule::Winding);
                s.save();
                s.translate(&graphene::Point::new(0.0, 1.2));
                s.append_stroke(&path, &gsk::Stroke::new(1.5), &gdk::RGBA::new(1.0, 1.0, 1.0, hi));
                s.restore();
                s.pop();

                if let Some(b) = border {
                    s.append_stroke(&path, &gsk::Stroke::new(1.0), &b);
                }
            }
            self.parent_snapshot(s);
        }
    }
}
