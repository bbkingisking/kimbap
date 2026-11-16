//! JPEG scheduling and sandbox lifecycle. Blocking filesystem and socket work
//! stays on one thread; a single pending slot always contains the latest request.
use kimbap_jpeg_ipc::{self as ipc, Frame};
use std::{
    collections::VecDeque,
    fs::{Metadata, OpenOptions},
    io::{self, Read},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{MetadataExt, OpenOptionsExt},
            net::UnixStream,
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
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
            .name("jpeg-scheduler".into())
            .spawn(move || run(state, send, worker))
            .expect("create JPEG scheduler");
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
                return Err("The selected item is not a regular JPEG file".into());
            }
            if metadata.len() > ipc::MAX_INPUT as u64 {
                return Err("JPEG exceeds the 64-MiB input limit".into());
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
                return Err("JPEG exceeds the 64-MiB input limit".into());
            }
            if !bytes.starts_with(&[255, 216, 255]) {
                return Err("This file is not a JPEG; other formats are not wired up yet".into());
            }
            if shared.queue.lock().unwrap().generation != request.generation {
                return Err("Superseded request".into());
            }
            if worker.is_none() {
                worker = Some(
                    Worker::spawn(&worker_path)
                        .map_err(|e| format!("Cannot start sandboxed JPEG worker: {e}"))?,
                );
            }
            let response = worker.as_mut().unwrap().decode(&bytes);
            let frame = match response {
                Ok(result) => result?,
                Err(error) => {
                    worker = None;
                    return Err(format!("JPEG worker failed: {error}"));
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

struct Worker {
    socket: UnixStream,
    child: Child,
}
impl Worker {
    fn spawn(path: &Path) -> io::Result<Self> {
        let path = path.canonicalize()?;
        let (mut socket, child_socket) = UnixStream::pair()?;
        socket.set_read_timeout(Some(Duration::from_secs(4)))?;
        socket.set_write_timeout(Some(Duration::from_secs(4)))?;
        let descriptor = child_socket.as_raw_fd();
        let preferred = performance_cpus();
        let mut command = Command::new("/usr/bin/bwrap");
        command
            .args([
                "--unshare-all",
                "--die-with-parent",
                "--new-session",
                "--clearenv",
                "--setenv",
                "LC_ALL",
                "C",
                "--ro-bind",
                "/usr",
                "/usr",
                "--symlink",
                "usr/lib64",
                "/lib64",
                "--symlink",
                "usr/lib",
                "/lib",
                "--ro-bind-try",
                "/etc/ld.so.cache",
                "/etc/ld.so.cache",
                "--ro-bind",
            ])
            .arg(path)
            .args([
                "/decoder", "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--", "/decoder",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        // Only async-signal-safe syscalls are allowed between fork and exec.
        unsafe {
            command.pre_exec(move || {
                if let Some(mask) = preferred
                    && libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mask)
                        != 0
                {
                    return Err(io::Error::last_os_error());
                }
                if descriptor == 3 {
                    if libc::fcntl(3, libc::F_SETFD, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                } else if libc::dup2(descriptor, 3) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        drop(child_socket);
        let mut worker = Self {
            socket: socket.try_clone()?,
            child,
        };
        let mut magic = [0; 4];
        socket.read_exact(&mut magic)?;
        if &magic != ipc::MAGIC {
            return Err(ipc::invalid("JPEG worker protocol version mismatch"));
        }
        worker.socket = socket;
        Ok(worker)
    }
    fn decode(&mut self, bytes: &[u8]) -> io::Result<Result<Arc<Frame>, String>> {
        ipc::write_request(&mut self.socket, bytes)?;
        ipc::read_response(&mut self.socket)
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// Respect taskset/cgroup restrictions. On systems without capacity metadata,
// or with homogeneous allowed cores, retain the scheduler's original CPU set.
fn performance_cpus() -> Option<libc::cpu_set_t> {
    let mut allowed = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    if unsafe { libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut allowed) }
        != 0
    {
        return None;
    }
    let mut capacities = Vec::new();
    for cpu in 0..libc::CPU_SETSIZE as usize {
        if unsafe { libc::CPU_ISSET(cpu, &allowed) } {
            let capacity =
                std::fs::read_to_string(format!("/sys/devices/system/cpu/cpu{cpu}/cpu_capacity"))
                    .ok()?
                    .trim()
                    .parse::<u32>()
                    .ok()?;
            capacities.push((cpu, capacity));
        }
    }
    let max = capacities.iter().map(|(_, c)| *c).max()?;
    let min = capacities.iter().map(|(_, c)| *c).min()?;
    if max == min {
        return None;
    }
    let mut preferred = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    for (cpu, capacity) in capacities {
        if capacity == max {
            unsafe { libc::CPU_SET(cpu, &mut preferred) }
        }
    }
    Some(preferred)
}
