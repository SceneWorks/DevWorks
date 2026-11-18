//! Pure helpers the desktop views call: constants, schema references, label and URL builders. A
//! view imports them from here and never reaches into [`crate::services`], so the views depend
//! on the protocol and these functions, not on how a service is built. Nothing here holds state
//! or performs I/O.

/// Convention manifest limits, its schema reference, and the editor's manifest writer.
pub mod conventions {
    pub use crate::services::conventions::model::{
        EditorConvention, EditorResource, EditorText, MAX_MARKDOWN_BYTES, manifest_schema,
        write_editor_manifest,
    };
}

/// Repository host links.
pub mod git {
    pub use crate::services::git::github::{Repository, branch_url};
}

/// The knowledge import format's identity.
pub mod knowledge {
    pub use crate::services::knowledge::import::{
        IMPORT_SCHEMA_ID, IMPORT_SCHEMA_REVISION, import_schema_digest,
    };
}

/// Package schema references.
pub mod packages {
    pub use crate::services::packages::{
        PROJECT_ENABLEMENT_SCHEMA, UPDATE_POLICY_SCHEMA, host_schema_ref,
    };
}

/// Provider connection profiles and the risk notice.
pub use crate::providers::profiles;

/// Whether desktop control was held when a click was received.
pub use crate::services::desktop::ControlEpoch;

/// What a job's native steps can do on this host.
pub use crate::services::jobs::NativeCapability;

/// The media type of a watcher's notice artifact.
pub use crate::services::watch::WATCHER_NOTICE_MEDIA_TYPE;

/// Work screens: search folding, the prerequisite bound and the default statuses.
pub mod work {
    pub use crate::services::work::deps::MAX_PREREQUISITES;
    pub use crate::services::work::fold;
    pub use crate::services::work::workflow::DEFAULT_STATUSES;
}

/// Router names.
pub mod router {
    pub use crate::services::router::latest::alias_family;
}
