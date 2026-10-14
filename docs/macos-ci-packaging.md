# macOS CI packaging

The macOS CI job runs formatting, Clippy, tests, and a launch smoke check of the packaged app. It builds the app and a disk image, verifies the image with `hdiutil`, mounts it to confirm it contains the app bundle, and checks the app's executable and `.flp` file association. CI uploads one artifact, `installers-macos-latest`, containing the `.dmg`.

When signing credentials are unavailable, the disk image is unsigned. It is suitable for CI and local packaging checks but is not Gatekeeper verified. With signing credentials, CI signs the app with a Developer ID Application certificate, notarizes it with Apple, checks the signature and stapled ticket, and uploads the verified disk image under the same artifact name. Apple describes the Developer ID requirement for distribution outside the Mac App Store in its [Gatekeeper signing guidance](https://developer.apple.com/developer-id/). The packaged app registers `.flp` project files and opens a project passed as its launch argument.

Add these repository Actions secrets under **Settings → Secrets and variables → Actions**:

| Secret | Value |
|---|---|
| `APPLE_CERTIFICATE` | Base64-encoded Developer ID Application `.p12`, including its private key |
| `APPLE_CERTIFICATE_PASSWORD` | Password used when exporting the `.p12` |
| `APPLE_SIGNING_IDENTITY` | Full identity name, such as `Developer ID Application: Example Team (ABCDE12345)` |
| `APPLE_API_KEY` | App Store Connect API key ID |
| `APPLE_API_ISSUER` | App Store Connect issuer ID |
| `APPLE_API_KEY_CONTENT` | Contents of the API key `.p8` file |

The workflow writes the API key and a temporary signed Packager config into the runner's workspace. Temporary signing files are removed after the job. If no secrets are configured, macOS tests and packaging still run and CI uploads only the unsigned installer artifact. A partial set of secrets fails the job so a misconfigured package cannot be mistaken for a verified one.
