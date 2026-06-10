//! OLE drag & drop onto panes.
//!
//! Plain drop pastes the quoted path(s) into the shell (flavored: "C:\…" for
//! pwsh/cmd, 'C:/…' for git-bash, '/mnt/c/…' for WSL). Hold **Ctrl** to copy
//! the files into the shell's live working directory, **Shift** to move them
//! there — the cwd is read from the shell process itself, so it follows your
//! `cd`s. Dropping on a browser pane opens the file.

use windows::core::implement;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, POINTL, WPARAM};
use windows::Win32::System::Com::{IDataObject, FORMATETC, DVASPECT_CONTENT, TYMED_HGLOBAL};
use windows::Win32::System::Ole::{
    IDropTarget, IDropTarget_Impl, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop,
    CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::{MK_CONTROL, MK_SHIFT, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

use crate::app;

/// What the user asked for with modifier keys.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DropOp {
    PastePath,
    Copy,
    Move,
}

impl DropOp {
    fn from_keys(keys: MODIFIERKEYS_FLAGS) -> DropOp {
        let ctrl = keys.0 & MK_CONTROL.0 != 0;
        let shift = keys.0 & MK_SHIFT.0 != 0;
        if shift {
            DropOp::Move
        } else if ctrl {
            DropOp::Copy
        } else {
            DropOp::PastePath
        }
    }

    fn effect(self) -> DROPEFFECT {
        match self {
            DropOp::PastePath => DROPEFFECT_LINK,
            DropOp::Copy => DROPEFFECT_COPY,
            DropOp::Move => DROPEFFECT_MOVE,
        }
    }
}

#[implement(IDropTarget)]
pub struct DropTarget {
    hwnd: HWND,
}

pub fn register(hwnd: HWND) {
    let target: IDropTarget = DropTarget { hwnd }.into();
    unsafe {
        if let Err(e) = RegisterDragDrop(hwnd, &target) {
            eprintln!("RegisterDragDrop failed: {e:?}");
        }
    }
}

pub fn revoke(hwnd: HWND) {
    unsafe {
        let _ = RevokeDragDrop(hwnd);
    }
}

impl DropTarget {
    /// Effect for the current cursor position + keys (NONE off panes).
    fn effect_at(&self, pt: &POINTL, keys: MODIFIERKEYS_FLAGS) -> DROPEFFECT {
        let mut p = POINT { x: pt.x, y: pt.y };
        unsafe {
            let _ = windows::Win32::Graphics::Gdi::ScreenToClient(self.hwnd, &mut p);
        }
        app::with_window(self.hwnd, |w| w.drop_effect_at(p.x, p.y, DropOp::from_keys(keys)))
            .unwrap_or(DROPEFFECT_NONE)
    }
}

impl IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(
        &self,
        _data: windows::core::Ref<'_, IDataObject>,
        keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        unsafe {
            *effect = self.effect_at(pt, keys);
        }
        Ok(())
    }

    fn DragOver(
        &self,
        keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        unsafe {
            *effect = self.effect_at(pt, keys);
        }
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn Drop(
        &self,
        data: windows::core::Ref<'_, IDataObject>,
        keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let op = DropOp::from_keys(keys);
        unsafe {
            *effect = DROPEFFECT_NONE;
        }
        let Some(data) = data.as_ref() else { return Ok(()) };
        let paths = extract_paths(data);
        if paths.is_empty() {
            return Ok(());
        }
        let mut p = POINT { x: pt.x, y: pt.y };
        unsafe {
            let _ = windows::Win32::Graphics::Gdi::ScreenToClient(self.hwnd, &mut p);
            *effect = op.effect();
        }
        // Defer to the message loop so the drag source isn't blocked while
        // we (potentially) run a shell file operation.
        let payload = Box::new((paths, op, p));
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                Some(self.hwnd),
                crate::window::WM_APP_DROP_FILES,
                WPARAM(Box::into_raw(payload) as usize),
                LPARAM(0),
            );
        }
        Ok(())
    }
}

/// Pull CF_HDROP file paths out of the data object.
fn extract_paths(data: &IDataObject) -> Vec<String> {
    let fmt = FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    let mut out = Vec::new();
    unsafe {
        let Ok(mut stg) = data.GetData(&fmt) else { return out };
        let hdrop = HDROP(stg.u.hGlobal.0);
        let count = DragQueryFileW(hdrop, 0xFFFF_FFFF, None);
        for i in 0..count {
            let len = DragQueryFileW(hdrop, i, None);
            if len == 0 {
                continue;
            }
            let mut buf = vec![0u16; len as usize + 1];
            let n = DragQueryFileW(hdrop, i, Some(&mut buf));
            if n > 0 {
                out.push(String::from_utf16_lossy(&buf[..n as usize]));
            }
        }
        ReleaseStgMedium(&mut stg);
    }
    out
}
