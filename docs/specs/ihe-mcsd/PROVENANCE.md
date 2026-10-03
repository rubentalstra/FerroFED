<!-- This file describes vendored third-party material; the bytes beside it
     keep their upstream licence, not the licence of this repository. -->

# Provenance: the IHE mCSD FHIR package

Vendored verbatim by `scripts/vendor/ihe-mcsd.sh`. Never edit a file here:
change the pin in docs/VERSIONS.md and re-run the script.

- Source: <https://packages.fhir.org/ihe.iti.mcsd/4.0.0>, the FHIR package registry's copy of the IG published at
  <https://profiles.ihe.net/ITI/mCSD/4.0.0/>
- Pin: package `ihe.iti.mcsd` version `4.0.0`, tarball sha256 `933a143d7bb14c66731a32f52a084c6cb92476aca1b917db77a4640f8a5290ad`
- Fetched: 2026-10-03
- Upstream licence: Creative Commons Attribution 4.0 International
  (`CC-BY-4.0`, the `license` of the package manifest, listed under What is
  left out;
  <https://creativecommons.org/licenses/by/4.0/>). The package ships no licence
  file of its own. Attribution: IHE International, IT Infrastructure Technical
  Committee, *Mobile Care Services Discovery (mCSD)* 4.0.0.
- FHIR version: 4.0.1
- Layout: the upstream paths inside the package, unchanged
- Files: 21 of the package's 114, listed below
- Tree digest (sha256 over the sorted per-file `sha256  path` listing,
  `PROVENANCE.md` excluded): `3bad4f6ee360f86321ef633a07c0e32c385901034539f2654eef03e4d0fabf27`
- Read by: #86 (the ITI-90 and ITI-91 client of `crates/ihe-iti`, whose
  tests hold its interactions to the capability statements and decode the
  example Organizations and Endpoints, and the harness directory of
  `tools/ferrofed-testkit`, which publishes the example Organizations)

## What is here

The artefacts of ITI-90, Find Matching Care Services, and ITI-91, Request
Care Services Updates, over the resources a federation registry reads: the
Directory and Query Client capability statements of ITI-90 with their
Organization, Endpoint and Location search parameters, the Directory and
Update Client capability statements of ITI-91 (`history-type` with
`_since`), the Organization, Endpoint, Endpoint for Document Sharing and
Location profiles, the endpoint-specific-type extension, the mCSD endpoint
type code system and its value sets, the two search parameters the IG
defines, and the IG's example Organizations and Endpoints. The package's
other files serve no reader here: the Practitioner, PractitionerRole,
HealthcareService and OrganizationAffiliation profiles, the Feed and Location
Distance options, the BALP audit profiles and examples, the transaction
Bundle example, the Schematron renderings, the OpenAPI renderings and the
registry's `.index.db`, a SQLite file. They are not taken.

| File | sha256 |
|---|---|
| `package/CapabilityStatement-IHE.mCSD.Directory.Update.json` | `32784328c54a95ea054bd83799b7cd2507e28f925205536b825e679da6df1139` |
| `package/CapabilityStatement-IHE.mCSD.Directory.json` | `22fbba39568ba780105e4836f42e30bb94b95fe9aadf55c0706dbad2f72a5436` |
| `package/CapabilityStatement-IHE.mCSD.QueryClient.json` | `1aaba29a631503b1ff54bdf082f832614f209d6b6a3207044d91440b89754691` |
| `package/CapabilityStatement-IHE.mCSD.UpdateClient.json` | `b136f2cf85ed2ccb018ef52870b608fc9e7726d39f659519f3b1e34d6171275e` |
| `package/CodeSystem-MCSDEndpointTypes.json` | `aa1c3d671c54ece37ec5cc87351495f7e7676a6cfb5c1a3368986e626e7064c8` |
| `package/ImplementationGuide-ihe.iti.mcsd.json` | `0ba7381110649e8dd9d119504ff913e2224fdb93e8fb7f45dc2eb3b7c47cfbcc` |
| `package/SearchParameter-Endpoint-EndpointSpecificType.json` | `dafb1944c3cb8d114784cbe43263e538c61b0f6598eee44c8b5e39ec9fecfde7` |
| `package/SearchParameter-IHE.mCSD.Search.PurposeOfUse.json` | `a2d0b9703cd1ef90d83cdace8ad792234391f35b8c08e66ef27af2a6c28943ea` |
| `package/StructureDefinition-IHE.mCSD.Endpoint.DocShare.json` | `95eb43cb0f14b95aa91411609e41566f5f92576600ba325501ba456364676521` |
| `package/StructureDefinition-IHE.mCSD.Endpoint.json` | `2fff4be00066b541d47f3c823e7b34e163e3d8b5c5815a0a85f8485eefa9052d` |
| `package/StructureDefinition-IHE.mCSD.Location.json` | `ae8ad9f6a9524145b8085e92b6c79316dcfb4345707d1e658871ae87e35d31fd` |
| `package/StructureDefinition-IHE.mCSD.Organization.json` | `865c230a6fee5b3536ba2679ce5a3e59a25ee00e29210abe32fc31845ac59877` |
| `package/StructureDefinition-ihe-endpointspecifictype.json` | `8abe32adb51d9fa26a59953c6f9bdbd68f81eeb3d9714e9b38351e85ca1b8e44` |
| `package/ValueSet-MCSDEndpointTypesCoreDocShareVS.json` | `56b7f7df04909fbfa203de6e32c7215060e3a0511482ddea9dd939ceff136aad` |
| `package/ValueSet-MCSDEndpointTypesVS.json` | `072a19970baad85dc736c48bfd53e062ca605a5dfd973c2fab8a91b952b13893` |
| `package/example/Endpoint-ex-endpointDicom.json` | `5dc772a6f3c1841f40728e643d14510c72c6f47498787b48f4a401f4fd70b3cc` |
| `package/example/Endpoint-ex-endpointXCAquery.json` | `8fa1eae19a1ca295bf7a9953eaf826c210b2b66064dbfc37e97bc08c5f4b8764` |
| `package/example/Endpoint-ex-endpointXCAretrieve.json` | `223b80ab0fda4bcfec9843d37aada9d32070d13f305a7a5970e867f69f6c8ced` |
| `package/example/Organization-ex-OrgA.json` | `3a5dbe78cbef0aa321868565c6af66ff5a7efce8b8ca3ad6d79d5a6f929bb9d5` |
| `package/example/Organization-ex-OrgB.json` | `94b21af05f8a3042efeea22cf9e75990ae73985932831129faa2ff8431088af0` |
| `package/example/Organization-ex-OrgC.json` | `e7eff15a5cdeade0fa61b1510cde5f316d2c47e360ffee9d5bbe331788354e4d` |

## What is left out

The package manifest, `package.json`. The script reads its name, version
and licence from the tarball and checks them against the pin. A vendored copy
would make this repository's dependency graph claim an npm package that
depends on `hl7.fhir.r4.core`, a FHIR registry package whose name the GitHub
advisory database flags as a malicious npm package; nothing here installs
either.

| File | sha256 |
|---|---|
| `package/package.json` | `7ad06d47d1a870331a5c0ebaf5fa001037b79d9139d3c1653cdd96c7e2be3e64` |
