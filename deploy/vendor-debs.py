#!/usr/bin/env python3
"""Vendor curl/ca-certificates .deb 闭包（R8-F6：APT 可复现）。

从固定 Debian snapshot（snapshot.debian.org，宿主可达；容器内不可达）
解析 curl + ca-certificates 的依赖闭包，下载 .deb + SHA256 校验，
写入 deploy/vendor-debs/（COPY 进镜像 dpkg -i 离线安装，运行镜像完全
移除 apt-get update——不再依赖可变仓库解析）。

用法: python3 deploy/vendor-debs.py --platform linux/amd64|linux/arm64
输出: amd64 -> deploy/vendor-debs/，arm64 -> deploy/vendor-debs-arm64/
"""
import argparse
import fcntl
import gzip
import hashlib
import os
import re
import shutil
import sys
import time
import urllib.error
import urllib.request
from typing import NamedTuple

SNAPSHOT = "https://snapshot.debian.org/archive/debian/20260816T000000Z"
DIST = "trixie"
COMP = "main"
BASE_DIR = os.path.dirname(os.path.abspath(__file__))

# curl 精确版本 + ca-certificates（最新）。
CURL_PIN = "8.14.1-2+deb13u4"

# R3-WINDOWS-001：Windows 不能检出的文件名字符（Debian epoch 分隔符 ':' 会
# 随版本号进入归档文件名）。生成时统一替换为 '_'——只清洗文件名，包字节、
# SHA256 与 dpkg 安装完全不变。
WINDOWS_INVALID_CHARS = re.compile(r'[<>:"/\\|?*\x00-\x1f]')


def archive_file_name(name: str, version: str, deb_arch: str) -> str:
    """归档文件名 = name_version_arch.deb，version 经 Windows 安全清洗。"""
    safe_version = WINDOWS_INVALID_CHARS.sub("_", version)
    return f"{name}_{safe_version}_{deb_arch}.deb"


class Target(NamedTuple):
    platform: str
    deb_arch: str
    output_dirname: str


def resolve_target(platform: str) -> Target:
    targets = {
        "linux/amd64": Target("linux/amd64", "amd64", "vendor-debs"),
        "linux/arm64": Target("linux/arm64", "arm64", "vendor-debs-arm64"),
    }
    try:
        return targets[platform]
    except KeyError as exc:
        raise ValueError(f"unsupported platform: {platform}") from exc


def fetch(url: str) -> bytes:
    attempts = 3
    for attempt in range(1, attempts + 1):
        try:
            with urllib.request.urlopen(url, timeout=60) as r:
                return r.read()
        except urllib.error.URLError:
            if attempt == attempts:
                raise
            print(f"   download retry {attempt}/{attempts}: {url}", file=sys.stderr)
            time.sleep(attempt)
    raise AssertionError("unreachable")


def parse_packages(data: bytes) -> dict:
    """解析 Packages 索引 -> {name: {fields}}"""
    text = data.decode("utf-8", "replace")
    pkgs = {}
    for stanza in text.split("\n\n"):
        fields = {}
        for line in stanza.splitlines():
            if line.startswith(" ") or not line:
                continue
            key, _, val = line.partition(":")
            fields.setdefault(key, []).append(val.strip())
        name = fields.get("Package", [None])[0]
        if name:
            pkgs.setdefault(name, []).append(fields)
    return pkgs


def dep_names(depends_field: list) -> list:
    """Depends 字段 -> 包名列表（取每个 or 组第一个）。"""
    names = []
    for entry in depends_field or []:
        for group in entry.split(","):
            alt = group.split("|")[0].strip()
            m = re.match(r"^([a-z0-9.+-]+)", alt)
            if m:
                names.append(m.group(1))
    return names


def main(argv=None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--platform",
        choices=("linux/amd64", "linux/arm64"),
        default="linux/amd64",
    )
    args = parser.parse_args(argv)
    target = resolve_target(args.platform)
    output = os.path.join(BASE_DIR, target.output_dirname)

    index_url = (
        f"{SNAPSHOT}/dists/{DIST}/{COMP}/binary-{target.deb_arch}/Packages.gz"
    )
    print(f"== 下载 snapshot Packages: {index_url}")
    raw = fetch(index_url)
    pkgs = parse_packages(gzip.decompress(raw))
    print(f"   包总数: {len(pkgs)}")

    # 选择闭包种子。
    curl_candidates = pkgs.get("curl", [])
    curl_ver = [c for c in curl_candidates if c["Version"][0] == CURL_PIN]
    if not curl_ver:
        print(f"FAIL: snapshot 中无 curl={CURL_PIN}")
        sys.exit(1)
    ca_candidates = pkgs.get("ca-certificates", [])
    if not ca_candidates:
        print("FAIL: 无 ca-certificates")
        sys.exit(1)
    seed = [curl_ver[0], ca_candidates[0]]

    # 依赖闭包（排除 Essential——base 镜像已有）。
    closure = {}
    queue = list(seed)
    while queue:
        stanza = queue.pop()
        name = stanza["Package"][0]
        if name in closure:
            continue
        version = stanza["Version"][0]
        closure[name] = (version, stanza)
        for dep in dep_names(stanza.get("Depends", [])):
            cands = pkgs.get(dep)
            if not cands:
                # R9-F9：缺失依赖必须 fail closed——假设 base 已有会掩盖
                # 闭包不完整（dpkg -i 在镜像构建时失败太晚）。
                print(f"FAIL: 依赖 {dep} 不在 snapshot Packages 中（闭包不完整）")
                sys.exit(1)
            # 选最新版本
            best = max(cands, key=lambda c: c["Version"][0])
            if "Essential" in best and best["Essential"][0] == "yes":
                continue  # base 镜像含 essential
            if best["Package"][0] not in closure:
                queue.append(best)

    # R12-F4：唯一临时目录（进程号）+ flock 互斥锁——并发执行不会互删；
    # 交换式替换避免 rmtree 大空窗（中断保留 .old 可恢复）。
    lock_path = output + ".lock"
    with open(lock_path, "w") as lockf:
        fcntl.flock(lockf, fcntl.LOCK_EX)
        try:
            owned_tmp = output + ".tmp-" + str(os.getpid())
            try:
                build_vendor(closure, target, output)
            finally:
                if os.path.isdir(owned_tmp):
                    shutil.rmtree(owned_tmp)
        finally:
            fcntl.flock(lockf, fcntl.LOCK_UN)
    return 0


def build_vendor(closure, target: Target, output: str):
    """在 flock 保护下执行闭包下载与交换式替换。"""
    tmp_output = output + ".tmp-" + str(os.getpid())
    if os.path.isdir(tmp_output):
        shutil.rmtree(tmp_output)
    os.makedirs(tmp_output)
    manifest = []
    for name, (version, stanza) in sorted(closure.items()):
        filenames = [f for f in stanza.get("Filename", []) if f.endswith(".deb")]
        if not filenames:
            # R10-F8/R11-F5：Filename 缺失 fail closed（死分支已删）。
            print(f"FAIL: {name} 无 .deb Filename（闭包不完整）")
            sys.exit(1)
        rel = filenames[0].lstrip("/")
        sha = stanza["SHA256"][0]
        size = int(stanza["Size"][0])
        url = f"{SNAPSHOT}/{rel}"
        out = os.path.join(
            tmp_output, archive_file_name(name, version, target.deb_arch)
        )
        print(f"   下载 {name}_{version}")
        data = fetch(url)
        if len(data) != size or hashlib.sha256(data).hexdigest() != sha:
            print(f"FAIL: {name} SHA256 校验失败"); sys.exit(1)
        with open(out, "wb") as f:
            f.write(data)
        manifest.append(f"{sha}  {os.path.basename(out)}\n")

    with open(os.path.join(tmp_output, "manifest.sha256"), "w") as f:
        f.writelines(manifest)
    # R13-F4：交换式替换（非原子指针——两次 rename 之间 canonical 路径
    # 短暂不存在；已由 flock 串行化 + 持锁恢复遗留 .old 缓解。如实声明：
    # 不是原子指针切换，消费者短暂窗口可能看不到 OUT）。
    if not os.path.isdir(output) and os.path.isdir(output + ".old"):
        # 上次中断恢复：先还原 .old（消费者不长期失联）。
        os.rename(output + ".old", output)
    if os.path.isdir(output):
        old_output = output + ".old"
        if os.path.isdir(old_output):
            shutil.rmtree(old_output)
        os.rename(output, old_output)
    os.rename(tmp_output, output)
    if os.path.isdir(output + ".old"):
        shutil.rmtree(output + ".old")
    print(f"== vendor 完成: {len(closure)} 包 -> {output}（持锁交换式替换）")
    for line in manifest:
        print("   " + line.rstrip())
    return 0


if __name__ == "__main__":
    sys.exit(main())
