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
             baduhan devtools       open DevTools for this tab's browser pane\n  \
             baduhan cdp            print the Chrome DevTools Protocol endpoint\n  \
             baduhan view <image>   show an image inline (PNG/JPEG/GIF/BMP)\n\n  \
             settings (apply live to running terminals):\n  \
             baduhan theme <file>   import a color theme (.itermcolors or WT json)\n  \
             baduhan font <family>  set the font family\n  \
             baduhan fontsize <pt>  set the default font size"
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
        "view" | "imgcat" => {
            // imgcat semantics: files as args, or stdin when none / "-".
            let files = &args[1..];
            let result = if files.is_empty() || files == ["-"] {
                view_stdin()
            } else {
                files.iter().try_for_each(|f| view_image(f))
            };
            match result {
                Ok(()) => std::process::exit(0),
                Err(e) => {
                    eprintln!("baduhan view: {e}");
                    std::process::exit(1);
                },
            }
        },
        "theme" => {
            let Some(file) = args.get(1) else { usage() };
            let exit = match import_theme(file) {
                Ok(name) => {
                    println!("theme applied: {name}");
                    0
                },
                Err(e) => {
                    eprintln!("baduhan theme: {e}");
                    1
                },
            };
            std::process::exit(exit);
        },
        "font" => {
            let family = args[1..].join(" ");
            if family.is_empty() {
                usage()
            }
            let mut cfg = crate::config::Config::load_or_create();
            cfg.font_family = family.clone();
            let _ = cfg.save();
            println!("font: {family}");
            std::process::exit(0);
        },
        "fontsize" => {
            let Some(size) = args.get(1).and_then(|s| s.parse::<f32>().ok()) else { usage() };
            let mut cfg = crate::config::Config::load_or_create();
            cfg.font_size = size.clamp(7.0, 32.0);
            let _ = cfg.save();
            println!("font size: {}", cfg.font_size);
            std::process::exit(0);
        },
        "cdp" => {
            // No window round-trip needed; the port lives in the config.
            let port = crate::config::Config::load_or_create().browser_debug_port;
            if port == 0 {
                eprintln!("baduhan cdp: disabled (browser_debug_port = 0 in settings.json)");
                std::process::exit(1);
            }
            println!("http://127.0.0.1:{port}");
            std::process::exit(0);
        },
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

/// Import a color theme into settings.json. Accepts iTerm2 .itermcolors
/// (XML plist) and Windows Terminal scheme json — the two formats every
/// theme collection ships.
fn import_theme(file: &str) -> anyhow::Result<String> {
    let text = std::fs::read_to_string(file)?;
    let scheme = if text.trim_start().starts_with('{') {
        crate::config::parse_wt_scheme(&text)
    } else {
        crate::config::parse_itermcolors(&text)
    };
    let Some(scheme) = scheme else {
        anyhow::bail!("not a recognizable .itermcolors or Windows Terminal scheme: {file}");
    };
    let mut cfg = crate::config::Config::load_or_create();
    cfg.scheme = Some(scheme);
    cfg.save()?;
    let name = std::path::Path::new(file)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| file.to_string());
    Ok(name)
}

/// Emit an image as an iTerm2 inline-image escape on stdout — exactly what
/// iTerm2's imgcat does. Raw bytes pass through; the terminal sizes/decodes
/// any WIC-supported format (PNG/JPEG/GIF/BMP/WebP/…).
fn view_image(path: &str) -> anyhow::Result<()> {
    emit_inline(&std::fs::read(path)?)
}

fn view_stdin() -> anyhow::Result<()> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::io::stdin().lock().read_to_end(&mut bytes)?;
    emit_inline(&bytes)
}

fn emit_inline(bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    anyhow::ensure!(!bytes.is_empty(), "empty input");
    let mut stdout = std::io::stdout().lock();
    write!(
        stdout,
        "\x1b]1337;File=inline=1;size={}:{}\x07",
        bytes.len(),
        crate::images::b64_encode(bytes)
    )?;
    stdout.flush()?;
    Ok(())
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
