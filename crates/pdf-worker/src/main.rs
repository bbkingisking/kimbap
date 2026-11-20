use hayro::{
    RenderCache, RenderSettings,
    hayro_interpret::{InterpreterSettings, util::TransformExt},
    hayro_syntax::{DecryptionError, LoadPdfError, Pdf},
    kurbo::Affine,
    vello_cpu::{
        PixmapMut, RasterizerSettings, RenderContext, Resources, TargetInit,
        color::palette::css::WHITE,
    },
};
use kimbap_worker::{
    self as ipc, invalid,
    pdf::{self, Request},
};
use std::{
    fs::File,
    io::{self, Write},
    os::{fd::FromRawFd, unix::net::UnixStream},
    time::Instant,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("kimbap PDF worker: {error}");
        std::process::exit(1);
    }
}
fn run() -> io::Result<()> {
    // All document parsing happens after confinement. Standard substitute fonts
    // and CMaps are embedded in Hayro; the worker never opens filesystem paths.
    ipc::sandbox::restrict()?;
    let mut socket = unsafe { UnixStream::from_raw_fd(3) };
    socket.write_all(pdf::MAGIC)?;
    let mut next = pdf::read_request_pdf(&mut socket)?;
    while let Some(request) = next {
        next = match request {
            Request::Open(bytes) => match Pdf::new(bytes) {
                Ok(document) if !document.pages().is_empty() => {
                    let pages = u32::try_from(document.pages().len())
                        .map_err(|_| invalid("PDF has too many pages"))?;
                    pdf::write_pages(&mut socket, pages)?;
                    serve_document(&mut socket, &document)?
                }
                result => {
                    let message = match result {
                        Ok(_) => "This PDF has no pages".into(),
                        Err(LoadPdfError::Decryption(DecryptionError::PasswordProtected)) => {
                            "This PDF requires a password; open it in your document viewer".into()
                        }
                        Err(error) => format!("Cannot open PDF: {error:?}"),
                    };
                    ipc::write_error(&mut socket, &message)?;
                    pdf::read_request_pdf(&mut socket)?
                }
            },
            Request::Render { .. } => {
                ipc::write_error(&mut socket, "No PDF is open")?;
                pdf::read_request_pdf(&mut socket)?
            }
        };
    }
    Ok(())
}

fn serve_document(socket: &mut UnixStream, document: &Pdf) -> io::Result<Option<Request>> {
    // Hayro's cache borrows the document. Scope both to this open-document loop
    // instead of creating a self-referential owner or throwing the cache away
    // between pages. A new Open drops all old glyph/image caches before parsing.
    let mut renderer = Renderer {
        document,
        cache: RenderCache::new(),
        interpreter: InterpreterSettings::default(),
        settings: RenderSettings::default(),
        context: RenderContext::new(0, 0),
        resources: Resources::default(),
    };
    loop {
        let (page, width, height) = match pdf::read_request_pdf(socket)? {
            Some(Request::Render {
                page,
                width,
                height,
            }) => (page, width, height),
            next => return Ok(next),
        };
        let start = Instant::now();
        match renderer.render(page, width, height) {
            Ok((file, w, h)) => {
                ipc::write_frame(socket, w, h, start.elapsed().as_micros() as u64, &file)?
            }
            Err(error) => ipc::write_error(socket, &error.to_string())?,
        }
    }
}

struct Renderer<'a> {
    document: &'a Pdf,
    cache: RenderCache<'a>,
    interpreter: InterpreterSettings,
    settings: RenderSettings,
    context: RenderContext,
    resources: Resources,
}
impl Renderer<'_> {
    fn render(
        &mut self,
        index: u32,
        max_width: u32,
        max_height: u32,
    ) -> io::Result<(File, u32, u32)> {
        let page = self
            .document
            .pages()
            .get(index as usize)
            .ok_or_else(|| invalid("PDF page is out of range"))?;
        let (w, h) = page.render_dimensions();
        let (w, h) = (f64::from(w), f64::from(h));
        if !w.is_finite() || !h.is_finite() || w <= 0. || h <= 0. {
            return Err(invalid("Invalid PDF page dimensions"));
        }
        let scale = (f64::from(max_width) / w).min(f64::from(max_height) / h);
        let width = (w * scale).round().clamp(1., f64::from(max_width)) as u32;
        let height = (h * scale).round().clamp(1., f64::from(max_height)) as u32;
        let (file, mut pixels) = ipc::create_pixels(width, height)?;
        self.context.reset_and_resize(width as u16, height as u16);
        // The initial transform handles page rotation and crop-box offsets.
        let transform = Affine::scale_non_uniform(f64::from(width) / w, f64::from(height) / h)
            * page.initial_transform(true).to_kurbo();
        hayro::render_into(
            page,
            &self.cache,
            &self.interpreter,
            &self.settings,
            &mut self.context,
            transform,
        );
        self.context.flush();
        let target = PixmapMut::new(width as u16, height as u16, &mut pixels)
            .ok_or_else(|| invalid("Invalid PDF pixel buffer"))?;
        // Rasterize directly into the shared RGBA mapping. Compositing on white
        // makes the premultiplied pixels opaque, as expected by the GTK canvas.
        self.context.render_with(
            target,
            &mut self.resources,
            RasterizerSettings {
                target_init: TargetInit::Clear(WHITE),
                ..Default::default()
            },
        );
        drop(pixels);
        ipc::seal_pixels(&file)?;
        Ok((file, width, height))
    }
}
