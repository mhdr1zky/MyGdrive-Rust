//! GTK4 has no global positioning API. Use EWMH only on the native X11 backend;
//! Wayland retains compositor-controlled placement. No X11 dependency is added.
use gtk::{
    glib::{self, translate::ToGlibPtr},
    prelude::*,
};
use libc::{c_char, c_int, c_long, c_uchar, c_ulong, c_void};
use std::{ffi::CStr, time::Duration};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}
impl Rect {
    fn contains(self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.width && y < self.y + self.height
    }
    fn intersection(self, other: Self) -> Option<Self> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let width = (self.x + self.width).min(other.x + other.width) - x;
        let height = (self.y + self.height).min(other.y + other.height) - y;
        (width > 0 && height > 0).then_some(Self {
            x,
            y,
            width,
            height,
        })
    }
}
fn placement(area: Rect, width: i32, height: i32, margin: i32, bottom: bool) -> Rect {
    let inset = margin.min((area.width.min(area.height) / 4).max(0));
    let width = width.min(area.width - inset * 2).max(1);
    let height = height.min(area.height - inset * 2).max(1);
    Rect {
        x: area.x + area.width - inset - width,
        y: if bottom {
            area.y + area.height - inset - height
        } else {
            area.y + inset
        },
        width,
        height,
    }
}
type Display = *mut c_void;
type Xid = c_ulong;
type GetDisplay = unsafe extern "C" fn(*mut gtk::gdk::ffi::GdkDisplay) -> Display;
type GetXid = unsafe extern "C" fn(*mut gtk::gdk::ffi::GdkSurface) -> Xid;
type Root = unsafe extern "C" fn(Display) -> Xid;
type Atom = unsafe extern "C" fn(Display, *const c_char, c_int) -> Xid;
type Property = unsafe extern "C" fn(
    Display,
    Xid,
    Xid,
    c_long,
    c_long,
    c_int,
    Xid,
    *mut Xid,
    *mut c_int,
    *mut c_ulong,
    *mut c_ulong,
    *mut *mut c_uchar,
) -> c_int;
type Free = unsafe extern "C" fn(*mut c_void) -> c_int;
type MoveResize = unsafe extern "C" fn(Display, Xid, c_int, c_int, u32, u32) -> c_int;
type Send = unsafe extern "C" fn(Display, Xid, c_int, c_long, *mut XEvent) -> c_int;
type Flush = unsafe extern "C" fn(Display) -> c_int;
#[repr(C)]
#[derive(Clone, Copy)]
struct ClientMessage {
    kind: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: Display,
    window: Xid,
    message_type: Xid,
    format: c_int,
    data: [c_long; 5],
}
#[repr(C)]
union XEvent {
    message: ClientMessage,
    padding: [c_long; 24],
}
struct X11 {
    display: GetDisplay,
    xid: GetXid,
    root: Root,
    atom: Atom,
    property: Property,
    free: Free,
    move_resize: MoveResize,
    send: Send,
    flush: Flush,
}
impl X11 {
    fn load() -> Option<Self> {
        macro_rules! symbol {
            ($name:literal, $ty:ty) => {{
                // SAFETY: Xlib/GDK remain loaded by GTK. Each signature matches
                // the installed public headers; a missing symbol disables placement.
                let pointer = unsafe {
                    libc::dlsym(libc::RTLD_DEFAULT, concat!($name, "\0").as_ptr().cast())
                };
                if pointer.is_null() {
                    return None;
                }
                unsafe { std::mem::transmute::<*mut c_void, $ty>(pointer) }
            }};
        }
        Some(Self {
            display: symbol!("gdk_x11_display_get_xdisplay", GetDisplay),
            xid: symbol!("gdk_x11_surface_get_xid", GetXid),
            root: symbol!("XDefaultRootWindow", Root),
            atom: symbol!("XInternAtom", Atom),
            property: symbol!("XGetWindowProperty", Property),
            free: symbol!("XFree", Free),
            move_resize: symbol!("XMoveResizeWindow", MoveResize),
            send: symbol!("XSendEvent", Send),
            flush: symbol!("XFlush", Flush),
        })
    }
    fn values(&self, display: Display, window: Xid, name: &CStr) -> Vec<c_long> {
        let (mut actual_type, mut format, mut count, mut remaining) = (0, 0, 0, 0);
        let mut data = std::ptr::null_mut();
        // SAFETY: valid GDK-owned X display/window; Xlib owns and allocates data.
        let status = unsafe {
            (self.property)(
                display,
                window,
                (self.atom)(display, name.as_ptr(), 0),
                0,
                4096,
                0,
                0,
                &mut actual_type,
                &mut format,
                &mut count,
                &mut remaining,
                &mut data,
            )
        };
        let values = if status == 0 && format == 32 && !data.is_null() && count <= 4096 {
            // Xlib exposes 32-bit format values in native longs, including on LP64.
            unsafe { std::slice::from_raw_parts(data.cast::<c_long>(), count as usize).to_vec() }
        } else {
            Vec::new()
        };
        if !data.is_null() {
            unsafe {
                (self.free)(data.cast());
            }
        }
        values
    }
}
fn position(window: &gtk::Window, anchor: Option<(i32, i32)>) {
    if !window.is_visible() {
        return;
    }
    let Some(surface) = window.surface() else {
        return;
    };
    let display = surface.display();
    if display.type_().name() != "GdkX11Display" {
        return;
    }
    let Some(api) = X11::load() else {
        return;
    };
    let monitors = display.monitors();
    let mut selected = display.monitor_at_surface(&surface);
    if let Some((x, y)) = anchor {
        for index in 0..monitors.n_items() {
            if let Some(monitor) = monitors
                .item(index)
                .and_then(|item| item.downcast::<gtk::gdk::Monitor>().ok())
            {
                let scale = monitor.scale_factor();
                let geometry = monitor.geometry();
                if (Rect {
                    x: geometry.x() * scale,
                    y: geometry.y() * scale,
                    width: geometry.width() * scale,
                    height: geometry.height() * scale,
                })
                .contains(x, y)
                {
                    selected = Some(monitor);
                    break;
                }
            }
        }
    }
    let Some(monitor) = selected else {
        return;
    };
    let scale = monitor.scale_factor().max(1);
    let geometry = monitor.geometry();
    let mut area = Rect {
        x: geometry.x() * scale,
        y: geometry.y() * scale,
        width: geometry.width() * scale,
        height: geometry.height() * scale,
    };
    // SAFETY: backend type was checked before using GDK's X11-only accessors.
    let (connection, xid) = unsafe {
        (
            (api.display)(display.to_glib_none().0),
            (api.xid)(surface.to_glib_none().0),
        )
    };
    if connection.is_null() || xid == 0 {
        return;
    }
    let root = unsafe { (api.root)(connection) };
    let desktop = api
        .values(connection, root, c"_NET_CURRENT_DESKTOP")
        .first()
        .copied()
        .unwrap_or(0)
        .max(0) as usize;
    let workareas = api.values(connection, root, c"_NET_WORKAREA");
    if let Some(values) = desktop.checked_mul(4).and_then(|offset| {
        offset
            .checked_add(4)
            .and_then(|end| workareas.get(offset..end))
    }) {
        let work = Rect {
            x: values[0] as i32,
            y: values[1] as i32,
            width: values[2] as i32,
            height: values[3] as i32,
        };
        if let Some(intersection) = area.intersection(work) {
            area = intersection;
        }
    }
    let extents = api.values(connection, xid, c"_NET_FRAME_EXTENTS");
    let left = extents.first().copied().unwrap_or(0).max(0) as i32;
    let right = extents.get(1).copied().unwrap_or(0).max(0) as i32;
    let top = extents.get(2).copied().unwrap_or(0).max(0) as i32;
    let bottom = extents.get(3).copied().unwrap_or(0).max(0) as i32;
    let target = placement(
        area,
        window.width().max(440) * scale + left + right,
        window.height().max(580) * scale + top + bottom,
        12 * scale,
        anchor.is_some_and(|(_, y)| y > area.y + area.height / 2),
    );
    let width = (target.width - left - right).max(1);
    let height = (target.height - top - bottom).max(1);
    // StaticGravity targets the client origin, accounting for the frame ourselves.
    let x = target.x + left;
    let y = target.y + top;
    let mut event = XEvent {
        message: ClientMessage {
            kind: 33,
            serial: 0,
            send_event: 1,
            display: connection,
            window: xid,
            message_type: unsafe { (api.atom)(connection, c"_NET_MOVERESIZE_WINDOW".as_ptr(), 0) },
            format: 32,
            data: [
                10 | (15 << 8) | (1 << 12),
                x as c_long,
                y as c_long,
                width as c_long,
                height as c_long,
            ],
        },
    };
    // SAFETY: both requests target this live GTK window on the main thread.
    unsafe {
        (api.move_resize)(connection, xid, x, y, width as u32, height as u32);
        (api.send)(connection, root, 0, (1 << 20) | (1 << 19), &mut event);
        (api.flush)(connection);
    }
}
pub fn on_right(window: &gtk::Window, anchor: Option<(i32, i32)>) {
    let weak = window.downgrade();
    glib::idle_add_local_once(move || {
        if let Some(window) = weak.upgrade() {
            position(&window, anchor);
        }
    });
    // Window managers may apply initial placement after mapping; reapply once
    // after decoration/work-area properties become available.
    let weak = window.downgrade();
    glib::timeout_add_local_once(Duration::from_millis(150), move || {
        if let Some(window) = weak.upgrade() {
            position(&window, anchor);
        }
    });
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn right_edge_fits_workarea_on_primary_secondary_and_small_monitors() {
        for area in [
            Rect {
                x: 0,
                y: 32,
                width: 1920,
                height: 1048,
            },
            Rect {
                x: -1280,
                y: 0,
                width: 1280,
                height: 984,
            },
            Rect {
                x: 1920,
                y: 40,
                width: 320,
                height: 440,
            },
        ] {
            for bottom in [false, true] {
                let result = placement(area, 440, 580, 12, bottom);
                assert_eq!(result.x + result.width, area.x + area.width - 12);
                assert!(area.contains(result.x, result.y));
                assert!(area.contains(result.x + result.width - 1, result.y + result.height - 1));
                if bottom {
                    assert_eq!(result.y + result.height, area.y + area.height - 12);
                } else {
                    assert_eq!(result.y, area.y + 12);
                }
            }
        }
    }
}
