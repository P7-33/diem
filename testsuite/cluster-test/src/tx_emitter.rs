// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

#![forbid(unsafe_code)]

use crate::{cluster::Cluster, instance::Instance};
use anyhow::{format_err, Result};
use diem_crypto::{
    ed25519::{Ed25519PrivateKey, Ed25519PublicKey},
    test_utils::KeyPair,
};
use diem_logger::*;
use diem_sdk::transaction_builder::{Currency, TransactionFactory};
use diem_types::{
    account_address::AccountAddress,
    account_config::{testnet_dd_account_address, XUS_NAME},
    chain_id::ChainId,
    transaction::authenticator::AuthenticationKey,
};
use forge::atomic_histogram::*;
use itertools::zip;
use rand::{
    prelude::ThreadRng,
    rngs::{OsRng, StdRng},
    seq::{IteratorRandom, SliceRandom},
    Rng, SeedableRng,
};
use std::{
    env, fmt, slice,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::runtime::Handle;

use diem_client::{views::AmountView, Client as JsonRpcClient, MethodRequest};
use diem_sdk::types::{AccountKey, LocalAccount};
use diem_types::{
    account_config::{diem_root_address, treasury_compliance_account_address},
    transaction::SignedTransaction,
};
use futures::future::{try_join_all, FutureExt};
use once_cell::sync::Lazy;
use std::{
    cmp::{max, min},
    ops::Sub,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::{task::JoinHandle, time};

const MAX_TXN_BATCH_SIZE: usize = 100; // Max transactions per account in mempool
                                       // Please make 'MAX_CHILD_VASP_NUM' consistency with 'MAX_CHILD_ACCOUNTS' constant under VASP.move
const MAX_CHILD_VASP_NUM: usize = 65536;
const MAX_VASP_ACCOUNT_NUM: usize = 16;
const DD_KEY: &str = "dd.key";

#[derive(Debug)]
pub enum InvalidTxType {
    /// invalid tx with wrong chain id
    ChainId,
    /// invalid tx with sender not on chain
    Sender,
    /// invalid tx with receiver not on chain
    Receiver,
    /// duplicate an exist tx
    Duplication,
    /// Last element of enum, please add new case above
    MaxValue,
}

pub struct TxEmitter {
    accounts: Vec<LocalAccount>,
    mint_key_pair: KeyPair<Ed25519PrivateKey, Ed25519PublicKey>,
    chain_id: ChainId,
    vasp: bool,
    tx_factory: TransactionFactory,
}

pub struct EmitJob {
    workers: Vec<Worker>,
    stop: Arc<AtomicBool>,
    stats: Arc<StatsAccumulator>,
}

#[derive(Default)]
struct StatsAccumulator {
    submitted: AtomicU64,
    committed: AtomicU64,
    expired: AtomicU64,
    latency: AtomicU64,
    latencies: Arc<AtomicHistogramAccumulator>,
}

#[derive(Debug, Default)]
pub struct TxStats {
    pub submitted: u64,
    pub committed: u64,
    pub expired: u64,
    pub latency: u64,
    pub latency_buckets: AtomicHistogramSnapshot,
}

#[derive(Debug, Default)]
pub struct TxStatsRate {
    pub submitted: u64,
    pub committed: u64,
    pub expired: u64,
    pub latency: u64,
    pub p99_latency: u64,
}

#[derive(Clone)]
pub struct EmitThreadParams {
    pub wait_millis: u64,
    pub wait_committed: bool,
}

impl Default for EmitThreadParams {
    fn default() -> Self {
        Self {
            wait_millis: 0,
            wait_committed: true,
        }
    }
}

#[derive(Clone)]
pub struct EmitJobRequest {
    pub instances: Vec<Instance>,
    pub accounts_per_client: usize,
    pub workers_per_ac: Option<usize>,
    pub thread_params: EmitThreadParams,
    pub gas_price: u64,
    pub invalid_tx: u64,
}

pub static REUSE_ACC: Lazy<bool> = Lazy::new(|| env::var("REUSE_ACC").is_ok());
// This can let CT has ability to switch between legacy tx script type and script fn tx type
// with more types of tx need to be supported, this can be changed to an enum in the future
pub static SCRIPT_FN: Lazy<bool> = Lazy::new(|| env::var("SCRIPT_FN").is_ok());

impl EmitJobRequest {
    pub fn for_instances(
        instances: Vec<Instance>,
        global_emit_job_request: &Option<EmitJobRequest>,
        gas_price: u64,
        invalid_tx: u64,
    ) -> Self {
        let mut req = match global_emit_job_request {
            Some(global_emit_job_request) => EmitJobRequest {
                instances,
                accounts_per_client: global_emit_job_request.accounts_per_client,
                workers_per_ac: global_emit_job_request.workers_per_ac,
                thread_params: global_emit_job_request.thread_params.clone(),
                gas_price,
                invalid_tx,
            },
            None => Self {
                instances,
                accounts_per_client: 15,
                workers_per_ac: None,
                thread_params: EmitThreadParams::default(),
                gas_price,
                invalid_tx,
            },
        };
        if invalid_tx != 0 {
            req.thread_params.wait_committed = false;
        }
        req
    }

    pub fn fixed_tps_params(instance_count: usize, tps: u64) -> (usize, u64) {
        if tps < 1 {
            panic!("Target tps {} can not less than 1", tps)
        }
        let num_workers = tps as usize / instance_count + 1;
        let wait_time = (instance_count * num_workers * 1000_usize / tps as usize) as u64;
        (num_workers, wait_time)
    }

    pub fn fixed_tps(instances: Vec<Instance>, tps: u64, gas_price: u64, invalid_tx: u64) -> Self {
        let (num_workers, wait_time) = EmitJobRequest::fixed_tps_params(instances.len(), tps);
        Self {
            instances,
            accounts_per_client: 1,
            workers_per_ac: Some(num_workers),
            thread_params: EmitThreadParams {
                wait_millis: wait_time,
                wait_committed: invalid_tx == 0,
            },
            gas_price,
            invalid_tx,
        }
    }
}

impl TxEmitter {
    pub fn new(cluster: &Cluster, vasp: bool) -> Self {
        Self {
            accounts: vec![],
            mint_key_pair: cluster.mint_key_pair().clone(),
            chain_id: cluster.chain_id,
            vasp,
            tx_factory: TransactionFactory::new(cluster.chain_id),
        }
    }

    pub fn take_account(&mut self) -> LocalAccount {
        self.accounts.remove(0)
    }

    pub fn clear(&mut self) {
        self.accounts.clear();
    }

    fn account_key(&self) -> AccountKey {
        AccountKey::from_private_key(self.mint_key_pair.private_key.clone())
    }

    fn pick_mint_instance<'a, 'b>(&'a self, instances: &'b [Instance]) -> &'b Instance {
        let mut rng = ThreadRng::default();
        instances
            .choose(&mut rng)
            .expect("Instances can not be empty")
    }

    fn pick_mint_client(&self, instances: &[Instance]) -> JsonRpcClient {
        self.pick_mint_instance(instances).json_rpc_client()
    }

    pub async fn submit_single_transaction(
        &self,
        instance: &Instance,
        sender: &mut LocalAccount,
        receiver: &AccountAddress,
        num_coins: u64,
    ) -> Result<Instant> {
        let client = instance.json_rpc_client();
        client
            .submit(&gen_transfer_txn_request(
                sender,
                receiver,
                num_coins,
                self.tx_factory.clone(),
            ))
            .await?;
        let deadline = Instant::now() + TXN_MAX_WAIT;
        Ok(deadline)
    }

    pub async fn start_job(&mut self, req: EmitJobRequest) -> Result<EmitJob> {
        let workers_per_ac = match req.workers_per_ac {
            Some(x) => x,
            None => {
                let target_threads = 300;
                // Trying to create somewhere between target_threads/2..target_threads threads
                // We want to have equal numbers of threads for each AC, so that they are equally loaded
                // Otherwise things like flamegrap/perf going to show different numbers depending on which AC is chosen
                // Also limiting number of threads as max 10 per AC for use cases with very small number of nodes or use --peers
                min(10, max(1, target_threads / req.instances.len()))
            }
        };
        let num_clients = req.instances.len() * workers_per_ac;
        info!(
            "Will use {} workers per AC with total {} AC clients",
            workers_per_ac, num_clients
        );
        let num_accounts = req.accounts_per_client * num_clients;
        if self.vasp {
            assert!(
                num_accounts <= MAX_VASP_ACCOUNT_NUM * MAX_CHILD_VASP_NUM,
                "VASP only supports to create max {} child accounts, but try to create {} accounts",
                MAX_VASP_ACCOUNT_NUM * MAX_CHILD_VASP_NUM,
                num_accounts
            );
        }
        info!(
            "Will create {} accounts_per_client with total {} accounts",
            req.accounts_per_client, num_accounts
        );
        self.mint_accounts(&req, num_accounts).await?;
        let all_accounts = self.accounts.split_off(self.accounts.len() - num_accounts);
        let mut workers = vec![];
        let all_addresses: Vec<_> = all_accounts.iter().map(|d| d.address()).collect();
        let all_addresses = Arc::new(all_addresses);
        let mut all_accounts = all_accounts.into_iter();
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(StatsAccumulator::default());
        let tokio_handle = Handle::current();
        for instance in &req.instances {
            for _ in 0..workers_per_ac {
                let client = instance.json_rpc_client();
                let accounts = (&mut all_accounts).take(req.accounts_per_client).collect();
                let all_addresses = all_addresses.clone();
                let stop = stop.clone();
                let params = req.thread_params.clone();
                let stats = Arc::clone(&stats);
                let worker = SubmissionWorker {
                    accounts,
                    client,
                    all_addresses,
                    stop,
                    params,
                    stats,
                    chain_id: self.chain_id,
                    invalid_tx: req.invalid_tx,
                };
                let join_handle = tokio_handle.spawn(worker.run(req.gas_price).boxed());
                workers.push(Worker { join_handle });
            }
        }
        info!("Tx emitter workers started");
        Ok(EmitJob {
            workers,
            stop,
            stats,
        })
    }

    async fn load_account_with_mint_key(
        &self,
        client: &JsonRpcClient,
        address: AccountAddress,
    ) -> Result<LocalAccount> {
        let sequence_number = query_sequence_numbers(client, &[address])
            .await
            .map_err(|e| {
                format_err!(
                    "query_sequence_numbers on {:?} for account {} failed: {}",
                    client,
                    address,
                    e
                )
            })?[0];
        Ok(LocalAccount::new(
            address,
            self.account_key(),
            sequence_number,
        ))
    }

    pub async fn load_diem_root_account(&self, client: &JsonRpcClient) -> Result<LocalAccount> {
        self.load_account_with_mint_key(client, diem_root_address())
            .await
    }

    pub async fn load_faucet_account(&self, client: &JsonRpcClient) -> Result<LocalAccount> {
        self.load_account_with_mint_key(client, testnet_dd_account_address())
            .await
    }

    pub async fn load_tc_account(&self, client: &JsonRpcClient) -> Result<LocalAccount> {
        self.load_account_with_mint_key(client, treasury_compliance_account_address())
            .await
    }

    pub async fn load_dd_account(&self, client: &JsonRpcClient) -> Result<LocalAccount> {
        let mint_key: Ed25519PrivateKey = generate_key::load_key(DD_KEY);
        let account_key = AccountKey::from_private_key(mint_key);
        let address = account_key.authentication_key().derived_address();
        let sequence_number = query_sequence_numbers(client, &[address])
            .await
            .map_err(|e| {
                format_err!(
                    "query_sequence_numbers on {:?} for dd account failed: {}",
                    client,
                    e
                )
            })?[0];
        Ok(LocalAccount::new(address, account_key, sequence_number))
    }

    pub async fn load_vasp_account(
        &self,
        client: &JsonRpcClient,
        index: usize,
    ) -> Result<LocalAccount> {
        let file = "vasp".to_owned() + index.to_string().as_str() + ".key";
        let mint_key: Ed25519PrivateKey = generate_key::load_key(file);
        let account_key = AccountKey::from_private_key(mint_key);
        let address = account_key.authentication_key().derived_address();
        let sequence_number = query_sequence_numbers(client, &[address])
            .await
            .map_err(|e| {
                format_err!(
                    "query_sequence_numbers on {:?} for dd account failed: {}",
                    client,
                    e
                )
            })?[0];
        Ok(LocalAccount::new(address, account_key, sequence_number))
    }

    pub async fn get_money_source(
        &self,
        instances: &[Instance],
        coins_total: u64,
    ) -> Result<LocalAccount> {
        let client = self.pick_mint_instance(instances).json_rpc_client();
        let faucet_account = if !self.vasp {
            info!("Creating and minting faucet account");
            let mut account = self.load_faucet_account(&client).await?;
            let mint_txn = gen_mint_request(&mut account, coins_total, &self.tx_factory);
            execute_and_wait_transactions(
                &mut self.pick_mint_client(instances),
                &mut account,
                vec![mint_txn],
            )
            .await
            .map_err(|e| format_err!("Failed to mint into faucet account: {}", e))?;
            account
        } else {
            info!("Loading faucet account from DD account");
            self.load_dd_account(&client).await?
        };
        let balance = retrieve_account_balance(&client, faucet_account.address()).await?;
        for b in balance {
            if b.currency.eq(XUS_NAME) {
                info!(
                    "DD account current balances are {}, requested {} coins",
                    b.amount, coins_total
                );
                break;
            }
        }
        Ok(faucet_account)
    }

    pub async fn get_seed_accounts(
        &self,
        instances: &[Instance],
        seed_account_num: usize,
    ) -> Result<Vec<LocalAccount>> {
        let client = self.pick_mint_instance(instances).json_rpc_client();
        let seed_accounts = if !self.vasp {
            info!("Creating and minting seeds accounts");
            let mut account = self.load_tc_account(&client).await?;
            let seed_accounts = create_seed_accounts(
                &mut account,
                seed_account_num,
                100,
                self.pick_mint_client(instances),
                self.chain_id,
            )
            .await
            .map_err(|e| format_err!("Failed to create seed accounts: {}", e))?;
            info!("Completed creating seed accounts");
            seed_accounts
        } else {
            let mut seed_accounts = vec![];
            info!("Loading VASP account as seed accounts");
            let load_account_num = min(seed_account_num, MAX_VASP_ACCOUNT_NUM);
            for i in 0..load_account_num {
                let account = self.load_vasp_account(&client, i).await?;
                seed_accounts.push(account);
            }
            info!("Loaded {} VASP accounts", seed_accounts.len());
            seed_accounts
        };
        Ok(seed_accounts)
    }

    pub async fn mint_accounts(
        &mut self,
        req: &EmitJobRequest,
        requested_accounts: usize,
    ) -> Result<()> {
        if self.accounts.len() >= requested_accounts {
            info!("Not minting accounts");
            return Ok(()); // Early return to skip printing 'Minting ...' logs
        }
        let expected_num_seed_accounts =
            if requested_accounts / req.instances.len() > MAX_CHILD_VASP_NUM {
                requested_accounts / MAX_CHILD_VASP_NUM + 1
            } else {
                req.instances.len()
            };
        let num_accounts = requested_accounts - self.accounts.len(); // Only minting extra accounts
        let coins_per_account = (SEND_AMOUNT + req.gas_price) * MAX_TXNS;
        let coins_total = coins_per_account * num_accounts as u64;

        let mut faucet_account = self.get_money_source(&req.instances, coins_total).await?;
        // Create seed accounts with which we can create actual accounts concurrently
        let seed_accounts = self
            .get_seed_accounts(&req.instances, expected_num_seed_accounts)
            .await?;
        let actual_num_seed_accounts = seed_accounts.len();
        let num_new_child_accounts =
            (num_accounts + actual_num_seed_accounts - 1) / actual_num_seed_accounts;
        let coins_per_seed_account = coins_per_account * num_new_child_accounts as u64;
        mint_to_new_accounts(
            &mut faucet_account,
            &seed_accounts,
            coins_per_seed_account as u64,
            100,
            self.pick_mint_client(&req.instances),
            self.chain_id,
        )
        .await
        .map_err(|e| format_err!("Failed to mint seed_accounts: {}", e))?;
        info!("Completed minting seed accounts");
        info!("Minting additional {} accounts", num_accounts);

        let seed_rngs = gen_rng_for_reusable_account(actual_num_seed_accounts);
        // For each seed account, create a future and transfer diem from that seed account to new accounts
        let account_futures = seed_accounts
            .into_iter()
            .enumerate()
            .map(|(i, seed_account)| {
                // Spawn new threads
                let index = i % req.instances.len();
                let instance = req.instances[index].clone();
                let client = instance.json_rpc_client();
                create_new_accounts(
                    seed_account,
                    num_new_child_accounts,
                    coins_per_account,
                    20,
                    client,
                    self.chain_id,
                    self.vasp || *REUSE_ACC,
                    seed_rngs[i].clone(),
                )
            });
        let mut minted_accounts = try_join_all(account_futures)
            .await
            .map_err(|e| format_err!("Failed to mint accounts {}", e))?
            .into_iter()
            .flatten()
            .collect();

        self.accounts.append(&mut minted_accounts);
        assert!(
            self.accounts.len() >= num_accounts,
            "Something wrong in mint_account, wanted to mint {}, only have {}",
            requested_accounts,
            self.accounts.len()
        );
        info!("Mint is done");
        Ok(())
    }

    pub fn peek_job_stats(&self, job: &EmitJob) -> TxStats {
        job.stats.accumulate()
    }

    pub async fn stop_job(&mut self, job: EmitJob) -> TxStats {
        job.stop.store(true, Ordering::Relaxed);
        for worker in job.workers {
            let mut accounts = worker
                .join_handle
                .await
                .expect("TxEmitter worker thread failed");
            self.accounts.append(&mut accounts);
        }
        job.stats.accumulate()
    }

    pub async fn periodic_stat(&mut self, job: &EmitJob, duration: Duration, interval_secs: u64) {
        let deadline = Instant::now() + duration;
        let mut prev_stats: Option<TxStats> = None;
        while Instant::now() < deadline {
            let window = Duration::from_secs(interval_secs);
            tokio::time::sleep(window).await;
            let stats = self.peek_job_stats(job);
            let delta = &stats - &prev_stats.unwrap_or_default();
            prev_stats = Some(stats);
            info!("{}", delta.rate(window));
        }
    }

    pub async fn emit_txn_for(
        &mut self,
        duration: Duration,
        emit_job_request: EmitJobRequest,
    ) -> Result<TxStats> {
        let job = self.start_job(emit_job_request).await?;
        tokio::time::sleep(duration).await;
        let stats = self.stop_job(job).await;
        Ok(stats)
    }

    pub async fn emit_txn_for_with_stats(
        &mut self,
        duration: Duration,
        emit_job_request: EmitJobRequest,
        interval_secs: u64,
    ) -> Result<TxStats> {
        let job = self.start_job(emit_job_request).await?;
        self.periodic_stat(&job, duration, interval_secs).await;
        let stats = self.stop_job(job).await;
        Ok(stats)
    }

    pub async fn query_sequence_numbers(
        &self,
        instance: &Instance,
        address: &AccountAddress,
    ) -> Result<u64> {
        let client = instance.json_rpc_client();
        let resp = client
            .get_account(*address)
            .await
            .map_err(|e| format_err!("[{:?}] get_accounts failed: {:?} ", client, e))?
            .into_inner();
        Ok(resp
            .as_ref()
            .ok_or_else(|| format_err!("account does not exist"))?
            .sequence_number)
    }
}

struct Worker {
    join_handle: JoinHandle<Vec<LocalAccount>>,
}

struct SubmissionWorker {
    accounts: Vec<LocalAccount>,
    client: JsonRpcClient,
    all_addresses: Arc<Vec<AccountAddress>>,
    stop: Arc<AtomicBool>,
    params: EmitThreadParams,
    stats: Arc<StatsAccumulator>,
    chain_id: ChainId,
    invalid_tx: u64,
}

fn get_invalid_type() -> InvalidTxType {
    let mut rng = rand::thread_rng();
    match rng.gen_range(0..InvalidTxType::MaxValue as usize) {
        1 => InvalidTxType::Receiver,
        2 => InvalidTxType::Sender,
        3 => InvalidTxType::ChainId,
        _ => InvalidTxType::Duplication,
    }
}

fn invalid_tx(
    sender: &mut LocalAccount,
    receiver: &AccountAddress,
    chain_id: ChainId,
    gas_price: u64,
    reqs: &[SignedTransaction],
) -> SignedTransaction {
    let seed: [u8; 32] = OsRng.gen();
    let mut rng = StdRng::from_seed(seed);
    let mut invalid_account = LocalAccount::generate(&mut rng);
    let invalid_address = invalid_account.address();
    let tx_factory = TransactionFactory::new(chain_id).with_gas_unit_price(gas_price);
    match get_invalid_type() {
        InvalidTxType::Receiver => {
            gen_transfer_txn_request(sender, &invalid_address, SEND_AMOUNT, tx_factory)
        }
        InvalidTxType::Sender => {
            gen_transfer_txn_request(&mut invalid_account, receiver, SEND_AMOUNT, tx_factory)
        }
        InvalidTxType::ChainId => gen_transfer_txn_request(
            sender,
            receiver,
            SEND_AMOUNT,
            tx_factory.with_chain_id(ChainId::new(255)),
        ),
        InvalidTxType::Duplication => {
            // if this is the first tx, default to generate invalid tx with wrong chain id
            // otherwise, make a duplication of an exist valid tx
            if reqs.is_empty() {
                gen_transfer_txn_request(
                    sender,
                    receiver,
                    SEND_AMOUNT,
                    tx_factory.with_chain_id(ChainId::new(255)),
                )
            } else {
                let random_index = rng.gen_range(0..reqs.len() as usize);
                reqs[random_index].clone()
            }
        }
        _ => panic!("wrong invalid type"),
    }
}

impl SubmissionWorker {
    #[allow(clippy::collapsible_if)]
    async fn run(mut self, gas_price: u64) -> Vec<LocalAccount> {
        let wait = Duration::from_millis(self.params.wait_millis);
        while !self.stop.load(Ordering::Relaxed) {
            let requests = self.gen_requests(gas_price);
            let num_requests = requests.len();
            let start_time = Instant::now();
            let wait_util = start_time + wait;
            let mut tx_offset_time = 0u64;
            for request in requests {
                let cur_time = Instant::now();
                tx_offset_time += (cur_time - start_time).as_millis() as u64;
                self.stats.submitted.fetch_add(1, Ordering::Relaxed);
                let resp = self.client.submit(&request).await;
                if let Err(e) = resp {
                    warn!("[{:?}] Failed to submit request: {:?}", self.client, e);
                }
            }
            if self.params.wait_committed {
                let end_time = (Instant::now() - start_time).as_millis() as u64;

                if let Err(uncommitted) =
                    wait_for_accounts_sequence(&self.client, &mut self.accounts).await
                {
                    let num_committed = (num_requests - uncommitted.len()) as u64;
                    let latency = end_time - tx_offset_time / num_requests as u64;
                    self.stats
                        .committed
                        .fetch_add(num_committed, Ordering::Relaxed);
                    self.stats
                        .expired
                        .fetch_add(uncommitted.len() as u64, Ordering::Relaxed);
                    self.stats.latency.fetch_add(
                        // To avoid negative result caused by uncommitted tx occur
                        // Simplified from:
                        // end_time * num_committed - (tx_offset_time/num_requests) * num_committed
                        // to
                        // (end_time - tx_offset_time / num_requests) * num_committed
                        latency * num_committed as u64,
                        Ordering::Relaxed,
                    );
                    self.stats
                        .latencies
                        .record_data_point(latency, num_committed);
                    info!(
                        "[{:?}] Transactions were not committed before expiration: {:?}",
                        self.client, uncommitted
                    );
                } else {
                    let latency = end_time - tx_offset_time / num_requests as u64;
                    self.stats
                        .committed
                        .fetch_add(num_requests as u64, Ordering::Relaxed);
                    self.stats
                        .latency
                        .fetch_add(latency * num_requests as u64, Ordering::Relaxed);
                    self.stats
                        .latencies
                        .record_data_point(latency, num_requests as u64);
                }
            }
            let now = Instant::now();
            if wait_util > now {
                time::sleep(wait_util - now).await;
            }
        }
        self.accounts
    }

    fn gen_requests(&mut self, gas_price: u64) -> Vec<SignedTransaction> {
        let mut rng = ThreadRng::default();
        let batch_size = max(MAX_TXN_BATCH_SIZE, self.accounts.len());
        let accounts = self
            .accounts
            .iter_mut()
            .choose_multiple(&mut rng, batch_size);
        let mut requests = Vec::with_capacity(accounts.len());
        let invalid_size = if self.invalid_tx != 0 {
            // if enable mix invalid tx, at least 1 invalid tx per batch
            max(1, accounts.len() * self.invalid_tx as usize / 100)
        } else {
            0
        };
        let mut num_valid_tx = accounts.len() - invalid_size;
        for sender in accounts {
            let receiver = self
                .all_addresses
                .choose(&mut rng)
                .expect("all_addresses can't be empty");
            if num_valid_tx > 0 {
                let tx_factory =
                    TransactionFactory::new(self.chain_id).with_gas_unit_price(gas_price);
                let request = gen_transfer_txn_request(sender, receiver, SEND_AMOUNT, tx_factory);
                requests.push(request);
                num_valid_tx -= 1;
            } else {
                let request = invalid_tx(sender, receiver, self.chain_id, gas_price, &requests);
                requests.push(request);
            }
        }

        requests
    }
}

async fn wait_for_accounts_sequence(
    client: &JsonRpcClient,
    accounts: &mut [LocalAccount],
) -> Result<(), Vec<(AccountAddress, u64)>> {
    let deadline = Instant::now() + TXN_MAX_WAIT;
    let addresses: Vec<_> = accounts.iter().map(|d| d.address()).collect();
    loop {
        match query_sequence_numbers(client, &addresses).await {
            Err(e) => {
                info!(
                    "Failed to query ledger info on accounts {:?} for instance {:?} : {:?}",
                    addresses, client, e
                );
                time::sleep(Duration::from_millis(300)).await;
            }
            Ok(sequence_numbers) => {
                if is_sequence_equal(accounts, &sequence_numbers) {
                    break;
                }
                let mut uncommitted = vec![];
                if Instant::now() > deadline {
                    for (account, sequence_number) in zip(accounts, &sequence_numbers) {
                        if account.sequence_number() != *sequence_number {
                            warn!("Wait deadline exceeded for account {}, expected sequence {}, got from server: {}", account.address(), account.sequence_number(), sequence_number);
                            uncommitted.push((account.address(), *sequence_number));
                            *account.sequence_number_mut() = *sequence_number;
                        }
                    }
                    return Err(uncommitted);
                }
            }
        }
        time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

fn is_sequence_equal(accounts: &[LocalAccount], sequence_numbers: &[u64]) -> bool {
    for (account, sequence_number) in zip(accounts, sequence_numbers) {
        if *sequence_number != account.sequence_number() {
            return false;
        }
    }
    true
}

async fn query_sequence_numbers(
    client: &JsonRpcClient,
    addresses: &[AccountAddress],
) -> Result<Vec<u64>> {
    let mut result = vec![];
    for addresses_batch in addresses.chunks(20) {
        let resp = client
            .batch(
                addresses_batch
                    .iter()
                    .map(|a| MethodRequest::get_account(*a))
                    .collect(),
            )
            .await?
            .into_iter()
            .map(|r| r.map_err(anyhow::Error::new))
            .map(|r| r.map(|response| response.into_inner().unwrap_get_account()))
            .collect::<Result<Vec<_>>>()
            .map_err(|e| format_err!("[{:?}] get_accounts failed: {:?} ", client, e))?;

        for item in resp.into_iter() {
            result.push(
                item.ok_or_else(|| format_err!("account does not exist"))?
                    .sequence_number,
            );
        }
    }
    Ok(result)
}

const TXN_EXPIRATION_SECONDS: i64 = 150;
const TXN_MAX_WAIT: Duration = Duration::from_secs(TXN_EXPIRATION_SECONDS as u64 + 30);
const MAX_TXNS: u64 = 1_000_000;
const SEND_AMOUNT: u64 = 1;

async fn retrieve_account_balance(
    client: &JsonRpcClient,
    address: AccountAddress,
) -> Result<Vec<AmountView>> {
    let resp = client
        .get_account(address)
        .await
        .map_err(|e| format_err!("[{:?}] get_accounts failed: {:?} ", client, e))?
        .into_inner();
    Ok(resp
        .ok_or_else(|| format_err!("account does not exist"))?
        .balances)
}

fn gen_mint_request(
    faucet_account: &mut LocalAccount,
    num_coins: u64,
    tx_factory: &TransactionFactory,
) -> SignedTransaction {
    let receiver = faucet_account.address();
    faucet_account.sign_with_transaction_builder(tx_factory.peer_to_peer(
        Currency::XUS,
        receiver,
        num_coins,
    ))
}

pub fn gen_transfer_txn_request(
    sender: &mut LocalAccount,
    receiver: &AccountAddress,
    num_coins: u64,
    mut tx_factory: TransactionFactory,
) -> SignedTransaction {
    if *SCRIPT_FN {
        tx_factory = tx_factory.with_diem_version(2);
    }
    sender.sign_with_transaction_builder(tx_factory.peer_to_peer(
        Currency::XUS,
        *receiver,
        num_coins,
    ))
}

fn gen_create_child_txn_request(
    sender: &mut LocalAccount,
    receiver_auth_key: AuthenticationKey,
    num_coins: u64,
    chain_id: ChainId,
) -> SignedTransaction {
    sender.sign_with_transaction_builder(
        TransactionFactory::new(chain_id).create_child_vasp_account(
            Currency::XUS,
            receiver_auth_key,
            false,
            num_coins,
        ),
    )
}

fn gen_create_account_txn_request(
    creation_account: &mut LocalAccount,
    account: &LocalAccount,
    chain_id: ChainId,
) -> SignedTransaction {
    creation_account.sign_with_transaction_builder(
        TransactionFactory::new(chain_id).create_parent_vasp_account(
            Currency::XUS,
            0,
            account.authentication_key(),
            "",
            false,
        ),
    )
}

fn gen_mint_txn_request(
    sender: &mut LocalAccount,
    receiver: &AccountAddress,
    num_coins: u64,
    chain_id: ChainId,
) -> SignedTransaction {
    sender.sign_with_transaction_builder(TransactionFactory::new(chain_id).peer_to_peer(
        Currency::XUS,
        *receiver,
        num_coins,
    ))
}

fn gen_random_accounts(num_accounts: usize) -> Vec<LocalAccount> {
    let seed: [u8; 32] = OsRng.gen();
    let mut rng = StdRng::from_seed(seed);
    (0..num_accounts)
        .map(|_| LocalAccount::generate(&mut rng))
        .collect()
}

fn gen_rng_for_reusable_account(count: usize) -> Vec<StdRng> {
    // use same seed for reuse account creation and reuse
    let mut seed = [
        0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0,
        0, 0,
    ];
    let mut rngs = vec![];
    for i in 0..count {
        seed[31] = i as u8;
        rngs.push(StdRng::from_seed(seed));
    }
    rngs
}

async fn gen_reusable_account(client: &JsonRpcClient, rng: &mut StdRng) -> Result<LocalAccount> {
    let account_key = AccountKey::generate(rng);
    let address = account_key.authentication_key().derived_address();
    let sequence_number = match query_sequence_numbers(client, &[address]).await {
        Ok(v) => v[0],
        Err(_) => 0,
    };
    Ok(LocalAccount::new(address, account_key, sequence_number))
}

async fn gen_reusable_accounts(
    client: &JsonRpcClient,
    num_accounts: usize,
    rng: &mut StdRng,
) -> Result<Vec<LocalAccount>> {
    let mut vasp_accounts = vec![];
    let mut i = 0;
    while i < num_accounts {
        vasp_accounts.push(gen_reusable_account(client, rng).await?);
        i += 1;
    }
    Ok(vasp_accounts)
}

fn gen_create_child_txn_requests(
    source_account: &mut LocalAccount,
    accounts: &[LocalAccount],
    amount: u64,
    chain_id: ChainId,
) -> Vec<SignedTransaction> {
    accounts
        .iter()
        .map(|account| {
            gen_create_child_txn_request(
                source_account,
                account.authentication_key(),
                amount,
                chain_id,
            )
        })
        .collect()
}

fn gen_account_creation_txn_requests(
    creation_account: &mut LocalAccount,
    accounts: &[LocalAccount],
    chain_id: ChainId,
) -> Vec<SignedTransaction> {
    accounts
        .iter()
        .map(|account| gen_create_account_txn_request(creation_account, account, chain_id))
        .collect()
}

fn gen_mint_txn_requests(
    sending_account: &mut LocalAccount,
    accounts: &[LocalAccount],
    amount: u64,
    chain_id: ChainId,
) -> Vec<SignedTransaction> {
    accounts
        .iter()
        .map(|account| gen_mint_txn_request(sending_account, &account.address(), amount, chain_id))
        .collect()
}

pub async fn execute_and_wait_transactions(
    client: &mut JsonRpcClient,
    account: &mut LocalAccount,
    txn: Vec<SignedTransaction>,
) -> Result<()> {
    debug!(
        "[{:?}] Submitting transactions {} - {} for {}",
        client,
        account.sequence_number() - txn.len() as u64,
        account.sequence_number(),
        account.address()
    );
    for request in txn {
        diem_retrier::retry_async(diem_retrier::fixed_retry_strategy(5_000, 20), || {
            let request = request.clone();
            let c = client.clone();
            let client_name = format!("{:?}", client);
            Box::pin(async move {
                let txn_str = format!("{}::{}", request.sender(), request.sequence_number());
                debug!("Submitting txn {}", txn_str);
                let resp = c.submit(&request).await;
                debug!("txn {} status: {:?}", txn_str, resp);

                resp.map_err(|e| format_err!("[{}] Failed to submit request: {:?}", client_name, e))
            })
        })
        .await?;
    }
    let r = wait_for_accounts_sequence(client, slice::from_mut(account))
        .await
        .map_err(|_| format_err!("Mint transactions were not committed before expiration"));
    debug!(
        "[{:?}] Account {} is at sequence number {} now",
        client,
        account.address(),
        account.sequence_number()
    );
    r
}

/// Create `num_new_accounts` by transferring diem from `source_account`. Return Vec of created
/// accounts
async fn create_new_accounts(
    mut source_account: LocalAccount,
    num_new_accounts: usize,
    diem_per_new_account: u64,
    max_num_accounts_per_batch: u64,
    mut client: JsonRpcClient,
    chain_id: ChainId,
    reuse_account: bool,
    mut rng: StdRng,
) -> Result<Vec<LocalAccount>> {
    let mut i = 0;
    let mut accounts = vec![];
    while i < num_new_accounts {
        let batch_size = min(
            max_num_accounts_per_batch as usize,
            min(MAX_TXN_BATCH_SIZE, num_new_accounts - i),
        );
        let mut batch = if reuse_account {
            info!("loading {} accounts if they exist", batch_size);
            gen_reusable_accounts(&client, batch_size, &mut rng).await?
        } else {
            gen_random_accounts(batch_size)
        };
        let requests = gen_create_child_txn_requests(
            &mut source_account,
            &batch,
            diem_per_new_account,
            chain_id,
        );
        execute_and_wait_transactions(&mut client, &mut source_account, requests).await?;
        i += batch.len();
        accounts.append(&mut batch);
    }
    Ok(accounts)
}

/// Create `num_new_accounts`. Return Vec of created accounts
async fn create_seed_accounts(
    creation_account: &mut LocalAccount,
    num_new_accounts: usize,
    max_num_accounts_per_batch: u64,
    mut client: JsonRpcClient,
    chain_id: ChainId,
) -> Result<Vec<LocalAccount>> {
    let mut i = 0;
    let mut accounts = vec![];
    while i < num_new_accounts {
        let mut batch = gen_random_accounts(min(
            max_num_accounts_per_batch as usize,
            min(MAX_TXN_BATCH_SIZE, num_new_accounts - i),
        ));
        let create_requests = gen_account_creation_txn_requests(creation_account, &batch, chain_id);
        execute_and_wait_transactions(&mut client, creation_account, create_requests).await?;
        i += batch.len();
        accounts.append(&mut batch);
    }
    Ok(accounts)
}

/// Mint `diem_per_new_account` from `minting_account` to each account in `accounts`.
async fn mint_to_new_accounts(
    minting_account: &mut LocalAccount,
    accounts: &[LocalAccount],
    diem_per_new_account: u64,
    max_num_accounts_per_batch: u64,
    mut client: JsonRpcClient,
    chain_id: ChainId,
) -> Result<()> {
    let mut left = accounts;
    let mut i = 0;
    let num_accounts = accounts.len();
    while !left.is_empty() {
        let batch_size = OsRng.gen::<usize>()
            % min(
                max_num_accounts_per_batch as usize,
                min(MAX_TXN_BATCH_SIZE, num_accounts - i),
            );
        let (to_batch, rest) = left.split_at(batch_size + 1);
        let mint_requests =
            gen_mint_txn_requests(minting_account, to_batch, diem_per_new_account, chain_id);
        execute_and_wait_transactions(&mut client, minting_account, mint_requests).await?;
        i += to_batch.len();
        left = rest;
    }
    Ok(())
}

impl StatsAccumulator {
    pub fn accumulate(&self) -> TxStats {
        TxStats {
            submitted: self.submitted.load(Ordering::Relaxed),
            committed: self.committed.load(Ordering::Relaxed),
            expired: self.expired.load(Ordering::Relaxed),
            latency: self.latency.load(Ordering::Relaxed),
            latency_buckets: self.latencies.snapshot(),
        }
    }
}

impl TxStats {
    pub fn rate(&self, window: Duration) -> TxStatsRate {
        TxStatsRate {
            submitted: self.submitted / window.as_secs(),
            committed: self.committed / window.as_secs(),
            expired: self.expired / window.as_secs(),
            latency: if self.committed == 0 {
                0u64
            } else {
                self.latency / self.committed
            },
            p99_latency: self.latency_buckets.percentile(99, 100),
        }
    }
}

impl Sub for &TxStats {
    type Output = TxStats;

    fn sub(self, other: &TxStats) -> TxStats {
        TxStats {
            submitted: self.submitted - other.submitted,
            committed: self.committed - other.committed,
            expired: self.expired - other.expired,
            latency: self.latency - other.latency,
            latency_buckets: &self.latency_buckets - &other.latency_buckets,
        }
    }
}

impl fmt::Display for TxStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "submitted: {}, committed: {}, expired: {}",
            self.submitted, self.committed, self.expired,
        )
    }
}

impl fmt::Display for TxStatsRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "submitted: {} txn/s, committed: {} txn/s, expired: {} txn/s, latency: {} ms, p99 latency: {} ms",
            self.submitted, self.committed, self.expired, self.latency, self.p99_latency,
        )
    }
}

#[cfg(test)]
mod test {
    use crate::tx_emitter::EmitJobRequest;

    #[test]
    pub fn test_fixed_tps_params() {
        let inst_num = 30;
        let target_tps = 10;
        let (num_workers, wait_time) = EmitJobRequest::fixed_tps_params(inst_num, target_tps);
        assert_eq!(num_workers, 1usize);
        assert_eq!(wait_time, 3000u64);
        let target_tps = 30;
        let (num_workers, wait_time) = EmitJobRequest::fixed_tps_params(inst_num, target_tps);
        assert_eq!(num_workers, 2usize);
        assert_eq!(wait_time, 2000u64);
    }
}
