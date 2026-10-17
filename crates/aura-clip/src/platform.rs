//! Native change tokens and "concealed content" markers per OS.

#[cfg(target_os = "macos")]
mod imp {
    use objc2::rc::autoreleasepool;
    use objc2_app_kit::NSPasteboard;

    pub const HAS_CHANGE_TOKEN: bool = true;

    /// `NSPasteboard.changeCount`. Reading it does not trigger the paste privacy prompt.
    pub fn change_token() -> Option<u64> {
        autoreleasepool(|_| Some(NSPasteboard::generalPasteboard().changeCount() as u64))
    }

    /// Markers from http://nspasteboard.org used by password managers.
    pub fn is_concealed() -> bool {
        const MARKERS: [&str; 2] = [
            "org.nspasteboard.ConcealedType",
            "org.nspasteboard.TransientType",
        ];
        autoreleasepool(|pool| {
            let Some(types) = NSPasteboard::generalPasteboard().types() else {
                return false;
            };
            types.iter().any(|t| {
                // SAFETY: the string does not outlive the pool.
                let s = unsafe { t.to_str(pool) };
                MARKERS.contains(&s)
            })
        })
    }
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::System::DataExchange::{
        GetClipboardSequenceNumber, IsClipboardFormatAvailable, RegisterClipboardFormatW,
    };

    pub const HAS_CHANGE_TOKEN: bool = true;

    pub fn change_token() -> Option<u64> {
        // SAFETY: no arguments, no preconditions.
        Some(unsafe { GetClipboardSequenceNumber() } as u64)
    }

    fn format_available(name: &str) -> bool {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is a valid NUL-terminated UTF-16 string.
        unsafe {
            let fmt = RegisterClipboardFormatW(wide.as_ptr());
            fmt != 0 && IsClipboardFormatAvailable(fmt) != 0
        }
    }

    /// Formats set by password managers to opt out of clipboard monitors and history.
    pub fn is_concealed() -> bool {
        format_available("ExcludeClipboardContentFromMonitorProcessing")
            || format_available("Clipboard Viewer Ignore")
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    use std::cell::RefCell;
    use std::time::{Duration, Instant};

    use x11rb::connection::Connection;
    use x11rb::protocol::Event;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, CreateWindowAux, WindowClass};
    use x11rb::rust_connection::RustConnection;

    pub const HAS_CHANGE_TOKEN: bool = false;

    /// Offered by KeePassXC, KWallet and other password managers (value `secret`).
    const MARKER: &str = "x-kde-passwordManagerHint";

    /// X11/Wayland have no cheap sequence number; the caller compares content.
    pub fn change_token() -> Option<u64> {
        None
    }

    /// True if the clipboard owner offers the KDE password-manager hint. Wayland
    /// compositors with the data-control protocol are asked directly; otherwise
    /// (X11, or GNOME Wayland through XWayland) the X selection's TARGETS are listed.
    pub fn is_concealed() -> bool {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            use wl_clipboard_rs::paste::{ClipboardType, Seat, get_mime_types};
            if let Ok(types) = get_mime_types(ClipboardType::Regular, Seat::Unspecified) {
                return types.contains(MARKER);
            }
        }
        X11.with(|cell| {
            let mut probe = cell.borrow_mut();
            if probe.is_none() {
                *probe = X11Probe::connect().ok();
            }
            match probe.as_ref().map(X11Probe::offers_marker) {
                Some(Ok(found)) => found,
                Some(Err(e)) => {
                    tracing::debug!("X11 TARGETS query failed: {e:#}");
                    *probe = None; // reconnect next time
                    false
                }
                None => false,
            }
        })
    }

    thread_local! {
        // Only the clipboard thread calls this; keep one X connection for it.
        static X11: RefCell<Option<X11Probe>> = const { RefCell::new(None) };
    }

    struct X11Probe {
        conn: RustConnection,
        window: u32,
        clipboard: u32,
        targets: u32,
        property: u32,
    }

    impl X11Probe {
        fn connect() -> anyhow::Result<Self> {
            let (conn, screen_num) = x11rb::connect(None)?;
            let screen = &conn.setup().roots[screen_num];
            let window = conn.generate_id()?;
            conn.create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                window,
                screen.root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new(),
            )?;
            let atom = |name: &[u8]| -> anyhow::Result<u32> {
                Ok(conn.intern_atom(false, name)?.reply()?.atom)
            };
            let (clipboard, targets, property) = (
                atom(b"CLIPBOARD")?,
                atom(b"TARGETS")?,
                atom(b"AURA_TARGETS")?,
            );
            conn.flush()?;
            Ok(Self {
                conn,
                window,
                clipboard,
                targets,
                property,
            })
        }

        fn offers_marker(&self) -> anyhow::Result<bool> {
            // If no client ever interned the marker atom, nobody can be offering it.
            let marker = self
                .conn
                .intern_atom(true, MARKER.as_bytes())?
                .reply()?
                .atom;
            if marker == x11rb::NONE {
                return Ok(false);
            }
            self.conn.convert_selection(
                self.window,
                self.clipboard,
                self.targets,
                self.property,
                x11rb::CURRENT_TIME,
            )?;
            self.conn.flush()?;
            let deadline = Instant::now() + Duration::from_millis(200);
            loop {
                match self.conn.poll_for_event()? {
                    Some(Event::SelectionNotify(ev)) if ev.requestor == self.window => {
                        if ev.property == x11rb::NONE {
                            return Ok(false); // no owner, or it refused TARGETS
                        }
                        break;
                    }
                    Some(_) => {}
                    None if Instant::now() > deadline => anyhow::bail!("TARGETS timed out"),
                    None => std::thread::sleep(Duration::from_millis(2)),
                }
            }
            let reply = self
                .conn
                .get_property(true, self.window, self.property, AtomEnum::ATOM, 0, 4096)?
                .reply()?;
            Ok(reply
                .value32()
                .is_some_and(|mut atoms| atoms.any(|a| a == marker)))
        }
    }
}

pub use imp::*;
