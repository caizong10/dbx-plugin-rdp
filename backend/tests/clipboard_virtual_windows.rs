//! Real OLE clipboard exercise on a private window station, never WinSta0.
#![cfg(windows)]

use dbx_plugin_rdp::clip_virtual::{VirtualClipboard, VirtualFileEntry};
use std::sync::{Arc, Mutex};
use windows::Win32::System::Com::*;
use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;
use windows::Win32::System::Ole::*;
use windows::Win32::System::Memory::*;
use windows::Win32::UI::Shell::FILEDESCRIPTORW;
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::core::PCWSTR;
use windows::core::Interface;
use windows::Win32::UI::Shell::IDataObjectAsyncCapability;

struct Station { previous: isize, private: isize }
#[link(name = "user32")]
extern "system" {
    fn CreateWindowStationW(name: *const u16, flags: u32, access: u32, security: *const std::ffi::c_void) -> isize;
    fn GetProcessWindowStation() -> isize;
    fn SetProcessWindowStation(station: isize) -> i32;
    fn CloseWindowStation(station: isize) -> i32;
}
impl Drop for Station {
    fn drop(&mut self) { unsafe { SetProcessWindowStation(self.previous); CloseWindowStation(self.private); } }
}
fn private_station() -> Station {
    unsafe {
        let previous = GetProcessWindowStation();
        let private = CreateWindowStationW(std::ptr::null(), 0, 0x037F, std::ptr::null());
        assert_ne!(private, 0);
        assert_ne!(SetProcessWindowStation(private), 0);
        Station { previous, private }
    }
}
fn format(name: &str, index: i32, tymed: TYMED) -> FORMATETC {
    let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    FORMATETC { cfFormat: unsafe { RegisterClipboardFormatW(PCWSTR(name.as_ptr())) } as u16,
        dwAspect: DVASPECT_CONTENT.0, lindex: index, tymed: tymed.0 as u32, ..Default::default() }
}

#[test]
fn virtual_file_metadata_is_immediate_and_contents_are_read_in_bounded_blocks() {
    let _station = private_station();
    unsafe { OleInitialize(None).unwrap(); }
    let calls = Arc::new(Mutex::new(Vec::new()));
    let observed = calls.clone();
    let clipboard = VirtualClipboard::new(vec![
        VirtualFileEntry { name: "folder".into(), relative_path: None, size: 0, is_directory: true },
        VirtualFileEntry { name: "large.bin".into(), relative_path: Some("folder".into()), size: 3 * 1024 * 1024, is_directory: false },
        VirtualFileEntry { name: "empty".into(), relative_path: Some("folder".into()), size: 0, is_directory: true },
    ], move |index, offset, length| {
        observed.lock().unwrap().push((index, offset, length));
        Ok(vec![0x5a; length as usize])
    }).unwrap();
    assert!(calls.lock().unwrap().is_empty(), "metadata must not predownload files");
    assert!(clipboard.formats().contains(&"FileGroupDescriptorW"));
    assert_eq!(clipboard.read_file(1, 0, 3 * 1024 * 1024).unwrap().len(), 1024 * 1024);
    calls.lock().unwrap().clear();
    let object = clipboard.data_object();
    unsafe {
        let asynchronous = object.cast::<IDataObjectAsyncCapability>().unwrap();
        assert!(asynchronous.GetAsyncMode().unwrap().as_bool());
        assert!(!asynchronous.InOperation().unwrap().as_bool());
        asynchronous.StartOperation(None::<&IBindCtx>).unwrap();
        assert!(asynchronous.InOperation().unwrap().as_bool());
        asynchronous.EndOperation(windows::Win32::Foundation::S_OK, None::<&IBindCtx>, 1).unwrap();
        assert!(!asynchronous.InOperation().unwrap().as_bool());
        OleSetClipboard(&object).unwrap();
        let received = OleGetClipboard().unwrap();
        let enumerator = received.EnumFormatEtc(DATADIR_GET.0 as u32).unwrap();
        let mut formats = [FORMATETC::default(); 3];
        let mut fetched = 0;
        enumerator.Next(&mut formats, Some(&mut fetched)).unwrap();
        assert_eq!(fetched, 3);
        assert!(formats.iter().any(|f| f.cfFormat == format("FileContents", 1, TYMED_ISTREAM).cfFormat));
        let mut medium = received.GetData(&format("FileGroupDescriptorW", -1, TYMED_HGLOBAL)).unwrap();
        let base = GlobalLock(medium.u.hGlobal) as *const u8;
        assert!(!base.is_null());
        assert_eq!(base.cast::<u32>().read_unaligned(), 3);
        let mut paths = Vec::new();
        let mut directories = Vec::new();
        let mut sizes = Vec::new();
        for i in 0..3 {
            let descriptor = base.add(4 + i * std::mem::size_of::<FILEDESCRIPTORW>()).cast::<FILEDESCRIPTORW>().read_unaligned();
            let name = descriptor.cFileName;
            paths.push(String::from_utf16_lossy(&name[..name.iter().position(|&c| c == 0).unwrap()]));
            directories.push(descriptor.dwFileAttributes & 0x10 != 0);
            sizes.push((u64::from(descriptor.nFileSizeHigh) << 32) | u64::from(descriptor.nFileSizeLow));
        }
        assert_eq!(paths, ["folder", "folder\\large.bin", "folder\\empty"]);
        assert_eq!(directories, [true, false, true]);
        assert_eq!(sizes, [0, 3 * 1024 * 1024, 0]);
        GlobalUnlock(medium.u.hGlobal).ok();
        ReleaseStgMedium(&mut medium);
        assert!(calls.lock().unwrap().is_empty());
        let mut medium = received.GetData(&format("FileContents", 1, TYMED_ISTREAM)).unwrap();
        let stream = medium.u.pstm.as_ref().unwrap();
        let mut data = vec![0u8; 3 * 1024 * 1024];
        let mut read = 0;
        stream.Read(data.as_mut_ptr().cast(), data.len() as u32, Some(&mut read)).unwrap();
        assert_eq!(read, 3 * 1024 * 1024);
        assert!(data.iter().all(|&byte| byte == 0x5a));
        stream.Seek(0, STREAM_SEEK_SET, None).unwrap();
        let destination = CreateStreamOnHGlobal(Default::default(), true).unwrap();
        let (mut copied, mut written) = (0, 0);
        stream.CopyTo(&destination, 3 * 1024 * 1024, Some(&mut copied), Some(&mut written)).unwrap();
        assert_eq!((copied, written), (3 * 1024 * 1024, 3 * 1024 * 1024));
        ReleaseStgMedium(&mut medium);
        OleSetClipboard(None::<&IDataObject>).unwrap();
        drop(received);
        drop(object);
        OleUninitialize();
    }
    let reads = calls.lock().unwrap();
    assert!(reads.len() >= 3);
    assert!(reads.iter().all(|&(i, _, n)| i == 1 && n <= 1024 * 1024));
    assert_eq!(reads[0].1, 0);
    assert_eq!(reads[1].1, 1024 * 1024);
    drop(reads);
    // Owner-thread integration: publication itself cannot invoke the reader,
    // and a generation cancellation must revoke only our own clipboard.
    assert!(dbx_plugin_rdp::clip_owner::offer_virtual(7, clipboard));
    assert!(dbx_plugin_rdp::clip_owner::offer_active());
    dbx_plugin_rdp::clip_owner::cancel_virtual(6);
    assert!(dbx_plugin_rdp::clip_owner::offer_active());
    dbx_plugin_rdp::clip_owner::cancel_virtual(7);
    assert!(!dbx_plugin_rdp::clip_owner::offer_active());
    assert!(dbx_plugin_rdp::clip_owner::offer_virtual(8, VirtualClipboard::new(vec![
        VirtualFileEntry { name: "x".into(), relative_path: None, size: 1, is_directory: false }
    ], |_, _, _| Ok(vec![1])).unwrap()));
    let replacement = vec![std::env::temp_dir().join("user-owned.txt")];
    dbx_plugin_rdp::clip_files::set_local_file_list(&replacement).unwrap();
    dbx_plugin_rdp::clip_owner::cancel_virtual(8);
    assert_eq!(dbx_plugin_rdp::clip_files::read_local_file_list().unwrap().unwrap().roots, replacement,
        "disconnect must not erase a newer user clipboard");
}
