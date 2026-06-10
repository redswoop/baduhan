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

        let mut child = pair.slave.spawn_command(cmd)?;

        // Put the shell (and thus everything it spawns) in a kill-on-close job.
        let job = Job::new().ok();
        if let (Some(job), Some(handle)) = (&job, child.as_raw_handle()) {
            unsafe {
                let _ = AssignProcessToJobObject(job.0, HANDLE(handle));
            }
        }

        let killer = child.clone_killer();
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

impl Drop for Pty {
    fn drop(&mut self) {
        // Job handle drop (KILL_ON_JOB_CLOSE) reaps the tree; kill() is belt
        // and suspenders for the direct child.
        if let Ok(mut killer) = self.killer.lock() {
            let _ = killer.kill();
        }
    }
}
