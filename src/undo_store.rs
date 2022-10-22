use std::marker::PhantomData;
#[cfg(feature = "persistence")]
use std::{fs::{File, OpenOptions, remove_file}, path::{Path, PathBuf}, io};
use rusqlite::{Connection, params};
use serde::{de::DeserializeOwned, Serialize};
use crate::cmd::{Cmd, SerializableCmd};

pub trait UndoStore {
    type ModelType;
    type CmdType;
    type AddCmdRet;
    type UndoErr;
    type CanUndoRet;

    fn model(&self) -> &Self::ModelType;
    fn add_cmd(&mut self, cmd: Self::CmdType) -> Self::AddCmdRet;
    fn can_undo(&self) -> Self::CanUndoRet;
    fn undo(&mut self) -> Self::UndoErr;
    fn can_redo(&self) -> Self::CanUndoRet;
    fn redo(&mut self) -> Self::UndoErr;
}

pub struct InMemoryUndoStore<M> where M: Default {
    model: M,
    store: Vec<Box<dyn Cmd<Model = M>>>,
    location: usize,
}

impl<M> InMemoryUndoStore<M> where M: Default {
    pub fn new(capacity: usize) -> Self {
        Self {
            model: M::default(),
            store: Vec::with_capacity(capacity),
            location: 0,
        }
    }
}

struct InMemorySnapshotCmd<M> {
    model_after_redo: M,
    model_after_undo: M,
}

impl<M> Cmd for InMemorySnapshotCmd<M> where M: Clone {
    type Model = M;

    fn undo(&self, model: &mut Self::Model) {
        *model = self.model_after_undo.clone();
    }

    fn redo(&self, model: &mut Self::Model) {
        *model = self.model_after_redo.clone();
    }
}

impl<M> UndoStore for InMemoryUndoStore<M> where M: Clone + Default + 'static {
    type ModelType = M;
    type AddCmdRet = ();
    type CmdType = Box<dyn Cmd<Model = M>>;
    type UndoErr = ();
    type CanUndoRet = bool;

    fn add_cmd(&mut self, cmd: Self::CmdType) {
        if self.location < self.store.len() {
            self.store.truncate(self.location);
        }
        cmd.redo(&mut self.model);

        while self.store.capacity() <= self.store.len() {
            self.store.remove(0);
        }
    
        self.store.push(cmd);
        self.location = self.store.len();
    }

    #[inline]
    fn can_undo(&self) -> bool {
        0 < self.location
    }

    fn undo(&mut self) {
        if self.can_undo() {
            self.location -= 1;
            let cmd = &self.store[self.location];
            cmd.undo(&mut self.model);
        }
    }

    #[inline]
    fn can_redo(&self) -> bool {
        self.location < self.store.len()
    }

    fn redo(&mut self) {
        if self.can_redo() {
            let cmd = &self.store[self.location];
            cmd.redo(&mut self.model);
            self.location += 1;
        }
    }

    fn model(&self) -> &M {
        &self.model
    }
}

#[cfg(feature = "persistence")]
pub struct SqliteUndoStore<C, M> where M: Default + Serialize + DeserializeOwned {
    phantom: PhantomData<C>,
    base_dir: PathBuf,
    sqlite_path: PathBuf,
    conn: Connection,
    model: M,
    cur_cmd_seq_no: i64,
    undo_limit: usize,
}

#[cfg(feature = "persistence")]
#[derive(Debug)]
pub enum SqliteUndoStoreError {
    FileError(PathBuf, std::io::Error),
    NotADirectory,
    CannotLock(PathBuf),
    CannotDeserialize(bincode::Error),
    OrphanSnapshot { snapshot_id: i64, command_id: i64 },
    DbError(PathBuf, rusqlite::Error),
}

#[cfg(feature = "persistence")]
#[derive(Debug)]
pub enum SqliteUndoStoreAddCmdError {
    CannotWriteCmd(PathBuf, io::Error),
    SerializeError(bincode::Error),
    NeedCompaction(PathBuf),
    DbError(PathBuf, rusqlite::Error),
}

#[cfg(feature = "persistence")]
#[derive(Debug)]
pub enum SqliteUndoStoreLoadCmdError {
    NotFound(PathBuf),
    CannotReadCmd(PathBuf, io::Error),
    DeserializeError(serde_json::Error),
}

#[cfg(feature = "persistence")]
#[derive(Debug)]
pub enum SqliteUndoStoreCanUndoError {
    DbError(PathBuf, rusqlite::Error),
}

#[cfg(feature = "persistence")]
#[derive(Debug)]
pub enum SqliteUndoStoreUndoRedoError {
    CannotUndoRedo,
    DbError(PathBuf, rusqlite::Error),
    CannotDeserialize(PathBuf, i64, bincode::Error),
}

#[cfg(feature = "persistence")]
impl<C, M> SqliteUndoStore<C, M> where M: Default + Serialize + DeserializeOwned {
    fn lock_file_path(base_dir: &Path) -> PathBuf {
        let mut path: PathBuf = base_dir.to_path_buf();
        path.push("lock");
        path
    }

    fn try_lock(base_dir: &Path) -> Result<File, SqliteUndoStoreError> {
        let lock_file_path = Self::lock_file_path(base_dir);
        OpenOptions::new().write(true).create_new(true).open(&lock_file_path).map_err(|_|
            SqliteUndoStoreError::CannotLock(lock_file_path)
        )
    }

    fn unlock(&self) -> io::Result<()> {
        let p = Self::lock_file_path(&self.base_dir);
        remove_file(&p)
    }

    #[inline]
    fn open_existing<P: AsRef<Path>>(path: P) -> rusqlite::Result<Connection> {
        Connection::open(&path)
    }

    fn create_new<P: AsRef<Path>>(path: P) -> rusqlite::Result<Connection> {
        let conn = Connection::open(&path)?;
        Self::create_tables(&conn)?;
        Ok(conn)
    }

    fn create_tables(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch(
            "begin;
            create table command(serialized blob not null);    
            create table snapshot(snapshot_id integer primary key not null, serialized blob not null);    
            commit;"
        )?;
        Ok(())
    }

    // Open the specified directory or newly create it if that does not exist.
    pub fn open<P: AsRef<Path>>(dir: P, undo_limit: Option<usize>) -> Result<Self, SqliteUndoStoreError>
        where C: SerializableCmd<Model = M> + DeserializeOwned
    {
        let mut sqlite_path = dir.as_ref().to_path_buf();
        sqlite_path.push("db.sqlite");
        let copy_sqlite_path = sqlite_path.clone();

        let conn = if dir.as_ref().exists() {
            if ! dir.as_ref().is_dir() {
                return Err(SqliteUndoStoreError::NotADirectory)
            }
            Self::try_lock(dir.as_ref())?;
            Self::open_existing(&sqlite_path)
        } else {
            std::fs::create_dir(dir.as_ref()).map_err(|e| SqliteUndoStoreError::FileError(dir.as_ref().to_path_buf(), e))?;
            Self::try_lock(dir.as_ref())?;
            Self::create_new(&sqlite_path)
        }.map_err(|e|
            SqliteUndoStoreError::DbError(copy_sqlite_path, e)
        )?;
        let mut store = SqliteUndoStore {
            base_dir: dir.as_ref().to_path_buf(), model: M::default(), 
            cur_cmd_seq_no: 0, phantom: PhantomData,
            undo_limit: undo_limit.unwrap_or(100), sqlite_path, conn,
        };
        store.restore_model()?;
        Ok(store)
    }

    #[inline]
    fn db<F, T>(&self, f: F) -> Result<T, SqliteUndoStoreError> where F: FnOnce() -> Result<T, rusqlite::Error> {
        f().map_err(|e| SqliteUndoStoreError::DbError(self.sqlite_path.clone(), e))
    }

    #[inline]
    fn db_add_cmd<F, T>(&self, f: F) -> Result<T, SqliteUndoStoreAddCmdError> where F: FnOnce() -> Result<T, rusqlite::Error> {
        f().map_err(|e| SqliteUndoStoreAddCmdError::DbError(self.sqlite_path.clone(), e))
    }

    #[inline]
    fn db_can_undo<F, T>(&self, f: F) -> Result<T, SqliteUndoStoreCanUndoError> where F: FnOnce() -> Result<T, rusqlite::Error> {
        f().map_err(|e| SqliteUndoStoreCanUndoError::DbError(self.sqlite_path.clone(), e))
    }

    #[inline]
    fn db_undo<F, T>(&self, f: F) -> Result<T, SqliteUndoStoreUndoRedoError> where F: FnOnce() -> Result<T, rusqlite::Error> {
        f().map_err(|e| SqliteUndoStoreUndoRedoError::DbError(self.sqlite_path.clone(), e))
    }

    fn load_last_snapshot(&self) -> Result<(i64, M), SqliteUndoStoreError> {
        let mut stmt = self.db(|| self.conn.prepare(
            "select snapshot_id, serialized from snapshot order by snapshot_id desc limit 1"
        ))?;
        let mut rows = self.db(|| stmt.query([]))?;

        if let Some(row) = self.db(|| rows.next())? {
            let id: i64 = self.db(|| row.get(0))?;
            let serialized: Vec<u8> = self.db(|| row.get(1))?;
            let snapshot: M = bincode::deserialize(&serialized).map_err(|e|
                SqliteUndoStoreError::CannotDeserialize(e)
            )?;
            Ok((id, snapshot))
        } else {
            Ok((0, M::default()))
        }
    }

    fn restore_model(&mut self) -> Result<(), SqliteUndoStoreError>
        where C: SerializableCmd<Model = M> + DeserializeOwned
    {
        let (mut last_id, mut model) = self.load_last_snapshot()?;
        let mut stmt = self.db(|| self.conn.prepare(
            "select rowid, serialized from command as cmd where ?1 < rowid order by rowid"
        ))?;
        let mut rows = self.db(|| stmt.query([last_id]))?;

        if let Some(first_row) = self.db(|| rows.next())? {
            let id: i64 = self.db(|| first_row.get(0))?;

            if id != last_id + 1 {
                return Err(SqliteUndoStoreError::OrphanSnapshot {
                    snapshot_id: last_id, command_id: id,
                })
            }

            let serialized: Vec<u8> = self.db(|| first_row.get(1))?;
            let cmd: C = bincode::deserialize(&serialized).map_err(|e|
                SqliteUndoStoreError::CannotDeserialize(e)
            )?;

            cmd.redo(&mut model);
            last_id = id;
        }

        while let Some(row) = self.db(|| rows.next())? {
            let id: i64 = self.db(|| row.get(0))?;
            let serialized: Vec<u8> = self.db(|| row.get(1))?;
            let cmd: C = bincode::deserialize(&serialized).map_err(|e|
                SqliteUndoStoreError::CannotDeserialize(e)
            )?;
            cmd.redo(&mut model);
            last_id = id;
        }
        self.model = model;
        self.cur_cmd_seq_no = last_id;

        Ok(())
    }

    #[inline]
    fn rewind_undo_records(&mut self) -> Result<usize, rusqlite::Error> {
        self.conn.execute(
            "delete from command where ?1 < rowid", params![self.cur_cmd_seq_no]
        )
    }

    fn trim_undo_records(&self) -> Result<usize, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "delete from command as c0 where c0.rowid not in (
                select rowid from command as c1 order by rowid desc limit ?1
            )"
        )?;

        Ok(stmt.execute(params![self.undo_limit])?)
    }

    fn trim_snapshots(&self) -> Result<usize, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "delete from snapshot where ?1 < snapshot_id"
        )?;

        Ok(stmt.execute(params![self.undo_limit])?)
    }

    fn save_snapshot(&self) -> Result<(), SqliteUndoStoreAddCmdError> {
        let mut stmt = 
        self.db_add_cmd(|| self.conn.prepare(
            "insert into snapshot (snapshot_id, serialized) values (?1, ?2)"
        ))?;
        let serialized = bincode::serialize(&self.model).map_err(|e|
            SqliteUndoStoreAddCmdError::SerializeError(e)
        )?;
        self.db_add_cmd(|| stmt.execute(params![self.cur_cmd_seq_no, serialized]))?;

        Ok(())
    }
}

#[cfg(feature = "persistence")]
impl<C, M> Drop for SqliteUndoStore<C, M> where M: Default + Serialize + DeserializeOwned {
    fn drop(&mut self) {
        let _ = self.unlock();
    }
}

pub const MAX_ROWID: i64 = 9_223_372_036_854_775_807;

#[cfg(feature = "persistence")]
impl<Command, M> UndoStore for SqliteUndoStore<Command, M>
  where M: Default + Serialize + DeserializeOwned + 'static, Command: SerializableCmd<Model = M>
{
    type ModelType = M;
    type AddCmdRet = Result<(), SqliteUndoStoreAddCmdError>;
    type CmdType = Box<Command>;
    type UndoErr = Result<(), SqliteUndoStoreUndoRedoError>;
    type CanUndoRet = Result<bool, SqliteUndoStoreCanUndoError>;

    fn model(&self) -> &M { &self.model }


    fn add_cmd(&mut self, cmd: Self::CmdType) -> Self::AddCmdRet {
        let serialized: Vec<u8> = bincode::serialize(&cmd).map_err(|e| SqliteUndoStoreAddCmdError::SerializeError(e))?;
        self.db_add_cmd(|| self.conn.execute(
            "insert into command (serialized) values (?1) ", params![serialized])
        )?;

        cmd.redo(&mut self.model);
        self.cur_cmd_seq_no = self.conn.last_insert_rowid();
        if self.cur_cmd_seq_no == MAX_ROWID {
            return Err(SqliteUndoStoreAddCmdError::NeedCompaction(self.sqlite_path.clone()));
        }
        self.db_add_cmd(|| self.trim_snapshots())?;
        let removed_count = self.db_add_cmd(|| self.trim_undo_records())?;
        if removed_count != 0 {
            self.save_snapshot()?;
        }
        Ok(())
    }

    fn can_undo(&self) -> Self::CanUndoRet {
        let mut stmt = self.db_can_undo(|| self.conn.prepare(
            "select 1 from command where rowid <= ?1"
        ))?;
        let mut rows = self.db_can_undo(|| stmt.query(params![self.cur_cmd_seq_no]))?;
        let row = self.db_can_undo(|| rows.next())?;
        Ok(row.is_some())
    }

    fn undo(&mut self) -> Self::UndoErr {
        let mut stmt = self.db_undo(|| self.conn.prepare(
            "select serialized from command where rowid = ?1"
        ))?;
        let mut rows = self.db_undo(|| stmt.query(params![self.cur_cmd_seq_no]))?;
        if let Some(row) = self.db_undo(|| rows.next())? {
            let serialized: Vec<u8> = self.db_undo(|| row.get(0))?;
            let cmd: Self::CmdType = bincode::deserialize(&serialized).map_err(|e|
                SqliteUndoStoreUndoRedoError::CannotDeserialize(
                    self.sqlite_path.clone(), self.cur_cmd_seq_no, e
                )
            )?;
            cmd.undo(&mut self.model);
            self.cur_cmd_seq_no -= 1;
            Ok(())
        } else {
            Err(SqliteUndoStoreUndoRedoError::CannotUndoRedo)
        }
    }

    fn can_redo(&self) -> Self::CanUndoRet {
        let mut stmt = self.db_can_undo(|| self.conn.prepare(
            "select 1 from command where ?1 < rowid"
        ))?;
        let mut rows = self.db_can_undo(|| stmt.query(params![self.cur_cmd_seq_no]))?;
        let row = self.db_can_undo(|| rows.next())?;
        Ok(row.is_some())
    }

    fn redo(&mut self) -> Self::UndoErr {
        let mut  stmt = self.db_undo(|| self.conn.prepare(
            "select serialized from command where rowid = ?1"
        ))?;
        let mut rows = self.db_undo(|| stmt.query(params![self.cur_cmd_seq_no + 1]))?;
        if let Some(row) = self.db_undo(|| rows.next())? {
            let serialized: Vec<u8> = self.db_undo(|| row.get(0))?;
            let cmd: Self::CmdType = bincode::deserialize(&serialized).map_err(|e|
                SqliteUndoStoreUndoRedoError::CannotDeserialize(
                    self.sqlite_path.clone(), self.cur_cmd_seq_no, e
                )
            )?;
            cmd.redo(&mut self.model);
            self.cur_cmd_seq_no += 1;
            Ok(())
        } else {
            Err(SqliteUndoStoreUndoRedoError::CannotUndoRedo)
        }
    }
}

#[cfg(test)]
mod tests {
    use serde::{Serialize, Deserialize};
    use tempfile::tempdir;
    use crate::{undo_store::SqliteUndoStoreError, cmd::SerializableCmd};
    use super::{Cmd, InMemoryUndoStore, UndoStore, SqliteUndoStore};

    enum SumCmd {
        Add(i32), Sub(i32),
    }

    #[derive(PartialEq, Debug, Clone)]
    struct Sum(i32);

    impl Default for Sum {
        fn default() -> Self {
            Self(0)
        }
    }

    impl Cmd for SumCmd {
        type Model = Sum;

        fn undo(&self, model: &mut Self::Model) {
            match self {
                SumCmd::Add(i) => model.0 -= *i,
                SumCmd::Sub(i) => model.0 += *i,
            }
        }

        fn redo(&self, model: &mut Self::Model) {
            match self {
                SumCmd::Add(i) => model.0 += *i,
                SumCmd::Sub(i) => model.0 -= *i,
            }
        }
    }

    trait Model {
        fn add(&mut self, to_add: i32);
        fn sub(&mut self, to_sub: i32);
    }

    impl Model for InMemoryUndoStore<Sum> {
        fn add(&mut self, to_add: i32) {
            self.add_cmd(Box::new(SumCmd::Add(to_add)));
        }

        fn sub(&mut self, to_sub: i32) {
            self.add_cmd(Box::new(SumCmd::Sub(to_sub)));
        }
    }

    #[test]
    fn can_undo_in_memory_store() {
        let mut store: InMemoryUndoStore<Sum> = InMemoryUndoStore::new(3);
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
        let mut store: InMemoryUndoStore<Sum> = InMemoryUndoStore::new(3);
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
        let mut store: InMemoryUndoStore<Sum> = InMemoryUndoStore::new(3);

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

    #[cfg(feature = "persistence")]
    #[derive(Serialize, Deserialize)]
    struct SerSum(i32);

    #[cfg(feature = "persistence")]
    impl Default for SerSum {
        fn default() -> Self {
            Self(0)
        }
    }

    #[cfg(feature = "persistence")]
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    enum SerSumCmd {
        Add(i32), Sub(i32),
    }

    #[cfg(feature = "persistence")]
    impl Cmd for SerSumCmd {
        type Model = SerSum;

        fn undo(&self, model: &mut Self::Model) {
            match self {
                SerSumCmd::Add(i) => { model.0 -= i },
                SerSumCmd::Sub(i) => { model.0 += i },
            }
        }

        fn redo(&self, model: &mut Self::Model) {
            match self {
                SerSumCmd::Add(i) => { model.0 += i },
                SerSumCmd::Sub(i) => { model.0 -= i },
            }
        }
    }

    #[cfg(feature = "persistence")]
    impl SerializableCmd for SerSumCmd {
    }

    #[cfg(feature = "persistence")]
    trait SerModel {
        fn add(&mut self, to_add: i32) -> Result<(), super::SqliteUndoStoreAddCmdError>;
        fn sub(&mut self, to_sub: i32) -> Result<(), super::SqliteUndoStoreAddCmdError>;
    }

    #[cfg(feature = "persistence")]
    impl SerModel for SqliteUndoStore::<SerSumCmd, SerSum> {
        fn add(&mut self, to_add: i32) -> Result<(), super::SqliteUndoStoreAddCmdError> {
            self.add_cmd(Box::new(SerSumCmd::Add(to_add)))
        }

        fn sub(&mut self, to_sub: i32) -> Result<(), super::SqliteUndoStoreAddCmdError> {
            self.add_cmd(Box::new(SerSumCmd::Sub(to_sub)))
        }
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn file_store_can_lock() {
        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let store = SqliteUndoStore::<SerSumCmd, SerSum>::open(dir.clone(), None).unwrap();

        let store2_err = SqliteUndoStore::<SerSumCmd, SerSum>::open(dir.clone(), None).err().unwrap();
        match store2_err {
            SqliteUndoStoreError::CannotLock(err_dir) => {
                let lock_file_path = SqliteUndoStore::<SerSumCmd, i32>::lock_file_path(&dir);
                assert_eq!(err_dir.as_path(), lock_file_path);
            },
            _ => {panic!("Test failed. {:?}", store2_err)},
        };

        drop(store);

        let _ = SqliteUndoStore::<SerSumCmd, SerSum>::open(dir.clone(), None).unwrap();
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn can_serialize_cmd() {
        use rusqlite::{Connection, params};

        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = SqliteUndoStore::<SerSumCmd, SerSum>::open(dir.clone(), None).unwrap();

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

    #[cfg(feature = "persistence")]
    #[test]
    fn can_undo_serialize_cmd() {
        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = SqliteUndoStore::<SerSumCmd, SerSum>::open(dir.clone(), None).unwrap();

        store.add(123).unwrap();
        assert_eq!(store.model().0, 123);
        store.sub(234).unwrap();
        assert_eq!(store.model().0, 123 - 234);

        assert!(store.can_undo().unwrap());
        store.undo().unwrap();
        assert_eq!(store.model().0, 123);

        store.undo().unwrap();
        assert_eq!(store.model().0, 0);

        store.redo().unwrap();
        assert_eq!(store.model().0, 123);

        store.redo().unwrap();
        assert_eq!(store.model().0, 123 - 234);
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn file_undo_store_can_serialize() {
        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = SqliteUndoStore::<SerSumCmd, SerSum>::open(dir.clone(), None).unwrap();
        store.add(123).unwrap();
        assert_eq!(store.model().0, 123);

        drop(store);
        
        let mut store = SqliteUndoStore::<SerSumCmd, SerSum>::open(dir.clone(), None).unwrap();
        assert_eq!(store.model().0, 123);
        store.undo().unwrap();
        assert_eq!(store.model().0, 0);
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn file_undo_store_undo_and_add() {
        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = SqliteUndoStore::<SerSumCmd, SerSum>::open(dir.clone(), None).unwrap();
        store.add(1).unwrap();
        store.add(2).unwrap();
        store.add(3).unwrap();
        // 1, 2, 3
        assert_eq!(store.model().0, 6);

        store.undo().unwrap();
        // 1, 2
        store.undo().unwrap();
        // 1

        store.add(100).unwrap();
        // 1, 100
        assert_eq!(store.model().0, 101);
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn file_undo_store_can_set_limit() {
        use super::SqliteUndoStoreUndoRedoError;

        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = SqliteUndoStore::<SerSumCmd, SerSum>::open(dir.clone(), Some(5)).unwrap();
        store.add(1).unwrap();
        store.add(2).unwrap();
        store.add(3).unwrap();
        store.add(4).unwrap();
        store.add(5).unwrap();
        store.add(6).unwrap(); // 2, 3, 4, 5, 6
        assert_eq!(store.model().0, 1 + 2 + 3 + 4 + 5 + 6);

        store.undo().unwrap();
        store.undo().unwrap();
        store.undo().unwrap();
        store.undo().unwrap();
        store.undo().unwrap();
        assert_eq!(store.model().0, 1);

        let err = store.undo().err().unwrap();
        match &err {
            SqliteUndoStoreUndoRedoError::CannotUndoRedo => {},
            _ => panic!("Error {:?}", err),
        }
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn file_undo_store_can_set_limit_and_recover() {
        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = SqliteUndoStore::<SerSumCmd, SerSum>::open(dir.clone(), Some(5)).unwrap();
        store.add(1).unwrap();
        store.add(2).unwrap();
        store.add(3).unwrap();
        store.add(4).unwrap();
        store.add(5).unwrap();
        store.add(6).unwrap(); // 2, 3, 4, 5, 6 (1 is deleted since undo limit is 5.)
        assert_eq!(store.model().0, 1 + 2 + 3 + 4 + 5 + 6);
        drop(store);

        let store = SqliteUndoStore::<SerSumCmd, SerSum>::open(dir.clone(), Some(5)).unwrap();
        assert_eq!(store.model().0, 1 + 2 + 3 + 4 + 5 + 6);
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn file_undo_store_can_restore() {
        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = SqliteUndoStore::<SerSumCmd, SerSum>::open(dir.clone(), Some(5)).unwrap();
        store.add(1).unwrap();
        store.add(2).unwrap();
        store.add(3).unwrap();
        store.add(4).unwrap();
        store.add(5).unwrap();

        // 2, 3, 4, 5, 6 (1 is deleted since undo limit is 5.)
        // snapshot of 6 is taken.
        store.add(6).unwrap();

        store.undo().unwrap(); // 2, 3, 4, 5
        store.undo().unwrap(); // 2, 3, 4
        assert_eq!(store.model().0, 1 + 2 + 3 + 4);
        store.add(7).unwrap();
        assert_eq!(store.model().0, 1 + 2 + 3 + 4 + 7);

        drop(store);

        let store = SqliteUndoStore::<SerSumCmd, SerSum>::open(dir.clone(), Some(5)).unwrap();
        assert_eq!(store.model().0, 1 + 2 + 3 + 4 + 7);
    }
}
