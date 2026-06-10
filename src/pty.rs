//! ConPTY session: pseudo-console, shell process, job object, reader thread.
//!
//! Every pane gets its own job object with KILL_ON_JOB_CLOSE so the entire
//! child process tree is torn down when the pane closes, no orphans.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

/// Owns a job object handle; closing it (on drop) kills the whole tree.
struct Job(HANDLE);
unsafe impl Send for Job {}

impl Job {
    fn new() -> Result<Self> {
        unsafe {
            let job = CreateJobObjectW(None, windows::core::PCWSTR::null())?;
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )?;
            Ok(Job(job))
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub struct Pty {
    master: Box<dyn MasterPty + Send>,
    pub writer: Arc<Mutex<Box<dyn Write + Send>>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    /// PID of the spawned shell — used to read its live cwd for drag & drop.
    pub child_pid: Option<u32>,
    _job: Option<Job>,
}

impl Pty {
    /// Spawn `command` (argv: exe + args) in a fresh ConPTY. `on_output` runs
    /// on a background reader thread for every chunk; `on_exit` runs once
    /// when the child dies.
    pub fn spawn(
        command: &[String],
        cwd: Option<&str>,
        cols: u16,
        rows: u16,
        extra_env: &[(String, String)],
        on_output: impl FnMut(&[u8]) + Send + 'static,
        on_exit: impl FnOnce() + Send + 'static,
    ) -> Result<Pty> {
        anyhow::ensure!(!command.is_empty(), "empty command");
        let pair = native_pty_system().openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(&command[0]);
        cmd.args(&command[1..]);
        if let Some(cwd) = cwd {
            cmd.cwd(cwd);
        } else if let Some(home) = std::env::var_os("USERPROFILE") {
            cmd.cwd(home);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        for (k, v) in extra_env {
            cmd.env(k, v);
        }

        let mut child = pair.slave.spawn_command(cmd)?;

        // Put the shell (and thus everything it spawns) in a kill-on-close job.
        let job = Job::new().ok();
        if let (Some(job), Some(handle)) = (&job, child.as_raw_handle()) {
            unsafe {
                let _ = AssignProcessToJobObject(job.0, HANDLE(handle));
            }
        }

        let killer = child.clone_killer();
        let child_pid = child.process_id();
        let mut reader = pair.master.try_clone_reader()?;
        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));

        // ConPTY quirk: the output pipe does NOT hit EOF when the child
        // exits (conhost keeps it open until the pseudo-console closes),
        // so exit detection must wait on the process itself.
        std::thread::Builder::new().name("pty-wait".into()).spawn(move || {
            let _ = child.wait();
            on_exit();
        })?;

        {
            let mut on_output = on_output;
            std::thread::Builder::new().name("pty-reader".into()).spawn(move || {
                let mut buf = [0u8; 64 * 1024];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => on_output(&buf[..n]),
                    }
                }
            })?;
        }

        Ok(Pty {
            master: pair.master,
            writer,
            killer: Mutex::new(killer),
            child_pid,
            _job: job,
        })
    }

    pub fn write(&self, bytes: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
    }
}

/// Read a live process's current directory from its PEB
/// (PEB → ProcessParameters → CurrentDirectory.DosPath). x64-to-x64 only,
/// which is all we spawn. Returns None for anything we can't read — callers
/// fall back to path-pasting.
pub fn process_cwd(pid: u32) -> Option<String> {
    use windows::core::{s, w};
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
    };

    #[repr(C)]
    struct ProcessBasicInformation {
        exit_status: isize,
        peb_base: usize,
        affinity_mask: usize,
        base_priority: isize,
        unique_pid: usize,
        parent_pid: usize,
    }
    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum: u16,
        _pad: u32,
        buffer: usize,
    }
    type NtQip = unsafe extern "system" fn(
        windows::Win32::Foundation::HANDLE,
        u32,
        *mut core::ffi::c_void,
        u32,
        *mut u32,
    ) -> i32;

    unsafe {
        let ntdll = GetModuleHandleW(w!("ntdll.dll")).ok()?;
        let f = GetProcAddress(ntdll, s!("NtQueryInformationProcess"))?;
        let nt_query: NtQip = std::mem::transmute(f);

        let h = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid).ok()?;
        let result = (|| -> Option<String> {
            let mut pbi: ProcessBasicInformation = std::mem::zeroed();
            if nt_query(
                h,
                0, // ProcessBasicInformation
                &mut pbi as *mut _ as *mut _,
                std::mem::size_of::<ProcessBasicInformation>() as u32,
                std::ptr::null_mut(),
            ) != 0
            {
                return None;
            }
            // x64 PEB: ProcessParameters at +0x20;
            // RTL_USER_PROCESS_PARAMETERS: CurrentDirectory.DosPath at +0x38.
            let mut params: usize = 0;
            ReadProcessMemory(
                h,
                (pbi.peb_base + 0x20) as *const _,
                &mut params as *mut _ as *mut _,
                std::mem::size_of::<usize>(),
                None,
            )
            .ok()?;
            let mut us: UnicodeString = std::mem::zeroed();
            ReadProcessMemory(
                h,
                (params + 0x38) as *const _,
                &mut us as *mut _ as *mut _,
                std::mem::size_of::<UnicodeString>(),
                None,
            )
            .ok()?;
            let chars = (us.length / 2) as usize;
            if chars == 0 || chars > 4096 {
                return None;
            }
            let mut buf = vec![0u16; chars];
            ReadProcessMemory(
                h,
                us.buffer as *const _,
                buf.as_mut_ptr() as *mut _,
                us.length as usize,
                None,
            )
            .ok()?;
            let s = String::from_utf16_lossy(&buf);
            // The PEB stores it with a trailing backslash (except drive roots).
            let trimmed = if s.len() > 3 { s.trim_end_matches('\\').to_string() } else { s };
            Some(trimmed)
        })();
        let _ = CloseHandle(h);
        result
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // Job handle drop (KILL_ON_JOB_CLOSE) reaps the tree; kill() is belt
        // and suspenders for the direct child.
        if let Ok(mut killer) = self.killer.lock() {
            let _ = killer.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_cwd_reads_own_peb() {
        // Validates the PEB/ProcessParameters offsets against ourselves.
        let cwd = process_cwd(std::process::id()).expect("read own cwd");
        let expected = std::env::current_dir().unwrap();
        assert_eq!(
            cwd.to_ascii_lowercase().trim_end_matches('\\'),
            expected.to_string_lossy().to_ascii_lowercase().trim_end_matches('\\')
        );
    }
}
