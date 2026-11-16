use gtk::{gdk, glib, graphene, prelude::*, subclass::prelude::*};
use kimbap::viewport::Viewport;
use std::cell::{Cell, RefCell};

mod imp {
    use super::*;
    #[derive(Default)]
    pub struct ImageCanvas {
        pub texture: RefCell<Option<gdk::Texture>>,
        pub view: RefCell<Viewport>,
        pub pointer: Cell<Option<[f64; 2]>>,
        pub pinch_start: Cell<f64>,
        pub drag_start: Cell<[f64; 2]>,
    }
    #[glib::object_subclass]
    impl ObjectSubclass for ImageCanvas {
        const NAME: &'static str = "KimbapImageCanvas";
        type Type = super::ImageCanvas;
        type ParentType = gtk::Widget;
    }
    impl ObjectImpl for ImageCanvas {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_hexpand(true);
            obj.set_vexpand(true);
            obj.set_overflow(gtk::Overflow::Hidden);
            let motion = gtk::EventControllerMotion::new();
            motion.connect_motion(glib::clone!(
                #[weak]
                obj,
                move |_, x, y| obj.imp().pointer.set(Some([x, y]))
            ));
            obj.add_controller(motion);
            let zoom = gtk::GestureZoom::new();
            zoom.connect_begin(glib::clone!(
                #[weak]
                obj,
                move |_, _| obj.imp().pinch_start.set(obj.imp().view.borrow().zoom)
            ));
            zoom.connect_scale_changed(glib::clone!(
                #[weak]
                obj,
                move |gesture, scale| {
                    let anchor = gesture
                        .bounding_box_center()
                        .map(|(x, y)| [x, y])
                        .or(obj.imp().pointer.get())
                        .unwrap_or([obj.width() as f64 / 2., obj.height() as f64 / 2.]);
                    obj.zoom_to(obj.imp().pinch_start.get() * scale, anchor);
                }
            ));
            obj.add_controller(zoom);
            let drag = gtk::GestureDrag::new();
            drag.set_button(1);
            drag.connect_drag_begin(glib::clone!(
                #[weak]
                obj,
                move |_, _, _| obj.imp().drag_start.set(obj.imp().view.borrow().offset)
            ));
            drag.connect_drag_update(glib::clone!(
                #[weak]
                obj,
                move |_, dx, dy| {
                    let start = obj.imp().drag_start.get();
                    obj.imp()
                        .view
                        .borrow_mut()
                        .pan_to([start[0] + dx, start[1] + dy]);
                    obj.queue_draw();
                }
            ));
            obj.add_controller(drag);
            let scroll =
                gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
            scroll.connect_scroll(glib::clone!(
                #[weak]
                obj,
                #[upgrade_or]
                glib::Propagation::Proceed,
                move |controller, dx, dy| {
                    if controller
                        .current_event_state()
                        .contains(gdk::ModifierType::CONTROL_MASK)
                    {
                        obj.zoom_by((-dy * 0.15).exp());
                    } else {
                        let factor = if controller.unit() == gdk::ScrollUnit::Wheel {
                            40.
                        } else {
                            1.
                        };
                        let offset = obj.imp().view.borrow().offset;
                        obj.imp()
                            .view
                            .borrow_mut()
                            .pan_to([offset[0] - dx * factor, offset[1] - dy * factor]);
                        obj.queue_draw();
                    }
                    glib::Propagation::Stop
                }
            ));
            obj.add_controller(scroll);
        }
    }
    impl WidgetImpl for ImageCanvas {
        fn measure(&self, _: gtk::Orientation, _: i32) -> (i32, i32, i32, i32) {
            (1, 1, -1, -1)
        }
        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            self.parent_size_allocate(width, height, baseline);
            self.view.borrow_mut().resize(width as f64, height as f64);
        }
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            if let Some(texture) = self.texture.borrow().as_ref() {
                let bounds = self.view.borrow().bounds();
                snapshot.push_clip(&graphene::Rect::new(
                    0.,
                    0.,
                    self.obj().width() as f32,
                    self.obj().height() as f32,
                ));
                snapshot.append_scaled_texture(
                    texture,
                    gtk::gsk::ScalingFilter::Trilinear,
                    &graphene::Rect::new(bounds[0], bounds[1], bounds[2], bounds[3]),
                );
                snapshot.pop();
            }
        }
    }
}
glib::wrapper! {pub struct ImageCanvas(ObjectSubclass<imp::ImageCanvas>) @extends gtk::Widget,@implements gtk::Accessible,gtk::Buildable,gtk::ConstraintTarget;}
impl ImageCanvas {
    pub fn new() -> Self {
        glib::Object::new()
    }
    pub fn set_texture(&self, texture: Option<&gdk::Texture>) {
        *self.imp().texture.borrow_mut() = texture.cloned();
        let mut view = self.imp().view.borrow_mut();
        if let Some(texture) = texture {
            view.image = [texture.width() as f64, texture.height() as f64];
        }
        view.reset();
        drop(view);
        self.queue_draw();
    }
    pub fn texture(&self) -> Option<gdk::Texture> {
        self.imp().texture.borrow().clone()
    }
    pub fn fit(&self) {
        self.imp().view.borrow_mut().reset();
        self.queue_draw();
    }
    pub fn native_size(&self) {
        let scale = self
            .native()
            .and_then(|n| n.surface())
            .map(|s| s.scale())
            .unwrap_or(1.);
        let fit = self.imp().view.borrow().fit();
        self.zoom_to(
            1. / (scale * fit),
            [self.width() as f64 / 2., self.height() as f64 / 2.],
        );
    }
    pub fn zoom_by(&self, factor: f64) {
        let zoom = self.imp().view.borrow().zoom;
        let anchor = self
            .imp()
            .pointer
            .get()
            .unwrap_or([self.width() as f64 / 2., self.height() as f64 / 2.]);
        self.zoom_to(zoom * factor, anchor);
    }
    pub fn zoom_to(&self, zoom: f64, anchor: [f64; 2]) {
        self.imp().view.borrow_mut().zoom_to(zoom, anchor);
        self.queue_draw();
    }
}
