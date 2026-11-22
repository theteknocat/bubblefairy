//! The pinned chats' tiles: a grid of up to three columns (one when the sidebar is narrow) whose tiles can be dragged
//! into a new order. The dragged tile follows the pointer; the others slide out of its way and open a gap where it
//! would land; letting go settles it there.

use std::cell::{Cell, RefCell};

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{glib, graphene, gsk};

glib::wrapper! {
    pub struct TileGrid(ObjectSubclass<imp::TileGrid>)
        @extends gtk::Widget,
        @implements gtk::Buildable, gtk::Accessible, gtk::ConstraintTarget;
}

impl Default for TileGrid {
    fn default() -> Self {
        Self::new()
    }
}

impl TileGrid {
    pub fn new() -> Self {
        let g: Self = glib::Object::new();
        g.install_drag();
        g
    }

    pub fn append(&self, w: &impl IsA<gtk::Widget>) {
        let w = w.clone().upcast::<gtk::Widget>();
        w.set_parent(self);
        self.imp().items.borrow_mut().push(imp::Item { w, pos: (0.0, 0.0), placed: false });
        self.queue_resize();
    }

    pub fn remove_all(&self) {
        self.imp().stop_drag();
        for it in self.imp().items.take() {
            it.w.unparent();
        }
        self.queue_resize();
    }

    /// The tiles in their present order.
    pub fn tiles(&self) -> Vec<gtk::Widget> {
        self.imp().items.borrow().iter().map(|i| i.w.clone()).collect()
    }

    /// Called with the tiles in their new order after one was dragged somewhere else.
    pub fn connect_reordered(&self, f: impl Fn(Vec<gtk::Widget>) + 'static) {
        self.imp().on_reorder.replace(Some(Box::new(f)));
    }

    /// Dev aid (`BF_TILE_DRAG`): drag tile `from` right by `cells` slots over half a second, hold, then let go.
    pub fn dev_drag(&self, from: usize, cells: f64) {
        let imp = self.imp();
        let (cw, ch) = imp.cell_size();
        let (x0, y0) = (cw * (from % imp.col_count()) as f64 + cw / 2.0, ch * (from / imp.col_count()) as f64 + ch / 2.0);
        imp.press.set(Some((from, x0, y0)));
        imp.start_drag(self, from, x0, y0);
        let (me, n) = (self.downgrade(), Cell::new(0));
        glib::timeout_add_local(std::time::Duration::from_millis(16), move || {
            let Some(me) = me.upgrade() else { return glib::ControlFlow::Break };
            n.set(n.get() + 1);
            let f = (n.get() as f64 / 30.0).min(1.0);
            me.imp().drag_to(&me, x0 + cells * cw * f, y0);
            if n.get() == 200 {
                me.imp().end_drag(&me);
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    }

    fn install_drag(&self) {
        let drag = gtk::GestureDrag::new();
        drag.set_propagation_phase(gtk::PropagationPhase::Capture);
        let me = self.downgrade();
        drag.connect_drag_begin(move |_, x, y| {
            if let Some(me) = me.upgrade() {
                me.imp().press.set(me.imp().item_at(x, y).map(|i| (i, x, y)));
            }
        });
        let me = self.downgrade();
        drag.connect_drag_update(move |g, dx, dy| {
            let Some(me) = me.upgrade() else { return };
            let imp = me.imp();
            let Some((i, x0, y0)) = imp.press.get() else { return };
            if !imp.dragging() {
                if dx * dx + dy * dy < 36.0 {
                    return; // still a click
                }
                g.set_state(gtk::EventSequenceState::Claimed); // the tile's own click is off
                imp.start_drag(&me, i, x0, y0);
            }
            imp.drag_to(&me, x0 + dx, y0 + dy);
        });
        let me = self.downgrade();
        drag.connect_drag_end(move |_, _, _| {
            if let Some(me) = me.upgrade() {
                me.imp().press.set(None);
                me.imp().end_drag(&me);
            }
        });
        self.add_controller(drag);
    }
}

mod imp {
    use super::*;

    pub struct Item {
        pub w: gtk::Widget,
        /// where it is drawn now (slides toward its slot)
        pub pos: (f64, f64),
        pub placed: bool,
    }

    struct Drag {
        /// the dragged tile's place in `items` (changes as it passes others)
        at: usize,
        /// where in the tile the pointer took hold
        grab: (f64, f64),
        /// the order when the drag began
        before: Vec<gtk::Widget>,
        pointer: (f64, f64),
    }

    #[derive(Default)]
    pub struct TileGrid {
        pub items: RefCell<Vec<Item>>,
        pub press: Cell<Option<(usize, f64, f64)>>,
        drag: RefCell<Option<Drag>>,
        /// columns, cell width and height as last laid out
        cols: Cell<usize>,
        /// the width it was last laid out at: a hidden widget reports 0, and asked for a height then it counted one column
        /// (a grid several tiles too tall once it was shown again)
        last_w: Cell<i32>,
        cell: Cell<(f64, f64)>,
        tick: RefCell<Option<gtk::TickCallbackId>>,
        last_frame: Cell<i64>,
        pub on_reorder: RefCell<Option<Box<dyn Fn(Vec<gtk::Widget>)>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TileGrid {
        const NAME: &'static str = "BfTileGrid";
        type Type = super::TileGrid;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for TileGrid {
        fn dispose(&self) {
            for it in self.items.take() {
                it.w.unparent();
            }
        }
    }

    const MAX_COLS: usize = 3;

    impl TileGrid {
        pub fn cell_size(&self) -> (f64, f64) {
            self.cell.get()
        }

        pub fn col_count(&self) -> usize {
            self.cols.get().max(1)
        }

        pub fn dragging(&self) -> bool {
            self.drag.borrow().is_some()
        }

        /// How many columns fit `width`: as many as there is room for, up to three.
        fn cols_for(&self, width: i32) -> usize {
            let min = self.items.borrow().iter().map(|i| i.w.measure(gtk::Orientation::Horizontal, -1).0).max().unwrap_or(1).max(1);
            ((width / min).max(1) as usize).min(MAX_COLS)
        }

        fn slot(&self, i: usize) -> (f64, f64) {
            let (cw, ch) = self.cell.get();
            let cols = self.cols.get().max(1);
            ((i % cols) as f64 * cw, (i / cols) as f64 * ch)
        }

        /// The tile drawn under (x, y).
        pub fn item_at(&self, x: f64, y: f64) -> Option<usize> {
            let (cw, ch) = self.cell.get();
            self.items.borrow().iter().position(|it| x >= it.pos.0 && x < it.pos.0 + cw && y >= it.pos.1 && y < it.pos.1 + ch)
        }

        pub fn start_drag(&self, obj: &super::TileGrid, i: usize, x: f64, y: f64) {
            let (pos, w) = {
                let items = self.items.borrow();
                (items[i].pos, items[i].w.clone())
            };
            let before = self.items.borrow().iter().map(|i| i.w.clone()).collect();
            self.drag.replace(Some(Drag { at: i, grab: (x - pos.0, y - pos.1), before, pointer: (x, y) }));
            w.add_css_class("dragging");
            w.insert_before(obj, None::<&gtk::Widget>); // drawn on top of the others
            self.ensure_tick(obj);
        }

        /// The pointer is at (x, y): the tile follows, and takes the slot it is over.
        pub fn drag_to(&self, obj: &super::TileGrid, x: f64, y: f64) {
            let (cw, ch) = self.cell.get();
            let cols = self.cols.get().max(1);
            let n = self.items.borrow().len();
            let target = {
                let mut d = self.drag.borrow_mut();
                let Some(d) = d.as_mut() else { return };
                d.pointer = (x, y);
                // the middle of the tile decides
                let (mx, my) = (x - d.grab.0 + cw / 2.0, y - d.grab.1 + ch / 2.0);
                let col = ((mx / cw).floor().max(0.0) as usize).min(cols - 1);
                let row = (my / ch).floor().max(0.0) as usize;
                ((row * cols + col).min(n.saturating_sub(1)), d.at)
            };
            if target.0 != target.1 {
                let mut items = self.items.borrow_mut();
                let it = items.remove(target.1);
                items.insert(target.0, it);
                drop(items);
                if let Some(d) = self.drag.borrow_mut().as_mut() {
                    d.at = target.0;
                }
            }
            self.ensure_tick(obj);
            obj.queue_allocate();
        }

        pub fn end_drag(&self, obj: &super::TileGrid) {
            let Some(d) = self.drag.take() else { return };
            if let Some(it) = self.items.borrow().get(d.at) {
                it.w.remove_css_class("dragging");
            }
            let now: Vec<gtk::Widget> = self.items.borrow().iter().map(|i| i.w.clone()).collect();
            self.ensure_tick(obj); // it settles into its slot
            if now != d.before
                && let Some(f) = self.on_reorder.borrow().as_ref()
            {
                f(now);
            }
        }

        /// Throw away a drag in progress (the tiles are being replaced).
        pub fn stop_drag(&self) {
            if let Some(d) = self.drag.take()
                && let Some(it) = self.items.borrow().get(d.at)
            {
                it.w.remove_css_class("dragging");
            }
            self.press.set(None);
        }

        /// Slide the tiles toward their slots each frame while any is on its way.
        fn ensure_tick(&self, obj: &super::TileGrid) {
            if self.tick.borrow().is_some() {
                return;
            }
            self.last_frame.set(0);
            let id = obj.add_tick_callback(|obj, clock| {
                let imp = obj.imp();
                let now = clock.frame_time();
                let dt = if imp.last_frame.get() == 0 { 16_000.0 } else { (now - imp.last_frame.get()) as f64 };
                imp.last_frame.set(now);
                // exponential approach: about 60 ms to cover most of the distance, whatever the frame rate
                let k = 1.0 - (-dt / 1000.0 / 55.0).exp();
                let dragged = imp.drag.borrow().as_ref().map(|d| d.at);
                let mut moving = dragged.is_some();
                {
                    let mut items = imp.items.borrow_mut();
                    for i in 0..items.len() {
                        if Some(i) == dragged {
                            continue;
                        }
                        let (tx, ty) = imp.slot(i);
                        let it = &mut items[i];
                        let (dx, dy) = (tx - it.pos.0, ty - it.pos.1);
                        if dx.abs() < 0.4 && dy.abs() < 0.4 {
                            it.pos = (tx, ty);
                        } else {
                            it.pos = (it.pos.0 + dx * k, it.pos.1 + dy * k);
                            moving = true;
                        }
                    }
                }
                obj.queue_allocate();
                if moving {
                    glib::ControlFlow::Continue
                } else {
                    imp.tick.replace(None);
                    glib::ControlFlow::Break
                }
            });
            self.tick.replace(Some(id));
        }
    }

    impl WidgetImpl for TileGrid {
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let items = self.items.borrow();
            if items.is_empty() {
                return (0, 0, -1, -1);
            }
            if orientation == gtk::Orientation::Horizontal {
                let min = items.iter().map(|i| i.w.measure(orientation, -1).0).max().unwrap_or(0);
                let nat = items.iter().map(|i| i.w.measure(orientation, -1).1).max().unwrap_or(0);
                return (min, nat * MAX_COLS as i32, -1, -1);
            }
            drop(items);
            // asked without a width (the scroller does): the width it has now, else a single column's worth
            let known = self.obj().width().max(self.last_w.get());
            let width = if for_size >= 0 { for_size } else { known.max(self.cols_for(0) as i32 * self.items.borrow().iter().map(|i| i.w.measure(gtk::Orientation::Horizontal, -1).0).max().unwrap_or(1)) };
            let cols = self.cols_for(width);
            let cw = width / cols as i32;
            let items = self.items.borrow();
            let ch = items.iter().map(|i| i.w.measure(gtk::Orientation::Vertical, cw).1).max().unwrap_or(0);
            let rows = items.len().div_ceil(cols) as i32;
            (rows * ch, rows * ch, -1, -1)
        }

        fn size_allocate(&self, width: i32, _height: i32, _baseline: i32) {
            self.last_w.set(width);
            let cols = self.cols_for(width);
            let cw = width as f64 / cols as f64;
            let ch = self.items.borrow().iter().map(|i| i.w.measure(gtk::Orientation::Vertical, cw as i32).1).max().unwrap_or(0) as f64;
            // a height asked for without a width is cached by GTK: when the column count changes, ask again
            if self.cols.replace(cols) != cols {
                let obj = self.obj().clone();
                glib::idle_add_local_once(move || obj.queue_resize());
            }
            self.cell.set((cw, ch));
            let animating = self.tick.borrow().is_some();
            let drag = self.drag.borrow().as_ref().map(|d| (d.at, d.grab, d.pointer));
            let mut items = self.items.borrow_mut();
            let rows = items.len().div_ceil(cols).max(1);
            for i in 0..items.len() {
                let (tx, ty) = ((i % cols) as f64 * cw, (i / cols) as f64 * ch);
                let it = &mut items[i];
                if let Some((_, grab, p)) = drag.filter(|d| d.0 == i) {
                    // follows the pointer, kept inside the grid
                    it.pos = ((p.0 - grab.0).clamp(0.0, (width as f64 - cw).max(0.0)), (p.1 - grab.1).clamp(0.0, ((rows - 1) as f64 * ch).max(0.0)));
                } else if !it.placed || !animating {
                    it.pos = (tx, ty); // laid out afresh (first time, or the width changed): no sliding
                }
                it.placed = true;
                it.w.allocate(cw.ceil() as i32, ch.ceil() as i32, -1, Some(gsk::Transform::new().translate(&graphene::Point::new(it.pos.0 as f32, it.pos.1 as f32))));
            }
        }
    }
}
