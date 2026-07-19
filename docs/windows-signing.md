# Windows signing preparation

Release binaries will use Microsoft Azure Artifact Signing (formerly Trusted Signing) with a
Public Trust certificate profile. Local development binaries remain unsigned until the Azure
resources are provisioned.

The future release workflow should authenticate through GitHub OIDC, not a long-lived client
secret, and use the official `azure/artifact-signing-action` on an x86-64 Windows signing job.
Both architecture-specific executables can be collected and signed in that job.

Required configuration:

- Azure tenant, subscription, and application/client IDs
- Artifact Signing endpoint
- Signing account name
- Certificate profile name
- `Artifact Signing Certificate Profile Signer` role on the federated identity

Signing must happen before checksums, SBOM metadata, provenance, and release upload are generated.
