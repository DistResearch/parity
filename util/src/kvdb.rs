// Copyright 2015, 2016 Ethcore (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

//! Key-Value store abstraction with `RocksDB` backend.

use common::*;
use elastic_array::*;
use std::default::Default;
use rlp::{UntrustedRlp, RlpType, View, Compressible};
use rocksdb::{DB, Writable, WriteBatch, WriteOptions, IteratorMode, DBIterator,
	Options, DBCompactionStyle, BlockBasedOptions, Direction, Cache, Column, ReadOptions};

const DB_BACKGROUND_FLUSHES: i32 = 2;
const DB_BACKGROUND_COMPACTIONS: i32 = 2;

/// Write transaction. Batches a sequence of put/delete operations for efficiency.
pub struct DBTransaction {
	ops: Mutex<Vec<DBOp>>,
}

enum DBOp {
	Insert {
		col: Option<u32>,
		key: ElasticArray32<u8>,
		value: Bytes,
	},
	InsertCompressed {
		col: Option<u32>,
		key: ElasticArray32<u8>,
		value: Bytes,
	},
	Delete {
		col: Option<u32>,
		key: ElasticArray32<u8>,
	}
}

impl DBTransaction {
	/// Create new transaction.
	pub fn new(_db: &Database) -> DBTransaction {
		DBTransaction {
			ops: Mutex::new(Vec::with_capacity(256)),
		}
	}

	/// Insert a key-value pair in the transaction. Any existing value value will be overwritten upon write.
	pub fn put(&self, col: Option<u32>, key: &[u8], value: &[u8]) -> Result<(), String> {
		let mut ekey = ElasticArray32::new();
		ekey.append_slice(key);
		self.ops.lock().push(DBOp::Insert {
			col: col,
			key: ekey,
			value: value.to_vec(),
		});
		Ok(())
	}

	/// Insert a key-value pair in the transaction. Any existing value value will be overwritten upon write.
	pub fn put_vec(&self, col: Option<u32>, key: &[u8], value: Bytes) -> Result<(), String> {
		let mut ekey = ElasticArray32::new();
		ekey.append_slice(key);
		self.ops.lock().push(DBOp::Insert {
			col: col,
			key: ekey,
			value: value,
		});
		Ok(())
	}

	/// Insert a key-value pair in the transaction. Any existing value value will be overwritten upon write.
	/// Value will be RLP-compressed on  flush
	pub fn put_compressed(&self, col: Option<u32>, key: &[u8], value: Bytes) -> Result<(), String> {
		let mut ekey = ElasticArray32::new();
		ekey.append_slice(key);
		self.ops.lock().push(DBOp::InsertCompressed {
			col: col,
			key: ekey,
			value: value,
		});
		Ok(())
	}

	/// Delete value by key.
	pub fn delete(&self, col: Option<u32>, key: &[u8]) -> Result<(), String> {
		let mut ekey = ElasticArray32::new();
		ekey.append_slice(key);
		self.ops.lock().push(DBOp::Delete {
			col: col,
			key: ekey,
		});
		Ok(())
	}
}

enum KeyState {
	Insert(Bytes),
	InsertCompressed(Bytes),
	Delete,
}

/// Compaction profile for the database settings
#[derive(Clone, Copy)]
pub struct CompactionProfile {
	/// L0-L1 target file size
	pub initial_file_size: u64,
	/// L2-LN target file size multiplier
	pub file_size_multiplier: i32,
	/// rate limiter for background flushes and compactions, bytes/sec, if any
	pub write_rate_limit: Option<u64>,
}

impl Default for CompactionProfile {
	/// Default profile suitable for most storage
	fn default() -> CompactionProfile {
		CompactionProfile {
			initial_file_size: 32 * 1024 * 1024,
			file_size_multiplier: 2,
			write_rate_limit: None,
		}
	}
}

impl CompactionProfile {
	/// Slow hdd compaction profile
	pub fn hdd() -> CompactionProfile {
		CompactionProfile {
			initial_file_size: 192 * 1024 * 1024,
			file_size_multiplier: 1,
			write_rate_limit: Some(8 * 1024 * 1024),
		}
	}
}

/// Database configuration
#[derive(Clone, Copy)]
pub struct DatabaseConfig {
	/// Max number of open files.
	pub max_open_files: i32,
	/// Cache-size
	pub cache_size: Option<usize>,
	/// Compaction profile
	pub compaction: CompactionProfile,
	/// Set number of columns
	pub columns: Option<u32>,
	/// Should we keep WAL enabled?
	pub wal: bool,
}

impl DatabaseConfig {
	/// Create new `DatabaseConfig` with default parameters and specified set of columns.
	pub fn with_columns(columns: Option<u32>) -> Self {
		let mut config = Self::default();
		config.columns = columns;
		config
	}
}

impl Default for DatabaseConfig {
	fn default() -> DatabaseConfig {
		DatabaseConfig {
			cache_size: None,
			max_open_files: 512,
			compaction: CompactionProfile::default(),
			columns: None,
			wal: true,
		}
	}
}

/// Database iterator for flushed data only
pub struct DatabaseIterator {
	iter: DBIterator,
}

impl<'a> Iterator for DatabaseIterator {
	type Item = (Box<[u8]>, Box<[u8]>);

    fn next(&mut self) -> Option<Self::Item> {
		self.iter.next()
	}
}

/// Key-Value database.
pub struct Database {
	db: DB,
	write_opts: WriteOptions,
	cfs: Vec<Column>,
	read_opts: ReadOptions,
	// Dirty values added with `write_buffered`. Cleaned on `flush`.
	overlay: RwLock<Vec<HashMap<ElasticArray32<u8>, KeyState>>>,
	// Values currently being flushed. Cleared when `flush` completes.
	flushing: RwLock<Vec<HashMap<ElasticArray32<u8>, KeyState>>>,
	// Prevents concurrent flushes.
	flushing_lock: Mutex<()>,
}

impl Database {
	/// Open database with default settings.
	pub fn open_default(path: &str) -> Result<Database, String> {
		Database::open(&DatabaseConfig::default(), path)
	}

	/// Open database file. Creates if it does not exist.
	pub fn open(config: &DatabaseConfig, path: &str) -> Result<Database, String> {
		let mut opts = Options::new();
		if let Some(rate_limit) = config.compaction.write_rate_limit {
			try!(opts.set_parsed_options(&format!("rate_limiter_bytes_per_sec={}", rate_limit)));
		}
		try!(opts.set_parsed_options(&format!("max_total_wal_size={}", 64 * 1024 * 1024)));
		try!(opts.set_parsed_options("verify_checksums_in_compaction=0"));
		opts.set_max_open_files(config.max_open_files);
		opts.create_if_missing(true);
		opts.set_use_fsync(false);

		opts.set_max_background_flushes(DB_BACKGROUND_FLUSHES);
		opts.set_max_background_compactions(DB_BACKGROUND_COMPACTIONS);

		// compaction settings
		opts.set_compaction_style(DBCompactionStyle::DBUniversalCompaction);
		opts.set_target_file_size_base(config.compaction.initial_file_size);
		opts.set_target_file_size_multiplier(config.compaction.file_size_multiplier);

		let mut cf_options = Vec::with_capacity(config.columns.unwrap_or(0) as usize);

		for _ in 0 .. config.columns.unwrap_or(0) {
			let mut opts = Options::new();
			opts.set_compaction_style(DBCompactionStyle::DBUniversalCompaction);
			opts.set_target_file_size_base(config.compaction.initial_file_size);
			opts.set_target_file_size_multiplier(config.compaction.file_size_multiplier);
			if let Some(cache_size) = config.cache_size {
				let mut block_opts = BlockBasedOptions::new();
				// all goes to read cache
				block_opts.set_cache(Cache::new(cache_size * 1024 * 1024));
				opts.set_block_based_table_factory(&block_opts);
			}
			cf_options.push(opts);
		}

		let mut write_opts = WriteOptions::new();
		if !config.wal {
			write_opts.disable_wal(true);
		}
		let mut read_opts = ReadOptions::new();
		read_opts.set_verify_checksums(false);

		let mut cfs: Vec<Column> = Vec::new();
		let db = match config.columns {
			Some(columns) => {
				let cfnames: Vec<_> = (0..columns).map(|c| format!("col{}", c)).collect();
				let cfnames: Vec<&str> = cfnames.iter().map(|n| n as &str).collect();
				match DB::open_cf(&opts, path, &cfnames, &cf_options) {
					Ok(db) => {
						cfs = cfnames.iter().map(|n| db.cf_handle(n).unwrap()).collect();
						assert!(cfs.len() == columns as usize);
						Ok(db)
					}
					Err(_) => {
						// retry and create CFs
						match DB::open_cf(&opts, path, &[], &[]) {
							Ok(mut db) => {
								cfs = cfnames.iter().enumerate().map(|(i, n)| db.create_cf(n, &cf_options[i]).unwrap()).collect();
								Ok(db)
							},
							err @ Err(_) => err,
						}
					}
				}
			},
			None => DB::open(&opts, path)
		};
		let db = match db {
			Ok(db) => db,
			Err(ref s) if s.starts_with("Corruption:") => {
				info!("{}", s);
				info!("Attempting DB repair for {}", path);
				try!(DB::repair(&opts, path));
				try!(DB::open(&opts, path))
			},
			Err(s) => { return Err(s); }
		};
		Ok(Database {
			db: db,
			write_opts: write_opts,
			overlay: RwLock::new((0..(cfs.len() + 1)).map(|_| HashMap::new()).collect()),
			flushing: RwLock::new((0..(cfs.len() + 1)).map(|_| HashMap::new()).collect()),
			cfs: cfs,
			flushing_lock: Mutex::new(()),
			read_opts: read_opts,
		})
	}

	/// Creates new transaction for this database.
	pub fn transaction(&self) -> DBTransaction {
		DBTransaction::new(self)
	}


	fn to_overlay_column(col: Option<u32>) -> usize {
		col.map_or(0, |c| (c + 1) as usize)
	}

	/// Commit transaction to database.
	pub fn write_buffered(&self, tr: DBTransaction) -> Result<(), String> {
		let mut overlay = self.overlay.write();
		let ops = tr.ops.into_inner();
		for op in ops {
			match op {
				DBOp::Insert { col, key, value } => {
					let c = Self::to_overlay_column(col);
					overlay[c].insert(key, KeyState::Insert(value));
				},
				DBOp::InsertCompressed { col, key, value } => {
					let c = Self::to_overlay_column(col);
					overlay[c].insert(key, KeyState::InsertCompressed(value));
				},
				DBOp::Delete { col, key } => {
					let c = Self::to_overlay_column(col);
					overlay[c].insert(key, KeyState::Delete);
				},
			}
		};
		Ok(())
	}

	/// Commit buffered changes to database.
	pub fn flush(&self) -> Result<(), String> {
		let _lock = self.flushing_lock.lock();
		mem::swap(&mut *self.overlay.write(), &mut *self.flushing.write());
		let batch = WriteBatch::new();
		for (c, column) in self.flushing.read().iter().enumerate() {
			for (key, state) in column.iter() {
				match *state {
					KeyState::Delete => {
						if c > 0 {
							try!(batch.delete_cf(self.cfs[c - 1], &key));
						} else {
							try!(batch.delete(&key));
						}
					},
					KeyState::Insert(ref value) => {
						if c > 0 {
							try!(batch.put_cf(self.cfs[c - 1], &key, &value));
						} else {
							try!(batch.put(&key, &value));
						}
					},
					KeyState::InsertCompressed(ref value) => {
						let compressed = UntrustedRlp::new(&value).compress(RlpType::Blocks);
						if c > 0 {
							try!(batch.put_cf(self.cfs[c - 1], &key, &compressed));
						} else {
							try!(batch.put(&key, &value));
						}
					}
				}
			}
		}
		try!(self.db.write_opt(batch, &self.write_opts));
		for column in self.flushing.write().iter_mut() {
			column.clear();
		}
		Ok(())
	}


	/// Commit transaction to database.
	pub fn write(&self, tr: DBTransaction) -> Result<(), String> {
		let batch = WriteBatch::new();
		let ops = tr.ops.into_inner();
		for op in ops {
			match op {
				DBOp::Insert { col, key, value } => {
					try!(col.map_or_else(|| batch.put(&key, &value), |c| batch.put_cf(self.cfs[c as usize], &key, &value)))
				},
				DBOp::InsertCompressed { col, key, value } => {
					let compressed = UntrustedRlp::new(&value).compress(RlpType::Blocks);
					try!(col.map_or_else(|| batch.put(&key, &compressed), |c| batch.put_cf(self.cfs[c as usize], &key, &compressed)))
				},
				DBOp::Delete { col, key } => {
					try!(col.map_or_else(|| batch.delete(&key), |c| batch.delete_cf(self.cfs[c as usize], &key)))
				},
			}
		}
		self.db.write_opt(batch, &self.write_opts)
	}

	/// Get value by key.
	pub fn get(&self, col: Option<u32>, key: &[u8]) -> Result<Option<Bytes>, String> {
		let overlay = &self.overlay.read()[Self::to_overlay_column(col)];
		match overlay.get(key) {
			Some(&KeyState::Insert(ref value)) | Some(&KeyState::InsertCompressed(ref value)) => Ok(Some(value.clone())),
			Some(&KeyState::Delete) => Ok(None),
			None => {
				let flushing = &self.flushing.read()[Self::to_overlay_column(col)];
				match flushing.get(key) {
					Some(&KeyState::Insert(ref value)) | Some(&KeyState::InsertCompressed(ref value)) => Ok(Some(value.clone())),
					Some(&KeyState::Delete) => Ok(None),
					None => {
						col.map_or_else(
							|| self.db.get_opt(key, &self.read_opts).map(|r| r.map(|v| v.to_vec())),
							|c| self.db.get_cf_opt(self.cfs[c as usize], key, &self.read_opts).map(|r| r.map(|v| v.to_vec())))
					},
				}
			},
		}
	}

	/// Get value by partial key. Prefix size should match configured prefix size. Only searches flushed values.
	// TODO: support prefix seek for unflushed ata
	pub fn get_by_prefix(&self, col: Option<u32>, prefix: &[u8]) -> Option<Box<[u8]>> {
		let mut iter = col.map_or_else(|| self.db.iterator_opt(IteratorMode::From(prefix, Direction::Forward), &self.read_opts),
			|c| self.db.iterator_cf_opt(self.cfs[c as usize], IteratorMode::From(prefix, Direction::Forward), &self.read_opts).unwrap());
		match iter.next() {
			// TODO: use prefix_same_as_start read option (not availabele in C API currently)
			Some((k, v)) => if k[0 .. prefix.len()] == prefix[..] { Some(v) } else { None },
			_ => None
		}
	}

	/// Get database iterator for flushed data.
	pub fn iter(&self, col: Option<u32>) -> DatabaseIterator {
		//TODO: iterate over overlay
		col.map_or_else(|| DatabaseIterator { iter: self.db.iterator_opt(IteratorMode::Start, &self.read_opts) },
			|c| DatabaseIterator { iter: self.db.iterator_cf_opt(self.cfs[c as usize], IteratorMode::Start, &self.read_opts).unwrap() })
	}
}

#[cfg(test)]
mod tests {
	use hash::*;
	use super::*;
	use devtools::*;
	use std::str::FromStr;

	fn test_db(config: &DatabaseConfig) {
		let path = RandomTempPath::create_dir();
		let db = Database::open(config, path.as_path().to_str().unwrap()).unwrap();
		let key1 = H256::from_str("02c69be41d0b7e40352fc85be1cd65eb03d40ef8427a0ca4596b1ead9a00e9fc").unwrap();
		let key2 = H256::from_str("03c69be41d0b7e40352fc85be1cd65eb03d40ef8427a0ca4596b1ead9a00e9fc").unwrap();
		let key3 = H256::from_str("01c69be41d0b7e40352fc85be1cd65eb03d40ef8427a0ca4596b1ead9a00e9fc").unwrap();

		let batch = db.transaction();
		batch.put(None, &key1, b"cat").unwrap();
		batch.put(None, &key2, b"dog").unwrap();
		db.write(batch).unwrap();

		assert_eq!(&*db.get(None, &key1).unwrap().unwrap(), b"cat");

		let contents: Vec<_> = db.iter(None).collect();
		assert_eq!(contents.len(), 2);
		assert_eq!(&*contents[0].0, &*key1);
		assert_eq!(&*contents[0].1, b"cat");
		assert_eq!(&*contents[1].0, &*key2);
		assert_eq!(&*contents[1].1, b"dog");

		let batch = db.transaction();
		batch.delete(None, &key1).unwrap();
		db.write(batch).unwrap();

		assert!(db.get(None, &key1).unwrap().is_none());

		let batch = db.transaction();
		batch.put(None, &key1, b"cat").unwrap();
		db.write(batch).unwrap();

		let transaction = db.transaction();
		transaction.put(None, &key3, b"elephant").unwrap();
		transaction.delete(None, &key1).unwrap();
		db.write(transaction).unwrap();
		assert!(db.get(None, &key1).unwrap().is_none());
		assert_eq!(&*db.get(None, &key3).unwrap().unwrap(), b"elephant");

		assert_eq!(&*db.get_by_prefix(None, &key3).unwrap(), b"elephant");
		assert_eq!(&*db.get_by_prefix(None, &key2).unwrap(), b"dog");

		let transaction = db.transaction();
		transaction.put(None, &key1, b"horse").unwrap();
		transaction.delete(None, &key3).unwrap();
		db.write_buffered(transaction).unwrap();
		assert!(db.get(None, &key3).unwrap().is_none());
		assert_eq!(&*db.get(None, &key1).unwrap().unwrap(), b"horse");

		db.flush().unwrap();
		assert!(db.get(None, &key3).unwrap().is_none());
		assert_eq!(&*db.get(None, &key1).unwrap().unwrap(), b"horse");
	}

	#[test]
	fn kvdb() {
		let path = RandomTempPath::create_dir();
		let _ = Database::open_default(path.as_path().to_str().unwrap()).unwrap();
		test_db(&DatabaseConfig::default());
	}
}
