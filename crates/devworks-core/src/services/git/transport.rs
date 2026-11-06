//! All push network I/O runs with a private client Git directory and immutable host auth.
//! A file/SSH/HTTPS server is a distinct authority; this does not confine its receive hooks.
use super::*;
use crate::ports::git_transport::GitTransportContext;
use crate::ports::process::ProcessSpec;
use crate::protocol::OperationId;
use crate::services::indexing::helper::HelperLimits;
use std::path::PathBuf;

pub(super) struct PushTransport<'a> {
    context: Box<dyn GitTransportContext>,
    runner: &'a dyn HelperRunner,
    objects: PathBuf,
    environment: Vec<(String, String)>,
    pub digest: String,
}
fn refused() -> Failure {
    Refusal::new(
        DomainErrorCode::UnsupportedCapability,
        "git_transport_isolation",
        None,
    )
    .into()
}
fn auth_key(key: &str) -> bool {
    matches!(key, "core.sshcommand" | "core.askpass" | "ssh.variant")
        || key.starts_with("credential.")
        || key.starts_with("http.")
}
fn executable_key(key: &str) -> bool {
    matches!(key, "core.sshcommand" | "core.askpass" | "core.gitproxy")
        || (key.starts_with("credential.")
            && (key.ends_with(".helper") || key.ends_with(".credentialstore")))
        || (key.starts_with("remote.")
            && [".uploadpack", ".receivepack", ".vcs", ".proxy"]
                .iter()
                .any(|end| key.ends_with(end)))
        || (key.starts_with("hook.") && key.ends_with(".command"))
}
fn quoted(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}
impl GitService {
    pub(super) fn push_transport<'a>(
        &'a self,
        git: &Git<'_>,
        path: &Path,
        format: &ObjectFormat,
        repository: &Id,
    ) -> Result<PushTransport<'a>, Failure> {
        let files = self.config.transport.as_ref().ok_or_else(refused)?;
        let (_, common) = git.metadata_dirs(path).map_err(|_| refused())?;
        let common_path = Path::new(&common);
        let repository_root = common_path.parent().ok_or_else(refused)?;
        let bytes = git
            .transport_configuration(path)
            .map_err(|e| read_failure(e, repository))?;
        let fields: Vec<_> = bytes
            .split(|b| *b == 0)
            .filter(|part| !part.is_empty())
            .collect();
        if !fields.len().is_multiple_of(3) {
            return Err(refused());
        }
        let mut config = String::from(
            "[core]\n bare = true\n fsmonitor = \n hooksPath = /dev/null\n[protocol]\n version = 2\n allow = never\n[protocol \"https\"]\n allow = always\n[protocol \"ssh\"]\n allow = always\n[protocol \"file\"]\n allow = always\n[http]\n followRedirects = false\n[maintenance]\n auto = false\n[gc]\n auto = 0\n[push]\n gpgSign = false\n",
        );
        let mut auth = Vec::new();
        for parts in fields.as_chunks::<3>().0 {
            let scope = std::str::from_utf8(parts[0]).map_err(|_| refused())?;
            // origin is intentionally not included in proposal/journal evidence either.
            let entry = std::str::from_utf8(parts[2]).map_err(|_| refused())?;
            let (key, value) = entry.split_once('\n').unwrap_or((entry, "true"));
            let lower = key.to_ascii_lowercase();
            if matches!(scope, "local" | "worktree") && executable_key(&lower) {
                return Err(Refusal::new(
                    DomainErrorCode::PermissionDenied,
                    "git_repository_transport_program",
                    None,
                )
                .into());
            }
            if matches!(scope, "system" | "global") && auth_key(&lower) {
                let origin = std::str::from_utf8(parts[1])
                    .map_err(|_| refused())?
                    .strip_prefix("file:")
                    .ok_or_else(refused)?;
                if !files
                    .trusted_origin(Path::new(origin), &[path, repository_root])
                    .map_err(|_| refused())?
                {
                    return Err(Refusal::new(
                        DomainErrorCode::PermissionDenied,
                        "git_repository_auth_include",
                        None,
                    )
                    .into());
                }
                auth.push((key.to_owned(), value.to_owned()));
            }
        }
        // Preserve trusted multivalue authentication order. Never retain include/url/remote,
        // hook/alias/maintenance settings or source repository configuration.
        for (key, value) in auth {
            let (section, rest) = key.split_once('.').ok_or_else(refused)?;
            if let Some((subsection, name)) = rest.rsplit_once('.') {
                config.push_str(&format!(
                    "[{section} {}]\n {name} = {}\n",
                    quoted(subsection),
                    quoted(&value)
                ));
            } else {
                config.push_str(&format!("[{section}]\n {rest} = {}\n", quoted(&value)));
            }
        }
        // Host authentication may not redirect this exact reviewed destination.
        config.push_str("[http]\n followRedirects = false\n");
        if matches!(format, ObjectFormat::Sha256) {
            config.push_str(
                "[core]\n repositoryFormatVersion = 1\n[extensions]\n objectFormat = sha256\n",
            );
        }
        let mut environment: Vec<_> = self
            .helpers
            .environment()
            .into_iter()
            .filter(|(key, _)| !key.to_ascii_uppercase().starts_with("GIT_"))
            .collect();
        environment.sort();
        let fingerprint =
            serde_json::to_vec(&(self.config.program.as_str(), config.as_str(), &environment))
                .map_err(|_| refused())?;
        let digest = crate::fingerprint::sha256_hex(&fingerprint);
        let context = files.create(config.as_bytes()).map_err(|_| refused())?;
        Ok(PushTransport {
            context,
            runner: &*self.helpers,
            objects: PathBuf::from(common).join("objects"),
            environment,
            digest,
        })
    }
}
impl HelperRunner for PushTransport<'_> {
    fn environment(&self) -> Vec<(String, String)> {
        self.environment.clone()
    }
    fn run(
        &self,
        ctx: &InvocationContext,
        spec: ProcessSpec,
        limits: HelperLimits,
        stop: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<u8>, HelperFault> {
        self.runner.run(ctx, self.isolate(spec)?, limits, stop)
    }
    fn run_recorded(
        &self,
        ctx: &InvocationContext,
        spec: ProcessSpec,
        limits: HelperLimits,
        stop: &(dyn Fn() -> bool + Sync),
        operation: &OperationId,
        approved_hold: bool,
    ) -> Result<Vec<u8>, HelperFault> {
        self.runner.run_recorded(
            ctx,
            self.isolate(spec)?,
            limits,
            stop,
            operation,
            approved_hold,
        )
    }
}
impl PushTransport<'_> {
    fn isolate(&self, mut spec: ProcessSpec) -> Result<ProcessSpec, HelperFault> {
        let dir = self.context.directory();
        let value = |path: &Path| {
            crate::services::indexing::git::path_argument(path)
                .map_err(|_| HelperFault::Refused("Git transport path unavailable".into()))
        };
        spec.cwd = Some(dir.to_owned());
        spec.env
            .retain(|(key, _)| !key.to_ascii_uppercase().starts_with("GIT_"));
        spec.env.extend([
            ("GIT_DIR".into(), value(dir)?),
            ("GIT_OBJECT_DIRECTORY".into(), value(&self.objects)?),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("GIT_CONFIG_SYSTEM".into(), null_device().into()),
            ("GIT_CONFIG_GLOBAL".into(), null_device().into()),
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
            ("GIT_OPTIONAL_LOCKS".into(), "0".into()),
            ("GIT_NO_REPLACE_OBJECTS".into(), "1".into()),
        ]);
        Ok(spec)
    }
}
#[cfg(windows)]
fn null_device() -> &'static str {
    "NUL"
}
#[cfg(not(windows))]
fn null_device() -> &'static str {
    "/dev/null"
}
