//! The web search and fetch section of Settings > Agent tools (DW-51), headless on the real
//! runtime: the fetch runs through the same `web.fetch` use case agents call (native transport,
//! egress policy, artifact and activity record), its provenance and trust are shown, a refused
//! fetch and an unconfigured search are shown honestly, and the activity record lists both. A
//! scripted resolver and a mapping transport keep every request on this machine.
mod common;

use common::*;
use devworks_adapters::web::transport::NativeTransport;
use devworks_core::ports::secrets::NoSecretStore;
use devworks_core::ports::web::{HopRequest, HopResponse, HttpTransport, NetFault, Resolver};
use devworks_core::services::web::WebPorts;
use devworks_core::settings::catalog::Category;
use devworks_desktop::host::Engine;
use eframe::egui::accesskit::Role;
use egui_kittest::kittest::Queryable;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::Instant;

const PUBLIC: &str = "93.184.216.34";

struct Names;
impl Resolver for Names {
    fn resolve(
        &self,
        host: &str,
        _: u16,
        _: Instant,
        _: &[devworks_core::ports::storage::Cancellation],
    ) -> Result<Vec<IpAddr>, NetFault> {
        match host {
            "docs.test" => Ok(vec![PUBLIC.parse().unwrap()]),
            _ => Err(NetFault::NotSent {
                reason: "unknown".into(),
                timed_out: false,
            }),
        }
    }
}

/// The native transport with the scripted public address mapped onto the local server.
struct Mapped(NativeTransport, SocketAddr);
impl HttpTransport for Mapped {
    fn get(&self, request: &HopRequest<'_>) -> Result<HopResponse, NetFault> {
        let mut r = request.clone();
        if r.addr.ip() == PUBLIC.parse::<IpAddr>().unwrap() {
            r.addr = self.1;
        }
        self.0.get(&r)
    }
}

fn server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { return };
            let mut head = Vec::new();
            let mut b = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if s.read(&mut b).unwrap_or(0) == 0 {
                    break;
                }
                head.push(b[0]);
            }
            let body =
                b"<html><head><title>Docs</title></head><body><p>Hello docs</p></body></html>";
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            );
            let _ = s.write_all(body);
        }
    });
    addr
}

fn type_into(
    h: &mut egui_kittest::Harness<'_, devworks_desktop::shell::Shell>,
    label: &str,
    text: &str,
) {
    h.get_by_role_and_label(Role::TextInput, label).focus();
    h.step();
    h.get_by_role_and_label(Role::TextInput, label)
        .type_text(text);
    h.step();
}

#[test]
fn agent_tools_settings_fetch_search_and_show_web_activity() {
    let root = Root::new("web-panel");
    let mut config = config(&root.0);
    let addr = server();
    config.web = WebPorts {
        resolver: Arc::new(Names),
        transport: Arc::new(Mapped(NativeTransport::new(), addr)),
        providers: Vec::new(),
        secrets: Arc::new(NoSecretStore),
    };
    let engine = Engine::start(config).expect("the runtime boots");
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 1400.0);
    settle(&mut h);
    click_nav(&mut h, "Settings");
    click(&mut h, Role::Button, Category::Tools.label());
    // The new settings are listed with the policy summary.
    for label in [
        "Web page fetching",
        "Web search provider",
        "Fetch redirect limit",
        "Fetch from private networks",
    ] {
        assert!(has_text(&h, label), "{label}");
    }
    assert!(has_text(&h, "No search provider: search is unavailable"));
    assert!(has_text(&h, "Loopback and private networks blocked"));
    // A fetch runs the shared use case and shows provenance and trust.
    type_into(&mut h, "Fetch URL", "http://docs.test/guide");
    click(&mut h, Role::Button, "Fetch");
    assert!(
        has_text(&h, "Untrusted content"),
        "the result is marked untrusted"
    );
    assert!(has_text(&h, "HTTP 200"));
    assert!(has_text(&h, "Final URL: http://docs.test/guide"));
    assert!(has_text(&h, "Kept as artifact"));
    // A loopback fetch is refused by the egress policy, not attempted.
    let field = "Fetch URL";
    h.get_by_role_and_label(Role::TextInput, field).focus();
    h.step();
    h.state_mut().settings.web.url.clear();
    h.step();
    type_into(&mut h, field, &format!("http://127.0.0.1:{}/", addr.port()));
    click(&mut h, Role::Button, "Fetch");
    assert!(
        has_text(&h, "web_destination_Loopback"),
        "the refusal names its reason"
    );
    // Without a provider a search is refused as unsupported, never answered empty.
    type_into(&mut h, "Search query", "rust async");
    click(&mut h, Role::Button, "Search");
    assert!(has_text(&h, "UnsupportedCapability"));
    // The activity record lists every attempt.
    assert!(has_text(&h, "fetched"));
    assert!(has_text(&h, "web_destination_Loopback"));
    assert!(has_text(&h, "web_search_provider"));
    h.state_mut().on_exit();
}
