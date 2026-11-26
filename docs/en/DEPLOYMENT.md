# Deployment Guide (EN)

[English](DEPLOYMENT.md) | [简体中文](../zh/DEPLOYMENT.md)

Build, install, configure, verify, and troubleshoot `mod_s3` on a live
FreeSWITCH host.

## 1. Build

```bash
cd mod_s3
cargo build --release -p mod-s3   # → target/release/libmod_s3.so
cargo test --workspace            # 28 unit tests
```

Toolchain notes:

- Rust stable; `libclang` required once for fswtch-sys bindgen.
- First build compiles the aws-sdk-s3 tree (several minutes); subsequent
  builds are incremental.
- macOS note: `build.rs` adds `-Wl,-undefined,dynamic_lookup` so the
  cdylib links without FreeSWITCH symbols; Linux needs no such flag.

## 2. Install

```bash
install -m 0755 target/release/libmod_s3.so ${FS_PREFIX}/mod/mod_s3.so
```

FreeSWITCH loads modules as `<name>.so` — the file must be named
`mod_s3.so` (the Rust crate produces `libmod_s3.so`; rename on install
as above).

Add to `conf/autoload_configs/modules.conf.xml`:

```xml
<load module="mod_s3"/>
```

Create `conf/autoload_configs/mod_s3.conf.xml` (full reference below).

## 3. `mod_s3.conf.xml` reference

```xml
<configuration name="mod_s3.conf">
  <settings>
    <!-- ===== S3 endpoint ===== -->
    <param name="endpoint"          value="http://rustfs:9000"/>
    <param name="region"            value="us-east-1"/>
    <param name="force-path-style"  value="true"/>
    <param name="bucket"            value="callcenter-recordings"/>
    <param name="access-key"        value=""/>
    <param name="secret-key"        value=""/>
    <param name="session-token"     value=""/>
    <param name="multipart-threshold" value="256"/>   <!-- MiB；0=始终单次 PUT -->

    <!-- ===== triggers & paths ===== -->
    <param name="auto-upload"       value="true"/>
    <param name="recordings-root"   value="/var/lib/freeswitch/recordings"/>
    <param name="key-prefix"        value="callcenter/recordings"/>
    <param name="key-template"      value="{prefix}/{yyyy}/{MM}/{dd}/{uuid}-{file}"/>

    <!-- ===== security limits ===== -->
    <param name="allowed-roots"     value="/var/lib/freeswitch/recordings"/>
    <param name="allowed-ext"       value=".wav,.mp3,.ogg,.opus"/>
    <param name="max-bytes"         value="104857600"/>
    <param name="min-age-ms"        value="2000"/>

    <!-- ===== transcode ===== -->
    <param name="transcode"         value="none"/>   <!-- none|auto|ffmpeg -->
    <param name="target-format"     value="mp3"/>
    <param name="ffmpeg-bin"        value="/usr/local/bin/ffmpeg"/>
    <param name="ffmpeg-args"       value=""/>       <!-- empty = per-format default -->
    <param name="transcode-timeout" value="120"/>
    <param name="temp-dir"          value="/tmp/mod_s3"/>

    <!-- ===== delete policy ===== -->
    <param name="delete-after-upload" value="false"/>
    <param name="delete-on-failure"   value="false"/>

    <!-- ===== runtime ===== -->
    <param name="workers"           value="2"/>
    <param name="queue-size"        value="64"/>
    <param name="job-retries"       value="3"/>
  </settings>
</configuration>
```

### Parameter notes

| param | default | notes |
| --- | --- | --- |
| `endpoint` | none (AWS public cloud) | MinIO/RustFS require explicit endpoint |
| `force-path-style` | `true` | required by MinIO/RustFS; set `false` for AWS virtual-hosted style |
| `multipart-threshold` | 256 (MiB) | files larger than this upload via multipart (32 MiB parts, 4-way parallel); `0` = always single PUT |
| `key-template` | `{prefix}/{yyyy}/{MM}/{dd}/{file}` | `{var}` placeholders; see ESL_GUIDE |
| `allowed-roots` | empty (= deny all? see below) | comma/space separated; **must** include `recordings-root` |
| `allowed-ext` | `.wav .mp3 .ogg .opus` | lowercase, dot-prefixed |
| `min-age-ms` | 2000 | skip files whose mtime is younger |
| `transcode` | `none` | `auto` transcodes only when src≠target; `ffmpeg` always |
| `ffmpeg-args` | empty | whitespace-split; must include `-f <fmt>`; overrides defaults |
| `delete-after-upload` | `false` | per-job `delete` arg overrides |
| `workers` | 2 | tokio worker threads inside the module runtime |
| `queue-size` | 64 | bounded queue; full → `-ERR queue full` |
| `job-retries` | 3 | retries exclude the first attempt; capped at 10 |

> **Credentials**: environment variables win over conf values —
> `MOD_S3_ACCESS_KEY_ID`, `MOD_S3_SECRET_ACCESS_KEY`, `MOD_S3_ENDPOINT`,
> `MOD_S3_BUCKET`, `MOD_S3_REGION`. Prefer env vars in production so no
> secret lands in the image.

## 4. Provisioning ffmpeg (optional)

FreeSWITCH does **not** ship ffmpeg. Three options:

1. **Recommended — mount a static binary** (zero image growth):

   ```yaml
   # docker-compose.yml, freeswitch service
   volumes:
     - ./docker/callcenter/bin/ffmpeg:/usr/local/bin/ffmpeg:ro
   ```

   Place an architecture-matched static build (amd64/arm64) at
   `docker/callcenter/bin/ffmpeg`. `ffmpeg-bin` already defaults to
   `/usr/local/bin/ffmpeg`.

2. **apt install** in the runtime image (`+80–120 MB`):

   ```dockerfile
   RUN apt-get update && apt-get install -y --no-install-recommends ffmpeg \
       && rm -rf /var/lib/apt/lists/*
   ```

3. **Skip entirely** — keep `transcode=none`.

Verify encoder availability on the target binary:

```bash
ffmpeg -hide_banner -encoders | grep -E 'mp3|opus|vorbis'
```

Default arg presets: mp3 `-ac 1 -ar 16000 -b:a 32k -f mp3`;
ogg/opus `-ac 1 -ar 16000 -b:a 16k -f ogg -c:a libopus`;
wav `-ac 1 -ar 16000 -f wav`.

## 5. Verify deployment

```bash
./scripts/verify-deployment.sh
# or manually:
fs_cli -x "module_exists mod_s3"     # → true
fs_cli -x "api s3_upload"            # → -USAGE ...
fs_cli -x "api s3_jobs"              # → no jobs (or job list)
```

Script honors `FS_CLI` and `FS_PREFIX` env overrides.

### Live end-to-end check

1. Place a WAV under the allowed root:

   ```bash
   fs_cli -x "api s3_upload /var/lib/freeswitch/recordings/test.wav"
   # → +OK job=1
   ```

2. Watch the job:

   ```bash
   fs_cli -x "api s3_jobs"
   # → job=1 status=done path=... key=callcenter/recordings/... size=...
   ```

3. Confirm the object exists with `sha256` metadata equal to the local
   file's `sha256sum`, and subscribe to `mod_s3::upload` over ESL for the
   completion event.

4. For automatic uploads, place a real call through the recording path
   and confirm `mod_s3::upload` fires with `"trigger":"record_stop"`.

## 6. Troubleshooting

| symptom | likely cause | fix |
| --- | --- | --- |
| `module_exists mod_s3` → false | `.so` not in `mod/` or load line missing | check filename `mod_s3.so`, `modules.conf.xml` |
| `-ERR path outside allowed roots` | path not under `allowed-roots`, or root missing | align `allowed-roots` with `recordings-root` |
| `-ERR file mtime too recent` | `min-age-ms` too high for just-finished file | lower `min-age-ms` or retry after a moment |
| `s3 error: ...` in `mod_s3::upload` | endpoint/credentials/bucket wrong | check `MOD_S3_*` env, `endpoint`, `force-path-style` |
| transcode failed: spawn | ffmpeg missing at `ffmpeg-bin` | provision ffmpeg or set `transcode=none` |
| RECORD_STOP never enqueues | header field names differ on this FS version | check `fs_cli` log for "RECORD_STOP without file path header"; use `s3_upload` fallback |
| queue full errors | uploads slower than recording rate | raise `queue-size`/`workers`, or check S3 latency |

FS log lines to grep: `mod_s3:` prefix in `freeswitch.log` covers config
load, event bind, enqueue decisions, transcode stats, deletes, and
worker lifecycle.

## 7. Deployment checklist

- [ ] `libmod_s3.so` installed as `mod_s3.so` under `${FS_PREFIX}/mod/`
- [ ] `<load module="mod_s3"/>` present and `module_exists` → true
- [ ] `mod_s3.conf.xml` in place; endpoint/bucket/credentials verified
- [ ] `allowed-roots` covers every path callers may upload
- [ ] ffmpeg provisioned **iff** `transcode != none`
- [ ] `fs_cli -x "api s3_upload <test.wav>"` → `+OK job=N` → `status=done`
- [ ] Object visible in bucket with correct key + `sha256` metadata
- [ ] (optional) `delete-after-upload` validated on a scratch file
- [ ] (optional) RECORD_STOP auto-upload observed on a real call

## Known limitations

| limitation | status | workaround |
| --- | --- | --- |
| RECORD_STOP header names | `Record-File-Path` confirmed in FS source (`switch_ivr_async.c`); candidate list + header-value dump on miss | verify on target; `s3_upload` fallback |
| Multipart peak memory | 32 MiB × 4 parts buffered per batch (~128 MiB transient) | lower `multipart-threshold` scope or accept the transient; recordings rarely hit it |
