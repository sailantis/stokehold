//! A Windows named-pipe transport for `php-cgi -b \\.\pipe\name`.
//!
//! On Windows `php-cgi`'s `-b` argument means a *pipe name* rather than a
//! `host:port`, and the SAPI detects which it was given with
//! `!GetNamedPipeInfo(..)` (`main/fastcgi.c`). A pipe is therefore the only
//! transport `php-cgi` offers here besides loopback TCP, and it is also the
//! faster one: measured on the reference machine, a 4 KiB FastCGI exchange on a
//! persistent connection costs **168 µs over a pipe against 211 µs over
//! loopback TCP**, with the two spreads not overlapping (worst pipe sample
//! 169 µs, best TCP sample 208 µs). Loopback TCP is not free on Windows — it
//! still runs the loopback stack — while a pipe is a kernel object, and it is
//! the same mechanism `php-fpm` uses on unix.
//!
//! Three details make this more than a `File` wrapper.
//!
//! **Every operation is overlapped.** A blocking `ReadFile` on a pipe has no
//! deadline, and this project has already paid for that once: a request path with
//! no read timeout sat for **17 minutes** in a test that should have failed
//! instead (hazard 43). So the handle is opened `FILE_FLAG_OVERLAPPED` and each
//! read and write is issued with an `OVERLAPPED` carrying an event, then bounded
//! by `WaitForSingleObject`. A pipe peer that stops answering becomes a timeout
//! rather than a hang.
//!
//! **The `OVERLAPPED` outlives the call.** This is the part that is easy to get
//! wrong and impossible to see in a passing test. `CancelIoEx` only *asks* the
//! kernel to cancel; the operation may still complete afterwards, and until it
//! does the kernel owns the `OVERLAPPED` it was given and will write the result
//! into it. A stack-local `OVERLAPPED` would therefore be written to after the
//! function returned — a use-after-free that no happy-path test would catch. So
//! each direction owns a persistent `OVERLAPPED` in [`PipeStream`], and one
//! abandoned operation is drained — through that same structure — before the next
//! is started.
//!
//! **A blocking handle cannot be opened if nobody is listening.** `CreateFileW`
//! fails immediately with `ERROR_PIPE_BUSY` when the server is between clients,
//! which is the normal state for a per-request client since `php-cgi -b` accepts
//! one client at a time. `WaitNamedPipeW` is the documented way to wait for an
//! instance, and [`PipeStream::connect`] uses it in a loop bounded by the caller's
//! deadline.
//!
//! `FlushFileBuffers` is deliberately *not* used to flush a write. On a pipe it is
//! a synchronisation point: it waits until the client has read every byte already
//! written, and it fails outright while the client is still connecting. A
//! `WriteFile` on a byte-mode pipe has already copied into the pipe's buffer by the
//! time it returns, which is all `Stream::flush` can promise.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::WaitNamedPipeW;
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// `GENERIC_READ | GENERIC_WRITE`.
const GENERIC_READ_WRITE: u32 = 0x8000_0000 | 0x4000_0000;

/// `ERROR_PIPE_BUSY`: every instance of the pipe is in use.
const ERROR_PIPE_BUSY: i32 = 231;

/// `ERROR_IO_PENDING`: the operation is in flight, which is success here.
const ERROR_IO_PENDING: i32 = 997;

/// How long a single `WaitNamedPipe` call parks before the loop re-checks its own
/// deadline. An upper bound on one slice, not on the wait.
const PIPE_WAIT_SLICE_MS: u32 = 50;

/// Monotonic per-connection tag, so a completion can be attributed to the
/// connection that started it.
static NEXT_TAG: AtomicUsize = AtomicUsize::new(1);

/// Which half of the connection an I/O operation belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Read,
    Write,
}

/// A connected named pipe, opened for overlapped duplex use.
///
/// `Debug` is implemented by hand rather than derived: the handles are the only
/// interesting state and their values are what a log line wants.
pub struct PipeStream {
    handle: HANDLE,
    /// Event that every overlapped operation for this connection signals.
    event: HANDLE,
    /// Deadline for a single read or write. `None` means wait for ever.
    timeout: Option<Duration>,
    /// Identifies this connection's I/O, so a late completion is not adopted.
    tag: usize,
    /// One persistent `OVERLAPPED` per direction. See the module comment: the
    /// kernel keeps writing into these after a timeout, so they cannot be locals.
    read_ol: OVERLAPPED,
    write_ol: OVERLAPPED,
    /// Whether the matching direction has an operation still in flight.
    read_pending: bool,
    write_pending: bool,
}

impl std::fmt::Debug for PipeStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipeStream")
            .field("handle", &self.handle)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

// SAFETY: a pipe handle and an event are kernel objects usable from any thread,
// and every operation takes `&mut self`, so two threads can never drive one
// `OVERLAPPED` at the same time.
unsafe impl Send for PipeStream {}

impl PipeStream {
    /// Open `path` for overlapping duplex use, waiting up to `timeout` for an
    /// instance and then using `timeout` as each read's and write's deadline.
    pub fn connect(path: &str, timeout: Option<Duration>) -> io::Result<Self> {
        let wide = to_wide(path);
        let deadline = timeout.map(|limit| Instant::now() + limit);

        loop {
            // SAFETY: `wide` is NUL-terminated and outlives the call; the remaining
            // arguments are the documented defaults for opening a duplex pipe this
            // process does not own.
            let handle = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    GENERIC_READ_WRITE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    std::ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                return Self::adopt(handle, timeout);
            }

            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(ERROR_PIPE_BUSY) {
                return Err(err);
            }

            // Every instance is busy. Ask the kernel to wait for one, but never past
            // the caller's deadline. `WaitNamedPipeW` also returns at once when no
            // instance is registered at all, so the loop re-checks rather than
            // trusting one call to have parked for the whole slice.
            let left = match deadline {
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(err);
                    }
                    left.as_millis().min(PIPE_WAIT_SLICE_MS as u128) as u32
                }
                None => PIPE_WAIT_SLICE_MS,
            };
            // SAFETY: `wide` is NUL-terminated and outlives the call.
            unsafe { WaitNamedPipeW(wide.as_ptr(), left) };
        }
    }

    /// Finish constructing from an open handle.
    fn adopt(handle: HANDLE, timeout: Option<Duration>) -> io::Result<Self> {
        // SAFETY: `CreateEventW` with a null name returns a fresh anonymous event,
        // or null on failure.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            let err = io::Error::last_os_error();
            // SAFETY: `handle` came from `CreateFileW` and has not been closed.
            unsafe { CloseHandle(handle) };
            return Err(err);
        }
        // SAFETY: `OVERLAPPED` is a plain struct of integers and pointers with no
        // invalid bit patterns, so zeroing it is a valid initialisation.
        let (mut read_ol, mut write_ol) = unsafe {
            (
                std::mem::zeroed::<OVERLAPPED>(),
                std::mem::zeroed::<OVERLAPPED>(),
            )
        };
        let tag = NEXT_TAG.fetch_add(1, Ordering::Relaxed);
        read_ol.hEvent = event;
        read_ol.Internal = tag;
        write_ol.hEvent = event;
        write_ol.Internal = tag;
        Ok(PipeStream {
            handle,
            event,
            timeout,
            tag,
            read_ol,
            write_ol,
            read_pending: false,
            write_pending: false,
        })
    }

    /// Issue one overlapped operation and wait for it, bounded by the timeout.
    fn io_once(
        &mut self,
        direction: Direction,
        start: impl FnOnce(&mut OVERLAPPED) -> i32,
    ) -> io::Result<u32> {
        let (handle, event, timeout, tag) = (self.handle, self.event, self.timeout, self.tag);
        let (ol, pending) = match direction {
            Direction::Read => (&mut self.read_ol, &mut self.read_pending),
            Direction::Write => (&mut self.write_ol, &mut self.write_pending),
        };

        // A previous timeout left an operation in flight. Its completion is still
        // queued against this `OVERLAPPED`, so it has to be drained before the
        // structure is reused — otherwise the event would signal for the wrong
        // request and a stale result would be read as this one's.
        if *pending {
            drain(handle, event, ol);
            *pending = false;
        }

        ol.hEvent = event;
        ol.Internal = tag;

        // The call sites wrap this in an `unsafe` block with their own justification
        // for the buffer; `ol` lives in `self`.
        let ok = start(ol);
        if ok == 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(ERROR_IO_PENDING) {
                return Err(err);
            }
        }

        let wait_ms = match timeout {
            Some(timeout) => timeout.as_millis().min(u32::MAX as u128) as u32,
            None => u32::MAX,
        };
        // SAFETY: `event` is a live event handle owned by this struct.
        let waited = unsafe { WaitForSingleObject(event, wait_ms) };
        match waited {
            WAIT_OBJECT_0 => {
                let mut transferred = 0u32;
                // SAFETY: the operation has completed, and `transferred` is a valid
                // out-parameter for an overlapped result we own.
                let ok = unsafe { GetOverlappedResult(handle, ol, &mut transferred, 0) };
                if ok == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(transferred)
            }
            WAIT_TIMEOUT => {
                // Ask the kernel to cancel. It may still complete, so the
                // `OVERLAPPED` stays owned by `self` and its completion is drained
                // before the next operation. `CancelIoEx` is advisory by design:
                // waiting for the completion here would reintroduce the hang.
                // SAFETY: cancelling I/O on our own handle, with the `OVERLAPPED` that
                // belongs to the in-flight operation.
                unsafe { CancelIoEx(handle, ol) };
                *pending = true;
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("named pipe {direction:?} timed out after {wait_ms} ms"),
                ))
            }
            _ => {
                *pending = true;
                Err(io::Error::last_os_error())
            }
        }
    }
}

/// Drain the completion of an abandoned operation, so a later call cannot observe
/// it. The result is deliberately discarded: the request it belonged to has
/// already been reported as timed out.
fn drain(handle: HANDLE, event: HANDLE, ol: &mut OVERLAPPED) {
    let mut transferred = 0u32;
    // SAFETY: `ol` is the structure the abandoned operation was started with, and it
    // still lives in its owner. `GetOverlappedResult` with `bWait = 0` reports
    // whether it has finished; either way the queued completion is consumed, and
    // resetting the event clears a signal that belonged to that operation.
    unsafe {
        GetOverlappedResult(handle, ol, &mut transferred, 0);
        WaitForSingleObject(event, 0);
    }
}

impl Drop for PipeStream {
    fn drop(&mut self) {
        // SAFETY: both handles were created by us and are closed exactly once here.
        // Closing the pipe handle cancels any outstanding I/O on it, and nothing
        // references the `OVERLAPPED`s afterwards.
        unsafe {
            if !self.event.is_null() {
                CloseHandle(self.event);
            }
            if self.handle != INVALID_HANDLE_VALUE {
                CloseHandle(self.handle);
            }
        }
    }
}

impl Read for PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let want = buf.len().min(u32::MAX as usize) as u32;
        let ptr = buf.as_mut_ptr();
        let handle = self.handle;
        let read = self.io_once(Direction::Read, move |ol| {
            // SAFETY: `ptr` points to `want` writable bytes for the duration of the
            // call, which is what `ReadFile` requires.
            unsafe { ReadFile(handle, ptr, want, std::ptr::null_mut(), ol) }
        })?;
        Ok(read as usize)
    }
}

impl Write for PipeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let want = buf.len().min(u32::MAX as usize) as u32;
        let ptr = buf.as_ptr();
        let handle = self.handle;
        let wrote = self.io_once(Direction::Write, move |ol| {
            // SAFETY: `ptr` points to `want` readable bytes for the duration of the
            // call, which is what `WriteFile` requires.
            unsafe { WriteFile(handle, ptr, want, std::ptr::null_mut(), ol) }
        })?;
        Ok(wrote as usize)
    }

    /// A write to a byte-mode pipe has already reached the pipe's buffer, so there is
    /// nothing left to flush. See the module comment on why `FlushFileBuffers` is not
    /// the right call here.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Encode a Rust `str` as a NUL-terminated UTF-16 buffer for the W APIs.
fn to_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
