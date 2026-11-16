use kimbap::jpeg::Decoder;
use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};
use turbojpeg::{Image, PixelFormat, Subsamp};

struct FixtureDir(PathBuf);
impl FixtureDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("kimbap-worker-test-{}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn jpeg() -> Vec<u8> {
    let pixels = vec![
        200, 20, 10, 10, 200, 20, 20, 10, 200, 220, 200, 10, 200, 10, 220, 10, 220, 200,
    ];
    turbojpeg::compress(
        Image {
            pixels: &pixels[..],
            width: 3,
            pitch: 9,
            height: 2,
            format: PixelFormat::RGB,
        },
        100,
        Subsamp::None,
    )
    .unwrap()
    .to_vec()
}
fn segment(jpeg: &[u8], marker: u8, data: &[u8]) -> Vec<u8> {
    let mut out = vec![255, 216, 255, marker];
    out.extend_from_slice(&((data.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(data);
    out.extend_from_slice(&jpeg[2..]);
    out
}
fn oriented(jpeg: &[u8], orientation: u8) -> Vec<u8> {
    let mut exif = b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".to_vec();
    exif.extend_from_slice(&[orientation, 0, 0, 0, 0, 0, 0, 0]);
    segment(jpeg, 225, &exif)
}

#[test]
fn persistent_sandbox_decodes_recovers_and_prefers_latest() {
    let files = FixtureDir::new();
    let plain = jpeg();
    let original = files.0.join("source.jpeg");
    fs::write(&original, &plain).unwrap();
    let (decoder, replies) = Decoder::new(PathBuf::from(env!("CARGO_BIN_EXE_kimbap-jpeg-worker")));
    decoder.load(1, original.clone());
    let first = replies.recv_blocking().unwrap().result.unwrap();
    assert_eq!((first.frame.width, first.frame.height), (3, 2));
    assert!(!first.cache_hit);
    let pid = first.worker_pid.unwrap();
    let mut pending = vec![pid];
    let mut confined = Vec::new();
    while let Some(process) = pending.pop() {
        let children = fs::read_to_string(format!("/proc/{process}/task/{process}/children"))
            .unwrap_or_default();
        pending.extend(
            children
                .split_whitespace()
                .map(|n| n.parse::<u32>().unwrap()),
        );
        let status = fs::read_to_string(format!("/proc/{process}/status")).unwrap();
        if status.contains("Seccomp:\t2") {
            assert!(status.contains("NoNewPrivs:\t1"));
            confined.push(process);
        }
    }
    assert!(!confined.is_empty(), "decoder has no seccomp filter");
    decoder.load(2, original.clone());
    let cached = replies.recv_blocking().unwrap().result.unwrap();
    assert!(cached.cache_hit);
    assert_eq!(cached.worker_pid, Some(pid));
    assert!(std::sync::Arc::ptr_eq(&first.frame, &cached.frame));
    let expected_order = [
        [0, 1, 2, 3, 4, 5],
        [2, 1, 0, 5, 4, 3],
        [5, 4, 3, 2, 1, 0],
        [3, 4, 5, 0, 1, 2],
        [0, 3, 1, 4, 2, 5],
        [3, 0, 4, 1, 5, 2],
        [5, 2, 4, 1, 3, 0],
        [2, 5, 1, 4, 0, 3],
    ];
    for orientation in 1..=8 {
        let path = files.0.join(format!("orientation-{orientation}.jpeg"));
        fs::write(&path, oriented(&plain, orientation)).unwrap();
        decoder.load(orientation as u64 + 2, path);
        let image = replies.recv_blocking().unwrap().result.unwrap();
        let shape = if orientation >= 5 { (2, 3) } else { (3, 2) };
        assert_eq!((image.frame.width, image.frame.height), shape);
        assert_eq!(image.worker_pid, Some(pid));
        for (pixel, index) in image
            .frame
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .zip(expected_order[orientation as usize - 1])
        {
            assert_eq!(pixel, &first.frame.pixels[index * 4..index * 4 + 4]);
        }
    }
    let invalid = files.0.join("truncated.jpeg");
    fs::write(&invalid, &plain[..plain.len() / 2]).unwrap();
    decoder.load(11, invalid);
    assert!(replies.recv_blocking().unwrap().result.is_err());
    let mut profile = b"ICC_PROFILE\0\x01\x01".to_vec();
    profile.extend_from_slice(b"invalid profile");
    let invalid = files.0.join("bad-icc.jpeg");
    fs::write(&invalid, segment(&plain, 226, &profile)).unwrap();
    decoder.load(12, invalid);
    let error = replies.recv_blocking().unwrap().result.err().unwrap();
    assert!(error.contains("ICC"), "{error}");
    // A real sRGB profile exercises LCMS, rather than silently discarding ICC.
    let mut profile = b"ICC_PROFILE\0\x01\x01".to_vec();
    profile.extend(lcms2::Profile::new_srgb().icc().unwrap());
    let profiled = files.0.join("profiled.jpeg");
    fs::write(&profiled, segment(&plain, 226, &profile)).unwrap();
    decoder.load(13, profiled);
    let image = replies.recv_blocking().unwrap().result.unwrap();
    assert_eq!(image.worker_pid, Some(pid));
    assert!(
        image
            .frame
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .all(|p| p[3] == 255)
    );
    // Changing the same path invalidates the metadata-keyed decoded cache.
    fs::write(&original, oriented(&plain, 6)).unwrap();
    decoder.load(14, original.clone());
    let image = replies.recv_blocking().unwrap().result.unwrap();
    assert_eq!((image.frame.width, image.frame.height), (2, 3));
    assert!(!image.cache_hit);
    for generation in 15..=114 {
        decoder.load(generation, original.clone());
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let reply = replies.recv_blocking().unwrap();
        if reply.generation == 114 {
            assert!(reply.result.is_ok());
            break;
        }
        assert!(Instant::now() < deadline);
    }
    // A crashed decoder must fail visibly, then restart for the next request.
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
    std::thread::sleep(Duration::from_millis(30));
    let restart = files.0.join("restart.jpeg");
    fs::write(&restart, &plain).unwrap();
    decoder.load(115, restart.clone());
    assert!(replies.recv_blocking().unwrap().result.is_err());
    decoder.load(116, restart);
    let recovered = replies.recv_blocking().unwrap().result.unwrap();
    let replacement = recovered.worker_pid.unwrap();
    assert_ne!(replacement, pid);
    decoder.cancel(117);
    replies.close();
    drop(decoder);
    assert!(
        !PathBuf::from(format!("/proc/{pid}")).exists(),
        "worker survived scheduler shutdown"
    );
    assert!(
        !PathBuf::from(format!("/proc/{replacement}")).exists(),
        "replacement worker survived scheduler shutdown"
    );
}
