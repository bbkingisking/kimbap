mod canvas;
mod image_backend;
mod pdf_backend;
mod text_backend;
mod viewer;

use std::{
    cell::RefCell,
    error::Error,
    path::PathBuf,
    process::ExitCode,
    rc::{Rc, Weak},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use gio::prelude::*;
use glib::variant::ToVariant;
use kimbap::{BUS_NAME, Command, PreviewEndpoint};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn main() -> ExitCode {
    env_logger::init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("kimbap: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let mut bus_name = BUS_NAME.to_owned();
    let mut transport_only = false;
    let mut worker = std::env::current_exe()?.with_file_name("kimbap-jpeg-worker");
    let mut pdf_worker = std::env::current_exe()?.with_file_name("kimbap-pdf-worker");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bus-name" => bus_name = args.next().ok_or("--bus-name requires a name")?,
            "--transport-only" => transport_only = true,
            "--jpeg-worker" => {
                worker = PathBuf::from(args.next().ok_or("--jpeg-worker requires a path")?)
            }
            "--pdf-worker" => {
                pdf_worker = PathBuf::from(args.next().ok_or("--pdf-worker requires a path")?)
            }
            "--version" => {
                println!("kimbap {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                println!(
                    "Usage: kimbap [--bus-name NAME] [--jpeg-worker PATH] [--pdf-worker PATH] [--transport-only]\n\n\
                     Format backends in a shared GTK4 preview window.\n\n\
                     --bus-name NAME  Alternative name for isolated development\n\
                     --jpeg-worker    Path to the bundled decoder executable\n\
                     --pdf-worker     Path to the bundled PDF renderer executable\n\
                     --transport-only Export the no-op interface without a display\n\n\
                     Set RUST_LOG=kimbap=debug for command and rendering diagnostics."
                );
                return Ok(());
            }
            _ => return Err(format!("unknown argument {arg:?}; see --help").into()),
        }
    }
    if !gio::dbus_is_name(&bus_name) || bus_name.starts_with(':') {
        return Err(format!("not a well-known D-Bus name: {bus_name:?}").into());
    }

    let context = glib::MainContext::default();
    let connection = context.block_on(gio::bus_get_future(gio::BusType::Session))?;
    connection.set_exit_on_close(false);
    let occupied = context.block_on(connection.call_future(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "NameHasOwner",
        Some(&(bus_name.as_str(),).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        5000,
    ))?;
    if occupied.get::<(bool,)>() == Some((true,)) {
        return Err(format!(
            "D-Bus name {bus_name} is already owned; stop its owner explicitly or use --bus-name"
        )
        .into());
    }
    if !transport_only {
        gtk::init()?;
        #[cfg(feature = "video")]
        kimbap::video::initialize()?;
    }
    let ui: Rc<RefCell<Weak<viewer::Viewer>>> = Rc::new(RefCell::new(Weak::new()));
    let dispatch = ui.clone();
    let endpoint = Rc::new(PreviewEndpoint::export(&connection, move |command| {
        match &command {
            Command::ShowFile(request) => log::debug!(
                "ShowFile uri={:?} parent={} close_if_already_shown={} activation={} at_us={}",
                request.uri,
                !request.window_handle.is_empty(),
                request.close_if_already_shown,
                !request.activation_token.is_empty(),
                glib::monotonic_time(),
            ),
            Command::Close => log::debug!("Close"),
        }
        if let Some(viewer) = dispatch.borrow().upgrade() {
            viewer.handle(command);
        }
    })?);
    let viewer = if transport_only {
        None
    } else {
        Some(viewer::Viewer::new(endpoint.clone(), worker, pdf_worker)?)
    };
    if let Some(viewer) = &viewer {
        *ui.borrow_mut() = Rc::downgrade(viewer);
    }

    // DO_NOT_QUEUE: never replace Sushi, nor wait silently to take its name later.
    // Export first so clients can introspect/call immediately after name acquisition.
    let reply = context.block_on(connection.call_future(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "RequestName",
        Some(&(bus_name.as_str(), 4u32).to_variant()),
        Some(glib::VariantTy::new("(u)")?),
        gio::DBusCallFlags::NONE,
        5_000,
    ))?;
    if reply.get::<(u32,)>() != Some((1,)) {
        return Err(format!(
            "D-Bus name {bus_name} is already owned; stop its owner explicitly or use --bus-name"
        )
        .into());
    }

    let main_loop = glib::MainLoop::new(Some(&context), false);
    let failed = Arc::new(AtomicBool::new(false));
    let closed = connection.connect_closed({
        let main_loop = main_loop.clone();
        let failed = failed.clone();
        move |_, _, _| {
            eprintln!("kimbap: session bus connection closed");
            failed.store(true, Ordering::Relaxed);
            main_loop.quit();
        }
    });
    let lost = connection.subscribe_to_signal(
        Some("org.freedesktop.DBus"),
        Some("org.freedesktop.DBus"),
        Some("NameLost"),
        Some("/org/freedesktop/DBus"),
        Some(&bus_name),
        gio::DBusSignalFlags::NONE,
        {
            let main_loop = main_loop.clone();
            let failed = failed.clone();
            move |_| {
                eprintln!("kimbap: preview bus name lost");
                failed.store(true, Ordering::Relaxed);
                main_loop.quit();
            }
        },
    );
    let signals: Vec<_> = [2, 15]
        .into_iter()
        .map(|signal| {
            let main_loop = main_loop.clone();
            glib_unix::unix_signal_add_local(signal, move || {
                main_loop.quit();
                glib::ControlFlow::Continue
            })
        })
        .collect();

    eprintln!(
        "kimbap: ready on {bus_name} ({})",
        if transport_only {
            "transport only"
        } else if cfg!(feature = "video") {
            "JPEG/PNG + PDF + video viewer"
        } else {
            "JPEG/PNG + PDF viewer"
        }
    );
    main_loop.run();
    for signal in signals {
        signal.remove();
    }
    drop(lost);
    connection.disconnect(closed);
    if let Some(viewer) = viewer {
        viewer.close();
        drop(viewer);
    }
    drop(endpoint);
    if !connection.is_closed() {
        context.block_on(connection.call_future(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "ReleaseName",
            Some(&(bus_name.as_str(),).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            5_000,
        ))?;
    }
    if failed.load(Ordering::Relaxed) {
        return Err("preview service disconnected".into());
    }
    Ok(())
}
