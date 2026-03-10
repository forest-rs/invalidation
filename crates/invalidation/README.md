# leit_core

`leit_core` defines shared identifiers and lightweight retrieval vocabulary for
the Leit kernel.

The crate should remain small, explicit, and `no_std`-friendly where practical.
It is the place for long-lived core primitives that multiple kernel crates need
to share without pulling in orchestration or product policy.

## Overview

Goal: provide a stable home for shared kernel IDs, score vocabulary, and common
result shapes.

Non-goals: orchestration, async runtime glue, benchmark code, or
feature-specific integrations.

## Concepts

- Typed IDs: use newtypes for Leit-owned identifiers instead of raw integers or
  aliases.
- Shared vocabulary: define common score and hit/result shapes once so kernel
  crates speak the same language.
- Small center: keep the crate predictable, cheap to depend on, and free of
  higher-level policy.

## Glossary

- Typed ID: a small newtype wrapper such as `FieldId` or `SegmentId`.
- Shared vocabulary: common kernel-facing types such as scores and ranked hits.
- Kernel crate: one of the small core crates that implements retrieval
  mechanics without orchestration.

## Usage

The current crate surface is still a placeholder:

## Extension Points

- Add typed identifiers that are part of the public or semi-public kernel
  architecture.
- Add shared result and score vocabulary when multiple kernel crates need the
  same semantics.
- Keep examples and benchmarks in separate workspace crates instead of adding
  heavy dev-dependencies here.

## Gotchas

- This README describes the intended role of `leit_core`; the concrete API is
  not built out yet.
- Application-defined identifiers should remain application-defined rather than
  being forced into Leit-owned core types.
