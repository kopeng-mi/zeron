# Fork notes

Personal fork of [zeronsh/zeron](https://github.com/zeronsh/zeron), kept at
[kopeng-mi/zeron](https://github.com/kopeng-mi/zeron) for daily use on Windows.
We develop on this fork only and don't send changes upstream. New upstream
commits get synced in, and we keep working on top of them.

## Remotes

| Remote     | URL                                    | Use                     |
|------------|----------------------------------------|-------------------------|
| `origin`   | https://github.com/kopeng-mi/zeron.git | our fork, push here     |
| `upstream` | https://github.com/zeronsh/zeron.git   | pull new commits only   |

One-time setup:

```sh
git remote add upstream https://github.com/zeronsh/zeron.git
```

## Syncing with upstream

```sh
git fetch upstream
git checkout main
git rebase upstream/main      # replay our patches on top
mbx test -p zeron-harness --lib
mbx build -p zeron
git push --force-with-lease origin main
```

If a patch conflicts, check whether upstream fixed the same problem. If it
did, drop our patch.

## How we deal with problems

1. **Investigate first.** Find the root cause before changing code: read the
   code, check logs in `%LOCALAPPDATA%\Zeron\logs`, and look at saved chats in
   `%LOCALAPPDATA%\Zeron\profiles\local\journals\*.jsonl`.
2. **Agree on the fix** before editing, for anything non-trivial.
3. **Keep patches small.** Change only what the problem needs and match the
   surrounding code, so rebases on upstream stay easy.
4. **Add a test** next to the change when the code has tests.
5. **Verify.** Run `mbx test -p zeron-harness --lib` (or the affected crate)
   and `mbx build -p zeron`, then try it in the app.
6. **Log it** in the table below.

## Building on Windows

- Use the **MSVC** Rust toolchain. `ort-sys` has no prebuilt binaries for
  `x86_64-pc-windows-gnu`, so a GNU `cargo` (e.g. from msys64) fails.
- `mbx build -p zeron` produces `target\debug\zeron.exe`. The first build
  takes ~20 min. Use `--release` for a faster, smaller exe.

## Patch log

| Patch | Problem | Files |
|-------|---------|-------|
| Windows shell tool names | On Windows, Claude Code's shell tool is `PowerShell` and opencode's is `shell`, so shell calls showed as generic "Tool" chips with no command or collapsed output. Mapped both to `Exec`. Only affects new chats; saved calls stay as they were. | `crates/harness/src/claude/normalize.rs`, `crates/harness/src/opencode/mod.rs`, `crates/harness/src/opencode/tests.rs` |
