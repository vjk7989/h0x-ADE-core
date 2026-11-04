# macOS release signing

In h0x-core's GitHub **release** environment, add these secrets with the
**same values as h0x-ade's release environment**:

- `APPLE_CERTIFICATE_P12`: base64-encoded Developer ID Application certificate
  exported as a `.p12`, including its private key.
- `APPLE_CERTIFICATE_PASSWORD`: the password for that `.p12`.

No Apple API key, issuer, or notarization secrets are needed. The release
workflow signs both macOS architectures before packaging on tag pushes when
these secrets are set. With neither secret it uses ad-hoc signing; setting
only one is an error. Manual/untagged builds always use ad-hoc signing, even
if secrets exist.

The Developer ID signature uses the stable identifier `tech.pavii.h0xcore` so
macOS privacy grants can survive updates once core runs as its own responsible
process. Existing grants to ad-hoc builds may need to be granted again on the
first Developer ID release. Keep the signing team and identifier stable.

Hardened runtime is deliberately off: it strips `DYLD_*` variables from core's
environment before agents inherit it. This CLI is downloaded by curl without
quarantine, so it does not need notarization to run.

The distribution installer downloads with curl, extracts, copies, chmods, and
renames the binary (or delegates to `h0x-core update --from`). `h0x-core update` stages a
copy, chmods it, checks it runs, then renames it into place. Neither re-signs,
strips, nor adds quarantine. HyperOx's `src/core_bin.rs` delegates installation
to this installer and updates to `h0x-core update`; it does not alter the signature.
The embedded signature survives these operations unchanged.
