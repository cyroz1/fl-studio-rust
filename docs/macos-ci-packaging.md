# macOS CI packaging

The macOS CI job always runs formatting, Clippy, and tests. It only creates or uploads a macOS installer when it can sign the app with a Developer ID Application certificate and notarize it with Apple. This prevents CI from publishing an unsigned bundle that macOS reports as damaged.

Add these repository Actions secrets under **Settings → Secrets and variables → Actions**:

| Secret | Value |
|---|---|
| `APPLE_CERTIFICATE` | Base64-encoded Developer ID Application `.p12`, including its private key |
| `APPLE_CERTIFICATE_PASSWORD` | Password used when exporting the `.p12` |
| `APPLE_SIGNING_IDENTITY` | Full identity name, such as `Developer ID Application: Example Team (ABCDE12345)` |
| `APPLE_API_KEY` | App Store Connect API key ID |
| `APPLE_API_ISSUER` | App Store Connect issuer ID |
| `APPLE_API_KEY_CONTENT` | Contents of the API key `.p8` file |

The workflow writes the API key and a temporary signed Packager config into the runner's workspace, signs and notarizes the app, checks the signature and stapled ticket with Apple's tools, and uploads only the `.dmg`. Temporary signing files are removed after the job. If no secrets are configured, macOS tests still run and the job summary explains why the installer was skipped. A partial set of secrets fails the job so a misconfigured package cannot be mistaken for a verified one.
