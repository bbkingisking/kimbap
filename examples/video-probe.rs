//! Isolated libmpv/GTK experiment. No Nautilus endpoint or installed service changes.
use gtk::prelude::*;
use kimbap::{
    backend::{Backend, Event},
    preferences::Preferences,
    video::VideoBackend,
};
use std::{path::PathBuf, rc::Rc, time::Duration};
fn main() {
    let mut args = std::env::args().skip(1);
    let file = PathBuf::from(args.next().expect("video filename"));
    let seconds = args.next().map(|v| v.parse::<u64>().unwrap()).unwrap_or(12);
    let mode = args.next().unwrap_or("auto".into());
    gtk::init().unwrap();
    kimbap::video::initialize().unwrap();
    let application = gtk::Application::builder()
        .application_id("dev.kimbap.VideoProbe")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    application.register(None::<&gio::Cancellable>).unwrap();
    let window = gtk::Window::builder()
        .application(&application)
        .title("Kimbap video experiment")
        .default_width(960)
        .default_height(640)
        .build();
    let header = gtk::HeaderBar::new();
    window.set_titlebar(Some(&header));
    let prefs = Preferences::load(Preferences::path()).unwrap_or_else(|e| {
        eprintln!("{e}");
        Preferences::defaults(Preferences::path())
    });
    let backend = VideoBackend::new(
        prefs,
        Rc::new(|event| match event {
            Event::Ready {
                generation,
                width,
                height,
            } => eprintln!("probe ready {generation} {width}x{height}"),
            Event::Failed { message, .. } => eprintln!("probe error: {message}"),
        }),
        true,
        mode,
        std::env::var("KIMBAP_TEST_AO").ok(),
    );
    header.pack_end(&backend.controls());
    window.set_child(Some(&backend.widget()));
    let main_loop = glib::MainLoop::new(None, false);
    let close = main_loop.clone();
    window.connect_close_request(move |_| {
        close.quit();
        glib::Propagation::Proceed
    });
    backend.load(1, file);
    window.present();
    let exit = main_loop.clone();
    glib::timeout_add_local_once(Duration::from_secs(seconds), move || exit.quit());
    main_loop.run();
    backend.stop(2);
    window.destroy();
    drop(backend);
}
