//! Windows identities used by the disk budget. Keep all handle ownership here.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::ptr::{null, null_mut};

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::{
    ERROR_NO_MORE_FILES, HANDLE, INVALID_HANDLE_VALUE, LocalFree, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ID_INFO, FileIdInfo, GetDiskFreeSpaceExW, GetFileInformationByHandleEx,
    GetFinalPathNameByHandleW, SYNCHRONIZE, VOLUME_NAME_GUID,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
    QueryInformationJobObject,
};
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA,
    PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
};

use crate::disk_budget::{Owner, Writers};

pub(super) fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain([0]).collect()
}

pub(super) fn checked(ok: i32) -> io::Result<()> {
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(super) fn owned(raw: HANDLE) -> io::Result<OwnedHandle> {
    if raw.is_null() || raw == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: callers transfer a valid owned Win32 handle or an error sentinel.
        Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
    }
}

pub(super) struct SecurityDescriptor(windows_sys::Win32::Security::PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    pub fn new(sddl: &str) -> io::Result<Self> {
        let sddl = wide(OsStr::new(sddl));
        let mut descriptor = null_mut();
        // SAFETY: sddl is terminated, and Windows allocates the returned descriptor.
        checked(unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        })?;
        Ok(Self(descriptor))
    }

    pub fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: new obtained this descriptor from Windows using LocalAlloc.
        unsafe { LocalFree(self.0) };
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Anchor {
    pub volume: String,
    serial: u64,
    file: [u8; 16],
}

pub(super) fn anchor(file: &File) -> io::Result<Anchor> {
    let handle = file.as_raw_handle();
    let mut id = FILE_ID_INFO::default();
    // SAFETY: The borrowed file is open and the buffer is the documented size.
    checked(unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            std::ptr::from_mut(&mut id).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    })?;
    let mut path = vec![0; 32768];
    // SAFETY: path provides space for the indicated number of UTF-16 units.
    let count = unsafe {
        GetFinalPathNameByHandleW(
            handle,
            path.as_mut_ptr(),
            path.len() as u32,
            VOLUME_NAME_GUID,
        )
    } as usize;
    if count == 0 {
        return Err(io::Error::last_os_error());
    }
    if count >= path.len() {
        return Err(io::Error::other("disk budget anchor path is too long"));
    }
    let path = String::from_utf16_lossy(&path[..count]);
    let guid = path
        .strip_prefix(r"\\?\Volume{")
        .and_then(|suffix| suffix.split_once('}'))
        .filter(|(_, tail)| tail.starts_with('\\'))
        .map(|(guid, _)| guid.to_ascii_lowercase())
        .ok_or_else(|| io::Error::other("disk budget anchor has no local volume GUID"))?;
    Ok(Anchor {
        volume: guid,
        serial: id.VolumeSerialNumber,
        file: id.FileId.Identifier,
    })
}

pub(super) fn free_bytes(volume: &str) -> io::Result<u64> {
    // Use the verified volume itself, independent of drive-letter mappings and junctions.
    let volume = wide(OsStr::new(&format!(r"\\?\Volume{{{volume}}}\")));
    let mut available = 0;
    let mut physical = 0;
    // SAFETY: volume is NUL-terminated and the two outputs are valid pointers.
    checked(unsafe {
        GetDiskFreeSpaceExW(volume.as_ptr(), &mut available, null_mut(), &mut physical)
    })?;
    Ok(available.min(physical))
}

pub(super) struct Process {
    handle: OwnedHandle,
    pub owner: Owner,
}

impl Process {
    #[cfg(test)]
    pub fn current() -> io::Result<Self> {
        Self::open(std::process::id(), false)
    }

    #[cfg(test)]
    pub fn verified(owner: &Owner) -> io::Result<Self> {
        let process = Self::open(owner.pid, false)?;
        if process.owner != *owner {
            return Err(io::Error::other("Windows process identity changed"));
        }
        Ok(process)
    }

    pub fn open(pid: u32, assign: bool) -> io::Result<Self> {
        let mut access = PROCESS_QUERY_LIMITED_INFORMATION;
        if assign {
            access |= PROCESS_SET_QUOTA | PROCESS_TERMINATE | SYNCHRONIZE;
        }
        // SAFETY: OpenProcess returns a new handle to the requested PID or fails.
        let handle = owned(unsafe { OpenProcess(access, 0, pid) })?;
        // SAFETY: the handle has process query rights and every output is allocated.
        let mut creation = Default::default();
        let mut exit = Default::default();
        let mut kernel = Default::default();
        let mut user = Default::default();
        checked(unsafe {
            GetProcessTimes(
                handle.as_raw_handle(),
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        })?;
        if exit.dwHighDateTime != 0 || exit.dwLowDateTime != 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Windows process already exited",
            ));
        }
        Ok(Self {
            handle,
            owner: Owner {
                pid,
                creation_time: (u64::from(creation.dwHighDateTime) << 32)
                    | u64::from(creation.dwLowDateTime),
            },
        })
    }

    pub fn refuse_runner_start(&self) -> io::Result<()> {
        // SAFETY: this exact process handle was opened and checked against the
        // job-start hook's installed worker image and current ancestry.
        checked(unsafe { TerminateProcess(self.handle.as_raw_handle(), 1) })?;
        // SAFETY: open requested SYNCHRONIZE on the same process handle.
        if unsafe { WaitForSingleObject(self.handle.as_raw_handle(), 5000) } != WAIT_OBJECT_0 {
            return Err(io::Error::other(
                "could not confirm refused runner worker exited",
            ));
        }
        Ok(())
    }

    fn image(&self) -> io::Result<PathBuf> {
        let mut path = vec![0; 32768];
        let mut count = path.len() as u32;
        // SAFETY: self.handle is valid and path provides the indicated number of UTF-16 units.
        checked(unsafe {
            QueryFullProcessImageNameW(
                self.handle.as_raw_handle(),
                0,
                path.as_mut_ptr(),
                &mut count,
            )
        })?;
        Ok(PathBuf::from(OsString::from_wide(&path[..count as usize])))
    }

    pub fn runner_ancestor(&self, images: &[PathBuf]) -> io::Result<Self> {
        // SAFETY: The snapshot holds process entries for this host.
        let snapshot = owned(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) })?;
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        // SAFETY: entry is initialized with its required size.
        checked(unsafe { Process32FirstW(snapshot.as_raw_handle(), &mut entry) })?;
        let mut parents = HashMap::new();
        loop {
            parents.insert(entry.th32ProcessID, entry.th32ParentProcessID);
            // SAFETY: same valid snapshot and entry as Process32FirstW.
            if unsafe { Process32NextW(snapshot.as_raw_handle(), &mut entry) } == 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(ERROR_NO_MORE_FILES as i32) {
                    return Err(error);
                }
                break;
            }
        }

        let mut child = self.owner.clone();
        for _ in 0..parents.len() {
            let Some(&pid) = parents.get(&child.pid).filter(|&&pid| pid != 0) else {
                break;
            };
            let parent = Self::open(pid, false)?;
            if parent.owner.creation_time > child.creation_time || parent.owner == child {
                break;
            }
            let image = parent.image()?.canonicalize()?;
            if images.contains(&image) {
                // Keep the first handle open while taking the second, preventing PID reuse.
                let worker = Self::open(pid, true)?;
                if worker.owner == parent.owner {
                    return Ok(worker);
                }
                break;
            }
            child = parent.owner;
        }
        Err(io::Error::other(
            "disk budget command must run under a provisioned Actions runner worker",
        ))
    }
}

pub(super) struct Job(OwnedHandle);

impl Job {
    pub fn create() -> io::Result<Self> {
        // SAFETY: a fresh unnamed Job with no breakaway limits starts empty.
        owned(unsafe { CreateJobObjectW(null(), null()) }).map(Self)
    }

    pub fn assign(&self, process: &Process) -> io::Result<()> {
        // SAFETY: self owns the new job and process holds a valid process handle.
        checked(unsafe {
            AssignProcessToJobObject(self.0.as_raw_handle(), process.handle.as_raw_handle())
        })
    }

    pub fn contains(&self, process: &Process) -> io::Result<bool> {
        let mut member = 0;
        // SAFETY: process and job handles are valid and member is a writable BOOL.
        checked(unsafe {
            IsProcessInJob(
                process.handle.as_raw_handle(),
                self.0.as_raw_handle(),
                &mut member,
            )
        })?;
        Ok(member != 0)
    }

    #[cfg(test)]
    pub fn without_query_for_test(&self) -> io::Result<Self> {
        use windows_sys::Win32::Foundation::DuplicateHandle;
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut copy = null_mut();
        // SAFETY: the test duplicates a live Job handle without JOB_QUERY.
        checked(unsafe {
            let current = GetCurrentProcess();
            DuplicateHandle(
                current,
                self.0.as_raw_handle(),
                current,
                &mut copy,
                0x0008,
                0,
                0,
            )
        })?;
        owned(copy).map(Self)
    }

    pub fn writers(&self) -> Writers {
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: self holds the job open and info is the correct output type.
        let result = unsafe {
            QueryInformationJobObject(
                self.0.as_raw_handle(),
                JobObjectBasicAccountingInformation,
                std::ptr::from_mut(&mut info).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                null_mut(),
            )
        };
        match (result, info.ActiveProcesses) {
            (0, _) => Writers::Unknown,
            (_, 0) => Writers::Stopped,
            _ => Writers::Active,
        }
    }
}
