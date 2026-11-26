# 部署指南（中文）

[English](../en/DEPLOYMENT.md) | [简体中文](DEPLOYMENT.md)

在真实 FreeSWITCH 主机上构建、安装、配置、验证与排查 `mod_s3`。

## 1. 构建

```bash
cd mod_s3
cargo build --release -p mod-s3   # → target/release/libmod_s3.so
cargo test --workspace            # 28 项单元测试
```

工具链说明：

- Rust stable；首次构建需要 `libclang`（fswtch-sys bindgen）。
- 首次编译会拉起 aws-sdk-s3 整棵依赖树（数分钟）；之后增量构建很快。
- macOS 说明：`build.rs` 会加 `-Wl,-undefined,dynamic_lookup`，让
  cdylib 在没有 FreeSWITCH 符号的情况下链接；Linux 无需此参数。

## 2. 安装

```bash
install -m 0755 target/release/libmod_s3.so ${FS_PREFIX}/mod/mod_s3.so
```

FreeSWITCH 按 `<name>.so` 加载模块——文件必须叫 `mod_s3.so`
（Rust crate 产物是 `libmod_s3.so`，安装时按上面重命名）。

在 `conf/autoload_configs/modules.conf.xml` 中增加：

```xml
<load module="mod_s3"/>
```

创建 `conf/autoload_configs/mod_s3.conf.xml`（完整参考见下）。

## 3. `mod_s3.conf.xml` 参数参考

```xml
<configuration name="mod_s3.conf">
  <settings>
    <!-- ===== S3 端点 ===== -->
    <param name="endpoint"          value="http://rustfs:9000"/>
    <param name="region"            value="us-east-1"/>
    <param name="force-path-style"  value="true"/>
    <param name="bucket"            value="callcenter-recordings"/>
    <param name="access-key"        value=""/>
    <param name="secret-key"        value=""/>
    <param name="session-token"     value=""/>
    <param name="multipart-threshold" value="256"/>   <!-- MiB；0=始终单次 PUT -->

    <!-- ===== 触发与路径 ===== -->
    <param name="auto-upload"       value="true"/>
    <param name="recordings-root"   value="/var/lib/freeswitch/recordings"/>
    <param name="key-prefix"        value="callcenter/recordings"/>
    <param name="key-template"      value="{prefix}/{yyyy}/{MM}/{dd}/{uuid}-{file}"/>

    <!-- ===== 安全边界 ===== -->
    <param name="allowed-roots"     value="/var/lib/freeswitch/recordings"/>
    <param name="allowed-ext"       value=".wav,.mp3,.ogg,.opus"/>
    <param name="max-bytes"         value="104857600"/>
    <param name="min-age-ms"        value="2000"/>

    <!-- ===== 转码 ===== -->
    <param name="transcode"         value="none"/>   <!-- none|auto|ffmpeg -->
    <param name="target-format"     value="mp3"/>
    <param name="ffmpeg-bin"        value="/usr/local/bin/ffmpeg"/>
    <param name="ffmpeg-args"       value=""/>       <!-- 空 = 按格式默认档位 -->
    <param name="transcode-timeout" value="120"/>
    <param name="temp-dir"          value="/tmp/mod_s3"/>

    <!-- ===== 删除策略 ===== -->
    <param name="delete-after-upload" value="false"/>
    <param name="delete-on-failure"   value="false"/>

    <!-- ===== 运行参数 ===== -->
    <param name="workers"           value="2"/>
    <param name="queue-size"        value="64"/>
    <param name="job-retries"       value="3"/>
  </settings>
</configuration>
```

### 参数说明

| 参数 | 默认 | 说明 |
| --- | --- | --- |
| `endpoint` | 无（AWS 公有云） | MinIO/RustFS 必须显式配置 |
| `force-path-style` | `true` | MinIO/RustFS 必需；AWS 虚拟主机风格设 `false` |
| `multipart-threshold` | 256（MiB） | 超过该大小走 multipart（32 MiB/片，4 路并行）；`0` = 始终单次 PUT |
| `key-template` | `{prefix}/{yyyy}/{MM}/{dd}/{file}` | `{var}` 占位符，见 ESL_GUIDE |
| `allowed-roots` | 空 | 逗号/空格分隔；**必须**覆盖 `recordings-root` |
| `allowed-ext` | `.wav .mp3 .ogg .opus` | 小写、带点 |
| `min-age-ms` | 2000 | 跳过 mtime 过新的文件 |
| `transcode` | `none` | `auto` 仅在源≠目标时转码；`ffmpeg` 强制 |
| `ffmpeg-args` | 空 | 空白拆分；须含 `-f <fmt>`；非空则整体覆盖默认档位 |
| `delete-after-upload` | `false` | 单任务 `delete` 参数可覆盖 |
| `workers` | 2 | 模块内 tokio runtime 的 worker 线程数 |
| `queue-size` | 64 | 有界队列；满 → `-ERR queue full` |
| `job-retries` | 3 | 不含首次，封顶 10 |

> **凭证**：环境变量优先于 conf 值——`MOD_S3_ACCESS_KEY_ID`、
> `MOD_S3_SECRET_ACCESS_KEY`、`MOD_S3_ENDPOINT`、`MOD_S3_BUCKET`、
> `MOD_S3_REGION`。生产建议走环境变量，避免密钥进镜像。

## 4. ffmpeg 引入（可选）

FreeSWITCH **不自带** ffmpeg。三种方式：

1. **推荐——挂载静态二进制**（镜像零增量）：

   ```yaml
   # docker-compose.yml，freeswitch 服务
   volumes:
     - ./docker/callcenter/bin/ffmpeg:/usr/local/bin/ffmpeg:ro
   ```

   在 `docker/callcenter/bin/ffmpeg` 放置与 `CALLCENTER_PLATFORM`
   匹配架构（amd64/arm64）的静态构建。`ffmpeg-bin` 默认值
   `/usr/local/bin/ffmpeg` 正好命中。

2. **runtime apt 安装**（+80–120 MB）：

   ```dockerfile
   RUN apt-get update && apt-get install -y --no-install-recommends ffmpeg \
       && rm -rf /var/lib/apt/lists/*
   ```

3. **完全不装**——保持 `transcode=none`。

在目标二进制上确认编码器可用：

```bash
ffmpeg -hide_banner -encoders | grep -E 'mp3|opus|vorbis'
```

默认参数档位：mp3 `-ac 1 -ar 16000 -b:a 32k -f mp3`；
ogg/opus `-ac 1 -ar 16000 -b:a 16k -f ogg -c:a libopus`；
wav `-ac 1 -ar 16000 -f wav`。

## 5. 验证部署

```bash
./scripts/verify-deployment.sh
# 或手动：
fs_cli -x "module_exists mod_s3"     # → true
fs_cli -x "api s3_upload"            # → -USAGE ...
fs_cli -x "api s3_jobs"              # → no jobs（或任务列表）
```

脚本支持 `FS_CLI` 与 `FS_PREFIX` 环境变量覆盖。

### 端到端实测

1. 在 allowed root 下放一个 WAV：

   ```bash
   fs_cli -x "api s3_upload /var/lib/freeswitch/recordings/test.wav"
   # → +OK job=1
   ```

2. 查看任务：

   ```bash
   fs_cli -x "api s3_jobs"
   # → job=1 status=done path=... key=callcenter/recordings/... size=...
   ```

3. 确认对象存在、key 正确、`sha256` metadata 与本地 `sha256sum`
   一致；ESL 订阅 `mod_s3::upload` 确认完成事件。

4. 自动上传：走一通真实录音电话，确认 `mod_s3::upload` 带
   `"trigger":"record_stop"` 触发。

## 6. 故障排查

| 现象 | 可能原因 | 处理 |
| --- | --- | --- |
| `module_exists mod_s3` → false | `.so` 不在 `mod/` 或缺 load 行 | 检查文件名 `mod_s3.so` 与 `modules.conf.xml` |
| `-ERR path outside allowed roots` | 路径不在 `allowed-roots` 下或 root 不存在 | 对齐 `allowed-roots` 与 `recordings-root` |
| `-ERR file mtime too recent` | `min-age-ms` 对刚写完的文件过大 | 调低 `min-age-ms` 或稍后重试 |
| 事件里 `s3 error: ...` | endpoint/凭证/bucket 错误 | 检查 `MOD_S3_*` 环境变量、`endpoint`、`force-path-style` |
| transcode failed: spawn | `ffmpeg-bin` 处无 ffmpeg | 引入 ffmpeg 或设 `transcode=none` |
| RECORD_STOP 不入队 | 该 FS 版本事件头字段名不同 | 看日志 "RECORD_STOP without file path header"；兜底用 `s3_upload` |
| queue full | 上传慢于录音产生速度 | 提高 `queue-size`/`workers` 或检查 S3 延迟 |

日志定位：`freeswitch.log` 中 `mod_s3:` 前缀覆盖配置加载、事件绑定、
入队决策、转码耗时、删除与 worker 生命周期。

## 7. 部署清单

- [ ] `libmod_s3.so` 已安装为 `${FS_PREFIX}/mod/mod_s3.so`
- [ ] `modules.conf.xml` 有 `<load module="mod_s3"/>`，`module_exists` → true
- [ ] `mod_s3.conf.xml` 就位；endpoint/bucket/凭证已验证
- [ ] `allowed-roots` 覆盖所有可能的上传路径
- [ ] 仅当 `transcode != none` 时已引入 ffmpeg
- [ ] `fs_cli -x "api s3_upload <test.wav>"` → `+OK job=N` → `status=done`
- [ ] 桶内对象 key 与 `sha256` metadata 正确
- [ ] （可选）`delete-after-upload` 在测试文件上验证
- [ ] （可选）真实通话观察到 RECORD_STOP 自动上传

## 已知限制

| 限制 | 状态 | 规避方式 |
| --- | --- | --- |
| RECORD_STOP 头名 | FS 源码已确认 `Record-File-Path`（`switch_ivr_async.c`）；候选列表 + miss 时头值 dump | 目标版本实测；`s3_upload` 兜底 |
| multipart 峰值内存 | 每批 32 MiB × 4 片缓冲（约 128 MiB 瞬时） | 录音场景基本不会触发；介意可调小分片或接受瞬时占用 |
