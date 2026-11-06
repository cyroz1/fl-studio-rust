# macOS CI packaging

The macOS job runs on Apple Silicon and always uploads one disk image (`.dmg`). When all six Apple signing secrets are configured, CI signs and notarizes it, installs and launches the app from the image, and validates its stapled tickets and Gatekeeper acceptance. With no signing secrets, CI ad-hoc signs the app (codesign identity "-"), launches it locally as a smoke check, verifies the bundle signature, and uploads the ad-hoc signed disk image. An ad-hoc signed download is not notarized, so Gatekeeper still asks for an explicit Open Anyway on first launch, but it no longer reports the app as damaged the way a fully unsigned Apple Silicon bundle is. App bundles, screenshots, and intermediate files are not uploaded.

Signed macOS distribution requires all six Apple signing and notarization secrets below. With no secrets configured, CI publishes an ad-hoc signed Apple Silicon disk image after a local app smoke check. A partially configured set fails before packaging so misconfigured signing cannot silently fall back to an ad-hoc build. Add the secrets under **Settings → Secrets and variables → Actions**:

| Secret | Value |
|---|---|
| `APPLE_CERTIFICATE` | Base64-encoded Developer ID Application `.p12`, including its private key |
| `APPLE_CERTIFICATE_PASSWORD` | Password used when exporting the `.p12` |
| `APPLE_SIGNING_IDENTITY` | Full identity name, such as `Developer ID Application: Example Team (ABCDE12345)` |
| `APPLE_API_KEY` | App Store Connect API key ID |
| `APPLE_API_ISSUER` | App Store Connect issuer ID |
| `APPLE_API_KEY_CONTENT` | Contents of the API key `.p8` file |

CI imports the Developer ID certificate into a temporary keychain, signs and notarizes the app and disk image, installs the app from the DMG, and verifies the signature, stapled ticket, and Gatekeeper assessment. It removes the temporary keychain, certificate, API key, and signed Packager configuration after the job. Apple documents the Developer ID requirement for software distributed outside the Mac App Store in its [Gatekeeper signing guidance](https://developer.apple.com/developer-id/).

Signing never runs for pull requests. Every step that reads the Apple secrets, writes the certificate or `.p8` key, or uses the signing keychain is gated on `github.event_name != 'pull_request'`, so a pull request build (including one from a fork, where the secrets are unavailable anyway) always takes the secretless ad-hoc smoke path: no signing material is written to the runner before repo-controlled build hooks and the packaged app execute. Signing and notarization run on pushes to `main` and manual `workflow_dispatch` runs. All third-party action references in the workflow are pinned to full commit SHAs.
