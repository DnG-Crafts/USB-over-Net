//! USB/IP wire protocol, version 1.1.1.
//!
//! Everything on the wire is big-endian. This crate has no I/O of its own beyond
//! `std::io::Read` helpers, so it can be shared by the server, the client and tests.
//!
//! Scope so far: the *operation* phase (`DEVLIST`, `IMPORT`). The URB phase
//! (`CMD_SUBMIT` / `RET_SUBMIT` / `CMD_UNLINK` / `RET_UNLINK`) comes next.

use std::fmt;
use std::io::{self, Read};

pub mod urb;
pub use urb::*;

pub const USBIP_VERSION: u16 = 0x0111;
pub const DEFAULT_PORT: u16 = 3240;
/// DNS-SD service type servers advertise and clients browse for.
pub const MDNS_SERVICE_TYPE: &str = "_usbip._tcp.local.";

pub const OP_REQ_DEVLIST: u16 = 0x8005;
pub const OP_REP_DEVLIST: u16 = 0x0005;
pub const OP_REQ_IMPORT: u16 = 0x8003;
pub const OP_REP_IMPORT: u16 = 0x0003;
/// usb-over-net extension (not part of USB/IP): ask which human-readable names the server's
/// devices have. Stock clients never send it; a server that does not know it just closes the
/// connection, which the client treats as "no names".
pub const OP_REQ_NAMES: u16 = 0x9001;
pub const OP_REP_NAMES: u16 = 0x1001;
/// Limits so a hostile peer cannot make us allocate without bound.
pub const MAX_NAMES: u32 = 4096;
pub const MAX_NAME_LEN: usize = 256;

/// Status codes used in `OpHeader::status` (same values as the Linux tools).
pub const ST_OK: u32 = 0;
pub const ST_NA: u32 = 1;
pub const ST_DEV_BUSY: u32 = 2;
pub const ST_DEV_ERR: u32 = 3;
pub const ST_NODEV: u32 = 4;
pub const ST_ERROR: u32 = 5;

pub const OP_HEADER_LEN: usize = 8;
pub const PATH_LEN: usize = 256;
pub const BUSID_LEN: usize = 32;
pub const USB_DEVICE_LEN: usize = 312;
pub const USB_INTERFACE_LEN: usize = 4;

// ---------------------------------------------------------------- errors

#[derive(Debug)]
pub enum ProtoError {
    Io(io::Error),
    BadVersion(u16),
    UnexpectedCode { expected: u16, got: u16 },
    /// The peer replied with a non-zero status.
    Status(u32),
    /// A URB command code we do not know.
    BadCommand(u32),
    /// A length or count field larger than we are willing to allocate.
    TooLarge(u32),
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "i/o error: {e}"),
            Self::BadVersion(v) => write!(f, "unsupported protocol version {v:#06x}"),
            Self::UnexpectedCode { expected, got } => {
                write!(f, "unexpected reply code {got:#06x} (expected {expected:#06x})")
            }
            Self::Status(s) => write!(f, "server returned status {s} ({})", status_name(*s)),
            Self::BadCommand(c) => write!(f, "unknown URB command {c:#06x}"),
            Self::TooLarge(n) => write!(f, "length/count field too large ({n})"),
        }
    }
}

impl std::error::Error for ProtoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for ProtoError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

pub fn status_name(status: u32) -> &'static str {
    match status {
        ST_OK => "ok",
        ST_NA => "not available",
        ST_DEV_BUSY => "device busy",
        ST_DEV_ERR => "device error",
        ST_NODEV => "no such device",
        ST_ERROR => "error",
        _ => "unknown",
    }
}

// ---------------------------------------------------------------- helpers

/// Encode `s` as a NUL-terminated, NUL-padded fixed-size field (truncating if needed).
pub fn encode_fixed_str<const N: usize>(s: &str) -> [u8; N] {
    let mut out = [0u8; N];
    let n = s.len().min(N - 1);
    out[..n].copy_from_slice(&s.as_bytes()[..n]);
    out
}

/// Decode a NUL-terminated fixed-size field.
pub fn decode_fixed_str(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

// ---------------------------------------------------------------- header

/// The 8-byte header that starts every operation-phase message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpHeader {
    pub version: u16,
    pub code: u16,
    pub status: u32,
}

impl OpHeader {
    pub fn new(code: u16, status: u32) -> Self {
        Self { version: USBIP_VERSION, code, status }
    }

    pub fn encode(&self) -> [u8; OP_HEADER_LEN] {
        let mut b = [0u8; OP_HEADER_LEN];
        b[0..2].copy_from_slice(&self.version.to_be_bytes());
        b[2..4].copy_from_slice(&self.code.to_be_bytes());
        b[4..8].copy_from_slice(&self.status.to_be_bytes());
        b
    }

    pub fn decode(b: &[u8; OP_HEADER_LEN]) -> Self {
        Self {
            version: u16::from_be_bytes([b[0], b[1]]),
            code: u16::from_be_bytes([b[2], b[3]]),
            status: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
        }
    }

    pub fn read_from<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut b = [0u8; OP_HEADER_LEN];
        r.read_exact(&mut b)?;
        Ok(Self::decode(&b))
    }

    /// Validate a reply header: version, code and status.
    fn check_reply(&self, expected: u16) -> Result<(), ProtoError> {
        if self.version != USBIP_VERSION {
            return Err(ProtoError::BadVersion(self.version));
        }
        if self.code != expected {
            return Err(ProtoError::UnexpectedCode { expected, got: self.code });
        }
        if self.status != ST_OK {
            return Err(ProtoError::Status(self.status));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- device

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speed {
    Unknown = 0,
    Low = 1,
    Full = 2,
    High = 3,
    Wireless = 4,
    Super = 5,
    SuperPlus = 6,
}

impl Speed {
    pub fn from_u32(v: u32) -> Self {
        match v {
            1 => Self::Low,
            2 => Self::Full,
            3 => Self::High,
            4 => Self::Wireless,
            5 => Self::Super,
            6 => Self::SuperPlus,
            _ => Self::Unknown,
        }
    }
}

/// `struct usbip_usb_device` (312 bytes on the wire).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDevice {
    pub path: String,
    pub busid: String,
    pub busnum: u32,
    pub devnum: u32,
    pub speed: Speed,
    pub id_vendor: u16,
    pub id_product: u16,
    pub bcd_device: u16,
    pub device_class: u8,
    pub device_subclass: u8,
    pub device_protocol: u8,
    pub configuration_value: u8,
    pub num_configurations: u8,
    pub num_interfaces: u8,
}

impl UsbDevice {
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&encode_fixed_str::<PATH_LEN>(&self.path));
        out.extend_from_slice(&encode_fixed_str::<BUSID_LEN>(&self.busid));
        out.extend_from_slice(&self.busnum.to_be_bytes());
        out.extend_from_slice(&self.devnum.to_be_bytes());
        out.extend_from_slice(&(self.speed as u32).to_be_bytes());
        out.extend_from_slice(&self.id_vendor.to_be_bytes());
        out.extend_from_slice(&self.id_product.to_be_bytes());
        out.extend_from_slice(&self.bcd_device.to_be_bytes());
        out.extend_from_slice(&[
            self.device_class,
            self.device_subclass,
            self.device_protocol,
            self.configuration_value,
            self.num_configurations,
            self.num_interfaces,
        ]);
    }

    pub fn decode(b: &[u8; USB_DEVICE_LEN]) -> Self {
        let be32 = |o: usize| u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let be16 = |o: usize| u16::from_be_bytes([b[o], b[o + 1]]);
        Self {
            path: decode_fixed_str(&b[0..256]),
            busid: decode_fixed_str(&b[256..288]),
            busnum: be32(288),
            devnum: be32(292),
            speed: Speed::from_u32(be32(296)),
            id_vendor: be16(300),
            id_product: be16(302),
            bcd_device: be16(304),
            device_class: b[306],
            device_subclass: b[307],
            device_protocol: b[308],
            configuration_value: b[309],
            num_configurations: b[310],
            num_interfaces: b[311],
        }
    }

    /// The `devid` used in URB headers later: `(busnum << 16) | devnum`.
    pub fn devid(&self) -> u32 {
        (self.busnum << 16) | self.devnum
    }
}

/// `struct usbip_usb_interface` (4 bytes on the wire, last byte is padding).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsbInterface {
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
}

impl UsbInterface {
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&[self.class, self.subclass, self.protocol, 0]);
    }

    pub fn decode(b: &[u8; USB_INTERFACE_LEN]) -> Self {
        Self { class: b[0], subclass: b[1], protocol: b[2] }
    }
}

/// A device plus its interfaces, as carried by `OP_REP_DEVLIST`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedDevice {
    pub device: UsbDevice,
    pub interfaces: Vec<UsbInterface>,
}

impl ExportedDevice {
    /// Builds an entry and keeps `num_interfaces` consistent with the list.
    pub fn new(mut device: UsbDevice, interfaces: Vec<UsbInterface>) -> Self {
        device.num_interfaces = interfaces.len().min(u8::MAX as usize) as u8;
        Self { device, interfaces }
    }
}

// ---------------------------------------------------------------- DEVLIST

pub fn encode_devlist_request() -> [u8; OP_HEADER_LEN] {
    OpHeader::new(OP_REQ_DEVLIST, ST_OK).encode()
}

pub fn encode_devlist_reply(devs: &[ExportedDevice]) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        OP_HEADER_LEN + 4 + devs.len() * (USB_DEVICE_LEN + 4 * USB_INTERFACE_LEN),
    );
    out.extend_from_slice(&OpHeader::new(OP_REP_DEVLIST, ST_OK).encode());
    out.extend_from_slice(&(devs.len() as u32).to_be_bytes());
    for d in devs {
        d.device.encode(&mut out);
        // Write exactly `num_interfaces` entries so the stream stays parseable.
        for i in d.interfaces.iter().take(d.device.num_interfaces as usize) {
            i.encode(&mut out);
        }
    }
    out
}

pub fn read_devlist_reply<R: Read>(r: &mut R) -> Result<Vec<ExportedDevice>, ProtoError> {
    OpHeader::read_from(r)?.check_reply(OP_REP_DEVLIST)?;
    let mut n = [0u8; 4];
    r.read_exact(&mut n)?;
    let n = u32::from_be_bytes(n);

    // Don't pre-allocate from an untrusted count.
    let mut out = Vec::new();
    for _ in 0..n {
        let mut b = [0u8; USB_DEVICE_LEN];
        r.read_exact(&mut b)?;
        let device = UsbDevice::decode(&b);
        let mut interfaces = Vec::with_capacity(device.num_interfaces as usize);
        for _ in 0..device.num_interfaces {
            let mut i = [0u8; USB_INTERFACE_LEN];
            r.read_exact(&mut i)?;
            interfaces.push(UsbInterface::decode(&i));
        }
        out.push(ExportedDevice { device, interfaces });
    }
    Ok(out)
}

// ---------------------------------------------------------------- NAMES (extension)

pub fn encode_names_request() -> [u8; OP_HEADER_LEN] {
    OpHeader::new(OP_REQ_NAMES, ST_OK).encode()
}

/// `(busid, name)` pairs. Names longer than `MAX_NAME_LEN` bytes are cut at a character boundary.
pub fn encode_names_reply(names: &[(String, String)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&OpHeader::new(OP_REP_NAMES, ST_OK).encode());
    out.extend_from_slice(&(names.len() as u32).to_be_bytes());
    for (busid, name) in names {
        out.extend_from_slice(&encode_fixed_str::<BUSID_LEN>(busid));
        let mut end = name.len().min(MAX_NAME_LEN);
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        out.extend_from_slice(&(end as u16).to_be_bytes());
        out.extend_from_slice(&name.as_bytes()[..end]);
    }
    out
}

pub fn read_names_reply<R: Read>(r: &mut R) -> Result<Vec<(String, String)>, ProtoError> {
    OpHeader::read_from(r)?.check_reply(OP_REP_NAMES)?;
    let mut n = [0u8; 4];
    r.read_exact(&mut n)?;
    let n = u32::from_be_bytes(n);
    if n > MAX_NAMES {
        return Err(ProtoError::TooLarge(n));
    }
    let mut out = Vec::new();
    for _ in 0..n {
        let mut busid = [0u8; BUSID_LEN];
        r.read_exact(&mut busid)?;
        let mut len = [0u8; 2];
        r.read_exact(&mut len)?;
        let len = usize::from(u16::from_be_bytes(len));
        if len > MAX_NAME_LEN {
            return Err(ProtoError::TooLarge(len as u32));
        }
        let mut name = vec![0u8; len];
        r.read_exact(&mut name)?;
        out.push((decode_fixed_str(&busid), String::from_utf8_lossy(&name).into_owned()));
    }
    Ok(out)
}

// ---------------------------------------------------------------- IMPORT

pub fn encode_import_request(busid: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(OP_HEADER_LEN + BUSID_LEN);
    out.extend_from_slice(&OpHeader::new(OP_REQ_IMPORT, ST_OK).encode());
    out.extend_from_slice(&encode_fixed_str::<BUSID_LEN>(busid));
    out
}

/// Successful reply: header followed by the device record.
pub fn encode_import_reply_ok(dev: &UsbDevice) -> Vec<u8> {
    let mut out = Vec::with_capacity(OP_HEADER_LEN + USB_DEVICE_LEN);
    out.extend_from_slice(&OpHeader::new(OP_REP_IMPORT, ST_OK).encode());
    dev.encode(&mut out);
    out
}

/// Failure reply: header only, no body.
pub fn encode_import_reply_err(status: u32) -> [u8; OP_HEADER_LEN] {
    debug_assert_ne!(status, ST_OK);
    OpHeader::new(OP_REP_IMPORT, status).encode()
}

pub fn read_import_reply<R: Read>(r: &mut R) -> Result<UsbDevice, ProtoError> {
    OpHeader::read_from(r)?.check_reply(OP_REP_IMPORT)?;
    let mut b = [0u8; USB_DEVICE_LEN];
    r.read_exact(&mut b)?;
    Ok(UsbDevice::decode(&b))
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn names_roundtrip_and_limits() {
        let names = vec![
            ("1-1.3".to_string(), "Logitech HD Webcam C505".to_string()),
            ("1-2".to_string(), "Kéyboard ✓".to_string()),
        ];
        let bytes = encode_names_reply(&names);
        assert_eq!(read_names_reply(&mut &bytes[..]).unwrap(), names);

        // Over-long names are cut on a character boundary, never in the middle of one.
        let long = vec![("1-9".to_string(), "é".repeat(300))];
        let cut = read_names_reply(&mut &encode_names_reply(&long)[..]).unwrap();
        assert_eq!(cut[0].1, "é".repeat(128));

        // A hostile count is refused instead of allocating.
        let mut evil = OpHeader::new(OP_REP_NAMES, ST_OK).encode().to_vec();
        evil.extend_from_slice(&(MAX_NAMES + 1).to_be_bytes());
        assert!(matches!(read_names_reply(&mut &evil[..]), Err(ProtoError::TooLarge(_))));

        // An old server just closes the connection: an I/O error, not a panic.
        assert!(read_names_reply(&mut &[][..]).is_err());
    }

    fn sample_device(busid: &str) -> UsbDevice {
        UsbDevice {
            path: format!("/sys/bus/usb/devices/{busid}"),
            busid: busid.into(),
            busnum: 1,
            devnum: 7,
            speed: Speed::High,
            id_vendor: 0x0403,
            id_product: 0x6001,
            bcd_device: 0x0600,
            device_class: 0,
            device_subclass: 0,
            device_protocol: 0,
            configuration_value: 1,
            num_configurations: 1,
            num_interfaces: 0,
        }
    }

    #[test]
    fn header_wire_layout() {
        let h = OpHeader::new(OP_REQ_DEVLIST, 0).encode();
        assert_eq!(h, [0x01, 0x11, 0x80, 0x05, 0, 0, 0, 0]);
        assert_eq!(OpHeader::decode(&h), OpHeader::new(OP_REQ_DEVLIST, 0));
    }

    #[test]
    fn usb_device_is_312_bytes_with_expected_offsets() {
        let mut buf = Vec::new();
        sample_device("1-1.2").encode(&mut buf);
        assert_eq!(buf.len(), USB_DEVICE_LEN);
        assert_eq!(&buf[256..261], b"1-1.2");
        assert_eq!(&buf[300..302], &[0x04, 0x03]); // idVendor 0x0403 (BE)
        assert_eq!(&buf[302..304], &[0x60, 0x01]); // idProduct 0x6001 (BE)
        assert_eq!(buf[309], 1); // bConfigurationValue

        let arr: [u8; USB_DEVICE_LEN] = buf.try_into().unwrap();
        assert_eq!(UsbDevice::decode(&arr), sample_device("1-1.2"));
    }

    #[test]
    fn devlist_roundtrip() {
        let devs = vec![
            ExportedDevice::new(
                sample_device("1-1.2"),
                vec![
                    UsbInterface { class: 0xff, subclass: 0xff, protocol: 0xff },
                    UsbInterface { class: 8, subclass: 6, protocol: 0x50 },
                ],
            ),
            ExportedDevice::new(sample_device("1-1.3"), vec![]),
        ];
        let bytes = encode_devlist_reply(&devs);
        let got = read_devlist_reply(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(got, devs);
    }

    #[test]
    fn empty_devlist() {
        let bytes = encode_devlist_reply(&[]);
        assert_eq!(bytes.len(), OP_HEADER_LEN + 4);
        assert!(read_devlist_reply(&mut Cursor::new(bytes)).unwrap().is_empty());
    }

    #[test]
    fn import_roundtrip_and_errors() {
        let dev = sample_device("1-1.2");
        let ok = encode_import_reply_ok(&dev);
        assert_eq!(read_import_reply(&mut Cursor::new(ok)).unwrap(), dev);

        let err = encode_import_reply_err(ST_NODEV);
        match read_import_reply(&mut Cursor::new(err)) {
            Err(ProtoError::Status(ST_NODEV)) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn import_request_layout() {
        let req = encode_import_request("1-1.2");
        assert_eq!(req.len(), OP_HEADER_LEN + BUSID_LEN);
        assert_eq!(&req[0..4], &[0x01, 0x11, 0x80, 0x03]);
        assert_eq!(decode_fixed_str(&req[OP_HEADER_LEN..]), "1-1.2");
    }

    #[test]
    fn rejects_wrong_reply_code() {
        let bytes = OpHeader::new(OP_REP_IMPORT, ST_OK).encode();
        assert!(matches!(
            read_devlist_reply(&mut Cursor::new(bytes)),
            Err(ProtoError::UnexpectedCode { .. })
        ));
    }

    #[test]
    fn long_strings_are_truncated_and_terminated() {
        let f: [u8; BUSID_LEN] = encode_fixed_str(&"x".repeat(100));
        assert_eq!(f[BUSID_LEN - 1], 0);
        assert_eq!(decode_fixed_str(&f).len(), BUSID_LEN - 1);
    }
}
