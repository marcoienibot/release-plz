#!/usr/bin/env bash
# Recreate the six-layer review stack from release-plz/release-plz#3069.
# Requires git and an authenticated GitHub CLI; installs github/gh-stack if absent.
# Usage: bash recreate-pr3069-stack.sh [directory-for-isolated-checkout]
# On failure, rerun with the printed directory to resume. PRs are created as drafts.
set -euo pipefail

fail() { printf '%s\n' "$*" >&2; exit 1; }
command -v git >/dev/null || fail 'git is required.'
command -v gh >/dev/null || fail 'GitHub CLI (gh) is required.'
account=$(gh api user --jq .login)
[[ "$account" == marcoieni ]] || fail "Expected marcoieni; authenticated as $account. Use gh auth switch --user marcoieni."
target_repo=release-plz/release-plz
source_repo=marcoienibot/release-plz
export GH_REPO="$target_repo"
permission=$(gh repo view "$target_repo" --json viewerPermission --jq .viewerPermission)
case "$permission" in ADMIN|MAINTAIN|WRITE) ;; *) fail 'Write access to release-plz/release-plz is required.' ;; esac
if ! gh stack --help >/dev/null 2>&1; then
  gh extension install github/gh-stack
fi
# Fail before publishing branches if the native Stacks API is unavailable.
gh api "repos/$target_repo/stacks" >/dev/null

base=1f8dd28cada6ed7ac6cece686af58f88c6fa0684
original=f5ff5d14ab20c10708b8009f2f0768ef76d15320
branches=(
  pr-3069/01-git-history
  pr-3069/02-branch-walk
  pr-3069/03-retained-changes
  pr-3069/04-text-conflicts
  pr-3069/05-file-equality
  pr-3069/06-remove-old-api
)
commits=(
  3821884f4fd286d866ab13ed67ac3c270f66e183
  461a10cc8098ad78846368c318c3e2ebf291ab08
  2763d66bd380aa6d250b76ef7246790bb7483eff
  f090c297669c4c4e16c81135888ba637ed6663c2
  d95b5cb9d482013c45daa380972a667a7a1f5ed0
  8cd0679cbe12d9b3fb29976a2c22f92cbe7a726e
)
titles=(
  'feat(git_cmd): add bounded path history walks'
  'fix(changelog): collect package history across merged branches'
  'fix(changelog): retain changes surviving equal package snapshots'
  'fix(changelog): distinguish independent edits within conflicted lines'
  'fix(changelog): align retained files with package equality'
  'refactor(git_cmd)!: remove checkout-per-commit history helpers'
)

stack_dir=${1:-$(mktemp -d "${TMPDIR:-/tmp}/release-plz-pr3069.XXXXXX")}
mkdir -p "$stack_dir"
cd "$stack_dir"
stack_dir=$(pwd -P)
printf 'Stack checkout: %s\n' "$stack_dir"
trap 'printf "Stack checkout retained at %s; rerun this script with that directory to resume.\n" "$stack_dir" >&2' ERR
if [[ ! -d .git ]]; then
  [[ -z "$(ls -A)" ]] || fail 'Use an empty directory for the isolated checkout.'
  git init -q
  git remote add origin "https://github.com/$target_repo.git"
  git remote add source "https://github.com/$source_repo.git"
  git config pr3069.recreate true
else
  [[ "$(git config --get pr3069.recreate || true)" == true ]] || fail 'This directory was not created by this script.'
fi
[[ "$(git remote get-url origin)" == "https://github.com/$target_repo.git" ]] || fail 'Unexpected origin remote.'
[[ "$(git remote get-url source)" == "https://github.com/$source_repo.git" ]] || fail 'Unexpected source remote.'
[[ -z "$(git status --porcelain)" ]] || fail 'The stack checkout has local changes; preserve them before resuming.'
# Authenticate HTTPS pushes using gh, without modifying your global Git config.
git config --replace-all credential.https://github.com.helper ''
git config --add credential.https://github.com.helper '!gh auth git-credential'
gh repo set-default "$target_repo"
git fetch --no-tags origin main:refs/remotes/origin/main
refspecs=()
for branch in "${branches[@]}"; do
  refspecs+=("refs/heads/$branch:refs/remotes/source/$branch")
done
git fetch --no-tags source "${refspecs[@]}"
git fetch --no-tags source "$original"
git merge-base --is-ancestor "$base" origin/main || fail 'The original base is not in upstream main.'
previous=$base
for i in "${!branches[@]}"; do
  branch=${branches[$i]}
  expected=${commits[$i]}
  actual=$(git rev-parse "refs/remotes/source/$branch")
  [[ "$actual" == "$expected" ]] || fail "Source branch changed: $branch. Expected $expected, found $actual."
  [[ "$(git rev-parse "$expected^")" == "$previous" ]] || fail "Unexpected parent for $branch."
  previous=$expected
  # Refuse to overwrite somebody else's branch or replace a modified local layer.
  remote_sha=$(git ls-remote --heads origin "refs/heads/$branch" | cut -f1)
  [[ -z "$remote_sha" || "$remote_sha" == "$expected" ]] || fail "Upstream branch already exists at another commit: $branch."
  if git show-ref --verify --quiet "refs/heads/$branch"; then
    [[ "$(git rev-parse "$branch")" == "$expected" ]] || fail "Local branch changed: $branch."
  else
    git branch "$branch" "$expected"
  fi
  if [[ -n "$remote_sha" ]]; then
    git fetch --no-tags origin "refs/heads/$branch:refs/remotes/origin/$branch"
  fi
done
git diff --exit-code "$original" "${commits[5]}" --
# Keep the recorded base locally so the imported commit trees remain identical.
# GitHub targets the current upstream main; this script never pushes main.
if ! git show-ref --verify --quiet refs/heads/main; then
  git branch main "$base"
fi
git checkout "${branches[5]}"
if [[ ! -f .git/gh-stack ]]; then
  gh stack init --base main "${branches[@]}"
fi
body_dir=$(mktemp -d "$stack_dir/.git/pr3069-bodies.XXXXXX")
cat > "$body_dir/1.md" <<'PR3069_BODY_1'
Add explicit-tip, date-ordered path history walks so callers can collect all merged branches before checking out historical snapshots. Missing exclusion commits are tolerated; existing exclusions on divergent branches still remove their shared history. A separate full-history ancestor walk follows parents hidden by Git's simplified walk.

The existing checkout helpers remain available until layer 6. Tests cover explicit tips, exclusions, missing/divergent boundaries, date-ordered limits across branches, and full-history ancestry. The dated Git fixture is shared with the core regression tests.

Validation: `cargo test -p git_cmd --all-features` (16 tests).

Layer 1/6 of the review split of [release-plz/release-plz#3069](https://github.com/release-plz/release-plz/pull/3069), related to [#2989](https://github.com/release-plz/release-plz/issues/2989). Review bottom to top: Git helpers → branch walk → retained changes → text conflicts → file equality → API cleanup.

- [x] I have read the [AI Policy](https://github.com/release-plz/.github/blob/main/AI_POLICY.md) and the [contributing guidelines](https://github.com/release-plz/release-plz/blob/main/CONTRIBUTING.md).

## AI Disclosure

At @marcoieni's request, Codex split the existing implementation and tests from #3069 into review layers and prepared this description. The source PR records the prior Codex and Claude contributions. Mat Jones's original branch-walk contribution from #2857 retains authorship in layer 2 and co-authorship in layer 1.
PR3069_BODY_1
cat > "$body_dir/2.md" <<'PR3069_BODY_2'
The checkout-per-commit walk loses sibling branches when HEAD moves into one lineage. Collect the path-limited commit list once from the tip, then inspect the snapshots. Tag and published-commit boundaries exclude released history, including tagged packages that are not published to a registry.

Equal snapshots prune their full-history ancestors, with a final filter handling ancestors visited before their boundary. Dependency-only updates are checked after returning to the branch tip, and blocked checkouts recover the `--allow-dirty` hint. Package inspection shares Cargo.lock restoration.

The 14 history tests cover sibling and late merges, boundaries, reverts, discarded branches in both walk orders, empty package ranges, dependency updates, limits, and dirty checkouts. Layers 3–5 refine equal-snapshot pruning for changes that survive through another lineage.

Validation: `cargo test -p release_plz_core --all-features --lib history_tests` (14 tests).

Layer 2/6 of the review split of [release-plz/release-plz#3069](https://github.com/release-plz/release-plz/pull/3069), related to [#2989](https://github.com/release-plz/release-plz/issues/2989). Review bottom to top: Git helpers → branch walk → retained changes → text conflicts → file equality → API cleanup.

- [x] I have read the [AI Policy](https://github.com/release-plz/.github/blob/main/AI_POLICY.md) and the [contributing guidelines](https://github.com/release-plz/release-plz/blob/main/CONTRIBUTING.md).

## AI Disclosure

At @marcoieni's request, Codex split the existing implementation and tests from #3069 into review layers and prepared this description. The source PR records the prior Codex and Claude contributions. Mat Jones's original branch-walk contribution from #2857 retains authorship in layer 2 and co-authorship in layer 1.
PR3069_BODY_2
cat > "$body_dir/3.md" <<'PR3069_BODY_3'
An ancestor of an equal package snapshot can still contribute a change through another merge lineage. Track the simplified parent graph, stop each lineage at equal snapshots, and check surviving candidates with in-memory inverse changes against the release and HEAD. This preserves breaking-change markers that ancestry pruning alone would discard.

The check uses Cargo's file lists from both snapshots, handles merge resolutions relative to the first parent, and keeps conservative behavior when conflicts cannot prove a contribution absent. Tests cover conflict resolutions, deletions, and changes moved to another file.

Validation: `cargo test -p release_plz_core --all-features --lib history_tests` (17 tests). Same-line text refinement follows in layer 4; specialized package file equality follows in layer 5.

Layer 3/6 of the review split of [release-plz/release-plz#3069](https://github.com/release-plz/release-plz/pull/3069), related to [#2989](https://github.com/release-plz/release-plz/issues/2989). Review bottom to top: Git helpers → branch walk → retained changes → text conflicts → file equality → API cleanup.

- [x] I have read the [AI Policy](https://github.com/release-plz/.github/blob/main/AI_POLICY.md) and the [contributing guidelines](https://github.com/release-plz/release-plz/blob/main/CONTRIBUTING.md).

## AI Disclosure

At @marcoieni's request, Codex split the existing implementation and tests from #3069 into review layers and prepared this description. The source PR records the prior Codex and Claude contributions. Mat Jones's original branch-walk contribution from #2857 retains authorship in layer 2 and co-authorship in layer 1.
PR3069_BODY_3
cat > "$body_dir/4.md" <<'PR3069_BODY_4'
Sequential edits to the same source line can make an inverse change conflict even when that contribution is absent from the released package. Refine supported text conflicts at Unicode-character granularity using libgit2's three-way merge, retaining conservative handling for unresolved, binary, large, or unsupported conflicts.

Five regressions cover retained merge resolutions, sequential API changes, later edits that preserve or restore the signature, discarded sibling changes, and already-released breaking markers. They assert both the selected commits and resulting version bump across different dates and release boundaries.

Validation: `cargo test -p release_plz_core --all-features --lib history_tests` (22 tests).

Layer 4/6 of the review split of [release-plz/release-plz#3069](https://github.com/release-plz/release-plz/pull/3069), related to [#2989](https://github.com/release-plz/release-plz/issues/2989). Review bottom to top: Git helpers → branch walk → retained changes → text conflicts → file equality → API cleanup.

- [x] I have read the [AI Policy](https://github.com/release-plz/.github/blob/main/AI_POLICY.md) and the [contributing guidelines](https://github.com/release-plz/release-plz/blob/main/CONTRIBUTING.md).

## AI Disclosure

At @marcoieni's request, Codex split the existing implementation and tests from #3069 into review layers and prepared this description. The source PR records the prior Codex and Claude contributions. Mat Jones's original branch-walk contribution from #2857 retains authorship in layer 2 and co-authorship in layer 1.
PR3069_BODY_4
cat > "$body_dir/5.md" <<'PR3069_BODY_5'
Align retained-change detection with package snapshot equality: ignore generated metadata at the package root; distinguish nested metadata content changes from additions/deletions; ignore executable bits; respect symlinks and `core.symlinks=false`; and compare configured README changes, including external targets and link retargeting.

Canonical and raw repository paths are both accepted when converting README paths. Ten additional regressions cover these file-comparison cases, including a repository accessed through a symlink. This completes the behavioral changes from the original PR.

Validation: `cargo test -p release_plz_core --all-features --lib history_tests` (32 tests).

Layer 5/6 of the review split of [release-plz/release-plz#3069](https://github.com/release-plz/release-plz/pull/3069), related to [#2989](https://github.com/release-plz/release-plz/issues/2989). Review bottom to top: Git helpers → branch walk → retained changes → text conflicts → file equality → API cleanup.

- [x] I have read the [AI Policy](https://github.com/release-plz/.github/blob/main/AI_POLICY.md) and the [contributing guidelines](https://github.com/release-plz/release-plz/blob/main/CONTRIBUTING.md).

## AI Disclosure

At @marcoieni's request, Codex split the existing implementation and tests from #3069 into review layers and prepared this description. The source PR records the prior Codex and Claude contributions. Mat Jones's original branch-walk contribution from #2857 retains authorship in layer 2 and co-authorship in layer 1.
PR3069_BODY_5
cat > "$body_dir/6.md" <<'PR3069_BODY_6'
Remove `Repo::checkout_last_commit_at_paths` and `Repo::checkout_previous_commit_at_paths`, their private helpers, and the two tests dedicated to that API. The updater now uses the explicit-tip history walk introduced by the earlier layers.

This is a breaking `git_cmd` API change and requires a 0.7.0 release. It is isolated so it can be reviewed or deferred independently of the behavioral fixes.

The completed stack has exactly the same Git tree as original PR #3069 at `f5ff5d14ab20c10708b8009f2f0768ef76d15320`.

Validation: `cargo test --all-features --workspace`, `cargo clippy --all-targets --all-features --workspace -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check`.

Layer 6/6 of the review split of [release-plz/release-plz#3069](https://github.com/release-plz/release-plz/pull/3069), related to [#2989](https://github.com/release-plz/release-plz/issues/2989). Review bottom to top: Git helpers → branch walk → retained changes → text conflicts → file equality → API cleanup.

- [x] I have read the [AI Policy](https://github.com/release-plz/.github/blob/main/AI_POLICY.md) and the [contributing guidelines](https://github.com/release-plz/release-plz/blob/main/CONTRIBUTING.md).

## AI Disclosure

At @marcoieni's request, Codex split the existing implementation and tests from #3069 into review layers and prepared this description. The source PR records the prior Codex and Claude contributions. Mat Jones's original branch-walk contribution from #2857 retains authorship in layer 2 and co-authorship in layer 1.
PR3069_BODY_6

# Push only the six stack branches; main is never pushed.
gh stack push --remote origin
parent=main
for i in "${!branches[@]}"; do
  branch=${branches[$i]}
  number=$(gh pr list --repo "$target_repo" --head "$branch" --state open --json number --jq '.[0].number // empty')
  if [[ -z "$number" ]]; then
    # Create with the full description and AI disclosure already present.
    gh pr create --repo "$target_repo" --base "$parent" --head "$branch" \
      --draft --title "${titles[$i]}" --body-file "$body_dir/$((i + 1)).md"
    number=$(gh pr list --repo "$target_repo" --head "$branch" --state open --json number --jq '.[0].number // empty')
  fi
  [[ -n "$number" ]] || fail "No open PR found for $branch."
  parent=$branch
  # REST avoids the old gh pr edit command's dependency on retired Projects APIs.
  gh api --method PATCH "repos/$target_repo/pulls/$number" \
    --raw-field "title=${titles[$i]}" \
    --field "body=@$body_dir/$((i + 1)).md" >/dev/null
  printf 'Layer %s: https://github.com/%s/pull/%s\n' "$((i + 1))" "$target_repo" "$number"
done
# Link the draft PRs into a native GitHub stack and synchronize their bases.
gh stack submit --auto --remote origin
# Verify native stack membership and order, not just the existence of the PRs.
first_pr=$(gh pr list --repo "$target_repo" --head "${branches[0]}" --state open --json number --jq '.[0].number')
stack_number=$(gh api "repos/$target_repo/stacks?pull_request=$first_pr" --jq '.[0].number // empty')
[[ -n "$stack_number" ]] || fail 'PRs exist but GitHub did not create a native stack. Rerun the script to retry.'
actual_branches=$(gh api "repos/$target_repo/stacks/$stack_number" --jq '.pull_requests[].head.ref')
expected_branches=$(printf '%s\n' "${branches[@]}")
[[ "$actual_branches" == "$expected_branches" ]] || fail 'Unexpected native stack composition.'
gh stack view --short
printf '\nCreated draft stack #%s: https://github.com/%s/pull/%s\nCheckout: %s\n' "$stack_number" "$target_repo" "$first_pr" "$stack_dir"
