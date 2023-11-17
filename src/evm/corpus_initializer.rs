use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fs::File,
    io::Write,
    ops::Deref,
    path::Path,
    rc::Rc,
    time::Duration,
};

use bytes::Bytes;
use hex;
use itertools::Itertools;
use libafl::{
    corpus::{Corpus, Testcase},
    prelude::HasMetadata,
    schedulers::Scheduler,
    state::HasCorpus,
};
use libafl_bolts::impl_serdeany;
use revm_primitives::{Bytecode, Env};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info};

use super::{scheduler::ABIScheduler, srcmap::parser::SourceMapLocation};
/// Utilities to initialize the corpus
/// Add all potential calls with default args to the corpus
use crate::evm::abi::{get_abi_type_boxed, BoxedABI};
#[cfg(feature = "print_txn_corpus")]
use crate::fuzzer::DUMP_FILE_COUNT;
use crate::{
    dump_txn,
    evm::{
        blaz::builder::BuildJobResult,
        bytecode_analyzer,
        contract_utils::{extract_sig_from_contract, ABIConfig, ContractLoader},
        input::{ConciseEVMInput, EVMInput, EVMInputTy},
        middlewares::cheatcode::CHEATCODE_ADDRESS,
        mutator::AccessPattern,
        onchain::{abi_decompiler::fetch_abi_heimdall, flashloan::register_borrow_txn, BLACKLIST_ADDR},
        presets::Preset,
        types::{
            fixed_address,
            EVMAddress,
            EVMExecutionResult,
            EVMFuzzState,
            EVMInfantStateState,
            EVMStagedVMState,
            ProjectSourceMapTy,
            EVMU256,
        },
        vm::{EVMExecutor, EVMState},
    },
    fuzzer::REPLAY,
    generic_vm::vm_executor::GenericVM,
    input::ConciseSerde,
    state::HasCaller,
    state_input::StagedVMState,
};

pub const INITIAL_BALANCE: u128 = 100_000_000_000_000_000_000; // 100 ether

pub struct EVMCorpusInitializer<'a, SC, ISC>
where
    SC: ABIScheduler<State = EVMFuzzState> + Clone,
    ISC: Scheduler<State = EVMInfantStateState>,
{
    executor: &'a mut EVMExecutor<EVMInput, EVMFuzzState, EVMState, ConciseEVMInput, SC>,
    scheduler: SC,
    infant_scheduler: ISC,
    state: &'a mut EVMFuzzState,
    #[cfg(feature = "use_presets")]
    presets: Vec<&'a dyn Preset<EVMInput, EVMFuzzState, EVMState, SC>>,
    work_dir: String,
}

#[derive(Default)]
pub struct EVMInitializationArtifacts {
    pub address_to_sourcemap: ProjectSourceMapTy,
    pub address_to_bytecode: HashMap<EVMAddress, Bytecode>,
    pub address_to_abi: HashMap<EVMAddress, Vec<ABIConfig>>,
    pub address_to_abi_object: HashMap<EVMAddress, Vec<BoxedABI>>,
    pub address_to_name: HashMap<EVMAddress, String>,
    pub initial_state: EVMStagedVMState,
    pub initial_env: Env,
    pub build_artifacts: HashMap<EVMAddress, BuildJobResult>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ABIMap {
    pub signature_to_abi: HashMap<[u8; 4], ABIConfig>,
}

impl_serdeany!(ABIMap);

impl ABIMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, abi: ABIConfig) {
        self.signature_to_abi.insert(abi.function, abi);
    }

    pub fn get(&self, signature: &[u8; 4]) -> Option<&ABIConfig> {
        self.signature_to_abi.get(signature)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SourceMapMap {
    pub address_to_sourcemap: ProjectSourceMapTy,
}

impl_serdeany!(SourceMapMap);

impl SourceMapMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, address: EVMAddress, sourcemap: Option<HashMap<usize, SourceMapLocation>>) {
        self.address_to_sourcemap.insert(address, sourcemap);
    }

    pub fn get(&self, address: &EVMAddress) -> Option<&Option<HashMap<usize, SourceMapLocation>>> {
        self.address_to_sourcemap.get(address)
    }
}

#[macro_export]
macro_rules! handle_contract_insertion {
    ($state: expr, $host: expr, $deployed_address: expr, $abi: expr) => {
        let (is_erc20, is_pair) = match $host.flashloan_middleware {
            Some(ref middleware) => {
                let mut mid = middleware.deref().borrow_mut();
                mid.on_contract_insertion(&$deployed_address, &$abi, $state)
            }
            None => (false, false),
        };
        if is_erc20 {
            // scheduler should be mutable but host cannot be borrowed as mutable
            let scheduler = $host.scheduler.clone();
            register_borrow_txn(scheduler, $state, $deployed_address);
        }
        if is_pair {
            let mut mid = $host.flashloan_middleware.as_ref().unwrap().deref().borrow_mut();
            mid.on_pair_insertion(&$host, $state, $deployed_address);
        }
    };
}

macro_rules! wrap_input {
    ($input: expr) => {{
        let mut tc = Testcase::new($input);
        tc.set_exec_time(Duration::from_secs(0));
        tc
    }};
}

macro_rules! add_input_to_corpus {
    ($state: expr, $scheduler: expr, $input: expr) => {
        let idx = $state.add_tx_to_corpus(wrap_input!($input)).expect("failed to add");
        $scheduler.on_add($state, idx).expect("failed to call scheduler on_add");
    };
    ($state:expr, $scheduler:expr, $input:expr, $artifacts:expr) => {
        let idx = $state.add_tx_to_corpus(wrap_input!($input)).expect("failed to add");
        $scheduler
            .on_add_artifacts($state, idx, $artifacts)
            .expect("failed to call scheduler on_add_artifact");
    };
}

impl<'a, SC, ISC> EVMCorpusInitializer<'a, SC, ISC>
where
    SC: ABIScheduler<State = EVMFuzzState> + Clone + 'static,
    ISC: Scheduler<State = EVMInfantStateState>,
{
    pub fn new(
        executor: &'a mut EVMExecutor<EVMInput, EVMFuzzState, EVMState, ConciseEVMInput, SC>,
        scheduler: SC,
        infant_scheduler: ISC,
        state: &'a mut EVMFuzzState,
        work_dir: String,
    ) -> Self {
        Self {
            executor,
            scheduler,
            infant_scheduler,
            state,
            #[cfg(feature = "use_presets")]
            presets: vec![],
            work_dir,
        }
    }

    #[cfg(feature = "use_presets")]
    pub fn register_preset(&mut self, preset: &'a dyn Preset<EVMInput, EVMFuzzState, EVMState, SC>) {
        self.presets.push(preset);
    }

    pub fn initialize(&mut self, loader: &mut ContractLoader) -> EVMInitializationArtifacts {
        self.state.metadata_map_mut().insert(ABIMap::new());
        self.setup_default_callers();
        self.setup_contract_callers();
        self.init_cheatcode_contract();
        self.initialize_contract(loader);
        self.initialize_corpus(loader)
    }

    pub fn initialize_contract(&mut self, loader: &mut ContractLoader) {
        self.executor
            .host
            .evmstate
            .set_balance(self.executor.deployer, EVMU256::from(INITIAL_BALANCE));
        for contract in &mut loader.contracts {
            info!("Deploying contract: {}", contract.name);
            let deployed_address = if !contract.is_code_deployed {
                match self.executor.deploy(
                    Bytecode::new_raw(Bytes::from(contract.code.clone())),
                    Some(Bytes::from(contract.constructor_args.clone())),
                    contract.deployed_address,
                    self.state,
                ) {
                    Some(addr) => addr,
                    None => {
                        error!("Failed to deploy contract: {}", contract.name);
                        // we could also panic here
                        continue;
                    }
                }
            } else {
                debug!("Contract {} is already deployed", contract.name);
                // directly set bytecode
                let contract_code = Bytecode::new_raw(Bytes::from(contract.code.clone()));
                bytecode_analyzer::add_analysis_result_to_state(&contract_code, self.state);
                self.executor
                    .host
                    .set_code(contract.deployed_address, contract_code, self.state);
                contract.deployed_address
            };
            contract.deployed_address = deployed_address;
            info!("Contract {} deployed to: {deployed_address:?}", contract.name);
            self.state.add_address(&deployed_address);
        }
        info!("Deployed all contracts\n");
    }

    pub fn initialize_corpus(&mut self, loader: &mut ContractLoader) -> EVMInitializationArtifacts {
        let mut artifacts = EVMInitializationArtifacts {
            address_to_bytecode: HashMap::new(),
            address_to_sourcemap: HashMap::new(),
            address_to_abi: HashMap::new(),
            address_to_abi_object: Default::default(),
            address_to_name: Default::default(),
            initial_state: StagedVMState::new_with_state(match loader.setup_data {
                Some(ref setup_data) => setup_data.evmstate.clone(),
                None => self.executor.host.evmstate.clone(),
            }),
            build_artifacts: Default::default(),
            initial_env: match loader.setup_data {
                Some(ref setup_data) => setup_data.env.clone(),
                None => Default::default(),
            },
        };
        for contract in &mut loader.contracts {
            if contract.abi.is_empty() {
                // this contract's abi is not available, we will use 3 layers to handle this
                // 1. Extract abi from bytecode, and see do we have any function sig available
                //    in state
                // 2. Use Heimdall to extract abi
                // 3. Reconfirm on failures of heimdall
                debug!("Contract {} has no abi", contract.name);
                let contract_code = hex::encode(contract.code.clone());
                let sigs = extract_sig_from_contract(&contract_code);
                let mut unknown_sigs: usize = 0;
                for sig in &sigs {
                    if let Some(abi) = self.state.metadata_map().get::<ABIMap>().unwrap().get(sig) {
                        contract.abi.push(abi.clone());
                    } else {
                        unknown_sigs += 1;
                    }
                }

                if unknown_sigs >= sigs.len() / 30 {
                    debug!("Too many unknown function signature for {:?}, we are going to decompile this contract using Heimdall", contract.name);
                    let abis = fetch_abi_heimdall(contract_code)
                        .iter()
                        .map(|abi| {
                            if let Some(known_abi) =
                                self.state.metadata_map().get::<ABIMap>().unwrap().get(&abi.function)
                            {
                                known_abi
                            } else {
                                abi
                            }
                        })
                        .cloned()
                        .collect_vec();
                    contract.abi = abis;
                }
            }

            artifacts
                .address_to_sourcemap
                .insert(contract.deployed_address, contract.source_map.clone());
            artifacts
                .address_to_abi
                .insert(contract.deployed_address, contract.abi.clone());
            let mut code = vec![];
            if let Some(c) = self.executor.host.code.clone().get(&contract.deployed_address) {
                code.extend_from_slice(c.bytecode());
            }
            artifacts
                .address_to_bytecode
                .insert(contract.deployed_address, Bytecode::new_raw(Bytes::from(code)));

            let mut name = contract.name.clone().trim_end_matches('*').to_string();
            if name != format!("{:?}", contract.deployed_address) {
                name = format!("{}({:?})", name, contract.deployed_address.clone());
            }
            artifacts.address_to_name.insert(contract.deployed_address, name);

            if let Some(build_artifact) = &contract.build_artifact {
                artifacts
                    .build_artifacts
                    .insert(contract.deployed_address, build_artifact.clone());
            }

            {
                handle_contract_insertion!(
                    self.state,
                    self.executor.host,
                    contract.deployed_address,
                    contract.abi.clone()
                );
            }

            if unsafe {
                BLACKLIST_ADDR.is_some() && BLACKLIST_ADDR.as_ref().unwrap().contains(&contract.deployed_address)
            } {
                continue;
            }

            for abi in contract.abi.clone() {
                let name = &abi.function_name;

                if name.starts_with("invariant_") || name.starts_with("echidna_") || name == "setUp" || name == "failed"
                {
                    debug!("Skipping function: {}", name);
                    continue;
                }

                self.add_abi(&abi, contract.deployed_address, &mut artifacts);
            }
        }

        let mut tc = Testcase::new(artifacts.initial_state.clone());
        tc.set_exec_time(Duration::from_secs(0));
        let idx = self
            .state
            .infant_states_state
            .corpus_mut()
            .add(tc)
            .expect("failed to add");
        self.infant_scheduler
            .on_add(&mut self.state.infant_states_state, idx)
            .expect("failed to call infant scheduler on_add");
        artifacts
    }

    pub fn setup_default_callers(&mut self) {
        let default_callers = HashSet::from([
            fixed_address("8EF508Aca04B32Ff3ba5003177cb18BfA6Cd79dd"),
            fixed_address("35c9dfd76bf02107ff4f7128Bd69716612d31dDb"),
            // fixed_address("5E6B78f0748ACd4Fb4868dF6eCcfE41398aE09cb"),
        ]);

        for caller in default_callers {
            self.state.add_caller(&caller);
            self.executor
                .host
                .evmstate
                .set_balance(caller, EVMU256::from(INITIAL_BALANCE));
        }
    }

    pub fn setup_contract_callers(&mut self) {
        let contract_callers = HashSet::from([
            fixed_address("e1A425f1AC34A8a441566f93c82dD730639c8510"),
            fixed_address("68Dd4F5AC792eAaa5e36f4f4e0474E0625dc9024"),
            // fixed_address("aF97EE5eef1B02E12B650B8127D8E8a6cD722bD2"),
        ]);
        for caller in contract_callers {
            self.state.add_caller(&caller);
            self.executor
                .host
                .set_code(caller, Bytecode::new_raw(Bytes::from(vec![0xfd, 0x00])), self.state);
            self.executor
                .host
                .evmstate
                .set_balance(caller, EVMU256::from(INITIAL_BALANCE));
        }
    }

    pub fn init_cheatcode_contract(&mut self) {
        self.executor.host.set_code(
            CHEATCODE_ADDRESS,
            Bytecode::new_raw(Bytes::from(vec![0xfd, 0x00])),
            self.state,
        );
    }

    fn add_abi(&mut self, abi: &ABIConfig, deployed_address: EVMAddress, artifacts: &mut EVMInitializationArtifacts) {
        if abi.is_constructor {
            return;
        }

        match self.state.hash_to_address.get_mut(abi.function.clone().as_slice()) {
            Some(addrs) => {
                addrs.insert(deployed_address);
            }
            None => {
                self.state
                    .hash_to_address
                    .insert(abi.function, HashSet::from([deployed_address]));
            }
        }
        #[cfg(not(feature = "fuzz_static"))]
        if abi.is_static {
            return;
        }
        let mut abi_instance = get_abi_type_boxed(&abi.abi);
        abi_instance.set_func_with_signature(abi.function, &abi.function_name, &abi.abi);

        artifacts
            .address_to_abi_object
            .entry(deployed_address)
            .or_default()
            .push(abi_instance.clone());
        let input = EVMInput {
            caller: self.state.get_rand_caller(),
            contract: deployed_address,
            data: if abi.function_name != "!receive!" {
                Some(abi_instance)
            } else {
                None
            },
            sstate: StagedVMState::new_uninitialized(),
            sstate_idx: 0,
            txn_value: if abi.is_payable { Some(EVMU256::ZERO) } else { None },
            step: false,
            env: artifacts.initial_env.clone(),
            access_pattern: Rc::new(RefCell::new(AccessPattern::new())),
            liquidation_percent: 0,
            input_type: EVMInputTy::ABI,
            direct_data: Default::default(),
            randomness: vec![0],
            repeat: 1,
        };
        add_input_to_corpus!(self.state, &mut self.scheduler, input.clone(), artifacts);
        #[cfg(feature = "print_txn_corpus")]
        {
            let corpus_dir = format!("{}/corpus", self.work_dir.as_str());
            dump_txn!(corpus_dir, &input)
        }
        #[cfg(feature = "use_presets")]
        {
            let presets = self.presets.clone();
            for p in presets {
                let presets = p.presets(abi.function, &input, self.executor);
                presets.iter().for_each(|preset| {
                    add_input_to_corpus!(self.state, &mut self.scheduler, preset.clone());
                });
            }
        }
    }
}
