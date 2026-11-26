# ESL / API Guide (EN)

[English](ESL_GUIDE.md) | [简体中文](../zh/ESL_GUIDE.md)

Commands, events, key templates, and integration notes for `mod_s3`.

## Command surface

```
s3_upload <path> [bucket] [key|filename] [format] [delete] [metadata]
uuid_s3 <uuid> upload <path> [format] [bucket] [key|filename] [delete] [metadata]
uuid_s3 <uuid> jobs [status] | cancel <job_id>
s3_jobs [status]
s3_cancel <job_id>
s3_presign <get|put> <key> [bucket] [expires_secs]
```

Response contract: `+OK job=<id>` on accept, `-ERR <reason>` on
validation failure, `-USAGE` on bad arity. Upload itself is always
asynchronous — the API thread only validates and enqueues.

### `s3_upload`

Global, session-less upload. `path` may be absolute or relative to
`recordings-root`. All trailing args optional:

| arg | meaning |
| --- | --- |
| `bucket` | overrides config `bucket` for this job |
| `key` | object key **or plain filename** — see below; overrides template |
| `format` | `mp3`/`opus`/`ogg`/`wav` transcode target; `orig`/`none`/`copy` = no transcode |
| `delete` | `1/true/yes/on/delete` or `0/false/no/off/keep`; overrides `delete-after-upload` |
| `metadata` | JSON fragment echoed into the `mod_s3::upload` event |

### `uuid_s3`

Session-aware variant. Before enqueueing it snapshots channel
variables (`uuid`, `caller_id_number`, `destination_number`,
`call_direction`, `sip_call_id`, `record_file`, `call_uuid`,
`uuid_direction`, `Hunt-Destination-Number`) into the template
variables. If the session no longer exists the snapshot is empty and
the job proceeds — check the event's `uuid` field for correlation.

```
uuid_s3 <uuid> jobs [status]     # same listing as s3_jobs
uuid_s3 <uuid> cancel <job_id>   # same as s3_cancel
```

### Argument quoting

FreeSWITCH `switch_separate_string` semantics: **single quotes** group
arguments containing spaces; backslash escapes (`\n \r \t \s \\ \'`)
are honored. Example over ESL:

```
s3_upload '/var/lib/freeswitch/recordings/my call.wav' my-bucket '' mp3 1 '{"recordingId":"123"}'
```

### `s3_presign`

Generates a presigned URL locally (SigV4, no network I/O):

```
s3_presign get callcenter/recordings/2026/11/26/abc.mp3        → +OK url=https://…
s3_presign put staging/incoming.wav my-bucket 600              → +OK url=…
```

`expires_secs` defaults to 3600 and is clamped to S3's 7-day
maximum. `get` is for playback/download links; `put` lets an external
client upload straight to S3 (the object bypasses pathguard/sha256/
verify — use only for trusted flows).

## Object key templating

When `key` is not given explicitly, `key-template` renders with `{var}`
placeholders. Unknown variables render empty; the result is sanitized
to `[A-Za-z0-9._/-]` (other chars → `-`, slashes collapsed, trailing
`/` and `.` trimmed).

### Filename shorthand

The `key` argument is smart about plain filenames: if it contains no
`/`, `\`, or `{`, it is treated as a **filename stem** — the configured
`key-template` still supplies the directory layout, the extension is
replaced by the upload's destination extension, and `name`/`file`/
`file_stem`/`file_ext` template variables are overridden accordingly.

```
uuid_s3 <uuid> upload '/rec/abc.wav' mp3 '' 'my-recording'
# → key-template renders e.g. callcenter/recordings/2026/11/26/my-recording.mp3
```

A value containing `/` or `{` is always treated as a full object key
(sanitized, no template rendering). To embed the filename in a custom
layout use the `{name}` variable inside `key-template`.

### Built-in variables

| variable | source |
| --- | --- |
| `{prefix}` | config `key-prefix` |
| `{yyyy}` `{MM}` `{dd}` `{HH}` `{mm}` `{ss}` | UTC time at render |
| `{file}` | source filename incl. extension |
| `{file_stem}` | filename without extension |
| `{file_ext}` | source extension, dot-prefixed (`.wav`) |
| `{dst_ext}` | post-transcode extension, no dot (`mp3`) |
| `{name}` | filename stem when the `key` arg is a plain filename |
| `{uuid}` `{caller_id_number}` `{destination_number}` `{call_direction}` `{sip_call_id}` … | channel variables (uuid_s3 / RECORD_STOP) |

Default template: `{prefix}/{yyyy}/{MM}/{dd}/{file}` →
`callcenter/recordings/2026/11/26/<uuid>-<file>` style layouts are easy:

```xml
<param name="key-template" value="{prefix}/{yyyy}/{MM}/{dd}/{uuid}-{file_stem}.{dst_ext}"/>
```

## Events

Four subclasses are reserved at load; subscribe via ESL `event plain
CUSTOM` and filter on `Event-Name`.

### `mod_s3::queued`

```json
{"job_id": 7, "status": "queued"}
```

Fired right after enqueue — useful to correlate an API call with the
later completion event.

### `mod_s3::progress`

```json
{"job_id": 7, "key": "callcenter/recordings/…/abc.mp3",
 "uploaded_bytes": 33554432, "total_bytes": 104857600}
```

Fired during upload: once per completed multipart batch (4 × 32 MiB
parts in flight) and at start/end for single-PUT uploads. Gives UIs a
byte-level progress signal; frequency is bounded by part count.

### `mod_s3::record_start`

```json
{"path": "/var/lib/freeswitch/recordings/abc.wav", "uuid": "abc-123-…"}
```

Fired on `SWITCH_EVENT_RECORD_START` (when `auto-upload` binds it).
Purely informational — pair it with `mod_s3::upload` to correlate
"recording started → upload finished" on the backend.

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

| field | notes |
| --- | --- |
| `status` | `done` \| `failed` |
| `key`/`bucket`/`sha256`/`etag` | `null` on early failures (guard/queue) |
| `src_format`/`dst_format` | extension without dot; `dst_format` differs only when transcoded |
| `deleted` | whether the **source** file was removed after success |
| `trigger` | `api` \| `api_session` \| `record_stop` |
| `error` | present on `failed` |
| `metadata` | caller-supplied JSON (object/array embedded raw; anything else becomes a string) |
| `uuid` | present when the job is tied to a session |

Enqueue-level failures from the RECORD_STOP path also emit
`mod_s3::upload` with `status:"failed"` and a `trigger:"record_stop"`
body so subscribers see every attempted upload.

## Timing & ordering rules

- `s3_upload` returns `+OK` **before** any network I/O; poll `s3_jobs`
  or wait for `mod_s3::upload` for the outcome.
- `s3_cancel` only works while `status=queued`; a running job finishes
  (its result still emits the event).
- Jobs execute strictly one-at-a-time on the single consumer thread;
  `workers` controls tokio parallelism *inside* upload HTTP calls, not
  job concurrency.
- `min-age-ms` may reject a file written milliseconds ago — retry or
  wait for the mtime to age.

## Java/backend integration sketch

The legacy chain was: FS writes WAV → backend `RecordingService` on
`RECORD_STOP` → HTTP → `recording-media` sidecar → S3.

With mod_s3 the sidecar hop disappears:

1. Keep `auto-upload=true`; subscribe ESL to `mod_s3::upload`.
2. On `status=done`, mark the recording uploaded using `key`/`sha256`/
   `metadata.recordingId` for correlation.
3. For manual flows call `uuid_s3 <uuid> upload <path> …` over the
   existing ESL channel instead of the HTTP sidecar endpoint.
4. Transition period: gate with a config flag (`mod_s3` vs `sidecar`)
   and keep the sidecar profile until parity is proven.
