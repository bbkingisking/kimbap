mod core;
mod gl;
use crate::{
    backend::{Backend, Event, Notify},
    preferences::Preferences,
};
use core::{Control, Core, Signal};
use gl::Renderer;
use gtk::{gdk, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::Instant,
};

/// Call once on the main thread after GTK initialization, before starting video.
/// GTK applies the user's locale; libmpv requires the numeric category to be C.
/// Keep translations and all other locale categories in the user's locale.
pub fn initialize() -> Result<(), String> {
    if unsafe { libc::setlocale(libc::LC_NUMERIC, c"C".as_ptr()) }.is_null() {
        return Err("Cannot initialize libmpv's required C numeric locale".into());
    }
    Ok(())
}

pub struct VideoBackend {
    area: gtk::GLArea,
    button: gtk::Button,
    preferences: Preferences,
    muted: Cell<bool>,
    control: RefCell<Option<Control>>,
    core: RefCell<Option<Arc<Core>>>,
    initialization_error: RefCell<Option<String>>,
    renderer: RefCell<Option<Renderer>>,
    notify: Notify,
    generation: Cell<u64>,
    info: Cell<Option<(u64, i32, i32)>>,
    ready: Cell<Option<u64>>,
    frames: Cell<u64>,
    calls: Cell<u64>,
    render_wakes: Cell<u64>,
    started: Cell<Instant>,
    tasks: RefCell<Vec<glib::JoinHandle<()>>>,
    wake: async_channel::Sender<()>,
}
impl VideoBackend {
    pub fn new(
        preferences: Preferences,
        notify: Notify,
        hwdec: String,
        ao: Option<String>,
    ) -> Rc<Self> {
        let area = gtk::GLArea::builder()
            .hexpand(true)
            .vexpand(true)
            .auto_render(false)
            .build();
        area.set_allowed_apis(gdk::GLAPI::GL);
        area.set_required_version(3, 3);
        let muted = preferences.muted().unwrap_or(true);
        let button = gtk::Button::with_label(if muted { "Unmute" } else { "Mute" });
        let (control, signals) = Control::new(hwdec, ao);
        control.mute(muted);
        let (wake, updates) = async_channel::bounded(1);
        let backend = Rc::new(Self {
            area,
            button,
            preferences,
            muted: Cell::new(muted),
            control: RefCell::new(Some(control)),
            core: RefCell::new(None),
            initialization_error: RefCell::new(None),
            renderer: RefCell::new(None),
            notify,
            generation: Cell::new(0),
            info: Cell::new(None),
            ready: Cell::new(None),
            frames: Cell::new(0),
            calls: Cell::new(0),
            render_wakes: Cell::new(0),
            started: Cell::new(Instant::now()),
            tasks: RefCell::new(Vec::new()),
            wake,
        });
        let weak = Rc::downgrade(&backend);
        backend.area.connect_realize(move |_| {
            if let Some(v) = weak.upgrade() {
                v.initialize_renderer();
            }
        });
        let weak = Rc::downgrade(&backend);
        backend.area.connect_unrealize(move |_| {
            if let Some(v) = weak.upgrade() {
                v.release_renderer();
            }
        });
        backend
            .area
            .connect_resize(|area, _, _| area.queue_render());
        let weak = Rc::downgrade(&backend);
        backend.area.connect_render(move |_, _| {
            let Some(v) = weak.upgrade() else {
                return glib::Propagation::Stop;
            };
            v.calls.set(v.calls.get() + 1);
            if v.calls.get() < 5 {
                log::debug!(
                    "GL render call={} size={}x{} scale={} renderer={}",
                    v.calls.get(),
                    v.area.width(),
                    v.area.height(),
                    v.area.scale_factor(),
                    v.renderer.borrow().is_some()
                );
            }
            let result = v.renderer.borrow().as_ref().map(|r| {
                r.render(
                    v.area.width() * v.area.scale_factor(),
                    v.area.height() * v.area.scale_factor(),
                )
            });
            match result {
                Some(Ok(new)) => {
                    if new {
                        v.frames.set(v.frames.get() + 1);
                    }
                    if v.frames.get() > 0 {
                        v.maybe_ready();
                    }
                }
                Some(Err(message)) => v.failed(message),
                None => (),
            };
            glib::Propagation::Stop
        });
        let weak = Rc::downgrade(&backend);
        backend.button.connect_clicked(move |_| {
            if let Some(v) = weak.upgrade() {
                v.toggle_mute();
            }
        });
        let weak = Rc::downgrade(&backend);
        let task = glib::MainContext::default().spawn_local(async move {
            while let Ok(signal) = signals.recv().await {
                let Some(v) = weak.upgrade() else {
                    break;
                };
                v.signal(signal);
            }
        });
        backend.tasks.borrow_mut().push(task);
        let weak = Rc::downgrade(&backend);
        let task = glib::MainContext::default().spawn_local(async move {
            while updates.recv().await.is_ok() {
                let Some(v) = weak.upgrade() else {
                    break;
                };
                v.render_wakes.set(v.render_wakes.get() + 1);
                v.area.queue_render();
            }
        });
        backend.tasks.borrow_mut().push(task);
        backend
    }
    fn initialize_renderer(&self) {
        if self.renderer.borrow().is_some() || !self.area.is_realized() {
            return;
        }
        self.area.make_current();
        if let Some(error) = self.area.error() {
            self.initialization_failed(format!("Cannot create video GL context: {error}"));
            return;
        }
        let Some(core) = self.core.borrow().clone() else {
            return;
        };
        let display = self
            .area
            .display()
            .downcast::<gdk4_wayland::WaylandDisplay>()
            .ok()
            .and_then(|d| d.wl_display_raw())
            .map(|p| p.as_ptr())
            .unwrap_or(std::ptr::null_mut());
        match Renderer::new(core, display, self.wake.clone()) {
            Ok(renderer) => {
                log::debug!(
                    "video renderer created generation={}",
                    self.generation.get()
                );
                self.renderer.replace(Some(renderer));
                if let Some(control) = self.control.borrow().as_ref() {
                    control.renderer(true);
                }
                self.area.queue_render();
            }
            Err(message) => self.initialization_failed(message),
        }
    }
    fn initialization_failed(&self, message: String) {
        self.initialization_error.replace(Some(message.clone()));
        self.failed(message);
    }
    fn release_renderer(&self) {
        if let Some(control) = self.control.borrow().as_ref() {
            control.renderer(false);
            control.stop();
        }
        if self.renderer.borrow().is_some() {
            log::debug!(
                "video renderer released generation={}",
                self.generation.get()
            );
            self.area.make_current();
            self.renderer.borrow_mut().take();
        }
    }
    fn failed(&self, message: String) {
        eprintln!("kimbap: video failed: {message}");
        (self.notify)(Event::Failed {
            generation: self.generation.get(),
            message,
        });
    }
    fn signal(&self, signal: Signal) {
        match signal {
            Signal::Core(core) => {
                self.core.replace(Some(core));
                self.initialize_renderer();
            }
            Signal::Info {
                generation,
                width,
                height,
            } if generation == self.generation.get() => {
                self.info.set(Some((generation, width, height)));
                self.maybe_ready();
                self.area.queue_render();
            }
            Signal::Failed {
                generation: 0,
                message,
            } => self.initialization_failed(message),
            Signal::Failed {
                generation,
                message,
            } if generation == self.generation.get() => self.failed(message),
            Signal::Stats(stats) if stats.generation == self.generation.get() => {
                log::debug!(
                    "video-stats generation={} codec={} hwdec={} position={:.3} duration={:.3} dropped={} decoder_dropped={} eof={} pause={:?} paused_for_cache={:?} mute={} mapped={} render_wakes={} render_calls={} frames={}",
                    stats.generation,
                    stats.codec,
                    stats.hwdec,
                    stats.position,
                    stats.duration,
                    stats.dropped,
                    stats.decoder_dropped,
                    stats.eof,
                    stats.paused,
                    stats.paused_for_cache,
                    stats.muted,
                    self.area.is_mapped(),
                    self.render_wakes.get(),
                    self.calls.get(),
                    self.frames.get()
                );
            }
            _ => (),
        }
    }
    fn maybe_ready(&self) {
        if let Some((generation, width, height)) = self.info.get()
            && self.frames.get() > 0
            && self.ready.get() != Some(generation)
        {
            self.ready.set(Some(generation));
            log::debug!(
                "video-ready generation={generation} size={width}x{height} elapsed_us={}",
                self.started.get().elapsed().as_micros()
            );
            (self.notify)(Event::Ready {
                generation,
                width,
                height,
            });
        }
    }
    pub fn toggle_mute(&self) {
        let muted = !self.muted.get();
        self.muted.set(muted);
        if let Some(control) = self.control.borrow().as_ref() {
            control.mute(muted);
        }
        let label = if muted { "Unmute" } else { "Mute" };
        match self.preferences.set_muted(muted) {
            Ok(()) => {
                self.button.set_label(label);
                self.button.set_tooltip_text(None);
            }
            Err(error) => {
                self.button.set_label(&format!("{label} (not saved)"));
                self.button.set_tooltip_text(Some(&error));
                eprintln!("kimbap: {error}");
            }
        }
    }
}
impl Backend for VideoBackend {
    fn widget(&self) -> gtk::Widget {
        self.area.clone().upcast()
    }
    fn controls(&self) -> gtk::Widget {
        self.button.clone().upcast()
    }
    fn load(&self, generation: u64, path: PathBuf) {
        self.generation.set(generation);
        self.info.set(None);
        self.ready.set(None);
        self.frames.set(0);
        self.calls.set(0);
        self.render_wakes.set(0);
        log::debug!("video load requested generation={generation}");
        self.started.set(Instant::now());
        if let Some(message) = self.initialization_error.borrow().clone() {
            self.failed(message);
            return;
        }
        if let Some(control) = self.control.borrow().as_ref() {
            control.mute(self.muted.get());
            control.load(generation, path);
        }
    }
    fn stop(&self, generation: u64) {
        log::debug!(
            "video stop requested generation={generation} previous={}",
            self.generation.get()
        );
        self.generation.set(generation);
        self.info.set(None);
        self.ready.set(None);
        if let Some(control) = self.control.borrow().as_ref() {
            control.stop();
        }
    }
    fn key(&self, key: gdk::Key, _: gdk::ModifierType) -> bool {
        if key == gdk::Key::m {
            self.toggle_mute();
            true
        } else {
            false
        }
    }
}
impl Drop for VideoBackend {
    fn drop(&mut self) {
        for task in self.tasks.get_mut().drain(..) {
            task.abort();
        }
        self.release_renderer();
        self.core.get_mut().take();
        self.control.get_mut().take();
    }
}
