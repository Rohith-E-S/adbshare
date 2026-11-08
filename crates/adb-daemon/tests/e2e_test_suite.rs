//! Opaque-Box E2E Test Suite for ADBShare.
//!
//! Covers Tiers 1-4 according to `TEST_INFRA.md` and `PROJECT.md § Feature Inventory`:
//! - Tier 1: Feature Coverage (>=5 test cases per feature, 90 test cases)
//! - Tier 2: Boundary & Corner Cases (>=5 test cases per feature, 90 test cases)
//! - Tier 3: Cross-Feature Interactions (10 pairwise test cases)
//! - Tier 4: Real-World Application Scenarios (5 application scenarios)

#[path = "e2e_test_suite/harness.rs"]
pub mod harness;

#[path = "e2e_test_suite/tier1.rs"]
pub mod tier1;

#[path = "e2e_test_suite/tier2.rs"]
pub mod tier2;

#[path = "e2e_test_suite/tier3.rs"]
pub mod tier3;

#[path = "e2e_test_suite/tier4.rs"]
pub mod tier4;
