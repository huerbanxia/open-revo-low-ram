# 更新日志 (Changelog)

本文档记录 OpenRevo 项目的所有重大更新、功能特性、问题修复与硬件兼容性演进。
版本格式严格遵循 [语义化版本 2.0.0 (Semantic Versioning)](https://semver.org/lang/zh-CN/)。

---

## [Unreleased] - 待发布

### ✨ 新增 (Added)
- 新增 `config.json` 三个开关：`idle_unload_secs`（面板隐藏后多少秒卸载 WebView，默认 **1**，等价"关掉即释放"；0=关闭）、
  `idle_unload_blur_secs`（失焦自动折叠的独立阈值，默认 **0** = 跟随 `idle_unload_secs`）、
  `osd_idle_unload_secs`（OSD 浮层隐藏后多少秒卸载，默认 120，0=关闭并恢复开机预热）。详见 `docs/LOW_RAM_MODE.md`。

### ⚡ 优化 (Changed)
- **低内存空闲卸载**：面板/OSD 隐藏超过阈值后直接销毁对应 WebviewWindow（不再是 hide），让 WebView2 的 browser / renderer / GPU / network 进程整套退出。实测隐藏态由 **335MB / 8 进程降至 18MB / 1 进程**（仅剩 Rust 主进程），托盘、全局热键、EC/WMI 监听、遥测心跳全部保留；再次打开面板按需重建（冷启动约 300~800ms）。
- 两条隐藏路径默认同一节奏：面板一收起约 1 秒即释放 WebView。若想给失焦自动折叠留宽限（它是高频动作），把 `idle_unload_blur_secs` 设成正数即可。
- OSD 浮层不再开机预热：默认改为首次弹出时按需重建，启动常驻内存再降约 60MB（原 335MB → 275MB）。
- 主面板标签页选择持久化到 localStorage，WebView 重建后仍回到原来那一页。

### 🐛 修复 (Fixed)
- 修复低内存模式在启动态"抢跑"的问题：主窗口是 `visible:false` 创建的，前端挂载完成才会发 `frontend_ready`
  解锁 OSD/挂件队列；阈值缩短到 1 秒后，WebView 会在握手前被销毁，导致 OSD 与桌面挂件再也不弹。
  启动态现在先等握手完成（最多 30 秒）再按阈值计时。
- 修复 `report_window_visibility` 采信前端声明的问题：托盘常驻时窗口是 `visible:false` 创建的，前端挂载后仍无条件上报 `visible=true`，导致隐藏态一直被当作前台，遥测心跳无法进入待机、低功耗策略失效。现以真实窗口状态为准。
- 修复窗口数归零时 Tauri 结束事件循环的问题（拦截 `ExitRequested`），否则销毁 WebView 会把托盘常驻的无界面后台一并带走。
- 修复托盘菜单构建时为读取窗口模式而误触发窗口重建（`ensure_window_alive` 现在会真正按需重建，读数场景改用只读查询）。

### 🛡️ 硬件与安全 (Security / Hardware)
- `build.rs` 增加 `OPENREVO_MANIFEST_LEVEL=asInvoker` 构建开关，用于产出免提权的调试包（正式包仍为 `requireAdministrator`，行为不变）。

---

<!-- 历史版本发布记录将归档于下方 -->
