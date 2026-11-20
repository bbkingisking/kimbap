//! Bounded UTF-8 previews, for files recognized as text by the system MIME database.
use gtk::{glib, prelude::*};
use kimbap::backend::{Backend, Event, Notify};
use std::{borrow::Cow, cell::RefCell, path::PathBuf, rc::Rc};

const LIMIT: usize = 1024 * 1024;

pub struct TextBackend {
    scroll: gtk::ScrolledWindow,
    view: gtk::TextView,
    status: gtk::Label,
    task: RefCell<Option<glib::JoinHandle<()>>>,
    notify: Notify,
}

impl TextBackend {
    pub fn new(notify: Notify) -> Rc<Self> {
        let view = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .left_margin(20)
            .right_margin(20)
            .top_margin(16)
            .bottom_margin(16)
            .build();
        view.add_css_class("text-preview");
        let scroll = gtk::ScrolledWindow::builder().child(&view).build();
        Rc::new(Self {
            scroll,
            view,
            status: gtk::Label::new(None),
            task: RefCell::new(None),
            notify,
        })
    }
}

// Read one extra byte to distinguish an exact-limit file from a truncated one.
// Only an incomplete trailing UTF-8 sequence at the cutoff may be discarded.
fn decode(bytes: &[u8]) -> Result<(Cow<'_, str>, bool), String> {
    let truncated = bytes.len() > LIMIT;
    let prefix = &bytes[..bytes.len().min(LIMIT)];
    let text = match std::str::from_utf8(prefix) {
        Ok(text) => text,
        Err(error) if truncated && error.error_len().is_none() => {
            std::str::from_utf8(&prefix[..error.valid_up_to()]).unwrap()
        }
        Err(_) => return Err("This file is not UTF-8 text".into()),
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    // Preserve line structure; render other controls instead of hiding them or
    // passing NUL through GTK's string API. No control-density heuristic needed.
    let unusual = |c: char| c.is_control() && !matches!(c, '\n' | '\r' | '\t');
    let display = if text.chars().any(unusual) {
        let mut display = String::with_capacity(text.len());
        for c in text.chars() {
            if unusual(c) {
                match c as u32 {
                    0..=31 => display.push(char::from_u32(0x2400 + c as u32).unwrap()),
                    127 => display.push('␡'),
                    _ => display.push_str(&format!("\\u{{{:04X}}}", c as u32)),
                }
            } else {
                display.push(c);
            }
        }
        Cow::Owned(display)
    } else {
        Cow::Borrowed(text)
    };
    Ok((display, truncated))
}

impl Backend for TextBackend {
    fn widget(&self) -> gtk::Widget {
        self.scroll.clone().upcast()
    }

    fn controls(&self) -> gtk::Widget {
        self.status.clone().upcast()
    }

    fn load(&self, generation: u64, path: PathBuf) {
        self.stop(generation);
        let view = self.view.clone();
        let scroll = self.scroll.clone();
        let status = self.status.clone();
        let notify = self.notify.clone();
        let task = glib::MainContext::default().spawn_local(async move {
            let result = async {
                let stream = gio::File::for_path(path)
                    .read_future(glib::Priority::DEFAULT)
                    .await
                    .map_err(|e| format!("Cannot read this file: {e}"))?;
                let mut bytes = Vec::new();
                while bytes.len() <= LIMIT {
                    let chunk = stream
                        .read_bytes_future(
                            (LIMIT + 1 - bytes.len()).min(64 * 1024),
                            glib::Priority::DEFAULT,
                        )
                        .await
                        .map_err(|e| format!("Cannot read this file: {e}"))?;
                    if chunk.is_empty() {
                        break;
                    }
                    bytes.extend_from_slice(&chunk);
                }
                let (text, truncated) = decode(&bytes)?;
                view.buffer().set_text(&text);
                status.set_label(if truncated {
                    "First 1 MiB · truncated"
                } else {
                    "UTF-8"
                });
                scroll.hadjustment().set_value(0.);
                scroll.vadjustment().set_value(0.);
                Ok::<_, String>(())
            }
            .await;
            notify(match result {
                Ok(()) => Event::Ready {
                    generation,
                    width: 900,
                    height: 700,
                },
                Err(message) => Event::Failed {
                    generation,
                    message,
                },
            });
        });
        self.task.replace(Some(task));
    }

    fn stop(&self, _: u64) {
        if let Some(task) = self.task.borrow_mut().take() {
            task.abort();
        }
        self.view.buffer().set_text("");
        self.status.set_label("");
    }
}

impl Drop for TextBackend {
    fn drop(&mut self) {
        if let Some(task) = self.task.get_mut().take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_text_without_relying_on_extensions() {
        for text in [
            "",
            "{\"한글\": true}\n",
            "key: value\n\t- item",
            "\u{feff}hello",
        ] {
            assert_eq!(
                decode(text.as_bytes()).unwrap(),
                (Cow::Borrowed(text.trim_start_matches('\u{feff}')), false)
            );
        }
    }

    #[test]
    fn rejects_invalid_encoding() {
        for bytes in [&[0xff, 0xfe], &[0xe2, 0x82]] {
            assert!(decode(bytes).is_err());
        }
    }

    #[test]
    fn displays_controls_instead_of_rejecting_text() {
        assert_eq!(decode(b"export\x1e\x04\n").unwrap().0, "export␞␄\n");
        assert_eq!(decode(b"a\0b\x1b\x7f").unwrap().0, "a␀b␛␡");
        assert_eq!(decode("a\u{85}b".as_bytes()).unwrap().0, "a\\u{0085}b");
    }

    #[test]
    fn handles_limit_and_split_codepoints() {
        let mut bytes = vec![b'a'; LIMIT - 1];
        bytes.extend_from_slice("한".as_bytes());
        let (text, truncated) = decode(&bytes).unwrap();
        assert!(truncated);
        assert_eq!(text.len(), LIMIT - 1);
        assert_eq!(decode(&vec![b'a'; LIMIT]).unwrap().1, false);
        bytes[0] = 0xff;
        assert!(decode(&bytes).is_err());
    }
}
