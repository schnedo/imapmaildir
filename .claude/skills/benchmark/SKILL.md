---
name: benchmark
description: Benchmark imapmaildir end-to-end against a rootless local dovecot (no docker), comparing a baseline commit with the working tree. Use when asked to benchmark/measure performance of sync, moves, hooks, memory or uploads.
---

Sandbox has no docker. `/` is tmpfs (fsync free), the project dir is a btrfs bind mount, so keep client data under `target/benchdata`. Always `nix develop -c ...` (OpenSSL etc). The nix store is a chroot store: dovecot paths only resolve inside `nix shell`.

## Steps

1. **Binaries** in `/tmp/benchL/bin/{old,new}`:
   - new: `cargo build --release` in the repo, copy `target/release/imapmaildir`.
   - old: `git archive <baseline> | tar -x -C /tmp/benchL/head`, then `find /tmp/benchL/head -type f -exec touch {} +` (git archive keeps commit mtimes, so cargo would reuse a stale build), then inside `nix develop` started in the repo (NOT a git worktree: the dev shell hook installer uninstalls shared git hooks there): `cd /tmp/benchL/head && CARGO_TARGET_DIR=<repo>/target/bench-head cargo build --release`. `cmp` old/new, check `.git/hooks` still has `commit-msg`, `pre-commit`.
2. **Dovecot:** `sh .claude/skills/benchmark/local-dovecot-start.sh` (config in /tmp/imaptest, imaps 31993, user "user"/"password", mail in /tmp/imaptest/home/user/mail; runs 30 min). Dovecot 2.4 rejects one-line blocks with settings; no `imap4rev2_enable`.
3. **Run** a script: `nix develop -c python3 .claude/skills/benchmark/<script>.py <mode> N reps`. Scripts alternate old/new order. Run the client with `--log-level warn` (default is Trace).
4. **Cleanup:** kill processes whose /proc/*/comm is dovecot, anvil, log, config, imap-login, imap, auth, stats; `rm -rf` target/benchdata target/bench-head /tmp/benchL /tmp/imaptest using literal paths (rm with `$VAR/$(...)` targets is blocked).

## Scripts (in this directory)

- `bench2.py <resync|deliver|remoteflags> N reps`: no-change resync (N mails), N local deliveries into `new/` then one sync, server `\Flagged` toggle on N mails then one sync.
- `bench6.py <move|hookflags|hookslow|hookdeliver> N reps`: UID MOVE of N mails INBOX→Archive (old: delete+upload); change-hook batching over 3 mailboxes (`NOHOOK=1`, `SLEEP=` knobs, see docstring).
- `bench.py`: rejected-change retry scenario (continuous sync, remote STORE then local rename all rejected `[MODIFIED]`, measures time to server state). `CONFLICT=0` skips the remote STORE, `MODE=delete` deletes local files. Uses user "user" at /tmp/imaptest. `proxy.py`: TCP proxy adding one-way delay for RTT simulation.
- `bench3.py <large|small> <reps> <bins...>`: initial-sync peak RSS via `os.wait4`. large = 150×10 MB (server on tmpfs, ~1.5 GB), small = 6000×2 KB. `STALL=3` holds the sqlite write lock 3 s to simulate slow storage (queue growth only shows then).
- `bench4.py <large|small> <reps> <bins...>` (bins in /tmp/benchU/bin): upload sync after copying N local mails into an empty-synced client; prints wall, client CPU, peak RSS. Wall is dovecot-bound/noisy → compare CPU. `COLD=1` drops page cache of the copies.
- `bench5.py <deliver|idle> <reps> [notmuch:0/1]`: notmuch-enabled deliver/upload N and idle flag updates (bins /tmp/benchI/bin/{old,new}; run inside `nix shell nixpkgs#notmuch ...`, see its docstring).

## Gotchas

- **Initial sync is server-bound:** dovecot's `imap-login` (TLS proxy) burns ≈0.7 s CPU per connection, `imap` ≈0.25 s, client mailbox threads ≈0.28 s each. Client-side changes don't show in initial-sync wall time, compare client user/sys CPU (per-thread via /proc/<pid>/task/*/stat).
- Verify results too: expected mail counts, no "panic|error" in logs. Report median/mean/sd over ≥5 pairs.
- Don't write benchmark results into this file; put findings in the commit/PR text.

## Reference results (for orientation; localhost, NVMe/btrfs)

- 2026-10-18 batching sqlite: initial 6000 mails ≈1.3–1.45 s, resync ≈0.06 s, local/remote flags 1000×3 ≈0.12 s, remote delete ≈0.10 s (fsync dominated before batching: 15 s).
- 2026-10-23: retry path N=1000 per-UID 7.2 s → batched 0.22 s. Off-runtime listing: resync 20k ≈0.09 s, deliver 2000 0.48 → 0.46 s. io_uring RENAMEAT custom op was 3–4% slower than sync rename (rejected). Byte budget fix: stalled peak 1.1 GiB → 150 MiB, unstalled wall unchanged. Streaming APPEND: large upload peak 155 → 27 MiB, CPU 2.08 → 1.27 s, wall equal. Double-buffered upload chunks: no change.
- 2026-11-06, a29e551 → 1be3782: resync 20k 0.08 → 0.065 s; deliver 2000 / remoteflags 5000 / hookflags 2000×3 / hookdeliver 2000 equal (~0.49 / ~0.47 / ~0.30 / ~0.82 s); move 1000 ~0.52 → ~0.55 s (≤+8%, noisy); hookslow 300×3 with 0.2 s hook 0.39 → 0.61 s (hooks now serialized, 2 runs instead of 3 parallel).
