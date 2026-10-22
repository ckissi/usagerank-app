//! Keep macOS placement in AppKit points, including across displays with different scales.

use objc2::MainThreadMarker;
use objc2_app_kit::NSWindow;
use objc2_foundation::{NSPoint, NSRect, NSSize};
use tauri::{AppHandle, WebviewWindow};

use crate::{POPOVER_WIDTH, TRAY_ID};

/// Read the live status-item screen and change size and position in one main-thread operation.
pub fn place(app: &AppHandle, window: &WebviewWindow, height: Option<f64>) -> bool {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return false;
    };
    let window = window.clone();
    tray.with_inner_tray_icon(move |tray| {
        let Some(mtm) = MainThreadMarker::new() else {
            return false;
        };
        let Some(button) = tray.ns_status_item().and_then(|item| item.button(mtm)) else {
            return false;
        };
        let Some(status_window) = button.window() else {
            return false;
        };
        let Some(screen) = status_window.screen() else {
            return false;
        };
        let Ok(pointer) = window.ns_window() else {
            return false;
        };
        if pointer.is_null() {
            return false;
        }
        // Tauri owns the window. This reference stays inside its main-thread callback.
        let native = unsafe { &*pointer.cast::<NSWindow>() };
        let current = native.frame();
        let target = frame(
            status_window.frame(),
            screen.frame(),
            screen.visibleFrame(),
            height.unwrap_or(current.size.height),
        );
        // AppKit can round to backing pixels. Do not correct that rounding on each move event.
        if (current.origin.x - target.origin.x).abs() > 0.5
            || (current.origin.y - target.origin.y).abs() > 0.5
            || (current.size.width - target.size.width).abs() > 0.5
            || (current.size.height - target.size.height).abs() > 0.5
        {
            native.setFrame_display(target, true);
        }
        true
    })
    .unwrap_or(false)
}

fn frame(tray: NSRect, screen: NSRect, visible: NSRect, height: f64) -> NSRect {
    let left = visible.origin.x;
    let right = left + visible.size.width;
    let visible_top = visible.origin.y + visible.size.height;
    let screen_top = screen.origin.y + screen.size.height;
    let tray_top = tray.origin.y + tray.size.height;
    // Ignore a displaced status window's vertical anchor. Only a rect at the menu bar is valid.
    let top = if (tray_top - screen_top).abs() <= 1.0 {
        visible_top.min(tray.origin.y)
    } else {
        visible_top
    };
    let x = tray.origin.x.clamp(left, (right - POPOVER_WIDTH).max(left));
    NSRect::new(
        NSPoint::new(x, top - height),
        NSSize::new(POPOVER_WIDTH, height),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
        NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
    }

    #[test]
    fn resize_keeps_the_top_under_the_menu_bar() {
        let screen = rect(0.0, 0.0, 1512.0, 982.0);
        let visible = rect(0.0, 80.0, 1512.0, 870.0);
        let tray = rect(600.0, 950.0, 100.0, 32.0);
        for height in [760.0, 450.0, 700.0, 200.0] {
            let placed = frame(tray, screen, visible, height);
            assert_eq!(placed.origin.x, 600.0);
            assert_eq!(placed.origin.y + placed.size.height, 950.0);
        }
    }

    #[test]
    fn displaced_status_window_cannot_pull_the_popup_down() {
        let placed = frame(
            rect(600.0, 450.0, 100.0, 32.0),
            rect(0.0, 0.0, 1512.0, 982.0),
            rect(0.0, 80.0, 1512.0, 870.0),
            500.0,
        );
        assert_eq!(placed.origin, NSPoint::new(600.0, 450.0));
    }

    #[test]
    fn native_points_handle_displays_left_and_above_the_primary() {
        for (x, y) in [(-1920.0, 0.0), (0.0, 982.0), (1512.0, -300.0)] {
            let screen = rect(x, y, 1920.0, 1080.0);
            let visible = rect(x, y, 1920.0, 1056.0);
            let placed = frame(
                rect(x + 900.0, y + 1056.0, 80.0, 24.0),
                screen,
                visible,
                600.0,
            );
            assert_eq!(placed.origin, NSPoint::new(x + 900.0, y + 456.0));
        }
    }

    #[test]
    fn right_edge_and_menu_bar_auto_hide_stay_on_screen() {
        let screen = rect(0.0, 0.0, 1512.0, 982.0);
        let placed = frame(rect(1450.0, 958.0, 62.0, 24.0), screen, screen, 500.0);
        assert_eq!(placed.origin, NSPoint::new(1512.0 - POPOVER_WIDTH, 458.0));
    }
}
