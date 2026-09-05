# update

The `release-plz update` command updates the version and the changelog of the
packages containing unreleased changes.

The command:

- Downloads the packages of the project from the cargo registry.
- Compares the local packages with the downloaded ones to determine the new commits.
- Checks for API breaking changes in libraries if
  [cargo-semver-checks](https://github.com/obi1kenobi/cargo-semver-checks)
  is installed.
  _Warning:_ `cargo-semver-checks` doesn't catch every semver violation.
- Updates the packages versions based on the messages of the new commits (based
  on [conventional commits](https://www.conventionalcommits.org/) and
  [semantic versioning](https://semver.org/)).
- Updates the packages changelogs with the messages of the new commits.
- Updates all dependencies by running `cargo update` (disabled by default).

In the following example, I run `release-plz` on the `release-plz` project itself.
`Release-plz` increases the version and the changelog of the packages with
unpublished changes.

![release-plz update](https://user-images.githubusercontent.com/11428655/160762832-54300ddb-ec9c-4538-a611-c66490c47333.gif)

To learn more, run `release-plz update --help`.

## Preview and check pending releases

Run `release-plz update --dry-run` to print the planned releases without updating
manifests, changelogs, or dependencies. It exits successfully when the analysis succeeds,
even if releases are pending.

For a CI check, use `release-plz update --check`. This also leaves release files unchanged
and exits with status 1 when releases are pending, or 0 when no releases are pending.
Analysis errors also return a nonzero status; read the error output to distinguish failures.

Both modes still inspect the project and may download published packages for comparison.
They are supported by `update` only.
