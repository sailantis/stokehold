//! Transports a FastCGI client can use to reach a backend.
//!
//! `php-fpm` is normally reached over a unix socket, `php-cgi -b` over loopback
//! TCP (its only option on Windows). Both are exposed behind one type so the
//! client above it does not care which it is talking to.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::error::{FcgiError, Result};

/// A connected backend, over either transport.
#[derive(Debug)]
pub enum Stream {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
}

/// A parsed backend address: either `host:port` or `unix:/path`.
///
/// The `Unix` variant only exists where unix sockets do, so no code path can
/// construct one on Windows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Tcp(String),
    #[cfg(unix)]
    Unix(String),
}

impl Endpoint {
    /// Parse `host:port` or `unix:/path`.
    ///
    /// The `unix:` prefix is stokehold's own convention, matching the
    /// configuration surface (`socket = "unix:/run/php/php8.3-fpm.sock"`).
    pub fn parse(spec: &str) -> Result<Self> {
        let spec = spec.trim();
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
                    format!("unix sockets are not available on this platform: {spec}"),
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
                // adds delay to the header/PARAMS/STDIN sequence.
                let _ = stream.set_nodelay(true);
                Ok(Stream::Tcp(stream))
            }
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
        match self {
            Stream::Tcp(stream) => stream.read(buf),
            #[cfg(unix)]
            Stream::Unix(stream) => stream.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Stream::Tcp(stream) => stream.write(buf),
            #[cfg(unix)]
            Stream::Unix(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Stream::Tcp(stream) => stream.flush(),
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
    fn connect_to_a_closed_port_fails_cleanly() {
        // Port 1 on loopback is not listening; this must be an error, not a panic.
        let endpoint = Endpoint::Tcp("127.0.0.1:1".to_string());
        assert!(endpoint.connect(Some(Duration::from_millis(200))).is_err());
    }
}
