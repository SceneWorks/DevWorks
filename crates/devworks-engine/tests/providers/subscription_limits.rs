//! Native HTTP -> trusted response head -> source -> owned sink -> router feedback.
use super::*;
use devworks_adapters::provider_stream::NativeStreamingHttp;
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    thread,
    time::{Duration, Instant},
};
struct LocalHttp {
    endpoint: String,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    worker: Option<thread::JoinHandle<()>>,
}
impl LocalHttp {
    fn new(status: u16, headers: &[(&str, &str)], body: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let response = format!(
            "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n{body}",
            body.len(),
            headers
                .iter()
                .map(|(name, value)| format!("{name}: {value}\r\n"))
                .collect::<String>()
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("local response not consumed: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            captured.lock().unwrap().push(request(&mut stream));
            stream.write_all(response.as_bytes()).unwrap();
        });
        Self {
            endpoint: format!("http://{address}/v1"),
            requests,
            worker: Some(worker),
        }
    }
    fn finish(mut self) {
        self.worker.take().unwrap().join().unwrap();
    }
}
impl Drop for LocalHttp {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn request(stream: &mut TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut byte = [0];
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 65536);
        assert_eq!(stream.read(&mut byte).unwrap(), 1);
        bytes.push(byte[0]);
    }
    let count = String::from_utf8_lossy(&bytes)
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|n| n.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    let mut body = vec![0; count];
    stream.read_exact(&mut body).unwrap();
    bytes.extend(body);
    bytes
}
fn native(f: &mut Fx, endpoint: &str) {
    let endpoint = endpoint.to_owned();
    write(f.store.as_ref(), move |tx| {
        let mut connection = tx.router().connection(&id("connection"))?.unwrap();
        let expected = connection.revision;
        connection.endpoint = endpoint;
        if let Some(risk) = connection.risk.as_mut() {
            risk.binding = devworks_core::services::router::connections::profiles::risk_binding(
                &connection.profile,
                &connection.endpoint,
                connection.account_digest.as_deref(),
            );
        }
        tx.router()
            .put_connection(&connection, Some(expected))
            .map(|_| ())
    })
    .unwrap();
    f.driver = NativeProviders::new(
        f.store.clone(),
        f.artifacts.clone(),
        f.connections.clone(),
        f.usage.clone(),
        f.driver.schemas().clone(),
        ProviderComposition::new(Arc::new(NativeStreamingHttp::direct().unwrap())),
    );
}
#[test]
fn real_http_subscription_windows_create_exact_router_cooldown_without_fallback_reset() {
    runtime().block_on(async{
        for (profile,headers,until) in [
            ("CodexSubscriptionResponses",vec![("x-codex-primary-used-percent","100"),("x-codex-primary-reset-after-seconds","37")],T0+37_000),
            ("ClaudeSubscriptionMessages",vec![("anthropic-ratelimit-unified-5h-status","rejected"),("anthropic-ratelimit-unified-5h-reset","1700000041")],T0+41_000),
        ]{
            let server=LocalHttp::new(429,&headers,r#"{"error":{"code":"rate_limit_error","message":"subscription window exhausted"}}"#);
            let mut f=Fx::new(profile,Script::sse(""),false);native(&mut f,&server.endpoint);
            let result=f.produce(f.generation(vec![])).await.unwrap();assert_eq!(result.result.outcome,Some(AttemptEnd::Failed));
            assert_eq!(result.result.retry_at_ms,Some(until),"{profile}: only actual HTTP subscription headers can provide this reset");
            let cooldowns=read(f.store.as_ref(),|s|s.router().cooldowns());assert_eq!(cooldowns.len(),1);assert_eq!(cooldowns[0].model_id,f.model);assert_eq!(cooldowns[0].until_ms,until);
            let mut next=f.generation(vec![]);next.request_id=id("next-exhausted-request");
            let blocked=f.produce(next).await.unwrap();assert!(matches!(blocked.accounting,DriveOutcome::Refused(_)),"the router must refuse a fresh allocation while the recorded window is cooling: {:?}",blocked.accounting);
            assert_eq!(server.requests.lock().unwrap().len(),1,"failed inference was never replayed");
            let connection=read(f.store.as_ref(),|s|s.router().connection(&id("connection"))).unwrap();assert_eq!(connection.lifecycle,"Connected");
            assert!(f.http.lock().unwrap().requests.lock().unwrap().is_empty(),"no scripted ResponseHead bypassed the real adapter");server.finish();
        }
    });
}
