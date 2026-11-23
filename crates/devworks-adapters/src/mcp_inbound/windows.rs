//! Same-user incoming MCP IPC on Windows. The OS restricts opening the pipe. The
//! server also checks the token of the peer whose message the pipe just read.

use super::{AuthenticatedClient, Binding, CredentialError, Credentials, Secret};
use devworks_core::protocol::Id;
use std::ffi::c_void;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{NonNull, null_mut};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, RevertToSelf, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Pipes::ImpersonateNamedPipeClient;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken,
};

pub struct PrivatePipe {
    pipe: NamedPipeServer,
    name: String,
    owner: String,
}

impl PrivatePipe {
    /// A store-specific unambiguous name in the Windows local pipe namespace.
    pub fn bind(store: &Id) -> io::Result<Self> {
        let name = format!(r"\\.\pipe\devworks-mcp-{}", store.as_str()); // dev-host: allow-path Windows named pipe namespace
        let owner = process_sid()?;
        let sddl: Vec<u16> = format!("O:{owner}D:P(A;;GA;;;{owner})\0")
            .encode_utf16()
            .collect();
        let mut descriptor = null_mut();
        // SAFETY: SDDL is NUL-terminated and descriptor points to an initialized out pointer.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let descriptor =
            LocalBuffer(NonNull::new(descriptor).ok_or_else(io::Error::last_os_error)?);
        let mut security = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0.as_ptr(),
            bInheritHandle: 0,
        };
        // SAFETY: The descriptor and security attributes live through the system call. The
        // handle is deliberately non-inheritable. The pipe refuses remote connections.
        let pipe = unsafe {
            ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(
                    &name,
                    (&mut security as *mut SECURITY_ATTRIBUTES).cast(),
                )?
        };
        Ok(Self { pipe, name, owner })
    }

    pub fn path(&self) -> &str {
        &self.name
    }

    pub async fn connect(&self) -> io::Result<()> {
        self.pipe.connect().await
    }

    pub fn stream(&mut self) -> &mut NamedPipeServer {
        &mut self.pipe
    }

    /// Call this after the bounded pairing frame was read. Win32 associates impersonation with
    /// the security context of the last message read, even for the same open client connection.
    pub fn authenticate(
        &self,
        credentials: &Credentials,
        credential: &Secret,
    ) -> Result<AuthenticatedClient, CredentialError> {
        let peer = pipe_peer_sid(&self.pipe).map_err(|_| CredentialError::Denied)?;
        if peer != self.owner {
            return Err(CredentialError::Denied);
        }
        credentials.authenticate(Binding::LocalIpc, credential)
    }
}

struct LocalBuffer(NonNull<c_void>);

impl Drop for LocalBuffer {
    fn drop(&mut self) {
        // SAFETY: The buffer was allocated by the Windows security string conversion API.
        unsafe { LocalFree(self.0.as_ptr()) };
    }
}

fn process_sid() -> io::Result<String> {
    let mut handle = null_mut();
    // SAFETY: The process pseudo handle is valid and handle is an initialized out pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: OpenProcessToken returned a new token handle, now owned by this function.
    token_sid(unsafe { OwnedHandle::from_raw_handle(handle) })
}

struct Impersonating;

impl Drop for Impersonating {
    fn drop(&mut self) {
        // SAFETY: This runs on the synchronous thread which successfully impersonated the peer.
        if unsafe { RevertToSelf() } == 0 {
            // Continuing could run later operations under the peer's identity.
            std::process::abort();
        }
    }
}

fn pipe_peer_sid(pipe: &NamedPipeServer) -> io::Result<String> {
    // SAFETY: This is the live server handle of a named pipe. No asynchronous work occurs
    // until the impersonation is reverted.
    if unsafe { ImpersonateNamedPipeClient(pipe.as_raw_handle()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _impersonating = Impersonating;
    let mut handle = null_mut();
    // SAFETY: The thread is impersonating a pipe client and handle is a valid out pointer.
    // OpenAsSelf opens the identification token without elevating its rights.
    if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut handle) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: OpenThreadToken returned a new token handle, now owned by this function.
    token_sid(unsafe { OwnedHandle::from_raw_handle(handle) })
}

fn token_sid(token: OwnedHandle) -> io::Result<String> {
    let mut length = 0;
    // SAFETY: With a null buffer and zero length this call asks Windows for the buffer size.
    if unsafe { GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut length) }
        != 0
    {
        return Err(io::Error::other("Expected a token SID size"));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32)
        || !(std::mem::size_of::<TOKEN_USER>() as u32..=4096).contains(&length)
    {
        return Err(error);
    }
    let mut buffer = vec![0_usize; (length as usize).div_ceil(std::mem::size_of::<usize>())];
    let mut written = 0;
    // SAFETY: The buffer is pointer-aligned and holds at least length bytes. Its size and type
    // are those required by the first GetTokenInformation call.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            length,
            &mut written,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if written < std::mem::size_of::<TOKEN_USER>() as u32 {
        return Err(io::Error::other("Incomplete token SID"));
    }
    // SAFETY: On success TokenUser initialized TOKEN_USER with a SID valid while buffer lives.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = null_mut();
    // SAFETY: The SID is still live and sid is an initialized output pointer.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let owner = LocalBuffer(NonNull::new(sid.cast()).ok_or_else(io::Error::last_os_error)?);
    let sid = owner.0.as_ptr().cast::<u16>();
    let mut length = 0;
    // SAFETY: ConvertSidToStringSidW returned a NUL-terminated UTF-16 string.
    while unsafe { *sid.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: The preceding loop found the terminator inside this live allocation.
    let value = unsafe { std::slice::from_raw_parts(sid, length) };
    String::from_utf16(value).map_err(|_| io::Error::other("Invalid token SID"))
}
