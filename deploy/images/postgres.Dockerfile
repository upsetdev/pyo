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
ARG BASE_16=ghcr.io/cloudnative-pg/postgresql:16-system-trixie@sha256:a20a00bcf6f111424628a520e0b15fca28bcca939dcfdfef681697ea868e44a2
ARG BASE_17=ghcr.io/cloudnative-pg/postgresql:17-system-trixie@sha256:0f34d3fb12f415713ec94c3f2aa8295c7975f7114a0a8d360f26c958253bd032
ARG BASE_18=ghcr.io/cloudnative-pg/postgresql:18-system-trixie@sha256:c437b7c6c00aa7e66f5d6de1dd83602391fdebd4ff776ffb1ab7aaba5e7351ad

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
