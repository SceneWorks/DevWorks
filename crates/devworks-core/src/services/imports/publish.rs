use super::{HistoryKind, SourceRecord};
use crate::context::InvocationContext;
use crate::fingerprint::sha256_hex;
use crate::ports::IdKind;
use crate::ports::storage::{
    ACTIVE, FEATURE, NewEvent, ProjectRead, RepositoryFilter, StorageError, StoredProject,
    StoredWork, WorkImportMapping, WorkImportRepositoryBinding, WriteTransaction,
};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::{EventId, Id, ProjectId, RootId};
use crate::services::kernel::Failure;
use crate::services::work::{deps, workflow};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Publication {
    Added,
    Changed,
    Unchanged,
    MissingReference,
    DestinationChanged,
}

pub(super) fn destination(namespace: &Id, kind: &str, source: &Id) -> Id {
    let material = format!("{}\0{kind}\0{}", namespace.as_str(), source.as_str());
    Id::new(format!("import-{}", &sha256_hex(material.as_bytes())[..48]))
        .expect("bounded source-derived import ID")
}

pub(super) fn publish_record(
    tx: &mut dyn WriteTransaction,
    context: &InvocationContext,
    namespace: &Id,
    record: &SourceRecord,
    captured_through_ms: i64,
    repository_binding: Option<&WorkImportRepositoryBinding>,
    publish_attachment: impl FnOnce(
        &mut dyn WriteTransaction,
        ProjectId,
    ) -> Result<Option<ArtifactRef>, StorageError>,
) -> Result<Publication, StorageError> {
    let previous = tx
        .work_imports()?
        .import_mapping(namespace, record.kind(), record.id())?;
    let digest = record
        .digest()
        .map_err(|_| StorageError::InvalidRecord("import source record".into()))?;
    if let Some(mut mapping) = previous.clone()
        && mapping.source_digest == digest
    {
        if captured_through_ms > mapping.observed_through_ms {
            mapping.observed_through_ms = captured_through_ms;
            tx.work_imports()?.put_import_mapping(&mapping)?;
        }
        return Ok(Publication::Unchanged);
    }

    if previous
        .as_ref()
        .is_some_and(|m| m.observed_through_ms >= captured_through_ms)
    {
        return Ok(Publication::DestinationChanged);
    }
    let project = ProjectId(destination(namespace, "Project", record.project()));
    if !matches!(record, SourceRecord::Project { .. })
        && !tx
            .projects()
            .project(&project)?
            .is_some_and(|p| p.lifecycle == ACTIVE)
    {
        return Ok(Publication::DestinationChanged);
    }
    let local = destination(namespace, record.kind(), record.id());
    let expected = previous
        .as_ref()
        .and_then(|mapping| mapping.destination_revision);
    let revision = match record {
        SourceRecord::Project {
            name,
            description,
            stamp,
            ..
        } => {
            let current = tx.projects().project(&project)?;
            if let Some(mut current) = current {
                if Some(current.revision) != expected || current.lifecycle != ACTIVE {
                    return Ok(Publication::DestinationChanged);
                }
                let before = current.revision;
                current.revision = next(before)?;
                current.name.clone_from(name);
                current.description.clone_from(description);
                current.updated_at_ms = stamp.updated_at_ms;
                tx.projects().update_project(&current, before)?;
                Some(current.revision)
            } else if expected.is_some() {
                return Ok(Publication::DestinationChanged);
            } else {
                tx.projects().insert_project(&StoredProject {
                    id: project.clone(),
                    name: name.clone(),
                    description: description.clone(),
                    lifecycle: ACTIVE.into(),
                    revision: 1,
                    scope_revision: 1,
                    created_at_ms: stamp.created_at_ms,
                    updated_at_ms: stamp.updated_at_ms,
                    archived_at_ms: None,
                    deleted_at_ms: None,
                    archive_epoch: 0,
                    archive_stop_pending: false,
                })?;
                workflow::ensure_workflow(tx, &project)?;
                Some(1)
            }
        }
        SourceRecord::Feature {
            title,
            description,
            stamp,
            ..
        }
        | SourceRecord::Item {
            title,
            description,
            stamp,
            ..
        } => {
            let parent = match record {
                SourceRecord::Item {
                    feature_id: Some(parent),
                    ..
                } => {
                    let parent = destination(namespace, "Feature", parent);
                    if !tx
                        .work()
                        .work(&parent)?
                        .is_some_and(|f| f.is_feature() && f.project_id == project)
                    {
                        return Ok(Publication::MissingReference);
                    }
                    Some(parent)
                }
                _ => None,
            };
            let historical = historical(record)?;
            let current = tx.work().work(&local)?;
            if let Some(mut current) = current {
                if Some(current.revision) != expected
                    || current.project_id != project
                    || current.parent_feature_id != parent
                {
                    return Ok(Publication::DestinationChanged);
                }
                let before = current.revision;
                current.revision = next(before)?;
                current.title.clone_from(title);
                current.updated_at_ms = stamp.updated_at_ms;
                current.search_key = search(title, description);
                current.brief["description"] = json!(description);
                current.brief["importedHistory"] = historical;
                tx.work().update_work(&current, before)?;
                Some(current.revision)
            } else if expected.is_some() {
                return Ok(Publication::DestinationChanged);
            } else {
                if tx
                    .projects()
                    .project(&project)?
                    .is_none_or(|p| p.lifecycle != ACTIVE)
                {
                    return Ok(Publication::MissingReference);
                }
                let status = workflow::default_status(tx, &project)?;
                let (kind, priority, rank) = match record {
                    SourceRecord::Item {
                        priority, order, ..
                    } => ("task", priority.rank(), *order),
                    _ => (FEATURE, 0, 0),
                };
                let work = StoredWork {
                    id: local.clone(),
                    project_id: project.clone(),
                    kind: kind.into(),
                    parent_feature_id: parent,
                    status_id: Some(status.id),
                    title: title.clone(),
                    priority,
                    manual_order: rank.max(0),
                    revision: 1,
                    scope_revision: 1,
                    created_at_ms: stamp.created_at_ms,
                    updated_at_ms: stamp.updated_at_ms,
                    created_by: Some(context.principal().clone()),
                    updated_by: None,
                    search_key: search(title, description),
                    caller_ref: None,
                    completion: None,
                    brief: json!({"description":description,"importedHistory":historical}),
                };
                tx.work().insert_work(&work)?;
                tx.work().bump_queue(&project, &work.queue())?;
                Some(1)
            }
        }
        SourceRecord::Dependency {
            blocked, blocker, ..
        } => {
            let blocked = destination(namespace, "Item", blocked);
            let blocker = destination(namespace, "Item", blocker);
            let Some(blocked_work) = tx.work().work(&blocked)? else {
                return Ok(Publication::MissingReference);
            };
            if !tx
                .work()
                .work(&blocker)?
                .is_some_and(|work| work.project_id == project)
                || blocked_work.project_id != project
            {
                return Ok(Publication::MissingReference);
            }
            let mut links = tx.work().prerequisites(&blocked)?;
            if links.contains(&blocker) {
                None
            } else {
                let blocked_source = match record {
                    SourceRecord::Dependency { blocked, .. } => blocked,
                    _ => unreachable!("the active record is a dependency"),
                };
                let witness =
                    tx.work_imports()?
                        .import_mapping(namespace, "Item", blocked_source)?;
                if witness.as_ref().and_then(|m| m.destination_revision)
                    != Some(blocked_work.revision)
                {
                    return Ok(Publication::DestinationChanged);
                }
                links.push(blocker);
                if deps::closes_cycle(tx.work(), &blocked, &links)?.is_some() {
                    return Ok(Publication::MissingReference);
                }
                if let Some(updated) =
                    deps::set_links(tx, context, &project, &blocked, &links).map_err(failure)?
                    && let Some(mut witness) = witness
                {
                    witness.destination_revision = Some(updated.revision);
                    tx.work_imports()?.put_import_mapping(&witness)?;
                }
                None
            }
        }
        SourceRecord::Repository { remote, role, .. } => {
            let observed =
                repository_target(tx.projects(), record.id(), &project, remote.as_deref())?;
            if repository_binding != Some(&observed) {
                return Ok(Publication::DestinationChanged);
            }
            let Some((repository, _)) = observed.target else {
                return Ok(Publication::MissingReference);
            };
            let memberships = tx.projects().repository_projects(&repository)?;
            if role == "owned" && memberships.iter().any(|owner| owner != &project) {
                return Ok(Publication::DestinationChanged);
            }
            let mut members = tx.projects().members(&project)?;
            if !members.contains(&repository) {
                let mut imported = tx
                    .work_imports()?
                    .import_mapping(namespace, "Project", record.project())?
                    .ok_or_else(|| {
                        StorageError::InvalidRecord("missing imported project witness".into())
                    })?;
                let before = tx
                    .projects()
                    .project(&project)?
                    .ok_or(StorageError::ScopeDenied)?;
                if Some(before.revision) != imported.destination_revision {
                    return Ok(Publication::DestinationChanged);
                }
                members.push(repository.clone());
                let updated = crate::services::projects::publish_membership(
                    tx,
                    context,
                    &before,
                    &members,
                    &[repository],
                    &[],
                )
                .map_err(failure)?;
                imported.destination_revision = Some(updated.revision);
                tx.work_imports()?.put_import_mapping(&imported)?;
            }
            None
        }
        SourceRecord::History { .. } => None,
    };
    let event = match record {
        SourceRecord::History {
            subject_id,
            history_kind,
            stamp,
            evidence,
            ..
        } => {
            let root = history_root(tx, namespace, &project, subject_id)?;
            if subject_id != record.project() && root.is_none() {
                return Ok(Publication::MissingReference);
            }
            let evidence = evidence_with_attachment(
                evidence,
                matches!(
                    history_kind,
                    HistoryKind::Attachment | HistoryKind::TranscriptEvidence
                ),
                publish_attachment(tx, project.clone())?,
            );
            HistoricalEvent {
                project: project.clone(),
                root,
                aggregate: local.as_str().into(),
                kind: "ImportedHistoricalEvidence",
                payload: json!({"sourceNamespace":namespace,"sourceIdentity":record.id(),
                    "sourceSubject":subject_id,"kind":history_kind,"stamp":stamp,"evidence":evidence}),
            }
        }
        _ => HistoricalEvent {
            project: project.clone(),
            root: match record {
                SourceRecord::Feature { .. } | SourceRecord::Item { .. } => {
                    Some(RootId(local.clone()))
                }
                _ => None,
            },
            aggregate: local.as_str().into(),
            kind: "WorkSnapshotRecordImported",
            payload: json!({"sourceNamespace":namespace,"sourceIdentity":record.id(),
                "kind":record.kind(),"destination":local,"source":historical(record)?}),
        },
    };
    record_evidence(tx, context, event)?;
    tx.work_imports()?.put_import_mapping(&WorkImportMapping {
        source_namespace: namespace.clone(),
        source_kind: record.kind().into(),
        source_id: record.id().clone(),
        source_digest: digest,
        observed_through_ms: captured_through_ms,
        destination_id: local,
        destination_revision: revision,
        project_id: Some(project),
    })?;
    Ok(if previous.is_some() {
        Publication::Changed
    } else {
        Publication::Added
    })
}

fn next(value: i64) -> Result<i64, StorageError> {
    value
        .checked_add(1)
        .ok_or_else(|| StorageError::InvalidRecord("import revision".into()))
}

fn search(title: &str, description: &str) -> String {
    format!("{title}\n{description}").to_lowercase()
}

fn historical(record: &SourceRecord) -> Result<Value, StorageError> {
    let mut value = serde_json::to_value(record)
        .map_err(|_| StorageError::InvalidRecord("import evidence".into()))?;
    if let Some(fields) = value.as_object_mut() {
        fields.remove("description");
    }
    Ok(value)
}

fn history_root(
    tx: &mut dyn WriteTransaction,
    namespace: &Id,
    project: &ProjectId,
    source: &Id,
) -> Result<Option<RootId>, StorageError> {
    for kind in ["Item", "Feature"] {
        let id = destination(namespace, kind, source);
        if tx
            .work()
            .work(&id)?
            .is_some_and(|work| &work.project_id == project)
        {
            return Ok(Some(RootId(id)));
        }
    }
    Ok(None)
}

fn evidence_with_attachment(
    data: &Value,
    attachment: bool,
    artifact: Option<ArtifactRef>,
) -> Value {
    if let Some(full) = artifact
        .as_ref()
        .filter(|a| a.media_type == "application/json")
    {
        return json!({"fullEvidence":full,"title":data.get("title"),"sourceLink":data.get("url")});
    }
    let mut evidence = data.clone();
    if attachment && let Some(object) = evidence.as_object_mut() {
        object.remove("bytesBase64");
        object.insert("artifact".into(), json!(artifact));
    }
    evidence
}

pub(super) fn repository_target(
    read: &dyn ProjectRead,
    source_id: &Id,
    project: &ProjectId,
    remote: Option<&str>,
) -> Result<WorkImportRepositoryBinding, StorageError> {
    let target = match match_repository(read, remote, None)? {
        Some(id) => read.repository(&id)?.map(|row| (id, row.revision)),
        None => None,
    };
    let already_member = match &target {
        Some((id, _)) => read.members(project)?.contains(id),
        None => false,
    };
    Ok(WorkImportRepositoryBinding {
        source_id: source_id.clone(),
        target,
        already_member,
    })
}

pub(super) fn match_repository(
    projects: &dyn ProjectRead,
    remote: Option<&str>,
    project: Option<&ProjectId>,
) -> Result<Option<Id>, StorageError> {
    let Some((host, path)) = remote.and_then(remote_identity) else {
        return Ok(None);
    };
    let filter = RepositoryFilter {
        text: path.rsplit('/').next().map(str::to_owned),
        project: project.cloned(),
        ..Default::default()
    };
    let mut result = None;
    let mut after = None;
    loop {
        let repositories = projects.repositories(&filter, after.as_ref(), 128)?;
        let final_page = repositories.len() < 128;
        after = repositories.last().map(|row| row.id.clone());
        for repository in repositories {
            if repository.lifecycle != ACTIVE
                || !repository
                    .remote_url
                    .as_deref()
                    .and_then(remote_identity)
                    .is_some_and(|(h, p)| host == h && path == p)
            {
                continue;
            }
            if result.replace(repository.id).is_some() {
                return Ok(None);
            }
        }
        if final_page {
            return Ok(result);
        }
    }
}

fn remote_identity(remote: &str) -> Option<(String, String)> {
    let (host, path) = if let Ok(url) = url::Url::parse(remote)
        && matches!(url.scheme(), "https" | "http" | "ssh")
    {
        let mut host = url.host_str()?.to_ascii_lowercase();
        if let Some(port) = url.port()
            && (url.scheme() != "ssh" || port != 22)
        {
            host.push_str(&format!(":{port}"));
        }
        (host, url.path().to_owned())
    } else {
        let (user_host, path) = remote.split_once(':')?;
        let (_, host) = user_host.split_once('@')?;
        (host.to_ascii_lowercase(), path.to_owned())
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    if path.is_empty() || !path.contains('/') || host.is_empty() {
        return None;
    }
    Some((
        host.clone(),
        if host == "github.com" {
            path.to_ascii_lowercase()
        } else {
            path.to_owned()
        },
    ))
}

struct HistoricalEvent {
    project: ProjectId,
    root: Option<RootId>,
    aggregate: String,
    kind: &'static str,
    payload: Value,
}

fn record_evidence(
    tx: &mut dyn WriteTransaction,
    context: &InvocationContext,
    mut event: HistoricalEvent,
) -> Result<(), StorageError> {
    if let Some(root) = &event.root {
        event.payload["subject"] = json!({"kind":"Root","rootId":root});
    }
    let id = EventId(tx.new_id(IdKind::Event));
    let (aggregate_kind, aggregate_id) = if event.root.is_none() {
        ("imported_project_work", event.project.0.as_str().to_owned())
    } else {
        ("imported_work", event.aggregate)
    };
    tx.history().append(NewEvent {
        id,
        actor: context.principal().clone(),
        project_id: Some(event.project),
        root_id: event.root,
        operation_id: None,
        aggregate_kind: aggregate_kind.into(),
        aggregate_id,
        aggregate_revision: 1,
        kind: event.kind.into(),
        payload: event.payload,
    })?;
    Ok(())
}

fn failure(error: Failure) -> StorageError {
    match error {
        Failure::Storage(error) => error,
        Failure::Refused(_) => StorageError::ScopeDenied,
    }
}
