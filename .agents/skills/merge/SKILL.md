---
name: merge
description: >-
  Merge (land) changes in the hey repository onto main and push to origin.
  Only invoke when the user has explicitly requested merging or landing the
  change, not for review, preparation, passing checks, or skill installation.
disable-model-invocation: true
metadata:
  delta-action: land
---

# Merge changes in hey

Invoking this skill (including `/merge`) is the explicit merge request. Proceed with the workflow; do not ask again for permission to merge and do not reply that you are waiting for a merge request.

The project has no CI, no pull-request process, and no documented contribution policy. Changes land as commits directly on `main`, pushed to `origin` (`https://github.com/brendon-felix/hey.git`). Never force-push.

## 1. Preflight

- `git --no-optional-locks status` and `git branch --show-current`. Work from `main`.
- Identify exactly which changes belong to the requested change. If the working tree mixes in unrelated or ambiguous edits, stop and ask which to include; never discard or commit unrelated work.

## 2. Commit

- Stage only the requested changes (`git add <paths>`, `git rm` for deletions).
- Use short lowercase imperative messages matching history (e.g. `add mcp support`, `fix default for animations config setting`). Commit with `git commit -m "..."`.
- If the `version` in `Cargo.toml` changed, commit it separately as `bump version` (with the `Cargo.lock` update).

## 3. Sync

- `git fetch origin`, then `git rebase origin/main` if `main` is behind.
- Conflicts: resolve them when the intended result is clear, preserving both sides' unrelated work, then `GIT_EDITOR=true git rebase --continue`. If intent is ambiguous or resolution could lose work, `git rebase --abort`, report the conflict, and wait for the user.

## 4. Verify

Run on the exact commit to be pushed (after any rebase). The crate is defined in `Cargo.toml`:

```sh
cargo build
cargo test
cargo clippy
```

All must succeed. Do not merge with failing or unrun checks; report the failure instead. Fix only failures clearly caused by the change, then re-verify.

## 5. Push and confirm

- `git push origin main` (no `--force`). If rejected because the remote moved, return to step 3.
- `git fetch origin` and confirm `git rev-parse origin/main` equals `git rev-parse HEAD`.

If any blocker prevents this, report that the change has not been merged and why.
