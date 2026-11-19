//! PDF scheduling: retain one open document and a bounded LRU of rendered pages.
//! File reads, native-worker startup and IPC never run on GTK's main context.
use crate::worker::Worker;
use kimbap_worker::{self as ipc, Frame, pdf as wire};
use std::{
    collections::VecDeque,
    fs::{Metadata, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};
const CACHE_BYTES: usize = 64 * 1024 * 1024;
const IDLE: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct Rendered {
    pub frame: Arc<Frame>,
    pub pages: u32,
    pub cache_hit: bool,
    pub worker_pid: Option<u32>,
    pub elapsed: Duration,
}
pub struct Reply {
    pub generation: u64,
    pub revision: u64,
    pub page: u32,
    pub result: Result<Rendered, String>,
}
struct Request {
    generation: u64,
    revision: u64,
    path: PathBuf,
    page: u32,
    width: u32,
    height: u32,
}
#[derive(Default)]
struct Queue {
    pending: Option<Request>,
    revision: u64,
    active: bool,
    stopped: bool,
}
struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
}
pub struct Decoder {
    shared: Arc<Shared>,
    thread: Option<thread::JoinHandle<()>>,
    replies: async_channel::Sender<Reply>,
}
impl Decoder {
    pub fn new(worker: PathBuf, library: PathBuf) -> (Self, async_channel::Receiver<Reply>) {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            ready: Condvar::new(),
        });
        let (send, receive) = async_channel::bounded(1);
        let state = shared.clone();
        let replies = send.clone();
        let thread = thread::Builder::new()
            .name("pdf-scheduler".into())
            .spawn(move || run(state, send, worker, library))
            .expect("create PDF scheduler");
        (
            Self {
                shared,
                thread: Some(thread),
                replies,
            },
            receive,
        )
    }
    pub fn render(
        &self,
        generation: u64,
        path: PathBuf,
        page: u32,
        width: u32,
        height: u32,
    ) -> u64 {
        let mut queue = self.shared.queue.lock().unwrap();
        queue.revision = queue.revision.wrapping_add(1);
        let revision = queue.revision;
        queue.active = true;
        queue.pending = Some(Request {
            generation,
            revision,
            path,
            page,
            width,
            height,
        });
        self.shared.ready.notify_one();
        revision
    }
    pub fn cancel(&self) {
        let mut queue = self.shared.queue.lock().unwrap();
        queue.revision = queue.revision.wrapping_add(1);
        queue.active = false;
        queue.pending = None;
        self.shared.ready.notify_one();
    }
}
impl Drop for Decoder {
    fn drop(&mut self) {
        self.replies.close();
        self.shared.queue.lock().unwrap().stopped = true;
        self.shared.ready.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
#[derive(Clone, PartialEq, Eq)]
struct Key {
    device: u64,
    inode: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}
impl From<&Metadata> for Key {
    fn from(m: &Metadata) -> Self {
        Self {
            device: m.dev(),
            inode: m.ino(),
            size: m.len(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        }
    }
}
#[derive(PartialEq, Eq)]
struct CacheKey {
    file: Key,
    page: u32,
    width: u32,
    height: u32,
}
fn run(
    shared: Arc<Shared>,
    send: async_channel::Sender<Reply>,
    worker_path: PathBuf,
    library: PathBuf,
) {
    let mut worker: Option<Worker> = None;
    let mut opened: Option<(Key, u32)> = None;
    let mut cache: VecDeque<(CacheKey, u32, Arc<Frame>)> = VecDeque::new();
    let mut cached = 0;
    loop {
        let request = {
            let mut queue = shared.queue.lock().unwrap();
            while queue.pending.is_none() && !queue.stopped {
                let (next, timeout) = shared.ready.wait_timeout(queue, IDLE).unwrap();
                queue = next;
                if timeout.timed_out() && !queue.active {
                    worker = None;
                    opened = None;
                    cache.clear();
                    cached = 0;
                }
            }
            if queue.stopped {
                break;
            }
            queue.pending.take().unwrap()
        };
        let start = Instant::now();
        let result = (|| -> Result<Rendered, String> {
            wire::validate_size(request.width, request.height).map_err(|e| e.to_string())?;
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&request.path)
                .map_err(|e| format!("Cannot open PDF: {e}"))?;
            let metadata = file
                .metadata()
                .map_err(|e| format!("Cannot inspect PDF: {e}"))?;
            if !metadata.is_file() {
                return Err("The selected item is not a regular PDF file".into());
            }
            if metadata.len() > ipc::MAX_INPUT as u64 {
                return Err("PDF exceeds the 64-MiB input limit".into());
            }
            let key = Key::from(&metadata);
            let cache_key = CacheKey {
                file: key.clone(),
                page: request.page,
                width: request.width,
                height: request.height,
            };
            if let Some(index) = cache.iter().position(|(k, _, _)| *k == cache_key) {
                let entry = cache.remove(index).unwrap();
                let pages = entry.1;
                let frame = entry.2.clone();
                cache.push_back(entry);
                return Ok(Rendered {
                    frame,
                    pages,
                    cache_hit: true,
                    worker_pid: worker.as_ref().map(|w| w.child.id()),
                    elapsed: start.elapsed(),
                });
            }
            if shared.queue.lock().unwrap().revision != request.revision {
                return Err("Superseded request".into());
            }
            if opened.as_ref().is_none_or(|(old, _)| *old != key) {
                let mut bytes = Vec::with_capacity(metadata.len() as usize);
                (&mut file)
                    .take(ipc::MAX_INPUT as u64 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|e| format!("Cannot read PDF: {e}"))?;
                if bytes.len() > ipc::MAX_INPUT {
                    return Err("PDF exceeds the 64-MiB input limit".into());
                }
                if shared.queue.lock().unwrap().revision != request.revision {
                    return Err("Superseded request".into());
                }
                if worker.is_none() {
                    let process = Worker::spawn(&worker_path, Some(&library), wire::MAGIC)
                        .map_err(|error| format!(
                            "Cannot start sandboxed PDF worker: {error}. Check --pdf-worker and --pdfium-library"
                        ))?;
                    worker = Some(process);
                }
                opened = None;
                let response = (|| {
                    let socket = &mut worker.as_mut().unwrap().socket;
                    wire::write_open(socket, &bytes)?;
                    wire::read_pages(socket)
                })();
                match response {
                    Ok(Ok(pages)) => opened = Some((key, pages)),
                    Ok(Err(message)) => return Err(message),
                    Err(error) => {
                        worker = None;
                        return Err(format!("PDF worker failed: {error}"));
                    }
                }
            }
            if shared.queue.lock().unwrap().revision != request.revision {
                return Err("Superseded request".into());
            }
            let pages = opened.as_ref().unwrap().1;
            if request.page >= pages {
                return Err("PDF page is out of range".into());
            }
            let response = (|| {
                let socket = &mut worker.as_mut().unwrap().socket;
                wire::write_render(socket, request.page, request.width, request.height)?;
                ipc::read_response(socket)
            })();
            let frame = match response {
                Ok(result) => result?,
                Err(error) => {
                    worker = None;
                    opened = None;
                    return Err(format!("PDF worker failed: {error}"));
                }
            };
            if frame.width > request.width || frame.height > request.height {
                worker = None;
                opened = None;
                return Err("PDF worker returned oversized pixels".into());
            }
            let size = frame.pixels.len();
            while cached + size > CACHE_BYTES && !cache.is_empty() {
                cached -= cache.pop_front().unwrap().2.pixels.len();
            }
            if size <= CACHE_BYTES {
                cached += size;
                cache.push_back((cache_key, pages, frame.clone()));
            }
            Ok(Rendered {
                frame,
                pages,
                cache_hit: false,
                worker_pid: worker.as_ref().map(|w| w.child.id()),
                elapsed: start.elapsed(),
            })
        })();
        if shared.queue.lock().unwrap().revision != request.revision {
            continue;
        }
        if send
            .send_blocking(Reply {
                generation: request.generation,
                revision: request.revision,
                page: request.page,
                result,
            })
            .is_err()
        {
            break;
        }
    }
}
