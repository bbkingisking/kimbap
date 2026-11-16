//! The preview shell owns Nautilus/window behavior; format backends own content.
use gtk::gdk;
use std::{path::PathBuf, rc::Rc};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Jpeg,
    Video,
}
impl Kind {
    pub fn for_content_type(content_type: &str) -> Option<Self> {
        if gio::content_type_is_a(content_type, "image/jpeg") {
            Some(Self::Jpeg)
        } else if content_type.starts_with("video/") {
            Some(Self::Video)
        } else {
            None
        }
    }
}
pub enum Event {
    Ready {
        generation: u64,
        width: i32,
        height: i32,
    },
    Failed {
        generation: u64,
        message: String,
    },
}
pub type Notify = Rc<dyn Fn(Event)>;
pub trait Backend {
    fn widget(&self) -> gtk::Widget;
    fn controls(&self) -> gtk::Widget;
    fn load(&self, generation: u64, path: PathBuf);
    fn stop(&self, generation: u64);
    fn key(&self, _: gdk::Key, _: gdk::ModifierType) -> bool {
        false
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn routes_without_decoding_fallback() {
        assert_eq!(Kind::for_content_type("image/jpeg"), Some(Kind::Jpeg));
        assert_eq!(
            Kind::for_content_type("video/x-matroska"),
            Some(Kind::Video)
        );
        assert_eq!(Kind::for_content_type("video/webm"), Some(Kind::Video));
        assert_eq!(Kind::for_content_type("text/plain"), None);
    }
}
