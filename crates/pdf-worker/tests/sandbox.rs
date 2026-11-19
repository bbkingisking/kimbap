use kimbap::pdf::Decoder;
use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

struct Fixtures(PathBuf);
impl Fixtures {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("kimbap-pdf-test-{}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }
}
impl Drop for Fixtures {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn pdf() -> Vec<u8> {
    // Two differently sized pages exercise navigation, fitting and RGBA order.
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 300] /Contents 4 0 R >>".into(),
        stream("1 0 0 rg 0 0 200 300 re f"),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] /Contents 6 0 R >>".into(),
        stream("0 1 0 rg 0 0 300 200 re f"),
    ];
    let mut bytes = b"%PDF-1.7\n".to_vec();
    let mut offsets = vec![0];
    for (i, object) in objects.iter().enumerate() {
        offsets.push(bytes.len());
        bytes.extend_from_slice(format!("{} 0 obj\n{}\nendobj\n", i + 1, object).as_bytes());
    }
    let xref = bytes.len();
    bytes.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len()).as_bytes());
    for offset in offsets.iter().skip(1) {
        bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    bytes.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            offsets.len()
        )
        .as_bytes(),
    );
    bytes
}
fn stream(content: &str) -> String {
    format!(
        "<< /Length {} >>\nstream\n{content}\nendstream",
        content.len()
    )
}

#[test]
#[ignore = "requires PDFIUM_LIBRARY pointing to a compatible native PDFium build and working bubblewrap"]
fn persistent_pdf_worker_renders_navigates_recovers_and_cancels() {
    let fixtures = Fixtures::new();
    let original = fixtures.write("pages.pdf", &pdf());
    let library = PathBuf::from(std::env::var_os("PDFIUM_LIBRARY").expect("set PDFIUM_LIBRARY"));
    let (decoder, replies) = Decoder::new(
        PathBuf::from(env!("CARGO_BIN_EXE_kimbap-pdf-worker")),
        library,
    );
    decoder.render(1, original.clone(), 0, 120, 180);
    let first = replies.recv_blocking().unwrap().result.unwrap();
    assert_eq!(first.pages, 2);
    assert_eq!((first.frame.width, first.frame.height), (120, 180));
    assert_eq!(&first.frame.pixels[..4], &[255, 0, 0, 255]);
    assert!(!first.cache_hit);
    let pid = first.worker_pid.unwrap();
    let mut pending = vec![pid];
    let mut confined = false;
    while let Some(process) = pending.pop() {
        let children = fs::read_to_string(format!("/proc/{process}/task/{process}/children"))
            .unwrap_or_default();
        pending.extend(
            children
                .split_whitespace()
                .map(|p| p.parse::<u32>().unwrap()),
        );
        let status = fs::read_to_string(format!("/proc/{process}/status")).unwrap();
        if status.contains("Seccomp:\t2") {
            assert!(status.contains("NoNewPrivs:\t1"));
            confined = true;
        }
    }
    assert!(confined, "PDF worker has no seccomp filter");
    decoder.render(1, original.clone(), 1, 120, 180);
    let second = replies.recv_blocking().unwrap().result.unwrap();
    assert_eq!((second.frame.width, second.frame.height), (120, 80));
    assert_eq!(&second.frame.pixels[..4], &[0, 255, 0, 255]);
    assert_eq!(second.worker_pid, Some(pid));
    decoder.render(1, original.clone(), 0, 120, 180);
    let cached = replies.recv_blocking().unwrap().result.unwrap();
    assert!(cached.cache_hit);
    assert!(std::sync::Arc::ptr_eq(&first.frame, &cached.frame));
    decoder.render(1, original.clone(), 0, 240, 360);
    let zoomed = replies.recv_blocking().unwrap().result.unwrap();
    assert_eq!((zoomed.frame.width, zoomed.frame.height), (240, 360));
    assert!(!zoomed.cache_hit);
    decoder.render(1, original.clone(), 2, 120, 180);
    assert!(
        replies
            .recv_blocking()
            .unwrap()
            .result
            .unwrap_err()
            .contains("out of range")
    );
    let invalid = fixtures.write("invalid.pdf", b"not a pdf");
    decoder.render(2, invalid, 0, 120, 180);
    assert!(replies.recv_blocking().unwrap().result.is_err());
    let locked = fixtures.write("locked.pdf", include_bytes!("fixtures/password.pdf"));
    decoder.render(3, locked, 0, 120, 180);
    assert!(
        replies
            .recv_blocking()
            .unwrap()
            .result
            .unwrap_err()
            .contains("password")
    );
    // After two failed opens, cache misses must reopen the intended document.
    decoder.render(4, original.clone(), 1, 130, 180);
    assert_eq!(
        replies.recv_blocking().unwrap().result.unwrap().worker_pid,
        Some(pid)
    );
    let mut latest = 0;
    for i in 0..100 {
        latest = decoder.render(5, original.clone(), i % 2, 140, 200);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let reply = replies.recv_blocking().unwrap();
        if reply.revision == latest {
            assert_eq!(reply.page, 1);
            assert!(reply.result.is_ok());
            break;
        }
        assert!(Instant::now() < deadline);
    }
    // A changed file must not reuse the old document or rendered-page cache.
    fs::write(&original, b"broken replacement").unwrap();
    decoder.render(6, original.clone(), 0, 120, 180);
    assert!(replies.recv_blocking().unwrap().result.is_err());
    fs::write(&original, pdf()).unwrap();
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
    std::thread::sleep(Duration::from_millis(30));
    decoder.render(7, original.clone(), 0, 150, 210);
    assert!(replies.recv_blocking().unwrap().result.is_err());
    decoder.render(8, original, 0, 150, 210);
    let replacement = replies
        .recv_blocking()
        .unwrap()
        .result
        .unwrap()
        .worker_pid
        .unwrap();
    assert_ne!(replacement, pid);
    decoder.cancel();
    replies.close();
    drop(decoder);
    for pid in [pid, replacement] {
        assert!(
            !PathBuf::from(format!("/proc/{pid}")).exists(),
            "worker survived shutdown"
        );
    }
}
