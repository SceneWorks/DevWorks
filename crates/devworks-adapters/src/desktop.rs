//! Native screen capture and input. The caller must hold the desktop lease and current consent;
//! this adapter never makes that decision. A Wayland compositor needs a separate portal-based
//! input session, so Xwayland's partial reach is not advertised as desktop control.
use devworks_core::ports::desktop::{
    DesktopAvailability, DesktopCapabilities, DesktopFault, DesktopPlatform,
};
use devworks_core::protocol::records::{DesktopInput, KeyModifier, MouseButton};
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use image::GenericImage;
use image::{DynamicImage, ImageFormat, RgbaImage};
use std::io::Cursor;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use xcap::{Monitor, Window};

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
        } else if !capture_available() {
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
        capture_native()
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
        if own_window_targeted(action, &enigo)? {
            return Err(DesktopFault::Unsupported);
        }
        match action {
            DesktopInput::Click { x, y, button } => {
                let (target_x, target_y) = click_position(*x, *y)?;
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

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn capture_available() -> bool {
    Monitor::all().is_ok()
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn own_window_targeted(action: &DesktopInput, enigo: &Enigo) -> Result<bool, DesktopFault> {
    let point = match action {
        DesktopInput::Click { x, y, .. } => Some(click_position(*x, *y)?),
        DesktopInput::Scroll { .. } => {
            Some(enigo.location().map_err(|_| DesktopFault::NativeFailure)?)
        }
        _ => None,
    };
    for window in Window::all().map_err(|_| DesktopFault::NativeFailure)? {
        if window.pid().map_err(|_| DesktopFault::NativeFailure)? != std::process::id() {
            continue;
        }
        if let Some((x, y)) = point {
            let (left, top, width, height) = (
                window.x().map_err(|_| DesktopFault::NativeFailure)?,
                window.y().map_err(|_| DesktopFault::NativeFailure)?,
                window.width().map_err(|_| DesktopFault::NativeFailure)?,
                window.height().map_err(|_| DesktopFault::NativeFailure)?,
            );
            if i64::from(x) >= i64::from(left)
                && i64::from(x) < i64::from(left) + i64::from(width)
                && i64::from(y) >= i64::from(top)
                && i64::from(y) < i64::from(top) + i64::from(height)
            {
                return Ok(true);
            }
        } else if window
            .is_focused()
            .map_err(|_| DesktopFault::NativeFailure)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(target_os = "linux")]
fn capture_available() -> bool {
    use x11rb::connection::Connection;
    x11rb::connect(None).is_ok_and(|(connection, _)| connection.setup().roots.len() == 1)
}

#[cfg(target_os = "linux")]
fn own_window_targeted(action: &DesktopInput, enigo: &Enigo) -> Result<bool, DesktopFault> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};
    let (connection, screen_number) =
        x11rb::connect(None).map_err(|_| DesktopFault::NativeFailure)?;
    let root = connection.setup().roots[screen_number].root;
    let pid_atom = connection
        .intern_atom(false, b"_NET_WM_PID")
        .map_err(|_| DesktopFault::NativeFailure)?
        .reply()
        .map_err(|_| DesktopFault::NativeFailure)?
        .atom;
    let own_pid = std::process::id();
    let pid_of = |window| -> Result<Option<u32>, DesktopFault> {
        Ok(connection
            .get_property(false, window, pid_atom, AtomEnum::CARDINAL, 0, 1)
            .map_err(|_| DesktopFault::NativeFailure)?
            .reply()
            .map_err(|_| DesktopFault::NativeFailure)?
            .value32()
            .and_then(|mut values| values.next()))
    };
    match action {
        DesktopInput::Click { .. } | DesktopInput::Scroll { .. } => {
            let (x, y) = match action {
                DesktopInput::Click { x, y, .. } => click_position(*x, *y)?,
                _ => enigo.location().map_err(|_| DesktopFault::NativeFailure)?,
            };
            let mut windows = vec![(root, 0_usize)];
            let mut visited = 0_usize;
            while let Some((window, depth)) = windows.pop() {
                visited += 1;
                if visited > 2048 {
                    return Err(DesktopFault::Unsupported);
                }
                if pid_of(window)? == Some(own_pid) {
                    let geometry = connection
                        .get_geometry(window)
                        .map_err(|_| DesktopFault::NativeFailure)?
                        .reply()
                        .map_err(|_| DesktopFault::NativeFailure)?;
                    let location = connection
                        .translate_coordinates(window, root, 0, 0)
                        .map_err(|_| DesktopFault::NativeFailure)?
                        .reply()
                        .map_err(|_| DesktopFault::NativeFailure)?;
                    if i64::from(x) >= i64::from(location.dst_x)
                        && i64::from(x) < i64::from(location.dst_x) + i64::from(geometry.width)
                        && i64::from(y) >= i64::from(location.dst_y)
                        && i64::from(y) < i64::from(location.dst_y) + i64::from(geometry.height)
                    {
                        return Ok(true);
                    }
                }
                if depth < 3 {
                    let tree = connection
                        .query_tree(window)
                        .map_err(|_| DesktopFault::NativeFailure)?
                        .reply()
                        .map_err(|_| DesktopFault::NativeFailure)?;
                    windows.extend(tree.children.into_iter().map(|child| (child, depth + 1)));
                }
            }
        }
        _ => {
            let mut window = connection
                .get_input_focus()
                .map_err(|_| DesktopFault::NativeFailure)?
                .reply()
                .map_err(|_| DesktopFault::NativeFailure)?
                .focus;
            for _ in 0..16 {
                if window == root || window == 0 {
                    break;
                }
                if pid_of(window)? == Some(own_pid) {
                    return Ok(true);
                }
                window = connection
                    .query_tree(window)
                    .map_err(|_| DesktopFault::NativeFailure)?
                    .reply()
                    .map_err(|_| DesktopFault::NativeFailure)?
                    .parent;
            }
        }
    }
    Ok(false)
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn capture_native() -> Result<Vec<u8>, DesktopFault> {
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

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn click_position(x: i32, y: i32) -> Result<(i32, i32), DesktopFault> {
    // Image coordinates start at the virtual display bounding rectangle, which can be negative.
    let monitors = Monitor::all().map_err(|_| DesktopFault::NativeFailure)?;
    let left = monitors
        .iter()
        .map(|m| m.x().map_err(|_| DesktopFault::NativeFailure))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .min()
        .ok_or(DesktopFault::Unsupported)?;
    let top = monitors
        .iter()
        .map(|m| m.y().map_err(|_| DesktopFault::NativeFailure))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .min()
        .ok_or(DesktopFault::Unsupported)?;
    let target_x = x.checked_add(left).ok_or(DesktopFault::NativeFailure)?;
    let target_y = y.checked_add(top).ok_or(DesktopFault::NativeFailure)?;
    let on_monitor = monitors.iter().any(|m| {
        let (Ok(mx), Ok(my), Ok(w), Ok(h)) = (m.x(), m.y(), m.width(), m.height()) else {
            return false;
        };
        i64::from(target_x) >= i64::from(mx)
            && i64::from(target_x) < i64::from(mx) + i64::from(w)
            && i64::from(target_y) >= i64::from(my)
            && i64::from(target_y) < i64::from(my) + i64::from(h)
    });
    if !on_monitor {
        return Err(DesktopFault::Unsupported);
    }
    Ok((target_x, target_y))
}

#[cfg(target_os = "linux")]
fn click_position(x: i32, y: i32) -> Result<(i32, i32), DesktopFault> {
    use x11rb::connection::Connection;
    let (connection, screen_number) =
        x11rb::connect(None).map_err(|_| DesktopFault::NativeFailure)?;
    if connection.setup().roots.len() != 1 {
        return Err(DesktopFault::Unsupported);
    }
    let screen = &connection.setup().roots[screen_number];
    if x < 0
        || y < 0
        || x >= i32::from(screen.width_in_pixels)
        || y >= i32::from(screen.height_in_pixels)
    {
        return Err(DesktopFault::Unsupported);
    }
    Ok((x, y))
}

#[cfg(target_os = "linux")]
fn capture_native() -> Result<Vec<u8>, DesktopFault> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{
        ConnectionExt, ImageFormat as XImageFormat, ImageOrder, VisualClass,
    };
    let (connection, screen_number) =
        x11rb::connect(None).map_err(|_| DesktopFault::NativeFailure)?;
    let setup = connection.setup();
    if setup.roots.len() != 1 {
        return Err(DesktopFault::Unsupported);
    }
    let screen = &setup.roots[screen_number];
    let width = screen.width_in_pixels;
    let height = screen.height_in_pixels;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 100_000_000 {
        return Err(DesktopFault::Unsupported);
    }
    let format = setup
        .pixmap_formats
        .iter()
        .find(|f| f.depth == screen.root_depth)
        .ok_or(DesktopFault::Unsupported)?;
    let bpp = usize::from(format.bits_per_pixel);
    let pad = usize::from(format.scanline_pad);
    if !matches!(bpp, 16 | 24 | 32) || !matches!(pad, 8 | 16 | 32) {
        return Err(DesktopFault::Unsupported);
    }
    let visual = screen
        .allowed_depths
        .iter()
        .flat_map(|d| &d.visuals)
        .find(|v| v.visual_id == screen.root_visual && v.class == VisualClass::TRUE_COLOR)
        .ok_or(DesktopFault::Unsupported)?;
    let reply = connection
        .get_image(
            XImageFormat::Z_PIXMAP,
            screen.root,
            0,
            0,
            width,
            height,
            u32::MAX,
        )
        .map_err(|_| DesktopFault::NativeFailure)?
        .reply()
        .map_err(|_| DesktopFault::NativeFailure)?;
    if reply.depth != screen.root_depth {
        return Err(DesktopFault::NativeFailure);
    }
    let stride = (usize::from(width) * bpp).div_ceil(pad) * pad / 8;
    if reply.data.len() < stride * usize::from(height) {
        return Err(DesktopFault::NativeFailure);
    }
    let mut image = RgbaImage::new(u32::from(width), u32::from(height));
    for y in 0..usize::from(height) {
        for x in 0..usize::from(width) {
            let at = y * stride + x * (bpp / 8);
            let bytes = &reply.data[at..at + bpp / 8];
            let pixel = if setup.image_byte_order == ImageOrder::LSB_FIRST {
                bytes
                    .iter()
                    .rev()
                    .fold(0_u32, |acc, byte| (acc << 8) | u32::from(*byte))
            } else {
                bytes
                    .iter()
                    .fold(0_u32, |acc, byte| (acc << 8) | u32::from(*byte))
            };
            image.put_pixel(
                x as u32,
                y as u32,
                image::Rgba([
                    color_channel(pixel, visual.red_mask)?,
                    color_channel(pixel, visual.green_mask)?,
                    color_channel(pixel, visual.blue_mask)?,
                    255,
                ]),
            );
        }
    }
    let mut png = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image)
        .write_to(&mut png, ImageFormat::Png)
        .map_err(|_| DesktopFault::NativeFailure)?;
    Ok(png.into_inner())
}

#[cfg(target_os = "linux")]
fn color_channel(pixel: u32, mask: u32) -> Result<u8, DesktopFault> {
    if mask == 0 {
        return Err(DesktopFault::Unsupported);
    }
    let shift = mask.trailing_zeros();
    let max = mask >> shift;
    let value = (pixel & mask) >> shift;
    Ok(((u64::from(value) * 255 + u64::from(max) / 2) / u64::from(max)) as u8)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #[test]
    fn x11_true_color_masks_decode_native_pixels() {
        use super::color_channel;
        let pixel = 0x0012_80ff;
        assert_eq!(color_channel(pixel, 0x00ff_0000).unwrap(), 0x12);
        assert_eq!(color_channel(pixel, 0x0000_ff00).unwrap(), 0x80);
        assert_eq!(color_channel(pixel, 0x0000_00ff).unwrap(), 0xff);
        assert_eq!(color_channel(0xf800, 0xf800).unwrap(), 255);
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
