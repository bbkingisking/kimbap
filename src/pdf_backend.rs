//! Single-page PDF presentation; arrows remain reserved for Nautilus selection.
use crate::canvas::{ImageCanvas, Pixels};
use gtk::{gdk, glib, prelude::*};
use kimbap::{
    backend::{Backend, Event, Notify},
    pdf::{Decoder, Reply},
};
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    time::Duration,
};

pub struct PdfBackend {
    canvas: ImageCanvas,
    controls: gtk::Box,
    previous: gtk::Button,
    next: gtk::Button,
    label: gtk::Label,
    decoder: Decoder,
    notify: Notify,
    path: RefCell<Option<PathBuf>>,
    generation: Cell<u64>,
    revision: Cell<u64>,
    page: Cell<u32>,
    pages: Cell<u32>,
    page_size: Cell<Option<[f64; 2]>>,
    bounds: Cell<(u32, u32)>,
    ready: Cell<bool>,
    resize: RefCell<Option<glib::SourceId>>,
    task: RefCell<Option<glib::JoinHandle<()>>>,
}
impl PdfBackend {
    pub fn new(worker: PathBuf, notify: Notify) -> Rc<Self> {
        let (decoder, receive) = Decoder::new(worker);
        let backend = Rc::new(Self {
            canvas: ImageCanvas::for_document(),
            controls: gtk::Box::new(gtk::Orientation::Horizontal, 6),
            previous: gtk::Button::with_label("‹"),
            next: gtk::Button::with_label("›"),
            label: gtk::Label::new(None),
            decoder,
            notify,
            path: RefCell::new(None),
            generation: Cell::new(0),
            revision: Cell::new(0),
            page: Cell::new(0),
            pages: Cell::new(0),
            page_size: Cell::new(None),
            bounds: Cell::new((0, 0)),
            ready: Cell::new(false),
            resize: RefCell::new(None),
            task: RefCell::new(None),
        });
        backend
            .previous
            .set_tooltip_text(Some("Previous page (Page Up)"));
        backend.next.set_tooltip_text(Some("Next page (Page Down)"));
        backend.controls.append(&backend.previous);
        backend.controls.append(&backend.label);
        backend.controls.append(&backend.next);
        for (button, forward) in [(&backend.previous, false), (&backend.next, true)] {
            let weak = Rc::downgrade(&backend);
            button.connect_clicked(move |_| {
                if let Some(v) = weak.upgrade() {
                    v.turn(forward);
                }
            });
        }
        for (label, tooltip, action) in [
            ("−", "Zoom out", 0),
            ("Fit", "Fit page", 1),
            ("+", "Zoom in", 2),
        ] {
            let button = gtk::Button::with_label(label);
            button.set_tooltip_text(Some(tooltip));
            let weak = Rc::downgrade(&backend);
            button.connect_clicked(move |_| {
                if let Some(v) = weak.upgrade() {
                    match action {
                        0 => v.canvas.zoom_by(1. / 1.25),
                        1 => v.canvas.fit(),
                        _ => v.canvas.zoom_by(1.25),
                    }
                }
            });
            backend.controls.append(&button);
        }
        let weak = Rc::downgrade(&backend);
        backend
            .canvas
            .connect_local("view-changed", false, move |_| {
                if let Some(v) = weak.upgrade() {
                    v.schedule_render();
                }
                None
            });
        let scale_handler = Rc::new(RefCell::new(None::<(gdk::Surface, glib::SignalHandlerId)>));
        let weak = Rc::downgrade(&backend);
        let handler = scale_handler.clone();
        backend.canvas.connect_realize(move |canvas| {
            if let Some(surface) = canvas.native().and_then(|native| native.surface()) {
                let weak = weak.clone();
                let id = surface.connect_scale_notify(move |_| {
                    if let Some(v) = weak.upgrade() {
                        v.schedule_render();
                    }
                });
                handler.replace(Some((surface, id)));
            }
        });
        backend.canvas.connect_unrealize(move |_| {
            if let Some((surface, id)) = scale_handler.borrow_mut().take() {
                surface.disconnect(id);
            }
        });
        let weak = Rc::downgrade(&backend);
        backend
            .task
            .replace(Some(glib::MainContext::default().spawn_local(async move {
                while let Ok(reply) = receive.recv().await {
                    let Some(v) = weak.upgrade() else {
                        break;
                    };
                    v.receive(reply);
                }
            })));
        backend.update_controls();
        backend
    }
    fn update_controls(&self) {
        self.previous
            .set_sensitive(self.pages.get() > 0 && self.page.get() > 0);
        self.next
            .set_sensitive(self.pages.get() > 0 && self.page.get() + 1 < self.pages.get());
        if self.pages.get() > 0 {
            self.label
                .set_text(&format!("{} / {}", self.page.get() + 1, self.pages.get()));
        } else {
            self.label.set_text("Loading…");
        }
    }
    fn cancel_resize(&self) {
        if let Some(timer) = self.resize.borrow_mut().take() {
            timer.remove();
        }
    }
    fn schedule_render(self: &Rc<Self>) {
        if self.path.borrow().is_none() {
            return;
        }
        self.cancel_resize();
        let weak = Rc::downgrade(self);
        self.resize.replace(Some(glib::timeout_add_local_once(
            Duration::from_millis(80),
            move || {
                if let Some(v) = weak.upgrade() {
                    v.resize.borrow_mut().take();
                    v.request(false);
                }
            },
        )));
    }
    fn request(&self, force: bool) {
        let Some(path) = self.path.borrow().clone() else {
            return;
        };
        let bounds = self.canvas.render_bounds(self.page_size.get());
        if !force && self.bounds.get() == bounds {
            return;
        }
        self.bounds.set(bounds);
        log::debug!(
            "PDF render viewport={}x{} scale={} target={}x{}",
            self.canvas.width(),
            self.canvas.height(),
            self.canvas
                .native()
                .and_then(|n| n.surface())
                .map(|s| s.scale())
                .unwrap_or(1.),
            bounds.0,
            bounds.1
        );
        self.revision.set(self.decoder.render(
            self.generation.get(),
            path,
            self.page.get(),
            bounds.0,
            bounds.1,
        ));
    }
    fn turn(&self, forward: bool) {
        let page = self.page.get();
        let target = if forward {
            page.saturating_add(1)
        } else {
            page.saturating_sub(1)
        };
        if target == page || target >= self.pages.get() {
            return;
        }
        self.cancel_resize();
        self.page.set(target);
        self.page_size.set(None);
        self.canvas.fit();
        self.update_controls();
        self.request(true);
    }
    fn receive(self: &Rc<Self>, reply: Reply) {
        if reply.generation != self.generation.get()
            || reply.revision != self.revision.get()
            || self.path.borrow().is_none()
        {
            return;
        }
        match reply.result {
            Ok(rendered) => {
                let frame = rendered.frame;
                let (width, height) = (frame.width as i32, frame.height as i32);
                let decode_us = frame.decode_us;
                let bytes = glib::Bytes::from_owned(Pixels(frame));
                let texture = gdk::MemoryTexture::new(
                    width,
                    height,
                    gdk::MemoryFormat::R8g8b8a8,
                    &bytes,
                    width as usize * 4,
                );
                // Keep the same layout geometry across rerenders of this page.
                // Rounded bitmap dimensions can vary with the requested density.
                let page_size = self
                    .page_size
                    .get()
                    .unwrap_or([width as f64, height as f64]);
                self.page_size.set(Some(page_size));
                self.canvas.replace_texture(texture.upcast_ref(), page_size);
                self.pages.set(rendered.pages);
                self.update_controls();
                // Opening a document starts without its aspect ratio. Once the
                // page is known, allocate the density budget to its real bounds.
                if self.canvas.render_bounds(Some(page_size)) != self.bounds.get() {
                    self.schedule_render();
                }
                log::debug!(
                    "PDF ready generation={} page={}/{} size={}x{} render_us={} load_us={} cache={}",
                    reply.generation,
                    reply.page + 1,
                    rendered.pages,
                    width,
                    height,
                    decode_us,
                    rendered.elapsed.as_micros(),
                    rendered.cache_hit
                );
                // Resizing after every zoom render would resize the window and
                // trigger another render. Only the first page sizes the shell.
                if !self.ready.replace(true) {
                    (self.notify)(Event::Ready {
                        generation: reply.generation,
                        width,
                        height,
                    });
                }
            }
            Err(message) => (self.notify)(Event::Failed {
                generation: reply.generation,
                message,
            }),
        }
    }
}
impl Backend for PdfBackend {
    fn widget(&self) -> gtk::Widget {
        self.canvas.clone().upcast()
    }
    fn controls(&self) -> gtk::Widget {
        self.controls.clone().upcast()
    }
    fn load(&self, generation: u64, path: PathBuf) {
        self.cancel_resize();
        self.decoder.cancel();
        self.generation.set(generation);
        self.path.replace(Some(path));
        self.page.set(0);
        self.page_size.set(None);
        self.pages.set(0);
        self.ready.set(false);
        self.canvas.set_texture(None);
        self.update_controls();
        self.request(true);
    }
    fn stop(&self, generation: u64) {
        self.cancel_resize();
        self.generation.set(generation);
        self.path.borrow_mut().take();
        self.decoder.cancel();
        self.canvas.set_texture(None);
    }
    fn key(&self, key: gdk::Key, modifiers: gdk::ModifierType) -> bool {
        let ctrl = modifiers.contains(gdk::ModifierType::CONTROL_MASK);
        match key {
            gdk::Key::Page_Up => self.turn(false),
            gdk::Key::Page_Down => self.turn(true),
            gdk::Key::_0 if ctrl => self.canvas.fit(),
            gdk::Key::plus | gdk::Key::equal | gdk::Key::KP_Add => self.canvas.zoom_by(1.25),
            gdk::Key::minus | gdk::Key::KP_Subtract => self.canvas.zoom_by(1. / 1.25),
            _ => return false,
        };
        true
    }
}
impl Drop for PdfBackend {
    fn drop(&mut self) {
        self.cancel_resize();
        if let Some(task) = self.task.get_mut().take() {
            task.abort();
        }
    }
}
