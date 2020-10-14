// Copyright (c) Facebook, Inc. and its affiliates.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashSet;
use std::fs::File;
use std::hash::Hash;
use std::io::{self, Write};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Result};
use cursive::utils::markup::StyledString;
use regex::Regex;
use serde_json::{json, Value};
use toml::value::Value as TValue;

use common::dateutil;
use common::util::translate_datetime;
use model;

use store::advance::Advance;
use store::Direction;

#[macro_use]
pub mod get;
pub mod cgroup;
pub mod command;
pub mod disk;
mod fill;
pub mod iface;
pub mod network;
pub mod print;
pub mod process;
pub mod system;
pub mod tmain;
pub mod transport;

pub use command::DumpCommand;
use command::{
    CgroupField, DiskField, GeneralOpt, IfaceField, NetworkField, OutputFormat, ProcField,
    SysField, TransportField,
};
use fill::Dfill;
use get::Dget;
use print::Dprint;
use tmain::{Dump, IterExecResult};

const BELOW_DUMP_RC: &str = "/.config/below/dumprc";

// The DumpType trait is the key of how we make our dump generic.
// Basically, the DumpType trait will be required by all dump related
// traits to provide a guideline on what's the concrete type looks like.
// For how traits work altogether, please take a look at tmain.rs.
// # Types:
// Model ==> The real model typle, like CgroupModel or SingleProcessModel.
// FieldsType ==> The enum tag type we defined in command.rs, like Sys.
// DataType ==> Our struct that implement the BelowDecor per dump module.
pub trait DumpType {
    type Model: Default;
    type FieldsType: Eq + Hash;
    type DataType;
}

fn get_advance(
    logger: slog::Logger,
    dir: PathBuf,
    host: Option<String>,
    port: Option<u16>,
    opts: &command::GeneralOpt,
) -> Result<(SystemTime, Advance)> {
    let mut time_begin = UNIX_EPOCH
        + Duration::from_secs(
            dateutil::HgTime::parse(&opts.begin)
                .ok_or_else(|| anyhow!("Unrecognized begin format"))?
                .unixtime,
        );

    let mut time_end = if opts.end.is_none() {
        SystemTime::now()
    } else {
        UNIX_EPOCH
            + Duration::from_secs(
                dateutil::HgTime::parse(opts.end.as_ref().unwrap())
                    .ok_or_else(|| anyhow!("Unrecognized end format"))?
                    .unixtime,
            )
    };

    if let Some(days) = opts.yesterdays.as_ref() {
        if days.is_empty() || days.find(|c: char| c != 'y').is_some() {
            bail!("Unrecognized days adjuster format: {}", days);
        }
        let time_to_deduct = Duration::from_secs(days.chars().count() as u64 * 86400);
        time_begin -= time_to_deduct;
        time_end -= time_to_deduct;
    }

    let mut advance = if let Some(host) = host {
        Advance::new_with_remote(logger, host, port, time_begin)?
    } else {
        Advance::new(logger.clone(), dir, time_begin)
    };

    advance.initialize();

    Ok((time_end, advance))
}

/// Try to read $HOME/.config/below/dumprc file and generate a list of keys which will
/// be used as fields. Any errors happen in this function will directly trigger a panic.
pub fn parse_pattern<T: FromStr>(
    filename: String,
    pattern_key: String,
    section_key: &str,
) -> Option<Vec<T>> {
    let dumprc_map = match std::fs::read_to_string(filename) {
        Ok(dumprc_str) => match dumprc_str.parse::<TValue>() {
            Ok(dumprc) => dumprc
                .as_table()
                .expect("Failed to parse dumprc: File may be empty.")
                .to_owned(),
            Err(e) => panic!("Failed to parse dumprc file: {}", e),
        },
        Err(e) => panic!("Failed to read dumprc file: {}", e),
    };

    Some(
        dumprc_map
            .get(section_key)
            .unwrap_or_else(|| panic!("Failed to get section key: [{}]", section_key))
            .get(&pattern_key)
            .unwrap_or_else(|| panic!("Failed to get pattern key: {}", pattern_key))
            .as_array()
            .unwrap_or_else(|| panic!("Failed to parse pattern {} value to array.", pattern_key))
            .iter()
            .map(|field| {
                T::from_str(
                    field.as_str().unwrap_or_else(|| {
                        panic!("Failed to parse field key {} into string", field)
                    }),
                )
                .or_else(|_| Err(format!("Failed to parse field key: {}", field)))
                .unwrap()
            })
            .collect(),
    )
}

pub fn run(
    logger: slog::Logger,
    dir: PathBuf,
    host: Option<String>,
    port: Option<u16>,
    cmd: DumpCommand,
) -> Result<()> {
    let filename = format!(
        "{}{}",
        std::env::var("HOME").expect("Fail to obtain HOME env var"),
        BELOW_DUMP_RC
    );

    match cmd {
        DumpCommand::System {
            fields,
            opts,
            pattern,
        } => {
            let (time_end, advance) = get_advance(logger, dir, host, port, &opts)?;
            let mut sys = system::System::new(opts, advance, time_end, None);
            if let Some(pattern_key) = pattern {
                sys.init(parse_pattern(filename, pattern_key, "system"));
            } else {
                sys.init(fields);
            }
            sys.exec()
        }
        DumpCommand::Disk {
            fields,
            opts,
            select,
            pattern,
        } => {
            let (time_end, advance) = get_advance(logger, dir, host, port, &opts)?;
            let mut disk = disk::Disk::new(opts, advance, time_end, select);
            if let Some(pattern_key) = pattern {
                disk.init(parse_pattern(filename, pattern_key, "disk"));
            } else {
                disk.init(fields);
            }
            disk.exec()
        }
        DumpCommand::Process {
            fields,
            opts,
            select,
            pattern,
        } => {
            let (time_end, advance) = get_advance(logger, dir, host, port, &opts)?;
            let mut process = process::Process::new(opts, advance, time_end, select);
            if let Some(pattern_key) = pattern {
                process.init(parse_pattern(filename, pattern_key, "process"));
            } else {
                process.init(fields);
            }
            process.exec()
        }
        DumpCommand::Cgroup {
            fields,
            opts,
            select,
            pattern,
        } => {
            let (time_end, advance) = get_advance(logger, dir, host, port, &opts)?;
            let mut cgroup = cgroup::Cgroup::new(opts, advance, time_end, select);
            if let Some(pattern_key) = pattern {
                cgroup.init(parse_pattern(filename, pattern_key, "cgroup"));
            } else {
                cgroup.init(fields);
            }
            cgroup.exec()
        }
        DumpCommand::Iface {
            fields,
            opts,
            select,
            pattern,
        } => {
            let (time_end, advance) = get_advance(logger, dir, host, port, &opts)?;
            let mut iface = iface::Iface::new(opts, advance, time_end, select);
            if let Some(pattern_key) = pattern {
                iface.init(parse_pattern(filename, pattern_key, "iface"));
            } else {
                iface.init(fields);
            }
            iface.exec()
        }
        DumpCommand::Network {
            fields,
            opts,
            select,
            pattern,
        } => {
            let (time_end, advance) = get_advance(logger, dir, host, port, &opts)?;
            let mut network = network::Network::new(opts, advance, time_end, select);
            if let Some(pattern_key) = pattern {
                network.init(parse_pattern(filename, pattern_key, "network"));
            } else {
                network.init(fields);
            }
            network.exec()
        }
        DumpCommand::Transport {
            fields,
            opts,
            select,
            pattern,
        } => {
            let (time_end, advance) = get_advance(logger, dir, host, port, &opts)?;
            let mut transport = transport::Transport::new(opts, advance, time_end, select);
            if let Some(pattern_key) = pattern {
                transport.init(parse_pattern(filename, pattern_key, "transport"));
            } else {
                transport.init(fields);
            }
            transport.exec()
        }
    }
}
