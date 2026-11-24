//! 投屏控制条与手柄键位。
//!
//! scrcpy 的投屏窗口是它自己的 SDL 窗口，本应用无法往里加控件。做法是为每条带窗口的会话
//! 另开一个无边框小窗口（控制条），贴在投屏窗口正下方并跟随它移动（Windows，见 `win`）。
//! 控制条提供「返回 / 主页 / 任务」三个导航键和一个手柄模式开关。
//!
//! 手柄模式开启后，投屏窗口（或它的控制条）在前台时，按下已映射的键盘按键会被拦下，
//! 改为对设备发送对应的 Android 手柄按键事件（`input gamepad keyevent`）。
//! 只支持按钮：摇杆与扳机的模拟量无法用按键事件表达，LT / RT 按数字按钮处理；
//! 也没有「按住」语义，每次按下发送一次点按。
//!
//! 所有按键都经同一个工作线程串行发给 adb，避免阻塞键盘钩子或界面线程。

use crate::apps::environment_dir;
use crate::assets::ADB_BIN;
use crate::config::AppConfig;
use crate::device::{self, mutate_and_persist};
use crate::logger;
use crate::session;
use crate::ui_message::{codes, UserError};
use crate::AppState;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};

#[cfg(windows)]
mod win;

pub const EVENT_GAMEPAD_CHANGED: &str = "control-gamepad-changed";
pub const EVENT_OPEN_GAMEPAD_SETTINGS: &str = "open-gamepad-settings";

const KEY_HOME: u32 = 3;
const KEY_BACK: u32 = 4;
const KEY_APP_SWITCH: u32 = 187;
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// 手柄按钮：`(按钮 id, Android 键值, 默认键盘按键)`。
///
/// 默认按键用 `KeyboardEvent.code` 的写法，前端录入与后端钩子两边用同一种字符串。
const BUTTONS: &[(&str, u32, &str)] = &[
    ("up", 19, "ArrowUp"),
    ("down", 20, "ArrowDown"),
    ("left", 21, "ArrowLeft"),
    ("right", 22, "ArrowRight"),
    ("a", 96, "KeyJ"),
    ("b", 97, "KeyK"),
    ("x", 99, "KeyU"),
    ("y", 100, "KeyI"),
    ("lb", 102, "KeyQ"),
    ("rb", 103, "KeyE"),
    ("lt", 104, "KeyZ"),
    ("rt", 105, "KeyC"),
    ("l3", 106, "KeyR"),
    ("r3", 107, "KeyT"),
    ("start", 108, "Enter"),
    ("back", 109, "Backspace"),
    ("guide", 110, "Home"),
];

// ── 配置 ──

fn default_true() -> bool {
    true
}

fn default_map() -> HashMap<String, String> {
    BUTTONS
        .iter()
        .map(|(id, _, key)| (id.to_string(), key.to_string()))
        .collect()
}

/// 控制条偏好：后端拥有，前端经专用命令修改（保存配置时不参与合并）。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ControlPrefs {
    /// 是否在投屏窗口下方显示控制条。
    #[serde(default = "default_true")]
    pub bar_enabled: bool,
    /// 手柄按钮 id → 键盘按键（`KeyboardEvent.code`）；空串表示未绑定。
    #[serde(default = "default_map")]
    pub gamepad_map: HashMap<String, String>,
}

impl Default for ControlPrefs {
    fn default() -> Self {
        ControlPrefs {
            bar_enabled: true,
            gamepad_map: default_map(),
        }
    }
}

impl ControlPrefs {
    /// 补齐缺失的按钮、丢弃未知按钮；不支持或重复的按键清空（先出现的按钮保留）。
    pub fn normalize(&mut self) {
        let supported = supported_codes();
        let mut used: HashSet<String> = HashSet::new();
        let mut out = HashMap::new();
        for (id, _, default) in BUTTONS {
            let value = self
                .gamepad_map
                .get(*id)
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| default.to_string());
            let value = if value.is_empty() || !supported.contains(&value) || !used.insert(value.clone())
            {
                String::new()
            } else {
                value
            };
            out.insert(id.to_string(), value);
        }
        self.gamepad_map = out;
    }
}

fn validate_map(map: &HashMap<String, String>) -> Result<(), UserError> {
    let supported = supported_codes();
    let mut used: HashSet<&str> = HashSet::new();
    for (id, code) in map {
        if !BUTTONS.iter().any(|(b, _, _)| b == id) {
            return Err(UserError::new(codes::CONTROL_MAP_INVALID).with_detail(id.clone()));
        }
        if code.is_empty() {
            continue;
        }
        if !supported.contains(code) {
            return Err(UserError::new(codes::CONTROL_KEY_UNSUPPORTED).with_detail(code.clone()));
        }
        if !used.insert(code.as_str()) {
            return Err(UserError::new(codes::CONTROL_KEY_DUPLICATE).with_detail(code.clone()));
        }
    }
    Ok(())
}

// ── 键码表 ──

/// Windows 虚拟键码 → `KeyboardEvent.code`。只覆盖常用按键（美式键盘布局的符号键）。
pub(crate) fn vk_to_code(vk: u32, extended: bool) -> Option<String> {
    let name = match vk {
        0x41..=0x5A => return Some(format!("Key{}", char::from_u32(vk)?)),
        0x30..=0x39 => return Some(format!("Digit{}", char::from_u32(vk)?)),
        0x60..=0x69 => return Some(format!("Numpad{}", vk - 0x60)),
        0x70..=0x7B => return Some(format!("F{}", vk - 0x6F)),
        0x6A => "NumpadMultiply",
        0x6B => "NumpadAdd",
        0x6D => "NumpadSubtract",
        0x6E => "NumpadDecimal",
        0x6F => "NumpadDivide",
        0x25 => "ArrowLeft",
        0x26 => "ArrowUp",
        0x27 => "ArrowRight",
        0x28 => "ArrowDown",
        0x20 => "Space",
        0x0D => {
            if extended {
                "NumpadEnter"
            } else {
                "Enter"
            }
        }
        0x09 => "Tab",
        0x08 => "Backspace",
        0x2E => "Delete",
        0x2D => "Insert",
        0x24 => "Home",
        0x23 => "End",
        0x21 => "PageUp",
        0x22 => "PageDown",
        0xA0 => "ShiftLeft",
        0xA1 => "ShiftRight",
        0xA2 => "ControlLeft",
        0xA3 => "ControlRight",
        0xA4 => "AltLeft",
        0xA5 => "AltRight",
        0xBA => "Semicolon",
        0xBB => "Equal",
        0xBC => "Comma",
        0xBD => "Minus",
        0xBE => "Period",
        0xBF => "Slash",
        0xC0 => "Backquote",
        0xDB => "BracketLeft",
        0xDC => "Backslash",
        0xDD => "BracketRight",
        0xDE => "Quote",
        _ => return None,
    };
    Some(name.to_string())
}

/// 钩子能识别的全部按键写法：界面录入的按键必须落在这个集合里，否则永远不会触发。
fn supported_codes() -> &'static HashSet<String> {
    static CODES: OnceLock<HashSet<String>> = OnceLock::new();
    CODES.get_or_init(|| {
        (0..=255u32)
            .flat_map(|vk| [false, true].into_iter().filter_map(move |e| vk_to_code(vk, e)))
            .collect()
    })
}

// ── 共享状态 ──

#[derive(Default)]
pub(crate) struct Shared {
    /// 已开启手柄模式的设备。
    pub gamepad_on: HashSet<String>,
    /// 键盘按键 → Android 键值。
    pub keymap: HashMap<String, u32>,
    /// 已被拦下、尚未抬起的虚拟键码：用来忽略自动重复，并让抬起事件成对被拦下。
    pub pressed: HashSet<u32>,
    #[cfg(windows)]
    pub tracked: Vec<win::Tracked>,
}

/// 至少一台设备开启了手柄模式：键盘钩子的快速出口，关闭时钩子对按键零开销。
pub(crate) static GAMEPAD_ACTIVE: AtomicBool = AtomicBool::new(false);

fn shared_cell() -> &'static Mutex<Shared> {
    static CELL: OnceLock<Mutex<Shared>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(Shared::default()))
}

pub(crate) fn shared() -> MutexGuard<'static, Shared> {
    shared_cell().lock().unwrap_or_else(|e| e.into_inner())
}

/// 键盘钩子专用：拿不到锁就放行按键，绝不在钩子回调里阻塞。
pub(crate) fn try_shared() -> Option<MutexGuard<'static, Shared>> {
    shared_cell().try_lock().ok()
}

fn refresh_keymap(prefs: &ControlPrefs) {
    let mut keymap = HashMap::new();
    for (id, key, _) in BUTTONS {
        if let Some(code) = prefs.gamepad_map.get(*id).filter(|c| !c.is_empty()) {
            keymap.insert(code.clone(), *key);
        }
    }
    shared().keymap = keymap;
}

// ── 按键发送 ──

struct Job {
    device_id: String,
    key: u32,
    gamepad: bool,
}

static WORKER: OnceLock<Sender<Job>> = OnceLock::new();
/// 设备不认 `input gamepad` 来源时退回普通 `input keyevent`，并记住这个结论。
static GAMEPAD_SOURCE_BAD: AtomicBool = AtomicBool::new(false);

pub(crate) fn push_job(device_id: &str, key: u32, gamepad: bool) {
    if let Some(tx) = WORKER.get() {
        let _ = tx.send(Job {
            device_id: device_id.to_string(),
            key,
            gamepad,
        });
    }
}

fn run_job(app: &AppHandle, job: &Job) {
    let state = app.state::<AppState>();
    let Some(record) = session::active_records(&state, &job.device_id).into_iter().next() else {
        logger::warn("control", format!("设备 {} 没有运行中的会话，按键 {} 未发送", job.device_id, job.key));
        return;
    };
    let Ok(dir) = environment_dir(app) else {
        logger::warn("control", "取运行环境目录失败，按键未发送");
        return;
    };
    let adb = dir.join(ADB_BIN);
    let key = job.key.to_string();
    let conn = record.connection_id;

    let send = |with_source: bool| -> Result<String, String> {
        let mut args: Vec<&str> = vec!["-s", conn.as_str(), "shell", "input"];
        if with_source {
            args.push("gamepad");
        }
        args.push("keyevent");
        args.push(key.as_str());
        device::run_captured(&adb, &args, SEND_TIMEOUT)
    };

    let with_source = job.gamepad && !GAMEPAD_SOURCE_BAD.load(Ordering::Relaxed);
    if let Err(e) = send(with_source) {
        if with_source {
            // 只有普通按键事件能成功时，才断定是设备不认 gamepad 来源（排除设备掉线等一次性失败）。
            match send(false) {
                Ok(_) => {
                    GAMEPAD_SOURCE_BAD.store(true, Ordering::Relaxed);
                    logger::warn(
                        "control",
                        format!("设备不接受 input gamepad，改用普通按键事件：{e}"),
                    );
                }
                Err(e2) => logger::warn("control", format!("发送按键 {key} 失败：{e2}")),
            }
        } else {
            logger::warn("control", format!("发送按键 {key} 失败：{e}"));
        }
    }
}

/// 启动：载入键位、起发送线程，Windows 上再起窗口跟随线程。
pub fn start(app: &AppHandle) {
    {
        let state = app.state::<AppState>();
        let prefs = state.config.lock().unwrap().control.clone();
        refresh_keymap(&prefs);
    }
    let (tx, rx) = mpsc::channel::<Job>();
    let handle = app.clone();
    std::thread::spawn(move || {
        for job in rx {
            run_job(&handle, &job);
        }
    });
    let _ = WORKER.set(tx);
    #[cfg(windows)]
    win::start(app.clone());
}

// ── 命令 ──

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct BarContext {
    pub device_id: String,
    pub alias: String,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct GamepadChanged {
    device_id: String,
    on: bool,
}

/// 控制条窗口自查：窗口标签 `bar-<pid>`，据 pid 找到它服务的会话与设备。
#[tauri::command]
pub fn bar_context(window: tauri::WebviewWindow) -> Result<BarContext, UserError> {
    let pid = window
        .label()
        .strip_prefix("bar-")
        .and_then(|p| p.parse::<u32>().ok())
        .ok_or_else(|| UserError::new(codes::SESSION_NO_SESSION))?;
    let state = window.app_handle().state::<AppState>();
    session::session_infos(&state)
        .into_iter()
        .find(|s| s.pid == pid)
        .map(|s| BarContext {
            device_id: s.device_id,
            alias: s.alias,
        })
        .ok_or_else(|| UserError::new(codes::SESSION_NO_SESSION))
}

#[tauri::command]
pub fn get_control_support() -> bool {
    cfg!(windows)
}

#[tauri::command]
pub fn get_gamepad_modes() -> Vec<String> {
    shared().gamepad_on.iter().cloned().collect()
}

#[tauri::command]
pub fn set_gamepad_mode(app: AppHandle, device_id: String, on: bool) -> Result<(), UserError> {
    {
        let mut sh = shared();
        if on {
            sh.gamepad_on.insert(device_id.clone());
        } else {
            sh.gamepad_on.remove(&device_id);
            sh.pressed.clear();
        }
        GAMEPAD_ACTIVE.store(!sh.gamepad_on.is_empty(), Ordering::Relaxed);
    }
    #[cfg(windows)]
    {
        if on {
            win::ensure_hook();
        }
    }
    logger::info(
        "control",
        format!("设备 {device_id} 手柄模式{}", if on { "开启" } else { "关闭" }),
    );
    emit_gamepad_changed(&app, device_id, on);
    Ok(())
}

/// 通知控制条窗口手柄模式的实际状态（后端自动关闭时也要发，保持按钮与后端一致）。
pub(crate) fn emit_gamepad_changed(app: &AppHandle, device_id: String, on: bool) {
    let _ = app.emit(EVENT_GAMEPAD_CHANGED, GamepadChanged { device_id, on });
}

/// 导航键：`back` / `home` / `recents`。
#[tauri::command]
pub fn send_nav_key(device_id: String, key: String) -> Result<(), UserError> {
    let code = match key.as_str() {
        "back" => KEY_BACK,
        "home" => KEY_HOME,
        "recents" => KEY_APP_SWITCH,
        _ => return Err(UserError::new(codes::CONTROL_MAP_INVALID).with_detail(key)),
    };
    push_job(&device_id, code, false);
    Ok(())
}

/// 控制条上的齿轮：把主窗口带到前台并切到设置页（切页由前端监听事件完成）。
#[tauri::command]
pub fn open_gamepad_settings(app: AppHandle) -> Result<(), UserError> {
    crate::present_main_window(&app, "控制条")
        .map_err(|e| UserError::new(codes::INTERNAL).with_detail(e))?;
    let _ = app.emit_to("main", EVENT_OPEN_GAMEPAD_SETTINGS, ());
    Ok(())
}

/// 保存手柄键位：传入完整映射，校验通过后落盘并立即生效。
#[tauri::command]
pub fn set_gamepad_map(
    app: AppHandle,
    state: State<'_, AppState>,
    map: HashMap<String, String>,
) -> Result<AppConfig, UserError> {
    validate_map(&map)?;
    let cfg = mutate_and_persist(&app, &state, |cfg| {
        cfg.control.gamepad_map = map;
        cfg.control.normalize();
        Ok(())
    })?;
    refresh_keymap(&cfg.control);
    Ok(cfg)
}

#[tauri::command]
pub fn set_control_bar_enabled(
    app: AppHandle,
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<AppConfig, UserError> {
    mutate_and_persist(&app, &state, |cfg| {
        cfg.control.bar_enabled = enabled;
        Ok(())
    })
}
