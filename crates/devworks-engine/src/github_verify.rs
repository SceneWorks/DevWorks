//! Host-owned GitHub reproduction. A local container receives a Git archive on stdin, without
//! a working-copy mount, host credentials, shared Git metadata or external network access, and
//! executes only a check definition committed in `<oid>:.devworks/github-checks.json`.
use crate::dispatch::{Consumer, Delivery, DeliveryHandler, Outcome};
use crate::effect_consumer::{PanicReasons, STANDARD_RETRY, effect_consumer};
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::process::ProcessSpec;
use devworks_core::ports::storage::{
    ApplicationStore, Cancellation, OperationRecord, StorageError, WriteOptions, WriteTransaction,
    run_write,
};
use devworks_core::protocol::{Counter, Id, OperationState, Revision};
use devworks_core::services::effect::{EffectOutcome, JournaledEffect};
use devworks_core::services::git::client::Git;
use devworks_core::services::git::helper::{HelperFault, HelperLimits, HelperRunner};
use devworks_core::services::github::{GitHubService, verification};
use devworks_core::services::runs::ControlService;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const LIMIT: usize = 64 * 1024 * 1024;
const RUNNER: &str = r#"mkdir -p /verification/source
tar -xf - -C /verification/source
cd /verification/source
printf '%s\n' "$1" >&2
shift
exec "$@" >&2"#;

pub struct Verifier {
    service: Arc<GitHubService>,
    store: Arc<dyn ApplicationStore>,
    helpers: Arc<dyn HelperRunner>,
    control: Arc<ControlService>,
    git: String,
}

impl Verifier {
    pub fn consumer(
        service: Arc<GitHubService>,
        store: Arc<dyn ApplicationStore>,
        helpers: Arc<dyn HelperRunner>,
        control: Arc<ControlService>,
        git: String,
    ) -> Consumer {
        let owner = Arc::new(Self {
            service,
            store,
            helpers,
            control,
            git,
        });
        let mut consumer = effect_consumer(
            verification::CONSUMER,
            owner.clone(),
            PanicReasons {
                execute: "GitHub verification interrupted after launch",
                observe: "GitHub verification evidence unavailable",
            },
            STANDARD_RETRY,
            Some(1),
        );
        consumer.handler = owner;
        consumer
    }

    fn execute_once(
        &self,
        operation: &OperationRecord,
        cancelled: &Cancellation,
    ) -> Result<Option<verification::Witness>, HelperFault> {
        let job = verification::job(operation).map_err(storage)?;
        let Some(location) = self
            .service
            .verification_target(operation, &job)
            .map_err(storage)?
        else {
            return Ok(None);
        };
        let ctx = context(&*self.store, operation, &job).map_err(storage)?;
        let stopped = || cancelled.is_cancelled() || self.service.is_stopped();
        let probe = HelperLimits {
            deadline: Duration::from_secs(5),
            max_stdout: 4096,
        };
        let program = "docker";
        let inspect = docker_spec(
            program,
            &[
                "context",
                "inspect",
                "--format",
                "{{.Endpoints.docker.Host}}",
            ],
            None,
        );
        let host = match self.helpers.run(&ctx, inspect, probe, &stopped) {
            Ok(data) => String::from_utf8(data).ok(),
            Err(HelperFault::Cancelled) => return Err(HelperFault::Cancelled),
            Err(_) => return Ok(None),
        };
        let Some(host) = host
            .as_deref()
            .map(str::trim)
            .filter(|host| local_socket(host))
        else {
            return Ok(None);
        };
        let git = Git {
            runner: &*self.helpers,
            ctx: &ctx,
            program: &self.git,
            deadline: Duration::from_secs(30),
            stop: &stopped,
        };
        // Re-verify against the immutable commit's committed check manifest before launching any container.
        let Ok(Some(manifest_bytes)) = git.show_blob(
            &location,
            &job.checkout,
            verification::CHECKS_MANIFEST_PATH,
            65_536,
        ) else {
            return Ok(None);
        };
        let checks = verification::parse_manifest(&manifest_bytes);
        if !checks.iter().any(|check| {
            check.id == job.recipe.check_id
                && check.image == job.recipe.image
                && check.argv == job.recipe.argv
        }) {
            return Ok(None);
        }
        let Ok(archive) = git.archive_commit(&location, &job.checkout, LIMIT) else {
            return Ok(None);
        };
        if stopped()
            || self
                .service
                .verification_target(operation, &job)
                .map_err(storage)?
                .as_ref()
                != Some(&location)
        {
            return Ok(None);
        }
        let name = format!(
            "devworks-proof-{}",
            &sha256_hex(operation.id.0.as_str().as_bytes())[..32]
        );
        let marker = format!("devworks-proof-start:{name}");
        let argv = command(host, &name, &marker, &job);
        let digest = sha256_hex(argv.join("\0").as_bytes());
        let refs = argv.iter().skip(1).map(String::as_str).collect::<Vec<_>>();
        let mut spec = docker_spec(program, &refs, Some(archive));
        // No registered checkout path or application-data path is passed to Docker.
        spec.cwd = None;
        let result = self.helpers.run_recorded(
            &ctx,
            spec,
            HelperLimits {
                deadline: Duration::from_secs(180),
                max_stdout: 64 * 1024,
            },
            &stopped,
            &operation.id,
            false,
        );
        match result {
            Err(HelperFault::Failed {
                code: Some(code),
                stderr,
            }) if (1..=124).contains(&code)
                && reproduced(
                    &stderr,
                    &marker,
                    &job.recipe.test_identifier,
                    &job.recipe.expected_failure,
                ) =>
            {
                Ok(Some(verification::Witness {
                    checkout: job.checkout,
                    argv_digest: digest,
                    exit_code: i64::from(code),
                    evidence_digest: sha256_hex(stderr.as_bytes()),
                }))
            }
            Err(HelperFault::Cancelled | HelperFault::TimedOut | HelperFault::Uncertain(_)) => {
                // Remove only this application's named proof container, which is created with
                // the exact operation label. A random host container can never match both.
                self.reap(&ctx, host, &name, &operation.id.0);
                Err(HelperFault::Cancelled)
            }
            _ => Ok(None),
        }
    }

    fn reap(&self, ctx: &InvocationContext, host: &str, name: &str, id: &Id) {
        let limits = HelperLimits {
            deadline: Duration::from_secs(5),
            max_stdout: 4096,
        };
        let inspect = docker_spec(
            "docker",
            &[
                "--host",
                host,
                "inspect",
                "--format",
                r#"{{index .Config.Labels "devworks.verification"}}"#,
                name,
            ],
            None,
        );
        let owned = self
            .helpers
            .run(ctx, inspect, limits, &|| false)
            .is_ok_and(|value| {
                std::str::from_utf8(&value).is_ok_and(|value| value.trim() == id.as_str())
            });
        if owned {
            let stop = docker_spec("docker", &["--host", host, "rm", "--force", name], None);
            let _ = self.helpers.run(ctx, stop, limits, &|| false);
        }
    }
}

impl JournaledEffect for Verifier {
    fn resumes_after_observation(&self) -> bool {
        true
    }
    fn observes_delivery_cancellation(&self) -> bool {
        true
    }
    fn execute(&self, operation: &OperationRecord) -> EffectOutcome {
        self.execute_cancellable(operation, &Cancellation::new())
    }
    fn execute_cancellable(
        &self,
        operation: &OperationRecord,
        cancelled: &Cancellation,
    ) -> EffectOutcome {
        if operation.state != OperationState::Dispatching {
            return EffectOutcome::Failed("Invalid GitHub verification attempt".into());
        }
        let witness = match self.execute_once(operation, cancelled) {
            Ok(witness) => witness,
            Err(_) => {
                return EffectOutcome::Unknown(
                    "GitHub verification waits for its recorded result".into(),
                );
            }
        };
        let proof = json!({"witness":witness});
        let id = operation.id.clone();
        let Some(attempt) = operation.latest_attempt().map(|row| row.attempt_no) else {
            return EffectOutcome::Unknown("GitHub verification attempt was not recorded".into());
        };
        let durable = proof.clone();
        match run_write(&*self.store, WriteOptions::observed(), move |tx| {
            tx.operations()
                .observe(&id, attempt, json!({"verificationResult":durable}))
        }) {
            Ok(()) => EffectOutcome::Completed(proof),
            Err(_) => EffectOutcome::Unknown("GitHub verification result is not committed".into()),
        }
    }
    fn observe(&self, operation: &OperationRecord) -> EffectOutcome {
        operation
            .latest_attempt()
            .and_then(|attempt| {
                attempt
                    .observations
                    .iter()
                    .rev()
                    .find_map(|entry| entry.get("verificationResult"))
            })
            .map_or_else(
                || EffectOutcome::Unknown("No sealed GitHub verification result".into()),
                |entry| EffectOutcome::Completed(entry.clone()),
            )
    }
}

impl DeliveryHandler for Verifier {
    fn apply(
        &self,
        tx: &mut dyn WriteTransaction,
        delivery: &Delivery,
    ) -> Result<Outcome, StorageError> {
        let id = delivery.entry.operation_id.as_ref().ok_or_else(invalid)?;
        let record = tx.operations().operation(id)?.ok_or_else(invalid)?;
        let proof = record
            .latest_attempt()
            .and_then(|attempt| {
                attempt
                    .observations
                    .iter()
                    .rev()
                    .find_map(|entry| entry.pointer("/verificationResult/witness"))
            })
            .ok_or_else(invalid)?;
        let witness = serde_json::from_value(proof.clone()).map_err(|_| invalid())?;
        match verification::settle(tx, &self.control, &record, witness) {
            Ok(()) => Ok(Outcome::Completed),
            Err(StorageError::ScopeDenied) => Ok(Outcome::Hold(
                "GitHub report or verification scope changed".into(),
            )),
            Err(error) => Err(error),
        }
    }
}

fn docker_spec(program: &str, args: &[&str], stdin: Option<Vec<u8>>) -> ProcessSpec {
    let mut argv = vec![program.to_owned()];
    argv.extend(args.iter().map(|arg| (*arg).to_owned()));
    let mut spec = ProcessSpec::new(&argv).expect("Docker command");
    spec.clear_env = true;
    for key in [
        "PATH",
        "HOME",
        "USERPROFILE",
        "SYSTEMROOT",
        "WINDIR",
        "LOCALAPPDATA",
        "APPDATA",
    ] {
        if let Ok(value) = std::env::var(key) {
            spec.env.push((key.to_owned(), value));
        }
    }
    spec.stdin = stdin;
    spec
}

fn local_socket(host: &str) -> bool {
    !host.chars().any(char::is_control)
        && (host
            .strip_prefix("unix://")
            .is_some_and(|path| Path::new(path).is_absolute() && !path.starts_with("//"))
            || host.starts_with("npipe:////./pipe/"))
}

fn command(host: &str, name: &str, marker: &str, job: &verification::Job) -> Vec<String> {
    let args = [
        "docker",
        "--host",
        host,
        "run",
        "--rm",
        "--pull=never",
        "--network=none",
        "--read-only",
        "--cap-drop=ALL",
        "--security-opt=no-new-privileges",
        "--pids-limit=128",
        "--memory=2g",
        "--cpus=2",
        "--user=65534:65534",
        "--name",
        name,
        "--tmpfs",
        "/verification:rw,exec,nosuid,nodev,size=512m,mode=1777",
        "--tmpfs",
        "/tmp:rw,exec,nosuid,nodev,size=128m,mode=1777", // dev-host: allow-path ephemeral tmpfs inside isolated container
        "--env",
        "HOME=/tmp", // dev-host: allow-path container-internal HOME on ephemeral tmpfs
        "--interactive",
        "--entrypoint",
        "/bin/sh",
    ];
    let mut argv = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
    // All host options precede the image. Every trusted check argument follows the fixed entrypoint.
    let tail = ["-eu", "-c", RUNNER, "devworks-proof", marker];
    argv.extend([
        "--label".into(),
        format!("devworks.verification={}", job.operation_id.0.as_str()),
        job.recipe.image.clone(),
    ]);
    argv.extend(tail.into_iter().map(str::to_owned));
    argv.extend(job.recipe.argv.clone());
    argv
}

fn reproduced(stderr: &str, marker: &str, test: &str, expected: &str) -> bool {
    stderr.split_once(marker).is_some_and(|(_, trace)| {
        trace.lines().any(|line| line.contains(test))
            && trace.lines().any(|line| line.contains(expected))
    })
}

fn context(
    store: &dyn ApplicationStore,
    operation: &OperationRecord,
    job: &verification::Job,
) -> Result<InvocationContext, StorageError> {
    let info = store.info();
    Ok(HostIngress::new(store)
        .context(
            info.host_actor,
            PrincipalKind::Host,
            operation.id.0.clone(),
            Id::new("github-verifier").expect("static host identity"),
            Counter::new(info.restore_epoch).map_err(|_| invalid())?,
            Revision::new(1).expect("positive policy revision"),
        )
        .with_lineage(
            Some(job.recipe.project_id.clone()),
            operation.root_id.clone(),
            None,
        ))
}

fn storage(error: StorageError) -> HelperFault {
    HelperFault::Storage(error.to_string())
}
fn invalid() -> StorageError {
    StorageError::InvalidRecord("Invalid GitHub verification result".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn setup_or_unrelated_failure_cannot_match_the_report() {
        assert!(!reproduced(
            "test-id\nexpected assertion",
            "ready",
            "test-id",
            "expected assertion"
        ));
        assert!(!reproduced(
            "ready\ntest-id\nmissing cargo package",
            "ready",
            "test-id",
            "expected assertion"
        ));
        assert!(reproduced(
            "ready\ntest-id\nexpected assertion",
            "ready",
            "test-id",
            "expected assertion"
        ));
    }

    #[test]
    fn verifier_rejects_remote_daemons() {
        assert!(local_socket("unix:///var/run/docker.sock"));
        assert!(local_socket("npipe:////./pipe/docker_engine"));
        assert!(!local_socket("ssh://build-host"));
        assert!(!local_socket("tcp://127.0.0.1:2375"));
    }
}
