# kimbap

A personal replacement for [sushi](https://github.com/GNOME/sushi), the file previewer for nautilus. Mainly using different backend libs that happen to be faster on my specific machine. Not intended to be used by anyone else, this repo is just a public push mirror backup.

Build the application and its image/PDF workers with `cargo xtask release`
(add `--features video` for libmpv). The desktop application uses GTK4 and
`/usr/bin/bwrap` at runtime.

PDF previews use [Hayro](https://github.com/LaurenzV/hayro) 0.9.0, a pure Rust
renderer. The PDF worker is a fully static musl executable, with standard
substitute fonts and CMaps embedded. Its sandbox does not expose host fonts or
libraries, and its seccomp filter denies opening filesystem paths.

`cargo xtask pdf-worker` builds just this worker and copies it beside the
application in `target/release`. The build supports native Linux aarch64 and
x86_64 hosts; install the matching Rust musl target with `rustup target add`
(`aarch64-unknown-linux-musl` or `x86_64-unknown-linux-musl`) and a native linker
(`musl-gcc` on aarch64). The build task uses Cargo and `readelf` to reject any
worker with an ELF interpreter or shared-library dependencies. Keep
`target/release/pdf-licenses` with redistributed workers. Workers are found
beside the application by default; `--jpeg-worker` and `--pdf-worker` override
their locations.

JPEG and PNG share the image canvas, cache, and sandboxed `kimbap-jpeg-worker`
(the executable and option retain their original names). PNG decoding uses the
Rust `png` crate, preserves transparency, expands palettes and grayscale, and
reduces 16-bit samples to 8-bit RGBA. Embedded ICC profiles are converted to
sRGB; PNG gamma/chromaticity chunks alone are currently not color-managed.
APNG shows its default still image. Images are limited to 64 MiB compressed,
40 megapixels, and 20,000 pixels per edge.

PDF controls: Previous/Next or Page Up/Page Down change pages; +/− and Ctrl+0
zoom or fit. Arrow keys continue to navigate Nautilus files. Rendering follows
window size, display scale, and zoom, using 2× supersampling for text clarity
at fractional scales, up to 4096 pixels per page edge. Inputs are
limited to 64 MiB; password-protected files show an error.

Run `cargo test --workspace --all-features` for
regular host checks. The PDF worker is built separately for musl; exercise its
native sandbox integration after building it:

```sh
cargo xtask pdf-worker
KIMBAP_PDF_WORKER="$PWD/target/release/kimbap-pdf-worker" cargo test --test pdf -- --ignored
```

For an isolated preview session, run `RUST_LOG=kimbap=debug target/release/kimbap --bus-name dev.kimbap.Previewer`.
