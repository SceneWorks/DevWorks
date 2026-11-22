//! The LocalSystem broker is the only ordinary ledger writer and holds the
//! original Job handles. Clients can request admission only for themselves or
//! an installed Actions worker in their own process ancestry.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::ptr::null_mut;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_SUCCESS,
    GetLastError, LocalFree, SetLastError,
};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{IsWellKnownSid, OWNER_SECURITY_INFORMATION, WinLocalSystemSid};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_GENERIC_READ, FILE_WRITE_DATA, PIPE_ACCESS_DUPLEX,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_READMODE_MESSAGE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE, PeekNamedPipe, WaitNamedPipeW,
};

use windows_sys::Win32::System::Threading::CreateMutexW;

use super::native::{Job, Process, SecurityDescriptor, checked, free_bytes, owned, wide};
use super::{MachinePolicy, POLL, Reservation, Writers, failure};
use crate::disk_budget::Admission;

const PIPE: &str = r"\\.\pipe\SceneWorks.DevHost.DiskBudget";
const MUTEX: &str = r"Global\SceneWorks.DevHost.DiskBudget";
const LIMIT: usize = 4096;
const INSTANCES: u32 = 32;
const CONNECT_MS: u32 = 5000;

#[derive(Deserialize, Serialize)]
pub(super) enum Request {
    Local,
    Run(String),
    Runner(String),
    Grow(String),
    Status,
}

#[derive(Deserialize, Serialize)]
pub(super) enum Response {
    Granted(Reservation),
    Status {
        leases: usize,
        reserved_bytes: u64,
        free_bytes: u64,
    },
    Refused(String),
}

struct Lease {
    request: Reservation,
    job: Job,
}

struct Broker {
    policy: MachinePolicy,
    boot: String,
    jobs: Mutex<BTreeMap<String, Lease>>,
}

impl Broker {
    fn jobs(&self) -> io::Result<MutexGuard<'_, BTreeMap<String, Lease>>> {
        self.jobs
            .lock()
            .map_err(|_| failure("disk budget broker lost its live job map"))
    }

    fn request(&self, caller: Process, request: Request) -> io::Result<Response> {
        if matches!(request, Request::Status) {
            let ledger = self.policy.ledger();
            let leases = ledger.reservations()?;
            let bytes = leases
                .iter()
                .try_fold(0u64, |sum, r| sum.checked_add(r.bytes))
                .ok_or_else(|| failure("disk budget reservation total overflow"))?;
            return Ok(Response::Status {
                leases: leases.len(),
                reserved_bytes: bytes,
                free_bytes: free_bytes(&self.policy.anchor.volume)?,
            });
        }

        let owner = if matches!(request, Request::Runner(_)) {
            // The hook already exists when we attach the worker. No build writer
            // may run from that shell or before the host's admission boundary.
            caller.runner_ancestor(&self.policy.worker_images)?
        } else {
            caller
        };
        let (reservation, newly_attached) = self.select_job(&owner, request)?;
        self.admit(reservation, newly_attached)
    }

    fn select_job(&self, process: &Process, request: Request) -> io::Result<(Reservation, bool)> {
        let mut jobs = self.jobs()?;
        for lease in jobs.values() {
            if !lease.job.contains(process)? {
                continue;
            }
            let profile = match &request {
                Request::Local => return Ok((lease.request.clone(), false)),
                Request::Run(profile) | Request::Runner(profile) | Request::Grow(profile) => {
                    profile
                }
                Request::Status => unreachable!(),
            };
            return Ok((
                self.policy
                    .request(&self.boot, lease.request.owner.clone(), profile)?,
                false,
            ));
        }

        let profile = match &request {
            Request::Grow(_) => return Err(failure("disk budget top-up requires an admitted Job")),
            Request::Local => "local",
            Request::Run(profile) | Request::Runner(profile) => profile,
            Request::Status => unreachable!(),
        };
        let reservation = self
            .policy
            .request(&self.boot, process.owner.clone(), profile)?;
        let floor = if matches!(&request, Request::Runner(_)) {
            "runner"
        } else {
            "local"
        };
        if reservation.bytes < self.policy.profile(floor)? {
            return Err(failure(format!(
                "disk budget profile is smaller than the {floor} baseline"
            )));
        }
        // An earlier broker in this boot may have admitted the same still-live
        // owner. A new (possibly nested) Job cannot prove its earlier scope drained.
        if self
            .policy
            .ledger()
            .reservations()?
            .iter()
            .any(|lease| lease.id == reservation.id)
        {
            return Err(failure(
                "disk budget owner already has an uninspectable reservation",
            ));
        }
        let job = Job::create()?;
        job.assign(process)?;
        jobs.insert(
            reservation.id.clone(),
            Lease {
                request: reservation.clone(),
                job,
            },
        );
        Ok((reservation, true))
    }

    fn admit(&self, request: Reservation, newly_attached: bool) -> io::Result<Response> {
        let deadline = Instant::now() + self.policy.wait();
        loop {
            {
                let jobs = self.jobs()?;
                let lease = jobs
                    .get(&request.id)
                    .ok_or_else(|| failure("disk budget owner exited during admission"))?;
                if lease.job.writers() != Writers::Active {
                    return Err(failure("disk budget cannot verify a live job"));
                }
                match self
                    .policy
                    .ledger()
                    .reserve(request.clone(), || free_bytes(&self.policy.anchor.volume))?
                {
                    Admission::Granted(grant) => {
                        if newly_attached {
                            eprintln!(
                                "dev-host disk budget reserved {} bytes for PID {}",
                                grant.bytes, grant.owner.pid
                            );
                        }
                        return Ok(Response::Granted(grant));
                    }
                    Admission::Wait {
                        available_bytes,
                        required_bytes,
                    } => {
                        if Instant::now() >= deadline {
                            return Err(failure(format!(
                                "disk budget wait expired with {available_bytes} bytes free and {required_bytes} bytes required"
                            )));
                        }
                    }
                }
            }
            std::thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
        }
    }

    fn reap(&self) -> io::Result<()> {
        let mut jobs = self.jobs()?;
        let ledger = self.policy.ledger();
        jobs.retain(|id, lease| {
            if lease.job.writers() != Writers::Stopped {
                return true;
            }
            match ledger.release(id, &lease.request.owner, |_| lease.job.writers()) {
                Ok(_) => false,
                Err(error) => {
                    eprintln!(
                        "dev-host disk budget could not release PID {}: {error}",
                        lease.request.owner.pid
                    );
                    true
                }
            }
        });
        Ok(())
    }
}

fn exclusive_server() -> io::Result<OwnedHandle> {
    let name = wide(OsStr::new(MUTEX));
    let security = SecurityDescriptor::new("O:SYD:P(A;;GA;;;SY)")?;
    // SAFETY: the name is terminated and the security attributes are valid.
    let (handle, status) = unsafe {
        SetLastError(0);
        let handle = CreateMutexW(&security.attributes(), 0, name.as_ptr());
        (handle, GetLastError())
    };
    let handle = owned(handle)?;
    if status == ERROR_ALREADY_EXISTS {
        return Err(failure("Windows disk budget broker is already running"));
    }
    Ok(handle)
}

fn listener(first: bool) -> io::Result<File> {
    let name = wide(OsStr::new(PIPE));
    // FILE_GENERIC_READ plus FILE_WRITE_DATA, deliberately excluding
    // FILE_CREATE_PIPE_INSTANCE. Only LocalSystem can own the listener.
    let security = SecurityDescriptor::new("O:SYD:P(A;;GA;;;SY)(A;;0x12008b;;;AU)")?;
    let flags = PIPE_ACCESS_DUPLEX
        | if first {
            FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            0
        };
    // SAFETY: name and security attributes are valid for this call.
    let pipe = owned(unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            flags,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_REJECT_REMOTE_CLIENTS,
            INSTANCES,
            LIMIT as u32,
            LIMIT as u32,
            CONNECT_MS,
            &security.attributes(),
        )
    })?;
    Ok(File::from(pipe))
}

fn accept(pipe: &File) -> io::Result<()> {
    // SAFETY: pipe is a synchronous server instance and needs no OVERLAPPED.
    match checked(unsafe { ConnectNamedPipe(pipe.as_raw_handle(), null_mut()) }) {
        Err(error) if error.raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32) => Ok(()),
        result => result,
    }
}

fn serve_peer(broker: &Broker, mut pipe: File) -> io::Result<()> {
    let handle = pipe.as_raw_handle();
    let mut pid = 0;
    // SAFETY: handle is a connected server and pid is writable.
    checked(unsafe { GetNamedPipeClientProcessId(handle, &mut pid) })?;
    // Pin the PID before reading anything supplied by the client, so an already
    // exited caller cannot be replaced by a later process in the same request.
    let peer = Process::open(pid, true)?;
    let deadline = Instant::now() + std::time::Duration::from_millis(u64::from(CONNECT_MS));
    loop {
        let mut available = 0;
        // SAFETY: only the byte count is requested.
        checked(unsafe {
            PeekNamedPipe(
                handle,
                null_mut(),
                0,
                null_mut(),
                &mut available,
                null_mut(),
            )
        })?;
        if available > 0 {
            break;
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                ErrorKind::TimedOut,
                "disk budget request did not arrive",
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let mut bytes = [0; LIMIT];
    let length = pipe.read(&mut bytes)?;
    let request: Request = serde_json::from_slice(&bytes[..length]).map_err(io::Error::other)?;
    let response = match broker.request(peer, request) {
        Ok(response) => response,
        Err(error) => Response::Refused(error.to_string()),
    };
    pipe.write_all(&serde_json::to_vec(&response).map_err(io::Error::other)?)
}

pub(super) fn serve(policy: MachinePolicy) -> io::Result<()> {
    let _exclusive = exclusive_server()?;
    let mut pipe = listener(true)?;
    let boot = super::boot_id()?;
    policy.ledger().reconcile(|lease| {
        if policy.previously_booted(&boot, lease) {
            Writers::Stopped
        } else {
            Writers::Unknown
        }
    })?;
    let broker = Arc::new(Broker {
        policy,
        boot,
        jobs: Mutex::default(),
    });
    let reaper = Arc::clone(&broker);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(POLL);
            if let Err(error) = reaper.reap() {
                eprintln!("dev-host disk budget reap failed: {error}");
            }
        }
    });
    loop {
        match accept(&pipe) {
            Ok(()) => {
                let broker = Arc::clone(&broker);
                std::thread::spawn(move || {
                    if let Err(error) = serve_peer(&broker, pipe) {
                        eprintln!("dev-host disk budget client failed: {error}");
                    }
                });
            }
            Err(error) if error.raw_os_error() == Some(ERROR_NO_DATA as i32) => drop(pipe),
            Err(error) => return Err(error),
        }
        // Normally another server instance exists while this one is created.
        // Leave the active instances intact when the instance cap is reached.
        loop {
            match listener(false) {
                Ok(next) => {
                    pipe = next;
                    break;
                }
                Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(error) => return Err(error),
            }
        }
    }
}

fn expect_system_owner(pipe: &File) -> io::Result<()> {
    let mut descriptor = null_mut();
    let mut owner = null_mut();
    // SAFETY: both outputs are writable. Windows allocates the descriptor.
    let status = unsafe {
        GetSecurityInfo(
            pipe.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    // SAFETY: owner points into the descriptor until LocalFree.
    let system = !owner.is_null() && unsafe { IsWellKnownSid(owner, WinLocalSystemSid) } != 0;
    unsafe { LocalFree(descriptor) };
    if !system {
        return Err(failure(
            "Windows disk budget pipe is not owned by LocalSystem",
        ));
    }
    Ok(())
}

pub(super) fn client(request: Request) -> io::Result<Response> {
    let name = wide(OsStr::new(PIPE));
    let open = || {
        OpenOptions::new()
            .access_mode(FILE_GENERIC_READ | FILE_WRITE_DATA)
            .security_qos_flags(SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION)
            .open(PIPE)
    };
    let mut pipe = match open() {
        Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
            // SAFETY: name is NUL-terminated.
            checked(unsafe { WaitNamedPipeW(name.as_ptr(), CONNECT_MS) })?;
            open()?
        }
        other => other?,
    };
    expect_system_owner(&pipe)?;
    pipe.write_all(&serde_json::to_vec(&request).map_err(io::Error::other)?)?;
    let mut reply = Vec::new();
    // Stop at the protocol limit if the peer returns malformed data.
    pipe.take(LIMIT as u64).read_to_end(&mut reply)?;
    let response = serde_json::from_slice(&reply).map_err(io::Error::other)?;
    match response {
        Response::Refused(error) => Err(failure(error)),
        other => Ok(other),
    }
}
