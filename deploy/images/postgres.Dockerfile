# syntax=docker/dockerfile:1.7
# Pyo's Postgres: CloudNativePG's `system` image for one major version
# (PG_MAJOR: 16, 17 or 18), pinned, plus the extensions its databases can
# turn on that the image doesn't have: pg_cron, pg_hint_plan, pg_partman and
# TimescaleDB (the Apache 2 build), from the PostgreSQL apt repository
# (PGDG) the base already uses. pgvector and pgaudit come with the base.
# Built with each release as postgres-<major>; pyo-db-controller runs the one
# for a database's version (PYO_DB_POSTGRES_IMAGE_<major>).
#
# `system`, not `standard`: the base keeps barman-cloud, which Postgres
# backups use (barmanObjectStore).

ARG PG_MAJOR=18
ARG BASE_16=ghcr.io/cloudnative-pg/postgresql:16-system-trixie@sha256:1546be7d23066aaf9eb75b07df17c843c98483cede7afdbaf01dca785fb3bf60
ARG BASE_17=ghcr.io/cloudnative-pg/postgresql:17-system-trixie@sha256:0c4a645863c9f9b756c3dabb3082af89e904d65c8ea2d78aad6c74fe450484cc
ARG BASE_18=ghcr.io/cloudnative-pg/postgresql:18-system-trixie@sha256:48a0038fdd9672d12853d5300666b2592f97f9d34a993ea42e3ecb5a9a5e43e3

FROM ${BASE_16} AS base-16
FROM ${BASE_17} AS base-17
FROM ${BASE_18} AS base-18

FROM base-${PG_MAJOR}
ARG PG_MAJOR
USER root
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      "postgresql-${PG_MAJOR}-cron" \
      "postgresql-${PG_MAJOR}-pg-hint-plan" \
      "postgresql-${PG_MAJOR}-partman" \
      "postgresql-${PG_MAJOR}-timescaledb" \
 && apt-get purge -y --auto-remove -o APT::AutoRemove::RecommendsImportant=false \
 && rm -rf /var/lib/apt/lists/* /var/cache/* /var/log/*
# CloudNativePG runs Postgres as uid 26.
USER 26
