# Preparing a KeyDock release

## Checks and artifacts

CI runs Rust formatting, native tests, Clippy and the manifest's minimum Rust version on a Windows runner. The package matrix builds the x64 and ARM64 executables and uses WiX 6 through `installer/build.ps1`. The MSI database's ProductVersion and architecture template and the portable ZIP and its checksums are verified before upload.

The WiX project runs ICE validation with three documented exceptions: ICE38, ICE43 and ICE57 flag the all-users Start menu shortcut and its HKLM key path in this per-machine package. Other ICE errors fail the build. An error accessing Windows Installer requires a capable validation host.

GitHub Actions release tags and manual **build** runs prepare artifacts. Each architecture has an MSI, a portable ZIP, an MSI notices ZIP, `SHA256SUMS.txt` and build provenance. Retain the exact binaries you validate on the target Windows desktop. A new build can have a different hash even at the same source revision. ARM64 compilation and packaging establish build coverage, with native release testing still required.

## Signing

The verified initial x64 candidate is unsigned. When signing is available, use a protected signing service or an isolated signing host with restricted access. Sign and timestamp the executable before packaging it. Sign and timestamp the MSI after WiX builds it. Verify both with `signtool verify /pa /v`, verify the executable embedded in the MSI, and regenerate the portable ZIP and all checksums from the signed files. Validate the final downloads. The current automated promotion uses the verified outputs of its candidate run. Promote offline-signed files through an owner-controlled draft release after the same validation.

## Repository and owner approval

Create a clean GitHub repository from the single-commit source snapshot. Keep development refs, pull requests, issues and old Git objects in the private repository. Confirm the destination and check its branches, tags, issues, pull requests and releases before enabling visibility. Use `main` as default and require the **Rust checks**, **Package (x64)** and **Package (arm64)** CI checks. Protect `main` from force pushes and deletion, prefer linear history, and protect version tags. Enable issues, Dependabot security updates and private vulnerability reporting. Discussions are optional.

Use the description “Automatically switch Windows keyboard layouts when docking or connecting external keyboards.” Suggested topics are `windows`, `rust`, `keyboard-layout`, `keyboard`, `desktop-utility`, `hid` and `productivity`. The existing KeyDock icon is project branding. A screenshot should come from a real Windows desktop.

KeyDock is MIT-licensed. Include the MIT license, runtime notices and WiX notices described in [THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md). Refresh the crate notices when dependency versions change. The packager takes the standard-library notices from the build toolchain's Rust documentation. Confirm the Windows release gates on the candidate and obtain the owner's approval to switch the clean repository to public.

## Publish a verified x64 candidate

The first release can use the preserved, Windows-validated MSI and executable. Run `installer/package-release.ps1` with `-ValidatedMsi` and `-KeyDockExe` referencing the verified pair. Confirm their recorded hashes and select the original binary's Rust toolchain for its notices. Publish that verified bundle through an owner-controlled draft after all release gates pass. The automated path below applies to artifacts built by a successful candidate run.

1. Configure a `public-release` GitHub environment restricted to version tags and require review where available. Set its `KEYDOCK_APPROVED_RELEASE_SHA` variable only after owner approval for the exact commit.
2. Create the intended version tag on the approved `main` commit. Confirm the **Release** build run succeeds, and validate the exact x64 MSI and ZIP that run produced. Record the run ID and SHA-256 of its `SHA256SUMS.txt`.
3. Manually dispatch **Release** on the matching tag with operation `promote`, the candidate run ID and approved manifest hash. Promotion validates the tag, commit, license and successful build run, downloads the approved artifacts, and verifies checksums again. It uploads a draft and makes it public only after the assets have been attached.
4. Check the public downloads and release notes, and clear the approval variable.

References: [GitHub environments](https://docs.github.com/en/actions/how-tos/deploy/configure-and-manage-deployments/manage-environments), [repository visibility](https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/managing-repository-settings/setting-repository-visibility), and [WiX validation](https://docs.firegiant.com/wix/tools/validation/).
