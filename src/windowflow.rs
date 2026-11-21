//! Window behaviour for the extended desktop, built into the sender:
//!
//! * a window the user opens while working on the receiver's screen must not
//!   settle on the primary one (and a dialog follows its application);
//! * a window moved across the 150% <-> 200% DPI boundary keeps its physical
//!   size instead of growing by the scale ratio;
//! * when the receiver's screen leaves the desktop (Win+P, unplug, the
//!   display guard rebuilding it) the windows it held are remembered and put
//!   back when it returns, unless the user moved them in the meantime.
//!
//! One polling thread, no hooks, no separate helper process.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use tracing::info;
use windows::core::{BOOL, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
use windows::Win32::UI::WindowsAndMessaging::*;

const IGNORED_CLASSES: &[&str] = &[
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "Progman",
    "WorkerW",
    "Windows.UI.Core.CoreWindow",
    "XamlExplorerHostIslandWindow",
    "TopLevelWindowForOverflowXamlIsland",
    "ApplicationManager_ImmersiveShellWindow",
    "MultitaskingViewFrame",
    "ForegroundStaging",
    "TaskListThumbnailWnd",
    "NotifyIconOverflowWindow",
    "SystemTray_Main",
    "OpenDisplayTray",
    "#32768",
];

#[derive(Clone, Copy, Default)]
struct WinState {
    last_rect: RECT,
    last_mon: isize,
    primary_rect: Option<RECT>,
    target_rect: Option<RECT>,
    displaced: bool,
    displaced_rect: RECT,
    /// The physical size this window should have after crossing a DPI
    /// boundary. Kept for a moment because the DPI change can land a few
    /// frames late and while the user is still dragging the window.
    pending: Option<PendingFix>,
    /// We placed this window on a screen at this moment. If the app moves it
    /// back to another screen on its own (saved-position restores), the
    /// window is returned once — unless the user acted in between.
    placed_on: Option<(isize, Instant)>,
}

/// Where an app's windows live, and when the app was last seen running.
#[derive(Clone, Copy)]
struct AppGroup {
    mon: isize,
    anchor: Instant,
    /// Refreshed every poll while any window of the app is visible, so a
    /// relaunch after the app closed starts a fresh session.
    seen: Instant,
    /// Set while the app has no window anywhere: its windows were closed, so
    /// the next launch re-uses this screen instead of trusting ambient clicks
    /// (a tray click on another screen, say). Cleared the moment a window is
    /// seen or a direct act happens elsewhere.
    closed_here: bool,
}

/// A physical-size correction waiting for the crossing to settle.
#[derive(Clone, Copy)]
struct PendingFix {
    mon: isize,
    w: i32,
    h: i32,
    /// Backstop so a stale entry can never linger.
    until: Instant,
    /// The DPI change has actually moved the size away from the expected one.
    deviated: bool,
    /// Last poll's left-button state: a fresh press means the user has taken
    /// the window again, and the correction is abandoned.
    was_down: bool,
    /// The correction has been written to the log (it retries every poll).
    logged: bool,
}

#[derive(Clone, Copy)]
struct Op {
    desired: RECT,
    mon: isize,
    until: Instant,
    placement: bool,
    remax: bool,
}

#[derive(Clone)]
struct WinInfo {
    hwnd: HWND,
    rect: RECT,
    iconic: bool,
    zoomed: bool,
    owner: HWND,
    tool: bool,
    class: String,
    pid: u32,
    visible: bool,
}

struct Ctx {
    items: Vec<WinInfo>,
}

unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam.0 as *mut Ctx);
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return BOOL(1);
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    let mut class = [0u16; 128];
    let n = GetClassNameW(hwnd, &mut class);
    let class = String::from_utf16_lossy(&class[..n.max(0) as usize]);
    let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
    ctx.items.push(WinInfo {
        hwnd,
        rect,
        iconic: IsIconic(hwnd).as_bool(),
        zoomed: IsZoomed(hwnd).as_bool(),
        owner: GetWindow(hwnd, GW_OWNER).unwrap_or(HWND(std::ptr::null_mut())),
        tool: ex & WS_EX_TOOLWINDOW.0 as isize != 0,
        class,
        pid,
        visible: IsWindowVisible(hwnd).as_bool(),
    });
    BOOL(1)
}

fn enumerate() -> Vec<WinInfo> {
    let mut ctx = Ctx { items: Vec::new() };
    let _ = unsafe { EnumWindows(Some(collect), LPARAM(&mut ctx as *mut _ as isize)) };
    ctx.items
}

fn mon_of_window(hwnd: HWND) -> isize {
    unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST).0 as isize }
}

fn mon_of_point(x: i32, y: i32) -> isize {
    unsafe { MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST).0 as isize }
}

fn primary_mon() -> isize {
    mon_of_point(0, 0)
}

fn work_area(mon: isize) -> RECT {
    if mon == 0 {
        return RECT { left: 0, top: 0, right: 1920, bottom: 1080 };
    }
    unsafe {
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(HMONITOR(mon as *mut _), &mut mi).as_bool() {
            mi.rcWork
        } else {
            RECT { left: 0, top: 0, right: 1920, bottom: 1080 }
        }
    }
}

fn monitor_bounds(mon: isize) -> RECT {
    if mon == 0 {
        return RECT { left: 0, top: 0, right: 1920, bottom: 1080 };
    }
    unsafe {
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(HMONITOR(mon as *mut _), &mut mi).as_bool() {
            mi.rcMonitor
        } else {
            RECT { left: 0, top: 0, right: 1920, bottom: 1080 }
        }
    }
}

/// A window that fills its whole monitor, taskbar area included: a slide
/// show, a video player, a game. That is the app's or the shell's explicit
/// placement — it must never be dragged to another screen.
///
/// A maximized window is different: it stops at the work area, so the taskbar
/// strip stays visible and its rect falls short of the monitor's edge.
fn covers_monitor(r: RECT, mon: isize) -> bool {
    covers(r, monitor_bounds(mon))
}

fn covers(r: RECT, m: RECT) -> bool {
    const TOL: i32 = 2;
    r.left <= m.left + TOL
        && r.top <= m.top + TOL
        && r.right >= m.right - TOL
        && r.bottom >= m.bottom - TOL
}

fn size(r: RECT) -> (i32, i32) {
    (r.right - r.left, r.bottom - r.top)
}

fn point_in(r: RECT, p: POINT) -> bool {
    p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
}

fn clampsize(r: RECT, work: RECT) -> (i32, i32) {
    let (w, h) = size(r);
    (w.min(work.right - work.left - 20).max(320), h.min(work.bottom - work.top - 20).max(240))
}

fn clamp_pos(x: i32, y: i32, w: i32, h: i32, work: RECT) -> (i32, i32) {
    let mut x = x;
    let mut y = y;
    if x < work.left {
        x = work.left;
    }
    if y < work.top {
        y = work.top;
    }
    if x + w > work.right {
        x = work.right - w;
    }
    if y + h > work.bottom {
        y = work.bottom - h;
    }
    (x.max(work.left), y.max(work.top))
}

/// A window Windows snapped to one of the standard zones of this monitor's
/// work area (half, third, quarter, ...).
///
/// A snap is the user's explicit placement, so the physical-size restore must
/// not undo it when the window crosses the DPI boundary (Win+Shift+Arrow on a
/// snapped window, an app re-snapping itself). The check looks at both axes:
/// the length matches a standard fraction of the work area within a few pixels
/// and the rect is anchored to an edge or centred.
fn is_snap_zone(r: RECT, work: RECT) -> bool {
    const TOL: i32 = 6;
    const FRACS: [f32; 6] = [0.25, 1.0 / 3.0, 0.5, 2.0 / 3.0, 0.75, 1.0];
    let fits = |start: i32, len: i32, w0: i32, wlen: i32| -> bool {
        let size_ok = FRACS
            .iter()
            .any(|f| (len as f32 - f * wlen as f32).abs() <= TOL as f32 + 2.0);
        let anchored = (start - w0).abs() <= TOL
            || (start + len - (w0 + wlen)).abs() <= TOL
            || (start + len / 2 - (w0 + wlen / 2)).abs() <= TOL;
        size_ok && anchored
    };
    let (rw, rh) = size(r);
    let (ww, wh) = size(work);
    let full = rw >= ww - 2 * TOL && rh >= wh - 2 * TOL;
    !full && fits(r.left, rw, work.left, ww) && fits(r.top, rh, work.top, wh)
}

fn cursor_mon() -> isize {
    unsafe {
        let mut p = POINT::default();
        if GetCursorPos(&mut p).is_ok() {
            mon_of_point(p.x, p.y)
        } else {
            0
        }
    }
}

/// The executable name of a process, lower-cased; a pid string when it cannot
/// be read. Windows of a multi-process app (Chromium-based editors, browsers,
/// Douyin, Office helpers) must share one window group.
fn process_name(pid: u32) -> String {
    let mut buf = [0u16; 1024];
    let mut size = buf.len() as u32;
    let name = unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return format!("pid:{pid}");
        };
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);
        if ok.is_err() {
            return format!("pid:{pid}");
        }
        String::from_utf16_lossy(&buf[..size as usize])
    };
    let file = name.rsplit('\\').next().unwrap_or(&name);
    file.to_ascii_lowercase()
}

/// A left click happened since the last look (or is being held).
///
/// The receiver may drive the pointer instead of sending touch events, so a
/// click is the only sign of the user acting at the pointer — and a click is
/// always newer than a menu that has been sitting open.
fn click_now() -> bool {
    let s = unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) };
    s & 1 != 0 || s < 0
}

/// UWP windows stay "visible" while DWM cloaks them; treat that as hidden.
fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked = 0u32;
    unsafe {
        windows::Win32::Graphics::Dwm::DwmGetWindowAttribute(
            hwnd,
            windows::Win32::Graphics::Dwm::DWMWA_CLOAKED,
            &mut cloaked as *mut _ as *mut _,
            4,
        )
        .is_ok()
            && cloaked != 0
    }
}

fn class_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn mon_name(m: isize, primary: isize, target: isize) -> &'static str {
    if m == primary {
        "primary"
    } else if m == target {
        "second"
    } else {
        "other"
    }
}

/// The receiver's screen: the rightmost non-primary monitor.
fn pick_target() -> Option<(String, RECT, isize)> {
    let all = crate::display::monitors_basic();
    let primary = all.iter().find(|m| m.primary).map(|m| m.rect);
    let mut best: Option<&crate::display::Monitor> = None;
    for m in &all {
        if m.primary {
            continue;
        }
        if best.map_or(true, |b| m.rect.right > b.rect.right) {
            best = Some(m);
        }
    }
    let b = best?;
    let _ = primary;
    let cx = (b.rect.left + b.rect.right) / 2;
    let cy = (b.rect.top + b.rect.bottom) / 2;
    Some((b.device_name.clone(), b.rect, mon_of_point(cx, cy)))
}

pub fn start() {
    let _ = std::thread::Builder::new().name("window-flow".into()).spawn(run);
}

fn run() {
    let own_pid = std::process::id();
    let mut primary = primary_mon();
    let mut target_dev: Option<String> = None;
    let mut target_mon: isize = 0;
    let mut states: HashMap<isize, WinState> = HashMap::new();
    let mut ops: HashMap<isize, Op> = HashMap::new();
    // Windows already on the desktop when we start are never re-placed.
    let mut place_tried: HashSet<isize> = HashSet::new();
    let mut seen_shown: HashSet<isize> = HashSet::new();
    for w in enumerate() {
        place_tried.insert(w.hwnd.0 as isize);
    }
    let mut last_refresh = Instant::now() - Duration::from_secs(5);
    let mut last_prune = Instant::now();
    let mut last_fg: isize = 0;
    let mut last_fg_mon: isize = 0;
    let mut last_fg_class = String::new();
    // Where each running app's windows live (pid -> screen). An app that opens
    // several windows keeps them together even when shell surfaces take the
    // focus in between (Douyin, Office, anything with helper windows).
    let mut groups: HashMap<String, AppGroup> = HashMap::new();
    // pid -> executable name, so the poll loop never has to open a process.
    let mut names: HashMap<u32, String> = HashMap::new();
    // The last click: screen, exact point, time. The point matters for the
    // return watchdog — only a click on the window itself is the user taking
    // it over; clicking the tray or another app must not cancel the watch.
    let mut click_ctx: Option<(isize, POINT, Instant)> = None;
    let mut last_cursor_mon: isize = 0;
    // Windows we moved recently: if the app puts itself back where it was
    // (saved positions restored late), return it once more.
    let mut second: HashMap<isize, (RECT, RECT, Instant)> = HashMap::new();
    info!("window-flow: watching the extended desktop");

    loop {
        std::thread::sleep(Duration::from_millis(100));

        if last_refresh.elapsed() >= Duration::from_secs(1) {
            last_refresh = Instant::now();
            let p = primary_mon();
            if p != 0 && p != primary {
                primary = p;
                info!("window-flow: the primary display changed");
            }
            let now = pick_target();
            match (&now, &target_dev) {
                (Some((dev, _rect, mon)), Some(old)) if old == dev && *mon == target_mon => {}
                (Some((dev, rect, mon)), _) => {
                    let appeared = target_dev.is_none();
                    target_dev = Some(dev.clone());
                    target_mon = *mon;
                    if appeared {
                        info!("window-flow: second screen {dev} at ({}, {})", rect.left, rect.top);
                        // Put back what the system dumped on the primary.
                        let mut n = 0;
                        for (h, s) in states.iter_mut() {
                            if !s.displaced {
                                continue;
                            }
                            let hwnd = HWND(*h as *mut _);
                            if !unsafe { IsWindow(Some(hwnd)).as_bool() } {
                                continue;
                            }
                            if mon_of_window(hwnd) != primary {
                                continue;
                            }
                            let mut r = RECT::default();
                            if unsafe { GetWindowRect(hwnd, &mut r) }.is_err() {
                                continue;
                            }
                            if (r.left - s.displaced_rect.left).abs() > 60
                                || (r.top - s.displaced_rect.top).abs() > 60
                            {
                                continue; // the user moved it: leave it alone
                            }
                            if let Some(want) = s.target_rect {
                                let work = work_area(target_mon);
                                let (w, hh) = clampsize(want, work);
                                let (x, y) = clamp_pos(want.left, want.top, w, hh, work);
                                ops.insert(
                                    *h,
                                    Op {
                                        desired: RECT { left: x, top: y, right: x + w, bottom: y + hh },
                                        mon: target_mon,
                                        until: Instant::now() + Duration::from_millis(1500),
                                        placement: true,
                                        remax: false,
                                    },
                                );
                                s.displaced = false;
                                n += 1;
                            }
                        }
                        if n > 0 {
                            info!("window-flow: restoring {n} window(s) to the second screen");
                        }
                    }
                }
                (None, Some(_)) => {
                    target_dev = None;
                    target_mon = 0;
                    info!("window-flow: second screen left the desktop");
                }
                (None, None) => {}
            }
        }

        let items = enumerate();
        if target_mon == 0 {
            // The second screen is gone: remember where its windows ended up on
            // the primary so they can be put back when it returns.
            for w in &items {
                if !w.visible
                    || w.iconic
                    || w.tool
                    || w.pid == own_pid
                    || IGNORED_CLASSES.contains(&w.class.as_str())
                    || !w.owner.0.is_null()
                {
                    continue;
                }
                let (rw, rh) = size(w.rect);
                if rw < 160 || rh < 100 || mon_of_window(w.hwnd) != primary {
                    continue;
                }
                let h = w.hwnd.0 as isize;
                if let Some(st) = states.get_mut(&h) {
                    if st.target_rect.is_some() && !st.displaced {
                        st.displaced = true;
                        st.displaced_rect = w.rect;
                        info!("window-flow: remembering {} that left the second screen", w.class);
                    }
                }
            }
            continue;
        }

        let cursor = cursor_mon();
        let work = work_area(target_mon);
        if cursor != 0 && cursor != last_cursor_mon {
            info!(
                "window-flow: pointer entered the {} screen",
                mon_name(cursor, primary, target_mon)
            );
            last_cursor_mon = cursor;
        }
        // A finger or Pencil contact is the user acting on that screen. Touch
        // input does not move the pointer, so the pointer cannot be trusted for
        // launches started from the receiver's screen.
        let touch: Option<(isize, Instant)> = crate::input::last_contact().and_then(|(x, y, at)| {
            let m = mon_of_point(x, y);
            (m != 0).then_some((m, at))
        });
        // A context menu open right now. Its timestamp stays the moment it
        // appeared: a menu left open for minutes must not outrank a newer
        // touch on the other screen. While it stays open its context stands;
        // after it closes the context lives on for twenty seconds, so the
        // window it launched still lands on its screen.
        let mut open_menu: Option<isize> = None;
        let now_seen = Instant::now();
        for w in &items {
            // An app that is still running keeps its window group alive; a
            // relaunch after a gap is a new session with a fresh anchor.
            if w.visible
                && let Some(name) = names.get(&w.pid)
                && let Some(g) = groups.get_mut(name)
            {
                g.seen = now_seen;
                g.closed_here = false;
            }
            if open_menu.is_none() && w.visible && w.class == "#32768" && !is_cloaked(w.hwnd) {
                let m = mon_of_window(w.hwnd);
                if m != 0 {
                    open_menu = Some(m);
                }
            }
        }
        // An app whose windows have all closed keeps the screen it lived on:
        // its next launch comes back there, so closing on the second screen
        // and quitting from a tray click on the primary does not lose it.
        groups.retain(|_, g| {
            if g.seen.elapsed() >= Duration::from_secs(3) && !g.closed_here {
                g.closed_here = true;
            }
            g.seen.elapsed() < Duration::from_secs(120)
        });
        let _ = open_menu;
        let _ = touch;
        // A click at the pointer: the newest act, and the only one there is
        // when the receiver drives the pointer instead of sending touches.
        if cursor != 0 && click_now() {
            let mut pt = POINT::default();
            let _ = unsafe { GetCursorPos(&mut pt) };
            click_ctx = Some((cursor, pt, Instant::now()));
        }

        // Owners before their owned windows: a dialog's decision must see its
        // app's move (a pending op), not the screen the app is leaving.
        let mut order: Vec<WinInfo> = Vec::with_capacity(items.len());
        order.extend(items.iter().filter(|w| w.owner.0.is_null()).cloned());
        order.extend(items.iter().filter(|w| !w.owner.0.is_null()).cloned());
        for w in order {
            let h = w.hwnd.0 as isize;
            let shown = w.visible && !is_cloaked(w.hwnd);
            let fresh_show = shown && seen_shown.insert(h);
            if fresh_show {
                info!(
                    "window-flow: show hwnd={h} class={} rect=({}, {})-({}, {}) mon={} owner={} zoomed={} tool={} known={} ignored={}",
                    w.class,
                    w.rect.left,
                    w.rect.top,
                    w.rect.right,
                    w.rect.bottom,
                    mon_name(mon_of_window(w.hwnd), primary, target_mon),
                    w.owner.0 as isize,
                    w.zoomed,
                    w.tool,
                    place_tried.contains(&h),
                    IGNORED_CLASSES.contains(&w.class.as_str())
                );
            } else if !shown {
                seen_shown.remove(&h);
            }
            if w.pid == own_pid || w.tool || IGNORED_CLASSES.contains(&w.class.as_str()) {
                continue;
            }
            // A window that goes hidden and shows again is a fresh launch as far
            // as placement is concerned: tray apps and reused hidden windows.
            // UWP frames (Settings among them) stay "visible" while DWM cloaks
            // them, so cloaked counts as hidden.
            if !shown {
                place_tried.remove(&h);
                continue;
            }
            let (rw, rh) = size(w.rect);
            let owner_visible =
                !w.owner.0.is_null() && unsafe { IsWindowVisible(w.owner).as_bool() };

            // Owned windows with a visible owner: a dialog belongs where its app is.
            if owner_visible {
                if place_tried.contains(&h) || w.iconic || w.zoomed {
                    continue;
                }
                if rw < 100 || rh < 60 {
                    continue;
                }
                place_tried.insert(h);
                let on = mon_of_window(w.hwnd);
                // The owner may be mid-move: follow where it is going.
                let omon = ops
                    .get(&(w.owner.0 as isize))
                    .map(|op| op.mon)
                    .unwrap_or_else(|| mon_of_window(w.owner));
                // A dialog belongs on its application's screen, either way round.
                let cross = omon != on
                    && (omon == target_mon || omon == primary)
                    && (on == target_mon || on == primary)
                    && !covers_monitor(w.rect, on);
                if cross {
                    let dwork = work_area(omon);
                    let mut orc = RECT::default();
                    if unsafe { GetWindowRect(w.owner, &mut orc) }.is_ok() {
                        let dx = orc.left + (orc.right - orc.left - rw) / 2;
                        let dy = orc.top + (orc.bottom - orc.top - rh) / 3;
                        let (x, y) = clamp_pos(dx, dy, rw, rh, dwork);
                        ops.insert(
                            h,
                            Op {
                                desired: RECT { left: x, top: y, right: x + rw, bottom: y + rh },
                                mon: omon,
                                until: Instant::now() + Duration::from_millis(1500),
                                placement: true,
                                remax: false,
                            },
                        );
                        info!(
                            "window-flow: dialog {} -> {} screen",
                            w.class,
                            mon_name(omon, primary, target_mon)
                        );
                        // Watch this one too: apps such as Douyin restore
                        // their saved position a minute after launch.
                        let st = states.entry(h).or_insert(WinState {
                            last_rect: w.rect,
                            last_mon: on,
                            ..Default::default()
                        });
                        st.placed_on = Some((omon, Instant::now()));
                    }
                }
                continue;
            }

            if w.iconic {
                continue;
            }
            if unsafe { GetAncestor(w.hwnd, GA_ROOT) } != w.hwnd {
                continue;
            }
            if rw < 160 || rh < 100 {
                continue;
            }

            let mon = mon_of_window(w.hwnd);
            let st = states.entry(h).or_insert(WinState {
                last_rect: w.rect,
                last_mon: mon,
                ..Default::default()
            });

            if mon == target_mon {
                st.target_rect = Some(w.rect);
                st.displaced = false;
            }

            if !place_tried.contains(&h) {
                place_tried.insert(h);
                // Launch-screen placement is deliberately NOT done: where a
                // new window opens is Windows'/the app's business (2026-11-20
                // request). Only the record-keeping below is kept.
                let app = match names.get(&w.pid) {
                    Some(n) => n.clone(),
                    None => {
                        let n = process_name(w.pid);
                        names.insert(w.pid, n.clone());
                        n
                    }
                };
                // A fullscreen arrival (slide show, video, game) means the app
                // or the shell chose the screen — remember the group there, so
                // a relaunch after closing returns to the same screen.
                let mon_live = groups
                    .get(&app)
                    .map_or(false, |g| g.seen.elapsed() < Duration::from_secs(3));
                if covers_monitor(w.rect, mon) && !mon_live {
                    groups.insert(
                        app.clone(),
                        AppGroup { mon, anchor: Instant::now(), seen: Instant::now(), closed_here: false },
                    );
                    info!("window-flow: {} opened fullscreen on the {} screen", w.class, mon_name(mon, primary, target_mon));
                } else if !mon_live && !groups.contains_key(&app) {
                    groups.insert(
                        app.clone(),
                        AppGroup { mon, anchor: Instant::now(), seen: Instant::now(), closed_here: false },
                    );
                }
            } else if fresh_show {
                info!(
                    "window-flow: {} shown again but already known; not placed",
                    w.class
                );
            }

            // Physical-size restore across the DPI boundary. A window that
            // arrives snapped keeps its zone: the snap is the user's explicit
            // placement (Win+Shift+Arrow keeps it across screens), not a
            // DPI accident to correct.
            if mon != st.last_mon {
                let cur_work = if mon == target_mon { work } else { work_area(mon) };
                if w.zoomed || covers_monitor(w.rect, mon) {
                    st.pending = None;
                } else if is_snap_zone(w.rect, cur_work) {
                    info!("window-flow: {} arrived snapped; keeping the snap", w.class);
                    st.pending = None;
                } else if mon == target_mon && st.last_mon != target_mon {
                    // A window that was snapped on the screen it left keeps
                    // whatever size Windows gives it (Win+Shift+Arrow unsnaps
                    // first, so that is the app's own restore size).
                    if !is_snap_zone(st.last_rect, work_area(st.last_mon)) {
                        let (ew, eh) = clampsize(st.last_rect, work);
                        let (sw, sh) = size(st.last_rect);
                        info!(
                            "window-flow: {} crossed to the second screen (last {}x{} on {}, now {}x{}); expecting {}x{}",
                            w.class,
                            sw,
                            sh,
                            mon_name(st.last_mon, primary, target_mon),
                            rw,
                            rh,
                            ew,
                            eh
                        );
                        let down = unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } < 0;
                        st.pending = Some(PendingFix {
                            mon: target_mon,
                            w: ew,
                            h: eh,
                            until: Instant::now() + Duration::from_secs(10),
                            deviated: false,
                            was_down: down,
                            logged: false,
                        });
                    }
                } else if mon == primary && st.last_mon == target_mon {
                    if let Some(pr) = st.primary_rect {
                        let work_p = work_area(primary);
                        if !is_snap_zone(pr, work_p) {
                            let (ew, eh) = clampsize(pr, work_p);
                            let down = unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } < 0;
                            st.pending = Some(PendingFix {
                                mon: primary,
                                w: ew,
                                h: eh,
                                until: Instant::now() + Duration::from_secs(10),
                                deviated: false,
                                was_down: down,
                                logged: false,
                            });
                        }
                    }
                }
                // A window that changed screens outside our own moves (the
                // user dragged it, the app moved itself) re-anchors its app's
                // group. Three seconds keeps our own moves out of it.
                let app = match names.get(&w.pid) {
                    Some(n) => n.clone(),
                    None => {
                        let n = process_name(w.pid);
                        names.insert(w.pid, n.clone());
                        n
                    }
                };
                if mon != 0
                    && let Some(g) = groups.get(&app)
                    && g.mon != mon
                    && g.anchor.elapsed() > Duration::from_secs(3)
                {
                    groups.insert(
                        app.clone(),
                        AppGroup { mon, anchor: Instant::now(), seen: Instant::now(), closed_here: false },
                    );
                }
                // A window we just placed somewhere must not wander back by
                // itself: some apps restore their saved position right after
                // we move them (Douyin does). Return it once, keeping the
                // app's own size, unless the user has acted since.
                if let Some((want, at)) = st.placed_on {
                    // Arriving on the wanted screen is expected and keeps the
                    // watch armed; leaving it within ninety seconds is what
                    // gets returned (Douyin restores its saved position about
                    // a minute after launch). Any user act cancels the watch.
                    if at.elapsed() > Duration::from_secs(90) {
                        st.placed_on = None;
                    } else if mon != want {
                        // Only a click on the window itself means the user
                        // took it over; ambient clicks elsewhere (tray, chat)
                        // must not cancel the watch.
                        let user_act = click_ctx.map_or(false, |(_, pt, t)| {
                            t > at && point_in(w.rect, pt)
                        });
                        if user_act {
                            st.placed_on = None;
                        } else if mon != 0 {
                            let (cw, ch) = size(w.rect);
                            let work_w = work_area(want);
                            // Back where it was on that screen, not at the
                            // corner the clamped foreign rect would give.
                            let remembered = if want == target_mon {
                                st.target_rect
                            } else if want == primary {
                                st.primary_rect
                            } else {
                                None
                            };
                            let (px, py) = remembered
                                .map(|r| (r.left, r.top))
                                .unwrap_or((w.rect.left, w.rect.top));
                            let (x, y) = clamp_pos(px, py, cw, ch, work_w);
                            ops.insert(
                                h,
                                Op {
                                    desired: RECT { left: x, top: y, right: x + cw, bottom: y + ch },
                                    mon: want,
                                    until: Instant::now() + Duration::from_millis(1500),
                                    placement: true,
                                    remax: false,
                                },
                            );
                            info!(
                                "window-flow: {} went back to the {} screen on its own; returning it",
                                w.class,
                                mon_name(want, primary, target_mon)
                            );
                            st.placed_on = None;
                        }
                    }
                }
            }
            // The DPI change can land after the crossing poll: wait until it
            // has actually resized the window, then put the physical size back
            // once. The shell ignores SetWindowPos while the user still holds
            // the window, and a fresh button press means the user has taken
            // the window again (a resize must not be fought).
            if let Some(mut p) = st.pending {
                let (cw, ch) = size(w.rect);
                let down = unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } < 0;
                let pressed_again = down && !p.was_down;
                p.was_down = down;
                if pressed_again || Instant::now() > p.until || mon != p.mon || w.zoomed {
                    st.pending = None;
                } else {
                    if (cw - p.w).abs() > 8 || (ch - p.h).abs() > 8 {
                        p.deviated = true;
                    }
                    if p.deviated {
                        if (cw - p.w).abs() <= 8 && (ch - p.h).abs() <= 8 {
                            st.pending = None; // corrected
                        } else if !down {
                            if !p.logged {
                                p.logged = true;
                                info!(
                                    "window-flow: {} crossed at the wrong size ({}x{}); restoring {}x{} on the {} screen",
                                    w.class,
                                    cw,
                                    ch,
                                    p.w,
                                    p.h,
                                    mon_name(p.mon, primary, target_mon)
                                );
                            }
                            let work_m = work_area(p.mon);
                            let (x, y) = clamp_pos(w.rect.left, w.rect.top, p.w, p.h, work_m);
                            ops.insert(
                                h,
                                Op {
                                    desired: RECT {
                                        left: x,
                                        top: y,
                                        right: x + p.w,
                                        bottom: y + p.h,
                                    },
                                    mon: p.mon,
                                    until: Instant::now() + Duration::from_millis(800),
                                    placement: false,
                                    remax: false,
                                },
                            );
                        }
                    }
                    if st.pending.is_some() {
                        st.pending = Some(p);
                    }
                }
            }

            st.last_mon = mon;
            st.last_rect = w.rect;
            if mon == primary {
                st.primary_rect = Some(w.rect);
            }
        }

        // Activation never moves a window (native Windows behaviour). Only new
        // windows, dialogs and the return-after-unplug rules move. The
        // foreground is still tracked: a shell surface that takes the focus
        // (Start, tray flyout) marks where the user is working.
        let fg_hwnd = unsafe { GetForegroundWindow() };
        let fg_now = fg_hwnd.0 as isize;
        if fg_now != last_fg {
            if last_fg != 0 && fg_now != 0 && target_mon != 0 {
                // The previous foreground window is often destroyed by the time we
                // sample (Start menu, flyouts): use what we cached while it lived.
                let pclass = last_fg_class.clone();
                let nclass = class_of(fg_hwnd);
                let prev_mon = last_fg_mon;
                let new_mon = mon_of_window(fg_hwnd);
                let first_seen = !place_tried.contains(&fg_now);
                if prev_mon != new_mon || first_seen {
                    info!(
                        "window-flow: fg {pclass}[{}] -> {nclass}[{}] first_seen={first_seen}",
                        mon_name(prev_mon, primary, target_mon),
                        mon_name(new_mon, primary, target_mon)
                    );
                }
            }
            if fg_now != 0 {
                let m = mon_of_window(fg_hwnd);
                if m != 0 {
                    last_fg_mon = m;
                    last_fg_class = class_of(fg_hwnd);
                }
            }
            last_fg = fg_now;
        }

        // Execute pending moves.
        let mut done: Vec<isize> = Vec::new();
        for (h, op) in ops.iter() {
            let hwnd = HWND(*h as *mut _);
            if !unsafe { IsWindow(Some(hwnd)).as_bool() }
                || !unsafe { IsWindowVisible(hwnd).as_bool() }
                || unsafe { IsIconic(hwnd).as_bool() }
            {
                done.push(*h);
                continue;
            }
            if Instant::now() > op.until {
                done.push(*h);
                continue;
            }
            // A physics op (placement: false) only changes the size on the
            // same monitor, so "already on the right monitor" is not success.
            let mut cur = RECT::default();
            let cur_ok = unsafe { GetWindowRect(hwnd, &mut cur) }.is_ok();
            let at_size = cur_ok
                && (cur.right - cur.left - (op.desired.right - op.desired.left)).abs() <= 8
                && (cur.bottom - cur.top - (op.desired.bottom - op.desired.top)).abs() <= 8;
            if mon_of_window(hwnd) == op.mon && (op.placement || op.remax || at_size) {
                if op.remax {
                    unsafe { let _ = ShowWindow(hwnd, SW_MAXIMIZE); }
                }
                done.push(*h);
                continue;
            }
            if unsafe { IsZoomed(hwnd).as_bool() } && !op.placement {
                done.push(*h);
                continue;
            }
            let mut r = RECT::default();
            if unsafe { GetWindowRect(hwnd, &mut r) }.is_err() {
                done.push(*h);
                continue;
            }
            let (w, hh) = size(op.desired);
            let moved_ok = unsafe {
                SetWindowPos(
                    hwnd,
                    None,
                    op.desired.left,
                    op.desired.top,
                    w,
                    hh,
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                )
                .is_ok()
            };
            let mut after = RECT::default();
            let after_ok = unsafe { GetWindowRect(hwnd, &mut after) }.is_ok();
            let size_ok = after_ok
                && (after.right - after.left - (op.desired.right - op.desired.left)).abs() <= 8
                && (after.bottom - after.top - (op.desired.bottom - op.desired.top)).abs() <= 8;
            let ok = moved_ok
                && after_ok
                && mon_of_window(hwnd) == op.mon
                && (op.placement || op.remax || size_ok);
            if ok {
                if op.placement {
                    // Watch it for a few seconds: some apps restore their saved
                    // position after we have moved them.
                    second.insert(*h, (r, op.desired, Instant::now() + Duration::from_secs(4)));
                }
                if op.remax {
                    unsafe { let _ = ShowWindow(hwnd, SW_MAXIMIZE); }
                }
            } else if Instant::now() > op.until {
                // Still wrong after the whole retry window (UIPI, or the app
                // insists on its position): say so, do not claim success.
                info!(
                    "window-flow: could not move a window to the {} screen",
                    mon_name(op.mon, primary, target_mon)
                );
                done.push(*h);
            }
            // Otherwise retry on the next tick: a window that is still starting
            // often refuses the first attempts.
        }
        for h in done {
            ops.remove(&h);
        }

        // Second chance: an app that restored its own saved position wins the
        // first round; put the window back on the second screen once more.
        if !second.is_empty() {
            let mut clear2: Vec<isize> = Vec::new();
            for (h, (origin, desired, until)) in second.iter() {
                let hwnd = HWND(*h as *mut _);
                if Instant::now() > *until
                    || !unsafe { IsWindow(Some(hwnd)).as_bool() }
                    || !unsafe { IsWindowVisible(hwnd).as_bool() }
                    || unsafe { IsIconic(hwnd).as_bool() }
                {
                    clear2.push(*h);
                    continue;
                }
                if mon_of_window(hwnd) != primary {
                    clear2.push(*h);
                    continue;
                }
                let mut r = RECT::default();
                if unsafe { GetWindowRect(hwnd, &mut r) }.is_err() {
                    clear2.push(*h);
                    continue;
                }
                if (r.left - origin.left).abs() > 30 || (r.top - origin.top).abs() > 30 {
                    clear2.push(*h); // moved somewhere else on purpose
                    continue;
                }
                let work = work_area(target_mon);
                let (w, hh) = size(*desired);
                let (x, y) = clamp_pos(desired.left, desired.top, w, hh, work);
                let _ = unsafe {
                    SetWindowPos(
                        hwnd,
                        None,
                        x,
                        y,
                        w,
                        hh,
                        SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                    )
                };
                info!("window-flow: a window put itself back on the primary; returning it");
                clear2.push(*h);
            }
            for h in clear2 {
                second.remove(&h);
            }
        }

        if last_prune.elapsed() >= Duration::from_secs(60) {
            last_prune = Instant::now();
            place_tried.retain(|h| unsafe { IsWindow(Some(HWND(*h as *mut _))).as_bool() });
            states.retain(|h, _| unsafe { IsWindow(Some(HWND(*h as *mut _))).as_bool() });
            names.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snap_zones() {
        let work = RECT { left: 0, top: 0, right: 3840, bottom: 2088 };
        let r = |l, t, rr, b| RECT { left: l, top: t, right: rr, bottom: b };
        // Left half, right half, quarters, thirds: snapped.
        assert!(is_snap_zone(r(0, 0, 1920, 2088), work));
        assert!(is_snap_zone(r(1920, 0, 3840, 2088), work));
        assert!(is_snap_zone(r(1920, 0, 3840, 1044), work));
        assert!(is_snap_zone(r(0, 0, 1280, 2088), work));
        assert!(is_snap_zone(r(1280, 0, 2560, 2088), work)); // centred third
        // An arbitrary floating window is not a snap.
        assert!(!is_snap_zone(r(300, 200, 1300, 900), work));
        // A full-screen window is not a snap (maximize is handled separately).
        assert!(!is_snap_zone(r(0, 0, 3840, 2088), work));
        // Nearly half but not anchored to any edge.
        assert!(!is_snap_zone(r(500, 0, 2420, 2088), work));
    }

    #[test]
    fn full_screen_windows() {
        let mon = RECT { left: 3840, top: 0, right: 6572, bottom: 2048 };
        let r = |l, t, rr, b| RECT { left: l, top: t, right: rr, bottom: b };
        // A slide show fills the whole monitor.
        assert!(covers(r(3840, 0, 6572, 2048), mon));
        // A maximized window leaves the taskbar strip free: not full screen.
        assert!(!covers(r(3829, -11, 6583, 1963), mon));
        // An ordinary window: not full screen.
        assert!(!covers(r(4200, 900, 5400, 1950), mon));
    }

    #[test]
    fn clamp_into_second_screen() {
        let work = RECT { left: 3840, top: 0, right: 6572, bottom: 2048 };
        let (w, h) = clampsize(RECT { left: 0, top: 0, right: 5000, bottom: 5000 }, work);
        assert_eq!(w, 2732 - 20);
        assert_eq!(h, 2048 - 20);
        let (x, y) = clamp_pos(100, -50, 400, 300, work);
        assert_eq!(x, 3840);
        assert_eq!(y, 0);
        let (x, y) = clamp_pos(7000, 3000, 400, 300, work);
        assert_eq!(x, 6572 - 400);
        assert_eq!(y, 2048 - 300);
    }
}
