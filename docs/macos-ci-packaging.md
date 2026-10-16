# macOS CI packaging

The macOS CI job runs formatting, Clippy, tests, and a launch smoke check of the packaged app. It checks the app executable and `.flp` file association. CI uploads a macOS artifact only when it has signed the app with a Developer ID certificate and completed Apple notarization; the artifact contains only the verified `.dmg` installer. Without signing credentials, CI builds and smoke-tests an unsigned `.app` but uploads no macOS artifact.

Gatekeeper may report a quarantined, unsigned app as “damaged.” Removing quarantine is only a local testing workaround for an app build you trust; it does not make the app Gatekeeper verified. For an app already copied into Applications, run:

```sh
xattr -dr com.apple.quarantine "/Applications/FL Studio Rebuild.app"
open "/Applications/FL Studio Rebuild.app"
```

With signing credentials, CI signs the app with a Developer ID Application certificate, notarizes it with Apple, checks its signature and stapled ticket, and assesses it with Gatekeeper before uploading the `.dmg`. Apple describes the Developer ID requirement for software distributed outside the Mac App Store in its [Gatekeeper signing guidance](https://developer.apple.com/developer-id/). The packaged app registers `.flp` project files and opens a project passed as its launch argument.

Add these repository Actions secrets under **Settings → Secrets and variables → Actions**:

| Secret | Value |
|---|---|
| `APPLE_CERTIFICATE` | Base64-encoded Developer ID Application `.p12`, including its private key |
| `APPLE_CERTIFICATE_PASSWORD` | Password used when exporting the `.p12` |
| `APPLE_SIGNING_IDENTITY` | Full identity name, such as `Developer ID Application: Example Team (ABCDE12345)` |
| `APPLE_API_KEY` | App Store Connect API key ID |
| `APPLE_API_ISSUER` | App Store Connect issuer ID |
| `APPLE_API_KEY_CONTENT` | Contents of the API key `.p8` file |

The workflow writes the API key and a temporary signed Packager config into the runner's workspace. Temporary signing files are removed after the job. If no secrets are configured, macOS tests and the unsigned app smoke check still run, but no macOS artifact is uploaded. A partial set of secrets fails the job so a misconfigured package cannot be mistaken for a verified one.
