#!/usr/bin/env bash
# SPDX-FileCopyrightText: Vernum Projecten B.V.
# SPDX-License-Identifier: BUSL-1.1
# scripts/vendor/ihe-mcsd.sh
#
# Vendors the IHE mCSD 4.0.0 FHIR package artefacts the care services
# directory client of crates/ihe-iti (feature `mcsd`, #86) and the harness
# directory of tools/ferrofed-testkit read into docs/specs/ihe-mcsd/: the
# Organization, Endpoint and Location profiles with the endpoint-specific-type
# extension and the endpoint type code system and value sets, the search
# parameters the IG defines, the Directory and Query Client capability
# statements of ITI-90 and the Directory and Update Client capability
# statements of ITI-91, the ImplementationGuide, and the IG's own Organization
# and Endpoint examples, which the client's tests decode and the harness
# directory publishes. The package manifest is read for its name, version and
# licence and left out of the tree (the dependency-manifest rule of
# scripts/vendor/lib/corpus.sh).
#
# The "IHE mCSD FHIR package" row of docs/VERSIONS.md pins the package by
# version and by the sha256 of the registry tarball, so a republished package
# under the same version fails the fetch instead of changing the tree.
#
# Usage:
#   scripts/vendor/ihe-mcsd.sh
#
# Requires: curl, tar, shasum, jq.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"

# shellcheck source=scripts/vendor/lib/corpus.sh
# shellcheck disable=SC1091 # shellcheck is not run with -x; the library is checked on its own
. "$root/scripts/vendor/lib/corpus.sh"

corpus_require curl tar shasum jq

dest="docs/specs/ihe-mcsd"
name="ihe.iti.mcsd"

pin="$(corpus_pin_cell "IHE mCSD FHIR package")"
version="$(awk '{ for (i = 1; i < NF; i++) if ($i == "version") { v = $(i + 1); gsub(/[`,.;:]+$/, "", v); gsub(/`/, "", v); print v; exit } }' <<< "$pin")"
want="$(awk '{ for (i = 1; i <= NF; i++) { t = $i; gsub(/[`,.;:]/, "", t); if (t ~ /^[0-9a-f]{64}$/) { print t; exit } } }' <<< "$pin")"
[ -n "$version" ] || die "the pin names no package version"
[ -n "$want" ] || die "the pin names no package sha256"

# The artefacts of the ITI-90 Find Matching Care Services and ITI-91 Request
# Care Services Updates transactions over Organization, Endpoint and Location,
# at their upstream paths inside the package. The Practitioner,
# HealthcareService and OrganizationAffiliation profiles, the Feed and Location
# Distance options, the audit (BALP) profiles and examples, and the transaction
# Bundle example, which holds every resource type, serve no reader here and are
# not taken.
paths=(
  package/ImplementationGuide-ihe.iti.mcsd.json
  package/CapabilityStatement-IHE.mCSD.Directory.json
  package/CapabilityStatement-IHE.mCSD.QueryClient.json
  package/CapabilityStatement-IHE.mCSD.Directory.Update.json
  package/CapabilityStatement-IHE.mCSD.UpdateClient.json
  package/StructureDefinition-IHE.mCSD.Organization.json
  package/StructureDefinition-IHE.mCSD.Endpoint.json
  package/StructureDefinition-IHE.mCSD.Endpoint.DocShare.json
  package/StructureDefinition-IHE.mCSD.Location.json
  package/StructureDefinition-ihe-endpointspecifictype.json
  package/CodeSystem-MCSDEndpointTypes.json
  package/ValueSet-MCSDEndpointTypesVS.json
  package/ValueSet-MCSDEndpointTypesCoreDocShareVS.json
  package/SearchParameter-Endpoint-EndpointSpecificType.json
  package/SearchParameter-IHE.mCSD.Search.PurposeOfUse.json
  package/example/Organization-ex-OrgA.json
  package/example/Organization-ex-OrgB.json
  package/example/Organization-ex-OrgC.json
  package/example/Endpoint-ex-endpointDicom.json
  package/example/Endpoint-ex-endpointXCAquery.json
  package/example/Endpoint-ex-endpointXCAretrieve.json
)

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

url="https://packages.fhir.org/$name/$version"
say "fetching $url"
corpus_download "$url" "$tmp/package.tgz"
got="$(corpus_sha256 "$tmp/package.tgz")"
[ "$got" = "$want" ] || die "the $name $version tarball has sha256 $got, the pin records $want"
tar -xzf "$tmp/package.tgz" -C "$tmp"

manifest_name="$(jq -r .name "$tmp/package/package.json")"
manifest_version="$(jq -r .version "$tmp/package/package.json")"
licence="$(jq -r .license "$tmp/package/package.json")"
fhir="$(jq -r '.fhirVersions | join(", ")' "$tmp/package/package.json")"
[ "$manifest_name" = "$name" ] || die "the package names itself $manifest_name, not $name"
[ "$manifest_version" = "$version" ] || die "the package declares version $manifest_version, not $version"
[ "$licence" = "CC-BY-4.0" ] || die "the package declares licence $licence, not CC-BY-4.0"

rm -rf "$dest"
mkdir -p "$dest"
corpus_take "$tmp" "$dest" "${paths[@]}"

# shellcheck disable=SC2016 # the backticks are Markdown, not a command substitution
dropped="$(printf '| `%s` | `%s` |' package/package.json "$(corpus_sha256 "$tmp/package/package.json")")"

rows=""
while IFS= read -r file; do
  path="${file#"$dest"/}"
  rows="$rows
| \`$path\` | \`$(corpus_sha256 "$file")\` |"
done < <(find "$dest" -type f ! -name PROVENANCE.md | LC_ALL=C sort)

total="$(tar -tzf "$tmp/package.tgz" | grep -cv '/$')"
files="$(corpus_file_count "$dest")"
digest="$(corpus_tree_digest "$dest")"
fetched="$(corpus_fetched)"

cat > "$dest/PROVENANCE.md" << PROV
<!-- This file describes vendored third-party material; the bytes beside it
     keep their upstream licence, not the licence of this repository. -->

# Provenance: the IHE mCSD FHIR package

Vendored verbatim by \`scripts/vendor/ihe-mcsd.sh\`. Never edit a file here:
change the pin in docs/VERSIONS.md and re-run the script.

- Source: <$url>, the FHIR package registry's copy of the IG published at
  <https://profiles.ihe.net/ITI/mCSD/4.0.0/>
- Pin: package \`$name\` version \`$version\`, tarball sha256 \`$want\`
- Fetched: $fetched
- Upstream licence: Creative Commons Attribution 4.0 International
  (\`$licence\`, the \`license\` of the package manifest, listed under What is
  left out;
  <https://creativecommons.org/licenses/by/4.0/>). The package ships no licence
  file of its own. Attribution: IHE International, IT Infrastructure Technical
  Committee, *Mobile Care Services Discovery (mCSD)* $version.
- FHIR version: $fhir
- Layout: the upstream paths inside the package, unchanged
- Files: $files of the package's $total, listed below
- Tree digest (sha256 over the sorted per-file \`sha256  path\` listing,
  \`PROVENANCE.md\` excluded): \`$digest\`
- Read by: #86 (the ITI-90 and ITI-91 client of \`crates/ihe-iti\`, whose
  tests hold its interactions to the capability statements and decode the
  example Organizations and Endpoints, and the harness directory of
  \`tools/ferrofed-testkit\`, which publishes the example Organizations)

## What is here

The artefacts of ITI-90, Find Matching Care Services, and ITI-91, Request
Care Services Updates, over the resources a federation registry reads: the
Directory and Query Client capability statements of ITI-90 with their
Organization, Endpoint and Location search parameters, the Directory and
Update Client capability statements of ITI-91 (\`history-type\` with
\`_since\`), the Organization, Endpoint, Endpoint for Document Sharing and
Location profiles, the endpoint-specific-type extension, the mCSD endpoint
type code system and its value sets, the two search parameters the IG
defines, and the IG's example Organizations and Endpoints. The package's
other files serve no reader here: the Practitioner, PractitionerRole,
HealthcareService and OrganizationAffiliation profiles, the Feed and Location
Distance options, the BALP audit profiles and examples, the transaction
Bundle example, the Schematron renderings, the OpenAPI renderings and the
registry's \`.index.db\`, a SQLite file. They are not taken.

| File | sha256 |
|---|---|$rows

## What is left out

The package manifest, \`package.json\`. The script reads its name, version
and licence from the tarball and checks them against the pin. A vendored copy
would make this repository's dependency graph claim an npm package that
depends on \`hl7.fhir.r4.core\`, a FHIR registry package whose name the GitHub
advisory database flags as a malicious npm package; nothing here installs
either.

| File | sha256 |
|---|---|
$dropped
PROV

say "$files files, tree digest $digest"
say "done"
