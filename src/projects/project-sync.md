---
name: project-sync
description: Decide whether and how to commit and sync a project's copies between the user's machine and the host, and how to handle a project with no git remote. Use after changing files in a project that lives on more than one machine, before starting work on one, when asked to sync or merge, or when `project` reports copies that differ.
---

# Project sync

The agent normally works on the host's copy of a project. The user's machine
holds another. Git is how they are kept alike. This skill is the policy; edit
it to change how hey behaves. Start with `project` `status`.

## Decide first

1. **Is the client connected?** `status` shows `unreachable` for a copy on a
   machine that is away. If so, do not sync now: commit locally if the rules
   below say so, and the user's machine catches up when it reconnects.
2. **Are the copies already the same commit and both clean?** Then do nothing.
3. **Which side is ahead?** Fetch on the side that has a remote (`git fetch`),
   then compare with `git merge-base --is-ancestor` and `git rev-list --count`.
   - One side only ahead: fast-forward the other (`git pull --ff-only`).
   - Both moved (diverged): do **not** merge or rebase on your own. Show the
     user both sides (`git log --oneline` of each) and ask with `ask_user`.
   - Uncommitted changes on the client side: never touch them. Wait for the
     user, or tell them what is waiting on the host.

## Commit and sync

Commit and push **automatically** only when all of these hold:

- the work is a finished, coherent step (it builds, tests you ran pass);
- you are on a branch named `hey/<topic>`, not the user's own branch or
  `main`. Create it if you are on another: `git switch -c hey/<topic>`;
- the user did not ask to review before committing.

Then: `git add` the files you changed (not `-A` blindly), commit with a message
in the repository's existing style, and `git push` the branch to the shared
remote. On the client the user pulls it when ready; if the client is connected
you may run `git fetch` there, but do not switch their branch or touch their
working tree.

**Wait for the user** (say what is ready and ask) when:

- the branch is `main`, `master`, or the user's own working branch;
- the history would be rewritten (force push, rebase, amend of pushed commits);
- there are conflicts, or the diff includes files you did not mean to change;
- the repository holds secrets-looking files (`.env`, keys) in the diff.

Never use `--force`, `reset --hard` or `clean` on a copy you did not create.

## A project with no git remote

`project` `status` shows no `git remote`. The host copy was made, or must be
made, without a shared remote. Decide in this order, and tell the user which
you chose:

1. **Ask for a remote.** Often best: suggest the user create one, then
   `git remote add origin <url>` on the client and `record` the project again.
2. **Bare repository on the host.** If the host has SSH access to the client
   or the client to the host, make a bare repo on the host
   (`git init --bare ~/.local/share/hey/repos/<name>.git`), push the client's
   branch to it, clone it for the host copy, and `record` the copy. The client
   adds it as a remote named `hey`. Only do this after the user agreed to a
   host copy (`project` `adopt` asks).
3. **Folder copy.** If only the client is configured to push files (`[sync]
   paths` in its config), the host holds a one-way file copy under its sync
   directory. Treat it as read-only for syncing: report edits made there and
   let the user bring them over; do not overwrite their files.

If the project is not a git repository at all, do not run `git init` in it
without asking.
