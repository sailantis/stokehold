//! Transports a FastCGI client can use to reach a backend.
//!
//! `php-fpm` is normally reached over a unix socket, `php-cgi -b` over loopback
//! TCP or a unix socket. On Windows `php-cgi -b` can bind neither: the same
//! argument means a *named pipe* there. All three are exposed behind one type so
//! the client above it does not care which it is talking to.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::error::{FcgiError, Result};
#[cfg(windows)]
use crate::pipe::PipeStream;

/// A connected backend, over any transport.
#[derive(Debug)]
pub enum Stream {
    Tcp(TcpStream),
    #[cfg(windows)]
    Pipe(PipeStream),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
}

/// Ask the kernel to stop delaying this connection's ACKs.
///
/// A Linux `php-cgi` child never sets `TCP_NODELAY`. `main/fastcgi.c` guards that
/// call with `req->keep && req->tcp`, and `req->tcp` is assigned only under
/// `#ifdef _WIN32` (line 897), so on Linux it keeps the zero `calloc` gave it and
/// the call is never reached. The child therefore runs with **Nagle enabled**.
///
/// That only matters because php-cgi flushes in 8 KiB chunks (`out_buf[1024*8]`).
/// A response larger than 8 KiB leaves a short final segment, and Nagle withholds
/// it until the previous data is acknowledged. The `FCGI_KEEP_CONN` connection stays
/// open, so Linux's ACK is a delayed ACK — 40 ms on this host. Because the withheld
/// tail is still outstanding when the *next* request begins, the stall is not a
/// one-off: it repeats for every request on that connection, and it is the reason a
/// 12 KB page measured a flat 42.0 ms per request over loopback TCP against 1.2 ms
/// over a unix socket.
///
/// `TCP_QUICKACK` is the receiver's lever: it disables delayed ACKs for the bytes
/// that follow, so the withheld segment is released at once. The kernel resets the
/// flag after it has been used (and after every `recv`), so it has to be re-armed
/// before each read rather than set once at connect time — see [`Stream::read`].
///
/// It takes effect from the *next* read, so arming it before reading a response is
/// timely: the ACKs being delayed are exactly the ones that read will generate.
///
/// A failure is not actionable — a non-TCP socket or a kernel without the option
/// reports `EINVAL` — and it is deliberately ignored rather than turned into a
/// request failure: correct delivery never depended on this, only latency.
#[cfg(target_os = "linux")]
fn arm_immediate_ack(fd: std::os::fd::RawFd) {
    // SAFETY: `fd` is an open socket owned by the caller for the duration of this
    // call, and `TCP_QUICKACK` takes an `int` argument, which is what is passed.
    unsafe {
        let one: libc::c_int = 1;
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_QUICKACK,
            std::ptr::addr_of!(one).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

/// A parsed backend address: `host:port`, `unix:/path`, or on Windows a pipe
/// name.
///
/// The `Unix` variant only exists where unix sockets do and `Pipe` only where
/// named pipes do, so no code path can construct either on the wrong platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Tcp(String),
    #[cfg(windows)]
    Pipe(String),
    #[cfg(unix)]
    Unix(String),
}

impl std::fmt::Display for Endpoint {
    /// The reverse of [`Endpoint::parse`], so an endpoint can travel through a
    /// log line, an error message or a config template and come back unchanged.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Endpoint::Tcp(spec) => write!(f, "{spec}"),
            #[cfg(windows)]
            Endpoint::Pipe(path) => write!(f, "{path}"),
            #[cfg(unix)]
            Endpoint::Unix(path) => write!(f, "unix:{path}"),
        }
    }
}

/// The prefix that marks a Windows pipe name, matching what `php-cgi -b` takes.
#[cfg(windows)]
pub const PIPE_PREFIX: &str = r"\\.\pipe\";

impl Endpoint {
    /// Parse `host:port`, `unix:/path`, or a Windows pipe name.
    ///
    /// The `unix:` prefix is volucer's own convention, matching the
    /// configuration surface (`socket = "unix:/run/php/php8.3-fpm.sock"`). A pipe
    /// needs no convention: `\\.\pipe\name` is what `php-cgi -b` is given
    /// verbatim, so a configuration can carry the same string to both.
    pub fn parse(spec: &str) -> Result<Self> {
        let spec = spec.trim();
        #[cfg(windows)]
        if spec.starts_with(PIPE_PREFIX) {
            return Ok(Endpoint::Pipe(spec.to_string()));
        }
        if let Some(path) = spec.strip_prefix("unix:") {
            if path.is_empty() {
                return Err(FcgiError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "unix socket path is empty",
                )));
            }
            #[cfg(not(unix))]
            {
                return Err(FcgiError::Io(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    format!(
                        "unix sockets are not available on this platform, so {spec:?} cannot be \
                         used; php-cgi's `-b` binds a named pipe here — write \
                         `{PIPE_PREFIX}name` instead"
                    ),
                )));
            }
            #[cfg(unix)]
            return Ok(Endpoint::Unix(path.to_string()));
        }
        if spec.is_empty() {
            return Err(FcgiError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "endpoint is empty",
            )));
        }
        Ok(Endpoint::Tcp(spec.to_string()))
    }

    /// Connect, with a timeout applied to both connect and subsequent reads.
    pub fn connect(&self, timeout: Option<Duration>) -> Result<Stream> {
        match self {
            Endpoint::Tcp(spec) => {
                let addr = resolve(spec)?;
                let stream = match timeout {
                    Some(timeout) => TcpStream::connect_timeout(&addr, timeout)?,
                    None => TcpStream::connect(addr)?,
                };
                stream.set_read_timeout(timeout)?;
                stream.set_write_timeout(timeout)?;
                // FastCGI requests are small and latency-sensitive; Nagle only
                // adds delay to the header/PARAMS/STDIN sequence. This is our own
                // sending side. The receiving side needs the other half of the
                // pair — see `arm_immediate_ack` for why, and for what a Linux
                // php-cgi backend does instead.
                let _ = stream.set_nodelay(true);
                Ok(Stream::Tcp(stream))
            }
            #[cfg(windows)]
            Endpoint::Pipe(path) => Ok(Stream::Pipe(PipeStream::connect(path, timeout)?)),
            #[cfg(unix)]
            Endpoint::Unix(path) => {
                let stream = std::os::unix::net::UnixStream::connect(path)?;
                stream.set_read_timeout(timeout)?;
                stream.set_write_timeout(timeout)?;
                Ok(Stream::Unix(stream))
            }
        }
    }
}

fn resolve(spec: &str) -> Result<SocketAddr> {
    spec.to_socket_addrs()?.next().ok_or_else(|| {
        FcgiError::Io(std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            format!("could not resolve {spec:?}"),
        ))
    })
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // Re-armed per read on purpose: the kernel clears `TCP_QUICKACK` after using
        // it (and after every `recv`), so a single call at connect time would cover
        // only the first response. See `arm_immediate_ack` for why this matters.
        #[cfg(target_os = "linux")]
        if let Stream::Tcp(stream) = self {
            use std::os::fd::AsRawFd;
            arm_immediate_ack(stream.as_raw_fd());
        }
        match self {
            Stream::Tcp(stream) => stream.read(buf),
            #[cfg(windows)]
            Stream::Pipe(stream) => stream.read(buf),
            #[cfg(unix)]
            Stream::Unix(stream) => stream.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Stream::Tcp(stream) => stream.write(buf),
            #[cfg(windows)]
            Stream::Pipe(stream) => stream.write(buf),
            #[cfg(unix)]
            Stream::Unix(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Stream::Tcp(stream) => stream.flush(),
            #[cfg(windows)]
            Stream::Pipe(stream) => stream.flush(),
            #[cfg(unix)]
            Stream::Unix(stream) => stream.flush(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn parses_a_unix_endpoint() {
        assert_eq!(
            Endpoint::parse("unix:/run/php/php8.3-fpm.sock").unwrap(),
            Endpoint::Unix("/run/php/php8.3-fpm.sock".to_string())
        );
    }

    #[test]
    fn rejects_an_empty_endpoint() {
        assert!(Endpoint::parse("").is_err());
        assert!(Endpoint::parse("unix:").is_err());
    }

    #[cfg(not(unix))]
    #[test]
    fn unix_sockets_are_reported_as_unsupported_on_windows() {
        let err = Endpoint::parse("unix:/run/php.sock").unwrap_err();
        assert!(err.to_string().contains("not available"), "{err}");
    }

    #[cfg(windows)]
    #[test]
    fn parses_a_named_pipe_endpoint() {
        // A pipe name is what `php-cgi -b` is given on Windows, so it has to parse
        // as itself rather than as `host:port`.
        assert_eq!(
            Endpoint::parse(r"\\.\pipe\volucer-cgi-0").unwrap(),
            Endpoint::Pipe(r"\\.\pipe\volucer-cgi-0".to_string())
        );
        assert_eq!(
            Endpoint::parse(r"  \\.\pipe\trimmed  ").unwrap(),
            Endpoint::Pipe(r"\\.\pipe\trimmed".to_string())
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_pipe_endpoint_round_trips_through_display() {
        // The string must survive a log line or an error message unchanged, because
        // it is also what the pool hands to `php-cgi -b`.
        for spec in [r"\\.\pipe\volucer-cgi-0", r"\\.\pipe\a"] {
            let endpoint = Endpoint::parse(spec).unwrap();
            assert_eq!(endpoint.to_string(), spec);
            assert_eq!(Endpoint::parse(&endpoint.to_string()).unwrap(), endpoint);
        }
    }

    #[test]
    fn parses_a_tcp_endpoint_and_trims_whitespace() {
        assert_eq!(
            Endpoint::parse("127.0.0.1:9000").unwrap(),
            Endpoint::Tcp("127.0.0.1:9000".to_string())
        );
        assert_eq!(
            Endpoint::parse(" 127.0.0.1:9000 ").unwrap(),
            Endpoint::Tcp("127.0.0.1:9000".to_string())
        );
    }

    #[test]
    fn an_endpoint_round_trips_through_display() {
        for spec in ["127.0.0.1:9000", " 192.0.2.1:1234 "] {
            let endpoint = Endpoint::parse(spec).unwrap();
            assert_eq!(
                Endpoint::parse(&endpoint.to_string()).unwrap(),
                endpoint,
                "Display must be the inverse of parse"
            );
        }
        #[cfg(unix)]
        {
            let endpoint = Endpoint::parse("unix:/run/php.sock").unwrap();
            assert_eq!(endpoint.to_string(), "unix:/run/php.sock");
            assert_eq!(Endpoint::parse(&endpoint.to_string()).unwrap(), endpoint);
        }
    }

    #[test]
    fn connect_to_a_closed_port_fails_cleanly() {
        // Port 1 on loopback is not listening; this must be an error, not a panic.
        let endpoint = Endpoint::Tcp("127.0.0.1:1".to_string());
        assert!(endpoint.connect(Some(Duration::from_millis(200))).is_err());
    }

    /// The option has to be accepted by a real TCP socket, and by a unix socket too.
    ///
    /// This does not prove the effect — `getsockopt(TCP_QUICKACK)` reads 1 both before
    /// and after setting it, because it reports the negation of the kernel's
    /// `icsk_ack.pingpong` and that is already 0 on a fresh socket either way. What it
    /// does cover is the mistake that would otherwise fail silently: a wrong option
    /// number, a wrong level, or a wrong argument size, all of which the kernel rejects
    /// with `EINVAL` and which `arm_immediate_ack` deliberately ignores. A `setsockopt`
    /// that returns -1 is invisible at every call site, so it is checked here instead.
    ///
    /// The effect itself is verified by measurement, not by a unit test: see hazard 30
    /// in `docs/plan.md` for the 42.005 ms -> 1.238 ms before/after on a real php-cgi.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_immediate_ack_option_is_accepted_by_a_real_socket() {
        use std::os::fd::AsRawFd;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let client = TcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");

        for (name, fd) in [("client", client.as_raw_fd()), ("server", server.as_raw_fd())] {
            let one: libc::c_int = 1;
            // SAFETY: both descriptors are open for the duration of the call, and
            // `TCP_QUICKACK` takes an `int` argument, which is what is passed.
            let rc = unsafe {
                libc::setsockopt(
                    fd,
                    libc::IPPROTO_TCP,
                    libc::TCP_QUICKACK,
                    std::ptr::addr_of!(one).cast(),
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
            assert_eq!(
                rc,
                0,
                "TCP_QUICKACK was rejected on the {name} socket: {}",
                std::io::Error::last_os_error()
            );
        }

        // And a unix socket must not be handed a TCP-level option: the arm function is
        // only called for `Stream::Tcp`, but proving `EINVAL` here is what keeps that
        // guard honest if the match arm is ever widened.
        let dir = std::env::temp_dir().join(format!("volucer-qa-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("s.sock");
        let bound = std::os::unix::net::UnixListener::bind(&path).expect("bind unix");
        let _peer = std::os::unix::net::UnixStream::connect(&path).expect("connect unix");
        let (accepted, _) = bound.accept().expect("accept unix");
        let one: libc::c_int = 1;
        // SAFETY: as above; the descriptor is open and the argument is an `int`.
        let rc = unsafe {
            libc::setsockopt(
                accepted.as_raw_fd(),
                libc::IPPROTO_TCP,
                libc::TCP_QUICKACK,
                std::ptr::addr_of!(one).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        assert_ne!(
            rc, 0,
            "a unix socket must reject a TCP-level option; if this ever succeeds, the \
             Stream::Tcp guard in arm_immediate_ack is not doing anything"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
