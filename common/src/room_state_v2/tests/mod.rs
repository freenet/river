//! Law tests over the real implementation (the executable model's families
//! and random universes, ported), mutants as negative tests, and unit tests
//! of encoding, validation, summaries and deltas.

mod bounds_tests;
mod builder;
mod convert_tests;
mod corpus;
mod families;
mod fastcbor_tests;
mod harness;
mod laws;
mod mechanisms;
mod selftest;
mod sizes;
mod state_tests;
mod universe;
