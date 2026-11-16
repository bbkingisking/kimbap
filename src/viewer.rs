use crate::canvas::ImageCanvas;
use gtk::{gdk, glib, prelude::*};
use kimbap::{
    Command, Direction, Event, PreviewEndpoint, PreviewRequest, WindowState,
    jpeg::{Decoder, Reply},
};
use kimbap_jpeg_ipc::Frame;
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::Instant,
};

struct Pixels(Arc<Frame>);
impl AsRef<[u8]> for Pixels {
    fn as_ref(&self) -> &[u8] {
        &self.0.pixels
    }
}

pub struct Viewer {
    endpoint: Rc<PreviewEndpoint>,
    window: gtk::Window,
    canvas: ImageCanvas,
    stack: gtk::Stack,
    error: gtk::Label,
    title: gtk::Label,
    spinner: gtk::Spinner,
    decoder: Decoder,
    generation: Cell<u64>,
    current: RefCell<Option<PreviewRequest>>,
    task: RefCell<Option<glib::JoinHandle<()>>>,
    loading: RefCell<Option<glib::SourceId>>,
    started: Cell<Option<Instant>>,
    verbose: bool,
    paint: RefCell<Option<(gdk::FrameClock, glib::SignalHandlerId)>>,
}
impl Viewer {
    pub fn new(
        endpoint: Rc<PreviewEndpoint>,
        worker: PathBuf,
        verbose: bool,
    ) -> Result<Rc<Self>, glib::Error> {
        let css = gtk::CssProvider::new();
        css.load_from_string(".kimbap {background:#181616;color:#c5c9c5;font-family:Pretendard,sans-serif;} .kimbap headerbar {background:#0d0c0c;color:#c5c9c5;} .kimbap .preview-error {color:#c4746e;padding:32px;} .kimbap button {color:#c5c9c5;} ");
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
        window.set_titlebar(Some(&header));
        let canvas = ImageCanvas::new();
        let spinner = gtk::Spinner::new();
        spinner.set_halign(gtk::Align::Center);
        spinner.set_valign(gtk::Align::Center);
        let error = gtk::Label::new(None);
        error.set_wrap(true);
        error.set_selectable(true);
        error.set_max_width_chars(70);
        error.add_css_class("preview-error");
        let stack = gtk::Stack::new();
        stack.set_hexpand(true);
        stack.set_vexpand(true);
        stack.add_named(&canvas, Some("image"));
        stack.add_named(&error, Some("error"));
        stack.add_named(&spinner, Some("loading"));
        window.set_child(Some(&stack));
        let (decoder, receive) = Decoder::new(worker);
        let viewer = Rc::new(Self {
            endpoint,
            window,
            canvas,
            stack,
            error,
            title,
            spinner,
            decoder,
            generation: Cell::new(0),
            current: RefCell::new(None),
            task: RefCell::new(None),
            loading: RefCell::new(None),
            started: Cell::new(None),
            verbose,
            paint: RefCell::new(None),
        });
        for (label, tooltip, action) in [
            ("−", "Zoom out", 0),
            ("Fit", "Fit image", 1),
            ("+", "Zoom in", 2),
        ] {
            let button = gtk::Button::with_label(label);
            button.set_tooltip_text(Some(tooltip));
            let weak = Rc::downgrade(&viewer);
            button.connect_clicked(move |_| {
                if let Some(v) = weak.upgrade() {
                    match action {
                        0 => v.canvas.zoom_by(1. / 1.25),
                        1 => v.canvas.fit(),
                        _ => v.canvas.zoom_by(1.25),
                    }
                }
            });
            header.pack_start(&button);
        }
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
        viewer.canvas.add_controller(click);
        let weak = Rc::downgrade(&viewer);
        let task = glib::MainContext::default().spawn_local(async move {
            while let Ok(reply) = receive.recv().await {
                let Some(viewer) = weak.upgrade() else {
                    break;
                };
                viewer.receive(reply);
            }
        });
        viewer.task.replace(Some(task));
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
            .unwrap_or_else(|| "Image preview".into());
        self.window.set_title(Some(&basename));
        self.title.set_label(&basename);
        self.current.replace(Some(request));
        if same && !first {
            self.window.present();
            self.publish(parent);
            return;
        }
        let generation = self.next_generation();
        self.decoder.cancel(generation);
        self.started.set(Some(requested));
        self.cancel_loading();
        self.cancel_paint();
        if first {
            self.canvas.set_texture(None);
            self.stack.set_visible_child_name("loading");
            self.spinner.start();
        }
        self.window.present();
        self.publish(parent);
        let Some(path) = file.path() else {
            self.failure("Only local JPEG files are supported right now");
            return;
        };
        let weak = Rc::downgrade(self);
        let timer =
            glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                if let Some(v) = weak.upgrade() {
                    v.loading.borrow_mut().take();
                    v.stack.set_visible_child_name("loading");
                    v.spinner.start();
                }
            });
        self.loading.replace(Some(timer));
        self.decoder.load(generation, path);
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
    fn receive(self: &Rc<Self>, reply: Reply) {
        if reply.generation != self.generation.get() || !self.window.is_visible() {
            return;
        }
        self.cancel_loading();
        self.spinner.stop();
        match reply.result {
            Ok(decoded) => {
                let frame = decoded.frame;
                let width = frame.width as i32;
                let height = frame.height as i32;
                let decode_us = frame.decode_us;
                let bytes = glib::Bytes::from_owned(Pixels(frame));
                let texture = gdk::MemoryTexture::new(
                    width,
                    height,
                    gdk::MemoryFormat::R8g8b8a8,
                    &bytes,
                    width as usize * 4,
                );
                self.canvas.set_texture(Some(texture.upcast_ref()));
                self.stack.set_visible_child_name("image");
                if !self.window.is_fullscreen() {
                    self.size_for_image(width, height);
                }
                self.trace_paint(reply.generation);
                if self.verbose {
                    eprintln!(
                        "kimbap: image ready generation={} size={}x{} decode_us={} load_us={} cache={} total_us={}",
                        reply.generation,
                        width,
                        height,
                        decode_us,
                        decoded.elapsed.as_micros(),
                        decoded.cache_hit,
                        self.started
                            .get()
                            .map(|s| s.elapsed().as_micros())
                            .unwrap_or(0)
                    );
                }
            }
            Err(error) => self.failure(&error),
        }
    }
    fn size_for_image(&self, width: i32, height: i32) {
        if let Some(surface) = self.window.surface()
            && let Some(monitor) = surface.display().monitor_at_surface(&surface)
        {
            let geometry = monitor.geometry();
            let max_w = (geometry.width() as f64 * 0.95).max(320.);
            let max_h = (geometry.height() as f64 * 0.9 - 52.).max(240.);
            let ratio = (max_w / width as f64).min(max_h / height as f64);
            self.window.set_default_size(
                (width as f64 * ratio).round() as i32,
                (height as f64 * ratio).round() as i32,
            );
        }
    }
    fn trace_paint(self: &Rc<Self>, generation: u64) {
        self.cancel_paint();
        if !self.verbose {
            return;
        }
        if let Some(clock) = self.window.frame_clock() {
            let weak = Rc::downgrade(self);
            let id = clock.connect_after_paint(move |_| {
                if let Some(viewer) = weak.upgrade() {
                    viewer.cancel_paint();
                    if viewer.generation.get() == generation {
                        eprintln!(
                            "kimbap: image painted generation={generation} total_us={}",
                            viewer
                                .started
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
    fn failure(&self, message: &str) {
        self.cancel_loading();
        self.spinner.stop();
        self.canvas.set_texture(None);
        self.error
            .set_label(&format!("Could not preview this image\n\n{message}"));
        self.stack.set_visible_child_name("error");
        eprintln!("kimbap: JPEG preview failed: {message}");
    }
    pub fn close(&self) {
        let generation = self.next_generation();
        self.decoder.cancel(generation);
        self.cancel_loading();
        self.cancel_paint();
        self.spinner.stop();
        self.window.set_visible(false);
        self.canvas.set_texture(None);
        self.current.borrow_mut().take();
        self.publish(String::new());
    }
    fn event(&self, event: Event) {
        if self.verbose
            && let Event::Selection(direction) = &event
        {
            eprintln!(
                "kimbap: selection direction={} at_us={}",
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
        let ctrl = modifiers.contains(gdk::ModifierType::CONTROL_MASK);
        match key {
            gdk::Key::Escape | gdk::Key::space | gdk::Key::q => self.close(),
            gdk::Key::Left => self.event(Event::Selection(Direction::Left)),
            gdk::Key::Right => self.event(Event::Selection(Direction::Right)),
            gdk::Key::Up => self.event(Event::Selection(Direction::Up)),
            gdk::Key::Down => self.event(Event::Selection(Direction::Down)),
            gdk::Key::F11 => self.fullscreen(),
            gdk::Key::_0 if ctrl => self.canvas.fit(),
            gdk::Key::_1 if ctrl => self.canvas.native_size(),
            gdk::Key::plus | gdk::Key::equal | gdk::Key::KP_Add => self.canvas.zoom_by(1.25),
            gdk::Key::minus | gdk::Key::KP_Subtract => self.canvas.zoom_by(1. / 1.25),
            gdk::Key::c if ctrl => {
                if let Some(texture) = self.canvas.texture() {
                    self.window.clipboard().set_texture(&texture);
                }
            }
            gdk::Key::F2 => {
                let uri = self.current.borrow().as_ref().map(|r| r.uri.clone());
                if let Some(uri) = uri {
                    self.close();
                    // Let the compositor return focus before Nautilus opens
                    // its rename popover, as required by the existing Sushi UI.
                    let endpoint = self.endpoint.clone();
                    glib::timeout_add_local_once(
                        std::time::Duration::from_millis(200),
                        move || {
                            if let Err(error) = endpoint.emit(&Event::Rename(uri)) {
                                eprintln!("kimbap: Nautilus rename event: {error}");
                            }
                        },
                    );
                }
            }
            gdk::Key::Delete => {
                let uri = self.current.borrow().as_ref().map(|r| r.uri.clone());
                if let Some(uri) = uri {
                    self.event(Event::Trash(uri));
                }
            }
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    }
}
impl Drop for Viewer {
    fn drop(&mut self) {
        self.cancel_loading();
        self.cancel_paint();
        if let Some(task) = self.task.get_mut().take() {
            task.abort();
        }
        self.window.destroy();
    }
}
