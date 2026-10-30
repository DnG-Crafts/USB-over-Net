mod advertise;
mod devices;
mod session;
mod usb;

use std::collections::HashSet;
use std::net::{Shutdown, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use usbip_proto::{
    decode_fixed_str, encode_devlist_reply, encode_import_reply_err, encode_import_reply_ok,
    encode_names_reply, ExportedDevice, OpHeader, BUSID_LEN, OP_HEADER_LEN, OP_REP_DEVLIST, OP_REQ_DEVLIST,
    OP_REQ_IMPORT, OP_REQ_NAMES, ST_DEV_BUSY, ST_DEV_ERR, ST_ERROR, ST_NODEV, USBIP_VERSION,
};

use devices::VidPid;
use usb::LibusbBackend;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Parser, Debug)]
#[command(version, about = "Userspace USB/IP server (libusb based)")]
struct Args {
    /// Address to listen on. The default is every network interface so clients can find this
    /// server. There is no TLS or authentication yet, so use `--allow` and a trusted network;
    /// pass 127.0.0.1:3240 to keep it local.
    #[arg(long, default_value = "0.0.0.0:3240")]
    listen: SocketAddr,

    /// Only export devices with this VID:PID (hex), e.g. `--allow 0403:6001`.
    /// Repeatable. If omitted, every non-hub device is exported.
    #[arg(long = "allow", value_name = "VID:PID")]
    allow: Vec<VidPid>,

    /// Name shown to clients that discover this server (default: the host name).
    #[arg(long)]
    name: Option<String>,

    /// Do not announce this server on the LAN with mDNS.
    #[arg(long)]
    no_mdns: bool,
}

/// Tracks which devices are currently attached so a second client gets "busy".
#[derive(Default)]
struct Registry {
    in_use: Mutex<HashSet<String>>,
}

/// Held for the lifetime of an attachment; releases the device when dropped.
struct Lease {
    registry: Arc<Registry>,
    busid: String,
}

impl Registry {
    fn try_acquire(self: &Arc<Self>, busid: &str) -> Option<Lease> {
        let mut set = self.in_use.lock().unwrap_or_else(|e| e.into_inner());
        set.insert(busid.to_string()).then(|| Lease { registry: Arc::clone(self), busid: busid.to_string() })
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.registry.in_use.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.busid);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let args = Args::parse();
    let allow: Arc<[VidPid]> = Arc::from(args.allow);
    let registry = Arc::new(Registry::default());

    let listener = TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("binding {}", args.listen))?;
    info!("listening on {}", args.listen);
    if allow.is_empty() {
        info!("no --allow filter: all non-hub devices are exported");
    }
    if !listener.local_addr()?.ip().is_loopback() {
        warn!(
            "no authentication: anyone on this network can attach the exported devices. \
             Limit them with --allow VID:PID and do not expose port {} to the internet.",
            listener.local_addr()?.port()
        );
    }

    // Keep the announcement alive for as long as we run. Loopback-only servers are not advertised.
    let _advert = if args.no_mdns {
        None
    } else if listener.local_addr()?.ip().is_loopback() {
        info!("listening on loopback only: not announcing on the LAN");
        None
    } else {
        let name = args.name.clone().unwrap_or_else(advertise::local_hostname);
        match advertise::Advertisement::start(&name, listener.local_addr()?.port()) {
            Ok(a) => {
                info!("announcing as \"{name}\" (_usbip._tcp)");
                Some(a)
            }
            Err(e) => {
                warn!("mDNS announcement disabled: {e:#}");
                None
            }
        }
    };

    loop {
        let (sock, peer) = listener.accept().await?;
        sock.set_nodelay(true).ok();
        let (allow, registry) = (Arc::clone(&allow), Arc::clone(&registry));
        tokio::spawn(async move {
            if let Err(e) = handle_conn(sock, allow, registry).await {
                warn!(%peer, "connection ended: {e:#}");
            }
        });
    }
}

async fn list_devices(allow: &Arc<[VidPid]>) -> Result<Vec<ExportedDevice>> {
    let allow = Arc::clone(allow);
    // libusb enumeration is blocking.
    let devs = tokio::task::spawn_blocking(move || devices::enumerate(&allow)).await??;
    Ok(devs)
}

async fn handle_conn(mut sock: TcpStream, allow: Arc<[VidPid]>, registry: Arc<Registry>) -> Result<()> {
    let mut raw = [0u8; OP_HEADER_LEN];
    timeout(REQUEST_TIMEOUT, sock.read_exact(&mut raw))
        .await
        .context("timed out waiting for request")??;
    let hdr = OpHeader::decode(&raw);
    if hdr.version != USBIP_VERSION {
        warn!("client speaks version {:#06x}, expected {USBIP_VERSION:#06x}", hdr.version);
    }

    match hdr.code {
        OP_REQ_DEVLIST => {
            let devs = match list_devices(&allow).await {
                Ok(d) => d,
                Err(e) => {
                    warn!("DEVLIST failed: {e:#}");
                    sock.write_all(&OpHeader::new(OP_REP_DEVLIST, ST_ERROR).encode()).await?;
                    sock.shutdown().await.ok();
                    return Ok(());
                }
            };
            info!("DEVLIST: returning {} device(s)", devs.len());
            sock.write_all(&encode_devlist_reply(&devs)).await?;
            sock.shutdown().await.ok();
            Ok(())
        }
        OP_REQ_NAMES => {
            // Extension: device names for the client's list. Failure just means "no names".
            let allow = Arc::clone(&allow);
            let names = tokio::task::spawn_blocking(move || devices::names(&allow)).await?.unwrap_or_else(|e| {
                warn!("NAMES failed: {e:#}");
                Vec::new()
            });
            info!("NAMES: returning {} name(s)", names.len());
            sock.write_all(&encode_names_reply(&names)).await?;
            sock.shutdown().await.ok();
            Ok(())
        }
        OP_REQ_IMPORT => {
            let mut busid_raw = [0u8; BUSID_LEN];
            timeout(REQUEST_TIMEOUT, sock.read_exact(&mut busid_raw))
                .await
                .context("timed out waiting for busid")??;
            let busid = decode_fixed_str(&busid_raw);
            import(sock, &busid, &allow, &registry).await
        }
        other => bail!("unsupported operation code {other:#06x}"),
    }
}

/// Reject an IMPORT with `status` and close the connection.
async fn refuse(mut sock: TcpStream, status: u32) -> Result<()> {
    sock.write_all(&encode_import_reply_err(status)).await?;
    sock.shutdown().await.ok();
    Ok(())
}

async fn import(sock: TcpStream, busid: &str, allow: &Arc<[VidPid]>, registry: &Arc<Registry>) -> Result<()> {
    let devs = match list_devices(allow).await {
        Ok(d) => d,
        Err(e) => {
            warn!("IMPORT {busid}: device enumeration failed: {e:#}");
            return refuse(sock, ST_ERROR).await;
        }
    };
    let Some(exported) = devs.into_iter().find(|d| d.device.busid == busid) else {
        info!("IMPORT {busid}: no such device (or not allowed)");
        return refuse(sock, ST_NODEV).await;
    };
    let dev = exported.device;
    let label = format!("{busid} ({:04x}:{:04x})", dev.id_vendor, dev.id_product);

    let Some(lease) = registry.try_acquire(busid) else {
        info!("IMPORT {label}: busy");
        return refuse(sock, ST_DEV_BUSY).await;
    };

    // Open and claim before telling the client "ok", so a device we cannot drive is refused cleanly.
    let (bus, addr) = (dev.busnum as u8, dev.devnum as u8);
    let backend = match tokio::task::spawn_blocking(move || LibusbBackend::open(bus, addr)).await? {
        Ok(b) => Arc::new(b),
        Err(e) => {
            warn!("IMPORT {label}: cannot open device: {e}");
            return refuse(sock, ST_DEV_ERR).await;
        }
    };

    let mut sock = sock;
    sock.write_all(&encode_import_reply_ok(&dev)).await?;
    sock.flush().await?;
    info!("{label}: attached");

    // The URB phase is blocking, thread-based code; hand the socket over to it.
    let std_sock = sock.into_std()?;
    std_sock.set_nonblocking(false)?;
    let reader = std_sock.try_clone()?;
    let closer = std_sock.try_clone()?;
    let outcome = tokio::task::spawn_blocking(move || {
        let r = session::run(backend, reader, std_sock); // `backend` is dropped (device released) in here
        let _ = closer.shutdown(Shutdown::Both);
        r
    })
    .await?;

    drop(lease); // only now may another client take the device
    match outcome {
        Ok(()) => info!("{label}: detached"),
        Err(e) => warn!("{label}: session ended: {e}"),
    }
    Ok(())
}
