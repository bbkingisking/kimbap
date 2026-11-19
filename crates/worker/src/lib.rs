//! Shared native-worker transport and confinement. Pixels cross the sandbox in
//! sealed memfds, without GTK, GLib, or decoder dependencies.
pub mod pdf;
pub mod sandbox;
use memmap2::Mmap;
use rustix::{
    fs::{MemfdFlags, SealFlags, fcntl_add_seals, fcntl_get_seals},
    net::{
        self, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
        SendAncillaryMessage, SendFlags,
    },
};
use std::{
    fs::File,
    io::{self, IoSlice, IoSliceMut, Read, Write},
    mem::MaybeUninit,
    os::{fd::AsFd, unix::net::UnixStream},
    sync::Arc,
};

pub use memmap2::MmapMut;
pub const JPEG_MAGIC: &[u8; 4] = b"KJP1";
pub const MAX_INPUT: usize = 64 * 1024 * 1024;
pub const MAX_PIXELS: usize = 40_000_000;
pub const MAX_ERROR: usize = 16 * 1024;

pub fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
pub fn pixel_len(width: u32, height: u32) -> io::Result<usize> {
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| invalid("Pixel dimensions overflow"))?;
    if width == 0 || height == 0 || width > 20_000 || height > 20_000 || pixels > MAX_PIXELS {
        return Err(invalid("Frame exceeds the 40-megapixel limit"));
    }
    Ok(pixels * 4)
}
pub fn seals() -> SealFlags {
    SealFlags::WRITE | SealFlags::GROW | SealFlags::SHRINK | SealFlags::SEAL
}
pub fn create_pixels(width: u32, height: u32) -> io::Result<(File, MmapMut)> {
    let len = pixel_len(width, height)?;
    let file = File::from(rustix::fs::memfd_create(
        "kimbap-rgba",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )?);
    file.set_len(len as u64)?;
    // This mapping is private to the worker until it is dropped and sealed.
    let pixels = unsafe { MmapMut::map_mut(&file)? };
    Ok((file, pixels))
}
pub fn seal_pixels(file: &File) -> io::Result<()> {
    fcntl_add_seals(file, seals())?;
    Ok(())
}

#[derive(Debug)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub decode_us: u64,
    pub pixels: Mmap,
}
impl AsRef<[u8]> for Frame {
    fn as_ref(&self) -> &[u8] {
        &self.pixels
    }
}

pub fn write_request(socket: &mut UnixStream, bytes: &[u8]) -> io::Result<()> {
    if bytes.is_empty() || bytes.len() > MAX_INPUT {
        return Err(invalid("Input exceeds the 64-MiB limit"));
    }
    socket.write_all(&(bytes.len() as u32).to_le_bytes())?;
    socket.write_all(bytes)
}
pub fn read_request(socket: &mut UnixStream) -> io::Result<Option<Vec<u8>>> {
    let mut length = [0; 4];
    match socket.read(&mut length[..1])? {
        0 => return Ok(None),
        1 => (),
        _ => unreachable!(),
    }
    socket.read_exact(&mut length[1..])?;
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > MAX_INPUT {
        return Err(invalid("Invalid input request length"));
    }
    let mut bytes = vec![0; length];
    socket.read_exact(&mut bytes)?;
    Ok(Some(bytes))
}
pub fn write_error(socket: &mut UnixStream, message: &str) -> io::Result<()> {
    let mut message = message;
    if message.len() > MAX_ERROR {
        let mut end = MAX_ERROR;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message = &message[..end];
    }
    socket.write_all(&[0])?;
    socket.write_all(&(message.len() as u32).to_le_bytes())?;
    socket.write_all(message.as_bytes())
}
pub fn write_frame(
    socket: &mut UnixStream,
    width: u32,
    height: u32,
    decode_us: u64,
    file: &File,
) -> io::Result<()> {
    socket.write_all(&[1])?;
    socket.write_all(&width.to_le_bytes())?;
    socket.write_all(&height.to_le_bytes())?;
    socket.write_all(&decode_us.to_le_bytes())?;
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    let fds = [file.as_fd()];
    if !control.push(SendAncillaryMessage::ScmRights(&fds)) {
        return Err(invalid("Cannot construct pixel descriptor message"));
    }
    if net::sendmsg(
        &*socket,
        &[IoSlice::new(b"P")],
        &mut control,
        SendFlags::NOSIGNAL,
    )? != 1
    {
        return Err(invalid("Cannot send pixel descriptor"));
    }
    Ok(())
}
pub fn read_status(socket: &mut UnixStream) -> io::Result<Result<(), String>> {
    let mut tag = [0];
    socket.read_exact(&mut tag)?;
    if tag[0] == 0 {
        let mut size = [0; 4];
        socket.read_exact(&mut size)?;
        let size = u32::from_le_bytes(size) as usize;
        if size > MAX_ERROR {
            return Err(invalid("Worker error exceeds protocol limit"));
        }
        let mut bytes = vec![0; size];
        socket.read_exact(&mut bytes)?;
        return Ok(Err(
            String::from_utf8(bytes).map_err(|_| invalid("Invalid worker error encoding"))?
        ));
    }
    if tag[0] != 1 {
        return Err(invalid("Unknown worker response"));
    }
    Ok(Ok(()))
}

pub fn read_response(socket: &mut UnixStream) -> io::Result<Result<Arc<Frame>, String>> {
    if let Err(message) = read_status(socket)? {
        return Ok(Err(message));
    }
    let mut header = [0; 16];
    socket.read_exact(&mut header)?;
    let width = u32::from_le_bytes(header[..4].try_into().unwrap());
    let height = u32::from_le_bytes(header[4..8].try_into().unwrap());
    let decode_us = u64::from_le_bytes(header[8..].try_into().unwrap());
    let len = pixel_len(width, height)?;
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(2))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let mut marker = [0];
    let received = net::recvmsg(
        &*socket,
        &mut [IoSliceMut::new(&mut marker)],
        &mut control,
        RecvFlags::CMSG_CLOEXEC,
    )?;
    if received.bytes != 1 || marker != *b"P" || received.flags.contains(net::ReturnFlags::CTRUNC) {
        return Err(invalid("Invalid pixel descriptor message"));
    }
    let mut fds = Vec::new();
    for message in control.drain() {
        if let RecvAncillaryMessage::ScmRights(rights) = message {
            fds.extend(rights);
        }
    }
    if fds.len() != 1 {
        return Err(invalid("Expected one sealed pixel descriptor"));
    }
    let file = File::from(fds.pop().unwrap());
    if file.metadata()?.len() != len as u64 || !fcntl_get_seals(&file)?.contains(seals()) {
        return Err(invalid(
            "Worker returned mutable or incorrectly sized pixels",
        ));
    }
    // All writes, truncation and growth are sealed before mapping. The worker
    // cannot mutate these bytes while GTK or the GPU is reading them.
    let pixels = unsafe { Mmap::map(&file)? };
    Ok(Ok(Arc::new(Frame {
        width,
        height,
        decode_us,
        pixels,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unsealed_pixels() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let (file, mut pixels) = create_pixels(2, 3).unwrap();
        pixels.fill(23);
        drop(pixels);
        write_frame(&mut a, 2, 3, 10, &file).unwrap();
        assert!(
            read_response(&mut b)
                .unwrap_err()
                .to_string()
                .contains("mutable")
        );
    }
    #[test]
    fn sealed_pixels_and_errors_roundtrip() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let (file, mut pixels) = create_pixels(2, 3).unwrap();
        pixels.fill(23);
        drop(pixels);
        seal_pixels(&file).unwrap();
        write_frame(&mut a, 2, 3, 10, &file).unwrap();
        let frame = read_response(&mut b).unwrap().unwrap();
        assert_eq!(frame.as_ref().as_ref(), &[23; 24]);
        write_error(&mut a, "Invalid JPEG 漢字").unwrap();
        assert_eq!(
            read_response(&mut b).unwrap().unwrap_err(),
            "Invalid JPEG 漢字"
        );
    }
    #[test]
    fn dimensions_are_bounded() {
        assert!(pixel_len(u32::MAX, u32::MAX).is_err());
        assert!(pixel_len(0, 1).is_err());
        assert!(pixel_len(10_000, 10_000).is_err());
    }
}
