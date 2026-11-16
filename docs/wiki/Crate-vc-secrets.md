# Crate: vc-secrets

**Purpose.** Read secrets. Implements `vc_core::secrets::SecretSource` twice:

| Source | Use | How |
|---|---|---|
| `SecretManagerSource` | production | GCP Secret Manager over REST, `GET {endpoint}/v1/projects/{project}/secrets/{name}/versions/latest:access`, with an ADC token for the VM's `crawler-runtime` account; the payload's CRC32C is verified |
| `EnvSource` | tests, development | `VC_SECRET_<NAME>`: `crawler-pg-password` becomes `VC_SECRET_CRAWLER_PG_PASSWORD` |

Path: `crates/vc-secrets`. Dependencies: `reqwest`, `google-cloud-auth`.

## Key items

`from_config(config)` picks the backend; `valid_secret_name(name)` checks names before any
request; `TokenProvider` with `AdcTokens` and `StaticToken`; small `base64` and `crc32c`
helpers.

## Rules

- Secret values and tokens never appear in `Debug` output, errors or logs.
- Secret Manager is not a crawl target, so no `ClaimPermit` is involved.
- In production, `deploy.sh` also copies secrets into tmpfs files for containers; see
  [Operations and Deploy](Operations-and-Deploy).

## Config

`[secrets]`: backend (`secret_manager` or `env`), project, env prefix, endpoint, timeout.

## Tests

`tests/secrets.rs`: `secret_manager_reads_latest_version`, `secret_manager_maps_errors`,
`secret_manager_rejects_bad_checksum_and_payload`,
`secret_names_are_validated_before_any_request`, `env_source_reads_prefixed_uppercased_names`,
`from_config_picks_the_backend`.
