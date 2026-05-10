-- Initial schema for the myblog playground.
-- v1 hand-authored; future versions will be generated from models/*.json diffs.

CREATE EXTENSION IF NOT EXISTS "pgcrypto";

CREATE TABLE authors (
    id         UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    name       VARCHAR(120) NOT NULL,
    email      VARCHAR(255) NOT NULL UNIQUE,
    bio        TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE posts (
    id           UUID         PRIMARY KEY DEFAULT gen_random_uuid(),
    slug         VARCHAR(200) NOT NULL UNIQUE,
    title        VARCHAR(200) NOT NULL,
    body         TEXT         NOT NULL,
    author_id    UUID         NOT NULL REFERENCES authors(id) ON DELETE RESTRICT,
    published_at TIMESTAMPTZ,
    created_at   TIMESTAMPTZ  NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ  NOT NULL DEFAULT now()
);

CREATE INDEX posts_published_at_idx ON posts (published_at);
CREATE INDEX posts_author_id_idx    ON posts (author_id);

CREATE TABLE comments (
    id           UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    post_id      UUID        NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    author_name  VARCHAR(80) NOT NULL,
    author_email VARCHAR(255) NOT NULL,
    body         TEXT        NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX comments_post_id_created_at_idx ON comments (post_id, created_at);
