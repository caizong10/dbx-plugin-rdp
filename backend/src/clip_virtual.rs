//! Virtual shell files: metadata is immediate, bytes are read on demand.
//! No RDP or socket access belongs here; readers are supplied by the session.
#![cfg(windows)]
#![allow(non_snake_case)]

use std::mem::ManuallyDrop;
use std::sync::{Arc, Mutex};
use windows::core::{implement, BOOL, Error, HRESULT, Ref, Result, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::*;
use windows::Win32::UI::Shell::{FILEDESCRIPTORW, IDataObjectAsyncCapability, IDataObjectAsyncCapability_Impl};
use std::sync::atomic::{AtomicBool, Ordering};

const BLOCK: u32 = 1024 * 1024;
type Reader = dyn Fn(usize, u64, u32) -> std::result::Result<Vec<u8>, String> + Send + Sync;

#[derive(Clone, Debug)]
pub struct VirtualFileEntry {
    pub name: String,
    pub relative_path: Option<String>,
    pub size: u64,
    pub is_directory: bool,
}

#[derive(Clone)]
pub struct VirtualClipboard { entries: Arc<Vec<VirtualFileEntry>>, reader: Arc<Reader> }

impl VirtualClipboard {
    pub fn new(entries: Vec<VirtualFileEntry>, reader: impl Fn(usize, u64, u32) -> std::result::Result<Vec<u8>, String> + Send + Sync + 'static) -> std::result::Result<Self, String> {
        if entries.is_empty() { return Err("empty virtual file list".into()); }
        for entry in &entries {
            let path = entry_path(entry);
            if path.encode_utf16().count() >= 260 || path.starts_with(['/', '\\']) || path.contains(':') ||
                path.split(['/', '\\']).any(|p| p.is_empty() || p == "." || p == ".." || p.contains('\0') || ironrdp_cliprdr::is_windows_device_name(p)) {
                return Err("unsafe or unsupported virtual file path".into());
            }
        }
        Ok(Self { entries: Arc::new(entries), reader: Arc::new(reader) })
    }
    pub fn formats(&self) -> Vec<&'static str> { vec!["FileGroupDescriptorW", "FileContents", "Preferred DropEffect"] }
    pub fn read_file(&self, index: usize, offset: u64, length: u32) -> std::result::Result<Vec<u8>, String> {
        let file = self.entries.get(index).ok_or("file index out of range")?;
        if file.is_directory { return Err("cannot read directory stream".into()); }
        if offset >= file.size || length == 0 { return Ok(Vec::new()); }
        let length = u64::from(length.min(BLOCK)).min(file.size - offset) as u32;
        let data = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.reader)(index, offset, length)))
            .map_err(|_| "virtual reader panicked")??;
        if data.is_empty() || data.len() > length as usize { return Err("invalid remote stream length".into()); }
        Ok(data)
    }
    pub fn data_object(&self) -> IDataObject { DataObject { source: self.clone(), asynchronous: AtomicBool::new(true), operation: AtomicBool::new(false) }.into() }
}

fn entry_path(entry: &VirtualFileEntry) -> String {
    match entry.relative_path.as_deref().filter(|p| !p.is_empty()) {
        Some(parent) => format!("{parent}\\{}", entry.name), None => entry.name.clone(),
    }
}
fn id(name: &str) -> u16 {
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    unsafe { RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) as u16 }
}
fn fmt(name: &str, index: i32, medium: TYMED) -> FORMATETC {
    FORMATETC { cfFormat: id(name), dwAspect: DVASPECT_CONTENT.0, lindex: index, tymed: medium.0 as u32, ..Default::default() }
}
fn formats() -> Vec<FORMATETC> {
    vec![fmt("FileGroupDescriptorW", -1, TYMED_HGLOBAL), fmt("FileContents", -1, TYMED_ISTREAM), fmt("Preferred DropEffect", -1, TYMED_HGLOBAL)]
}
fn memory(bytes: &[u8]) -> Result<STGMEDIUM> {
    unsafe {
        let handle = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len())?;
        let pointer = GlobalLock(handle);
        if pointer.is_null() { GlobalFree(Some(handle)).ok(); return Err(Error::from_hresult(E_OUTOFMEMORY)); }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast(), bytes.len());
        GlobalUnlock(handle).ok();
        Ok(STGMEDIUM { tymed: TYMED_HGLOBAL.0 as u32, u: STGMEDIUM_0 { hGlobal: handle }, ..Default::default() })
    }
}

#[implement(IDataObject, IDataObjectAsyncCapability)]
struct DataObject { source: VirtualClipboard, asynchronous: AtomicBool, operation: AtomicBool }
impl IDataObjectAsyncCapability_Impl for DataObject_Impl {
    fn SetAsyncMode(&self, value: BOOL) -> Result<()> { self.asynchronous.store(value.as_bool(), Ordering::Release); Ok(()) }
    fn GetAsyncMode(&self) -> Result<BOOL> { Ok(self.asynchronous.load(Ordering::Acquire).into()) }
    fn StartOperation(&self, _: Ref<IBindCtx>) -> Result<()> { self.operation.store(true, Ordering::Release); Ok(()) }
    fn InOperation(&self) -> Result<BOOL> { Ok(self.operation.load(Ordering::Acquire).into()) }
    fn EndOperation(&self, _: HRESULT, _: Ref<IBindCtx>, _: u32) -> Result<()> { self.operation.store(false, Ordering::Release); Ok(()) }
}
impl IDataObject_Impl for DataObject_Impl {
    fn QueryGetData(&self, pointer: *const FORMATETC) -> HRESULT {
        if pointer.is_null() { return E_POINTER; }
        let f = unsafe { &*pointer };
        if f.dwAspect != DVASPECT_CONTENT.0 || !f.ptd.is_null() { return DV_E_DVASPECT; }
        if f.cfFormat == id("FileContents") {
            if f.lindex < 0 || f.lindex as usize >= self.source.entries.len() || self.source.entries[f.lindex as usize].is_directory { return DV_E_LINDEX; }
            if f.tymed & TYMED_ISTREAM.0 as u32 == 0 { return DV_E_TYMED; }
        } else if f.cfFormat == id("FileGroupDescriptorW") || f.cfFormat == id("Preferred DropEffect") {
            if f.lindex != -1 { return DV_E_LINDEX; }
            if f.tymed & TYMED_HGLOBAL.0 as u32 == 0 { return DV_E_TYMED; }
        } else { return DV_E_FORMATETC; }
        S_OK
    }
    fn GetData(&self, pointer: *const FORMATETC) -> Result<STGMEDIUM> {
        self.QueryGetData(pointer).ok()?;
        let f = unsafe { &*pointer };
        if f.cfFormat == id("Preferred DropEffect") { return memory(&1u32.to_le_bytes()); }
        if f.cfFormat == id("FileContents") {
            let stream: IStream = FileStream { source: self.source.clone(), index: f.lindex as usize, position: Mutex::new(0) }.into();
            return Ok(STGMEDIUM { tymed: TYMED_ISTREAM.0 as u32, u: STGMEDIUM_0 { pstm: ManuallyDrop::new(Some(stream)) }, ..Default::default() });
        }
        let mut bytes = vec![0u8; 4 + self.source.entries.len() * std::mem::size_of::<FILEDESCRIPTORW>()];
        bytes[..4].copy_from_slice(&(self.source.entries.len() as u32).to_le_bytes());
        for (i, entry) in self.source.entries.iter().enumerate() {
            let mut descriptor = FILEDESCRIPTORW { dwFlags: 0x4 | 0x40 | 0x4000 | 0x80000000,
                dwFileAttributes: if entry.is_directory { 0x10 } else { 0x80 },
                nFileSizeHigh: (entry.size >> 32) as u32, nFileSizeLow: entry.size as u32, ..Default::default() };
            let mut name = [0u16; 260];
            for (target, value) in name.iter_mut().zip(entry_path(entry).encode_utf16()) { *target = value; }
            descriptor.cFileName = name;
            unsafe { bytes.as_mut_ptr().add(4 + i * std::mem::size_of::<FILEDESCRIPTORW>()).cast::<FILEDESCRIPTORW>().write_unaligned(descriptor); }
        }
        memory(&bytes)
    }
    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> Result<()> { Err(E_NOTIMPL.into()) }
    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
        if out.is_null() { return E_POINTER; } unsafe { (*out).ptd = std::ptr::null_mut(); } E_NOTIMPL
    }
    fn SetData(&self, _: *const FORMATETC, _: *const STGMEDIUM, _: BOOL) -> Result<()> { Err(E_NOTIMPL.into()) }
    fn EnumFormatEtc(&self, direction: u32) -> Result<IEnumFORMATETC> {
        if direction != DATADIR_GET.0 as u32 { return Err(E_NOTIMPL.into()); }
        Ok(FormatEnumerator { position: Mutex::new(0) }.into())
    }
    fn DAdvise(&self, _: *const FORMATETC, _: u32, _: Ref<IAdviseSink>) -> Result<u32> { Err(OLE_E_ADVISENOTSUPPORTED.into()) }
    fn DUnadvise(&self, _: u32) -> Result<()> { Err(OLE_E_ADVISENOTSUPPORTED.into()) }
    fn EnumDAdvise(&self) -> Result<IEnumSTATDATA> { Err(OLE_E_ADVISENOTSUPPORTED.into()) }
}

#[implement(IEnumFORMATETC)]
struct FormatEnumerator { position: Mutex<usize> }
impl IEnumFORMATETC_Impl for FormatEnumerator_Impl {
    fn Next(&self, count: u32, output: *mut FORMATETC, fetched: *mut u32) -> HRESULT {
        if output.is_null() || (count != 1 && fetched.is_null()) { return E_POINTER; }
        let Ok(mut position) = self.position.lock() else { return E_FAIL; };
        let entries = formats();
        let n = (count as usize).min(entries.len().saturating_sub(*position));
        unsafe { for i in 0..n { output.add(i).write(entries[*position + i]); } if !fetched.is_null() { *fetched = n as u32; } }
        *position += n;
        if n == count as usize { S_OK } else { S_FALSE }
    }
    fn Skip(&self, count: u32) -> Result<()> {
        let mut position = self.position.lock().map_err(|_| Error::from_hresult(E_FAIL))?;
        *position = position.saturating_add(count as usize).min(3); Ok(())
    }
    fn Reset(&self) -> Result<()> { *self.position.lock().map_err(|_| Error::from_hresult(E_FAIL))? = 0; Ok(()) }
    fn Clone(&self) -> Result<IEnumFORMATETC> { Ok(FormatEnumerator { position: Mutex::new(*self.position.lock().map_err(|_| Error::from_hresult(E_FAIL))?) }.into()) }
}

#[implement(IStream)]
struct FileStream { source: VirtualClipboard, index: usize, position: Mutex<u64> }
impl ISequentialStream_Impl for FileStream_Impl {
    fn Read(&self, output: *mut std::ffi::c_void, count: u32, read: *mut u32) -> HRESULT {
        unsafe { if !read.is_null() { *read = 0; } }
        if output.is_null() && count != 0 { return STG_E_INVALIDPOINTER; }
        let Ok(mut position) = self.position.lock() else { return E_FAIL; };
        let mut total = 0u32;
        while total < count && *position < self.source.entries[self.index].size {
            match self.source.read_file(self.index, *position, count - total) {
                Ok(data) => {
                    unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), output.cast::<u8>().add(total as usize), data.len()); }
                    total += data.len() as u32; *position += data.len() as u64;
                    unsafe { if !read.is_null() { *read = total; } }
                }
                Err(_) => return STG_E_READFAULT,
            }
        }
        if total == count { S_OK } else { S_FALSE }
    }
    fn Write(&self, _: *const std::ffi::c_void, _: u32, written: *mut u32) -> HRESULT { unsafe { if !written.is_null() { *written = 0; } } STG_E_ACCESSDENIED }
}
impl IStream_Impl for FileStream_Impl {
    fn Seek(&self, movement: i64, origin: STREAM_SEEK, output: *mut u64) -> Result<()> {
        let mut position = self.position.lock().map_err(|_| Error::from_hresult(E_FAIL))?;
        let base = match origin { STREAM_SEEK_SET => 0, STREAM_SEEK_CUR => *position, STREAM_SEEK_END => self.source.entries[self.index].size, _ => return Err(STG_E_INVALIDFUNCTION.into()) };
        *position = base.checked_add_signed(movement).ok_or_else(|| Error::from_hresult(STG_E_INVALIDFUNCTION))?;
        unsafe { if !output.is_null() { *output = *position; } } Ok(())
    }
    fn Stat(&self, output: *mut STATSTG, _: &STATFLAG) -> Result<()> {
        if output.is_null() { return Err(E_POINTER.into()); }
        unsafe { output.write(STATSTG { r#type: STGTY_STREAM.0 as u32, cbSize: self.source.entries[self.index].size, ..Default::default() }); } Ok(())
    }
    fn Clone(&self) -> Result<IStream> { Ok(FileStream { source: self.source.clone(), index: self.index, position: Mutex::new(*self.position.lock().map_err(|_| Error::from_hresult(E_FAIL))?) }.into()) }
    fn SetSize(&self, _: u64) -> Result<()> { Err(STG_E_ACCESSDENIED.into()) }
    fn CopyTo(&self, target: Ref<IStream>, count: u64, read: *mut u64, written: *mut u64) -> Result<()> {
        let target = target.as_ref().ok_or_else(|| Error::from_hresult(E_POINTER))?;
        let mut total = 0u64;
        unsafe { if !read.is_null() { *read = 0; } if !written.is_null() { *written = 0; } }
        let mut bytes = vec![0u8; BLOCK as usize];
        while total < count {
            let mut n = 0;
            let status = self.Read(bytes.as_mut_ptr().cast(), (count - total).min(u64::from(BLOCK)) as u32, &mut n);
            status.ok()?;
            if n == 0 { break; }
            let mut output = 0;
            unsafe { target.Write(bytes.as_ptr().cast(), n, Some(&mut output)).ok()?; }
            total += u64::from(n);
            unsafe { if !read.is_null() { *read = total; } if !written.is_null() { *written += u64::from(output); } }
            if output != n { return Err(STG_E_WRITEFAULT.into()); }
            if status == S_FALSE { break; }
        }
        Ok(())
    }
    fn Commit(&self, _: &STGC) -> Result<()> { Ok(()) }
    fn Revert(&self) -> Result<()> { Err(E_NOTIMPL.into()) }
    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> Result<()> { Err(STG_E_INVALIDFUNCTION.into()) }
    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> Result<()> { Err(STG_E_INVALIDFUNCTION.into()) }
}
