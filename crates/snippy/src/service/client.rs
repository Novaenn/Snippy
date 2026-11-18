use crate::ipc::{Error, MAX_FRAME, MAX_REPLY, Reply, Request};
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    time::{Duration, Instant},
};
use zeroize::{Zeroize, Zeroizing};

pub(super) struct Client {
    pub id: u64,
    pub pid: Option<u32>,
    pub stream: UnixStream,
    pub watch: bool,
    pub closed: bool,
    pub pending: bool,
    pub request_id: u64,
    input: Zeroizing<Vec<u8>>,
    output: Zeroizing<Vec<u8>>,
    written: usize,
    private_output: bool,
    read_done: bool,
    pub deadline: Option<Instant>,
}
impl Client {
    pub fn new(id: u64, stream: UnixStream) -> Result<Self, Error> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            id,
            pid: rustix::net::sockopt::socket_peercred(&stream)
                .ok()
                .map(|cred| cred.pid)
                .map(|pid| pid.as_raw_nonzero().get() as u32),
            stream,
            watch: false,
            closed: false,
            pending: false,
            request_id: 0,
            // Full request capacity up front: growth would free unzeroed copies.
            input: Zeroizing::new(Vec::with_capacity(MAX_FRAME + 4)),
            output: Zeroizing::new(Vec::new()),
            written: 0,
            private_output: false,
            read_done: false,
            deadline: Some(Instant::now() + Duration::from_secs(2)),
        })
    }
    pub fn wants_write(&self) -> bool {
        self.written < self.output.len()
    }
    pub fn queue(&mut self, id: u64, reply: Reply) {
        let private = matches!(
            reply,
            Reply::Prompt(_)
                | Reply::Verification(_)
                | Reply::Page(_)
                | Reply::Image(_)
                | Reply::Config(_)
                | Reply::SensitiveApps(_)
                | Reply::Tags { .. }
                | Reply::Deleted { .. }
                | Reply::ManagerInfo(_)
        );
        let frame = reply.encode(id);
        if self.output.len() + frame.len()
            > if self.watch {
                MAX_FRAME * 4
            } else {
                MAX_REPLY + MAX_FRAME * 4
            }
        {
            self.closed = true;
            return;
        }
        self.private_output |= private;
        let needed = self.output.len() + frame.len();
        if needed > self.output.capacity() {
            // Move into one larger zeroizing buffer; the old one is wiped on drop.
            let mut output = Zeroizing::new(Vec::with_capacity(needed));
            output.extend_from_slice(&self.output);
            self.output = output;
        }
        self.output.extend_from_slice(&frame);
        self.deadline = Some(Instant::now() + Duration::from_secs(2));
    }
    pub fn clear_private(&mut self) {
        if self.private_output {
            if self.written != 0 {
                // A partially written frame cannot be removed without corrupting
                // framing. Disconnect and let the GUI clear its private view.
                self.closed = true;
                let _ = self.stream.shutdown(std::net::Shutdown::Both);
            }
            self.output.zeroize();
            self.written = 0;
            self.private_output = false;
            // A one-shot client whose answer was withdrawn has nothing left.
            if !self.watch && !self.pending {
                self.closed = true;
                let _ = self.stream.shutdown(std::net::Shutdown::Both);
            }
        }
    }
    pub fn read(&mut self) -> Result<Option<Request>, Error> {
        let mut bytes = Zeroizing::new([0; 1024]);
        loop {
            match self.stream.read(&mut *bytes) {
                Ok(0) => {
                    self.closed = true;
                    return Ok(None);
                }
                Ok(n) => {
                    if self.read_done || self.input.len() + n > MAX_FRAME + 4 {
                        return Err(Error::Protocol);
                    }
                    self.input.extend_from_slice(&bytes[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.closed = true;
                    return Ok(None);
                }
            }
        }
        if self.input.len() < 4 {
            return Ok(None);
        }
        let size =
            u32::from_be_bytes(self.input[..4].try_into().map_err(|_| Error::Protocol)?) as usize;
        if !(11..=MAX_FRAME).contains(&size) {
            return Err(Error::Protocol);
        }
        if self.input.len() < size + 4 {
            return Ok(None);
        }
        if self.input.len() != size + 4 {
            return Err(Error::Protocol);
        }
        // Keep a usable ID for the incompatible-version error without reflecting data.
        self.request_id =
            u64::from_be_bytes(self.input[6..14].try_into().map_err(|_| Error::Protocol)?);
        let decoded = Request::decode(&self.input[4..]);
        self.input.zeroize();
        self.read_done = true;
        let (id, request) = decoded?;
        self.request_id = id;
        self.deadline = Some(Instant::now() + Duration::from_secs(30));
        Ok(Some(request))
    }
    pub fn flush(&mut self) {
        while self.written < self.output.len() {
            match self.stream.write(&self.output[self.written..]) {
                Ok(0) => {
                    self.closed = true;
                    return;
                }
                Ok(n) => self.written += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.closed = true;
                    return;
                }
            }
        }
        self.output.zeroize();
        self.written = 0;
        self.private_output = false;
        if self.watch {
            self.deadline = None;
        }
        if !self.watch && !self.pending {
            self.closed = true
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn view() -> Reply {
        Reply::SensitiveApps(vec!["synthetic.app".into()])
    }
    #[test]
    fn withdrawn_private_reply_closes_one_shot_clients_but_keeps_the_gui() {
        let (_peer, server) = UnixStream::pair().unwrap();
        let mut client = Client::new(1, server).unwrap();
        assert!(client.input.capacity() >= MAX_FRAME + 4);
        client.queue(9, view());
        client.clear_private();
        assert!(client.closed && !client.wants_write());
        let (_peer, server) = UnixStream::pair().unwrap();
        let mut gui = Client::new(2, server).unwrap();
        gui.watch = true;
        gui.queue(0, view());
        gui.clear_private();
        assert!(!gui.closed && !gui.wants_write());
    }
    #[test]
    fn queued_output_grows_into_one_buffer() {
        let (_peer, server) = UnixStream::pair().unwrap();
        let mut client = Client::new(1, server).unwrap();
        let mut total = 0;
        for id in 1..=20 {
            let frame = view().encode(id);
            total += frame.len();
            client.queue(id, view());
        }
        assert_eq!(client.output.len(), total);
        assert!(!client.closed);
    }
}
