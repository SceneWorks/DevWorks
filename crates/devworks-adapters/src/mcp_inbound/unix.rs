//! Same-user local IPC. A private filesystem endpoint and kernel-provided peer credentials
//! are both required before the per-client application credential can be checked.

use super::{AuthenticatedClient, Binding, CredentialError, Credentials, Secret};
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tokio::net::{UnixListener, UnixStream};

pub struct PrivateListener {
    listener: UnixListener,
    path: PathBuf,
    inode: (u64, u64),
    owner: u32,
}

impl PrivateListener {
    /// The caller supplies a directory in the desktop's runtime root. This function owns only
    /// the socket entry inside it. An occupied path is refused.
    pub fn bind(directory: &Path) -> io::Result<Self> {
        // SAFETY: geteuid takes no arguments and returns the effective UID of this process.
        let owner = unsafe { libc::geteuid() };
        match fs::DirBuilder::new().mode(0o700).create(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata = fs::symlink_metadata(directory)?;
        if !metadata.is_dir() || metadata.uid() != owner || metadata.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "MCP IPC needs a private directory owned by the desktop user",
            ));
        }
        let path = directory.join("mcp.sock");
        let listener = UnixListener::bind(&path)?;
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_socket() || metadata.uid() != owner {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "MCP socket owner changed",
            ));
        }
        let owned = Self {
            listener,
            path,
            inode: (metadata.dev(), metadata.ino()),
            owner,
        };
        fs::set_permissions(&owned.path, fs::Permissions::from_mode(0o600))?;
        Ok(owned)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn accept(&self) -> io::Result<PrivateConnection> {
        let (stream, _) = self.listener.accept().await?;
        if stream.peer_cred()?.uid() != self.owner {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "MCP IPC peer denied",
            ));
        }
        Ok(PrivateConnection { stream })
    }
}

impl Drop for PrivateListener {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
            metadata.file_type().is_socket() && (metadata.dev(), metadata.ino()) == self.inode
        }) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub struct PrivateConnection {
    stream: UnixStream,
}

impl PrivateConnection {
    pub fn into_stream(self) -> UnixStream {
        self.stream
    }

    pub fn stream(&mut self) -> &mut UnixStream {
        &mut self.stream
    }

    pub fn authenticate(
        &self,
        credentials: &Credentials,
        credential: &Secret,
    ) -> Result<AuthenticatedClient, CredentialError> {
        credentials.authenticate(Binding::LocalIpc, credential)
    }
}
