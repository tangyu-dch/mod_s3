#!/usr/bin/env bash
# mod_s3 部署验证：在 FreeSWITCH 宿主机/容器内执行。
# 用法: ./scripts/verify-deployment.sh
# 覆盖: FS_CLI (默认 fs_cli)、FS_PREFIX (默认 /usr/local/freeswitch)
set -u

FS_CLI="${FS_CLI:-fs_cli}"
FS_PREFIX="${FS_PREFIX:-/usr/local/freeswitch}"
fail=0

check() { # check <desc> <cmd...>
    local desc="$1"; shift
    if "$@" >/dev/null 2>&1; then
        echo "PASS  $desc"
    else
        echo "FAIL  $desc"
        fail=1
    fi
}

fs_api() { "$FS_CLI" -x "api $*" 2>/dev/null; }

echo "== mod_s3 deployment verification =="
echo "fs_cli=$FS_CLI prefix=$FS_PREFIX"

# 1. .so 存在
if [ -f "$FS_PREFIX/mod/mod_s3.so" ]; then
    echo "PASS  $FS_PREFIX/mod/mod_s3.so exists"
else
    echo "FAIL  $FS_PREFIX/mod/mod_s3.so missing"
    fail=1
fi

# 2. modules.conf.xml load 项
if grep -qs '<load module="mod_s3"/>' "$FS_PREFIX/conf/autoload_configs/modules.conf.xml" 2>/dev/null; then
    echo "PASS  modules.conf.xml loads mod_s3"
else
    echo "FAIL  modules.conf.xml missing <load module=\"mod_s3\"/>"
    fail=1
fi

# 3. module_exists
if [ "$(fs_api module_exists mod_s3 | tr -d '[:space:]')" = "true" ]; then
    echo "PASS  module_exists mod_s3"
else
    echo "FAIL  module_exists mod_s3 -> $(fs_api module_exists mod_s3)"
    fail=1
fi

# 4. API 回调已注册（-USAGE 证明命令存在）
out="$(fs_api s3_upload 2>&1)"
case "$out" in
    *-USAGE*) echo "PASS  api s3_upload registered ($out)" ;;
    *)        echo "FAIL  api s3_upload -> $out"; fail=1 ;;
esac

out="$(fs_api s3_jobs 2>&1)"
case "$out" in
    *job=*|*"no jobs"*) echo "PASS  api s3_jobs works ($out)" ;;
    *)                  echo "FAIL  api s3_jobs -> $out"; fail=1 ;;
esac

# 5. conf 文件存在（警告而非失败——模块可用默认配置运行）
if [ -f "$FS_PREFIX/conf/autoload_configs/mod_s3.conf.xml" ]; then
    echo "PASS  mod_s3.conf.xml present"
else
    echo "WARN  mod_s3.conf.xml missing (module runs on defaults)"
fi

# 6. ldd 依赖可解析（.so 存在时）
if [ -f "$FS_PREFIX/mod/mod_s3.so" ]; then
    if ldd "$FS_PREFIX/mod/mod_s3.so" 2>/dev/null | grep -q "not found"; then
        echo "FAIL  unresolved shared libraries:"
        ldd "$FS_PREFIX/mod/mod_s3.so" 2>/dev/null | grep "not found" | sed 's/^/       /'
        fail=1
    else
        echo "PASS  all shared libraries resolve"
    fi
fi

echo "== done (fail=$fail) =="
exit $fail
