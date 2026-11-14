# 本地 API

| 项 | 内容 |
| --- | --- |
| 版本 | 0.1（M1 实现的契约） |
| 日期 | 2026-11-14 |
| 相关 | `waishe` 仓库决策记录 0004（托管方式与鉴权）；`waishe-driver-sdk` 仓库 `docs/capabilities.md`（能力模型） |

Dashboard 与 Bridge 之间的接口。Dashboard 由 Bridge 同源提供，只通过本接口与 Bridge 交互。

## 1. 基础

| 项 | 值 |
| --- | --- |
| 地址 | `http://127.0.0.1:27463`，被占用时顺延至 27472 |
| 静态资源 | `/`，Dashboard 构建产物 |
| API 前缀 | `/api/v1` |
| 数据格式 | JSON，UTF-8，字段名小驼峰 |
| 时间 | ISO 8601，UTC，例如 `2026-11-14T08:30:12Z` |

## 2. 鉴权

1. 托盘的“打开 Dashboard”生成一次性授权码，用默认浏览器打开 `/auth?code=…`。
2. 校验通过后下发 `ws_session` Cookie（`HttpOnly; SameSite=Strict; Path=/`），303 跳转到 `/`。
3. 此后每个请求都校验：Host 头必须是 `127.0.0.1:<端口>` 或 `localhost:<端口>`；带 Origin 头时必须是这两者之一；`/api/` 下的请求必须带有效会话。

失败时返回 `401 unauthorized` 或 `403 forbidden_host` / `forbidden_origin`，响应体不含设备信息。Dashboard 收到 401 时显示“请从系统托盘打开”。

## 3. 数据类型

### 3.1 值

| 能力值类型 `type` | JSON |
| --- | --- |
| `percent` | 0–100 的整数 |
| `bool` | `true` / `false` |
| `integer` | 整数 |
| `enum` | 字符串，取值见 `variants` |
| `text` | 字符串 |
| `dpi` | `{ "x": 1600, "y": 1600 }` |

### 3.2 读数 `Reading`

```json
{ "state": "ok", "value": 76, "observedAt": "2026-11-14T08:30:12Z", "stale": false }
{ "state": "unavailable", "reason": "disconnected", "observedAt": "…", "lastOk": { "value": 76, "observedAt": "…" } }
{ "state": "unknown" }
```

- `reason`：`asleep`、`disconnected`、`busy`、`timeout`、`io`、`protocol`。
- `stale`：为真表示历史值（读取时间超过时效，或设备已断开），界面必须与实时值区分。
- `lastOk`：最近一次成功的值，可能为 `null`。
- 设备没有的能力不出现在列表里，不用 `unsupported` 状态表达。

### 3.3 能力 `Capability`

```json
{
  "id": "mouse.dpi",
  "access": ["read", "write"],
  "type": "dpi",
  "unit": null,
  "variants": null,
  "constraint": { "kind": "dpi", "min": 100, "max": 30000, "step": 50, "independentXY": true },
  "persistence": "onboard",
  "precision": null,
  "verification": "verified",
  "reading": { "state": "ok", "value": { "x": 800, "y": 800 }, "observedAt": "…", "stale": false }
}
```

| 字段 | 说明 |
| --- | --- |
| `access` | `read`、`write`、`notify` 的子集 |
| `unit` | 仅 `integer`，例如 `"Hz"` |
| `variants` | 仅 `enum` |
| `constraint` | 仅可写能力。`{ "kind": "range", "min", "max", "step" }`、`{ "kind": "choices", "values": [125, 500, 1000] }` 或 `{ "kind": "dpi", "min", "max", "step", "independentXY" }` |
| `persistence` | 仅可写能力：`onboard`、`volatile`、`unknown` |
| `precision` | 仅 `battery.level`：`{ "kind": "exact" }` 或 `{ "kind": "steps", "count": 5 }` |
| `verification` | 当前连接方式下的验证等级：`verified`、`simulated`、`inferred`、`broken`，或 `null`（设备表未记录） |

### 3.4 设备 `Device`

```json
{
  "id": "dev_3f9c2a71b0",
  "name": "Razer DeathAdder V3 Pro",
  "alias": null,
  "hidden": false,
  "brand": "Razer",
  "model": "deathadder-v3-pro",
  "kind": "mouse",
  "link": "connected",
  "driver": { "id": "razer", "version": "0.1.0", "trust": "local" },
  "endpoints": [
    { "transport": "receiver", "vendorId": 5426, "productId": 183, "link": "connected", "active": true }
  ],
  "capabilities": [ /* Capability */ ]
}
```

- `link`：设备整体的连接状态，`connected`、`asleep`、`disconnected`、`unknown`。所有端点都已拔出时为 `disconnected`，设备仍保留在列表中，读数标记为历史值。
- 设备 ID 由 Bridge 在本地生成，不含序列号。

### 3.5 驱动 `Driver`

```json
{
  "id": "razer",
  "name": "Razer",
  "version": "0.1.0",
  "api": "0.1",
  "license": "AGPL-3.0-or-later",
  "repository": "https://github.com/WaiShe-Project/driver-razer",
  "description": "…",
  "trust": "local",
  "enabled": true,
  "status": "active",
  "fault": null,
  "models": [ { "model": "deathadder-v3-pro", "name": "Razer DeathAdder V3 Pro" } ]
}
```

- `trust`：`official`、`community`、`local`。M1 只有 `local`。
- `status`：`active`（已启用，正在服务或等待设备）、`disabled`（用户禁用）、`faulted`（连续故障后自动禁用，原因见 `fault`）。

### 3.6 设置 `Settings`

```json
{
  "language": "zh-CN",
  "autostart": true,
  "tray": { "mode": "lowest", "deviceId": null },
  "lowBattery": { "enabled": true, "threshold": 20, "intervalMinutes": 60 },
  "pollIntervalSeconds": 300,
  "developerMode": false,
  "network": { "registry": true, "updates": true }
}
```

- `language`：`zh-CN` 或 `en`。
- `tray.mode`：`lowest`（显示电量最低的设备，充电中的设备不参与）或 `device`（显示 `deviceId` 指定的设备）。

## 4. 接口

| 方法与路径 | 请求 | 成功响应 |
| --- | --- | --- |
| `GET /status` | — | `{ "version", "apiVersion": "0.1", "driverAbi": "0.1", "platform", "port", "vendorSoftware": ["Razer Synapse"] }` |
| `GET /devices` | — | `Device[]` |
| `GET /devices/{id}` | — | `Device` |
| `PATCH /devices/{id}` | `{ "alias"?: string \| null, "hidden"?: bool }` | `Device` |
| `PUT /devices/{id}/capabilities/{capabilityId}` | `{ "value": … }` | 见第 5 节 |
| `POST /devices/{id}/refresh` | — | 202，结果经事件流到达 |
| `GET /hid-devices` | — | `[{ "vendorId", "productId", "product", "manufacturer", "usagePage", "usage", "interfaceNumber", "driver": "razer" \| null }]` |
| `GET /drivers` | — | `Driver[]` |
| `POST /drivers` | 请求体为 `.wsdrv` 文件内容，`Content-Type: application/zip` | 201 `Driver`。需要开发者模式 |
| `PATCH /drivers/{id}` | `{ "enabled": bool }` | `Driver` |
| `DELETE /drivers/{id}` | — | 204 |
| `GET /settings` | — | `Settings` |
| `PATCH /settings` | `Settings` 的任意子集，嵌套对象按字段合并 | `Settings` |
| `GET /events` | — | `text/event-stream`，见第 6 节 |

## 5. 写入

```http
PUT /api/v1/devices/dev_3f9c2a71b0/capabilities/mouse.dpi
Content-Type: application/json

{ "value": { "x": 800, "y": 800 } }
```

```json
{ "outcome": "applied", "reading": { "state": "ok", "value": { "x": 800, "y": 800 }, "observedAt": "…", "stale": false }, "message": null }
```

| `outcome` | HTTP 状态 | 说明 |
| --- | --- | --- |
| `applied` | 200 | 已写入并回读确认 |
| `accepted` | 200 | 设备确认，无法回读 |
| `rejected` | 422 | 参数不满足约束，或设备拒绝；`message` 说明原因 |
| `failed` | 502 | 通信错误或超时；`reading` 为随后重新读取的结果 |

Dashboard 不做乐观更新，以响应为准。

## 6. 事件

服务器推送事件流。连接建立后 Dashboard 先取一次 `GET /devices` 作为基线。Bridge 以是否有事件流连接判断是否有观察者，有观察者时加快轮询。

| `event` | `data` |
| --- | --- |
| `device.added` | `Device` |
| `device.updated` | `Device`（端点、连接状态、别名等变化） |
| `device.removed` | `{ "id" }` |
| `capability.changed` | `{ "deviceId", "capability": Capability }` |
| `driver.changed` | `Driver`，或卸载时 `{ "id", "removed": true }` |
| `settings.changed` | `Settings` |

每 15 秒发送一次注释行保活。

## 7. 错误

```json
{ "error": { "code": "device_not_found", "message": "…" } }
```

| 代码 | HTTP 状态 |
| --- | --- |
| `unauthorized` | 401 |
| `forbidden_origin`、`forbidden_host`、`developer_mode_required` | 403 |
| `device_not_found`、`capability_not_found`、`driver_not_found` | 404 |
| `capability_not_writable` | 405 |
| `invalid_value`、`invalid_settings` | 422 |
| `driver_invalid`、`driver_incompatible` | 422 |
| `device_unavailable` | 502 |

## 8. 不提供的接口

- 发送或接收原始 HID 报告。
- 执行任意驱动函数。
- 读取设备序列号。
- 读取日志内容。

## 9. 版本

路径中的 `v1` 是主版本。`GET /status` 返回完整的 API 版本号。Dashboard 内嵌于 Bridge，二者版本始终匹配。
