//! The native owned-browser platform (DW-047) without a browser: discovery and per-target
//! qualification, owned session roots created new and removed without following links, staged
//! files with safe names and bounds, the private DevTools pipe command, and the pipe transport
//! (NUL-terminated messages) against an in-process peer over OS pipes speaking CDP shapes.
use devworks_adapters::browser::{Candidate, NativeBrowserPlatform, PipeTransport};
use devworks_core::ports::browser::{BrowserFault, BrowserPlatform, CdpTransport, Qualification};
use devworks_core::protocol::Id;
use devworks_core::services::browser::cdp::{Cdp, CdpError};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

struct Dir(PathBuf);
impl Dir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "devworks-browser-adapter-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn candidate(id: &str, paths: Vec<PathBuf>, names: &[&str]) -> Candidate {
    Candidate {
        id: id.into(),
        product: format!("Product {id}"),
        paths,
        names: names.iter().map(|n| (*n).to_owned()).collect(),
        unsupported: None,
    }
}

fn executable(path: &Path) {
    std::fs::write(path, b"#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn platform(d: &Dir) -> NativeBrowserPlatform {
    NativeBrowserPlatform::with_candidates(&d.0, vec![], vec![])
}

#[test]
fn discovery_reports_each_asset_present_missing_failed_or_unsupported() {
    let d = Dir::new();
    let bin = d.0.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let exe = bin.join("chrome");
    executable(&exe);
    let on_path = bin.join("chromium-browser");
    executable(&on_path);
    let dir_not_file = bin.join("edge-dir");
    std::fs::create_dir_all(&dir_not_file).unwrap();
    let mut unsupported = candidate("arm-only", vec![exe.clone()], &[]);
    unsupported.unsupported = Some("no build for this target".into());
    let p = NativeBrowserPlatform::with_candidates(
        &d.0,
        vec![
            candidate("chrome", vec![d.0.join("missing"), exe.clone()], &[]),
            candidate("chromium", vec![], &["chromium-browser"]),
            candidate("edge", vec![dir_not_file.clone()], &[]),
            candidate("missing", vec![d.0.join("nowhere")], &["no-such-browser"]),
            unsupported,
        ],
        vec![bin.clone()],
    );
    let assets = p.assets();
    let q = |id: &str| assets.iter().find(|a| a.id == id).unwrap().clone();
    assert_eq!(q("chrome").qualification, Qualification::Present);
    assert_eq!(q("chrome").program, Some(exe.clone()));
    assert_eq!(q("chromium").program, Some(on_path));
    assert!(matches!(q("edge").qualification, Qualification::Failed(_)));
    assert!(matches!(
        q("missing").qualification,
        Qualification::Missing(_)
    ));
    assert!(matches!(
        q("arm-only").qualification,
        Qualification::Unsupported(_)
    ));
    assert_eq!(
        q("arm-only").program,
        None,
        "an unsupported asset offers no program"
    );
    #[cfg(unix)]
    {
        // A file without execute permission is not offered.
        let plain = bin.join("plain");
        std::fs::write(&plain, b"x").unwrap();
        let p = NativeBrowserPlatform::with_candidates(
            &d.0,
            vec![candidate("plain", vec![plain], &[])],
            vec![],
        );
        assert!(matches!(
            p.assets()[0].qualification,
            Qualification::Failed(_)
        ));
    }
    // The configured executable is offered first as its own asset.
    let p = NativeBrowserPlatform::new(&d.0, Some(exe.clone()));
    let first = &p.assets()[0];
    assert_eq!(
        (first.id.as_str(), &first.qualification),
        ("configured", &Qualification::Present)
    );
    assert!(p.target().contains(std::env::consts::OS));
}

#[test]
fn session_roots_are_created_new_and_removed_without_following_links() {
    let d = Dir::new();
    let p = platform(&d);
    let s = Id::new("session-a").unwrap();
    let profile = p.create_profile(&s).unwrap();
    assert!(profile.user_data.is_dir());
    assert!(profile.user_data.starts_with(&profile.root));
    assert!(
        matches!(p.create_profile(&s), Err(BrowserFault::Refused(_))),
        "never reused"
    );
    assert_eq!(p.profiles().unwrap(), vec![s.clone()]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&profile.root)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "a session root is private");
        // A link inside the profile to a directory outside: removal never traverses it.
        let outside = d.0.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.txt"), b"keep").unwrap();
        std::os::unix::fs::symlink(&outside, profile.user_data.join("link")).unwrap();
        p.remove_profile(&s).unwrap();
        assert!(!profile.root.exists());
        assert_eq!(std::fs::read(outside.join("keep.txt")).unwrap(), b"keep");
        // A link in place of a session root is removed itself, its target untouched.
        let t = Id::new("session-b").unwrap();
        std::fs::create_dir_all(d.0.join("browser").join("sessions")).unwrap();
        std::os::unix::fs::symlink(
            &outside,
            d.0.join("browser").join("sessions").join("session-b"),
        )
        .unwrap();
        assert!(
            matches!(p.create_profile(&t), Err(BrowserFault::Refused(_))),
            "an existing link is refused"
        );
        p.remove_profile(&t).unwrap();
        assert!(outside.join("keep.txt").exists());
    }
    #[cfg(not(unix))]
    p.remove_profile(&s).unwrap();
    // Removing an absent root is not an error.
    p.remove_profile(&Id::new("never").unwrap()).unwrap();
}

#[test]
fn staged_files_have_safe_names_create_new_semantics_and_bounds() {
    let d = Dir::new();
    let p = platform(&d);
    let profile = p.create_profile(&Id::new("session-s").unwrap()).unwrap();
    let src = |b: &'static [u8]| std::io::Cursor::new(b);
    for bad in ["../x", "a/b", "a\\b", ".hidden", "", "c:evil"] {
        assert!(
            matches!(
                p.stage_file(&profile, "batch-1", bad, &mut src(b"x"), 10),
                Err(BrowserFault::Refused(_))
            ),
            "{bad:?}"
        );
        assert!(
            matches!(
                p.stage_file(&profile, bad, "f.txt", &mut src(b"x"), 10),
                Err(BrowserFault::Refused(_))
            ),
            "{bad:?}"
        );
    }
    let path = p
        .stage_file(&profile, "batch-1", "1-report.txt", &mut src(b"hello"), 10)
        .unwrap();
    assert_eq!(path, profile.root.join("batch-1").join("1-report.txt"));
    assert_eq!(std::fs::read(&path).unwrap(), b"hello");
    assert!(
        matches!(
            p.stage_file(&profile, "batch-1", "1-report.txt", &mut src(b"again"), 10),
            Err(BrowserFault::Refused(_))
        ),
        "never overwritten"
    );
    assert!(matches!(
        p.stage_file(
            &profile,
            "batch-1",
            "big.txt",
            &mut src(b"0123456789AB"),
            10
        ),
        Err(BrowserFault::Refused(_))
    ));
    assert!(
        !profile.root.join("batch-1").join("big.txt").exists(),
        "an oversize file is removed"
    );
    #[cfg(unix)]
    {
        let outside = d.0.join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, profile.root.join("batch-link")).unwrap();
        assert!(matches!(
            p.stage_file(&profile, "batch-link", "f.txt", &mut src(b"x"), 10),
            Err(BrowserFault::Refused(_))
        ));
        assert!(!outside.join("f.txt").exists());
        std::os::unix::fs::symlink(
            outside.join("target.txt"),
            profile.root.join("batch-1").join("2-link.txt"),
        )
        .unwrap();
        assert!(
            p.stage_file(&profile, "batch-1", "2-link.txt", &mut src(b"x"), 10)
                .is_err()
        );
        assert!(
            !outside.join("target.txt").exists(),
            "a planted link is never written through"
        );
    }
    p.remove_staged(&profile, "batch-1").unwrap();
    assert!(!profile.root.join("batch-1").exists());
}

/// An in-process DevTools pipe peer over OS pipes: reads NUL-terminated requests, emits an
/// event before each answer, and closes its end on `Browser.close`.
fn peer() -> (PipeTransport, std::thread::JoinHandle<Vec<Value>>) {
    let (from_host, host_writer) = std::io::pipe().unwrap();
    let (host_reader, mut to_host) = std::io::pipe().unwrap();
    let t = std::thread::spawn(move || {
        let mut seen = vec![];
        let mut reader = std::io::BufReader::new(from_host);
        loop {
            let mut m = Vec::new();
            if std::io::BufRead::read_until(&mut reader, 0, &mut m).unwrap_or(0) == 0 {
                break;
            }
            assert_eq!(m.pop(), Some(0), "every message is NUL-terminated");
            let v: Value = serde_json::from_slice(&m).unwrap();
            seen.push(v.clone());
            if v["method"] == "Browser.close" {
                break;
            }
            let event = json!({"method": "Runtime.consoleAPICalled", "sessionId": v["sessionId"], "params": {"type": "log"}});
            let answer = json!({"id": v["id"], "result": {"echo": v["method"]}});
            for out in [event, answer] {
                to_host.write_all(out.to_string().as_bytes()).unwrap();
                to_host.write_all(&[0]).unwrap();
            }
        }
        // The browser's end closes: the host sees the end of the channel.
        drop(to_host);
        seen
    });
    (
        PipeTransport::new(Box::new(host_writer), Box::new(host_reader)),
        t,
    )
}

#[test]
fn the_pipe_transport_carries_nul_terminated_cdp_messages_and_reports_closure() {
    let (transport, peer) = peer();
    let mut cdp = Cdp::new(Box::new(transport));
    let r = cdp
        .call(Some("S1"), "Page.enable", json!({}), Duration::from_secs(5))
        .unwrap();
    assert_eq!(r["echo"], "Page.enable");
    let events = cdp.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].session.as_deref(), Some("S1"));
    // Nothing arrives: a bounded read returns empty, not an error.
    cdp.pump(Duration::from_millis(50), Duration::from_millis(10))
        .unwrap();
    // The peer closes: the client reports a transport failure, never a hang or a success.
    let closed = cdp.call(None, "Browser.close", json!({}), Duration::from_secs(5));
    assert!(matches!(closed, Err(CdpError::Transport(_))), "{closed:?}");
    assert!(cdp.closed());
    let seen = peer.join().unwrap();
    assert_eq!(seen[0]["sessionId"], "S1");
    assert_eq!(seen.len(), 2);
}

#[test]
fn a_truncated_or_invalid_message_closes_the_pipe() {
    for bytes in [&b"{\"method\":\"half"[..], b"\xff\xfe\0"] {
        let (host_reader, mut to_host) = std::io::pipe().unwrap();
        let (_unused, host_writer) = std::io::pipe().unwrap();
        to_host.write_all(bytes).unwrap();
        drop(to_host);
        let mut t = PipeTransport::new(Box::new(host_writer), Box::new(host_reader));
        assert!(
            matches!(t.recv(Duration::from_secs(5)), Err(BrowserFault::Closed(_))),
            "{bytes:?}"
        );
        assert!(t.send("{}").is_err(), "a closed pipe refuses to send");
    }
}

#[test]
fn the_browser_command_asks_for_the_private_pipe_and_connect_needs_it() {
    let d = Dir::new();
    let p = platform(&d);
    let profile = p.create_profile(&Id::new("session-c").unwrap()).unwrap();
    let asset = devworks_core::ports::browser::BrowserAsset {
        id: "chrome".into(),
        product: "Chrome".into(),
        program: Some(PathBuf::from("/browser")),
        qualification: Qualification::Present,
    };
    let spec = p.command(&asset, &profile, &["--remote-debugging-pipe".into()]);
    let pipe = spec.control_pipe.expect("a private control pipe");
    assert_eq!(
        pipe.windows_args,
        vec!["--remote-debugging-io-pipes={read},{write}".to_owned()],
        "the inherited handles are named as Chromium's AdoptPipes reads them"
    );
    assert_eq!(spec.cwd.as_deref(), Some(profile.root.as_path()));
    assert!(matches!(
        p.connect(&profile, None),
        Err(BrowserFault::NotReady(_))
    ));
}
