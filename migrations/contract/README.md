# Contract migrations (spec §17.6)

Destructive steps of an earlier expand (drop a column the previous image no longer reads,
tighten a constraint, …). They run only via `vcrawl migrate --contract`, which refuses
unless the current image has run for `vcrawl.contract_min_days` (7) days with no other
image and no rollback in that window. Tracked in `_vc_contract_migrations`, apart from the
expand set. Name files `<version>_<description>.sql` with versions above every expand
migration of the release they contract, and start each with `SET lock_timeout = '5s';`.
