//! Windows taskbar widget: a small transparent strip just left of the notification area showing
//! "[icon] 30%  [icon] 93%". It is a layered child window of the taskbar (`Shell_TrayWnd`), drawn
//! with GDI into a premultiplied-alpha bitmap. Port of widget.cpp.
//!
//! Everything here runs on the UI thread (the event loop's), so state lives in a thread-local.

use std::cell::RefCell;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, EqualRect, DeleteObject, GdiFlush, GetDC, GetTextExtentPoint32W,
    GetTextFaceW, ReleaseDC, ScreenToClient, SelectObject, SetBkMode, SetTextColor, TextOutW, AC_SRC_ALPHA, AC_SRC_OVER,
    ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
    DEFAULT_PITCH, DIB_RGB_COLORS, HFONT, OUT_DEFAULT_PRECIS, TRANSPARENT,
};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{
    InitCommonControlsEx, ICC_WIN95_CLASSES, INITCOMMONCONTROLSEX, TOOLTIPS_CLASSW, TTF_IDISHWND, TTF_SUBCLASS,
    TTM_ADDTOOLW, TTM_SETMAXTIPWIDTH, TTM_UPDATETIPTEXTW, TTS_ALWAYSTIP, TTS_NOPREFIX, TTTOOLINFOW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, FindWindowExW, FindWindowW, GetClientRect,
    GetWindow, GetWindowRect, IsWindow, LoadCursorW, MoveWindow, PostMessageW, RegisterClassExW, SendMessageW,
    SetWindowPos, ShowWindow, UpdateLayeredWindow, GW_HWNDPREV, HWND_TOP, IDC_ARROW, MA_NOACTIVATE, SIZE_RESTORED,
    SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_SHOWNOACTIVATE, ULW_ALPHA, WM_LBUTTONUP, WM_MOUSEACTIVATE,
    WM_MOUSEMOVE, WM_RBUTTONUP, WM_SIZE, WNDCLASSEXW, WS_CHILD, WS_CLIPSIBLINGS, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOPMOST, WS_POPUP,
};

use crate::device::DeviceKind;

/// One device shown in the strip.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Item {
    pub kind: DeviceKind,
    pub percent: Option<u8>,
    /// Shown instead of the percentage when set (coarse levels like "Low").
    pub text: String,
    pub charging: bool,
    /// Asleep / stale value.
    pub dim: bool,
    pub low: bool,
    /// Picks the Bluetooth icon when the device type is unknown.
    pub bluetooth: bool,
}

const WM_MOUSELEAVE: u32 = 0x02A3;
const CLASS: &str = "WirelessBatteryCrossWidget";

#[derive(Clone, Copy)]
struct Rgb(i32, i32, i32);

struct State {
    wnd: HWND,
    taskbar: HWND,
    tip: HWND,
    rebar: HWND,
    on_click: fn(),
    light: bool,
    hover: bool,
    items: Vec<Item>,
    tip_text: Vec<u16>,
    last_tray: RECT,
    last_bar: RECT,
    last_w: i32,
    last_dpi: u32,
    size: SIZE,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.borrow_mut().as_mut().map(f))
}

fn dpi_of(taskbar: HWND) -> i32 {
    // SAFETY: plain query; an invalid handle just returns 0.
    let d = if taskbar.is_null() { 0 } else { unsafe { GetDpiForWindow(taskbar) } };
    if d == 0 { 96 } else { d as i32 }
}

fn px(v: i32, dpi: i32) -> i32 {
    v * dpi / 96
}

fn glyph(k: DeviceKind, bluetooth: bool) -> u16 {
    match k {
        DeviceKind::Headset => 0xE7F6,  // Headphone
        DeviceKind::Mouse => 0xE962,    // Mouse
        DeviceKind::Keyboard => 0xE765, // KeyboardClassic
        DeviceKind::Gamepad => 0xE7FC,  // Game
        DeviceKind::Other => {
            if bluetooth {
                0xE702 // Bluetooth
            } else {
                0xE772 // Devices
            }
        }
    }
}
const BOLT: u16 = 0xE945; // LightningBolt
const BATTERY_UNKNOWN: u16 = 0xE996; // shown when there is nothing to display

// ---- GDI text -> coverage mask ----------------------------------------------------------------

struct Font(HFONT);
impl Drop for Font {
    fn drop(&mut self) {
        // SAFETY: the font was created by CreateFontW and is no longer selected anywhere.
        unsafe { DeleteObject(self.0) };
    }
}

/// Creates a font, falling back when the preferred face is not installed (GDI would silently substitute).
fn make_font(px: i32, face: &str, fallback: &str) -> Font {
    let mut last = null_mut();
    for (i, f) in [face, fallback].into_iter().enumerate() {
        let name = wide(f);
        // SAFETY: `name` is NUL-terminated; the DC and font are released before leaving the block.
        unsafe {
            let h = CreateFontW(
                -px,
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                ANTIALIASED_QUALITY as u32,
                DEFAULT_PITCH as u32,
                name.as_ptr(),
            );
            let dc = CreateCompatibleDC(null_mut());
            let old = SelectObject(dc, h);
            let mut got = [0u16; 32];
            GetTextFaceW(dc, got.len() as i32, got.as_mut_ptr());
            SelectObject(dc, old);
            DeleteDC(dc);
            let got = String::from_utf16_lossy(&got[..got.iter().position(|c| *c == 0).unwrap_or(got.len())]);
            if got.eq_ignore_ascii_case(f) || i == 1 {
                return Font(h);
            }
            DeleteObject(h);
            last = h;
        }
    }
    Font(last)
}

#[derive(Default)]
struct Mask {
    w: i32,
    h: i32,
    a: Vec<u8>,
}

fn render_mask(text: &[u16], font: &Font) -> Mask {
    let mut m = Mask::default();
    // SAFETY: all GDI objects are created and destroyed within this function; `bits` points to a
    // W*H DIB section that stays valid until the bitmap is deleted.
    unsafe {
        let dc = CreateCompatibleDC(null_mut());
        let of = SelectObject(dc, font.0);
        let mut sz = SIZE { cx: 0, cy: 0 };
        GetTextExtentPoint32W(dc, text.as_ptr(), text.len() as i32, &mut sz);
        let (w, h) = (sz.cx + 8, sz.cy + 8);
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..std::mem::zeroed()
        };
        let mut bits: *mut core::ffi::c_void = null_mut();
        let bmp = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if bmp.is_null() || bits.is_null() {
            SelectObject(dc, of);
            DeleteDC(dc);
            return m;
        }
        let ob = SelectObject(dc, bmp);
        std::ptr::write_bytes(bits as *mut u8, 0, (w * h * 4) as usize);
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, 0x00FF_FFFF);
        TextOutW(dc, 4, 4, text.as_ptr(), text.len() as i32);
        GdiFlush();
        let p = std::slice::from_raw_parts(bits as *const u32, (w * h) as usize);
        let (mut x0, mut y0, mut x1, mut y1) = (w, h, -1, -1);
        for y in 0..h {
            for x in 0..w {
                if p[(y * w + x) as usize] & 0xFF != 0 {
                    x0 = x0.min(x);
                    x1 = x1.max(x);
                    y0 = y0.min(y);
                    y1 = y1.max(y);
                }
            }
        }
        if x1 >= 0 {
            m.w = x1 - x0 + 1;
            m.h = y1 - y0 + 1;
            m.a = vec![0; (m.w * m.h) as usize];
            for y in 0..m.h {
                for x in 0..m.w {
                    m.a[(y * m.w + x) as usize] = (p[((y0 + y) * w + x0 + x) as usize] & 0xFF) as u8;
                }
            }
        }
        SelectObject(dc, ob);
        SelectObject(dc, of);
        DeleteObject(bmp);
        DeleteDC(dc);
    }
    m
}

// ---- canvas -----------------------------------------------------------------------------------

struct Canvas {
    w: i32,
    h: i32,
    px: Vec<u32>, // premultiplied BGRA
}

impl Canvas {
    fn new(w: i32, h: i32) -> Self {
        Self { w, h, px: vec![0; (w * h) as usize] }
    }

    /// Source-over; `a` is 0..=255.
    fn blend(&mut self, x: i32, y: i32, c: Rgb, a: i32) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h || a <= 0 {
            return;
        }
        let i = (y * self.w + x) as usize;
        let d = self.px[i];
        let inv = 255 - a;
        let (da, dr, dg, db) = ((d >> 24) as i32, ((d >> 16) & 255) as i32, ((d >> 8) & 255) as i32, (d & 255) as i32);
        let oa = a + da * inv / 255;
        let or = c.0 * a / 255 + dr * inv / 255;
        let og = c.1 * a / 255 + dg * inv / 255;
        let ob = c.2 * a / 255 + db * inv / 255;
        self.px[i] = ((oa as u32) << 24) | ((or as u32) << 16) | ((og as u32) << 8) | ob as u32;
    }

    fn draw(&mut self, m: &Mask, x: i32, y: i32, c: Rgb, alpha: i32) {
        for j in 0..m.h {
            for i in 0..m.w {
                self.blend(x + i, y + j, c, m.a[(j * m.w + i) as usize] as i32 * alpha / 255);
            }
        }
    }
}

struct Piece {
    m: Mask,
    color: Rgb,
    alpha: i32,
    gap_after: i32,
}

fn layout(items: &[Item], light: bool, dpi: i32) -> (Vec<Piece>, i32) {
    let fg = if light { Rgb(0, 0, 0) } else { Rgb(255, 255, 255) };
    let red = if light { Rgb(196, 30, 40) } else { Rgb(255, 90, 90) };
    let charge = if light { Rgb(0, 120, 212) } else { Rgb(96, 205, 255) };
    let icons = make_font(px(16, dpi), "Segoe Fluent Icons", "Segoe MDL2 Assets");
    let bolt = make_font(px(11, dpi), "Segoe Fluent Icons", "Segoe MDL2 Assets");
    let text = make_font(px(13, dpi), "Segoe UI Variable Text", "Segoe UI");

    let mut out: Vec<Piece> = Vec::new();
    if items.is_empty() {
        out.push(Piece { m: render_mask(&[BATTERY_UNKNOWN], &icons), color: fg, alpha: 110, gap_after: 0 });
    }
    for (i, it) in items.iter().enumerate() {
        let a = if it.dim { 120 } else { 255 };
        let tc = if it.low { red } else { fg };
        out.push(Piece { m: render_mask(&[glyph(it.kind, it.bluetooth)], &icons), color: fg, alpha: a, gap_after: px(5, dpi) });
        let t = if !it.text.is_empty() {
            it.text.clone()
        } else if let Some(p) = it.percent {
            format!("{p}%")
        } else {
            "?".into()
        };
        let t: Vec<u16> = t.encode_utf16().collect();
        out.push(Piece { m: render_mask(&t, &text), color: tc, alpha: a, gap_after: if it.charging { px(2, dpi) } else { 0 } });
        if it.charging {
            out.push(Piece { m: render_mask(&[BOLT], &bolt), color: charge, alpha: a, gap_after: 0 });
        }
        if let Some(last) = out.last_mut() {
            last.gap_after = if i + 1 < items.len() { px(14, dpi) } else { 0 };
        }
    }
    let width = px(8, dpi) * 2 + out.iter().map(|p| p.m.w + p.gap_after).sum::<i32>();
    (out, width)
}

fn paint(s: &mut State) {
    if s.wnd.is_null() {
        return;
    }
    let dpi = dpi_of(s.taskbar);
    let mut bar = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: valid out-pointer.
    unsafe { GetClientRect(s.taskbar, &mut bar) };
    let h = bar.bottom - bar.top;
    let (pieces, w) = layout(&s.items, s.light, dpi);
    if w <= 0 || h <= 0 {
        return;
    }
    let mut cv = Canvas::new(w, h);

    // Nearly transparent background so the whole strip receives mouse input, plus a rounded
    // hover highlight like the system tray icons have.
    cv.px.fill(0x0100_0000);
    if s.hover {
        let (inset, r) = (px(4, dpi), px(4, dpi));
        let hc = if s.light { Rgb(0, 0, 0) } else { Rgb(255, 255, 255) };
        let ha = if s.light { 20 } else { 26 };
        for y in inset..h - inset {
            for x in 0..w {
                let dx = if x < r { r - x } else if x >= w - r { x - (w - r - 1) } else { 0 };
                let dy = if y < inset + r {
                    inset + r - y
                } else if y >= h - inset - r {
                    y - (h - inset - r - 1)
                } else {
                    0
                };
                if dx * dx + dy * dy <= r * r {
                    cv.blend(x, y, hc, ha);
                }
            }
        }
    }
    let mut x = px(8, dpi);
    for p in &pieces {
        cv.draw(&p.m, x, (h - p.m.h) / 2, p.color, p.alpha);
        x += p.m.w + p.gap_after;
    }

    // SAFETY: GDI objects are created/destroyed here; the DIB section is `w*h*4` bytes.
    unsafe {
        let screen = GetDC(null_mut());
        let mem = CreateCompatibleDC(screen);
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..std::mem::zeroed()
        };
        let mut bits: *mut core::ffi::c_void = null_mut();
        let bmp = CreateDIBSection(mem, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if !bmp.is_null() && !bits.is_null() {
            std::ptr::copy_nonoverlapping(cv.px.as_ptr(), bits as *mut u32, cv.px.len());
            let ob = SelectObject(mem, bmp);
            s.size = SIZE { cx: w, cy: h };
            let sz = SIZE { cx: w, cy: h };
            let src = POINT { x: 0, y: 0 };
            let bf = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
            UpdateLayeredWindow(s.wnd, screen, null(), &sz, mem, &src, 0, &bf, ULW_ALPHA);
            SelectObject(mem, ob);
            DeleteObject(bmp);
        }
        DeleteDC(mem);
        ReleaseDC(null_mut(), screen);
    }
    s.last_w = -1; // force a re-position with the new size
}

unsafe extern "system" fn wnd_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEMOVE => {
            let first = with(|s| !std::mem::replace(&mut s.hover, true)).unwrap_or(false);
            if first {
                let mut t = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: h, dwHoverTime: 0 };
                TrackMouseEvent(&mut t);
                with(|s| paint(s));
                reposition();
            }
            0
        }
        WM_MOUSELEAVE => {
            with(|s| {
                s.hover = false;
                paint(s);
            });
            reposition();
            0
        }
        WM_LBUTTONUP | WM_RBUTTONUP => {
            if let Some(f) = with(|s| s.on_click) {
                f();
            }
            0
        }
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

// ---- public API -------------------------------------------------------------------------------

/// Creates the widget on the primary taskbar. `on_click` runs on the UI thread for left/right clicks.
/// Returns false when there is no horizontal taskbar to attach to (the caller falls back to tray icons).
pub fn create(light: bool, on_click: fn()) -> bool {
    destroy();
    let class = wide(CLASS);
    let tray_class = wide("TrayNotifyWnd");
    // SAFETY: Win32 calls with NUL-terminated strings; every handle is checked before use.
    unsafe {
        let taskbar = FindWindowW(wide("Shell_TrayWnd").as_ptr(), null());
        if taskbar.is_null() || FindWindowExW(taskbar, null_mut(), tray_class.as_ptr(), null()).is_null() {
            return false;
        }
        let mut tb = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        GetWindowRect(taskbar, &mut tb);
        if tb.bottom - tb.top > tb.right - tb.left {
            return false; // vertical taskbar: use tray icons
        }
        // Windows 11's taskbar is a XAML island; on Windows 10 the task buttons live in ReBarWindow32
        // and have to be narrowed so they never slide under the widget.
        let bridge = wide("Windows.UI.Composition.DesktopWindowContentBridge");
        let rebar = if FindWindowExW(taskbar, null_mut(), bridge.as_ptr(), null()).is_null() {
            FindWindowExW(taskbar, null_mut(), wide("ReBarWindow32").as_ptr(), null())
        } else {
            null_mut()
        };

        let inst = GetModuleHandleW(null());
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: inst,
            hIcon: null_mut(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
            hIconSm: null_mut(),
        };
        RegisterClassExW(&wc); // already registered on re-create: harmless failure
        let wnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_NOACTIVATE,
            class.as_ptr(),
            wide("").as_ptr(),
            WS_CHILD | WS_CLIPSIBLINGS,
            0,
            0,
            1,
            1,
            taskbar,
            null_mut(),
            inst,
            null(),
        );
        if wnd.is_null() {
            return false;
        }

        let icc = INITCOMMONCONTROLSEX { dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_WIN95_CLASSES };
        InitCommonControlsEx(&icc);
        // The tooltip is owned by the taskbar-side window so it is not clipped by the strip.
        let tip = CreateWindowExW(
            WS_EX_TOPMOST,
            TOOLTIPS_CLASSW,
            null(),
            WS_POPUP | TTS_ALWAYSTIP | TTS_NOPREFIX,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            inst,
            null(),
        );
        let tip_text = wide("Wireless Battery");
        if !tip.is_null() {
            let mut ti: TTTOOLINFOW = std::mem::zeroed();
            ti.cbSize = std::mem::size_of::<TTTOOLINFOW>() as u32;
            ti.uFlags = TTF_IDISHWND | TTF_SUBCLASS;
            ti.hwnd = wnd;
            ti.uId = wnd as usize;
            ti.lpszText = tip_text.as_ptr() as *mut u16;
            SendMessageW(tip, TTM_ADDTOOLW, 0, &ti as *const _ as LPARAM);
            SendMessageW(tip, TTM_SETMAXTIPWIDTH, 0, 600);
        }

        let zero = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        let mut st = State {
            wnd,
            taskbar,
            tip,
            rebar,
            on_click,
            light,
            hover: false,
            items: Vec::new(),
            tip_text,
            last_tray: zero,
            last_bar: zero,
            last_w: -1,
            last_dpi: dpi_of(taskbar) as u32,
            size: SIZE { cx: 1, cy: 1 },
        };
        paint(&mut st);
        st.last_w = -1;
        STATE.with(|s| *s.borrow_mut() = Some(st));
        reposition();
        ShowWindow(wnd, SW_SHOWNOACTIVATE);
    }
    true
}

pub fn destroy() {
    let Some(st) = STATE.with(|s| s.borrow_mut().take()) else { return };
    // SAFETY: handles are checked with IsWindow before use.
    unsafe {
        if !st.rebar.is_null() && IsWindow(st.rebar) != 0 && !st.taskbar.is_null() && IsWindow(st.taskbar) != 0 {
            // Let Explorer lay the task buttons out again over the full width.
            let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            GetClientRect(st.taskbar, &mut r);
            let lp = ((r.bottom as u32 as usize) << 16 | (r.right as u32 as usize & 0xFFFF)) as LPARAM;
            PostMessageW(st.taskbar, WM_SIZE, SIZE_RESTORED as WPARAM, lp);
        }
        if !st.tip.is_null() {
            DestroyWindow(st.tip);
        }
        if !st.wnd.is_null() {
            DestroyWindow(st.wnd);
        }
    }
}

/// The window the context menu should be anchored to.
pub fn hwnd() -> Option<isize> {
    with(|s| s.wnd as isize)
}

pub fn alive() -> bool {
    // SAFETY: IsWindow accepts any handle value.
    with(|s| unsafe { IsWindow(s.wnd) != 0 && IsWindow(s.taskbar) != 0 }).unwrap_or(false)
}

pub fn set_items(items: Vec<Item>, tooltip: &str) {
    with(|s| {
        s.items = items;
        let t = wide(tooltip);
        if t != s.tip_text && !s.tip.is_null() {
            s.tip_text = t;
            // SAFETY: `tip_text` outlives the call (the control copies it).
            unsafe {
                let mut ti: TTTOOLINFOW = std::mem::zeroed();
                ti.cbSize = std::mem::size_of::<TTTOOLINFOW>() as u32;
                ti.hwnd = s.wnd;
                ti.uId = s.wnd as usize;
                ti.lpszText = s.tip_text.as_mut_ptr();
                SendMessageW(s.tip, TTM_UPDATETIPTEXTW, 0, &ti as *const _ as LPARAM);
            }
        }
        paint(s);
    });
    reposition();
}

pub fn set_light(light: bool) {
    with(|s| {
        if s.light != light {
            s.light = light;
            paint(s);
        }
    });
}

/// Re-checks the taskbar layout and moves the widget if needed; call about once a second.
/// Returns false when the taskbar it was attached to is gone.
pub fn reposition() -> bool {
    if !alive() {
        return false;
    }
    with(|s| {
        // SAFETY: handles were validated by `alive`; all out-pointers are valid locals.
        unsafe {
            let dpi = dpi_of(s.taskbar);
            if dpi as u32 != s.last_dpi {
                s.last_dpi = dpi as u32;
                paint(s);
            }
            let tray = FindWindowExW(s.taskbar, null_mut(), wide("TrayNotifyWnd").as_ptr(), null());
            if tray.is_null() {
                return true;
            }
            let (mut tr, mut bar) = (RECT { left: 0, top: 0, right: 0, bottom: 0 }, RECT { left: 0, top: 0, right: 0, bottom: 0 });
            GetWindowRect(tray, &mut tr);
            GetWindowRect(s.taskbar, &mut bar);
            let mut p = POINT { x: tr.left, y: bar.top };
            ScreenToClient(s.taskbar, &mut p);
            let left = p.x - s.size.cx - px(2, dpi);

            // Windows 10: Explorer re-expands the task buttons on its own layout passes; narrow them again.
            if !s.rebar.is_null() && IsWindow(s.rebar) != 0 {
                let mut rr = RECT { left: 0, top: 0, right: 0, bottom: 0 };
                GetWindowRect(s.rebar, &mut rr);
                let mut tl = POINT { x: rr.left, y: rr.top };
                ScreenToClient(s.taskbar, &mut tl);
                if tl.x + (rr.right - rr.left) > left && left > tl.x {
                    MoveWindow(s.rebar, tl.x, tl.y, left - tl.x, rr.bottom - rr.top, 1);
                }
            }

            // Keep us above the taskbar's own content (our sibling windows).
            let covered = !GetWindow(s.wnd, GW_HWNDPREV).is_null();
            if !covered && EqualRect(&tr, &s.last_tray) != 0 && EqualRect(&bar, &s.last_bar) != 0 && s.last_w == s.size.cx {
                return true;
            }
            s.last_tray = tr;
            s.last_bar = bar;
            s.last_w = s.size.cx;
            SetWindowPos(s.wnd, HWND_TOP, left, 0, s.size.cx, s.size.cy, SWP_NOACTIVATE | SWP_SHOWWINDOW);
            true
        }
    })
    .unwrap_or(false)
}

/// True when the taskbar uses the light theme (`SystemUsesLightTheme`).
pub fn light_taskbar() -> bool {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let key = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    let name = wide("SystemUsesLightTheme");
    let (mut v, mut sz) = (0u32, 4u32);
    // SAFETY: valid NUL-terminated strings and a 4-byte output buffer.
    let rc = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_DWORD, null_mut(), &mut v as *mut u32 as *mut _, &mut sz)
    };
    rc == 0 && v != 0
}
