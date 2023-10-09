cfg_if::cfg_if! {
    if #[cfg(feature = "persistence")] {
        use std::{path::PathBuf};
        use error_stack::{Report, Context};
    }
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

    // Save as
    CannotCopyStore {
        from: PathBuf, to: PathBuf, error: std::io::Error
    },

    // Restore
    CannotRestoreModel {
        snapshot_id: Option<i64>, not_foud_cmd_id: i64,
    },

    // Common
    FileError(PathBuf, std::io::Error),
    NotADirectory(PathBuf),
    CannotLock(std::path::PathBuf),
    CannotDeserialize { path: Option<std::path::PathBuf>, id: i64, ser_err: bincode::Error },
    OrphanSnapshot(PathBuf),
    DbError(std::path::PathBuf, Report<rusqlite::Error>),
    CmdSeqNoInconsistent,
}

#[cfg(feature = "persistence")]
impl std::fmt::Display for SqliteUndoStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SqliteUndoStoreError::CannotWriteCmd(path, io_err) => write!(f, "Cannot write command to {:?}: {:?}", path, io_err),
            SqliteUndoStoreError::SerializeError(ser_err) => write!(f, "Cannot serialize {:?}", ser_err),
            SqliteUndoStoreError::NeedCompaction(path) => write!(f, "Need compaction {:?}", path),
            SqliteUndoStoreError::NotFound(path) => write!(f, "Not found {:?}", path),
            SqliteUndoStoreError::CannotReadCmd(path, io_err) => write!(f, "Cannot read cmd {:?}: {:?}", path, io_err),
            SqliteUndoStoreError::DeserializeError(ser_err) => write!(f, "Cannot deserialize {:?}", ser_err),
            SqliteUndoStoreError::CannotUndoRedo => write!(f, "Cannot undo/redo."),
            SqliteUndoStoreError::CannotCopyStore { from, to, error } => write!(f, "Cannot copy store from {:?} to {:?}: {:?}", from, to, error),
            SqliteUndoStoreError::FileError(path, io_err) => write!(f, "File access error {:?}: {:?}", path, io_err),
            SqliteUndoStoreError::NotADirectory(path) => write!(f, "Specified path is not a directory: {:?}.", path),
            SqliteUndoStoreError::CannotLock(path) => write!(f, "Cannot lock: {:?}.", path),
            SqliteUndoStoreError::CannotDeserialize { path, id, ser_err } =>
                write!(f, "Cannot deserialize ").and_then(|_| 
                   if let Some(p) = path { write!(f, "{:?}, ", p) } else { Ok(()) }
                ).and_then(|_|
                    write!(f, "d: {:?}: {:?}", id, ser_err)
                ),
            SqliteUndoStoreError::OrphanSnapshot(path) => write!(f, "Orphan snapshot {:?}.", path),
            SqliteUndoStoreError::DbError(path, db_err) => write!(f, "Database error {:?}: {:?}", path, db_err),
            SqliteUndoStoreError::CmdSeqNoInconsistent => write!(f, "Command sequence number inconsistent."),
            SqliteUndoStoreError::CannotRestoreModel { snapshot_id, not_foud_cmd_id } => {
                write!(f, "Cannot restore model. ").and_then(|_| 
                    if let Some(snapshot_id) = snapshot_id {
                        write!(f, "Found snapshot_id: {}, ", snapshot_id)
                    } else {
                        Ok(())
                    }
                ).and_then(|_|
                    write!(f, "Command id {} not found.", not_foud_cmd_id)
                )
            },
        }
    }
}

#[cfg(feature = "persistence")]
impl Context for SqliteUndoStoreError {}
