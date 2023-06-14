use crate::cmd::Cmd;

pub trait UndoStore {
    type ModelType;
    type CmdType: Cmd<Model = Self::ModelType>;
    type ErrType;

    fn model(&self) -> &Self::ModelType;
    fn mutate(&mut self, f: Box<dyn FnOnce(&mut Self::ModelType) -> Result<Self::CmdType, Self::ErrType>>) -> Result<(), Self::ErrType>;
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
#[derive(Debug)]
pub enum SqliteUndoStoreError {
    // Add
    CannotWriteCmd(std::path::PathBuf, std::io::Error),
    SerializeError(bincode::Error),
    NeedCompaction(std::path::PathBuf),

    // Load
    NotFound(std::path::PathBuf),
    CannotReadCmd(std::path::PathBuf, std::io::Error),
    DeserializeError(serde_json::Error),

    // Undo/Redo
    CannotUndoRedo,

    // Common
    FileError(std::path::PathBuf, std::io::Error),
    NotADirectory,
    CannotLock(std::path::PathBuf),
    CannotDeserialize(Option<std::path::PathBuf>, i64, bincode::Error),
    OrphanSnapshot { snapshot_id: i64, command_id: i64 },
    DbError(std::path::PathBuf, rusqlite::Error),
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
        )
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
            commit;"
        )?;
        Ok(())
    }

    // Open the specified directory or newly create it if that does not exist.
    pub fn open<P: AsRef<std::path::Path>>(dir: P, undo_limit: Option<usize>) -> Result<Self, SqliteUndoStoreError>
        where C: crate::cmd::SerializableCmd<Model = M> + serde::de::DeserializeOwned
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
            std::fs::create_dir_all(dir.as_ref()).map_err(|e| SqliteUndoStoreError::FileError(dir.as_ref().to_path_buf(), e))?;
            Self::try_lock(dir.as_ref())?;
            Self::create_new(&sqlite_path)
        }.map_err(|e|
            SqliteUndoStoreError::DbError(copy_sqlite_path, e)
        )?;
        let mut store = SqliteUndoStore {
            base_dir: dir.as_ref().to_path_buf(), model: M::default(), 
            cur_cmd_seq_no: 0, phantom: std::marker::PhantomData, phantome: std::marker::PhantomData,
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
    fn db_add_cmd<F, T>(&self, f: F) -> Result<T, SqliteUndoStoreError> where F: FnOnce() -> Result<T, rusqlite::Error> {
        f().map_err(|e| SqliteUndoStoreError::DbError(self.sqlite_path.clone(), e))
    }

    #[inline]
    fn db_can_undo<F, T>(&self, f: F) -> Result<T, SqliteUndoStoreError> where F: FnOnce() -> Result<T, rusqlite::Error> {
        f().map_err(|e| SqliteUndoStoreError::DbError(self.sqlite_path.clone(), e))
    }

    #[inline]
    fn db_undo<F, T>(&self, f: F) -> Result<T, SqliteUndoStoreError> where F: FnOnce() -> Result<T, rusqlite::Error> {
        f().map_err(|e| SqliteUndoStoreError::DbError(self.sqlite_path.clone(), e))
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
                SqliteUndoStoreError::CannotDeserialize(None, id, e)
            )?;
            Ok((id, snapshot))
        } else {
            Ok((0, M::default()))
        }
    }

    fn restore_model(&mut self) -> Result<(), SqliteUndoStoreError>
        where C: crate::cmd::SerializableCmd<Model = M> + serde::de::DeserializeOwned
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
                SqliteUndoStoreError::CannotDeserialize(None, id, e)
            )?;

            cmd.redo(&mut model);
            last_id = id;
        }

        while let Some(row) = self.db(|| rows.next())? {
            let id: i64 = self.db(|| row.get(0))?;
            let serialized: Vec<u8> = self.db(|| row.get(1))?;
            let cmd: C = bincode::deserialize(&serialized).map_err(|e|
                SqliteUndoStoreError::CannotDeserialize(None, id, e)
            )?;

            cmd.redo(&mut model);
            last_id = id;
        }
        self.model = model;
        self.cur_cmd_seq_no = last_id;

        Ok(())
    }

    fn trim_undo_records(&self) -> Result<usize, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "delete from command as c0 where c0.rowid not in (
                select rowid from command as c1 order by rowid desc limit ?1
            )"
        )?;

        Ok(stmt.execute(rusqlite::params![self.undo_limit])?)
    }

    fn trim_snapshots(&self) -> Result<usize, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "delete from snapshot where ?1 < snapshot_id"
        )?;

        Ok(stmt.execute(rusqlite::params![self.undo_limit])?)
    }

    fn save_snapshot(&self) -> Result<(), SqliteUndoStoreError> {
        let mut stmt = 
        self.db_add_cmd(|| self.conn.prepare(
            "insert into snapshot (snapshot_id, serialized) values (?1, ?2)"
        ))?;
        let serialized = bincode::serialize(&self.model).map_err(|e|
            SqliteUndoStoreError::SerializeError(e)
        )?;
        self.db_add_cmd(|| stmt.execute(rusqlite::params![self.cur_cmd_seq_no, serialized]))?;

        Ok(())
    }

    fn _add_cmd(&mut self, cmd: C) -> Result<(), SqliteUndoStoreError> {
        let serialized: Vec<u8> = bincode::serialize(&cmd).map_err(
            |e| SqliteUndoStoreError::SerializeError(e)
        )?;
        self.db_add_cmd(|| self.conn.execute(
            "insert into command (serialized) values (?1) ", rusqlite::params![serialized])
        )?;

        self.cur_cmd_seq_no = self.conn.last_insert_rowid();
        if self.cur_cmd_seq_no == MAX_ROWID {
            return Err(SqliteUndoStoreError::NeedCompaction(self.sqlite_path.clone()));
        }
        self.db_add_cmd(|| self.trim_snapshots())?;
        let removed_count = self.db_add_cmd(|| self.trim_undo_records())?;
        if removed_count != 0 {
            self.save_snapshot()?;
        }
        Ok(())
    }

    fn _can_undo(&self) -> Result<bool, SqliteUndoStoreError> {
        let mut stmt = self.db_can_undo(|| self.conn.prepare(
            "select 1 from command where rowid <= ?1"
        ))?;
        let mut rows = self.db_can_undo(|| stmt.query(rusqlite::params![self.cur_cmd_seq_no]))?;
        let row = self.db_can_undo(|| rows.next())?;
        Ok(row.is_some())
    }

    fn _undo(&mut self) -> Result<(), SqliteUndoStoreError> {
        let mut stmt = self.db_undo(|| self.conn.prepare(
            "select serialized from command where rowid = ?1"
        ))?;
        let mut rows = self.db_undo(|| stmt.query(rusqlite::params![self.cur_cmd_seq_no]))?;
        if let Some(row) = self.db_undo(|| rows.next())? {
            let serialized: Vec<u8> = self.db_undo(|| row.get(0))?;
            let cmd: C = bincode::deserialize(&serialized).map_err(|e|
                SqliteUndoStoreError::CannotDeserialize(
                    Some(self.sqlite_path.clone()), self.cur_cmd_seq_no, e
                )
            )?;
            let res = cmd.undo(&mut self.model);
            self.cur_cmd_seq_no -= 1;
            Ok(res)
        } else {
            Err(SqliteUndoStoreError::CannotUndoRedo)
        }
    }

    fn _can_redo(&self) -> Result<bool, SqliteUndoStoreError> {
        let mut stmt = self.db_can_undo(|| self.conn.prepare(
            "select 1 from command where ?1 < rowid"
        ))?;
        let mut rows = self.db_can_undo(|| stmt.query(rusqlite::params![self.cur_cmd_seq_no]))?;
        let row = self.db_can_undo(|| rows.next())?;
        Ok(row.is_some())
    }

    fn _redo(&mut self) -> Result<(), SqliteUndoStoreError> {
        let mut  stmt = self.db_undo(|| self.conn.prepare(
            "select serialized from command where rowid = ?1"
        ))?;
        let mut rows = self.db_undo(|| stmt.query(rusqlite::params![self.cur_cmd_seq_no + 1]))?;
        if let Some(row) = self.db_undo(|| rows.next())? {
            let serialized: Vec<u8> = self.db_undo(|| row.get(0))?;
            let cmd: C = bincode::deserialize(&serialized).map_err(|e|
                SqliteUndoStoreError::CannotDeserialize(
                    Some(self.sqlite_path.clone()), self.cur_cmd_seq_no, e
                )
            )?;

            cmd.redo(&mut self.model);
            self.cur_cmd_seq_no += 1;
            Ok(())
        } else {
            Err(SqliteUndoStoreError::CannotUndoRedo)
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
        if let Err(e) = self._undo() {
            panic!("Cannot access database {:?}.", e);
        }
    }

    fn can_redo(&self) -> bool {
        match self._can_redo() {
            Ok(b) => b,
            Err(e) => panic!("Cannot access database {:?}.", e),
        }
    }

    fn redo(&mut self) {
        if let Err(e) = self._redo() {
            panic!("Cannot access database {:?}.", e);
        }
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
        match store2_err {
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

    #[should_panic]
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

        store.undo();
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
        store.add(5).unwrap();

        // 2, 3, 4, 5, 6 (1 is deleted since undo limit is 5.)
        // snapshot of 6 is taken.
        store.add(6).unwrap();

        store.undo(); // 2, 3, 4, 5
        store.undo(); // 2, 3, 4
        assert_eq!(store.model().0, 1 + 2 + 3 + 4);
        store.add(7).unwrap();
        assert_eq!(store.model().0, 1 + 2 + 3 + 4 + 7);

        drop(store);

        let store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), Some(5)).unwrap();
        assert_eq!(store.model().0, 1 + 2 + 3 + 4 + 7);
    }
}
