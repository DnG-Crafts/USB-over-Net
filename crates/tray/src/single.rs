//! Only one copy of the app should run (it owns the tray icon). A second launch, for example from
//! the Start menu while the first one sits in the tray, asks the first to show its window and exits.
//!
//! The first copy listens on a fixed local port; the handshake lets us tell it apart from some
//! unrelated program that happens to use the same port.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

const ADDR: &str = "127.0.0.1:38417";
const HELLO: &[u8] = b"usb-over-net:show\n";
const REPLY: &[u8] = b"usb-over-net:ok\n";
const IO_TIMEOUT: Duration = Duration::from_millis(700);

pub enum Instance {
    /// We are the first copy; keep the listener and `serve` it.
    First(TcpListener),
    /// Another copy was running and has been asked to show itself. Exit.
    Other,
    /// The port is taken by something that is not us (or could not be used); just run normally.
    Unguarded,
}

pub fn acquire() -> Instance {
    acquire_at(ADDR)
}

fn acquire_at(addr: &str) -> Instance {
    match TcpListener::bind(addr) {
        Ok(listener) => Instance::First(listener),
        Err(_) if ask_to_show(addr) => Instance::Other,
        Err(_) => Instance::Unguarded,
    }
}

fn ask_to_show(addr: &str) -> bool {
    let Ok(sock_addr) = addr.parse() else { return false };
    let Ok(mut s) = TcpStream::connect_timeout(&sock_addr, IO_TIMEOUT) else { return false };
    let _ = s.set_read_timeout(Some(IO_TIMEOUT));
    let _ = s.set_write_timeout(Some(IO_TIMEOUT));
    if s.write_all(HELLO).is_err() {
        return false;
    }
    let mut buf = [0u8; 32];
    matches!(s.read(&mut buf), Ok(n) if &buf[..n] == REPLY)
}

/// Answer other launches in the background; `on_show` runs whenever one asks us to show the window.
pub fn serve(listener: TcpListener, on_show: impl Fn() + Send + 'static) {
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
            let mut buf = [0u8; 32];
            if matches!(stream.read(&mut buf), Ok(n) if &buf[..n] == HELLO) {
                let _ = stream.write_all(REPLY);
                on_show();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn free_addr() -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().to_string()
    }

    #[test]
    fn second_launch_asks_the_first_to_show_itself() {
        let addr = free_addr();
        let Instance::First(listener) = acquire_at(&addr) else { panic!("expected to be first") };
        let shown = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&shown);
        serve(listener, move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });

        assert!(matches!(acquire_at(&addr), Instance::Other));
        assert!(matches!(acquire_at(&addr), Instance::Other));
        thread::sleep(Duration::from_millis(100));
        assert_eq!(shown.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn an_unrelated_program_on_the_port_does_not_block_us() {
        let addr = free_addr();
        let squatter = TcpListener::bind(&addr).unwrap(); // accepts nothing, answers nothing
        assert!(matches!(acquire_at(&addr), Instance::Unguarded));
        drop(squatter);
    }
}
