# KeyDock

KeyDock automatically switches your Windows keyboard layouts when you connect or disconnect an external keyboard. It runs in the notification area for the current user.

| Laptop only | External US keyboard connected |
| --- | --- |
| Russian + Spanish | Russian + US English |

Set the layouts to keep in each mode. Optionally choose a layout for new windows and request a switch in the app you are typing in.

## Get started

1. Choose the x64 MSI or portable ZIP from [GitHub Releases](https://github.com/wiresock/KeyDock/releases). For the MSI, run the installer and launch **KeyDock** from Start. For the portable version, extract the ZIP to a permanent folder and run `keydock.exe`.
2. In **Settings**, check that your laptop keyboard is marked **Built-in**. KeyDock detects USB and Bluetooth keyboards, groups composite devices, and lets you override classification with **Built-in**, **External**, or **Ignore**.
3. In **Laptop only** and **External keyboard connected**, select the layouts to keep. For the external profile, choose any external keyboard or a specific set of keyboards. Click **Apply** to test the result.
4. Open Settings later from the tray icon. The tray also offers **Re-apply now**, **Paused**, and access to the configuration and log. Enable **Start with Windows** in Settings if you want KeyDock to launch at sign-in.

A layout becomes managed when you save it in a profile. KeyDock can remove a managed layout when another profile is active. A mode left unconfigured stays absent when you save unrelated options.

**Update:** Run a newer MSI over the installed copy. For portable use, exit KeyDock and replace the executable. **Uninstall:** Use Windows **Installed apps** for the MSI. For portable use, disable **Start with Windows**, exit KeyDock and remove its folder. Both methods leave your configuration in `%APPDATA%\KeyDock`.

Verify downloads against the release's `SHA256SUMS.txt` using `Get-FileHash -Algorithm SHA256`. An unsigned download has no Authenticode publisher identity, and Windows may show a publisher or reputation warning.

## Compatibility

Windows 10/11

## Build from source

On Windows, install Rust 1.88 or newer, Visual Studio Build Tools with **Desktop development with C++** and a Windows SDK, and a .NET SDK supported by WiX 6. WiX 6 is restored from NuGet.

```powershell
cargo test --locked
rustup target add x86_64-pc-windows-msvc
cargo build --release --locked --target x86_64-pc-windows-msvc
.\installer\build.ps1 -Architecture x64 -SkipCargo
```

For ARM64, install the MSVC ARM64 tools and use target `aarch64-pc-windows-msvc` and installer architecture `arm64`.

## License

KeyDock is licensed under the [MIT License](LICENSE). See the [third-party notices](THIRD_PARTY_NOTICES.md) for bundled component terms.

See [Contributing](CONTRIBUTING.md), [reporting security issues](SECURITY.md), and the [changelog](CHANGELOG.md).
