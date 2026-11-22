//! Spawning the command: name resolution, low priority, process-tree cleanup, signals.
//!
//! The public functions here are the same on every OS; the per-OS work lives in the private
//! `imp` module (one for Windows, one for Unix).

use std::io;
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Set by the Ctrl+C / termination handler.
pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// How often a running command is checked (exit, `watch`).
const WATCH_EVERY: Duration = Duration::from_millis(250);
/// Exit code after a stop (memory ceiling, timeout, cancel): like `kill -9`.
pub const EXIT_STOPPED: i32 = 137;
/// Exit code after Ctrl+C.
pub const EXIT_INTERRUPTED: i32 = 130;

/// Characters that only mean something to a shell.
pub const SHELL_CHARS: &[char] = imp::SHELL_CHARS;

/// Install the Ctrl+C / SIGTERM handler: it sets [`INTERRUPTED`], and on Unix forwards
/// SIGINT to the running command's process group.
pub fn install_signal_handler() {
    imp::before_handler();
    let _ = ctrlc::set_handler(|| {
        INTERRUPTED.store(true, Ordering::SeqCst);
        imp::forward_interrupt();
    });
}

/// Test helper: deliver Ctrl+C to `pid` (a console event on Windows, SIGINT on Unix).
pub fn send_ctrl_c(pid: u32) -> bool {
    imp::send_ctrl_c(pid)
}

/// Build the command for `argv`, to run in `cwd`. A single string containing whitespace runs
/// through the shell. A relative program path (`./gradlew`) is resolved against `cwd`.
pub fn build(argv: &[String], cwd: &Path) -> io::Result<Command> {
    if argv.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "no command given"));
    }
    if crate::shell::is_shell_string(argv) {
        return Ok(shell_command(&argv[0]));
    }
    imp::build(argv, cwd)
}

/// One command line for the platform shell (`sh -c` / `cmd /d /s /c`).
pub fn shell_command(line: &str) -> Command {
    imp::shell_command(line)
}

/// How the command ended.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ended {
    /// Exit code to pass on (128+signal on Unix, 130 after Ctrl+C, 137 after a stop).
    pub code: i32,
    /// True when `watch` asked to stop the tree.
    pub stopped: bool,
}

/// Spawn and wait. `on_spawn` gets the child pid; `watch` is asked about every 250 ms and
/// returns true to stop the whole tree. After a normal exit, processes the command left
/// running on purpose (build daemons) are left alone; after Ctrl+C, a stop, or calmrun being
/// killed (Windows), the whole tree is ended.
pub fn run(
    cmd: Command,
    low_cpu: bool,
    on_spawn: impl FnOnce(u32),
    watch: &mut dyn FnMut(u32) -> bool,
) -> io::Result<Ended> {
    imp::run(cmd, low_cpu, on_spawn, watch)
}

/// Quote a word for display so it can be pasted back into this OS's usual shell.
pub fn quote_for_display(w: &str) -> String {
    const SAFE: &str = "_@%+=:,./-\\";
    if !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric() || SAFE.contains(c)) {
        return w.to_string();
    }
    imp::quote(w)
}

/// The exit code to pass on: 128 + signal on Unix.
pub fn code_of(status: ExitStatus) -> i32 {
    imp::signal_of(&status).map_or_else(|| status.code().unwrap_or(1), |sig| 128 + sig)
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::sync::atomic::AtomicI32;
    use std::time::Instant;

    /// Process group of the running child, for forwarding signals.
    static CHILD_PGID: AtomicI32 = AtomicI32::new(0);
    /// After a stop request: time to exit after SIGTERM before SIGKILL.
    const STOP_GRACE: Duration = Duration::from_secs(3);
    /// Nice increment for low CPU priority.
    const NICE: libc::c_int = 10;

    /// Characters that only mean something to this OS's shell.
    pub const SHELL_CHARS: &[char] = &[
        '&', '|', ';', '<', '>', '(', ')', '$', '`', '*', '?', '\n', '~', '{', '[',
    ];

    /// Prepare the process before the signal handler is installed.
    pub fn before_handler() {}

    /// Pass Ctrl+C on to the running command.
    pub fn forward_interrupt() {
        let pg = CHILD_PGID.load(Ordering::SeqCst);
        if pg > 0 {
            // SAFETY: plain syscall with a process group id we spawned.
            unsafe {
                libc::killpg(pg, libc::SIGINT);
            }
        }
    }

    /// See [`super::send_ctrl_c`].
    pub fn send_ctrl_c(pid: u32) -> bool {
        // SAFETY: plain syscall.
        unsafe { libc::kill(pid as i32, libc::SIGINT) == 0 }
    }

    /// The signal that ended the process, if any.
    pub fn signal_of(status: &ExitStatus) -> Option<i32> {
        status.signal()
    }

    /// Quote a word for this OS's usual shell.
    pub fn quote(w: &str) -> String {
        format!("'{}'", w.replace('\'', "'\\''"))
    }

    /// See [`super::shell_command`].
    pub fn shell_command(line: &str) -> Command {
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg(line);
        c
    }

    /// See [`super::build`].
    pub fn build(argv: &[String], cwd: &Path) -> io::Result<Command> {
        let p = Path::new(&argv[0]);
        let program = if p.is_relative() && argv[0].contains('/') {
            cwd.join(p)
        } else {
            p.to_path_buf()
        };
        let mut c = Command::new(program);
        c.args(&argv[1..]);
        Ok(c)
    }

    /// Whether we are the terminal's foreground group (then the child gets the terminal).
    fn owns_terminal() -> bool {
        // SAFETY: plain syscalls.
        unsafe { libc::isatty(0) == 1 && libc::tcgetpgrp(0) == libc::getpgrp() }
    }

    fn give_terminal_to(pgrp: libc::pid_t) {
        // SAFETY: plain syscalls; SIGTTOU is ignored around tcsetpgrp from a background group.
        unsafe {
            libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            libc::tcsetpgrp(0, pgrp);
            libc::signal(libc::SIGTTOU, libc::SIG_DFL);
        }
    }

    fn killpg(pid: i32, sig: libc::c_int) {
        // SAFETY: signalling the process group we created.
        unsafe {
            libc::killpg(pid, sig);
        }
    }

    /// See [`super::run`].
    pub fn run(
        mut cmd: Command,
        low_cpu: bool,
        on_spawn: impl FnOnce(u32),
        watch: &mut dyn FnMut(u32) -> bool,
    ) -> io::Result<Ended> {
        let tty = owns_terminal();
        cmd.process_group(0);
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                if low_cpu {
                    libc::nice(NICE);
                }
                if tty {
                    give_terminal_to(libc::getpid());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn()?;
        let pid = child.id() as i32;
        CHILD_PGID.store(pid, Ordering::SeqCst);
        on_spawn(child.id());
        let mut stopped_at: Option<Instant> = None;
        let status = loop {
            match child.try_wait() {
                Ok(Some(st)) => break Ok(st),
                Ok(None) => {}
                Err(e) => break Err(e),
            }
            match stopped_at {
                None if watch(child.id()) => {
                    killpg(pid, libc::SIGTERM);
                    stopped_at = Some(Instant::now());
                }
                Some(t) if t.elapsed() > STOP_GRACE => killpg(pid, libc::SIGKILL),
                _ => {}
            }
            std::thread::sleep(WATCH_EVERY);
        };
        CHILD_PGID.store(0, Ordering::SeqCst);
        if tty {
            // SAFETY: plain syscall.
            give_terminal_to(unsafe { libc::getpgrp() });
        }
        let stopped = stopped_at.is_some();
        // After Ctrl+C, leftovers of the command's group get SIGTERM so they can clean up;
        // after a stop (already past its grace period) they are killed. After a normal exit
        // they stay: they are daemons the command meant to leave running.
        if stopped {
            killpg(pid, libc::SIGKILL);
        } else if INTERRUPTED.load(Ordering::SeqCst) {
            killpg(pid, libc::SIGTERM);
        }
        let code = if stopped { EXIT_STOPPED } else { code_of(status?) };
        Ok(Ended { code, stopped })
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::path::PathBuf;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// Characters that only mean something to this OS's shell.
    pub const SHELL_CHARS: &[char] = &[
        '&', '|', ';', '<', '>', '(', ')', '$', '`', '*', '?', '\n', '%', '^', '!', '~',
    ];
    /// Exit status of a process ended by Ctrl+C.
    const STATUS_CONTROL_C_EXIT: i32 = 0xC000_013Au32 as i32;
    /// How long the Ctrl+C helper stays attached so the event is delivered.
    const CTRL_C_SETTLE: Duration = Duration::from_millis(500);

    /// Prepare the process before the signal handler is installed.
    pub fn before_handler() {
        // SAFETY: plain console call. A parent may have started us with Ctrl+C ignored (that
        // flag is inherited); turn processing back on so we and the command can react.
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(None, 0);
        }
    }

    /// The command shares our console and gets Ctrl+C by itself.
    pub fn forward_interrupt() {}

    /// The signal that ended the process, if any.
    pub fn signal_of(_: &ExitStatus) -> Option<i32> {
        None
    }

    /// Quote a word for this OS's usual shell.
    pub fn quote(w: &str) -> String {
        format!("\"{}\"", w.replace('"', "\\\""))
    }

    /// See [`super::send_ctrl_c`].
    pub fn send_ctrl_c(pid: u32) -> bool {
        use windows_sys::Win32::System::Console::{
            AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler, CTRL_C_EVENT,
        };
        // SAFETY: plain console calls; we ignore Ctrl+C ourselves before raising it.
        unsafe {
            FreeConsole();
            if AttachConsole(pid) == 0 {
                return false;
            }
            SetConsoleCtrlHandler(None, 1);
            let ok = GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0) != 0;
            std::thread::sleep(CTRL_C_SETTLE);
            FreeConsole();
            ok
        }
    }

    fn comspec() -> String {
        std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_string())
    }

    /// See [`super::shell_command`].
    pub fn shell_command(line: &str) -> Command {
        let mut c = Command::new(comspec());
        c.raw_arg(format!("/d /s /c \"{line}\""));
        c
    }

    /// See [`super::build`].
    pub fn build(argv: &[String], cwd: &Path) -> io::Result<Command> {
        let path = resolve(&argv[0], cwd).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("command not found: {}", argv[0]))
        })?;
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let mut c = match ext.as_str() {
            "cmd" | "bat" => {
                let mut line = caret(&path.to_string_lossy());
                for a in &argv[1..] {
                    line.push(' ');
                    line.push_str(&escape_argument(a));
                }
                let mut c = Command::new(comspec());
                c.raw_arg(format!("/d /s /c \"{line}\""));
                return Ok(c);
            }
            "ps1" => {
                let mut c = Command::new("powershell.exe");
                c.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
                    .arg(&path);
                c
            }
            _ => Command::new(&path),
        };
        c.args(&argv[1..]);
        Ok(c)
    }

    fn pathext() -> Vec<String> {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(|e| e.to_ascii_lowercase())
            .collect()
    }

    fn try_file(base: &Path, exts: &[String]) -> Option<PathBuf> {
        let has_ext = base.extension().is_some_and(|e| {
            exts.contains(&format!(".{}", e.to_string_lossy().to_ascii_lowercase())) || e == "ps1"
        });
        if has_ext && base.is_file() {
            return Some(base.to_path_buf());
        }
        exts.iter().find_map(|e| {
            let mut s = base.as_os_str().to_owned();
            s.push(e);
            let p = PathBuf::from(s);
            p.is_file().then_some(p)
        })
    }

    /// PATH + PATHEXT lookup, like cmd.exe does (so `npm` finds `npm.cmd`). Relative names are
    /// looked up in `cwd` (the directory the command will run in), and the result is an
    /// absolute path with backslashes, which `cmd /c` accepts.
    pub fn resolve(name: &str, cwd: &Path) -> Option<PathBuf> {
        let exts = pathext();
        let p = Path::new(name);
        if p.is_absolute() {
            return try_file(p, &exts);
        }
        if name.contains(['/', '\\']) {
            return try_file(&cwd.join(name.replace('/', "\\")), &exts);
        }
        // cmd.exe searches the current directory first; mirror that.
        if let Some(found) = try_file(&cwd.join(name), &exts) {
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

    /// Quote for the program's argv parser, then caret-escape every cmd metacharacter twice:
    /// a batch file re-parses its arguments when it expands %* / %1, so there are two cmd
    /// parses to survive (the approach of https://qntm.org/cmd, applied twice).
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

    /// A Job Object holding the command's tree. While `KILL_ON_JOB_CLOSE` is set, the whole
    /// tree dies when calmrun's handle closes (calmrun exits or is killed). A daemon that asks
    /// to break away (CREATE_BREAKAWAY_FROM_JOB) may.
    struct Job(HANDLE);

    impl Job {
        fn new() -> Option<Job> {
            // SAFETY: plain Win32 call; null name and attributes are allowed.
            let h = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if h.is_null() {
                return None;
            }
            let job = Job(h);
            job.set_flags(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK);
            Some(job)
        }

        fn set_flags(&self, flags: u32) -> bool {
            // SAFETY: the struct is plain data and the handle is ours.
            unsafe {
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = flags;
                SetInformationJobObject(
                    self.0,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) != 0
            }
        }

        fn assign(&self, process: HANDLE) -> bool {
            // SAFETY: both handles are valid for the duration of the call.
            unsafe { AssignProcessToJobObject(self.0, process) != 0 }
        }

        /// After a normal exit: let processes the command left behind (build daemons) live on.
        fn release(&self) {
            self.set_flags(JOB_OBJECT_LIMIT_BREAKAWAY_OK);
        }

        fn terminate(&self, code: u32) {
            // SAFETY: the handle is ours.
            unsafe {
                TerminateJobObject(self.0, code);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: closing our own handle once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// Resume every thread of a process started with CREATE_SUSPENDED.
    fn resume_threads(pid: u32) {
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
        };
        use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};
        // SAFETY: standard Toolhelp walk; every handle opened here is closed here.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snap.is_null() || snap == INVALID_HANDLE_VALUE {
                return;
            }
            let mut te: THREADENTRY32 = std::mem::zeroed();
            te.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            let mut ok = Thread32First(snap, &mut te) != 0;
            while ok {
                if te.th32OwnerProcessID == pid {
                    let t = OpenThread(THREAD_SUSPEND_RESUME, 0, te.th32ThreadID);
                    if !t.is_null() {
                        ResumeThread(t);
                        CloseHandle(t);
                    }
                }
                ok = Thread32Next(snap, &mut te) != 0;
            }
            CloseHandle(snap);
        }
    }

    /// See [`super::run`].
    pub fn run(
        mut cmd: Command,
        low_cpu: bool,
        on_spawn: impl FnOnce(u32),
        watch: &mut dyn FnMut(u32) -> bool,
    ) -> io::Result<Ended> {
        use windows_sys::Win32::System::Threading::{BELOW_NORMAL_PRIORITY_CLASS, CREATE_SUSPENDED};
        let job = Job::new();
        let priority = if low_cpu { BELOW_NORMAL_PRIORITY_CLASS } else { 0 };
        // Suspended until it is in the job, so no grandchild escapes before that.
        cmd.creation_flags(CREATE_SUSPENDED | priority);
        let mut child = cmd.spawn()?;
        if let Some(j) = &job {
            j.assign(child.as_raw_handle() as HANDLE);
        }
        resume_threads(child.id());
        on_spawn(child.id());
        let mut stopped = false;
        let status = loop {
            if let Some(st) = child.try_wait()? {
                break st;
            }
            if !stopped && watch(child.id()) {
                stopped = true;
                match &job {
                    Some(j) => j.terminate(EXIT_STOPPED as u32),
                    None => {
                        let _ = child.kill();
                    }
                }
            }
            std::thread::sleep(WATCH_EVERY);
        };
        let interrupted = INTERRUPTED.load(Ordering::SeqCst);
        if let Some(j) = job.as_ref().filter(|_| !interrupted && !stopped) {
            j.release();
        }
        let code = match code_of(status) {
            _ if stopped => EXIT_STOPPED,
            STATUS_CONTROL_C_EXIT if interrupted => EXIT_INTERRUPTED,
            c => c,
        };
        Ok(Ended { code, stopped })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn escapes_metacharacters() {
            assert_eq!(escape_argument("a b"), "^^^\"a^^^ b^^^\"");
            assert_eq!(escape_argument("x&y"), "^^^\"x^^^&y^^^\"");
            assert_eq!(escape_argument("say \"hi\""), "^^^\"say^^^ \\^^^\"hi\\^^^\"^^^\"");
            assert_eq!(escape_argument("dir\\"), "^^^\"dir\\\\^^^\"");
        }

        #[test]
        fn resolves_cmd_from_path() {
            let found = resolve("cmd", Path::new(".")).expect("cmd.exe on PATH");
            assert!(found.to_string_lossy().to_ascii_lowercase().ends_with("cmd.exe"));
        }
    }
}
