//! Real local HTTP boundary: no injected ResponseHead or Retry-After/body reset fallback.
use super::*;
use devworks_core::protocol::provider::EndpointProfile;
const NOW: i64 = 1_700_000_000_000;
fn subscription_headers() -> Vec<(&'static str, String)> {
    vec![
        ("x-codex-primary-used-percent", "100".into()),
        ("x-codex-primary-reset-at", "1700000060".into()),
        ("x-codex-primary-reset-after-seconds", "60".into()),
        ("x-codex-secondary-used-percent", "100".into()),
        ("x-codex-secondary-reset-at", "1700000120".into()),
        ("x-codex-secondary-reset-after-seconds", "120".into()),
        ("anthropic-ratelimit-unified-status", "rejected".into()),
        ("anthropic-ratelimit-unified-utilization", "1".into()),
        ("anthropic-ratelimit-unified-reset", "1700000180".into()),
        ("anthropic-ratelimit-unified-5h-status", "rejected".into()),
        ("anthropic-ratelimit-unified-5h-utilization", "1".into()),
        ("anthropic-ratelimit-unified-5h-reset", "1700000240".into()),
        ("anthropic-ratelimit-unified-7d-status", "rejected".into()),
        ("anthropic-ratelimit-unified-7d-utilization", "1".into()),
        ("anthropic-ratelimit-unified-7d-reset", "1700000300".into()),
    ]
}
fn server(headers: Vec<(&'static str, String)>) -> Server {
    Server::new(move |mut socket| {
        request_bytes(&mut socket);
        let mut response="HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: 2\r\n".to_owned();
        for (name, value) in headers {
            response.push_str(&format!("{name}: {value}\r\n"));
        }
        response.push_str("\r\n{}");
        socket.write_all(response.as_bytes()).unwrap();
    })
}
#[test]
fn real_http_retains_only_consumed_subscription_limit_headers_and_the_parser_sees_them() {
    let expected = subscription_headers();
    let mut headers = expected.clone();
    for name in [
        "set-cookie",
        "authorization",
        "proxy-authorization",
        "x-codex-private-credential",
        "anthropic-ratelimit-unified-private-credential",
    ] {
        headers.push((name, "must-not-retain-secret".into()));
    }
    let server = server(headers);
    let client = NativeStreamingHttp::direct().unwrap();
    let mut stream = client
        .open(
            &HttpRequest::get(server.url()),
            &limits(),
            &Cancellation::new(),
        )
        .unwrap();
    assert_eq!(stream.head().status, 429);
    assert!(stream.head().header("retry-after").is_none());
    for (name, value) in expected {
        assert_eq!(stream.head().header(name), Some(value.as_str()), "{name}");
    }
    assert!(
        !stream
            .head()
            .headers
            .iter()
            .any(|(_, value)| value.contains("must-not-retain-secret"))
    );
    for (profile, reset) in [
        (EndpointProfile::CodexSubscriptionResponses, NOW + 120_000),
        (EndpointProfile::ClaudeSubscriptionMessages, NOW + 300_000),
        (EndpointProfile::AnthropicMessages, NOW + 300_000),
    ] {
        let observed =
            devworks_core::providers::limits::headers(&profile, &stream.head().headers, NOW);
        assert!(observed.exhausted);
        assert_eq!(observed.reset_at_ms, Some(reset));
    }
    assert_eq!(all(&mut *stream).unwrap(), b"{}");
    stream.close().unwrap();
    server.finish();
}
#[test]
fn subscription_headers_still_obey_actual_total_byte_and_count_bounds() {
    for count_bound in [true, false] {
        let server = server(subscription_headers());
        let client = NativeStreamingHttp::direct().unwrap();
        let mut bounds = limits();
        if count_bound {
            bounds.max_headers = 4;
        } else {
            bounds.max_header_bytes = 100;
        }
        assert_eq!(
            expect_open_error(&client, &HttpRequest::get(server.url()), &bounds),
            after(TransportFailure::TooLarge)
        );
        server.finish();
    }
}
