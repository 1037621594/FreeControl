//! 设备文件浏览：目录列表、新建 / 重命名 / 删除、设备内复制 / 移动，以及与电脑之间的拉取 / 推送。
//!
//! 全部经运行环境内的 adb 完成。设备侧命令只用 toybox / mksh 都有的 `ls -lA`、`cp -r`、`mv`、
//! `rm -rf`、`mkdir`，路径一律单引号转义后再拼进 `adb shell` 的脚本，不信任任何外部输入。
//! 复制 / 移动 / 上传 / 下载默认不覆盖同名项目：命中的来源放进 `conflicts` 交给前端确认，
//! 确认后以 `overwrite = true` 只重跑这些来源。传输可经 `cancel_file_transfer` 中止。

use crate::apps::{device_and_connection, environment_dir};
use crate::assets::ADB_BIN;
use crate::download::run_blocking;
use crate::logger;
use crate::ui_message::{codes, UserError};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tauri::AppHandle;

const LIST_TIMEOUT: Duration = Duration::from_secs(30);
const OP_TIMEOUT: Duration = Duration::from_secs(60);
/// 设备内复制 / 移动 / 删除可能处理很大的目录树，预算放宽。
const BULK_TIMEOUT: Duration = Duration::from_secs(1800);
const WAIT_TICK: Duration = Duration::from_millis(30);
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub name: String,
    /// 目录，或指向目录的符号链接。
    pub is_dir: bool,
    pub is_link: bool,
    pub size: u64,
    /// `YYYY-MM-DD HH:MM`，取自设备 `ls` 的输出。
    pub modified: String,
    pub mode: String,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct FileListing {
    /// 规范化后的绝对路径。
    pub path: String,
    pub entries: Vec<FileEntry>,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct FileOpFailure {
    pub name: String,
    pub error: UserError,
}

#[derive(Serialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct FileOpOutcome {
    pub ok: u32,
    pub failed: Vec<FileOpFailure>,
    /// 因目标已存在而跳过的来源（与请求里的来源写法一致），确认覆盖后由前端带 `overwrite` 重发。
    pub conflicts: Vec<String>,
    pub canceled: bool,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PasteRequest {
    pub device_id: String,
    pub sources: Vec<String>,
    pub dest_dir: String,
    /// `copy` / `move`
    pub mode: String,
    pub overwrite: bool,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub device_id: String,
    pub paths: Vec<String>,
    pub local_dir: String,
    pub overwrite: bool,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PushRequest {
    pub device_id: String,
    pub local_paths: Vec<String>,
    pub dest_dir: String,
    pub overwrite: bool,
}

// ── 命令 ──

#[tauri::command]
pub async fn list_device_files(
    app: AppHandle,
    device_id: String,
    path: String,
) -> Result<FileListing, UserError> {
    run_blocking(move || list_blocking(&app, &device_id, &path)).await
}

#[tauri::command]
pub async fn make_device_dir(
    app: AppHandle,
    device_id: String,
    parent: String,
    name: String,
) -> Result<(), UserError> {
    run_blocking(move || make_dir_blocking(&app, &device_id, &parent, &name)).await
}

#[tauri::command]
pub async fn rename_device_file(
    app: AppHandle,
    device_id: String,
    path: String,
    new_name: String,
) -> Result<(), UserError> {
    run_blocking(move || rename_blocking(&app, &device_id, &path, &new_name)).await
}

#[tauri::command]
pub async fn delete_device_files(
    app: AppHandle,
    device_id: String,
    paths: Vec<String>,
) -> Result<FileOpOutcome, UserError> {
    run_blocking(move || delete_blocking(&app, &device_id, &paths)).await
}

#[tauri::command]
pub async fn paste_device_files(
    app: AppHandle,
    request: PasteRequest,
) -> Result<FileOpOutcome, UserError> {
    run_blocking(move || paste_blocking(&app, request)).await
}

#[tauri::command]
pub async fn pull_device_files(
    app: AppHandle,
    request: PullRequest,
) -> Result<FileOpOutcome, UserError> {
    run_blocking(move || pull_blocking(&app, request)).await
}

#[tauri::command]
pub async fn push_device_files(
    app: AppHandle,
    request: PushRequest,
) -> Result<FileOpOutcome, UserError> {
    run_blocking(move || push_blocking(&app, request)).await
}

/// 中止该设备正在进行的拉取 / 推送：标记取消并结束当前 adb 进程，剩余项目不再处理。
#[tauri::command]
pub async fn cancel_file_transfer(device_id: String) -> Result<(), UserError> {
    run_blocking(move || {
        let pid = {
            let mut state = transfers();
            state.canceled.insert(device_id.clone());
            let pid = state.pids.get(&device_id).copied();
            pid
        };
        if let Some(pid) = pid {
            kill_pid(pid);
        }
        Ok::<(), UserError>(())
    })
    .await
}

// ── 实现 ──

fn list_blocking(app: &AppHandle, device_id: &str, path: &str) -> Result<FileListing, UserError> {
    let path = device_path(path)?;
    let (_, conn) = device_and_connection(app, device_id)?;
    let dir = environment_dir(app)?;

    let script = format!("ls -lA {}", sh_quote(&dir_arg(&path)));
    let out = adb_shell(&dir, &conn, &script, LIST_TIMEOUT)
        .map_err(|e| UserError::new(codes::FILES_LIST_FAILED).with_detail(e))?;
    let mut entries = parse_listing(&out.stdout);
    if entries.is_empty() && (!out.success || !out.stderr.trim().is_empty()) {
        return Err(UserError::new(codes::FILES_LIST_FAILED).with_detail(detail_of(&out)));
    }

    let links: Vec<String> = entries
        .iter()
        .filter(|e| e.is_link)
        .map(|e| sh_quote(&e.name))
        .collect();
    if !links.is_empty() {
        let script = format!(
            "cd {} 2>/dev/null || exit 0; for n in {}; do if [ -d \"$n\" ]; then printf '%s\\n' \"$n\"; fi; done",
            sh_quote(&path),
            links.join(" ")
        );
        if let Ok(res) = adb_shell(&dir, &conn, &script, LIST_TIMEOUT) {
            let dirs: HashSet<&str> = res.stdout.lines().map(|l| l.trim_end_matches('\r')).collect();
            for entry in entries.iter_mut().filter(|e| e.is_link) {
                if dirs.contains(entry.name.as_str()) {
                    entry.is_dir = true;
                }
            }
        }
    }

    Ok(FileListing { path, entries })
}

fn make_dir_blocking(
    app: &AppHandle,
    device_id: &str,
    parent: &str,
    name: &str,
) -> Result<(), UserError> {
    let parent = device_path(parent)?;
    check_name(name)?;
    let (device, conn) = device_and_connection(app, device_id)?;
    let dir = environment_dir(app)?;
    let target = join_path(&parent, name);
    shell_ok(&dir, &conn, &format!("mkdir {}", sh_quote(&target)), OP_TIMEOUT)
        .map_err(|e| UserError::new(codes::FILES_OP_FAILED).with_detail(e))?;
    logger::info(
        "files",
        format!("新建文件夹 {}（设备「{}」）", target, device.alias),
    );
    Ok(())
}

fn rename_blocking(
    app: &AppHandle,
    device_id: &str,
    path: &str,
    new_name: &str,
) -> Result<(), UserError> {
    let src = protected_path(path)?;
    check_name(new_name)?;
    let dest = join_path(&parent_of(&src), new_name);
    let (device, conn) = device_and_connection(app, device_id)?;
    let dir = environment_dir(app)?;
    let (s, d) = (sh_quote(&src), sh_quote(&dest));
    let script = format!("if [ -e {d} ] || [ -L {d} ]; then echo EXISTS >&2; exit 3; fi; mv {s} {d}");
    shell_ok(&dir, &conn, &script, OP_TIMEOUT).map_err(|e| {
        if e.contains("EXISTS") {
            UserError::new(codes::FILES_NAME_EXISTS)
        } else {
            UserError::new(codes::FILES_OP_FAILED).with_detail(e)
        }
    })?;
    logger::info(
        "files",
        format!("重命名 {} → {}（设备「{}」）", src, dest, device.alias),
    );
    Ok(())
}

fn delete_blocking(
    app: &AppHandle,
    device_id: &str,
    paths: &[String],
) -> Result<FileOpOutcome, UserError> {
    let mut targets = Vec::new();
    for p in paths {
        targets.push(protected_path(p)?);
    }
    let mut outcome = FileOpOutcome::default();
    if targets.is_empty() {
        return Ok(outcome);
    }
    let (device, conn) = device_and_connection(app, device_id)?;
    let dir = environment_dir(app)?;

    let list = targets.iter().map(|p| sh_quote(p)).collect::<Vec<_>>().join(" ");
    let script = format!(
        "for p in {list}; do rm -rf \"$p\" 2>&1; if [ -e \"$p\" ] || [ -L \"$p\" ]; then printf 'FAIL\\t%s\\n' \"$p\"; fi; done"
    );
    let out = adb_shell(&dir, &conn, &script, BULK_TIMEOUT)
        .map_err(|e| UserError::new(codes::FILES_OP_FAILED).with_detail(e))?;

    let mut failed: HashSet<String> = HashSet::new();
    let mut reasons: Vec<&str> = Vec::new();
    for line in out.stdout.lines().map(|l| l.trim_end_matches('\r')) {
        if let Some(p) = line.strip_prefix("FAIL\t") {
            failed.insert(p.to_string());
        } else if !line.trim().is_empty() {
            reasons.push(line.trim());
        }
    }
    let reason: String = reasons.join("；").chars().take(400).collect();
    for p in &targets {
        if failed.contains(p) {
            outcome.failed.push(failure(base_of(p), codes::FILES_OP_FAILED, reason.clone()));
        } else {
            outcome.ok += 1;
        }
    }
    logger::info(
        "files",
        format!(
            "删除 {} 项，失败 {} 项（设备「{}」）",
            outcome.ok,
            outcome.failed.len(),
            device.alias
        ),
    );
    Ok(outcome)
}

fn paste_blocking(app: &AppHandle, req: PasteRequest) -> Result<FileOpOutcome, UserError> {
    let is_move = match req.mode.as_str() {
        "move" => true,
        "copy" => false,
        other => {
            return Err(UserError::new(codes::FILES_OP_FAILED)
                .with_detail(format!("未知的粘贴方式：{other}")))
        }
    };
    let dest = device_path(&req.dest_dir)?;
    let mut sources = Vec::new();
    for s in &req.sources {
        sources.push(protected_path(s)?);
    }
    let (device, conn) = device_and_connection(app, &req.device_id)?;
    let dir = environment_dir(app)?;
    let mut outcome = FileOpOutcome::default();

    // 来源就在目标目录里：移动无事可做，复制由脚本自动改名，都不参与同名检查。
    let existing = if req.overwrite {
        HashSet::new()
    } else {
        let names: Vec<String> = sources
            .iter()
            .filter(|s| parent_of(s) != dest)
            .map(|s| base_of(s).to_string())
            .collect();
        existing_names(&dir, &conn, &dest, &names)?
    };

    for src in &sources {
        let name = base_of(src);
        let same_dir = parent_of(src) == dest;
        if dest == *src || dest.starts_with(&format!("{src}/")) {
            outcome.failed.push(FileOpFailure {
                name: name.to_string(),
                error: UserError::new(codes::FILES_INTO_ITSELF),
            });
            continue;
        }
        if same_dir && is_move {
            outcome.ok += 1;
            continue;
        }
        if !same_dir && existing.contains(name) {
            outcome.conflicts.push(src.clone());
            continue;
        }
        let script = if same_dir {
            let (stem, ext) = split_name(name);
            format!(
                "d={}; stem={}; ext={}; i=1; while [ -e \"$d/$stem\"_copy\"$i$ext\" ]; do i=$((i+1)); done; cp -r {} \"$d/$stem\"_copy\"$i$ext\"",
                sh_quote(&dest),
                sh_quote(stem),
                sh_quote(ext),
                sh_quote(src)
            )
        } else if is_move {
            format!("mv -f {} {}", sh_quote(src), sh_quote(&dir_arg(&dest)))
        } else {
            format!("cp -r {} {}", sh_quote(src), sh_quote(&dir_arg(&dest)))
        };
        match shell_ok(&dir, &conn, &script, BULK_TIMEOUT) {
            Ok(()) => outcome.ok += 1,
            Err(e) => outcome.failed.push(failure(name, codes::FILES_OP_FAILED, e)),
        }
    }
    logger::info(
        "files",
        format!(
            "{} {} 项到 {}，失败 {} 项，待确认覆盖 {} 项（设备「{}」）",
            if is_move { "移动" } else { "复制" },
            outcome.ok,
            dest,
            outcome.failed.len(),
            outcome.conflicts.len(),
            device.alias
        ),
    );
    Ok(outcome)
}

fn pull_blocking(app: &AppHandle, req: PullRequest) -> Result<FileOpOutcome, UserError> {
    let local_dir = PathBuf::from(req.local_dir.trim());
    if !local_dir.is_dir() {
        return Err(UserError::new(codes::FILES_LOCAL_MISSING)
            .with_detail(local_dir.display().to_string()));
    }
    let mut paths = Vec::new();
    for p in &req.paths {
        paths.push(device_path(p)?);
    }
    let (device, conn) = device_and_connection(app, &req.device_id)?;
    let dir = environment_dir(app)?;
    let mut local_arg = local_dir.display().to_string();
    // 去掉末尾的 `\`（盘符根目录 `D:\` 除外），否则 adb 会把它拼成 `D:\tt\.`。
    while local_arg.len() > 3 && (local_arg.ends_with('\\') || local_arg.ends_with('/')) {
        local_arg.pop();
    }
    let mut outcome = FileOpOutcome::default();

    clear_canceled(&req.device_id);
    for p in &paths {
        if was_canceled(&req.device_id) {
            outcome.canceled = true;
            break;
        }
        let name = base_of(p);
        if !req.overwrite && local_dir.join(name).exists() {
            outcome.conflicts.push(p.clone());
            continue;
        }
        // 直接给出完整目标路径，避免 adb 在 Windows 上用目录参数拼接出 `D:\xx\.` 这类非法路径。
        // 目标已存在且是目录时（覆盖文件夹），仍传父目录，让 adb 合并进去而不是多嵌套一层。
        let target = local_dir.join(name);
        let dest_arg = if target.is_dir() {
            local_arg.clone()
        } else {
            target.display().to_string()
        };
        let args = vec![
            "-s".to_string(),
            conn.clone(),
            "pull".to_string(),
            p.clone(),
            dest_arg,
        ];
        let res = run_adb(&dir, &args, None, Some(req.device_id.as_str()));
        if was_canceled(&req.device_id) {
            outcome.canceled = true;
            break;
        }
        match res {
            Ok(o) if o.success => outcome.ok += 1,
            Ok(o) => outcome
                .failed
                .push(failure(name, codes::FILES_TRANSFER_FAILED, detail_of(&o))),
            Err(e) => outcome
                .failed
                .push(failure(name, codes::FILES_TRANSFER_FAILED, e)),
        }
    }
    clear_canceled(&req.device_id);
    logger::info(
        "files",
        format!(
            "下载 {} 项到 {}，失败 {} 项，待确认覆盖 {} 项（设备「{}」）",
            outcome.ok,
            local_arg,
            outcome.failed.len(),
            outcome.conflicts.len(),
            device.alias
        ),
    );
    Ok(outcome)
}

fn push_blocking(app: &AppHandle, req: PushRequest) -> Result<FileOpOutcome, UserError> {
    let dest = device_path(&req.dest_dir)?;
    let (device, conn) = device_and_connection(app, &req.device_id)?;
    let dir = environment_dir(app)?;
    let mut outcome = FileOpOutcome::default();

    // (本地路径, 推送后在设备上的名称)；本地不存在的直接记为失败。
    let mut items: Vec<(String, String)> = Vec::new();
    for lp in &req.local_paths {
        let local = Path::new(lp);
        let name = local
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.is_empty() || !local.exists() {
            outcome.failed.push(failure(
                if name.is_empty() { lp } else { &name },
                codes::FILES_LOCAL_MISSING,
                lp.clone(),
            ));
            continue;
        }
        items.push((lp.clone(), name));
    }

    let existing = if req.overwrite {
        HashSet::new()
    } else {
        let names: Vec<String> = items.iter().map(|(_, n)| n.clone()).collect();
        existing_names(&dir, &conn, &dest, &names)?
    };

    clear_canceled(&req.device_id);
    for (local, name) in &items {
        if was_canceled(&req.device_id) {
            outcome.canceled = true;
            break;
        }
        if existing.contains(name) {
            outcome.conflicts.push(local.clone());
            continue;
        }
        let args = vec![
            "-s".to_string(),
            conn.clone(),
            "push".to_string(),
            local.clone(),
            dir_arg(&dest),
        ];
        let res = run_adb(&dir, &args, None, Some(req.device_id.as_str()));
        if was_canceled(&req.device_id) {
            outcome.canceled = true;
            break;
        }
        match res {
            Ok(o) if o.success => outcome.ok += 1,
            Ok(o) => outcome
                .failed
                .push(failure(name, codes::FILES_TRANSFER_FAILED, detail_of(&o))),
            Err(e) => outcome
                .failed
                .push(failure(name, codes::FILES_TRANSFER_FAILED, e)),
        }
    }
    clear_canceled(&req.device_id);
    logger::info(
        "files",
        format!(
            "上传 {} 项到 {}，失败 {} 项，待确认覆盖 {} 项（设备「{}」）",
            outcome.ok,
            dest,
            outcome.failed.len(),
            outcome.conflicts.len(),
            device.alias
        ),
    );
    Ok(outcome)
}

/// 目标目录里已存在的同名项目（含悬空符号链接）。
fn existing_names(
    dir: &Path,
    conn: &str,
    dest_dir: &str,
    names: &[String],
) -> Result<HashSet<String>, UserError> {
    if names.is_empty() {
        return Ok(HashSet::new());
    }
    let list = names.iter().map(|n| sh_quote(n)).collect::<Vec<_>>().join(" ");
    let script = format!(
        "cd {} 2>/dev/null || exit 0; for n in {list}; do if [ -e \"$n\" ] || [ -L \"$n\" ]; then printf '%s\\n' \"$n\"; fi; done",
        sh_quote(dest_dir)
    );
    let out = adb_shell(dir, conn, &script, OP_TIMEOUT)
        .map_err(|e| UserError::new(codes::FILES_OP_FAILED).with_detail(e))?;
    Ok(out
        .stdout
        .lines()
        .map(|l| l.trim_end_matches('\r').to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

// ── adb 调用 ──

struct AdbOutput {
    stdout: String,
    stderr: String,
    success: bool,
}

#[derive(Default)]
struct TransferState {
    pids: HashMap<String, u32>,
    canceled: HashSet<String>,
}

fn transfers() -> MutexGuard<'static, TransferState> {
    static STATE: OnceLock<Mutex<TransferState>> = OnceLock::new();
    STATE
        .get_or_init(|| Mutex::new(TransferState::default()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn was_canceled(device_id: &str) -> bool {
    transfers().canceled.contains(device_id)
}

fn clear_canceled(device_id: &str) {
    transfers().canceled.remove(device_id);
}

fn kill_pid(pid: u32) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }
}

fn spawn_reader<R: Read + Send + 'static>(src: Option<R>) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut s) = src {
            let _ = s.read_to_end(&mut buf);
        }
        buf
    })
}

/// 运行一次 adb。输出由读线程并发收集（避免管道写满后子进程挂住）；
/// `track` 非空时登记进程号，供 `cancel_file_transfer` 结束它。
fn run_adb(
    dir: &Path,
    args: &[String],
    timeout: Option<Duration>,
    track: Option<&str>,
) -> Result<AdbOutput, String> {
    let adb = dir.join(ADB_BIN);
    if !adb.is_file() {
        return Err(format!("运行环境内未找到 {ADB_BIN}"));
    }
    let mut cmd = Command::new(&adb);
    cmd.args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().map_err(|e| format!("启动 adb 失败：{e}"))?;
    let pid = child.id();
    if let Some(id) = track {
        transfers().pids.insert(id.to_string(), pid);
    }
    let stdout_reader = spawn_reader(child.stdout.take());
    let stderr_reader = spawn_reader(child.stderr.take());

    let deadline = timeout.map(|t| Instant::now() + t);
    let waited: Result<bool, String> = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status.success()),
            Ok(None) => {
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    kill_pid(pid);
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(format!(
                        "adb 操作超时（超过 {} 秒）",
                        timeout.map(|t| t.as_secs()).unwrap_or(0)
                    ));
                }
                std::thread::sleep(WAIT_TICK);
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!("等待 adb 退出失败：{e}"));
            }
        }
    };
    if let Some(id) = track {
        transfers().pids.remove(id);
    }
    let success = waited?;
    let stdout = String::from_utf8_lossy(&stdout_reader.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_reader.join().unwrap_or_default()).into_owned();
    Ok(AdbOutput {
        stdout,
        stderr,
        success,
    })
}

fn adb_shell(dir: &Path, conn: &str, script: &str, timeout: Duration) -> Result<AdbOutput, String> {
    let args = vec![
        "-s".to_string(),
        conn.to_string(),
        "shell".to_string(),
        script.to_string(),
    ];
    run_adb(dir, &args, Some(timeout), None)
}

/// 退出码成功且 stderr 为空才算成功（旧设备的 `adb shell` 不回传退出码，只能靠 stderr 判断）。
fn shell_ok(dir: &Path, conn: &str, script: &str, timeout: Duration) -> Result<(), String> {
    let out = adb_shell(dir, conn, script, timeout)?;
    if out.success && out.stderr.trim().is_empty() {
        Ok(())
    } else {
        Err(detail_of(&out))
    }
}

fn detail_of(out: &AdbOutput) -> String {
    let text = if out.stderr.trim().is_empty() {
        out.stdout.trim()
    } else {
        out.stderr.trim()
    };
    if text.is_empty() {
        "操作未成功".to_string()
    } else {
        text.chars().take(400).collect()
    }
}

fn failure(name: &str, code: &str, detail: String) -> FileOpFailure {
    FileOpFailure {
        name: name.to_string(),
        error: UserError::new(code).with_detail(detail),
    }
}

// ── 路径与名称 ──

/// 单引号转义，使任意字符串都能安全拼进设备 shell 脚本。
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 设备路径规范化：必须是绝对路径、不含控制字符；折叠 `.` / `..` / 重复斜杠。
fn device_path(raw: &str) -> Result<String, UserError> {
    if !raw.starts_with('/') || raw.chars().any(|c| c.is_control()) {
        return Err(UserError::new(codes::FILES_PATH_INVALID).with_detail(raw.to_string()));
    }
    let mut parts: Vec<&str> = Vec::new();
    for seg in raw.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    Ok(if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", parts.join("/"))
    })
}

/// 会被改动或删除的路径：至少两级，避免误动 `/`、`/sdcard`、`/system` 这类顶层目录。
fn protected_path(raw: &str) -> Result<String, UserError> {
    let path = device_path(raw)?;
    if path.split('/').filter(|s| !s.is_empty()).count() < 2 {
        return Err(UserError::new(codes::FILES_PROTECTED).with_detail(path));
    }
    Ok(path)
}

fn check_name(name: &str) -> Result<(), UserError> {
    let bad = name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > 255
        || name.contains('/')
        || name.chars().any(|c| c.is_control());
    if bad {
        return Err(UserError::new(codes::FILES_NAME_INVALID));
    }
    Ok(())
}

fn parent_of(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => path[..i].to_string(),
    }
}

fn base_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn join_path(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// 末尾补 `/`：让指向目录的符号链接被跟随，也让 `cp` / `mv` / `push` 把它当目标目录。
fn dir_arg(path: &str) -> String {
    if path == "/" {
        "/".to_string()
    } else {
        format!("{path}/")
    }
}

fn split_name(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

// ── `ls -lA` 解析 ──

fn parse_listing(text: &str) -> Vec<FileEntry> {
    text.lines()
        .filter_map(|l| parse_line(l.trim_end_matches('\r')))
        .filter(|e| e.name != "." && e.name != "..")
        .collect()
}

fn is_date(token: &str) -> bool {
    let b = token.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

/// 解析一行 `ls -l`：`权限 [链接数] 属主 属组 [大小] YYYY-MM-DD HH:MM 名称`。
/// 以日期列为锚点，兼容 toybox（含链接数与大小）与旧版 toolbox（无链接数、目录无大小）。
fn parse_line(line: &str) -> Option<FileEntry> {
    let kind = line.chars().next()?;
    if !matches!(kind, '-' | 'd' | 'l' | 'b' | 'c' | 'p' | 's') {
        return None;
    }
    let bytes = line.as_bytes();
    let mut toks: Vec<(usize, usize)> = Vec::with_capacity(9);
    let mut i = 0;
    while i < bytes.len() && toks.len() < 9 {
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let start = i;
        while i < bytes.len() && bytes[i] != b' ' && bytes[i] != b'\t' {
            i += 1;
        }
        toks.push((start, i));
    }
    let tok = |k: usize| &line[toks[k].0..toks[k].1];
    if toks.len() < 4 || tok(0).len() < 10 {
        return None;
    }
    let date_idx = (2..toks.len() - 1).find(|&k| is_date(tok(k)))?;

    let rest = &line[toks[date_idx + 1].1..];
    let raw_name = rest.strip_prefix(' ').unwrap_or_else(|| rest.trim_start());
    let is_link = kind == 'l';
    let name = if is_link {
        raw_name.split_once(" -> ").map(|(n, _)| n).unwrap_or(raw_name)
    } else {
        raw_name
    };
    if name.is_empty() {
        return None;
    }
    let size = if date_idx >= 4 {
        tok(date_idx - 1).parse::<u64>().unwrap_or(0)
    } else {
        0
    };
    let time = tok(date_idx + 1);
    Some(FileEntry {
        name: name.to_string(),
        is_dir: kind == 'd',
        is_link,
        size,
        modified: format!("{} {}", tok(date_idx), time.get(..5).unwrap_or(time)),
        mode: tok(0).to_string(),
    })
}
