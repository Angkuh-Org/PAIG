//! Game rules for PAIG, enforced as code.
//!
//! The rulebook is intentionally a Rust module, not prose: constants here are
//! used by the server, the build gate reads the same numbers, and `--rules`
//! prints the current scorecard. A rule that cannot be checked is not a rule.

pub const PROJECT: &str = "PAIG";
pub const RULEBOOK_VERSION: u32 = 1;

/// G2: the only runtime libraries we are allowed to link are the C runtime and
/// glibc support libraries. TLS is vendored and static; libssl/libcrypto are
/// not sanctioned anymore.
pub const ALLOWED_RUNTIME_LIBS: &[&str] = &[
    "libc.so.6",
    "libgcc_s.so.1",
    "libm.so.6",
    "libpthread.so.0",
    "libdl.so.2",
    "librt.so.1",
];

/// G3: hard per-request/connection limits. Peak memory is not a global byte
/// budget: it scales with concurrent workers and request copies.
// TODO(paig): add a global in-flight byte budget instead of only per-request
// caps if RAM must be bounded under concurrent large bodies.
pub const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_HEADER_BYTES: usize = 64 * 1024;
pub const MAX_CONNECTIONS: usize = 256;
pub const MAX_IDLE_PER_HOST: usize = 4;
pub const THREAD_STACK_BYTES: usize = 256 * 1024;
pub const WORKER_THREADS: usize = 32;

/// G4: size target for the stripped release binary (1.5 MiB).
pub const TARGET_BINARY_BYTES: u64 = 1_572_864;
/// G5: server-side streaming must not accumulate the whole body.
pub const STREAM_FLUSH_BYTES: usize = 8 * 1024;

/// G6: default network identity is loopback only; binding wider is an explicit
/// config choice, never a default.
pub const DEFAULT_PORT: u16 = 8899;

const _: () = assert!(MAX_BODY_BYTES == 64 * 1024 * 1024);
const _: () = assert!(MAX_HEADER_BYTES < MAX_BODY_BYTES);
const _: () = assert!(MAX_CONNECTIONS >= WORKER_THREADS);
const _: () = assert!(THREAD_STACK_BYTES >= 128 * 1024);
const _: () = assert!(DEFAULT_PORT == 8899);

pub struct Rule {
    pub id: &'static str,
    pub title: &'static str,
    pub detail: &'static str,
}

pub const RULES: &[Rule] = &[
    Rule {
        id: "G1",
        title: "No cargo, no crates",
        detail: "Gateway: one rustc invocation plus the vendored Mbed TLS C archive.",
    },
    Rule {
        id: "G2",
        title: "Only sanctioned runtime libs",
        detail: "libc and glibc support libs; vendored Mbed TLS is linked statically.",
    },
    Rule {
        id: "G3",
        title: "Bounded memory",
        detail: "64 MiB body, 64 KiB headers, 256 connections, 256 KiB stacks.",
    },
    Rule {
        id: "G4",
        title: "Small binary",
        detail: "Stripped release target <= 1.5 MiB; the build gate warns on regression.",
    },
    Rule {
        id: "G5",
        title: "Stream, do not hoard",
        detail: "SSE is forwarded chunk by chunk; no full-response buffering.",
    },
    Rule {
        id: "G6",
        title: "Loopback by default",
        detail: "127.0.0.1:8899 unless config explicitly says otherwise.",
    },
    Rule {
        id: "G7",
        title: "Edge compatibility",
        detail: "OpenAI shape at /v1; backend quirks normalized per handler.",
    },
];

pub struct Level {
    pub number: u8,
    pub name: &'static str,
    pub mission: &'static str,
    pub boss: &'static str,
}

pub const LEVELS: &[Level] = &[
    Level {
        number: 1,
        name: "Cold Start",
        mission: "Compile with rustc only, serve /health and /v1/models.",
        boss: "paig-build",
    },
    Level {
        number: 2,
        name: "Wire Up",
        mission: "Config precedence, HTTP client, Command Code translation.",
        boss: "first real chat completion",
    },
    Level {
        number: 3,
        name: "Backends",
        mission: "OpenCode free/go, Kilo, Bob, Mistral routing.",
        boss: "model-prefix collision",
    },
    Level {
        number: 4,
        name: "Stream Discipline",
        mission: "SSE passthrough, usage/finish observation, ledger.",
        boss: "stalled upstream",
    },
    Level {
        number: 5,
        name: "Parity Boss",
        mission: "Full JS parity, mock-upstream tests, size/latency checks.",
        boss: "the oracle",
    },
];

/// Runtime half of G4: the build gate warns, this reports the actual binary.
pub fn scorecard() -> String {
    let size = std::env::current_exe()
        .ok()
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|meta| meta.len())
        .unwrap_or(0);
    let verdict = if size == 0 {
        "unknown"
    } else if size <= TARGET_BINARY_BYTES {
        "pass"
    } else {
        "over target"
    };
    format!(
        "binary: {size} bytes / {TARGET_BINARY_BYTES} target ({verdict}); libs: {}",
        ALLOWED_RUNTIME_LIBS.join(", ")
    )
}
