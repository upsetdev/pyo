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
ARG BASE_16=ghcr.io/cloudnative-pg/postgresql:16-system-trixie@sha256:1e55b58c8baecce687cc625b47c90d5787f5b410d199da429378ebd8b7e16e4c
ARG BASE_17=ghcr.io/cloudnative-pg/postgresql:17-system-trixie@sha256:4aabc4e49078b9ceae3cd37e307e7ac7c59ace827f6c17ae6bb8648c88e42ca0
ARG BASE_18=ghcr.io/cloudnative-pg/postgresql:18-system-trixie@sha256:b4bbd2e623f1a8833e9a2a4210480ef215b6d13fc0988b06786a778a5c699a20

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
