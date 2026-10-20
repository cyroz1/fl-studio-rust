# macOS CI packaging

Each platform job uploads exactly one installer artifact: a Linux AppImage, a Windows setup executable, or a macOS disk image (`.dmg`). CI installs and launches each packaged app before upload. The macOS job signs and notarizes both the app and disk image, validates their stapled tickets, and checks Gatekeeper acceptance for the disk image and installed app. App bundles, screenshots, and intermediate files are not uploaded.

The macOS job requires all six Apple signing and notarization secrets below. It fails before packaging if any secret is missing, so CI never publishes an unsigned app that Gatekeeper could label “damaged.” Add the secrets under **Settings → Secrets and variables → Actions**:

| Secret | Value |
|---|---|
| `APPLE_CERTIFICATE` | Base64-encoded Developer ID Application `.p12`, including its private key |
| `APPLE_CERTIFICATE_PASSWORD` | Password used when exporting the `.p12` |
| `APPLE_SIGNING_IDENTITY` | Full identity name, such as `Developer ID Application: Example Team (ABCDE12345)` |
| `APPLE_API_KEY` | App Store Connect API key ID |
| `APPLE_API_ISSUER` | App Store Connect issuer ID |
| `APPLE_API_KEY_CONTENT` | Contents of the API key `.p8` file |

CI imports the Developer ID certificate into a temporary keychain, signs and notarizes the app and disk image, installs the app from the DMG, and verifies the signature, stapled ticket, and Gatekeeper assessment. It removes the temporary keychain, certificate, API key, and signed Packager configuration after the job. Apple documents the Developer ID requirement for software distributed outside the Mac App Store in its [Gatekeeper signing guidance](https://developer.apple.com/developer-id/).
