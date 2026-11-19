//! PDF commands retain one document in the worker; only opening sends file bytes.
use crate::{MAX_INPUT, invalid, read_request, read_status, write_request};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;

pub const MAGIC: &[u8; 4] = b"KPD1";
pub const MAX_EDGE: u32 = 4096;
pub enum Request {
    Open(Vec<u8>),
    Render { page: u32, width: u32, height: u32 },
}
pub fn write_open(socket: &mut UnixStream, bytes: &[u8]) -> io::Result<()> {
    if bytes.is_empty() || bytes.len() > MAX_INPUT {
        return Err(invalid("PDF exceeds the 64-MiB input limit"));
    }
    socket.write_all(&[0])?;
    write_request(socket, bytes)
}
pub fn write_render(socket: &mut UnixStream, page: u32, width: u32, height: u32) -> io::Result<()> {
    validate_size(width, height)?;
    socket.write_all(&[1])?;
    for value in [page, width, height] {
        socket.write_all(&value.to_le_bytes())?;
    }
    Ok(())
}
pub fn validate_size(width: u32, height: u32) -> io::Result<()> {
    if width == 0 || height == 0 || width > MAX_EDGE || height > MAX_EDGE {
        return Err(invalid("PDF render size must be between 1 and 4096 pixels"));
    }
    Ok(())
}
pub fn read_request_pdf(socket: &mut UnixStream) -> io::Result<Option<Request>> {
    let mut tag = [0];
    if socket.read(&mut tag)? == 0 {
        return Ok(None);
    }
    match tag[0] {
        0 => Ok(Some(Request::Open(
            read_request(socket)?.ok_or_else(|| invalid("Missing PDF bytes"))?,
        ))),
        1 => {
            let mut bytes = [0; 12];
            socket.read_exact(&mut bytes)?;
            let value = |offset| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
            let (page, width, height) = (value(0), value(4), value(8));
            validate_size(width, height)?;
            Ok(Some(Request::Render {
                page,
                width,
                height,
            }))
        }
        _ => Err(invalid("Unknown PDF request")),
    }
}
pub fn write_pages(socket: &mut UnixStream, pages: u32) -> io::Result<()> {
    socket.write_all(&[1])?;
    socket.write_all(&pages.to_le_bytes())
}
pub fn read_pages(socket: &mut UnixStream) -> io::Result<Result<u32, String>> {
    if let Err(message) = read_status(socket)? {
        return Ok(Err(message));
    }
    let mut bytes = [0; 4];
    socket.read_exact(&mut bytes)?;
    let pages = u32::from_le_bytes(bytes);
    // Bound metadata from the worker independently of native page index types.
    if pages == 0 || pages > 1_000_000 {
        return Err(invalid("Invalid PDF page count"));
    }
    Ok(Ok(pages))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requests_roundtrip_and_bound_dimensions() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        write_open(&mut a, b"%PDF-1.7").unwrap();
        assert!(
            matches!(read_request_pdf(&mut b).unwrap(),Some(Request::Open(bytes)) if bytes==b"%PDF-1.7")
        );
        write_render(&mut a, 2, 1200, 1600).unwrap();
        assert!(matches!(
            read_request_pdf(&mut b).unwrap(),
            Some(Request::Render {
                page: 2,
                width: 1200,
                height: 1600
            })
        ));
        assert!(write_render(&mut a, 0, 0, 1).is_err());
        assert!(write_render(&mut a, 0, MAX_EDGE + 1, 1).is_err());
        write_pages(&mut a, 2).unwrap();
        assert_eq!(read_pages(&mut b).unwrap().unwrap(), 2);
        crate::write_error(&mut a, "Locked PDF").unwrap();
        assert_eq!(read_pages(&mut b).unwrap().unwrap_err(), "Locked PDF");
    }
}
