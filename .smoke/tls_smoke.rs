// Throwaway smoke test: prove rustc can link versioned OpenSSL libs and that
// certificate verification actually rejects bad chains/names.
use std::ffi::{c_char, c_int, c_long, c_void, CString};
use std::net::TcpStream;
use std::os::unix::io::AsRawFd;
use std::ptr;

#[repr(C)]
struct SslCtx {
    _p: [u8; 0],
}
#[repr(C)]
struct Ssl {
    _p: [u8; 0],
}

#[link(name = ":libssl.so.3")]
extern "C" {
    fn OPENSSL_init_ssl(opts: u64, settings: *const c_void) -> c_int;
    fn TLS_client_method() -> *const c_void;
    fn SSL_CTX_new(method: *const c_void) -> *mut SslCtx;
    fn SSL_CTX_free(ctx: *mut SslCtx);
    fn SSL_CTX_set_default_verify_paths(ctx: *mut SslCtx) -> c_int;
    fn SSL_CTX_set_verify(ctx: *mut SslCtx, mode: c_int, cb: *const c_void);
    fn SSL_new(ctx: *mut SslCtx) -> *mut Ssl;
    fn SSL_free(ssl: *mut Ssl);
    fn SSL_set_fd(ssl: *mut Ssl, fd: c_int) -> c_int;
    fn SSL_ctrl(ssl: *mut Ssl, cmd: c_int, larg: c_long, parg: *mut c_void) -> c_long;
    fn SSL_set1_host(ssl: *mut Ssl, host: *const c_char) -> c_int;
    fn SSL_connect(ssl: *mut Ssl) -> c_int;
    fn SSL_read(ssl: *mut Ssl, buf: *mut c_void, num: c_int) -> c_int;
    fn SSL_write(ssl: *mut Ssl, buf: *const c_void, num: c_int) -> c_int;
    fn SSL_get_error(ssl: *const Ssl, ret: c_int) -> c_int;
    fn SSL_get_verify_result(ssl: *const Ssl) -> c_long;
    fn SSL_shutdown(ssl: *mut Ssl) -> c_int;
}

const SSL_VERIFY_PEER: c_int = 1;
// Confirmed against openssl-3.5.5 include/openssl/ssl.h.in:1265
const SSL_CTRL_SET_TLSEXT_HOSTNAME: c_int = 55;
// Confirmed against openssl-3.5.5 include/openssl/tls1.h:171
const TLSEXT_NAMETYPE_HOST_NAME: c_long = 0;
const SSL_ERROR_ZERO_RETURN: c_int = 6;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let host = args.get(1).map(String::as_str).unwrap_or("example.com");
    let expect_fail = args.get(2).map(String::as_str) == Some("fail");

    let stream = TcpStream::connect((host, 443)).expect("tcp connect");
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
    unsafe {
        assert_eq!(OPENSSL_init_ssl(0, ptr::null()), 1);
        let method = TLS_client_method();
        let ctx = SSL_CTX_new(method);
        assert!(!ctx.is_null(), "SSL_CTX_new");
        assert_eq!(SSL_CTX_set_default_verify_paths(ctx), 1, "trust store");
        SSL_CTX_set_verify(ctx, SSL_VERIFY_PEER, ptr::null());
        let ssl = SSL_new(ctx);
        assert!(!ssl.is_null(), "SSL_new");
        assert_eq!(SSL_set_fd(ssl, stream.as_raw_fd()), 1, "SSL_set_fd");
        let cname = CString::new(host).unwrap();
        assert_eq!(SSL_set1_host(ssl, cname.as_ptr()), 1, "SSL_set1_host");
        assert!(
            SSL_ctrl(
                ssl,
                SSL_CTRL_SET_TLSEXT_HOSTNAME,
                TLSEXT_NAMETYPE_HOST_NAME,
                cname.as_ptr() as *mut c_void
            ) > 0,
            "SNI"
        );
        let rc = SSL_connect(ssl);
        if rc != 1 {
            let err = SSL_get_error(ssl, rc);
            let verify = SSL_get_verify_result(ssl);
            SSL_free(ssl);
            SSL_CTX_free(ctx);
            if expect_fail {
                println!("OK rejected host={host} ssl_error={err} verify={verify}");
                return;
            }
            panic!("SSL_connect failed host={host}: ssl_error={err} verify={verify}");
        }
        if expect_fail {
            SSL_shutdown(ssl);
            SSL_free(ssl);
            SSL_CTX_free(ctx);
            panic!("expected TLS rejection but connected to {host}");
        }
        let req = format!(
            "GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nAccept-Encoding: identity\r\n\r\n"
        );
        assert_eq!(
            SSL_write(ssl, req.as_ptr() as *const c_void, req.len() as c_int),
            req.len() as c_int
        );
        let mut buf = [0u8; 4096];
        let mut total = 0usize;
        loop {
            let n = SSL_read(ssl, buf.as_mut_ptr() as *mut c_void, buf.len() as c_int);
            if n > 0 {
                total += n as usize;
                continue;
            }
            let err = SSL_get_error(ssl, n);
            if err == SSL_ERROR_ZERO_RETURN || n == 0 {
                break;
            }
            panic!("SSL_read failed: {err}");
        }
        SSL_shutdown(ssl);
        SSL_free(ssl);
        SSL_CTX_free(ctx);
        println!("OK verified host={host} bytes={total}");
    }
}
