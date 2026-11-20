---
name: hey-sync
description: >-
  Keep the hey repository identical on turing (~/Projects/hey) and fermi
  (~/Projects/hey-clone) through the shared GitHub remote. Use before starting
  work on hey, before switching machines, when asked to sync, push or pull hey
  between machines, when the two checkouts look different, or when a push or
  fetch on either machine fails.
---

# Sync hey between turing and fermi

hey lives in two checkouts of one repository. GitHub (`origin`, branch `main`) is the single source of truth. The checkouts never copy files to each other as a way of syncing: they exchange commits through `origin`. If they drift, history diverges and work gets lost or duplicated.

| | turing (macOS) | fermi (Linux) |
|---|---|---|
| Path | `~/Projects/hey` | `~/Projects/hey-clone` |
| `origin` | `https://github.com/brendon-felix/hey.git` | `git@github.com:brendon-felix/hey.git` (same repo over SSH) |
| Auth | `gh` login (keyring) | SSH key `~/.ssh/id_github_fermi` registered on GitHub |

Both must point at `brendon-felix/hey`. Check with `git remote -v`.

## Rules

1. **Start clean.** Before editing, run `git fetch origin && git status -sb`. If behind, `git pull --rebase origin main`. Never start on a stale checkout.
2. **Finish pushed.** End every piece of work by committing and pushing to `origin main`, on whichever machine you are on. Uncommitted work exists on one machine only, so it counts as unsynced.
3. **Rebase, never force.** Use `git rebase origin/main`, not merge commits or `--force`. Resolve conflicts keeping both sides' unrelated work; if intent is unclear, `git rebase --abort` and ask.
4. **One writer at a time.** Do not work on the same files on both machines concurrently.
5. **Do not copy source files between machines** (`write`/`edit` against the other machine, `scp`, `rsync`) to sync. That is how the checkouts diverged before. Commit and push instead.
6. Landing a change (verify with `cargo build`, `cargo test`, `cargo clippy`, then push) is the `merge` skill's job. Use it for the commit-and-push step.

## Check state

On each machine:

```sh
git fetch origin
git status -sb                                  # ahead/behind and dirty files
git rev-list --left-right --count HEAD...origin/main   # "ahead behind"
```

In sync means `0 0` and a clean tree. Both machines should show the same `git rev-parse --short HEAD`.

## Normal flow

On the machine where work was done:

```sh
git add -A && git commit -m "short lowercase imperative message"
git fetch origin && git rebase origin/main
cargo build && cargo test
git push origin main
```

On the other machine: `git pull --rebase origin main` (a fast-forward when it has no local commits).

## If fermi cannot push

Fermi authenticates over SSH. Test with `ssh -T git@github.com`: it should greet `brendon-felix`.

- `Permission denied (publickey)`: the key `~/.ssh/id_github_fermi.pub` is not registered on GitHub. Add it at https://github.com/settings/ssh/new (type "Authentication Key"), or from turing with `gh auth refresh -h github.com -s admin:public_key` followed by `gh ssh-key add ~/key.pub --title fermi --type authentication`. Make sure fermi's `~/.ssh/config` uses that key for `github.com` (`IdentityFile ~/.ssh/id_github_fermi`, `IdentitiesOnly yes`).
- `Host key verification failed`: GitHub's host keys are missing from `~/.ssh/known_hosts`. Take them from https://api.github.com/meta (`ssh_keys`) rather than trusting on first use.
- `could not read Username`: `origin` is still the HTTPS URL. Run `git remote set-url origin git@github.com:brendon-felix/hey.git`.

## Fallback: push fermi's commits through turing

Turing can reach fermi over Tailscale (`felixb@100.112.215.8`); fermi cannot reach turing over SSH. When fermi cannot push, do this on turing:

```sh
cd ~/Projects/hey
git fetch ssh://felixb@100.112.215.8/home/felixb/Projects/hey-clone main
git merge --ff-only FETCH_HEAD     # if it is not a fast-forward, rebase fermi's commits onto origin/main on fermi first
git push origin main
```

Then on fermi run `git fetch origin && git status -sb` to confirm `0 0`. `FETCH_HEAD` holds only committed work, so commit on fermi first.

## Do not

- Force-push, or `reset --hard` over uncommitted work on either machine. Check `git status` first, because fermi has carried local-only patches before.
- Leave one machine ahead of or behind `origin/main` at the end of a task. Report the state if you cannot fix it.
- Deploy from an unsynced checkout. The `hey-deploy` skill builds from fermi's checkout, so sync first.
