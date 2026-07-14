//! Reactive, platform-specific process-exit observation.
//!
//! Callers arm a watcher before requesting or signaling shutdown, then reuse
//! that same owned watcher across graceful and forced-stop deadlines. The
//! watcher observes exit but does not itself establish signaling authority or
//! process identity.

#[cfg(test)]
#[path = "process_exit_tests.rs"]
mod tests;

use std::io;
use std::time::{Duration, Instant};

/// The result of attempting to arm a process-exit watcher.
#[derive(Debug)]
pub(crate) enum WatchState {
    /// The process is alive and its exit can be observed.
    Watching(ProcessExitWatcher),
    /// The process had already exited when observation was armed.
    AlreadyExited,
}

/// The result of waiting for an observed process to exit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WaitOutcome {
    /// The observed process exited.
    Exited,
    /// The deadline elapsed while the process remained alive.
    TimedOut,
}

/// An owned, reusable handle that reactively observes one process.
#[derive(Debug)]
pub(crate) struct ProcessExitWatcher {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pidfd: std::os::fd::OwnedFd,
    #[cfg(target_os = "macos")]
    kqueue: std::os::fd::OwnedFd,
    #[cfg(target_os = "macos")]
    pid: libc::pid_t,
}

impl ProcessExitWatcher {
    /// Arm an exit watcher for a positive process ID.
    pub(crate) fn arm(pid: libc::pid_t) -> io::Result<WatchState> {
        if pid <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "process ID must be positive",
            ));
        }

        Self::new_platform(pid)
    }

    /// Wait reactively until the process exits or `timeout` elapses.
    pub(crate) fn wait(&self, timeout: Duration) -> io::Result<WaitOutcome> {
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "process-exit timeout exceeds the monotonic clock range",
            )
        })?;
        self.wait_platform(Some(deadline))
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn new_platform(pid: libc::pid_t) -> io::Result<WatchState> {
        use std::os::fd::FromRawFd;

        // SAFETY: pidfd_open has no pointer arguments. A successful descriptor is
        // immediately transferred to OwnedFd, which closes it on every exit path.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0_u32) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::ESRCH) {
                Ok(WatchState::AlreadyExited)
            } else {
                Err(error)
            };
        }
        // SAFETY: `fd` is a newly-created, valid descriptor owned by this function.
        let pidfd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as libc::c_int) };
        Ok(WatchState::Watching(Self { pidfd }))
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn wait_platform(&self, deadline: Option<Instant>) -> io::Result<WaitOutcome> {
        use std::os::fd::AsRawFd;

        loop {
            let timeout_ms = remaining_poll_millis(deadline);
            let mut event = libc::pollfd {
                fd: self.pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: `event` points to one initialized pollfd for the duration of
            // this call, and its descriptor remains owned by `self`.
            let result = unsafe { libc::poll(&mut event, 1, timeout_ms) };
            if result == 0 {
                if deadline.is_none_or(|value| Instant::now() >= value) {
                    return Ok(WaitOutcome::TimedOut);
                }
                continue;
            }
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }

            if event.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                return Err(io::Error::other(format!(
                    "pidfd poll returned error events {:#x}",
                    event.revents
                )));
            }
            if event.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                return Ok(WaitOutcome::Exited);
            }
            return Err(io::Error::other(format!(
                "pidfd poll returned unexpected events {:#x}",
                event.revents
            )));
        }
    }

    #[cfg(target_os = "macos")]
    fn new_platform(pid: libc::pid_t) -> io::Result<WatchState> {
        use std::os::fd::FromRawFd;
        use std::ptr;

        // SAFETY: kqueue takes no arguments and returns a new descriptor.
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a newly-created, valid descriptor owned by this function.
        let kqueue = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        let registration = libc::kevent {
            ident: pid as libc::uintptr_t,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: ptr::null_mut(),
        };
        // SAFETY: the registration points to one initialized event, the output
        // list is empty, and the owned kqueue descriptor remains open.
        let result = unsafe {
            libc::kevent(
                std::os::fd::AsRawFd::as_raw_fd(&kqueue),
                &registration,
                1,
                ptr::null_mut(),
                0,
                ptr::null(),
            )
        };
        if result < 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::ESRCH) {
                Ok(WatchState::AlreadyExited)
            } else {
                Err(error)
            };
        }
        Ok(WatchState::Watching(Self { kqueue, pid }))
    }

    #[cfg(target_os = "macos")]
    fn wait_platform(&self, deadline: Option<Instant>) -> io::Result<WaitOutcome> {
        use std::mem::MaybeUninit;
        use std::os::fd::AsRawFd;

        loop {
            let timeout = remaining_timespec(deadline);
            let mut event = MaybeUninit::<libc::kevent>::uninit();
            // SAFETY: `event` provides space for one result, the change list is
            // empty, and `timeout` remains alive for the duration of the call.
            let result = unsafe {
                libc::kevent(
                    self.kqueue.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    event.as_mut_ptr(),
                    1,
                    &timeout,
                )
            };
            if result == 0 {
                if deadline.is_none_or(|value| Instant::now() >= value) {
                    return Ok(WaitOutcome::TimedOut);
                }
                continue;
            }
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }

            // SAFETY: kevent returned one initialized output event.
            let event = unsafe { event.assume_init() };
            if event.flags & libc::EV_ERROR != 0 {
                return Err(io::Error::from_raw_os_error(event.data as i32));
            }
            if event.ident != self.pid as libc::uintptr_t
                || event.filter != libc::EVFILT_PROC
                || event.fflags & libc::NOTE_EXIT == 0
            {
                return Err(io::Error::other("kqueue returned an unexpected event"));
            }
            return Ok(WaitOutcome::Exited);
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn remaining_poll_millis(deadline: Option<Instant>) -> libc::c_int {
    let Some(remaining) = deadline.and_then(|value| value.checked_duration_since(Instant::now()))
    else {
        return 0;
    };
    if remaining.is_zero() {
        0
    } else {
        remaining.as_millis().max(1).min(libc::c_int::MAX as u128) as libc::c_int
    }
}

#[cfg(target_os = "macos")]
fn remaining_timespec(deadline: Option<Instant>) -> libc::timespec {
    let remaining = deadline
        .and_then(|value| value.checked_duration_since(Instant::now()))
        .unwrap_or_default();
    libc::timespec {
        tv_sec: remaining.as_secs().min(libc::time_t::MAX as u64) as libc::time_t,
        tv_nsec: remaining.subsec_nanos().into(),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
compile_error!("process-exit observation is supported only on Linux, Android, and macOS");
