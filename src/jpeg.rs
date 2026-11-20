//! JPEG/PNG scheduling and sandbox lifecycle. Blocking filesystem and socket work
//! stays on one thread; a single pending slot always contains the latest request.
use crate::worker::Worker;
use kimbap_worker::{self as ipc, Frame};
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

pub struct Decoded {
    pub frame: Arc<Frame>,
    pub cache_hit: bool,
    pub worker_pid: Option<u32>,
    pub elapsed: Duration,
}
pub struct Reply {
    pub generation: u64,
    pub result: Result<Decoded, String>,
}
struct Request {
    generation: u64,
    path: PathBuf,
}
#[derive(Default)]
struct Queue {
    pending: Option<Request>,
    generation: u64,
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
    pub fn new(worker: PathBuf) -> (Self, async_channel::Receiver<Reply>) {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            ready: Condvar::new(),
        });
        let (send, receive) = async_channel::bounded(1);
        let state = shared.clone();
        let replies = send.clone();
        let thread = thread::Builder::new()
            .name("image-scheduler".into())
            .spawn(move || run(state, send, worker))
            .expect("create image scheduler");
        (
            Self {
                shared,
                thread: Some(thread),
                replies,
            },
            receive,
        )
    }
    pub fn load(&self, generation: u64, path: PathBuf) {
        let mut queue = self.shared.queue.lock().unwrap();
        queue.generation = generation;
        queue.active = true;
        queue.pending = Some(Request { generation, path });
        self.shared.ready.notify_one();
    }
    pub fn cancel(&self, generation: u64) {
        let mut queue = self.shared.queue.lock().unwrap();
        queue.generation = generation;
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
        // Closing the channel unblocks a pending result send even when the UI
        // context has already stopped and cannot poll its receiver again.
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
    seconds: i64,
    nanos: i64,
}
impl From<&Metadata> for Key {
    fn from(m: &Metadata) -> Self {
        Self {
            device: m.dev(),
            inode: m.ino(),
            size: m.len(),
            seconds: m.mtime(),
            nanos: m.mtime_nsec(),
        }
    }
}
fn run(shared: Arc<Shared>, send: async_channel::Sender<Reply>, worker_path: PathBuf) {
    let mut worker: Option<Worker> = None;
    let mut cache: VecDeque<(Key, Arc<Frame>)> = VecDeque::new();
    let mut cached = 0usize;
    loop {
        let request = {
            let mut queue = shared.queue.lock().unwrap();
            while queue.pending.is_none() && !queue.stopped {
                let (next, timeout) = shared.ready.wait_timeout(queue, IDLE).unwrap();
                queue = next;
                if timeout.timed_out() && !queue.active {
                    worker = None;
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
        let result = (|| -> Result<Decoded, String> {
            // O_NONBLOCK prevents FIFO/device opens from blocking this thread.
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&request.path)
                .map_err(|e| format!("Cannot open image: {e}"))?;
            let metadata = file
                .metadata()
                .map_err(|e| format!("Cannot inspect image: {e}"))?;
            if !metadata.is_file() {
                return Err("The selected item is not a regular image file".into());
            }
            if metadata.len() > ipc::MAX_INPUT as u64 {
                return Err("Image exceeds the 64-MiB input limit".into());
            }
            let key = Key::from(&metadata);
            if let Some(index) = cache.iter().position(|(k, _)| *k == key) {
                let entry = cache.remove(index).unwrap();
                let frame = entry.1.clone();
                cache.push_back(entry);
                return Ok(Decoded {
                    frame,
                    cache_hit: true,
                    worker_pid: worker.as_ref().map(|w| w.child.id()),
                    elapsed: start.elapsed(),
                });
            }
            let mut bytes = Vec::with_capacity(metadata.len() as usize);
            (&mut file)
                .take(ipc::MAX_INPUT as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| format!("Cannot read image: {e}"))?;
            if bytes.len() > ipc::MAX_INPUT {
                return Err("Image exceeds the 64-MiB input limit".into());
            }
            if !bytes.starts_with(&[255, 216, 255]) && !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
                return Err("This file is not a supported JPEG or PNG image".into());
            }
            if shared.queue.lock().unwrap().generation != request.generation {
                return Err("Superseded request".into());
            }
            if worker.is_none() {
                worker = Some(
                    Worker::spawn(&worker_path, false, ipc::JPEG_MAGIC)
                        .map_err(|e| format!("Cannot start sandboxed image worker: {e}"))?,
                );
            }
            let response = (|| {
                let socket = &mut worker.as_mut().unwrap().socket;
                ipc::write_request(socket, &bytes)?;
                ipc::read_response(socket)
            })();
            let frame = match response {
                Ok(result) => result?,
                Err(error) => {
                    worker = None;
                    return Err(format!("Image worker failed: {error}"));
                }
            };
            let size = frame.pixels.len();
            while cached + size > CACHE_BYTES && !cache.is_empty() {
                let (_, old) = cache.pop_front().unwrap();
                cached -= old.pixels.len();
            }
            if size <= CACHE_BYTES {
                cached += size;
                cache.push_back((key, frame.clone()));
            }
            Ok(Decoded {
                frame,
                cache_hit: false,
                worker_pid: worker.as_ref().map(|w| w.child.id()),
                elapsed: start.elapsed(),
            })
        })();
        if shared.queue.lock().unwrap().generation != request.generation {
            continue;
        }
        if send
            .send_blocking(Reply {
                generation: request.generation,
                result,
            })
            .is_err()
        {
            break;
        }
    }
}
