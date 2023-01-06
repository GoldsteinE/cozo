/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

use ::sqlite::Connection;
use either::{Either, Left, Right};
use miette::{bail, miette, IntoDiagnostic, Result};
use sqlite::{ConnectionWithFullMutex, State, Statement};

use crate::data::tuple::{check_key_for_validity, Tuple};
use crate::data::value::ValidityTs;
use crate::runtime::relation::{decode_tuple_from_kv, extend_tuple_from_v};
use crate::storage::{Storage, StoreTx};
use crate::utils::swap_option_result;

/// The Sqlite storage engine
#[derive(Clone)]
pub struct SqliteStorage {
    lock: Arc<RwLock<()>>,
    name: String,
    pool: Arc<Mutex<Vec<ConnectionWithFullMutex>>>,
}

/// Create a sqlite backed database.
/// This is slower than [`new_cozo_rocksdb`](crate::new_cozo_rocksdb)
/// but uses way less resources and is much easier to compile for exotic
/// environments.
/// Supports concurrent readers but only a single writer.
///
/// You must provide a disk-based path: `:memory:` is not OK.
/// If you want a pure memory storage, use [`new_cozo_mem`](crate::new_cozo_mem).
pub fn new_cozo_sqlite(path: String) -> Result<crate::Db<SqliteStorage>> {
    if path.is_empty() {
        bail!("empty path for sqlite storage")
    }
    let conn = Connection::open_with_full_mutex(&path).into_diagnostic()?;
    let query = r#"
        create table if not exists cozo
        (
            k BLOB primary key,
            v BLOB
        );
    "#;
    let mut statement = conn.prepare(query).unwrap();
    while statement.next().into_diagnostic()? != State::Done {}

    let ret = crate::Db::new(SqliteStorage {
        lock: Arc::new(Default::default()),
        name: path,
        pool: Arc::new(Mutex::new(vec![])),
    })?;

    ret.initialize()?;
    Ok(ret)
}

impl<'s> Storage<'s> for SqliteStorage {
    type Tx = SqliteTx<'s>;

    fn transact(&'s self, write: bool) -> Result<Self::Tx> {
        let conn = {
            match self.pool.lock().unwrap().pop() {
                None => Connection::open_with_full_mutex(&self.name).into_diagnostic()?,
                Some(conn) => conn,
            }
        };
        let lock = if write {
            Right(self.lock.write().unwrap())
        } else {
            Left(self.lock.read().unwrap())
        };
        if write {
            let mut stmt = conn.prepare("begin;").into_diagnostic()?;
            while stmt.next().into_diagnostic()? != State::Done {}
        }
        Ok(SqliteTx {
            lock,
            storage: self,
            conn: Some(conn),
            stmts: [
                Mutex::new(None),
                Mutex::new(None),
                Mutex::new(None),
                Mutex::new(None),
            ],
            committed: false,
        })
    }

    fn del_range(&'_ self, lower: &[u8], upper: &[u8]) -> Result<()> {
        let lower_b = lower.to_vec();
        let upper_b = upper.to_vec();
        let query = r#"
                delete from cozo where k >= ? and k < ?;
            "#;
        let lock = self.lock.clone();
        let name = self.name.clone();
        let closure = move || {
            let _locked = lock.write().unwrap();
            let conn = sqlite::open(&name).unwrap();
            let mut statement = conn.prepare(query).unwrap();
            statement.bind((1, &lower_b as &[u8])).unwrap();
            statement.bind((2, &upper_b as &[u8])).unwrap();
            while statement.next().unwrap() != State::Done {}
        };
        std::thread::spawn(closure);
        Ok(())
    }

    fn batch_put<'a>(
        &'a self,
        data: Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>,
    ) -> Result<()> {
        let mut tx = self.transact(true)?;
        for result in data {
            let (key, val) = result?;
            tx.put(&key, &val)?;
        }
        tx.commit()?;
        Ok(())
    }

    fn range_compact(&'_ self, _lower: &[u8], _upper: &[u8]) -> Result<()> {
        let mut pool = self.pool.lock().unwrap();
        while pool.pop().is_some() {}
        Ok(())
    }

    fn storage_kind(&self) -> &'static str {
        "sqlite"
    }
}

pub struct SqliteTx<'a> {
    lock: Either<RwLockReadGuard<'a, ()>, RwLockWriteGuard<'a, ()>>,
    storage: &'a SqliteStorage,
    conn: Option<ConnectionWithFullMutex>,
    stmts: [Mutex<Option<Statement<'a>>>; N_CACHED_QUERIES],
    committed: bool,
}

unsafe impl Sync for SqliteTx<'_> {}

const N_QUERIES: usize = 6;
const N_CACHED_QUERIES: usize = 4;
const QUERIES: [&str; N_QUERIES] = [
    "select v from cozo where k = ?;",
    "insert into cozo(k, v) values (?, ?) on conflict(k) do update set v=excluded.v;",
    "delete from cozo where k = ?;",
    "select 1 from cozo where k = ?;",
    "select k, v from cozo where k >= ? and k < ? order by k;",
    "select k, v from cozo where k >= ? and k < ? order by k limit 1;",
];

const GET_QUERY: usize = 0;
const PUT_QUERY: usize = 1;
const DEL_QUERY: usize = 2;
const EXISTS_QUERY: usize = 3;
const RANGE_QUERY: usize = 4;
const SKIP_RANGE_QUERY: usize = 5;

impl Drop for SqliteTx<'_> {
    fn drop(&mut self) {
        if let Right(RwLockWriteGuard { .. }) = self.lock {
            if !self.committed {
                let query = r#"rollback;"#;
                let _ = self.conn.as_ref().unwrap().execute(query);
            }
        }
        let mut pool = self.storage.pool.lock().unwrap();
        let conn = self.conn.take().unwrap();
        pool.push(conn)
    }
}

impl<'s> SqliteTx<'s> {
    fn ensure_stmt(&self, idx: usize) {
        let mut stmt = self.stmts[idx].lock().unwrap();
        if stmt.is_none() {
            let query = QUERIES[idx];
            let prepared = self.conn.as_ref().unwrap().prepare(query).unwrap();

            // Casting away the lifetime!
            // This is OK because we are abiding by the contract of the underlying C pointer,
            // as required by Sqlite's implementation
            let prepared = unsafe { std::mem::transmute(prepared) };

            *stmt = Some(prepared)
        }
    }
}

impl<'s> StoreTx<'s> for SqliteTx<'s> {
    fn get(&self, key: &[u8], _for_update: bool) -> Result<Option<Vec<u8>>> {
        self.ensure_stmt(GET_QUERY);
        let mut statement = self.stmts[GET_QUERY].lock().unwrap();
        let statement = statement.as_mut().unwrap();
        statement.reset().unwrap();

        statement.bind((1, key)).unwrap();
        Ok(match statement.next().into_diagnostic()? {
            State::Row => {
                let res = statement.read::<Vec<u8>, _>(0).into_diagnostic()?;
                Some(res)
            }
            State::Done => None,
        })
    }

    fn put(&mut self, key: &[u8], val: &[u8]) -> Result<()> {
        self.ensure_stmt(PUT_QUERY);
        let mut statement = self.stmts[PUT_QUERY].lock().unwrap();
        let statement = statement.as_mut().unwrap();
        statement.reset().unwrap();

        statement.bind((1, key)).unwrap();
        statement.bind((2, val)).unwrap();
        while statement.next().into_diagnostic()? != State::Done {}
        Ok(())
    }

    fn del(&mut self, key: &[u8]) -> Result<()> {
        self.ensure_stmt(DEL_QUERY);
        let mut statement = self.stmts[DEL_QUERY].lock().unwrap();
        let statement = statement.as_mut().unwrap();
        statement.reset().unwrap();

        statement.bind((1, key)).unwrap();
        while statement.next().into_diagnostic()? != State::Done {}

        Ok(())
    }

    fn exists(&self, key: &[u8], _for_update: bool) -> Result<bool> {
        self.ensure_stmt(EXISTS_QUERY);
        let mut statement = self.stmts[EXISTS_QUERY].lock().unwrap();
        let statement = statement.as_mut().unwrap();
        statement.reset().unwrap();

        statement.bind((1, key)).unwrap();
        Ok(match statement.next().into_diagnostic()? {
            State::Row => true,
            State::Done => false,
        })
    }

    fn commit(&mut self) -> Result<()> {
        if let Right(RwLockWriteGuard { .. }) = self.lock {
            if !self.committed {
                let query = r#"commit;"#;
                let mut statement = self.conn.as_ref().unwrap().prepare(query).unwrap();
                while statement.next().into_diagnostic()? != State::Done {}
                self.committed = true;
            } else {
                bail!("multiple commits")
            }
        }
        Ok(())
    }

    fn range_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a>
    where
        's: 'a,
    {
        // Range scans cannot use cached prepared statements, as several of them
        // can be used at the same time.
        let query = QUERIES[RANGE_QUERY];
        let mut statement = self.conn.as_ref().unwrap().prepare(query).unwrap();
        statement.bind((1, lower)).unwrap();
        statement.bind((2, upper)).unwrap();
        Box::new(TupleIter(statement))
    }

    fn range_skip_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
        valid_at: ValidityTs,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        let query = QUERIES[SKIP_RANGE_QUERY];
        let statement = self.conn.as_ref().unwrap().prepare(query).unwrap();
        Box::new(SkipIter {
            stmt: statement,
            valid_at,
            next_bound: lower.to_vec(),
            upper_bound: upper.to_vec(),
        })
    }

    fn range_scan<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>
    where
        's: 'a,
    {
        let query = QUERIES[RANGE_QUERY];
        let mut statement = self.conn.as_ref().unwrap().prepare(query).unwrap();
        statement.bind((1, lower)).unwrap();
        statement.bind((2, upper)).unwrap();
        Box::new(RawIter(statement))
    }

    fn total_scan<'a>(&'a self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>
    where
        's: 'a,
    {
        let statement = self
            .conn
            .as_ref()
            .unwrap()
            .prepare("select k, v from cozo order by k;")
            .unwrap();
        Box::new(RawIter(statement))
    }
}

struct TupleIter<'l>(Statement<'l>);

impl<'l> Iterator for TupleIter<'l> {
    type Item = Result<Tuple>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.0.next() {
            Ok(State::Done) => None,
            Ok(State::Row) => {
                let k = self.0.read::<Vec<u8>, _>(0).unwrap();
                let v = self.0.read::<Vec<u8>, _>(1).unwrap();
                let tuple = decode_tuple_from_kv(&k, &v);
                Some(Ok(tuple))
            }
            Err(err) => Some(Err(miette!(err))),
        }
    }
}

struct RawIter<'l>(Statement<'l>);

impl<'l> Iterator for RawIter<'l> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.0.next() {
            Ok(State::Done) => None,
            Ok(State::Row) => {
                let k = self.0.read::<Vec<u8>, _>(0).unwrap();
                let v = self.0.read::<Vec<u8>, _>(1).unwrap();
                Some(Ok((k, v)))
            }
            Err(err) => Some(Err(miette!(err))),
        }
    }
}

struct SkipIter<'l> {
    stmt: Statement<'l>,
    valid_at: ValidityTs,
    next_bound: Vec<u8>,
    upper_bound: Vec<u8>,
}

impl<'l> SkipIter<'l> {
    fn next_inner(&mut self) -> Result<Option<Tuple>> {
        loop {
            self.stmt.reset().into_diagnostic()?;
            self.stmt.bind((1, &self.next_bound as &[u8])).unwrap();
            self.stmt.bind((2, &self.upper_bound as &[u8])).unwrap();

            match self.stmt.next().into_diagnostic()? {
                State::Done => return Ok(None),
                State::Row => {
                    let k = self.stmt.read::<Vec<u8>, _>(0).unwrap();
                    let (ret, nxt_bound) = check_key_for_validity(&k, self.valid_at);
                    self.next_bound = nxt_bound;
                    if let Some(mut tup) = ret {
                        let v = self.stmt.read::<Vec<u8>, _>(1).unwrap();
                        extend_tuple_from_v(&mut tup, &v);
                        return Ok(Some(tup));
                    }
                }
            }
        }
    }
}

impl<'l> Iterator for SkipIter<'l> {
    type Item = Result<Tuple>;

    fn next(&mut self) -> Option<Self::Item> {
        swap_option_result(self.next_inner())
    }
}
