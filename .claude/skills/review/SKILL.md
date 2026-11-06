---
name: review
description: Review code changes with 3 parallel subagents (code quality, correctness, performance). Use when asked to review a diff, branch, commit range or the working tree.
---

Review target = `$ARGUMENTS` (commit range, branch, or file paths). Default: uncommitted changes plus commits of the current branch not in `main` (`git diff main...HEAD` + `git diff HEAD`).

1. Determine the target diff yourself (`git diff --stat` to size it). Don't paste the diff into the prompts; give each subagent the exact git command to see it.
2. Launch 3 subagents in ONE message (parallel, subagent_type `general-purpose`, read-only: they must not edit files, commit or run benchmarks). Each prompt: the diff command, the focus below, "read surrounding code for context, report only concrete findings with `file:line`, severity (high/medium/low), why, and a suggested fix; no praise, no style nitpicks outside your focus, say 'no findings' if clean".
   - **Code quality:** readability, naming, duplication, dead code, module boundaries and abstractions, error handling style, docstrings (repo requires them), consistency with surrounding code and CLAUDE.md/README conventions, test coverage of new behavior, clippy-pedantic style issues.
   - **Correctness:** logic bugs, edge cases (empty/overflow/large inputs), IMAP/maildir semantics (UIDs, UIDVALIDITY, MODSEQ, flags, renames, move/copy atomicity), race conditions and ordering between watchers/sync tasks/hooks, cancellation and shutdown paths, error propagation, data loss risks (local mail files, sqlite state), unsafe/FFI soundness, tests that don't really assert the behavior.
   - **Performance:** memory (peak RSS, unbounded queues/buffers, copies of mail bodies, byte budgets) and throughput (round trips and pipelining, per-mail syscalls or sqlite transactions that could be batched, fsyncs, blocking calls on the runtime thread, lock contention, O(n²) on mailbox size). Quantify when possible; suggest the `benchmark` skill scenario that would verify a suspected regression rather than running it.
3. Merge the reports: dedupe, drop findings you verified as wrong (check the code yourself for high-severity ones), order by severity. Output a short list grouped by severity with `file:line`; end with a one-line verdict. Do not apply fixes unless asked.
