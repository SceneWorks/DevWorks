//! Recovery of a version stored with an older compact inspection. The logic lives in core
//! (`devworks_core::services::packages::recovery`) so core's own `addons.invoke` preparation
//! shares it with the host.
pub(crate) use devworks_core::services::packages::recovery::{Refused, compact, recover};
