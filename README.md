# Review split of release-plz/release-plz#3069

Six draft PRs form native GitHub stack #13 in `marcoienibot/release-plz`. Review from bottom to top. The original PR remains open and unchanged.

| Layer | PR | Scope |
| --- | --- | --- |
| 1 | [#7](https://github.com/marcoienibot/release-plz/pull/7) | feat(git_cmd): add bounded path history walks |
| 2 | [#8](https://github.com/marcoienibot/release-plz/pull/8) | fix(changelog): collect package history across merged branches |
| 3 | [#9](https://github.com/marcoienibot/release-plz/pull/9) | fix(changelog): retain changes surviving equal package snapshots |
| 4 | [#10](https://github.com/marcoienibot/release-plz/pull/10) | fix(changelog): distinguish independent edits within conflicted lines |
| 5 | [#11](https://github.com/marcoienibot/release-plz/pull/11) | fix(changelog): align retained files with package equality |
| 6 | [#12](https://github.com/marcoienibot/release-plz/pull/12) | refactor(git_cmd)!: remove checkout-per-commit history helpers |

## Recreate upstream as marcoieni

Download `recreate-pr3069-stack.sh`, inspect it, and run:

```bash
bash recreate-pr3069-stack.sh
```

The script requires Git and an authenticated GitHub CLI. It installs `github/gh-stack` if absent, verifies that the authenticated account is `marcoieni` with write access, and creates an isolated temporary checkout. It fetches the six fork branches and verifies their pinned commits, parent chain, and final tree before publishing them to `release-plz/release-plz`.

It uses `gh stack init`, `gh stack push`, and `gh stack submit` to create the same native stack upstream. All PRs are drafts with their complete descriptions and AI disclosures. Existing branches at different commits are rejected. The script does not push upstream main or close the original PR.

If interrupted, rerun with the printed checkout directory:

```bash
bash recreate-pr3069-stack.sh /path/printed/by/the/script
```

## Verification

- Final stack tree equals original PR commit `f5ff5d14ab20c10708b8009f2f0768ef76d15320` exactly: tree `b51ad06ebfcee5f39a9974df7f753b313a29a7db`.
- Layer 1: 16 Git helper tests pass.
- Layers 2–5: 14, 17, 22, and 32 history tests pass respectively.
- Final workspace suite: 379 tests pass, 3 ignored, including 87 Docker integration tests.
- Full workspace Clippy (`--all-targets --all-features -- -D warnings`) and formatting pass at every layer.
- The script passes Bash syntax checking and ShellCheck. Its fork-targeted variant created the native stack and was used to check resumability.

Local validation used `--target-dir /home/marco.guest/work/release-plz/target` to share build artifacts without changing Cargo subprocess output paths inside the tests.

Layer 6 is the isolated breaking `git_cmd` API cleanup and requires a 0.7.0 release. Mat Jones's original contribution retains authorship in layer 2 and co-authorship in layer 1. Codex split the existing Codex/Claude-assisted implementation at @marcoieni's request; details of the previous work remain in the source PR.

`manifest.json` records the original base and the six exact branch heads. This helper branch is separate from the six code PRs.
