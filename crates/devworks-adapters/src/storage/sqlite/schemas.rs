//! Dedicated metadata repositories. Source rows and native driver types never enter this store.
use super::{
    conn::db,
    repos::{Snapshot, Unit},
};
use devworks_core::ports::storage::*;
use devworks_core::protocol::{
    Check, Id, OperationId,
    records::{SchemaObject, SchemaRelation},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;
fn invalid(message: &str) -> StorageError {
    StorageError::InvalidRecord(message.into())
}
fn encode<T: Serialize>(v: &T) -> Result<String, StorageError> {
    serde_json::to_string(v).map_err(|_| invalid("invalid metadata record"))
}
fn decode<T: DeserializeOwned>(v: String) -> Result<T, StorageError> {
    serde_json::from_str(&v).map_err(|_| invalid("invalid stored metadata record"))
}
fn limit(v: u16) -> Result<i64, StorageError> {
    if v == 0 || v > MAX_PAGE {
        Err(invalid("metadata page needs 1..=500"))
    } else {
        Ok(i64::from(v))
    }
}
fn source(conn: &Connection, id: &Id) -> Result<Option<StoredSchemaSource>, StorageError> {
    db(conn
        .query_row(
            "SELECT body_json FROM schema_sources WHERE id=?1",
            [id.as_str()],
            |r| r.get::<_, String>(0),
        )
        .optional())?
    .map(decode)
    .transpose()
}
fn sources(
    conn: &Connection,
    v: &SchemaVisibility,
    after: Option<&Id>,
    n: u16,
) -> Result<Vec<StoredSchemaSource>, StorageError> {
    let sql = "SELECT s.body_json FROM schema_sources s WHERE s.deleted=0 AND ((?2=1 AND (s.owner_id=?1 OR json_extract(s.body_json,'$.connection.config.workspaceExposed')=1)) OR (?3 IS NOT NULL AND EXISTS(SELECT 1 FROM json_each(s.body_json,'$.connection.config.projectAccess') p WHERE json_extract(p.value,'$.projectId')=?3 AND json_extract(p.value,'$.metadataEnabled')=1))) AND (?4=0 OR (coalesce(json_extract(s.body_json,'$.connection.config.enabled'),1)=1 AND coalesce(json_extract(s.body_json,'$.connection.config.exposureMode'),'Hidden')!='Hidden')) AND (?5 IS NULL OR s.id>?5) ORDER BY s.id LIMIT ?6";
    let mut q = db(conn.prepare_cached(sql))?;
    let rows = db(db(q.query_map(
        params![
            v.actor_id.0.as_str(),
            v.workspace,
            v.project_id.as_ref().map(Id::as_str),
            v.exposed_only,
            after.map(Id::as_str),
            limit(n)?
        ],
        |r| r.get::<_, String>(0),
    ))?
    .collect::<rusqlite::Result<Vec<_>>>())?;
    rows.into_iter().map(decode).collect()
}
fn view(conn: &Connection, id: &Id) -> Result<Option<StoredSchemaView>, StorageError> {
    db(conn.query_row("SELECT json_object('id',id,'connection_id',connection_id,'source_revision',source_revision,'configuration_revision',configuration_revision,'coverage',json(coverage_json),'operation_id',operation_id,'digest',digest,'sealed',json('true'),'created_at_ms',created_at) FROM schema_views WHERE id=?1 AND sealed=1",[id.as_str()],|r|r.get::<_,String>(0)).optional())?.map(decode).transpose()
}
fn pin(conn: &Connection, id: &Id) -> Result<Option<StoredSchemaReadPin>, StorageError> {
    db(conn
        .query_row(
            "SELECT body_json FROM schema_read_pins WHERE id=?1",
            [id.as_str()],
            |r| r.get::<_, String>(0),
        )
        .optional())?
    .map(decode)
    .transpose()
}
fn objects(
    conn: &Connection,
    view: &Id,
    f: &SchemaObjectFilter,
    after: Option<&Id>,
    n: u16,
) -> Result<Vec<SchemaObject>, StorageError> {
    let kinds = encode(&f.kinds)?;
    let mut q=db(conn.prepare_cached("SELECT o.body_json FROM schema_objects o JOIN schema_views v ON v.id=o.view_id WHERE v.sealed=1 AND o.view_id=?1 AND (json_array_length(?2)=0 OR o.kind IN(SELECT value FROM json_each(?2))) AND (?3 IS NULL OR o.parent_id=?3) AND (?4 IS NULL OR devworks_graph_name_matches(json_extract(o.body_json,'$.metadata.name'),o.qualified_name,?4,1)) AND (?5 IS NULL OR o.id>?5) AND (?7 IS NULL OR o.id=?7) ORDER BY o.id LIMIT ?6"))?;
    let rows = db(db(q.query_map(
        params![
            view.as_str(),
            kinds,
            f.parent_id.as_ref().map(Id::as_str),
            f.text,
            after.map(Id::as_str),
            limit(n)?,
            f.object_id.as_ref().map(Id::as_str)
        ],
        |r| r.get::<_, String>(0),
    ))?
    .collect::<rusqlite::Result<Vec<_>>>())?;
    rows.into_iter().map(decode).collect()
}
fn relations(
    conn: &Connection,
    view: &Id,
    objects: &[Id],
    after: Option<&Id>,
    n: u16,
) -> Result<Vec<SchemaRelation>, StorageError> {
    if objects.len() > usize::from(MAX_PAGE) {
        return Err(invalid("too many schema relationship endpoints"));
    }
    let ids = encode(&json!(objects.iter().map(Id::as_str).collect::<Vec<_>>()))?;
    let mut q=db(conn.prepare_cached("SELECT r.body_json FROM schema_relations r JOIN schema_views v ON v.id=r.view_id WHERE v.sealed=1 AND r.view_id=?1 AND (json_array_length(?2)=0 OR r.from_id IN(SELECT value FROM json_each(?2)) OR r.to_id IN(SELECT value FROM json_each(?2))) AND (?3 IS NULL OR r.id>?3) ORDER BY r.id LIMIT ?4"))?;
    let rows = db(db(q.query_map(
        params![view.as_str(), ids, after.map(Id::as_str), limit(n)?],
        |r| r.get::<_, String>(0),
    ))?
    .collect::<rusqlite::Result<Vec<_>>>())?;
    rows.into_iter().map(decode).collect()
}
macro_rules! reads {
    ($t:ty) => {
        impl SchemaRead for $t {
            fn schema_pin(&self,id:&Id)->Result<Option<StoredSchemaReadPin>,StorageError>{pin(self.conn,id)}
            fn schema_active_pin(&self,intent:&str,now:i64)->Result<Option<StoredSchemaReadPin>,StorageError>{
                db(self.conn.query_row("SELECT body_json FROM schema_read_pins WHERE intent_digest=?1 AND expires_at>?2 ORDER BY expires_at DESC,id LIMIT 1",params![intent,now],|r|r.get::<_,String>(0)).optional())?.map(decode).transpose()
            }

            fn schema_credential_in_use(&self, handle: &Id) -> Result<bool, StorageError> {
                db(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM schema_sources WHERE deleted=0 AND json_extract(body_json,'$.connection.credentialRef')=?1)", [handle.as_str()], |r| r.get(0)))
            }
            fn schema_inventory_revision(&self) -> Result<i64, StorageError> {
                db(self.conn.query_row(
                    "SELECT revision FROM schema_inventory WHERE singleton=1",
                    [],
                    |r| r.get(0),
                ))
            }
            fn schema_source(&self, id: &Id) -> Result<Option<StoredSchemaSource>, StorageError> {
                source(self.conn, id)
            }
            fn schema_sources(
                &self,
                v: &SchemaVisibility,
                after: Option<&Id>,
                n: u16,
            ) -> Result<Vec<StoredSchemaSource>, StorageError> {
                sources(self.conn, v, after, n)
            }
            fn schema_view(&self, id: &Id) -> Result<Option<StoredSchemaView>, StorageError> {
                view(self.conn, id)
            }
            fn schema_objects(
                &self,
                v: &Id,
                f: &SchemaObjectFilter,
                after: Option<&Id>,
                n: u16,
            ) -> Result<Vec<SchemaObject>, StorageError> {
                objects(self.conn, v, f, after, n)
            }
            fn schema_relations(
                &self,
                v: &Id,
                ids: &[Id],
                after: Option<&Id>,
                n: u16,
            ) -> Result<Vec<SchemaRelation>, StorageError> {
                relations(self.conn, v, ids, after, n)
            }
            fn schema_publication(
                &self,
                op: &OperationId,
            ) -> Result<Option<StoredSchemaPublication>, StorageError> {
                db(self
                    .conn
                    .query_row(
                        "SELECT body_json FROM schema_publications WHERE operation_id=?1",
                        [op.0.as_str()],
                        |r| r.get::<_, String>(0),
                    )
                    .optional())?
                .map(decode)
                .transpose()
            }
        }
    };
}
reads!(Snapshot<'_>);
reads!(Unit<'_>);
impl SchemaRepository for Unit<'_> {
    fn create_schema_pin(&mut self, row: &StoredSchemaReadPin) -> Result<(), StorageError> {
        if row.store_id != self.info.store_id
            || row.restore_epoch != self.info.restore_epoch
            || row.created_at_ms > self.now
            || row.expires_at_ms <= self.now
            || row.expires_at_ms.saturating_sub(self.now) > GRAPH_PIN_TTL_MS
            || row.owner_namespace.is_empty()
            || row.scope_digest.len() != 64
            || row.intent_digest.len() != 64
        {
            return Err(StorageError::ScopeDenied);
        }
        let retained = view(self.conn, &row.view_id)?.ok_or(StorageError::ViewUnavailable)?;
        let source = source(self.conn, &row.connection_id)?.ok_or(StorageError::ViewUnavailable)?;
        if source.deleted
            || source.configuration_revision != row.configuration_revision
            || retained.configuration_revision != row.configuration_revision
            || retained.connection_id != row.connection_id
            || retained.digest != row.view_digest
        {
            return Err(StorageError::ViewUnavailable);
        }
        db(self.conn.execute("INSERT INTO schema_read_pins(id,view_id,intent_digest,expires_at,body_json) VALUES(?1,?2,?3,?4,?5)",params![row.id.as_str(),row.view_id.as_str(),row.intent_digest,row.expires_at_ms,encode(row)?]))?;
        Ok(())
    }
    fn expire_schema_pins(&mut self, now: i64, n: u16) -> Result<usize, StorageError> {
        db(self.conn.execute("DELETE FROM schema_read_pins WHERE id IN(SELECT id FROM schema_read_pins WHERE expires_at<=?1 ORDER BY expires_at,id LIMIT ?2)",params![now,limit(n)?]))
    }

    fn create_schema_source(&mut self, row: &StoredSchemaSource) -> Result<(), StorageError> {
        row.connection
            .check()
            .map_err(|_| invalid("invalid schema source"))?;
        if row.configuration_revision != 1
            || row.connection.config.is_none()
            || row.connection.owner_id.as_ref() != Some(&row.owner_id)
        {
            return Err(invalid("invalid schema source configuration owner"));
        }
        if row.connection.current_view_id.is_some() {
            return Err(invalid("new source cannot publish a schema view"));
        }
        if row.connection.revision.get() != 1 {
            return Err(invalid("new schema source revision must be one"));
        }
        db(self.conn.execute("INSERT INTO schema_sources(id,revision,owner_id,deleted,body_json,configuration_revision) VALUES(?1,1,?2,?3,?4,?5)",params![row.connection.id.as_str(),row.owner_id.0.as_str(),row.deleted,encode(row)?,row.configuration_revision]))?;
        Ok(())
    }
    fn revise_schema_source(
        &mut self,
        row: &StoredSchemaSource,
        expected: i64,
    ) -> Result<(), StorageError> {
        row.connection
            .check()
            .map_err(|_| invalid("invalid schema source"))?;
        let old = source(self.conn, &row.connection.id)?
            .ok_or_else(|| invalid("schema source missing"))?;
        if old.connection.revision.get() != expected {
            return Err(StorageError::conflict(
                devworks_core::protocol::RevisionDimension::Connection,
                "schema_source",
                row.connection.id.clone(),
                devworks_core::protocol::Revision::new(expected)
                    .map_err(|_| invalid("invalid expected schema revision"))?,
                old.connection.revision,
            ));
        }
        if row.owner_id != old.owner_id
            || row.created_at_ms != old.created_at_ms
            || row.updated_at_ms < old.updated_at_ms
        {
            return Err(invalid("schema source ownership/creation identity changed"));
        }
        if row.connection.revision.get()
            != expected
                .checked_add(1)
                .ok_or_else(|| invalid("schema source revision overflow"))?
        {
            return Err(invalid("schema source revision must advance once"));
        }
        let changed_config = serde_json::to_value(&row.connection.config)
            .map_err(|_| invalid("schema config"))?
            != serde_json::to_value(&old.connection.config)
                .map_err(|_| invalid("schema config"))?
            || row.deleted != old.deleted;
        let expected_config = old
            .configuration_revision
            .checked_add(i64::from(changed_config))
            .ok_or_else(|| invalid("schema config revision overflow"))?;
        if row.configuration_revision != expected_config
            || row.connection.config.is_none()
            || row.connection.owner_id.as_ref() != Some(&row.owner_id)
        {
            return Err(invalid(
                "schema configuration revision or owner changed incorrectly",
            ));
        }
        if let Some(id) = &row.connection.current_view_id {
            let view = view(self.conn, id)?.ok_or(StorageError::ViewUnavailable)?;
            if row.deleted
                || view.connection_id != row.connection.id
                || view.configuration_revision != row.configuration_revision
                || view.source_revision > row.connection.revision.get()
            {
                return Err(StorageError::ScopeDenied);
            }
        }
        let changed=db(self.conn.execute("UPDATE schema_sources SET revision=?1,deleted=?2,body_json=?3,configuration_revision=?7 WHERE id=?4 AND revision=?5 AND owner_id=?6",params![row.connection.revision.get(),row.deleted,encode(row)?,row.connection.id.as_str(),expected,row.owner_id.0.as_str(),row.configuration_revision]))?;
        if changed != 1 {
            return Err(StorageError::ScopeDenied);
        }
        Ok(())
    }
    fn discard_schema_staging(&mut self, connection: &Id, n: u16) -> Result<bool, StorageError> {
        let n = limit(n)?;
        let removed=db(self.conn.execute("DELETE FROM schema_relations WHERE rowid IN (SELECT r.rowid FROM schema_relations r JOIN schema_views v ON v.id=r.view_id WHERE v.connection_id=?1 AND v.sealed=0 LIMIT ?2)",params![connection.as_str(),n]))?;
        db(self.conn.execute("DELETE FROM schema_objects WHERE rowid IN (SELECT o.rowid FROM schema_objects o JOIN schema_views v ON v.id=o.view_id WHERE v.connection_id=?1 AND v.sealed=0 LIMIT ?2)",params![connection.as_str(),n-removed as i64]))?;
        db(self.conn.execute("DELETE FROM schema_views WHERE id IN (SELECT v.id FROM schema_views v WHERE v.connection_id=?1 AND v.sealed=0 AND NOT EXISTS(SELECT 1 FROM schema_objects o WHERE o.view_id=v.id) AND NOT EXISTS(SELECT 1 FROM schema_relations r WHERE r.view_id=v.id) LIMIT ?2)",params![connection.as_str(),n]))?;
        db(self.conn.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM schema_views WHERE connection_id=?1 AND sealed=0)",
            [connection.as_str()],
            |r| r.get(0),
        ))
    }
    fn create_schema_view(&mut self, row: &StoredSchemaView) -> Result<(), StorageError> {
        if row.sealed {
            return Err(invalid("new schema view must be staged"));
        }
        db(self.conn.execute("INSERT INTO schema_views(id,connection_id,source_revision,operation_id,digest,sealed,created_at,configuration_revision,coverage_json) VALUES(?1,?2,?3,?4,?5,0,?6,?7,?8)",params![row.id.as_str(),row.connection_id.as_str(),row.source_revision,row.operation_id.0.as_str(),row.digest,row.created_at_ms,row.configuration_revision,encode(&row.coverage)?]))?;
        Ok(())
    }
    fn append_schema_rows(
        &mut self,
        view: &Id,
        objects: &[SchemaObject],
        relations: &[SchemaRelation],
    ) -> Result<(), StorageError> {
        if objects.len() + relations.len() > usize::from(MAX_PAGE) {
            return Err(invalid("schema staging batch exceeds500"));
        }
        let staging: bool = db(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM schema_views WHERE id=?1 AND sealed=0)",
            [view.as_str()],
            |r| r.get(0),
        ))?;
        if !staging {
            return Err(StorageError::ScopeDenied);
        }
        for o in objects {
            o.check().map_err(|_| invalid("invalid schema object"))?;
            if &o.view_id != view {
                return Err(StorageError::ScopeDenied);
            }
            let kind = serde_json::to_value(&o.kind).map_err(|_| invalid("invalid schema kind"))?;
            let kind = kind
                .as_str()
                .ok_or_else(|| invalid("invalid schema kind"))?;
            db(self.conn.execute("INSERT INTO schema_objects(view_id,id,kind,parent_id,qualified_name,body_json) VALUES(?1,?2,?3,?4,?5,?6)",params![view.as_str(),o.id.as_str(),kind,o.parent_id.as_ref().map(Id::as_str),o.qualified_name.0,encode(o)?]))?;
        }
        for r in relations {
            r.check().map_err(|_| invalid("invalid schema relation"))?;
            if &r.view_id != view {
                return Err(StorageError::ScopeDenied);
            }
            db(self.conn.execute("INSERT INTO schema_relations(view_id,id,from_id,to_id,body_json) VALUES(?1,?2,?3,?4,?5)",params![view.as_str(),r.id.as_str(),r.from_object_id.as_str(),r.to_object_id.as_str(),encode(r)?]))?;
        }
        Ok(())
    }
    fn seal_schema_view(&mut self, view: &Id) -> Result<(), StorageError> {
        let orphan:bool=db(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM schema_objects o WHERE o.view_id=?1 AND o.parent_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM schema_objects p WHERE p.view_id=o.view_id AND p.id=o.parent_id))",[view.as_str()],|r|r.get(0)))?;
        if orphan {
            return Err(invalid("schema view has missing parent objects"));
        }
        if db(self.conn.execute(
            "UPDATE schema_views SET sealed=1 WHERE id=?1 AND sealed=0",
            [view.as_str()],
        ))? != 1
        {
            return Err(StorageError::ScopeDenied);
        }
        Ok(())
    }
    fn remove_schema_views(&mut self, c: &Id) -> Result<(), StorageError> {
        db(self.conn.execute(
            "DELETE FROM schema_views WHERE connection_id=?1",
            [c.as_str()],
        ))?;
        Ok(())
    }
    fn record_schema_publication(
        &mut self,
        row: &StoredSchemaPublication,
    ) -> Result<(), StorageError> {
        row.result
            .check()
            .map_err(|_| invalid("invalid schema publication"))?;
        db(self.conn.execute("INSERT INTO schema_publications(operation_id,connection_id,body_json) VALUES(?1,?2,?3)",params![row.operation_id.0.as_str(),row.connection_id.as_str(),encode(row)?]))?;
        Ok(())
    }
}
