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
             baduhan view <image>   show an image inline (PNG/JPEG/GIF/BMP)"
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
            let Some(path) = args.get(1) else { usage() };
            match view_image(path) {
                Ok(()) => std::process::exit(0),
                Err(e) => {
                    eprintln!("baduhan view: {e}");
                    std::process::exit(1);
                },
            }
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

/// Emit an image as an iTerm2 inline-image escape on stdout (imgcat clone).
/// Non-PNG inputs are transcoded to PNG via WIC so the terminal side only
/// ever parses one container.
fn view_image(path: &str) -> anyhow::Result<()> {
    use std::io::Write;
    let bytes = std::fs::read(path)?;
    let png = if crate::images::png_dimensions(&bytes).is_some() {
        bytes
    } else {
        wic_to_png(&bytes)?
    };
    let mut stdout = std::io::stdout().lock();
    write!(
        stdout,
        "\x1b]1337;File=inline=1;size={}:{}\x07",
        png.len(),
        crate::images::b64_encode(&png)
    )?;
    stdout.flush()?;
    Ok(())
}

/// Transcode any WIC-decodable image to PNG in memory.
fn wic_to_png(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    use windows::core::Interface;
    use windows::Win32::Graphics::Imaging::*;
    use windows::Win32::System::Com::*;
    use windows::Win32::UI::Shell::SHCreateMemStream;
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let wic: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let in_stream =
            SHCreateMemStream(Some(bytes)).ok_or_else(|| anyhow::anyhow!("stream"))?;
        let decoder =
            wic.CreateDecoderFromStream(&in_stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)?;
        let frame = decoder.GetFrame(0)?;

        let out_stream =
            SHCreateMemStream(None).ok_or_else(|| anyhow::anyhow!("stream"))?;
        let encoder = wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
        encoder.Initialize(&out_stream, WICBitmapEncoderNoCache)?;
        let mut out_frame = None;
        encoder.CreateNewFrame(&mut out_frame, std::ptr::null_mut())?;
        let out_frame = out_frame.ok_or_else(|| anyhow::anyhow!("no frame"))?;
        out_frame.Initialize(None)?;
        out_frame.WriteSource(&frame.cast::<IWICBitmapSource>()?, std::ptr::null())?;
        out_frame.Commit()?;
        encoder.Commit()?;

        // Read the stream back.
        use windows::Win32::System::Com::STREAM_SEEK_SET;
        out_stream.Seek(0, STREAM_SEEK_SET, None)?;
        let mut out = Vec::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            let mut read = 0u32;
            let hr = out_stream.Read(
                buf.as_mut_ptr() as *mut _,
                buf.len() as u32,
                Some(&mut read),
            );
            if read == 0 {
                hr.ok()?;
                break;
            }
            out.extend_from_slice(&buf[..read as usize]);
        }
        Ok(out)
    }
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
