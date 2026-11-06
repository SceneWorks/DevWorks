//! Native inference producers: profile codecs and real streaming HTTP remain inside the
//! runtime's owned accounting lifecycle, with a separate trusted durable result sink.
//!
//! `InferenceRequest`'s authority/control/instruction/cancellation revisions are opaque
//! originating-producer generations. Canonical request hashing binds them to replay identity;
//! they never mint authority or attest that the host loaded newer instructions. Actual host
//! identity, current principal/policy, scoped artifact references and session context epoch are
//! resolved independently. For managed work, ControlService::begin_dispatch serializes the
//! real activation owner, root controls, redirect incorporation and dispatch boundary. An earlier
//! redirect/Pause refuses dispatch; an already admitted request is in flight. The final native
//! send gate rechecks current ownership, pauses, artifact visibility and package publication.
//! Instruction text is exactly the scoped context artifact; this module makes no independent
//! claim that its caller assembled all currently active instructions.
pub mod driver;
pub mod schemas;
pub mod sink;
pub mod source;
