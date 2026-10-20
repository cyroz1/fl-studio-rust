# macOS CI packaging

The macOS artifact is a Developer ID signed, notarized, and stapled DMG. CI imports the Developer ID certificate into a temporary keychain, builds and verifies the app bundle, signs and notarizes the disk image, checks Gatekeeper acceptance, then installs and launches the app from the DMG. CI uploads only the verified DMG. Without all six Apple signing and notarization secrets, the macOS matrix job fails with setup instructions instead of publishing an unsigned app that Gatekeeper may label “damaged.”

The Linux runner installs the Debian package and launches the AppImage; Windows installs and launches its setup executable. CI uploads exactly one installer per platform: the Linux AppImage, the Windows setup executable, or the signed macOS DMG. Screenshots, additional package formats, and unsigned macOS previews are not uploaded as CI artifacts.

Apple requires Developer ID signing for software distributed outside the Mac App Store; see [Apple's Gatekeeper signing guidance](https://developer.apple.com/developer-id/). Add these repository Actions secrets under Settings → Secrets and variables → Actions:

| Secret | Value |
|---|---|
| APPLE_CERTIFICATE | Base64-encoded Developer ID Application .p12, including its private key |
| APPLE_CERTIFICATE_PASSWORD | Password used when exporting the .p12 |
| APPLE_SIGNING_IDENTITY | Full identity name, such as Developer ID Application: Example Team (ABCDE12345) |
| APPLE_API_KEY | App Store Connect API key ID |
| APPLE_API_ISSUER | App Store Connect issuer ID |
| APPLE_API_KEY_CONTENT | Contents of the API key .p8 file |

The workflow removes the temporary keychain, certificate, API key, and signed Packager configuration after the macOS job.
