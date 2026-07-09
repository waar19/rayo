# Code Signing Decision for Rayo

## Objective

Reduce SmartScreen warnings, speed up user trust, and reduce manual friction in Winget moderation.

## Options Reviewed

### 1) Azure Trusted Signing

- Estimated cost: low monthly subscription.
- Setup effort: medium (Azure identity + GitHub OIDC + release workflow step).
- Benefits:
  - Authenticode signatures from Microsoft-trusted chain.
  - Better SmartScreen reputation growth than unsigned binaries.
  - Works well with CI/CD and automated releases.
- Risks:
  - Requires Azure subscription and one-time setup.
  - Requires secure secret/identity management.

### 2) OV Code Signing Certificate (traditional CA)

- Estimated cost: medium annual certificate cost.
- Setup effort: medium-high (certificate purchase, validation, secure key handling).
- Benefits:
  - Industry-standard signing approach.
- Risks:
  - More operational overhead for certificate lifecycle and key protection.

### 3) No Signing

- Estimated cost: none.
- Setup effort: none.
- Benefits:
  - No platform cost.
- Risks:
  - Frequent SmartScreen warnings.
  - Slower trust/adoption, more user support friction.

## Decision

Recommended path: **Azure Trusted Signing** for next release cycle.

Reason:
- Lowest friction path for automated signing in current GitHub-based release pipeline.
- Best tradeoff between user trust, operational cost, and long-term distribution quality.

## Implementation Status

- Decision documented and approved as technical recommendation.
- Workflow integration intentionally deferred until Azure tenant, profile, and OIDC permissions are confirmed.

## Required Inputs to Enable in CI

- Azure tenant and subscription ready.
- Trusted Signing account + certificate profile created.
- GitHub Actions OIDC trust configured.
- Repository secrets/variables for signing profile identifiers.

## Integration Plan (when inputs are ready)

1. Add signing step in `.github/workflows/release.yml` after artifacts are built.
2. Sign:
   - `dist/rayo-windows/rayo-cli.exe`
   - `dist/rayo-windows/rayo-service.exe`
   - `dist/rayo-windows/rayo-gui.exe`
   - `dist/RayoSetup.exe`
3. Add verification step (`Get-AuthenticodeSignature`) and fail release if signature invalid.
4. Publish signed assets only.
