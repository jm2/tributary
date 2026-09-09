//! Fixture-driven decision tests for the bot review gate
//! (`.github/workflows/bot-review-gate.yml`).
//!
//! The gate is the machine-readable merge evidence for the repository's
//! all-green policy, and its audit required the decisions themselves — not
//! just the workflow text — to be tested. These tests extract the run script
//! from the workflow YAML (so the tested bytes are the bytes Actions
//! executes), run it under bash with a stub `gh` that serves recorded API
//! pages from `tests/fixtures/bot_review_gate/<scenario>/`, and assert the
//! pass/fail decision and the reported reason codes for each policy
//! situation: unresolved threads (outdated included), outstanding change
//! requests, stale review evidence, pagination completeness, head binding,
//! and the documented refresh paths.

#![cfg(unix)]

mod evidence;
mod fail_closed;
mod harness;
mod substitution;
