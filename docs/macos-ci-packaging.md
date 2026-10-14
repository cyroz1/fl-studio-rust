# macOS CI packaging

The macOS CI job always runs formatting, Clippy, tests, and a launch smoke check of the packaged app. It uploads a separate `macos-native-gui-preview` artifact with a screenshot of the native startup window. Without signing credentials, it builds an unsigned `.app` and `.dmg`, verifies that the app executable and `.flp` file association are present, checks the disk image, and confirms the mounted image contains the app bundle. The `macos-unsigned-dmg-preview` artifact provides the disk image for CI and local packaging checks; it is unsigned and not Gatekeeper verified. CI also uploads a separate `macos-unsigned-gui-preview` artifact for local UI review. The `.app` is inside a tar archive because GitHub Actions ZIP artifacts reset file permissions and can strip the app executable bit ([`actions/upload-artifact` permission behavior](https://github.com/actions/upload-artifact#permission-loss)). CI verifies that the executable bit survives the tar archive. Gatekeeper may report that an unsigned, quarantined preview app is “damaged.” Extract the GUI preview artifact once; `README-FIRST.txt` and the app archive are together in the extracted folder. To open a preview from this trusted repository, open Terminal, change to that folder, and run:

```sh
tar -xzf "FL Studio Rebuild.app.tar.gz"
xattr -dr com.apple.quarantine "FL Studio Rebuild.app"
open "FL Studio Rebuild.app"
```

For a downloadable build that Gatekeeper can verify, CI creates and uploads a signed and notarized disk image when it can sign the app with a Developer ID Application certificate and notarize it with Apple. Apple describes the Developer ID requirement for software distributed outside the Mac App Store in its [Gatekeeper signing guidance](https://developer.apple.com/developer-id/). The packaged app registers `.flp` project files and opens a project passed as its launch argument.

Add these repository Actions secrets under **Settings → Secrets and variables → Actions**:

| Secret | Value |
|---|---|
| `APPLE_CERTIFICATE` | Base64-encoded Developer ID Application `.p12`, including its private key |
| `APPLE_CERTIFICATE_PASSWORD` | Password used when exporting the `.p12` |
| `APPLE_SIGNING_IDENTITY` | Full identity name, such as `Developer ID Application: Example Team (ABCDE12345)` |
| `APPLE_API_KEY` | App Store Connect API key ID |
| `APPLE_API_ISSUER` | App Store Connect issuer ID |
| `APPLE_API_KEY_CONTENT` | Contents of the API key `.p8` file |

The workflow writes the API key and a temporary signed Packager config into the runner's workspace, signs and notarizes the app, checks the signature and stapled ticket with Apple's tools, and uploads the signed `.dmg`. Temporary signing files are removed after the job. If no secrets are configured, macOS tests and packaging still run and CI uploads clearly labeled unsigned disk image and GUI preview artifacts. A partial set of secrets fails the job so a misconfigured package cannot be mistaken for a verified one.
