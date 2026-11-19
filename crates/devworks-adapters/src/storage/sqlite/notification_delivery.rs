//! Durable OS-submission fencing. Platform/policy qualification happens in the host worker.
use super::conn::db;
use super::repos::Unit;
use super::support::{conflict_strict, id_of, invalid};
use devworks_core::ports::storage::*;
use devworks_core::protocol::{ActorId, Id, RevisionDimension};
use rusqlite::{Connection, OptionalExtension, params};
pub(super) fn read(
    conn: &Connection,
    user: &ActorId,
    notification: &Id,
) -> Result<Option<StoredOsNotificationDelivery>, StorageError> {
    type Raw = (i64, String, i64, String, Option<String>, i64, i64);
    let raw: Option<Raw> = db(conn
        .query_row(
            "SELECT revision,boot_id,attempt,state,detail,created_at,updated_at FROM \
        os_notification_deliveries WHERE user_id=?1 AND notification_id=?2",
            params![user.0.as_str(), notification.as_str()],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .optional())?;
    raw.map(
        |(revision, boot, attempt, state, detail, created_at_ms, updated_at_ms)| {
            Ok(StoredOsNotificationDelivery {
                user_id: user.clone(),
                notification_id: notification.clone(),
                revision,
                boot_id: id_of(boot)?,
                attempt,
                state,
                detail,
                created_at_ms,
                updated_at_ms,
            })
        },
    )
    .transpose()
}
pub(super) fn claim(
    unit: &mut Unit<'_>,
    user: &ActorId,
    notification: &Id,
    expected: Option<i64>,
    retry: bool,
) -> Result<StoredOsNotificationDelivery, StorageError> {
    if unit.notification_users()?.as_slice() != std::slice::from_ref(user) {
        return Err(StorageError::ScopeDenied);
    }
    let n = unit
        .notification(user, notification)?
        .ok_or_else(|| StorageError::NotFound {
            kind: "notification",
            id: notification.as_str().into(),
        })?;
    if !n.requires_input || n.acknowledged {
        return Err(StorageError::ScopeDenied);
    }
    let boot = unit
        .store()
        .boot_id
        .clone()
        .ok_or_else(|| invalid("OS delivery requires the current host boot"))?;
    let old = read(unit.conn, user, notification)?;
    let (revision, attempt, created, updated) = match (expected, old) {
        (None, None) => (1, 1, unit.now_ms(), unit.now_ms()),
        (None, Some(_)) => {
            return Err(StorageError::Duplicate {
                kind: "os_notification_delivery",
                id: notification.as_str().into(),
            });
        }
        (Some(_), None) => {
            return Err(StorageError::NotFound {
                kind: "os_notification_delivery",
                id: notification.as_str().into(),
            });
        }
        (Some(e), Some(old)) => {
            if e != old.revision {
                return Err(conflict_strict(
                    RevisionDimension::Claim,
                    "os_notification_delivery",
                    notification.as_str(),
                    e,
                    old.revision,
                ));
            }
            if !retry || !matches!(old.state.as_str(), "Denied" | "Suppressed" | "Unavailable") {
                return Err(invalid(
                    "OS submission may retry only an explicitly requalified known non-delivery",
                ));
            }
            (
                old.revision
                    .checked_add(1)
                    .ok_or_else(|| invalid("OS delivery revision overflow"))?,
                old.attempt
                    .checked_add(1)
                    .ok_or_else(|| invalid("OS delivery attempt overflow"))?,
                old.created_at_ms,
                unit.now_ms().max(old.updated_at_ms),
            )
        }
    };
    db(unit.conn.execute(
        "INSERT INTO os_notification_deliveries(user_id,notification_id,revision,\
        boot_id,attempt,state,detail,created_at,updated_at) VALUES(?1,?2,?3,?4,\
        ?5,'Claimed',NULL,?6,?7) ON CONFLICT(user_id,notification_id) DO UPDATE \
        SET revision=excluded.revision,boot_id=excluded.boot_id,\
        attempt=excluded.attempt,state='Claimed',detail=NULL,\
        updated_at=excluded.updated_at",
        params![
            user.0.as_str(),
            notification.as_str(),
            revision,
            boot.as_str(),
            attempt,
            created,
            updated
        ],
    ))?;
    read(unit.conn, user, notification)?.ok_or_else(|| invalid("claimed OS delivery disappeared"))
}
pub(super) fn settle(
    unit: &mut Unit<'_>,
    user: &ActorId,
    notification: &Id,
    fence: &OsNotificationDeliveryFence,
    state: &str,
    detail: Option<&str>,
) -> Result<StoredOsNotificationDelivery, StorageError> {
    if !matches!(
        state,
        "Submitted" | "Denied" | "Suppressed" | "Unavailable" | "Unknown"
    ) {
        return Err(invalid("unknown OS delivery outcome"));
    }
    let old = read(unit.conn, user, notification)?.ok_or_else(|| StorageError::NotFound {
        kind: "os_notification_delivery",
        id: notification.as_str().into(),
    })?;
    if old.revision != fence.revision {
        return Err(conflict_strict(
            RevisionDimension::Claim,
            "os_notification_delivery",
            notification.as_str(),
            fence.revision,
            old.revision,
        ));
    }
    if old.state != "Claimed"
        || old.boot_id != fence.boot_id
        || old.attempt != fence.attempt
        || unit.store().boot_id.as_ref() != Some(&fence.boot_id)
    {
        return Err(StorageError::LeaseLost {
            kind: "os_notification_delivery",
            id: notification.as_str().into(),
        });
    }
    if detail.is_some_and(|s| s.len() > 4096) {
        return Err(invalid("OS delivery detail exceeds bound"));
    }
    let detail = detail.map(|s| devworks_core::authority::redact::Redactor::new().redact_text(s));
    let revision = old
        .revision
        .checked_add(1)
        .ok_or_else(|| invalid("OS delivery revision overflow"))?;
    db(unit.conn.execute(
        "UPDATE os_notification_deliveries SET revision=?3,state=?4,detail=?5,\
        updated_at=?6 WHERE user_id=?1 AND notification_id=?2 AND revision=?7 \
        AND boot_id=?8 AND attempt=?9 AND state='Claimed'",
        params![
            user.0.as_str(),
            notification.as_str(),
            revision,
            state,
            detail,
            unit.now_ms().max(old.updated_at_ms),
            fence.revision,
            fence.boot_id.as_str(),
            fence.attempt
        ],
    ))?;
    read(unit.conn, user, notification)?.ok_or_else(|| invalid("settled OS delivery disappeared"))
}
pub(super) fn recover(unit: &mut Unit<'_>) -> Result<u64, StorageError> {
    let boot = unit
        .store()
        .boot_id
        .as_ref()
        .ok_or_else(|| invalid("OS delivery recovery requires a host boot"))?;
    let n = db(unit.conn.execute(
        "UPDATE os_notification_deliveries SET revision=revision+1,\
        state='Unknown',detail='submission outcome unknown after host restart',\
        updated_at=max(updated_at,?2) WHERE state='Claimed' AND boot_id<>?1",
        params![boot.as_str(), unit.now_ms()],
    ))?;
    u64::try_from(n).map_err(|_| invalid("OS delivery recovery count overflow"))
}
