#!/usr/bin/env bash
# Promotes the crawler image (deploy.yml, job `promote`, which runs only after
# CI passed on main and the build + Trivy job passed):
#   1. Still main's HEAD? If main moved on, stop: nothing is signed and
#      `prod` doesn't move (the newer commit's run promotes its own image).
#   2. cosign keyless sign IMAGE@DIGEST (GitHub OIDC → Sigstore), verify it
#      exactly as the VM's deploy.sh will (CERT_IDENTITY/CERT_ISSUER), then
#      move IMAGE:prod to it and check the tag resolves to that digest.
# Signing happens only here, so every image signed as
# deploy.yml@refs/heads/main passed CI and was main's HEAD when signed.
#
# Env: IMAGE DIGEST REPO SHA CERT_IDENTITY CERT_ISSUER; GITHUB_STEP_SUMMARY optional.
set -euo pipefail

: "${IMAGE:?}" "${DIGEST:?}" "${REPO:?}" "${SHA:?}" "${CERT_IDENTITY:?}" "${CERT_ISSUER:?}"
summary="${GITHUB_STEP_SUMMARY:-/dev/null}"
[[ "$DIGEST" =~ ^sha256:[0-9a-f]{64}$ ]] || { echo "::error::not a digest: '$DIGEST'"; exit 1; }
prod_digest() {
  docker buildx imagetools inspect "$IMAGE:prod" --format '{{json .Manifest}}' | jq -r .digest
}

head="$(git ls-remote "https://github.com/$REPO" refs/heads/main | cut -f1)"
[[ "$head" =~ ^[0-9a-f]{40}$ ]] || { echo "::error::could not read main's HEAD (got '$head')"; exit 1; }
if [[ "$head" != "$SHA" ]]; then
  echo "::notice::main moved on to $head; the image for $SHA is not signed and prod is not moved"
  echo "- main moved on, not promoted" >>"$summary"
  exit 0
fi

ref="$IMAGE@$DIGEST"
cosign sign --yes "$ref"
cosign verify --certificate-identity "$CERT_IDENTITY" --certificate-oidc-issuer "$CERT_ISSUER" "$ref" >/dev/null
# --prefer-index=false: carbon-copy the signed manifest; the default wraps a
# single manifest in a new index, so :prod would get an unsigned digest.
docker buildx imagetools create --prefer-index=false --tag "$IMAGE:prod" "$ref"
got="$(prod_digest)"
[[ "$got" == "$DIGEST" ]] || { echo "::error::$IMAGE:prod is $got, expected $DIGEST"; exit 1; }
echo "- \`$ref\` signed and promoted to \`prod\` (the VM deploys it within about 2 minutes)" >>"$summary"
