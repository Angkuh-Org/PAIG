//! PAIG build orchestrator. The gateway is still one rustc invocation; this
//! tool schedules it and enforces G1/G2/G4 from src/support/rules.rs.
//!
//! build.sh bootstraps this file with one rustc invocation when it is missing
//! or stale, then runs build/paig-build [command].

use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

#[allow(dead_code)]
#[path = "../src/support/rules.rs"]
mod rules;

const COMMON: &[&str] = &[
    "--edition",
    "2024",
    "-C",
    "opt-level=3",
    "-C",
    "lto=fat",
    "-C",
    "codegen-units=1",
    "-C",
    "panic=abort",
    "-C",
    "debug-assertions=off",
    "-C",
    "overflow-checks=off",
    "-C",
    "strip=symbols",
    "-C",
    "target-cpu=native",
    "-C",
    "link-arg=-Wl,--gc-sections",
    "-C",
    "link-arg=-Wl,--as-needed",
    // bfd rejects rustc's -nodefaultlibs ordering with the mbedtls archive
    // (__stack_chk_guard / DSO missing); lld links it and stays smaller.
    "-C",
    "link-arg=-fuse-ld=lld",
];

const MBEDTLS_CFLAGS: &[&str] = &[
    "-O2",
    "-march=native",
    "-fPIC",
    "-ffunction-sections",
    "-fdata-sections",
    "-fvisibility=hidden",
    "-fno-unwind-tables",
    "-fno-asynchronous-unwind-tables",
    "-DNDEBUG",
    "-DMBEDTLS_CONFIG_FILE=\"paig_config.h\"",
];

fn main() {
    let root = repo_root();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str);
    let result = match command {
        None => build(&root),
        Some("--check") => check(&root),
        Some("test") => test(&root, &args[1..]),
        Some("clean") => clean(&root),
        Some("-h") | Some("--help") | Some("help") => {
            usage();
            Ok(())
        }
        Some(other) => {
            eprintln!("paig-build: unknown command: {other}");
            usage();
            std::process::exit(2);
        }
    };
    if let Err(error) = result {
        eprintln!("paig-build: {error}");
        std::process::exit(1);
    }
}

fn usage() {
    println!(
        "usage: paig-build [--check | test | clean]\n\n\
         \x20 (none)       incremental release build + G1/G2/G4 gates\n\
         \x20 --check      type-check only -> build/paig.rmeta\n\
         \x20 test         unit tests, then the integration test\n\
         \x20 clean        remove build/paig and intermediate artifacts"
    );
}

fn repo_root() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().and_then(Path::parent).map(Path::to_path_buf))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn rustc_name() -> String {
    std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string())
}

fn mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|meta| meta.modified()).ok()
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

fn current_stamp() -> String {
    let rustc = rustc_name();
    let version = Command::new(&rustc)
        .arg("--version")
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default();
    format!("rustc: {rustc}\nversion: {version}\nflags: {}\n", COMMON.join(" "))
}

fn cc_name() -> String {
    std::env::var("CC").unwrap_or_else(|_| "cc".to_string())
}

fn ar_name() -> String {
    std::env::var("AR").unwrap_or_else(|_| "ar".to_string())
}

fn newer_than(source: &Path, object: &Path) -> bool {
    match (mtime(source), mtime(object)) {
        (Some(source_time), Some(object_time)) => source_time > object_time,
        _ => true,
    }
}

fn tree_newer_than_headers(dir: &Path, stamp: SystemTime) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if tree_newer_than_headers(&path, stamp) {
                return true;
            }
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("h")
            && mtime(&path).map(|time| time > stamp).unwrap_or(false)
        {
            return true;
        }
    }
    false
}

fn c_stamp(root: &Path) -> String {
    let cc = cc_name();
    let version = Command::new(&cc)
        .arg("--version")
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default();
    let sources =
        fs::read_to_string(root.join("vendor/mbedtls/sources.txt")).unwrap_or_default();
    format!(
        "cc: {cc}\nversion: {version}\nflags: {}\nsources:\n{sources}\n",
        MBEDTLS_CFLAGS.join(" ")
    )
}

fn build_jobs(limit: usize) -> usize {
    if let Ok(value) = std::env::var("PAIG_BUILD_JOBS") {
        if let Ok(count) = value.parse::<usize>() {
            if count > 0 {
                return count.min(limit);
            }
        }
    }
    std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(4)
        .min(limit)
}

fn compile_c_jobs(root: &Path, jobs: Vec<(PathBuf, PathBuf)>) -> Result<(), String> {
    if jobs.is_empty() {
        return Ok(());
    }
    let cc = cc_name();
    let next = AtomicUsize::new(0);
    let errors = Mutex::new(Vec::new());
    let workers = build_jobs(jobs.len());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= jobs.len() {
                    break;
                }
                let (source, object) = &jobs[index];
                let mut command = Command::new(&cc);
                command
                    .current_dir(root)
                    .args(MBEDTLS_CFLAGS)
                    .arg("-I")
                    .arg(root.join("vendor/mbedtls"))
                    .arg("-I")
                    .arg(root.join("vendor/mbedtls/include"))
                    .arg("-I")
                    .arg(root.join("vendor/mbedtls/library"))
                    .arg("-c")
                    .arg(source)
                    .arg("-o")
                    .arg(object);
                match command.status() {
                    Ok(status) if status.success() => {}
                    Ok(status) => errors.lock().unwrap().push(format!(
                        "C compile failed ({status}): {}",
                        source.display()
                    )),
                    Err(error) => errors
                        .lock()
                        .unwrap()
                        .push(format!("cannot run {cc}: {error}")),
                }
            });
        }
    });
    let errors = match errors.into_inner() {
        Ok(errors) => errors,
        Err(poisoned) => poisoned.into_inner(),
    };
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

fn build_mbedtls(root: &Path) -> Result<PathBuf, String> {
    let build_dir = root.join("build/mbedtls");
    let obj_dir = build_dir.join("obj");
    fs::create_dir_all(&build_dir)
        .map_err(|error| format!("cannot create {}: {error}", build_dir.display()))?;
    let sources_text = fs::read_to_string(root.join("vendor/mbedtls/sources.txt"))
        .map_err(|error| format!("cannot read vendor/mbedtls/sources.txt: {error}"))?;
    let sources: Vec<&str> = sources_text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    if sources.is_empty() {
        return Err("vendor/mbedtls/sources.txt is empty".to_string());
    }

    let archive = build_dir.join("libmbedtls.a");
    let stamp_path = build_dir.join("stamp");
    let stamp = c_stamp(root);
    let mut full = fs::read_to_string(&stamp_path).unwrap_or_default() != stamp;
    if !full {
        match mtime(&archive) {
            Some(archive_time) => {
                full = tree_newer_than_headers(&root.join("vendor/mbedtls/include"), archive_time)
                    || tree_newer_than_headers(
                        &root.join("vendor/mbedtls/library"),
                        archive_time,
                    );
            }
            None => full = true,
        }
    }
    if full {
        let _ = fs::remove_dir_all(&obj_dir);
        let _ = fs::remove_file(&archive);
    }
    fs::create_dir_all(&obj_dir)
        .map_err(|error| format!("cannot create {}: {error}", obj_dir.display()))?;

    let config = root.join("vendor/mbedtls/paig_config.h");
    let shim_source = root.join("src/transport/tls_shim.c");
    let shim_header = root.join("src/transport/tls_shim.h");
    let mut jobs = Vec::new();
    let mut objects = Vec::new();
    for name in sources {
        let source = root.join("vendor/mbedtls/library").join(name);
        if !source.exists() {
            return Err(format!("missing C source: {}", source.display()));
        }
        let object = obj_dir.join(format!("{}.o", name.trim_end_matches(".c")));
        if full || newer_than(&source, &object) || newer_than(&config, &object) {
            jobs.push((source, object.clone()));
        }
        objects.push(object);
    }

    let shim_object = obj_dir.join("tls_shim.o");
    if full
        || newer_than(&shim_source, &shim_object)
        || newer_than(&shim_header, &shim_object)
    {
        jobs.push((shim_source, shim_object.clone()));
    }
    objects.push(shim_object);

    let job_count = jobs.len();
    let spinner = if job_count == 0 {
        None
    } else {
        Spinner::start("building", "build/mbedtls/libmbedtls.a", "1;33")
    };
    if job_count > 0 && !color() {
        println!(
            "  [C] compiling {job_count} Mbed TLS sources -> build/mbedtls/libmbedtls.a"
        );
    }
    compile_c_jobs(root, jobs)?;
    if let Some(spinner) = spinner {
        spinner.finish();
    }

    let archive_stale = full
        || !archive.exists()
        || objects.iter().any(|object| newer_than(object, &archive));
    if archive_stale {
        let _ = fs::remove_file(&archive);
        let ar = ar_name();
        let mut command = Command::new(&ar);
        command.current_dir(root).arg("rcs").arg(&archive);
        for object in &objects {
            command.arg(object);
        }
        let status = command
            .status()
            .map_err(|error| format!("cannot run {ar}: {error}"))?;
        if !status.success() {
            return Err(format!("{ar} failed ({status})"));
        }
        fs::write(&stamp_path, stamp)
            .map_err(|error| format!("cannot write {}: {error}", stamp_path.display()))?;
    }
    Ok(archive)
}

fn up_to_date(
    root: &Path,
    bin: &Path,
    stamp_path: &Path,
    archive: &Path,
) -> Result<bool, String> {
    let Some(bin_time) = mtime(bin) else {
        return Ok(false);
    };
    if fs::read_to_string(stamp_path).unwrap_or_default() != current_stamp() {
        return Ok(false);
    }
    if let Ok(orchestrator) = std::env::current_exe() {
        if mtime(&orchestrator).map(|time| time > bin_time).unwrap_or(false) {
            return Ok(false);
        }
    }
    match mtime(archive) {
        Some(archive_time) if archive_time <= bin_time => {}
        _ => return Ok(false),
    }
    if mtime(&root.join("vendor/mbedtls/ca-bundle.pem"))
        .map(|time| time > bin_time)
        .unwrap_or(true)
    {
        return Ok(false);
    }
    let mut sources = Vec::new();
    rust_sources(&root.join("src"), &mut sources);
    for source in sources {
        match mtime(&source) {
            Some(time) if time < bin_time => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

fn build(root: &Path) -> Result<(), String> {
    check_g1(root)?;
    let build_dir = root.join("build");
    fs::create_dir_all(&build_dir)
        .map_err(|error| format!("cannot create {}: {error}", build_dir.display()))?;
    let archive = build_mbedtls(root)?;
    let bin = build_dir.join("paig");
    let stamp = build_dir.join("paig.stamp");
    let rebuilt = !up_to_date(root, &bin, &stamp, &archive)?;
    let shown = bin.strip_prefix(root).unwrap_or_else(|_| bin.as_path());
    let spinner = if rebuilt {
        Spinner::start("building", &shown.to_string_lossy(), "1;32")
    } else {
        None
    };
    if rebuilt {
        let mut command = Command::new(rustc_name());
        command
            .args(COMMON)
            .arg("-C")
            .arg(format!("link-arg={}", archive.display()))
            .arg("-C")
            .arg("link-arg=-lpthread")
            .arg("-o")
            .arg(&bin)
            .arg(root.join("src/main.rs"))
            .current_dir(root);
        run(command)?;
        if let Some(spinner) = spinner {
            spinner.finish();
        }
        fs::write(&stamp, current_stamp())
            .map_err(|error| format!("cannot write {}: {error}", stamp.display()))?;
    }
    check_g2(&bin)?;
    let size = check_g4(&bin)?;
    let (icon, ascii, style, state) = if rebuilt {
        ("\u{f1b2}", "[+]", "1;32", "built")
    } else {
        ("\u{f00c}", "[=]", "1;36", "up to date")
    };
    report(icon, ascii, style, state, shown, Some(size));
    Ok(())
}

fn check(root: &Path) -> Result<(), String> {
    check_g1(root)?;
    let build_dir = root.join("build");
    fs::create_dir_all(&build_dir)
        .map_err(|error| format!("cannot create {}: {error}", build_dir.display()))?;
    let rmeta = build_dir.join("paig.rmeta");
    let mut command = Command::new(rustc_name());
    command
        .args(COMMON)
        .arg("--emit=metadata")
        .arg("-o")
        .arg(&rmeta)
        .arg(root.join("src/main.rs"))
        .current_dir(root);
    let shown = rmeta.strip_prefix(root).unwrap_or_else(|_| rmeta.as_path());
    let spinner = Spinner::start("checking", &shown.to_string_lossy(), "1;34");
    run(command)?;
    if let Some(spinner) = spinner {
        spinner.finish();
    }
    report("\u{f058}", "[ok]", "1;34", "checked", shown, None);
    Ok(())
}

fn test(root: &Path, filters: &[String]) -> Result<(), String> {
    check_g1(root)?;
    let build_dir = root.join("build");
    fs::create_dir_all(&build_dir)
        .map_err(|error| format!("cannot create {}: {error}", build_dir.display()))?;
    let archive = build_mbedtls(root)?;
    let test_bin = build_dir.join("paig-test");
    let mut command = Command::new(rustc_name());
    command
        .args(["--edition", "2024", "--test", "-C", "opt-level=2"])
        .arg("-C")
        .arg(format!("link-arg={}", archive.display()))
        .arg("-C")
        .arg("link-arg=-lpthread")
        // Same lld pin as COMMON: bfd rejects the -nodefaultlibs ordering.
        .arg("-C")
        .arg("link-arg=-fuse-ld=lld")
        .arg("-o")
        .arg(&test_bin)
        .arg(root.join("src/main.rs"))
        .current_dir(root);
    run(command)?;
    let mut unit = Command::new(&test_bin);
    unit.args(filters).current_dir(root);
    run(unit)?;
    if filters.is_empty() {
        build(root)?;
        let mut integration = Command::new("sh");
        integration
            .arg(root.join("tests/integration.sh"))
            .current_dir(root);
        run(integration)?;
    }
    Ok(())
}

fn clean(root: &Path) -> Result<(), String> {
    let build_dir = root.join("build");
    for name in ["paig", "paig.rmeta", "paig-test", "paig.stamp"] {
        let path = build_dir.join(name);
        let shown = path.strip_prefix(root).unwrap_or_else(|_| path.as_path());
        match fs::remove_file(&path) {
            Ok(()) => report("\u{f1f8}", "[-]", "1;33", "removed", shown, None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("cannot remove {}: {error}", path.display())),
        }
    }
    let mbedtls = build_dir.join("mbedtls");
    let shown = mbedtls
        .strip_prefix(root)
        .unwrap_or_else(|_| mbedtls.as_path());
    match fs::remove_dir_all(&mbedtls) {
        Ok(()) => report("\u{f1f8}", "[-]", "1;33", "removed", shown, None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("cannot remove {}: {error}", mbedtls.display())),
    }
    Ok(())
}

fn grouped(value: u64) -> String {
    let text = value.to_string();
    let mut out = String::with_capacity(text.len() + text.len() / 3);
    for (index, ch) in text.chars().enumerate() {
        if index > 0 && (text.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn color() -> bool {
    if std::env::var_os("NO_COLOR")
        .map(|value| !value.is_empty())
        .unwrap_or(false)
    {
        return false;
    }
    if std::env::var_os("TERM")
        .map(|value| value.to_string_lossy() == "dumb")
        .unwrap_or(false)
    {
        return false;
    }
    std::io::stdout().is_terminal()
}

fn paint(text: &str, style: &str) -> String {
    if color() {
        format!("\x1b[{style}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn report(icon: &str, ascii: &str, style: &str, state: &str, path: &Path, size: Option<u64>) {
    let icon = if color() { icon } else { ascii };
    let state = format!("{state:<10}");
    match size {
        Some(size) => println!(
            "{}  {}  {}  {} bytes",
            paint(icon, style),
            paint(&state, style),
            path.display(),
            grouped(size)
        ),
        None => println!(
            "{}  {}  {}",
            paint(icon, style),
            paint(&state, style),
            path.display()
        ),
    }
}

struct Spinner {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Spinner {
    fn start(label: &'static str, target: &str, style: &'static str) -> Option<Self> {
        if !color() {
            return None;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let worker = stop.clone();
        let target = target.to_string();
        let handle = thread::spawn(move || {
            let frames = [
                "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}",
                "\u{2834}", "\u{2826}", "\u{2827}", "\u{2807}", "\u{280f}",
            ];
            let started = Instant::now();
            let mut index = 0usize;
            while !worker.load(Ordering::Relaxed) {
                print!(
                    "\r\x1b[2K{} {} {}... {:.1}s",
                    paint(frames[index % frames.len()], style),
                    paint(label, "2"),
                    target,
                    started.elapsed().as_secs_f64()
                );
                let _ = std::io::stdout().flush();
                index += 1;
                thread::sleep(Duration::from_millis(80));
            }
        });
        Some(Self {
            stop,
            handle: Some(handle),
        })
    }

    fn finish(mut self) {
        self.clear();
    }

    fn clear(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        print!("\r\x1b[2K");
        let _ = std::io::stdout().flush();
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        if self.handle.is_some() {
            self.clear();
        }
    }
}

fn check_g1(root: &Path) -> Result<(), String> {
    for name in ["Cargo.toml", "Cargo.lock"] {
        if root.join(name).exists() {
            return Err(format!("G1 violation: {name} present"));
        }
    }
    let mut sources = Vec::new();
    rust_sources(&root.join("src"), &mut sources);
    for source in sources {
        let text = fs::read_to_string(&source)
            .map_err(|error| format!("cannot read {}: {error}", source.display()))?;
        if text.contains("extern crate") {
            return Err(format!("G1 violation: extern crate in {}", source.display()));
        }
    }
    Ok(())
}

fn check_g2(bin: &Path) -> Result<(), String> {
    let output = Command::new("ldd")
        .arg(bin)
        .output()
        .map_err(|error| format!("G2 violation: cannot run ldd: {error}"))?;
    if !output.status.success() && output.stdout.is_empty() {
        return Err("G2 violation: ldd failed; cannot verify runtime libraries".to_string());
    }
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some(name) = line.split_whitespace().next() else {
            continue;
        };
        if name == "linux-vdso.so.1" || rules::ALLOWED_RUNTIME_LIBS.contains(&name) {
            continue;
        }
        if name.starts_with("/lib/ld-linux") || name.starts_with("/lib64/ld-linux") {
            continue;
        }
        return Err(format!("G2 violation: unexpected runtime library: {name}"));
    }
    Ok(())
}

fn check_g4(bin: &Path) -> Result<u64, String> {
    let size = fs::metadata(bin)
        .map_err(|error| format!("cannot stat {}: {error}", bin.display()))?
        .len();
    if size > rules::TARGET_BINARY_BYTES {
        eprintln!(
            "warning: binary exceeds G4 target ({} > {} bytes)",
            size,
            rules::TARGET_BINARY_BYTES
        );
    }
    Ok(size)
}

fn run(mut command: Command) -> Result<(), String> {
    let label = format!("{command:?}");
    let status = command
        .status()
        .map_err(|error| format!("cannot run {label}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("command failed ({status}): {label}"))
    }
}
