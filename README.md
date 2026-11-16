# Kimbap

A Rust preview service for Nautilus, with a GTK4 JPEG viewer and a persistent,
sandboxed libjpeg-turbo worker. The transport, shared window, JPEG canvas, and
decoder scheduling are separate modules so more formats can be added later.
No Sushi delegation or video backend is wired up yet.

JPEG failures are shown in the window and logged to stderr. There is **no JPEG
fallback**. Supported inputs are local, ordinary 8-bit RGB/YCbCr and grayscale
JPEGs, including progressive coding, EXIF orientations and supported embedded
ICC profiles. Profiles are converted to sRGB with Little CMS; unprofiled images
are assumed to be sRGB. CMYK/YCCK, unsupported precision, malformed metadata,
remote URIs and non-JPEG inputs produce explicit errors.

## Build and run

Requires Rust 1.92+, CMake, a C toolchain, pkg-config, GTK4 4.20+ development
libraries and bubblewrap. Fedora packages include `gtk4-devel`, `glib2-devel`,
`cmake` and `bubblewrap`. The worker bundles libjpeg-turbo and Little CMS;
`require-simd` prevents accidentally building a scalar JPEG decoder. The current
lockfile's turbojpeg-sys 1.2.0 bundles libjpeg-turbo 3.1.0; this is distinct from
the system 3.1.4.1 used in the earlier decoder study. GTK/GLib remain dynamically
linked. The worker can be fully static with musl.

```sh
cargo build --release --workspace
# On this ARM64 machine, with the musl target and musl-gcc already installed:
cargo build --release -p kimbap-jpeg-worker --target aarch64-unknown-linux-musl
GSK_RENDERER=vulkan target/release/kimbap --bus-name dev.kimbap.Previewer \
  --jpeg-worker target/aarch64-unknown-linux-musl/release/kimbap-jpeg-worker
```

In another terminal:

```sh
gdbus call --session --dest dev.kimbap.Previewer \
  --object-path /org/gnome/NautilusPreviewer \
  --method org.gnome.NautilusPreviewer2.ShowFile \
  file:///absolute/path/to/image.jpeg '' false ''
```

Without `--jpeg-worker`, the helper is expected beside the frontend executable.
The regular workspace build supplies a GNU helper there; pass the musl path above
or manually place the static helper beside the frontend when deploying. The
sandbox supports both builds. `--verbose` adds timings and accepted requests;
activation tokens are omitted. `--transport-only` exports the original no-op
interface without requiring a display, for protocol development/tests.

This repository installs no desktop service or activation files. Nautilus's
normal desktop activation remains unchanged. To test real Nautilus, the public
name must temporarily belong to Kimbap; don't run two public preview owners.

## Controls

| Input | Action |
| --- | --- |
| Trackpad pinch | Zoom around the gesture/pointer |
| Two-finger scroll or left-button drag | Pan; offsets clamp to image edges |
| Fit button / Ctrl+0 | Fit the image |
| Ctrl+1 | 100% physical pixels, including fractional display scaling |
| + / − / Ctrl+scroll | Zoom |
| Double click / F11 | Toggle fullscreen |
| Arrow keys | Ask Nautilus to select the next item in that direction |
| Space / Escape / q | Close preview |
| Ctrl+C | Copy the full-resolution decoded image |
| F2 / Delete | Forward rename / trash to Nautilus |

The same GTK window is reused for navigation. A new image resets fit/pan;
zooming uses the retained full-resolution texture and never requests another
decode. Trilinear GPU filtering handles downscaling. The default palette is
Kanagawa Dragon, with Pretendard preferred for interface text.

## Decoder lifecycle

`src/jpeg.rs` keeps blocking file/socket work off the GTK thread. Its queue has
one replaceable pending request, and stale generations cannot update the viewer.
An in-progress decode finishes before the latest pending request; navigation
does not repeatedly kill/restart a healthy worker. Transport errors kill the
worker and show an error; the next request starts a new one.

The worker starts on the first JPEG cache miss. It survives image changes and
is released, with the cache, after 30 seconds without an open preview. The small
frontend remains alive until SIGINT/SIGTERM or a lost bus connection/name.

Linux CPU-capacity metadata selects the highest-capacity cores from the existing
allowed CPU set. On this M1 that is cores 4–7. Homogeneous/missing capacity data
leaves placement unchanged; taskset/cgroup restrictions are respected. The GTK
process and system power policy are not pinned or changed.

Bubblewrap supplies isolated namespaces and hides the user's home and session
bus. A seccomp allowlist is installed before processing images; decoding cannot
open files, create sockets, execute processes or clone threads. The worker receives
compressed bytes over one inherited Unix socket and returns sealed memfds.
The frontend validates dimensions, descriptor count, size and write/grow/shrink
seals before mapping pixels read-only into a Gdk.MemoryTexture. Old textures
cannot be overwritten by a later decode. Read-only `/usr` is mounted for GNU
helper development; the musl helper itself needs no system decoder libraries.

Limits: 64 MiB compressed input, 40 MP / 20,000 pixels per side, 4 MiB ICC,
100 progressive scans, a 1.5 GiB worker address-space limit, and four-second socket
I/O timeouts. The decoded LRU cache is bounded to 64 MiB, keyed by device/inode,
size and nanosecond mtime. The current frame and graphics resources are additional
to that budget; no attempt is made to guess Nautilus's sort/filter order for
prefetch. Native orientation, metadata parsing and colour conversion stay inside
the sandbox.

## Protocol and verification

Default bus name: `org.gnome.NautilusPreviewer`.
Object path: `/org/gnome/NautilusPreviewer`.
Interface: `org.gnome.NautilusPreviewer2` (Nautilus 51).
The XML in `data/` follows
[Nautilus's preview client](https://gitlab.gnome.org/GNOME/nautilus/-/blob/51.1/src/nautilus-previewer.c).

`src/lib.rs` preserves URI, parent handle, close-if-already-shown and activation
token, reports actual visibility/parent state, and exposes typed navigation/action
events. Wayland exported parents and startup tokens are applied to the window.
An occupied public name is never replaced or queued for later ownership.
X11 parent-window integration is not implemented yet.

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo build --release -p kimbap --examples
# Pipeline timings, excluding GTK/presentation:
target/release/examples/jpeg-bench \
  target/aarch64-unknown-linux-musl/release/kimbap-jpeg-worker ~/sushi-test/*.jpeg
```

Tests cover the isolated-bus protocol, sealed memory transport, viewport geometry,
all eight EXIF mappings, real sandboxed decode, ICC conversion/errors, cache
invalidation, latest-request scheduling, worker crash/restart and shutdown.
Tests use a private D-Bus session and generated images; no media is tracked.

On 2026-11-16, all 38 current ~/sushi-test JPEGs decoded through one static worker
for three passes. CSVs in `benchmarks/` record worker/pipeline and actual Nautilus
switching measurements. These are working-desktop observations, not isolated lab
comparisons or panel-presentation timings. The CSV `worker_pid` identifies the
bubblewrap supervisor/worker instance, not a PID inside the sandbox namespace.

A real Nautilus test sent 149 requests with one GTK window throughout and no JPEG
errors. At 120 ms between keys, 72/74 requests reached a GTK paint callback;
selection-event-to-paint median/p95 was 68.18/119.23 ms. At 40 ms, 41/74 reached
paint, with the final image correct. Fast-run timings describe the surviving
requests and must not be treated as throughput for every requested image. The
window's Wayland parent, same-file toggle, close/reopen, explicit error screen,
error recovery and keyboard zoom/reset were exercised. The user confirmed
physical trackpad pinch and pan. Normal Sushi activation was restored afterward;
no desktop deployment change is part of this milestone.
