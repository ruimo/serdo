use std::time::Duration;
use crate::cmd::Cmd;

cfg_if::cfg_if! {
    if #[cfg(feature = "persistence")] {
        use std::path::PathBuf;
        use error_stack::Report;
        use crate::sqlite_undo_store_error::SqliteUndoStoreError;
        use std::path::{Path};
        use error_stack::{Result, report};
        use rusqlite::Connection;
        use std::sync::mpsc::Receiver;
        use std::sync::mpsc;
        use std::{sync::mpsc::Sender, thread};
        use error_stack::bail;
    }
}

pub trait UndoStore {
    type ModelType;
    type CmdType: Cmd<Model = Self::ModelType>;
    type ErrType;

    fn model(&self) -> &Self::ModelType;

    /// Mutate model and add a command. Returns sequence number of the command. If this is an in-memory store, the sequece number is always zero.
    fn mutate(&mut self, f: Box<dyn FnOnce(&mut Self::ModelType) -> Result<Self::CmdType, Self::ErrType>>) -> Result<(), Self::ErrType>;

    /// Mutate a part of model that is out of scope to manage undo/redo operations.
    fn irreversible_mutate<R>(&mut self, f: Box<dyn FnOnce(&mut Self::ModelType) -> R>) -> R where Self: Sized;

    /// Add a command. Returns sequence number of the command. If this is an in-memory store, the sequece number is always zero.
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
#[derive(Debug)]
enum PersistCmd {
    Open { dir: std::path::PathBuf },
    Close,
    AddCmd { seq_no: i64, ser_cmd: Vec<u8> },
    Undo,
    Redo,
}

#[cfg(feature = "persistence")]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Debug)]
enum PersistResp {
    OpenOk { serialized_model: Vec<u8>, seq_no: i64, min_max_seq_no: Option<(i64, i64)> },
    OpenErr(Report<SqliteUndoStoreError>),

    CloseOk,
    CloseErr(Report<SqliteUndoStoreError>),

    AddCmdOk { seq_no: i64 },
    AddCmdErr(Report<SqliteUndoStoreError>),

    UndoOk { seq_no: i64, serialized_command: Vec<u8> },
    UndoErr(Report<SqliteUndoStoreError>),

    RedoOk { seq_no: i64, serialized_command: Vec<u8> }    ,
    RedoErr(Report<SqliteUndoStoreError>),
}

#[cfg(feature = "persistence")]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
enum PersisterServerState<M>
    where M: Default + serde::Serialize + serde::de::DeserializeOwned
{
    Idle,
    Loaded {
        base_dir: std::path::PathBuf,
        sqlite_path: std::path::PathBuf,
        cur_cmd_seq_no: i64,
        model: M,
        conn: rusqlite::Connection,
    }
}

#[cfg(feature = "persistence")]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
struct PersisterServer<C, M, E>
  where C: crate::cmd::SerializableCmd<Model = M>, M: Default + serde::Serialize + serde::de::DeserializeOwned
{
    phantom: std::marker::PhantomData<C>,
    phantome: std::marker::PhantomData<E>,
    receiver: Receiver<PersistCmd>,
    sender: Sender<PersistResp>,
    undo_limit: usize,
    state: PersisterServerState<M>,
}

#[cfg(feature = "persistence")]
enum OpenStatus {
    Idle,
    Opening(PathBuf),
    Ok(PathBuf),
    Err(PathBuf, SqliteUndoStoreError),
}

#[cfg(feature = "persistence")]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
struct PersisterClient {
    receiver: Receiver<PersistResp>,
    sender: Sender<PersistCmd>,
    last_seq_no: i64,
    last_processed_seq_no: Option<i64>,
    min_seq_no: Option<i64>,
    max_seq_no: Option<i64>,
    undo_limit: usize,
}

#[cfg(feature = "persistence")]
impl PersisterClient {
    fn open(receiver: Receiver<PersistResp>, sender: Sender<PersistCmd>, dir: PathBuf, undo_limit: usize)
       -> Result<(Self, Vec<u8>), SqliteUndoStoreError> 
    {
        sender.send(PersistCmd::Open { dir }).unwrap();
        let msg = receiver.recv().unwrap();
        let (serialized_model, seq_no, min_max_seq_no) = match msg {
            PersistResp::OpenOk { serialized_model, seq_no, min_max_seq_no } =>
                (serialized_model, seq_no, min_max_seq_no),
            PersistResp::OpenErr(report) => return Err(report),
            PersistResp::CloseOk => {
                return Err(report!(SqliteUndoStoreError::CmdSequenceError));
            },
            PersistResp::CloseErr(report) => {
                println!("Unexpected close error {:?}", report);
                return Err(report!(SqliteUndoStoreError::CmdSequenceError));
            }
            PersistResp::AddCmdOk { seq_no: _ } => {
                return Err(report!(SqliteUndoStoreError::CmdSequenceError));
            }
            PersistResp::AddCmdErr(err) => {
                return Err(err);
            }
            PersistResp::UndoOk { seq_no: _, serialized_command: _ } => {
                return Err(report!(SqliteUndoStoreError::CmdSequenceError));
            }
            PersistResp::UndoErr(report) => {
                println!("Unexpected undo error {:?}", report);
                return Err(report!(SqliteUndoStoreError::CmdSequenceError));
            }
            PersistResp::RedoOk { seq_no: _, serialized_command: _ } => {
                return Err(report!(SqliteUndoStoreError::CmdSequenceError));
            }
            PersistResp::RedoErr(report) => {
                println!("Unexpected redo error {:?}", report);
                return Err(report!(SqliteUndoStoreError::CmdSequenceError));
            }
        };
        let (min_seq_no, max_seq_no) = if let Some((min, max)) = min_max_seq_no {
            (Some(min), Some(max))
        } else { (None, None) };

        Ok((Self { receiver, sender, last_seq_no: seq_no, min_seq_no, max_seq_no, last_processed_seq_no: None, undo_limit }, serialized_model))
    }

    fn undo(&mut self) -> Result<(i64, Vec<u8>), SqliteUndoStoreError> {
        self.post_cmd(PersistCmd::Undo);
        let resp = self.wait_undo_resp();
        if let Ok(r) = &resp {
            let seq_no = r.0;
            if seq_no != self.last_seq_no {
                println!("Unexpected sequence number: {} != {}", seq_no, self.last_seq_no);
                bail!(SqliteUndoStoreError::CmdSequenceError);
            }
        }

        self.last_seq_no -= 1;
        resp
    }

    fn redo(&mut self) -> Result<(i64, Vec<u8>), SqliteUndoStoreError> {
        self.post_cmd(PersistCmd::Redo);
        let resp = self.wait_redo_resp();

        if let Ok((seq_no, _)) = &resp {
            if *seq_no != self.last_seq_no {
                println!("Unexpected sequence number: {} != {}", seq_no, self.last_seq_no);
                bail!(SqliteUndoStoreError::CmdSequenceError);
            }
        }

        self.last_seq_no += 1;
        resp
    }

    fn post_cmd(&self, cmd: PersistCmd) {
        self.sender.send(cmd).unwrap();
    }

    fn add_command(&mut self, ser_cmd: Vec<u8>) {
        self.post_cmd(PersistCmd::AddCmd { seq_no: self.last_seq_no, ser_cmd });
        self.last_seq_no += 1;
        match self.min_seq_no {
            Some(min_seq_no) => {
                if min_seq_no + (self.undo_limit as i64) <= self.last_seq_no {
                    self.min_seq_no = Some(self.last_seq_no - (self.undo_limit as i64) + 1);
                }
            }
            None => self.min_seq_no = Some(self.last_seq_no),
        }
    }

    fn process_resp(&mut self) -> Result<(), SqliteUndoStoreError> {
        loop {
            match self.receiver.recv_timeout(Duration::ZERO) {
                Ok(resp) => match resp {
                    PersistResp::OpenOk { serialized_model: _, seq_no: _, min_max_seq_no: _ } =>
                        return Err(report!(SqliteUndoStoreError::CmdSequenceError)),
                    PersistResp::OpenErr(err) => {
                        println!("Open error {:?}", err);
                        return Err(report!(SqliteUndoStoreError::CmdSequenceError));
                    }
                    PersistResp::CloseOk => return Err(report!(SqliteUndoStoreError::CmdSequenceError)),
                    PersistResp::CloseErr(err) => {
                        println!("Close error {:?}", err);
                        return Err(report!(SqliteUndoStoreError::CmdSequenceError));
                    }
                    PersistResp::AddCmdOk { seq_no } => {
                        self.last_processed_seq_no = Some(seq_no);
                        self.max_seq_no = Some(seq_no);
                    }
                    PersistResp::AddCmdErr(err) => {
                        return Err(err);
                    }
                    PersistResp::UndoOk { seq_no: _, serialized_command: _ } =>
                        return Err(report!(SqliteUndoStoreError::CmdSequenceError)),
                    PersistResp::UndoErr(err) => {
                        println!("Undo error {:?}", err);
                        return Err(report!(SqliteUndoStoreError::CmdSequenceError));
                    }
                    PersistResp::RedoOk { seq_no: _, serialized_command: _ } =>
                        return Err(report!(SqliteUndoStoreError::CmdSequenceError)),
                    PersistResp::RedoErr(err) => {
                        println!("Redo error {:?}", err);
                        return Err(report!(SqliteUndoStoreError::CmdSequenceError));
                    }
                }
                Err(_) => break,
            }
        }
        Ok(())
    }

    fn wait_undo_resp(&mut self) -> Result<(i64, Vec<u8>), SqliteUndoStoreError> {
        loop {
            match self.receiver.recv() {
                Ok(resp) => match resp {
                    PersistResp::OpenOk { serialized_model: _, seq_no: _, min_max_seq_no: _ } =>
                        return Err(report!(SqliteUndoStoreError::CmdSequenceError)),
                    PersistResp::OpenErr(err) => {
                        println!("Open error {:?}", err);
                        return Err(report!(SqliteUndoStoreError::CmdSequenceError));
                    }
                    PersistResp::CloseOk => return Err(report!(SqliteUndoStoreError::CmdSequenceError)),
                    PersistResp::CloseErr(err) => {
                        println!("Close error {:?}", err);
                        return Err(report!(SqliteUndoStoreError::CmdSequenceError));
                    }
                    PersistResp::AddCmdOk { seq_no } => {
                        self.last_processed_seq_no = Some(seq_no);
                    }
                    PersistResp::AddCmdErr(err) => {
                        return Err(err);
                    }
                    PersistResp::UndoOk { seq_no, serialized_command } => return Ok((seq_no, serialized_command)),
                    PersistResp::UndoErr(err) => return Err(err),
                    PersistResp::RedoOk { seq_no: _, serialized_command: _ } =>
                        return Err(report!(SqliteUndoStoreError::CmdSequenceError)),
                    PersistResp::RedoErr(err) => {
                        println!("Redo error {:?}", err);
                        return Err(report!(SqliteUndoStoreError::CmdSequenceError));
                    }
                }
                Err(err) => {
                    println!("Fail to communicate persister server: {:?}", err);
                }
            }
        }
    }

    fn wait_redo_resp(&mut self) -> Result<(i64, Vec<u8>), SqliteUndoStoreError> {
        loop {
            match self.receiver.recv() {
                Ok(resp) => match resp {
                    PersistResp::OpenOk { serialized_model: _, seq_no: _, min_max_seq_no: _ } =>
                        return Err(report!(SqliteUndoStoreError::CmdSequenceError)),
                    PersistResp::OpenErr(err) => {
                        println!("Open error {:?}", err);
                        return Err(report!(SqliteUndoStoreError::CmdSequenceError));
                    }
                    PersistResp::CloseOk => return Err(report!(SqliteUndoStoreError::CmdSequenceError)),
                    PersistResp::CloseErr(err) => {
                        println!("Close error {:?}", err);
                        return Err(report!(SqliteUndoStoreError::CmdSequenceError));
                    }
                    PersistResp::AddCmdOk { seq_no } => {
                        self.last_processed_seq_no = Some(seq_no);
                    }
                    PersistResp::AddCmdErr(err) => {
                        return Err(err);
                    }
                    PersistResp::UndoOk { seq_no: _, serialized_command: _ } => 
                        return Err(report!(SqliteUndoStoreError::CmdSequenceError)),
                    PersistResp::UndoErr(err) => {
                        println!("Undo error {:?}", err);
                        return Err(report!(SqliteUndoStoreError::CmdSequenceError));
                    }
                    PersistResp::RedoOk { seq_no, serialized_command } => return Ok((seq_no, serialized_command)),
                    PersistResp::RedoErr(err) => return Err(err),
                }
                Err(err) => {
                    println!("Fail to communicate persister server: {:?}", err);
                }
            }
        }
    }

    fn can_undo(&self) -> bool {
        if let Some(min_seq_no) = self.min_seq_no {
            min_seq_no <= self.last_seq_no
        } else { false }
    }

    fn can_redo(&self) -> bool {
        if let Some(max_seq_no) = self.max_seq_no {
            self.last_seq_no < max_seq_no
        } else { false }
    }

    fn saved(&self) -> bool {
        if let Some(proceesed_seq_no) = self.last_processed_seq_no {
            proceesed_seq_no == self.last_seq_no
        } else { false }
    }
}

#[cfg(feature = "persistence")]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
impl Drop for PersisterClient {
    fn drop(&mut self) {
        self.sender.send(PersistCmd::Close).unwrap();
        loop {
            match self.receiver.recv().unwrap() {
                PersistResp::OpenOk { serialized_model: _, seq_no: _, min_max_seq_no: _ } => {
                    println!("Unexpected open.");
                }
                PersistResp::OpenErr(e) => {
                    println!("Unexpeced open error: {:?}", e);
                },
                PersistResp::CloseOk => {
                    break;
                }
                PersistResp::CloseErr(err) => {
                    println!("Unexpeced close error: {:?}", err);
                    break;
                }
                PersistResp::AddCmdOk { seq_no: _ } => {
                }
                PersistResp::AddCmdErr(err) => {
                    println!("Add command error: {:?}", err);
                }
                PersistResp::UndoOk { seq_no: _, serialized_command: _ } => {}
                PersistResp::UndoErr(err) => {
                    println!("Undo error: {:?}", err);
                }
                PersistResp::RedoOk { seq_no: _, serialized_command: _ } => {}
                PersistResp::RedoErr(err) => {
                    println!("Redo error: {:?}", err);
                }
            }
        }
    }
}

macro_rules! send {
    ($sender:expr, $msg:expr) => {
        tracing::trace!("Persister server response: {:?}", $msg);
        $sender.send($msg).unwrap();    
    };
}


#[cfg(feature = "persistence")]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
impl<C, M, E> PersisterServer<C, M, E>
    where C: crate::cmd::SerializableCmd<Model = M>, M: Default + serde::Serialize + serde::de::DeserializeOwned
{
    fn new(
        receiver: Receiver<PersistCmd>,
        sender: Sender<PersistResp>,
        undo_limit: usize,
        merge_timeout: Option<Duration>,
    ) -> Self {
        Self {
            undo_limit,
            phantom: std::marker::PhantomData, phantome: std::marker::PhantomData,
            receiver, sender, state: PersisterServerState::Idle
        }
    }

    fn start(mut self) {
        loop {
            let msg = self.receiver.recv();
            tracing::trace!("PersisterServer received msg: {:?}", msg);
            match msg {
                Ok(cmd) => {
                    match cmd {
                        PersistCmd::Open { dir } => {
                            if let PersisterServerState::Loaded { base_dir, sqlite_path: _, cur_cmd_seq_no: _, model: _, conn: _ } = &self.state {
                                tracing::error!("Already opend.");
                                send!(self.sender, PersistResp::OpenErr(report!(SqliteUndoStoreError::AlreadyOpened)));
                            }

                            let mut sqlite_path = dir.clone();
                            sqlite_path.push(SQLITE_FILE_NAME);
                            match Self::open_sqlite(dir.clone(), sqlite_path.clone()) {
                                Ok(conn) => {
                                    tracing::trace!("Succeed to open sqlite file: {:?}", sqlite_path);
                                    let db = Db::new(sqlite_path.clone(), &conn);
                                    match Self::restore_model(&sqlite_path, &conn) {
                                        Ok((cur_cmd_seq_no, model)) => {
                                            tracing::trace!("Succeed to restore model(seq: {})", cur_cmd_seq_no);

                                            match bincode::serialize(&model) {
                                                Ok(sm) => {
                                                    match Self::min_max_seq_no(&db) {
                                                        Ok(min_max_seq_no) => {
                                                            tracing::trace!("Min/Max: {:?}", min_max_seq_no);
                                                            let msg = PersistResp::OpenOk { serialized_model: sm, seq_no: cur_cmd_seq_no, min_max_seq_no };
                                                            send!(self.sender, msg);
                                                            self.state = PersisterServerState::Loaded {
                                                                base_dir: dir, sqlite_path, cur_cmd_seq_no, model, conn
                                                            };
                                                        }
                                                        Err(err) => {
                                                            tracing::error!("Cannot retrieve min/max seq {:?}", err);
                                                            let msg = PersistResp::OpenErr(report!(err));
                                                            send!(self.sender, msg);
                                                        }
                                                    };
                                                },
                                                Err(err) => {
                                                    tracing::error!("Cannot serialize model: {:?}", err);
                                                    let msg = PersistResp::OpenErr(
                                                        report!(
                                                            SqliteUndoStoreError::CannotDeserialize {
                                                                path: Some(sqlite_path.clone()), seq_no: cur_cmd_seq_no, ser_err: err
                                                            }
                                                        )
                                                    );
                                                    send!(self.sender, msg);
                                                }
                                            };
                                        }
                                        Err(err) => {
                                            tracing::error!("Cannot restore model: {:?}", err);
                                            let msg = PersistResp::OpenErr(err);
                                            send!(self.sender, msg);
                                        }
                                    }
                                }
                                Err(err) => {
                                    tracing::error!("Cannot open sqlite(path={:?}): {:?}", sqlite_path, err);
                                    let msg = PersistResp::OpenErr(err);
                                    send!(self.sender, msg);
                                }
                            }
                        }
                        PersistCmd::Close => {
                            match &self.state {
                                PersisterServerState::Idle => {
                                    tracing::trace!("Already closed.");
                                    // Already closed.
                                    send!(self.sender, PersistResp::CloseOk);
                                    break;
                                }
                                PersisterServerState::Loaded { base_dir, sqlite_path: _, cur_cmd_seq_no: _, model: _, conn: _ } => {
                                    Self::unlock(base_dir).unwrap();
                                    send!(self.sender, PersistResp::CloseOk);
                                    break;
                                }
                            }                        
                        }
                        PersistCmd::AddCmd { seq_no, ser_cmd } => {
                            match self.add_cmd(seq_no, ser_cmd) {
                                Ok(_) => {
                                    tracing::trace!("Cmd add ok");
                                    let seq_no = seq_no + 1;
                                    if let PersisterServerState::Loaded { base_dir: _, conn: _, cur_cmd_seq_no, sqlite_path: _, model: _ } = &mut self.state {
                                        *cur_cmd_seq_no = seq_no;
                                    }
                                    send!(self.sender, PersistResp::AddCmdOk { seq_no });
                                },
                                Err(err) => {
                                    tracing::error!("Add cmd error {:?}", err);
                                    let msg = PersistResp::AddCmdErr(err);
                                    send!(self.sender, msg);
                                }
                            }
                        }
                        PersistCmd::Undo => {
                            match self.undo() {
                                Ok((seq_no, serialized_command)) => {
                                    tracing::trace!("Undo ok seq:{}", seq_no);
                                    let msg = PersistResp::UndoOk { seq_no, serialized_command };
                                    send!(self.sender, msg);
                                }
                                Err(err) => {
                                    tracing::error!("Undo err {:?}", err);
                                    let msg = PersistResp::UndoErr(err);
                                    send!(self.sender, msg);
                                }
                            }
                        }
                        PersistCmd::Redo => {
                            match self.redo() {
                                Ok((seq_no, serialized_command)) => {
                                    tracing::trace!("Redo ok seq:{}", seq_no);
                                    let msg = PersistResp::RedoOk { seq_no, serialized_command };
                                    send!(self.sender, msg);
                                }
                                Err(err) => {
                                    tracing::error!("Redo err {:?}", err);
                                    let msg = PersistResp::RedoErr(err);
                                    send!(self.sender, msg);
                                }
                            }
                        }
                    }
                }
                Err(err) => {
                    tracing::error!("Persister server cannot contact the client {:?}", err);
                    break;
                }
            }
        }
    }

    fn min_max_seq_no(db: &Db) -> Result<Option<(i64, i64)>, SqliteUndoStoreError> {
        db.exec(|conn| {
            let mut stmt = conn.prepare(
                "select count(command_id), min(command_id), max(command_id) from command"
            )?;
            let mut rows = stmt.query([])?;
            let row = rows.next()?.unwrap();
            let count: i64 = row.get(0)?;
            Ok(
                if count == 0 {
                    None
                } else {
                    let min: i64 = row.get(1)?;
                    let max: i64 = row.get(2)?;
                    Some((min, max))
                }
            )
        })
    }

    fn undo(&mut self) -> Result<(i64, Vec<u8>), SqliteUndoStoreError> {
        match &mut self.state {
            PersisterServerState::Idle => {
                send!(self.sender, PersistResp::UndoErr(report!(SqliteUndoStoreError::NotOpend)));
                Err(report!(SqliteUndoStoreError::NotOpend))
            }
            PersisterServerState::Loaded { base_dir: _, sqlite_path, cur_cmd_seq_no, model, conn } => {
                let db = Db::new(sqlite_path.clone(), conn);
                let ser_cmd: Option<Vec<u8>> = db.exec(|conn| {
                    let mut stmt = conn.prepare(
                        "select serialized from command where command_id = ?1"
                    )?;
                    let mut rows = stmt.query(rusqlite::params![*cur_cmd_seq_no])?;
                    let ser_cmd = match rows.next()? {
                        Some(row) => {
                            let ser: Vec<u8> = row.get(0)?;
                            Some(ser)
                        },
                        None => None,
                    };
                    Ok(ser_cmd)
                })?;
                if let Some(ser_cmd) = ser_cmd {
                    let cmd: C = bincode::deserialize(&ser_cmd).map_err(|ser_err|
                        SqliteUndoStoreError::CannotDeserialize {
                            path: Some(sqlite_path.clone()), seq_no: *cur_cmd_seq_no, ser_err
                        }
                    )?;
                    cmd.undo(model);
                    *cur_cmd_seq_no -= 1;
                    Self::save_seq_no(sqlite_path, conn, *cur_cmd_seq_no)?;
                    Ok((*cur_cmd_seq_no + 1, ser_cmd))
                } else {
                    bail!(SqliteUndoStoreError::CannotUndoRedo);
                }
            }
        }
    }

    fn redo(&mut self) -> Result<(i64, Vec<u8>), SqliteUndoStoreError> {
        match &mut self.state {
            PersisterServerState::Idle => {
                send!(self.sender, PersistResp::RedoErr(report!(SqliteUndoStoreError::NotOpend)));
                Err(report!(SqliteUndoStoreError::NotOpend))
            }
            PersisterServerState::Loaded { base_dir: _, sqlite_path, cur_cmd_seq_no, model, conn } => {
                let db = Db::new(sqlite_path.clone(), conn);
                let ser_cmd: Option<Vec<u8>> = db.exec(|conn| {
                    let mut  stmt = conn.prepare(
                        "select serialized from command where command_id = ?1"
                    )?;
                    let mut rows = stmt.query(rusqlite::params![*cur_cmd_seq_no + 1])?;
                    let ser_cmd = match rows.next()? {
                        Some(row) => {
                            let ser: Vec<u8> = row.get(0)?;
                            Some(ser)
                        },
                        None => None,
                    };
                    Ok(ser_cmd)
                })?;
                if let Some(ser_cmd) = ser_cmd {
                    let cmd: C = bincode::deserialize(&ser_cmd).map_err(|ser_err|
                        SqliteUndoStoreError::CannotDeserialize {
                            path: Some(sqlite_path.clone()), seq_no: *cur_cmd_seq_no, ser_err
                        }
                    )?;
                    cmd.redo(model);
                    *cur_cmd_seq_no += 1;
                    Self::save_seq_no(sqlite_path, conn, *cur_cmd_seq_no)?;
                    Ok((*cur_cmd_seq_no - 1, ser_cmd))
                } else {
                    bail!(SqliteUndoStoreError::CannotUndoRedo);
                }
            }
        }
    }

    fn add_cmd(&mut self, seq_no: i64, ser_cmd: Vec<u8>) -> Result<(), SqliteUndoStoreError>{
        match &mut self.state {
            PersisterServerState::Idle => {
                send!(self.sender, PersistResp::AddCmdErr(report!(SqliteUndoStoreError::NotOpend)));
                Err(report!(SqliteUndoStoreError::NotOpend))
            }
            PersisterServerState::Loaded { base_dir: _, sqlite_path, cur_cmd_seq_no: _, model, conn } => {
                let cmd: C = bincode::deserialize(&ser_cmd).map_err(|ser_err|
                    SqliteUndoStoreError::CannotDeserialize { path: None, seq_no, ser_err }
                )?;
                cmd.redo(model);

                let seq_no = seq_no + 1;
                let db = Db::new(sqlite_path.clone(), conn);
                // This is the case you add commands after undo() some.
                let delete_count = db.exec(|conn| conn.execute(
                    "delete from command where ?1 <= command_id", rusqlite::params![seq_no]
                ))?;
                tracing::trace!("add_cmd() removed cmd (seqno <= {}): count: {}", seq_no, delete_count);

                db.exec(|conn| conn.execute(
                    "insert into command (command_id, serialized) values (?1, ?2)", rusqlite::params![seq_no, ser_cmd]
                ))?;
                tracing::trace!("add_cmd() inserted cmd seq no:{}", seq_no);

                if seq_no == MAX_COMMAND_ID {
                    tracing::error!("add_cmd() seq no reaced MAX_COMMAND_ID:{}", seq_no);
                    send!(self.sender, PersistResp::AddCmdErr(report!(SqliteUndoStoreError::NeedCompaction(sqlite_path.clone()))));
                }
                Self::save_seq_no(sqlite_path, conn, seq_no)?;
                let removed_count = db.exec(|conn| Self::trim_undo_records(conn, self.undo_limit))?;
                tracing::trace!("add_cmd() trimmed commands. Removed count: {}", removed_count);
                if removed_count != 0 {
                    let serialized = bincode::serialize(&model).map_err(SqliteUndoStoreError::from)?;

                    match Self::get_last_snapshot_id(conn, &sqlite_path)? {
                        None => {
                            Self::save_snapshot(&db, &serialized, seq_no)?
                        }
                        Some(last_snapshot_id) => {
                            if last_snapshot_id < seq_no - (self.undo_limit as i64) {
                                Self::save_snapshot(&db, &serialized, seq_no)?
                            }
                        }
                    }
                }

                if delete_count != 0 {
                    db.exec(|conn| conn.execute("delete from snapshot", rusqlite::params![]))?;
                    tracing::trace!("add_cmd() removed all snapshots.");
                    let serialized = bincode::serialize(&model).map_err(SqliteUndoStoreError::from)?;
                    Self::save_snapshot(&db, &serialized, seq_no)?;
                } else {
                    db.exec(|conn| Self::trim_snapshots(conn))?;
                }

                Ok(())
            }
        }
    }

    fn lock_file_path(base_dir: &std::path::Path) -> std::path::PathBuf {
        let mut path: std::path::PathBuf = base_dir.to_path_buf();
        path.push("lock");
        path
    }

    fn try_lock(base_dir: &std::path::Path) -> Result<std::fs::File, SqliteUndoStoreError> {
        let lock_file_path = Self::lock_file_path(base_dir);
        std::fs::OpenOptions::new().write(true).create_new(true).open(&lock_file_path).map_err(|error|
            report!(SqliteUndoStoreError::CannotLock { path: lock_file_path, error })
        )
    }

    fn unlock(dir: &PathBuf) -> std::io::Result<()> {
        let p = Self::lock_file_path(dir);
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
            create table command(command_id integer primary key not null, serialized blob not null);
            create table snapshot(snapshot_id integer primary key not null, serialized blob not null);
            create table cmd_seq_no(cur_cmd_seq_no integer);
            create table version(version integer not null);
            insert into version (version) values (1);
            commit;"
        )?;
        Ok(())
    }

    fn open_sqlite<P: AsRef<Path>>(dir: P, sqlite_path: PathBuf) -> Result<Connection, SqliteUndoStoreError> {
        let conn = if sqlite_path.exists() {
            if ! dir.as_ref().is_dir() {
                return Err(SqliteUndoStoreError::NotADirectory(dir.as_ref().to_owned())).map_err(Report::from)
            }
            Self::try_lock(dir.as_ref())?;
            Self::open_existing(&sqlite_path)
        } else {
            std::fs::create_dir_all(dir.as_ref()).map_err(|e| SqliteUndoStoreError::FileError(dir.as_ref().to_path_buf(), e))?;
            Self::try_lock(dir.as_ref())?;
            Self::create_new(&sqlite_path)
        }.map_err(|e|
            SqliteUndoStoreError::DbError(sqlite_path, report!(e))
        )?;
        Ok(conn)        
    }

    fn get_cur_seq_no(conn: &Connection) -> Result<i64, rusqlite::Error> {
        let mut stmt = conn.prepare(
            "select count(cur_cmd_seq_no), max(cur_cmd_seq_no) from cmd_seq_no"
        )?;
        let mut rows = stmt.query([]).map_err(Report::from)?;
                
        let row = rows.next().map_err(Report::from)?.unwrap();
        let count: i64 = row.get(0).map_err(Report::from)?;
        if 1 < count {
            panic!("Command sequence number inconsistent. The record count({}) should be 0 or 1.", count);
        } else {
            let cur_seq: Option<i64> = row.get(1).map_err(Report::from)?;
            match cur_seq {
                None => {
                    conn.execute("insert into cmd_seq_no (cur_cmd_seq_no) values (0)", rusqlite::params![]).map_err(Report::from)?;
                    Ok(0)
                },
                Some(seq) => Ok(seq)
            }
        }
    }

    fn restore_model(sqlite_path: &PathBuf, conn: &Connection) -> Result<(i64, M), SqliteUndoStoreError> {
        let cur_seq_no = Self::get_cur_seq_no(conn).map_err(|e| SqliteUndoStoreError::DbError(sqlite_path.clone(), e))?;
        if cur_seq_no == 0 {
            return Ok((0, M::default()))
        }
        
        match Self::load_last_snapshot(&sqlite_path, conn)? {
            Some((last_snapshot_id, mut model)) => {
                tracing::trace!("loading snapshot. Snapshot id: {}, cmd seq no: {}.", last_snapshot_id, cur_seq_no);

                // Restore with snapshot.
                if cur_seq_no < last_snapshot_id {
                    let mut stmt = Self::db(
                        sqlite_path,
                        || conn.prepare(
                        "select command_id, serialized from command where ?1 < command_id and command_id <= ?2 order by command_id desc"
                        )
                    )?;
                    let mut rows = Self::db(
                        sqlite_path,
                        || stmt.query([cur_seq_no, last_snapshot_id])
                    )?;
                            
                    let mut cmd_id = last_snapshot_id;

                    while let Some(row) = Self::db(sqlite_path, || rows.next())? {
                        let id: i64 = Self::db(sqlite_path, || row.get(0))?;
                        tracing::trace!("loading snapshot. cmd id: {}.", id);
                        if id != cmd_id {
                            return Err(report!(SqliteUndoStoreError::CannotRestoreModel {
                                snapshot_id: Some(last_snapshot_id), not_foud_cmd_id: cmd_id 
                            }))
                        }
                        cmd_id -= 1;
                                
                        let serialized: Vec<u8> = Self::db(sqlite_path, || row.get(1))?;
                        let cmd: C = bincode::deserialize(&serialized).map_err(|ser_err|
                            SqliteUndoStoreError::CannotDeserialize { path: None, seq_no: id, ser_err }
                        )?;
                        cmd.undo(&mut model);
                    }
                } else if last_snapshot_id < cur_seq_no {
                    let mut stmt = Self::db(
                        sqlite_path,
                        || conn.prepare(
                            "select command_id, serialized from command where ?1 < command_id and command_id <= ?2 order by command_id asc"
                        )
                    )?;
                    let mut rows = Self::db(sqlite_path, || stmt.query([last_snapshot_id, cur_seq_no]))?;
                            
                    let mut cmd_id = last_snapshot_id + 1;

                    while let Some(row) = Self::db(sqlite_path, || rows.next())? {
                        let id: i64 = Self::db(
                            sqlite_path, || row.get(0)
                        )?;
                        tracing::trace!("loading snapshot. cmd id: {}.", id);
                        if id != cmd_id {
                            return Err(report!(SqliteUndoStoreError::CannotRestoreModel {
                                snapshot_id: Some(last_snapshot_id), not_foud_cmd_id: cmd_id
                            }))
                        }
                        cmd_id += 1;
                        
                        let serialized: Vec<u8> = Self::db(sqlite_path, || row.get(1))?;
                        let cmd: C = bincode::deserialize(&serialized).map_err(|ser_err|
                            SqliteUndoStoreError::CannotDeserialize { path: None, seq_no: id, ser_err }
                        )?;
                        cmd.redo(&mut model);
                    }
                }
        
                Ok((cur_seq_no, model))
            },
            None => {
                // Restore without snapshot.
                Ok((cur_seq_no, Self::load_without_snapshot(sqlite_path, conn, cur_seq_no)?))
            },
        }
    }

    fn load_last_snapshot(sqlite_path: &PathBuf, conn: &Connection) -> Result<Option<(i64, M)>, SqliteUndoStoreError> {
        let mut stmt = Self::db(
            sqlite_path,
            || conn.prepare(
            "select snapshot_id, serialized from snapshot
                    where
                      snapshot_id >= (select min(command_id) from command) -1
                      and snapshot_id <= (select max(command_id) from command)
                    order by snapshot_id desc limit 1"
            )
        )?;
        let mut rows = Self::db(sqlite_path, || stmt.query([]))?;
        
        if let Some(row) = Self::db(sqlite_path, || rows.next())? {
            let id: i64 = Self::db(sqlite_path, || row.get(0))?;
            let serialized: Vec<u8> = Self::db(sqlite_path, || row.get(1))?;
            let snapshot: M = bincode::deserialize(&serialized).map_err(|ser_err|
                SqliteUndoStoreError::CannotDeserialize { path: None, seq_no: id, ser_err }
            )?;
            Ok(Some((id, snapshot)))
        } else {
            Ok(None)
        }
    }

    fn load_without_snapshot(sqlite_path: &PathBuf, conn: &Connection, cur_seq_no: i64) -> Result<M, SqliteUndoStoreError> {
        let mut stmt = Self::db(
            sqlite_path,
            || conn.prepare(
                "select command_id, serialized from command where command_id <= ?1"
            )
        )?;
        let mut rows = Self::db(sqlite_path, || stmt.query([cur_seq_no]))?;

        let mut cmd_id = 1;
        let mut model = M::default();
        while let Some(row) = Self::db(sqlite_path, || rows.next())? {
            let id: i64 = Self::db(sqlite_path, || row.get(0))?;
            if id != cmd_id {
                return Err(report!(SqliteUndoStoreError::CannotRestoreModel { snapshot_id: None, not_foud_cmd_id: cmd_id }))
            }
            cmd_id += 1;
            
            let serialized: Vec<u8> = Self::db(sqlite_path, || row.get(1))?;
            let cmd: C = bincode::deserialize(&serialized).map_err(|ser_err|
                SqliteUndoStoreError::CannotDeserialize { path: None, seq_no: id, ser_err }
            )?;
            cmd.redo(&mut model);
        }
        Ok(model)
    }

    fn save_seq_no(sqlite_path: &PathBuf, conn: &Connection, seq_no: i64) -> Result<(), SqliteUndoStoreError> {
        Self::db(
            &sqlite_path,
            || conn.execute("update cmd_seq_no set cur_cmd_seq_no = ?1", rusqlite::params![seq_no])
        )?;
        tracing::trace!("Saved seq no: {}", seq_no);
        Ok(())
    }

    fn trim_undo_records(conn: &Connection, undo_limit: usize) -> std::result::Result<usize, rusqlite::Error> {
        let mut stmt = conn.prepare(
            "delete from command as c0 where c0.command_id not in (
                select command_id from command as c1 order by command_id desc limit ?1
            )"
        )?;
        
        Ok(stmt.execute(rusqlite::params![undo_limit])?)
    }

    fn get_last_snapshot_id(conn: &Connection, sqlite_path: &PathBuf) -> Result<Option<i64>, SqliteUndoStoreError> {
        let mut stmt = Self::db(
            &sqlite_path,
            || conn.prepare(
                "select max(snapshot_id) from snapshot"
            )
        )?;
        let mut rows = Self::db(&sqlite_path, || stmt.query([]))?;
        let row = rows.next().unwrap();
        Ok(row.unwrap().get(0).unwrap())
    }

    fn trim_snapshots(conn: &Connection) -> std::result::Result<usize, rusqlite::Error> {
        let mut stmt = conn.prepare(
            "delete from snapshot where snapshot_id < (select max(snapshot_id) from snapshot)"
        )?;
        tracing::trace!("Snapshot trimmed.");
        
        Ok(stmt.execute(rusqlite::params![])?)
    }

    fn save_snapshot(db: &Db, ser_model: &Vec<u8>, seq_no: i64) -> Result<(), SqliteUndoStoreError> {
        db.exec(|conn| {
            let mut stmt = conn.prepare(
                "insert into snapshot (snapshot_id, serialized) values (?1, ?2)"
            )?;
            stmt.execute(rusqlite::params![seq_no, ser_model])
        })?;
        tracing::trace!("Snapshot saved: snapshot id: {}", seq_no);
        
        Ok(())
    }

    #[inline]
    fn db<F, T>(sqlite_path: &PathBuf, f: F) -> Result<T, SqliteUndoStoreError> where F: FnOnce() -> std::result::Result<T, rusqlite::Error> {
        f().map_err(|e| {
            report!(SqliteUndoStoreError::DbError(sqlite_path.clone(), report!(e)))
        })
    }
}

#[cfg(feature = "persistence")]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
struct Db<'a> {
    sqlite_path: PathBuf,
    conn: &'a Connection,
}

#[cfg(feature = "persistence")]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
impl<'a> Db<'a> {
    fn new(sqlite_path: PathBuf, conn: &'a Connection) -> Self {
        Self { sqlite_path, conn }
    }

    #[inline]
    fn exec<F, T>(&self, f: F) -> Result<T, SqliteUndoStoreError> where F: FnOnce(&Connection) -> std::result::Result<T, rusqlite::Error> {
        let sqlite_path = self.sqlite_path.clone();
        f(self.conn).map_err(|e| {
            report!(SqliteUndoStoreError::DbError(sqlite_path, report!(e)))
        })
    }
}

#[cfg(feature = "persistence")]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct SqliteUndoStore<C, M, E>
  where C: crate::cmd::SerializableCmd<Model = M>, M: Default + serde::Serialize + serde::de::DeserializeOwned
{
    phantom: std::marker::PhantomData<C>,
    phantome: std::marker::PhantomData<E>,
    model: M,
    options: Options<M>,
    persister_client: PersisterClient,
    base_dir: std::path::PathBuf,
}

pub const SQLITE_FILE_NAME: &'static str = "db.sqlite";
pub const DEFAULT_UNDO_LIMIT: usize = 100;

pub struct Options<M> {
    pub undo_limit: usize,
    pub merge_timeout: Option<Duration>,

    /// Called when a snapshot is restored. If you have states that are out of scope to manage undo/redo operations, you can restore them here.
    pub on_snapshot_restored: Option<Box<dyn FnOnce(M) -> M>>,
}

impl<M> Options<M> {
    pub fn new() -> Self {
        Self {
            undo_limit: DEFAULT_UNDO_LIMIT,
            merge_timeout: None,
            on_snapshot_restored: None,
        }
    }

    pub fn with_undo_limit(self, l: usize) -> Self {
        Self {
            undo_limit: l,
            ..self
        }
    }

    pub fn with_merge_timeout(self, timeout: Duration) -> Self {
        Self {
            merge_timeout: Some(timeout),
            ..self
        }
    }

    pub fn with_on_snapshot_restored(self, on_snapshot_restored: Box<dyn FnOnce(M) -> M>) -> Self {
        Self {
            on_snapshot_restored: Some(on_snapshot_restored),
            ..self
        }
    }
}

#[cfg(feature = "persistence")]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
impl<C, M, E> SqliteUndoStore<C, M, E> where C: crate::cmd::SerializableCmd<Model = M>, M: Default + serde::Serialize + serde::de::DeserializeOwned {
    pub fn dir(&self) -> &std::path::PathBuf {
        &self.base_dir
    }

    // Open the specified directory or newly create it if that does not exist.
    pub fn open<P: AsRef<Path>>(dir: P, mut options: Options<M>) -> Result<Self, SqliteUndoStoreError>
        where C: crate::cmd::SerializableCmd<Model = M> + serde::de::DeserializeOwned
    {
        let (cmd_sender, cmd_receiver) = mpsc::channel();
        let (resp_sender, resp_receiver) = mpsc::channel();
        
        let undo_limit = options.undo_limit;
        let merge_timeout = options.merge_timeout;
        thread::spawn(move || {
            let persister_server: PersisterServer<C, M, E> = PersisterServer::new(
                cmd_receiver, resp_sender, undo_limit, merge_timeout,
            );
            persister_server.start();
        });

        let (persister_client, serialized_model) = PersisterClient::open(
            resp_receiver, cmd_sender, dir.as_ref().to_path_buf(), options.undo_limit
        )?;
        let model: M = bincode::deserialize(&serialized_model).map_err(|e|
            SqliteUndoStoreError::CannotDeserialize {
                path: Some(dir.as_ref().to_path_buf()), seq_no: persister_client.last_seq_no, ser_err: e
            }
        )?;

        let model = if let Some(f) = options.on_snapshot_restored.take() {
            f(model)
        } else { model };

        let store = SqliteUndoStore {
            base_dir: dir.as_ref().to_path_buf(), model,
            phantom: std::marker::PhantomData, phantome: std::marker::PhantomData,
            options, persister_client,
        };

        Ok(store)
    }

    pub fn save_as<P: AsRef<Path>>(&mut self, save_to: P) -> Result<(), SqliteUndoStoreError> {
        let mut to = save_to.as_ref().to_path_buf();
        to.push(SQLITE_FILE_NAME);

        let mut from = self.base_dir.clone();
        from.push(SQLITE_FILE_NAME);

        std::fs::copy(&from, &to).map_err(|error| Report::from(SqliteUndoStoreError::CannotCopyStore { from: from.clone(), to: to.clone(), error }))?;
        Ok(())
    }

    fn _add_cmd(&mut self, cmd: C) -> Result<(), SqliteUndoStoreError> {
        let serialized: Vec<u8> = bincode::serialize(&cmd).map_err(
            |e| SqliteUndoStoreError::SerializeError(e)
        )?;

        self.persister_client.add_command(serialized);
        Ok(())
    }

    pub fn saved(&mut self) -> Result<bool, SqliteUndoStoreError> {
        self.persister_client.process_resp()?;
        Ok(self.persister_client.saved())
    }

    pub fn wait_until_saved(&mut self) {
        loop {
            if self.saved().unwrap() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn _undo(&mut self) -> Result<(), SqliteUndoStoreError> {
        let (seq_no, ser_cmd) = self.persister_client.undo()?;
        let cmd: C = bincode::deserialize(&ser_cmd).map_err(|ser_err|
            SqliteUndoStoreError::CannotDeserialize {
                path: Some(self.base_dir.clone()), seq_no, ser_err
            }
        )?;
        cmd.undo(&mut self.model);
        Ok(())

        // let mut stmt = self.db_undo(|| self.conn.prepare(
        //     "select serialized from command where command_id = ?1"
        // ).map_err(Report::from))?;
        // let mut rows = self.db_undo(|| stmt.query(rusqlite::params![self.cur_cmd_seq_no]).map_err(Report::from))?;
        // if let Some(row) = self.db_undo(|| rows.next().map_err(Report::from))? {
        //     let serialized: Vec<u8> = self.db_undo(|| row.get(0).map_err(Report::from))?;
        //     let cmd: C = bincode::deserialize(&serialized).map_err(|ser_err|
        //         SqliteUndoStoreError::CannotDeserialize {
        //             path: Some(self.sqlite_path.clone()), id: self.cur_cmd_seq_no, ser_err
        //         }
        //     )?;
        //     let res = cmd.undo(&mut self.model);
        //     self.cur_cmd_seq_no -= 1;
        //     self.save_seq_no()?;
        //     Ok(res)
        // } else {
        //     bail!(SqliteUndoStoreError::CannotUndoRedo)
        // }
    }

    fn _redo(&mut self) -> Result<(), SqliteUndoStoreError> {
        let (seq_no, ser_cmd) = self.persister_client.redo()?;
        let cmd: C = bincode::deserialize(&ser_cmd).map_err(|ser_err|
            SqliteUndoStoreError::CannotDeserialize {
                path: Some(self.base_dir.clone()), seq_no, ser_err
            }
        )?;
        cmd.redo(&mut self.model);

        // let mut  stmt = self.db_undo(|| self.conn.prepare(
        //     "select serialized from command where command_id = ?1"
        // ).map_err(Report::from))?;
        // let mut rows = self.db_undo(|| stmt.query(rusqlite::params![self.cur_cmd_seq_no + 1]).map_err(Report::from))?;
        // if let Some(row) = self.db_undo(|| rows.next().map_err(Report::from))? {
        //     let serialized: Vec<u8> = self.db_undo(|| row.get(0).map_err(Report::from))?;
        //     let cmd: C = bincode::deserialize(&serialized).map_err(|ser_err|
        //         SqliteUndoStoreError::CannotDeserialize {
        //             path: Some(self.sqlite_path.clone()), id: self.cur_cmd_seq_no, ser_err
        //         }
        //     )?;

        //     cmd.redo(&mut self.model);
        //     self.cur_cmd_seq_no += 1;
        //     self.save_seq_no()?;
        //     Ok(())
        // } else {
        //     bail!(SqliteUndoStoreError::CannotUndoRedo)
        // }
Ok(())
    }
}

pub const MAX_COMMAND_ID: i64 = 9_223_372_036_854_775_807;

// #[cfg(feature = "persistence")]
// impl<C, M, E> SqliteUndoStore<C, M, E>
//   where M: Default + serde::Serialize + serde::de::DeserializeOwned + 'static, C: crate::cmd::SerializableCmd<Model = M>
// {
// }

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
        match f(&mut self.model) {
            Ok(cmd) => {
                if let Err(e) = self._add_cmd(cmd) {
                    panic!("Cannot contact persister server {:?}.", e);
                }
                Ok(())
            }
            Err(err) => {
                Err(err)
            }
        }
    }

    fn add_cmd(&mut self, cmd: Self::CmdType) {
        cmd.redo(&mut self.model);

        match self._add_cmd(cmd) {
            Err(e) => {
                panic!("Cannot contact persister server {:?}.", e);
            }
            Ok(_) => ()
        }
    }

    fn undo(&mut self) {
        if self.can_undo() {
            if let Err(e) = self._undo() {
                panic!("Cannot contact persister server {:?}.", e);
            }
        }
    }

    fn can_undo(&self) -> bool {
        self.persister_client.can_undo()

        // let mut stmt = self.db_can_undo(|| self.conn.prepare(
        //     "select 1 from command where command_id <= ?1"
        // ).map_err(Report::from))?;
        // let mut rows = self.db_can_undo(|| stmt.query(rusqlite::params![self.cur_cmd_seq_no]).map_err(Report::from))?;
        // let row = self.db_can_undo(|| rows.next().map_err(Report::from))?;
        // Ok(row.is_some())
    }

    fn can_redo(&self) -> bool {
        self.persister_client.can_redo()

        // let mut stmt = self.db_can_undo(|| self.conn.prepare(
        //     "select 1 from command where ?1 < command_id"
        // ).map_err(Report::from))?;
        // let mut rows = self.db_can_undo(|| stmt.query(rusqlite::params![self.cur_cmd_seq_no]).map_err(Report::from))?;
        // let row = self.db_can_undo(|| rows.next().map_err(Report::from))?;
        // Ok(row.is_some())
    }

    // fn can_redo(&self) -> bool {
    //     match self._can_redo() {
    //         Ok(b) => b,
    //         Err(e) => panic!("Cannot access database {:?}.", e),
    //     }
    // }

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
    use std::{thread, time::Duration};

    use error_stack::Result;
    use tracing::level_filters::LevelFilter;
    use crate::undo_store::{self, SQLITE_FILE_NAME};
    use super::{Cmd, SqliteUndoStore, UndoStore};

    #[derive(serde::Serialize, serde::Deserialize)]
    enum Trace {
        Add, Sub
    }

    #[derive(serde::Serialize, serde::Deserialize)]
    struct SerSum {
        pub value: i32,
        pub trace: Vec<Trace>,
        #[serde(skip)]
        pub trace_count: usize,
    }

    impl SerSum {
        fn value(&self) -> i32 {
            self.value
        }

        fn trace_count(&self) -> usize {
            self.trace_count
        }

        fn add(&mut self, i: i32) {
            let new_val = self.value + i;
            self.value = new_val;
            self.trace.push(Trace::Add);
            self.trace_count += 1;
        }

        fn sub(&mut self, i: i32) {
            let new_val = self.value - i;
            self.value = new_val;
            self.trace.push(Trace::Sub);
            self.trace_count += 1;
        }
    }

    impl Default for SerSum {
        fn default() -> Self {
            Self { value: 0, trace: vec![], trace_count: 0 }
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
                SerSumCmd::Add(i) => model.add(*i),
                SerSumCmd::Sub(i) => model.sub(*i),
            }
        }

        fn undo(&self, model: &mut Self::Model) {
            match self {
                SerSumCmd::Add(i) => model.value -= *i,
                SerSumCmd::Sub(i) => model.value += *i,
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
        let store = super::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), undo_store::Options::new()).unwrap();

        let store2_err = super::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), undo_store::Options::new()).err().unwrap();
        match store2_err.downcast_ref::<super::SqliteUndoStoreError>().unwrap() {
            super::SqliteUndoStoreError::CannotLock { path: err_dir, error: _ } => {
                let lock_file_path = crate::undo_store::PersisterServer::<SerSumCmd, SerSum, ()>::lock_file_path(&dir);
                assert_eq!(err_dir.as_path(), lock_file_path);
            },
            _ => {panic!("Test failed. {:?}", store2_err)},
        };

        drop(store);

        let _ = super::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), undo_store::Options::new()).unwrap();
    }

    fn wait_add_cmd_completion(store: &mut SqliteUndoStore::<SerSumCmd, SerSum, ()>) {
        loop {
            if store.saved().unwrap() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    #[test]
    fn can_serialize_cmd() {
        use rusqlite::{Connection, params};
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), undo_store::Options::new()).unwrap();

        store.add(123).unwrap();
        assert_eq!(store.model().value(), 123);
        store.sub(234).unwrap();
        assert_eq!(store.model().value(), 123 - 234);

        let mut path = dir;
        path.push("db.sqlite");
        wait_add_cmd_completion(&mut store);

        let conn = Connection::open(&path).unwrap();
        let mut stmt = conn.prepare(
            "select command_id, serialized from command order by command_id"
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
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), undo_store::Options::new()).unwrap();

        assert_eq!(store.can_undo(), false);

        store.add(123).unwrap();
        assert_eq!(store.model().value(), 123);
        wait_add_cmd_completion(&mut store);
        assert!(store.can_undo());

        store.sub(234).unwrap();
        assert_eq!(store.model().value(), 123 - 234);
        wait_add_cmd_completion(&mut store);
        assert!(store.can_undo());

        store.undo();
        assert_eq!(store.model().value(), 123);

        store.undo();
        assert_eq!(store.model().value(), 0);

        store.redo();
        assert_eq!(store.model().value(), 123);

        store.redo();
        assert_eq!(store.model().value(), 123 - 234);
    }

    #[test]
    fn file_undo_store_can_serialize() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), undo_store::Options::new()).unwrap();
        store.add(123).unwrap();
        wait_add_cmd_completion(&mut store);
        assert_eq!(store.model().value(), 123);

        drop(store);
        
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), undo_store::Options::new()).unwrap();
        assert_eq!(store.model().value(), 123);
        store.undo();
        assert_eq!(store.model().value(), 0);
    }

    #[test]
    fn file_undo_store_undo_and_add() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), undo_store::Options::new()).unwrap();
        store.add(1).unwrap();
        store.add(2).unwrap();
        store.add(3).unwrap();
        wait_add_cmd_completion(&mut store);
        // 1, 2, 3
        assert_eq!(store.model().value(), 6);

        store.undo();
        // 1, 2
        store.undo();
        // 1

        store.add(100).unwrap();
        wait_add_cmd_completion(&mut store);
        // 1, 100
        assert_eq!(store.model().value(), 101);
    }

    #[test]
    fn file_undo_store_can_set_limit() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), undo_store::Options::new().with_undo_limit(5)).unwrap();
        store.add(1).unwrap();
        store.add(2).unwrap();
        store.add(3).unwrap();
        store.add(4).unwrap();
        store.add(5).unwrap();
        store.add(6).unwrap(); // 2, 3, 4, 5, 6
        wait_add_cmd_completion(&mut store);
        assert_eq!(store.model().value(), 1 + 2 + 3 + 4 + 5 + 6);

        store.undo();
        store.undo();
        store.undo();
        store.undo();
        store.undo();
        assert_eq!(store.model().value(), 1);

        store.undo(); // Just ignored.
        assert_eq!(store.model().value(), 1);
    }


    #[test]
    fn file_undo_store_can_set_limit_and_recover() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), undo_store::Options::new().with_undo_limit(5)).unwrap();
        store.add(1).unwrap();
        store.add(2).unwrap();
        store.add(3).unwrap();
        store.add(4).unwrap();
        store.add(5).unwrap();
        store.add(6).unwrap(); // 2, 3, 4, 5, 6 (1 is deleted since undo limit is 5.)
        wait_add_cmd_completion(&mut store);
        assert_eq!(store.model().value(), 1 + 2 + 3 + 4 + 5 + 6);
        drop(store);

        let store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), undo_store::Options::new().with_undo_limit(5)).unwrap();
        assert_eq!(store.model().value(), 1 + 2 + 3 + 4 + 5 + 6);
    }

    #[test]
    fn file_undo_store_can_restore() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut sqlite_path = dir.clone();
        sqlite_path.push(SQLITE_FILE_NAME);

        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), undo_store::Options::new().with_undo_limit(5)).unwrap();
        store.add(1).unwrap();
        store.add(2).unwrap();
        store.add(3).unwrap();
        store.add(4).unwrap();
        wait_add_cmd_completion(&mut store);

        {
            let conn = rusqlite::Connection::open(&sqlite_path).unwrap();
            assert_eq!(cmd_ids(&conn), [1, 2, 3, 4]);
        }

        store.add(5).unwrap();
        wait_add_cmd_completion(&mut store);
        {
            let conn = rusqlite::Connection::open(&sqlite_path).unwrap();
            assert_eq!(cmd_ids(&conn), [1, 2, 3, 4, 5]);
        }

        // 2, 3, 4, 5, 6 (1 is deleted since undo limit is 5.)
        // snapshot of 6 is taken.
        store.add(6).unwrap();
        wait_add_cmd_completion(&mut store);
        {
            let conn = rusqlite::Connection::open(&sqlite_path).unwrap();
            assert_eq!(cmd_ids(&conn), [2, 3, 4, 5, 6]);
        }

        // [1] -cmd2(+2)-> [3] -cmd3(+3)-> [6] -cmd4(+4)-> [10] -cmd5(+5)-> [15] -cmd6(+6)-> [21]
        //                                                                                   ^ snapshot(id=6)
        assert_eq!(store.model().value(), 1 + 2 + 3 + 4 + 5 + 6);
        {
            let conn = rusqlite::Connection::open(&sqlite_path).unwrap();
            assert_eq!(cmd_ids(&conn), [2, 3, 4, 5, 6]);
            assert_eq!(snapshot_ids(&conn), [6]);
        }

        drop(store);
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), undo_store::Options::new().with_undo_limit(5)).unwrap();
        assert_eq!(store.model().value(), 1 + 2 + 3 + 4 + 5 + 6);
        {
            let conn = rusqlite::Connection::open(&sqlite_path).unwrap();
            assert_eq!(cmd_ids(&conn), [2, 3, 4, 5, 6]);
            assert_eq!(snapshot_ids(&conn), [6]);
        }

        store.undo();
        // [1] -cmd2(+2)-> [3] -cmd3(+3)-> [6] -cmd4(+4)-> [10] -cmd5(+5)-> [15] -cmd6(+6)-> [21]
        //                                                                  ^ current        ^ snapshot(id=6)
        assert_eq!(store.model().value(), 1 + 2 + 3 + 4 + 5);
        {
            let conn = rusqlite::Connection::open(&sqlite_path).unwrap();
            assert_eq!(cmd_ids(&conn), [2, 3, 4, 5, 6]);
            assert_eq!(snapshot_ids(&conn), [6]);
        }

        drop(store);
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), undo_store::Options::new().with_undo_limit(5)).unwrap();
        assert_eq!(store.model().value(), 1 + 2 + 3 + 4 + 5);

        store.undo();
        // [1] -cmd2(+2)-> [3] -cmd3(+3)-> [6] -cmd4(+4)-> [10] -cmd5(+5)-> [15] -cmd6(+6)-> [21]
        //                                                 ^ current                         ^ snapshot(id=6)
        assert_eq!(store.model().value(), 1 + 2 + 3 + 4);

        drop(store);
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), undo_store::Options::new().with_undo_limit(5)).unwrap();
        assert_eq!(store.model().value(), 1 + 2 + 3 + 4);

        store.add(7).unwrap();
        wait_add_cmd_completion(&mut store);
        // [1] -cmd2(+2)-> [3] -cmd3(+3)-> [6] -cmd4(+4)-> [10] -cmd5(+7)-> [15]
        //                                                                  ^ snapshot(id=5)
        assert_eq!(store.model().value(), 1 + 2 + 3 + 4 + 7);
        {
            let conn = rusqlite::Connection::open(&sqlite_path).unwrap();
            assert_eq!(cmd_ids(&conn), [2, 3, 4, 5]);
            assert_eq!(snapshot_ids(&conn), [5]);
        }

        drop(store);

        let store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), undo_store::Options::new().with_undo_limit(5)).unwrap();
        assert_eq!(store.model().value(), 1 + 2 + 3 + 4 + 7);
    }

    #[test]
    fn can_save_as() {
        use tempfile::tempdir;

        let from_dir = tempdir().unwrap();
        let mut from_dir = from_dir.as_ref().to_path_buf();
        from_dir.push("klavier");

        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(from_dir.clone(), undo_store::Options::new().with_undo_limit(5)).unwrap();
        store.add(1).unwrap();
        store.add(2).unwrap();
        store.add(3).unwrap();
        store.add(4).unwrap();
        store.add(5).unwrap();
        wait_add_cmd_completion(&mut store);

        // 2, 3, 4, 5, 6 (1 is deleted since undo limit is 5.)
        // snapshot of 6 is taken.
        store.add(6).unwrap();
        wait_add_cmd_completion(&mut store);

        let to_dir = tempdir().unwrap();
        let to_dir = to_dir.as_ref().to_path_buf();
        
        store.save_as(to_dir.clone()).unwrap();
        drop(store);
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(to_dir.clone(), undo_store::Options::new().with_undo_limit(5)).unwrap();

        assert_eq!(store.model().value(), 1 + 2 + 3 + 4 + 5 + 6);
        
        store.undo();
        assert_eq!(store.model().value(), 1 + 2 + 3 + 4 + 5);

        store.redo();
        assert_eq!(store.model().value(), 1 + 2 + 3 + 4 + 5 + 6);

        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(from_dir.clone(), undo_store::Options::new().with_undo_limit(5)).unwrap();
        assert_eq!(store.model().value(), 1 + 2 + 3 + 4 + 5 + 6);
        store.undo();
        assert_eq!(store.model().value(), 1 + 2 + 3 + 4 + 5);
        store.redo();
        assert_eq!(store.model().value(), 1 + 2 + 3 + 4 + 5 + 6);
    }

    #[test]
    fn can_restore_undo_point() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), undo_store::Options::new().with_undo_limit(5)).unwrap();
        store.add(1).unwrap();
        store.add(2).unwrap();
        store.add(3).unwrap();
        wait_add_cmd_completion(&mut store);

        assert_eq!(store.model().value(), 6);

        store.undo(); // 2, 3, 4, 5
        store.undo(); // 2, 3, 4
        assert_eq!(store.model().value(), 1);

        drop(store);

        let store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), undo_store::Options::new().with_undo_limit(5)).unwrap();
        assert_eq!(store.model().value(), 1);
    }

    #[test]
    fn can_restore_undo_point_pattern1() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(
            dir.clone(), undo_store::Options::new().with_undo_limit(1)
        ).unwrap();
        store.add(1).unwrap();
        store.add(2).unwrap();
        wait_add_cmd_completion(&mut store);

        //     Snapshot
        // +1, +2
        //      ^cur_seq_no

        assert_eq!(store.model().value(), 3);

        store.undo();
        //     Snapshot
        // +1, +2
        //  ^cur_seq_no

        assert_eq!(store.model().value(), 1);

        drop(store);

        let store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), undo_store::Options::new().with_undo_limit(5)).unwrap();
        assert_eq!(store.model().value(), 1);
    }

    #[test]
    fn can_restore_undo_point_pattern2() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(
            dir.clone(), undo_store::Options::new().with_undo_limit(1)
        ).unwrap();
        store.add(1).unwrap();
        store.add(2).unwrap();
        wait_add_cmd_completion(&mut store);

        //     Snapshot
        // +1, +2
        //      ^cur_seq_no

        store.add(3).unwrap();
        //     Snapshot
        // +1, +2, +3
        //          ^cur_seq_no
        wait_add_cmd_completion(&mut store);

        assert_eq!(store.model().value(), 6);

        store.undo();
        //     Snapshot
        // +1, +2, +3
        //      ^cur_seq_no

        assert_eq!(store.model().value(), 3);

        drop(store);

        let store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), undo_store::Options::new().with_undo_limit(5)).unwrap();
        assert_eq!(store.model().value(), 3);
    }

    pub fn enable_logging() {
        tracing_subscriber::fmt()
        .event_format(
            tracing_subscriber::fmt::format()
                .with_file(true)
                .with_line_number(true)
        )
        .with_max_level(LevelFilter::TRACE)
        .init();
    }

    #[test]
    fn can_restore_undo_point_pattern3() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), undo_store::Options::new().with_undo_limit(1)).unwrap();
        store.add(1).unwrap();
        store.add(2).unwrap();
        wait_add_cmd_completion(&mut store);
        let sqlite_path = dir.join(SQLITE_FILE_NAME);
        {
            let conn = rusqlite::Connection::open(&sqlite_path).unwrap();
            assert_eq!(cmd_ids(&conn), [2]);
            assert_eq!(snapshot_ids(&conn), [2]);
        }

        //     Snapshot
        // +1, +2
        //      ^cur_seq_no

        store.add(3).unwrap();
        store.add(4).unwrap();
        wait_add_cmd_completion(&mut store);
        //     Snapshot
        // +1, +2, +3, +4
        //              ^cur_seq_no

        assert_eq!(store.model().value(), 10);

        store.undo();
        //     Snapshot
        // +1, +2, +3, +4
        //          ^cur_seq_no

        assert_eq!(store.model().value(), 6);
        {
            let conn = rusqlite::Connection::open(&sqlite_path).unwrap();
            assert_eq!(cmd_ids(&conn), [4]);
        }

        drop(store);

        let store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), undo_store::Options::new().with_undo_limit(5)).unwrap();
        {
            let conn = rusqlite::Connection::open(&sqlite_path).unwrap();
            assert_eq!(cmd_ids(&conn), [4]);
        }
        assert_eq!(store.model().value(), 6);
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
            "select command_id from command order by command_id asc"
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
        let sqlite_path = dir.join(SQLITE_FILE_NAME);
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), undo_store::Options::new().with_undo_limit(3)).unwrap();
        store.add(1).unwrap(); // cmd1
        store.add(2).unwrap(); // cmd2
        store.add(3).unwrap(); // cmd3
        wait_add_cmd_completion(&mut store);

        // [0] -cmd1(+1)-> [1] -cmd2(+2)-> [3] -cmd3(+3)-> [6]
        {
            let conn = rusqlite::Connection::open(&sqlite_path).unwrap();
            assert_eq!(snapshot_ids(&conn).len(), 0);
        }

        // cmd1 should be removed because undo_limit = 3.
        store.add(4).unwrap();
        wait_add_cmd_completion(&mut store);
        // [1] -cmd2(+2)-> [3] -cmd3(+3)-> [6] -cmd4(+4)-> [10]
        //                                                 ^ snap(id=4)

        {
            let conn = rusqlite::Connection::open(&sqlite_path).unwrap();
            let ids = snapshot_ids(&conn);
            assert_eq!(ids.len(), 1);
            assert_eq!(ids[0], 4);
        }

        store.add(5).unwrap();
        wait_add_cmd_completion(&mut store);
        // [3] -cmd3(+3)-> [6] -cmd4(+4)-> [10] -cmd5(+5)-> [15]
        //                                 ^ snap(id=4)
        assert_eq!(store.model().value(), 15);
        {
            let conn = rusqlite::Connection::open(&sqlite_path).unwrap();
            let ids = snapshot_ids(&conn);
            assert_eq!(ids.len(), 1);
            assert_eq!(ids[0], 4);
        }

        store.add(6).unwrap();
        wait_add_cmd_completion(&mut store);
        // [6] -cmd4(+4)-> [10] -cmd5(+5)-> [15] -cmd6(+6)-> [21]
        //                 ^ snap(id=4)
        assert_eq!(store.model().value(), 21);
        {
            let conn = rusqlite::Connection::open(&sqlite_path).unwrap();
            let ids = snapshot_ids(&conn);
            assert_eq!(ids.len(), 1);
            assert_eq!(ids[0], 4);
        }

        store.add(7).unwrap();
        wait_add_cmd_completion(&mut store);
        // [10] -cmd5(+5)-> [15] -cmd6(+6)-> [21] -cmd7(+7)-> [28]
        // ^ snap(id=4)
        assert_eq!(store.model().value(), 28);
        {
            let conn = rusqlite::Connection::open(&sqlite_path).unwrap();
            let ids = snapshot_ids(&conn);
            assert_eq!(ids.len(), 1);
            assert_eq!(ids[0], 4);
        }

        store.add(8).unwrap();
        wait_add_cmd_completion(&mut store);
        // [15] -cmd6(+6)-> [21] -cmd7(+7)-> [28] -cmd8(+8)-> [36]
        //                                                    ^ snap(id=8)
        assert_eq!(store.model().value(), 36);
        {
            let conn = rusqlite::Connection::open(&sqlite_path).unwrap();
            let ids = snapshot_ids(&conn);
            assert_eq!(ids.len(), 1);
            assert_eq!(ids[0], 8);
        }

        drop(store);
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), undo_store::Options::new()).unwrap();
        assert_eq!(store.model().value(), 36);

        store.undo();
        assert_eq!(store.model().value(), 28);
    }

    #[test]
    fn on_snapshot_restored() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let mut dir = dir.as_ref().to_path_buf();
        dir.push("klavier");
        let mut store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), undo_store::Options::new().with_undo_limit(3)).unwrap();
        store.add(1).unwrap(); // cmd1
        store.add(2).unwrap(); // cmd2
        store.add(3).unwrap(); // cmd3
        store.add(4).unwrap(); // cmd3
        store.add(5).unwrap(); // cmd3
        wait_add_cmd_completion(&mut store);

        assert_eq!(store.model().value(), 15);
        assert_eq!(store.model().trace_count(), 5);
        drop(store);

        let store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(dir.clone(), undo_store::Options::new().with_undo_limit(3)).unwrap();
        assert_eq!(store.model().value(), 15);
        assert_ne!(store.model().trace_count(), 5);
        drop(store);

        let store = crate::undo_store::SqliteUndoStore::<SerSumCmd, SerSum, ()>::open(
            dir.clone(),
            undo_store::Options::new()
                .with_undo_limit(3)
                .with_on_snapshot_restored(Box::new(|sersum: SerSum| SerSum { trace_count: sersum.trace.len(), ..sersum }))
        ).unwrap();
        assert_eq!(store.model().value(), 15);
        assert_eq!(store.model().trace_count(), 5);
    }
}
