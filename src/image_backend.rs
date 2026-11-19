//! JPEG presentation adapter. Worker scheduling and the canvas remain independent
//! of the shared preview window and of every other format backend.
use crate::canvas::{ImageCanvas, Pixels};
use gtk::{gdk, glib, prelude::*};
use kimbap::{
    backend::{Backend, Event, Notify},
    jpeg::{Decoder, Reply},
};
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    time::Instant,
};
pub struct ImageBackend {
    canvas: ImageCanvas,
    controls: gtk::Box,
    decoder: Decoder,
    notify: Notify,
    generation: Cell<u64>,
    task: RefCell<Option<glib::JoinHandle<()>>>,
    started: Cell<Instant>,
}
impl ImageBackend {
    pub fn new(worker: PathBuf, notify: Notify) -> Rc<Self> {
        let canvas = ImageCanvas::new();
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let (decoder, receive) = Decoder::new(worker);
        let backend = Rc::new(Self {
            canvas,
            controls,
            decoder,
            notify,
            generation: Cell::new(0),
            task: RefCell::new(None),
            started: Cell::new(Instant::now()),
        });
        for (label, tooltip, action) in [
            ("−", "Zoom out", 0),
            ("Fit", "Fit image", 1),
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
        let task = glib::MainContext::default().spawn_local(async move {
            while let Ok(reply) = receive.recv().await {
                let Some(v) = weak.upgrade() else {
                    break;
                };
                v.receive(reply);
            }
        });
        backend.task.replace(Some(task));
        backend
    }
    fn receive(&self, reply: Reply) {
        if reply.generation != self.generation.get() {
            return;
        }
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
                log::debug!(
                    "image ready generation={} size={}x{} decode_us={} load_us={} cache={} elapsed_us={}",
                    reply.generation,
                    width,
                    height,
                    decode_us,
                    decoded.elapsed.as_micros(),
                    decoded.cache_hit,
                    self.started.get().elapsed().as_micros()
                );
                (self.notify)(Event::Ready {
                    generation: reply.generation,
                    width,
                    height,
                });
            }
            Err(message) => (self.notify)(Event::Failed {
                generation: reply.generation,
                message,
            }),
        }
    }
}
impl Backend for ImageBackend {
    fn widget(&self) -> gtk::Widget {
        self.canvas.clone().upcast()
    }
    fn controls(&self) -> gtk::Widget {
        self.controls.clone().upcast()
    }
    fn load(&self, generation: u64, path: PathBuf) {
        self.generation.set(generation);
        self.started.set(Instant::now());
        self.decoder.load(generation, path);
    }
    fn stop(&self, generation: u64) {
        self.generation.set(generation);
        self.decoder.cancel(generation);
        self.canvas.set_texture(None);
    }
    fn key(&self, key: gdk::Key, modifiers: gdk::ModifierType) -> bool {
        let ctrl = modifiers.contains(gdk::ModifierType::CONTROL_MASK);
        match key {
            gdk::Key::_0 if ctrl => self.canvas.fit(),
            gdk::Key::_1 if ctrl => self.canvas.native_size(),
            gdk::Key::plus | gdk::Key::equal | gdk::Key::KP_Add => self.canvas.zoom_by(1.25),
            gdk::Key::minus | gdk::Key::KP_Subtract => self.canvas.zoom_by(1. / 1.25),
            gdk::Key::c if ctrl => {
                if let Some(texture) = self.canvas.texture() {
                    self.canvas.clipboard().set_texture(&texture);
                }
            }
            _ => return false,
        };
        true
    }
}
impl Drop for ImageBackend {
    fn drop(&mut self) {
        if let Some(task) = self.task.get_mut().take() {
            task.abort();
        }
    }
}
