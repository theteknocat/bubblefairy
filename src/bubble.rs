//! A message bubble drawn as one vector shape (body + tail), so the gradient, shadow, highlight and
//! border all follow the tail. GTK CSS has no clip-path or pseudo-elements, hence the custom drawing.
//! Padding and text colour still come from CSS (`.bubble` classes); only the background is drawn here.

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};

/// Width of the gutter on the tail side, kept on every bubble so a run of bubbles lines up.
pub const TAIL: f32 = 8.0;
const R: f32 = 18.0; // corner radius
const R_JOIN: f32 = 6.0; // corner facing a neighbouring bubble from the same sender

#[derive(Clone, Copy, Default)]
pub struct Look {
    pub me: bool,
    pub sms: bool,
    pub tail: bool,
    pub join_prev: bool,
    pub join_next: bool,
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

    /// Add content (text, file rows) inside the bubble's padding.
    pub fn add(&self, w: &impl IsA<gtk::Widget>) {
        if let Some(inner) = self.first_child().and_downcast::<gtk::Box>() {
            inner.append(w);
        }
    }
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
    let rr = |join: bool| if join { R_JOIN.min(max) } else { R.min(max) };
    // the side facing the bubble's own edge tucks in where it joins its neighbours
    let (tl, tr, br, bl) = (rr(false), rr(look.join_prev), rr(look.join_next), rr(false));
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
    use std::cell::Cell;

    #[derive(Default)]
    pub struct Bubble {
        pub look: Cell<Look>,
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
            if width > TAIL && height > 0.0 {
                let look = self.look.get();
                let dark = w.root().is_some_and(|r| r.has_css_class("dark"));
                let (top, bottom, shadow, hi, border) = palette(look, dark);
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
