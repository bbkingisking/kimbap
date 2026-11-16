use std::io;

pub fn restrict() -> io::Result<()> {
    // Kernel limits cover native allocations too; a decoder bug cannot consume
    // unbounded address space, descriptors, or create files in the namespace.
    for (resource, value) in [
        (libc::RLIMIT_AS, 1536 * 1024 * 1024),
        (libc::RLIMIT_NOFILE, 32),
        (libc::RLIMIT_CORE, 0),
    ] {
        let limit = libc::rlimit {
            rlim_cur: value,
            rlim_max: value,
        };
        if unsafe { libc::setrlimit(resource, &limit) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(target_arch = "aarch64")]
    let arch = 0xc00000b7u32;
    #[cfg(target_arch = "x86_64")]
    let arch = 0xc000003eu32;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    compile_error!("Define the seccomp audit architecture for this target");
    let allowed = [
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_close,
        libc::SYS_recvfrom,
        libc::SYS_sendto,
        libc::SYS_statx,
        libc::SYS_recvmsg,
        libc::SYS_sendmsg,
        libc::SYS_lseek,
        libc::SYS_fstat,
        libc::SYS_newfstatat,
        libc::SYS_mmap,
        libc::SYS_munmap,
        libc::SYS_mprotect,
        libc::SYS_brk,
        libc::SYS_madvise,
        libc::SYS_mremap,
        libc::SYS_futex,
        libc::SYS_fcntl,
        libc::SYS_memfd_create,
        libc::SYS_ftruncate,
        libc::SYS_getrandom,
        libc::SYS_clock_gettime,
        libc::SYS_exit,
        libc::SYS_exit_group,
        libc::SYS_rt_sigaction,
        libc::SYS_rt_sigreturn,
        libc::SYS_rt_sigprocmask,
        libc::SYS_sigaltstack,
        libc::SYS_restart_syscall,
        libc::SYS_getpid,
        libc::SYS_gettid,
        libc::SYS_sched_yield,
    ];
    let stmt = |code, k| libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    };
    let jump = |k, jt, jf| libc::sock_filter {
        code: 0x15,
        jt,
        jf,
        k,
    };
    let mut filter = vec![
        stmt(0x20, 4),
        jump(arch, 1, 0),
        stmt(0x06, libc::SECCOMP_RET_KILL_PROCESS),
        stmt(0x20, 0),
    ];
    for syscall in allowed {
        filter.push(jump(syscall as u32, 0, 1));
        filter.push(stmt(0x06, libc::SECCOMP_RET_ALLOW));
    }
    filter.push(stmt(0x06, libc::SECCOMP_RET_ERRNO | libc::EPERM as u32));
    let program = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_mut_ptr(),
    };
    // No pointers survive this call: the kernel copies the BPF program.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0
        || unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
