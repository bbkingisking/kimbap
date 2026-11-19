//! The preview shell owns window/activation/navigation state. Format backends
//! supply a widget, controls, and generation-tagged readiness/failure events.
use crate::{image_backend::ImageBackend, pdf_backend::PdfBackend};
use gtk::{gdk, glib, prelude::*};
use kimbap::{
    Command, Direction, Event as NautilusEvent, PreviewEndpoint, PreviewRequest, WindowState,
    backend::{Backend, Event, Kind, Notify},
};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
    time::Instant,
};
pub struct Viewer {
    endpoint: Rc<PreviewEndpoint>,
    window: gtk::Window,
    stack: gtk::Stack,
    error: gtk::Label,
    title: gtk::Label,
    spinner: gtk::Spinner,
    controls: gtk::Box,
    backends: RefCell<HashMap<Kind, Rc<dyn Backend>>>,
    #[cfg(feature = "video")]
    notify: Notify,
    video_enabled: bool,
    active: Cell<Option<Kind>>,
    generation: Cell<u64>,
    current: RefCell<Option<PreviewRequest>>,
    routing: RefCell<Option<glib::JoinHandle<()>>>,
    loading: RefCell<Option<glib::SourceId>>,
    started: Cell<Option<Instant>>,
    paint: RefCell<Option<(gdk::FrameClock, glib::SignalHandlerId)>>,
}
impl Viewer {
    pub fn new(
        endpoint: Rc<PreviewEndpoint>,
        worker: PathBuf,
        pdf_worker: PathBuf,
        pdfium_library: PathBuf,
        video_enabled: bool,
    ) -> Result<Rc<Self>, glib::Error> {
        let css = gtk::CssProvider::new();
        css.load_from_string(".kimbap {background:#181616;color:#c5c9c5;font-family:Pretendard,sans-serif;} .kimbap headerbar {background:#0d0c0c;color:#c5c9c5;} .kimbap .preview-error {color:#c4746e;padding:32px;} .kimbap button {color:#c5c9c5;} .kimbap .video-progress {padding:8px 6px;} .kimbap .video-progress slider {min-width:10px;min-height:10px;background:#8ba4b0;border:0;} .kimbap .video-progress trough {min-height:4px;border:0;border-radius:0;background:#0d0c0c;} .kimbap .video-progress highlight {min-height:4px;border:0;border-radius:0;background:#8ba4b0;}");
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
        let application = gtk::Application::builder()
            .application_id("dev.kimbap.Previewer")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        application.register(None::<&gio::Cancellable>)?;
        let window = gtk::Window::builder()
            .application(&application)
            .title("Kimbap")
            .default_width(900)
            .default_height(700)
            .hide_on_close(true)
            .build();
        window.add_css_class("kimbap");
        let header = gtk::HeaderBar::new();
        let title = gtk::Label::new(Some("Kimbap"));
        title.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        header.set_title_widget(Some(&title));
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.pack_start(&controls);
        window.set_titlebar(Some(&header));
        let error = gtk::Label::new(None);
        error.set_wrap(true);
        error.set_selectable(true);
        error.set_max_width_chars(70);
        error.add_css_class("preview-error");
        let stack = gtk::Stack::new();
        stack.set_hexpand(true);
        stack.set_vexpand(true);
        stack.add_named(&error, Some("error"));
        let spinner = gtk::Spinner::new();
        spinner.set_halign(gtk::Align::Center);
        spinner.set_valign(gtk::Align::Center);
        spinner.set_visible(false);
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&stack));
        overlay.add_overlay(&spinner);
        window.set_child(Some(&overlay));
        let viewer = Rc::new_cyclic(|weak: &std::rc::Weak<Self>| {
            let weak = weak.clone();
            let notify: Notify = Rc::new(move |event| {
                if let Some(v) = weak.upgrade() {
                    v.backend_event(event);
                }
            });
            let image: Rc<dyn Backend> = ImageBackend::new(worker, notify.clone());
            stack.add_named(&image.widget(), Some("jpeg"));
            let pdf: Rc<dyn Backend> = PdfBackend::new(pdf_worker, pdfium_library, notify.clone());
            stack.add_named(&pdf.widget(), Some("pdf"));
            Self {
                endpoint,
                window,
                stack,
                error,
                title,
                spinner,
                controls,
                backends: RefCell::new(HashMap::from([(Kind::Jpeg, image), (Kind::Pdf, pdf)])),
                #[cfg(feature = "video")]
                notify,
                video_enabled,
                active: Cell::new(None),
                generation: Cell::new(0),
                current: RefCell::new(None),
                routing: RefCell::new(None),
                loading: RefCell::new(None),
                started: Cell::new(None),
                paint: RefCell::new(None),
            }
        });
        let weak = Rc::downgrade(&viewer);
        viewer.window.connect_close_request(move |_| {
            if let Some(v) = weak.upgrade() {
                v.close();
            }
            glib::Propagation::Stop
        });
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&viewer);
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            weak.upgrade()
                .map(|v| v.key(key, modifiers))
                .unwrap_or(glib::Propagation::Proceed)
        });
        viewer.window.add_controller(keys);
        let click = gtk::GestureClick::new();
        click.set_button(1);
        let weak = Rc::downgrade(&viewer);
        click.connect_pressed(move |_, count, _, _| {
            if count == 2
                && let Some(v) = weak.upgrade()
            {
                v.fullscreen();
            }
        });
        viewer.stack.add_controller(click);
        Ok(viewer)
    }
    pub fn handle(self: &Rc<Self>, command: Command) {
        match command {
            Command::ShowFile(request) => self.show(request),
            Command::Close => self.close(),
        }
    }
    fn next_generation(&self) -> u64 {
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        generation
    }
    fn backend(&self, kind: Kind) -> Result<Rc<dyn Backend>, String> {
        if let Some(backend) = self.backends.borrow().get(&kind) {
            return Ok(backend.clone());
        }
        if kind == Kind::Video && self.video_enabled {
            #[cfg(feature = "video")]
            {
                let preferences = kimbap::preferences::Preferences::load(
                    kimbap::preferences::Preferences::path(),
                )?;
                let backend: Rc<dyn Backend> = kimbap::video::VideoBackend::new(
                    preferences,
                    self.notify.clone(),
                    "auto".into(),
                    None,
                );
                self.stack.add_named(&backend.widget(), Some("video"));
                self.backends.borrow_mut().insert(kind, backend.clone());
                return Ok(backend);
            }
        }
        Err("Video previews are not enabled in this session".into())
    }
    fn show(self: &Rc<Self>, request: PreviewRequest) {
        let requested = Instant::now();
        let same = self.current.borrow().as_ref().is_some_and(|old| {
            gio::File::for_uri(&old.uri).equal(&gio::File::for_uri(&request.uri))
        });
        if same && self.window.is_visible() && request.close_if_already_shown {
            self.close();
            return;
        }
        let first = !self.window.is_visible();
        self.window.set_startup_id(&request.activation_token);
        gtk::prelude::WidgetExt::realize(&self.window);
        let parent = self.set_parent(&request.window_handle);
        let file = gio::File::for_uri(&request.uri);
        let basename = file
            .basename()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "File preview".into());
        self.window.set_title(Some(&basename));
        self.title.set_label(&basename);
        self.current.replace(Some(request));
        if same && !first {
            self.window.present();
            self.publish(parent);
            return;
        }
        let generation = self.next_generation();
        self.started.set(Some(requested));
        self.cancel_loading();
        self.cancel_paint();
        self.cancel_routing();
        // Stop audio promptly while MIME lookup runs. A newer video load then
        // replaces this stop request; no native client call blocks GTK.
        if self.active.get() == Some(Kind::Video)
            && let Some(video) = self.backends.borrow().get(&Kind::Video)
        {
            video.stop(generation);
        }
        self.window.present();
        self.publish(parent);
        let Some(path) = file.path() else {
            self.failure("Only local files are supported right now");
            return;
        };
        let weak = Rc::downgrade(self);
        let timer =
            glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                if let Some(v) = weak.upgrade() {
                    v.loading.borrow_mut().take();
                    v.spinner.set_visible(true);
                    v.spinner.start();
                }
            });
        self.loading.replace(Some(timer));
        let weak = Rc::downgrade(self);
        let task = glib::MainContext::default().spawn_local(async move {
            let info = file
                .query_info_future(
                    "standard::type,standard::content-type",
                    gio::FileQueryInfoFlags::NONE,
                    glib::Priority::DEFAULT,
                )
                .await;
            let Some(viewer) = weak.upgrade() else {
                return;
            };
            if viewer.generation.get() != generation {
                return;
            }
            match info {
                Ok(info) => {
                    if info.file_type() != gio::FileType::Regular {
                        viewer.failure("This item is not a supported regular file");
                        return;
                    }
                    let kind = info
                        .content_type()
                        .as_deref()
                        .and_then(Kind::for_content_type);
                    let Some(kind) = kind else {
                        viewer.failure("This file type does not have a preview backend yet");
                        return;
                    };
                    match viewer.backend(kind) {
                        Ok(backend) => {
                            if viewer.active.get() != Some(kind) {
                                if let Some(old) = viewer
                                    .active
                                    .get()
                                    .and_then(|k| viewer.backends.borrow().get(&k).cloned())
                                {
                                    old.stop(generation);
                                }
                                while let Some(child) = viewer.controls.first_child() {
                                    viewer.controls.remove(&child);
                                }
                                viewer.controls.append(&backend.controls());
                            }
                            viewer.active.set(Some(kind));
                            viewer.stack.set_visible_child_name(match kind {
                                Kind::Jpeg => "jpeg",
                                Kind::Pdf => "pdf",
                                Kind::Video => "video",
                            });
                            backend.load(generation, path);
                        }
                        Err(error) => viewer.failure(&error),
                    }
                }
                Err(error) => viewer.failure(&format!("Cannot inspect this file: {error}")),
            }
        });
        self.routing.replace(Some(task));
    }
    fn backend_event(self: &Rc<Self>, event: Event) {
        match event {
            Event::Ready {
                generation,
                width,
                height,
            } if generation == self.generation.get() && self.window.is_visible() => {
                self.cancel_loading();
                self.spinner.stop();
                self.spinner.set_visible(false);
                if !self.window.is_fullscreen() {
                    self.size_for_content(width, height);
                }
                self.trace_paint(generation);
            }
            Event::Failed {
                generation,
                message,
            } if generation == self.generation.get() && self.window.is_visible() => {
                self.failure(&message)
            }
            _ => (),
        }
    }
    fn set_parent(&self, handle: &str) -> String {
        if let Some(surface) = self.window.surface() {
            surface.set_property("transient-for", None::<&gdk::Surface>);
        }
        if handle.is_empty() {
            return String::new();
        }
        if let Some(exported) = handle.strip_prefix("wayland:")
            && let Some(surface) = self
                .window
                .surface()
                .and_then(|s| s.downcast::<gdk4_wayland::WaylandToplevel>().ok())
            && surface.set_transient_for_exported(exported)
        {
            return handle.into();
        }
        eprintln!("kimbap: could not attach preview to the supplied parent window");
        String::new()
    }
    fn publish(&self, parent: String) {
        if let Err(error) = self.endpoint.set_window_state(WindowState {
            visible: self.window.is_visible(),
            parent_handle: parent,
        }) {
            eprintln!("kimbap: window state: {error}");
        }
    }
    fn size_for_content(&self, width: i32, height: i32) {
        if let Some(surface) = self.window.surface()
            && let Some(monitor) = surface.display().monitor_at_surface(&surface)
        {
            let geometry = monitor.geometry();
            let max_w = (geometry.width() as f64 * 0.95).max(320.);
            let max_h = (geometry.height() as f64 * 0.90 - 52.).max(240.);
            let ratio = (max_w / width as f64).min(max_h / height as f64);
            self.window.set_default_size(
                (width as f64 * ratio).round() as i32,
                (height as f64 * ratio).round() as i32,
            );
        }
    }
    fn trace_paint(self: &Rc<Self>, generation: u64) {
        self.cancel_paint();
        if !log::log_enabled!(log::Level::Debug) {
            return;
        }
        if let Some(clock) = self.window.frame_clock() {
            let weak = Rc::downgrade(self);
            let id = clock.connect_after_paint(move |_| {
                if let Some(v) = weak.upgrade() {
                    v.cancel_paint();
                    if v.generation.get() == generation {
                        log::debug!(
                            "backend painted generation={generation} total_us={}",
                            v.started
                                .get()
                                .map(|t| t.elapsed().as_micros())
                                .unwrap_or(0)
                        );
                    }
                }
            });
            self.paint.replace(Some((clock, id)));
        }
    }
    fn cancel_paint(&self) {
        if let Some((clock, id)) = self.paint.borrow_mut().take() {
            clock.disconnect(id);
        }
    }
    fn cancel_loading(&self) {
        if let Some(timer) = self.loading.borrow_mut().take() {
            timer.remove();
        }
    }
    fn cancel_routing(&self) {
        if let Some(task) = self.routing.borrow_mut().take() {
            task.abort();
        }
    }
    fn failure(&self, message: &str) {
        self.cancel_loading();
        self.spinner.stop();
        self.spinner.set_visible(false);
        for backend in self.backends.borrow().values() {
            backend.stop(self.generation.get());
        }
        self.error
            .set_label(&format!("Could not preview this file\n\n{message}"));
        self.stack.set_visible_child_name("error");
        eprintln!("kimbap: preview failed: {message}");
    }
    pub fn close(&self) {
        let generation = self.next_generation();
        self.cancel_routing();
        self.cancel_loading();
        self.cancel_paint();
        for backend in self.backends.borrow().values() {
            backend.stop(generation);
        }
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.window.set_visible(false);
        self.current.borrow_mut().take();
        self.publish(String::new());
    }
    fn event(&self, event: NautilusEvent) {
        if let NautilusEvent::Selection(direction) = &event {
            log::debug!(
                "selection direction={} at_us={}",
                *direction as u32,
                glib::monotonic_time()
            );
        }
        if let Err(error) = self.endpoint.emit(&event) {
            eprintln!("kimbap: Nautilus event: {error}");
        }
    }
    fn fullscreen(&self) {
        if self.window.is_fullscreen() {
            self.window.unfullscreen();
        } else {
            self.window.fullscreen();
        }
    }
    fn key(&self, key: gdk::Key, modifiers: gdk::ModifierType) -> glib::Propagation {
        match key {
            gdk::Key::Escape | gdk::Key::space | gdk::Key::q => self.close(),
            gdk::Key::Left => self.event(NautilusEvent::Selection(Direction::Left)),
            gdk::Key::Right => self.event(NautilusEvent::Selection(Direction::Right)),
            gdk::Key::Up => self.event(NautilusEvent::Selection(Direction::Up)),
            gdk::Key::Down => self.event(NautilusEvent::Selection(Direction::Down)),
            gdk::Key::F11 => self.fullscreen(),
            gdk::Key::F2 => {
                let uri = self.current.borrow().as_ref().map(|r| r.uri.clone());
                if let Some(uri) = uri {
                    self.close();
                    let endpoint = self.endpoint.clone();
                    glib::timeout_add_local_once(
                        std::time::Duration::from_millis(200),
                        move || {
                            if let Err(error) = endpoint.emit(&NautilusEvent::Rename(uri)) {
                                eprintln!("kimbap: Nautilus rename event: {error}");
                            }
                        },
                    );
                }
            }
            gdk::Key::Delete => {
                if let Some(uri) = self.current.borrow().as_ref().map(|r| r.uri.clone()) {
                    self.event(NautilusEvent::Trash(uri));
                }
            }
            _ => {
                let handled = self
                    .active
                    .get()
                    .and_then(|k| self.backends.borrow().get(&k).cloned())
                    .is_some_and(|b| b.key(key, modifiers));
                if !handled {
                    return glib::Propagation::Proceed;
                }
            }
        }
        glib::Propagation::Stop
    }
}
impl Drop for Viewer {
    fn drop(&mut self) {
        self.cancel_routing();
        self.cancel_loading();
        self.cancel_paint();
        self.window.destroy();
        self.backends.get_mut().clear();
    }
}
