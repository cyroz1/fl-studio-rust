# macOS CI packaging

The macOS CI job runs formatting, Clippy, tests, and a launch smoke check of the packaged app. It builds the app and a disk image, verifies the image with `hdiutil`, mounts it to confirm it contains the app bundle, and checks the app executable and `.flp` file association. CI uploads one artifact, `installers-macos-latest`, containing only the `.dmg` installer.

When signing credentials are unavailable, the disk image is unsigned. After downloading this trusted CI build, macOS may block the app or report that it is damaged because the quarantined app has no Developer ID signature. For local testing, mount the DMG, drag `FL Studio Rebuild.app` to Applications, then remove quarantine from that app in Terminal and open it:

```sh
xattr -dr com.apple.quarantine "/Applications/FL Studio Rebuild.app"
open "/Applications/FL Studio Rebuild.app"
```

This workaround is for local testing of a trusted CI artifact; it does not make the app Gatekeeper verified. With signing credentials, CI signs the app with a Developer ID Application certificate, notarizes it with Apple, checks its signature and stapled ticket, and uploads the verified disk image under the same artifact name. Apple describes the Developer ID requirement for software distributed outside the Mac App Store in its [Gatekeeper signing guidance](https://developer.apple.com/developer-id/). The packaged app registers `.flp` project files and opens a project passed as its launch argument.

Add these repository Actions secrets under **Settings → Secrets and variables → Actions**:

| Secret | Value |
|---|---|
| `APPLE_CERTIFICATE` | Base64-encoded Developer ID Application `.p12`, including its private key |
| `APPLE_CERTIFICATE_PASSWORD` | Password used when exporting the `.p12` |
| `APPLE_SIGNING_IDENTITY` | Full identity name, such as `Developer ID Application: Example Team (ABCDE12345)` |
| `APPLE_API_KEY` | App Store Connect API key ID |
| `APPLE_API_ISSUER` | App Store Connect issuer ID |
| `APPLE_API_KEY_CONTENT` | Contents of the API key `.p8` file |

The workflow writes the API key and a temporary signed Packager config into the runner's workspace. Temporary signing files are removed after the job. If no secrets are configured, macOS tests and packaging still run and CI uploads only the unsigned installer. A partial set of secrets fails the job so a misconfigured package cannot be mistaken for a verified one.
