//! Neutral building blocks the gateway, authority, settings, ports and providers share with the
//! services without depending on them. Services re-export these where callers already use the
//! old path, so the dependency points one way: services use the kernel, never the reverse.
pub(crate) mod analysis;
pub mod checkout;
pub mod derivation;
pub mod document_evidence;
pub mod egress;
pub mod failure;
pub(crate) mod graph_request;
pub mod graph_view;
pub mod index_config;
pub mod notice;
pub mod project_visibility;
pub mod reply_intent;
pub mod run_roots;
pub mod work_brief;
pub(crate) mod work_plan;
