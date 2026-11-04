//! Name resolution through the operating system (`getaddrinfo` / `GetAddrInfoW`).
//!
//! The system call has no timeout or cancellation of its own, so it runs on a short-lived helper
//! thread and the caller waits in slices of at most [`crate::web::transport::WAIT`], stopping at
//! the hop deadline or on cancellation; a lookup that outlives the wait is abandoned (its thread
//! ends when the system call returns) and reported as not sent. The answer is used once: core
//! checks every address and connects only to a checked one.
use super::transport::WAIT;
use devworks_core::ports::storage::Cancellation;
use devworks_core::ports::web::{NetFault, Resolver};
use std::net::{IpAddr, ToSocketAddrs};
use std::sync::mpsc;
use std::time::Instant;

/// Resolves through the operating system's resolver.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemResolver;

impl Resolver for SystemResolver {
    fn resolve(
        &self,
        host: &str,
        port: u16,
        deadline: Instant,
        cancel: &[Cancellation],
    ) -> Result<Vec<IpAddr>, NetFault> {
        let not_sent = |reason: String, timed_out: bool| NetFault::NotSent { reason, timed_out };
        if host.is_empty() || host.bytes().any(|b| b == 0 || b.is_ascii_whitespace()) {
            return Err(not_sent("an invalid host name".into(), false));
        }
        let (tx, rx) = mpsc::channel();
        let name = host.to_owned();
        std::thread::Builder::new()
            .name("devworks-web-resolve".into())
            .spawn(move || {
                let answer = (name.as_str(), port)
                    .to_socket_addrs()
                    .map(|addrs| addrs.map(|a| a.ip()).collect::<Vec<_>>());
                let _ = tx.send(answer);
            })
            .map_err(|e| not_sent(format!("resolution could not start: {e}"), false))?;
        loop {
            if cancel.iter().any(Cancellation::is_cancelled) {
                return Err(NetFault::Cancelled { sent: false });
            }
            let wait = deadline.saturating_duration_since(Instant::now());
            if wait.is_zero() {
                return Err(not_sent(
                    format!("resolving {host} did not finish in time"),
                    true,
                ));
            }
            match rx.recv_timeout(wait.min(WAIT)) {
                Ok(Ok(mut addrs)) => {
                    let mut seen = std::collections::HashSet::new();
                    addrs.retain(|a| seen.insert(*a));
                    return if addrs.is_empty() {
                        Err(not_sent(format!("{host} has no addresses"), false))
                    } else {
                        Ok(addrs)
                    };
                }
                Ok(Err(e)) => {
                    return Err(not_sent(
                        format!("{host} could not be resolved: {e}"),
                        false,
                    ));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(not_sent(format!("resolving {host} failed"), false));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn localhost_resolves_and_cancellation_and_deadlines_stop_the_wait() {
        let far = Instant::now() + Duration::from_secs(10);
        let addrs = SystemResolver.resolve("localhost", 80, far, &[]).unwrap();
        assert!(addrs.iter().all(IpAddr::is_loopback), "{addrs:?}");
        let cancelled = Cancellation::new();
        cancelled.cancel();
        assert_eq!(
            SystemResolver.resolve("localhost", 80, far, &[cancelled]),
            Err(NetFault::Cancelled { sent: false })
        );
        let past = Instant::now();
        assert!(matches!(
            SystemResolver.resolve("localhost", 80, past, &[]),
            Err(NetFault::NotSent {
                timed_out: true,
                ..
            })
        ));
        assert!(SystemResolver.resolve("bad host", 80, far, &[]).is_err());
    }
}
