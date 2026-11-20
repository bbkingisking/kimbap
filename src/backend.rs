//! The preview shell owns Nautilus/window behavior; format backends own content.
use gtk::gdk;
use std::{
    path::{Path, PathBuf},
    rc::Rc,
};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Image,
    Pdf,
    Video,
    Text,
}
impl Kind {
    pub fn for_content_type(content_type: &str) -> Option<Self> {
        if gio::content_type_is_a(content_type, "image/jpeg")
            || gio::content_type_is_a(content_type, "image/png")
        {
            Some(Self::Image)
        } else if gio::content_type_is_a(content_type, "application/pdf") {
            Some(Self::Pdf)
        } else if content_type.starts_with("video/") {
            Some(Self::Video)
        } else if Self::is_text(content_type) {
            Some(Self::Text)
        } else {
            None
        }
    }
    fn is_text(content_type: &str) -> bool {
        gio::content_type_is_a(content_type, "text/plain")
            || gio::content_type_is_a(content_type, "application/json")
            || gio::content_type_is_a(content_type, "application/yaml")
            || gio::content_type_is_a(content_type, "application/x-yaml")
    }

    pub fn for_file(content_type: Option<&str>, path: &Path) -> Option<Self> {
        // Keep content-based media routing authoritative. Consult the filename
        // only for text: exported text may contain controls that confuse sniffing.
        content_type.and_then(Self::for_content_type).or_else(|| {
            let (named_type, _) = gio::content_type_guess(Some(path), None::<&[u8]>);
            Self::is_text(&named_type).then_some(Self::Text)
        })
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
    fn filename_can_recognize_text_despite_binary_sniffing() {
        for name in ["paper.fr-t1.txt", "script.py", "config.yaml", "data.json"] {
            assert_eq!(
                Kind::for_file(Some("application/octet-stream"), Path::new(name)),
                Some(Kind::Text)
            );
        }
        assert_eq!(
            Kind::for_file(Some("application/octet-stream"), Path::new("unknown.bin")),
            None
        );
        assert_eq!(
            Kind::for_file(Some("text/plain"), Path::new("extensionless")),
            Some(Kind::Text)
        );
        assert_eq!(
            Kind::for_file(Some("image/png"), Path::new("misleading.txt")),
            Some(Kind::Image)
        );
    }

    #[test]
    fn routes_without_decoding_fallback() {
        assert_eq!(Kind::for_content_type("image/jpeg"), Some(Kind::Image));
        assert_eq!(Kind::for_content_type("image/png"), Some(Kind::Image));
        assert_eq!(
            Kind::for_content_type("video/x-matroska"),
            Some(Kind::Video)
        );
        assert_eq!(Kind::for_content_type("video/webm"), Some(Kind::Video));
        assert_eq!(Kind::for_content_type("application/pdf"), Some(Kind::Pdf));
        assert_eq!(Kind::for_content_type("text/plain"), Some(Kind::Text));
        for mime in ["application/json", "application/yaml", "text/x-python"] {
            assert_eq!(Kind::for_content_type(mime), Some(Kind::Text));
        }
        assert_eq!(Kind::for_content_type("application/octet-stream"), None);
    }
}
