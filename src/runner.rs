//! Spawning the command: name resolution, low priority, process tree cleanup, signals.

use std::io;
use std::process::{Command, ExitStatus};
#[cfg(unix)]
use std::sync::atomic::AtomicI32;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set by the Ctrl+C / termination handler.
pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);
/// Process group of the running child (Unix), for forwarding signals.
#[cfg(unix)]
pub static CHILD_PGID: AtomicI32 = AtomicI32::new(0);

pub fn install_signal_handler() {
    let _ = ctrlc::set_handler(|| {
        INTERRUPTED.store(true, Ordering::SeqCst);
        #[cfg(unix)]
        {
            let pg = CHILD_PGID.load(Ordering::SeqCst);
            if pg > 0 {
                // SAFETY: plain syscall with a process group id we spawned.
                unsafe {
                    libc::killpg(pg, libc::SIGINT);
                }
            }
        }
    });
}

/// Build the command for `argv`. A single string containing whitespace runs through the shell.
pub fn build(argv: &[String]) -> io::Result<Command> {
    if argv.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "no command given"));
    }
    if crate::detect::is_shell_string(argv) {
        return Ok(shell_command(&argv[0]));
    }
    platform_build(argv)
}

#[cfg(unix)]
fn shell_command(line: &str) -> Command {
    let mut c = Command::new("/bin/sh");
    c.arg("-c").arg(line);
    c
}

#[cfg(unix)]
fn platform_build(argv: &[String]) -> io::Result<Command> {
    let mut c = Command::new(&argv[0]);
    c.args(&argv[1..]);
    Ok(c)
}

#[cfg(windows)]
fn comspec() -> String {
    std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_string())
}

#[cfg(windows)]
fn shell_command(line: &str) -> Command {
    use std::os::windows::process::CommandExt;
    let mut c = Command::new(comspec());
    c.raw_arg(format!("/d /s /c \"{line}\""));
    c
}

#[cfg(windows)]
fn platform_build(argv: &[String]) -> io::Result<Command> {
    use std::os::windows::process::CommandExt;
    let path = win::resolve(&argv[0])
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("command not found: {}", argv[0])))?;
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "cmd" | "bat" => {
            // A batch file re-parses its arguments when it expands %* / %1, so every cmd
            // metacharacter needs two levels of caret escaping (one per cmd parse).
            let mut line = win::escape_command(&path.to_string_lossy());
            for a in &argv[1..] {
                line.push(' ');
                line.push_str(&win::escape_argument(a));
            }
            let mut c = Command::new(comspec());
            c.raw_arg(format!("/d /s /c \"{line}\""));
            Ok(c)
        }
        "ps1" => {
            let mut c = Command::new("powershell.exe");
            c.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
                .arg(&path)
                .args(&argv[1..]);
            Ok(c)
        }
        _ => {
            let mut c = Command::new(&path);
            c.args(&argv[1..]);
            Ok(c)
        }
    }
}

#[cfg(windows)]
pub mod win {
    use std::path::{Path, PathBuf};

    fn pathext() -> Vec<String> {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(|e| e.to_ascii_lowercase())
            .collect()
    }

    fn try_file(base: &Path, exts: &[String]) -> Option<PathBuf> {
        let has_ext = base
            .extension()
            .map(|e| exts.contains(&format!(".{}", e.to_string_lossy().to_ascii_lowercase())) || e == "ps1")
            .unwrap_or(false);
        if has_ext && base.is_file() {
            return Some(base.to_path_buf());
        }
        for e in exts {
            let mut s = base.as_os_str().to_owned();
            s.push(e);
            let p = PathBuf::from(s);
            if p.is_file() {
                return Some(p);
            }
        }
        None
    }

    /// PATH + PATHEXT lookup, like cmd.exe does (so `npm` finds `npm.cmd`).
    pub fn resolve(name: &str) -> Option<PathBuf> {
        let exts = pathext();
        let p = Path::new(name);
        if p.is_absolute() || name.contains('/') || name.contains('\\') {
            return try_file(p, &exts);
        }
        if let Some(found) = try_file(&std::env::current_dir().ok()?.join(name), &exts) {
            // cmd.exe searches the current directory first; mirror that.
            return Some(found);
        }
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .filter(|d| !d.as_os_str().is_empty())
            .find_map(|d| try_file(&d.join(name), &exts))
    }

    const META: &[char] = &[
        '(', ')', ']', '[', '%', '!', '^', '"', '`', '<', '>', '&', '|', ';', ',', ' ', '*', '?',
    ];

    fn caret(s: &str) -> String {
        let mut out = String::with_capacity(s.len() * 2);
        for c in s.chars() {
            if META.contains(&c) {
                out.push('^');
            }
            out.push(c);
        }
        out
    }

    pub fn escape_command(s: &str) -> String {
        caret(s)
    }

    /// Quote for the program's argv parser, then caret-escape every cmd metacharacter so cmd
    /// passes it through literally (the approach of https://qntm.org/cmd, applied twice).
    pub fn escape_argument(arg: &str) -> String {
        let mut quoted = String::from("\"");
        let mut backslashes = 0;
        for c in arg.chars() {
            match c {
                '\\' => backslashes += 1,
                '"' => {
                    quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                    quoted.push('"');
                    backslashes = 0;
                }
                _ => {
                    quoted.extend(std::iter::repeat_n('\\', backslashes));
                    quoted.push(c);
                    backslashes = 0;
                }
            }
        }
        quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
        quoted.push('"');
        caret(&caret(&quoted))
    }

    /// Put this process in a job that kills every process in it when the last handle closes,
    /// so the command's whole tree dies with calmrun. The child inherits the job; a daemon that
    /// asks to break away (CREATE_BREAKAWAY_FROM_JOB) may.
    pub fn join_kill_on_close_job() {
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: Win32 calls with valid arguments; the job handle is deliberately kept open
        // for the life of the process.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags =
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok != 0 {
                AssignProcessToJobObject(job, GetCurrentProcess());
            }
        }
    }
}

/// Spawn and wait. Returns the exit code to pass on (128+signal on Unix) and the child pid
/// via `on_spawn`.
pub fn run(mut cmd: Command, low_cpu: bool, on_spawn: impl FnOnce(u32)) -> io::Result<i32> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::BELOW_NORMAL_PRIORITY_CLASS;
        win::join_kill_on_close_job();
        if low_cpu {
            cmd.creation_flags(BELOW_NORMAL_PRIORITY_CLASS);
        }
        let mut child = cmd.spawn()?;
        on_spawn(child.id());
        // Ctrl+C reaches the child through the shared console; we just wait for it.
        let status = child.wait()?;
        Ok(code_of(status))
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: plain syscalls. Only hand over the terminal when we are its foreground group.
        let tty = unsafe { libc::isatty(0) == 1 && libc::tcgetpgrp(0) == libc::getpgrp() };
        cmd.process_group(0);
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                if low_cpu {
                    libc::nice(10);
                }
                if tty {
                    // Make the child's group the terminal's foreground group so it can read
                    // input and receives Ctrl+C directly.
                    libc::signal(libc::SIGTTOU, libc::SIG_IGN);
                    libc::tcsetpgrp(0, libc::getpid());
                    libc::signal(libc::SIGTTOU, libc::SIG_DFL);
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn()?;
        let pid = child.id() as i32;
        CHILD_PGID.store(pid, Ordering::SeqCst);
        on_spawn(child.id());
        let status = child.wait();
        CHILD_PGID.store(0, Ordering::SeqCst);
        // SAFETY: plain syscalls on our own process group / terminal.
        unsafe {
            if tty {
                libc::signal(libc::SIGTTOU, libc::SIG_IGN);
                libc::tcsetpgrp(0, libc::getpgrp());
                libc::signal(libc::SIGTTOU, libc::SIG_DFL);
            }
            // Leftover processes of the command's group die with it.
            libc::killpg(pid, libc::SIGTERM);
        }
        Ok(code_of(status?))
    }
}

pub fn code_of(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    status.code().unwrap_or(1)
}

#[cfg(all(test, windows))]
mod tests {
    use super::win::*;

    #[test]
    fn escapes_metacharacters() {
        assert_eq!(escape_argument("a b"), "^^^\"a^^^ b^^^\"");
        assert_eq!(escape_argument("x&y"), "^^^\"x^^^&y^^^\"");
        assert_eq!(escape_argument("say \"hi\""), "^^^\"say^^^ \\^^^\"hi\\^^^\"^^^\"");
        assert_eq!(escape_argument("dir\\"), "^^^\"dir\\\\^^^\"");
    }

    #[test]
    fn resolves_cmd_from_path() {
        let found = resolve("cmd").expect("cmd.exe on PATH");
        assert!(found.to_string_lossy().to_ascii_lowercase().ends_with("cmd.exe"));
    }
}
