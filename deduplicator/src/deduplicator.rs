use std::cmp::max;
use std::fs::File;
use std::mem::drop;
use std::path::PathBuf;
use std::thread;

use crossbeam_channel as channel;
use importer_openaddress::OpenAddress;
use importer_tools::Address;
use itertools::Itertools;
use libflate::gzip::Encoder;
use prog_rs::prelude::*;
use rusqlite::DropBehavior;

use crate::db_hashes::{DbHashes, HashIterItem};
use crate::dedupe::{hash_address, is_duplicate};
use crate::utils::is_constraint_violation_error;

const CHANNEL_SIZES: usize = 100_000;

pub struct Deduplicator {
    db: DbHashes,
}

impl Deduplicator {
    pub fn new(output_path: PathBuf) -> rusqlite::Result<Self> {
        Ok(Self {
            db: DbHashes::new(output_path)?,
        })
    }

    pub fn get_db_inserter<F, R>(&mut self, filter: F, ranking: R) -> rusqlite::Result<DbInserter>
    where
        F: Fn(&Address) -> bool + Clone + Send + 'static,
        R: Fn(&Address) -> f64 + Clone + Send + 'static,
    {
        Ok(DbInserter::new(&self.db, filter, ranking)?)
    }

    pub fn compute_duplicates(&mut self) -> rusqlite::Result<()> {
        println!("Build index on hashes");
        self.db.create_hashes_index()?;

        // --- Query collisions from DB
        let count_addresses_before = self.db.count_addresses()?;
        let count_hashes = self.db.count_hashes()?;

        println!(
            "Compute hash collisions ({} addresses, {} hashes)",
            count_addresses_before, count_hashes
        );

        let conn_get_collisions = self.db.get_conn()?;
        let mut sorted_hashes = DbHashes::get_sorted_hashes(&conn_get_collisions)?;

        // Eliminate false positives in parallel using following pipeline:
        //
        // [     col_sender     ] main thread
        //            |
        //            |  (address, rank)
        //            v
        // [    col_receiver    ]
        // [         |||        ] worker threads
        // [     del_sender     ]
        //            |
        //            |  (address, rank, hashes)
        //            v
        // [    del_receiver    ] writer thread

        let nb_workers = max(3, num_cpus::get()) - 2;
        let (col_sender, col_receiver) = channel::bounded::<Vec<HashIterItem>>(CHANNEL_SIZES);
        let (del_sender, del_receiver) = channel::bounded(CHANNEL_SIZES);

        // --- Init worker threads

        for _ in 0..nb_workers {
            let col_receiver = col_receiver.clone();
            let del_sender = del_sender.clone();

            thread::spawn(move || {
                for mut pack in col_receiver {
                    if pack.len() > 5000 {
                        // In practice this should not happen often, however in the case where this
                        // issue is raised, it would be necessary to implement a specific way of
                        // handling big packs (for example by computing more accurate hashes in
                        // RAM).
                        eprintln!("Performance danger: skipping pack of length {}", pack.len());
                        continue;
                    }

                    // Place items we want to keep the most (ie. with greater rank) at the begining
                    // of the array.
                    pack.sort_unstable_by(|item_1, item_2| {
                        (item_1.rank, item_1.id)
                            .partial_cmp(&(item_2.rank, item_2.id))
                            .unwrap_or_else(|| item_1.id.cmp(&item_2.id))
                            .reverse()
                    });

                    // Keep track of addresses that will not be removed, each address will only be
                    // compared with "first" element of other equivalence classes.
                    let mut kept_items: Vec<_> = pack.first().into_iter().collect();

                    for item in &pack[1..] {
                        let item_is_duplicate = kept_items
                            .iter()
                            .any(|kept| is_duplicate(&item.address, &kept.address));

                        if item_is_duplicate {
                            del_sender.send(item.id).expect(
                                "failed sending id to delete: channel may have closed to early",
                            );
                        } else {
                            kept_items.push(item);
                        }
                    }
                }
            });
        }

        // Drop channels that were cloned before being sent
        drop(col_receiver);
        drop(del_sender);

        // --- Init writer thread

        let mut conn_insert = self.db.get_conn()?;

        let writer_thread = thread::spawn(move || {
            let mut tran_insert = conn_insert
                .transaction()
                .expect("failed to init transaction");
            tran_insert.set_drop_behavior(DropBehavior::Commit);
            let mut inserter =
                DbHashes::get_inserter(&mut tran_insert).expect("failed to init inserter");
            let to_delete: std::collections::HashSet<_> = del_receiver.iter().collect();
            for id in to_delete {
                match inserter.insert_to_delete(id) {
                    Err(err) if !is_constraint_violation_error(&err) => {
                        eprintln!("failed to insert id to delete in the database: {}", err)
                    }
                    _ => (),
                }
            }
        });

        // --- Send conflicting pairs into channels

        // Pack conflicting items together
        let conflicting_packs = sorted_hashes
            .iter()?
            .progress()
            .with_iter_size(count_hashes as usize)
            .filter_map(|item| {
                item.map_err(|err| eprintln!("failed retrieving hash: {}", err))
                    .ok()
            })
            .group_by(|addr| addr.hash);

        // Remove packs of single elements
        let conflicting_packs = conflicting_packs
            .into_iter()
            .map(|(_key, pack)| pack.collect::<Vec<_>>())
            .filter(|pack| pack.len() >= 2);

        for pack in conflicting_packs {
            col_sender
                .send(pack)
                .expect("failed to send collision: channel may have closed too early");
        }

        drop(col_sender);
        writer_thread.join().expect("failed joining writing thread");
        Ok(())
    }

    pub fn apply_and_clean(&self, keep_construction_tables: bool) -> rusqlite::Result<()> {
        println!(
            "Appling deletion ({} addresses)",
            self.db.count_to_delete()?
        );
        self.db.apply_addresses_to_delete()?;

        if !keep_construction_tables {
            println!("Cleaning database");
            self.db.cleanup_database()?;

            println!("Vacuum database");
            self.db.vacuum()?;
        }

        Ok(())
    }

    pub fn openaddress_dump(&self, path: &PathBuf) -> rusqlite::Result<()> {
        // Fetch addresses
        let conn = self.db.get_conn()?;
        let mut addresses = DbHashes::get_addresses(&conn)?;

        // Init dump file
        let file = File::create(path).expect("failed to open dump file");
        let mut encoder = Encoder::new(file).expect("failed to init encoder");

        {
            let mut writer = csv::Writer::from_writer(&mut encoder);

            for address in addresses.iter()? {
                writer
                    .serialize(OpenAddress::from(address?))
                    .unwrap_or_else(|err| eprintln!("failed to write address: {}", err));
            }
        }

        encoder.finish().as_result().expect("failed to end dump");
        Ok(())
    }
}

//  ___                     _   _
// |_ _|_ __  ___  ___ _ __| |_(_) ___  _ __
//  | || '_ \/ __|/ _ \ '__| __| |/ _ \| '_ \
//  | || | | \__ \  __/ |  | |_| | (_) | | | |
// |___|_| |_|___/\___|_|   \__|_|\___/|_| |_|
//
//
// Compute hashes in parallel using following pipeline:
//
// [     addr_sender      ] main thread
//            |
//            |  address
//            v
// [    addr_receiver     ]
// [         |||          ] worker threads
// [     hash_sender      ]
//            |
//            |  (address, rank, hashes)
//            v
// [     hash_receiver    ] writer thread

pub struct DbInserter<'db> {
    db: &'db DbHashes,
    addr_sender: channel::Sender<Address>,
    writer_thread: thread::JoinHandle<()>,
}

impl<'db> DbInserter<'db> {
    pub fn new<F, R>(db: &'db DbHashes, filter: F, ranking: R) -> rusqlite::Result<Self>
    where
        F: Fn(&Address) -> bool + Clone + Send + 'static,
        R: Fn(&Address) -> f64 + Clone + Send + 'static,
    {
        let nb_workers = max(3, num_cpus::get()) - 2;
        let (addr_sender, addr_receiver) = channel::bounded(CHANNEL_SIZES);
        let (hash_sender, hash_receiver) = channel::bounded(CHANNEL_SIZES);

        // --- Init worker threads

        for _ in 0..nb_workers {
            let addr_receiver = addr_receiver.clone();
            let hash_sender = hash_sender.clone();
            let filter = filter.clone();
            let ranking = ranking.clone();

            thread::spawn(move || {
                for address in addr_receiver.into_iter().filter(filter) {
                    let rank = ranking(&address);
                    let hashes: Vec<_> = hash_address(&address).collect();

                    if hashes.is_empty() {
                        eprintln!("found an address that can't be hashed: {:?}", address);
                    }

                    hash_sender
                        .send((address, rank, hashes))
                        .expect("failed sending hashes: channel may have closed too early");
                }
            });
        }

        // --- Init writer thread

        let mut conn = db.get_conn()?;
        let writer_thread = thread::spawn(move || {
            let mut tran = conn.transaction().expect("failed to init transaction");
            tran.set_drop_behavior(DropBehavior::Commit);
            let mut inserter = DbHashes::get_inserter(&mut tran).expect("failed to init inserter");

            for (address, rank, hashes) in hash_receiver {
                let addr_id = inserter.insert_address(&address, rank);

                match addr_id {
                    Ok(addr_id) => {
                        for hash in hashes {
                            inserter
                                .insert_hash(addr_id, hash as i64)
                                .map_err(|err| {
                                    if !is_constraint_violation_error(&err) {
                                        eprintln!("failed inserting hash: {}", err);
                                    }
                                })
                                .ok();
                        }
                    }
                    Err(err) if !is_constraint_violation_error(&err) => {
                        eprintln!("failed inserting address: {}", err);
                    }
                    _ => (),
                }
            }
        });

        Ok(Self {
            db,
            addr_sender,
            writer_thread,
        })
    }
}

impl<'db> Drop for DbInserter<'db> {
    fn drop(&mut self) {
        // Close sender channel, this will end writer threads
        let (closed_sender, _) = channel::unbounded();
        std::mem::replace(&mut self.addr_sender, closed_sender);

        // Wait for writer thread to finish writing
        let writer_thread = std::mem::replace(&mut self.writer_thread, thread::spawn(|| ()));
        writer_thread.join().expect("failed to join writer thread");
    }
}

impl<'db> importer_tools::CompatibleDB for DbInserter<'db> {
    fn flush(&mut self) {}

    fn insert(&mut self, addr: Address) {
        if addr.number.as_ref().map(|num| num == "S/N").unwrap_or(true) {
            // house number is not specified
            return;
        }

        self.addr_sender
            .send(addr)
            .expect("failed sending address: channel may have closed too early");
    }

    fn get_nb_cities(&self) -> i64 {
        self.db.count_cities().expect("failed counting cities")
    }

    fn get_nb_addresses(&self) -> i64 {
        self.db
            .count_addresses()
            .expect("failed counting addresses")
    }

    fn get_nb_errors(&self) -> i64 {
        0
    }

    fn get_nb_by_errors_kind(&self) -> Vec<(String, i64)> {
        Vec::new()
    }

    fn get_address(&self, _: i32, _: &str) -> Vec<Address> {
        Vec::new()
    }
}