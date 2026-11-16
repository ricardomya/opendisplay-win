//! Inject receiver input (§6.1) into Windows, mapped onto the streamed
//! monitor's rectangle. Finger touches become real touch events in tablet
//! touch mode (the default) and mouse events otherwise; scroll drives the
//! wheel, Pencil is a synthetic pen with pressure and tilt.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

use opendisplay_proto::control::{
    Key, Pencil, PencilPhase, Pointer, PointerScroll, Proximity, Scroll, Touch, TouchPhase, Touches,
};
use tracing::{debug, warn};
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::UI::Controls::{
    CreateSyntheticPointerDevice, HSYNTHETICPOINTERDEVICE,
    POINTER_FEEDBACK_DEFAULT, POINTER_TYPE_INFO, POINTER_TYPE_INFO_0,
};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Input::Pointer::*;
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, PEN_MASK_PRESSURE, PEN_MASK_TILT_X, PEN_MASK_TILT_Y, PT_PEN, PT_TOUCH,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SetCursorPos,
};

/// Wheel units per video pixel of finger travel (120 = one notch).
const WHEEL_PER_PIXEL: f64 = 2.0;

/// Finger touches become real Windows touch events instead of mouse events.
/// The tray app writes this; the injector reads it per event, so a toggle
/// takes effect immediately.
pub static TOUCH_MODE: AtomicBool = AtomicBool::new(true);

pub fn set_touch_mode(on: bool) {
    TOUCH_MODE.store(on, Ordering::Relaxed);
    tracing::info!("input: tablet touch mode {on}");
}

// POINTER_INFO's touch masks are not exported by the windows crate.
const TOUCH_MASK_CONTACTAREA: u32 = 0x1;
const TOUCH_MASK_ORIENTATION: u32 = 0x2;
const TOUCH_MASK_PRESSURE: u32 = 0x4;


/// HID Keyboard/Keypad usage -> PC/AT set-1 scan code. Scan codes keep the
/// Windows keyboard layout and IME in charge of text, the Windows analogue of
/// the macOS injector's keycode path. `None` = usage we do not forward.
fn hid_to_scan(code: u32) -> Option<(u16, bool)> {
    const LETTERS: [u16; 26] = [
        0x1E, 0x30, 0x2E, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31, 0x18,
        0x19, 0x10, 0x13, 0x1F, 0x14, 0x16, 0x2F, 0x11, 0x2D, 0x15, 0x2C,
    ];
    const DIGITS: [u16; 10] = [0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B];
    const FKEYS: [u16; 12] = [0x3B, 0x3C, 0x3D, 0x3E, 0x3F, 0x40, 0x41, 0x42, 0x43, 0x44, 0x57, 0x58];
    match code {
        0x04..=0x1D => Some((LETTERS[(code - 0x04) as usize], false)),
        0x1E..=0x27 => Some((DIGITS[(code - 0x1E) as usize], false)),
        0x3A..=0x45 => Some((FKEYS[(code - 0x3A) as usize], false)),
        0x28 => Some((0x1C, false)),         // Enter
        0x29 => Some((0x01, false)),         // Escape
        0x2A => Some((0x0E, false)),         // Backspace
        0x2B => Some((0x0F, false)),         // Tab
        0x2C => Some((0x39, false)),         // Space
        0x2D => Some((0x0C, false)),         // -
        0x2E => Some((0x0D, false)),         // =
        0x2F => Some((0x1A, false)),         // [
        0x30 => Some((0x1B, false)),         // ]
        0x31 => Some((0x2B, false)),         // backslash
        0x33 => Some((0x27, false)),         // ;
        0x34 => Some((0x28, false)),         // '
        0x35 => Some((0x29, false)),         // `
        0x36 => Some((0x33, false)),         // ,
        0x37 => Some((0x34, false)),         // .
        0x38 => Some((0x35, false)),         // /
        0x39 => Some((0x3A, false)),         // Caps Lock
        0x46 => Some((0x37, true)),          // Print Screen
        0x47 => Some((0x46, false)),         // Scroll Lock
        0x49 => Some((0x52, true)),          // Insert
        0x4A => Some((0x47, true)),          // Home
        0x4B => Some((0x49, true)),          // Page Up
        0x4C => Some((0x53, true)),          // Delete
        0x4D => Some((0x4F, true)),          // End
        0x4E => Some((0x51, true)),          // Page Down
        0x4F => Some((0x4D, true)),          // Right
        0x50 => Some((0x4B, true)),          // Left
        0x51 => Some((0x50, true)),          // Down
        0x52 => Some((0x48, true)),          // Up
        0x53 => Some((0x45, false)),         // Num Lock
        0x54 => Some((0x35, true)),          // Keypad /
        0x55 => Some((0x37, false)),         // Keypad *
        0x56 => Some((0x4A, false)),         // Keypad -
        0x57 => Some((0x4E, false)),         // Keypad +
        0x58 => Some((0x1C, true)),          // Keypad Enter
        0x59 => Some((0x4F, false)),         // Keypad 1
        0x5A => Some((0x50, false)),         // Keypad 2
        0x5B => Some((0x51, false)),         // Keypad 3
        0x5C => Some((0x4B, false)),         // Keypad 4
        0x5D => Some((0x4C, false)),         // Keypad 5
        0x5E => Some((0x4D, false)),         // Keypad 6
        0x5F => Some((0x47, false)),         // Keypad 7
        0x60 => Some((0x48, false)),         // Keypad 8
        0x61 => Some((0x49, false)),         // Keypad 9
        0x62 => Some((0x52, false)),         // Keypad 0
        0x63 => Some((0x53, false)),         // Keypad .
        0x64 => Some((0x56, false)),         // Non-US backslash
        0x65 => Some((0x5D, true)),          // Application
        0x67 => Some((0x59, false)),         // Keypad =
        0x68 => Some((0x64, false)),         // F13
        0x69 => Some((0x65, false)),
        0x6A => Some((0x66, false)),
        0x6B => Some((0x67, false)),
        0x6C => Some((0x68, false)),
        0x6D => Some((0x69, false)),
        0x6E => Some((0x6A, false)),
        0x6F => Some((0x6B, false)),
        0x70 => Some((0x6C, false)),
        0x71 => Some((0x6D, false)),
        0x72 => Some((0x6E, false)),
        0x73 => Some((0x76, false)),         // F24
        0xE0 => Some((0x1D, false)),         // Left Control
        0xE1 => Some((0x2A, false)),         // Left Shift
        0xE2 => Some((0x38, false)),         // Left Alt
        0xE3 => Some((0x5B, true)),          // Left GUI
        0xE4 => Some((0x1D, true)),          // Right Control
        0xE5 => Some((0x36, false)),         // Right Shift
        0xE6 => Some((0x38, true)),          // Right Alt
        0xE7 => Some((0x5C, true)),          // Right GUI
        _ => None,
    }
}

pub struct Injector {
    rect: RECT,
    pen: Option<HSYNTHETICPOINTERDEVICE>,
    pen_tried: bool,
    pen_down: bool,
    pen_in_range: bool,
    touch: Option<HSYNTHETICPOINTERDEVICE>,
    touch_tried: bool,
    mouse_down: bool,
    wheel_acc: (f64, f64),
    /// HID usages this session pressed, so a reset never releases a key that
    /// belongs to a local keyboard.
    keys_down: HashSet<u32>,
    /// Pointer button this session holds (0 = none), 1/2/3.
    pointer_button: u32,
    /// A touch contact this session injected and has not lifted yet.
    touch_active: bool,
    touch_pos: POINT,
    /// Active multi-touch contacts, keyed by the receiver's contact id.
    multi_active: HashMap<u32, POINT>,
}

impl Injector {
    pub fn new(rect: RECT) -> Self {
        Self {
            rect,
            pen: None,
            pen_tried: false,
            pen_down: false,
            pen_in_range: false,
            touch: None,
            touch_tried: false,
            mouse_down: false,
            wheel_acc: (0.0, 0.0),
            keys_down: HashSet::new(),
            pointer_button: 0,
            touch_active: false,
            touch_pos: POINT { x: 0, y: 0 },
            multi_active: HashMap::new(),
        }
    }

    pub fn set_rect(&mut self, rect: RECT) {
        self.rect = rect;
    }

    fn to_screen(&self, x: f64, y: f64) -> POINT {
        let w = (self.rect.right - self.rect.left) as f64;
        let h = (self.rect.bottom - self.rect.top) as f64;
        POINT {
            x: self.rect.left + (x.clamp(0.0, 1.0) * (w - 1.0)).round() as i32,
            y: self.rect.top + (y.clamp(0.0, 1.0) * (h - 1.0)).round() as i32,
        }
    }

    fn buttons(flags: MOUSE_EVENT_FLAGS, data: i32) {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: data as u32,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
    }

    pub fn touch(&mut self, t: &Touch) {
        let p = self.to_screen(t.x, t.y);
        self.touch_pos = p;
        // A legacy single touch replaces any multi-touch gesture in flight.
        if !self.multi_active.is_empty() {
            self.release_multi(false);
        }

        if TOUCH_MODE.load(Ordering::Relaxed) && self.touch_device().is_some() {
            // A mode switch mid-press must not leave a mouse button down.
            if self.mouse_down {
                Self::buttons(MOUSEEVENTF_LEFTUP, 0);
                self.mouse_down = false;
            }
            self.inject_touch(&t.phase, p.x, p.y);
            return;
        }

        // Leaving touch mode mid-press: lift the contact before the mouse path.
        if self.touch_active {
            self.inject_touch_raw(POINTER_FLAG_UP, 0, p.x, p.y);
            self.touch_active = false;
        }
        unsafe {
            let _ = SetCursorPos(p.x, p.y);
        }
        match t.phase {
            TouchPhase::Began if !self.mouse_down => {
                Self::buttons(MOUSEEVENTF_LEFTDOWN, 0);
                self.mouse_down = true;
            }
            TouchPhase::Ended | TouchPhase::Cancelled if self.mouse_down => {
                Self::buttons(MOUSEEVENTF_LEFTUP, 0);
                self.mouse_down = false;
            }
            _ => {}
        }
    }

    fn inject_touch(&mut self, phase: &TouchPhase, x: i32, y: i32) {
        let down = matches!(phase, TouchPhase::Began);
        let cancelled = matches!(phase, TouchPhase::Cancelled);
        let up = matches!(phase, TouchPhase::Ended) || cancelled;
        if down && self.touch_active {
            // Defensive: a began without an end would otherwise stack contacts.
            self.inject_touch_raw(POINTER_FLAG_UP | POINTER_FLAG_CANCELED, 0, x, y);
            self.touch_active = false;
        }
        if !down && !self.touch_active {
            return;
        }
        let flags = if down {
            POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
        } else if up {
            // A cancelled touch (a second finger joined for a scroll) must not
            // be interpreted as a tap by the target window.
            if cancelled {
                POINTER_FLAG_UP | POINTER_FLAG_CANCELED
            } else {
                POINTER_FLAG_UP
            }
        } else {
            POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
        };
        self.inject_touch_raw(flags, 0, x, y);
        self.touch_active = !up;
    }

    fn touch_device(&mut self) -> Option<HSYNTHETICPOINTERDEVICE> {
        if !self.touch_tried {
            self.touch_tried = true;
            // InjectTouchInput rejects synthetic contacts on this desktop
            // (E_INVALIDARG even on the primary monitor); the synthetic
            // pointer device is the path Windows accepts for touch as well as
            // for pen.
            match unsafe { CreateSyntheticPointerDevice(PT_TOUCH, 10, POINTER_FEEDBACK_DEFAULT) } {
                Ok(d) => self.touch = Some(d),
                Err(e) => warn!("no synthetic touch ({e}); finger touches stay mouse events"),
            }
        }
        self.touch
    }

    fn touch_info(id: u32, flags: POINTER_FLAGS, x: i32, y: i32) -> POINTER_TYPE_INFO {
        let contact = RECT {
            left: x - 3,
            top: y - 3,
            right: x + 3,
            bottom: y + 3,
        };
        POINTER_TYPE_INFO {
            r#type: PT_TOUCH,
            Anonymous: POINTER_TYPE_INFO_0 {
                touchInfo: POINTER_TOUCH_INFO {
                    pointerInfo: POINTER_INFO {
                        pointerType: PT_TOUCH,
                        pointerId: id,
                        pointerFlags: flags,
                        ptPixelLocation: POINT { x, y },
                        ..Default::default()
                    },
                    touchFlags: 0,
                    touchMask: TOUCH_MASK_CONTACTAREA
                        | TOUCH_MASK_ORIENTATION
                        | TOUCH_MASK_PRESSURE,
                    rcContact: contact,
                    rcContactRaw: contact,
                    orientation: 90,
                    pressure: 32000,
                },
            },
        }
    }

    fn inject_touch_raw(&mut self, flags: POINTER_FLAGS, id: u32, x: i32, y: i32) {
        let Some(dev) = self.touch_device() else { return };
        let info = Self::touch_info(id, flags, x, y);
        if let Err(e) = unsafe { InjectSyntheticPointerInput(dev, &[info]) } {
            debug!("touch injection failed: {e}");
        }
    }

    /// Full multi-touch snapshot from tablet touch mode. Windows receives one
    /// contact per finger, so apps that support touch can pinch, pan and use
    /// multi-finger gestures natively.
    pub fn touches(&mut self, t: &Touches) {
        if !TOUCH_MODE.load(Ordering::Relaxed) || self.touch_device().is_none() {
            self.release_multi(t.cancelled);
            return;
        }
        // A legacy single contact and a snapshot must not coexist.
        if self.touch_active {
            self.inject_touch_raw(
                POINTER_FLAG_UP | POINTER_FLAG_CANCELED,
                0,
                self.touch_pos.x,
                self.touch_pos.y,
            );
            self.touch_active = false;
        }

        let Some(dev) = self.touch_device() else { return };
        // Each InjectSyntheticPointerInput call is a complete frame: every
        // active contact must be in it, or Windows lifts the ones left out.
        let mut frame: Vec<POINTER_TYPE_INFO> = Vec::new();
        let mut now: HashMap<u32, POINT> = HashMap::new();
        for c in t.contacts.iter().take(10) {
            if c.id == 0 || !c.x.is_finite() || !c.y.is_finite() {
                continue;
            }
            let p = self.to_screen(c.x, c.y);
            now.insert(c.id, p);
            let flags = if self.multi_active.contains_key(&c.id) {
                POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
            } else {
                POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
            };
            frame.push(Self::touch_info(c.id, flags, p.x, p.y));
        }

        let missing: Vec<(u32, POINT)> = self
            .multi_active
            .iter()
            .filter(|(id, _)| !now.contains_key(id))
            .map(|(id, p)| (*id, *p))
            .collect();
        for (id, p) in missing {
            let flags = if t.cancelled {
                POINTER_FLAG_UP | POINTER_FLAG_CANCELED
            } else {
                POINTER_FLAG_UP
            };
            frame.push(Self::touch_info(id, flags, p.x, p.y));
        }
        if !frame.is_empty() {
            if let Err(e) = unsafe { InjectSyntheticPointerInput(dev, &frame) } {
                debug!("multi-touch injection failed: {e}");
            }
        }
        self.multi_active = now;
    }

    fn release_multi(&mut self, cancelled: bool) {
        let Some(dev) = self.touch_device() else {
            self.multi_active.clear();
            return;
        };
        let flags = if cancelled {
            POINTER_FLAG_UP | POINTER_FLAG_CANCELED
        } else {
            POINTER_FLAG_UP
        };
        let frame: Vec<POINTER_TYPE_INFO> = self
            .multi_active
            .drain()
            .map(|(id, p)| Self::touch_info(id, flags, p.x, p.y))
            .collect();
        if !frame.is_empty() {
            let _ = unsafe { InjectSyntheticPointerInput(dev, &frame) };
        }
    }

    pub fn scroll(&mut self, s: &Scroll) {
        self.wheel(s.dx, s.dy);
    }

    /// Trackpad scroll from the pv 4 `pointerScroll` message: same video-pixel
    /// units and natural-scrolling sign as the legacy `scroll`.
    pub fn pointer_scroll(&mut self, s: &PointerScroll) {
        self.wheel(s.dx, s.dy);
    }

    fn wheel(&mut self, dx: f64, dy: f64) {
        // Natural-scrolling sign (§6.1): fingers down => dy > 0 => content
        // moves down, which on Windows is a positive (up) wheel delta.
        self.wheel_acc.0 += -dx * WHEEL_PER_PIXEL;
        self.wheel_acc.1 += dy * WHEEL_PER_PIXEL;
        let v = self.wheel_acc.1.trunc();
        if v != 0.0 {
            self.wheel_acc.1 -= v;
            Self::buttons(MOUSEEVENTF_WHEEL, v as i32);
        }
        let h = self.wheel_acc.0.trunc();
        if h != 0.0 {
            self.wheel_acc.0 -= h;
            Self::buttons(MOUSEEVENTF_HWHEEL, h as i32);
        }
    }

    // --- pv 4 hardware keyboard / pointer (upstream PR #316) ---------------

    fn key_event(scan: u16, extended: bool, down: bool) {
        let mut flags = KEYEVENTF_SCANCODE;
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
    }

    pub fn key(&mut self, k: &Key) {
        let Some((scan, extended)) = hid_to_scan(k.code) else {
            return;
        };
        if k.down {
            // UIKit repeats pressesBegan for a held key; Windows owns repeat.
            if !self.keys_down.insert(k.code) {
                return;
            }
        } else if !self.keys_down.remove(&k.code) {
            // Never pressed by us: leave another keyboard's key alone.
            return;
        }
        Self::key_event(scan, extended, k.down);
    }

    fn button(button: u32, down: bool) {
        let flag = match (button, down) {
            (1, true) => MOUSEEVENTF_LEFTDOWN,
            (1, false) => MOUSEEVENTF_LEFTUP,
            (2, true) => MOUSEEVENTF_RIGHTDOWN,
            (2, false) => MOUSEEVENTF_RIGHTUP,
            (3, true) => MOUSEEVENTF_MIDDLEDOWN,
            (3, false) => MOUSEEVENTF_MIDDLEUP,
            _ => return,
        };
        Self::buttons(flag, 0);
    }

    fn mouse_absolute(x: i32, y: i32) {
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        if vw <= 1 || vh <= 1 {
            return;
        }
        let ax = (((x - vx) as i64 * 65535) / (vw as i64 - 1)) as i32;
        let ay = (((y - vy) as i64 * 65535) / (vh as i64 - 1)) as i32;
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: ax,
                    dy: ay,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
    }

    fn release_pointer_button(&mut self) {
        if self.pointer_button != 0 {
            Self::button(self.pointer_button, false);
            self.pointer_button = 0;
        }
    }

    pub fn pointer(&mut self, p: &Pointer) {
        let phase = p.phase.to_ascii_lowercase();
        if phase == "cancelled" {
            self.release_pointer_button();
            return;
        }
        let (Some(x), Some(y)) = (p.x, p.y) else {
            return;
        };
        let pt = self.to_screen(x, y);
        match phase.as_str() {
            "began" => {
                let b = p.button.clamp(1, 3);
                if self.pointer_button != 0 {
                    Self::button(self.pointer_button, false);
                }
                Self::mouse_absolute(pt.x, pt.y);
                Self::button(b, true);
                self.pointer_button = b;
            }
            "moved" => Self::mouse_absolute(pt.x, pt.y),
            "ended" => {
                Self::mouse_absolute(pt.x, pt.y);
                self.release_pointer_button();
            }
            _ => {}
        }
    }

    /// Release this session's owned keys and pointer button (pv 4 `inputReset`).
    pub fn reset(&mut self) {
        for code in self.keys_down.drain() {
            if let Some((scan, extended)) = hid_to_scan(code) {
                Self::key_event(scan, extended, false);
            }
        }
        self.release_pointer_button();
    }

    fn pen_device(&mut self) -> Option<HSYNTHETICPOINTERDEVICE> {
        if !self.pen_tried {
            self.pen_tried = true;
            match unsafe { CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT) } {
                Ok(d) => self.pen = Some(d),
                Err(e) => warn!("no synthetic pen ({e}); Pencil drives the mouse"),
            }
        }
        self.pen
    }

    fn inject_pen(&mut self, x: f64, y: f64, flags: POINTER_FLAGS, pressure: u32, tilt: (i32, i32)) -> bool {
        let Some(dev) = self.pen_device() else { return false };
        let p = self.to_screen(x, y);
        let info = POINTER_TYPE_INFO {
            r#type: PT_PEN,
            Anonymous: POINTER_TYPE_INFO_0 {
                penInfo: POINTER_PEN_INFO {
                    pointerInfo: POINTER_INFO {
                        pointerType: PT_PEN,
                        pointerId: 0,
                        pointerFlags: flags,
                        ptPixelLocation: p,
                        ..Default::default()
                    },
                    penFlags: 0,
                    penMask: PEN_MASK_PRESSURE | PEN_MASK_TILT_X | PEN_MASK_TILT_Y,
                    pressure,
                    rotation: 0,
                    tiltX: tilt.0,
                    tiltY: tilt.1,
                },
            },
        };
        match unsafe { InjectSyntheticPointerInput(dev, &[info]) } {
            Ok(()) => true,
            Err(e) => {
                debug!("pen injection failed: {e}");
                false
            }
        }
    }

    pub fn pencil(&mut self, p: &Pencil) {
        // Tilt from iOS altitude (pi/2 = perpendicular) and azimuth.
        let lean = (std::f64::consts::FRAC_PI_2 - p.altitude).clamp(0.0, 1.5);
        let tilt = (
            (lean.tan() * p.azimuth.cos()).atan().to_degrees().round() as i32,
            (lean.tan() * p.azimuth.sin()).atan().to_degrees().round() as i32,
        );
        let pressure = (p.pressure.clamp(0.0, 1.0) * 1024.0) as u32;
        let (flags, pressure) = match p.phase {
            PencilPhase::Down => {
                self.pen_down = true;
                (POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_DOWN, pressure.max(1))
            }
            PencilPhase::Move if self.pen_down => {
                (POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_UPDATE, pressure.max(1))
            }
            PencilPhase::Up => {
                self.pen_down = false;
                (POINTER_FLAG_INRANGE | POINTER_FLAG_UP, 0)
            }
            _ => (POINTER_FLAG_INRANGE | POINTER_FLAG_UPDATE, 0), // hover
        };
        self.pen_in_range = true;
        if !self.inject_pen(p.x, p.y, flags, pressure, tilt) {
            // No pen support: behave like a finger.
            let phase = match p.phase {
                PencilPhase::Down => TouchPhase::Began,
                PencilPhase::Up => TouchPhase::Ended,
                _ if self.pen_down => TouchPhase::Moved,
                _ => return,
            };
            self.touch(&Touch { phase, x: p.x, y: p.y, t: None });
        }
    }

    pub fn proximity(&mut self, p: &Proximity) {
        if !p.entering && self.pen_in_range {
            self.pen_in_range = false;
            // Leaving hover range: an update without INRANGE.
            self.inject_pen(p.x, p.y, POINTER_FLAG_UPDATE, 0, (0, 0));
        }
    }

    /// Release anything held when the session ends.
    pub fn release(&mut self) {
        self.reset();
        if self.mouse_down {
            Self::buttons(MOUSEEVENTF_LEFTUP, 0);
            self.mouse_down = false;
        }
        if self.pen_down {
            self.pen_down = false;
            self.inject_pen(0.5, 0.5, POINTER_FLAG_UP, 0, (0, 0));
        }
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        self.release();
        if let Some(d) = self.pen.take() {
            unsafe { windows::Win32::UI::Controls::DestroySyntheticPointerDevice(d) };
        }
    }
}
