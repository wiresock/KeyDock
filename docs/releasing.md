# Preparing a KeyDock release

## Version PRs

All source changes go through pull requests to protected `main`. Use squash merge after the three required PR checks pass and review conversations are resolved.

1. Complete and test the implementation.
2. Increase the package version in `Cargo.toml` to `X.Y.Z` and refresh `Cargo.lock`. Running `cargo check` refreshes the root package entry. Use `cargo check --locked` to confirm the result.
3. Add `docs/releases/X.Y.Z.md` with user-visible changes, installation instructions, compatibility, and limitations.
4. Open a PR, wait for CI, then squash merge it. The merge authorizes that version for automatic publication after its own checks.

PR CI compares the proposed package version with the PR base. On a main push, Release compares the push event's before and after revisions. An unchanged version runs CI without creating a tag or release. An increase builds both architectures and publishes x64 from the verified merge commit. ARM64 packaging establishes build coverage. Native ARM64 validation is a separate requirement for promoting ARM64 downloads.

## Build and publication

The main workflow reuses the PR checks for the exact merged revision. The x64 publish job uses the artifact from that run, checks build provenance, MSI version and architecture, and verifies the checksum manifest. The WiX package runs ICE validation with the existing ICE38, ICE43 and ICE57 exclusions for the per-machine Start menu shortcut.

A manual Release workflow run builds candidates without publishing. Use this to check the package and metadata pipeline. The publish job runs only on the protected main push after successful checks and verifies that the selected commit belongs to main and is a merged PR. It creates an annotated `vX.Y.Z` tag and uploads the x64 MSI, portable ZIP, notices ZIP and SHA-256 manifest to a draft before publishing. GitHub's built-in token supplies release access only to that job.

For a failure after tagging, rerun the failed publish job in the same run while its artifact is retained. A retry reuses the correct tag and completed uploads. A different tag target, metadata or uploaded checksum fails the job. Keep the original candidate files when diagnosing a mismatch.

## Bootstrap v0.3.12

The owner approves the first release separately. Use the preserved unsigned Windows-tested x64 MSI and executable. Their recorded SHA-256 values identify the release inputs. The packager supports `-ValidatedMsi` and `-KeyDockExe` to bundle those files with the notices. Use the original compiler toolchain for the standard-library notices.

After owner approval, verify the original binary hashes, prepare an owner-controlled draft at the chosen source revision, and attach the verified x64 package, portable ZIP, notices ZIP and checksum manifest. Use `docs/releases/0.3.12.md` as the description. Publish the bootstrap release before merging a future release-version PR. The automatic publisher requires the first release to be public.


- Verified x64 MSI SHA-256: `503E98511C840BADEB9A8B60F41AB3EB2B65D4DEC8CA1BBE12624653B54E1080`
- Verified portable executable SHA-256: `387150232843900749DE453FEE1358F74BC59B71D8EE90F1F24D4B4DF0E50416`

## Signing

The binaries can be released unsigned. If signing is introduced, use a protected signing service or isolated signing host. Sign and timestamp the executable before packaging it and sign and timestamp the MSI after WiX builds it. Verify both signatures and regenerate the portable ZIP and checksums before validating the final downloads.

See [third-party notices](../THIRD_PARTY_NOTICES.md) and [contributing](../CONTRIBUTING.md).
