use std::{
    cell::RefCell,
    collections::{hash_map::DefaultHasher, HashMap, HashSet},
    env,
    fmt::Debug,
    hash::{Hash, Hasher},
    panic,
    rc::Rc,
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use itertools::Itertools;
use reqwest::header::HeaderMap;
use retry::{delay::Fixed, retry_with_index, OperationResult};
use revm_primitives::B160;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{debug, error, info, warn};

use super::{get_uniswap_info, PairContext, PathContext, TokenContext, UniswapProvider};
use crate::evm::{
    onchain::endpoints::{Chain, OnChainConfig, PairData},
    types::{EVMAddress, EVMU256},
};

pub struct Info {
    routes: Vec<Vec<PairData>>,
    basic_info: BasicInfo,
}

pub struct BasicInfo {
    weth: String,
    is_weth: bool,
}

const MAX_HOPS: u32 = 2; // Assuming the value of MAX_HOPS

pub fn fetch_uniswap_path(onchain: &mut OnChainConfig, token_address: EVMAddress) -> TokenContext {
    let token = format!("{:?}", token_address);
    let info: Info = find_path_subgraph(onchain, &token);

    let basic_info = info.basic_info;
    if basic_info.weth.is_empty() {
        warn!("failed to find weth address");
        return TokenContext::default();
    }
    let weth = EVMAddress::from_str(&basic_info.weth).unwrap();
    let is_weth = basic_info.is_weth;

    let routes = info.routes;

    let paths_parsed = routes
        .iter()
        .map(|pairs| {
            let mut path_parsed: PathContext = Default::default();
            pairs.iter().for_each(|pair| {
                match pair.src.as_str() {
                    "v2" => {
                        // let decimals0 = pair["decimals0"].as_u64().expect("failed to parse
                        // decimals0"); let decimals1 =
                        // pair["decimals1"].as_u64().expect("failed to parse decimals1");
                        // let next = EVMAddress::from_str(pair["next"].as_str().expect("failed to parse
                        // next")).expect("failed to parse next");

                        path_parsed.route.push(Rc::new(RefCell::new(PairContext {
                            pair_address: EVMAddress::from_str(pair.pair.as_str()).expect("failed to parse pair"),
                            next_hop: EVMAddress::from_str(pair.next.as_str()).expect("failed to parse pair"),
                            side: pair.in_ as u8,
                            uniswap_info: Arc::new(get_uniswap_info(
                                &UniswapProvider::from_str(pair.src_exact.as_str()).unwrap(),
                                &Chain::from_str(&onchain.chain_name).unwrap(),
                            )),
                            initial_reserves: (
                                EVMU256::try_from_be_slice(&hex::decode(&pair.initial_reserves_0).unwrap()).unwrap(),
                                EVMU256::try_from_be_slice(&hex::decode(&pair.initial_reserves_1).unwrap()).unwrap(),
                            ),
                        })));
                    }
                    "pegged" => {
                        // always live at final
                        path_parsed.final_pegged_ratio = EVMU256::from(pair.rate);
                        path_parsed.final_pegged_pair = Rc::new(RefCell::new(Some(PairContext {
                            pair_address: EVMAddress::from_str(pair.pair.as_str()).expect("failed to parse pair"),
                            next_hop: EVMAddress::from_str(pair.next.as_str()).expect("failed to parse pair"),
                            side: pair.in_ as u8,
                            uniswap_info: Arc::new(get_uniswap_info(
                                &UniswapProvider::from_str(pair.src_exact.as_str()).unwrap(),
                                &Chain::from_str(&onchain.chain_name).unwrap(),
                            )),
                            initial_reserves: (
                                EVMU256::try_from_be_slice(&hex::decode(&pair.initial_reserves_0).unwrap()).unwrap(),
                                EVMU256::try_from_be_slice(&hex::decode(&pair.initial_reserves_1).unwrap()).unwrap(),
                            ),
                        })));
                    }
                    "pegged_weth" => {
                        path_parsed.final_pegged_ratio = EVMU256::from(pair.rate);
                        path_parsed.final_pegged_pair = Rc::new(RefCell::new(None));
                    }
                    _ => unimplemented!("unknown swap path source"),
                }
            });
            path_parsed
        })
        .collect();

    TokenContext {
        swaps: paths_parsed,
        is_weth,
        weth_address: weth,
        address: token_address,
    }
}

pub fn get_weth(network: &str) -> String {
    let pegged_token = get_pegged_token(network);

    match network {
        "eth" => return pegged_token.get("WETH").unwrap().to_string(),
        "bsc" => return pegged_token.get("WBNB").unwrap().to_string(),
        "polygon" => return pegged_token.get("WMATIC").unwrap().to_string(),
        "local" => return pegged_token.get("ZERO").unwrap().to_string(),
        // "mumbai" => panic!("Not supported"),
        _ => {
            warn!("Unknown network");
            "".to_string()
        }
    }
}

fn get_pegged_token(network: &str) -> HashMap<String, String> {
    match network {
        "eth" => [
            ("WETH", "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"),
            ("USDC", "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
            ("USDT", "0xdac17f958d2ee523a2206206994597c13d831ec7"),
            ("DAI", "0x6b175474e89094c44da98b954eedeac495271d0f"),
            ("WBTC", "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599"),
            ("WMATIC", "0x7d1afa7b718fb893db30a3abc0cfc608aacfebb0"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect(),
        "bsc" => [
            ("WBNB", "0xbb4cdb9cbd36b01bd1cbaebf2de08d9173bc095c"),
            ("USDC", "0x8ac76a51cc950d9822d68b83fe1ad97b32cd580d"),
            ("USDT", "0x55d398326f99059ff775485246999027b3197955"),
            ("DAI", "0x1af3f329e8be154074d8769d1ffa4ee058b1dbc3"),
            ("WBTC", "0x7130d2a12b9bcbfae4f2634d864a1ee1ce3ead9c"),
            ("WETH", "0x2170ed0880ac9a755fd29b2688956bd959f933f8"),
            ("BUSD", "0xe9e7cea3dedca5984780bafc599bd69add087d56"),
            ("CAKE", "0x0e09fabb73bd3ade0a17ecc321fd13a19e81ce82"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect(),
        "polygon" => [
            ("WMATIC", "0x0d500b1d8e8ef31e21c99d1db9a6444d3adf1270"),
            ("USDC", "0x2791bca1f2de4661ed88a30c99a7a9449aa84174"),
            ("USDT", "0xc2132d05d31c914a87c6611c10748aeb04b58e8f"),
            ("DAI", "0x8f3cf7ad23cd3cadbd9735aff958023239c6a063"),
            ("WBTC", "0x1bfd67037b42cf73acf2047067bd4f2c47d9bfd6"),
            ("WETH", "0x7ceb23fd6bc0add59e62ac25578270cff1b9f619"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect(),
        "local" => [("ZERO", "0x0000000000000000000000000000000000000000")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        _ => {
            warn!("[Flashloan] Network is not supported");
            HashMap::new()
        }
    }
}

fn get_pair(onchain: &mut OnChainConfig, token: &str, network: &str, is_pegged: bool) -> Vec<PairData> {
    let token = token.to_lowercase();
    info!("fetching pairs for {token}");
    if token == get_weth(network) {
        return vec![];
    }
    let weth = get_weth(network);
    let pegged_tokens = get_pegged_token(network);
    let mut pairs = onchain.get_pair(
        token.as_str(),
        network,
        is_pegged || pegged_tokens.values().contains(&token),
        weth,
    );
    if pairs.len() > 10 {
        pairs.retain(|p| pegged_tokens.values().contains(&p.next));
    }
    pairs
}

fn get_all_hops(
    onchain: &mut OnChainConfig,
    token: &str,
    network: &str,
    hop: u32,
    known: &mut HashSet<String>,
) -> HashMap<String, Vec<PairData>> {
    known.insert(token.to_string());

    if hop > MAX_HOPS {
        return HashMap::new();
    }

    let mut hops: HashMap<String, Vec<PairData>> = HashMap::new();
    hops.insert(token.to_string(), get_pair(onchain, token, network, false));

    let pegged_tokens = get_pegged_token(network);

    for i in hops.clone().get(token).unwrap() {
        if pegged_tokens.values().any(|v| v == &i.next) || known.contains(&i.next) {
            continue;
        }
        let next_hops = get_all_hops(onchain, &i.next, network, hop + 1, known);
        hops.extend(next_hops);
    }

    hops
}

fn get_pegged_next_hop(onchain: &mut OnChainConfig, token: &str, network: &str) -> PairData {
    if token == get_weth(network) {
        return PairData {
            src: "pegged_weth".to_string(),
            rate: 1_000_000,
            in_: 0,
            next: "".to_string(),
            pair: "".to_string(),
            initial_reserves_0: "".to_string(),
            initial_reserves_1: "".to_string(),
            src_exact: "".to_string(),
            decimals_0: 0,
            decimals_1: 0,
        };
    }
    let mut peg_info = get_pair(onchain, token, network, true)
        .first()
        .expect("Unexpected RPC error, consider setting env <ETH_RPC_URL> ")
        .clone();

    add_reserve_info(onchain, &mut peg_info);
    let p0 = i128::from_str_radix(&peg_info.initial_reserves_0, 16).unwrap();
    let p1 = i128::from_str_radix(&peg_info.initial_reserves_1, 16).unwrap();

    if peg_info.in_ == 0 {
        peg_info.rate = (p1 as f64 / p0 as f64 * 1_000_000.0).round() as u32;
    } else {
        peg_info.rate = (p0 as f64 / p1 as f64 * 1_000_000.0).round() as u32;
    }

    PairData {
        src: "pegged".to_string(),
        ..peg_info.clone()
    }
}

/// returns whether the pair is significant
fn add_reserve_info(onchain: &mut OnChainConfig, pair_data: &mut PairData) -> bool {
    if pair_data.src == "pegged_weth" {
        return true;
    }

    let reserves = onchain.fetch_reserve(&pair_data.pair);
    pair_data.initial_reserves_0 = reserves.0;
    pair_data.initial_reserves_1 = reserves.1;

    let reserves_0 = EVMU256::from(i128::from_str_radix(&pair_data.initial_reserves_0, 16).unwrap());
    let reserves_1 = EVMU256::from(i128::from_str_radix(&pair_data.initial_reserves_1, 16).unwrap());

    // bypass for incorrect decimal implementation
    let min_r0 = if pair_data.decimals_0 == 0 {
        EVMU256::ZERO
    } else {
        EVMU256::from(10).pow(EVMU256::from(pair_data.decimals_0 - 1))
    };

    let min_r1 = if pair_data.decimals_1 == 0 {
        EVMU256::ZERO
    } else {
        EVMU256::from(10).pow(EVMU256::from(pair_data.decimals_1 - 1))
    };

    reserves_0 > min_r0 && reserves_1 > min_r1
}

fn with_info(routes: Vec<Vec<PairData>>, network: &str, token: &str) -> Info {
    Info {
        routes,
        basic_info: BasicInfo {
            weth: get_weth(network),
            is_weth: token == get_weth(network),
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn dfs(
    onchain: &mut OnChainConfig,
    token: &str,
    network: &str,
    path: &mut Vec<PairData>,
    visited: &mut HashSet<String>,
    pegged_tokens: &HashMap<String, String>,
    hops: &HashMap<String, Vec<PairData>>,
    routes: &mut Vec<Vec<PairData>>,
) {
    if pegged_tokens.values().any(|v| v == token) {
        let mut new_path = path.clone();
        new_path.push(get_pegged_next_hop(onchain, token, network));
        routes.push(new_path);
        return;
    }
    visited.insert(token.to_string());
    if !hops.contains_key(token) {
        return;
    }
    for hop in hops.get(token).unwrap() {
        if visited.contains(&hop.next) {
            continue;
        }
        path.push(hop.clone());
        dfs(onchain, &hop.next, network, path, visited, pegged_tokens, hops, routes);
        path.pop();
    }
}

fn find_path_subgraph(onchain: &mut OnChainConfig, token: &str) -> Info {
    let network = onchain.chain_name.clone();
    let pegged_tokens = get_pegged_token(network.as_str());

    if pegged_tokens.values().any(|v| v == token) {
        let hop = get_pegged_next_hop(onchain, token, network.as_str());
        return with_info(vec![vec![hop]], network.as_str(), token);
    }

    let mut known: HashSet<String> = HashSet::new();
    let hops = get_all_hops(onchain, token, network.as_str(), 0, &mut known);

    let mut routes: Vec<Vec<PairData>> = vec![];

    dfs(
        onchain,
        token,
        network.as_str(),
        &mut vec![],
        &mut HashSet::new(),
        &pegged_tokens,
        &hops,
        &mut routes,
    );

    let mut routes_without_low_liquidity_idx = vec![];

    for (kth, route) in (&mut routes).iter_mut().enumerate() {
        let mut low_liquidity = false;
        for hop in route {
            low_liquidity |= !add_reserve_info(onchain, hop);
        }
        if !low_liquidity {
            routes_without_low_liquidity_idx.push(kth);
        }
    }

    let routes_without_low_liquidity = routes_without_low_liquidity_idx
        .iter()
        .map(|&idx| routes[idx].clone())
        .collect();

    with_info(routes_without_low_liquidity, network.as_str(), token)
}

mod tests {
    use super::*;
    use crate::evm::{
        onchain::endpoints::Chain::{BSC, ETH},
        types::EVMAddress,
    };

    #[test]
    fn test_get_pegged_next_hop() {
        let mut config = OnChainConfig::new(BSC, 22055611);
        let token = "0xbb4cdb9cbd36b01bd1cbaebf2de08d9173bc095c";
        let v = get_pegged_next_hop(&mut config, token, "bsc");
        assert!(v.src == "pegged_weth");
    }

    #[test]
    fn test_get_all_hops() {
        let mut config = OnChainConfig::new(BSC, 22055611);
        let mut known: HashSet<String> = HashSet::new();
        let v: HashMap<String, Vec<PairData>> = get_all_hops(
            &mut config,
            "0x0e09fabb73bd3ade0a17ecc321fd13a19e81ce82",
            "bsc",
            0,
            &mut known,
        );
        assert!(!v.is_empty());
    }

    #[test]
    fn test_get_pair() {
        let mut config = OnChainConfig::new(BSC, 22055611);
        let v = get_pair(&mut config, "0x0e09fabb73bd3ade0a17ecc321fd13a19e81ce82", "bsc", false);

        debug!("{:?}", v);
        assert!(!v.is_empty());
    }

    #[test]
    fn test_fetch_uniswap_path() {
        let mut config = OnChainConfig::new(BSC, 22055611);
        let v = fetch_uniswap_path(
            &mut config,
            EVMAddress::from_str("0xcff086ead392ccb39c49ecda8c974ad5238452ac").unwrap(),
        );
        assert!(!v.swaps.is_empty());
        assert!(!v.weth_address.is_zero());
        assert!(!v.address.is_zero());
    }
}
