//! Background lifecycle owned by the gateway binary itself: start, stop,
//! restart, and status. The pid and log live in ~/.paig/.

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::util;

unsafe extern "C" {
    fn setsid() -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
}

const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;
const EPERM: i32 = 1;

pub fn dispatch(argv: &[String]) -> Option<i32> {
    match argv.first().map(String::as_str)? {
        "--start" => Some(start()),
        "--stop" => Some(stop(false)),
        "--restart" => Some(restart()),
        "--status" => Some(status()),
        _ => None,
    }
}

fn paths() -> (PathBuf, PathBuf) {
    let dir = util::paig_home();
    (dir.join("paig.pid"), dir.join("paig.log"))
}

fn ensure_home() -> Result<PathBuf, String> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = util::paig_home();
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder
        .create(&dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    Ok(dir)
}

fn parse_pid(text: &str) -> Option<i32> {
    text.trim().parse::<i32>().ok().filter(|pid| *pid > 0)
}

fn alive(pid: i32) -> bool {
    if unsafe { kill(pid, 0) } == 0 {
        return true;
    }
    // A live process owned by another user answers EPERM, not ESRCH.
    io::Error::last_os_error().raw_os_error() == Some(EPERM)
}

fn is_paig_arg0(arg0: &str) -> bool {
    arg0 == "paig" || arg0.ends_with("/paig")
}

fn is_paig(pid: i32) -> bool {
    // Fail closed: an unreadable cmdline cannot prove the PID is ours.
    let Ok(bytes) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let first = bytes.split(|byte| *byte == 0).next().unwrap_or(&[]);
    is_paig_arg0(&String::from_utf8_lossy(first))
}

fn running_pid(pidfile: &Path) -> Option<i32> {
    let pid = parse_pid(&fs::read_to_string(pidfile).ok()?)?;
    if alive(pid) && is_paig(pid) {
        Some(pid)
    } else {
        None
    }
}

fn send(pid: i32, signal: i32) {
    unsafe {
        kill(pid, signal);
    }
}

fn badge(pretty: &'static str, ascii: &'static str, style: &str, stderr: bool) -> String {
    util::paint(util::icon(pretty, ascii, stderr), style, stderr)
}

fn fail(message: &str) {
    eprintln!("{}  {message}", badge("\u{f057}", "[x]", "1;31", true));
}

fn start() -> i32 {
    let dir = match ensure_home() {
        Ok(dir) => dir,
        Err(error) => {
            fail(&format!("paig: {error}"));
            return 1;
        }
    };
    let pidfile = dir.join("paig.pid");
    let logfile = dir.join("paig.log");
    if let Some(pid) = running_pid(&pidfile) {
        println!(
            "{}  paig already running (pid {pid})",
            badge("\u{f05a}", "[i]", "1;34", false)
        );
        return 0;
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            fail(&format!("paig: cannot locate the executable: {error}"));
            return 1;
        }
    };
    let log = match OpenOptions::new().create(true).append(true).open(&logfile) {
        Ok(log) => log,
        Err(error) => {
            fail(&format!("paig: cannot open {}: {error}", logfile.display()));
            return 1;
        }
    };
    let log_err = match log.try_clone() {
        Ok(log) => log,
        Err(error) => {
            fail(&format!(
                "paig: cannot duplicate {}: {error}",
                logfile.display()
            ));
            return 1;
        }
    };
    let mut command = Command::new(&exe);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    unsafe {
        command.pre_exec(|| {
            if setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            fail(&format!("paig: cannot spawn {}: {error}", exe.display()));
            return 1;
        }
    };
    let pid = child.id() as i32;
    if let Err(error) = fs::write(&pidfile, format!("{pid}\n")) {
        fail(&format!("paig: cannot write {}: {error}", pidfile.display()));
        let _ = child.kill();
        return 1;
    }
    let mut exited = None;
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(10));
        match child.try_wait() {
            Ok(Some(status)) => {
                exited = Some(status);
                break;
            }
            Ok(None) => {}
            Err(error) => {
                fail(&format!("paig: cannot check the started process: {error}"));
                return 1;
            }
        }
    }
    if let Some(status) = exited {
        let _ = fs::remove_file(&pidfile);
        fail(&format!(
            "paig failed to start ({status}); see {}",
            logfile.display()
        ));
        return 1;
    }
    println!(
        "{}  paig started (pid {pid}, log {})",
        badge("\u{f04b}", "[>]", "1;32", false),
        logfile.display()
    );
    0
}

fn stop(quiet: bool) -> i32 {
    let (pidfile, _) = paths();
    let Some(pid) = running_pid(&pidfile) else {
        let _ = fs::remove_file(&pidfile);
        if !quiet {
            println!("{}  paig not running", badge("\u{f05a}", "[i]", "1;34", false));
        }
        return 0;
    };
    send(pid, SIGTERM);
    for _ in 0..20 {
        if !alive(pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if alive(pid) {
        send(pid, SIGKILL);
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = fs::remove_file(&pidfile);
    println!(
        "{}  paig stopped (pid {pid})",
        badge("\u{f04d}", "[#]", "1;33", false)
    );
    0
}

fn restart() -> i32 {
    let code = stop(true);
    if code != 0 {
        return code;
    }
    start()
}

fn status() -> i32 {
    let (pidfile, _) = paths();
    match running_pid(&pidfile) {
        Some(pid) => {
            println!(
                "{}  paig running (pid {pid})",
                badge("\u{f058}", "[ok]", "1;32", false)
            );
            0
        }
        None => {
            let _ = fs::remove_file(&pidfile);
            println!("{}  paig not running", badge("\u{f05a}", "[i]", "1;34", false));
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_parsing_rejects_garbage() {
        assert_eq!(parse_pid("123\n"), Some(123));
        assert_eq!(parse_pid(""), None);
        assert_eq!(parse_pid("0"), None);
        assert_eq!(parse_pid("-4"), None);
        assert_eq!(parse_pid("12x"), None);
    }

    #[test]
    fn cmdline_matches_only_the_paig_binary() {
        assert!(is_paig_arg0("paig"));
        assert!(is_paig_arg0("./build/paig"));
        assert!(is_paig_arg0("/home/han/workspace/Angkuh-Org/PAIG/build/paig"));
        assert!(!is_paig_arg0("paig-test"));
        assert!(!is_paig_arg0("notpaig"));
        assert!(!is_paig_arg0("/tmp/paig-build"));
    }
}
