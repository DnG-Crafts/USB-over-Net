//! USB/IP URB phase: `CMD_SUBMIT`, `RET_SUBMIT`, `CMD_UNLINK`, `RET_UNLINK`.
//!
//! After a successful `IMPORT` the connection carries a stream of 48-byte headers,
//! each optionally followed by payload. Layout (all big-endian):
//!
//! ```text
//!  0  command   4  seqnum   8  devid   12 direction   16 ep        <- basic header (20)
//! CMD_SUBMIT : 20 transfer_flags | 24 transfer_buffer_length | 28 start_frame
//!              32 number_of_packets | 36 interval | 40 setup[8]
//! RET_SUBMIT : 20 status | 24 actual_length | 28 start_frame | 32 number_of_packets
//!              36 error_count | 40 padding[8]
//! CMD_UNLINK : 20 unlink_seqnum | 24 padding[24]
//! RET_UNLINK : 20 status | 24 padding[24]
//! ```
//!
//! Payload after `CMD_SUBMIT` (OUT only) or `RET_SUBMIT` (IN only): the transfer data,
//! then `number_of_packets` 16-byte isochronous descriptors if the URB is isochronous.

use std::io::{self, Read};

use crate::ProtoError;

pub const URB_HEADER_LEN: usize = 48;
pub const ISO_DESC_LEN: usize = 16;

pub const CMD_SUBMIT: u32 = 0x0001;
pub const CMD_UNLINK: u32 = 0x0002;
pub const RET_SUBMIT: u32 = 0x0003;
pub const RET_UNLINK: u32 = 0x0004;

pub const DIR_OUT: u32 = 0;
pub const DIR_IN: u32 = 1;

/// `number_of_packets` value for non-isochronous URBs (0xffffffff on the wire).
pub const NON_ISO: i32 = -1;

/// Largest transfer buffer we accept from a peer (guards against hostile lengths).
pub const MAX_TRANSFER_LEN: usize = 16 * 1024 * 1024;
/// Largest isochronous packet count we accept.
pub const MAX_ISO_PACKETS: usize = 1024;

/// Linux errno values used in `status` fields (sent negated).
pub mod errno {
    pub const ENODEV: i32 = 19;
    pub const EINVAL: i32 = 22;
    pub const EPIPE: i32 = 32;
    pub const EOVERFLOW: i32 = 75;
    pub const ECONNRESET: i32 = 104;
    pub const ETIMEDOUT: i32 = 110;
    pub const ENOSYS: i32 = 38;
    pub const EPROTO: i32 = 71;
    pub const EIO: i32 = 5;
    pub const EBUSY: i32 = 16;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsoPacketDescriptor {
    pub offset: u32,
    pub length: u32,
    pub actual_length: u32,
    pub status: i32,
}

impl IsoPacketDescriptor {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.offset.to_be_bytes());
        out.extend_from_slice(&self.length.to_be_bytes());
        out.extend_from_slice(&self.actual_length.to_be_bytes());
        out.extend_from_slice(&self.status.to_be_bytes());
    }

    fn decode(b: &[u8]) -> Self {
        Self {
            offset: be32(b, 0),
            length: be32(b, 4),
            actual_length: be32(b, 8),
            status: be32(b, 12) as i32,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdSubmit {
    pub seqnum: u32,
    pub devid: u32,
    pub direction: u32,
    pub ep: u32,
    pub transfer_flags: u32,
    pub transfer_buffer_length: i32,
    pub start_frame: i32,
    pub number_of_packets: i32,
    pub interval: i32,
    pub setup: [u8; 8],
    /// OUT payload (empty for IN).
    pub data: Vec<u8>,
    pub iso: Vec<IsoPacketDescriptor>,
}

impl CmdSubmit {
    pub fn is_in(&self) -> bool {
        self.direction == DIR_IN
    }

    pub fn is_iso(&self) -> bool {
        self.number_of_packets != NON_ISO && self.number_of_packets != 0
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(URB_HEADER_LEN + self.data.len());
        put_basic(&mut out, CMD_SUBMIT, self.seqnum, self.devid, self.direction, self.ep);
        out.extend_from_slice(&self.transfer_flags.to_be_bytes());
        out.extend_from_slice(&self.transfer_buffer_length.to_be_bytes());
        out.extend_from_slice(&self.start_frame.to_be_bytes());
        out.extend_from_slice(&self.number_of_packets.to_be_bytes());
        out.extend_from_slice(&self.interval.to_be_bytes());
        out.extend_from_slice(&self.setup);
        out.extend_from_slice(&self.data);
        for d in &self.iso {
            d.encode(&mut out);
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdUnlink {
    pub seqnum: u32,
    pub devid: u32,
    pub direction: u32,
    pub ep: u32,
    /// Sequence number of the `CMD_SUBMIT` to cancel.
    pub unlink_seqnum: u32,
}

impl CmdUnlink {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(URB_HEADER_LEN);
        put_basic(&mut out, CMD_UNLINK, self.seqnum, self.devid, self.direction, self.ep);
        out.extend_from_slice(&self.unlink_seqnum.to_be_bytes());
        out.resize(URB_HEADER_LEN, 0);
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Submit(CmdSubmit),
    Unlink(CmdUnlink),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetSubmit {
    pub seqnum: u32,
    pub status: i32,
    pub actual_length: i32,
    pub start_frame: i32,
    pub number_of_packets: i32,
    pub error_count: i32,
    /// IN payload (empty for OUT or on error).
    pub data: Vec<u8>,
    pub iso: Vec<IsoPacketDescriptor>,
}

impl RetSubmit {
    /// A reply without payload, e.g. an OUT completion or an error.
    pub fn simple(seqnum: u32, status: i32, actual_length: i32) -> Self {
        Self {
            seqnum,
            status,
            actual_length,
            start_frame: 0,
            number_of_packets: NON_ISO,
            error_count: 0,
            data: Vec::new(),
            iso: Vec::new(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(URB_HEADER_LEN + self.data.len());
        put_basic(&mut out, RET_SUBMIT, self.seqnum, 0, 0, 0);
        out.extend_from_slice(&self.status.to_be_bytes());
        out.extend_from_slice(&self.actual_length.to_be_bytes());
        out.extend_from_slice(&self.start_frame.to_be_bytes());
        out.extend_from_slice(&self.number_of_packets.to_be_bytes());
        out.extend_from_slice(&self.error_count.to_be_bytes());
        out.resize(URB_HEADER_LEN, 0);
        out.extend_from_slice(&self.data);
        for d in &self.iso {
            d.encode(&mut out);
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetUnlink {
    pub seqnum: u32,
    pub status: i32,
}

impl RetUnlink {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(URB_HEADER_LEN);
        put_basic(&mut out, RET_UNLINK, self.seqnum, 0, 0, 0);
        out.extend_from_slice(&self.status.to_be_bytes());
        out.resize(URB_HEADER_LEN, 0);
        out
    }
}

/// Any message the server may send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Submit(RetSubmit),
    Unlink(RetUnlink),
}

impl Reply {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Reply::Submit(r) => r.encode(),
            Reply::Unlink(r) => r.encode(),
        }
    }
}

// ---------------------------------------------------------------- reading

/// Read one command (header plus payload) sent by a client.
///
/// Returns `Ok(None)` on a clean EOF at a message boundary (the client detached).
pub fn read_command<R: Read>(r: &mut R) -> Result<Option<Command>, ProtoError> {
    let mut h = [0u8; URB_HEADER_LEN];
    if !read_exact_or_eof(r, &mut h)? {
        return Ok(None);
    }
    let (command, seqnum, devid, direction, ep) = (be32(&h, 0), be32(&h, 4), be32(&h, 8), be32(&h, 12), be32(&h, 16));

    match command {
        CMD_SUBMIT => {
            let len = be32(&h, 24) as i32;
            let npkt = be32(&h, 32) as i32;
            if len < 0 || len as usize > MAX_TRANSFER_LEN {
                return Err(ProtoError::TooLarge(len as u32));
            }
            let iso_count = if npkt == NON_ISO { 0 } else { npkt.max(0) as usize };
            if iso_count > MAX_ISO_PACKETS {
                return Err(ProtoError::TooLarge(npkt as u32));
            }
            let mut setup = [0u8; 8];
            setup.copy_from_slice(&h[40..48]);

            // OUT transfers carry their data inline; IN transfers carry none.
            let mut data = Vec::new();
            if direction == DIR_OUT && len > 0 {
                data = read_vec(r, len as usize)?;
            }
            let mut iso = Vec::with_capacity(iso_count);
            if iso_count > 0 {
                let raw = read_vec(r, iso_count * ISO_DESC_LEN)?;
                iso.extend(raw.chunks_exact(ISO_DESC_LEN).map(IsoPacketDescriptor::decode));
            }
            Ok(Some(Command::Submit(CmdSubmit {
                seqnum,
                devid,
                direction,
                ep,
                transfer_flags: be32(&h, 20),
                transfer_buffer_length: len,
                start_frame: be32(&h, 28) as i32,
                number_of_packets: npkt,
                interval: be32(&h, 36) as i32,
                setup,
                data,
                iso,
            })))
        }
        CMD_UNLINK => Ok(Some(Command::Unlink(CmdUnlink {
            seqnum,
            devid,
            direction,
            ep,
            unlink_seqnum: be32(&h, 20),
        }))),
        other => Err(ProtoError::BadCommand(other)),
    }
}

/// Read a server reply. `submit_is_in` tells whether a `RET_SUBMIT` carries IN data
/// (the reader must know, because the wire format does not say).
pub fn read_reply<R: Read>(r: &mut R, submit_is_in: impl Fn(u32) -> bool) -> Result<Reply, ProtoError> {
    let mut h = [0u8; URB_HEADER_LEN];
    r.read_exact(&mut h)?;
    let (command, seqnum) = (be32(&h, 0), be32(&h, 4));
    match command {
        RET_SUBMIT => {
            let actual_length = be32(&h, 24) as i32;
            let npkt = be32(&h, 32) as i32;
            let iso_count = if npkt == NON_ISO { 0 } else { npkt.max(0) as usize };
            if actual_length < 0 || actual_length as usize > MAX_TRANSFER_LEN || iso_count > MAX_ISO_PACKETS {
                return Err(ProtoError::TooLarge(actual_length as u32));
            }
            let data = if submit_is_in(seqnum) && actual_length > 0 {
                read_vec(r, actual_length as usize)?
            } else {
                Vec::new()
            };
            let mut iso = Vec::with_capacity(iso_count);
            if iso_count > 0 {
                let raw = read_vec(r, iso_count * ISO_DESC_LEN)?;
                iso.extend(raw.chunks_exact(ISO_DESC_LEN).map(IsoPacketDescriptor::decode));
            }
            Ok(Reply::Submit(RetSubmit {
                seqnum,
                status: be32(&h, 20) as i32,
                actual_length,
                start_frame: be32(&h, 28) as i32,
                number_of_packets: npkt,
                error_count: be32(&h, 36) as i32,
                data,
                iso,
            }))
        }
        RET_UNLINK => Ok(Reply::Unlink(RetUnlink { seqnum, status: be32(&h, 20) as i32 })),
        other => Err(ProtoError::BadCommand(other)),
    }
}

// ---------------------------------------------------------------- helpers

fn put_basic(out: &mut Vec<u8>, command: u32, seqnum: u32, devid: u32, direction: u32, ep: u32) {
    for v in [command, seqnum, devid, direction, ep] {
        out.extend_from_slice(&v.to_be_bytes());
    }
}

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Like `read_exact`, but `Ok(false)` if the stream ended before the first byte.
fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

fn read_vec<R: Read>(r: &mut R, len: usize) -> io::Result<Vec<u8>> {
    let mut v = vec![0u8; len];
    r.read_exact(&mut v)?;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn get_descriptor() -> CmdSubmit {
        CmdSubmit {
            seqnum: 7,
            devid: (1 << 16) | 5,
            direction: DIR_IN,
            ep: 0,
            transfer_flags: 0,
            transfer_buffer_length: 18,
            start_frame: 0,
            number_of_packets: NON_ISO,
            interval: 0,
            setup: [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00], // GET_DESCRIPTOR(device)
            data: vec![],
            iso: vec![],
        }
    }

    #[test]
    fn cmd_submit_golden_bytes() {
        let b = get_descriptor().encode();
        assert_eq!(b.len(), URB_HEADER_LEN);
        assert_eq!(&b[0..4], &[0, 0, 0, 1]); // CMD_SUBMIT
        assert_eq!(&b[4..8], &[0, 0, 0, 7]); // seqnum
        assert_eq!(&b[8..12], &[0, 1, 0, 5]); // devid
        assert_eq!(&b[12..16], &[0, 0, 0, 1]); // IN
        assert_eq!(&b[24..28], &[0, 0, 0, 18]); // length
        assert_eq!(&b[32..36], &[0xff; 4]); // not isochronous
        assert_eq!(&b[40..48], &[0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00]);
    }

    #[test]
    fn submit_roundtrip_in_and_out() {
        let c = get_descriptor();
        let got = read_command(&mut Cursor::new(c.encode())).unwrap().unwrap();
        assert_eq!(got, Command::Submit(c));

        let mut out = get_descriptor();
        out.direction = DIR_OUT;
        out.ep = 2;
        out.transfer_buffer_length = 4;
        out.data = vec![1, 2, 3, 4];
        let got = read_command(&mut Cursor::new(out.encode())).unwrap().unwrap();
        assert_eq!(got, Command::Submit(out));
    }

    #[test]
    fn iso_descriptors_are_consumed_so_the_stream_stays_in_sync() {
        let mut iso = get_descriptor();
        iso.direction = DIR_OUT;
        iso.ep = 1;
        iso.transfer_buffer_length = 3;
        iso.data = vec![9, 9, 9];
        iso.number_of_packets = 2;
        iso.iso = vec![
            IsoPacketDescriptor { offset: 0, length: 2, actual_length: 0, status: 0 },
            IsoPacketDescriptor { offset: 2, length: 1, actual_length: 0, status: 0 },
        ];
        let next = CmdUnlink { seqnum: 8, devid: 0, direction: 0, ep: 0, unlink_seqnum: 7 };

        let mut stream = iso.encode();
        stream.extend(next.encode());
        let mut cur = Cursor::new(stream);
        assert_eq!(read_command(&mut cur).unwrap().unwrap(), Command::Submit(iso));
        assert_eq!(read_command(&mut cur).unwrap().unwrap(), Command::Unlink(next));
        assert_eq!(read_command(&mut cur).unwrap(), None); // clean EOF
    }

    #[test]
    fn unlink_and_replies_roundtrip() {
        let u = CmdUnlink { seqnum: 9, devid: 1, direction: 0, ep: 0, unlink_seqnum: 4 };
        assert_eq!(u.encode().len(), URB_HEADER_LEN);
        assert_eq!(read_command(&mut Cursor::new(u.encode())).unwrap().unwrap(), Command::Unlink(u));

        let ru = RetUnlink { seqnum: 9, status: -errno::ECONNRESET };
        let got = read_reply(&mut Cursor::new(ru.encode()), |_| false).unwrap();
        assert_eq!(got, Reply::Unlink(ru));

        let mut rs = RetSubmit::simple(7, 0, 4);
        rs.data = vec![0xde, 0xad, 0xbe, 0xef];
        let bytes = rs.encode();
        assert_eq!(bytes.len(), URB_HEADER_LEN + 4);
        assert_eq!(read_reply(&mut Cursor::new(bytes.clone()), |s| s == 7).unwrap(), Reply::Submit(rs.clone()));
        // An OUT completion has no payload even if actual_length is non-zero.
        let mut out = RetSubmit::simple(8, 0, 4);
        out.data.clear();
        assert_eq!(out.encode().len(), URB_HEADER_LEN);
    }

    #[test]
    fn negative_status_survives_the_wire() {
        let r = RetSubmit::simple(1, -errno::EPIPE, 0);
        match read_reply(&mut Cursor::new(r.encode()), |_| false).unwrap() {
            Reply::Submit(x) => assert_eq!(x.status, -32),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn hostile_input_is_rejected_not_allocated() {
        let mut c = get_descriptor();
        c.transfer_buffer_length = i32::MAX;
        assert!(matches!(read_command(&mut Cursor::new(c.encode())), Err(ProtoError::TooLarge(_))));

        let mut c = get_descriptor();
        c.number_of_packets = 1_000_000;
        assert!(matches!(read_command(&mut Cursor::new(c.encode())), Err(ProtoError::TooLarge(_))));

        let mut bad = get_descriptor().encode();
        bad[3] = 0x77;
        assert!(matches!(read_command(&mut Cursor::new(bad)), Err(ProtoError::BadCommand(0x77))));
    }

    #[test]
    fn truncated_message_is_an_error_but_empty_stream_is_eof() {
        assert_eq!(read_command(&mut Cursor::new(Vec::<u8>::new())).unwrap(), None);
        let b = get_descriptor().encode();
        assert!(read_command(&mut Cursor::new(b[..30].to_vec())).is_err());
    }
}
