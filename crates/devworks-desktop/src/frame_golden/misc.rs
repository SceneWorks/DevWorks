//! Sample frames built outside a screen: the attachment size limit read and the receipt lookup
//! that resolves a purged command receipt.
use super::Silent;
use crate::{attachments, backend};
use devworks_core::protocol::RequestKey;

pub fn frames() -> Vec<(&'static str, Vec<u8>)> {
    let b = &Silent;
    let key: RequestKey = super::from(serde_json::json!("original-key"));
    vec![
        ("attachment_max_bytes", attachments::max_bytes_query(b)),
        ("receipt_lookup", backend::receipt_lookup(b, &key)),
        (
            "settings_repository_read",
            crate::ops::repository_settings_read(b, "project-a", "repo-a", &["delivery.autoMerge"]),
        ),
    ]
}
