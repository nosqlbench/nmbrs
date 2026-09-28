// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Native-link directives for the optional `engine-cassandra-cpp` feature.
//!
//! `cassandra-cpp-sys` declares `-lcassandra -luv -lssl -lcrypto`,
//! but recent cpp-driver builds bundle minizip which calls into
//! zlib (`inflate`, `crc32`, ...). Statically linking against
//! `libcassandra.a` therefore needs `-lz` too. This build script
//! adds it whenever the feature is enabled.

fn main() {
    if std::env::var_os("CARGO_FEATURE_ENGINE_CASSANDRA_CPP").is_some() {
        println!("cargo:rustc-link-lib=z");
    }
}
