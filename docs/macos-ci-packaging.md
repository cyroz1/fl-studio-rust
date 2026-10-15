# macOS CI packaging

The macOS CI job always runs formatting, Clippy, tests, and a launch smoke check of the packaged app. It uploads only the macOS disk image installer. With signing credentials, CI signs and notarizes the app before packaging the disk image. Without signing credentials, CI builds an unsigned `.app` and `.dmg`, checks that the app executable and `.flp` file association are present, and uploads the unsigned disk image. Gatekeeper may block an unsigned, quarantined app; configure signing and notarization for a verified downloadable installer. Apple describes the Developer ID requirement for software distributed outside the Mac App Store in its [Gatekeeper signing guidance](https://developer.apple.com/developer-id/). The packaged app registers `.flp` project files and opens a project passed as its launch argument.

Add these repository Actions secrets under **Settings → Secrets and variables → Actions**:

| Secret | Value |
|---|---|
| `APPLE_CERTIFICATE` | Base64-encoded Developer ID Application `.p12`, including its private key |
| `APPLE_CERTIFICATE_PASSWORD` | Password used when exporting the `.p12` |
| `APPLE_SIGNING_IDENTITY` | Full identity name, such as `Developer ID Application: Example Team (ABCDE12345)` |
| `APPLE_API_KEY` | App Store Connect API key ID |
| `APPLE_API_ISSUER` | App Store Connect issuer ID |
| `APPLE_API_KEY_CONTENT` | Contents of the API key `.p8` file |

The workflow writes the API key and a temporary signed Packager config into the runner's workspace, signs and notarizes the app, checks the signature and stapled ticket with Apple's tools, and uploads the `.dmg`. Temporary signing files are removed after the job. If no secrets are configured, macOS tests still run and CI uploads only the unsigned `.dmg`. A partial set of secrets fails the job so a misconfigured package cannot be mistaken for a verified one.
