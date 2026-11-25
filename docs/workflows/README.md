# CWF Workflow State

This directory is the tracked project boundary for CWF workflow artifacts.

- Feature artifacts are created under `docs/workflows/features/`.
- Long-lived codebase context belongs under `docs/workflows/codebase/`.
- Worker and leader SQLite databases, run logs, leases, caches, and provider
  credentials are host-owned runtime state and must not be committed here.
- Repository changes are developed in isolated worker worktrees and merged
  through the CWF merge queue after review.
