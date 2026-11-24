//! Windows 实现：控制条窗口贴合投屏窗口，以及手柄模式用的全局键盘钩子。
//!
//! 控制条是本进程内的一个无边框 WebView 窗口（标签 `bar-<scrcpy pid>`）。它被设为投屏窗口的
//! 「所有者窗口」：系统因此让它始终叠在投屏窗口之上、随其最小化而隐藏，也不占任务栏位置。
//! 位置与显示全部走原生调用（`SetWindowPos` / `ShowWindow(SW_SHOWNOACTIVATE)`），
//! 控制条出现或移动时不会抢走投屏窗口的焦点。

use super::{push_job, shared, try_shared, vk_to_code, GAMEPAD_ACTIVE};
use crate::logger;
use crate::session;
use crate::AppState;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::atomic::Ordering;
use std::sync::Once;
use std::time::Duration;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, EnumWindows, GetForegroundWindow, GetMessageW,
    GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    SetWindowLongPtrW, SetWindowPos, SetWindowsHookExW, ShowWindow, TranslateMessage,
    GWLP_HWNDPARENT, GWL_EXSTYLE, MSG, SWP_NOACTIVATE, SWP_NOZORDER, SW_HIDE, SW_SHOWNOACTIVATE,
    WS_EX_NOACTIVATE,
};

/// 控制条高度（逻辑像素）。
const BAR_HEIGHT: f64 = 40.0;
const TICK: Duration = Duration::from_millis(250);

const LLKHF_EXTENDED: u32 = 0x01;
const LLKHF_INJECTED: u32 = 0x10;
const LLKHF_UP: u32 = 0x80;
const WH_KEYBOARD_LL: i32 = 13;

/// 一条会话的窗口对应关系，供键盘钩子判断「前台窗口属于哪台设备」。
#[derive(Clone, Debug)]
pub(crate) struct Tracked {
    pub device_id: String,
    pub scrcpy: isize,
    pub bar: isize,
}

struct Bar {
    /// 控制条窗口句柄；0 表示创建失败（不再重试，避免每个周期都刷日志）。
    hwnd: isize,
    owner: isize,
    rect: Option<(i32, i32, i32, i32)>,
    shown: bool,
}

pub(crate) fn start(app: AppHandle) {
    std::thread::spawn(move || {
        let mut bars: HashMap<u32, Bar> = HashMap::new();
        loop {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                tick(&app, &mut bars)
            }));
            if result.is_err() {
                logger::warn("control", "控制条跟随线程出现异常，已忽略本轮");
            }
            std::thread::sleep(TICK);
        }
    });
}

fn label_of(pid: u32) -> String {
    format!("bar-{pid}")
}

fn tick(app: &AppHandle, bars: &mut HashMap<u32, Bar>) {
    let state = app.state::<AppState>();
    let enabled = state.config.lock().unwrap().control.bar_enabled;
    let running: Vec<session::SessionInfo> = if enabled {
        session::session_infos(&state)
            .into_iter()
            .filter(|s| s.running)
            .collect()
    } else {
        Vec::new()
    };
    let alive: HashSet<u32> = running.iter().map(|s| s.pid).collect();

    bars.retain(|pid, _| {
        if alive.contains(pid) {
            return true;
        }
        if let Some(win) = app.get_webview_window(&label_of(*pid)) {
            let _ = win.destroy();
        }
        false
    });

    let mut tracked: Vec<Tracked> = Vec::new();
    for s in &running {
        let Some(scrcpy) = find_window(s.pid) else {
            if let Some(bar) = bars.get_mut(&s.pid) {
                set_shown(bar, false);
            }
            continue;
        };
        if unsafe { IsIconic(scrcpy as HWND) } != 0 {
            if let Some(bar) = bars.get_mut(&s.pid) {
                set_shown(bar, false);
            }
            continue;
        }
        let bar = bars
            .entry(s.pid)
            .or_insert_with(|| create_bar(app, s.pid));
        if bar.hwnd == 0 {
            continue;
        }
        tracked.push(Tracked {
            device_id: s.device_id.clone(),
            scrcpy,
            bar: bar.hwnd,
        });

        if bar.owner != scrcpy {
            unsafe { SetWindowLongPtrW(bar.hwnd as HWND, GWLP_HWNDPARENT, scrcpy as isize) };
            bar.owner = scrcpy;
        }
        let Some(r) = (unsafe { frame_rect(scrcpy) }) else {
            continue;
        };
        let scale = app
            .get_webview_window(&label_of(s.pid))
            .and_then(|w| w.scale_factor().ok())
            .unwrap_or(1.0);
        let h = (BAR_HEIGHT * scale).round() as i32;
        let (x, mut y, w) = (r.left, r.bottom, r.right - r.left);
        // 投屏窗口贴着工作区底边时，控制条会落到任务栏下面：改为叠在画面最下沿。
        if let Some(a) = session::primary_work_area() {
            if x >= a.x && x < a.x + a.width && y + h > a.y + a.height {
                y = (r.bottom - h).max(a.y);
            }
        }
        let target = (x, y, w, h);
        if bar.rect != Some(target) {
            unsafe {
                SetWindowPos(
                    bar.hwnd as HWND,
                    null_mut(),
                    x,
                    y,
                    w,
                    h,
                    SWP_NOACTIVATE | SWP_NOZORDER,
                );
            }
            bar.rect = Some(target);
        }
        set_shown(bar, true);
    }

    let mut sh = shared();
    sh.tracked = tracked;
    // 设备已没有任何会话窗口：手柄模式随之关闭，避免下次连接时意外处于开启状态。
    let live: HashSet<String> = sh.tracked.iter().map(|t| t.device_id.clone()).collect();
    let before = sh.gamepad_on.len();
    sh.gamepad_on.retain(|d| live.contains(d));
    if sh.gamepad_on.len() != before {
        sh.pressed.clear();
        GAMEPAD_ACTIVE.store(!sh.gamepad_on.is_empty(), Ordering::Relaxed);
    }
}

fn set_shown(bar: &mut Bar, shown: bool) {
    if bar.hwnd == 0 || bar.shown == shown {
        return;
    }
    unsafe {
        ShowWindow(bar.hwnd as HWND, if shown { SW_SHOWNOACTIVATE } else { SW_HIDE });
    }
    bar.shown = shown;
}

fn create_bar(app: &AppHandle, pid: u32) -> Bar {
    let failed = Bar {
        hwnd: 0,
        owner: 0,
        rect: None,
        shown: false,
    };
    let label = label_of(pid);
    let win = match app.get_webview_window(&label) {
        Some(w) => w,
        None => match WebviewWindowBuilder::new(app, label.clone(), WebviewUrl::App("bar.html".into()))
            .title("FreeControl")
            .decorations(false)
            .resizable(false)
            .skip_taskbar(true)
            .focused(false)
            .visible(false)
            .inner_size(320.0, BAR_HEIGHT)
            .build()
        {
            Ok(w) => w,
            Err(e) => {
                logger::warn("control", format!("创建控制条窗口失败（pid {pid}）：{e}"));
                return failed;
            }
        },
    };
    let Ok(handle) = win.hwnd() else {
        logger::warn("control", format!("取控制条窗口句柄失败（pid {pid}）"));
        return failed;
    };
    let hwnd = handle.0 as isize;
    // 点控制条按钮不应让投屏窗口失去前台。
    unsafe {
        let ex = GetWindowLongPtrW(hwnd as HWND, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd as HWND, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE as isize);
    }
    Bar {
        hwnd,
        owner: 0,
        rect: None,
        shown: false,
    }
}

// ── 窗口查找 ──

struct Find {
    pid: u32,
    best: HWND,
    area: i64,
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> i32 {
    let find = &mut *(lparam as *mut Find);
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid == find.pid && IsWindowVisible(hwnd) != 0 {
        let mut r = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetWindowRect(hwnd, &mut r) != 0 {
            let area = (r.right - r.left) as i64 * (r.bottom - r.top) as i64;
            if area > find.area {
                find.area = area;
                find.best = hwnd;
            }
        }
    }
    1
}

/// 某个进程最大的可见顶层窗口（scrcpy 只有一个 SDL 窗口）。
fn find_window(pid: u32) -> Option<isize> {
    let mut find = Find {
        pid,
        best: null_mut(),
        area: 0,
    };
    unsafe { EnumWindows(Some(enum_proc), &mut find as *mut Find as LPARAM) };
    if find.best.is_null() {
        None
    } else {
        Some(find.best as isize)
    }
}

/// 窗口的可见边界；取不到时退回含隐形缩放边框的外框。
unsafe fn frame_rect(hwnd: isize) -> Option<RECT> {
    let h = hwnd as HWND;
    let mut r = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let hr = DwmGetWindowAttribute(
        h,
        DWMWA_EXTENDED_FRAME_BOUNDS,
        &mut r as *mut RECT as *mut c_void,
        std::mem::size_of::<RECT>() as u32,
    );
    if hr == 0 {
        return Some(r);
    }
    if GetWindowRect(h, &mut r) != 0 {
        Some(r)
    } else {
        None
    }
}

// ── 键盘钩子 ──

#[repr(C)]
struct KbdLlHook {
    vk_code: u32,
    scan_code: u32,
    flags: u32,
    time: u32,
    extra: usize,
}

/// 首次开启手柄模式时才安装钩子（独立线程 + 消息循环），从不开启就不会有任何系统级钩子。
pub(crate) fn ensure_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::thread::spawn(|| unsafe {
            let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_proc), GetModuleHandleW(null()), 0);
            if hook.is_null() {
                logger::warn("control", "安装全局键盘钩子失败，手柄模式不可用");
                return;
            }
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        });
    });
}

unsafe extern "system" fn ll_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == 0 && GAMEPAD_ACTIVE.load(Ordering::Relaxed) {
        let info = &*(lparam as *const KbdLlHook);
        if handle_key(info.vk_code, info.flags) {
            return 1;
        }
    }
    CallNextHookEx(null_mut(), code, wparam, lparam)
}

/// 返回 `true` 表示按键已被手柄映射接管（不再交给前台窗口）。
fn handle_key(vk: u32, flags: u32) -> bool {
    if flags & LLKHF_INJECTED != 0 {
        return false;
    }
    let Some(code) = vk_to_code(vk, flags & LLKHF_EXTENDED != 0) else {
        return false;
    };
    let Some(mut sh) = try_shared() else {
        return false;
    };
    if flags & LLKHF_UP != 0 {
        return sh.pressed.remove(&vk);
    }
    let fg = unsafe { GetForegroundWindow() } as isize;
    let Some(device) = sh
        .tracked
        .iter()
        .find(|t| t.scrcpy == fg || t.bar == fg)
        .map(|t| t.device_id.clone())
    else {
        return false;
    };
    if !sh.gamepad_on.contains(&device) {
        return false;
    }
    let Some(&key) = sh.keymap.get(code.as_str()) else {
        return false;
    };
    if sh.pressed.insert(vk) {
        push_job(&device, key, true);
    }
    true
}
