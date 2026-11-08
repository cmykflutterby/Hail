use super::{is_block_uevent, ChangeSource};
use anyhow::{Context, Result};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

/// Waits on two kernel notification channels:
/// - `/proc/self/mountinfo` raises POLLPRI whenever the mount table changes
///   (mount, unmount, remount), which is exactly when the volume list changes;
/// - a `NETLINK_KOBJECT_UEVENT` socket reports block-device add/remove/change,
///   catching drives pulled without being unmounted.
///
/// Neither needs root or a udev daemon.
pub struct OsEventSource {
    mounts: File,
    uevents: Option<OwnedFd>,
}

fn open_uevent_socket() -> Result<OwnedFd> {
    // SAFETY: plain socket(2) call; the result is checked before use.
    let raw = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            libc::NETLINK_KOBJECT_UEVENT,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error()).context("creating uevent netlink socket");
    }
    // SAFETY: `raw` is a freshly created descriptor owned by nobody else.
    let socket = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: zero is a valid sockaddr_nl; fields are set below.
    let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    address.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    // Kernel uevent multicast group.
    address.nl_groups = 1;
    // SAFETY: address points to a valid sockaddr_nl of the stated size.
    let bound = unsafe {
        libc::bind(
            socket.as_raw_fd(),
            (&address as *const libc::sockaddr_nl).cast(),
            size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if bound < 0 {
        return Err(io::Error::last_os_error()).context("binding uevent netlink socket");
    }
    Ok(socket)
}

impl OsEventSource {
    pub fn new() -> Result<Self> {
        let mounts = File::open("/proc/self/mountinfo").context("opening /proc/self/mountinfo")?;
        let uevents = match open_uevent_socket() {
            Ok(socket) => Some(socket),
            Err(error) => {
                eprintln!(
                    "hail: kernel uevents unavailable ({error:#}); using mount-table notifications only"
                );
                None
            }
        };
        Ok(Self { mounts, uevents })
    }

    fn drain_uevents(socket: &OwnedFd) -> Result<bool> {
        let mut block = false;
        let mut buffer = [0u8; 8192];
        loop {
            // SAFETY: buffer is valid for its full length.
            let read = unsafe {
                libc::recv(
                    socket.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if read >= 0 {
                block |= is_block_uevent(&buffer[..read as usize]);
                continue;
            }
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EAGAIN) => return Ok(block),
                Some(libc::EINTR) => continue,
                // Receive queue overflowed: events were lost, so force a rescan.
                Some(libc::ENOBUFS) => block = true,
                _ => return Err(error).context("reading kernel uevents"),
            }
        }
    }
}

impl ChangeSource for OsEventSource {
    fn wait(&mut self, timeout: Option<Duration>) -> Result<bool> {
        let timeout_ms = match timeout {
            None => -1,
            // Round up so a sub-millisecond remainder does not busy-loop.
            Some(timeout) => timeout.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32,
        };
        let mut fds = vec![libc::pollfd {
            fd: self.mounts.as_raw_fd(),
            events: libc::POLLPRI,
            revents: 0,
        }];
        if let Some(socket) = &self.uevents {
            fds.push(libc::pollfd {
                fd: socket.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        // SAFETY: fds is a valid array of pollfd of the stated length.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(error).context("waiting for kernel notifications");
        }
        let mut changed = fds[0].revents & (libc::POLLPRI | libc::POLLERR) != 0;
        if let (Some(socket), Some(entry)) = (&self.uevents, fds.get(1)) {
            if entry.revents != 0 {
                changed |= Self::drain_uevents(socket)?;
            }
        }
        Ok(changed)
    }
}
