//! The Windows Credential Manager: generic credentials (`CRED_TYPE_GENERIC`) persisted for the
//! signed-in user on this machine (`CRED_PERSIST_LOCAL_MACHINE`: protected by DPAPI with the
//! user's logon credentials, never roaming). The target name is `DevWorks/<namespace>/<handle>`.
//!
//! A credential blob holds at most `CRED_MAX_CREDENTIAL_BLOB_SIZE` (2560) bytes, less than an
//! OAuth token pair can need, so a value is split: the first part (`<target>`) starts with a
//! two-byte header (format 1, part count) and later parts are `<target>/part<i>`. Later parts
//! are written first and the first part last, so a reader never sees a header naming parts that
//! do not exist yet; a delete removes the first part first, so a partial delete reads as absent.
//! `ERROR_NOT_FOUND` is an absent item; `ERROR_NO_SUCH_LOGON_SESSION` (no profile, a service
//! session) is reported as unavailable.
use super::{SERVICE, handle_of};
use devworks_core::ports::secrets::{Secret, SecretError, SecretKey, SecretStore};
use devworks_core::protocol::Id;
use std::ptr;
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_NO_SUCH_LOGON_SESSION, ERROR_NOT_FOUND, FILETIME, GetLastError,
};
use windows_sys::Win32::Security::Credentials::{
    CRED_MAX_CREDENTIAL_BLOB_SIZE, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW,
    CredDeleteW, CredEnumerateW, CredFree, CredReadW, CredWriteW,
};

pub struct CredentialManager;

const FORMAT: u8 = 1;
const HEADER: usize = 2;
const BLOB: usize = CRED_MAX_CREDENTIAL_BLOB_SIZE as usize;
const MAX_PARTS: usize = 255;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
fn target(key: &SecretKey) -> String {
    format!("{SERVICE}/{}", key.account())
}
fn part(base: &str, i: usize) -> String {
    format!("{base}/part{i}")
}
fn last_error() -> u32 {
    // SAFETY: reads the calling thread's last-error value.
    unsafe { GetLastError() }
}
fn error(code: u32) -> SecretError {
    match code {
        ERROR_NO_SUCH_LOGON_SESSION => {
            SecretError::Unavailable("no user logon session holds a credential store".into())
        }
        ERROR_ACCESS_DENIED => SecretError::Denied,
        c => SecretError::Failed(format!("Win32 error {c}")),
    }
}

fn write(name: &str, blob: &[u8]) -> Result<(), SecretError> {
    let mut name = wide(name);
    let mut user = wide(SERVICE);
    let credential = CREDENTIALW {
        Flags: 0,
        Type: CRED_TYPE_GENERIC,
        TargetName: name.as_mut_ptr(),
        Comment: ptr::null_mut(),
        LastWritten: FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        },
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_ptr() as *mut u8,
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        AttributeCount: 0,
        Attributes: ptr::null_mut(),
        TargetAlias: ptr::null_mut(),
        UserName: user.as_mut_ptr(),
    };
    // SAFETY: every pointer refers to a live buffer for the call's duration; CredWriteW copies.
    if unsafe { CredWriteW(&credential, 0) } == 0 {
        return Err(error(last_error()));
    }
    Ok(())
}

/// The blob of `name`, or `None` when it does not exist.
fn read(name: &str) -> Result<Option<Vec<u8>>, SecretError> {
    let name = wide(name);
    let mut out: *mut CREDENTIALW = ptr::null_mut();
    // SAFETY: `name` is NUL-terminated; on success `out` points to a buffer freed below.
    if unsafe { CredReadW(name.as_ptr(), CRED_TYPE_GENERIC, 0, &mut out) } == 0 {
        let code = last_error();
        return if code == ERROR_NOT_FOUND {
            Ok(None)
        } else {
            Err(error(code))
        };
    }
    // SAFETY: CredReadW succeeded, so `out` is a valid credential until CredFree.
    let blob = unsafe {
        let c = &*out;
        let bytes = if c.CredentialBlob.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize).to_vec()
        };
        CredFree(out as *const std::ffi::c_void);
        bytes
    };
    Ok(Some(blob))
}

/// Deletes `name`; `false` when it did not exist.
fn remove(name: &str) -> Result<bool, SecretError> {
    let name = wide(name);
    // SAFETY: `name` is NUL-terminated.
    if unsafe { CredDeleteW(name.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0 {
        let code = last_error();
        return if code == ERROR_NOT_FOUND {
            Ok(false)
        } else {
            Err(error(code))
        };
    }
    Ok(true)
}

impl SecretStore for CredentialManager {
    fn name(&self) -> &'static str {
        "Windows Credential Manager"
    }

    fn put(&self, key: &SecretKey, secret: &Secret) -> Result<(), SecretError> {
        let base = target(key);
        let bytes = secret.expose().as_bytes();
        let first = (BLOB - HEADER).min(bytes.len());
        let rest: Vec<&[u8]> = bytes[first..].chunks(BLOB).collect();
        if rest.len() + 1 > MAX_PARTS {
            return Err(SecretError::TooLarge);
        }
        for (i, chunk) in rest.iter().enumerate() {
            write(&part(&base, i + 1), chunk)?;
        }
        let mut head = Vec::with_capacity(HEADER + first);
        head.push(FORMAT);
        head.push((rest.len() + 1) as u8);
        head.extend_from_slice(&bytes[..first]);
        let previous = read(&base)?.and_then(|b| b.get(1).copied()).unwrap_or(1) as usize;
        write(&base, &head)?;
        // Parts a longer previous value used and this one does not.
        for i in rest.len() + 1..previous {
            remove(&part(&base, i))?;
        }
        head.iter_mut().for_each(|b| *b = 0);
        Ok(())
    }

    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError> {
        let base = target(key);
        let Some(head) = read(&base)? else {
            return Ok(None);
        };
        if head.len() < HEADER || head[0] != FORMAT || head[1] == 0 {
            return Err(SecretError::Failed("unrecognized credential format".into()));
        }
        let mut bytes = head[HEADER..].to_vec();
        for i in 1..head[1] as usize {
            match read(&part(&base, i))? {
                Some(chunk) => bytes.extend_from_slice(&chunk),
                None => return Err(SecretError::Failed("a credential part is missing".into())),
            }
        }
        String::from_utf8(bytes)
            .map(|s| Some(Secret::new(s)))
            .map_err(|_| SecretError::Failed("the credential is not UTF-8".into()))
    }

    fn delete(&self, key: &SecretKey) -> Result<bool, SecretError> {
        let base = target(key);
        let parts = read(&base)?.and_then(|b| b.get(1).copied()).unwrap_or(1) as usize;
        let existed = remove(&base)?;
        for i in 1..parts {
            remove(&part(&base, i))?;
        }
        Ok(existed)
    }

    fn list(&self, namespace: &str) -> Result<Vec<Id>, SecretError> {
        let filter = wide(&format!("{SERVICE}/{namespace}/*"));
        let mut count: u32 = 0;
        let mut items: *mut *mut CREDENTIALW = ptr::null_mut();
        // SAFETY: `filter` is NUL-terminated; on success `items` is an array freed below.
        if unsafe { CredEnumerateW(filter.as_ptr(), 0, &mut count, &mut items) } == 0 {
            let code = last_error();
            return if code == ERROR_NOT_FOUND {
                Ok(Vec::new())
            } else {
                Err(error(code))
            };
        }
        let prefix = format!("{SERVICE}/");
        let mut out = Vec::new();
        // SAFETY: CredEnumerateW succeeded: `items` holds `count` valid credential pointers
        // whose NUL-terminated target names live until CredFree.
        unsafe {
            for i in 0..count as usize {
                let c = &**items.add(i);
                if c.TargetName.is_null() {
                    continue;
                }
                let mut len = 0;
                while *c.TargetName.add(len) != 0 {
                    len += 1;
                }
                let name = String::from_utf16_lossy(std::slice::from_raw_parts(c.TargetName, len));
                if let Some(account) = name.strip_prefix(&prefix)
                    && let Some(h) = handle_of(account, namespace)
                {
                    out.push(h);
                }
            }
            CredFree(items as *const std::ffi::c_void);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    /// The real Credential Manager. Opt-in (`--ignored`).
    #[test]
    #[ignore = "touches the real Windows Credential Manager; run locally with --ignored"]
    fn credential_manager_round_trip() {
        super::super::round_trip(&super::CredentialManager);
    }
}
