# kimbap

A personal replacement for [sushi](https://github.com/GNOME/sushi), the file previewer for nautilus. Mainly using different backend libs that happen to be faster on my specific machine. Not intended to be used by anyone else, this repo is just a public push mirror backup.

Build the application and its JPEG/PDF workers with `cargo build --release`
(add `--features video` for libmpv). Runtime dependencies include GTK4 and
`/usr/bin/bwrap`.

PDF previews use `pdfium-render` 0.9.5 with the PDFium 8125 API. Supply a native,
non-V8 `libpdfium.so` for your architecture alongside `target/release/kimbap`,
or pass `--pdfium-library /absolute/path/to/libpdfium.so`. Native builds are
available from [pdfium-binaries](https://github.com/bblanchon/pdfium-binaries/releases);
keep their bundled license notices with the library. Tested on aarch64 with
PDFium 159.0.8149.0. The workers are found beside the application by default;
`--jpeg-worker` and `--pdf-worker` override their locations.

PDF controls: Previous/Next or Page Up/Page Down change pages; +/− and Ctrl+0
zoom or fit. Arrow keys continue to navigate Nautilus files. Rendering follows
window size, display scale, and zoom, using 2× supersampling for text clarity
at fractional scales, up to 4096 pixels per page edge. Inputs are
limited to 64 MiB; password-protected files show an error.

Run `cargo test --workspace --all-features` for the regular checks. The native
PDF sandbox test additionally needs a library:

```sh
PDFIUM_LIBRARY=/absolute/path/to/libpdfium.so cargo test -p kimbap-pdf-worker --test sandbox -- --ignored
```

For an isolated preview session, run `target/release/kimbap --bus-name dev.kimbap.Previewer --verbose`.
