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
    #[cfg(windows)]
    // SAFETY: plain console call. A parent may have started us with Ctrl+C ignored (that flag
    // is inherited); turn processing back on so we and the command can react to it.
    unsafe {
        windows_sys::Win32::System::Console::SetConsoleCtrlHandler(None, 0);
    }
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
pub fn shell_command(line: &str) -> Command {
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
pub fn shell_command(line: &str) -> Command {
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

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// A Job Object holding the command's process tree. While `KILL_ON_JOB_CLOSE` is set, the
    /// whole tree dies when calmrun's handle closes (calmrun exits or is killed). A daemon that
    /// asks to break away (CREATE_BREAKAWAY_FROM_JOB) may.
    pub struct Job(HANDLE);

    impl Job {
        pub fn new() -> Option<Job> {
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

        pub fn assign(&self, process: HANDLE) -> bool {
            // SAFETY: both handles are valid for the duration of the call.
            unsafe { AssignProcessToJobObject(self.0, process) != 0 }
        }

        /// After a normal exit: let processes the command left behind (build daemons) live on.
        pub fn release(&self) {
            self.set_flags(JOB_OBJECT_LIMIT_BREAKAWAY_OK);
        }

        pub fn terminate(&self, code: u32) {
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
    pub fn resume_threads(pid: u32) {
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
        };
        use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};
        // SAFETY: standard Toolhelp walk; every handle opened here is closed here.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snap.is_null() || snap == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
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

    /// Test helper: deliver Ctrl+C to the console that `pid` is attached to.
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
            std::thread::sleep(std::time::Duration::from_millis(500));
            FreeConsole();
            ok
        }
    }
}

/// How the command ended.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ended {
    /// Exit code to pass on (128+signal on Unix, 130 after Ctrl+C, 137 after a stop).
    pub code: i32,
    /// True when `watch` asked to stop the tree.
    pub stopped: bool,
}

const WATCH_EVERY: std::time::Duration = std::time::Duration::from_millis(250);
#[cfg(windows)]
const STATUS_CONTROL_C_EXIT: i32 = 0xC000_013Au32 as i32;

/// Spawn and wait. `on_spawn` gets the child pid; `watch` is asked about every 250 ms and
/// returns true to stop the whole tree. After a normal exit, processes the command left
/// running on purpose (build daemons) are left alone; after Ctrl+C, a stop, or calmrun being
/// killed, the whole tree is ended.
pub fn run(
    mut cmd: Command,
    low_cpu: bool,
    on_spawn: impl FnOnce(u32),
    watch: &mut dyn FnMut(u32) -> bool,
) -> io::Result<Ended> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::{BELOW_NORMAL_PRIORITY_CLASS, CREATE_SUSPENDED};
        let job = win::Job::new();
        let mut flags = CREATE_SUSPENDED;
        if low_cpu {
            flags |= BELOW_NORMAL_PRIORITY_CLASS;
        }
        cmd.creation_flags(flags);
        let mut child = cmd.spawn()?;
        if let Some(j) = &job {
            j.assign(child.as_raw_handle() as _);
        }
        win::resume_threads(child.id());
        on_spawn(child.id());
        // Ctrl+C reaches the child through the shared console; we only watch.
        let mut stopped = false;
        let status = loop {
            if let Some(st) = child.try_wait()? {
                break st;
            }
            if !stopped && watch(child.id()) {
                stopped = true;
                match &job {
                    Some(j) => j.terminate(137),
                    None => {
                        let _ = child.kill();
                    }
                }
            }
            std::thread::sleep(WATCH_EVERY);
        };
        let interrupted = INTERRUPTED.load(Ordering::SeqCst);
        if let Some(j) = &job {
            if !interrupted && !stopped {
                j.release();
            }
        }
        let mut code = code_of(status);
        if stopped {
            code = 137;
        } else if interrupted && code == STATUS_CONTROL_C_EXIT {
            code = 130;
        }
        Ok(Ended { code, stopped })
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
        let mut stopped_at: Option<std::time::Instant> = None;
        let status = loop {
            match child.try_wait() {
                Ok(Some(st)) => break Ok(st),
                Ok(None) => {}
                Err(e) => break Err(e),
            }
            match stopped_at {
                None if watch(child.id()) => {
                    // SAFETY: signalling the process group we created.
                    unsafe {
                        libc::killpg(pid, libc::SIGTERM);
                    }
                    stopped_at = Some(std::time::Instant::now());
                }
                Some(t) if t.elapsed() > std::time::Duration::from_secs(3) => {
                    // SAFETY: as above.
                    unsafe {
                        libc::killpg(pid, libc::SIGKILL);
                    }
                }
                _ => {}
            }
            std::thread::sleep(WATCH_EVERY);
        };
        CHILD_PGID.store(0, Ordering::SeqCst);
        let stopped = stopped_at.is_some();
        // SAFETY: plain syscalls on our own process group / terminal.
        unsafe {
            if tty {
                libc::signal(libc::SIGTTOU, libc::SIG_IGN);
                libc::tcsetpgrp(0, libc::getpgrp());
                libc::signal(libc::SIGTTOU, libc::SIG_DFL);
            }
            // After Ctrl+C or a stop, leftovers of the command's group go too. After a normal
            // exit they stay: they are daemons the command meant to leave running.
            if stopped || INTERRUPTED.load(Ordering::SeqCst) {
                libc::killpg(pid, libc::SIGKILL);
            }
        }
        let code = if stopped { 137 } else { code_of(status?) };
        Ok(Ended { code, stopped })
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
