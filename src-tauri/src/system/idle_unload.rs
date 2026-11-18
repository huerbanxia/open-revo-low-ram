// OpenRevo - Idle WebView Unload (低内存空闲卸载)
// ------------------------------------------------------------------
// 背景：Tauri 在 Windows 上跑的是 WebView2（Chromium 多进程）。调用 window.hide() 只是
//       不画窗口，browser / renderer / GPU / network 四个进程一个都不会退。本项目实测
//       隐藏态 WebView 侧占 ~300MB private，而 Rust 主进程只有 ~20MB。
//
// 策略：面板隐藏超过 N 秒后，直接 destroy 掉对应的 WebviewWindow（不是 hide），让
//       Chromium 整套进程退出；托盘、全局热键、EC/WMI 监听、遥测心跳都在 Rust 侧，
//       功能不退化，代价是下次打开面板要 300~800ms 冷启动。
//
// 两条独立通道：
//   · main：主控制面板（迷你抽屉 / 完整面板），隐藏后延迟卸载
//   · osd ：OSD HUD 浮层，出厂是"常驻预热"的，白白占着一整个 renderer
//
// 语义约定：
//   · delay == 0 表示禁用该通道，回到原来的常驻热态行为
//   · arm_* 每次调用推进一个代际令牌，任何一次显示动作都会让旧定时器立刻失效，
//     所以反复开关面板不会误杀正在用的窗口
// ------------------------------------------------------------------

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};

/// 全局 AppHandle：隐藏/显示动作分散在窗口事件、托盘、前端命令里，逐处传 handle 太啰嗦。
static APP: OnceLock<AppHandle> = OnceLock::new();

/// 代际令牌：递增即代表"上一次卸载计划作废"
static MAIN_EPOCH: AtomicU64 = AtomicU64::new(0);
static OSD_EPOCH: AtomicU64 = AtomicU64::new(0);

/// 启动时绑定 AppHandle（在 setup 里调用一次）
pub fn bind_app(app: &AppHandle) {
    let _ = APP.set(app.clone());
}

fn app_handle() -> Option<AppHandle> {
    APP.get().cloned()
}

/// 主面板隐藏后安排一次延迟卸载；delay=0 时直接返回（保持常驻）
pub fn arm_main() {
    let Some(app) = app_handle() else { return };
    let delay = crate::system::config::load_config().idle_unload_secs;
    if delay == 0 {
        return;
    }
    let epoch = MAIN_EPOCH.fetch_add(1, Ordering::SeqCst) + 1;
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(delay);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(500));
            // 期间被显示过 / 计划被重置 → 放弃本次卸载
            if MAIN_EPOCH.load(Ordering::SeqCst) != epoch {
                return;
            }
            // 窗口已重新可见（例如自动折叠被取消）→ 放弃
            match app.get_webview_window("main") {
                Some(w) if !w.is_visible().unwrap_or(false) && !w.is_minimized().unwrap_or(false) => {}
                _ => return,
            }
        }
        if MAIN_EPOCH.load(Ordering::SeqCst) != epoch {
            return;
        }
        unload_main(&app);
    });
}

/// 取消主面板的待卸载计划（面板被重新显示时调用）
pub fn cancel_main() {
    MAIN_EPOCH.fetch_add(1, Ordering::SeqCst);
}

/// OSD 浮层隐藏后安排一次延迟卸载
pub fn arm_osd() {
    let Some(app) = app_handle() else { return };
    let delay = crate::system::config::load_config().osd_idle_unload_secs;
    if delay == 0 {
        return;
    }
    let epoch = OSD_EPOCH.fetch_add(1, Ordering::SeqCst) + 1;
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(delay);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(500));
            if OSD_EPOCH.load(Ordering::SeqCst) != epoch {
                return;
            }
            match app.get_webview_window("osd") {
                Some(w) if !w.is_visible().unwrap_or(false) => {}
                _ => return,
            }
        }
        if OSD_EPOCH.load(Ordering::SeqCst) != epoch {
            return;
        }
        if let Some(w) = app.get_webview_window("osd") {
            if w.is_visible().unwrap_or(false) {
                return;
            }
            crate::log_info!(
                "LOWRAM",
                "Idle unload: destroying hidden 'osd' webview to release Chromium processes"
            );
            let _ = w.destroy();
        }
    });
}

/// 取消 OSD 的待卸载计划
pub fn cancel_osd() {
    OSD_EPOCH.fetch_add(1, Ordering::SeqCst);
}

/// 立即卸载主面板 WebView（仅在隐藏/最小化状态执行，避免销毁正在使用的界面）
pub fn unload_main(app: &AppHandle) {
    let Some(win) = app.get_webview_window("main") else {
        return;
    };
    if win.is_visible().unwrap_or(false) || win.is_minimized().unwrap_or(false) {
        return;
    }

    // destroy 不走 WindowEvent::CloseRequested，几何位置必须在这里自己落盘，
    // 否则重建后会丢失完整面板的位置。
    persist_main_geometry(&win);

    crate::log_info!(
        "LOWRAM",
        "Idle unload: destroying hidden 'main' webview to release Chromium processes (tray/hotkeys/EC keep running in Rust)"
    );
    if let Err(e) = win.destroy() {
        crate::log_error!("LOWRAM", "Failed to destroy 'main' webview: {}", e);
    }
}

/// 与 CloseRequested 分支保持同一套几何持久化口径
fn persist_main_geometry(win: &tauri::WebviewWindow) {
    let scale = win.scale_factor().unwrap_or(1.0);
    if let Ok(sz) = win.inner_size() {
        if (sz.width as f64 / scale) > 600.0 {
            if let Ok(pos) = win.outer_position() {
                crate::commands::hardware_cmd::save_full_window_pos(pos.x, pos.y);
            }
        }
    }
}
