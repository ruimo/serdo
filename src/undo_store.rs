use crate::cmd::Cmd;

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

pub struct InMemoryUndoStore<C, M> where M: Default {
    model: M,
    store: Vec<C>,
    location: usize,
}

impl<C, M> InMemoryUndoStore<C, M> where M: Default {
    pub fn new(capacity: usize) -> Self {
        Self {
            model: M::default(),
            store: Vec::with_capacity(capacity),
            location: 0,
        }
    }
}

/*
struct InMemorySnapshotCmd<M> {
    model_after_redo: M,
    model_after_undo: M,
}

impl<M> Cmd for InMemorySnapshotCmd<M> where M: Clone {
    type Model = M;
    type RedoResp;
    type RedoErr;

    fn undo(&self, model: &mut Self::Model) {
        *model = self.model_after_undo.clone();
    }

    fn redo(&mut self, model: &mut Self::Model) {
        *model = self.model_after_redo.clone();
    }
}
*/

impl<C, M> UndoStore for InMemoryUndoStore<C, M>
    where M: Default + 'static,
    C: Cmd<Model = M>
{
    type ModelType = M;
    type AddCmdRet = Result<C::RedoResp, C::RedoErr>;
    type CmdType = C;
    type UndoErr = ();
    type CanUndoRet = bool;

    fn add_cmd(&mut self, mut cmd: Self::CmdType) -> Self::AddCmdRet {
        if self.location < self.store.len() {
            self.store.truncate(self.location);
        }
        let resp: Self::AddCmdRet = cmd.redo(&mut self.model);
        if resp.is_err() {
            return resp;
        }

        while self.store.capacity() <= self.store.len() {
            self.store.remove(0);
        }
    
        self.store.push(cmd);
        self.location = self.store.len();
        resp
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
            let cmd = &mut self.store[self.location];
            // redo should success.
            let _ = cmd.redo(&mut self.model);
            self.location += 1;
        }
    }

    fn model(&self) -> &M {
        &self.model
    }
}

#[cfg(feature = "persistence")]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct SqliteUndoStore<C, M> where M: Default + serde::Serialize + serde::de::DeserializeOwned {
    phantom: std::marker::PhantomData<C>,
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
    FileError(std::path::PathBuf, std::io::Error),
    NotADirectory,
    CannotLock(std::path::PathBuf),
    CannotDeserialize(bincode::Error),
    OrphanSnapshot { snapshot_id: i64, command_id: i64 },
    DbError(std::path::PathBuf, rusqlite::Error),
}

#[cfg(feature = "persistence")]
#[derive(Debug)]
pub enum SqliteUndoStoreAddCmdError {
    CannotWriteCmd(std::path::PathBuf, std::io::Error),
    SerializeError(bincode::Error),
    NeedCompaction(std::path::PathBuf),
    DbError(std::path::PathBuf, rusqlite::Error),
}

#[cfg(feature = "persistence")]
#[derive(Debug)]
pub enum SqliteUndoStoreLoadCmdError {
    NotFound(std::path::PathBuf),
    CannotReadCmd(std::path::PathBuf, std::io::Error),
    DeserializeError(serde_json::Error),
}

#[cfg(feature = "persistence")]
#[derive(Debug)]
pub enum SqliteUndoStoreCanUndoError {
    DbError(std::path::PathBuf, rusqlite::Error),
}

#[cfg(feature = "persistence")]
#[derive(Debug)]
pub enum SqliteUndoStoreUndoRedoError {
    CannotUndoRedo,
    DbError(std::path::PathBuf, rusqlite::Error),
    CannotDeserialize(std::path::PathBuf, i64, bincode::Error),
}

#[cfg(feature = "persistence")]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
impl<C, M> SqliteUndoStore<C, M> where M: Default + serde::Serialize + serde::de::DeserializeOwned {
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
            std::fs::create_dir(dir.as_ref()).map_err(|e| SqliteUndoStoreError::FileError(dir.as_ref().to_path_buf(), e))?;
            Self::try_lock(dir.as_ref())?;
            Self::create_new(&sqlite_path)
        }.map_err(|e|
            SqliteUndoStoreError::DbError(copy_sqlite_path, e)
        )?;
        let mut store = SqliteUndoStore {
            base_dir: dir.as_ref().to_path_buf(), model: M::default(), 
            cur_cmd_seq_no: 0, phantom: std::marker::PhantomData,
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
            let mut cmd: C = bincode::deserialize(&serialized).map_err(|e|
                SqliteUndoStoreError::CannotDeserialize(e)
            )?;

            // redo should success.
            let _ = cmd.redo(&mut model);
            last_id = id;
        }

        while let Some(row) = self.db(|| rows.next())? {
            let id: i64 = self.db(|| row.get(0))?;
            let serialized: Vec<u8> = self.db(|| row.get(1))?;
            let mut cmd: C = bincode::deserialize(&serialized).map_err(|e|
                SqliteUndoStoreError::CannotDeserialize(e)
            )?;
            // redo should success.             
            let _ = cmd.redo(&mut model);
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

    fn save_snapshot(&self) -> Result<(), SqliteUndoStoreAddCmdError> {
        let mut stmt = 
        self.db_add_cmd(|| self.conn.prepare(
            "insert into snapshot (snapshot_id, serialized) values (?1, ?2)"
        ))?;
        let serialized = bincode::serialize(&self.model).map_err(|e|
            SqliteUndoStoreAddCmdError::SerializeError(e)
        )?;
        self.db_add_cmd(|| stmt.execute(rusqlite::params![self.cur_cmd_seq_no, serialized]))?;

        Ok(())
    }
}

#[cfg(feature = "persistence")]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
impl<C, M> Drop for SqliteUndoStore<C, M> where M: Default + serde::Serialize + serde::de::DeserializeOwned {
    fn drop(&mut self) {
        let _ = self.unlock();
    }
}

pub const MAX_ROWID: i64 = 9_223_372_036_854_775_807;

#[cfg(feature = "persistence")]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
impl<Command, M> UndoStore for SqliteUndoStore<Command, M>
  where M: Default + serde::Serialize + serde::de::DeserializeOwned + 'static, Command: crate::cmd::SerializableCmd<Model = M>
{
    type ModelType = M;
    type AddCmdRet = Result<Result<Command::RedoResp, Command::RedoErr>, SqliteUndoStoreAddCmdError>;
    type CmdType = Command;
    type UndoErr = Result<(), SqliteUndoStoreUndoRedoError>;
    type CanUndoRet = Result<bool, SqliteUndoStoreCanUndoError>;

    fn model(&self) -> &M { &self.model }

    fn add_cmd(&mut self, mut cmd: Self::CmdType) -> Self::AddCmdRet {
        let ret: Result<<Command as Cmd>::RedoResp, <Command as Cmd>::RedoErr> = cmd.redo(&mut self.model);
        if ret.is_err() {
            return Ok(ret);
        }

        let serialized: Vec<u8> = bincode::serialize(&cmd).map_err(
            |e| SqliteUndoStoreAddCmdError::SerializeError(e)
        )?;
        self.db_add_cmd(|| self.conn.execute(
            "insert into command (serialized) values (?1) ", rusqlite::params![serialized])
        )?;

        self.cur_cmd_seq_no = self.conn.last_insert_rowid();
        if self.cur_cmd_seq_no == MAX_ROWID {
            return Err(SqliteUndoStoreAddCmdError::NeedCompaction(self.sqlite_path.clone()));
        }
        self.db_add_cmd(|| self.trim_snapshots())?;
        let removed_count = self.db_add_cmd(|| self.trim_undo_records())?;
        if removed_count != 0 {
            self.save_snapshot()?;
        }
        Ok(ret)
    }

    fn can_undo(&self) -> Self::CanUndoRet {
        let mut stmt = self.db_can_undo(|| self.conn.prepare(
            "select 1 from command where rowid <= ?1"
        ))?;
        let mut rows = self.db_can_undo(|| stmt.query(rusqlite::params![self.cur_cmd_seq_no]))?;
        let row = self.db_can_undo(|| rows.next())?;
        Ok(row.is_some())
    }

    fn undo(&mut self) -> Self::UndoErr {
        let mut stmt = self.db_undo(|| self.conn.prepare(
            "select serialized from command where rowid = ?1"
        ))?;
        let mut rows = self.db_undo(|| stmt.query(rusqlite::params![self.cur_cmd_seq_no]))?;
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
        let mut rows = self.db_can_undo(|| stmt.query(rusqlite::params![self.cur_cmd_seq_no]))?;
        let row = self.db_can_undo(|| rows.next())?;
        Ok(row.is_some())
    }

    fn redo(&mut self) -> Self::UndoErr {
        let mut  stmt = self.db_undo(|| self.conn.prepare(
            "select serialized from command where rowid = ?1"
        ))?;
        let mut rows = self.db_undo(|| stmt.query(rusqlite::params![self.cur_cmd_seq_no + 1]))?;
        if let Some(row) = self.db_undo(|| rows.next())? {
            let serialized: Vec<u8> = self.db_undo(|| row.get(0))?;
            let mut cmd: Self::CmdType = bincode::deserialize(&serialized).map_err(|e|
                SqliteUndoStoreUndoRedoError::CannotDeserialize(
                    self.sqlite_path.clone(), self.cur_cmd_seq_no, e
                )
            )?;
            // redo() should success.
            let _ = cmd.redo(&mut self.model);
            self.cur_cmd_seq_no += 1;
            Ok(())
        } else {
            Err(SqliteUndoStoreUndoRedoError::CannotUndoRedo)
        }
    }
}
#[cfg(test)]
mod tests {
    use tempfile::tempdir;
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
        type RedoErr = ();
        type RedoResp = ();

        fn undo(&self, model: &mut Self::Model) {
            match self {
                SumCmd::Add(i) => model.0 -= *i,
                SumCmd::Sub(i) => model.0 += *i,
            }
        }

        fn redo(&mut self, model: &mut Self::Model) -> Result<Self::RedoResp, Self::RedoErr> {
            match self {
                SumCmd::Add(i) => {
                    model.0 += *i;
                    Ok(())
                },
                SumCmd::Sub(i) => {
                    model.0 -= *i;
                    Ok(())
                },
            }
        }
    }

    #[derive(PartialEq, Debug)]
    enum CmdErr {
        Err0, Err1
    }

    #[derive(PartialEq, Debug)]
    enum CmdResp {
        Resp0, Resp1
    }

    enum MayErrCmd {
        Add(i32), Sub(i32), SideEffectAdd10(i32),
    }

    impl Cmd for MayErrCmd {
        type Model = Sum;
        type RedoErr = CmdErr;
        type RedoResp = CmdResp;

        fn undo(&self, model: &mut Self::Model) {
            match self {
                MayErrCmd::Add(i) => model.0 -= *i,
                MayErrCmd::Sub(i) => model.0 += *i,
                MayErrCmd::SideEffectAdd10(i) => model.0 -= *i,
            }
        }

        fn redo(&mut self, model: &mut Self::Model) -> Result<Self::RedoResp, Self::RedoErr> {
            match self {
                MayErrCmd::Add(i) => {
                    if *i == 999 {
                        return Err(CmdErr::Err1);
                    }
                    model.0 += *i;
                    Ok(CmdResp::Resp1)
                },
                MayErrCmd::Sub(i) => {
                    model.0 -= *i;
                    Ok(CmdResp::Resp0)
                },
                MayErrCmd::SideEffectAdd10(i) => {
                    *i += 10;
                    model.0 += *i;
                    Ok(CmdResp::Resp1)
                },
            }
        }
    }

    trait Model {
        type Resp;
        fn add(&mut self, to_add: i32) -> Self::Resp;
        fn sub(&mut self, to_sub: i32) -> Self::Resp;
    }

    impl Model for InMemoryUndoStore<SumCmd, Sum> {
        type Resp = ();

        fn add(&mut self, to_add: i32) {
            self.add_cmd(SumCmd::Add(to_add));
        }

        fn sub(&mut self, to_sub: i32) {
            self.add_cmd(SumCmd::Sub(to_sub));
        }
    }

    impl Model for InMemoryUndoStore<MayErrCmd, Sum> {
        type Resp = Result<CmdResp, CmdErr>;
        fn add(&mut self, to_add: i32) -> Self::Resp {
            if to_add == 12345 {
                self.add_cmd(MayErrCmd::SideEffectAdd10(to_add))
            } else {
                self.add_cmd(MayErrCmd::Add(to_add))
            }
        }

        fn sub(&mut self, to_sub: i32) -> Self::Resp{
            self.add_cmd(MayErrCmd::Sub(to_sub))
        }
    }

    #[test]
    fn can_undo_in_memory_store() {
        let mut store: InMemoryUndoStore<SumCmd, Sum> = InMemoryUndoStore::new(3);
        assert_eq!(store.can_undo(), false);
        store.add(3);
        assert_eq!(store.model().0, 3);

        assert_eq!(store.can_undo(), true);
        store.undo();
        assert_eq!(store.model().0, 0);

        assert_eq!(store.can_undo(), false);
    }

    #[test]
    fn can_receive_resp() {
        let mut store: InMemoryUndoStore<MayErrCmd, Sum> = InMemoryUndoStore::new(3);
        assert_eq!(store.add(3), Ok(CmdResp::Resp1));
        assert_eq!(store.add(999), Err(CmdErr::Err1));
        assert_eq!(store.model().0, 3);
    }

    #[test]
    fn can_have_side_effect() {
        let mut store: InMemoryUndoStore<MayErrCmd, Sum> = InMemoryUndoStore::new(3);
        assert_eq!(store.add(12345), Ok(CmdResp::Resp1));
        assert_eq!(store.model().0, 12345 + 10);
    }

    #[test]
    fn can_undo_redo_in_memory_store() {
        let mut store: InMemoryUndoStore<SumCmd, Sum> = InMemoryUndoStore::new(3);
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
        let mut store: InMemoryUndoStore<SumCmd, Sum> = InMemoryUndoStore::new(3);

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
    #[derive(serde::Serialize, serde::Deserialize)]
    struct SerSum(i32);

    #[cfg(feature = "persistence")]
    impl Default for SerSum {
        fn default() -> Self {
            Self(0)
        }
    }

    #[cfg(feature = "persistence")]
    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
    enum SerSumCmd {
        Add(i32), Sub(i32),
    }

    #[cfg(feature = "persistence")]
    impl Cmd for SerSumCmd {
        type Model = SerSum;
        type RedoResp = ();
        type RedoErr = ();

        fn undo(&self, model: &mut Self::Model) {
            match self {
                SerSumCmd::Add(i) => { model.0 -= i },
                SerSumCmd::Sub(i) => { model.0 += i },
            }
        }

        fn redo(&mut self, model: &mut Self::Model) -> Result<Self::RedoResp, Self::RedoErr> {
            match self {
                SerSumCmd::Add(i) => {
                    model.0 += *i;
                    Ok(())
                },
                SerSumCmd::Sub(i) => {
                     model.0 -= *i;
                     Ok(())
                },
            }
        }
    }

    #[cfg(feature = "persistence")]
    impl crate::cmd::SerializableCmd for SerSumCmd {
    }

    #[cfg(feature = "persistence")]
    trait SerModel {
        fn add(&mut self, to_add: i32) -> Result<(), super::SqliteUndoStoreAddCmdError>;
        fn sub(&mut self, to_sub: i32) -> Result<(), super::SqliteUndoStoreAddCmdError>;
    }

    #[cfg(feature = "persistence")]
    impl SerModel for super::SqliteUndoStore::<SerSumCmd, SerSum> {
        fn add(&mut self, to_add: i32) -> Result<(), super::SqliteUndoStoreAddCmdError> {
            self.add_cmd(SerSumCmd::Add(to_add));
            Ok(())
        }

        fn sub(&mut self, to_sub: i32) -> Result<(), super::SqliteUndoStoreAddCmdError> {
            self.add_cmd(SerSumCmd::Sub(to_sub));
            Ok(())
        }
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn file_store_can_lock() {
        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let store = super::SqliteUndoStore::<SerSumCmd, SerSum>::open(dir.clone(), None).unwrap();

        let store2_err = super::SqliteUndoStore::<SerSumCmd, SerSum>::open(dir.clone(), None).err().unwrap();
        match store2_err {
            super::SqliteUndoStoreError::CannotLock(err_dir) => {
                let lock_file_path = crate::undo_store::SqliteUndoStore::<SerSumCmd, i32>::lock_file_path(&dir);
                assert_eq!(err_dir.as_path(), lock_file_path);
            },
            _ => {panic!("Test failed. {:?}", store2_err)},
        };

        drop(store);

        let _ = super::SqliteUndoStore::<SerSumCmd, SerSum>::open(dir.clone(), None).unwrap();
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn can_serialize_cmd() {
        use rusqlite::{Connection, params};

        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum>::open(dir.clone(), None).unwrap();

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
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum>::open(dir.clone(), None).unwrap();

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
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum>::open(dir.clone(), None).unwrap();
        store.add(123).unwrap();
        assert_eq!(store.model().0, 123);

        drop(store);
        
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum>::open(dir.clone(), None).unwrap();
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
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum>::open(dir.clone(), None).unwrap();
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
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum>::open(dir.clone(), Some(5)).unwrap();
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
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum>::open(dir.clone(), Some(5)).unwrap();
        store.add(1).unwrap();
        store.add(2).unwrap();
        store.add(3).unwrap();
        store.add(4).unwrap();
        store.add(5).unwrap();
        store.add(6).unwrap(); // 2, 3, 4, 5, 6 (1 is deleted since undo limit is 5.)
        assert_eq!(store.model().0, 1 + 2 + 3 + 4 + 5 + 6);
        drop(store);

        let store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum>::open(dir.clone(), Some(5)).unwrap();
        assert_eq!(store.model().0, 1 + 2 + 3 + 4 + 5 + 6);
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn file_undo_store_can_restore() {
        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum>::open(dir.clone(), Some(5)).unwrap();
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

        let store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum>::open(dir.clone(), Some(5)).unwrap();
        assert_eq!(store.model().0, 1 + 2 + 3 + 4 + 7);
    }
}
