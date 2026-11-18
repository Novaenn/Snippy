//! Same-user bounded control protocol and private verification metadata.
mod wire;
use std::{
    env, fmt,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{Read, Write},
    os::unix::{
        fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    time::Duration,
};
pub use wire::{
    ConfigView, MAX_FRAME, MAX_REPLY, ManagerInfo, Prompt, Reply, Request, State, VERSION,
    VaultState, Verification,
};
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Error {
    Unavailable = 1,
    UnsafePath,
    AlreadyRunning,
    Protocol,
    Version,
    Unauthorized,
    Busy,
    Stale,
    Canceled,
    WrongPassword,
    PassMismatch,
    InvalidPass,
    Storage,
    Gui,
    Io,
    AlreadyOpen,
    AlreadyExists,
    X11,
    SelectionChanged,
    Unsupported,
    TooLarge,
    TransferTimeout,
    InvalidContent,
    Quota,
    DiskFull,
    InvalidLabel,
    Configuration,
    InvalidMetadata,
    NotFound,
    Autostart,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unavailable => "service unavailable; start 'snippy daemon' first",
            Self::UnsafePath => {
                "application paths must be absolute, private, user-owned, and not symlinks"
            }
            Self::AlreadyRunning => "a service is already running; existing socket was preserved",
            Self::Protocol => "invalid or oversized IPC message",
            Self::Version => "incompatible IPC version; restart matching service and GUI binaries",
            Self::Unauthorized => "IPC peer does not belong to the current user",
            Self::Busy => "service is busy; retry after the pending operation",
            Self::Stale => "vault session changed; refresh and retry",
            Self::Canceled => "operation canceled; no pending save will be committed",
            Self::WrongPassword => "could not unlock: incorrect passphrase or damaged vault",
            Self::PassMismatch => "passphrase confirmation does not match",
            Self::InvalidPass => {
                "passphrase must be nonempty, at most 1024 bytes, and contain no NUL"
            }
            Self::Storage => "vault operation failed; existing vault was not reset",
            Self::Gui => "could not launch GUI; build snippy-gui beside the snippy executable",
            Self::Io => "service filesystem or transport operation failed",
            Self::AlreadyOpen => "existing vault window was presented",
            Self::AlreadyExists => "vault already exists; unlock it instead",
            Self::SelectionChanged => "clipboard changed; invoke Save again for the current selection",
            Self::Unsupported => "clipboard is empty or offers no supported text/PNG format",
            Self::TooLarge => "clipboard exceeds the configured clip or image decoding limit",
            Self::TransferTimeout => "clipboard transfer timed out; invoke Save again",
            Self::InvalidContent => "clipboard content is empty, malformed, or has an invalid representation",
            Self::Quota => "vault quota reached; existing items were preserved",
            Self::DiskFull => "storage is full; existing history was preserved",
            Self::InvalidLabel => "manual label must be at most 256 bytes with no control characters, and is allowed only for Unknown sources",
            Self::X11 => "a native X11 session with XFixes is required; Wayland is unsupported",
            Self::Configuration => {
                "settings file is invalid, unsafe, or unreadable; fix or remove it"
            }
            Self::InvalidMetadata => {
                "tag or settings metadata is invalid; check lengths, limits, and control characters"
            }
            Self::NotFound => "saved item no longer exists; refresh the manager",
            Self::Autostart => {
                "could not update the XDG autostart entry; check the config directory permissions"
            }
        })
    }
}
impl Error {
    pub fn from_code(code: u8) -> Option<Self> {
        const ERRORS: [Error; 30] = [
            Error::Unavailable,
            Error::UnsafePath,
            Error::AlreadyRunning,
            Error::Protocol,
            Error::Version,
            Error::Unauthorized,
            Error::Busy,
            Error::Stale,
            Error::Canceled,
            Error::WrongPassword,
            Error::PassMismatch,
            Error::InvalidPass,
            Error::Storage,
            Error::Gui,
            Error::Io,
            Error::AlreadyOpen,
            Error::AlreadyExists,
            Error::X11,
            Error::SelectionChanged,
            Error::Unsupported,
            Error::TooLarge,
            Error::TransferTimeout,
            Error::InvalidContent,
            Error::Quota,
            Error::DiskFull,
            Error::InvalidLabel,
            Error::Configuration,
            Error::InvalidMetadata,
            Error::NotFound,
            Error::Autostart,
        ];
        code.checked_sub(1)
            .and_then(|index| ERRORS.get(index as usize).copied())
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(_: std::io::Error) -> Self {
        Self::Io
    }
}
impl From<crate::storage::StorageError> for Error {
    fn from(value: crate::storage::StorageError) -> Self {
        use crate::storage::StorageError as S;
        match value {
            S::UnlockFailed => Self::WrongPassword,
            S::PassphraseMismatch => Self::PassMismatch,
            S::InvalidPassphrase => Self::InvalidPass,
            S::AlreadyExists => Self::AlreadyExists,
            S::UnsafePath => Self::UnsafePath,
            S::ClipTooLarge => Self::TooLarge,
            S::QuotaExceeded => Self::Quota,
            S::DiskFull => Self::DiskFull,
            S::InvalidContent => Self::InvalidContent,
            S::InvalidMetadata => Self::InvalidMetadata,
            S::NotFound => Self::NotFound,
            S::Canceled => Self::Canceled,
            _ => Self::Storage,
        }
    }
}

#[derive(Clone)]
pub struct Paths {
    pub runtime: PathBuf,
    pub vault: PathBuf,
}
impl Paths {
    pub fn discover() -> Result<Self, Error> {
        fn absolute(p: PathBuf) -> Result<PathBuf, Error> {
            if p.is_absolute() {
                Ok(p)
            } else {
                Err(Error::UnsafePath)
            }
        }
        let runtime = match env::var_os("XDG_RUNTIME_DIR").filter(|p| !p.is_empty()) {
            Some(root) => absolute(root.into())?.join("snippy"),
            None => PathBuf::from(format!(
                "/tmp/snippy-{}",
                rustix::process::geteuid().as_raw()
            )),
        };
        let data = match env::var_os("XDG_DATA_HOME").filter(|p| !p.is_empty()) {
            Some(root) => absolute(root.into())?,
            None => {
                absolute(env::var_os("HOME").ok_or(Error::UnsafePath)?.into())?.join(".local/share")
            }
        };
        Ok(Self {
            runtime,
            vault: data.join("snippy"),
        })
    }
    pub fn socket(&self) -> PathBuf {
        self.runtime.join("service.sock")
    }
    pub fn connect(&self) -> Result<UnixStream, Error> {
        private(&self.runtime, true).map_err(|_| Error::Unavailable)?;
        let m = private(&self.socket(), false).map_err(|_| Error::Unavailable)?;
        if !m.file_type().is_socket() {
            return Err(Error::UnsafePath);
        }
        let stream = UnixStream::connect(self.socket()).map_err(|_| Error::Unavailable)?;
        verify_peer(&stream)?;
        stream.set_read_timeout(Some(Duration::from_secs(40)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        Ok(stream)
    }
}
pub(crate) fn private(path: &Path, directory: bool) -> Result<fs::Metadata, Error> {
    let m = fs::symlink_metadata(path)?;
    if m.uid() != rustix::process::geteuid().as_raw()
        || m.mode() & 0o077 != 0
        || m.file_type().is_symlink()
        || (directory && !m.is_dir())
    {
        return Err(Error::UnsafePath);
    }
    Ok(m)
}
pub(crate) fn make_private(path: &Path) -> Result<(), Error> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(_) => return Err(Error::Io),
    }
    private(path, true)?;
    Ok(())
}
pub(crate) fn process_lock(path: &Path) -> Result<File, Error> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)?;
    let m = file.metadata()?;
    if !m.is_file() || m.uid() != rustix::process::geteuid().as_raw() || m.mode() & 0o077 != 0 {
        return Err(Error::UnsafePath);
    }
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .map_err(|_| Error::AlreadyRunning)?;
    Ok(file)
}
pub fn verify_peer(stream: &UnixStream) -> Result<(), Error> {
    let uid = rustix::net::sockopt::socket_peercred(stream)
        .map_err(|_| Error::Unauthorized)?
        .uid;
    check_uid(uid.as_raw(), rustix::process::geteuid().as_raw())
}
pub(crate) fn check_uid(peer: u32, own: u32) -> Result<(), Error> {
    if peer == own {
        Ok(())
    } else {
        Err(Error::Unauthorized)
    }
}
pub fn call(paths: &Paths, request: Request) -> Result<State, Error> {
    exchange(&mut paths.connect()?, request)
}
pub fn exchange(stream: &mut UnixStream, request: Request) -> Result<State, Error> {
    match exchange_reply(stream, request)? {
        Reply::State(s) => Ok(s),
        Reply::Error(e) => Err(e),
        _ => Err(Error::Protocol),
    }
}
pub fn exchange_reply(stream: &mut UnixStream, request: Request) -> Result<Reply, Error> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let frame = request.encode(id)?;
    drop(request);
    stream.write_all(&frame)?;
    drop(frame);
    match read_reply(stream)? {
        (got, Reply::Error(e)) if got == id => Err(e),
        (got, reply) if got == id => Ok(reply),
        _ => Err(Error::Protocol),
    }
}
pub fn subscribe(paths: &Paths) -> Result<(UnixStream, State), Error> {
    let mut stream = paths.connect()?;
    let state = exchange(&mut stream, Request::Watch)?;
    stream.set_read_timeout(None)?;
    Ok((stream, state))
}
pub fn read_reply(stream: &mut UnixStream) -> Result<(u64, Reply), Error> {
    let mut header = [0; 4];
    stream.read_exact(&mut header)?;
    let size = u32::from_be_bytes(header) as usize;
    if !(11..=MAX_REPLY).contains(&size) {
        return Err(Error::Protocol);
    }
    let mut body = Zeroizing::new(vec![0; size]);
    stream.read_exact(&mut body)?;
    Reply::decode(&body)
}
