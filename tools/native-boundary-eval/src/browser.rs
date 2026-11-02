use serde_json::{Value, json};
use std::net::TcpStream;
use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tungstenite::{Message, WebSocket, connect, stream::MaybeTlsStream};
struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}
struct Cdp {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    next: u64,
    events: Vec<String>,
}
impl Cdp {
    fn open(url: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let (socket, _) = connect(url)?;
        Ok(Self {
            socket,
            next: 0,
            events: vec![],
        })
    }
    fn call(&mut self, method: &str, params: Value) -> Result<Value, Box<dyn std::error::Error>> {
        self.next += 1;
        let id = self.next;
        self.socket.send(Message::Text(
            json!({"id":id,"method":method,"params":params})
                .to_string()
                .into(),
        ))?;
        loop {
            let message = self.socket.read()?;
            if let Message::Text(text) = message {
                let v: Value = serde_json::from_str(&text)?;
                if let Some(name) = v["method"].as_str() {
                    self.events.push(name.to_owned());
                }
                if v["id"].as_u64() == Some(id) {
                    if !v["error"].is_null() {
                        return Err(format!("CDP {method}: {}", v["error"]).into());
                    }
                    return Ok(v["result"].clone());
                }
            }
        }
    }
    fn eval(&mut self, expression: &str) -> Result<Value, Box<dyn std::error::Error>> {
        let v = self.call(
            "Runtime.evaluate",
            json!({"expression":expression,"returnByValue":true}),
        )?;
        if !v["exceptionDetails"].is_null() {
            return Err("fixture script exception".into());
        }
        Ok(v["result"]["value"].clone())
    }
}
pub fn run(chrome: &str, owned: &str) -> Result<Value, Box<dyn std::error::Error>> {
    let root = Path::new(owned).canonicalize()?;
    if fs::read_dir(&root)?.next().is_some() {
        return Err("owned browser fixture directory must start empty".into());
    }
    let profile = root.join("profile");
    fs::create_dir(&profile)?;
    fs::write(root.join("input.txt"), b"owned upload bytes")?;
    fs::write(
        root.join("index.html"),
        "<!doctype html><title>Owned DevWorks fixture</title><label>Fixture input<input id='text'></label><input id='file' type='file'><button id='go' onclick=\"document.querySelector('#result').textContent=document.querySelector('#text').value\">Apply</button><p id='result'></p>",
    )?;
    let mut child = OwnedChild(
        Command::new(chrome)
            .args([
                "--headless=new",
                "--remote-debugging-port=0",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-background-networking",
                "--disable-component-update",
                "--disable-sync",
                "--disable-crash-reporter",
            ])
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("about:blank")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let port_file = profile.join("DevToolsActivePort");
    while !port_file.exists() {
        if Instant::now() > deadline || child.0.try_wait()?.is_some() {
            return Err("owned Chrome did not become ready".into());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let port = fs::read_to_string(port_file)?
        .lines()
        .next()
        .ok_or("missing CDP port")?
        .parse::<u16>()?;
    let http = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()?;
    let version: Value = http
        .get(format!("http://127.0.0.1:{port}/json/version"))
        .send()?
        .json()?;
    let targets: Value = http
        .get(format!("http://127.0.0.1:{port}/json/list"))
        .send()?
        .json()?;
    let target = targets
        .as_array()
        .ok_or("target list")?
        .iter()
        .find(|v| v["type"] == "page")
        .ok_or("owned page")?;
    let mut cdp = Cdp::open(
        target["webSocketDebuggerUrl"]
            .as_str()
            .ok_or("page socket")?,
    )?;
    if let MaybeTlsStream::Plain(stream) = cdp.socket.get_mut() {
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    }
    let mut checks = vec!["native_rust_discovers_owned_headless_browser_without_node"];
    cdp.call("Page.enable", json!({}))?;
    cdp.call("Runtime.enable", json!({}))?;
    cdp.call("Network.enable", json!({}))?;
    cdp.call(
        "Page.navigate",
        json!({"url":format!("file://{}",root.join("index.html").display())}),
    )?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while cdp.eval("document.readyState==='complete'&&!!document.querySelector('#text')")? != true {
        if Instant::now() > deadline {
            return Err("fixture navigation timeout".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    checks.push("native_page_navigation_and_readiness");
    cdp.eval("document.querySelector('#text').focus()")?;
    cdp.call("Input.insertText", json!({"text":"native input"}))?;
    cdp.eval("document.querySelector('#go').click()")?;
    if cdp.eval("document.querySelector('#result').textContent")? != "native input" {
        return Err("input did not apply".into());
    }
    checks.push("native_input_and_action_observation");
    let document = cdp.call("DOM.getDocument", json!({}))?;
    let node = cdp.call(
        "DOM.querySelector",
        json!({"nodeId":document["root"]["nodeId"],"selector":"#file"}),
    )?;
    cdp.call(
        "DOM.setFileInputFiles",
        json!({"nodeId":node["nodeId"],"files":[root.join("input.txt")]}),
    )?;
    if cdp.eval("document.querySelector('#file').files[0].name")? != "input.txt" {
        return Err("upload did not bind".into());
    }
    checks.push("host_owned_file_input_upload");
    let ax = cdp.call("Accessibility.getFullAXTree", json!({}))?;
    if !ax.to_string().contains("Fixture input") {
        return Err("AX tree missing label".into());
    }
    checks.push("native_accessibility_snapshot");
    cdp.eval("console.log('owned-fixture-log')")?;
    cdp.eval("document.title")?;
    if !cdp.events.iter().any(|s| s == "Runtime.consoleAPICalled")
        || !cdp.events.iter().any(|s| s == "Network.requestWillBeSent")
    {
        return Err("console/network evidence missing".into());
    }
    checks.push("console_and_network_notifications_observed");
    let shot = cdp.call("Page.captureScreenshot", json!({"format":"png"}))?;
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(shot["data"].as_str().ok_or("screenshot payload")?)?;
    if !bytes.starts_with(b"\x89PNG") {
        return Err("screenshot type".into());
    }
    fs::write(root.join("evidence.png"), &bytes)?;
    checks.push("native_screenshot_evidence");
    let hist = cdp.call("Page.getNavigationHistory", json!({}))?;
    if hist["entries"].as_array().map_or(0, Vec::len) < 2 {
        return Err("navigation history".into());
    }
    checks.push("navigation_history_identity_available");
    let mut browser = Cdp::open(
        version["webSocketDebuggerUrl"]
            .as_str()
            .ok_or("browser socket")?,
    )?;
    let _ = browser.call("Browser.close", json!({}));
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.0.try_wait()?.is_none() {
        if Instant::now() > deadline {
            return Err("owned Chrome did not exit after close".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    checks.push("owned_browser_close_and_child_reaped");
    Ok(
        json!({"host":"macOS arm64","browser":version["Browser"],"protocol":version["Protocol-Version"],"nativeClient":"reqwest 0.12.28 + tungstenite 0.30.0","passed":checks.len(),"checks":checks,"screenshotBytes":bytes.len(),"noInference":true,"visibleTabsOpened":0,"limits":["Owned headless Chrome with local fixture only; not general browser authority, upload-path enforcement, download/network policy or OS process-tree qualification.","App packaging/update/signing and Chromium assets for every target remain qualification work."]}),
    )
}
