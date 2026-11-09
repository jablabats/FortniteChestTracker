use std::mem::size_of;

use gpui::{Context, Window};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::{
    Foundation::{HWND, RECT},
    Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow},
    UI::{
        HiDpi::GetDpiForWindow,
        WindowsAndMessaging::{
            GWL_STYLE, GetForegroundWindow, GetWindowLongPtrW, GetWindowRect, HWND_NOTOPMOST,
            HWND_TOPMOST, SWP_FRAMECHANGED, SWP_NOACTIVATE, SetWindowLongPtrW, SetWindowPos,
            WS_CAPTION, WS_THICKFRAME,
        },
    },
};

#[derive(Default)]
pub struct Pin {
    pub enabled: bool,
    restored: Option<(RECT, isize)>,
}

impl Pin {
    pub fn compact(&self) -> bool {
        self.restored.is_some()
    }

    pub fn update<T: 'static>(&mut self, window: &Window, cx: &Context<T>) {
        let compact = self.enabled && !window.is_window_active();
        if compact == self.compact() {
            return;
        }
        let Ok(handle) = HasWindowHandle::window_handle(window) else {
            return;
        };
        let RawWindowHandle::Win32(handle) = handle.as_raw() else {
            return;
        };
        let hwnd = HWND(handle.hwnd.get() as *mut _);
        let (bounds, layer, style) = if compact {
            let Ok((original, compact)) = compact_bounds(hwnd) else {
                return;
            };
            let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) };
            self.restored = Some((original, style));
            (
                compact,
                HWND_TOPMOST,
                style & !(WS_CAPTION | WS_THICKFRAME).0 as isize,
            )
        } else {
            let (bounds, style) = self.restored.take().unwrap();
            (bounds, HWND_NOTOPMOST, style)
        };
        // Resize after GPUI finishes dispatching the activation event.
        cx.spawn(async move |_, _| {
            if let Err(error) = unsafe {
                SetWindowLongPtrW(hwnd, GWL_STYLE, style);
                SetWindowPos(
                    hwnd,
                    Some(layer),
                    bounds.left,
                    bounds.top,
                    bounds.right - bounds.left,
                    bounds.bottom - bounds.top,
                    SWP_NOACTIVATE | SWP_FRAMECHANGED,
                )
            } {
                eprintln!("Could not resize pinned window: {error}");
            }
        })
        .detach();
    }
}

fn compact_bounds(hwnd: HWND) -> windows::core::Result<(RECT, RECT)> {
    let mut original = RECT::default();
    unsafe {
        GetWindowRect(hwnd, &mut original)?;
        let foreground = GetForegroundWindow();
        let target = if foreground.is_invalid() {
            hwnd
        } else {
            foreground
        };
        let monitor = MonitorFromWindow(target, MONITOR_DEFAULTTONEAREST);
        let mut monitor_info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        GetMonitorInfoW(monitor, &mut monitor_info).ok()?;
        let dpi = GetDpiForWindow(hwnd).max(96) as i32;
        let left = monitor_info.rcWork.left + 16;
        let top = monitor_info.rcWork.top + 16;
        let width = 432 * dpi / 96;
        let height = 92 * dpi / 96;
        Ok((
            original,
            RECT {
                left,
                top,
                right: left + width,
                bottom: top + height,
            },
        ))
    }
}
