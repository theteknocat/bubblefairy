//! The chat's scrollable content, anchored to its bottom edge instead of its top.
//!
//! GTK's Viewport keeps the scroll value (the distance from the top) when its content or size changes. In a
//! chat that makes everything jump: older messages loaded above, a picture arriving, or the composer growing
//! all shift what you're looking at, and fixing it afterwards happens a frame late. Here the distance from
//! the bottom is what's kept, and it's applied inside the same layout pass, so nothing visibly moves:
//! - at the bottom ("stuck") the view stays at the bottom as content grows;
//! - scrolled up, the gap between the view's bottom edge and the content's end stays the same.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{glib, graphene, gsk};

glib::wrapper! {
    pub struct ChatScroll(ObjectSubclass<imp::ChatScroll>)
        @extends gtk::Widget,
        @implements gtk::Scrollable, gtk::Buildable, gtk::Accessible, gtk::ConstraintTarget;
}

impl ChatScroll {
    /// `stuck` is shared with the window: true while the view follows the newest message.
    pub fn new(child: &impl IsA<gtk::Widget>, stuck: Rc<Cell<bool>>) -> Self {
        let s: Self = glib::Object::new();
        s.imp().stuck.replace(stuck);
        child.set_parent(&s);
        s.imp().child.replace(Some(child.clone().upcast()));
        s
    }

    /// The next layout keeps the view where it is (distance from the top) instead of the distance from the
    /// bottom: for messages added below what's showing.
    pub fn hold(&self) {
        self.imp().hold.set(true);
        self.queue_allocate();
    }
}

mod imp {
    use super::*;

    /// within this distance of the bottom counts as "at the bottom"
    const STICK_PX: f64 = 40.0;

    #[derive(glib::Properties)]
    #[properties(wrapper_type = super::ChatScroll)]
    pub struct ChatScroll {
        #[property(override_interface = gtk::Scrollable, get, set = Self::set_hadjustment, nullable)]
        pub hadjustment: RefCell<Option<gtk::Adjustment>>,
        #[property(override_interface = gtk::Scrollable, get, set = Self::set_vadjustment, nullable)]
        pub vadjustment: RefCell<Option<gtk::Adjustment>>,
        #[property(override_interface = gtk::Scrollable, get, set)]
        pub hscroll_policy: Cell<gtk::ScrollablePolicy>,
        #[property(override_interface = gtk::Scrollable, get, set)]
        pub vscroll_policy: Cell<gtk::ScrollablePolicy>,
        pub child: RefCell<Option<gtk::Widget>>,
        pub stuck: RefCell<Rc<Cell<bool>>>,
        /// distance between the view's bottom edge and the end of the content
        pub gap: Cell<f64>,
        /// we are changing the adjustment ourselves (not the user scrolling)
        pub adjusting: Cell<bool>,
        pub hold: Cell<bool>,
    }

    impl Default for ChatScroll {
        fn default() -> Self {
            Self {
                hadjustment: RefCell::default(),
                vadjustment: RefCell::default(),
                hscroll_policy: Cell::new(gtk::ScrollablePolicy::Minimum),
                vscroll_policy: Cell::new(gtk::ScrollablePolicy::Minimum),
                child: RefCell::default(),
                stuck: RefCell::default(),
                gap: Cell::new(0.0),
                adjusting: Cell::new(false),
                hold: Cell::new(false),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ChatScroll {
        const NAME: &'static str = "BfChatScroll";
        type Type = super::ChatScroll;
        type ParentType = gtk::Widget;
        type Interfaces = (gtk::Scrollable,);
    }

    impl ChatScroll {
        fn set_hadjustment(&self, adj: Option<gtk::Adjustment>) {
            self.hadjustment.replace(adj);
            self.obj().queue_allocate();
        }

        fn set_vadjustment(&self, adj: Option<gtk::Adjustment>) {
            if let Some(a) = &adj {
                let w = self.obj().downgrade();
                a.connect_value_changed(move |a| {
                    let Some(w) = w.upgrade() else { return };
                    let me = w.imp();
                    if !me.adjusting.get() {
                        // the user scrolled: remember where they are
                        let gap = a.upper() - a.value() - a.page_size();
                        me.gap.set(gap.max(0.0));
                        me.stuck.borrow().set(gap <= STICK_PX);
                    }
                    w.queue_allocate();
                });
            }
            self.vadjustment.replace(adj);
            self.obj().queue_allocate();
        }
    }

    #[glib::derived_properties]
    impl ObjectImpl for ChatScroll {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().set_overflow(gtk::Overflow::Hidden); // the content is allocated taller than the view
        }

        fn dispose(&self) {
            if let Some(c) = self.child.take() {
                c.unparent();
            }
        }
    }

    impl ScrollableImpl for ChatScroll {}

    impl WidgetImpl for ChatScroll {
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let Some(c) = self.child.borrow().clone() else { return (0, 0, -1, -1) };
            if orientation == gtk::Orientation::Horizontal {
                let (min, nat, _, _) = c.measure(orientation, for_size);
                (min, nat, -1, -1)
            } else {
                (0, 0, -1, -1) // scrolled: any height will do
            }
        }

        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            let Some(c) = self.child.borrow().clone() else { return };
            // natural height: pictures can shrink to nothing, so their minimum would understate the content
            let (_, nat, _, _) = c.measure(gtk::Orientation::Vertical, width);
            let page = height as f64;
            let upper = (nat as f64).max(page);
            let max = upper - page;
            if self.hold.replace(false)
                && let Some(a) = self.vadjustment.borrow().as_ref()
            {
                self.gap.set((max - a.value()).max(0.0));
                self.stuck.borrow().set(false);
            }
            let value = if self.stuck.borrow().get() { max } else { (max - self.gap.get()).clamp(0.0, max) };
            // the gap is only ever changed by the user's scrolling: while the content is briefly shorter (a re-render)
            // the view is clamped, but the gap to return to is kept

            self.adjusting.set(true);
            if let Some(a) = self.vadjustment.borrow().as_ref() {
                a.configure(value, 0.0, upper, page * 0.1, page * 0.9, page);
            }
            if let Some(a) = self.hadjustment.borrow().as_ref() {
                a.configure(0.0, 0.0, width as f64, 1.0, 1.0, width as f64);
            }
            self.adjusting.set(false);
            // configure may have settled on a slightly different value (clamping)
            let value = self.vadjustment.borrow().as_ref().map_or(value, |a| a.value());
            c.allocate(width, upper.ceil() as i32, -1, Some(gsk::Transform::new().translate(&graphene::Point::new(0.0, -value as f32))));
        }
    }
}
