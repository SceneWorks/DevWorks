//! Long polling for retained agent messages. Each pass commits delivery independently, so a
//! waiting caller holds no SQLite write unit or connection between polls.
use crate::store;
use devworks_core::context::InvocationContext;
use devworks_core::ports::storage::{ApplicationStore, StorageError};
use devworks_core::protocol::ProjectId;
use devworks_core::services::agents::retained::{
    AckSelection, Acknowledged, DeliveredInbox, Recipient, Resolution, acknowledge, deliver_inbox,
};
use std::sync::Arc;
use std::time::Duration;

pub struct RetainedInboxDriver {
    store: Arc<dyn ApplicationStore>,
}

impl RetainedInboxDriver {
    pub fn new(store: Arc<dyn ApplicationStore>) -> Self {
        Self { store }
    }

    pub async fn receive(
        &self,
        context: InvocationContext,
        project: ProjectId,
        recipient: Recipient,
        after: i64,
        include_read: bool,
        limit: u16,
        wait_seconds: u8,
    ) -> Result<DeliveredInbox, StorageError> {
        if wait_seconds > 55 {
            return Err(StorageError::InvalidRecord(
                "wait_seconds exceeds 55".into(),
            ));
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(u64::from(wait_seconds));
        loop {
            let context = context.clone();
            let project = project.clone();
            let recipient = recipient.clone();
            let page = store::write(&self.store, move |tx| {
                deliver_inbox(
                    tx,
                    &context,
                    &project,
                    &recipient,
                    after,
                    include_read,
                    limit,
                )
            })
            .await?;
            if !page.items.is_empty() || tokio::time::Instant::now() >= deadline {
                return Ok(page);
            }
            tokio::time::sleep_until(std::cmp::min(
                deadline,
                tokio::time::Instant::now() + Duration::from_millis(500),
            ))
            .await;
        }
    }

    pub async fn acknowledge(
        &self,
        context: InvocationContext,
        project: ProjectId,
        recipient: Recipient,
        selection: AckSelection,
        resolution: Option<Resolution>,
    ) -> Result<Acknowledged, StorageError> {
        store::write(&self.store, move |tx| {
            acknowledge(tx, &context, &project, &recipient, selection, resolution)
        })
        .await
    }
}
