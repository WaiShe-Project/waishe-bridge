# 本地 API

| 项 | 内容 |
| --- | --- |
| 版本 | 草案 0.1，非规范 |
| 日期 | 2026-11-14 |
| 相关 | `waishe` 仓库决策记录 0004（托管方式与鉴权）；`waishe-driver-sdk` 仓库 `docs/capabilities.md`（能力模型） |

Dashboard 与 Bridge 之间的接口。路径、字段名和事件名在 M1 实现时定稿，本稿用于确定接口的形状和边界。

## 1. 基础

| 项 | 值 |
| --- | --- |
| 地址 | `http://127.0.0.1:27463`（端口暂定） |
| 静态资源 | `/` |
| API 前缀 | `/api/v1` |
| 数据格式 | JSON，UTF-8 |
| 时间 | ISO 8601，UTC |

## 2. 鉴权

按决策记录 0004：

1. 用户从托盘打开 Dashboard，Bridge 生成一次性授权码并打开 `/auth?code=…`。
2. 校验通过后下发会话 Cookie，重定向到 `/`。
3. 此后每个请求都校验：Host 头、会话、Origin（请求带有该头时）。

任一校验不通过返回 `401` 或 `403`，响应体不含任何设备信息。只读接口同样需要会话。

## 3. 资源

### 状态

| 方法与路径 | 说明 |
| --- | --- |
| `GET /status` | Bridge 版本、平台、支持的驱动接口版本、Registry 索引时间、检测到的厂商软件 |

### 设备

| 方法与路径 | 说明 |
| --- | --- |
| `GET /devices` | 全部已识别设备及其能力与最近读数 |
| `GET /devices/{deviceId}` | 单个设备 |
| `PUT /devices/{deviceId}/capabilities/{capabilityId}` | 写入一项能力。请求体 `{ "value": … }` |
| `POST /devices/{deviceId}/refresh` | 立即重新读取该设备的全部可读能力 |
| `PATCH /devices/{deviceId}` | 修改别名、是否隐藏、是否显示在托盘（P1） |
| `GET /hid-devices` | 全部 HID 设备的基础识别信息，用于“显示全部 HID 设备”视图和设备支持请求 |

### 驱动

| 方法与路径 | 说明 |
| --- | --- |
| `GET /drivers` | 已安装的驱动：版本、信任层级、许可证、状态、支持的型号与能力 |
| `POST /drivers` | 安装。来源为 Registry 条目，或在开发者模式下上传本地驱动包 |
| `PATCH /drivers/{driverId}` | 启用或禁用 |
| `DELETE /drivers/{driverId}` | 卸载 |
| `POST /drivers/{driverId}/update` | 更新到指定版本或最新版本（M2） |
| `POST /drivers/{driverId}/rollback` | 回滚到上一版本（M2） |

### Registry（M2）

| 方法与路径 | 说明 |
| --- | --- |
| `GET /registry` | 本地索引的版本与时间；当前设备可用但未安装的驱动；可用更新 |
| `POST /registry/refresh` | 立即拉取索引 |

### 设置

| 方法与路径 | 说明 |
| --- | --- |
| `GET /settings` | 全部设置 |
| `PATCH /settings` | 修改：自启、托盘显示策略、通知阈值与间隔、轮询间隔、开发者模式、联网开关 |

### 事件

| 方法与路径 | 说明 |
| --- | --- |
| `GET /events` | 服务器推送的事件流。Dashboard 打开期间保持连接，Bridge 据此判断是否有观察者 |

## 4. 设备的表示

数值为示例，不代表设备的实际规格。

```json
{
  "id": "dev_3f9c2a",
  "name": "Razer DeathAdder V3 Pro",
  "brand": "Razer",
  "model": "deathadder-v3-pro",
  "kind": "mouse",
  "driver": { "id": "razer", "version": "0.1.0", "trust": "local" },
  "endpoints": [
    { "transport": "receiver", "link": "connected", "active": true },
    { "transport": "wired", "link": "disconnected", "active": false }
  ],
  "capabilities": [
    {
      "id": "battery.level",
      "access": ["read"],
      "type": "percent",
      "precision": "exact",
      "verification": "verified",
      "reading": {
        "state": "ok",
        "value": 76,
        "observedAt": "2026-11-14T08:30:12Z",
        "stale": false
      }
    },
    {
      "id": "mouse.dpi",
      "access": ["read", "write"],
      "type": "dpi",
      "constraint": { "kind": "range", "min": 100, "max": 30000, "step": 50, "independentXY": true },
      "persistence": "onboard",
      "verification": "verified",
      "reading": {
        "state": "ok",
        "value": { "x": 1600, "y": 1600 },
        "observedAt": "2026-11-14T08:30:12Z",
        "stale": false
      }
    },
    {
      "id": "battery.charging",
      "access": ["read"],
      "type": "enum",
      "verification": "inferred",
      "reading": { "state": "unavailable", "reason": "asleep", "lastOk": null }
    }
  ]
}
```

规则：

- `reading.state` 为 `ok`、`unavailable`、`unknown` 之一。设备没有的能力不出现在列表中。
- `stale` 为真表示历史值，界面必须与实时值区分。
- `unavailable` 时可附带 `lastOk`，即最近一次成功的值与时间，供界面按历史值展示。
- 设备 ID 由 Bridge 在本地生成。序列号不出现在 API 中。

## 5. 写入

请求：

```http
PUT /api/v1/devices/dev_3f9c2a/capabilities/mouse.dpi
Content-Type: application/json

{ "value": { "x": 800, "y": 800 } }
```

响应：

```json
{
  "outcome": "applied",
  "reading": {
    "state": "ok",
    "value": { "x": 800, "y": 800 },
    "observedAt": "2026-11-14T08:31:02Z",
    "stale": false
  }
}
```

| `outcome` | HTTP 状态 | 说明 |
| --- | --- | --- |
| `applied` | 200 | 已写入并回读确认 |
| `accepted` | 200 | 设备确认，无法回读。`reading` 为驱动推定的值，并标明未经回读 |
| `rejected` | 422 | 参数不满足约束，或设备拒绝。附原因 |
| `failed` | 502 | 通信错误或超时。`reading` 为随后重新读取的结果，可能是 `unavailable` |

写入会导致设备重新枚举时，请求保持到回读完成或超时。Dashboard 不做乐观更新，以响应为准。

## 6. 事件

| 事件 | 触发 |
| --- | --- |
| `device.added` / `device.removed` | 设备出现或消失 |
| `device.updated` | 端点、链路状态、别名变化 |
| `capability.changed` | 读数或其状态变化，包括变为历史值 |
| `driver.changed` | 驱动安装、卸载、启用、禁用、故障、更新、回滚 |
| `registry.changed` | 索引更新，或出现新的可用驱动与更新 |
| `settings.changed` | 设置变化 |

事件只携带变化的部分。连接建立后 Dashboard 先取一次 `GET /devices` 作为基线。

## 7. 错误

```json
{ "error": { "code": "device_not_found", "message": "…" } }
```

| 代码 | HTTP 状态 |
| --- | --- |
| `unauthorized` | 401 |
| `forbidden_origin`、`forbidden_host` | 403 |
| `device_not_found`、`capability_not_found`、`driver_not_found` | 404 |
| `capability_not_writable` | 405 |
| `invalid_value` | 422 |
| `driver_incompatible`、`driver_untrusted`、`driver_verification_failed` | 422 |
| `developer_mode_required` | 403 |
| `device_unavailable` | 502 |

## 8. 不提供的接口

- 发送或接收原始 HID 报告。
- 执行任意驱动函数。
- 读取设备序列号。
- 读取日志内容（诊断包导出为 P1，另行设计）。

## 9. 版本

路径中的 `v1` 是主版本。同一主版本内只做向后兼容的增加。`GET /status` 返回完整的 API 版本号，Dashboard 据此判断兼容性。
