//! Owner-only disposable bare Git contexts. No public artifact or durable secret snapshot.
use devworks_core::ports::git_transport::{GitTransportContext, GitTransportFiles};
use std::path::{Path, PathBuf};
#[derive(Default)]
pub struct NativeGitTransportFiles;
struct Context(PathBuf);
impl GitTransportContext for Context {
    fn directory(&self) -> &Path {
        &self.0
    }
}
impl Drop for Context {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
impl GitTransportFiles for NativeGitTransportFiles {
    fn trusted_origin(&self, origin: &Path, repositories: &[&Path]) -> Result<bool, String> {
        let origin = std::fs::canonicalize(origin).map_err(|_| "Git auth origin unavailable")?;
        for repository in repositories {
            let root = std::fs::canonicalize(repository)
                .map_err(|_| "Git repository identity unavailable")?;
            if origin.starts_with(root) {
                return Ok(false);
            }
        }
        Ok(true)
    }
    fn create(&self, configuration: &[u8]) -> Result<Box<dyn GitTransportContext>, String> {
        if configuration.len() > 1024 * 1024 {
            return Err("Git transport configuration exceeds its bound".into());
        }
        let mut nonce = [0u8; 24];
        getrandom::fill(&mut nonce).map_err(|_| "Git transport entropy unavailable")?;
        let name: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
        let path = std::env::temp_dir().join(format!("devworks-git-transport-{name}"));
        private_directory(&path).map_err(|_| "Private Git transport directory unavailable")?;
        let context = Context(path);
        let write = || -> std::io::Result<()> {
            std::fs::create_dir(context.0.join("objects"))?;
            std::fs::create_dir(context.0.join("refs"))?;
            std::fs::write(
                context.0.join("HEAD"),
                b"ref: refs/heads/devworks-transport\n",
            )?;
            use std::io::Write;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options
                .open(context.0.join("config"))?
                .write_all(configuration)
        };
        write().map_err(|_| "Private Git transport configuration unavailable")?;
        Ok(Box::new(context))
    }
}
#[cfg(unix)]
fn private_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(path)
}
#[cfg(windows)]
fn private_directory(path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SECURITY_ATTRIBUTES,
        },
        Storage::FileSystem::CreateDirectoryW,
    };
    // A protected DACL grants only SYSTEM and the owner, inherited by config children.
    let sddl: Vec<u16> = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;OW)\0"
        .encode_utf16()
        .collect();
    let mut descriptor = std::ptr::null_mut();
    unsafe {
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let result = CreateDirectoryW(path.as_ptr(), &attributes);
        let error = std::io::Error::last_os_error();
        LocalFree(descriptor);
        if result == 0 { Err(error) } else { Ok(()) }
    }
}
#[cfg(not(any(unix, windows)))]
fn private_directory(_: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Private Git context unsupported",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configuration_is_private_and_removed_after_the_context_lifetime() {
        let context = NativeGitTransportFiles
            .create(b"[core]\n bare = true\n")
            .unwrap();
        let directory = context.directory().to_path_buf();
        assert!(directory.join("config").is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(directory.join("config"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(
            !NativeGitTransportFiles
                .trusted_origin(&directory.join("config"), &[&directory])
                .unwrap()
        );
        drop(context);
        assert!(!directory.exists());
    }
}
