#![feature(error_generic_member_access)]

mod args;
mod db;
mod dfiutils;
mod lang;
mod models;

use args::{get_args, verbosity_to_level, Args};
use db::{
    sqlite_begin_tx, sqlite_commit_and_begin_tx, sqlite_commit_tx, sqlite_create_index_factory,
    sqlite_get_stmts, SqliteBlockStore,
};
use dfiutils::{extract_dfi_addresses, token_id_to_symbol_maybe, CliDriver};
use lang::OptionExt;
use lang::Result;
use models::{Block, IcxLogData, IcxTxSet, TxType};
use std::collections::HashMap;
use std::{error::request_ref, io::BufRead};
use tracing::debug;
use tracing::{error, info};

fn run(args: &Args) -> Result<()> {
    let db_path = match args.sqlite_path.is_empty() {
        true => None,
        false => Some(args.sqlite_path.as_str()),
    };
    let defid_log_path = match args.defid_log_path.is_empty() {
        true => None,
        false => Some(args.defid_log_path.as_str()),
    };
    let tx_graph_table = args.enable_graph_table;
    let defid_log_matcher = args.defid_log_matcher.as_str();

    let start_height = args.start_height;
    let end_height = args.end_height;

    info!("{:?}", args);

    let quit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, std::sync::Arc::clone(&quit))?;

    let mut icx_data_map = HashMap::<String, IcxLogData>::default();

    if let Some(defid_log_path) = defid_log_path {
        let file = std::fs::File::open(defid_log_path)?;
        let mut reader = std::io::BufReader::new(file);

        let mut line_buffer = String::new();
        while reader.read_line(&mut line_buffer)? != 0 {
            if line_buffer.contains(defid_log_matcher) {
                // parse the line only on the valid json
                if let Some(json_start) = line_buffer.find('{') {
                    let json_str = &line_buffer[json_start..];
                    if let Ok(icx_data) = serde_json::from_str::<IcxLogData>(json_str) {
                        icx_data_map.insert(icx_data.claim_tx.clone(), icx_data);
                    } else {
                        error!("json parse failure: {}", json_str);
                    }
                }
            }
            line_buffer.clear();
        }

        info!("icx log file entries: {}", icx_data_map.len());
    }

    let mut cli = CliDriver::with_cli_path(args.defi_cli_path.clone());
    let sql_store = SqliteBlockStore::new(db_path)?;

    let chain_height = cli.get_block_count()?;
    let iter_end_height = if chain_height < end_height {
        chain_height
    } else {
        end_height
    };

    let sconn = &sql_store.conn;
    let mut stmts = sqlite_get_stmts(sconn)?;
    sqlite_begin_tx(sconn)?;

    let mut err = Option::None;
    for height in start_height..=iter_end_height {
        // TODO: Abstract this out to a fn so error control is better. For now, handle cli errors
        let hash = match cli.get_block_hash(height) {
            Ok(hash) => hash,
            Err(e) => {
                err = Some(e);
                break;
            }
        };
        let block = match cli.get_block(&hash, Some(4)) {
            Ok(block) => block,
            Err(e) => {
                err = Some(e);
                break;
            }
        };

        let block: Block = serde_json::from_value(block)?;

        debug!("[{}] hash: {}", height, &hash);
        {
            let block_json = serde_json::to_string(&block)?;
            stmts[0].execute(rusqlite::params![height, &hash, &block_json])?;
        }

        for tx in block.tx {
            let tx_in_addrs = dfiutils::get_txin_addr_val_list(&tx.vin, &sql_store)?;
            let tx_out_addrs = dfiutils::get_txout_addr_val_list(&tx, &tx.vout);

            let mut tx_type = tx.vm.as_ref().map(|x| TxType::from(x.txtype.as_ref()));
            let tx_out = tx_out_addrs
                .iter()
                .filter(|x| x.0 != "x") // strip coinbase out
                .cloned()
                .collect::<HashMap<_, _>>();

            let mut dvm_addrs = vec![];

            if tx_in_addrs.is_empty() {
                tx_type = Some(TxType::Coinbase);
            }

            if !matches!(
                &tx_type,
                Some(TxType::Coinbase) | Some(TxType::Unknown) | Some(TxType::Utxo) | None
            ) {
                let dvm_data = tx.vm.as_ref().map(|x| x.msg.to_string()).unwrap();
                dvm_addrs = extract_dfi_addresses(&dvm_data);
            }
            let mut icx_claim_data: Option<IcxTxSet> = None;
            let mut icx_addr = empty();
            let mut icx_amt = empty();
            let mut swap_from = empty();
            let mut swap_to = empty();
            let mut swap_amt = empty();

            match tx_type {
                //  Some(TxType::CompositeSwap) not enabled < 2m.
                Some(TxType::PoolSwap) => {
                    let swap_data = &tx.vm.as_ref().ok_or_err()?.msg;
                    let from_token = swap_data["fromToken"].as_str().ok_or_err()?;
                    let to_token = swap_data["toToken"].as_str().ok_or_err()?;
                    let amt = swap_data["fromAmount"].as_f64().ok_or_err()?;
                    swap_from = token_id_to_symbol_maybe(from_token).to_string();
                    swap_to = token_id_to_symbol_maybe(to_token).to_string();
                    swap_amt = format!("{:.9}", amt);
                }
                Some(TxType::ICXClaimDFCHTLC) => {
                    let icx_data = icx_data_map.get(tx.txid.as_str());
                    if let Some(icx_data) = icx_data {
                        icx_claim_data = Some(IcxTxSet {
                            order_tx: &icx_data.order_tx,
                            claim_tx: &icx_data.claim_tx,
                            offer_tx: &icx_data.offer_tx,
                            dfchtlc_tx: &icx_data.dfchtlc_tx,
                        });
                        icx_addr = icx_data.address.clone();
                        icx_amt = icx_data.amount.clone();
                    }
                }
                _ => {}
            }

            // Transform to final strings. Mostly empty strings for non relevant fields

            let tx_type_str = tx_type.clone().unwrap_or(TxType::Unknown).to_string();
            let dvm_addrs_json = if dvm_addrs.is_empty() {
                empty()
            } else {
                serde_json::to_string(&dvm_addrs)?
            };
            let tx_in_json = if tx_in_addrs.is_empty() {
                empty()
            } else {
                serde_json::to_string(&tx_in_addrs)?
            };
            let tx_out_json = if tx_out_addrs.is_empty() {
                empty()
            } else {
                serde_json::to_string(&tx_out)?
            };
            let tx_json = serde_json::to_string(&tx)?;
            let icx_claim_data = if icx_claim_data.is_none() {
                empty()
            } else {
                serde_json::to_string(&icx_claim_data.unwrap())?
            };

            stmts[1].execute(rusqlite::params![
                &tx.txid,
                height,
                &tx_type_str,
                &tx_in_json,
                &tx_out_json,
                &dvm_addrs_json,
                &tx_json,
                &icx_claim_data,
                &icx_addr,
                &icx_amt,
                &swap_from,
                &swap_to,
                &swap_amt,
            ])?;

            if tx_graph_table {
                // DVM addresses are parsed for all matching addresses inside the
                // DVM data. There is no clean in and out: this requires specific
                // knowledge of each message and there's no clear convention of this.
                // So instead, we workaround this as we know that if tx in and dvm addr
                // is the same, they were _likely_ source.
                // We partition these out first. Later we iterate through the
                // in dvm addresses as well in case no other edges were added. This
                // should cover the case where they were also the dest.

                let txid = &tx.txid;
                let (tx_in_dvm_addrs, tx_out_dvm_addrs): (Vec<_>, Vec<_>) = dvm_addrs
                    .iter()
                    .cloned()
                    .partition(|addr| tx_in_addrs.iter().any(|(in_addr, _)| in_addr == addr));

                let mut changeset = HashMap::new();

                for (out_addr, _) in tx_out_addrs.iter().filter(|x| x.0 != "x") {
                    for (in_addr, _) in tx_in_addrs.iter() {
                        let k = [in_addr.clone(), txid.clone(), out_addr.clone()];
                        changeset.insert(k, 0);
                    }
                }

                let mut dmod = false;
                for out_addr in tx_out_dvm_addrs {
                    for in_addr in tx_in_dvm_addrs.iter() {
                        let k = [out_addr.clone(), txid.clone(), in_addr.clone()];
                        let v = changeset.get_mut(&k);
                        if let Some(v) = v {
                            // we set to DVM + UTXO
                            if *v == 0 {
                                *v = 2;
                                dmod = true;
                            }
                        } else {
                            // we set this with DVM only
                            changeset.insert(k, 1);
                            dmod = true;
                        }
                    }
                }

                if !dmod && !dvm_addrs.is_empty() {
                    // we've not added any dvm addrs despite having them
                    // could imply dvm in_addrs are also the dvm_out_addrs
                    let out_addrs = tx_in_dvm_addrs;

                    for (in_addr, _) in tx_in_addrs.iter() {
                        for out_addr in out_addrs.iter() {
                            let k = [in_addr.clone(), txid.clone(), out_addr.clone()];
                            let v = changeset.get_mut(&k);
                            if let Some(v) = v {
                                if *v == 0 {
                                    *v = 2;
                                }
                            } else {
                                changeset.insert(k, 1);
                            }
                        }
                    }
                }

                for ([in_addr, txid, out_addr], v) in changeset {
                    stmts[2].execute(rusqlite::params![in_addr, txid, out_addr, v])?;
                }
            }
        }

        if height % 10000 == 0 {
            sqlite_commit_and_begin_tx(sconn)?;
            info!("processed: [{}] / [{}]", height, end_height);
        }
        if quit.load(std::sync::atomic::Ordering::Relaxed) {
            info!("int: early exit");
            break;
        }
    }

    info!("flushing db");
    sqlite_commit_tx(sconn)?;

    for (name, indexer) in sqlite_create_index_factory(sconn) {
        if quit.load(std::sync::atomic::Ordering::Relaxed) {
            info!("int: early exit indexes");
            break;
        }
        info!("creating index: {}..", name);
        indexer()?;
    }

    if let Some(e) = err {
        return Err(e);
    }

    info!("done");
    Ok(())
}

// Just a short convenience alias for internal use.
fn empty() -> String {
    String::new()
}

fn main_fallible() -> Result<()> {
    std::env::set_var("RUST_BACKTRACE", "1");
    let args = get_args();
    tracing_subscriber::fmt::fmt()
        .with_max_level(verbosity_to_level(args.verbosity, Some(2)))
        .compact()
        .init();
    run(args)?;

    Ok(())
}

fn main() {
    let res = main_fallible();
    if let Err(e) = res {
        error!("{e}");
        let bt = request_ref::<std::backtrace::Backtrace>(&e);
        if let Some(bt) = bt {
            error!("{bt}");
        }
    }
}
