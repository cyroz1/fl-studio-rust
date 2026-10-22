# macOS CI packaging

Linux and Windows jobs upload their installer artifacts. When all six Apple signing secrets are configured, the macOS job uploads one signed and notarized disk image (`.dmg`) after installing and launching the app and validating its stapled tickets and Gatekeeper acceptance. When none of the Apple signing secrets are configured, CI builds and launches the unsigned app locally as a smoke check but does not upload a macOS artifact. App bundles, screenshots, and intermediate files are not uploaded.

Signed macOS distribution requires all six Apple signing and notarization secrets below. With no secrets configured, CI skips signing and only runs a local smoke check. A partially configured set fails before packaging so misconfigured signing cannot silently fall back to an unsigned build. Add the secrets under **Settings → Secrets and variables → Actions**:

| Secret | Value |
|---|---|
| `APPLE_CERTIFICATE` | Base64-encoded Developer ID Application `.p12`, including its private key |
| `APPLE_CERTIFICATE_PASSWORD` | Password used when exporting the `.p12` |
| `APPLE_SIGNING_IDENTITY` | Full identity name, such as `Developer ID Application: Example Team (ABCDE12345)` |
| `APPLE_API_KEY` | App Store Connect API key ID |
| `APPLE_API_ISSUER` | App Store Connect issuer ID |
| `APPLE_API_KEY_CONTENT` | Contents of the API key `.p8` file |

CI imports the Developer ID certificate into a temporary keychain, signs and notarizes the app and disk image, installs the app from the DMG, and verifies the signature, stapled ticket, and Gatekeeper assessment. It removes the temporary keychain, certificate, API key, and signed Packager configuration after the job. Apple documents the Developer ID requirement for software distributed outside the Mac App Store in its [Gatekeeper signing guidance](https://developer.apple.com/developer-id/).
