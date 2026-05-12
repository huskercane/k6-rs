//! k6-conformance: behavioral parity harness between upstream k6 and k6-rs.
//!
//! Architecture: both runners feed a canonical intermediate model (`CanonicalRun`)
//! via adapters. The diff layer operates only on `CanonicalRun` and must have
//! zero adapter-specific branches.

pub mod adapters;
pub mod canonical;
pub mod diff;
pub mod expectations;
pub mod fixtures;
pub mod report;
pub mod runner;
