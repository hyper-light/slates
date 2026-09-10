//! Test fixtures shared by the daemon's integration tests. Each integration test is its own crate and
//! compiles this module afresh, so a helper one test does not use is dead code there — allowed here.
#![allow(dead_code)]

pub(crate) mod nfs;
