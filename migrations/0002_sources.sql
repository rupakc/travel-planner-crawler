-- 0002 sources and scout proposals (owner: vc-sources 17xx).
SET lock_timeout = '5s';

CREATE TABLE sources (
    source_id      text PRIMARY KEY CHECK (source_id ~ '^[a-z0-9][a-z0-9-]{1,62}$'),
    name           text NOT NULL,
    category       text NOT NULL,
    source_class   text NOT NULL CHECK (source_class IN
                     ('official', 'reference', 'editorial', 'community', 'commercial_api')),
    licence_class  text NOT NULL CHECK (licence_class IN
                     ('open', 'share_alike', 'facts_only', 'api_terms', 'restricted')),
    licence        jsonb NOT NULL,                 -- {class, spdx, attribution, url}
    method         text NOT NULL CHECK (method IN ('html', 'rss', 'sitemap', 'api', 'dump')),
    politeness_key text,
    enabled        boolean NOT NULL DEFAULT true,
    spec           jsonb NOT NULL,                 -- the full SourceSpec
    spec_hash      text NOT NULL,
    quality_score  real NOT NULL DEFAULT 1.0,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    CHECK (licence_class <> 'restricted' OR NOT enabled)
);

-- Source scout output: proposals only; a human approves (spec §5.7).
CREATE TABLE source_candidates (
    candidate_id bigserial PRIMARY KEY,
    host         text NOT NULL UNIQUE,
    proposed_by  text NOT NULL,                    -- 'scout' | operator
    destination  text,
    kinds        text[] NOT NULL DEFAULT '{}',
    checks       jsonb NOT NULL DEFAULT '{}',      -- robots, denylist, ToS scan, feeds, licence hints
    status       text NOT NULL DEFAULT 'proposed'
                   CHECK (status IN ('proposed', 'approved', 'rejected')),
    created_at   timestamptz NOT NULL DEFAULT now(),
    decided_at   timestamptz,
    decided_by   text
);
