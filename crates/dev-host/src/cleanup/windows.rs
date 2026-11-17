//! The Windows live-process scan behind `ProcessLiveness`: what `lsof` lists on macOS, read
//! through Win32 calls for every process the current user may open.
//!
//! - Open files and folders: the system handle table (`NtQuerySystemInformation` with
//!   `SystemExtendedHandleInformation`). Each file handle of a process is duplicated into this
//!   process and named with `GetFinalPathNameByHandleW`. A process's working folder is an open
//!   folder handle, so it is listed too. Only disk handles are named: naming a pipe can block.
//! - Running and mapped files: each process's image (`QueryFullProcessImageNameW`), its loaded
//!   modules (`EnumProcessModulesEx`) and its mapped file views (`VirtualQueryEx` with
//!   `GetMappedFileNameW`), so an executable, a DLL or a mapped data file needs no open handle.
//!
//! Processes this user cannot open (other users' processes, elevated ones when this process
//! is not elevated) are skipped; [`is_elevated`] tells the caller which case applies. All of the crate's `unsafe` for the scan lives here.

use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsString, c_void};
use std::fs::File;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;

use windows_sys::Wdk::System::SystemInformation::{
    NtQuerySystemInformation, SYSTEM_INFORMATION_CLASS,
};
use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, HMODULE, STATUS_INFO_LENGTH_MISMATCH,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_NAME_NORMALIZED, FILE_TYPE_DISK, GetFileType, GetFinalPathNameByHandleW, QueryDosDeviceW,
    VOLUME_NAME_DOS,
};
use windows_sys::Win32::System::Memory::{MEM_MAPPED, MEMORY_BASIC_INFORMATION, VirtualQueryEx};
use windows_sys::Win32::System::ProcessStatus::{
    EnumProcessModulesEx, GetMappedFileNameW, GetModuleFileNameExW, LIST_MODULES_ALL,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_DUP_HANDLE, PROCESS_NAME_WIN32,
    PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    QueryFullProcessImageNameW,
};

/// ntexapi.h: the information class that returns `SYSTEM_HANDLE_INFORMATION_EX`.
const SYSTEM_EXTENDED_HANDLE_INFORMATION: SYSTEM_INFORMATION_CLASS = 64;

/// The longest path a Win32 call returns, in UTF-16 units.
const MAX_PATH_UNITS: usize = 32_768;

/// ntexapi.h `SYSTEM_HANDLE_TABLE_ENTRY_INFO_EX`. The table is
/// `{ NumberOfHandles: ULONG_PTR, Reserved: ULONG_PTR, Handles: [entry] }`.
#[repr(C)]
#[derive(Clone, Copy)]
// Laid out to match the C struct; the scan reads only some fields.
#[allow(dead_code)]
struct HandleEntry {
    object: *mut c_void,
    unique_process_id: usize,
    handle_value: usize,
    granted_access: u32,
    creator_back_trace_index: u16,
    object_type_index: u16,
    handle_attributes: u32,
    reserved: u32,
}

/// Whether this process runs with an elevated (administrator) token.
pub(super) fn is_elevated() -> Result<bool, String> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: `token` is a valid out pointer; on success it is a handle this function owns.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(format!(
            "OpenProcessToken failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: `token` is a valid handle from the call above; `OwnedHandle` closes it once.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut returned = 0u32;
    // SAFETY: the buffer is a `TOKEN_ELEVATION` and its exact size is passed.
    let ok = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    };
    if ok == 0 {
        return Err(format!(
            "GetTokenInformation failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(elevation.TokenIsElevated != 0)
}

/// Every path a live process this user may open has open, mapped or running, each resolved
/// to its final path (a module loaded through a junction or SUBST drive names its real file).
pub(super) fn open_paths() -> Result<Vec<PathBuf>, String> {
    // A file handle of this process whose table entry gives the object type number of files.
    let probe = std::env::current_exe()
        .and_then(File::open)
        .map_err(|e| format!("cannot open this program to find the file handle type: {e}"))?;
    let table = handle_table()?;
    let own_pid = std::process::id() as usize;
    let probe_value = probe.as_raw_handle() as usize;
    let file_type = table
        .iter()
        .find(|e| e.unique_process_id == own_pid && e.handle_value == probe_value)
        .map(|e| e.object_type_index)
        .ok_or("the system handle table does not list this process's own file handle")?;
    drop(probe);

    let mut by_pid: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for e in &table {
        let handles = by_pid.entry(e.unique_process_id).or_default();
        if e.object_type_index == file_type {
            handles.push(e.handle_value);
        }
    }
    drop(table);

    let devices = dos_devices();
    let mut name = vec![0u16; MAX_PATH_UNITS];
    let mut out = Vec::new();
    let mut named: HashSet<PathBuf> = HashSet::new();
    for (pid, handles) in by_pid {
        let Ok(pid) = u32::try_from(pid) else {
            continue;
        };
        if pid == 0 {
            continue;
        }
        if let Some(process) =
            open_process(pid, PROCESS_DUP_HANDLE | PROCESS_QUERY_LIMITED_INFORMATION)
        {
            let process = process.as_raw_handle();
            for value in handles {
                if let Some(path) = handle_path(process, value) {
                    out.push(path);
                }
            }
            if let Some(image) = image_path(process, &mut name) {
                named.insert(image);
            }
        }
        if let Some(process) = open_process(pid, PROCESS_QUERY_INFORMATION | PROCESS_VM_READ) {
            let process = process.as_raw_handle();
            modules(process, &mut name, &mut named);
            mapped_views(process, &mut name, &devices, &mut named);
        }
    }
    // Handle paths are final already; the others name the file as it was opened.
    out.extend(named.into_iter().map(|p| p.canonicalize().unwrap_or(p)));
    Ok(out)
}

/// The system handle table, read into a buffer grown until it fits.
fn handle_table() -> Result<Vec<HandleEntry>, String> {
    // usize elements keep the buffer aligned for the table's pointer-sized fields.
    let unit = size_of::<usize>();
    let mut buf: Vec<usize> = vec![0; (4 << 20) / unit];
    for _ in 0..8 {
        let bytes = u32::try_from(buf.len() * unit)
            .map_err(|_| "the system handle table is larger than 4 GB".to_owned())?;
        let mut needed = 0u32;
        // SAFETY: `buf` is writable for `bytes` bytes and `needed` is a valid out pointer.
        let status = unsafe {
            NtQuerySystemInformation(
                SYSTEM_EXTENDED_HANDLE_INFORMATION,
                buf.as_mut_ptr().cast(),
                bytes,
                &mut needed,
            )
        };
        if status == STATUS_INFO_LENGTH_MISMATCH {
            // The table grows between calls; leave room for that.
            let want = (needed as usize).max(buf.len() * unit) * 2;
            buf = vec![0; want.div_ceil(unit)];
            continue;
        }
        if status < 0 {
            return Err(format!(
                "cannot read the system handle table (NTSTATUS 0x{status:08X})"
            ));
        }
        let count = buf[0];
        let room = (buf.len() - 2) * unit / size_of::<HandleEntry>();
        if count > room {
            return Err(format!(
                "the system handle table lists {count} handles but holds room for {room}"
            ));
        }
        // SAFETY: the call succeeded, so the buffer starts with the two-word header and then
        // `count` entries, which fit inside `buf` (checked above); the offset is aligned for
        // `HandleEntry`, whose alignment is that of usize.
        let entries =
            unsafe { std::slice::from_raw_parts(buf.as_ptr().add(2).cast::<HandleEntry>(), count) };
        return Ok(entries.to_vec());
    }
    Err("the system handle table kept growing while it was read".to_owned())
}

/// `pid` opened with `access`, or `None` when this user may not open it.
fn open_process(pid: u32, access: u32) -> Option<OwnedHandle> {
    // SAFETY: plain call; a null result means failure.
    let handle = unsafe { OpenProcess(access, 0, pid) };
    if handle.is_null() {
        return None;
    }
    // SAFETY: `handle` is a fresh process handle this function owns.
    Some(unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// The final path of handle `value` of `process`, when it is a disk file or folder.
fn handle_path(process: HANDLE, value: usize) -> Option<PathBuf> {
    let mut dup: HANDLE = std::ptr::null_mut();
    // SAFETY: `process` is open with PROCESS_DUP_HANDLE; `value` is a handle number from its
    // table entry (a closed or reused number fails the call or names another object, both
    // harmless); `dup` is a valid out pointer.
    let ok = unsafe {
        DuplicateHandle(
            process,
            value as HANDLE,
            GetCurrentProcess(),
            &mut dup,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == 0 || dup.is_null() {
        return None;
    }
    // SAFETY: `dup` is a fresh handle in this process that this function owns.
    let dup = unsafe { OwnedHandle::from_raw_handle(dup) };
    let raw = dup.as_raw_handle();
    // SAFETY: `raw` is a valid handle. Only disk handles are named: a name query on a
    // synchronous pipe waits for its pending read.
    if unsafe { GetFileType(raw) } != FILE_TYPE_DISK {
        return None;
    }
    let mut buf = vec![0u16; 512];
    for _ in 0..3 {
        let len = u32::try_from(buf.len()).ok()?;
        // SAFETY: `buf` is writable for `len` UTF-16 units.
        let n = unsafe {
            GetFinalPathNameByHandleW(
                raw,
                buf.as_mut_ptr(),
                len,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        } as usize;
        if n == 0 {
            return None;
        }
        if n < buf.len() {
            return Some(PathBuf::from(OsString::from_wide(&buf[..n])));
        }
        // Too small: `n` is the size needed, terminator included.
        buf = vec![0; n + 1];
    }
    None
}

/// The executable `process` runs.
fn image_path(process: HANDLE, name: &mut [u16]) -> Option<PathBuf> {
    let mut len = u32::try_from(name.len()).ok()?;
    // SAFETY: `name` is writable for `len` units; `len` is a valid in/out pointer.
    let ok = unsafe {
        QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, name.as_mut_ptr(), &mut len)
    };
    (ok != 0).then(|| PathBuf::from(OsString::from_wide(&name[..len as usize])))
}

/// Adds the files of every module (the executable and DLLs) `process` has loaded.
fn modules(process: HANDLE, name: &mut [u16], out: &mut HashSet<PathBuf>) {
    let mut list: Vec<HMODULE> = vec![std::ptr::null_mut(); 1024];
    let mut count = 0;
    for _ in 0..3 {
        let Ok(bytes) = u32::try_from(list.len() * size_of::<HMODULE>()) else {
            return;
        };
        let mut needed = 0u32;
        // SAFETY: `list` is writable for `bytes` bytes; `needed` is a valid out pointer.
        let ok = unsafe {
            EnumProcessModulesEx(
                process,
                list.as_mut_ptr(),
                bytes,
                &mut needed,
                LIST_MODULES_ALL,
            )
        };
        if ok == 0 {
            return;
        }
        count = needed as usize / size_of::<HMODULE>();
        if count <= list.len() {
            break;
        }
        list = vec![std::ptr::null_mut(); count + 64];
    }
    let len = u32::try_from(name.len()).unwrap_or(u32::MAX);
    for &module in &list[..count.min(list.len())] {
        // SAFETY: `name` is writable for `len` units; a stale module handle fails the call.
        let n = unsafe { GetModuleFileNameExW(process, module, name.as_mut_ptr(), len) } as usize;
        if n > 0 && n < name.len() {
            out.insert(PathBuf::from(OsString::from_wide(&name[..n])));
        }
    }
}

/// Adds the files of `process`'s mapped data views (a mapped file needs no open handle).
fn mapped_views(
    process: HANDLE,
    name: &mut [u16],
    devices: &[(String, String)],
    out: &mut HashSet<PathBuf>,
) {
    let len = u32::try_from(name.len()).unwrap_or(u32::MAX);
    let mut address = 0usize;
    let mut last_base: *mut c_void = std::ptr::null_mut();
    loop {
        let mut info = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: `info` is writable for its size; the address is only read as a number.
        let got = unsafe {
            VirtualQueryEx(
                process,
                address as *const c_void,
                &mut info,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if got == 0 {
            return;
        }
        if info.Type == MEM_MAPPED && info.AllocationBase != last_base {
            last_base = info.AllocationBase;
            // SAFETY: `name` is writable for `len` units.
            let n = unsafe { GetMappedFileNameW(process, info.BaseAddress, name.as_mut_ptr(), len) }
                as usize;
            if n > 0 && n < name.len() {
                let device = String::from_utf16_lossy(&name[..n]);
                if let Some(path) = super::dos_path(&device, devices) {
                    out.insert(PathBuf::from(path));
                }
            }
        }
        match (info.BaseAddress as usize).checked_add(info.RegionSize) {
            Some(next) if next > address => address = next,
            _ => return,
        }
    }
}

/// `("C:", "\Device\HarddiskVolume3")` for every drive letter in use.
fn dos_devices() -> Vec<(String, String)> {
    let mut target = vec![0u16; 1024];
    let len = u32::try_from(target.len()).unwrap_or(u32::MAX);
    (b'A'..=b'Z')
        .filter_map(|letter| {
            let drive = format!("{}:", letter as char);
            let wide: Vec<u16> = std::ffi::OsStr::new(&drive)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            // SAFETY: `wide` is NUL-terminated; `target` is writable for `len` units.
            let n = unsafe { QueryDosDeviceW(wide.as_ptr(), target.as_mut_ptr(), len) } as usize;
            // The result is a list of NUL-terminated names; the first is the current one.
            let first = target[..n].split(|&u| u == 0).next()?;
            (!first.is_empty()).then(|| (drive, String::from_utf16_lossy(first)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn elevation_query_succeeds() {
        super::is_elevated().expect("the token elevation query works");
    }
}
