//! Nautilus's preview protocol, independent of any format or renderer.
//!
//! Export on the GLib main context used by the eventual preview UI. Exporting
//! does not acquire a bus name or create a window; the executable controls the
//! service lifecycle and currently accepts commands without acting on them.

use std::{cell::RefCell, collections::HashMap, rc::Rc};

use glib::variant::ToVariant;

pub const BUS_NAME: &str = "org.gnome.NautilusPreviewer";
pub const OBJECT_PATH: &str = "/org/gnome/NautilusPreviewer";
pub const INTERFACE: &str = "org.gnome.NautilusPreviewer2";
pub const INTERFACE_XML: &str = include_str!("../data/org.gnome.NautilusPreviewer2.xml");

#[derive(Clone, PartialEq, Eq)]
pub struct PreviewRequest {
    /// Preserve the URI verbatim: Nautilus may request remote files or folders.
    pub uri: String,
    /// Opaque Wayland/X11 parent identifier, not a window title.
    pub window_handle: String,
    pub close_if_already_shown: bool,
    /// Opaque launch token. Do not log it or reuse it for another request.
    pub activation_token: String,
}

#[derive(Clone, PartialEq, Eq)]
pub enum Command {
    ShowFile(PreviewRequest),
    Close,
}

/// Numeric values belong to GtkDirectionType, as expected by Nautilus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Direction {
    TabForward = 0,
    TabBackward = 1,
    Up = 2,
    Down = 3,
    Left = 4,
    Right = 5,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Selection(Direction),
    NavigateTo(String),
    Rename(String),
    Trash(String),
}

/// Actual window state, not merely the most recent accepted ShowFile request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowState {
    pub visible: bool,
    pub parent_handle: String,
}

/// Registration is removed on drop. Keep this alive for the service lifetime.
pub struct PreviewEndpoint {
    connection: gio::DBusConnection,
    registration: Option<gio::RegistrationId>,
    state: Rc<RefCell<WindowState>>,
}

impl PreviewEndpoint {
    pub fn export(
        connection: &gio::DBusConnection,
        handle_command: impl Fn(Command) + 'static,
    ) -> Result<Self, glib::Error> {
        let info = gio::DBusNodeInfo::for_xml(INTERFACE_XML)?
            .lookup_interface(INTERFACE)
            .expect("embedded preview interface is present");
        let state = Rc::new(RefCell::new(WindowState::default()));
        let property_state = state.clone();
        let registration = connection
            .register_object(OBJECT_PATH, &info)
            .method_call(move |_, _, _, _, method, parameters, invocation| {
                let command = match method {
                    "ShowFile" => {
                        let Some((uri, window_handle, close_if_already_shown, activation_token)) =
                            parameters.get::<(String, String, bool, String)>()
                        else {
                            invocation.return_dbus_error(
                                "org.freedesktop.DBus.Error.InvalidArgs",
                                "ShowFile expects (ssbs)",
                            );
                            return;
                        };
                        Command::ShowFile(PreviewRequest {
                            uri,
                            window_handle,
                            close_if_already_shown,
                            activation_token,
                        })
                    }
                    "Close" => Command::Close,
                    _ => {
                        invocation.return_dbus_error(
                            "org.freedesktop.DBus.Error.UnknownMethod",
                            "Unknown preview method",
                        );
                        return;
                    }
                };
                handle_command(command);
                invocation.return_value(Some(&().to_variant()));
            })
            .property(move |_, _, _, _, name| {
                let state = property_state.borrow();
                match name {
                    "Visible" => state.visible.to_variant(),
                    "ParentHandle" => state.parent_handle.to_variant(),
                    _ => unreachable!("GIO validates properties against the interface"),
                }
            })
            .build()?;
        Ok(Self {
            connection: connection.clone(),
            registration: Some(registration),
            state,
        })
    }

    /// Future UI code calls this when window state actually changes. The no-op
    /// executable never calls it, so Nautilus correctly sees Visible=false.
    pub fn set_window_state(&self, next: WindowState) -> Result<(), glib::Error> {
        let mut changed = HashMap::<&str, glib::Variant>::new();
        {
            let mut current = self.state.borrow_mut();
            if current.visible != next.visible {
                changed.insert("Visible", next.visible.to_variant());
            }
            if current.parent_handle != next.parent_handle {
                changed.insert("ParentHandle", next.parent_handle.to_variant());
            }
            *current = next;
        }
        if !changed.is_empty() {
            self.connection.emit_signal(
                None,
                OBJECT_PATH,
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                Some(&(INTERFACE, changed, Vec::<String>::new()).to_variant()),
            )?;
        }
        Ok(())
    }

    /// Future UI actions go back to Nautilus; Kimbap never guesses file order.
    pub fn emit(&self, event: &Event) -> Result<(), glib::Error> {
        let (name, parameters) = match event {
            Event::Selection(direction) => ("SelectionEvent", (*direction as u32,).to_variant()),
            Event::NavigateTo(uri) => ("NavigateTo", (uri,).to_variant()),
            Event::Rename(uri) => ("Rename", (uri,).to_variant()),
            Event::Trash(uri) => ("Trash", (uri,).to_variant()),
        };
        self.connection
            .emit_signal(None, OBJECT_PATH, INTERFACE, name, Some(&parameters))
    }
}

impl Drop for PreviewEndpoint {
    fn drop(&mut self) {
        if let Some(registration) = self.registration.take() {
            let _ = self.connection.unregister_object(registration);
        }
    }
}
