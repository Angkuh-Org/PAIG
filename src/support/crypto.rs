//! Mbed TLS primitives outside the TLS stream: SHA-256 for thread/session ids
//! and the shared CTR_DRBG for request ids.

use std::ffi::c_int;

unsafe extern "C" {
    fn paig_sha256(data: *const u8, len: usize, out: *mut u8) -> c_int;
    fn paig_random(buf: *mut u8, len: usize) -> c_int;
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    unsafe {
        paig_sha256(data.as_ptr(), data.len(), out.as_mut_ptr());
    }
    out
}

pub fn random_bytes(buf: &mut [u8]) -> bool {
    if buf.is_empty() {
        return true;
    }
    unsafe { paig_random(buf.as_mut_ptr(), buf.len()) == 0 }
}

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    if !random_bytes(&mut buf) {
        // RNG failure is effectively fatal; fall back to a time-derived value
        // so an error path still has a unique-ish id.
        let now = crate::util::now_nanos();
        for (index, slot) in buf.iter_mut().enumerate() {
            *slot = (now >> ((index % 8) * 8)) as u8 ^ index as u8;
        }
    }
    let mut out = String::with_capacity(bytes * 2);
    for byte in buf {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{random_hex, sha256};

    #[test]
    fn sha256_matches_known_vector() {
        let digest = sha256(b"abc");
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn random_hex_has_requested_width() {
        let value = random_hex(16);
        assert_eq!(value.len(), 32);
        assert!(value.chars().all(|ch| ch.is_ascii_hexdigit()));
    }
}
