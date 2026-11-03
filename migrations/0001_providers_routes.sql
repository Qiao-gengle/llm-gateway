CREATE EXTENSION IF NOT EXISTS pgcrypto; -- gen_random_uuid()

CREATE OR REPLACE FUNCTION touch_updated_at() RETURNS trigger AS $$
BEGIN NEW.updated_at = now(); RETURN NEW; END;
$$ LANGUAGE plpgsql;

-- 上游提供商（需求 9、13）
CREATE TABLE providers (
    id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name               TEXT NOT NULL UNIQUE CHECK (char_length(name) BETWEEN 1 AND 64),
    protocol           TEXT NOT NULL CHECK (protocol IN ('openai', 'anthropic')),
    base_url           TEXT NOT NULL,
    api_key_nonce      BYTEA NOT NULL CHECK (octet_length(api_key_nonce) = 12),
    api_key_ciphertext BYTEA NOT NULL,           -- 密文 || 16 字节 GCM tag
    connect_timeout_s  INTEGER NOT NULL DEFAULT 5   CHECK (connect_timeout_s > 0),
    ttfb_timeout_s     INTEGER NOT NULL DEFAULT 60  CHECK (ttfb_timeout_s > 0),
    total_timeout_s    INTEGER NOT NULL DEFAULT 600 CHECK (total_timeout_s > 0),
    enabled            BOOLEAN NOT NULL DEFAULT TRUE,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (connect_timeout_s <= total_timeout_s AND ttfb_timeout_s <= total_timeout_s)
);
CREATE TRIGGER providers_touch BEFORE UPDATE ON providers
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

-- 路由（需求 10）
CREATE TABLE routes (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    public_model TEXT NOT NULL UNIQUE CHECK (char_length(public_model) BETWEEN 1 AND 256),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TRIGGER routes_touch BEFORE UPDATE ON routes
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

CREATE TABLE route_targets (
    id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    route_id           UUID NOT NULL REFERENCES routes(id) ON DELETE CASCADE,
    position           INTEGER NOT NULL,           -- 保持 Admin 提交时的顺序
    provider_id        UUID NOT NULL REFERENCES providers(id) ON DELETE RESTRICT,
    upstream_model     TEXT NOT NULL CHECK (char_length(upstream_model) BETWEEN 1 AND 256),
    priority           INTEGER NOT NULL CHECK (priority BETWEEN 0 AND 1000),
    weight             INTEGER NOT NULL CHECK (weight BETWEEN 1 AND 10000),
    default_max_tokens INTEGER CHECK (default_max_tokens IS NULL OR default_max_tokens > 0),
    enabled            BOOLEAN NOT NULL DEFAULT TRUE,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (route_id, position)
);
CREATE INDEX route_targets_provider_idx ON route_targets(provider_id);
CREATE TRIGGER route_targets_touch BEFORE UPDATE ON route_targets
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
