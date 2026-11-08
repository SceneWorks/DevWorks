//! Native screen capture and input. The caller must hold the desktop lease and current consent;
//! this adapter never makes that decision. A Wayland compositor needs a separate portal-based
//! input session, so Xwayland's partial reach is not advertised as desktop control.
use devworks_core::ports::desktop::{
    DesktopAvailability, DesktopCapabilities, DesktopFault, DesktopPlatform,
};
use devworks_core::protocol::records::{DesktopInput, KeyModifier, MouseButton};
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use image::{DynamicImage, GenericImage, ImageFormat, RgbaImage};
use std::io::Cursor;
use xcap::Monitor;

#[derive(Default)]
pub struct NativeDesktop;

impl DesktopPlatform for NativeDesktop {
    fn capabilities(&self) -> DesktopCapabilities {
        let target = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        if cfg!(target_os = "linux") && wayland() {
            return DesktopCapabilities {
                target,
                capture: DesktopAvailability::Unsupported("wayland-portal-not-qualified"),
                input: DesktopAvailability::Unsupported("wayland-input-not-qualified"),
            };
        }
        let capture = if !screen_permission() {
            DesktopAvailability::PermissionRequired("screen-recording")
        } else if Monitor::all().is_err() {
            DesktopAvailability::Unsupported("screen-capture-unavailable")
        } else {
            DesktopAvailability::Available
        };
        let input = if !input_permission() {
            DesktopAvailability::PermissionRequired("accessibility-input")
        } else if Enigo::new(&Settings::default()).is_err() {
            DesktopAvailability::Unsupported("desktop-input-unavailable")
        } else {
            DesktopAvailability::Available
        };
        DesktopCapabilities {
            target,
            capture,
            input,
        }
    }

    fn capture_png(&self) -> Result<Vec<u8>, DesktopFault> {
        match self.capabilities().capture {
            DesktopAvailability::Available => {}
            DesktopAvailability::PermissionRequired(_) => {
                return Err(DesktopFault::PermissionRequired);
            }
            DesktopAvailability::Unsupported(_) => return Err(DesktopFault::Unsupported),
        }
        let monitors = Monitor::all().map_err(|_| DesktopFault::NativeFailure)?;
        if monitors.is_empty() || monitors.len() > 16 {
            return Err(DesktopFault::Unsupported);
        }
        let mut frames = Vec::with_capacity(monitors.len());
        let mut pixels = 0_u64;
        let mut captured_pixels = 0_u64;
        for monitor in monitors {
            let dimensions = u64::from(monitor.width().map_err(|_| DesktopFault::NativeFailure)?)
                * u64::from(monitor.height().map_err(|_| DesktopFault::NativeFailure)?);
            pixels = pixels
                .checked_add(dimensions)
                .ok_or(DesktopFault::NativeFailure)?;
            if pixels > 100_000_000 {
                return Err(DesktopFault::NativeFailure);
            }
            let x = monitor.x().map_err(|_| DesktopFault::NativeFailure)?;
            let y = monitor.y().map_err(|_| DesktopFault::NativeFailure)?;
            let frame = monitor
                .capture_image()
                .map_err(|_| DesktopFault::NativeFailure)?;
            captured_pixels = captured_pixels
                .checked_add(u64::from(frame.width()) * u64::from(frame.height()))
                .ok_or(DesktopFault::NativeFailure)?;
            if captured_pixels > 100_000_000 {
                return Err(DesktopFault::NativeFailure);
            }
            frames.push((x, y, frame));
        }
        let left = frames.iter().map(|(x, _, _)| i64::from(*x)).min().unwrap();
        let top = frames.iter().map(|(_, y, _)| i64::from(*y)).min().unwrap();
        let right = frames
            .iter()
            .map(|(x, _, image)| i64::from(*x) + i64::from(image.width()))
            .max()
            .unwrap();
        let bottom = frames
            .iter()
            .map(|(_, y, image)| i64::from(*y) + i64::from(image.height()))
            .max()
            .unwrap();
        let width = u32::try_from(right - left).map_err(|_| DesktopFault::NativeFailure)?;
        let height = u32::try_from(bottom - top).map_err(|_| DesktopFault::NativeFailure)?;
        if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 100_000_000 {
            return Err(DesktopFault::NativeFailure);
        }
        let mut canvas = RgbaImage::new(width, height);
        for (x, y, frame) in frames {
            canvas
                .copy_from(
                    &frame,
                    (i64::from(x) - left) as u32,
                    (i64::from(y) - top) as u32,
                )
                .map_err(|_| DesktopFault::NativeFailure)?;
        }
        let mut png = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(canvas)
            .write_to(&mut png, ImageFormat::Png)
            .map_err(|_| DesktopFault::NativeFailure)?;
        Ok(png.into_inner())
    }

    fn inject(&self, action: &DesktopInput) -> Result<(), DesktopFault> {
        match self.capabilities().input {
            DesktopAvailability::Available => {}
            DesktopAvailability::PermissionRequired(_) => {
                return Err(DesktopFault::PermissionRequired);
            }
            DesktopAvailability::Unsupported(_) => return Err(DesktopFault::Unsupported),
        }
        let mut enigo =
            Enigo::new(&Settings::default()).map_err(|_| DesktopFault::NativeFailure)?;
        match action {
            DesktopInput::Click { x, y, button } => {
                // Capture uses the bounding rectangle of every monitor. Its image origin may
                // differ from the OS virtual desktop origin when a display sits left or above
                // the primary display.
                let monitors = Monitor::all().map_err(|_| DesktopFault::NativeFailure)?;
                let left = monitors
                    .iter()
                    .map(|monitor| monitor.x().map_err(|_| DesktopFault::NativeFailure))
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .min()
                    .ok_or(DesktopFault::Unsupported)?;
                let top = monitors
                    .iter()
                    .map(|monitor| monitor.y().map_err(|_| DesktopFault::NativeFailure))
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .min()
                    .ok_or(DesktopFault::Unsupported)?;
                let target_x = x.checked_add(left).ok_or(DesktopFault::NativeFailure)?;
                let target_y = y.checked_add(top).ok_or(DesktopFault::NativeFailure)?;
                let on_monitor = monitors.iter().any(|monitor| {
                    let (Ok(mx), Ok(my), Ok(width), Ok(height)) =
                        (monitor.x(), monitor.y(), monitor.width(), monitor.height())
                    else {
                        return false;
                    };
                    i64::from(target_x) >= i64::from(mx)
                        && i64::from(target_x) < i64::from(mx) + i64::from(width)
                        && i64::from(target_y) >= i64::from(my)
                        && i64::from(target_y) < i64::from(my) + i64::from(height)
                });
                if !on_monitor {
                    return Err(DesktopFault::Unsupported);
                }
                enigo
                    .move_mouse(target_x, target_y, Coordinate::Abs)
                    .map_err(|_| DesktopFault::NativeFailure)?;
                enigo
                    .button(
                        match button {
                            MouseButton::Primary => Button::Left,
                            MouseButton::Secondary => Button::Right,
                            MouseButton::Middle => Button::Middle,
                        },
                        Direction::Click,
                    )
                    .map_err(|_| DesktopFault::NativeFailure)?;
            }
            DesktopInput::Text { text } => enigo
                .text(&text.0)
                .map_err(|_| DesktopFault::NativeFailure)?,
            DesktopInput::Key { key, modifiers } => {
                let key = key_from_label(&key.0).ok_or(DesktopFault::Unsupported)?;
                let mut pressed = Vec::new();
                for modifier in &modifiers.0 {
                    let modifier = match modifier {
                        KeyModifier::Shift => Key::Shift,
                        KeyModifier::Control => Key::Control,
                        KeyModifier::Alt => Key::Alt,
                        KeyModifier::Meta => Key::Meta,
                    };
                    if enigo.key(modifier, Direction::Press).is_err() {
                        for held in pressed.into_iter().rev() {
                            let _ = enigo.key(held, Direction::Release);
                        }
                        return Err(DesktopFault::NativeFailure);
                    }
                    pressed.push(modifier);
                }
                let result = enigo.key(key, Direction::Click);
                for held in pressed.into_iter().rev() {
                    let _ = enigo.key(held, Direction::Release);
                }
                result.map_err(|_| DesktopFault::NativeFailure)?;
            }
            DesktopInput::Scroll { x, y } => {
                if *x != 0 {
                    enigo
                        .scroll(*x, Axis::Horizontal)
                        .map_err(|_| DesktopFault::NativeFailure)?;
                }
                if *y != 0 {
                    enigo
                        .scroll(*y, Axis::Vertical)
                        .map_err(|_| DesktopFault::NativeFailure)?;
                }
            }
        }
        Ok(())
    }
}

fn key_from_label(label: &str) -> Option<Key> {
    Some(match label {
        "Enter" | "Return" => Key::Return,
        "Tab" => Key::Tab,
        "Escape" => Key::Escape,
        "Backspace" => Key::Backspace,
        "Delete" => Key::Delete,
        "Space" => Key::Space,
        "Home" => Key::Home,
        "End" => Key::End,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        "ArrowLeft" => Key::LeftArrow,
        "ArrowRight" => Key::RightArrow,
        "ArrowUp" => Key::UpArrow,
        "ArrowDown" => Key::DownArrow,
        "F1" => Key::F1,
        "F2" => Key::F2,
        "F3" => Key::F3,
        "F4" => Key::F4,
        "F5" => Key::F5,
        "F6" => Key::F6,
        "F7" => Key::F7,
        "F8" => Key::F8,
        "F9" => Key::F9,
        "F10" => Key::F10,
        "F11" => Key::F11,
        "F12" => Key::F12,
        _ => {
            let mut chars = label.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            Key::Unicode(c)
        }
    })
}

fn wayland() -> bool {
    std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v.eq_ignore_ascii_case("wayland"))
        || std::env::var_os("WAYLAND_DISPLAY").is_some()
}

#[cfg(target_os = "macos")]
fn screen_permission() -> bool {
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
    }
    unsafe { CGPreflightScreenCaptureAccess() }
}
#[cfg(not(target_os = "macos"))]
fn screen_permission() -> bool {
    true
}

#[cfg(target_os = "macos")]
fn input_permission() -> bool {
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
    }
    unsafe { AXIsProcessTrusted() }
}
#[cfg(not(target_os = "macos"))]
fn input_permission() -> bool {
    true
}
