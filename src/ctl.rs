//! Shell → browser control channel. Running `baduhan browse <url>` (or
//! `reload` / `devtools`) inside a baduhan pane targets the browser pane of
//! that pane's tab — the classic "dev server on the left, browser on the
//! right" loop, scriptable from the shell.
//!
//! Transport: the spawning pane gets BADUHAN_PANE/BADUHAN_EXE in its
//! environment (plus WSLENV plumbing so WSL shells inherit them); the CLI
//! sends WM_COPYDATA with a JSON payload to every BaduhanMainWindow until
//! one claims ownership of the pane.

use serde::{Deserialize, Serialize};

pub const CTL_MAGIC: usize = 0xBAD0_0001;

#[derive(Serialize, Deserialize)]
pub struct CtlReq {
    pub pane: u64,
    pub verb: String,
    #[serde(default)]
    pub arg: String,
}

/// Client mode: parse args, find the owning window, deliver, exit.
pub fn run(args: &[String]) -> ! {
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe {
        // Release builds use the windows subsystem; reattach to the calling
        // shell's console so usage/errors are visible.
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }

    let usage = || -> ! {
        eprintln!(
            "usage (inside a baduhan pane):\n  \
             baduhan browse <url>   load a URL in this tab's browser pane\n  \
             baduhan reload         reload this tab's browser pane\n  \
             baduhan devtools       open DevTools for this tab's browser pane"
        );
        std::process::exit(2);
    };

    let verb = args.first().map(String::as_str).unwrap_or("");
    let (verb, arg) = match verb {
        "browse" | "open" => {
            let Some(url) = args.get(1) else { usage() };
            ("browse", url.clone())
        },
        "reload" => ("reload", String::new()),
        "devtools" => ("devtools", String::new()),
        _ => usage(),
    };

    let Some(pane) = std::env::var("BADUHAN_PANE").ok().and_then(|s| s.parse::<u64>().ok())
    else {
        eprintln!("baduhan {verb}: BADUHAN_PANE not set — run this inside a baduhan pane");
        std::process::exit(1);
    };

    let payload =
        serde_json::to_string(&CtlReq { pane, verb: verb.into(), arg }).unwrap_or_default();
    if deliver(pane, payload.as_bytes()) {
        std::process::exit(0);
    }
    eprintln!("baduhan {verb}: no running baduhan window owns pane {pane}");
    std::process::exit(1);
}

/// Send the payload to each baduhan top-level window until one handles it.
fn deliver(_pane: u64, payload: &[u8]) -> bool {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::System::DataExchange::COPYDATASTRUCT;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, SendMessageW, WM_COPYDATA,
    };

    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        unsafe {
            let mut name = [0u16; 64];
            let n = GetClassNameW(hwnd, &mut name);
            if n > 0 && String::from_utf16_lossy(&name[..n as usize]) == "BaduhanMainWindow" {
                let v = &mut *(lparam.0 as *mut Vec<isize>);
                v.push(hwnd.0 as isize);
            }
            BOOL(1)
        }
    }

    let mut windows_found: Vec<isize> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut windows_found as *mut _ as isize));
    }
    for h in windows_found {
        let cds = COPYDATASTRUCT {
            dwData: CTL_MAGIC,
            cbData: payload.len() as u32,
            lpData: payload.as_ptr() as *mut _,
        };
        let res = unsafe {
            SendMessageW(
                HWND(h as *mut _),
                WM_COPYDATA,
                Some(WPARAM(0)),
                Some(LPARAM(&cds as *const _ as isize)),
            )
        };
        if res.0 != 0 {
            return true;
        }
    }
    false
}
