//! Durable per-occurrence vision capture and normalization. The direct reader's original
//! text and citation identities remain retained while a separate routed extraction is built.
use super::{DocumentExtraction, DocumentIndexer, Job, SOURCE_LIMIT, code, invalid};
use crate::{
    context::InvocationContext,
    fingerprint::canonical_digest,
    locks::RwLockExt,
    ports::{
        AuthorityFence,
        storage::{
            ArtifactScope, AttemptState, Cancellation, DocumentCheckpoint, QueueEntry, ReadOptions,
            StorageError, WriteTransaction, read_snapshot,
        },
    },
    protocol::{
        Id, OperationState, Revision,
        data::{CoverageState, Locator},
        provider::ArtifactRef,
    },
    services::{
        artifacts::{host::HostArtifactPublication, lifecycle::ExactReadRefusal, version_ref},
        documents::{
            extraction::{
                ExtractedBlock, ExtractionControl, ExtractionInput, ExtractionLimits,
                normalize_text,
            },
            vision::{
                self, MAX_TRANSCRIPTION_BYTES, VisionExecution, VisionInvocation, VisionMaterial,
                VisionOutcome, VisionRaster, VisionTarget, VisionUnit,
            },
        },
    },
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

pub(super) const OUTCOME_UNKNOWN: &str = "VisionOutcomeUnknown";

pub(super) struct VisionIndex {
    pub checkpoint: DocumentCheckpoint,
    pub diagnostic: Option<Id>,
    pub held: bool,
    pub witness: Option<String>,
}

impl VisionIndex {
    pub(super) fn direct(checkpoint: &DocumentCheckpoint, diagnostic: Option<Id>) -> Self {
        Self {
            checkpoint: checkpoint.clone(),
            diagnostic,
            held: false,
            witness: None,
        }
    }
}

impl DocumentIndexer {
    pub(super) fn check_reextraction(
        &self,
        tx: &mut dyn WriteTransaction,
        version: &Id,
        algorithm: &Revision,
    ) -> Result<(), StorageError> {
        let Some(base) = tx
            .documents()
            .latest_document_direct_extraction(version, algorithm)?
        else {
            return Ok(());
        };
        for target in tx.documents().document_vision_targets(&base.id)? {
            let Some(unit) = tx
                .documents()
                .document_vision_unit(&base.id, &target.id())?
            else {
                continue;
            };
            if matches!(unit.outcome, VisionOutcome::Unknown) {
                return Err(StorageError::Busy);
            }
            if !matches!(unit.outcome, VisionOutcome::Ready) || unit.material.is_none() {
                continue;
            }
            let Some(route) = tx.router().route_request(&vision::request_id(&unit))? else {
                continue;
            };
            let operation = tx
                .operations()
                .operation(&route.operation_id)?
                .ok_or_else(invalid)?;
            if matches!(
                operation.state,
                OperationState::Dispatching | OperationState::OutcomeUnknown
            ) {
                return Err(StorageError::Busy);
            }
            for journal in &operation.attempts {
                if let Some(attempt) = tx.usage().inference_attempt(&journal.id.0)? {
                    let balance = tx
                        .usage()
                        .attempt_balance(&attempt.attempt_id)?
                        .ok_or_else(invalid)?;
                    if !matches!(
                        attempt.state,
                        AttemptState::Settled | AttemptState::CancelledBeforeDispatch
                    ) || !balance.local_quiescent
                        || balance.unknown_tail
                    {
                        return Err(StorageError::Busy);
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn apply_vision(
        &self,
        entry: &QueueEntry,
        job: &Job,
        checkpoint: &DocumentCheckpoint,
        cancellation: &Cancellation,
    ) -> Result<VisionIndex, StorageError> {
        let base = checkpoint.id.clone();
        let (context, policy, targets, states) = self.fenced(entry, job, move |tx, ctx, _| {
            let targets = tx.documents().document_vision_targets(&base)?;
            let policy = vision::policy(tx.configuration())?;
            let states = targets
                .iter()
                .map(|target| tx.documents().document_vision_unit(&base, &target.id()))
                .collect::<Result<Vec<_>, _>>()?;
            Ok((ctx, policy, targets, states))
        })?;
        if targets.is_empty() {
            return Ok(VisionIndex::direct(checkpoint, None));
        }
        if targets.len() > vision::MAX_TARGETS {
            return Err(invalid());
        }
        let unresolved = states
            .iter()
            .flatten()
            .any(|unit| matches!(unit.outcome, VisionOutcome::Unknown));
        let producer = self.vision.read_locked().clone();
        if !policy.enabled || producer.is_none() {
            if unresolved {
                return Ok(VisionIndex {
                    checkpoint: checkpoint.clone(),
                    diagnostic: Some(code(OUTCOME_UNKNOWN)),
                    held: true,
                    witness: None,
                });
            }
            return Ok(VisionIndex::direct(
                checkpoint,
                Some(code(if policy.enabled {
                    "VisionRouteUnavailable"
                } else {
                    "VisionDisabled"
                })),
            ));
        }
        let producer = producer.expect("available vision producer");
        let mut units = targets
            .iter()
            .zip(states)
            .filter_map(|(target, state)| state.map(|unit| (target.id().as_str().to_owned(), unit)))
            .collect::<BTreeMap<_, _>>();
        let pending = targets
            .iter()
            .filter(|target| !units.contains_key(target.id().as_str()))
            .cloned()
            .collect::<Vec<_>>();
        if !pending.is_empty() {
            let source = exact_bytes(
                &self.artifacts,
                self.store.as_ref(),
                &context,
                &job.source,
                SOURCE_LIMIT,
            )?;
            let mut capture = |target: &VisionTarget, raster: Result<VisionRaster, Id>| {
                if cancellation.is_cancelled() {
                    return Err(StorageError::Cancelled);
                }
                let id = target.id();
                if units.contains_key(id.as_str()) || !pending.iter().any(|item| item.id() == id) {
                    return Err(invalid());
                }
                let mut unit = VisionUnit {
                    extraction: checkpoint.id.clone(),
                    target: id.clone(),
                    origin: entry.id.clone(),
                    raster: None,
                    material: None,
                    outcome: VisionOutcome::Ready,
                };
                match raster {
                    Ok(raster) if raster.validate(target) && raster.png.is_empty() => {
                        unit.outcome = VisionOutcome::Blank
                    }
                    Ok(raster) if raster.validate(target) => {
                        unit = self.capture_vision(
                            entry,
                            job,
                            unit,
                            &raster.png,
                            ImageArtifact::Raster {
                                width: raster.width,
                                height: raster.height,
                            },
                            &policy.witness,
                        )?;
                    }
                    Ok(_) => {
                        unit.outcome = VisionOutcome::Failed {
                            diagnostic: code("VisionRasterInvalid"),
                        }
                    }
                    Err(diagnostic) => unit.outcome = VisionOutcome::Failed { diagnostic },
                }
                if unit.raster.is_none() {
                    self.store_vision_unit(entry, job, &unit, &policy.witness)?;
                }
                units.insert(id.as_str().to_owned(), unit);
                Ok(())
            };
            let control = ExtractionControl {
                cancelled: &|| cancellation.is_cancelled(),
                deadline: Instant::now() + Duration::from_secs(60),
            };
            self.extractor.render_vision(
                ExtractionInput {
                    version: &job.source,
                    revision: &checkpoint.revision,
                    bytes: &source,
                },
                &pending,
                &control,
                &mut capture,
            )?;
        }
        let mut fences = job.fences.clone();
        fences.extend([
            AuthorityFence::DocumentIndex {
                run: entry.id.clone(),
                generation: entry.generation,
            },
            AuthorityFence::DocumentVision {
                settings_witness: policy.witness.clone(),
            },
        ]);
        let mut held = false;
        let mut diagnostic = None;
        for target in &targets {
            if cancellation.is_cancelled() {
                return Err(StorageError::Cancelled);
            }
            let id = target.id();
            let unit = units.get_mut(id.as_str()).ok_or_else(invalid)?;
            if let VisionOutcome::Failed { diagnostic: reason } = &unit.outcome {
                diagnostic.get_or_insert_with(|| reason.clone());
            }
            if !matches!(unit.outcome, VisionOutcome::Ready | VisionOutcome::Unknown) {
                continue;
            }
            // Preserve a single unresolved paid occurrence before starting another.
            if held && matches!(unit.outcome, VisionOutcome::Ready) {
                continue;
            }
            let observe_only = matches!(unit.outcome, VisionOutcome::Unknown);
            if unit.material.is_none() {
                let raster = unit.raster.clone().ok_or_else(invalid)?;
                let bytes = vision::material(raster);
                *unit = self.capture_vision(
                    entry,
                    job,
                    unit.clone(),
                    &bytes,
                    ImageArtifact::Context,
                    &policy.witness,
                )?;
            }
            let work = VisionInvocation {
                document: job.source.clone(),
                request: vision::request_id(unit),
                material: unit.material.clone().ok_or_else(invalid)?,
                fences: fences.clone(),
            };
            let outcome = producer.execute(&context, &work, cancellation, observe_only)?;
            unit.outcome = match outcome {
                VisionExecution::Recorded(evidence) if evidence.request == work.request => {
                    VisionOutcome::Complete { evidence }
                }
                VisionExecution::Recorded(_) => return Err(invalid()),
                VisionExecution::Unavailable if !observe_only => {
                    diagnostic = Some(code("VisionRouteUnavailable"));
                    continue;
                }
                VisionExecution::Unavailable | VisionExecution::Unknown => {
                    held = true;
                    VisionOutcome::Unknown
                }
                VisionExecution::Failed { diagnostic: reason } => {
                    diagnostic.get_or_insert_with(|| reason.clone());
                    VisionOutcome::Failed { diagnostic: reason }
                }
            };
            self.store_vision_unit(entry, job, unit, &policy.witness)?;
        }
        let populated =
            self.compose_vision(entry, job, checkpoint, (&targets, &units), cancellation)?;
        let witness = (populated.id != checkpoint.id).then_some(policy.witness);
        Ok(VisionIndex {
            checkpoint: populated,
            diagnostic: if held {
                Some(code(OUTCOME_UNKNOWN))
            } else {
                diagnostic
            },
            held,
            witness,
        })
    }

    fn capture_vision(
        &self,
        entry: &QueueEntry,
        job: &Job,
        mut unit: VisionUnit,
        bytes: &[u8],
        kind: ImageArtifact,
        witness: &str,
    ) -> Result<VisionUnit, StorageError> {
        let request = vision::request_id(&unit);
        let staged = self
            .artifacts
            .stage_host_bytes(
                self.store.as_ref(),
                bytes,
                json!({"kind":"DocumentVisionCapture","requestId":request}),
            )?
            .map_err(|_| invalid())?;
        let (pending, service, source, witness) = (
            staged.clone(),
            self.artifacts.clone(),
            job.source.clone(),
            witness.to_owned(),
        );
        let committed = self.fenced(entry, job, move |tx, context, _| {
            settings_hold(tx.configuration(), &witness)?;
            let mut provenance = json!({
                "kind":"DocumentVisionInput","requestId":request,"documentSource":source,
                "targetId":unit.target,"extractionId":unit.extraction,
            });
            if let ImageArtifact::Raster { width, height } = kind {
                provenance["width"] = json!(width);
                provenance["height"] = json!(height);
            }
            let version = service.commit_host_bytes_in_scope(
                tx,
                &pending,
                HostArtifactPublication {
                    title: kind.title(),
                    media_type: kind.media_type(),
                    provenance,
                    scope: context
                        .project_id()
                        .cloned()
                        .map_or(ArtifactScope::Workspace, ArtifactScope::Project),
                    owner: context.principal().clone(),
                },
            )?;
            let artifact = version_ref(&version);
            match kind {
                ImageArtifact::Raster { .. } => unit.raster = Some(artifact),
                ImageArtifact::Context => {
                    unit.material = Some(VisionMaterial {
                        context: artifact,
                        operation: pending.operation_id.clone(),
                    })
                }
            }
            tx.documents().put_document_vision_unit(&unit)?;
            Ok(unit)
        });
        self.artifacts
            .settle_host_bytes(self.store.as_ref(), &staged, committed.is_ok());
        committed
    }

    fn store_vision_unit(
        &self,
        entry: &QueueEntry,
        job: &Job,
        unit: &VisionUnit,
        witness: &str,
    ) -> Result<(), StorageError> {
        let (unit, witness) = (unit.clone(), witness.to_owned());
        self.fenced(entry, job, move |tx, _, _| {
            // Retain an ambiguous settlement even if the setting changed after admission.
            // This writes no text or request material and remains exact-source fenced.
            if !matches!(unit.outcome, VisionOutcome::Unknown) {
                settings_hold(tx.configuration(), &witness)?;
            }
            tx.documents().put_document_vision_unit(&unit)
        })
    }

    fn compose_vision(
        &self,
        entry: &QueueEntry,
        job: &Job,
        checkpoint: &DocumentCheckpoint,
        population: (&[VisionTarget], &BTreeMap<String, VisionUnit>),
        cancellation: &Cancellation,
    ) -> Result<DocumentCheckpoint, StorageError> {
        let (targets, units) = population;
        let key = format!(
            "vision:{}",
            canonical_digest(&json!([checkpoint.id, units])).map_err(|_| invalid())?
        );
        let (owner, version) = (key.clone(), checkpoint.version.clone());
        let (context, existing, revision) = self.fenced(entry, job, move |tx, ctx, _| {
            let existing = tx
                .documents()
                .document_extraction_by_key(&version, &owner)?;
            let revision = tx.documents().next_document_extraction_revision(&version)?;
            Ok((ctx, existing, revision))
        })?;
        if let Some(existing) = existing {
            return Ok(existing);
        }
        let read_options = ReadOptions {
            cancellation: Some(cancellation.clone()),
            deadline_ms: None,
        };
        let direct = read_snapshot(self.store.as_ref(), &read_options, |read| {
            read.documents().document_blocks(&checkpoint.id)
        })?;
        if direct.len() != checkpoint.blocks as usize {
            return Err(invalid());
        }
        let limits = ExtractionLimits::default();
        let mut remaining = limits
            .normalized_bytes
            .checked_sub(direct.iter().map(|block| block.text.len()).sum::<usize>())
            .ok_or_else(invalid)?;
        let mut passages = direct
            .into_iter()
            .map(|block| {
                (
                    block.citation.locator,
                    block.headings,
                    block.text,
                    "document-direct",
                )
            })
            .collect::<Vec<_>>();
        let control = ExtractionControl {
            cancelled: &|| cancellation.is_cancelled(),
            deadline: Instant::now() + Duration::from_secs(30),
        };
        let mut gaps = checkpoint.coverage.gaps.clone();
        let mut complete = true;
        for target in targets {
            if cancellation.is_cancelled() {
                return Err(StorageError::Cancelled);
            }
            match &units.get(target.id().as_str()).ok_or_else(invalid)?.outcome {
                VisionOutcome::Blank => {}
                VisionOutcome::Complete { evidence } => {
                    let raw = exact_bytes(
                        &self.artifacts,
                        self.store.as_ref(),
                        &context,
                        &evidence.output,
                        MAX_TRANSCRIPTION_BYTES as u64,
                    )?;
                    let text = std::str::from_utf8(&raw).map_err(|_| invalid())?;
                    if text.trim().is_empty() {
                        continue;
                    }
                    let normalized = match normalize_text(text, remaining, &control) {
                        Ok(text) => text,
                        Err(_) if cancellation.is_cancelled() => {
                            return Err(StorageError::Cancelled);
                        }
                        Err(_) => {
                            gaps.push(code("VisionTextLimit"));
                            complete = false;
                            continue;
                        }
                    };
                    let mut tail = normalized.as_str();
                    while !tail.is_empty() {
                        if passages.len() >= limits.blocks {
                            gaps.push(code("VisionTextLimit"));
                            complete = false;
                            break;
                        }
                        let boundary = tail.floor_char_boundary(limits.chunk_bytes.min(tail.len()));
                        let (text, rest) = tail.split_at(boundary);
                        passages.push((
                            target.locator(),
                            Arc::from([]),
                            text.to_owned(),
                            "document-vision",
                        ));
                        remaining -= text.len();
                        tail = rest;
                    }
                }
                VisionOutcome::Failed { diagnostic } => {
                    gaps.push(diagnostic.clone());
                    complete = false;
                }
                VisionOutcome::Ready | VisionOutcome::Unknown => complete = false,
            }
        }
        if complete {
            gaps.retain(|gap| {
                !matches!(gap.as_str(), "PdfPageNeedsRaster" | "WordImageNeedsVision")
            });
        }
        if passages.is_empty() {
            gaps.push(code("VisionNoText"));
        }
        gaps.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        gaps.dedup();
        let mut coverage = checkpoint.coverage.clone();
        coverage.more_gaps |= gaps.len() > 128;
        gaps.truncate(128);
        coverage.state = if passages.is_empty() {
            CoverageState::Unavailable
        } else if gaps.is_empty() && !coverage.more_gaps {
            CoverageState::Complete
        } else {
            CoverageState::Partial
        };
        coverage.gaps = gaps;
        if !units.values().any(|unit| {
            matches!(
                unit.outcome,
                VisionOutcome::Complete { .. } | VisionOutcome::Blank
            )
        }) {
            return Ok(checkpoint.clone());
        }
        if matches!(targets.first(), Some(VisionTarget::Pdf { .. })) {
            passages.sort_by_key(|(locator, _, _, _)| match locator {
                Locator::Pdf { page } => *page,
                _ => 0,
            });
        }
        let input = ExtractionInput {
            version: &job.source,
            revision: &revision,
            bytes: &[],
        };
        let blocks = passages
            .into_iter()
            .enumerate()
            .map(|(ordinal, (locator, headings, text, engine))| {
                ExtractedBlock::new(&input, engine, locator, ordinal, headings, text)
            })
            .collect();
        self.save_checkpoint(
            entry,
            job,
            revision,
            key,
            DocumentExtraction {
                blocks,
                coverage,
                vision_targets: vec![],
            },
        )
    }
}

#[derive(Clone, Copy)]
enum ImageArtifact {
    Raster { width: u32, height: u32 },
    Context,
}
impl ImageArtifact {
    fn title(self) -> &'static str {
        match self {
            Self::Raster { .. } => "Document vision raster",
            Self::Context => "Document vision request",
        }
    }

    fn media_type(self) -> &'static str {
        match self {
            Self::Raster { .. } => "image/png",
            Self::Context => "application/vnd.devworks.provider-context+json",
        }
    }
}

fn settings_hold(
    read: &dyn crate::ports::storage::ConfigurationRead,
    witness: &str,
) -> Result<(), StorageError> {
    let actual = vision::policy(read)?;
    (actual.enabled && actual.witness == witness)
        .then_some(())
        .ok_or(StorageError::ScopeDenied)
}

fn exact_bytes(
    service: &crate::services::artifacts::ArtifactService,
    store: &dyn crate::ports::storage::ApplicationStore,
    context: &InvocationContext,
    reference: &ArtifactRef,
    limit: u64,
) -> Result<Vec<u8>, StorageError> {
    service
        .read_exact(store, context, reference, limit)
        .map(|(bytes, _)| bytes)
        .map_err(|error| match error {
            ExactReadRefusal::Storage(error) => error,
            _ => StorageError::ViewUnavailable,
        })
}
