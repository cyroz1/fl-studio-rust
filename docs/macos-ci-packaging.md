# macOS CI packaging

The macOS CI job always runs formatting, Clippy, and tests. Without signing credentials, it builds an unsigned `.app`, checks that its executable and `.flp` file association are present, and uploads a short-lived `macos-unsigned-gui-preview` artifact for local UI testing. This artifact is not a distributable installer and may be blocked by Gatekeeper. To open a preview from this trusted repository, download and extract the artifact, extract the included `fl-studio-rebuild-macos-unsigned-preview.zip`, then run:

```sh
xattr -dr com.apple.quarantine "FL Studio Rebuild.app"
open "FL Studio Rebuild.app"
```

For a normal downloadable build that Gatekeeper can verify, CI creates and uploads a disk image only when it can sign the app with a Developer ID Application certificate and notarize it with Apple. Apple describes the Developer ID requirement for software distributed outside the Mac App Store in its [Gatekeeper signing guidance](https://developer.apple.com/developer-id/). The packaged app registers `.flp` project files and opens a project passed as its launch argument.

Add these repository Actions secrets under **Settings → Secrets and variables → Actions**:

| Secret | Value |
|---|---|
| `APPLE_CERTIFICATE` | Base64-encoded Developer ID Application `.p12`, including its private key |
| `APPLE_CERTIFICATE_PASSWORD` | Password used when exporting the `.p12` |
| `APPLE_SIGNING_IDENTITY` | Full identity name, such as `Developer ID Application: Example Team (ABCDE12345)` |
| `APPLE_API_KEY` | App Store Connect API key ID |
| `APPLE_API_ISSUER` | App Store Connect issuer ID |
| `APPLE_API_KEY_CONTENT` | Contents of the API key `.p8` file |

The workflow writes the API key and a temporary signed Packager config into the runner's workspace, signs and notarizes the app, checks the signature and stapled ticket with Apple's tools, and uploads only the `.dmg`. Temporary signing files are removed after the job. If no secrets are configured, macOS tests still run and CI uploads only the clearly labeled unsigned GUI preview. A partial set of secrets fails the job so a misconfigured package cannot be mistaken for a verified one.
