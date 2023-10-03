use move_vm_types::loaded_data::runtime_types::Type;
use move_vm_types::values::Value;

pub mod corpus_initializer;
pub mod input;
pub mod movevm;
pub mod mutator;
pub mod oracles;
pub mod scheduler;
pub mod types;
pub mod vm_state;

use clap::Parser;
use hex::{decode, encode};
use ityfuzz::fuzzers::move_fuzzer::{move_fuzzer, MoveFuzzConfig};
use ityfuzz::oracle::{Oracle, Producer};
use ityfuzz::r#const;
use ityfuzz::state::FuzzState;
use serde::Deserialize;
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::env;
use std::rc::Rc;
use std::str::FromStr;

/// CLI for ItyFuzz for Move smart contracts
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct MoveArgs {
    /// Glob pattern / address to find contracts
    #[arg(short, long)]
    target: String,

    /// Seed for the RNG
    #[arg(short, long, default_value = "0")]
    seed: u64,
}

pub fn move_main(args: MoveArgs) {
    move_fuzzer(&MoveFuzzConfig {
        target: args.target,
        work_dir: "./work_dir".to_string(),
        seed: args.seed,
    });
}