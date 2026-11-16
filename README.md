# Kimbap

A Rust preview service for Nautilus, with a GTK4 JPEG viewer and a persistent,
sandboxed libjpeg-turbo worker. The transport, shared window, JPEG canvas, and
decoder scheduling are separate modules so more formats can be added later.
An optional experimental libmpv backend shares the window and Nautilus dispatch.
The installed desktop build enables JPEG and video previews; no Sushi delegation is used.

JPEG failures are shown in the window and logged to stderr. There is **no JPEG
fallback**. Supported inputs are local, ordinary 8-bit RGB/YCbCr and grayscale
JPEGs, including progressive coding, EXIF orientations and supported embedded
ICC profiles. Profiles are converted to sRGB with Little CMS; unprofiled images
are assumed to be sRGB. CMYK/YCCK, unsupported precision, malformed metadata,
remote URIs and inputs without an enabled backend produce explicit errors.

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

Desktop deployment is owned by `~/src/sway-desktop`. On this machine the frontend
and static worker are installed in `~/.local/opt/kimbap/bin/`, with a
`~/.local/bin/kimbap` symlink. `kimbap.service` owns the public preview name,
starts with the graphical session and is D-Bus activated. It sets
`GSK_RENDERER=vulkan` and passes `--experimental-video` to a frontend built with
`--features video`. The former Sushi unit is retired; unsupported formats
show Kimbap's explicit error. No Sushi fallback is active.

This application repository contains no installer or desktop activation sources.
After manually replacing both installed binaries, restart `kimbap.service`.
Don't run a second public preview owner; use `--bus-name` for development.

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
physical trackpad pinch and pan. Those implementation tests temporarily restored
Sushi. Subsequent desktop deployment switched the real activation path to Kimbap;
Space, Down/Up, floating/parent state, close/reopen and cold D-Bus activation were
verified against the installed binaries.

## Experimental video backend

Build with `cargo build --release -p kimbap --features video`; this also requires
`mpv-devel` (libmpv 0.41 was tested) and EGL development libraries. Run the result
with `--experimental-video --bus-name dev.kimbap.VideoTest` and the usual
`--jpeg-worker` path. This opt-in does not modify the installed desktop service.
To exercise the renderer without D-Bus:

```sh
cargo run --release --features video --example video-probe -- /path/to/video.mp4 12 auto
```

The preview shell selects a backend using GIO content types. Each backend owns
its widget, controls and load/stop lifecycle; readiness and failure events carry
the request generation. JPEG and video use the same window, activation and
Nautilus navigation. Unsupported files and playback failures are explicit errors.
The libmpv core and its blocking client APIs run on a dedicated control thread;
the GTK GLArea owns the OpenGL render API/context. There is one replaceable
pending video request. Switching away or closing queues a stop, including audio;
the initialized engine remains available for the next preview. EOF retains the
last frame. Video decoding currently runs inside the frontend process, separate
from the JPEG worker's sandbox.

The only video control is Mute/Unmute (`m` also toggles it). It defaults to muted
and persists in `$XDG_CONFIG_HOME/kimbap/preferences.ini` (normally
`~/.config/kimbap/preferences.ini`), group `[video]`, boolean `muted`. Write errors
are visible on the button and logged. mpv's personal config, scripts and default
key bindings are disabled. Hardware decoding uses `hwdec=auto`; actual selection
is available in `--verbose` logs, rather than inferred from the requested setting.

On this M1/Asahi machine, short visible tests with the user's samples selected
VA-API for H.264 (720p and 1080p portrait), HEVC (1080p portrait), and VP9
(3840×2026 at 50 fps). AV1 (1080p30) and a generated VP8 (640×360 at 24 fps) used
software decoding. `benchmarks/video-probe-2026-11-16.csv` records representative
isolated probe observations: H.264/HEVC/VP8 had zero reported drops; VP9/AV1 each
reported one startup drop and then kept pace. Frame-update counts are renderer
notifications, not independently measured panel presentations. These are short
working-desktop tests, not sustained thermal/power benchmarks. The VP8 sample
is synthetic. Hidden-workspace probes were discarded because Sway withheld their
rendering. GTK's integer buffer scale also differs from native fractional-scale
players, so these runs do not establish comparative rendering efficiency.

A separate D-Bus name exercised all supplied clips, JPEG/video switches, a burst
of 30 requests spaced about 80 ms apart, close/reopen, corrupt-video errors and
recovery, and clean shutdown. Rapid replacement exposed stale dimensions at
mpv's FILE_LOADED event; readiness now waits for PLAYBACK_RESTART. The mute key
was checked against mpv's actual state and persisted across process recreation,
using a temporary XDG config directory. The installed JPEG service was unchanged.

Standalone mpv 0.41 with clean configuration, OpenGL/Wayland and `hwdec=auto`
also selected VA-API for VP9 and HEVC, with zero drops after roughly 6.7 seconds
of playback each. The embedded probe's single VP9 startup drop did not continue
during playback. This baseline does not compare power use or prove that another
engine would be faster; no competing backend has been implemented.

The live desktop subsequently enabled this backend in both systemd and direct
D-Bus activation. Deployment exposed GTK's `en_US.UTF-8` numeric locale, which
libmpv rejects; startup now resets only LC_NUMERIC to C after GTK initialization.
The terminal experiments had inherited C.UTF-8 and missed this requirement.
Live checks verified moving frames for all five supplied videos, JPEG recovery,
Space/arrow navigation, parented floating windows and close/reopen. Other locale
categories retain the desktop settings.
