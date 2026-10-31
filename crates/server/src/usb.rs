//! `UsbBackend` implemented on libusb.
//!
//! Transfers use libusb's *asynchronous* API wrapped in a wait loop (the same pattern libusb's
//! own synchronous calls use). Compared with `rusb`'s blocking calls this gives us two things
//! USB/IP needs: a transfer can be cancelled mid-flight (`CMD_UNLINK`), and a cancelled or
//! timed-out transfer never silently loses bytes that had already arrived.
//!
//! NOTE: this module needs a real device to exercise; the session logic above it is covered
//! by tests with a fake backend.

use std::collections::HashMap;
use std::os::raw::{c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Mutex, MutexGuard};

use libusb1_sys as ffi;
use libusb1_sys::constants::*;
use rusb::{Context, DeviceHandle, TransferType, UsbContext};
use usbip_proto::errno;

use usbip_proto::IsoPacketDescriptor;

use crate::session::{Completed, IsoDone, IsoRequest, UsbBackend};

/// Control transfers must finish within 5 s per the USB spec.
const CONTROL_TIMEOUT_MS: u32 = 5000;
const SETUP_LEN: usize = 8;

#[derive(Default)]
struct State {
    claimed: Vec<u8>,
    /// endpoint address -> LIBUSB_TRANSFER_TYPE_* for the currently active alt settings.
    ep_types: HashMap<u8, u8>,
}

pub struct LibusbBackend {
    ctx: Context,
    handle: DeviceHandle<Context>,
    state: Mutex<State>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn rusb_errno(e: rusb::Error) -> i32 {
    use rusb::Error::*;
    match e {
        Timeout => errno::ETIMEDOUT,
        Pipe => errno::EPIPE,
        NoDevice | NotFound => errno::ENODEV,
        Overflow => errno::EOVERFLOW,
        Busy => errno::EBUSY,
        InvalidParam => errno::EINVAL,
        NotSupported => errno::ENOSYS,
        _ => errno::EIO,
    }
}

fn type_const(t: TransferType) -> u8 {
    match t {
        TransferType::Control => LIBUSB_TRANSFER_TYPE_CONTROL,
        TransferType::Isochronous => LIBUSB_TRANSFER_TYPE_ISOCHRONOUS,
        TransferType::Bulk => LIBUSB_TRANSFER_TYPE_BULK,
        TransferType::Interrupt => LIBUSB_TRANSFER_TYPE_INTERRUPT,
    }
}

impl LibusbBackend {
    /// Open the device at `bus`/`address`, detach any kernel driver and claim every interface.
    pub fn open(bus: u8, address: u8) -> rusb::Result<Self> {
        let ctx = Context::new()?;
        let device = ctx
            .devices()?
            .iter()
            .find(|d| d.bus_number() == bus && d.address() == address)
            .ok_or(rusb::Error::NoDevice)?;
        let handle = device.open()?;
        // Not supported on every platform; where it is, the kernel driver is re-attached on release.
        let _ = handle.set_auto_detach_kernel_driver(true);

        let backend = Self { ctx, handle, state: Mutex::default() };
        backend.claim_all()?;
        Ok(backend)
    }

    fn release_all(&self) {
        let mut st = lock(&self.state);
        for n in st.claimed.drain(..) {
            let _ = self.handle.release_interface(n);
        }
        st.ep_types.clear();
    }

    /// Claim every interface of the active configuration (alt setting 0) and note endpoint types.
    fn claim_all(&self) -> rusb::Result<()> {
        self.release_all();
        let cfg = match self.handle.device().active_config_descriptor() {
            Ok(c) => c,
            Err(rusb::Error::NotFound) => return Ok(()), // unconfigured: the client will SET_CONFIGURATION
            Err(e) => return Err(e),
        };
        let mut st = lock(&self.state);
        for iface in cfg.interfaces() {
            let n = iface.number();
            self.handle.claim_interface(n)?;
            st.claimed.push(n);
            if let Some(alt0) = iface.descriptors().find(|d| d.setting_number() == 0) {
                for ep in alt0.endpoint_descriptors() {
                    st.ep_types.insert(ep.address(), type_const(ep.transfer_type()));
                }
            }
        }
        Ok(())
    }

    /// Submit one transfer and wait for it, cancelling if `cancel` becomes true.
    /// Returns the number of bytes transferred (excluding the control setup packet).
    fn run_transfer(&self, kind: u8, endpoint: u8, buf: &mut [u8], timeout_ms: u32, cancel: &AtomicBool) -> Result<usize, i32> {
        // SAFETY: `transfer` is allocated here and freed here, and only after libusb has told us
        // (through `done`) that the callback ran, so libusb no longer touches it. `buf` and `done`
        // outlive the transfer for the same reason. The callback only stores into `done`.
        unsafe {
            let transfer = ffi::libusb_alloc_transfer(0);
            if transfer.is_null() {
                return Err(errno::EIO);
            }
            let done = AtomicI32::new(0);
            let user_data = &done as *const AtomicI32 as *mut c_void;
            let dev = self.handle.as_raw();

            match kind {
                LIBUSB_TRANSFER_TYPE_CONTROL => {
                    ffi::libusb_fill_control_transfer(transfer, dev, buf.as_mut_ptr(), on_complete, user_data, timeout_ms)
                }
                LIBUSB_TRANSFER_TYPE_BULK => ffi::libusb_fill_bulk_transfer(
                    transfer, dev, endpoint, buf.as_mut_ptr(), buf.len() as c_int, on_complete, user_data, timeout_ms,
                ),
                LIBUSB_TRANSFER_TYPE_INTERRUPT => ffi::libusb_fill_interrupt_transfer(
                    transfer, dev, endpoint, buf.as_mut_ptr(), buf.len() as c_int, on_complete, user_data, timeout_ms,
                ),
                _ => {
                    ffi::libusb_free_transfer(transfer);
                    return Err(errno::ENOSYS);
                }
            }

            let rc = ffi::libusb_submit_transfer(transfer);
            if rc != 0 {
                ffi::libusb_free_transfer(transfer);
                tracing::warn!("libusb refused to start a {} byte transfer on {endpoint:#04x} (libusb error {rc})", buf.len());
                return Err(libusb_errno(rc));
            }

            wait_until_done(self.ctx.as_raw(), transfer, &done, cancel);

            let (status, actual) = ((*transfer).status, (*transfer).actual_length.max(0) as usize);
            ffi::libusb_free_transfer(transfer);
            match status {
                LIBUSB_TRANSFER_COMPLETED => Ok(actual),
                LIBUSB_TRANSFER_CANCELLED => {
                    tracing::debug!("cancelled on {endpoint:#04x} after {actual} of {} bytes", buf.len());
                    Err(errno::ECONNRESET)
                }
                LIBUSB_TRANSFER_STALL => Err(errno::EPIPE),
                LIBUSB_TRANSFER_NO_DEVICE => Err(errno::ENODEV),
                LIBUSB_TRANSFER_OVERFLOW => Err(errno::EOVERFLOW),
                LIBUSB_TRANSFER_TIMED_OUT => Err(errno::ETIMEDOUT),
                _ => Err(errno::EIO),
            }
        }
    }
}

/// Drive libusb's event loop until `transfer` has completed, cancelling it if `cancel` is set.
///
/// # Safety
/// `transfer` must be a submitted transfer whose callback stores into `done`, and both must stay
/// valid until this returns.
unsafe fn wait_until_done(ctx: *mut ffi::libusb_context, transfer: *mut ffi::libusb_transfer, done: &AtomicI32, cancel: &AtomicBool) {
    let mut cancelled = false;
    while done.load(Ordering::SeqCst) == 0 {
        if !cancelled && cancel.load(Ordering::SeqCst) {
            ffi::libusb_cancel_transfer(transfer);
            cancelled = true;
        }
        let tv = libc::timeval { tv_sec: 0, tv_usec: 50_000 };
        let rc = ffi::libusb_handle_events_timeout_completed(ctx, &tv, done.as_ptr());
        if rc != 0 && rc != LIBUSB_ERROR_TIMEOUT && rc != LIBUSB_ERROR_INTERRUPTED {
            // Event handling failed; we must still wait for the callback before freeing.
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

/// An isochronous transfer that libusb is working on.
pub struct IsoPending {
    ctx: *mut ffi::libusb_context,
    transfer: *mut ffi::libusb_transfer,
    /// Boxed so its address survives moves of this struct; libusb's callback writes to it.
    done: Box<AtomicI32>,
    /// libusb lays packets out back to back; this is where each one starts.
    buf: Vec<u8>,
    starts: Vec<usize>,
    request: Vec<IsoPacketDescriptor>,
    is_in: bool,
}

// SAFETY: the raw pointers refer to a libusb context and transfer, both of which libusb allows
// to be used from any thread, and the backend that owns the context outlives every IsoPending.
unsafe impl Send for IsoPending {}

impl Drop for IsoPending {
    fn drop(&mut self) {
        // Dropped without being waited for (not expected): make sure libusb is finished with the
        // buffer before it is freed.
        // SAFETY: `transfer` is only non-null while it is submitted and not yet freed.
        unsafe {
            if !self.transfer.is_null() {
                wait_until_done(self.ctx, self.transfer, &self.done, &AtomicBool::new(true));
                ffi::libusb_free_transfer(self.transfer);
            }
        }
    }
}

fn iso_status(status: c_int) -> i32 {
    match status {
        LIBUSB_TRANSFER_COMPLETED => 0,
        LIBUSB_TRANSFER_CANCELLED => -errno::ECONNRESET,
        LIBUSB_TRANSFER_STALL => -errno::EPIPE,
        LIBUSB_TRANSFER_NO_DEVICE => -errno::ENODEV,
        LIBUSB_TRANSFER_OVERFLOW => -errno::EOVERFLOW,
        LIBUSB_TRANSFER_TIMED_OUT => -errno::ETIMEDOUT,
        _ => -errno::EPROTO, // the device or bus garbled this packet; the video driver skips it
    }
}

extern "system" fn on_complete(transfer: *mut ffi::libusb_transfer) {
    // SAFETY: user_data points at the AtomicI32 owned by the thread waiting in `run_transfer`,
    // which does not return (and so does not drop it) until this store has happened.
    unsafe {
        let done = (*transfer).user_data as *const AtomicI32;
        (*done).store(1, Ordering::SeqCst);
    }
}

fn libusb_errno(rc: c_int) -> i32 {
    match rc {
        LIBUSB_ERROR_NO_DEVICE | LIBUSB_ERROR_NOT_FOUND => errno::ENODEV,
        LIBUSB_ERROR_BUSY => errno::EBUSY,
        LIBUSB_ERROR_INVALID_PARAM => errno::EINVAL,
        LIBUSB_ERROR_PIPE => errno::EPIPE,
        LIBUSB_ERROR_OVERFLOW => errno::EOVERFLOW,
        LIBUSB_ERROR_NOT_SUPPORTED => errno::ENOSYS,
        LIBUSB_ERROR_NO_MEM => errno::ENOMEM,
        _ => errno::EIO,
    }
}

impl UsbBackend for LibusbBackend {
    fn control(&self, setup: [u8; 8], out_data: &[u8], cancel: &AtomicBool) -> Result<Completed, i32> {
        let is_in = setup[0] & 0x80 != 0;
        let w_length = u16::from_le_bytes([setup[6], setup[7]]) as usize;

        let mut buf = Vec::with_capacity(SETUP_LEN + w_length);
        buf.extend_from_slice(&setup);
        if !is_in {
            buf.extend_from_slice(&out_data[..out_data.len().min(w_length)]);
        }
        buf.resize(SETUP_LEN + w_length, 0); // libusb reads wLength from the setup packet

        let n = self.run_transfer(LIBUSB_TRANSFER_TYPE_CONTROL, 0, &mut buf, CONTROL_TIMEOUT_MS, cancel)?;
        if is_in {
            Ok(Completed { data: buf[SETUP_LEN..SETUP_LEN + n.min(w_length)].to_vec(), actual_length: n })
        } else {
            Ok(Completed { data: Vec::new(), actual_length: n })
        }
    }

    fn transfer(&self, endpoint: u8, out_data: &[u8], in_len: usize, cancel: &AtomicBool) -> Result<Completed, i32> {
        let kind = lock(&self.state).ep_types.get(&endpoint).copied().unwrap_or(LIBUSB_TRANSFER_TYPE_BULK);
        let is_in = endpoint & 0x80 != 0;
        let mut buf = if is_in { vec![0u8; in_len] } else { out_data.to_vec() };

        // No timeout: the URB runs until it completes or the client unlinks it.
        let n = self.run_transfer(kind, endpoint, &mut buf, 0, cancel)?;
        if is_in {
            buf.truncate(n);
            Ok(Completed { actual_length: n, data: buf })
        } else {
            Ok(Completed { data: Vec::new(), actual_length: n })
        }
    }

    fn clear_halt(&self, endpoint: u8) -> Result<(), i32> {
        self.handle.clear_halt(endpoint).map_err(rusb_errno)
    }

    fn set_interface(&self, interface: u8, alt: u8) -> Result<(), i32> {
        self.handle.set_alternate_setting(interface, alt).map_err(rusb_errno)?;
        // Endpoint types can differ between alt settings; refresh the ones for this interface.
        if let Ok(cfg) = self.handle.device().active_config_descriptor() {
            let mut st = lock(&self.state);
            for iface in cfg.interfaces().filter(|i| i.number() == interface) {
                if let Some(d) = iface.descriptors().find(|d| d.setting_number() == alt) {
                    for ep in d.endpoint_descriptors() {
                        st.ep_types.insert(ep.address(), type_const(ep.transfer_type()));
                    }
                }
            }
        }
        Ok(())
    }

    fn set_configuration(&self, config: u8) -> Result<(), i32> {
        // Already in that configuration (the usual case): re-applying it would only tear down
        // and re-claim every interface, which can fail on devices the kernel is still using.
        if self.handle.active_configuration().is_ok_and(|c| c == config) {
            return Ok(());
        }
        self.release_all();
        self.handle.set_active_configuration(config).map_err(rusb_errno)?;
        self.claim_all().map_err(rusb_errno)
    }

    fn reset(&self) -> Result<(), i32> {
        self.handle.reset().map_err(rusb_errno)?;
        let _ = self.claim_all(); // best effort: interfaces normally survive a reset
        Ok(())
    }

    type IsoPending = IsoPending;

    fn iso_submit(&self, req: IsoRequest) -> Result<IsoPending, i32> {
        let is_in = req.endpoint & 0x80 != 0;
        let count = req.packets.len();
        let total: usize = req.packets.iter().map(|p| p.length as usize).sum();
        if count == 0 || total > usbip_proto::MAX_TRANSFER_LEN {
            return Err(errno::EINVAL);
        }

        // libusb wants the packets back to back; the client's offsets may leave gaps.
        let mut buf = vec![0u8; total.max(1)];
        let mut starts = Vec::with_capacity(count);
        let mut pos = 0;
        for p in &req.packets {
            starts.push(pos);
            if !is_in {
                let from = (p.offset as usize).min(req.data.len());
                let to = (from + p.length as usize).min(req.data.len());
                buf[pos..pos + (to - from)].copy_from_slice(&req.data[from..to]);
            }
            pos += p.length as usize;
        }

        // SAFETY: `transfer` is freed by `IsoPending::drop`/`iso_wait` only after libusb is done
        // with it. `buf` and `done` are owned by the returned struct, and their heap addresses do
        // not change when it moves.
        unsafe {
            let transfer = ffi::libusb_alloc_transfer(count as c_int);
            if transfer.is_null() {
                return Err(errno::EIO);
            }
            let done = Box::new(AtomicI32::new(0));
            let user_data = &*done as *const AtomicI32 as *mut c_void;
            ffi::libusb_fill_iso_transfer(
                transfer,
                self.handle.as_raw(),
                req.endpoint,
                buf.as_mut_ptr(),
                total as c_int,
                count as c_int,
                on_complete,
                user_data,
                0,
            );
            let descs = (*transfer).iso_packet_desc.as_mut_ptr();
            for (i, p) in req.packets.iter().enumerate() {
                (*descs.add(i)).length = p.length;
            }
            let rc = ffi::libusb_submit_transfer(transfer);
            if rc != 0 {
                ffi::libusb_free_transfer(transfer);
                return Err(libusb_errno(rc));
            }
            Ok(IsoPending { ctx: self.ctx.as_raw(), transfer, done, buf, starts, request: req.packets, is_in })
        }
    }

    fn iso_wait(&self, mut p: IsoPending, cancel: &AtomicBool) -> Result<IsoDone, i32> {
        // SAFETY: `p.transfer` was submitted by `iso_submit` and is still live; it is freed here
        // and nulled so `Drop` does not free it again.
        unsafe {
            wait_until_done(p.ctx, p.transfer, &p.done, cancel);
            let transfer = std::mem::replace(&mut p.transfer, std::ptr::null_mut());
            let status = (*transfer).status;
            let descs = (*transfer).iso_packet_desc.as_ptr();

            let mut data = Vec::new();
            let mut packets = Vec::with_capacity(p.request.len());
            for (i, want) in p.request.iter().enumerate() {
                let d = &*descs.add(i);
                let actual = (d.actual_length as usize).min(want.length as usize);
                if p.is_in {
                    data.extend_from_slice(&p.buf[p.starts[i]..p.starts[i] + actual]);
                }
                packets.push(IsoPacketDescriptor {
                    actual_length: actual as u32,
                    status: if status == LIBUSB_TRANSFER_COMPLETED { iso_status(d.status) } else { iso_status(status) },
                    ..*want
                });
            }
            ffi::libusb_free_transfer(transfer);

            match status {
                LIBUSB_TRANSFER_CANCELLED => Err(errno::ECONNRESET),
                LIBUSB_TRANSFER_NO_DEVICE => Err(errno::ENODEV),
                _ => Ok(IsoDone { data, packets }),
            }
        }
    }
}

impl Drop for LibusbBackend {
    fn drop(&mut self) {
        self.release_all(); // kernel drivers are re-attached because auto-detach is on
    }
}
