use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::OnceLock;
use std::time::Duration;
use parking_lot::Mutex;
use tauri::{AppHandle, Emitter, Manager};

use crate::driver::ec_bridge::SharedEcBridge;
use crate::driver::gpu_bridge::SharedGpuBridge;
use crate::commands::system_cmd::collect_hardware_state;

static ACTIVE_SUBSCRIBERS: AtomicU32 = AtomicU32::new(0);
static MAIN_WINDOW_VISIBLE: AtomicBool = AtomicBool::new(true);
static SCREEN_SUSPENDED: AtomicBool = AtomicBool::new(false);

fn get_subscribed_sensors() -> &'static Mutex<HashMap<String, HashSet<String>>> {
    static SENSORS: OnceLock<Mutex<HashMap<String, HashSet<String>>>> = OnceLock::new();
    SENSORS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a widget or external consumer with specific sensor needs
pub fn register_consumer(consumer_id: &str, sensors: &[String]) {
    let mut map = get_subscribed_sensors().lock();
    let set: HashSet<String> = sensors.iter().cloned().collect();
    map.insert(consumer_id.to_string(), set);
    let count = ACTIVE_SUBSCRIBERS.fetch_add(1, Ordering::SeqCst) + 1;
    crate::log_info!(
        "TELEMETRY",
        "Registered consumer '{}' with {} sensors. Total active consumers: {}",
        consumer_id,
        sensors.len(),
        count
    );
}

/// Unregister a widget or external consumer when closed
pub fn unregister_consumer(consumer_id: &str) {
    let mut map = get_subscribed_sensors().lock();
    if map.remove(consumer_id).is_some() {
        let count = ACTIVE_SUBSCRIBERS.fetch_sub(1, Ordering::SeqCst).saturating_sub(1);
        crate::log_info!(
            "TELEMETRY",
            "Unregistered consumer '{}'. Remaining active consumers: {}",
            consumer_id,
            count
        );
    }
}

/// Report main console window visibility (foreground vs tray)
pub fn set_main_window_visible(visible: bool) {
    let old = MAIN_WINDOW_VISIBLE.swap(visible, Ordering::SeqCst);
    // 面板隐藏即开始倒计时卸载 WebView（低内存模式）；重新可见则作废卸载计划。
    // 这里是所有隐藏路径（关闭按钮、托盘切换、失焦自动折叠、最小化）的唯一汇合点。
    if visible {
        crate::system::idle_unload::cancel_main();
    } else {
        crate::system::idle_unload::arm_main();
    }
    if old != visible {
        crate::log_info!(
            "TELEMETRY",
            "Main window visibility changed to: {}. (Heartbeat policy updated)",
            visible
        );
    }
}

static REBOOT_FREEZE: AtomicBool = AtomicBool::new(false);

/// Report system reboot in progress to completely freeze telemetry polling and GPU hardware access
pub fn set_reboot_freeze(freeze: bool) {
    REBOOT_FREEZE.store(freeze, Ordering::SeqCst);
    if freeze {
        crate::log_info!("TELEMETRY", "System reboot initiated: telemetry heartbeat completely frozen.");
    }
}

/// Report screen lock / display sleep event from power listener
pub fn set_screen_suspended(suspended: bool) {
    let old = SCREEN_SUSPENDED.swap(suspended, Ordering::SeqCst);
    if old != suspended {
        crate::log_info!(
            "TELEMETRY",
            "Screen suspended changed to: {}. (Telemetry loop parked/resumed)",
            suspended
        );
    }
}

/// Check if telemetry heartbeat should be actively pulsing (Level 1)
pub fn is_telemetry_active() -> bool {
    if REBOOT_FREEZE.load(Ordering::Relaxed) || SCREEN_SUSPENDED.load(Ordering::Relaxed) {
        return false;
    }
    MAIN_WINDOW_VISIBLE.load(Ordering::Relaxed)
        || ACTIVE_SUBSCRIBERS.load(Ordering::Relaxed) > 0
        || crate::system::watcher_hub::has_active_telemetry_watchers()
}

/// Determine whether NVIDIA dGPU queries (NVAPI) should be executed
/// CRITICAL DEFENSE: If no GPU metrics are requested and main console is closed,
/// NEVER query GPU to prevent waking up NVIDIA dGPU from D3Cold ultra-low power state.
pub fn should_query_gpu() -> bool {
    if MAIN_WINDOW_VISIBLE.load(Ordering::Relaxed) {
        return true;
    }
    if crate::system::watcher_hub::should_query_gpu_for_watchers() {
        return true;
    }
    let map = get_subscribed_sensors().lock();
    for (_id, sensors) in map.iter() {
        if sensors.is_empty() {
            // Empty sensors list in declaration means "all metrics required"
            return true;
        }
        for s in sensors {
            if s.starts_with("gpu") {
                return true;
            }
        }
    }
    false
}

/// Start the background dynamic telemetry heartbeat loop
pub fn start_telemetry_loop(
    app: AppHandle,
    ec_bridge: SharedEcBridge,
    gpu_bridge: SharedGpuBridge,
) {
    std::thread::Builder::new()
        .name("openrevo-telemetry-heartbeat".to_string())
        .spawn(move || {
            crate::log_info!("TELEMETRY", "Adaptive dynamic telemetry heartbeat loop initialized.");
            loop {
                // State 0: System reboot in progress -> Complete freeze to release bus
                if REBOOT_FREEZE.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(2000));
                    continue;
                }

                // State 3: Screen suspended / Lock screen -> Deep Freeze
                if SCREEN_SUSPENDED.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(2000));
                    continue;
                }

                if is_telemetry_active() {
                    // State 1: Active Pulse (1.5s interval)
                    let query_gpu = should_query_gpu();
                    let state = collect_hardware_state(&ec_bridge, &gpu_bridge, query_gpu);
                    if !SCREEN_SUSPENDED.load(Ordering::SeqCst) && !REBOOT_FREEZE.load(Ordering::SeqCst) {
                        let _ = app.emit("hardware-state-updated", &state);
                        crate::system::watcher_hub::evaluate_telemetry_watchers(&state);
                    }
                    std::thread::sleep(Duration::from_millis(1500));
                } else {
                    // State 2: Standby Low Power (20s slow heartbeat, zero events emitted)
                    std::thread::sleep(Duration::from_secs(20));
                }
            }
        })
        .expect("Failed to spawn openrevo telemetry heartbeat thread");
}

#[tauri::command]
pub fn report_window_visibility(app: tauri::AppHandle, visible: bool) -> Result<(), String> {
    // 前端"我可见了"的声明不可信：托盘常驻场景下窗口是 visible:false 创建的，前端挂载完
    // 仍会无条件上报 visible=true。这里以真实窗口状态为准，否则隐藏态永远被当作前台，
    // 遥测不会进待机、低内存卸载也永远不触发。
    let real_visible = app
        .get_webview_window("main")
        .map(|w| w.is_visible().unwrap_or(false))
        .unwrap_or(false);
    set_main_window_visible(visible && real_visible);
    Ok(())
}

#[tauri::command]
pub fn get_telemetry_heartbeat_status() -> Result<serde_json::Value, String> {
    let consumers = ACTIVE_SUBSCRIBERS.load(Ordering::Relaxed);
    let main_visible = MAIN_WINDOW_VISIBLE.load(Ordering::Relaxed);
    let screen_suspended = SCREEN_SUSPENDED.load(Ordering::Relaxed);
    let active = is_telemetry_active();
    let query_gpu = should_query_gpu();

    Ok(serde_json::json!({
        "active": active,
        "consumers": consumers,
        "main_visible": main_visible,
        "screen_suspended": screen_suspended,
        "query_gpu": query_gpu,
    }))
}
