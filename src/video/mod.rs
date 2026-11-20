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
    container: gtk::Box,
    area: gtk::GLArea,
    audio: gtk::Box,
    filename: gtk::Label,
    progress: gtk::Scale,
    elapsed: gtk::Label,
    total: gtk::Label,
    scrubbing: Cell<bool>,
    scrolling: Cell<bool>,
    duration: Cell<f64>,
    button: gtk::Button,
    play: gtk::Button,
    paused: Cell<bool>,
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
        let progress = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0., 1., 0.001);
        progress.set_draw_value(false);
        progress.set_sensitive(false);
        progress.set_hexpand(true);
        progress.add_css_class("video-progress");
        progress.update_property(&[gtk::accessible::Property::Label("Playback position")]);
        let elapsed = gtk::Label::new(Some("0:00"));
        let total = gtk::Label::new(Some("--:--"));
        for label in [&elapsed, &total] {
            let attributes = gtk::pango::AttrList::new();
            attributes.insert(gtk::pango::AttrFontFeatures::new("tnum=1"));
            label.set_attributes(Some(&attributes));
            label.add_css_class("dim-label");
            label.set_width_chars(5);
        }
        elapsed.set_xalign(0.);
        total.set_xalign(1.);
        let timeline = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        timeline.set_margin_start(12);
        timeline.set_margin_end(12);
        timeline.set_margin_top(6);
        timeline.set_margin_bottom(6);
        let play = gtk::Button::from_icon_name("media-playback-pause-symbolic");
        play.add_css_class("flat");
        play.set_sensitive(false);
        play.set_tooltip_text(Some("Pause"));
        play.update_property(&[gtk::accessible::Property::Label("Pause")]);
        let muted = preferences.muted().unwrap_or(true);
        let button = gtk::Button::from_icon_name(if muted {
            "audio-volume-muted-symbolic"
        } else {
            "audio-volume-high-symbolic"
        });
        button.add_css_class("flat");
        let action = if muted { "Unmute" } else { "Mute" };
        button.set_tooltip_text(Some(action));
        button.update_property(&[gtk::accessible::Property::Label(action)]);
        timeline.append(&play);
        timeline.append(&elapsed);
        timeline.append(&progress);
        timeline.append(&total);
        timeline.append(&button);
        let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let audio = gtk::Box::new(gtk::Orientation::Vertical, 12);
        audio.add_css_class("audio-preview");
        audio.set_valign(gtk::Align::Fill);
        audio.set_halign(gtk::Align::Fill);
        let details = gtk::Box::new(gtk::Orientation::Vertical, 12);
        details.set_margin_start(24);
        details.set_margin_end(24);
        details.set_margin_top(24);
        details.set_margin_bottom(24);
        details.set_valign(gtk::Align::Center);
        details.set_vexpand(true);
        let icon = gtk::Image::from_icon_name("audio-x-generic-symbolic");
        icon.set_pixel_size(64);
        details.append(&icon);
        let filename = gtk::Label::new(None);
        filename.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        filename.set_margin_start(24);
        filename.set_margin_end(24);
        details.append(&filename);
        details.append(&gtk::Label::new(Some("Audio only")));
        audio.append(&details);
        audio.set_visible(false);
        // Cover the GL surface without unrealizing it: releasing the renderer
        // stops playback, and it must remain available for the next video.
        let display = gtk::Overlay::new();
        display.set_child(Some(&area));
        display.add_overlay(&audio);
        container.append(&display);
        container.append(&timeline);
        let (control, signals) = Control::new(hwdec, ao);
        control.mute(muted);
        let (wake, updates) = async_channel::bounded(1);
        let backend = Rc::new(Self {
            container,
            area,
            audio,
            filename,
            progress,
            elapsed,
            total,
            scrubbing: Cell::new(false),
            scrolling: Cell::new(false),
            duration: Cell::new(0.),
            button,
            play,
            paused: Cell::new(false),
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
        backend.progress.connect_value_changed(move |scale| {
            if let Some(v) = weak.upgrade() {
                v.elapsed
                    .set_label(&format_time(scale.value() * v.duration.get()));
            }
        });
        let weak = Rc::downgrade(&backend);
        backend.progress.connect_change_value(move |_, _, value| {
            if let Some(v) = weak.upgrade()
                && let Some(control) = v.control.borrow().as_ref()
            {
                control.seek(v.generation.get(), value.clamp(0., 1.));
            }
            glib::Propagation::Proceed
        });
        // Playback updates must not move the handle while the user drags it.
        let pointer = gtk::EventControllerLegacy::new();
        pointer.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&backend);
        pointer.connect_event(move |_, event| {
            if let Some(v) = weak.upgrade() {
                match event.event_type() {
                    gdk::EventType::ButtonPress | gdk::EventType::TouchBegin => {
                        v.scrubbing.set(true);
                    }
                    gdk::EventType::ButtonRelease
                    | gdk::EventType::TouchEnd
                    | gdk::EventType::TouchCancel => v.scrubbing.set(false),
                    _ => (),
                }
            }
            glib::Propagation::Proceed
        });
        backend.progress.add_controller(pointer);
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        // Capture over both the video and the scale before the scale's own
        // scroll handler, which uses a duration-dependent adjustment step.
        scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&backend);
        scroll.connect_scroll_begin(move |_| {
            if let Some(v) = weak.upgrade() {
                v.scrolling.set(true);
            }
        });
        let weak = Rc::downgrade(&backend);
        scroll.connect_scroll_end(move |_| {
            if let Some(v) = weak.upgrade() {
                v.scrolling.set(false);
            }
        });
        let weak = Rc::downgrade(&backend);
        scroll.connect_scroll(move |controller, dx, dy| {
            let Some(v) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if !v.progress.is_sensitive() || !dx.is_finite() || dx.abs() <= dy.abs() {
                return glib::Propagation::Proceed;
            }
            // Surface deltas are pixels: 100 px moves ten seconds regardless
            // of clip length. Wheel devices report discrete steps instead.
            let seconds = if controller.unit() == gdk::ScrollUnit::Wheel {
                dx * 5.
            } else {
                dx * 0.1
            };
            let fraction = (v.progress.value() + seconds / v.duration.get()).clamp(0., 1.);
            v.progress.set_value(fraction);
            if let Some(control) = v.control.borrow().as_ref() {
                control.seek(v.generation.get(), fraction);
            }
            glib::Propagation::Stop
        });
        backend.container.add_controller(scroll);
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
        backend.play.connect_clicked(move |_| {
            if let Some(v) = weak.upgrade() {
                let paused = !v.paused.get();
                v.update_paused(paused);
                if let Some(control) = v.control.borrow().as_ref() {
                    control.pause(v.generation.get(), paused);
                }
            }
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
            Signal::AudioOnly { generation } if generation == self.generation.get() => {
                self.audio.set_visible(true);
                self.ready.set(Some(generation));
                (self.notify)(Event::Ready {
                    expand: false,
                    generation,
                    width: 528,
                    height: 268,
                });
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
            Signal::Progress {
                generation,
                position,
                duration,
                seekable,
                paused,
            } if generation == self.generation.get() => {
                let fraction = if position.is_finite() && duration.is_finite() && duration > 0. {
                    (position / duration).clamp(0., 1.)
                } else {
                    0.
                };
                self.play.set_sensitive(true);
                self.update_paused(paused);
                self.duration.set(duration);
                self.total.set_label(&format_time(if duration > 0. {
                    duration
                } else {
                    f64::NAN
                }));
                self.progress
                    .set_sensitive(seekable && duration.is_finite() && duration > 0.);
                if !self.scrubbing.get() && !self.scrolling.get() {
                    self.progress.set_value(fraction);
                }
            }
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
                expand: true,
                generation,
                width,
                height,
            });
        }
    }
    fn update_paused(&self, paused: bool) {
        self.paused.set(paused);
        self.play.set_icon_name(if paused {
            "media-playback-start-symbolic"
        } else {
            "media-playback-pause-symbolic"
        });
        let action = if paused { "Play" } else { "Pause" };
        self.play.set_tooltip_text(Some(action));
        self.play
            .update_property(&[gtk::accessible::Property::Label(action)]);
    }
    pub fn toggle_mute(&self) {
        let muted = !self.muted.get();
        self.muted.set(muted);
        if let Some(control) = self.control.borrow().as_ref() {
            control.mute(muted);
        }
        let label = if muted { "Unmute" } else { "Mute" };
        self.button.set_icon_name(if muted {
            "audio-volume-muted-symbolic"
        } else {
            "audio-volume-high-symbolic"
        });
        self.button
            .update_property(&[gtk::accessible::Property::Label(label)]);
        match self.preferences.set_muted(muted) {
            Ok(()) => {
                self.button.set_tooltip_text(Some(label));
            }
            Err(error) => {
                self.button
                    .set_tooltip_text(Some(&format!("{label} (not saved): {error}")));
                eprintln!("kimbap: {error}");
            }
        }
    }
}
impl Backend for VideoBackend {
    fn widget(&self) -> gtk::Widget {
        self.container.clone().upcast()
    }
    fn controls(&self) -> gtk::Widget {
        gtk::Box::new(gtk::Orientation::Horizontal, 0).upcast()
    }
    fn load(&self, generation: u64, path: PathBuf) {
        self.generation.set(generation);
        self.audio.set_visible(false);
        self.filename
            .set_label(&path.file_name().unwrap_or_default().to_string_lossy());
        self.scrubbing.set(false);
        self.scrolling.set(false);
        self.duration.set(0.);
        self.update_paused(false);
        self.play.set_sensitive(false);
        self.elapsed.set_label("0:00");
        self.total.set_label("--:--");
        self.progress.set_value(0.);
        self.progress.set_sensitive(false);
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
        self.scrubbing.set(false);
        self.scrolling.set(false);
        self.duration.set(0.);
        self.update_paused(false);
        self.play.set_sensitive(false);
        self.elapsed.set_label("0:00");
        self.total.set_label("--:--");
        self.progress.set_value(0.);
        self.progress.set_sensitive(false);
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

fn format_time(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0. {
        return "--:--".into();
    }
    let seconds = seconds.floor() as u64;
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}
