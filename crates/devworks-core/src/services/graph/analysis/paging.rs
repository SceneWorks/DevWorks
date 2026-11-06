//! Deterministic report sections share the query's authenticated continuation.
use crate::ports::storage::StorageError;
use crate::services::graph::read::{DrilldownPosition, Position, Reader};
use crate::services::sessions::Failure;
use serde_json::{Value, json};

const FIELDS: &[&str] = &[
    "observedNodeCounts",
    "endpoints/nodes",
    "services/nodes",
    "tables/nodes",
    "recorded/endpoints",
    "recorded/services",
    "recorded/externalDependencies",
    "recorded/databaseTables",
];
const DRILLS: &[(&str, &str)] = &[
    ("endpoints", "Route"),
    ("services", "Service"),
    ("tables", "Table"),
];
struct Pager<'a, 'b> {
    reader: &'a mut Reader<'b>,
    at: usize,
    emitted: usize,
    stopped: bool,
}
impl Pager<'_, '_> {
    fn take(&mut self, row: &Value) -> Result<bool, Failure> {
        let index = self.at;
        self.at += 1;
        if index < self.reader.position.offset || self.stopped {
            return Ok(false);
        }
        if self.emitted == self.reader.limit || !self.reader.retain_wire(row)? {
            self.reader.next = Some(Position {
                offset: index,
                ..Default::default()
            });
            self.stopped = true;
            return Ok(false);
        }
        self.emitted += 1;
        Ok(true)
    }
    fn modules(&mut self, all: Vec<Value>) -> Result<Vec<Value>, Failure> {
        let mut out = Vec::new();
        for (index, module) in all.into_iter().enumerate() {
            let mut shown = skeleton(&module);
            let mut selected = self.take(&shown)?;
            for path in FIELDS {
                if let Some(rows) = module
                    .pointer(&format!("/{path}"))
                    .and_then(Value::as_array)
                {
                    for row in rows {
                        if self.take(row)? {
                            shown
                                .pointer_mut(&format!("/{path}"))
                                .and_then(Value::as_array_mut)
                                .expect("skeleton array")
                                .push(row.clone());
                            selected = true;
                        }
                    }
                }
            }
            if selected {
                finish_drills(self.reader, &module, &mut shown, index)?;
                out.push(shown);
            }
        }
        Ok(out)
    }
}
fn skeleton(module: &Value) -> Value {
    let mut shown = module.clone();
    for path in FIELDS {
        if let Some(array) = shown
            .pointer_mut(&format!("/{path}"))
            .and_then(Value::as_array_mut)
        {
            array.clear();
        }
    }
    shown
}
fn finish_drills(
    reader: &Reader<'_>,
    original: &Value,
    shown: &mut Value,
    index: usize,
) -> Result<(), Failure> {
    for (field, kind) in DRILLS {
        let all = original[*field]["nodes"]
            .as_array()
            .expect("node references");
        let page = shown[*field]["nodes"].as_array().expect("node references");
        // The global page's selected rows are contiguous for this field. An independent
        // drill-down cursor starts after the final displayed reference, or at zero.
        let end = page
            .last()
            .and_then(|last| all.iter().position(|row| row == last))
            .map_or(0, |at| at + 1);
        set_cursor(reader, shown, field, kind, index, end, all.len())?;
    }
    Ok(())
}
fn set_cursor(
    reader: &Reader<'_>,
    shown: &mut Value,
    field: &str,
    kind: &str,
    index: usize,
    end: usize,
    total: usize,
) -> Result<(), Failure> {
    let more = end < total;
    shown[field]["hasMore"] = json!(more);
    if more {
        let position = Position {
            offset: end,
            drilldown: Some(DrilldownPosition {
                module_index: index,
                node_kind: kind.into(),
            }),
            ..Default::default()
        };
        shown[field]["nextCursor"] = json!(reader.cursor(&position)?);
    } else {
        shown[field]
            .as_object_mut()
            .expect("drilldown")
            .remove("nextCursor");
    }
    Ok(())
}

pub(super) fn sections(
    reader: &mut Reader<'_>,
    value: &mut Value,
    sections: &[&str],
) -> Result<(), Failure> {
    if let Some(drilldown) = reader.position.drilldown.clone() {
        let all = value["internalModules"]
            .as_array()
            .ok_or(StorageError::CursorExpired)?;
        let module = all
            .get(drilldown.module_index)
            .ok_or(StorageError::CursorExpired)?
            .clone();
        let Some((field, _)) = DRILLS.iter().find(|(_, kind)| *kind == drilldown.node_kind) else {
            return Err(StorageError::CursorExpired.into());
        };
        let rows = module[*field]["nodes"].as_array().expect("references");
        if reader.position.offset > rows.len() {
            return Err(StorageError::CursorExpired.into());
        }
        let mut shown = skeleton(&module);
        let mut end = reader.position.offset;
        for row in rows.iter().skip(reader.position.offset) {
            if end - reader.position.offset == reader.limit || !reader.retain_wire(row)? {
                break;
            }
            shown[*field]["nodes"]
                .as_array_mut()
                .expect("references")
                .push(row.clone());
            end += 1;
        }
        finish_drills(reader, &module, &mut shown, drilldown.module_index)?;
        set_cursor(
            reader,
            &mut shown,
            field,
            &drilldown.node_kind,
            drilldown.module_index,
            end,
            rows.len(),
        )?;
        if end < rows.len() {
            reader.next = Some(Position {
                offset: end,
                drilldown: Some(drilldown),
                ..Default::default()
            });
        }
        for name in sections {
            if *name != "internalModules"
                && let Some(array) = value
                    .pointer_mut(&format!("/{name}"))
                    .and_then(Value::as_array_mut)
            {
                array.clear();
            }
        }
        value["internalModules"] = json!([shown]);
        return Ok(());
    }
    let mut pager = Pager {
        reader,
        at: 0,
        emitted: 0,
        stopped: false,
    };
    for name in sections {
        let rows = value
            .pointer_mut(&format!("/{name}"))
            .and_then(Value::as_array_mut)
            .expect("report section");
        let all = std::mem::take(rows);
        if *name == "internalModules" {
            *rows = pager.modules(all)?;
        } else {
            for row in all {
                if pager.take(&row)? {
                    rows.push(row);
                }
            }
        }
    }
    if pager.reader.position.offset > pager.at {
        return Err(StorageError::CursorExpired.into());
    }
    Ok(())
}

pub(super) fn metadata(
    metadata: &crate::services::graph::read::Metadata<'_>,
    value: &mut Value,
    sections: &[&str],
) -> Result<(), Failure> {
    let mut at = 0usize;
    let mut emitted = 0usize;
    let mut stopped = false;
    for name in sections {
        let rows = value
            .pointer_mut(&format!("/{name}"))
            .and_then(Value::as_array_mut)
            .expect("metadata section");
        for row in std::mem::take(rows) {
            let index = at;
            at += 1;
            if index < metadata.position.offset || stopped {
                continue;
            }
            if emitted == metadata.limit || !metadata.retain_wire(&row)? {
                *metadata.next.borrow_mut() = Some(Position {
                    offset: index,
                    ..Default::default()
                });
                stopped = true;
                continue;
            }
            rows.push(row);
            emitted += 1;
        }
    }
    if metadata.position.offset > at {
        return Err(StorageError::CursorExpired.into());
    }
    Ok(())
}
