# ESL / API 指南（中文）

[English](../en/ESL_GUIDE.md) | [简体中文](ESL_GUIDE.md)

`mod_s3` 的命令、事件、key 模板与集成说明。

## 命令面

```
s3_upload <path> [bucket] [key|文件名] [format] [delete] [metadata]
uuid_s3 <uuid> upload <path> [format] [bucket] [key|文件名] [delete] [metadata]
uuid_s3 <uuid> jobs [status] | cancel <job_id>
s3_jobs [status]
s3_cancel <job_id>
s3_presign <get|put> <key> [bucket] [expires_secs]
```

响应约定：受理 `+OK job=<id>`，校验失败 `-ERR <reason>`，
参数不足 `-USAGE`。上传本身永远是异步的——API 线程只做校验
和入队。

### `s3_upload`

全局、无会话上传。`path` 可为绝对路径或相对 `recordings-root`。
其余参数均可选：

| 参数 | 含义 |
| --- | --- |
| `bucket` | 覆盖本任务的配置 `bucket` |
| `key` | object key **或普通文件名**——见下文；覆盖模板渲染 |
| `format` | `mp3`/`opus`/`ogg`/`wav` 转码目标；`orig`/`none`/`copy` = 不转码 |
| `delete` | `1/true/yes/on/delete` 或 `0/false/no/off/keep`；覆盖 `delete-after-upload` |
| `metadata` | JSON 片段，透传进 `mod_s3::upload` 事件 |

### `uuid_s3`

会话感知变体。入队前快照 channel variables（`uuid`、
`caller_id_number`、`destination_number`、`call_direction`、
`sip_call_id`、`record_file`、`call_uuid`、`uuid_direction`、
`Hunt-Destination-Number`）注入模板变量。会话已消失时快照为空、
任务照常执行——用事件里的 `uuid` 字段关联。

```
uuid_s3 <uuid> jobs [status]     # 与 s3_jobs 相同
uuid_s3 <uuid> cancel <job_id>   # 与 s3_cancel 相同
```

### 参数引号

遵循 FreeSWITCH `switch_separate_string` 语义：**单引号**包裹
含空格参数；反斜杠转义（`\n \r \t \s \\ \'`）生效。ESL 示例：

```
s3_upload '/var/lib/freeswitch/recordings/my call.wav' my-bucket '' mp3 1 '{"recordingId":"123"}'
```

### `s3_presign`

本地生成预签名 URL（SigV4 签名，无网络 IO）：

```
s3_presign get callcenter/recordings/2026/11/26/abc.mp3   → +OK url=https://…
s3_presign put staging/incoming.wav my-bucket 600         → +OK url=…
```

`expires_secs` 默认 3600，钳制在 S3 上限 7 天。`get` 用于
回放/下载链接；`put` 让外部客户端直传 S3（该路径绕过
pathguard/sha256/HEAD 校验，仅限可信场景）。

## Object key 模板

未显式给 `key` 时按 `key-template` 渲染 `{var}` 占位符。未知变量
渲染为空；结果收敛到 `[A-Za-z0-9._/-]`（其余字符 → `-`，连续 `/`
折叠，去首尾 `/` 与 `.`）。

### 文件名简写

`key` 参数对普通文件名有智能识别：不含 `/`、`\`、`{` 时按**文件名
词干**处理——目录结构仍由 `key-template` 提供，扩展名替换为上传
目标格式，并覆盖 `name`/`file`/`file_stem`/`file_ext` 模板变量。

```
uuid_s3 <uuid> upload '/rec/abc.wav' mp3 '' 'my-recording'
# → key-template 渲染为 callcenter/recordings/2026/11/26/my-recording.mp3
```

含 `/` 或 `{` 的值始终按完整 object key 处理（sanitize，不做模板
渲染）。如需在自定义布局中嵌入文件名，用 `key-template` 里的
`{name}` 变量。

### 内置变量

| 变量 | 来源 |
| --- | --- |
| `{prefix}` | 配置 `key-prefix` |
| `{yyyy}` `{MM}` `{dd}` `{HH}` `{mm}` `{ss}` | 渲染时刻 UTC 时间 |
| `{file}` | 源文件名（含扩展名） |
| `{file_stem}` | 不含扩展名的文件名 |
| `{file_ext}` | 源扩展名，带点（`.wav`） |
| `{dst_ext}` | 转码后扩展名，不带点（`mp3`） |
| `{name}` | `key` 参数为普通文件名时的文件名词干 |
| `{uuid}` `{caller_id_number}` `{destination_number}` `{call_direction}` `{sip_call_id}` … | channel variables（uuid_s3 / RECORD_STOP） |

默认模板 `{prefix}/{yyyy}/{MM}/{dd}/{file}`；常见布局如：

```xml
<param name="key-template" value="{prefix}/{yyyy}/{MM}/{dd}/{uuid}-{file_stem}.{dst_ext}"/>
```

## 事件

四个子类在加载时预留；ESL 用 `event plain CUSTOM` 订阅并按
`Event-Name` 过滤。

### `mod_s3::queued`

```json
{"job_id": 7, "status": "queued"}
```

入队后立即触发——可用于把 API 调用与后续完成事件关联。

### `mod_s3::progress`

```json
{"job_id": 7, "key": "callcenter/recordings/…/abc.mp3",
 "uploaded_bytes": 33554432, "total_bytes": 104857600}
```

上传过程中触发：multipart 每批分片完成一次（4 × 32 MiB 分片
并行），单次 PUT 在开始/结束各一次。为前端提供字节级进度；
频率受分片数天然限制。

### `mod_s3::record_start`

```json
{"path": "/var/lib/freeswitch/recordings/abc.wav", "uuid": "abc-123-…"}
```

`SWITCH_EVENT_RECORD_START` 触发（`auto-upload` 绑定时）。纯通知
用途——与 `mod_s3::upload` 配对可在后端关联"录音开始→上传完成"。

### `mod_s3::upload`

```json
{
  "job_id": 7,
  "status": "done",
  "path": "/var/lib/freeswitch/recordings/abc.wav",
  "key": "callcenter/recordings/2026/11/26/abc.wav",
  "bucket": "callcenter-recordings",
  "size_bytes": 1048576,
  "sha256": "9f86d0…",
  "etag": "\"d41d8…\"",
  "src_format": "wav",
  "dst_format": "mp3",
  "deleted": false,
  "transcode_ms": 820,
  "upload_ms": 431,
  "trigger": "record_stop",
  "uuid": "abc-123-…",
  "metadata": {"recordingId": "123"}
}
```

| 字段 | 说明 |
| --- | --- |
| `status` | `done` \| `failed` |
| `key`/`bucket`/`sha256`/`etag` | 早期失败（guard/队列）时为 `null` |
| `src_format`/`dst_format` | 不带点扩展名；仅转码时两者不同 |
| `deleted` | 上传成功后**源文件**是否被删除 |
| `trigger` | `api` \| `api_session` \| `record_stop` |
| `error` | `failed` 时存在 |
| `metadata` | 调用方 JSON（对象/数组原文内嵌；其他类型降级为字符串） |
| `uuid` | 任务关联会话时存在 |

RECORD_STOP 路径的入队级失败同样发 `mod_s3::upload`
（`status:"failed"`、`trigger:"record_stop"`），订阅方能感知
每一次尝试。

## 时序与顺序规则

- `s3_upload` 在任何网络 IO **之前**返回 `+OK`；结果靠轮询
  `s3_jobs` 或等待 `mod_s3::upload`。
- `s3_cancel` 只对 `status=queued` 生效；运行中任务会跑完
  （结果仍发事件）。
- 任务在单消费线程上严格串行；`workers` 只控制上传 HTTP 调用
  内部的 tokio 并行度，不是任务并发度。
- `min-age-ms` 可能拒绝刚写完的文件——稍后重试即可。

## Java/后端接入示意

旧链路：FS 写 WAV → 后端 `RecordingService` 监听 `RECORD_STOP`
→ HTTP → `recording-media` sidecar → S3。

mod_s3 之后 sidecar 一跳消失：

1. 保持 `auto-upload=true`；ESL 订阅 `mod_s3::upload`。
2. `status=done` 时用 `key`/`sha256`/`metadata.recordingId`
   关联回填录音已上传状态。
3. 手动场景直接走现有 ESL 通道调
   `uuid_s3 <uuid> upload <path> …`，替代 sidecar HTTP 端点。
4. 过渡期用配置开关（`mod_s3` vs `sidecar`）灰度，sidecar
   profile 保留到能力对齐验证完成。
