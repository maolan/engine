//! Single crash-detection watchdog for all out-of-process plugin hosts.
//!
//! Exactly one background thread per engine process watches every plugin-host
//! child PID. When a watched child exits, the watchdog sets that processor's
//! `bypassed` flag (an `Arc<AtomicBool>`); the real-time audio path only ever
//! loads that flag, so no audio-thread code performs `waitpid` or any other
//! syscall for crash detection.
//!
//! Registration (`watch`/`unwatch`) is control-side only; a plain `Mutex`
//! guards the registry, which is never touched from the audio thread.
//!
//! Platform backends:
//! - FreeBSD/macOS: kqueue `EVFILT_PROC | NOTE_EXIT`, one event per PID.
//!   `NOTE_EXIT` fires when the process exits; because we only ever watch our
//!   own unreaped children, the PID cannot be reused while registered (we hold
//!   the registration until `unwatch`/`Drop`), so there is no ABA concern.
//! - Linux: `pidfd_open(2)` + `epoll`. A readable pidfd means the child has
//!   exited; pidfds pin the process, so PID reuse is likewise impossible.
//! - Windows: `OpenProcess(SYNCHRONIZE)` handles waited in batches of 63 on
//!   the single thread via `WaitForMultipleObjects` (plus one wake event).
//!
//! The watchdog deliberately does not own the `std::process::Child` handles;
//! it works purely from PIDs so the processors keep their existing
//! control-side ownership (`Drop` -> `ipc::drop_host`).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// One crash-detection watchdog for the whole engine process.
///
/// Cheap to clone conceptually, but there should be only one: use
/// [`ProcessWatchdog::global`].
pub struct ProcessWatchdog {
    inner: Arc<Inner>,
    wake: imp::Wake,
}

struct Inner {
    /// pid -> registration (bypass flag plus any platform handle).
    /// Source of truth for what the watcher thread must have registered.
    registry: Mutex<HashMap<u32, imp::Registration>>,
    /// Platform poll handle shared with the watcher thread (kqueue/epoll fd
    /// or the Windows wake event handle).
    poller: imp::Poller,
}

static GLOBAL: OnceLock<ProcessWatchdog> = OnceLock::new();

impl ProcessWatchdog {
    /// The process-wide watchdog, started on first use.
    pub fn global() -> &'static ProcessWatchdog {
        GLOBAL.get_or_init(Self::start)
    }

    /// Spawn the single watcher thread.
    pub fn start() -> Self {
        let poller = imp::Poller::new();
        let wake = poller.wake();
        let inner = Arc::new(Inner {
            registry: Mutex::new(HashMap::new()),
            poller: poller.clone(),
        });
        let thread_inner = Arc::clone(&inner);
        std::thread::Builder::new()
            .name("plugin-host-watchdog".to_string())
            .spawn(move || imp::watcher_thread(thread_inner))
            .expect("failed to spawn plugin-host watchdog thread");
        Self { inner, wake }
    }

    /// Watch `pid`; when it exits, `bypass` is set to `true`.
    ///
    /// Called from the control side at host-spawn time. If the process is
    /// already gone (registration failed), the flag is set immediately.
    pub fn watch(&self, pid: u32, bypass: Arc<AtomicBool>) {
        match imp::Registration::new(pid, Arc::clone(&bypass)) {
            Ok(reg) => {
                self.inner.registry.lock().unwrap().insert(pid, reg);
                self.wake.wake();
            }
            Err(e) => {
                tracing::warn!(pid, error = %e, "watchdog: cannot watch pid, treating as exited");
                // Registration failed: the process is already gone or
                // unwatachable; fail safe into bypass.
                bypass.store(true, Ordering::Relaxed);
            }
        }
    }

    /// Stop watching `pid` (clean shutdown only).
    pub fn unwatch(&self, pid: u32) {
        if let Some(mut reg) = self.inner.registry.lock().unwrap().remove(&pid) {
            reg.deregister(&self.inner.poller);
        }
        self.wake.wake();
    }
}

#[cfg(any(target_os = "freebsd", target_os = "macos"))]
mod imp {
    use super::{Arc, AtomicBool, Inner, Ordering};
    use std::collections::HashSet;
    use std::io;
    use std::os::unix::io::RawFd;

    /// A watched registration: on kqueue no per-pid handle is needed, the
    /// flag alone is stored.
    pub struct Registration {
        bypass: Arc<AtomicBool>,
    }

    impl Registration {
        pub fn new(_pid: u32, bypass: Arc<AtomicBool>) -> Result<Self, io::Error> {
            Ok(Self { bypass })
        }

        pub fn bypass(&self) -> &AtomicBool {
            &self.bypass
        }

        /// The watcher thread removes the kqueue filter; nothing to do here
        /// from the caller side.
        pub fn deregister(&mut self, _poller: &Poller) {}
    }

    /// Wake handle: a byte written to a pipe whose read end is in the kqueue.
    #[derive(Clone)]
    pub struct Wake {
        fd: RawFd,
    }

    impl Wake {
        pub fn wake(&self) {
            unsafe {
                libc::write(self.fd, [0u8; 1].as_ptr().cast(), 1);
            }
        }
    }

    /// kqueue fd plus the wake pipe (created in `new`).
    #[derive(Clone)]
    pub struct Poller {
        kq: RawFd,
        wake_read: RawFd,
        wake_write: RawFd,
    }

    impl Poller {
        pub fn new() -> Self {
            unsafe {
                let kq = libc::kqueue();
                assert!(kq >= 0, "kqueue failed: {}", io::Error::last_os_error());
                let mut fds = [0; 2];
                let rc = libc::pipe(fds.as_mut_ptr());
                assert!(rc == 0, "pipe failed: {}", io::Error::last_os_error());
                set_nonblocking(fds[0]);
                set_nonblocking(fds[1]);
                // Wake pipe read end: level-triggered read notification.
                kevent_add(kq, fds[0] as usize, libc::EVFILT_READ, libc::EV_ADD, 0);
                Self {
                    kq,
                    wake_read: fds[0],
                    wake_write: fds[1],
                }
            }
        }

        pub fn wake(&self) -> Wake {
            Wake {
                fd: self.wake_write,
            }
        }
    }

    fn kevent_add(kq: RawFd, ident: usize, filter: i16, flags: u16, fflags: u32) {
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        ev.ident = ident;
        ev.filter = filter;
        ev.flags = flags;
        ev.fflags = fflags;
        let rc = unsafe { libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        if rc < 0 {
            tracing::warn!(
                error = %io::Error::last_os_error(),
                "watchdog: kevent ({filter}, {flags}) failed"
            );
        }
    }

    /// Put a pipe fd into non-blocking mode so `drain_fd` terminates.
    fn set_nonblocking(fd: RawFd) {
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }
    }

    fn drain_fd(fd: RawFd) {
        let mut buf = [0u8; 64];
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
        }
    }

    pub fn watcher_thread(inner: Arc<Inner>) {
        let kq = inner.poller.kq;
        let wake_read = inner.poller.wake_read;
        // PIDs currently registered in the kqueue (thread-local mirror).
        let mut registered: HashSet<u32> = HashSet::new();
        let mut events: Vec<libc::kevent> = vec![unsafe { std::mem::zeroed() }; 64];
        loop {
            // Sync kqueue registrations with the registry.
            {
                let registry = inner.registry.lock().unwrap();
                for &pid in registry.keys() {
                    if !registered.contains(&pid) {
                        kevent_add(
                            kq,
                            pid as usize,
                            libc::EVFILT_PROC,
                            libc::EV_ADD,
                            libc::NOTE_EXIT,
                        );
                        registered.insert(pid);
                    }
                }
                let stale: Vec<u32> = registered
                    .iter()
                    .copied()
                    .filter(|pid| !registry.contains_key(pid))
                    .collect();
                drop(registry);
                for pid in stale {
                    kevent_add(
                        kq,
                        pid as usize,
                        libc::EVFILT_PROC,
                        libc::EV_DELETE | libc::EV_RECEIPT,
                        libc::NOTE_EXIT,
                    );
                    registered.remove(&pid);
                }
            }

            let n = unsafe {
                libc::kevent(
                    kq,
                    std::ptr::null(),
                    0,
                    events.as_mut_ptr(),
                    events.len() as i32,
                    std::ptr::null(),
                )
            };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                tracing::error!(error = %err, "watchdog: kevent wait failed");
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }

            for ev in &events[..n as usize] {
                if ev.filter == libc::EVFILT_READ && ev.ident == wake_read as usize && ev.data > 0 {
                    drain_fd(wake_read);
                    continue;
                }
                if ev.filter != libc::EVFILT_PROC {
                    continue;
                }
                let pid = ev.ident as u32;
                // FreeBSD reports the exit status in `data` when
                // NOTE_EXITSTATUS is used; we only need to know it exited.
                if let Some(reg) = inner.registry.lock().unwrap().remove(&pid) {
                    reg.bypass().store(true, Ordering::Relaxed);
                }
                kevent_add(
                    kq,
                    pid as usize,
                    libc::EVFILT_PROC,
                    libc::EV_DELETE | libc::EV_RECEIPT,
                    libc::NOTE_EXIT,
                );
                registered.remove(&pid);
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{Arc, AtomicBool, Inner, Ordering};
    use std::collections::HashMap as StdMap;
    use std::io;
    use std::os::unix::io::RawFd;

    /// Registration holds the pidfd; a readable pidfd means the child exited.
    pub struct Registration {
        fd: RawFd,
        bypass: Arc<AtomicBool>,
    }

    impl Registration {
        pub fn new(pid: u32, bypass: Arc<AtomicBool>) -> Result<Self, io::Error> {
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as i32, 0u32) } as i32;
            if fd < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(Self { fd, bypass })
            }
        }

        pub fn bypass(&self) -> &AtomicBool {
            &self.bypass
        }

        pub fn deregister(&mut self, poller: &Poller) {
            epoll_ctl(poller.epoll, libc::EPOLL_CTL_DEL, self.fd, 0);
            unsafe {
                libc::close(self.fd);
            }
        }
    }

    #[derive(Clone)]
    pub struct Wake {
        fd: RawFd,
    }

    impl Wake {
        pub fn wake(&self) {
            unsafe {
                libc::write(self.fd, [0u8; 1].as_ptr().cast(), 1);
            }
        }
    }

    /// epoll fd plus the wake pipe (created in `new`). The wake pipe is
    /// registered in epoll with data `0`; watched pidfds use the pid as data.
    #[derive(Clone)]
    pub struct Poller {
        epoll: RawFd,
        wake_read: RawFd,
        wake_write: RawFd,
    }

    impl Poller {
        pub fn new() -> Self {
            unsafe {
                let epoll = libc::epoll_create1(0);
                assert!(
                    epoll >= 0,
                    "epoll_create1 failed: {}",
                    io::Error::last_os_error()
                );
                let mut fds = [0; 2];
                let rc = libc::pipe(fds.as_mut_ptr());
                assert!(rc == 0, "pipe failed: {}", io::Error::last_os_error());
                set_nonblocking(fds[0]);
                set_nonblocking(fds[1]);
                epoll_ctl(epoll, libc::EPOLL_CTL_ADD, fds[0], libc::EPOLLIN as u32);
                Self {
                    epoll,
                    wake_read: fds[0],
                    wake_write: fds[1],
                }
            }
        }

        pub fn wake(&self) -> Wake {
            Wake {
                fd: self.wake_write,
            }
        }
    }

    fn epoll_ctl(epoll: RawFd, op: i32, fd: RawFd, events: u32) {
        let mut ev = libc::epoll_event { events, u64: 0 };
        let rc = unsafe { libc::epoll_ctl(epoll, op, fd, &mut ev) };
        if rc < 0 {
            tracing::warn!(
                error = %io::Error::last_os_error(),
                "watchdog: epoll_ctl op={op} fd={fd} failed"
            );
        }
    }

    fn set_nonblocking(fd: RawFd) {
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }
    }

    fn drain_fd(fd: RawFd) {
        let mut buf = [0u8; 64];
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
        }
    }

    pub fn watcher_thread(inner: Arc<Inner>) {
        let epoll = inner.poller.epoll;
        let wake_read = inner.poller.wake_read;
        // pid -> pidfd, thread-local mirror of what is in epoll.
        let mut registered: StdMap<u32, RawFd> = StdMap::new();
        let mut events: Vec<libc::epoll_event> = vec![libc::epoll_event { events: 0, u64: 0 }; 64];
        loop {
            {
                let registry = inner.registry.lock().unwrap();
                for (&pid, reg) in registry.iter() {
                    if !registered.contains_key(&pid) {
                        let mut ev = libc::epoll_event {
                            events: libc::EPOLLIN as u32,
                            u64: u64::from(pid),
                        };
                        let rc =
                            unsafe { libc::epoll_ctl(epoll, libc::EPOLL_CTL_ADD, reg.fd, &mut ev) };
                        if rc == 0 {
                            registered.insert(pid, reg.fd);
                        } else {
                            tracing::warn!(
                                error = %io::Error::last_os_error(),
                                "watchdog: epoll add pidfd for {pid} failed"
                            );
                        }
                    }
                }
                let stale: Vec<u32> = registered
                    .keys()
                    .copied()
                    .filter(|pid| !registry.contains_key(pid))
                    .collect();
                drop(registry);
                for pid in stale {
                    if let Some(fd) = registered.remove(&pid) {
                        epoll_ctl(epoll, libc::EPOLL_CTL_DEL, fd, 0);
                        unsafe {
                            libc::close(fd);
                        }
                    }
                }
            }

            let n =
                unsafe { libc::epoll_wait(epoll, events.as_mut_ptr(), events.len() as i32, -1) };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                tracing::error!(error = %err, "watchdog: epoll_wait failed");
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }

            for ev in &events[..n as usize] {
                // The wake pipe is registered with epoll data 0; pidfds carry
                // the pid. Real pids are never 0.
                if ev.u64 == 0 {
                    drain_fd(wake_read);
                    continue;
                }
                let pid = ev.u64 as u32;
                if let Some(mut reg) = inner.registry.lock().unwrap().remove(&pid) {
                    reg.bypass().store(true, Ordering::Relaxed);
                    reg.deregister(&inner.poller);
                }
                registered.remove(&pid);
            }
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::{Arc, AtomicBool, Inner, Ordering};
    use std::io;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Threading::{OpenProcess, SYNCHRONIZE, WaitForMultipleObjects};

    /// Registration holds the process handle waited on.
    pub struct Registration {
        handle: HANDLE,
        bypass: Arc<AtomicBool>,
    }

    impl Registration {
        pub fn new(pid: u32, bypass: Arc<AtomicBool>) -> Result<Self, io::Error> {
            let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) };
            if handle.is_invalid() {
                Err(io::Error::last_os_error())
            } else {
                Ok(Self { handle, bypass })
            }
        }

        pub fn bypass(&self) -> &AtomicBool {
            &self.bypass
        }

        pub fn deregister(&mut self, _poller: &Poller) {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }

    /// Auto-reset event used to wake the waiter when the registry changes.
    #[derive(Clone)]
    pub struct Wake {
        event: HANDLE,
    }

    impl Wake {
        pub fn wake(&self) {
            unsafe {
                let _ = windows::Win32::System::Threading::SetEvent(self.event);
            }
        }
    }

    #[derive(Clone)]
    pub struct Poller {
        wake_event: HANDLE,
    }

    impl Poller {
        pub fn new() -> Self {
            use windows::Win32::System::Threading::CreateEventW;
            let event = unsafe { CreateEventW(None, false, false, None) };
            assert!(!event.is_invalid(), "CreateEventW failed");
            Self { wake_event: event }
        }

        pub fn wake(&self) -> Wake {
            Wake {
                event: self.wake_event,
            }
        }
    }

    pub fn watcher_thread(inner: Arc<Inner>) {
        loop {
            // Snapshot up to 63 process handles (plus the wake event at index
            // 0). When more are registered the remainder is picked up on the
            // next pass after a handle fires or a wake arrives.
            let mut handles: Vec<HANDLE> = vec![inner.poller.wake_event];
            let mut pids: Vec<u32> = Vec::new();
            {
                let registry = inner.registry.lock().unwrap();
                for (&pid, reg) in registry.iter() {
                    if pids.len() == 63 {
                        break;
                    }
                    pids.push(pid);
                    handles.push(reg.handle);
                }
            }

            let n = unsafe { WaitForMultipleObjects(&handles, false, u32::MAX) } as usize;
            if n == 0 {
                // Wake event: registry changed; re-scan.
                continue;
            }
            if n >= handles.len() {
                continue;
            }
            let pid = pids[n - 1];
            if let Some(mut reg) = inner.registry.lock().unwrap().remove(&pid) {
                reg.bypass().store(true, Ordering::Relaxed);
                reg.deregister(&inner.poller);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    fn wait_for_flag(flag: &AtomicBool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while !flag.load(Ordering::Relaxed) {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "process facilities not supported by Miri on FreeBSD"
    )]
    #[test]
    fn watchdog_sets_flag_on_killed_child() {
        let wd = ProcessWatchdog::start();
        let mut child = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let flag = Arc::new(AtomicBool::new(false));
        wd.watch(child.id(), flag.clone());
        let _ = child.kill();
        assert!(
            wait_for_flag(&flag, Duration::from_secs(5)),
            "watchdog should set the bypass flag after the child is killed"
        );
        let _ = child.wait();
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "process facilities not supported by Miri on FreeBSD"
    )]
    #[test]
    fn watchdog_unwatched_clean_exit_does_not_set_flag() {
        let wd = ProcessWatchdog::start();
        let mut child = Command::new("sleep").arg("2").spawn().expect("spawn sleep");
        let flag = Arc::new(AtomicBool::new(false));
        wd.watch(child.id(), flag.clone());
        wd.unwatch(child.id());
        let _ = child.wait();
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !flag.load(Ordering::Relaxed),
            "flag must not be set for an unwatched child"
        );
    }
}
