// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

#![feature(plugin)]
#![cfg_attr(feature = "dev", plugin(clippy))]
#![cfg_attr(not(feature = "dev"), allow(unknown_lints))]
#![allow(needless_pass_by_value)]

extern crate tikv;
extern crate clap;
extern crate protobuf;
extern crate kvproto;
extern crate rocksdb;
extern crate grpcio;
extern crate futures;
#[cfg(test)]
extern crate tempdir;
extern crate rustc_serialize;

use std::{process, str, u64};
use std::iter::FromIterator;
use std::error::Error;
use std::sync::Arc;
use std::path::PathBuf;
use rustc_serialize::hex::{FromHex, ToHex};

use clap::{App, Arg, SubCommand};
use protobuf::Message;
use futures::{future, stream, Future, Stream};
use grpcio::{ChannelBuilder, Environment};
use protobuf::RepeatedField;
use protobuf::text_format::print_to_string;

use kvproto::raft_cmdpb::RaftCmdRequest;
use kvproto::raft_serverpb::PeerState;
use kvproto::eraftpb::Entry;
use kvproto::kvrpcpb::MvccInfo;
use kvproto::debugpb::*;
use kvproto::debugpb::DB as DBType;
use kvproto::debugpb_grpc::DebugClient;
use tikv::util::{self, escape, unescape};
use tikv::raftstore::store::{keys, Engines};
use tikv::raftstore::store::debug::{Debugger, RegionInfo};
use tikv::storage::{ALL_CFS, CF_DEFAULT, CF_LOCK, CF_WRITE};
use tikv::pd::{PdClient, RpcClient};

fn perror_and_exit<E: Error, T>(e: E) -> T {
    eprintln!("{}", e);
    process::exit(-1);
}

fn new_debug_executor(
    db: Option<&str>,
    raft_db: Option<&str>,
    host: Option<&str>,
) -> Box<DebugExecutor> {
    match (host, db) {
        (None, Some(kv_path)) => {
            let db = util::rocksdb::open(kv_path, ALL_CFS).unwrap();
            let raft_db = if let Some(raft_path) = raft_db {
                util::rocksdb::open(raft_path, &[CF_DEFAULT]).unwrap()
            } else {
                let raft_path = PathBuf::from(kv_path).join("../raft");
                util::rocksdb::open(raft_path.to_str().unwrap(), &[CF_DEFAULT]).unwrap()
            };
            Box::new(Debugger::new(Engines::new(Arc::new(db), Arc::new(raft_db)))) as
                Box<DebugExecutor>
        }
        (Some(remote), None) => {
            let env = Arc::new(Environment::new(1));
            let channel = ChannelBuilder::new(env).connect(remote);
            let client = DebugClient::new(channel);
            Box::new(client) as Box<DebugExecutor>
        }
        _ => unreachable!(),
    }
}

trait DebugExecutor {
    fn dump_value(&self, cf: &str, key: Vec<u8>) {
        let value = self.get_value_by_key(cf, key);
        println!("value: {}", escape(&value));
    }

    fn dump_region_size(&self, region: u64, cfs: Vec<&str>) -> usize {
        let sizes = self.get_region_size(region, cfs);
        let mut total_size = 0;
        println!("region id: {}", region);
        for (cf, size) in sizes {
            println!("cf {} region size: {}", cf, convert_gbmb(size as u64));
            total_size += size;
        }
        total_size
    }

    fn dump_all_region_size(&self, cfs: Vec<&str>) {
        let regions = self.get_all_meta_regions();
        let regions_number = regions.len();
        let mut total_size = 0;
        for region in regions {
            total_size += self.dump_region_size(region, cfs.clone());
        }
        println!("total region number: {}", regions_number);
        println!("total region size: {}", convert_gbmb(total_size as u64));
    }

    fn dump_region_info(&self, region: u64, skip_tombstone: bool) {
        let r = self.get_region_info(region);
        if skip_tombstone {
            let region_state = r.region_local_state.as_ref();
            if region_state.map_or(false, |s| s.get_state() == PeerState::Tombstone) {
                return;
            }
        }
        let region_state_key = keys::region_state_key(region);
        let raft_state_key = keys::raft_state_key(region);
        let apply_state_key = keys::apply_state_key(region);
        println!("region id: {}", region);
        println!("region state key: {}", escape(&region_state_key));
        println!("region state: {:?}", r.region_local_state);
        println!("raft state key: {}", escape(&raft_state_key));
        println!("raft state: {:?}", r.raft_local_state);
        println!("apply state key: {}", escape(&apply_state_key));
        println!("apply state: {:?}", r.raft_apply_state);
    }

    fn dump_all_region_info(&self, skip_tombstone: bool) {
        for region in self.get_all_meta_regions() {
            self.dump_region_info(region, skip_tombstone);
        }
    }

    fn dump_raft_log(&self, region: u64, index: u64) {
        let idx_key = keys::raft_log_key(region, index);
        println!("idx_key: {}", escape(&idx_key));
        println!("region: {}", region);
        println!("log index: {}", index);

        let mut entry = self.get_raft_log(region, index);
        let data = entry.take_data();
        println!("entry {:?}", entry);
        println!("msg len: {}", data.len());

        let mut msg = RaftCmdRequest::new();
        msg.merge_from_bytes(&data).unwrap();
        println!("{:?}", msg);
    }

    fn dump_mvccs_infos(
        &self,
        from: Vec<u8>,
        to: Option<Vec<u8>>,
        limit: Option<u64>,
        cfs: Vec<&str>,
        start_ts: Option<u64>,
        commit_ts: Option<u64>,
    ) {
        let to = to.unwrap_or_default();
        let limit = limit.unwrap_or_default();
        if to.is_empty() && limit == 0 {
            eprintln!(r#"please pass "to" or "limit""#);
            process::exit(-1);
        }
        if !to.is_empty() && to < from {
            eprintln!("The region's from pos must greater than the to pos.");
            process::exit(-1);
        }
        let scan_future = self.get_mvcc_infos(from, to, limit).for_each(
            move |(key, mut mvcc)| {
                println!("key: {}", escape(&key));
                if cfs.contains(&CF_LOCK) && mvcc.has_lock() {
                    let mut lock_info = mvcc.take_lock();
                    if start_ts.map_or(true, |ts| lock_info.get_lock_version() == ts) {
                        // FIXME: "lock type" is lost in kvproto.
                        let pk = escape(lock_info.get_primary_lock()).into_bytes();
                        let k = escape(lock_info.get_key()).into_bytes();
                        lock_info.set_primary_lock(pk);
                        lock_info.set_key(k);
                        println!("\tlock cf value: {}", print_to_string(&lock_info));
                    }
                }
                if cfs.contains(&CF_DEFAULT) {
                    for mut value_info in mvcc.take_values().into_iter() {
                        if commit_ts.map_or(true, |ts| value_info.get_ts() == ts) {
                            let value = escape(value_info.get_value()).into_bytes();
                            value_info.set_value(value);
                            println!("\tdefault cf value: {}", print_to_string(&value_info));
                        }
                    }
                }
                if cfs.contains(&CF_WRITE) {
                    for write_info in mvcc.take_writes().into_iter() {
                        if start_ts.map_or(true, |ts| write_info.get_start_ts() == ts) &&
                            commit_ts.map_or(true, |ts| write_info.get_commit_ts() == ts)
                        {
                            // FIXME: short_value is lost in kvproto.
                            println!("\t write cf value: {}", print_to_string(&write_info));
                        }
                    }
                }
                println!("");
                future::ok::<(), String>(())
            },
        );
        if let Err(e) = scan_future.wait() {
            eprintln!("{}", e);
            process::exit(-1);
        }
    }

    fn diff_region(
        &self,
        region: u64,
        db: Option<&str>,
        raft_db: Option<&str>,
        host: Option<&str>,
    ) {
        let rhs_debug_executor = new_debug_executor(db, raft_db, host);

        let r1 = self.get_region_info(region);
        let r2 = rhs_debug_executor.get_region_info(region);
        println!("region id: {}", region);
        println!("db1 region state: {:?}", r1.region_local_state);
        println!("db2 region state: {:?}", r2.region_local_state);
        println!("db1 apply state: {:?}", r1.raft_apply_state);
        println!("db2 apply state: {:?}", r2.raft_apply_state);

        match (r1.region_local_state, r2.region_local_state) {
            (None, None) => return,
            (Some(_), None) | (None, Some(_)) => {
                println!("db1 and db2 don't have same region local_state");
                return;
            }
            (Some(region_local_1), Some(region_local_2)) => {
                let start_key = keys::data_key(region_local_1.get_region().get_start_key());
                let end_key = keys::data_key(region_local_1.get_region().get_end_key());
                if start_key != keys::data_key(region_local_2.get_region().get_start_key()) ||
                    end_key != keys::data_key(region_local_2.get_region().get_end_key())
                {
                    println!("db1' range doesn't equal to db2' range.");
                    return;
                }
                let mvcc_infos_1 = self.get_mvcc_infos(start_key.clone(), end_key.clone(), 0);
                let mvcc_infos_2 = rhs_debug_executor.get_mvcc_infos(start_key, end_key, 0);
                let cmp_future = mvcc_infos_1.zip(mvcc_infos_2).for_each(|(item_1, item_2)| {
                    let (key1, mvcc1) = (escape(&item_1.0), item_1.1);
                    let (key2, mvcc2) = (escape(&item_2.0), item_2.1);
                    if key1 != key2 {
                        let err_msg = format!("db1 cur key: {}, db2 cur key: {}", key1, key2);
                        return future::err::<(), String>(err_msg);
                    }
                    if mvcc1 != mvcc2 {
                        let err_msg = format!(
                            "db1 key: {}, mvcc: {}\ndb2 key: {}, mvcc: {}",
                            key1,
                            print_to_string(&mvcc1),
                            key2,
                            print_to_string(&mvcc1),
                        );
                        return future::err::<(), String>(err_msg);
                    }
                    future::ok::<(), String>(())
                });
                match cmp_future.wait() {
                    Err(msg) => println!("{}", msg),
                    Ok(_) => println!("The region of db1 and db2 are equal."),
                }
            }
        }
    }

    fn compact(&self, db: DBType, cf: &str, from: Option<Vec<u8>>, to: Option<Vec<u8>>) {
        let from = from.unwrap_or_default();
        let to = to.unwrap_or_default();
        self.do_compact(db, cf, from, to);
    }

    fn set_region_tombstone(&self, region: u64, endpoints: Vec<String>) {
        let debugger = self.get_local_debugger().unwrap_or_else(|| {
            eprintln!("This command is only for local mode");
            process::exit(-1);
        });
        match RpcClient::new(&endpoints)
            .unwrap_or_else(perror_and_exit)
            .get_region_by_id(region)
            .wait()
            .unwrap_or_else(perror_and_exit)
        {
            Some(mut meta_region) => {
                let epoch = meta_region.take_region_epoch();
                let peers = meta_region.take_peers();
                debugger
                    .set_region_tombstone(region, epoch, peers)
                    .unwrap_or_else(perror_and_exit);
            }
            None => {
                eprintln!("no such region in pd: {}", region);
                process::exit(-1);
            }
        }
    }

    fn get_local_debugger(&self) -> Option<&Debugger>;

    fn get_all_meta_regions(&self) -> Vec<u64>;

    fn get_value_by_key(&self, cf: &str, key: Vec<u8>) -> Vec<u8>;

    fn get_region_size(&self, region: u64, cfs: Vec<&str>) -> Vec<(String, usize)>;

    fn get_region_info(&self, region: u64) -> RegionInfo;

    fn get_raft_log(&self, region: u64, index: u64) -> Entry;

    fn get_mvcc_infos(
        &self,
        from: Vec<u8>,
        to: Vec<u8>,
        limit: u64,
    ) -> Box<Stream<Item = (Vec<u8>, MvccInfo), Error = String>>;

    fn do_compact(&self, db: DBType, cf: &str, from: Vec<u8>, to: Vec<u8>);
}


impl DebugExecutor for DebugClient {
    fn get_local_debugger(&self) -> Option<&Debugger> {
        unimplemented!();
    }

    fn get_all_meta_regions(&self) -> Vec<u64> {
        unimplemented!();
    }

    fn get_value_by_key(&self, cf: &str, key: Vec<u8>) -> Vec<u8> {
        let mut req = GetRequest::new();
        req.set_db(DBType::KV);
        req.set_cf(cf.to_owned());
        req.set_key(key);
        let mut resp = self.get(req).unwrap_or_else(perror_and_exit);
        resp.take_value()
    }

    fn get_region_size(&self, region: u64, cfs: Vec<&str>) -> Vec<(String, usize)> {
        let cfs = cfs.into_iter().map(|s| s.to_owned()).collect();
        let mut req = RegionSizeRequest::new();
        req.set_cfs(RepeatedField::from_vec(cfs));
        req.set_region_id(region);
        let mut resp = self.region_size(req).unwrap_or_else(perror_and_exit);
        resp.take_entries()
            .into_iter()
            .map(|mut entry| (entry.take_cf(), entry.get_size() as usize))
            .collect()
    }

    fn get_region_info(&self, region: u64) -> RegionInfo {
        let mut req = RegionInfoRequest::new();
        req.set_region_id(region);
        let mut resp = self.region_info(req).unwrap_or_else(perror_and_exit);

        let mut region_info = RegionInfo::default();
        if resp.has_raft_local_state() {
            region_info.raft_local_state = Some(resp.take_raft_local_state());
        }
        if resp.has_raft_apply_state() {
            region_info.raft_apply_state = Some(resp.take_raft_apply_state());
        }
        if resp.has_region_local_state() {
            region_info.region_local_state = Some(resp.take_region_local_state());
        }
        region_info
    }

    fn get_raft_log(&self, region: u64, index: u64) -> Entry {
        let mut req = RaftLogRequest::new();
        req.set_region_id(region);
        req.set_log_index(index);
        let mut resp = self.raft_log(req).unwrap_or_else(perror_and_exit);
        resp.take_entry()
    }

    fn get_mvcc_infos(
        &self,
        from: Vec<u8>,
        to: Vec<u8>,
        limit: u64,
    ) -> Box<Stream<Item = (Vec<u8>, MvccInfo), Error = String>> {
        let mut req = ScanMvccRequest::new();
        req.set_from_key(from);
        req.set_to_key(to);
        req.set_limit(limit);
        Box::new(
            self.scan_mvcc(req)
                .map_err(|e| e.to_string())
                .map(|mut resp| (resp.take_key(), resp.take_info())),
        ) as Box<Stream<Item = (Vec<u8>, MvccInfo), Error = String>>
    }

    fn do_compact(&self, db: DBType, cf: &str, from: Vec<u8>, to: Vec<u8>) {
        let mut req = CompactRequest::new();
        req.set_db(db);
        req.set_cf(cf.to_owned());
        req.set_from_key(from);
        req.set_to_key(to);
        self.compact(req).unwrap_or_else(perror_and_exit);
        println!("success!");
    }
}

impl DebugExecutor for Debugger {
    fn get_local_debugger(&self) -> Option<&Debugger> {
        Some(self)
    }

    fn get_all_meta_regions(&self) -> Vec<u64> {
        self.get_all_meta_regions().unwrap_or_else(perror_and_exit)
    }

    fn get_value_by_key(&self, cf: &str, key: Vec<u8>) -> Vec<u8> {
        self.get(DBType::KV, cf, &key)
            .unwrap_or_else(perror_and_exit)
    }

    fn get_region_size(&self, region: u64, cfs: Vec<&str>) -> Vec<(String, usize)> {
        self.region_size(region, cfs)
            .unwrap_or_else(perror_and_exit)
            .into_iter()
            .map(|(cf, size)| (cf.to_owned(), size as usize))
            .collect()
    }

    fn get_region_info(&self, region: u64) -> RegionInfo {
        self.region_info(region).unwrap_or_else(perror_and_exit)
    }

    fn get_raft_log(&self, region: u64, index: u64) -> Entry {
        self.raft_log(region, index).unwrap_or_else(perror_and_exit)
    }

    fn get_mvcc_infos(
        &self,
        from: Vec<u8>,
        to: Vec<u8>,
        limit: u64,
    ) -> Box<Stream<Item = (Vec<u8>, MvccInfo), Error = String>> {
        let iter = self.scan_mvcc(&from, &to, limit)
            .unwrap_or_else(perror_and_exit);
        #[allow(deprecated)]
        let stream = stream::iter(iter).map_err(|e| e.to_string());
        Box::new(stream) as Box<Stream<Item = (Vec<u8>, MvccInfo), Error = String>>
    }

    fn do_compact(&self, db: DBType, cf: &str, from: Vec<u8>, to: Vec<u8>) {
        self.compact(db, cf, &from, &to)
            .unwrap_or_else(perror_and_exit);
        println!("success!");
    }
}

fn main() {
    let mut app = App::new("TiKV Ctl")
        .author("PingCAP")
        .about(
            "Distributed transactional key value database powered by Rust and Raft",
        )
        .arg(
            Arg::with_name("db")
                .required(true)
                .conflicts_with_all(&["host", "hex-to-escaped", "escaped-to-hex"])
                .long("db")
                .takes_value(true)
                .help("set rocksdb path"),
        )
        .arg(
            Arg::with_name("raftdb")
                .conflicts_with_all(&["host", "hex-to-escaped", "escaped-to-hex"])
                .long("raftdb")
                .takes_value(true)
                .help("set raft rocksdb path"),
        )
        .arg(
            Arg::with_name("host")
                .required(true)
                .conflicts_with_all(&["db", "raftdb", "hex-to-escaped", "escaped-to-hex"])
                .long("host")
                .takes_value(true)
                .help("set remote host"),
        )
        .arg(
            Arg::with_name("hex-to-escaped")
                .conflicts_with("escaped-to-hex")
                .long("to-escaped")
                .takes_value(true)
                .help("convert hex key to escaped key"),
        )
        .arg(
            Arg::with_name("escaped-to-hex")
                .conflicts_with("hex-to-escaped")
                .long("to-hex")
                .takes_value(true)
                .help("convert escaped key to hex key"),
        )
        .subcommand(
            SubCommand::with_name("raft")
                .about("print raft log entry")
                .subcommand(
                    SubCommand::with_name("log")
                        .about("print the raft log entry info")
                        .arg(
                            Arg::with_name("region")
                                .required_unless("key")
                                .conflicts_with("key")
                                .short("r")
                                .takes_value(true)
                                .help("set the region id"),
                        )
                        .arg(
                            Arg::with_name("index")
                                .required_unless("key")
                                .conflicts_with("key")
                                .short("i")
                                .takes_value(true)
                                .help("set the raft log index"),
                        )
                        .arg(
                            Arg::with_name("key")
                                .required_unless_one(&["region", "index"])
                                .conflicts_with_all(&["region", "index"])
                                .short("k")
                                .takes_value(true)
                                .help("set the raw key, in escaped form"),
                        ),
                )
                .subcommand(
                    SubCommand::with_name("region")
                        .about("print region info")
                        .arg(
                            Arg::with_name("region")
                                .short("r")
                                .takes_value(true)
                                .help("set the region id, if not specified, print all regions."),
                        )
                        .arg(
                            Arg::with_name("skip-tombstone")
                                .long("skip-tombstone")
                                .takes_value(false)
                                .help("skip tombstone region."),
                        ),
                ),
        )
        .subcommand(
            SubCommand::with_name("size")
                .about("print region size")
                .arg(
                    Arg::with_name("region")
                        .short("r")
                        .takes_value(true)
                        .help("set the region id, if not specified, print all regions."),
                )
                .arg(
                    Arg::with_name("cf")
                        .short("c")
                        .takes_value(true)
                        .multiple(true)
                        .use_delimiter(true)
                        .require_delimiter(true)
                        .value_delimiter(",")
                        .default_value("default,write,lock")
                        .help("set the cf name, if not specified, print all cf."),
                ),
        )
        .subcommand(
            SubCommand::with_name("scan")
                .about("print the range db range")
                .arg(
                    Arg::with_name("from")
                        .short("f")
                        .long("from")
                        .takes_value(true)
                        .help("set the scan from raw key, in escaped format"),
                )
                .arg(
                    Arg::with_name("to")
                        .short("t")
                        .long("to")
                        .takes_value(true)
                        .help("set the scan end raw key, in escaped format"),
                )
                .arg(
                    Arg::with_name("limit")
                        .long("limit")
                        .takes_value(true)
                        .help("set the scan limit"),
                )
                .arg(
                    Arg::with_name("start_ts")
                        .long("start-ts")
                        .takes_value(true)
                        .help("set the scan start_ts as filter"),
                )
                .arg(
                    Arg::with_name("commit_ts")
                        .long("commit-ts")
                        .takes_value(true)
                        .help("set the scan commit_ts as filter"),
                )
                .arg(
                    Arg::with_name("cf")
                        .long("cf")
                        .takes_value(true)
                        .multiple(true)
                        .use_delimiter(true)
                        .require_delimiter(true)
                        .value_delimiter(",")
                        .default_value(CF_DEFAULT)
                        .help("column family names, combined from default/lock/write"),
                ),
        )
        .subcommand(
            SubCommand::with_name("print")
                .about("print the raw value")
                .arg(
                    Arg::with_name("cf")
                        .short("c")
                        .takes_value(true)
                        .default_value(CF_DEFAULT)
                        .help("column family name"),
                )
                .arg(
                    Arg::with_name("key")
                        .required(true)
                        .short("k")
                        .takes_value(true)
                        .help("set the query raw key, in escaped form"),
                ),
        )
        .subcommand(
            SubCommand::with_name("mvcc")
                .about("print the mvcc value")
                .arg(
                    Arg::with_name("key")
                        .short("k")
                        .takes_value(true)
                        .help("set the query raw key, in escaped form"),
                )
                .arg(
                    Arg::with_name("cf")
                        .short("c")
                        .takes_value(true)
                        .multiple(true)
                        .use_delimiter(true)
                        .require_delimiter(true)
                        .value_delimiter(",")
                        .default_value(CF_DEFAULT)
                        .help("column family names, combined from default/lock/write"),
                )
                .arg(
                    Arg::with_name("start_ts")
                        .long("start-ts")
                        .takes_value(true)
                        .help("set start_ts as filter"),
                )
                .arg(
                    Arg::with_name("commit_ts")
                        .long("commit-ts")
                        .takes_value(true)
                        .help("set commit_ts as filter"),
                ),
        )
        .subcommand(
            SubCommand::with_name("diff")
                .about("diff two region keys")
                .arg(
                    Arg::with_name("region")
                        .short("r")
                        .takes_value(true)
                        .help("specify region id"),
                )
                .arg(
                    Arg::with_name("to_db")
                        .long("to-db")
                        .takes_value(true)
                        .help("to which db path"),
                )
                .arg(
                    Arg::with_name("to_host")
                        .long("to-host")
                        .takes_value(true)
                        .conflicts_with("to_db")
                        .help("to which remote host"),
                ),
        )
        .subcommand(
            SubCommand::with_name("compact")
                .about("compact a column family in a specified range")
                .arg(
                    Arg::with_name("db")
                        .short("d")
                        .takes_value(true)
                        .default_value("kv")
                        .help("kv or raft"),
                )
                .arg(
                    Arg::with_name("cf")
                        .short("c")
                        .takes_value(true)
                        .default_value(CF_DEFAULT)
                        .help("column family name, only can be default/lock/write"),
                )
                .arg(
                    Arg::with_name("from")
                        .short("f")
                        .long("from")
                        .takes_value(true)
                        .help("set the start raw key, in escaped form"),
                )
                .arg(
                    Arg::with_name("to")
                        .short("t")
                        .long("to")
                        .takes_value(true)
                        .help("set the end raw key, in escaped form"),
                ),
        )
        .subcommand(
            SubCommand::with_name("tombstone")
                .about("set a region on the node to tombstone by manual")
                .arg(
                    Arg::with_name("region")
                        .short("r")
                        .takes_value(true)
                        .help("the target region"),
                )
                .arg(
                    Arg::with_name("pd")
                        .short("p")
                        .takes_value(true)
                        .multiple(true)
                        .use_delimiter(true)
                        .require_delimiter(true)
                        .value_delimiter(",")
                        .help("the pd url"),
                ),
        );
    let matches = app.clone().get_matches();

    let hex_key = matches.value_of("hex-to-escaped");
    let escaped_key = matches.value_of("escaped-to-hex");
    match (hex_key, escaped_key) {
        (Some(hex), None) => {
            println!("{}", escape(&from_hex(hex)));
            return;
        }
        (None, Some(escaped)) => {
            println!("{}", &unescape(escaped).to_hex().to_uppercase());
            return;
        }
        _ => {}
    };

    let db = matches.value_of("db");
    let raft_db = matches.value_of("raftdb");
    let host = matches.value_of("host");

    let debug_executor = new_debug_executor(db, raft_db, host);

    if let Some(matches) = matches.subcommand_matches("print") {
        let cf = matches.value_of("cf").unwrap();
        let key = unescape(matches.value_of("key").unwrap());
        debug_executor.dump_value(cf, key);
    } else if let Some(matches) = matches.subcommand_matches("raft") {
        if let Some(matches) = matches.subcommand_matches("log") {
            let (id, index) = if let Some(key) = matches.value_of("key") {
                keys::decode_raft_log_key(&unescape(key)).unwrap()
            } else {
                let id = matches.value_of("region").unwrap().parse().unwrap();
                let index = matches.value_of("index").unwrap().parse().unwrap();
                (id, index)
            };
            debug_executor.dump_raft_log(id, index);
        } else if let Some(matches) = matches.subcommand_matches("region") {
            let skip_tombstone = matches.is_present("skip-tombstone");
            if let Some(id) = matches.value_of("region") {
                debug_executor.dump_region_info(id.parse().unwrap(), skip_tombstone);
            } else {
                debug_executor.dump_all_region_info(skip_tombstone);
            }
        } else {
            let _ = app.print_help();
        }
    } else if let Some(matches) = matches.subcommand_matches("size") {
        let cfs = Vec::from_iter(matches.values_of("cf").unwrap());
        if let Some(id) = matches.value_of("region") {
            debug_executor.dump_region_size(id.parse().unwrap(), cfs);
        } else {
            debug_executor.dump_all_region_size(cfs);
        }
    } else if let Some(matches) = matches.subcommand_matches("scan") {
        let from = unescape(matches.value_of("from").unwrap());
        let to = matches.value_of("to").map(|to| unescape(to));
        let limit = matches.value_of("limit").map(|s| s.parse().unwrap());
        let cfs = Vec::from_iter(matches.values_of("cf").unwrap());
        let start_ts = matches.value_of("start_ts").map(|s| s.parse().unwrap());
        let commit_ts = matches.value_of("commit_ts").map(|s| s.parse().unwrap());
        debug_executor.dump_mvccs_infos(from, to, limit, cfs, start_ts, commit_ts);
    } else if let Some(matches) = matches.subcommand_matches("mvcc") {
        let from = unescape(matches.value_of("key").unwrap());
        let cfs = Vec::from_iter(matches.values_of("cf").unwrap());
        let start_ts = matches.value_of("start_ts").map(|s| s.parse().unwrap());
        let commit_ts = matches.value_of("commit_ts").map(|s| s.parse().unwrap());
        debug_executor.dump_mvccs_infos(from, None, Some(1), cfs, start_ts, commit_ts);
    } else if let Some(matches) = matches.subcommand_matches("diff") {
        let region = matches.value_of("region").unwrap().parse().unwrap();
        let to_db = matches.value_of("to_db");
        let to_host = matches.value_of("to_host");
        debug_executor.diff_region(region, to_db, None, to_host);
    } else if let Some(matches) = matches.subcommand_matches("compact") {
        let db = matches.value_of("db").unwrap();
        let db_type = if db == "kv" { DBType::KV } else { DBType::RAFT };
        let cf = matches.value_of("cf").unwrap();
        let from_key = matches.value_of("from").map(|k| unescape(k));
        let to_key = matches.value_of("to").map(|k| unescape(k));
        debug_executor.compact(db_type, cf, from_key, to_key);
    } else if let Some(matches) = matches.subcommand_matches("tombstone") {
        let region = matches.value_of("region").unwrap().parse().unwrap();
        let pd_urls = Vec::from_iter(matches.values_of("pd").unwrap().map(|u| u.to_owned()));
        debug_executor.set_region_tombstone(region, pd_urls);
    } else {
        let _ = app.print_help();
    }

}

fn from_hex(key: &str) -> Vec<u8> {
    const HEX_PREFIX: &str = "0x";
    let mut s = String::from(key);
    if s.starts_with(HEX_PREFIX) {
        let len = s.len();
        let new_len = len.saturating_sub(HEX_PREFIX.len());
        s.truncate(new_len);
    }
    s.as_str().from_hex().unwrap()
}

fn convert_gbmb(mut bytes: u64) -> String {
    const GB: u64 = 1024 * 1024 * 1024;
    const MB: u64 = 1024 * 1024;
    if bytes < MB {
        return bytes.to_string();
    }
    let mb = if bytes % GB == 0 {
        String::from("")
    } else {
        format!("{:.3} MB ", (bytes % GB) as f64 / MB as f64)
    };
    bytes /= GB;
    let gb = if bytes == 0 {
        String::from("")
    } else {
        format!("{} GB ", bytes)
    };
    format!("{}{}", gb, mb)
}
