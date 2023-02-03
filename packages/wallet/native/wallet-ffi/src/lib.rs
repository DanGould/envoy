// SPDX-FileCopyrightText: 2022 Foundation Devices Inc.
//
// SPDX-License-Identifier: GPL-3.0-or-later

#[macro_use]
extern crate log;

use std::convert::TryFrom;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::ptr;

extern crate rand;

use std::cell::RefCell;
use std::error::Error;

use bdk::bitcoin::{Address, Network};
use bdk::blockchain::{ConfigurableBlockchain, ElectrumBlockchain, ElectrumBlockchainConfig};
use bdk::database::ConfigurableDatabase;
use bdk::electrum_client::{ConfigBuilder, ElectrumApi, Socks5Config};
use bdk::sled::Tree;
use bdk::wallet::AddressIndex;
use bdk::{electrum_client, SyncOptions};
use bdk::{FeeRate, Wallet};
use payjoin::{PjUri, PjUriExt};
use std::str::FromStr;

use bdk::bitcoin::consensus::encode::deserialize;
use bdk::bitcoin::consensus::encode::serialize;

use std::ptr::null_mut;

use crate::electrum_client::Client;
use bdk::bitcoin::secp256k1::Secp256k1;
use bdk::bitcoin::util::psbt::PartiallySignedTransaction;
use bdk::miniscript::psbt::PsbtExt;
use bdk::wallet::tx_builder::TxOrdering;
use bitcoin_hashes::hex::ToHex;
use std::sync::{Mutex, MutexGuard};

#[repr(C)]
pub enum NetworkType {
    Mainnet,
    Testnet,
    Signet,
    Regtest,
}

#[repr(C)]
pub struct Transaction {
    txid: *const c_char,
    received: u64,
    sent: u64,
    fee: u64,
    confirmation_height: u32,
    confirmation_time: u64,
}

#[repr(C)]
pub struct TransactionList {
    transactions_len: u32,
    transactions: *const Transaction,
}

#[repr(C)]
pub struct Seed {
    mnemonic: *const c_char,
    xprv: *const c_char,
    fingerprint: *const c_char,
}

#[repr(C)]
pub struct Psbt {
    sent: u64,
    received: u64,
    fee: u64,
    base64: *const c_char,
    txid: *const c_char,
    raw_tx: *const c_char,
}

#[repr(C)]
pub struct ServerFeatures {
    server_version: *const c_char,
    protocol_min: *const c_char,
    protocol_max: *const c_char,
    pruning: i64,
    genesis_hash: *const u8,
}

thread_local! {
    static LAST_ERROR: RefCell<Option<Box<dyn Error>>> = RefCell::new(None);
}

/// Update the most recent error, clearing whatever may have been there before.
pub fn update_last_error<E: Error + 'static>(err: E) {
    error!("Setting LAST_ERROR: {}", err);

    {
        // Print a pseudo-backtrace for this error, following back each error's
        // cause until we reach the root error.
        let mut cause = err.source();
        while let Some(parent_err) = cause {
            warn!("Caused by: {}", parent_err);
            cause = parent_err.source();
        }
    }

    LAST_ERROR.with(|prev| {
        *prev.borrow_mut() = Some(Box::new(err));
    });
}

/// Retrieve the most recent error, clearing it in the process.
pub fn take_last_error() -> Option<Box<dyn Error>> {
    LAST_ERROR.with(|prev| prev.borrow_mut().take())
}

#[no_mangle]
pub unsafe extern "C" fn wallet_last_error_message() -> *const c_char {
    let last_error = match take_last_error() {
        Some(err) => err,
        None => return CString::new("").unwrap().into_raw(),
    };

    let error_message = last_error.to_string();
    CString::new(error_message).unwrap().into_raw()
}

macro_rules! unwrap_or_return {
    ($a:expr,$b:expr) => {
        match $a {
            Ok(x) => x,
            Err(e) => {
                update_last_error(e);
                return $b;
            }
        }
    };
}

#[no_mangle]
pub unsafe extern "C" fn wallet_init(
    name: *const c_char,
    external_descriptor: *const c_char,
    internal_descriptor: *const c_char,
    data_dir: *const c_char,
    network: NetworkType,
) -> *mut Mutex<Wallet<Tree>> {
    let network = match network {
        NetworkType::Mainnet => Network::Bitcoin,
        NetworkType::Testnet => Network::Testnet,
        NetworkType::Signet => Network::Signet,
        NetworkType::Regtest => Network::Regtest,
    };

    let name = unwrap_or_return!(CStr::from_ptr(name).to_str(), null_mut());
    let external_descriptor =
        unwrap_or_return!(CStr::from_ptr(external_descriptor).to_str(), null_mut());
    let internal_descriptor =
        unwrap_or_return!(CStr::from_ptr(internal_descriptor).to_str(), null_mut());
    let data_dir = unwrap_or_return!(CStr::from_ptr(data_dir).to_str(), null_mut());

    let db_conf = bdk::database::any::SledDbConfiguration {
        path: data_dir.to_string(),
        tree_name: name.to_string(),
    };

    let db = unwrap_or_return!(sled::Tree::from_config(&db_conf), null_mut());

    let wallet = Mutex::new(unwrap_or_return!(
        Wallet::new(external_descriptor, Some(internal_descriptor), network, db),
        null_mut()
    ));

    let wallet_box = Box::new(wallet);
    Box::into_raw(wallet_box)
}

#[no_mangle]
pub unsafe extern "C" fn wallet_drop(wallet: *mut Mutex<Wallet<Tree>>) {
    drop(wallet);
}

#[no_mangle]
pub unsafe extern "C" fn wallet_get_address(wallet: *mut Mutex<Wallet<Tree>>) -> *const c_char {
    let wallet = get_wallet_mutex(wallet).lock().unwrap();

    let address = wallet
        .get_address(AddressIndex::New)
        .unwrap()
        .address
        .to_string();
    CString::new(address).unwrap().into_raw()
}

#[no_mangle]
pub unsafe extern "C" fn wallet_sync(
    wallet: *mut Mutex<Wallet<Tree>>,
    electrum_address: *const c_char,
    tor_port: i32,
) -> bool {
    let wallet = unwrap_or_return!(get_wallet_mutex(wallet).lock(), false);

    let electrum_address = unwrap_or_return!(CStr::from_ptr(electrum_address).to_str(), false);

    let blockchain = unwrap_or_return!(get_electrum_blockchain(tor_port, electrum_address), false);
    unwrap_or_return!(
        wallet.sync(&blockchain, SyncOptions { progress: None }),
        false
    );

    // Successful sync
    true
}

unsafe fn get_wallet_mutex(wallet: *mut Mutex<Wallet<Tree>>) -> &'static mut Mutex<Wallet<Tree>> {
    let wallet = {
        assert!(!wallet.is_null());
        &mut *wallet
    };
    wallet
}

fn get_electrum_blockchain_config(
    tor_port: i32,
    electrum_address: &str,
) -> ElectrumBlockchainConfig {
    if tor_port > 0 {
        ElectrumBlockchainConfig {
            url: electrum_address.parse().unwrap(),
            socks5: Some("127.0.0.1:".to_owned() + &tor_port.to_string()),
            retry: 0,
            timeout: None,
            stop_gap: 50,
            validate_domain: false,
        }
    } else {
        ElectrumBlockchainConfig {
            url: electrum_address.parse().unwrap(),
            socks5: None,
            retry: 0,
            timeout: Some(5),
            stop_gap: 50,
            validate_domain: false,
        }
    }
}

fn get_electrum_blockchain(
    tor_port: i32,
    electrum_address: &str,
) -> Result<ElectrumBlockchain, bdk::Error> {
    let config = get_electrum_blockchain_config(tor_port, electrum_address);
    ElectrumBlockchain::from_config(&config)
}

fn get_electrum_client(
    tor_port: i32,
    electrum_address: &str,
) -> Result<Client, electrum_client::Error> {
    let config: electrum_client::Config;
    if tor_port > 0 {
        let tor_config = Socks5Config {
            addr: "127.0.0.1:".to_owned() + &tor_port.to_string(),
            credentials: None,
        };
        config = ConfigBuilder::new()
            .validate_domain(false)
            .socks5(Some(tor_config))
            .unwrap()
            .build();
    } else {
        config = ConfigBuilder::new()
            .validate_domain(false)
            .socks5(None)
            .unwrap()
            .timeout(Some(5))
            .unwrap()
            .build();
    }

    Client::from_config(electrum_address, config)
}

#[no_mangle]
pub unsafe extern "C" fn wallet_get_balance(wallet: *mut Mutex<Wallet<Tree>>) -> u64 {
    let wallet = get_wallet_mutex(wallet).lock().unwrap();
    let balance = wallet.get_balance().unwrap();
    balance.confirmed + balance.immature + balance.trusted_pending + balance.untrusted_pending
}

#[no_mangle]
pub unsafe extern "C" fn wallet_get_fee_rate(
    electrum_address: *const c_char,
    tor_port: i32,
    target: u16,
) -> f64 {
    let electrum_address = CStr::from_ptr(electrum_address).to_str().unwrap();
    let client = match get_electrum_client(tor_port, electrum_address) {
        Ok(c) => c,
        Err(e) => {
            update_last_error(e);
            return -1.0;
        }
    };

    // BTC per kb
    client.estimate_fee(target as usize).unwrap_or(-1.0)
}

#[no_mangle]
pub unsafe extern "C" fn wallet_get_server_features(
    electrum_address: *const c_char,
    tor_port: i32,
) -> ServerFeatures {
    let error_return = ServerFeatures {
        server_version: ptr::null(),
        protocol_min: ptr::null(),
        protocol_max: ptr::null(),
        pruning: 0,
        genesis_hash: ptr::null(),
    };

    let electrum_address = CStr::from_ptr(electrum_address).to_str().unwrap();
    let client = unwrap_or_return!(
        get_electrum_client(tor_port, electrum_address),
        error_return
    );

    match client.server_features() {
        Ok(f) => {
            let genesis_hash = f.genesis_hash.clone();

            // Freed on Dart side
            std::mem::forget(genesis_hash);

            ServerFeatures {
                server_version: CString::new(f.server_version).unwrap().into_raw(),
                protocol_min: CString::new(f.protocol_min).unwrap().into_raw(),
                protocol_max: CString::new(f.protocol_max).unwrap().into_raw(),
                pruning: f.pruning.unwrap_or(-1),
                genesis_hash: genesis_hash.as_ptr(),
            }
        }
        Err(e) => {
            update_last_error(e);
            error_return
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn wallet_get_transactions(
    wallet: *mut Mutex<Wallet<Tree>>,
) -> TransactionList {
    let wallet = get_wallet_mutex(wallet).lock().unwrap();

    let transactions = wallet.list_transactions(true).unwrap();
    let transactions_len = transactions.len() as u32;

    let mut transactions_vec: Vec<Transaction> = vec![];

    for transaction in transactions {
        let confirmation_height: u32;
        let confirmation_time: u64;

        match transaction.confirmation_time.as_ref() {
            None => {
                confirmation_height = 0;
                confirmation_time = 0;
            }
            Some(_) => {
                confirmation_height = transaction.confirmation_time.as_ref().unwrap().height;
                confirmation_time = transaction.confirmation_time.as_ref().unwrap().timestamp;
            }
        }

        let tx = Transaction {
            txid: CString::new(format!("{}", transaction.txid))
                .unwrap()
                .into_raw(),
            received: transaction.received,
            sent: transaction.sent,
            fee: transaction.fee.unwrap(),
            confirmation_height,
            confirmation_time,
        };

        transactions_vec.push(tx);
    }

    let transactions_box = transactions_vec.into_boxed_slice();
    let txs_ptr = Box::into_raw(transactions_box);

    TransactionList {
        transactions_len,
        transactions: txs_ptr as _,
    }
}

fn psbt_extract_details(wallet: &Wallet<Tree>, psbt: &PartiallySignedTransaction) -> Psbt {
    let tx = psbt.clone().extract_tx();
    let raw_tx = serialize::<bdk::bitcoin::Transaction>(&tx).to_hex();

    let sent = tx
        .output
        .iter()
        .filter(|o| !wallet.is_mine(&o.script_pubkey).unwrap_or(false))
        .map(|o| o.value)
        .sum();

    let received = tx
        .output
        .iter()
        .filter(|o| wallet.is_mine(&o.script_pubkey).unwrap_or(false))
        .map(|o| o.value)
        .sum();

    let inputs_value: u64 = psbt
        .inputs
        .iter()
        .map(|i| match &i.witness_utxo {
            None => 0,
            Some(tx) => tx.value,
        })
        .sum();

    let encoded = base64::encode(&serialize(&psbt));
    let psbt = CString::new(encoded).unwrap().into_raw();

    return Psbt {
        sent,
        received,
        fee: inputs_value - sent - received,
        base64: psbt,
        txid: CString::new(tx.txid().to_hex()).unwrap().into_raw(),
        raw_tx: CString::new(raw_tx).unwrap().into_raw(),
    };
}

#[no_mangle]
pub unsafe extern "C" fn wallet_create_psbt(
    wallet: *mut Mutex<Wallet<Tree>>,
    send_to: *const c_char,
    amount: u64,
    fee_rate: f64,
) -> Psbt {
    use payjoin::UriExt;
    let error_return = Psbt {
        sent: 0,
        received: 0,
        fee: 0,
        base64: ptr::null(),
        txid: ptr::null(),
        raw_tx: ptr::null(),
    };
    let wallet = unwrap_or_return!(get_wallet_mutex(wallet).lock(), error_return);
    let destination  = CStr::from_ptr(send_to).to_str().unwrap();
    let fee_rate = FeeRate::from_sat_per_vb((fee_rate * 100000.0) as f32); // Multiplication here is t convert from BTC/vkb to sat/vb

    match payjoin::Uri::try_from(destination) {
        Ok(uri) => {
            let address = uri.address.clone();
            if let Ok(pj_uri) = uri.check_pj_supported() {
                return create_payjoin(&wallet, pj_uri, amount, fee_rate)
            } else {
                create_transaction(&wallet, address, amount, fee_rate)
            }
        }
        _ => {
            let address = match Address::from_str(destination) {
                Ok(a) => a,
                Err(e) => {
                    update_last_error(e);
                    return error_return;
                }
            };
            create_transaction(&wallet, address, amount, fee_rate)
        }
    }

}

fn create_payjoin(
    wallet: &Wallet<Tree>,
    pj_uri: PjUri<'_>,
    amount: u64,
    fee_rate: FeeRate,
) -> Psbt {
    let error_return = Psbt {
        sent: 0,
        received: 0,
        fee: 0,
        base64: ptr::null(),
        txid: ptr::null(),
        raw_tx: ptr::null(),
    };

    let mut builder = wallet.build_tx();
    builder
        .ordering(TxOrdering::Shuffle)
        .only_witness_utxo()
        .add_recipient(pj_uri.address.script_pubkey(), amount)
        .enable_rbf()
        .fee_rate(fee_rate);

    let (original_psbt, _) = builder.finish().expect("Could not create original psbt");

    // TODO set payjoin optional parameters using fee_rate
    let pj_params = payjoin::sender::Params::non_incentivizing();
    let (req, ctx) = pj_uri.create_pj_request(original_psbt, pj_params).expect("zoinks! can't create request");

    let response = reqwest::blocking::Client::new() // TODO use blocking client?
        .post(req.url)
        .header("Content-Type", "text/plain")
        .body(req.body)
        .send()
        .expect("failed to communicate with payjoin recipient");

    let payjoin_psbt = ctx.process_response(response).unwrap();
    psbt_extract_details(&wallet, &payjoin_psbt)
}

fn create_transaction(wallet: &Wallet<Tree>, send_to: Address, amount: u64, fee_rate: FeeRate) -> Psbt {
    let error_return = Psbt {
        sent: 0,
        received: 0,
        fee: 0,
        base64: ptr::null(),
        txid: ptr::null(),
        raw_tx: ptr::null(),
    };

    let mut builder = wallet.build_tx();
    builder
        .ordering(TxOrdering::Shuffle)
        .only_witness_utxo()
        .add_recipient(send_to.script_pubkey(), amount)
        .enable_rbf()
        .fee_rate(fee_rate);

    match builder.finish() {
        Ok((psbt, _)) => psbt_extract_details(&wallet, &psbt),
        Err(e) => {
            update_last_error(e);
            return error_return;
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn wallet_decode_psbt(
    wallet: *mut Mutex<Wallet<Tree>>,
    psbt: *const c_char,
) -> Psbt {
    let error_return = Psbt {
        sent: 0,
        received: 0,
        fee: 0,
        base64: ptr::null(),
        txid: ptr::null(),
        raw_tx: ptr::null(),
    };

    let wallet = unwrap_or_return!(get_wallet_mutex(wallet).lock(), error_return);
    let data = unwrap_or_return!(
        base64::decode(CStr::from_ptr(psbt).to_str().unwrap()),
        error_return
    );

    match deserialize::<PartiallySignedTransaction>(&data) {
        Ok(psbt) => {
            let secp = Secp256k1::verification_only();
            let finalized_psbt = PsbtExt::finalize(psbt, &secp).unwrap();
            psbt_extract_details(&wallet, &finalized_psbt)
        }
        Err(e) => {
            update_last_error(e);
            error_return
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn wallet_broadcast_tx(
    electrum_address: *const c_char,
    tor_port: i32,
    tx: *const c_char,
) -> *const c_char {
    let error_return = CString::new("").unwrap().into_raw();

    let electrum_address =
        unwrap_or_return!(CStr::from_ptr(electrum_address).to_str(), error_return);
    let client = unwrap_or_return!(
        get_electrum_client(tor_port, electrum_address),
        error_return
    );

    let hex_tx = unwrap_or_return!(CStr::from_ptr(tx).to_str(), error_return);
    let raw_tx = unwrap_or_return!(hex::decode(hex_tx), error_return);

    let tx: bdk::bitcoin::Transaction = unwrap_or_return!(deserialize(&*raw_tx), error_return);
    let txid = unwrap_or_return!(client.transaction_broadcast(&tx), error_return);

    unwrap_or_return!(CString::new(txid.to_string()), error_return).into_raw()
}

#[no_mangle]
pub unsafe extern "C" fn wallet_validate_address(
    wallet: *mut Mutex<Wallet<Tree>>,
    address: *const c_char,
) -> bool {
    let wallet = unwrap_or_return!(get_wallet_mutex(wallet).lock(), false);

    match Address::from_str(CStr::from_ptr(address).to_str().unwrap()) {
        Ok(a) => wallet.network() == a.network, // Only valid if it's on same network
        Err(_) => false,
    }
}

// Due to its simple signature this function is the one added (unused) to iOS swift codebase to force Xcode to link the lib
#[no_mangle]
pub unsafe extern "C" fn wallet_hello() {
    println!("Hello wallet");
}
