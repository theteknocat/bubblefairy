//! Rounded "pills" behind search matches. GtkLabel can't be subclassed, so the first time a label is highlighted it
//! is moved into an Overlay whose main child is a drawing area that paints the pills underneath the label. The
//! label keeps its own size and wrapping (`measure_overlay`), so nothing about the text's flow changes.

use std::cell::RefCell;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{glib, graphene};

glib::wrapper! {
    /// Holds one label and paints pills under it; its layout is a plain pass-through of the label's.
    pub struct Host(ObjectSubclass<imp::Host>) @extends gtk::Widget, @implements gtk::Buildable, gtk::Accessible, gtk::ConstraintTarget;
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Host {
        pub label: RefCell<Option<gtk::Label>>,
        pub pills: RefCell<(Vec<(usize, usize)>, (f64, f64, f64))>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Host {
        const NAME: &'static str = "BfPillHost";
        type Type = super::Host;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_layout_manager_type::<gtk::BinLayout>();
        }
    }

    impl ObjectImpl for Host {
        fn dispose(&self) {
            if let Some(l) = self.label.take() {
                l.unparent();
            }
        }
    }

    impl WidgetImpl for Host {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let (host, label) = (self.obj(), self.label.borrow().clone());
            if let Some(l) = label.as_ref().filter(|_| !self.pills.borrow().0.is_empty()) {
                let cr = snapshot.append_cairo(&graphene::Rect::new(0.0, 0.0, host.width() as f32, host.height() as f32));
                draw(host.upcast_ref(), l, &cr, &self.pills.borrow());
            }
            self.parent_snapshot(snapshot);
        }
    }
}

/// Paint pills behind these byte ranges of the label's text (empty: none). Labels not inside a Box that can be
/// wrapped return false, and the caller falls back to plain background attributes.
pub fn set_pills(l: &gtk::Label, ranges: Vec<(usize, usize)>, rgb: (f64, f64, f64)) -> bool {
    let host = match l.parent().and_then(|p| p.downcast::<Host>().ok()) {
        Some(h) => h,
        None if ranges.is_empty() => return true,
        None => match wrap(l) {
            Some(h) => h,
            None => return false,
        },
    };
    *host.imp().pills.borrow_mut() = (ranges, rgb);
    host.queue_draw();
    true
}

fn wrap(l: &gtk::Label) -> Option<Host> {
    let parent = l.parent()?.downcast::<gtk::Box>().ok()?;
    let host: Host = glib::Object::new();
    // The host stands in for the label in its row, so it takes over the label's placement.
    host.set_halign(l.halign());
    host.set_valign(l.valign());
    host.set_hexpand(l.hexpands());
    host.set_vexpand(l.vexpands());
    host.set_margin_start(l.margin_start());
    host.set_margin_end(l.margin_end());
    host.set_margin_top(l.margin_top());
    host.set_margin_bottom(l.margin_bottom());
    let before = l.prev_sibling();
    parent.remove(l);
    l.set_margin_start(0);
    l.set_margin_end(0);
    l.set_margin_top(0);
    l.set_margin_bottom(0);
    l.set_parent(&host);
    *host.imp().label.borrow_mut() = Some(l.clone());
    parent.insert_child_after(&host, before.as_ref());
    Some(host)
}

fn draw(area: &gtk::Widget, l: &gtk::Label, cr: &gtk::cairo::Context, pills: &(Vec<(usize, usize)>, (f64, f64, f64))) {
    if pills.0.is_empty() {
        return;
    }
    let Some(origin) = l.compute_point(area, &graphene::Point::new(0.0, 0.0)) else { return };
    let (ox, oy) = l.layout_offsets();
    let (ox, oy) = (origin.x() as f64 + ox as f64, origin.y() as f64 + oy as f64);
    let layout = l.layout();
    let scale = gtk::pango::SCALE as f64;
    let (aw, ah) = (area.width() as f64, area.height() as f64);
    // With an ellipsis, a match cut off by it (or hidden behind it) must get no pill: what shows is not the whole word.
    // Its true width, from the same text laid out without the ellipsis, tells.
    let full = layout.is_ellipsized().then(|| {
        let f = layout.copy();
        f.set_ellipsize(gtk::pango::EllipsizeMode::None);
        f.set_width(-1);
        f
    });
    let mut iter = layout.iter();
    loop {
        let Some(line) = iter.line_readonly() else { break };
        let (line_start, line_end) = (line.start_index(), line.start_index() + line.length());
        let (top, bottom) = iter.line_yrange();
        for &(s, e) in &pills.0 {
            let (s, e) = ((s as i32).max(line_start), (e as i32).min(line_end));
            if s >= e {
                continue;
            }
            if let Some(f) = &full {
                let width = |l: &gtk::pango::LayoutLine| l.x_ranges(s, e).chunks(2).map(|p| p[p.len() - 1] - p[0]).sum::<i32>();
                let whole = f.line_readonly(0).map(|fl| width(&fl)).unwrap_or(0);
                if (width(&line) - whole).abs() > gtk::pango::SCALE / 2 || whole == 0 {
                    continue;
                }
            }
            for pair in line.x_ranges(s, e).chunks(2) {
                let (x1, x2) = (pair[0], pair[pair.len() - 1]);
                // A little wider and taller than the text, but never past the label's own box: its row clips there.
                let (x0, y0) = ((ox + x1 as f64 / scale - 3.0).max(0.0), (oy + top as f64 / scale - 1.0).max(0.0));
                let (x1_, y1_) = ((ox + x2 as f64 / scale + 3.0).min(aw), (oy + bottom as f64 / scale + 1.0).min(ah));
                let (x, y, w, h) = (x0, y0, x1_ - x0, y1_ - y0);
                let r = h / 2.0;
                let path = |dy: f64, grow: f64| {
                    let (x, y, w, h, r) = (x - grow, y + dy - grow, w + 2.0 * grow, h + 2.0 * grow, r + grow);
                    cr.new_sub_path();
                    cr.arc(x + w - r, y + r, r, -std::f64::consts::FRAC_PI_2, std::f64::consts::FRAC_PI_2);
                    cr.arc(x + r, y + r, r, std::f64::consts::FRAC_PI_2, 3.0 * std::f64::consts::FRAC_PI_2);
                    cr.close_path();
                };
                // A soft shadow: a few faint, growing, lowered copies under the pill (cairo has no blur).
                cr.set_source_rgba(0.05, 0.02, 0.2, 0.14);
                for grow in [2.0, 1.0, 0.0] {
                    path(1.5, grow);
                    let _ = cr.fill();
                }
                cr.set_source_rgb(pills.1 .0, pills.1 .1, pills.1 .2);
                path(0.0, 0.0);
                let _ = cr.fill();
            }
        }
        if !iter.next_line() {
            break;
        }
    }
}
