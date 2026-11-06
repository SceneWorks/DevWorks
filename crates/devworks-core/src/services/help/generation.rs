//! Recoverable local catalog publication. Only a complete, source-fenced draft becomes visible.
use super::{
    APP_VERSION, EFFECT_CONSUMER, HelpService, JOB_KIND, bundle, commands::GenerationJob, metadata,
};
use crate::fingerprint::sha256_hex;
use crate::ports::packages::Inspection;
use crate::ports::storage::*;
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::{HelpProvenance, OperationRef};
use crate::protocol::results::OperationResult;
use crate::protocol::support::BoundedText;
use crate::protocol::{EventId, Id, OperationId, OperationState};
use crate::services::sessions::rev;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

#[derive(Clone, Debug, PartialEq)]
pub enum GenerationOutcome {
    Completed(Value),
    Failed(String),
    Unknown(String),
}

fn invalid(message: &str) -> StorageError {
    StorageError::InvalidRecord(message.into())
}
fn semantic(topic: &StoredHelpTopic) -> Value {
    json!({"id":topic.id,"version":topic.version,"title":topic.title,"digest":topic.content_digest,
        "availability":topic.availability,"reason":topic.reason,"provenance":topic.provenance,
        "operations":topic.related_operations,"screens":topic.screen_ids,"source":topic.source_kind,
        "package":topic.package_id,"manifest":topic.manifest_digest,"characters":topic.character_count})
}
fn completion(operation: &OperationId, head: &StoredHelpHead, no_change: bool) -> Value {
    json!({"result":OperationResult::HelpGenerateCapabilityDocs {value:OperationRef {
        operation_id:operation.clone(),state:OperationState::ObservedComplete}},
        "helpRevision":head.revision,"registryRevision":head.registry_revision,"generationId":head.generation_id,
        "noChange":no_change,"message":"Capability documentation regenerated."})
}
fn recorded_completion(operation: &OperationRecord) -> Option<Value> {
    operation
        .attempts
        .iter()
        .rev()
        .flat_map(|attempt| attempt.observations.iter().rev())
        .find_map(|observation| observation.get("helpPublication").cloned())
}

struct Document<'a> {
    id: Id,
    title: String,
    version: String,
    body: Body,
    provenance: HelpProvenance,
    source_kind: &'a str,
    operations: Vec<String>,
    screens: Vec<String>,
    package: Option<Id>,
    manifest: Option<String>,
}

enum Body {
    Inline(std::io::Cursor<String>),
    Guide(crate::services::packages::VerifiedHelp),
}
impl From<String> for Body {
    fn from(value: String) -> Self {
        Self::Inline(std::io::Cursor::new(value))
    }
}
impl Body {
    fn identity(&self) -> (String, u64) {
        match self {
            Self::Inline(c) => (sha256_hex(c.get_ref().as_bytes()), c.get_ref().len() as u64),
            Self::Guide(g) => (g.sha256.clone(), g.byte_length),
        }
    }
    fn reader(&mut self) -> &mut dyn std::io::Read {
        match self {
            Self::Inline(c) => c,
            Self::Guide(g) => g.reader.as_mut(),
        }
    }
    fn rewind(&mut self) -> std::io::Result<()> {
        match self {
            Self::Inline(c) => {
                c.set_position(0);
                Ok(())
            }
            Self::Guide(g) => g.reader.rewind(),
        }
    }
}
struct PreparedDocument {
    document: StoredHelpDocument,
    body: Option<Body>,
}
struct Control<'a> {
    service: &'a HelpService,
    job: &'a GenerationJob,
    calls: u32,
    error: Option<StorageError>,
}
impl Control<'_> {
    fn stopped(&mut self) -> bool {
        if self.error.is_some() {
            return true;
        }
        if self.service.is_stopped() {
            self.error = Some(StorageError::Cancelled);
            return true;
        }
        let check = self.calls.is_multiple_of(16);
        self.calls = self.calls.wrapping_add(1);
        if check {
            let state = read_snapshot(self.service.store.as_ref(), &ReadOptions::default(), |r| {
                r.queue().entry(&self.job.delivery_id)
            });
            match state {
                Ok(Some(row)) if !row.cancel_requested => {}
                Ok(Some(_)) => self.error = Some(StorageError::Cancelled),
                Ok(None) => self.error = Some(invalid("Help source delivery disappeared")),
                Err(error) => self.error = Some(error),
            }
        }
        self.error.is_some()
    }
    fn finish<T>(&mut self, result: Result<T, StorageError>) -> Result<T, StorageError> {
        match self.error.take() {
            Some(error) => Err(error),
            None => result,
        }
    }
}

impl HelpService {
    fn job(&self, operation: &OperationRecord) -> Result<GenerationJob, StorageError> {
        if operation.capability != "help.generate_capability_docs"
            || operation.schema_version != 1
            || operation.payload["kind"] != JOB_KIND
        {
            return Err(invalid("unsupported Help generation journal"));
        }
        serde_json::from_value(operation.payload.clone())
            .map_err(|_| invalid("invalid Help generation journal"))
    }

    fn current(
        &self,
        tx: &mut dyn WriteTransaction,
        operation: &OperationRecord,
        job: &GenerationJob,
    ) -> Result<(), StorageError> {
        if self.is_stopped() {
            return Err(StorageError::Cancelled);
        }
        if tx.store().store_id != job.store_id
            || tx.store().restore_epoch != job.restore_epoch
            || !crate::authority::service::fences_hold(tx, &job.fences)?
        {
            return Err(invalid("Help generation authority changed"));
        }
        let kind = tx.actors().actor_kind(&job.actor)?;
        if !matches!(
            (kind, job.actor_kind.as_str()),
            (Some(crate::context::PrincipalKind::Human), "Human")
                | (Some(crate::context::PrincipalKind::Host), "Host")
        ) {
            return Err(invalid("Help generation principal changed"));
        }
        let record = tx
            .operations()
            .operation(&operation.id)?
            .ok_or_else(|| invalid("Help operation disappeared"))?;
        if record.latest_attempt().map(|attempt| attempt.fence())
            != operation.latest_attempt().map(|attempt| attempt.fence())
            || !matches!(
                record.state,
                OperationState::Dispatching | OperationState::OutcomeUnknown
            )
        {
            return Err(StorageError::LeaseLost {
                kind: "help_generation",
                id: operation.id.0.as_str().into(),
            });
        }
        let entry = tx
            .queue()
            .entry(&job.delivery_id)?
            .ok_or_else(|| invalid("Help delivery disappeared"))?;
        if entry.consumer != EFFECT_CONSUMER || entry.operation_id.as_ref() != Some(&operation.id) {
            return Err(invalid("Help delivery binding changed"));
        }
        if entry.cancel_requested {
            return Err(StorageError::Cancelled);
        }
        let head = tx.help().help_head()?;
        let registry = tx.packages().registry_revision()?;
        if head.revision != job.expected_help_revision
            || head.generation_operation_id.as_ref() != Some(&operation.id)
            || registry != job.registry_revision
            || self.source_digest(registry) != job.input_digest
        {
            return Err(invalid("Help generation source changed"));
        }
        Ok(())
    }

    fn old_body(&self, generation: &Id, id: &Id) -> Result<String, StorageError> {
        let (topic, file) = read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
            let topic = read
                .help()
                .help_topic(generation, id)?
                .ok_or_else(|| invalid("previous Help topic disappeared"))?;
            let reference: ArtifactRef = serde_json::from_value(
                topic
                    .content_ref
                    .clone()
                    .ok_or(StorageError::ViewUnavailable)?,
            )
            .map_err(|_| invalid("Help reference"))?;
            let file = self
                .artifacts
                .capture_help_file(read, &self.store.info(), &reference)?;
            Ok((topic, file))
        })?;
        let mut stopped = || self.is_stopped();
        let mut source = self.artifacts.open_help_file(&file, &mut stopped)?;
        let mut body = String::new();
        super::text::visit(source.as_mut(), &mut stopped, |text| {
            body.push_str(text);
            Ok(false)
        })?;
        if sha256_hex(body.as_bytes()) != topic.content_digest
            || body.chars().count() as i64 != topic.character_count
        {
            return Err(invalid("previous Help source changed"));
        }
        Ok(body)
    }

    fn stage_document(
        &self,
        operation: &OperationRecord,
        job: &GenerationJob,
        input: PreparedDocument,
    ) -> Result<StoredHelpTopic, StorageError> {
        let PreparedDocument {
            mut document,
            mut body,
        } = input;
        let mut control = Control {
            service: self,
            job,
            calls: 0,
            error: None,
        };
        if control.stopped() {
            return control.finish(Err(StorageError::Cancelled));
        }
        let service = self.clone();
        let op = operation.clone();
        let source = job.clone();
        let metadata = document.topic.clone();
        let existing = run_write(&*self.store, WriteOptions::default(), move |tx| {
            service.current(tx, &op, &source)?;
            let existing = tx
                .help()
                .staged_help_topic(&source.generation, &metadata.id)?;
            if existing
                .as_ref()
                .is_some_and(|existing| semantic(existing) != semantic(&metadata))
            {
                return Err(invalid("Help draft source changed while recovering"));
            }
            Ok(existing)
        })?;
        if let Some(existing) = existing {
            return Ok(existing);
        }
        let old = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            let head = read.help().help_head()?;
            head.generation_id
                .as_ref()
                .map(|generation| read.help().help_topic(generation, &document.topic.id))
                .transpose()
                .map(Option::flatten)
        })?;
        document.topic.topic_revision = old.as_ref().map_or(1, |old| {
            old.topic_revision + i64::from(semantic(old) != semantic(&document.topic))
        });
        if document.topic.availability == "Available" {
            document.topic.content_ref =
                read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
                    read.help()
                        .help_content_reference(&document.topic.content_digest)
                })?;
            if let Some(reference) = document.topic.content_ref.clone() {
                let reference: ArtifactRef = serde_json::from_value(reference)
                    .map_err(|_| invalid("invalid previous Help artifact"))?;
                let captured =
                    read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
                        self.artifacts
                            .capture_help_file(read, &self.store.info(), &reference)
                    });
                let valid = match captured {
                    Ok(file) => self
                        .artifacts
                        .open_help_file(&file, &mut || control.stopped())
                        .map(|_| ()),
                    Err(error) => Err(error),
                };
                if valid.is_err() {
                    control.finish(Ok(()))?;
                    document.topic.content_ref = None;
                }
            }
        }
        let staged = if document.topic.availability == "Available"
            && document.topic.content_ref.is_none()
        {
            let body = body
                .as_mut()
                .ok_or_else(|| invalid("Help source reader missing"))?;
            body.rewind()
                .map_err(|_| invalid("Help source could not rewind"))?;
            let content = document
                .content
                .as_ref()
                .ok_or_else(|| invalid("Help projection missing"))?;
            let result=self.artifacts.stage_host_reader(self.store.as_ref(),crate::services::artifacts::host::HostStream {
                reader:body.reader(),digest:&content.digest,byte_count:content.byte_count,
                source:json!({"kind":"HelpTopic","generationId":job.generation,"topicId":document.topic.id,"helpIndexDigest":content.commitment().map_err(|_|invalid("Help index commitment"))?}),
            },&mut||control.stopped());
            Some(
                control
                    .finish(result)?
                    .map_err(|_| invalid("Help artifact bytes could not be published"))?,
            )
        } else {
            None
        };
        drop(body); // No full source, even an inline host document, enters the writer mailbox.
        let service = self.clone();
        let op = operation.clone();
        let source = job.clone();
        let artifact = staged.clone();
        let result = run_write(&*self.store, WriteOptions::default(), move |tx| {
            service.current(tx, &op, &source)?;
            if let Some(existing) = tx
                .help()
                .staged_help_topic(&source.generation, &document.topic.id)?
            {
                if semantic(&existing) != semantic(&document.topic) {
                    return Err(invalid("Help draft changed"));
                }
                return Ok((existing, false));
            }
            if let Some(staged) = artifact.as_ref() {
                let version=service.artifacts.commit_host_bytes(tx,staged,&document.topic.title,"text/markdown",
                    json!({"kind":"HelpTopic","generationId":source.generation,"topicId":document.topic.id,"operationId":op.id}))?;
                document.topic.content_ref = Some(
                    serde_json::to_value(crate::services::artifacts::version_ref(&version))
                        .map_err(|e| invalid(&e.to_string()))?,
                );
            }
            let topic = tx.help().stage_help_document(&document)?;
            Ok((topic, artifact.is_some()))
        });
        if let Some(staged) = staged.as_ref() {
            self.artifacts.settle_host_bytes(
                &*self.store,
                staged,
                result.as_ref().is_ok_and(|(_, committed)| *committed),
            );
        }
        result.map(|(topic, _)| topic)
    }

    fn document(
        &self,
        job: &GenerationJob,
        document: Document<'_>,
    ) -> Result<PreparedDocument, StorageError> {
        let Document {
            id,
            title,
            version,
            mut body,
            provenance,
            source_kind,
            operations,
            screens,
            package,
            manifest,
        } = document;
        let (digest, bytes) = body.identity();
        let mut control = Control {
            service: self,
            job,
            calls: 0,
            error: None,
        };
        let projected =
            super::text::index(body.reader(), &digest, bytes, &mut || control.stopped());
        let content = control.finish(projected)?;
        Ok(PreparedDocument {
            document: StoredHelpDocument {
                topic: StoredHelpTopic {
                    generation_id: job.generation.clone(),
                    id,
                    version,
                    title: title.clone(),
                    content_digest: digest,
                    content_ref: None,
                    availability: "Available".into(),
                    reason: None,
                    provenance: serde_json::to_value(provenance)
                        .map_err(|e| invalid(&e.to_string()))?,
                    related_operations: operations,
                    screen_ids: screens,
                    source_kind: source_kind.into(),
                    package_id: package,
                    manifest_digest: manifest,
                    character_count: content.character_count,
                    topic_revision: 1,
                },
                content: Some(content),
            },
            body: Some(body),
        })
    }

    fn build(
        &self,
        operation: &OperationRecord,
        job: &GenerationJob,
    ) -> Result<Value, StorageError> {
        let mut digest = Sha256::new();
        digest.update(job.input_digest.as_bytes());
        let mut count = 0i64;
        let mut stage = |document| -> Result<(), StorageError> {
            let topic = self.stage_document(operation, job, document)?;
            digest.update(semantic(&topic).to_string().as_bytes());
            digest.update([0]);
            count += 1;
            Ok(())
        };
        for topic in bundle::load()? {
            let provenance = HelpProvenance::Bundled {
                app_version: BoundedText(APP_VERSION.into()),
                content_digest: crate::protocol::provider::Digest(topic.digest),
            };
            stage(self.document(
                job,
                Document {
                    id: topic.id,
                    title: topic.title,
                    version: APP_VERSION.into(),
                    body: topic.body.into(),
                    provenance,
                    source_kind: "Bundled",
                    operations: topic.operations,
                    screens: topic.screens,
                    package: None,
                    manifest: None,
                },
            )?)?;
        }
        let registered = self
            .registered
            .read()
            .unwrap_or_else(|lock| lock.into_inner())
            .clone();
        for (operation_name, mode) in registered {
            let Some(metadata) = self
                .capability_metadata(&operation_name)
                .into_iter()
                .find(|metadata| metadata.mode.0 == mode)
            else {
                continue;
            };
            let id = Id::new(format!(
                "capability-{}",
                &sha256_hex(format!("{operation_name}\0{mode}").as_bytes())[..40]
            ))
            .expect("owned topic ID");
            let generated = metadata::render(&metadata);
            let old = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
                let head = read.help().help_head()?;
                head.generation_id
                    .as_ref()
                    .map(|generation| read.help().help_topic(generation, &id))
                    .transpose()
                    .map(Option::flatten)
            })?;
            let body = match old {
                Some(old) if old.source_kind == "Generated" && old.package_id.is_none() => {
                    metadata::replace_auto(&self.old_body(&old.generation_id, &old.id)?, &generated)
                }
                _ => format!("{generated}\n\n"),
            };
            let provenance = HelpProvenance::Generated {
                app_version: BoundedText(APP_VERSION.into()),
                registry_revision: rev(job.registry_revision),
                content_digest: crate::protocol::provider::Digest(sha256_hex(body.as_bytes())),
            };
            stage(self.document(
                job,
                Document {
                    id,
                    title: format!("{operation_name} ({mode})"),
                    version: APP_VERSION.into(),
                    body: body.into(),
                    provenance,
                    source_kind: "Generated",
                    operations: vec![operation_name],
                    screens: Vec::new(),
                    package: None,
                    manifest: None,
                },
            )?)?;
        }
        let mut after = None;
        loop {
            let rows = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
                if read.packages().registry_revision()? != job.registry_revision {
                    return Err(invalid("package registry changed"));
                }
                read.packages().packages(after.as_deref(), 100)
            })?;
            let done = rows.len() < 100;
            for package in rows {
                after = Some(package.package_id.clone());
                if !package.installed {
                    continue;
                }
                let Some(manifest) = package.active_digest.as_ref() else {
                    continue;
                };
                let version = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
                    read.packages()
                        .package_version(&package.package_id, manifest)
                })?
                .ok_or_else(|| invalid("active package inspection missing"))?;
                let package_id =
                    Id::new(&package.package_id).map_err(|_| invalid("invalid package ID"))?;
                let id = Id::new(format!(
                    "package-guide-{}",
                    &sha256_hex(package.package_id.as_bytes())[..40]
                ))
                .expect("owned guide ID");
                let mut control = Control {
                    service: self,
                    job,
                    calls: 0,
                    error: None,
                };
                let body = self
                    .packages
                    .as_ref()
                    .ok_or_else(|| {
                        crate::ports::packages::PackageFileError::Io(
                            "package file owner unavailable".into(),
                        )
                    })
                    .and_then(|service| {
                        service.open_verified_help(&version, &mut || control.stopped())
                    });
                control.finish(Ok(()))?;
                let inspection: Inspection = serde_json::from_value(version.inspection.clone())
                    .map_err(|_| invalid("invalid installed inspection"))?;
                let operations: Vec<String> = inspection
                    .contributions
                    .iter()
                    .map(|contribution| contribution.id.clone())
                    .collect();
                let prepared = match body {
                    Ok(body) => {
                        let provenance = HelpProvenance::Package {
                            package_id: package_id.clone(),
                            package_version: BoundedText(version.version.clone()),
                            manifest_digest: crate::protocol::provider::Digest(manifest.clone()),
                            content_digest: crate::protocol::provider::Digest(body.sha256.clone()),
                        };
                        match self.document(
                            job,
                            Document {
                                id: id.clone(),
                                title: format!("{} guide", package.package_id),
                                version: version.version.clone(),
                                body: Body::Guide(body),
                                provenance,
                                source_kind: "Package",
                                operations: operations.clone(),
                                screens: Vec::new(),
                                package: Some(package_id.clone()),
                                manifest: Some(manifest.clone()),
                            },
                        ) {
                            Ok(document) => Ok(document),
                            Err(StorageError::ViewUnavailable) => Err("Unavailable"),
                            Err(error) => return Err(error),
                        }
                    }
                    Err(crate::ports::packages::PackageFileError::Missing(_)) => Err("Missing"),
                    Err(_) => Err("Unavailable"),
                };
                match prepared {
                    Ok(document)=>stage(document)?,
                    Err(availability)=>stage(PreparedDocument {document:StoredHelpDocument {topic:StoredHelpTopic {
                        generation_id:job.generation.clone(),id,version:version.version.clone(),title:format!("{} guide",package.package_id),content_digest:String::new(),content_ref:None,availability:availability.into(),reason:Some("The exact installed guide could not be verified. Repair or reinstall this package version, then regenerate Help.".into()),provenance:Value::Null,related_operations:operations,screen_ids:Vec::new(),source_kind:"Package".into(),package_id:Some(package_id.clone()),manifest_digest:Some(manifest.clone()),character_count:0,topic_revision:1},content:None},body:None})?,
                }
                for contribution in inspection.contributions {
                    let id = Id::new(format!(
                        "package-contract-{}",
                        &sha256_hex(
                            format!("{}\0{}", package.package_id, contribution.id).as_bytes()
                        )[..40]
                    ))
                    .expect("owned contract ID");
                    let descriptor = contribution.descriptor.map(|value| {
                        serde_json::to_string_pretty(&value).expect("verified descriptor")
                    });
                    let body=format!("# Installed contribution contract\n\nPackage: `{}` version `{}`. Manifest digest: `{}`. Contribution: `{}` (`{}`). Contract digest: `{}`.\n\nThis is a verified installed declaration, not proof of activation or executable registration. Current package prerequisites, enablement, runtime availability and brokered authority still apply. Documentation never grants permissions.\n\n{}\n",package.package_id,version.version,manifest,contribution.id,contribution.kind,contribution.contract_digest,
                        descriptor.map_or("The original descriptor was not retained in this older inspection. Argument and result contract details are unknown.".into(),|descriptor|format!("```json\n{descriptor}\n```")));
                    let provenance = HelpProvenance::Package {
                        package_id: package_id.clone(),
                        package_version: BoundedText(version.version.clone()),
                        manifest_digest: crate::protocol::provider::Digest(manifest.clone()),
                        content_digest: crate::protocol::provider::Digest(sha256_hex(
                            body.as_bytes(),
                        )),
                    };
                    stage(self.document(
                        job,
                        Document {
                            id,
                            title: format!("{} contribution contract", package.package_id),
                            version: version.version.clone(),
                            body: body.into(),
                            provenance,
                            source_kind: "Package",
                            operations: vec![contribution.id],
                            screens: Vec::new(),
                            package: Some(package_id.clone()),
                            manifest: Some(manifest.clone()),
                        },
                    )?)?;
                }
            }
            if done {
                break;
            }
        }
        let source_digest = digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let service = self.clone();
        let operation = operation.clone();
        let job = job.clone();
        run_write(&*self.store, WriteOptions::default(), move |tx| {
            service.current(tx, &operation, &job)?;
            let old = tx.help().help_head()?;
            let candidate = StoredHelpHead {
                revision: job.expected_help_revision + 1,
                app_version: APP_VERSION.into(),
                registry_revision: job.registry_revision,
                source_digest,
                generation_id: Some(job.generation.clone()),
                generation_operation_id: Some(operation.id.clone()),
                updated_at_ms: tx.now_ms(),
            };
            let head = tx.help().publish_help_generation(
                job.expected_help_revision,
                job.registry_revision,
                Some(&operation.id),
                &candidate,
                count,
            )?;
            let result = completion(&operation.id, &head, head.revision == old.revision);
            let attempt = operation
                .latest_attempt()
                .ok_or_else(|| invalid("Help generation has no dispatched attempt"))?;
            tx.operations().observe(
                &operation.id,
                attempt.attempt_no,
                json!({"helpPublication":result}),
            )?;
            let event = EventId(tx.new_id(crate::ports::IdKind::Event));
            tx.history().append(NewEvent{id:event,actor:job.actor,project_id:None,root_id:None,operation_id:Some(operation.id.clone()),
                aggregate_kind:"help".into(),aggregate_id:"catalog".into(),aggregate_revision:head.revision,kind:"HelpGenerationPublished".into(),
                payload:json!({"helpRevision":head.revision,"registryRevision":head.registry_revision,"topicCount":count,"noChange":head.revision==old.revision})})?;
            Ok(result)
        })
    }

    /// Recovery first consumes an exact durable publication observation. Otherwise the same
    /// immutable draft can be resumed, under fresh source/authority/cancellation checks.
    pub fn execute(&self, operation: &OperationRecord) -> GenerationOutcome {
        let job = match self.job(operation) {
            Ok(job) => job,
            Err(_) => return GenerationOutcome::Failed("Invalid Help generation journal.".into()),
        };
        let current = match read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            read.operations().operation(&operation.id)
        }) {
            Ok(Some(current)) => current,
            Ok(None) => {
                return GenerationOutcome::Unknown(
                    "The original Help journal is unavailable; no new publication was attempted."
                        .into(),
                );
            }
            Err(_) => {
                return GenerationOutcome::Unknown(
                    "The Help journal could not be read; no new publication was attempted.".into(),
                );
            }
        };
        if let Some(result) = recorded_completion(&current) {
            return GenerationOutcome::Completed(result);
        }
        match self.build(operation,&job) {
            Ok(result)=>GenerationOutcome::Completed(result),
            Err(StorageError::Cancelled) if self.is_stopped()=>GenerationOutcome::Unknown("Help generation stopped before publication; its immutable draft is recoverable.".into()),
            Err(StorageError::Cancelled)=>GenerationOutcome::Failed("Help generation was cancelled before catalog publication.".into()),
            Err(StorageError::CommitUnknown | StorageError::Busy | StorageError::StorageUnavailable { .. }) => GenerationOutcome::Unknown("Help publication needs durable journal reconciliation; the original draft and operation are retained.".into()),
            Err(StorageError::LeaseLost {..})=>GenerationOutcome::Unknown("Help generation owner changed; no catalog was published.".into()),
            Err(_)=>GenerationOutcome::Failed("Help generation source, authority or local publication became unavailable; no partial catalog was published.".into()),
        }
    }
    pub fn observe(&self, operation: &OperationRecord) -> GenerationOutcome {
        self.execute(operation)
    }
}
