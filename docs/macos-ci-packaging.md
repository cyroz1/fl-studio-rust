# macOS CI packaging

The macOS CI job runs formatting, Clippy, tests, and a packaged-app launch smoke check. CI uploads installer files only: the Ubuntu `.deb`, the Windows setup `.exe`, and a macOS `.dmg` only after it has been signed and notarized. If Apple signing secrets are absent, CI still builds and inspects an unsigned app for its smoke check, but it does not upload a macOS artifact. This prevents an unsigned app from being mistaken for a Gatekeeper-ready installer.

Gatekeeper may report a quarantined, unsigned app as “damaged.” A Developer ID Application signature and Apple notarization are required for a normal download that Gatekeeper can verify. Removing quarantine is only a local testing workaround for an app build you trust; it does not make the app verified:

```sh
xattr -dr com.apple.quarantine "/Applications/FL Studio Rebuild.app"
open "/Applications/FL Studio Rebuild.app"
```

Configure these repository Actions secrets under **Settings → Secrets and variables → Actions** to publish the signed macOS installer:

| Secret | Value |
|---|---|
| `APPLE_CERTIFICATE` | Base64-encoded Developer ID Application `.p12`, including its private key |
| `APPLE_CERTIFICATE_PASSWORD` | Password used when exporting the `.p12` |
| `APPLE_SIGNING_IDENTITY` | Full identity name, such as `Developer ID Application: Example Team (ABCDE12345)` |
| `APPLE_API_KEY` | App Store Connect API key ID |
| `APPLE_API_ISSUER` | App Store Connect issuer ID |
| `APPLE_API_KEY_CONTENT` | Contents of the API key `.p8` file |

With all six secrets configured, CI signs and notarizes the app, validates its signature and stapled ticket, checks Gatekeeper assessment, and uploads only the `.dmg` as `installers-macos-latest`. A partial set of secrets fails the job so a misconfigured package cannot be mistaken for a verified installer. Apple describes the Developer ID requirement for software distributed outside the Mac App Store in its [Gatekeeper signing guidance](https://developer.apple.com/developer-id/).
