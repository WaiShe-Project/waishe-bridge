# waishe-bridge

WaiShe 的本地常驻程序：发现外设、运行驱动、在后台监控电量、提供系统托盘与通知，并为 Web Dashboard 提供本地 API。

**当前阶段：M1 完成（2026-11-14），尚未公开发布。** 平台为 Windows 11 x64。

## 职责

- 发现已连接的 HID 设备，监听插拔与睡眠唤醒，把多个接口归并为设备。
- 加载并隔离运行驱动，只把匹配的设备句柄交给对应驱动。
- 仲裁硬件访问：同一端点上的轮询、事件、写入串行执行。
- 缓存设备状态并标注时效，过期或断连的读数标记为历史值。
- 后台电量监控、系统托盘、低电量通知、开机自启，以及不依赖浏览器的原生设置面板（需求 APP-08）。
- 提供回环地址上的本地 API，并内嵌提供 Dashboard 静态资源。
- 驱动的安装、更新、回滚、禁用；拉取并本地匹配 Registry 索引。

## 明确不做

- 不包含任何品牌的协议实现。协议全部在驱动里。
- 不向网页提供原始 HID 通道。
- 不安装内核驱动，不注册系统服务，不需要管理员权限。
- 不向任何服务器上报设备清单、序列号或报文。
- 不终止或接管厂商软件。

## 运行形态

Bridge 是登录用户会话内的普通进程。托盘图标和系统通知需要用户会话，因此不采用 Windows 服务的形式。Dashboard 关闭后它继续运行；没有设备在线时不加载驱动，也不做周期性硬件访问。

## 规划的内部模块

| 模块 | 职责 |
| --- | --- |
| HID 后端 | 平台相关的枚举、热插拔通知、报告收发 |
| 设备管理器 | 接口归并、设备身份、驱动匹配 |
| 驱动宿主 | 沙箱、宿主函数、限额、故障计数与自动禁用 |
| 端点队列 | 每个端点的操作串行化 |
| 调度器 | 轮询间隔、观察者加速、休眠退避 |
| 状态存储 | 读数、时效判定、变更通知 |
| 本地 API | HTTP 接口、事件流、会话与来源校验、静态资源 |
| 托盘与通知 | 图标、设备状态菜单、低电量提醒 |
| 设置面板 | 原生窗口内嵌 WebView2，显示 Dashboard 的 `#/panel` 视图：设备状态、自启、低电量提醒、托盘显示策略（`waishe` 仓库决策记录 0005） |
| Registry 客户端 | 索引拉取与验签、驱动下载与校验 |
| 设置与日志 | 用户配置、滚动日志 |

平台相关代码限制在 HID 后端、托盘与通知、自启、安装包四处。

## 文档

| 文档 | 内容 | 状态 |
| --- | --- | --- |
| [本地 API](docs/local-api.md) | Dashboard 与 Bridge 之间的接口 | 0.1，已实现 |

项目级的需求、架构、路线图与决策记录在 `waishe` 仓库的 `docs/` 下。驱动接口在 `waishe-driver-sdk` 仓库。

## 构建与运行

需要 Rust（MSVC 工具链）。

```powershell
cargo build                      # 调试版；不含 Dashboard 时内嵌一个占位页面
.\target\debug\waishe-bridge.exe # 启动并打开 Dashboard
```

命令行参数：

| 参数 | 作用 |
| --- | --- |
| `--background` | 只在托盘启动（开机自启使用） |
| `--install-driver <文件.wsdrv>` | 安装本地驱动包后退出 |

环境变量：

| 变量 | 作用 |
| --- | --- |
| `WAISHE_BRIDGE_HOME` | 把设置、驱动、日志都放到该目录下（开发与测试） |
| `WAISHE_LOG` | 日志过滤，例如 `info,waishe_bridge=debug` |
| `WAISHE_DEV_SIGNIN` | 仅调试版：把一次性登录地址写入该文件，供脚本测试本地 API |

发布安装包：同一父目录下需有 `waishe` 仓库，并安装 Node.js 与 Inno Setup 7，然后运行

```powershell
.\scripts\build-release.ps1
```

产物为 `target\installer\WaiShe-Setup-<版本>-x64.exe` 及其 SHA-256。安装包按当前用户安装，不需要管理员权限，不包含任何驱动。

用户数据位置：设置在 `%APPDATA%\WaiShe\Bridge\config`，驱动、日志与设置面板的 WebView2 数据在 `%LOCALAPPDATA%\WaiShe\Bridge\data`。日志按天滚动，保留 7 天，不记录 HID 报文与序列号。

## 技术选型

Rust；驱动由 wasmi 运行（`waishe` 仓库决策记录 0003）；本地 API 用 axum；托盘用 tray-icon 与 winit；设置面板用 wry（WebView2，决策记录 0005）；HID 用 hidapi 的 Windows 原生后端，设备归并与插拔通知直接调用 Windows 配置管理器接口。

## 许可证

[MIT](LICENSE-MIT) 或 [Apache-2.0](LICENSE-APACHE) 双许可，使用者任选其一。

本仓库不得包含移植自 AGPL 或 GPL 项目的代码，见 `waishe` 仓库决策记录 0002。

## 声明

WaiShe 是独立的社区项目，与任何外设厂商没有关联。
