# build-proof 采集命令（T-002 / CP-002）

## base image digest（Q-02 冻结）

```bash
# AWS ECR Public（Canonical LTS mirror）manifest list
TOKEN=$(curl -s 'https://public.ecr.aws/token/?scope=repository:lts/ubuntu:pull&service=public.ecr.aws' \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["token"])')
curl -sI -H "Authorization: Bearer $TOKEN" \
  -H "Accept: application/vnd.oci.image.index.v1+json" \
  https://public.ecr.aws/v2/lts/ubuntu/manifests/24.04
# docker-content-digest: sha256:be20a0347f238b7d373edddc55923443b21dd9a60277bf8a93e43458cd0bf2fc

# amd64 platform digest
curl -s -H "Authorization: Bearer $TOKEN" \
  -H "Accept: application/vnd.oci.image.index.v1+json" \
  https://public.ecr.aws/v2/lts/ubuntu/manifests/24.04 \
  | python3 -c 'import json,sys; [print(m["platform"], m["digest"]) for m in json.load(sys.stdin)["manifests"]]'
# linux/amd64 -> sha256:019e8eb29a85e74d64925745884f2ec79aa27e3feab36353d24656f4d6b89467
```

## build-proof digest

```bash
cd codex-rs
rustup toolchain install 1.95.0 --profile minimal --component clippy,rustfmt,rust-src
cargo build --locked -p codex-transportd
shasum -a 256 target/debug/codex-transportd
```

## lifecycle matrix（端到端）

```bash
scripts/installation_lifecycle.sh codex-rs/target/debug/codex-transportd
```
