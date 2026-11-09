//! Long polling for retained agent messages. Each pass commits delivery independently, so a
//! waiting caller holds no SQLite write unit or connection between polls.
use crate::store;
use devworks_core::context::InvocationContext;
use devworks_core::ports::storage::{ApplicationStore, StorageError};
use devworks_core::protocol::ProjectId;
use devworks_core::services::agents::retained::{
    AckSelection, Acknowledged, DeliveredInbox, MessageEventPage, Recipient, Resolution,
    acknowledge, deliver_inbox, events,
};
use std::sync::Arc;
use std::time::Duration;

pub struct RetainedInboxDriver {
    store: Arc<dyn ApplicationStore>,
}

pub struct InboxSelection {
    pub recipient: Recipient,
    pub after: i64,
    pub include_read: bool,
    pub limit: u16,
    pub wait_seconds: u8,
}

impl RetainedInboxDriver {
    pub fn new(store: Arc<dyn ApplicationStore>) -> Self {
        Self { store }
    }

    pub async fn receive(
        &self,
        context: InvocationContext,
        project: ProjectId,
        selection: InboxSelection,
    ) -> Result<DeliveredInbox, StorageError> {
        if selection.wait_seconds > 55 {
            return Err(StorageError::InvalidRecord(
                "wait_seconds exceeds 55".into(),
            ));
        }
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(u64::from(selection.wait_seconds));
        loop {
            let context = context.clone();
            let project = project.clone();
            let recipient = selection.recipient.clone();
            let after = selection.after;
            let include_read = selection.include_read;
            let limit = selection.limit;
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

    pub async fn wait_events(
        &self,
        context: InvocationContext,
        project: ProjectId,
        after: Option<i64>,
        wait_seconds: u8,
    ) -> Result<MessageEventPage, StorageError> {
        if wait_seconds > 55 {
            return Err(StorageError::InvalidRecord(
                "wait_seconds exceeds 55".into(),
            ));
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(u64::from(wait_seconds));
        let mut cursor = after;
        loop {
            let context = context.clone();
            let project = project.clone();
            let page = store::read(&self.store, move |snapshot| {
                events(snapshot, &context, &project, cursor, 100)
            })
            .await?;
            cursor = Some(page.next_sequence);
            if !page.items.is_empty() || tokio::time::Instant::now() >= deadline {
                return Ok(page);
            }
            tokio::time::sleep_until(std::cmp::min(
                deadline,
                tokio::time::Instant::now() + Duration::from_secs(1),
            ))
            .await;
        }
    }
}
