//! The native owned-browser platform (DW-047): Chromium-family browser discovery and per-target
//! qualification, owned session roots under the data root, the launch command, and the private
//! DevTools pipe transport. No Node, Python or driver process is involved; the browser itself is
//! launched by the core process owner.
//!
//! **Discovery** reads the filesystem only: well-known install locations of Google Chrome,
//! Chromium and Microsoft Edge for this OS, `PATH` on Linux, and an explicitly configured
//! executable. Each asset reports `Present`, `Missing`, `Unsupported` (no build of that product
//! exists for this OS/CPU target) or `Failed` (not a regular executable file). Presence is not
//! qualification: the browser service qualifies an asset by a successful launch handshake.
//!
//! **Session roots** are `<data root>/browser/sessions/<session id>/` (mode 0700 on Unix), each
//! with an empty `profile/` user-data directory and the session's staged upload batches. A root
//! is created with create-new semantics (an existing entry is refused, never reused) and
//! removed without following links (`std::fs::remove_dir_all` removes a link, never its target).
//! Staged files are written with create-new and no-follow semantics into a fresh batch
//! directory; names are single safe components.
//!
//! **Transport.** The browser runs with `--remote-debugging-pipe` and no DevTools port: the
//! process owner gives it a private control channel of anonymous pipes (its file descriptors 3
//! and 4 on macOS and Linux; inherited handles named by `--remote-debugging-io-pipes=<in>,<out>`
//! on Windows, as Chromium's `AdoptPipes` reads them), and messages are NUL-terminated JSON.
//! No socket, port or name exists, so no other local process can attach. The channel closes
//! when the browser exits.
use devworks_core::ports::browser::{
    BrowserAsset, BrowserFault, BrowserPlatform, CdpTransport, Profile, Qualification,
    STAGED_NAME_MAX_BYTES, user_data_of,
};
use devworks_core::ports::process::{ControlChannel, ControlPipe, ProcessSpec};
use devworks_core::protocol::Id;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

/// Largest DevTools message accepted (full-page screenshots arrive as one message).
pub const MESSAGE_LIMIT: usize = 64 * 1024 * 1024;

/// Where one asset is looked for.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub id: String,
    pub product: String,
    /// Absolute paths tried in order.
    pub paths: Vec<PathBuf>,
    /// Program names searched on `PATH` (Linux).
    pub names: Vec<String>,
    /// Why the product has no build for this target, when it has none.
    pub unsupported: Option<String>,
}

/// The native platform.
pub struct NativeBrowserPlatform {
    sessions: PathBuf,
    candidates: Vec<Candidate>,
    search_path: Vec<PathBuf>,
}

impl NativeBrowserPlatform {
    /// The platform for this OS/CPU target, keeping session roots under `data_root`, with an
    /// optional explicitly configured browser executable (asset `configured`).
    pub fn new(data_root: &Path, configured: Option<PathBuf>) -> Self {
        let mut candidates = default_candidates();
        if let Some(p) = configured {
            candidates.insert(
                0,
                Candidate {
                    id: "configured".into(),
                    product: "Configured browser".into(),
                    paths: vec![p],
                    names: vec![],
                    unsupported: None,
                },
            );
        }
        let search_path = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default();
        Self::with_candidates(data_root, candidates, search_path)
    }

    /// A platform with explicit candidates and search path (tests and packaging).
    pub fn with_candidates(
        data_root: &Path,
        candidates: Vec<Candidate>,
        search_path: Vec<PathBuf>,
    ) -> Self {
        Self {
            sessions: data_root.join("browser").join("sessions"),
            candidates,
            search_path,
        }
    }

    fn root_of(&self, session: &Id) -> PathBuf {
        // An Id is 1-128 of [A-Za-z0-9_-]: always one safe path component.
        self.sessions.join(session.as_str())
    }

    fn profile_of(&self, session: &Id) -> Profile {
        let root = self.root_of(session);
        Profile {
            session_id: session.clone(),
            user_data: user_data_of(&root),
            root,
        }
    }

    /// The sessions directory, created when missing; a link or non-directory is refused.
    fn sessions_dir(&self) -> Result<&Path, BrowserFault> {
        std::fs::create_dir_all(&self.sessions).map_err(io)?;
        let meta = std::fs::symlink_metadata(&self.sessions).map_err(io)?;
        if !meta.is_dir() {
            return Err(BrowserFault::Refused(
                "the browser sessions directory is not a directory".into(),
            ));
        }
        Ok(&self.sessions)
    }
}

fn io(e: std::io::Error) -> BrowserFault {
    BrowserFault::Io(e.to_string())
}

/// One safe path component: non-empty, at most 128 bytes, no separator, no leading dot, no
/// control character.
fn component(name: &str) -> Result<&str, BrowserFault> {
    let ok = !name.is_empty()
        && name.len() <= STAGED_NAME_MAX_BYTES
        && !name.starts_with('.')
        && !name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':'));
    if ok {
        Ok(name)
    } else {
        Err(BrowserFault::Refused(format!(
            "{name:?} is not a single safe file name"
        )))
    }
}

#[cfg(unix)]
fn executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}
#[cfg(not(unix))]
fn executable(_: &std::fs::Metadata) -> bool {
    true
}

fn qualify(c: &Candidate, search_path: &[PathBuf]) -> BrowserAsset {
    let asset = |program: Option<PathBuf>, qualification| BrowserAsset {
        id: c.id.clone(),
        product: c.product.clone(),
        program,
        qualification,
    };
    if let Some(why) = &c.unsupported {
        return asset(None, Qualification::Unsupported(why.clone()));
    }
    let named = c
        .names
        .iter()
        .flat_map(|n| search_path.iter().map(move |d| d.join(n)));
    let mut failed = None;
    for p in c.paths.iter().cloned().chain(named) {
        // Discovery follows links (installed browsers are often linked into PATH).
        match std::fs::metadata(&p) {
            Ok(m) if m.is_file() && executable(&m) => {
                return asset(Some(p), Qualification::Present);
            }
            Ok(_) => {
                failed.get_or_insert_with(|| {
                    (
                        p.clone(),
                        format!("{} is not an executable file", p.display()),
                    )
                });
            }
            Err(_) => {}
        }
    }
    match failed {
        Some((p, why)) => asset(Some(p), Qualification::Failed(why)),
        None => asset(
            None,
            Qualification::Missing(format!(
                "{} was not found in its install locations",
                c.product
            )),
        ),
    }
}

/// The well-known install locations of this OS/CPU target.
pub fn default_candidates() -> Vec<Candidate> {
    let c = |id: &str, product: &str, paths: Vec<PathBuf>, names: &[&str]| Candidate {
        id: id.into(),
        product: product.into(),
        paths,
        names: names.iter().map(|n| (*n).to_owned()).collect(),
        unsupported: None,
    };
    if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let app = |bundle: &str, exe: &str| {
            let mut v = vec![PathBuf::from(format!(
                "/Applications/{bundle}.app/Contents/MacOS/{exe}"
            ))];
            if let Some(h) = &home {
                v.push(h.join(format!("Applications/{bundle}.app/Contents/MacOS/{exe}")));
            }
            v
        };
        vec![
            c(
                "chrome",
                "Google Chrome",
                app("Google Chrome", "Google Chrome"),
                &[],
            ),
            c("chromium", "Chromium", app("Chromium", "Chromium"), &[]),
            c(
                "edge",
                "Microsoft Edge",
                app("Microsoft Edge", "Microsoft Edge"),
                &[],
            ),
        ]
    } else if cfg!(windows) {
        let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
        let under = |roots: &[&str], rel: &str| {
            roots
                .iter()
                .filter_map(|r| env(r))
                .map(|r| r.join(rel))
                .collect::<Vec<_>>()
        };
        let all = ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"];
        vec![
            c(
                "chrome",
                "Google Chrome",
                under(&all, "Google\\Chrome\\Application\\chrome.exe"),
                &[],
            ),
            c(
                "chromium",
                "Chromium",
                under(&["LOCALAPPDATA"], "Chromium\\Application\\chrome.exe"),
                &[],
            ),
            c(
                "edge",
                "Microsoft Edge",
                under(&all, "Microsoft\\Edge\\Application\\msedge.exe"),
                &[],
            ),
        ]
    } else {
        let mut chrome = c(
            "chrome",
            "Google Chrome",
            vec!["/opt/google/chrome/chrome".into()],
            &["google-chrome", "google-chrome-stable"],
        );
        let mut edge = c(
            "edge",
            "Microsoft Edge",
            vec!["/opt/microsoft/msedge/msedge".into()],
            &["microsoft-edge", "microsoft-edge-stable"],
        );
        if cfg!(target_arch = "aarch64") {
            // Neither product publishes a Linux arm64 build; Chromium does.
            chrome.unsupported = Some("Google Chrome publishes no Linux arm64 build".into());
            edge.unsupported = Some("Microsoft Edge publishes no Linux arm64 build".into());
        }
        vec![
            chrome,
            c(
                "chromium",
                "Chromium",
                vec!["/snap/bin/chromium".into()],
                &["chromium", "chromium-browser"],
            ),
            edge,
        ]
    }
}

impl BrowserPlatform for NativeBrowserPlatform {
    fn target(&self) -> String {
        format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
    }

    fn assets(&self) -> Vec<BrowserAsset> {
        self.candidates
            .iter()
            .map(|c| qualify(c, &self.search_path))
            .collect()
    }

    fn create_profile(&self, session: &Id) -> Result<Profile, BrowserFault> {
        self.sessions_dir()?;
        let p = self.profile_of(session);
        // create_dir fails on an existing entry (a link included): never reused or followed.
        create_private_dir(&p.root)?;
        create_private_dir(&p.user_data)?;
        Ok(p)
    }

    fn remove_profile(&self, session: &Id) -> Result<(), BrowserFault> {
        let root = self.root_of(session);
        match std::fs::symlink_metadata(&root) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io(e)),
            // A link in place of the root is removed itself; its target is never traversed.
            Ok(m) if !m.is_dir() => std::fs::remove_file(&root).map_err(io),
            Ok(_) => std::fs::remove_dir_all(&root).map_err(io),
        }
    }

    fn profiles(&self) -> Result<Vec<Id>, BrowserFault> {
        let dir = match std::fs::read_dir(&self.sessions) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(io(e)),
        };
        let mut out = Vec::new();
        for e in dir {
            let e = e.map_err(io)?;
            if let Some(name) = e.file_name().to_str()
                && let Ok(id) = Id::new(name)
            {
                out.push(id);
            }
        }
        out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(out)
    }

    fn command(&self, asset: &BrowserAsset, profile: &Profile, args: &[String]) -> ProcessSpec {
        ProcessSpec {
            pipe_stdin: false,
            stdin: None,
            program: asset
                .program
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            args: args.to_vec(),
            cwd: Some(profile.root.clone()),
            env: Vec::new(),
            clear_env: false,
            capture_output: false,
            control_pipe: Some(ControlPipe {
                windows_args: vec!["--remote-debugging-io-pipes={read},{write}".into()],
            }),
        }
    }

    fn connect(
        &self,
        _profile: &Profile,
        control: Option<ControlChannel>,
    ) -> Result<Box<dyn CdpTransport>, BrowserFault> {
        let (writer, reader) = control.ok_or_else(|| {
            BrowserFault::NotReady("the browser was launched without its control pipe".into())
        })?;
        Ok(Box::new(PipeTransport::new(writer, reader)))
    }

    fn stage_file(
        &self,
        profile: &Profile,
        batch: &str,
        name: &str,
        source: &mut dyn Read,
        max_bytes: u64,
    ) -> Result<PathBuf, BrowserFault> {
        let batch = component(batch)?;
        let name = component(name)?;
        let root = std::fs::symlink_metadata(&profile.root).map_err(io)?;
        if !root.is_dir() {
            return Err(BrowserFault::Refused(
                "the session root is not a directory".into(),
            ));
        }
        let dir = profile.root.join(batch);
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                return Err(BrowserFault::Refused(
                    "the staging batch is not a directory".into(),
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => create_private_dir(&dir)?,
            Err(e) => return Err(io(e)),
        }
        let path = dir.join(name);
        let mut out = create_new(&path)?;
        let mut limited = source.take(max_bytes.saturating_add(1));
        let n = std::io::copy(&mut limited, &mut out).map_err(io);
        let n = match n {
            Ok(n) => n,
            Err(e) => {
                drop(out);
                let _ = std::fs::remove_file(&path);
                return Err(e);
            }
        };
        if n > max_bytes {
            drop(out);
            let _ = std::fs::remove_file(&path);
            return Err(BrowserFault::Refused(
                "the staged file exceeds its size bound".into(),
            ));
        }
        out.flush().map_err(io)?;
        out.sync_all().map_err(io)?;
        Ok(path)
    }

    fn read_staged(
        &self,
        profile: &Profile,
        path: &Path,
        max_bytes: u64,
    ) -> Result<Vec<u8>, BrowserFault> {
        if !path.starts_with(&profile.root)
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(BrowserFault::Refused(
                "the file is not staged under the session root".into(),
            ));
        }
        let meta = std::fs::symlink_metadata(path).map_err(io)?;
        if !meta.is_file() {
            return Err(BrowserFault::Refused(
                "the staged path is not a file".into(),
            ));
        }
        if meta.len() > max_bytes {
            return Err(BrowserFault::Refused(
                "the staged file exceeds its size bound".into(),
            ));
        }
        let mut out = Vec::new();
        std::fs::File::open(path)
            .and_then(|f| f.take(max_bytes + 1).read_to_end(&mut out))
            .map_err(io)?;
        if out.len() as u64 > max_bytes {
            return Err(BrowserFault::Refused(
                "the staged file exceeds its size bound".into(),
            ));
        }
        Ok(out)
    }

    fn remove_staged(&self, profile: &Profile, batch: &str) -> Result<(), BrowserFault> {
        let dir = profile.root.join(component(batch)?);
        match std::fs::symlink_metadata(&dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io(e)),
            Ok(m) if !m.is_dir() => std::fs::remove_file(&dir).map_err(io),
            Ok(_) => std::fs::remove_dir_all(&dir).map_err(io),
        }
    }
}

fn create_private_dir(path: &Path) -> Result<(), BrowserFault> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::AlreadyExists => {
                    BrowserFault::Refused(format!("{} already exists", path.display()))
                }
                _ => io(e),
            })
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => {
                BrowserFault::Refused(format!("{} already exists", path.display()))
            }
            _ => io(e),
        })
    }
}

fn create_new(path: &Path) -> Result<std::fs::File, BrowserFault> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW).mode(0o600);
    }
    o.open(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => {
            BrowserFault::Refused(format!("{} already exists", path.display()))
        }
        _ => io(e),
    })
}

/// Most messages buffered from the browser before the reader waits (the browser then waits too).
const BUFFERED: usize = 4096;
/// Most messages queued for the browser; a full queue means it stopped reading.
const QUEUED: usize = 256;
/// How long one write to the browser may block before the browser counts as no longer reading
/// its pipe.
pub const WRITE_STALL: Duration = Duration::from_secs(5);

/// A DevTools pipe: NUL-terminated JSON messages over the browser's private control channel. A
/// reader thread splits incoming messages into a bounded queue; it ends with the channel. A
/// writer thread drains a bounded outgoing queue, so a send never blocks: a browser that stops
/// reading (a full queue, or one write blocked past the stall bound) closes the transport, and
/// the session is then torn down through its process owner. Closing never waits for the writer.
pub struct PipeTransport {
    outgoing: Option<mpsc::SyncSender<Vec<u8>>>,
    writer: Arc<WriterState>,
    stall: Duration,
    incoming: mpsc::Receiver<Result<String, String>>,
    closed: bool,
}

/// What the writer thread reports: since when its current write blocks, and why it ended.
#[derive(Default)]
struct WriterState {
    busy_since: Mutex<Option<Instant>>,
    failed: Mutex<Option<String>>,
}

impl PipeTransport {
    pub fn new(writer: Box<dyn Write + Send>, reader: Box<dyn Read + Send>) -> Self {
        Self::with_stall_bound(writer, reader, WRITE_STALL)
    }

    /// [`Self::new`] with an explicit write stall bound.
    pub fn with_stall_bound(
        writer: Box<dyn Write + Send>,
        reader: Box<dyn Read + Send>,
        stall: Duration,
    ) -> Self {
        let (tx, incoming) = mpsc::sync_channel(BUFFERED);
        std::thread::Builder::new()
            .name("devtools-pipe".into())
            .spawn(move || read_messages(reader, tx))
            .ok();
        let (outgoing, queued) = mpsc::sync_channel(QUEUED);
        let state = Arc::new(WriterState::default());
        let st = state.clone();
        let spawned = std::thread::Builder::new()
            .name("devtools-pipe-writer".into())
            .spawn(move || write_messages(writer, queued, &st));
        if let Err(e) = spawned {
            *lock(&state.failed) = Some(format!("the DevTools pipe writer did not start: {e}"));
        }
        Self {
            outgoing: Some(outgoing),
            writer: state,
            stall,
            incoming,
            closed: false,
        }
    }

    /// Why the browser no longer takes messages, if it does not: the writer failed, or its
    /// current write has blocked past the stall bound.
    fn stalled(&self) -> Option<String> {
        if let Some(why) = lock(&self.writer.failed).clone() {
            return Some(why);
        }
        lock(&self.writer.busy_since)
            .filter(|t| t.elapsed() >= self.stall)
            .map(|_| "the browser stopped reading its DevTools pipe".to_owned())
    }

    fn fail(&mut self, why: String) -> BrowserFault {
        self.closed = true;
        self.outgoing = None;
        BrowserFault::Closed(why)
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Writes queued messages until the queue closes or a write fails. Dropping the writer closes
/// the host's write end, which tells the browser the session is over.
fn write_messages(
    mut writer: Box<dyn Write + Send>,
    queued: mpsc::Receiver<Vec<u8>>,
    state: &WriterState,
) {
    for message in queued {
        *lock(&state.busy_since) = Some(Instant::now());
        let written = writer.write_all(&message).and_then(|()| writer.flush());
        *lock(&state.busy_since) = None;
        if let Err(e) = written {
            *lock(&state.failed) = Some(format!("DevTools pipe: {e}"));
            return;
        }
    }
}

/// Reads NUL-terminated messages until the channel closes or a message exceeds its bound.
fn read_messages(reader: Box<dyn Read + Send>, tx: mpsc::SyncSender<Result<String, String>>) {
    let mut reader = BufReader::with_capacity(1 << 16, reader);
    loop {
        let mut message = Vec::new();
        let mut limited = (&mut reader).take(MESSAGE_LIMIT as u64 + 1);
        match limited.read_until(0, &mut message) {
            Ok(0) => {
                let _ = tx.send(Err("the browser closed its DevTools pipe".into()));
                return;
            }
            Ok(_) if message.last() == Some(&0) => {
                message.pop();
                let text = match String::from_utf8(message) {
                    Ok(t) => Ok(t),
                    Err(_) => Err("the browser sent a message that is not UTF-8".to_owned()),
                };
                let failed = text.is_err();
                if tx.send(text).is_err() || failed {
                    return;
                }
            }
            Ok(_) => {
                let why = if message.len() > MESSAGE_LIMIT {
                    "a DevTools message exceeded its size bound"
                } else {
                    "the browser closed its DevTools pipe mid-message"
                };
                let _ = tx.send(Err(why.into()));
                return;
            }
            Err(e) => {
                let _ = tx.send(Err(format!("DevTools pipe: {e}")));
                return;
            }
        }
    }
}

impl CdpTransport for PipeTransport {
    /// Queues one message; it never waits for the browser.
    fn send(&mut self, message: &str) -> Result<(), BrowserFault> {
        if self.closed {
            return Err(BrowserFault::Closed("the DevTools pipe is closed".into()));
        }
        if message.as_bytes().contains(&0) {
            return Err(BrowserFault::Protocol("a message contains NUL".into()));
        }
        if let Some(why) = self.stalled() {
            return Err(self.fail(why));
        }
        let mut bytes = Vec::with_capacity(message.len() + 1);
        bytes.extend_from_slice(message.as_bytes());
        bytes.push(0);
        let queued = match &self.outgoing {
            Some(q) => q.try_send(bytes),
            None => return Err(self.fail("the DevTools pipe is closed".into())),
        };
        match queued {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(_)) => {
                Err(self.fail("the browser stopped reading its DevTools pipe".into()))
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                let why = self
                    .stalled()
                    .unwrap_or_else(|| "the DevTools pipe is closed".into());
                Err(self.fail(why))
            }
        }
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<String>, BrowserFault> {
        if self.closed {
            return Err(BrowserFault::Closed("the DevTools pipe is closed".into()));
        }
        // A browser that stopped reading is lost even while it still writes.
        if let Some(why) = self.stalled() {
            return Err(self.fail(why));
        }
        match self.incoming.recv_timeout(timeout) {
            Ok(Ok(m)) => Ok(Some(m)),
            Ok(Err(why)) => {
                self.closed = true;
                Err(BrowserFault::Closed(why))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.closed = true;
                Err(BrowserFault::Closed("the DevTools pipe is closed".into()))
            }
        }
    }

    fn close(&mut self) {
        self.closed = true;
        // The writer ends once its queue closes (dropping the host's write end tells the browser
        // the session is over); a write still blocked ends when the browser's tree is stopped.
        // Nothing here waits for it.
        self.outgoing = None;
    }
}
