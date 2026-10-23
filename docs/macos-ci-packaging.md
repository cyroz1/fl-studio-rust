# macOS CI packaging

The macOS job runs on Apple Silicon and always uploads one disk image (`.dmg`). When all six Apple signing secrets are configured, CI signs and notarizes it, installs and launches the app from the image, and validates its stapled tickets and Gatekeeper acceptance. With no signing secrets, CI launches the unsigned app locally as a smoke check and uploads the unsigned disk image. App bundles, screenshots, and intermediate files are not uploaded.

Signed macOS distribution requires all six Apple signing and notarization secrets below. With no secrets configured, CI publishes an unsigned Apple Silicon disk image after a local app smoke check. A partially configured set fails before packaging so misconfigured signing cannot silently fall back to an unsigned build. Add the secrets under **Settings → Secrets and variables → Actions**:

| Secret | Value |
|---|---|
| `APPLE_CERTIFICATE` | Base64-encoded Developer ID Application `.p12`, including its private key |
| `APPLE_CERTIFICATE_PASSWORD` | Password used when exporting the `.p12` |
| `APPLE_SIGNING_IDENTITY` | Full identity name, such as `Developer ID Application: Example Team (ABCDE12345)` |
| `APPLE_API_KEY` | App Store Connect API key ID |
| `APPLE_API_ISSUER` | App Store Connect issuer ID |
| `APPLE_API_KEY_CONTENT` | Contents of the API key `.p8` file |

CI imports the Developer ID certificate into a temporary keychain, signs and notarizes the app and disk image, installs the app from the DMG, and verifies the signature, stapled ticket, and Gatekeeper assessment. It removes the temporary keychain, certificate, API key, and signed Packager configuration after the job. Apple documents the Developer ID requirement for software distributed outside the Mac App Store in its [Gatekeeper signing guidance](https://developer.apple.com/developer-id/).
