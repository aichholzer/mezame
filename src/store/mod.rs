//! Persistent state: the store behind every table, and the keys that
//! protect the secrets in it.
//!
//! This phase lands the module in steps: the master key and the cipher
//! first, the `Store` trait and its SQLite implementation next.

pub mod crypto;
