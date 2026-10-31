//! The URB phase of one imported device.
//!
//! A reader loop parses `CMD_SUBMIT` / `CMD_UNLINK` from the client. Each submit goes to a
//! per-endpoint worker thread, which keeps URBs on one endpoint in order while different
//! endpoints run concurrently (a blocked interrupt-IN must not stall control requests).
//!
//! Unlink rule, enforced under one lock: a URB is either answered by `RET_SUBMIT` or
//! cancelled by an unlink (`RET_UNLINK` status -ECONNRESET and *no* `RET_SUBMIT`), never both.
//! If the URB had already completed, `RET_UNLINK` carries status 0.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use usbip_proto::{
    errno, read_command, CmdSubmit, Command, IsoPacketDescriptor, ProtoError, Reply, RetSubmit, RetUnlink, DIR_OUT,
};

/// Result of a finished transfer. `data` is only meaningful for IN transfers.
pub struct Completed {
    pub data: Vec<u8>,
    pub actual_length: usize,
}

/// One isochronous URB. For OUT, `data` is the client's buffer, laid out by the packet offsets;
/// for IN it is empty.
pub struct IsoRequest {
    /// Endpoint address including the direction bit.
    pub endpoint: u8,
    pub data: Vec<u8>,
    pub packets: Vec<IsoPacketDescriptor>,
}

/// A finished isochronous URB. `packets` keep the client's offsets and lengths, with
/// `actual_length` and `status` filled in. For IN, `data` holds only the bytes that arrived,
/// packet after packet with no padding (the USB/IP wire layout).
pub struct IsoDone {
    pub data: Vec<u8>,
    pub packets: Vec<IsoPacketDescriptor>,
}

/// Everything the session needs from a real (or fake) USB device.
/// Errors are positive Linux errno values; the session negates them on the wire.
pub trait UsbBackend: Send + Sync + 'static {
    /// Control transfer on endpoint 0. `setup` is the raw 8-byte setup packet.
    fn control(&self, setup: [u8; 8], out_data: &[u8], cancel: &AtomicBool) -> Result<Completed, i32>;
    /// Bulk or interrupt transfer. `endpoint` includes the direction bit (0x80 = IN).
    fn transfer(&self, endpoint: u8, out_data: &[u8], in_len: usize, cancel: &AtomicBool) -> Result<Completed, i32>;
    fn clear_halt(&self, endpoint: u8) -> Result<(), i32>;
    fn set_interface(&self, interface: u8, alt: u8) -> Result<(), i32>;
    fn set_configuration(&self, config: u8) -> Result<(), i32>;
    fn reset(&self) -> Result<(), i32>;

    /// A transfer that has been handed to the host controller but has not finished yet.
    type IsoPending: Send + 'static;
    /// Start an isochronous transfer and return at once. Streaming devices (webcams, audio) need
    /// several of these queued back to back, so starting one must not wait for the previous.
    fn iso_submit(&self, req: IsoRequest) -> Result<Self::IsoPending, i32>;
    /// Wait for a transfer started by `iso_submit`, cancelling it if `cancel` becomes true.
    fn iso_wait(&self, pending: Self::IsoPending, cancel: &AtomicBool) -> Result<IsoDone, i32>;
}

/// How long an unlinked bulk/interrupt OUT transfer that is already on the wire may keep running.
///
/// Cancelling it halfway would drop the rest of its bytes, and a client that gives up after a short
/// timeout (the Windows driver for a CH340 serial adapter cancels a 2048-byte write after about 30 ms,
/// while the adapter needs 44 ms at 460800 baud) would silently lose data. So such a transfer is left to
/// finish; only one that is still not done after this long (a device that stopped listening) is cancelled.
#[cfg(not(test))]
const UNLINK_GRACE: Duration = Duration::from_secs(1);
#[cfg(test)]
const UNLINK_GRACE: Duration = Duration::from_millis(300);

struct Urb {
    cancel: AtomicBool,
    /// An OUT transfer on a non-control endpoint: unlinking it waits for `UNLINK_GRACE` once it has started.
    hold_unlink: bool,
    /// The worker has begun this transfer.
    started: AtomicBool,
    /// Sequence number of a `CMD_UNLINK` we have not answered yet because the transfer is still running.
    pending_unlink: Mutex<Option<u32>>,
}

impl Urb {
    fn new(cmd: &CmdSubmit) -> Arc<Self> {
        Arc::new(Self {
            cancel: AtomicBool::new(false),
            hold_unlink: cmd.direction == DIR_OUT && cmd.ep != 0 && !cmd.is_iso(),
            started: AtomicBool::new(false),
            pending_unlink: Mutex::new(None),
        })
    }
}

struct Job {
    cmd: CmdSubmit,
    urb: Arc<Urb>,
}

type Inflight = Arc<Mutex<HashMap<u32, Arc<Urb>>>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn send<W: Write>(w: &Mutex<W>, bytes: &[u8]) {
    let mut g = lock(w);
    // A failed write means the socket is gone; the reader loop will notice and wind down.
    let _ = g.write_all(bytes).and_then(|()| g.flush());
}

/// Run the session until the client disconnects or violates the protocol.
pub fn run<B, R, W>(backend: Arc<B>, mut reader: R, writer: W) -> Result<(), ProtoError>
where
    B: UsbBackend,
    R: Read,
    W: Write + Send + 'static,
{
    let writer = Arc::new(Mutex::new(writer));
    let inflight: Inflight = Arc::default();
    let mut workers: HashMap<u8, (Sender<Job>, JoinHandle<()>)> = HashMap::new();

    let result = loop {
        match read_command(&mut reader) {
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),

            Ok(Some(Command::Submit(cmd))) => {
                tracing::debug!(
                    "recv seq={} ep{} {} len={}{}",
                    cmd.seqnum, cmd.ep, if cmd.is_in() { "IN" } else { "OUT" }, cmd.transfer_buffer_length,
                    if cmd.is_iso() { " iso" } else { "" }
                );
                let urb = Urb::new(&cmd);
                lock(&inflight).insert(cmd.seqnum, Arc::clone(&urb));

                let key = endpoint_key(&cmd);
                let (tx, _) = workers.entry(key).or_insert_with(|| {
                    let (tx, rx) = channel();
                    let handle = {
                        let (b, w, i) = (Arc::clone(&backend), Arc::clone(&writer), Arc::clone(&inflight));
                        thread::spawn(move || worker_loop(b, rx, w, i))
                    };
                    (tx, handle)
                });
                if tx.send(Job { cmd, urb }).is_err() {
                    break Err(ProtoError::Io(std::io::ErrorKind::BrokenPipe.into()));
                }
            }

            Ok(Some(Command::Unlink(u))) => {
                // Decide and reply under the in-flight lock so a worker cannot sneak a
                // RET_SUBMIT out between the decision and the reply.
                let mut map = lock(&inflight);
                let target = map.get(&u.unlink_seqnum).cloned();
                match target {
                    None => {
                        tracing::debug!("recv unlink of seq={}: already finished", u.unlink_seqnum);
                        send(&writer, &Reply::Unlink(RetUnlink { seqnum: u.seqnum, status: 0 }).encode());
                    }
                    Some(urb) if urb.hold_unlink && urb.started.load(Ordering::SeqCst) => {
                        tracing::debug!(
                            "recv unlink of seq={}: OUT transfer is running, letting it finish (up to {} ms)",
                            u.unlink_seqnum, UNLINK_GRACE.as_millis()
                        );
                        *lock(&urb.pending_unlink) = Some(u.seqnum);
                        drop(map);
                        let (w, i, target) = (Arc::clone(&writer), Arc::clone(&inflight), u.unlink_seqnum);
                        thread::spawn(move || {
                            let deadline = Instant::now() + UNLINK_GRACE;
                            loop {
                                thread::sleep(Duration::from_millis(5));
                                let mut map = lock(&i);
                                let Some(urb) = map.get(&target).cloned() else {
                                    return; // it finished; the worker has answered both messages
                                };
                                if Instant::now() < deadline {
                                    continue;
                                }
                                // Still not done: the device is not taking the data. Cancel it for real.
                                map.remove(&target);
                                urb.cancel.store(true, Ordering::SeqCst);
                                if let Some(seq) = lock(&urb.pending_unlink).take() {
                                    send(&w, &Reply::Unlink(RetUnlink { seqnum: seq, status: -errno::ECONNRESET }).encode());
                                }
                                return;
                            }
                        });
                    }
                    Some(urb) => {
                        tracing::debug!("recv unlink of seq={}: still pending, cancelling", u.unlink_seqnum);
                        map.remove(&u.unlink_seqnum);
                        urb.cancel.store(true, Ordering::SeqCst);
                        send(&writer, &Reply::Unlink(RetUnlink { seqnum: u.seqnum, status: -errno::ECONNRESET }).encode());
                    }
                }
            }
        }
    };

    // Wind down: abort whatever is still running so the workers can exit, then wait for
    // them so the device is really idle when we return.
    for urb in lock(&inflight).values() {
        urb.cancel.store(true, Ordering::SeqCst);
    }
    for (_, (tx, handle)) in workers {
        drop(tx);
        let _ = handle.join();
    }
    result
}

/// URBs on one endpoint must stay ordered; ep0 is shared by both directions.
fn endpoint_key(c: &CmdSubmit) -> u8 {
    if c.ep == 0 {
        0
    } else {
        (c.ep as u8 & 0x0f) | if c.is_in() { 0x80 } else { 0 }
    }
}

type Completer<P> = (Sender<IsoJob<P>>, JoinHandle<()>);

struct IsoJob<P> {
    cmd: CmdSubmit,
    urb: Arc<Urb>,
    pending: P,
}

/// Send the reply for a finished URB unless an unlink already answered for it.
fn finish<W: Write>(inflight: &Inflight, writer: &Mutex<W>, seqnum: u32, reply: &RetSubmit) {
    let mut map = lock(inflight);
    if let Some(urb) = map.remove(&seqnum) {
        send(writer, &reply.encode());
        // An unlink we held back because this transfer was running: it was too late to cancel.
        if let Some(seq) = lock(&urb.pending_unlink).take() {
            send(writer, &Reply::Unlink(RetUnlink { seqnum: seq, status: 0 }).encode());
        }
    } // else: unlinked while running; RET_UNLINK was already sent, so stay silent
}

fn worker_loop<B: UsbBackend, W: Write + Send + 'static>(
    backend: Arc<B>,
    rx: Receiver<Job>,
    writer: Arc<Mutex<W>>,
    inflight: Inflight,
) {
    // Isochronous URBs are started here, in order, but finished on a second thread so that the
    // next one can be started while the previous is still in flight.
    let mut completer: Option<Completer<B::IsoPending>> = None;

    for job in rx {
        if job.urb.cancel.load(Ordering::SeqCst) {
            continue; // unlinked before it ever started; the unlink already replied
        }
        job.urb.started.store(true, Ordering::SeqCst);
        if !job.cmd.is_iso() {
            let started = Instant::now();
            let reply = execute(&*backend, &job.cmd, &job.urb.cancel);
            tracing::debug!("seq={} took {} ms", job.cmd.seqnum, started.elapsed().as_millis());
            finish(&inflight, &writer, job.cmd.seqnum, &reply);
            continue;
        }

        let req = IsoRequest {
            endpoint: (job.cmd.ep as u8 & 0x0f) | if job.cmd.is_in() { 0x80 } else { 0 },
            data: job.cmd.data.clone(),
            packets: job.cmd.iso.clone(),
        };
        match backend.iso_submit(req) {
            Ok(pending) => {
                let (tx, _) = completer.get_or_insert_with(|| {
                    let (tx, rx) = channel::<IsoJob<B::IsoPending>>();
                    let (b, w, i) = (Arc::clone(&backend), Arc::clone(&writer), Arc::clone(&inflight));
                    let handle = thread::spawn(move || {
                        for j in rx {
                            let result = b.iso_wait(j.pending, &j.urb.cancel);
                            finish(&i, &w, j.cmd.seqnum, &iso_reply(&j.cmd, result));
                        }
                    });
                    (tx, handle)
                });
                let _ = tx.send(IsoJob { cmd: job.cmd, urb: job.urb, pending });
            }
            Err(e) => finish(&inflight, &writer, job.cmd.seqnum, &iso_reply(&job.cmd, Err(e))),
        }
    }

    if let Some((tx, handle)) = completer {
        drop(tx);
        let _ = handle.join();
    }
}

fn execute<B: UsbBackend>(b: &B, c: &CmdSubmit, cancel: &AtomicBool) -> RetSubmit {
    let is_in;
    let outcome = if c.ep == 0 {
        is_in = c.setup[0] & 0x80 != 0; // the setup packet is authoritative for control
        match special_control(b, &c.setup) {
            Some(r) => r.map(|()| Completed { data: Vec::new(), actual_length: 0 }),
            None => b.control(c.setup, &c.data, cancel),
        }
    } else {
        is_in = c.is_in();
        let endpoint = (c.ep as u8 & 0x0f) | if is_in { 0x80 } else { 0 };
        let in_len = if is_in { c.transfer_buffer_length.max(0) as usize } else { 0 };
        b.transfer(endpoint, &c.data, in_len, cancel)
    };

    let mut rs = match outcome {
        Ok(done) => {
            let mut rs = RetSubmit::simple(c.seqnum, 0, done.actual_length as i32);
            if is_in {
                rs.data = done.data;
                rs.data.truncate(c.transfer_buffer_length.max(0) as usize);
                rs.actual_length = rs.data.len() as i32;
            }
            rs
        }
        Err(e) => RetSubmit::simple(c.seqnum, -e, 0),
    };
    // Answer in the client's own convention for "not isochronous" (Linux sends 0, Windows -1).
    if !c.is_iso() {
        rs.number_of_packets = c.number_of_packets;
    }
    // Failures are always logged (except a cancel the client asked for); successes only at debug.
    if rs.status != 0 && !(c.ep != 0 && rs.status == -errno::ECONNRESET) {
        if c.ep == 0 {
            tracing::warn!(
                "control request failed: setup={:02x?} len={} -> status {}",
                c.setup, c.transfer_buffer_length, rs.status
            );
        } else {
            tracing::warn!(
                "ep{} {} len={} failed -> status {} (errno {})",
                c.ep, if is_in { "IN" } else { "OUT" }, c.transfer_buffer_length, rs.status, -rs.status
            );
        }
    } else {
        tracing::debug!(
            "ep{} {} len={} setup={:02x?} -> status {} actual {}",
            c.ep, if is_in { "IN" } else { "OUT" }, c.transfer_buffer_length, c.setup, rs.status, rs.actual_length
        );
    }
    rs
}

/// Requests the kernel (and libusb) treat as device state changes rather than plain
/// control transfers. Same set the Linux usbip host driver intercepts.
fn special_control<B: UsbBackend>(b: &B, setup: &[u8; 8]) -> Option<Result<(), i32>> {
    let (request_type, request) = (setup[0], setup[1]);
    let value = u16::from_le_bytes([setup[2], setup[3]]);
    let index = u16::from_le_bytes([setup[4], setup[5]]);
    match (request_type, request) {
        (0x02, 1) if value == 0 => Some(b.clear_halt(index as u8)), // CLEAR_FEATURE(ENDPOINT_HALT)
        (0x01, 11) => Some(b.set_interface(index as u8, value as u8)), // SET_INTERFACE
        (0x00, 9) => Some(b.set_configuration(value as u8)),         // SET_CONFIGURATION
        (0x23, 3) if value == 4 => Some(b.reset()),                  // SET_FEATURE(PORT_RESET)
        _ => None,
    }
}

/// Build the `RET_SUBMIT` for an isochronous URB. Even a failed one carries one descriptor per
/// packet, so the client's stream stays in step.
fn iso_reply(c: &CmdSubmit, result: Result<IsoDone, i32>) -> RetSubmit {
    let mut rs = RetSubmit::simple(c.seqnum, 0, 0);
    rs.start_frame = c.start_frame;
    rs.number_of_packets = c.number_of_packets;
    match result {
        Ok(done) => {
            rs.error_count = done.packets.iter().filter(|p| p.status != 0).count() as i32;
            rs.actual_length = done.packets.iter().map(|p| p.actual_length).sum::<u32>() as i32;
            if c.is_in() {
                rs.data = done.data;
            }
            rs.iso = done.packets;
        }
        Err(e) => {
            rs.status = -e;
            rs.error_count = c.number_of_packets;
            rs.iso = c
                .iso
                .iter()
                .map(|d| IsoPacketDescriptor { actual_length: 0, status: -e, ..*d })
                .collect();
        }
    }
    rs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use usbip_proto::{read_reply, CmdUnlink, IsoPacketDescriptor, DIR_IN, DIR_OUT, NON_ISO};

    const GET_DESCRIPTOR: [u8; 8] = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x08, 0x00];

    #[derive(Default)]
    struct Fake {
        log: Mutex<Vec<String>>,
    }
    impl Fake {
        fn log(&self, s: impl Into<String>) {
            lock(&self.log).push(s.into());
        }
        fn entries(&self) -> Vec<String> {
            lock(&self.log).clone()
        }
    }

    impl UsbBackend for Fake {
        fn control(&self, setup: [u8; 8], out: &[u8], _c: &AtomicBool) -> Result<Completed, i32> {
            match setup[1] {
                0x06 => Ok(Completed { data: vec![0x12, 1, 0, 2, 0, 0, 0, 64], actual_length: 8 }),
                0xee => Err(errno::EPIPE),
                _ => {
                    self.log(format!("control {:02x} {:?}", setup[1], out));
                    Ok(Completed { data: vec![], actual_length: out.len() })
                }
            }
        }

        fn transfer(&self, ep: u8, out: &[u8], in_len: usize, cancel: &AtomicBool) -> Result<Completed, i32> {
            match ep {
                0x81 => loop {
                    // An IN endpoint with nothing to say: blocks until cancelled.
                    if cancel.load(Ordering::SeqCst) {
                        return Err(errno::ECONNRESET);
                    }
                    thread::sleep(Duration::from_millis(2));
                },
                0x82 => {
                    let mut d = vec![1, 2, 3];
                    d.truncate(in_len);
                    Ok(Completed { actual_length: d.len(), data: d })
                }
                0x04 => loop {
                    // An OUT endpoint whose device never takes the data: blocks until cancelled.
                    if cancel.load(Ordering::SeqCst) {
                        return Err(errno::ECONNRESET);
                    }
                    thread::sleep(Duration::from_millis(2));
                },
                0x02 => {
                    if out == [1] {
                        thread::sleep(Duration::from_millis(40)); // would be overtaken if not ordered
                    }
                    self.log(format!("out {:?}", out));
                    Ok(Completed { data: vec![], actual_length: out.len() })
                }
                _ => Err(errno::EINVAL),
            }
        }

        fn clear_halt(&self, ep: u8) -> Result<(), i32> {
            self.log(format!("clear_halt {ep:#04x}"));
            Ok(())
        }
        fn set_interface(&self, i: u8, a: u8) -> Result<(), i32> {
            self.log(format!("set_interface {i} {a}"));
            Ok(())
        }
        fn set_configuration(&self, c: u8) -> Result<(), i32> {
            self.log(format!("set_configuration {c}"));
            Ok(())
        }
        fn reset(&self) -> Result<(), i32> {
            self.log("reset");
            Ok(())
        }

        type IsoPending = IsoRequest;

        fn iso_submit(&self, req: IsoRequest) -> Result<IsoRequest, i32> {
            self.log(format!("iso_submit {:#04x} x{}", req.endpoint, req.packets.len()));
            match req.endpoint & 0x0f {
                3 => Ok(req),
                _ => Err(errno::EINVAL),
            }
        }

        /// IN: packet i delivers i bytes (value i). OUT: accepts everything.
        fn iso_wait(&self, req: IsoRequest, cancel: &AtomicBool) -> Result<IsoDone, i32> {
            if cancel.load(Ordering::SeqCst) {
                return Err(errno::ECONNRESET);
            }
            let is_in = req.endpoint & 0x80 != 0;
            let mut data = Vec::new();
            let packets = req
                .packets
                .iter()
                .enumerate()
                .map(|(i, d)| {
                    let n = if is_in { i as u32 } else { d.length };
                    if is_in {
                        data.extend(std::iter::repeat_n(i as u8, n as usize));
                    }
                    // The last packet of an IN transfer reports an error.
                    let bad = is_in && i + 1 == req.packets.len();
                    IsoPacketDescriptor { actual_length: n, status: if bad { -errno::EPROTO } else { 0 }, ..*d }
                })
                .collect();
            Ok(IsoDone { data, packets })
        }
    }

    struct Harness {
        client: TcpStream,
        fake: Arc<Fake>,
        done: mpsc::Receiver<Result<(), ProtoError>>,
        ins: Vec<u32>,
    }

    fn start() -> Harness {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let fake = Arc::new(Fake::default());
        let (tx, done) = mpsc::channel();
        let f = Arc::clone(&fake);
        thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let r = s.try_clone().unwrap();
            let _ = tx.send(run(f, r, s));
        });
        let client = TcpStream::connect(addr).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        Harness { client, fake, done, ins: Vec::new() }
    }

    impl Harness {
        fn send(&mut self, bytes: Vec<u8>) {
            self.client.write_all(&bytes).unwrap();
        }
        fn submit(&mut self, seqnum: u32, ep: u32, dir: u32, setup: [u8; 8], data: Vec<u8>, len: i32) {
            if dir == DIR_IN || (ep == 0 && setup[0] & 0x80 != 0) {
                self.ins.push(seqnum);
            }
            let c = CmdSubmit {
                seqnum,
                devid: 1 << 16 | 2,
                direction: dir,
                ep,
                transfer_flags: 0,
                transfer_buffer_length: len,
                start_frame: 0,
                number_of_packets: NON_ISO,
                interval: 0,
                setup,
                data,
                iso: vec![],
            };
            self.send(c.encode());
        }
        fn unlink(&mut self, seqnum: u32, target: u32) {
            self.send(CmdUnlink { seqnum, devid: 0, direction: 0, ep: 0, unlink_seqnum: target }.encode());
        }
        fn reply(&mut self) -> Reply {
            let ins = self.ins.clone();
            read_reply(&mut self.client, |s| ins.contains(&s)).unwrap()
        }
        fn finish(self) -> Result<(), ProtoError> {
            drop(self.client);
            self.done.recv_timeout(Duration::from_secs(5)).expect("session did not end")
        }
    }

    fn submit_reply(r: Reply) -> RetSubmit {
        match r {
            Reply::Submit(s) => s,
            other => panic!("expected RET_SUBMIT, got {other:?}"),
        }
    }

    #[test]
    fn control_in_returns_data() {
        let mut h = start();
        h.submit(1, 0, DIR_IN, GET_DESCRIPTOR, vec![], 8);
        let r = submit_reply(h.reply());
        assert_eq!((r.seqnum, r.status, r.actual_length), (1, 0, 8));
        assert_eq!(r.data, vec![0x12, 1, 0, 2, 0, 0, 0, 64]);
        h.finish().unwrap();
    }

    #[test]
    fn bulk_in_and_out_and_errors() {
        let mut h = start();
        h.submit(1, 2, DIR_OUT, [0; 8], vec![5, 6, 7], 3);
        let r = submit_reply(h.reply());
        assert_eq!((r.status, r.actual_length, r.data.len()), (0, 3, 0));

        h.submit(2, 2, DIR_IN, [0; 8], vec![], 2); // device ep 0x82 -> truncated to buffer size
        let r = submit_reply(h.reply());
        assert_eq!((r.status, r.actual_length, r.data), (0, 2, vec![1, 2]));

        h.submit(3, 7, DIR_OUT, [0; 8], vec![0], 1); // backend rejects: EINVAL
        let r = submit_reply(h.reply());
        assert_eq!((r.status, r.actual_length), (-errno::EINVAL, 0));

        h.submit(4, 0, DIR_IN, [0x80, 0xee, 0, 0, 0, 0, 4, 0], vec![], 4); // stall
        let r = submit_reply(h.reply());
        assert_eq!((r.status, r.actual_length, r.data.len()), (-errno::EPIPE, 0, 0));
        h.finish().unwrap();
    }

    #[test]
    fn urbs_on_one_endpoint_complete_in_order() {
        let mut h = start();
        for (seq, byte) in [(1u32, 1u8), (2, 2), (3, 3)] {
            h.submit(seq, 2, DIR_OUT, [0; 8], vec![byte], 1);
        }
        let seqs: Vec<u32> = (0..3).map(|_| submit_reply(h.reply()).seqnum).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
        assert_eq!(h.fake.entries(), vec!["out [1]", "out [2]", "out [3]"]);
        h.finish().unwrap();
    }

    #[test]
    fn blocked_endpoint_does_not_stall_control() {
        let mut h = start();
        h.submit(1, 1, DIR_IN, [0; 8], vec![], 64); // blocks forever
        h.submit(2, 0, DIR_IN, GET_DESCRIPTOR, vec![], 8);
        assert_eq!(submit_reply(h.reply()).seqnum, 2);
        h.finish().unwrap(); // disconnect must cancel the blocked URB and end the session
    }

    #[test]
    fn unlink_cancels_a_running_urb_and_suppresses_its_reply() {
        let mut h = start();
        h.submit(1, 1, DIR_IN, [0; 8], vec![], 64);
        h.unlink(2, 1);
        assert_eq!(h.reply(), Reply::Unlink(RetUnlink { seqnum: 2, status: -errno::ECONNRESET }));

        // If seq 1 had also been answered, its RET_SUBMIT would arrive before this one.
        h.submit(3, 0, DIR_IN, GET_DESCRIPTOR, vec![], 8);
        assert_eq!(submit_reply(h.reply()).seqnum, 3);
        h.finish().unwrap();
    }

    #[test]
    fn unlinking_a_running_out_transfer_lets_it_finish_instead_of_truncating_it() {
        let mut h = start();
        h.submit(1, 2, DIR_OUT, [0; 8], vec![1], 1); // the fake takes 40 ms to accept this
        thread::sleep(Duration::from_millis(10)); // now it is running
        h.unlink(2, 1);
        let r = submit_reply(h.reply());
        assert_eq!((r.seqnum, r.status, r.actual_length), (1, 0, 1));
        assert_eq!(h.reply(), Reply::Unlink(RetUnlink { seqnum: 2, status: 0 }));
        assert_eq!(h.fake.entries(), vec!["out [1]"]); // the data really went out
        h.finish().unwrap();
    }

    #[test]
    fn a_running_out_transfer_that_never_finishes_is_cancelled_after_the_grace_period() {
        let mut h = start();
        h.submit(1, 4, DIR_OUT, [0; 8], vec![9], 1); // blocks forever
        thread::sleep(Duration::from_millis(10));
        let t = Instant::now();
        h.unlink(2, 1);
        assert_eq!(h.reply(), Reply::Unlink(RetUnlink { seqnum: 2, status: -errno::ECONNRESET }));
        assert!(t.elapsed() >= UNLINK_GRACE - Duration::from_millis(50), "{:?}", t.elapsed());

        h.submit(3, 0, DIR_IN, GET_DESCRIPTOR, vec![], 8); // no RET_SUBMIT for seq 1 came first
        assert_eq!(submit_reply(h.reply()).seqnum, 3);
        h.finish().unwrap();
    }

    #[test]
    fn unlinking_an_out_transfer_that_has_not_started_cancels_it_at_once() {
        let mut h = start();
        h.submit(1, 2, DIR_OUT, [0; 8], vec![1], 1); // running for 40 ms
        h.submit(2, 2, DIR_OUT, [0; 8], vec![2], 1); // queued behind it
        thread::sleep(Duration::from_millis(10));
        h.unlink(3, 2);
        assert_eq!(h.reply(), Reply::Unlink(RetUnlink { seqnum: 3, status: -errno::ECONNRESET }));
        assert_eq!(submit_reply(h.reply()).seqnum, 1);
        assert_eq!(h.fake.entries(), vec!["out [1]"]); // seq 2 never reached the device
        h.finish().unwrap();
    }

    #[test]
    fn unlink_after_completion_reports_zero() {
        let mut h = start();
        h.submit(1, 0, DIR_IN, GET_DESCRIPTOR, vec![], 8);
        assert_eq!(submit_reply(h.reply()).seqnum, 1);
        h.unlink(2, 1);
        assert_eq!(h.reply(), Reply::Unlink(RetUnlink { seqnum: 2, status: 0 }));
        h.finish().unwrap();
    }

    #[test]
    fn device_state_requests_are_not_sent_as_plain_transfers() {
        let mut h = start();
        let reqs: [[u8; 8]; 4] = [
            [0x02, 0x01, 0x00, 0x00, 0x81, 0x00, 0x00, 0x00], // CLEAR_FEATURE(HALT) ep 0x81
            [0x01, 0x0b, 0x02, 0x00, 0x01, 0x00, 0x00, 0x00], // SET_INTERFACE iface 1 alt 2
            [0x00, 0x09, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00], // SET_CONFIGURATION 1
            [0x23, 0x03, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00], // port RESET
        ];
        for (i, setup) in reqs.iter().enumerate() {
            h.submit(i as u32 + 1, 0, DIR_OUT, *setup, vec![], 0);
            assert_eq!(submit_reply(h.reply()).status, 0);
        }
        assert_eq!(
            h.fake.entries(),
            vec!["clear_halt 0x81", "set_interface 1 2", "set_configuration 1", "reset"]
        );
        h.finish().unwrap();
    }

    fn iso_cmd(seqnum: u32, ep: u32, dir: u32, lengths: &[u32], data: Vec<u8>) -> CmdSubmit {
        let mut offset = 0;
        let iso = lengths
            .iter()
            .map(|&length| {
                let d = IsoPacketDescriptor { offset, length, actual_length: 0, status: 0 };
                offset += length;
                d
            })
            .collect::<Vec<_>>();
        CmdSubmit {
            seqnum,
            devid: 0,
            direction: dir,
            ep,
            transfer_flags: 0,
            transfer_buffer_length: offset as i32,
            start_frame: 5,
            number_of_packets: lengths.len() as i32,
            interval: 1,
            setup: [0; 8],
            data,
            iso,
        }
    }

    #[test]
    fn isochronous_in_returns_packed_data_and_per_packet_status() {
        let mut h = start();
        h.ins.push(1);
        h.send(iso_cmd(1, 3, DIR_IN, &[4, 4, 4, 4], vec![]).encode());
        let r = submit_reply(h.reply());
        assert_eq!((r.status, r.number_of_packets, r.start_frame, r.error_count), (0, 4, 5, 1));
        // Packets delivered 0, 1, 2 and 3 bytes: only those bytes travel, back to back.
        assert_eq!(r.data, vec![1, 2, 2, 3, 3, 3]);
        assert_eq!(r.actual_length, 6);
        // Offsets and lengths are the client's; only actual_length and status are ours.
        let got: Vec<_> = r.iso.iter().map(|d| (d.offset, d.length, d.actual_length, d.status)).collect();
        assert_eq!(got, vec![(0, 4, 0, 0), (4, 4, 1, 0), (8, 4, 2, 0), (12, 4, 3, -errno::EPROTO)]);

        h.submit(2, 0, DIR_IN, GET_DESCRIPTOR, vec![], 8); // stream still parses
        assert_eq!(submit_reply(h.reply()).seqnum, 2);
        h.finish().unwrap();
    }

    #[test]
    fn isochronous_out_is_accepted_without_payload_in_the_reply() {
        let mut h = start();
        h.send(iso_cmd(1, 3, DIR_OUT, &[2, 2], vec![1, 2, 3, 4]).encode());
        let r = submit_reply(h.reply());
        assert_eq!((r.status, r.actual_length, r.error_count, r.data.len(), r.iso.len()), (0, 4, 0, 0, 2));
        h.finish().unwrap();
    }

    #[test]
    fn several_isochronous_urbs_can_be_in_flight_and_complete_in_order() {
        let mut h = start();
        for seq in 1..=5u32 {
            h.ins.push(seq);
            h.send(iso_cmd(seq, 3, DIR_IN, &[8, 8], vec![]).encode());
        }
        let seqs: Vec<u32> = (0..5).map(|_| submit_reply(h.reply()).seqnum).collect();
        assert_eq!(seqs, vec![1, 2, 3, 4, 5]);
        assert_eq!(h.fake.entries().len(), 5);
        h.finish().unwrap();
    }

    #[test]
    fn a_failed_isochronous_submit_still_answers_every_packet() {
        let mut h = start();
        h.send(iso_cmd(1, 6, DIR_OUT, &[2, 2], vec![1, 2, 3, 4]).encode()); // backend refuses ep 6
        let r = submit_reply(h.reply());
        assert_eq!((r.status, r.error_count, r.iso.len()), (-errno::EINVAL, 2, 2));
        assert!(r.iso.iter().all(|d| d.status == -errno::EINVAL && d.actual_length == 0));

        h.submit(2, 0, DIR_IN, GET_DESCRIPTOR, vec![], 8);
        assert_eq!(submit_reply(h.reply()).seqnum, 2);
        h.finish().unwrap();
    }

    #[test]
    fn garbage_ends_the_session_with_an_error() {
        let mut h = start();
        let mut junk = vec![0u8; 48];
        junk[3] = 0x55;
        h.send(junk);
        let res = h.done.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(res, Err(ProtoError::BadCommand(0x55))));
    }
}
