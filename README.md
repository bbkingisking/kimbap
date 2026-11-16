# Kimbap

A Rust preview service for Nautilus. The first milestone is transport only:
`ShowFile` and `Close` acknowledge requests without opening files, creating
windows, or launching backends. Nothing delegates to Sushi yet.

The D-Bus boundary is independent of formats. `PreviewEndpoint` in `src/lib.rs`
converts calls into typed `Command`s and provides window-state updates and typed
events for future UI code. The executable currently logs commands only when
`--verbose` is supplied; activation tokens are never logged. Window state stays
`Visible=false`, `ParentHandle=""` until a real UI is connected. Consequently,
selection changes do not yet continuously preview files in Nautilus.

## Build and test

Requires Rust, a C toolchain, pkg-config, and GLib/GIO development libraries
(`glib2-devel` on Fedora). Tests also require `dbus-run-session` and `kill`.
The executable links system GLib/GIO dynamically; GTK is not a dependency yet.

```sh
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The integration test creates a private session bus, so `cargo test` never
replaces the desktop previewer. It exercises proxy Ping/introspection, method
arguments (including Unicode and opaque activation/parent data), 200 rapid
requests, readable/read-only properties, property notifications, all four
navigation/action signals, invalid calls, name collision, and SIGTERM shutdown.

To run alongside the existing desktop previewer without intercepting Nautilus:

```sh
cargo run -- --bus-name dev.kimbap.Previewer --verbose
```

In another terminal:

```sh
gdbus call --session --dest dev.kimbap.Previewer \
  --object-path /org/gnome/NautilusPreviewer \
  --method org.gnome.NautilusPreviewer2.ShowFile \
  file:///tmp/example.jpeg '' false ''
```

## Nautilus interface

Default bus name: `org.gnome.NautilusPreviewer`.
Object path: `/org/gnome/NautilusPreviewer`.
Interface: `org.gnome.NautilusPreviewer2` (Nautilus 51).
The wire definition is in `data/org.gnome.NautilusPreviewer2.xml` and follows
[Nautilus's preview client](https://gitlab.gnome.org/GNOME/nautilus/-/blob/51.1/src/nautilus-previewer.c).

Nautilus sends URI, exported parent handle, close-if-already-shown flag, and
activation token through `ShowFile(ssbs)`. The eventual window reports actual
visibility/parent state and sends selection directions or NavigateTo/Rename/Trash
events back to Nautilus. Kimbap does not infer directory order.

Running without `--bus-name` requires that the public name be free. Kimbap
fails immediately if another previewer owns it: it neither replaces that owner
nor queues for later ownership. It runs until SIGINT/SIGTERM or a lost bus
connection. Idle shutdown and activation/deployment policy are not implemented.
No desktop service or D-Bus activation files are installed by this project.

On 2026-11-16, the release binary was temporarily given the public name and
verified with real Nautilus Space requests for two different selected JPEGs.
Requests arrived with parent handles and activation tokens, no preview window
appeared, and the properties remained false/empty. The normal Sushi service was
restored afterward; its code, environment and activation files were unchanged.
