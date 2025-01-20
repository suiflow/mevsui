// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use sui_indexer_alt_metrics::MetricsArgs;
use sui_pg_db::DbArgs;

use crate::{data::system_package_task::SystemPackageTaskArgs, RpcArgs};

#[derive(clap::Parser, Debug, Clone)]
pub struct Args {
    #[command(flatten)]
    pub db_args: DbArgs,

    #[command(flatten)]
    pub rpc_args: RpcArgs,

    #[command(flatten)]
    pub system_package_task_args: SystemPackageTaskArgs,

    #[command(flatten)]
    pub metrics_args: MetricsArgs,
}
