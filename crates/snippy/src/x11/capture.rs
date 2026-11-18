//! Explicitly authorized, bounded selection transfers on a separate connection.
//! The metadata observer never issues ConvertSelection, including TARGETS.
use super::Snapshot;
use crate::ipc::Error;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use snippy_core::config::CaptureLimits;
use std::{
    io::Write,
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread::{self, JoinHandle},
    time::Instant,
};
use x11rb::{
    connection::Connection,
    protocol::{
        Event,
        xfixes::{ConnectionExt as _, SelectionEventMask},
        xproto::{AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, Property, WindowClass},
    },
    rust_connection::RustConnection,
};
use zeroize::Zeroizing;

pub struct Received {
    pub png: bool,
    pub latin1: bool,
    pub bytes: Zeroizing<Vec<u8>>,
}
pub struct Transfer {
    pub wake: UnixStream,
    cancel: UnixStream,
    pub result: Receiver<Result<Received, Error>>,
    join: Option<JoinHandle<()>>,
}
impl Transfer {
    pub fn start(
        snapshot: Snapshot,
        limits: CaptureLimits,
        revoked: Arc<AtomicBool>,
    ) -> Result<Self, Error> {
        let (wake, mut writer) = UnixStream::pair()?;
        let (cancel_read, cancel) = UnixStream::pair()?;
        let (send, result) = mpsc::sync_channel(1);
        let join = thread::spawn(move || {
            let result = Reader::connect(snapshot, limits, revoked, cancel_read)
                .and_then(|mut r| r.receive());
            let _ = send.send(result);
            let _ = writer.write(&[1]);
        });
        Ok(Self {
            wake,
            cancel,
            result,
            join: Some(join),
        })
    }
    pub fn cancel(&mut self) {
        let _ = self.cancel.write(&[1]);
    }
    pub fn finish(&mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
impl Drop for Transfer {
    fn drop(&mut self) {
        self.cancel();
        self.finish();
    }
}

struct Reader {
    connection: RustConnection,
    window: u32,
    clipboard: u32,
    property: u32,
    incr: u32,
    snapshot: Snapshot,
    limits: CaptureLimits,
    revoked: Arc<AtomicBool>,
    cancel: UnixStream,
    started: Instant,
    progress: Instant,
}
impl Reader {
    fn connect(
        snapshot: Snapshot,
        limits: CaptureLimits,
        revoked: Arc<AtomicBool>,
        cancel: UnixStream,
    ) -> Result<Self, Error> {
        if revoked.load(Ordering::SeqCst) {
            return Err(Error::Canceled);
        }
        let (connection, screen) = x11rb::connect(None).map_err(|_| Error::X11)?;
        let window = connection.generate_id().map_err(|_| Error::X11)?;
        connection
            .create_window(
                0,
                window,
                connection.setup().roots[screen].root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )
            .map_err(|_| Error::X11)?
            .check()
            .map_err(|_| Error::X11)?;
        connection
            .xfixes_query_version(5, 0)
            .map_err(|_| Error::X11)?
            .reply()
            .map_err(|_| Error::X11)?;
        let clipboard = atom(&connection, b"CLIPBOARD")?;
        let property = atom(&connection, b"_SNIPPY_CAPTURE")?;
        let incr = atom(&connection, b"INCR")?;
        connection
            .xfixes_select_selection_input(
                window,
                clipboard,
                SelectionEventMask::SET_SELECTION_OWNER
                    | SelectionEventMask::SELECTION_WINDOW_DESTROY
                    | SelectionEventMask::SELECTION_CLIENT_CLOSE,
            )
            .map_err(|_| Error::X11)?
            .check()
            .map_err(|_| Error::X11)?;
        let now = Instant::now();
        let mut reader = Self {
            connection,
            window,
            clipboard,
            property,
            incr,
            snapshot,
            limits,
            revoked,
            cancel,
            started: now,
            progress: now,
        };
        reader.validate()?;
        Ok(reader)
    }
    fn valid(&self) -> Result<(), Error> {
        if self.revoked.load(Ordering::SeqCst) {
            return Err(Error::Canceled);
        }
        let now = Instant::now();
        if now.duration_since(self.started) >= self.limits.transfer_total_timeout
            || now.duration_since(self.progress) >= self.limits.transfer_idle_timeout
        {
            return Err(Error::TransferTimeout);
        }
        Ok(())
    }
    fn ownership(&self, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::XfixesSelectionNotify(e) if e.response_type & 0x80 == 0 && e.window == self.window && e.selection == self.clipboard)
        {
            return Err(Error::SelectionChanged);
        }
        Ok(())
    }
    fn validate(&mut self) -> Result<(), Error> {
        self.valid()?;
        let owner = self
            .connection
            .get_selection_owner(self.clipboard)
            .map_err(|_| Error::X11)?
            .reply()
            .map_err(|_| Error::X11)?
            .owner;
        if owner == 0 || owner != self.snapshot.owner {
            return Err(Error::SelectionChanged);
        }
        // The round trip also brings all earlier XFixes notifications into the
        // connection queue, including a same-window SetSelectionOwner.
        while let Some(event) = self.connection.poll_for_event().map_err(|_| Error::X11)? {
            self.ownership(&event)?;
        }
        self.valid()
    }
    fn event(&mut self) -> Result<Event, Error> {
        loop {
            self.valid()?;
            if let Some(event) = self.connection.poll_for_event().map_err(|_| Error::X11)? {
                self.ownership(&event)?;
                return Ok(event);
            }
            use std::os::fd::AsFd;
            let mut fds = [
                PollFd::from_borrowed_fd(self.connection.stream().as_fd(), PollFlags::IN),
                PollFd::new(&self.cancel, PollFlags::IN),
            ];
            let deadline = (self.started + self.limits.transfer_total_timeout)
                .min(self.progress + self.limits.transfer_idle_timeout);
            let remaining = deadline.saturating_duration_since(Instant::now());
            let timeout = Timespec {
                tv_sec: remaining.as_secs().min(i64::MAX as u64) as i64,
                tv_nsec: remaining.subsec_nanos() as i64,
            };
            match poll(&mut fds, Some(&timeout)) {
                Ok(_) => (),
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => return Err(Error::X11),
            }
            if !fds[1].revents().is_empty() {
                return Err(Error::Canceled);
            }
            if fds[0].revents().intersects(PollFlags::HUP | PollFlags::ERR) {
                return Err(Error::X11);
            }
        }
    }
    fn receive(&mut self) -> Result<Received, Error> {
        let targets = atom(&self.connection, b"TARGETS")?;
        let png = atom(&self.connection, b"image/png")?;
        let utf8 = atom(&self.connection, b"UTF8_STRING")?;
        let utf8_mime = atom(&self.connection, b"text/plain;charset=utf-8")?;
        let plain = atom(&self.connection, b"text/plain")?;
        let (kind, format, bytes) = self.convert(targets, 16 * 1024)?;
        // Some owners label the TARGETS reply with type TARGETS instead of ATOM.
        if (kind != u32::from(AtomEnum::ATOM) && kind != targets)
            || format != 32
            || bytes.len() % 4 != 0
        {
            return Err(Error::InvalidContent);
        }
        // get_property serializes 32-bit values in the connection byte order.
        let offered = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| u32::from_ne_bytes(*b))
            .collect::<Vec<_>>();
        let target = [png, utf8, utf8_mime, plain, u32::from(AtomEnum::STRING)]
            .into_iter()
            .find(|t| offered.contains(t))
            .ok_or(Error::Unsupported)?;
        let cap = usize::try_from(self.limits.max_clip_bytes).map_err(|_| Error::TooLarge)?;
        self.validate()?;
        let (kind, format, bytes) = self.convert(target, cap)?;
        if kind != target || format != 8 || bytes.is_empty() {
            return Err(Error::InvalidContent);
        }
        self.validate()?;
        Ok(Received {
            png: target == png,
            latin1: target == u32::from(AtomEnum::STRING),
            bytes,
        })
    }
    fn convert(&mut self, target: u32, cap: usize) -> Result<(u32, u8, Zeroizing<Vec<u8>>), Error> {
        self.valid()?;
        self.connection
            .delete_property(self.window, self.property)
            .map_err(|_| Error::X11)?;
        self.connection
            .convert_selection(
                self.window,
                self.clipboard,
                target,
                self.property,
                self.snapshot
                    .selection_timestamp
                    .unwrap_or(x11rb::CURRENT_TIME),
            )
            .map_err(|_| Error::X11)?;
        self.connection.flush().map_err(|_| Error::X11)?;
        loop {
            if let Event::SelectionNotify(e) = self.event()?
                && e.requestor == self.window
                && e.selection == self.clipboard
                && e.target == target
            {
                if e.property == 0 {
                    return Err(Error::Unsupported);
                }
                if e.property != self.property {
                    return Err(Error::InvalidContent);
                }
                break;
            }
        }
        let (kind, format, bytes) = self.property(cap.max(4))?;
        self.progress = Instant::now();
        if kind != self.incr {
            return if bytes.len() <= cap {
                Ok((kind, format, bytes))
            } else {
                Err(Error::TooLarge)
            };
        }
        if format != 32 || bytes.len() != 4 {
            return Err(Error::InvalidContent);
        }
        let hint =
            u32::from_ne_bytes(bytes[..].try_into().map_err(|_| Error::InvalidContent)?) as usize;
        if hint > cap {
            return Err(Error::TooLarge);
        }
        let mut chunks = Chunks::new(cap, hint);
        loop {
            let event = self.event()?;
            if !matches!(event, Event::PropertyNotify(e) if e.window == self.window && e.atom == self.property && e.state == Property::NEW_VALUE && e.response_type & 0x80 == 0)
            {
                continue;
            }
            let (kind, format, bytes) = self.property(chunks.remaining())?;
            if kind == self.incr {
                return Err(Error::InvalidContent);
            }
            if chunks.push(kind, format, &bytes)? {
                return Ok((kind, format, chunks.bytes));
            }
            self.progress = Instant::now();
        }
    }
    fn property(&mut self, cap: usize) -> Result<(u32, u8, Zeroizing<Vec<u8>>), Error> {
        let mut result = Zeroizing::new(Vec::new());
        let mut offset = 0;
        let mut previous = None;
        loop {
            self.valid()?;
            let reply = self
                .connection
                .get_property(
                    true,
                    self.window,
                    self.property,
                    AtomEnum::ANY,
                    offset,
                    16 * 1024,
                )
                .map_err(|_| Error::X11)?
                .reply()
                .map_err(|_| Error::X11)?;
            let bytes = Zeroizing::new(reply.value);
            if previous.is_some_and(|p| p != (reply.type_, reply.format)) {
                return Err(Error::InvalidContent);
            }
            previous = Some((reply.type_, reply.format));
            if result
                .len()
                .checked_add(bytes.len())
                .and_then(|n| n.checked_add(reply.bytes_after as usize))
                .is_none_or(|n| n > cap)
            {
                return Err(Error::TooLarge);
            }
            append(&mut result, &bytes, reply.bytes_after as usize, cap);
            if !bytes.is_empty() {
                self.progress = Instant::now();
            }
            if reply.bytes_after == 0 {
                self.connection.flush().map_err(|_| Error::X11)?;
                return Ok((reply.type_, reply.format, result));
            }
            if bytes.is_empty() || bytes.len() % 4 != 0 {
                return Err(Error::InvalidContent);
            }
            offset += (bytes.len() / 4) as u32;
        }
    }
}
/// INCR chunks must keep one representation, respect the aggregate bound, and
/// end with a zero-length property. Transport and parser share this reducer.
struct Chunks {
    cap: usize,
    representation: Option<(u32, u8)>,
    bytes: Zeroizing<Vec<u8>>,
}
impl Chunks {
    fn new(cap: usize, hint: usize) -> Self {
        Self {
            cap,
            representation: None,
            bytes: Zeroizing::new(Vec::with_capacity(hint.min(cap))),
        }
    }
    fn remaining(&self) -> usize {
        self.cap - self.bytes.len()
    }
    fn push(&mut self, kind: u32, format: u8, bytes: &[u8]) -> Result<bool, Error> {
        if kind == 0
            || !matches!(format, 8 | 16 | 32)
            || self.representation.is_some_and(|p| p != (kind, format))
            || !bytes.len().is_multiple_of(format as usize / 8)
        {
            return Err(Error::InvalidContent);
        }
        if bytes.len() > self.remaining() {
            return Err(Error::TooLarge);
        }
        self.representation = Some((kind, format));
        append(&mut self.bytes, bytes, 0, self.cap);
        Ok(bytes.is_empty())
    }
}
/// Appends without letting `Vec` reallocate in place of us: a reallocation
/// would free the old payload copy without zeroizing it. Growing moves the
/// data into a new buffer and drops the old one through `Zeroizing`. The
/// caller has already checked that `buffer.len() + bytes.len() <= cap`.
fn append(buffer: &mut Zeroizing<Vec<u8>>, bytes: &[u8], expected: usize, cap: usize) {
    let needed = buffer.len() + bytes.len();
    if needed > buffer.capacity() {
        let capacity = needed
            .saturating_add(expected)
            .max(buffer.capacity().saturating_mul(2))
            .min(cap)
            .max(needed);
        let mut grown = Zeroizing::new(Vec::with_capacity(capacity));
        grown.extend_from_slice(buffer);
        *buffer = grown;
    }
    buffer.extend_from_slice(bytes);
}
fn atom(connection: &RustConnection, name: &[u8]) -> Result<u32, Error> {
    Ok(connection
        .intern_atom(false, name)
        .map_err(|_| Error::X11)?
        .reply()
        .map_err(|_| Error::X11)?
        .atom)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn incr_accepts_multiple_chunks_and_requires_a_consistent_terminator() {
        let mut chunks = Chunks::new(6, 0);
        assert_eq!(chunks.push(10, 8, b"abc"), Ok(false));
        assert_eq!(chunks.push(10, 8, b"def"), Ok(false));
        assert_eq!(chunks.push(11, 8, b""), Err(Error::InvalidContent));
        assert_eq!(chunks.push(10, 8, b""), Ok(true));
        assert_eq!(chunks.bytes.as_slice(), b"abcdef");
    }
    #[test]
    fn payload_buffers_grow_without_reallocating_in_place_and_stay_bounded() {
        let chunks = Chunks::new(64, 1 << 30);
        assert!((64..1 << 30).contains(&chunks.bytes.capacity()));
        let mut buffer = Zeroizing::new(Vec::new());
        append(&mut buffer, b"abcd", 12, 16);
        assert!(buffer.capacity() >= 16);
        let address = buffer.as_ptr();
        append(&mut buffer, b"efghijkl", 4, 16);
        assert_eq!(buffer.as_ptr(), address);
        append(&mut buffer, b"mnop", 0, 16);
        assert_eq!(buffer.as_slice(), b"abcdefghijklmnop");
        assert_eq!(buffer.as_ptr(), address);
        let mut small = Zeroizing::new(Vec::with_capacity(2));
        append(&mut small, b"ab", 0, 8);
        append(&mut small, b"c", 0, 8);
        assert_eq!(small.as_slice(), b"abc");
        assert!((3..=8).contains(&small.capacity()));
    }
    #[test]
    fn incr_rejects_aggregate_overflow_invalid_types_and_misaligned_data() {
        let mut chunks = Chunks::new(4, 0);
        chunks.push(10, 8, b"abc").unwrap();
        assert_eq!(chunks.push(10, 8, b"de"), Err(Error::TooLarge));
        assert_eq!(chunks.bytes.len(), 3);
        assert_eq!(chunks.push(10, 32, b""), Err(Error::InvalidContent));
        assert_eq!(
            Chunks::new(4, 0).push(0, 8, b""),
            Err(Error::InvalidContent)
        );
        assert_eq!(
            Chunks::new(4, 0).push(10, 32, b"a"),
            Err(Error::InvalidContent)
        );
        assert_eq!(
            Chunks::new(4, 0).push(10, 0, b""),
            Err(Error::InvalidContent)
        );
    }
}
