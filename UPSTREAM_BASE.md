# UPSTREAM_BASE

The replay1017 source line is based on the immutable OpenAI Codex
`rust-v0.163.0` release object.

| Field | Value |
|---|---|
| upstream repository | `https://github.com/openai/codex` |
| annotated tag | `rust-v0.163.0` |
| tag object | `129b8ec7ee6f4511557a4856279f529025c25af4` |
| peeled base commit | `83086188ae0614bd882818ff1aa23f3194962012` |
| base tree | `e91f335e7a03e1d14fa1e5fc7fc7620cdfc60842` |
| base `codex-rs/Cargo.lock` SHA-256 | `17cdd862471774ba7aef836aaf0a0a3ed76bd23e503b1fc7278c89d1f2719d03` |
| base `MODULE.bazel.lock` SHA-256 | `c38d161a51f8e1ed0bfb51d8677a545229809bd268da394fe900fecb181c0f3c` |
| Rust toolchain | `1.95.0` |
| license | Apache-2.0 |

Previous bases in this line: `rust-v0.159.2` (tag object
`8b9fa496bbf2c47aebd62e85a080b9a522a455b5`, peeled
`ff6aec96948b70d94983af2641a6b67c94faeff5`, tree
`406dfdd5c68f303a3a8d04f32b3965b3b0ca0361`, lock
`4fab6fe99180e2b8f305bd4b33c1b4bd825e3fa4cdef1045be750843bd3d431e`), which in
turn superseded `rust-v0.159.0` (`687a119f0fcaace47e1f1abcc77cec6c813fd6da`,
tree `bee1375c8502e4c4d71db017f7e087dfbb3e775c`, lock
`16b915e94730a7e598182620923fc6eae19e32366d3b5ef91128002575ae24af`).

What changes between `rust-v0.159.2` and `rust-v0.163.0` that matters to this
fork (the full upstream delta is 3153 files, +166,426/−47,284 lines):

| Area | Change | Impact on the fork |
|---|---|---|
| `codex-rs/shell-escalation/` | crate removed upstream | none: it is not a patch target and the fork's two `Cargo.toml` mentions were inherited, not added |
| `codex-rs/cloud-client/` | crate added upstream | comes along untouched |
| `shell-escalation` / `cloud-client` workspace members | swapped | `codex-transportd` is the only member the fork adds |
| `codex-rs/Cargo.lock` | 358 lines (+228/−130) | regenerated from upstream's official lock, never hand-merged |
| `codex-api/src/endpoint/responses_websocket.rs` | 225 lines (+183/−42) | the only real code conflict in the replay |
| `codex-rs/rust-toolchain.toml` | unchanged at `1.95.0` | toolchain pin holds |

`v8` stays at `150.4.0`, so the `v8_archive_sha256_*` pins in
`patches/transportd/series.yaml` are unchanged.

Verification uses the objects already shipped by this repository:

```bash
git cat-file -p 129b8ec7ee6f4511557a4856279f529025c25af4
git rev-parse '129b8ec7ee6f4511557a4856279f529025c25af4^{}'
git show 83086188ae0614bd882818ff1aa23f3194962012:codex-rs/Cargo.lock | shasum -a 256
git show 83086188ae0614bd882818ff1aa23f3194962012:MODULE.bazel.lock | shasum -a 256
```

Changing any base identity requires regenerating the complete patch series and
all source-bound evidence.

The fork's non-upstream base image is unchanged: `debian:trixie-slim` pinned by
digest in `codex-rs/codex-transportd/src/build_info.rs`.
