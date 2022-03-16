use crate::config::{MAX_BLOCK_SIZE, EMISSION_SPEED_FACTOR, FEE_PER_KB, MAX_SUPPLY, REGISTRATION_DIFFICULTY, DEV_FEE_PERCENT, MINIMUM_DIFFICULTY, GENESIS_BLOCK, DEV_ADDRESS};
use crate::globals::get_current_time;
use crate::crypto::key::PublicKey;
use crate::crypto::hash::{Hash, Hashable};
use crate::p2p::server::P2pServer;
use super::block::{Block, CompleteBlock};
use super::difficulty::{check_difficulty, calculate_difficulty};
use super::transaction::*;
use super::serializer::Serializer;
use super::error::BlockchainError;
use super::mempool::{Mempool, SortedTx};
use std::collections::HashMap;
use std::sync::atomic::{Ordering, AtomicU64};
use std::sync::Arc;

#[derive(serde::Serialize)]
pub struct Account {
    balance: u64,
    nonce: u64
}

impl Account {
    pub fn get_balance(&self) -> u64 {
        self.balance
    }

    pub fn get_nonce(&self) -> u64 {
        self.nonce
    }
}

#[derive(serde::Serialize)]
pub struct Blockchain {
    blocks: Vec<CompleteBlock>, // all blocks in blockchain: TODO use storage
    height: AtomicU64, // current block height 
    supply: AtomicU64, // current circulating supply based on coins already emitted
    top_hash: Hash, // current block top hash
    difficulty: AtomicU64, // difficulty for next block
    mempool: Mempool, // mempool to retrieve/add all txs
    #[serde(skip_serializing)]
    accounts: HashMap<PublicKey, Account>, // all accounts registered on chain: TODO use storage
    #[serde(skip_serializing)]
    p2p: Arc<P2pServer>,
    dev_address: PublicKey // Dev address for block fee
}

impl Blockchain {

    pub fn new() -> Self {
        let dev_address = match PublicKey::from_address(&DEV_ADDRESS.to_owned()) {
            Ok(addr) => addr,
            Err(e) => panic!("Invalid dev address: {}", e)
        };

        let mut blockchain = Blockchain {
            blocks: vec![],
            height: AtomicU64::new(0),
            supply: AtomicU64::new(0),
            top_hash: Hash::zero(),
            difficulty: AtomicU64::new(MINIMUM_DIFFICULTY),
            mempool: Mempool::new(),
            accounts: HashMap::new(),
            p2p: P2pServer::new(1, None, 8, String::from("127.0.0.1:2126")),
            dev_address: dev_address.clone()
        };

        if let Err(e) = blockchain.create_genesis_block() {
            panic!("Error on genesis block: {}", e);
        }

        blockchain
    }

    // function to include the genesis block and register the public dev key.
    fn create_genesis_block(&mut self) -> Result<(), BlockchainError> {
        if GENESIS_BLOCK.len() != 0 {
            println!("De-serializing genesis block...");
            match CompleteBlock::from_hex(GENESIS_BLOCK.to_owned()) {
                Ok(block) => {
                    let dev_address = self.dev_address.clone();
                    if *block.get_miner() != dev_address {
                        panic!("Genesis block is not mined by dev address!");
                    }
                    self.register_account(dev_address);
                    self.add_new_block(*block)?;
                },
                Err(e) => panic!("Error while de-serializing genesis block: {}", e)
            }
        } else {
            println!("No genesis block found...");
        }

        Ok(())
    }

    // mine a block for current difficulty
    pub fn mine_block(&mut self, key: PublicKey) -> Result<(), BlockchainError> {
        let mut block = self.get_block_template(key);
        let mut hash = block.hash();
        while !check_difficulty(&hash, self.get_difficulty())? {
            block.nonce += 1;
            block.timestamp = get_current_time();
            hash = block.hash();
        }

        let complete_block = self.build_complete_block_from_block(block)?;
        self.add_new_block(complete_block)
    }

    pub fn get_height(&self) -> u64 {
        self.height.load(Ordering::Relaxed)
    }

    pub fn get_difficulty(&self) -> u64 {
        self.difficulty.load(Ordering::Relaxed)
    }

    pub fn get_supply(&self) -> u64 {
        self.supply.load(Ordering::Relaxed)
    }

    pub fn get_dev_address(&self) -> &PublicKey {
        &self.dev_address
    }

    pub fn get_top_hash(&self) -> &Hash {
        &self.top_hash
    }

    pub fn get_top_block(&self) -> Result<&CompleteBlock, BlockchainError> {
        self.get_block_at_height(self.get_height() - 1)
    }

    pub fn add_tx_to_mempool(&mut self, tx: Transaction) -> Result<(), BlockchainError> {
        let hash = tx.hash();
        if self.mempool.contains_tx(&hash) {
            return Err(BlockchainError::TxAlreadyInMempool(hash))
        }

        self.verify_transaction_with_hash(&tx, &hash, false)?;
        /*if let Err(e) = self.get_p2p().broadcast_tx(&tx) {
            return Err(BlockchainError::ErrorOnP2p(e))
        }*/

        self.mempool.add_tx(hash, tx)
    }

    pub fn has_account(&self, account: &PublicKey) -> bool {
        self.accounts.contains_key(account)
    }

    pub fn get_account(&self, account: &PublicKey) -> Result<&Account, BlockchainError> {
        match self.accounts.get(account) {
            Some(v) => Ok(v),
            None => Err(BlockchainError::AddressNotRegistered(account.clone()))
        }
    }

    fn register_account(&mut self, pub_key: PublicKey) {
        self.accounts.insert(pub_key, Account {
            balance: 0,
            nonce: 0
        });
    }

    pub fn get_mut_account(&mut self, account: &PublicKey) -> Result<&mut Account, BlockchainError> {
        match self.accounts.get_mut(account) {
            Some(v) => Ok(v),
            None => Err(BlockchainError::AddressNotRegistered(account.clone()))
        }
    }

    pub fn get_mut_dev_address(&mut self) -> Result<&mut Account, BlockchainError> {
        match self.accounts.get_mut(&self.dev_address) {
            Some(v) => Ok(v),
            None => Err(BlockchainError::AddressNotRegistered(self.dev_address.clone()))
        }
    }

    pub fn get_block_template(&self, address: PublicKey) -> Block {
        let coinbase_tx = Transaction::new(0, TransactionData::Coinbase(CoinbaseTx {
            block_reward: get_block_reward(self.get_supply()),
            fee_reward: 0,
        }), address);
        let mut block = Block::new(self.get_height(), get_current_time(), self.top_hash.clone(), self.get_difficulty(), coinbase_tx, vec![]);
        let txs: &Vec<SortedTx> = self.mempool.get_sorted_txs();

        let mut total_fee = 0;
        let mut tx_size = 0;
        let mut index = 0;
        while txs.len() > index && block.size() + tx_size < MAX_BLOCK_SIZE {
            let tx = &txs[index];
            total_fee += tx.get_fee();
            tx_size += tx.get_size();
            block.txs_hashes.push(tx.get_hash().clone());
            index += 1;
        }

        match block.miner_tx.get_mut_data() {
            TransactionData::Coinbase(ref mut data) => {
                data.fee_reward = total_fee;
            },
            _ => {}
        }

        block
    }

    pub fn build_complete_block_from_block(&self, block: Block) -> Result<CompleteBlock, BlockchainError> {
        let mut transactions: Vec<Transaction> = vec![];
        for hash in &block.txs_hashes {
            let tx = self.mempool.view_tx(hash)?; // at this point, we don't want to lose/remove any tx, we clone it only
            transactions.push(tx.clone());
        }
        let complete_block = CompleteBlock::new(block, transactions);
        Ok(complete_block)
    }

    pub fn check_validity(&self) -> Result<(), BlockchainError> {
        if self.get_height() != self.blocks.len() as u64 {
            return Err(BlockchainError::InvalidBlockHeight(self.get_height(), self.blocks.len() as u64))
        }

        let mut circulating_supply = 0;
        for (height, block) in self.blocks.iter().enumerate() {
            let hash = block.hash();
            if block.get_height() != height as u64 {
                println!("Invalid block height for block {}, got {} but expected {}", block, block.get_height(), height);
                return Err(BlockchainError::InvalidBlockHeight(block.get_height(), height as u64))
            }

            if block.get_height() != 0 { // if not genesis, check parent block
                let previous_hash = self.get_block_at_height(block.get_height() - 1)?.hash();
                if previous_hash != *block.get_previous_hash() {
                    println!("Invalid previous block hash, expected {} got {}", previous_hash, block.get_previous_hash());
                    return Err(BlockchainError::InvalidHash(previous_hash, block.get_previous_hash().clone()));
                }
            }

            let txs_len = block.get_transactions().len();
            let txs_hashes_len = block.get_txs_hashes().len();
            if txs_len != txs_hashes_len {
                return Err(BlockchainError::InvalidBlockTxs(txs_hashes_len, txs_len));
            }

            if !check_difficulty(&hash, block.get_difficulty())? {
                return Err(BlockchainError::InvalidDifficulty(block.get_difficulty(), 0))
            }

            let coinbase_tx = match block.get_miner_tx().get_data() {
                TransactionData::Coinbase(tx) => tx,
                _ => return Err(BlockchainError::InvalidMinerTx)
            };
            let reward = get_block_reward(circulating_supply);
            if coinbase_tx.block_reward != reward {
                return Err(BlockchainError::InvalidBlockReward(coinbase_tx.block_reward, reward))
            }

            let mut fees: u64 = 0;
            for tx in block.get_transactions() {
                let tx_hash = tx.hash();
                if !tx.is_coinbase() {
                    self.verify_transaction(tx, true)?;
                } else {
                    return Err(BlockchainError::InvalidTxInBlock(tx_hash))
                }

                if !block.get_txs_hashes().contains(&tx_hash) { // check if tx is in txs hashes
                    return Err(BlockchainError::InvalidTxInBlock(tx_hash))
                }

                fees += tx.get_fee();
            }

            if fees != coinbase_tx.fee_reward {
                return Err(BlockchainError::InvalidBlockReward(coinbase_tx.block_reward + coinbase_tx.fee_reward, reward + fees))
            }

            circulating_supply += reward;
        }

        let mut total_supply_from_accounts = 0;
        for (_, account) in &self.accounts {
            total_supply_from_accounts += account.balance;
        }

        if circulating_supply != self.get_supply() {
            return Err(BlockchainError::InvalidCirculatingSupply(circulating_supply, self.get_supply()));
        }

        if total_supply_from_accounts != self.get_supply() {
            return Err(BlockchainError::InvalidCirculatingSupply(total_supply_from_accounts, self.get_supply()));
        }

        Ok(())
    }

    pub fn add_new_block(&mut self, block: CompleteBlock) -> Result<(), BlockchainError> {
        // TODO Lock it to be sure to have only one block at a time!!
        let current_height = self.get_height();
        let current_difficulty = self.get_difficulty();
        let block_hash = block.hash();
        if current_height != block.get_height() {
            return Err(BlockchainError::InvalidBlockHeight(current_height, block.get_height()));
        } else if current_difficulty != block.get_difficulty() || !check_difficulty(&block_hash, current_difficulty)? {
            return Err(BlockchainError::InvalidDifficulty(current_difficulty, block.get_difficulty()));
        } else if block.get_timestamp() > get_current_time() { // TODO accept a latency of max 30s
            return Err(BlockchainError::TimestampIsInFuture(get_current_time(), block.get_timestamp()));
        } else if current_height != 0 {
            let previous_block = self.get_block_at_height(current_height - 1)?;
            let previous_hash = previous_block.hash();
            if previous_hash != *block.get_previous_hash() {
                return Err(BlockchainError::InvalidPreviousBlockHash(block.get_previous_hash().clone(), previous_hash));
            }
            if previous_block.get_timestamp() > block.get_timestamp() { // block timestamp can't be less than previous block.
                return Err(BlockchainError::TimestampIsLessThanParent(block.get_timestamp()));
            }
            println!("Block Time for this block is: {}s", block.get_timestamp() - previous_block.get_timestamp());
        }

        let mut total_fees: u64 = 0;
        let mut total_tx_size: usize = 0;
        {// Transaction verification
            let hashes_len = block.get_txs_hashes().len();
            let txs_len = block.get_transactions().len();
            if  hashes_len != txs_len {
                return Err(BlockchainError::InvalidBlockTxs(hashes_len, txs_len));
            }
            let mut cache_tx: HashMap<Hash, bool> = HashMap::new(); // avoid using a TX multiple times
            let mut registrations: HashMap<&PublicKey, bool> = HashMap::new(); // avoid multiple registration of the same public key 
            for tx in block.get_transactions() {
                let tx_hash = tx.hash();
                // block can't contains the same tx and should have tx hash in block header
                if cache_tx.contains_key(&tx_hash) {
                    return Err(BlockchainError::TxAlreadyInBlock(tx_hash));
                }

                if !block.get_txs_hashes().contains(&tx_hash) {
                    return Err(BlockchainError::InvalidTxInBlock(tx_hash))
                }

                match tx.get_data() {
                    TransactionData::Coinbase(_) => {
                        return Err(BlockchainError::InvalidTxInBlock(tx_hash))
                    }
                    TransactionData::Registration => {
                        if registrations.contains_key(tx.get_sender()) {
                            return Err(BlockchainError::DuplicateRegistration(tx.get_sender().clone()))
                        }
                        registrations.insert(tx.get_sender(), true);
                    }
                    _ => {}
                };

                self.verify_transaction_with_hash(tx, &tx_hash, false)?;
                cache_tx.insert(tx_hash, true);
                total_fees += tx.get_fee();
                total_tx_size += tx.size();
            }

            if block.size() + total_tx_size > MAX_BLOCK_SIZE {
                return Err(BlockchainError::InvalidBlockSize(MAX_BLOCK_SIZE, block.size() + total_tx_size));
            }

            if cache_tx.len() != block.get_transactions().len() || cache_tx.len() != block.get_txs_hashes().len() {
                return Err(BlockchainError::InvalidBlockTxs(block.get_txs_hashes().len(), cache_tx.len()))
            }
        }

        // Miner Tx verification
        let block_reward = get_block_reward(self.get_supply());
        match block.get_miner_tx().get_data() {
            TransactionData::Coinbase(data) => { // reward contains block reward + fees from all txs included in this block
                if !self.has_account(block.get_miner_tx().get_sender()) {
                    return Err(BlockchainError::AddressNotRegistered(block.get_miner_tx().get_sender().clone()));
                }

                if block.get_miner_tx().get_fee() != 0 { // coinbase tx don't pay fee, if we have fee, they try to generate unauthorized coins
                    return Err(BlockchainError::InvalidTxFee(0, block.get_miner_tx().get_fee()))
                }

                if block.get_miner_tx().has_signature() { // Coinbase tx should not be signed (there is no sender, why signing it ?)
                    return Err(BlockchainError::InvalidTransactionSignature)
                }

                if data.block_reward != block_reward || data.fee_reward != total_fees {
                    return Err(BlockchainError::InvalidBlockReward(block_reward + total_fees, data.block_reward + data.fee_reward))
                }

                if data.fee_reward != total_fees {
                    return Err(BlockchainError::InvalidFeeReward(total_fees, data.fee_reward))
                }
            }
            _ => {
                return Err(BlockchainError::InvalidMinerTx)
            }
        }

        // Transaction execution
        for hash in block.get_txs_hashes() { // remove all txs present in mempool
            match self.mempool.remove_tx(hash) {
                Ok(_) => {
                    println!("Removing tx hash '{}' from mempool", hash);
                },
                Err(_) => {}
            };
        }

        for tx in block.get_transactions() { // execute all txs
            self.execute_transaction(tx)?;
        }
        self.execute_transaction(block.get_miner_tx())?; // execute coinbase tx

        let current_height = self.get_height();
        if current_height > 2 { // re calculate difficulty
            let difficulty = calculate_difficulty(self.get_block_at_height(current_height - 1)?, &block);
            self.difficulty.store(difficulty, Ordering::Relaxed);
        }

        if current_height > 0 {
            let block_time = block.get_timestamp() - self.get_top_block()?.get_timestamp();
            println!("Average block time ({}): {}s", self.get_height(), block_time);
        }

        if current_height != 0 {
            if let Err(e) = self.p2p.broadcast_block(&block) { // Broadcast block to other nodes
                println!("Error while broadcasting block: {}", e);
            }
        }

        println!("Adding new block '{}' at height {}", block_hash, current_height + 1);
        self.height.store(current_height + 1, Ordering::Relaxed);
        self.top_hash = block_hash;
        self.supply.fetch_add(block_reward, Ordering::Relaxed);
        self.blocks.push(block); // Add block to chain
        Ok(())
    }

    fn verify_transaction(&self, tx: &Transaction, disable_nonce_check: bool) -> Result<(), BlockchainError> {
        self.verify_transaction_with_hash(tx, &tx.hash(), disable_nonce_check)
    }

    fn verify_transaction_with_hash(&self, tx: &Transaction, hash: &Hash, disable_nonce_check: bool) -> Result<(), BlockchainError> {
        if tx.require_signature() && (!tx.has_signature() || !tx.verify_signature()) { // signature verification for tx types required
            return Err(BlockchainError::InvalidTransactionSignature)
        }

        let is_registration = tx.is_registration();
        if is_registration || tx.is_coinbase() {
            if tx.get_fee() != 0 { // coinbase & registration tx cannot have fee
                return Err(BlockchainError::InvalidTxFee(0, tx.get_fee()))
            }
        } else {
            let fee = calculate_tx_fee(tx.size());
            if tx.get_fee() < fee { // minimum fee verification
                return Err(BlockchainError::InvalidTxFee(fee, tx.get_fee()))
            }
        }

        if is_registration {
            if self.has_account(tx.get_sender()) && !disable_nonce_check {
                return Err(BlockchainError::AddressAlreadyRegistered(tx.get_sender().clone()))
            }

            if !check_difficulty(&hash, REGISTRATION_DIFFICULTY)? {
                return Err(BlockchainError::InvalidTxRegistrationPoW(hash.clone()))
            }

            return Ok(())
        }

        let account = self.get_account(tx.get_sender())?;
        if !disable_nonce_check && account.nonce != tx.get_nonce() {
            return Err(BlockchainError::InvalidTransactionNonce(account.nonce, tx.get_nonce()))
        }

        match tx.get_data() {
            TransactionData::Normal(txs) => {
                if txs.len() == 0 {
                    return Err(BlockchainError::TxEmpty(hash.clone()))
                }
                let mut total_coins = tx.get_fee();
                for output in txs {
                    total_coins += output.amount;
                    if output.to == *tx.get_sender() { // we can't transfer coins to ourself, why would you do that ?
                        return Err(BlockchainError::InvalidTransactionToSender(hash.clone()))
                    }

                    if !self.has_account(&output.to) { // verify that all receivers are registered
                        return Err(BlockchainError::AddressNotRegistered(output.to.clone()))
                    }
                }

                if account.balance < total_coins {
                    return Err(BlockchainError::NotEnoughFunds(tx.get_sender().clone(), total_coins))
                }
            }
            TransactionData::Burn(amount) => {
                if account.balance < amount + tx.get_fee() {
                    return Err(BlockchainError::NotEnoughFunds(tx.get_sender().clone(), amount + tx.get_fee()))
                }
            }
            TransactionData::Coinbase(_) => {
                return Err(BlockchainError::CoinbaseTxNotAllowed(hash.clone()));
            }
            _ => {
                panic!("Not implemented yet")
            }
        };

        Ok(())
    }

    fn execute_transaction(&mut self, transaction: &Transaction) -> Result<(), BlockchainError> {
        let mut amount = 0;
        match transaction.get_data() {
            TransactionData::Burn(burn_amount) => {
                amount += burn_amount + transaction.get_fee();
                //self.supply = self.supply - burn_amount; // by burning an amount, this amount can still be regenerated through block reward, should we prevent this ?
            }
            TransactionData::Normal(txs) => {
                let mut total = transaction.get_fee();
                for tx in txs {
                    let to_account = self.get_mut_account(&tx.to)?;
                    to_account.balance += tx.amount;
                    total += tx.amount;
                }

                amount += total;
            }
            TransactionData::Registration => {
                self.register_account(transaction.get_sender().clone());

                return Ok(())
            }
            TransactionData::Coinbase(data) => {
                let mut block_reward = data.block_reward;
                if DEV_FEE_PERCENT != 0 {
                    let dev_fee = block_reward * DEV_FEE_PERCENT / 100;
                    let account = self.get_mut_dev_address()?;
                    account.balance += dev_fee;
                    block_reward -= dev_fee;
                }

                let account = self.get_mut_account(transaction.get_sender())?;
                account.balance += block_reward + data.fee_reward;

                return Ok(()) // return now to prevent the nonce increment
            }
            _ => {
                panic!("not implemented")
            }
        };

        let account = self.get_mut_account(transaction.get_sender())?;
        account.balance -= amount;
        account.nonce += 1;

        Ok(())
    }

    pub fn get_block_at_height(&self, height: u64) -> Result<&CompleteBlock, BlockchainError> {
        if height > self.get_height() {
            return Err(BlockchainError::InvalidBlockHeight(self.get_height(), height))
        }

        Ok(&self.blocks[height as usize]) // TODO
    }

    pub fn get_block_by_hash(&self, hash: &Hash) -> Result<&CompleteBlock, BlockchainError> {
        for block in &self.blocks {
            if block.hash() == *hash {
                return Ok(&block)
            }
        }

        Err(BlockchainError::BlockNotFound(hash.clone()))
    }
}

pub fn get_block_reward(supply: u64) -> u64 {
    let base_reward = (MAX_SUPPLY - supply) >> EMISSION_SPEED_FACTOR;
    base_reward
}

pub fn calculate_tx_fee(tx_size: usize) -> u64 {
    let mut size_in_kb = tx_size as u64 / 1024;

    if tx_size % 1024 != 0 { //we consume a full kb for fee
        size_in_kb += 1;
    }
    
    size_in_kb * FEE_PER_KB
}

use std::fmt::{Display, Error, Formatter};

impl Display for Blockchain {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), Error> {
        write!(f, "Blockchain[height: {}, top_hash: {}, accounts: {}, supply: {}]", self.get_height(), self.top_hash, self.accounts.len(), self.get_supply())
    }
}