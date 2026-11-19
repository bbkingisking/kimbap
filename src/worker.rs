//! Native workers share a bounded socket and a private bubblewrap namespace.
use kimbap_worker as ipc;
use std::{
    io::{self, Read},
    os::{
        fd::AsRawFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};
pub(crate) struct Worker {
    pub socket: UnixStream,
    pub child: Child,
}
impl Worker {
    pub fn spawn(path: &Path, library: Option<&Path>, magic: &[u8; 4]) -> io::Result<Self> {
        let library = library.map(Path::canonicalize).transpose()?;
        let path = path.canonicalize()?;
        let (socket, child_socket) = UnixStream::pair()?;
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
                "/decoder", "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp",
            ]);
        if let Some(library) = library.as_ref() {
            // Mount the renderer explicitly, even when it lives outside /usr.
            command
                .args(["--ro-bind"])
                .arg(library)
                .arg("/pdfium.so")
                .args([
                    "--ro-bind-try",
                    "/etc/fonts",
                    "/etc/fonts",
                    "--ro-bind-try",
                    "/var/cache/fontconfig",
                    "/var/cache/fontconfig",
                ]);
        }
        command.args(["--", "/decoder"]);
        if library.is_some() {
            command.arg("/pdfium.so");
        }
        command
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
        let mut worker = Self { socket, child };
        let mut received = [0; 4];
        worker.socket.read_exact(&mut received)?;
        if &received != magic {
            return Err(ipc::invalid("Worker protocol version mismatch"));
        }
        Ok(worker)
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
