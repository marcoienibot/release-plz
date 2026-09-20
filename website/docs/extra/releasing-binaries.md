# Releasing binaries

## Using release-plz dist

[`release-plz dist`](../usage/dist.md) builds binary archives, writes checksums and
a download table, and publishes an existing draft GitHub release after all targets
are ready. Create drafts with `release-plz release`, run `dist build --target` on
each target's runner, then pass the CI matrix's target list to `dist publish` in
one final job.

CI still owns runners, toolchain installation and artifact transfer. Release-plz
does not generate distribution workflows. See the
[command guide](../usage/dist.md) for configuration, cross compilation, local
validation and retry behavior.

## Using other tools after release

If you are using release-plz to release your project, you can
run a CI job on the "tag" or "release" events to build and release the binaries.

Here is an example using `upload-rust-binary-action`:

:::info
To use this in your project, change:

- the repository owner from `"MyOwner"` to your username/organisation.
- the release name from `"my-bin-v"` to the release name of your binary according to
  [`git_release_name`](../config.md#the-git_release_name-field).

:::

```yaml
name: CD # Continuous Deployment

on:
  release:
    types: [published]

env:
  CARGO_INCREMENTAL: 0
  CARGO_NET_GIT_FETCH_WITH_CLI: true
  CARGO_NET_RETRY: 10
  CARGO_TERM_COLOR: always
  RUST_BACKTRACE: 1
  RUSTFLAGS: -D warnings
  RUSTUP_MAX_RETRIES: 10

defaults:
  run:
    shell: bash

jobs:
  upload-assets:
    name: ${{ matrix.target }}
    if: github.repository_owner == 'MyOwner' && startsWith(github.event.release.name, 'my-bin-v')
    runs-on: ${{ matrix.os }}
    permissions:
      contents: write
    strategy:
      matrix:
        include:
          - target: aarch64-unknown-linux-gnu
            os: ubuntu-22.04
          - target: aarch64-unknown-linux-musl
            os: ubuntu-22.04
          - target: aarch64-apple-darwin
            os: macos-14
          - target: aarch64-pc-windows-msvc
            os: windows-2022
          - target: x86_64-unknown-linux-gnu
            os: ubuntu-22.04
          - target: x86_64-unknown-linux-musl
            os: ubuntu-22.04
          - target: x86_64-pc-windows-msvc
            os: windows-2022
          - target: x86_64-unknown-freebsd
            os: ubuntu-22.04
    timeout-minutes: 60
    steps:
      - name: Checkout repository
        uses: actions/checkout@v6
        with:
          persist-credentials: false
      - name: Install Rust toolchain
        uses: dtolnay/rust-toolchain@stable
      - uses: taiki-e/setup-cross-toolchain-action@v1
        with:
          target: ${{ matrix.target }}
        if: startsWith(matrix.os, 'ubuntu') && !contains(matrix.target, '-musl')
      - uses: taiki-e/install-action@v2
        with:
          tool: cross
        if: contains(matrix.target, '-musl')
      - run: echo "RUSTFLAGS=${RUSTFLAGS} -C target-feature=+crt-static" >> "${GITHUB_ENV}"
        if: endsWith(matrix.target, 'windows-msvc')
      - uses: taiki-e/upload-rust-binary-action@v1
        with:
          bin: my-bin
          target: ${{ matrix.target }}
          tar: all
          zip: windows
          token: ${{ secrets.GITHUB_TOKEN }}
```

Some projects to consider for this task:

- [upload-rust-binary-action](https://github.com/taiki-e/upload-rust-binary-action):
  GitHub Action for building and uploading Rust binary to GitHub Releases.
- [cargo-dist](https://crates.io/crates/cargo-dist):
  shippable application packaging for Rust.

:::caution
To release a binary after release, the release-plz GitHub Action needs to
[trigger further workflow runs](../github/token.md).
:::
