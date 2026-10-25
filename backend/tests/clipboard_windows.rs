//! Exercise real Win32 clipboard formats on a PRIVATE window station so
//! this test cannot replace or read the user's interactive clipboard.
#![cfg(windows)]

#[test]
fn publishing_drop_effect_keeps_file_list_on_clipboard() {
    #[link(name = "user32")]
    extern "system" {
        fn CreateWindowStationW(name: *const u16, flags: u32, access: u32, security: *const std::ffi::c_void) -> isize;
        fn GetProcessWindowStation() -> isize;
        fn SetProcessWindowStation(station: isize) -> i32;
        fn CloseWindowStation(station: isize) -> i32;
    }
    struct Station { previous: isize, private: isize }
    impl Drop for Station {
        fn drop(&mut self) {
            unsafe {
                SetProcessWindowStation(self.previous);
                CloseWindowStation(self.private);
            }
        }
    }
    let _station = unsafe {
        let previous = GetProcessWindowStation();
        let private = CreateWindowStationW(std::ptr::null(), 0, 0x037F, std::ptr::null());
        assert_ne!(private, 0, "create isolated clipboard station: {}", std::io::Error::last_os_error());
        assert_ne!(SetProcessWindowStation(private), 0);
        Station { previous, private }
    };
    let paths = vec![std::env::temp_dir().join("dbx-rdp-clipboard-regression.txt")];
    dbx_plugin_rdp::clip_files::set_local_file_list(&paths).unwrap();
    let list = dbx_plugin_rdp::clip_files::read_local_file_list()
        .expect("CF_HDROP must survive adding Preferred DropEffect")
        .expect("published file list must be present");
    assert_eq!(list.roots, paths);
    let name: Vec<u16> = "Preferred DropEffect\0".encode_utf16().collect();
    let effect = unsafe { clipboard_win::raw::register_raw_format(&name) }.unwrap();
    let _clipboard = clipboard_win::Clipboard::new_attempts(10).unwrap();
    let value: Vec<u8> = clipboard_win::get(clipboard_win::formats::RawData(effect.get())).unwrap();
    assert_eq!(value, vec![1, 0, 0, 0]);
}
