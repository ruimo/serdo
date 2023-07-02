use crate::{cmd::Cmd};

cfg_if::cfg_if! {
    if #[cfg(feature = "persistence")] {
        use crate::sqlite_undo_store_error::SqliteUndoStoreError;
        use std::path::{Path};
        use error_stack::{IntoReport, Result, bail, report};
    }
}

pub trait UndoStore {
    type ModelType;
    type CmdType: Cmd<Model = Self::ModelType>;
    type ErrType;

    fn model(&self) -> &Self::ModelType;
    fn mutate(&mut self, f: Box<dyn FnOnce(&mut Self::ModelType) -> Result<Self::CmdType, Self::ErrType>>) -> Result<(), Self::ErrType>;
    fn irreversible_mutate<R>(&mut self, f: Box<dyn FnOnce(&mut Self::ModelType) -> R>) -> R where Self: Sized;
    fn add_cmd(&mut self, cmd: Self::CmdType);
    fn can_undo(&self) -> bool;
    fn undo(&mut self);
    fn can_redo(&self) -> bool;
    fn redo(&mut self);
}

#[derive(Debug)]
pub enum InMemoryStoreErr {
    // Undo/Redo
    CannotUndoRedo,
}

pub struct InMemoryUndoStore<C, M, E> where M: Default {
    phantom: std::marker::PhantomData<E>,
    model: M,
    store: Vec<C>,
    location: usize,
}

impl<C, M, E> InMemoryUndoStore<C, M, E> where M: Default {
    pub fn new(capacity: usize) -> Self {
        Self {
            phantom: std::marker::PhantomData,
            model: M::default(),
            store: Vec::with_capacity(capacity),
            location: 0,
        }
    }
}

impl<C, M, E> InMemoryUndoStore<C, M, E> where M: Default + 'static, C: Cmd<Model = M> {
    fn post_cmd(&mut self, cmd: C) {
        if self.location < self.store.len() {
            self.store.truncate(self.location);
        }

        while self.store.capacity() <= self.store.len() {
            self.store.remove(0);
        }
    
        self.store.push(cmd);
        self.location = self.store.len();
    }
}

impl<C, M, E> UndoStore for InMemoryUndoStore<C, M, E>
    where M: Default + 'static, C: Cmd<Model = M>
{
    type ModelType = M;
    type CmdType = C;
    type ErrType = E;

    fn mutate(&mut self, f: Box<dyn FnOnce(&mut Self::ModelType) -> Result<Self::CmdType, Self::ErrType>>) -> Result<(), Self::ErrType> {
        let result = f(&mut self.model);
        if let Ok(cmd) = result {
            self.post_cmd(cmd);
            Ok(())
        } else {
            result.map(|_| ())
        }
    }

    fn add_cmd(&mut self, cmd: Self::CmdType) {
        cmd.redo(&mut self.model);
        self.post_cmd(cmd);
    }

    #[inline]
    fn can_undo(&self) -> bool {
        0 < self.location
    }

    fn undo(&mut self) {
        if self.can_undo() {
            self.location -= 1;
            let cmd = &self.store[self.location];
            cmd.undo(&mut self.model)
        }
    }

    #[inline]
    fn can_redo(&self) -> bool {
        self.location < self.store.len()
    }

    fn redo(&mut self) {
        if self.can_redo() {
            let cmd = &mut self.store[self.location];
            cmd.redo(&mut self.model);
            self.location += 1;
        }
    }

    fn model(&self) -> &M {
        &self.model
    }

    fn irreversible_mutate<R>(&mut self, f: Box<dyn FnOnce(&mut Self::ModelType) -> R>) -> R {
        f(&mut self.model)
    }
}

#[cfg(feature = "persistence")]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct SqliteUndoStore<C, M, E> where C: crate::cmd::SerializableCmd<Model = M>, M: Default + serde::Serialize + serde::de::DeserializeOwned {
    phantom: std::marker::PhantomData<C>,
    phantome: std::marker::PhantomData<E>,
    base_dir: std::path::PathBuf,
    sqlite_path: std::path::PathBuf,
    conn: rusqlite::Connection,
    model: M,
    cur_cmd_seq_no: i64,
    undo_limit: usize,
}

#[cfg(feature = "persistence")]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
impl<C, M, E> SqliteUndoStore<C, M, E> where C: crate::cmd::SerializableCmd<Model = M>, M: Default + serde::Serialize + serde::de::DeserializeOwned {
    fn lock_file_path(base_dir: &std::path::Path) -> std::path::PathBuf {
        let mut path: std::path::PathBuf = base_dir.to_path_buf();
        path.push("lock");
        path
    }

    fn try_lock(base_dir: &std::path::Path) -> Result<std::fs::File, SqliteUndoStoreError> {
        let lock_file_path = Self::lock_file_path(base_dir);
        std::fs::OpenOptions::new().write(true).create_new(true).open(&lock_file_path).map_err(|_|
            SqliteUndoStoreError::CannotLock(lock_file_path)
        ).into_report()
    }

    fn unlock(&self) -> std::io::Result<()> {
        let p = Self::lock_file_path(&self.base_dir);
        std::fs::remove_file(&p)
    }

    #[inline]
    fn open_existing<P: AsRef<std::path::Path>>(path: P) -> rusqlite::Result<rusqlite::Connection> {
        rusqlite::Connection::open(&path)
    }

    fn create_new<P: AsRef<std::path::Path>>(path: P) -> rusqlite::Result<rusqlite::Connection> {
        let conn = rusqlite::Connection::open(&path)?;
        Self::create_tables(&conn)?;
        Ok(conn)
    }

    fn create_tables(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
        conn.execute_batch(
            "begin;
            create table command(serialized blob not null);
            create table snapshot(snapshot_id integer primary key not null, serialized blob not null);
            create table cmd_seq_no(cur_cmd_seq_no integer);
            commit;"
        )?;
        Ok(())
    }

    pub const SQLITE_FILE_NAME: &'static str = "db.sqlite";
    pub const DEFAULT_UNDO_LIMIT: usize = 100;

    // Open the specified directory or newly create it if that does not exist.
    pub fn open<P: AsRef<Path>>(dir: P, undo_limit: Option<usize>) -> Result<Self, SqliteUndoStoreError>
        where C: crate::cmd::SerializableCmd<Model = M> + serde::de::DeserializeOwned
    {
        let mut sqlite_path = dir.as_ref().to_path_buf();
        sqlite_path.push(Self::SQLITE_FILE_NAME);
        let copy_sqlite_path = sqlite_path.clone();

        let conn = if dir.as_ref().exists() {
            if ! dir.as_ref().is_dir() {
                return Err(SqliteUndoStoreError::NotADirectory(dir.as_ref().to_owned())).into_report()
            }
            Self::try_lock(dir.as_ref())?;
            Self::open_existing(&sqlite_path)
        } else {
            std::fs::create_dir_all(dir.as_ref()).map_err(|e| SqliteUndoStoreError::FileError(dir.as_ref().to_path_buf(), e))?;
            Self::try_lock(dir.as_ref())?;
            Self::create_new(&sqlite_path)
        }.map_err(|e|
            SqliteUndoStoreError::DbError(copy_sqlite_path, report!(e))
        )?;
        let mut store = SqliteUndoStore {
            base_dir: dir.as_ref().to_path_buf(), model: M::default(), 
            cur_cmd_seq_no: 0, phantom: std::marker::PhantomData, phantome: std::marker::PhantomData,
            undo_limit: undo_limit.unwrap_or(Self::DEFAULT_UNDO_LIMIT), sqlite_path, conn,
        };
        store.restore_model()?;
        Ok(store)
    }

    pub fn save_as<P: AsRef<Path>>(&self, save_to: P) -> Result<Self, SqliteUndoStoreError> {
        let mut to = save_to.as_ref().to_path_buf();
        to.push(Self::SQLITE_FILE_NAME);

        let mut from = self.base_dir.clone();
        from.push(Self::SQLITE_FILE_NAME);

        match std::fs::copy(&from, &to) {
            Ok(_) => Self::open(save_to, Some(self.undo_limit)),
            Err(error) => bail!(SqliteUndoStoreError::CannotCopyStore { from, to, error })
        }
    }

    #[inline]
    fn db<F, T>(&self, f: F) -> Result<T, SqliteUndoStoreError> where F: FnOnce() -> Result<T, rusqlite::Error> {
        f().map_err(|e| SqliteUndoStoreError::DbError(self.sqlite_path.clone(), e)).into_report()
    }

    #[inline]
    fn db_exec<F, T>(&self, f: F) -> Result<T, SqliteUndoStoreError> where F: FnOnce() -> Result<T, rusqlite::Error> {
        f().map_err(|e| SqliteUndoStoreError::DbError(self.sqlite_path.clone(), e)).into_report()
    }

    #[inline]
    fn db_can_undo<F, T>(&self, f: F) -> Result<T, SqliteUndoStoreError> where F: FnOnce() -> Result<T, rusqlite::Error> {
        f().map_err(|e| SqliteUndoStoreError::DbError(self.sqlite_path.clone(), e)).into_report()
    }

    #[inline]
    fn db_undo<F, T>(&self, f: F) -> Result<T, SqliteUndoStoreError> where F: FnOnce() -> Result<T, rusqlite::Error> {
        f().map_err(|e| SqliteUndoStoreError::DbError(self.sqlite_path.clone(), e)).into_report()
    }

    fn get_cur_seq_no(&self) -> Result<i64, SqliteUndoStoreError> {
        let mut stmt = self.db(|| self.conn.prepare(
            "select count(cur_cmd_seq_no), max(cur_cmd_seq_no) from cmd_seq_no"
        ).into_report())?;
        let mut rows = self.db(|| stmt.query([]).into_report())?;
        
        let row = self.db(|| rows.next().into_report())?.unwrap();
        let count: i64 = self.db(|| row.get(0).into_report())?;
        if 1 < count {
            bail!(SqliteUndoStoreError::CmdSeqNoInconsistent)
        } else {
            let cur_seq: Option<i64> = self.db(|| row.get(1).into_report())?;
            match cur_seq {
                None => {
                    self.db_exec(|| self.conn.execute("insert into cmd_seq_no (cur_cmd_seq_no) values (0)", rusqlite::params![]).into_report())?;
                    Ok(0)
                },
                Some(seq) => Ok(seq)
            }
        }
    }

    fn save_seq_no(&self) -> Result<(), SqliteUndoStoreError> {
        self.db_exec(|| self.conn.execute("update cmd_seq_no set cur_cmd_seq_no = ?1", rusqlite::params![self.cur_cmd_seq_no]).into_report()).map(|_| ())
    }

    fn load_without_snapshot(&self, cur_seq_no: i64) -> Result<M, SqliteUndoStoreError> {
        let mut stmt = self.db(|| self.conn.prepare(
            "select rowid, serialized from command where rowid <= ?1"
        ).into_report())?;
        let mut rows = self.db(|| stmt.query([cur_seq_no]).into_report())?;

        let mut cmd_id = 1;
        let mut model = M::default();
        while let Some(row) = self.db(|| rows.next().into_report())? {
            let id: i64 = self.db(|| row.get(0).into_report())?;
            if id != cmd_id {
                return Err(report!(SqliteUndoStoreError::CannotRestoreModel { snapshot_id: None, not_foud_cmd_id: cmd_id }))
            }
            cmd_id += 1;
            
            let serialized: Vec<u8> = self.db(|| row.get(1).into_report())?;
            let cmd: C = bincode::deserialize(&serialized).map_err(|ser_err|
                SqliteUndoStoreError::CannotDeserialize { path: None, id, ser_err }
            )?;
            cmd.redo(&mut model);
        }
        Ok(model)
    }

    fn load_last_snapshot(&self) -> Result<Option<(i64, M)>, SqliteUndoStoreError> {
        let mut stmt = self.db(|| self.conn.prepare(
            "select snapshot_id, serialized from snapshot
                  where
                    snapshot_id >= (select min(rowid) from command) -1
                    and snapshot_id <= (select max(rowid) from command)
                  order by snapshot_id desc limit 1"
        ).into_report())?;
        let mut rows = self.db(|| stmt.query([]).into_report())?;

        if let Some(row) = self.db(|| rows.next().into_report())? {
            let id: i64 = self.db(|| row.get(0).into_report())?;
            let serialized: Vec<u8> = self.db(|| row.get(1).into_report())?;
            let snapshot: M = bincode::deserialize(&serialized).map_err(|ser_err|
                SqliteUndoStoreError::CannotDeserialize { path: None, id, ser_err }
            )?;
            Ok(Some((id, snapshot)))
        } else {
            Ok(None)
        }
    }

    fn restore_model(&mut self) -> Result<(), SqliteUndoStoreError>
        where C: crate::cmd::SerializableCmd<Model = M> + serde::de::DeserializeOwned
    {
        let cur_seq_no = self.get_cur_seq_no()?;
        if cur_seq_no == 0 {
            self.model = M::default();
            self.cur_cmd_seq_no = 0;
            return Ok(())
        }

        match self.load_last_snapshot()? {
            Some((last_snapshot_id, mut model)) => {
                // Restore with snapshot.
                if cur_seq_no < last_snapshot_id {
                    let mut stmt = self.db(|| self.conn.prepare(
                        "select rowid, serialized from command where ?1 < rowid and rowid <= ?2 order by rowid desc"
                    ).into_report())?;
                    let mut rows = self.db(|| stmt.query([cur_seq_no, last_snapshot_id]).into_report())?;
                    
                    let mut cmd_id = last_snapshot_id;
                    while let Some(row) = self.db(|| rows.next().into_report())? {
                        let id: i64 = self.db(|| row.get(0).into_report())?;
                        if id != cmd_id {
                            return Err(report!(SqliteUndoStoreError::CannotRestoreModel {
                                snapshot_id: Some(last_snapshot_id), not_foud_cmd_id: cmd_id 
                            }))
                        }
                        cmd_id -= 1;
                        
                        let serialized: Vec<u8> = self.db(|| row.get(1).into_report())?;
                        let cmd: C = bincode::deserialize(&serialized).map_err(|ser_err|
                            SqliteUndoStoreError::CannotDeserialize { path: None, id, ser_err }
                        )?;
                        cmd.undo(&mut model);
                    }
                } else if last_snapshot_id < cur_seq_no {
                    let mut stmt = self.db(|| self.conn.prepare(
                        "select rowid, serialized from command where ?1 < rowid and rowid <= ?2 order by rowid asc"
                    ).into_report())?;
                    let mut rows = self.db(|| stmt.query([last_snapshot_id, cur_seq_no]).into_report())?;
                    
                    let mut cmd_id = last_snapshot_id + 1;
                    while let Some(row) = self.db(|| rows.next().into_report())? {
                        let id: i64 = self.db(|| row.get(0).into_report())?;
                        if id != cmd_id {
                            return Err(report!(SqliteUndoStoreError::CannotRestoreModel {
                                snapshot_id: Some(last_snapshot_id), not_foud_cmd_id: cmd_id
                            }))
                        }
                        cmd_id += 1;
                        
                        let serialized: Vec<u8> = self.db(|| row.get(1).into_report())?;
                        let cmd: C = bincode::deserialize(&serialized).map_err(|ser_err|
                            SqliteUndoStoreError::CannotDeserialize { path: None, id, ser_err }
                        )?;
                        cmd.redo(&mut model);
                    }
                }

                self.model = model;
                self.cur_cmd_seq_no = cur_seq_no;

                Ok(())
            },
            None => {
                // Restore without snapshot.
                self.model = self.load_without_snapshot(cur_seq_no)?;
                self.cur_cmd_seq_no = cur_seq_no;

                Ok(())
            },
        }
    }

    fn trim_undo_records(&self) -> Result<usize, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "delete from command as c0 where c0.rowid not in (
                select rowid from command as c1 order by rowid desc limit ?1
            )"
        )?;

        Ok(stmt.execute(rusqlite::params![self.undo_limit])?)
    }

    fn get_last_snapshot_id(&self) -> Result<Option<i64>, SqliteUndoStoreError> {
        let mut stmt = self.db(|| self.conn.prepare(
            "select max(snapshot_id) from snapshot"
        ).into_report())?;
        let mut rows = self.db(|| stmt.query([]).into_report())?;
        let row = rows.next().unwrap();
        Ok(row.unwrap().get(0).unwrap())
    }

    fn trim_snapshots(&self) -> Result<usize, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "delete from snapshot where snapshot_id < (select max(snapshot_id) from snapshot)"
        )?;

        Ok(stmt.execute(rusqlite::params![])?)
    }

    fn save_snapshot(&self) -> Result<(), SqliteUndoStoreError> {
        let mut stmt = self.db_exec(|| self.conn.prepare(
            "insert into snapshot (snapshot_id, serialized) values (?1, ?2)"
        ).into_report())?;
        let serialized = bincode::serialize(&self.model).map_err(|e|
            SqliteUndoStoreError::SerializeError(e)
        )?;
        self.db_exec(|| stmt.execute(rusqlite::params![self.cur_cmd_seq_no, serialized]).into_report())?;

        Ok(())
    }

    fn _add_cmd(&mut self, cmd: C) -> Result<(), SqliteUndoStoreError> {
        let serialized: Vec<u8> = bincode::serialize(&cmd).map_err(
            |e| SqliteUndoStoreError::SerializeError(e)
        )?;

        let delete_count = self.db_exec(|| self.conn.execute(
            "delete from command where ?1 < rowid", rusqlite::params![self.cur_cmd_seq_no]
        ).into_report())?;

        self.db_exec(|| self.conn.execute(
            "insert into command (serialized) values (?1)", rusqlite::params![serialized]
        ).into_report())?;

        self.cur_cmd_seq_no = self.conn.last_insert_rowid();
        if self.cur_cmd_seq_no == MAX_ROWID {
            return Err(report!(SqliteUndoStoreError::NeedCompaction(self.sqlite_path.clone())));
        }
        self.save_seq_no()?;
        let removed_count = self.db_exec(|| self.trim_undo_records())?;
        if removed_count != 0 {
            match self.get_last_snapshot_id()? {
                None => self.save_snapshot()?,
                Some(last_snapshot_id) => {
                    if last_snapshot_id < self.cur_cmd_seq_no - (self.undo_limit as i64) {
                        self.save_snapshot()?
                    }
                }
            }
        }

        if delete_count != 0 {
            self.db_exec(|| self.conn.execute("delete from snapshot", rusqlite::params![]).into_report())?;
            self.save_snapshot()?;
        } else {
            self.db_exec(|| self.trim_snapshots())?;
        }

        Ok(())
    }

    fn _can_undo(&self) -> Result<bool, SqliteUndoStoreError> {
        let mut stmt = self.db_can_undo(|| self.conn.prepare(
            "select 1 from command where rowid <= ?1"
        ).into_report())?;
        let mut rows = self.db_can_undo(|| stmt.query(rusqlite::params![self.cur_cmd_seq_no]).into_report())?;
        let row = self.db_can_undo(|| rows.next().into_report())?;
        Ok(row.is_some())
    }

    fn _undo(&mut self) -> Result<(), SqliteUndoStoreError> {
        let mut stmt = self.db_undo(|| self.conn.prepare(
            "select serialized from command where rowid = ?1"
        ).into_report())?;
        let mut rows = self.db_undo(|| stmt.query(rusqlite::params![self.cur_cmd_seq_no]).into_report())?;
        if let Some(row) = self.db_undo(|| rows.next().into_report())? {
            let serialized: Vec<u8> = self.db_undo(|| row.get(0).into_report())?;
            let cmd: C = bincode::deserialize(&serialized).map_err(|ser_err|
                SqliteUndoStoreError::CannotDeserialize {
                    path: Some(self.sqlite_path.clone()), id: self.cur_cmd_seq_no, ser_err
                }
            )?;
            let res = cmd.undo(&mut self.model);
            self.cur_cmd_seq_no -= 1;
            self.save_seq_no()?;
            Ok(res)
        } else {
            bail!(SqliteUndoStoreError::CannotUndoRedo)
        }
    }

    fn _can_redo(&self) -> Result<bool, SqliteUndoStoreError> {
        let mut stmt = self.db_can_undo(|| self.conn.prepare(
            "select 1 from command where ?1 < rowid"
        ).into_report())?;
        let mut rows = self.db_can_undo(|| stmt.query(rusqlite::params![self.cur_cmd_seq_no]).into_report())?;
        let row = self.db_can_undo(|| rows.next().into_report())?;
        Ok(row.is_some())
    }

    fn _redo(&mut self) -> Result<(), SqliteUndoStoreError> {
        let mut  stmt = self.db_undo(|| self.conn.prepare(
            "select serialized from command where rowid = ?1"
        ).into_report())?;
        let mut rows = self.db_undo(|| stmt.query(rusqlite::params![self.cur_cmd_seq_no + 1]).into_report())?;
        if let Some(row) = self.db_undo(|| rows.next().into_report())? {
            let serialized: Vec<u8> = self.db_undo(|| row.get(0).into_report())?;
            let cmd: C = bincode::deserialize(&serialized).map_err(|ser_err|
                SqliteUndoStoreError::CannotDeserialize {
                    path: Some(self.sqlite_path.clone()), id: self.cur_cmd_seq_no, ser_err
                }
            )?;

            cmd.redo(&mut self.model);
            self.cur_cmd_seq_no += 1;
            self.save_seq_no()?;
            Ok(())
        } else {
            bail!(SqliteUndoStoreError::CannotUndoRedo)
        }
    }
}

#[cfg(feature = "persistence")]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
impl<C, M, E> Drop for SqliteUndoStore<C, M, E> where C: crate::cmd::SerializableCmd<Model=M>, M: Default + serde::Serialize + serde::de::DeserializeOwned {
    fn drop(&mut self) {
        let _ = self.unlock();
    }
}

pub const MAX_ROWID: i64 = 9_223_372_036_854_775_807;

#[cfg(feature = "persistence")]
impl<C, M, E> SqliteUndoStore<C, M, E>
  where M: Default + serde::Serialize + serde::de::DeserializeOwned + 'static, C: crate::cmd::SerializableCmd<Model = M>
{
}

#[cfg(feature = "persistence")]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
impl<C, M, E> UndoStore for SqliteUndoStore<C, M, E>
  where M: Default + serde::Serialize + serde::de::DeserializeOwned + 'static, C: crate::cmd::SerializableCmd<Model = M>
{
    type ModelType = M;
    type CmdType = C;
    type ErrType = E;

    fn model(&self) -> &M { &self.model }

    fn mutate(&mut self, f: Box<dyn FnOnce(&mut Self::ModelType) -> Result<Self::CmdType, Self::ErrType>>) -> Result<(), Self::ErrType> {
        let result = f(&mut self.model);
        if let Ok(cmd) = result {
            if let Err(e) = self._add_cmd(cmd) {
                panic!("Cannot access database {:?}.", e);
            }
            Ok(())
        } else {
            result.map(|_| ())
        }
    }

    fn add_cmd(&mut self, cmd: Self::CmdType) {
        cmd.redo(&mut self.model);

        if let Err(e) = self._add_cmd(cmd) {
            panic!("Cannot access database {:?}.", e);
        }
    }

    fn can_undo(&self) -> bool {
        match self._can_undo() {
            Ok(b) => b,
            Err(e) => panic!("Cannot access database {:?}.", e),
        }
    }

    fn undo(&mut self) {
        if self.can_undo() {
            if let Err(e) = self._undo() {
                panic!("Cannot access database {:?}.", e);
            }
        }
    }

    fn can_redo(&self) -> bool {
        match self._can_redo() {
            Ok(b) => b,
            Err(e) => panic!("Cannot access database {:?}.", e),
        }
    }

    fn redo(&mut self) {
        if self.can_redo() {
            if let Err(e) = self._redo() {
                panic!("Cannot access database {:?}.", e);
            }
        }
    }

    fn irreversible_mutate<R>(&mut self, f: Box<dyn FnOnce(&mut Self::ModelType) -> R>) -> R {
        f(&mut self.model)
    }
}
#[cfg(test)]
mod tests {
    use super::{Cmd, InMemoryUndoStore, UndoStore};

    enum SumCmd {
        Add(i32), Sub(i32),
    }

    #[derive(PartialEq, Debug)]
    struct Sum(i32);

    impl Default for Sum {
        fn default() -> Self {
            Self(0)
        }
    }

    impl Cmd for SumCmd {
        type Model = Sum;

        fn redo(&self, model: &mut Self::Model) {
            match self {
                SumCmd::Add(i) => model.0 += *i,
                SumCmd::Sub(i) => model.0 -= *i,
            }
        }

        fn undo(&self, model: &mut Self::Model) {
            match self {
                SumCmd::Add(i) => model.0 -= *i,
                SumCmd::Sub(i) => model.0 += *i,
            }
        }
    }

    trait Model {
        type Resp;
        fn add(&mut self, to_add: i32) -> Self::Resp;
        fn sub(&mut self, to_sub: i32) -> Self::Resp;
    }

    impl Model for InMemoryUndoStore<SumCmd, Sum, ()> {
        type Resp = ();

        fn add(&mut self, to_add: i32) {
            self.add_cmd(SumCmd::Add(to_add));
        }

        fn sub(&mut self, to_sub: i32) {
            self.add_cmd(SumCmd::Sub(to_sub));
        }
    }

    #[test]
    fn can_undo_in_memory_store() {
        let mut store: InMemoryUndoStore<SumCmd, Sum, ()> = InMemoryUndoStore::new(3);
        assert_eq!(store.can_undo(), false);
        store.add(3);
        assert_eq!(store.model().0, 3);

        assert_eq!(store.can_undo(), true);
        store.undo();
        assert_eq!(store.model().0, 0);

        assert_eq!(store.can_undo(), false);
    }

    #[test]
    fn can_undo_redo_in_memory_store() {
        let mut store: InMemoryUndoStore<SumCmd, Sum, ()> = InMemoryUndoStore::new(3);
        assert_eq!(store.can_undo(), false);
        store.add(3);

        assert_eq!(store.can_undo(), true);
        assert_eq!(store.can_redo(), false);
        store.undo();
        assert_eq!(store.model().0, 0);

        assert_eq!(store.can_undo(), false);
        assert_eq!(store.can_redo(), true);
        store.redo();
        assert_eq!(store.model().0, 3);

        assert_eq!(store.can_undo(), true);
        assert_eq!(store.can_redo(), false);
        store.undo();
        assert_eq!(store.model().0, 0);
    }

    #[test]
    fn undo_and_add_cmd() {
        let mut store: InMemoryUndoStore<SumCmd, Sum, ()> = InMemoryUndoStore::new(3);

        store.add(3);
        store.add(4);
        store.add(5);

        // 3, 4, 5
        assert_eq!(store.model().0, 12);
        store.undo(); // Undo0 Cancel Add(5)
        // 3, 4
        assert_eq!(store.model().0, 7);

        store.add(6);
        // 3, 4, 6
        assert_eq!(store.model().0, 13);

        store.undo(); // Cancel Add(6)
        // 3, 4
        assert_eq!(store.model().0, 7);

        store.undo(); // Cancel Undo0
        // 3
        assert_eq!(store.model().0, 3);
    }
}

#[cfg(feature = "persistence")]
#[cfg(test)]
mod persistent_tests {
    use error_stack::{Result};
    use super::{Cmd, UndoStore};

    #[derive(serde::Serialize, serde::Deserialize)]
    struct SerSum(i32);

    impl Default for SerSum {
        fn default() -> Self {
            Self(0)
        }
    }

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
    enum SerSumCmd {
        Add(i32), Sub(i32),
    }

    impl Cmd for SerSumCmd {
        type Model = SerSum;

        fn redo(&self, model: &mut Self::Model) {
            match self {
                SerSumCmd::Add(i) => model.0 += *i,
                SerSumCmd::Sub(i) => model.0 -= *i,
            }
        }

        fn undo(&self, model: &mut Self::Model) {
            match self {
                SerSumCmd::Add(i) => model.0 -= *i,
                SerSumCmd::Sub(i) => model.0 += *i,
            }
        }
    }

    impl crate::cmd::SerializableCmd for SerSumCmd {
    }

    trait SerModel {
        fn add(&mut self, to_add: i32) -> Result<(), super::SqliteUndoStoreError>;
        fn sub(&mut self, to_sub: i32) -> Result<(), super::SqliteUndoStoreError>;
    }

    impl SerModel for super::SqliteUndoStore::<SerSumCmd, SerSum, ()> {
        fn add(&mut self, to_add: i32) -> Result<(), super::SqliteUndoStoreError> {
            self.add_cmd(SerSumCmd::Add(to_add));
            Ok(())
        }

        fn sub(&mut self, to_sub: i32) -> Result<(), super::SqliteUndoStoreError> {
            self.add_cmd(SerSumCmd::Sub(to_sub));
            Ok(())
        }
    }

    #[test]
    fn file_store_can_lock() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let store = super::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), None).unwrap();

        let store2_err = super::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), None).err().unwrap();
        match store2_err.downcast_ref::<super::SqliteUndoStoreError>().unwrap() {
            super::SqliteUndoStoreError::CannotLock(err_dir) => {
                let lock_file_path = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::lock_file_path(&dir);
                assert_eq!(err_dir.as_path(), lock_file_path);
            },
            _ => {panic!("Test failed. {:?}", store2_err)},
        };

        drop(store);

        let _ = super::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), None).unwrap();
    }

    #[test]
    fn can_serialize_cmd() {
        use rusqlite::{Connection, params};
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), None).unwrap();

        store.add(123).unwrap();
        assert_eq!(store.model().0, 123);
        store.sub(234).unwrap();
        assert_eq!(store.model().0, 123 - 234);

        let mut path = dir;
        path.push("db.sqlite");

        let conn = Connection::open(&path).unwrap();
        let mut stmt = conn.prepare(
            "select rowid, serialized from command order by rowid"
        ).unwrap();
        let mut rows = stmt.query(params![]).unwrap();

        let rec = rows.next().unwrap().unwrap();
        let id: i64 = rec.get(0).unwrap();
        assert_eq!(id, 1);
        let serialized: Vec<u8> = rec.get(1).unwrap();
        let cmd: SerSumCmd = bincode::deserialize(&serialized).unwrap();
        assert_eq!(cmd, SerSumCmd::Add(123));

        let rec = rows.next().unwrap().unwrap();
        let id: i64 = rec.get(0).unwrap();
        assert_eq!(id, 2);
        let serialized: Vec<u8> = rec.get(1).unwrap();
        let cmd: SerSumCmd = bincode::deserialize(&serialized).unwrap();
        assert_eq!(cmd, SerSumCmd::Sub(234));

        assert!(rows.next().unwrap().is_none());
    }

    #[test]
    fn can_undo_serialize_cmd() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), None).unwrap();

        store.add(123).unwrap();
        assert_eq!(store.model().0, 123);
        store.sub(234).unwrap();
        assert_eq!(store.model().0, 123 - 234);

        assert!(store.can_undo());
        store.undo();
        assert_eq!(store.model().0, 123);

        store.undo();
        assert_eq!(store.model().0, 0);

        store.redo();
        assert_eq!(store.model().0, 123);

        store.redo();
        assert_eq!(store.model().0, 123 - 234);
    }

    #[test]
    fn file_undo_store_can_serialize() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), None).unwrap();
        store.add(123).unwrap();
        assert_eq!(store.model().0, 123);

        drop(store);
        
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), None).unwrap();
        assert_eq!(store.model().0, 123);
        store.undo();
        assert_eq!(store.model().0, 0);
    }

    #[test]
    fn file_undo_store_undo_and_add() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), None).unwrap();
        store.add(1).unwrap();
        store.add(2).unwrap();
        store.add(3).unwrap();
        // 1, 2, 3
        assert_eq!(store.model().0, 6);

        store.undo();
        // 1, 2
        store.undo();
        // 1

        store.add(100).unwrap();
        // 1, 100
        assert_eq!(store.model().0, 101);
    }

    #[test]
    fn file_undo_store_can_set_limit() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), Some(5)).unwrap();
        store.add(1).unwrap();
        store.add(2).unwrap();
        store.add(3).unwrap();
        store.add(4).unwrap();
        store.add(5).unwrap();
        store.add(6).unwrap(); // 2, 3, 4, 5, 6
        assert_eq!(store.model().0, 1 + 2 + 3 + 4 + 5 + 6);

        store.undo();
        store.undo();
        store.undo();
        store.undo();
        store.undo();
        assert_eq!(store.model().0, 1);

        store.undo(); // Just ignored.
        assert_eq!(store.model().0, 1);
    }


    #[test]
    fn file_undo_store_can_set_limit_and_recover() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), Some(5)).unwrap();
        store.add(1).unwrap();
        store.add(2).unwrap();
        store.add(3).unwrap();
        store.add(4).unwrap();
        store.add(5).unwrap();
        store.add(6).unwrap(); // 2, 3, 4, 5, 6 (1 is deleted since undo limit is 5.)
        assert_eq!(store.model().0, 1 + 2 + 3 + 4 + 5 + 6);
        drop(store);

        let store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), Some(5)).unwrap();
        assert_eq!(store.model().0, 1 + 2 + 3 + 4 + 5 + 6);
    }

    #[test]
    fn file_undo_store_can_restore() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), Some(5)).unwrap();
        store.add(1).unwrap();
        store.add(2).unwrap();
        store.add(3).unwrap();
        store.add(4).unwrap();
        assert_eq!(cmd_ids(&store.conn), [1, 2, 3, 4]);

        store.add(5).unwrap();
        assert_eq!(cmd_ids(&store.conn), [1, 2, 3, 4, 5]);

        // 2, 3, 4, 5, 6 (1 is deleted since undo limit is 5.)
        // snapshot of 6 is taken.
        store.add(6).unwrap();
        assert_eq!(cmd_ids(&store.conn), [2, 3, 4, 5, 6]);

        // [1] -cmd2(+2)-> [3] -cmd3(+3)-> [6] -cmd4(+4)-> [10] -cmd5(+5)-> [15] -cmd6(+6)-> [21]
        //                                                                                   ^ snapshot(id=6)
        assert_eq!(store.model().0, 1 + 2 + 3 + 4 + 5 + 6);
        assert_eq!(snapshot_ids(&store.conn), [6]);

        drop(store);
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), Some(5)).unwrap();
        assert_eq!(store.model().0, 1 + 2 + 3 + 4 + 5 + 6);
        assert_eq!(snapshot_ids(&store.conn), [6]);

        store.undo();
        // [1] -cmd2(+2)-> [3] -cmd3(+3)-> [6] -cmd4(+4)-> [10] -cmd5(+5)-> [15] -cmd6(+6)-> [21]
        //                                                                  ^ current        ^ snapshot(id=6)
        assert_eq!(store.model().0, 1 + 2 + 3 + 4 + 5);
        assert_eq!(snapshot_ids(&store.conn), [6]);

        drop(store);
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), Some(5)).unwrap();
        assert_eq!(store.model().0, 1 + 2 + 3 + 4 + 5);

        store.undo();
        // [1] -cmd2(+2)-> [3] -cmd3(+3)-> [6] -cmd4(+4)-> [10] -cmd5(+5)-> [15] -cmd6(+6)-> [21]
        //                                                 ^ current                         ^ snapshot(id=6)
        assert_eq!(store.model().0, 1 + 2 + 3 + 4);

        drop(store);
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), Some(5)).unwrap();
        assert_eq!(store.model().0, 1 + 2 + 3 + 4);

        store.add(7).unwrap();
        // [1] -cmd2(+2)-> [3] -cmd3(+3)-> [6] -cmd4(+4)-> [10] -cmd5(+7)-> [15]
        //                                                                  ^ snapshot(id=5)
        assert_eq!(store.model().0, 1 + 2 + 3 + 4 + 7);
        assert_eq!(cmd_ids(&store.conn), [2, 3, 4, 5]);
        assert_eq!(snapshot_ids(&store.conn), [5]);

        drop(store);

        let store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), Some(5)).unwrap();
        assert_eq!(store.model().0, 1 + 2 + 3 + 4 + 7);
    }

    #[test]
    fn can_save_as() {
        use tempfile::tempdir;

        let from_dir = tempdir().unwrap();
        let mut from_dir = from_dir.as_ref().to_path_buf();
        from_dir.push("klavier");

        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(from_dir.clone(), Some(5)).unwrap();
        store.add(1).unwrap();
        store.add(2).unwrap();
        store.add(3).unwrap();
        store.add(4).unwrap();
        store.add(5).unwrap();

        // 2, 3, 4, 5, 6 (1 is deleted since undo limit is 5.)
        // snapshot of 6 is taken.
        store.add(6).unwrap();

        let to_dir = tempdir().unwrap();
        let to_dir = to_dir.as_ref().to_path_buf();
        
        let mut saved_store = store.save_as(to_dir).unwrap();
        drop(store);

        assert_eq!(saved_store.model().0, 1 + 2 + 3 + 4 + 5 + 6);
        
        saved_store.undo();
        assert_eq!(saved_store.model().0, 1 + 2 + 3 + 4 + 5);

        saved_store.redo();
        assert_eq!(saved_store.model().0, 1 + 2 + 3 + 4 + 5 + 6);

        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(from_dir.clone(), Some(5)).unwrap();
        assert_eq!(store.model().0, 1 + 2 + 3 + 4 + 5 + 6);
        store.undo();
        assert_eq!(store.model().0, 1 + 2 + 3 + 4 + 5);
        store.redo();
        assert_eq!(store.model().0, 1 + 2 + 3 + 4 + 5 + 6);
    }

    #[test]
    fn can_restore_undo_point() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), Some(5)).unwrap();
        store.add(1).unwrap();
        store.add(2).unwrap();
        store.add(3).unwrap();

        assert_eq!(store.model().0, 6);

        store.undo(); // 2, 3, 4, 5
        store.undo(); // 2, 3, 4
        assert_eq!(store.model().0, 1);

        drop(store);

        let store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), Some(5)).unwrap();
        assert_eq!(store.model().0, 1);
    }

    #[test]
    fn can_restore_undo_point_pattern1() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), Some(5)).unwrap();
        store.add(1).unwrap();
        store.add(2).unwrap();

        store.save_snapshot().unwrap();
        //     Snapshot
        // +1, +2
        //      ^cur_seq_no

        assert_eq!(store.model().0, 3);

        store.undo();
        //     Snapshot
        // +1, +2
        //  ^cur_seq_no

        assert_eq!(store.model().0, 1);

        drop(store);

        let store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), Some(5)).unwrap();
        assert_eq!(store.model().0, 1);
    }

    #[test]
    fn can_restore_undo_point_pattern2() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), Some(5)).unwrap();
        store.add(1).unwrap();
        store.add(2).unwrap();

        store.save_snapshot().unwrap();
        //     Snapshot
        // +1, +2
        //      ^cur_seq_no

        store.add(3).unwrap();
        //     Snapshot
        // +1, +2, +3
        //          ^cur_seq_no

        assert_eq!(store.model().0, 6);

        store.undo();
        //     Snapshot
        // +1, +2, +3
        //      ^cur_seq_no

        assert_eq!(store.model().0, 3);

        drop(store);

        let store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), Some(5)).unwrap();
        assert_eq!(store.model().0, 3);
    }

    #[test]
    fn can_restore_undo_point_pattern3() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), Some(5)).unwrap();
        store.add(1).unwrap();
        store.add(2).unwrap();
        assert_eq!(cmd_ids(&store.conn), [1, 2]);
        assert_eq!(snapshot_ids(&store.conn), Vec::new() as Vec<i64>);

        store.save_snapshot().unwrap();
        assert_eq!(snapshot_ids(&store.conn), [2]);
        //     Snapshot
        // +1, +2
        //      ^cur_seq_no

        store.add(3).unwrap();
        store.add(4).unwrap();
        //     Snapshot
        // +1, +2, +3, +4
        //              ^cur_seq_no

        assert_eq!(store.model().0, 10);

        store.undo();
        //     Snapshot
        // +1, +2, +3, +4
        //          ^cur_seq_no

        assert_eq!(store.model().0, 6);
        assert_eq!(cmd_ids(&store.conn), [1, 2, 3, 4]);

        drop(store);

        let store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), Some(5)).unwrap();
        assert_eq!(cmd_ids(&store.conn), [1, 2, 3, 4]);
        assert_eq!(store.model().0, 6);
    }

    fn snapshot_ids(conn: &rusqlite::Connection) -> Vec<i64> {
        let mut stmt = conn.prepare(
            "select snapshot_id from snapshot order by snapshot_id asc"
        ).unwrap();
        let mut rows = stmt.query([]).unwrap();

        let mut ids: Vec<i64> = vec![];
        loop {
            let row = rows.next().unwrap();
            match row {
                None => break,
                Some(rec) => ids.push(rec.get(0).unwrap()),
            }
        }            

        ids
    }

    fn cmd_ids(conn: &rusqlite::Connection) -> Vec<i64> {
        let mut stmt = conn.prepare(
            "select rowid from command order by rowid asc"
        ).unwrap();
        let mut rows = stmt.query([]).unwrap();

        let mut ids: Vec<i64> = vec![];
        loop {
            let row = rows.next().unwrap();
            match row {
                None => break,
                Some(rec) => ids.push(rec.get(0).unwrap()),
            }
        }            

        ids
    }

    #[test]
    fn snapshots_are_created() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), Some(3)).unwrap();
        store.add(1).unwrap(); // cmd1
        store.add(2).unwrap(); // cmd2
        store.add(3).unwrap(); // cmd3

        // [0] -cmd1(+1)-> [1] -cmd2(+2)-> [3] -cmd3(+3)-> [6]
        assert_eq!(snapshot_ids(&store.conn).len(), 0);

        // cmd1 should be removed because undo_limit = 3.
        store.add(4).unwrap();
        // [1] -cmd2(+2)-> [3] -cmd3(+3)-> [6] -cmd4(+4)-> [10]
        //                                                 ^ snap(id=4)

        let ids = snapshot_ids(&store.conn);
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], 4);

        store.add(5).unwrap();
        // [3] -cmd3(+3)-> [6] -cmd4(+4)-> [10] -cmd5(+5)-> [15]
        //                                 ^ snap(id=4)
        assert_eq!(store.model().0, 15);
        let ids = snapshot_ids(&store.conn);
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], 4);

        store.add(6).unwrap();
        // [6] -cmd4(+4)-> [10] -cmd5(+5)-> [15] -cmd6(+6)-> [21]
        //                 ^ snap(id=4)
        assert_eq!(store.model().0, 21);
        let ids = snapshot_ids(&store.conn);
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], 4);

        store.add(7).unwrap();
        // [10] -cmd5(+5)-> [15] -cmd6(+6)-> [21] -cmd7(+7)-> [28]
        // ^ snap(id=4)
        assert_eq!(store.model().0, 28);
        let ids = snapshot_ids(&store.conn);
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], 4);

        store.add(8).unwrap();
        // [15] -cmd6(+6)-> [21] -cmd7(+7)-> [28] -cmd8(+8)-> [36]
        //                                                    ^ snap(id=8)
        assert_eq!(store.model().0, 36);
        let ids = snapshot_ids(&store.conn);
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], 8);

        drop(store);
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), None).unwrap();
        assert_eq!(store.model().0, 36);

        store.undo();
        assert_eq!(store.model().0, 28);
    }
}
