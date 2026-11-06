//! DW-109: authored metadata, independent of membership, across the real gateway and store.
use super::*;

fn refused_metadata(reply: DomainCommandReply) -> Option<DomainFault> {
    match reply {
        CommandReply::Rejected { .. } => None,
        other => Some(super::refused(other)),
    }
}

fn get3(fx: &Fx, work: &Id) -> WorkRecord {
    fx.get_at("tasks.get", work, Some(3)).unwrap()
}
fn edit3(
    fx: &Fx,
    who: &InvocationContext,
    work: &WorkRecord,
    changes: Value,
) -> DomainCommandReply {
    fx.command_at(who, "tasks.edit", json!({"workId": work.id.as_str(), "expectedRevision": work.revision.as_str(), "changes": changes}), Some(3))
}
fn metadata(record: &WorkRecord) -> Value {
    json!({"type": record.task_type, "labels": record.labels})
}
fn created(fx: &Fx, project: &ProjectId, op: &str, task_type: &str, labels: Value) -> WorkRecord {
    work_of(applied(fx.command_at(
        &fx.agent(),
        op,
        json!({
            "projectId": project.0.as_str(), "title": "Metadata card", "description": "",
            "repositoryIds": [], "statusId": Fx::status(project, "backlog"), "priority": 0,
            "taskType": task_type, "labels": labels
        }),
        Some(3),
    )))
}

#[test]
fn authorable_metadata_is_revision_checked_and_survives_membership_and_reopen() {
    let fx = Fx::new();
    let p = fx.project("Metadata", &[]);
    let f = fx.feature(&p, "Feature", &[]);
    for (op, kind) in [("tasks.create", "Story"), ("issues.create", "Bug")] {
        let card = created(&fx, &p, op, kind, json!(["UI", "urgent"]));
        assert_eq!(
            metadata(&card),
            json!({"type": kind, "labels": ["UI", "urgent"]})
        );
        assert!(matches!(
            card.kind,
            devworks_core::protocol::records::WorkKind::Issue
        ));
        let initial = fx.read(|s| {
            Ok(s.history()
                .page(&EventFilter::Root(RootId(card.id.clone())), 0, 500)?
                .events
                .into_iter()
                .map(|e| (e.kind, e.aggregate_revision, e.payload))
                .collect::<Vec<_>>())
        });
        let queue = fx.queue(&p, PROJECT_QUEUE);
        let edited = work_of(applied(edit3(
            &fx,
            &fx.user(),
            &card,
            json!({"taskType": "Task", "labels": ["Backend"]}),
        )));
        assert_eq!(edited.revision.get(), card.revision.get() + 1);
        assert_eq!(
            metadata(&edited),
            json!({"type": "Task", "labels": ["Backend"]})
        );
        assert!(fx.queue(&p, PROJECT_QUEUE) > queue);
        let renamed = work_of(applied(edit3(
            &fx,
            &fx.agent(),
            &edited,
            json!({"title": "Renamed"}),
        )));
        assert_eq!(metadata(&renamed), metadata(&edited));
        let linked = work_of(applied(fx.command_at(&fx.user(), "tasks.set_feature", json!({"taskId": renamed.id.as_str(), "expectedRevision": renamed.revision.as_str(), "featureId": f.id.as_str(), "clearFeature": false}), Some(3))));
        assert!(matches!(
            linked.kind,
            devworks_core::protocol::records::WorkKind::Task
        ));
        assert_eq!(metadata(&get3(&fx, &card.id)), metadata(&edited));
        let unlinked = work_of(applied(fx.command_at(&fx.agent(), "tasks.set_feature", json!({"taskId": linked.id.as_str(), "expectedRevision": linked.revision.as_str(), "clearFeature": true}), Some(3))));
        assert!(unlinked.parent_id.is_none());
        assert_eq!(metadata(&get3(&fx, &card.id)), metadata(&edited));
        let history = fx.read(|s| {
            Ok(s.history()
                .page(&EventFilter::Root(RootId(card.id.clone())), 0, 500)?
                .events
                .into_iter()
                .map(|e| (e.kind, e.aggregate_revision, e.payload))
                .collect::<Vec<_>>())
        });
        assert_eq!(&history[..initial.len()], initial.as_slice());
    }
    let durable = created(&fx, &p, "tasks.create", "Bug", json!(["Durable"]));
    let work_id = durable.id.clone();
    let Fx {
        h,
        store,
        runs,
        cases,
        staged,
        ..
    } = fx;
    drop((store, runs, cases, staged));
    let reopened = h.store();
    let stored = read_snapshot(&reopened, &ReadOptions::default(), |s| {
        s.work().work(&work_id)
    })
    .unwrap()
    .unwrap();
    let brief = devworks_core::services::work::model::WorkBrief::of(&stored).unwrap();
    assert_eq!(serde_json::to_value(brief.task_type).unwrap(), json!("Bug"));
    assert_eq!(
        serde_json::to_value(brief.labels).unwrap(),
        json!(["Durable"])
    );
}

#[test]
fn invalid_or_stale_metadata_edits_are_atomic_and_explicit_clears_are_idempotent() {
    let fx = Fx::new();
    let p = fx.project("Metadata", &[]);
    let card = created(&fx, &p, "tasks.create", "Story", json!(["UI"]));
    let edited = work_of(applied(edit3(
        &fx,
        &fx.agent(),
        &card,
        json!({"taskType": "Bug"}),
    )));
    let stale = refused(edit3(&fx, &fx.user(), &card, json!({"labels": ["Stale"]})));
    assert_eq!(stale.code, DomainErrorCode::RevisionConflict);
    for patch in [
        json!({"taskType": "Task", "clearTaskType": true}),
        json!({"title": "Must roll back", "labels": ["UI", "ui"]}),
        json!({"title": "Must roll back", "labels": [" "]}),
    ] {
        let queue = fx.queue(&p, PROJECT_QUEUE);
        let _ = refused_metadata(edit3(&fx, &fx.user(), &edited, patch));
        let current = get3(&fx, &card.id);
        assert_eq!(current.revision.get(), edited.revision.get());
        assert_eq!(current.title.0, edited.title.0);
        assert_eq!(metadata(&current), metadata(&edited));
        assert_eq!(fx.queue(&p, PROJECT_QUEUE), queue);
    }
    let cleared = work_of(applied(edit3(
        &fx,
        &fx.user(),
        &edited,
        json!({"clearTaskType": true, "labels": []}),
    )));
    assert_eq!(metadata(&cleared), json!({"type": null, "labels": []}));
    let same = work_of(unchanged(edit3(
        &fx,
        &fx.user(),
        &cleared,
        json!({"clearTaskType": true, "labels": []}),
    )));
    assert_eq!(same.revision.get(), cleared.revision.get());
    let _ = refused_metadata(fx.command_at(&fx.user(), "issues.edit", json!({"workId": cleared.id.as_str(), "expectedRevision": cleared.revision.as_str(), "changes": {"labels": ["dup", "DUP"]}}), Some(3)));
}

#[test]
fn legacy_metadata_stays_absent_and_older_wire_edits_preserve_authored_values() {
    let fx = Fx::new();
    let p = fx.project("Legacy", &[]);
    let legacy = fx.task(&fx.user(), &p, None, "Legacy", 0);
    let legacy_id = legacy.id.clone();
    fx.write(move |tx| {
        let mut stored = tx.work().work(&legacy_id)?.unwrap();
        stored.brief.as_object_mut().unwrap().remove("taskType");
        stored.brief.as_object_mut().unwrap().remove("labels");
        let expected = stored.revision;
        stored.revision += 1;
        tx.work().update_work(&stored, expected)
    });
    assert_eq!(
        metadata(&get3(&fx, &legacy.id)),
        json!({"type": null, "labels": []})
    );
    let typed = work_of(applied(edit3(
        &fx,
        &fx.agent(),
        &get3(&fx, &legacy.id),
        json!({"taskType": "Task", "labels": ["Authored"]}),
    )));
    let older = fx.get_at("tasks.get", &typed.id, Some(2)).unwrap();
    assert!(older.task_type.is_none() && older.labels.is_none());
    let _ = applied(fx.command_at(&fx.user(), "tasks.edit", json!({"workId": typed.id.as_str(), "expectedRevision": typed.revision.as_str(), "changes": {"title": "Older client edit"}}), Some(2)));
    assert_eq!(metadata(&get3(&fx, &typed.id)), metadata(&typed));
}

#[test]
fn feature_labels_and_plan_entry_metadata_share_authoring_and_projection() {
    let fx = Fx::new();
    let p = fx.project("Planning", &[]);
    let feature = match applied(fx.command_at(&fx.agent(), "features.create", json!({"projectId": p.0.as_str(), "creation": {"kind": "Single", "title": "Feature", "description": "", "repositoryIds": [], "statusId": Fx::status(&p, "backlog"), "priority": 0, "labels": ["Release"]}}), Some(3))) {
        OperationResult::FeaturesCreate { value } => value.items.0[0].clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        metadata(&feature),
        json!({"type": null, "labels": ["Release"]})
    );
    let feature = match applied(fx.command_at(&fx.user(), "features.edit", json!({"workId": feature.id.as_str(), "expectedRevision": feature.revision.as_str(), "changes": {"labels": ["Edited"]}}), Some(3))) {
        OperationResult::FeaturesEdit { value } => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        metadata(&feature),
        json!({"type": null, "labels": ["Edited"]})
    );
    let doc = fx.artifact(&p, 1);
    let feature = fx.plan(&feature, &doc, &[("E1", "Work")]);
    let feature = fx.approve(&feature);
    let sync = |entry: Value| {
        let plan_revision = fx
            .read(|s| s.work().feature_plan(&feature.id))
            .unwrap()
            .revision;
        fx.command_at(&fx.agent(), "tasks.sync_plan", json!({"featureId": feature.id.as_str(), "expectedPlanRevision": plan_revision.to_string(), "expectedTaskSetRevision": fx.queue(&p, &feature_queue(&feature.id)).to_string(), "tasks": [entry]}), Some(3))
    };
    let mut entry = json!({"clientKey": "planned", "title": "Planned", "description": "", "repositoryIds": [], "dependencyKeys": [], "acceptance": [], "taskType": "Story", "labels": ["Plan"]});
    let first = match applied(sync(entry.clone())) {
        OperationResult::TasksSyncPlan { value } => value.items.0[0].clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        metadata(&first),
        json!({"type": "Story", "labels": ["Plan"]})
    );
    entry["expectedRevision"] = json!(first.revision.as_str());
    entry["taskType"] = json!("Bug");
    entry["labels"] = json!(["Replanned"]);
    let updated = match applied(sync(entry.clone())) {
        OperationResult::TasksSyncPlan { value } => value.items.0[0].clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        metadata(&updated),
        json!({"type": "Bug", "labels": ["Replanned"]})
    );
    entry["expectedRevision"] = json!(updated.revision.as_str());
    entry.as_object_mut().unwrap().remove("taskType");
    entry.as_object_mut().unwrap().remove("labels");
    let _ = applied(sync(entry));
    assert_eq!(metadata(&get3(&fx, &first.id)), metadata(&updated));
}

#[test]
fn staged_inline_and_initial_plans_obey_the_same_metadata_revision() {
    let fx = Fx::new();
    let p = fx.project("Plan metadata", &[]);
    let doc = fx.artifact(&p, 1);
    let make = |suffix: &str| {
        json!({"callerRef": format!("plan-{suffix}"), "features": [{
        "callerRef": format!("feature-{suffix}"), "title": "Feature", "description": "", "repositoryIds": [],
        "labels": ["Feature label"], "plan": draft(&doc, &[("E1", "Scope")], &[]),
        "tasks": [{"clientKey": format!("task-{suffix}"), "title": "Typed plan task", "description": "", "repositoryIds": [], "dependencyKeys": [], "acceptance": [], "taskType": "Bug", "labels": ["Task label"]}]
    }], "dependencyEdges": []})
    };
    let plan = make("staged");
    let bytes = serde_json::to_vec(&plan).unwrap();
    let artifact = fx.artifact_bytes(&p, &bytes, 71);
    fx.staged
        .0
        .lock()
        .unwrap()
        .insert(devworks_core::fingerprint::sha256_hex(&bytes), bytes);
    let stage = json!({"projectId": p.0.as_str(), "creation": {"kind": "StagedPlan", "artifact": artifact}});
    let _ = refused_metadata(fx.command_at(&fx.agent(), "features.create", stage.clone(), Some(2)));
    assert!(
        fx.list("features.list", &p, 50, None, None)
            .unwrap()
            .0
            .is_empty()
    );
    let _ = applied(fx.command_at(&fx.agent(), "features.create", stage, Some(3)));
    let _ = applied(fx.command_at(&fx.agent(), "features.create", json!({"projectId": p.0.as_str(), "creation": {"kind": "AtomicPlan", "plan": make("inline")}}), Some(3)));
    let (tasks, _) = fx.list("tasks.list", &p, 50, None, None).unwrap();
    assert_eq!(tasks.len(), 2);
    for task in tasks {
        assert_eq!(
            metadata(&get3(&fx, &task.id)),
            json!({"type": "Bug", "labels": ["Task label"]})
        );
    }
    let project = match applied(fx.command_at(&fx.user(), "projects.create", json!({"name": "Initial metadata", "description": "", "repositoryIds": [], "initialPlan": make("initial")}), Some(3))) {
        OperationResult::ProjectsCreate { value } => value,
        other => panic!("{other:?}"),
    };
    let (tasks, _) = fx.list("tasks.list", &project.id, 50, None, None).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(
        metadata(&get3(&fx, &tasks[0].id)),
        json!({"type": "Bug", "labels": ["Task label"]})
    );
}
