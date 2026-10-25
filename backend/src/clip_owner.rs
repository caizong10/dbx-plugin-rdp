//! Deferred-rendering clipboard owner (mstsc-style), Windows-only.
//!
//! WHY: the RDP server advertises a folder/multi-file copy as a bare
//! prelude, and every immediate FormatDataRequest against it returns
//! CB_RESPONSE_FAIL — the server-side Explorer has not rendered its
//! clipboard yet. mstsc succeeds against the same server by never pulling
//! on announcement: it writes a DELAYED-RENDER CF_HDROP entry
//! (SetClipboardData(CF_HDROP, NULL)) and only asks the server for data at
//! the moment the local user pastes, when Windows delivers
//! WM_RENDERFORMAT to the clipboard owner. That request lands after
//! rendering completed and succeeds.
//!
//! - one invisible Win32 window (NOT message-only: owners must receive
//!   WM_RENDERFORMAT) on its own thread with a classic message pump;
//! - `offer(armed)` writes the delayed HDROP + "Preferred DropEffect=COPY";
//! - WM_RENDERFORMAT(CF_HDROP) runs the armed callback (the synchronous
//!   RDP pull — blocking the owner thread briefly is what mstsc does),
//!   and writes a real HDROP built from the returned paths;
//! - a failed render writes an empty HDROP so Explorer unblocks.

#![allow(non_snake_case)]

use std::path::PathBuf;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW,
    TranslateMessage, CS_HREDRAW, CS_VREDRAW, MSG, WNDCLASSW, WM_RENDERALLFORMATS,
    WM_RENDERFORMAT, WS_EX_NOACTIVATE, WS_OVERLAPPED, WS_POPUP,
};

pub const CF_HDROP: u32 = 15;

/// Callback armed by `offer()`; runs on the owner thread the FIRST time a
/// local paste asks for the data. The returned paths are cached and served
/// to every later WM_RENDERFORMAT for the same clipboard generation —
/// delayed rendering is a REPEATABLE contract (mstsc answers every request;
/// a one-shot callback answered the menu-probe and left the actual paste
/// empty, which is exactly the observed failure).
pub type RenderCallback = Box<dyn FnOnce() -> Option<Vec<PathBuf>> + Send>;

static WINDOW: OnceLock<HWND> = OnceLock::new();
static ARMED: std::sync::Mutex<Option<RenderCallback>> = std::sync::Mutex::new(None);
/// Paths produced by the first render of the current generation; replayed
/// on every subsequent WM_RENDERFORMAT until a new offer() clears it.
static RENDERED: std::sync::Mutex<Option<Vec<PathBuf>>> = std::sync::Mutex::new(None);
/// When the current generation's first render completed. Ephemeral echo
/// suppression: the server re-announces our own paste back at us within a
/// couple of seconds; a LATER file announcement is a genuine new copy and
/// must re-arm (which clears the render cache).
static RENDERED_AT: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
/// True while our delayed CF_HDROP sits on the clipboard. Readers must NOT
/// open the clipboard for CF_HDROP in this state: GetClipboardData against
/// our own delayed entry fires WM_RENDERFORMAT synchronously on the owner
/// thread, which would try to talk to the session loop, which is the very
/// thread that is asking — instant deadlock.
static OFFER_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static VIRTUAL_OWNER: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
static VIRTUAL_SEQUENCE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
thread_local! {
    static VIRTUAL_OBJECT: std::cell::RefCell<Option<(u64, windows::Win32::System::Com::IDataObject)>> = const { std::cell::RefCell::new(None) };
}

/// Whether a delayed offer is currently on the clipboard.
pub fn offer_active() -> bool {
    let owner = VIRTUAL_OWNER.load(std::sync::atomic::Ordering::Acquire);
    if owner != 0 {
        return unsafe { windows_sys::Win32::System::DataExchange::GetClipboardOwner() == owner && windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() == VIRTUAL_SEQUENCE.load(std::sync::atomic::Ordering::Acquire) };
    }
    OFFER_ACTIVE.load(std::sync::atomic::Ordering::Relaxed)
}

/// The hybrid background pull placed REAL data on the clipboard (replacing
/// the delayed entry). Subsequent pastes read it directly; disarm the
/// delayed machinery so a stray WM_RENDERFORMAT cannot clobber it with an
/// empty replay. The staged paths also become the render cache: any later
/// WM_RENDERFORMAT (e.g. a paste while the quiet window expired) serves
/// these instead of an empty drop.
pub fn mark_background_published(paths: &[std::path::PathBuf]) {
    OFFER_ACTIVE.store(false, std::sync::atomic::Ordering::Relaxed);
    if let Ok(mut slot) = ARMED.lock() {
        *slot = None;
    }
    if let Ok(mut slot) = RENDERED.lock() {
        *slot = Some(paths.to_vec());
    }
    if let Ok(mut slot) = RENDERED_AT.lock() {
        *slot = Some(std::time::Instant::now());
    }
}

/// Whether a render already happened for the current generation AND it is
/// recent (within `window`). Used by the session loop to ignore the
/// server's echo announcement right after a local paste: re-arming would
/// EmptyClipboard and destroy what the user just pasted with. An older
/// render means a genuine new remote copy arrived; the caller then
/// re-arms normally (which clears the cache).
pub fn render_recent(window_ms: u128) -> bool {
    let rendered = RENDERED.lock().map(|slot| slot.is_some()).unwrap_or(false);
    if !rendered {
        return false;
    }
    RENDERED_AT
        .lock()
        .ok()
        .and_then(|slot| *slot)
        .map(|at| at.elapsed().as_millis() < window_ms)
        .unwrap_or(false)
}

/// Custom messages for the owner thread.
const WM_OFFER: u32 = 0x0400 + 0x500;
const WM_VIRTUAL: u32 = WM_OFFER + 1;
const WM_CANCEL_VIRTUAL: u32 = WM_OFFER + 2;

pub fn offer_virtual(generation: u64, source: crate::clip_virtual::VirtualClipboard) -> bool {
    ensure_owner();
    let Some(&hwnd) = WINDOW.get() else { return false; };
    let source = Box::new((generation, source));
    let pointer = Box::into_raw(source);
    // Synchronous registration only; GetData never downloads metadata.
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW(hwnd, WM_VIRTUAL, 0, pointer as isize) != 0 }
}

pub fn cancel_virtual(generation: u64) {
    if let Some(&hwnd) = WINDOW.get() {
        // u64 travels by pointer, also valid for 32-bit Windows builds.
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW(hwnd, WM_CANCEL_VIRTUAL, 0, &generation as *const u64 as isize); }
    }
}

extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_VIRTUAL => {
            use windows::Win32::System::Ole::OleSetClipboard;
            let source = unsafe { Box::from_raw(lparam as *mut (u64, crate::clip_virtual::VirtualClipboard)) };
            let object = source.1.data_object();
            match unsafe { OleSetClipboard(&object) } {
                Ok(()) => {
                    VIRTUAL_OBJECT.with(|slot| *slot.borrow_mut() = Some((source.0, object)));
                    VIRTUAL_OWNER.store(unsafe { windows_sys::Win32::System::DataExchange::GetClipboardOwner() }, std::sync::atomic::Ordering::Release);
                    VIRTUAL_SEQUENCE.store(unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() }, std::sync::atomic::Ordering::Release);
                    OFFER_ACTIVE.store(false, std::sync::atomic::Ordering::Relaxed);
                    crate::diag::note(&format!("virtual clipboard offered generation={}", source.0));
                    1
                }
                Err(error) => { crate::diag::note(&format!("virtual clipboard registration failed: {error}")); 0 }
            }
        }
        WM_CANCEL_VIRTUAL => {
            use windows::Win32::System::Ole::OleSetClipboard;
            use windows::core::Interface;
            #[link(name = "ole32")]
            extern "system" { fn OleIsCurrentClipboard(object: *mut std::ffi::c_void) -> i32; }
            let generation = unsafe { *(lparam as *const u64) };
            VIRTUAL_OBJECT.with(|slot| {
                let mut slot = slot.borrow_mut();
                if slot.as_ref().is_some_and(|(id, _)| generation == 0 || *id == generation) {
                    if let Some((_, object)) = slot.as_ref() {
                        if offer_active() && unsafe { OleIsCurrentClipboard(object.as_raw()) == 0 } {
                            let _ = unsafe { OleSetClipboard(None::<&windows::Win32::System::Com::IDataObject>) };
                        }
                    }
                    *slot = None;
                    VIRTUAL_OWNER.store(0, std::sync::atomic::Ordering::Release);
                }
            });
            0
        }
        WM_OFFER => {
            // Runs on the owner thread: the entire Open/Empty/Set/Close
            // sequence must happen on the window's own thread, or
            // SetClipboardData(delayed) misbehaves (observed failure when
            // driven from the session thread).
            unsafe {
                if OpenClipboard(hwnd) == 0 {
                    crate::diag::note("clip owner offer: OpenClipboard failed");
                    return 0;
                }
                let _ = EmptyClipboard();
                let delayed = SetClipboardData(CF_HDROP, 0);
                // PROBE FACT (verified live on this system): the delayed
                // SetClipboardData RETURNS NULL while the registration
                // actually succeeds — IsClipboardFormatAvailable reports the
                // format present immediately after. Availability, not the
                // returned handle, is the source of truth.
                let registered = if delayed != 0 {
                    true
                } else {
                    let avail = IsClipboardFormatAvailable(CF_HDROP);
                    crate::diag::note(&format!(
                        "clip owner offer: SetClipboardData returned 0; availability = {avail}"
                    ));
                    avail != 0
                };
                if !registered {
                    CloseClipboard();
                    crate::diag::note("clip owner offer: delayed entry NOT registered");
                    if let Ok(mut slot) = ARMED.lock() {
                        *slot = None;
                    }
                    if let Ok(mut slot) = RENDERED.lock() {
                        *slot = None;
                    }
                    OFFER_ACTIVE.store(false, std::sync::atomic::Ordering::Relaxed);
                    return 0;
                }
                // New generation: drop the previous render cache.
                if let Ok(mut slot) = RENDERED.lock() {
                    *slot = None;
                }
                if let Ok(mut slot) = RENDERED_AT.lock() {
                    *slot = None;
                }
                OFFER_ACTIVE.store(true, std::sync::atomic::Ordering::Relaxed);
                let name: Vec<u16> = "Preferred DropEffect\0".encode_utf16().collect();
                let effect_id = RegisterClipboardFormatW(name.as_ptr());
                if effect_id != 0 {
                    if let Some(hglobal) = build_drop_effect() {
                        SetClipboardData(effect_id, hglobal);
                    }
                }
                CloseClipboard();
            }
            crate::diag::note("clip owner: delayed CF_HDROP offered");
            0
        }
        WM_RENDERFORMAT => {
            if wparam as u32 == CF_HDROP {
                OFFER_ACTIVE.store(false, std::sync::atomic::Ordering::Relaxed);
                // ZERO-BLOCK render (the fix for the folder deadlock):
                // the render request must answer within milliseconds or
                // Windows holds the clipboard against everyone (observed
                // OPEN FAILED during the paste window). The background pull
                // (armed on every remote announcement) stages files ahead of
                // time; the render only serves what is already there.
                //   - Data staged -> write the real HDROP now.
                //   - Nothing staged yet -> write the PREVIOUS generation's
                //     files if present, else an empty drop (Explorer unblocks
                //     instantly; the background pull will replace it).
                // A blocking pull inside WM_RENDERFORMAT (the old design)
                // starved the session loop and deadlocked the clipboard for
                // the whole download window.
                let list = {
                    let cached = RENDERED
                        .lock()
                        .map(|slot| slot.as_ref().map(|v| v.to_vec()))
                        .ok()
                        .flatten();
                    match cached {
                        Some(paths) => {
                            crate::diag::note(&format!(
                                "render: serving {} staged file(s)",
                                paths.len()
                            ));
                            paths
                        }
                        None => {
                            // No staged data: do NOT block. Disarm and let
                            // the background path publish when it lands.
                            if let Ok(mut slot) = ARMED.lock() {
                                *slot = None;
                            }
                            crate::diag::note("render: nothing staged yet, empty drop (background pull continues)");
                            Vec::new()
                        }
                    }
                };
                unsafe {
                    if let Some(hglobal) = build_hdrop(&list) {
                        SetClipboardData(CF_HDROP, hglobal);
                        crate::diag::note(&format!(
                            "render: wrote HDROP with {} file(s)",
                            list.len()
                        ));
                    }
                }
            }
            0
        }
        WM_RENDERALLFORMATS => 0,
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// DROPFILES (shellapi.h): DWORD pFiles; POINT pt; BOOL fNC; BOOL fWide.
#[repr(C)]
struct DropFiles {
    p_files: u32,
    pt_x: i32,
    pt_y: i32,
    f_nc: i32,
    f_wide: i32,
}

/// Build a CF_HDROP global-memory block, returned as the isize handle
/// Windows' SetClipboardData expects. Ownership transfers to Windows.
unsafe fn build_hdrop(paths: &[PathBuf]) -> Option<isize> {
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

    const DROPFILES_SIZE: usize = std::mem::size_of::<DropFiles>();
    let mut wide = Vec::<u16>::new();
    for path in paths {
        wide.extend(path.to_string_lossy().encode_utf16());
        wide.push(0);
    }
    wide.push(0); // list terminator

    let hglobal = GlobalAlloc(GMEM_MOVEABLE, DROPFILES_SIZE + wide.len() * 2);
    if hglobal.is_null() {
        return None;
    }
    let base = GlobalLock(hglobal) as *mut u8;
    if base.is_null() {
        return None;
    }
    unsafe {
        *(base as *mut DropFiles) = DropFiles {
            p_files: DROPFILES_SIZE as u32,
            pt_x: 0,
            pt_y: 0,
            f_nc: 0,
            f_wide: 1,
        };
        let names = base.add(DROPFILES_SIZE) as *mut u16;
        for (i, unit) in wide.iter().enumerate() {
            *names.add(i) = *unit;
        }
        GlobalUnlock(hglobal);
    }
    Some(hglobal as isize)
}

fn owner_thread() {
    unsafe {
        if let Err(error) = windows::Win32::System::Ole::OleInitialize(None) {
            crate::diag::note(&format!("clipboard OLE initialization failed: {error}"));
            return;
        }
        let hinstance = GetModuleHandleW(std::ptr::null());
        let class_name: Vec<u16> = "DBXRDPClipOwner\0".encode_utf16().collect();
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance,
            lpszClassName: class_name.as_ptr(),
            hCursor: 0,
            hIcon: 0,
            hbrBackground: 0,
            cbClsExtra: 0,
            cbWndExtra: 0,
            lpszMenuName: std::ptr::null(),
        };
        let atom = RegisterClassW(&class);
        if atom == 0 {
            crate::diag::note("clip owner: RegisterClassW failed");
            return;
        }
        // Invisible real window (WS_POPUP without visible style). NOT
        // message-only: the clipboard owner must receive WM_RENDERFORMAT.
        let hwnd = CreateWindowExW(
            WS_EX_NOACTIVATE,
            atom as usize as *const u16,
            class_name.as_ptr(),
            WS_POPUP | WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            0,
            0,
            hinstance,
            std::ptr::null(),
        );
        if hwnd == 0 {
            crate::diag::note("clip owner: CreateWindowExW failed");
            return;
        }
        let _ = WINDOW.set(hwnd);
        crate::diag::note("clip owner: window ready");

        let mut msg: MSG = std::mem::zeroed();
        loop {
            let r = GetMessageW(&mut msg, 0, 0, 0);
            if r <= 0 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        windows::Win32::System::Ole::OleUninitialize();
    }
}

/// Spawn the clipboard-owner thread (idempotent, once per process).
pub fn ensure_owner() {
    static START: OnceLock<()> = OnceLock::new();
    START.get_or_init(|| {
        let _ = std::thread::Builder::new()
            .name("dbx-rdp-clip-owner".to_string())
            .spawn(owner_thread);
        // Give the window a moment to exist before the first offer.
        std::thread::sleep(std::time::Duration::from_millis(300));
    });
}

/// Offer a pending remote file copy as a delayed-render CF_HDROP, arming a
/// callback (legacy path — the render still calls it if no staged data
/// exists; new code should prefer `offer_noop` + background pull).
pub fn offer(armed: RenderCallback) -> bool {
    if let Ok(mut slot) = ARMED.lock() {
        *slot = Some(armed);
    }
    post_offer()
}

/// Arm the delayed CF_HDROP with NO render callback: WM_RENDERFORMAT serves
/// only staged data (RENDERED cache) or an empty drop, never blocks. The
/// background pull publishes the real HDROP independently.
pub fn offer_noop() -> bool {
    if let Ok(mut slot) = ARMED.lock() {
        *slot = None;
    }
    post_offer()
}

fn post_offer() -> bool {
    ensure_owner();
    let Some(&hwnd) = WINDOW.get() else { return false };
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;
        if PostMessageW(hwnd, WM_OFFER, 0, 0) == 0 {
            crate::diag::note("clip owner offer: PostMessageW failed");
            return false;
        }
    }
    true
}

unsafe fn build_drop_effect() -> Option<isize> {
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    let hglobal = GlobalAlloc(GMEM_MOVEABLE, 4);
    if hglobal.is_null() {
        return None;
    }
    let ptr = GlobalLock(hglobal) as *mut u32;
    if ptr.is_null() {
        return None;
    }
    *ptr = 1; // DROPEFFECT_COPY
    GlobalUnlock(hglobal);
    Some(hglobal as isize)
}

/// Drop a pending offer without touching the clipboard (used when the
/// remote clipboard changes before any local paste happened).
pub fn cancel() {
    if let Ok(mut slot) = ARMED.lock() {
        *slot = None;
    }
    if let Ok(mut slot) = RENDERED.lock() {
        *slot = None;
    }
    if let Ok(mut slot) = RENDERED_AT.lock() {
        *slot = None;
    }
}
