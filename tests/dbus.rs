use std::{
    cell::RefCell,
    collections::HashMap,
    fs::{self, File},
    path::PathBuf,
    process::{Child, Command as Process, Stdio},
    rc::Rc,
    time::{Duration, Instant},
};

use glib::variant::ToVariant;
use kimbap::{
    BUS_NAME, Command, Direction, Event, INTERFACE, OBJECT_PATH, PreviewEndpoint, WindowState,
};

// Always isolate tests from the desktop's real previewer and activation files.
#[test]
fn nautilus_protocol() {
    if std::env::var_os("KIMBAP_PRIVATE_TEST_BUS").is_none() {
        let output = Process::new("dbus-run-session")
            .arg("--")
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", "nautilus_protocol", "--nocapture"])
            .env("KIMBAP_PRIVATE_TEST_BUS", "1")
            .output()
            .expect("dbus-run-session must be installed");
        assert!(
            output.status.success(),
            "private-bus tests failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let context = glib::MainContext::new();
    context
        .with_thread_default(|| context.block_on(exercise_protocol()))
        .unwrap();
}

async fn call(
    connection: &gio::DBusConnection,
    destination: &str,
    interface: &str,
    method: &str,
    args: Option<&glib::Variant>,
) -> Result<glib::Variant, glib::Error> {
    connection
        .call_future(
            Some(destination),
            OBJECT_PATH,
            interface,
            method,
            args,
            None,
            gio::DBusCallFlags::NO_AUTO_START,
            2_000,
        )
        .await
}

async fn properties(
    connection: &gio::DBusConnection,
    destination: &str,
) -> HashMap<String, glib::Variant> {
    call(
        connection,
        destination,
        "org.freedesktop.DBus.Properties",
        "GetAll",
        Some(&(INTERFACE,).to_variant()),
    )
    .await
    .unwrap()
    .get::<(HashMap<String, glib::Variant>,)>()
    .unwrap()
    .0
}

async fn exercise_protocol() {
    let connection = gio::bus_get_future(gio::BusType::Session).await.unwrap();
    let child = TestProcess::start();
    let mut ready = false;
    for _ in 0..100 {
        if call(
            &connection,
            BUS_NAME,
            "org.freedesktop.DBus.Peer",
            "Ping",
            None,
        )
        .await
        .is_ok()
        {
            ready = true;
            break;
        }
        glib::timeout_future(Duration::from_millis(10)).await;
    }
    assert!(ready, "Kimbap did not acquire its name");

    // Nautilus builds a proxy and pings before sending its first request.
    let xml = call(
        &connection,
        BUS_NAME,
        "org.freedesktop.DBus.Introspectable",
        "Introspect",
        None,
    )
    .await
    .unwrap()
    .get::<(String,)>()
    .unwrap()
    .0;
    for member in [
        "ShowFile",
        "Close",
        "Visible",
        "ParentHandle",
        "SelectionEvent",
        "NavigateTo",
        "Rename",
        "Trash",
    ] {
        assert!(
            xml.contains(&format!("name=\"{member}\"")),
            "missing {member}"
        );
    }
    assert_eq!(
        properties(&connection, BUS_NAME).await["Visible"].get::<bool>(),
        Some(false)
    );
    assert_eq!(
        properties(&connection, BUS_NAME).await["ParentHandle"].str(),
        Some("")
    );

    // No file access or renderer exists: nonexistent/remote/folder URIs are accepted.
    for (uri, toggle) in [
        ("file:///does-not-exist/한글 image.jpeg", true),
        ("sftp://example.invalid/a%20file.mp4", false),
        ("file:///tmp/", true),
    ] {
        call(
            &connection,
            BUS_NAME,
            INTERFACE,
            "ShowFile",
            Some(&(uri, "wayland:opaque-parent", toggle, "never-log-this-token").to_variant()),
        )
        .await
        .unwrap();
    }
    for _ in 0..200 {
        call(
            &connection,
            BUS_NAME,
            INTERFACE,
            "ShowFile",
            Some(&("file:///no-op.jpeg", "x11:1", false, "").to_variant()),
        )
        .await
        .unwrap();
    }
    for _ in 0..2 {
        call(&connection, BUS_NAME, INTERFACE, "Close", None)
            .await
            .unwrap();
    }
    assert_eq!(
        properties(&connection, BUS_NAME).await["Visible"].get::<bool>(),
        Some(false)
    );
    assert_eq!(
        properties(&connection, BUS_NAME).await["ParentHandle"].str(),
        Some("")
    );

    let malformed = call(
        &connection,
        BUS_NAME,
        INTERFACE,
        "ShowFile",
        Some(&("wrong signature",).to_variant()),
    )
    .await
    .unwrap_err();
    assert!(malformed.matches(gio::DBusError::InvalidArgs));
    assert!(
        call(&connection, BUS_NAME, INTERFACE, "UnknownMethod", None)
            .await
            .is_err()
    );
    assert!(
        call(
            &connection,
            BUS_NAME,
            "org.freedesktop.DBus.Properties",
            "Set",
            Some(&(INTERFACE, "Visible", true.to_variant()).to_variant())
        )
        .await
        .is_err()
    );

    // A second executable must fail, without replacing or queuing behind the owner.
    let duplicate = Process::new(env!("CARGO_BIN_EXE_kimbap")).output().unwrap();
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("already owned"));
    call(
        &connection,
        BUS_NAME,
        "org.freedesktop.DBus.Peer",
        "Ping",
        None,
    )
    .await
    .unwrap();

    exercise_rust_boundary(&connection).await;
    let log = child.stop();
    assert!(log.contains("ShowFile uri=\"file:///does-not-exist/한글 image.jpeg\""));
    assert!(log.contains("parent=true close_if_already_shown=true activation=true"));
    assert!(log.contains("Close"));
    assert!(!log.contains("never-log-this-token"));
    assert!(
        call(
            &connection,
            BUS_NAME,
            "org.freedesktop.DBus.Peer",
            "Ping",
            None
        )
        .await
        .is_err()
    );

    // Reject invalid command-line options before attempting service registration.
    let invalid = Process::new(env!("CARGO_BIN_EXE_kimbap"))
        .args(["--bus-name", "not a bus name"])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    let unknown = Process::new(env!("CARGO_BIN_EXE_kimbap"))
        .arg("--unknown")
        .output()
        .unwrap();
    assert!(!unknown.status.success());
}

async fn exercise_rust_boundary(client: &gio::DBusConnection) {
    let exporter = gio::DBusConnection::for_address_future(
        &std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap(),
        gio::DBusConnectionFlags::AUTHENTICATION_CLIENT
            | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
        None,
    )
    .await
    .unwrap();
    let name = exporter.unique_name().unwrap();
    let commands = Rc::new(RefCell::new(Vec::new()));
    let endpoint = PreviewEndpoint::export(&exporter, {
        let commands = commands.clone();
        move |command| commands.borrow_mut().push(command)
    })
    .unwrap();
    let parameters = (
        "file:///tmp/漢字%20image.jpeg",
        "wayland:original",
        true,
        "original-token",
    );
    call(
        client,
        &name,
        INTERFACE,
        "ShowFile",
        Some(&parameters.to_variant()),
    )
    .await
    .unwrap();
    call(client, &name, INTERFACE, "Close", None).await.unwrap();
    {
        let received = commands.borrow();
        assert_eq!(received.len(), 2);
        let Command::ShowFile(request) = &received[0] else {
            panic!("expected ShowFile")
        };
        assert_eq!(request.uri, parameters.0);
        assert_eq!(request.window_handle, parameters.1);
        assert!(request.close_if_already_shown);
        assert_eq!(request.activation_token, parameters.3);
        assert!(received[1] == Command::Close);
    }

    let signals = Rc::new(RefCell::new(Vec::<(String, glib::Variant)>::new()));
    let _subscription = client.subscribe_to_signal(
        Some(&name),
        None,
        None,
        Some(OBJECT_PATH),
        None,
        gio::DBusSignalFlags::NONE,
        {
            let signals = signals.clone();
            move |signal| {
                signals
                    .borrow_mut()
                    .push((signal.signal_name.to_owned(), signal.parameters.clone()))
            }
        },
    );
    // Round trip ensures the bus has installed the signal subscription.
    call(client, &name, "org.freedesktop.DBus.Peer", "Ping", None)
        .await
        .unwrap();
    let state = WindowState {
        visible: true,
        parent_handle: "wayland:actual-window".into(),
    };
    endpoint.set_window_state(state.clone()).unwrap();
    endpoint.set_window_state(state).unwrap(); // No duplicate property notification.
    endpoint.emit(&Event::Selection(Direction::Down)).unwrap();
    endpoint
        .emit(&Event::NavigateTo("file:///tmp/".into()))
        .unwrap();
    endpoint
        .emit(&Event::Rename("file:///tmp/test".into()))
        .unwrap();
    endpoint
        .emit(&Event::Trash("file:///tmp/test".into()))
        .unwrap();
    for _ in 0..100 {
        if signals.borrow().len() >= 5 {
            break;
        }
        glib::timeout_future(Duration::from_millis(10)).await;
    }
    {
        let signals = signals.borrow();
        assert_eq!(signals.len(), 5);
        assert_eq!(signals[0].0, "PropertiesChanged");
        let changed = signals[0]
            .1
            .get::<(String, HashMap<String, glib::Variant>, Vec<String>)>()
            .unwrap();
        assert_eq!(changed.0, INTERFACE);
        assert_eq!(changed.1["Visible"].get::<bool>(), Some(true));
        assert_eq!(
            changed.1["ParentHandle"].str(),
            Some("wayland:actual-window")
        );
        assert!(changed.2.is_empty());
        for (index, name, parameters) in [
            (1, "SelectionEvent", (3u32,).to_variant()),
            (2, "NavigateTo", ("file:///tmp/",).to_variant()),
            (3, "Rename", ("file:///tmp/test",).to_variant()),
            (4, "Trash", ("file:///tmp/test",).to_variant()),
        ] {
            assert_eq!(signals[index], (name.into(), parameters));
        }
    }
    assert_eq!(
        properties(client, &name).await["Visible"].get::<bool>(),
        Some(true)
    );
    assert_eq!(
        properties(client, &name).await["ParentHandle"].str(),
        Some("wayland:actual-window")
    );
    endpoint.set_window_state(WindowState::default()).unwrap();
    assert_eq!(
        properties(client, &name).await["Visible"].get::<bool>(),
        Some(false)
    );
    drop(endpoint);
    assert!(call(client, &name, INTERFACE, "Close", None).await.is_err());
    exporter.close_future().await.unwrap();
}

struct TestProcess {
    child: Child,
    log: PathBuf,
}

impl TestProcess {
    fn start() -> Self {
        let log = std::env::temp_dir().join(format!("kimbap-test-{}.log", std::process::id()));
        let child = Process::new(env!("CARGO_BIN_EXE_kimbap"))
            .arg("--transport-only")
            .env("RUST_LOG", "kimbap=debug")
            .stdout(Stdio::null())
            .stderr(File::create(&log).unwrap())
            .spawn()
            .unwrap();
        Self { child, log }
    }

    fn stop(mut self) -> String {
        Process::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "Kimbap did not shut down cleanly");
                return fs::read_to_string(&self.log).unwrap();
            }
            assert!(Instant::now() < deadline, "shutdown timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for TestProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.log);
    }
}
