//! Verified TLS client over the vendored Mbed TLS shim. Certificate and
//! hostname verification are always on; the trust store is embedded.

use std::ffi::{c_char, c_int, c_uint, CStr, CString};
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

#[repr(C)]
struct PaigTls {
    _p: [u8; 0],
}

unsafe extern "C" {
    fn paig_tls_global_init(
        ca: *const u8,
        ca_len: usize,
        err: *mut c_char,
        err_len: usize,
    ) -> c_int;
    fn paig_tls_new(
        fd: c_int,
        host: *const c_char,
        err: *mut c_char,
        err_len: usize,
        verify: *mut c_uint,
    ) -> *mut PaigTls;
    fn paig_tls_read(tls: *mut PaigTls, buf: *mut u8, len: usize) -> c_int;
    fn paig_tls_write(tls: *mut PaigTls, buf: *const u8, len: usize) -> c_int;
    fn paig_tls_last_error(tls: *mut PaigTls, buf: *mut c_char, len: usize) -> c_int;
    fn paig_tls_free(tls: *mut PaigTls);
}

const PAIG_TLS_TIMEOUT: c_int = -1;
const PAIG_TLS_WANT_READ: c_int = -2;
const PAIG_TLS_WANT_WRITE: c_int = -3;

pub(crate) static CA_BUNDLE: &[u8] = include_bytes!("../../vendor/mbedtls/ca-bundle.pem");

fn c_text(buf: &[c_char]) -> String {
    if buf[0] == 0 {
        return String::new();
    }
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

fn ensure_global() -> Result<(), String> {
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| {
        let mut err = [0 as c_char; 512];
        let rc = unsafe {
            paig_tls_global_init(
                CA_BUNDLE.as_ptr(),
                CA_BUNDLE.len(),
                err.as_mut_ptr(),
                err.len(),
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            let message = c_text(&err);
            Err(if message.is_empty() {
                "cannot initialize TLS".to_string()
            } else {
                message
            })
        }
    })
    .clone()
}

fn tls_error(tls: *mut PaigTls, fallback: &str) -> io::Error {
    let mut buf = [0 as c_char; 512];
    let n = unsafe { paig_tls_last_error(tls, buf.as_mut_ptr(), buf.len()) };
    let message = if n > 0 {
        c_text(&buf)
    } else {
        fallback.to_string()
    };
    io::Error::new(io::ErrorKind::Other, message)
}

fn pick_error(
    tls: Option<io::Error>,
    connect: Option<io::Error>,
    host: &str,
    port: u16,
) -> io::Error {
    tls.or(connect).unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::Other,
            format!("cannot connect to {host}:{port}"),
        )
    })
}

pub struct TlsStream {
    tls: *mut PaigTls,
    _stream: TcpStream,
}

unsafe impl Send for TlsStream {}

impl TlsStream {
    pub fn connect(host: &str, port: u16, timeout: Duration) -> io::Result<Self> {
        ensure_global().map_err(|message| io::Error::new(io::ErrorKind::Other, message))?;
        let cname = CString::new(host)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "host contains NUL"))?;
        let mut last_connect_error = None;
        let mut last_tls_error = None;
        for addr in (host, port).to_socket_addrs()? {
            let stream = match TcpStream::connect_timeout(&addr, timeout) {
                Ok(stream) => stream,
                Err(error) => {
                    last_connect_error = Some(error);
                    continue;
                }
            };
            let _ = stream.set_nodelay(true);
            stream.set_read_timeout(Some(timeout))?;
            stream.set_write_timeout(Some(timeout))?;

            let mut err = [0 as c_char; 512];
            let mut verify: c_uint = 0;
            let tls = unsafe {
                paig_tls_new(
                    stream.as_raw_fd(),
                    cname.as_ptr(),
                    err.as_mut_ptr(),
                    err.len(),
                    &mut verify,
                )
            };
            if !tls.is_null() {
                return Ok(TlsStream {
                    tls,
                    _stream: stream,
                });
            }
            let message = c_text(&err);
            let message = if message.is_empty() {
                format!("TLS handshake failed for {host} ({addr})")
            } else {
                format!("TLS handshake failed for {host} ({addr}): {message}")
            };
            last_tls_error = Some(io::Error::new(io::ErrorKind::Other, message));
        }
        Err(pick_error(
            last_tls_error,
            last_connect_error,
            host,
            port,
        ))
    }

    pub fn set_socket_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self._stream.set_read_timeout(timeout)?;
        self._stream.set_write_timeout(timeout)
    }

    pub(crate) fn raw_fd(&self) -> RawFd {
        self._stream.as_raw_fd()
    }
}

impl Drop for TlsStream {
    fn drop(&mut self) {
        unsafe { paig_tls_free(self.tls) };
    }
}

/// Mbed TLS returns WANT_READ/WANT_WRITE when the transport is not ready and
/// expects the caller to retry with the same arguments. A blocking socket can
/// produce these repeatedly while the peer is quiet, so bound the retry loop
/// by wall clock instead of a strike count.
const WANT_RETRY_BUDGET: Duration = Duration::from_secs(60);

fn retry_want<F>(budget: Duration, mut call: F) -> Result<c_int, ()>
where
    F: FnMut() -> c_int,
{
    // Start the clock only once the transport actually asks for a retry. The
    // first result is the hot path; paying a clock read per TLS read/write
    // costs more than the retry budget it protects against.
    let mut started = None;
    loop {
        let rc = call();
        if rc != PAIG_TLS_WANT_READ && rc != PAIG_TLS_WANT_WRITE {
            return Ok(rc);
        }
        let start = *started.get_or_insert_with(Instant::now);
        if start.elapsed() >= budget {
            return Err(());
        }
    }
}

impl Read for TlsStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let len = buf.len().min(c_int::MAX as usize);
        let rc = retry_want(WANT_RETRY_BUDGET, || unsafe {
            paig_tls_read(self.tls, buf.as_mut_ptr(), len)
        })
        .map_err(|()| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "TLS read retry budget exhausted",
            )
        })?;
        match rc {
            n if n > 0 => Ok(n as usize),
            0 => Ok(0),
            PAIG_TLS_TIMEOUT => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "TLS read timed out",
            )),
            _ => Err(tls_error(self.tls, "TLS read failed")),
        }
    }
}

impl Write for TlsStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let len = buf.len().min(c_int::MAX as usize);
        let rc = retry_want(WANT_RETRY_BUDGET, || unsafe {
            paig_tls_write(self.tls, buf.as_ptr(), len)
        })
        .map_err(|()| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "TLS write retry budget exhausted",
            )
        })?;
        match rc {
            n if n > 0 => Ok(n as usize),
            0 => Ok(0),
            PAIG_TLS_TIMEOUT => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "TLS write timed out",
            )),
            _ => Err(tls_error(self.tls, "TLS write failed")),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ensure_global, pick_error, retry_want, PAIG_TLS_WANT_READ, PAIG_TLS_WANT_WRITE,
    };
    use std::io;
    use std::time::{Duration, Instant};

    #[test]
    fn embedded_ca_bundle_parses() {
        ensure_global().expect("embedded CA bundle must parse with zero failures");
    }

    #[test]
    fn embedded_ca_bundle_has_expected_count() {
        let marker = b"-----BEGIN CERTIFICATE-----";
        let count = super::CA_BUNDLE
            .windows(marker.len())
            .filter(|window| *window == marker)
            .count();
        assert_eq!(count, 118);
    }

    #[test]
    fn tls_error_wins_over_connect_error() {
        let tls = io::Error::new(io::ErrorKind::Other, "handshake failed");
        let connect = io::Error::from_raw_os_error(99);
        let picked = pick_error(Some(tls), Some(connect), "example.com", 443);
        assert_eq!(picked.to_string(), "handshake failed");
    }

    #[test]
    fn connect_error_is_used_without_tls_error() {
        let picked = pick_error(None, Some(io::Error::from_raw_os_error(99)), "example.com", 443);
        assert_eq!(picked.raw_os_error(), Some(99));
    }

    #[test]
    fn fallback_error_names_host_and_port() {
        let picked = pick_error(None, None, "example.com", 443);
        assert!(picked.to_string().contains("example.com:443"));
    }

    #[test]
    fn wants_retry_past_the_old_strike_limit() {
        let mut calls = 0;
        let rc = retry_want(Duration::from_secs(1), || {
            calls += 1;
            match calls {
                1 | 3 | 5 | 7 => PAIG_TLS_WANT_READ,
                2 | 4 | 6 | 8 => PAIG_TLS_WANT_WRITE,
                _ => 42,
            }
        })
        .expect("repeated WANT must keep retrying");
        assert_eq!(rc, 42);
        assert_eq!(calls, 9);
    }

    #[test]
    fn retry_budget_bounds_a_persistent_want() {
        let started = Instant::now();
        let result = retry_want(Duration::from_millis(10), || {
            std::thread::sleep(Duration::from_millis(2));
            PAIG_TLS_WANT_READ
        });
        assert!(result.is_err());
        assert!(started.elapsed() >= Duration::from_millis(10));
    }

    #[test]
    fn non_want_result_returns_immediately() {
        let mut calls = 0;
        let rc = retry_want(Duration::from_secs(1), || {
            calls += 1;
            7
        })
        .expect("plain result");
        assert_eq!(rc, 7);
        assert_eq!(calls, 1);
    }
}
