# 低内存空闲卸载（Low-RAM Idle Unload）

OpenRevo 是托盘常驻的控制中心：绝大多数时间没有任何界面。而 Tauri 在 Windows 上跑的是
WebView2（一整套 Chromium 多进程），`window.hide()` 只是不画窗口 —— browser / renderer /
GPU / network 四个进程一个都不会退，V8 堆与合成层也不释放。这就是"不显示 GUI 却常驻
300MB+"的全部原因。

本文记录该问题的量化结论与已落地的改造。

---

## 1. 实测基线（改造前，隐藏态）

用进程树 private bytes（commit）口径统计，避免任务管理器把共享页算进工作集：

| 进程 | 类型 | private |
|---|---|---|
| msedgewebview2 | gpu-process | ~125–172 MB |
| msedgewebview2 | renderer（主面板） | ~62 MB |
| msedgewebview2 | renderer（OSD） | ~40–60 MB |
| msedgewebview2 | browser | ~41 MB |
| msedgewebview2 | utility × 2 | ~23 MB |
| msedgewebview2 | crashpad-handler | ~4 MB |
| open-revo | Rust 主进程 | **~19 MB** |
| **合计** | **8 进程** | **~335 MB** |

关键数据（CDP 直读 WebView2 内部计数）：

- 主面板 DOM 节点仅 **308 个**，JS 堆 **3.1–4.6 MB**（强制 GC 后不变）
- OSD 页面 DOM **20 个**，JS 堆 **2.8 MB**

也就是说：**内存不是前端代码写出来的，是"存在一个 WebView2 实例"这件事本身的开销。**
任何前端层面的压缩（少几个 DOM、减小 bundle）都动不了这块大头。

---

## 2. 改造后的行为

| 场景 | 进程数 | private |
|---|---|---|
| 启动态（隐藏，OSD 不再预热） | 7 | ~275 MB |
| 隐藏 ≥ `idle_unload_secs`（默认 5s） | **1** | **~18 MB** |
| 面板展开（有界面时） | 7 | ~310 MB |
| 隐藏 ≥ `osd_idle_unload_secs`（默认 120s，OSD 卸载） | 视主面板而定 | 少一个 renderer |

即：**空闲常驻从 335MB 降到 18MB（-94.6%）**，这才是 README 里"常驻内存低至约 15MB"
本该有的状态。

---

## 3. 机制

1. 所有"面板隐藏"的路径（标题栏关闭、托盘切换、失焦自动折叠、最小化上送）都汇合到
   `telemetry::set_main_window_visible(false)`，在那里挂一次延迟卸载计划（`idle_unload::arm_main`）。
2. 计划执行前每 500ms 复查：窗口是否重新可见 / 计划是否被新动作作废（代际令牌）。
   到点仍处于隐藏态，则 `WebviewWindow::destroy()`（注意不是 `close()`：本项目在
   `CloseRequested` 里 `prevent_close()` 并把窗口 hide，`close()` 杀不掉 WebView）。
3. 销毁前手动落盘完整面板坐标（`destroy` 不走 `CloseRequested` 的持久化分支）。
4. 任何"要显示面板"的入口都会走 `window::ensure_window_alive()`：窗口不存在就地
   `WebviewWindowBuilder` 重建（参数与 `tauri.conf.json` 的 main 一致），再 `init_main_window`
   恢复尺寸/位置/前次模式。因此托盘左键、托盘菜单、单实例二次启动、插件唤醒全部零改动可用。
5. OSD 同理：`dismiss_osd_window` 后挂延迟卸载；`present_osd_window` 发现窗口不存在时
   登记"重建后要显示"并交给 `WebviewOrchestrator` 异步重建，重建完成后立刻呈现
   （否则会出现"只建不显"）。
6. 必须拦截 `RunEvent::ExitRequested`：WebView 全销毁后窗口数为 0，Tauri 默认会结束事件
   循环，那等于把托盘常驻一起干掉。只拦截"窗口关完了"（`code == None`），显式 `app.exit()`
   仍然放行。

---

## 4. 配置项

`%APPDATA%\OpenRevo\config.json`（或便携模式 `.\data\config.json`）新增：

| 字段 | 默认 | 说明 |
|---|---|---|
| `idle_unload_secs` | `5` | 主动关闭面板（×／托盘隐藏）后多少秒卸载 WebView；`0` = 关闭（恢复旧的常驻热态） |
| `idle_unload_blur_secs` | `240` | **失焦自动折叠**触发隐藏时的宽限期。这是高频动作（点一下别处就收），用户往往几秒内就回来，所以给宽一点，避免每次唤回都吃冷启动；`0` = 该路径不卸载 |
| `osd_idle_unload_secs` | `120` | OSD 浮层隐藏多久后卸载；`0` = 关闭，并恢复开机预热 OSD |

想完全回到旧行为：两项都设 0。

---

## 5. 代价（明确不藏）

- **首次打开面板有 300~800ms 冷启动**（WebView2 进程拉起 + 页面首帧）。主动关掉后 5 秒才释放，
  这 5 秒内再打开是热态、瞬间弹出；失焦自动折叠有 240 秒宽限，日常反复开合不会触发冷启动。
- **前端内存态不保留**：滚动位置、未提交的表单会丢。当前标签页已持久化到 localStorage，
  其余（弹窗、输入框）重建后回到初始状态。
- **OSD 首次弹出有约 150~300ms 延迟**（仅当上次使用距今超过 `osd_idle_unload_secs`）。
  依赖 OSD 逐帧手感（音量/亮度连续调节）的场景建议把该值调大，或设 0 常驻。
- **有界面时内存不变**：面板展开仍是 ~310MB，这是 Chromium 的固定成本，不接受就只能不用 WebView2。

---

## 6. 如何复测

工具在 `workbuddy\tools\revo-memprobe\`（不随仓库分发）：

```powershell
# 1) 以调试端口启动（正式包需要以管理员身份运行）
$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=9222"
.\open-revo.exe

# 2) 进程树内存（按 private bytes 排序，含 webview 子进程类型）
.\memtree.ps1 -RootPid <open-revo 的 PID>

# 3) WebView 内部计数：DOM 节点 / JS 堆 / 布局对象，并做一次强制 GC 复测
node cdp-heap.js 9222

# 4) 在页面里执行一段 JS（例如模拟点关闭：hide + report_window_visibility(false)）
node cdp-eval.js 9222 "<expression>"
```

测量口径提醒：任务管理器按应用分组会把多进程 working set 相加，其中 GPU 进程的
working set 大量是共享/可回收页；判断常驻成本请用 private bytes / commit。

---

## 7. 发版必读：别用裸 `cargo build --release`

Tauri 的前端内嵌依赖 `tauri/custom-protocol` 这个 feature，**只有官方 CLI 会自动带上它**。
直接 `cargo build --release` 出来的二进制虽然也是 release 优化版，但前端仍是开发模式：
运行时会去连 `tauri.conf.json` 的 `devUrl`（本项目 `http://localhost:1420`），没起前端服务
就是一片 `ERR_CONNECTION_REFUSED`——而且二进制里能 `strings` 到 `localhost:1420`、
找不到 `assets/index-*.js`。

正确做法：

```bash
pnpm tauri build --no-bundle      # 等价于项目脚本 pnpm tauri:build
```

校验发布包是否合格（两条都要满足）：

```bash
strings -a open-revo.exe | grep -c "localhost:1420"                    # 期望仅 1（conf 里的 devUrl 字符串）
strings -a open-revo.exe | grep -E "assets/index-[A-Za-z0-9_-]+\.js"    # 期望能命中内嵌资产名
```

需要"能打断点又不依赖本地服务"的调试包：`pnpm tauri build --debug --no-bundle`。

