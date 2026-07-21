# Windows signing with Azure Artifact Signing

The release workflow builds separate MSVC `x86_64` and `arm64` executables. Azure Artifact Signing
(formerly Trusted Signing) is prepared behind the repository variable
`AZURE_TRUSTED_SIGNING_ENABLED`. When it is not exactly `true`, the release publishes unsigned
Windows executables; all other release validation remains mandatory.

## Azure setup

Create a Public Trust signing account and certificate profile, then grant the GitHub federated
identity the **Artifact Signing Certificate Profile Signer** role. Configure GitHub OIDC for this
repository's `release` environment; no client secret is required. The environment restricts
deployments to release tags but does not require manual approval.

Repository secrets:

- `AZURE_CLIENT_ID`
- `AZURE_TENANT_ID`
- `AZURE_SUBSCRIPTION_ID`

Repository variables:

- `AZURE_TRUSTED_SIGNING_ENABLED=true`
- `AZURE_ARTIFACT_SIGNING_ENDPOINT`
- `AZURE_ARTIFACT_SIGNING_ACCOUNT_NAME`
- `AZURE_ARTIFACT_SIGNING_CERTIFICATE_PROFILE_NAME`

The signing job has `id-token: write`, authenticates with `azure/login`, signs both architecture
artifacts with SHA-256 and an RFC 3161 timestamp, and requires `Get-AuthenticodeSignature` to report
`Valid`. Signing happens before checksums, attestations, and publication.

Both executables use the static MSVC C runtime so each release asset remains a single binary. The
packager runs `dumpbin /dependents` and rejects VC/UCRT redistributable DLL imports before an
unsigned file can reach the signing job.

## Local verification

On Windows, verify a downloaded release with:

```powershell
Get-AuthenticodeSignature .\snapdog-os-installer-*-windows-*.exe | Format-List
```

The expected status is `Valid` when signing is enabled for that release.
