//! Neutral building blocks the gateway, authority, settings, ports and providers share with the
//! services without depending on them. Services re-export these where callers already use the
//! old path, so the dependency points one way: services use the kernel, never the reverse.
pub mod egress;
pub mod failure;
pub mod notice;
pub mod reply_intent;
pub mod run_roots;
pub mod work_brief;
