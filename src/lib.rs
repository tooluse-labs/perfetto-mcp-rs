// Copyright 2025 The perfetto-mcp-rs Authors
// SPDX-License-Identifier: Apache-2.0

pub(crate) mod proto {
    include!(concat!(env!("OUT_DIR"), "/perfetto.protos.rs"));
}

pub mod check_update;
pub mod download;
pub mod error;
pub mod install;
pub mod params;
pub(crate) mod query;
pub mod self_update;
pub mod server;
pub mod sql_templates;
pub mod stdlib_catalog;
pub(crate) mod telemetry;
pub mod tp_client;
pub mod tp_manager;
