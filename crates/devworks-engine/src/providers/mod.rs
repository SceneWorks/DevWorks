//! Native inference producers: profile codecs and real streaming HTTP remain inside the
//! runtime's owned accounting lifecycle, with a separate trusted durable result sink.
pub mod schemas;
pub mod sink;
pub mod source;
