use std::{io, env, borrow::Cow};
use clap::Parser;
use serdo::{cmd::{Cmd, SerializableCmd}, undo_store::{Options, SqliteUndoStore, UndoStore}};
use error_stack::{Result, report, Context};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Specify the milli-seconds to buffer commands.
    #[arg(short, long)]
    flash_period: Option<usize>,
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
enum EditorCmd {
    Append(String),
    DeleteAt {
        loc: usize,
        deleted: String,
    },
}

#[derive(Default)]
#[derive(serde::Serialize, serde::Deserialize)]
struct Buffer(Vec<String>);

impl Cmd for EditorCmd {
    type Model = Buffer;

    fn undo(&self, model: &mut Self::Model) {
        match self {
            EditorCmd::Append(_txt) => {
                model.0.remove(model.0.len() - 1);
            },
            EditorCmd::DeleteAt { loc, deleted } => {
                model.0.insert(*loc, deleted.clone());
            }
        }
    }

    fn redo(&self, model: &mut Self::Model) {
        match self {
            EditorCmd::Append(txt) => model.0.push(txt.clone()),
            EditorCmd::DeleteAt { loc, deleted: _ } => {
                model.0.remove(*loc);
            },
        }
    }
}

impl SerializableCmd for EditorCmd {}

#[derive(Debug)]
enum UndoStoreErr {
    InvalidIndex { max_index: usize },
}

impl std::fmt::Display for UndoStoreErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UndoStoreErr::InvalidIndex { max_index } => write!(f, "Invalid index {}", max_index),
        }
    }
}

impl Context for UndoStoreErr {}

trait Model: UndoStore {
    fn append(&mut self, txt: String);
    fn delete_at(&mut self, loc: usize) -> Result<(), UndoStoreErr>;
}

impl Model for SqliteUndoStore<EditorCmd, Buffer, UndoStoreErr> {
    fn append(&mut self, txt: String) {
        self.add_cmd(EditorCmd::Append(txt));
    }

    fn delete_at(&mut self, loc: usize) -> Result<(), UndoStoreErr> {
        self.mutate(
            Box::new(move |buf| {
                let len = buf.0.len();
                if len <= loc {
                    Err(report!(UndoStoreErr::InvalidIndex { max_index: len - 1 }))
                } else {
                    let deleted = buf.0.remove(loc);
                    Ok(EditorCmd::DeleteAt { loc, deleted })
                }
            })
        )
    }
}

#[derive(Debug, PartialEq)]
enum Resp {
    Cont, Msg(String), Quit,
}

struct App {
    store: Box<dyn Model<ModelType = Buffer, CmdType = EditorCmd, ErrType = UndoStoreErr>>,
}

impl App {
    fn new<P: AsRef<std::path::Path>>(dir: P, undo_limit: Option<usize>) -> Self {
        let mut options = Options::new();
        if let Some(limit) = undo_limit {
            options = options.with_undo_limit(limit);
        }
        Self {
            store: Box::new(SqliteUndoStore::<EditorCmd, Buffer, UndoStoreErr>::open(dir, options).unwrap()),
        }
    }

    fn buffer(&self) -> &Vec<String> { &self.store.model().0 }

    fn prompt(&self) -> Vec<Cow<str>> {
        let mut buf: Vec<Cow<str>> = vec!("Current buffer:".into());
        for line in self.buffer().iter() {
            buf.push(line.into());
        }
        buf.push("".into());
        buf.push(
            format!(
                "Command(+s: append text, -n: remove at line n, {}{}q: quit):",
                if self.store.can_undo() { "u: undo, " } else { "" },
                if self.store.can_redo() { "r: redo, " } else { "" }
            ).into()
        );
        buf
    }

    fn perform_cmd(&mut self, cmd: &str) -> Resp {
        if cmd.starts_with("+") {
            let txt = cmd[1..].trim();
            self.store.append(txt.to_owned());
            Resp::Cont
        } else if cmd.starts_with("-") {
            let loc: usize = cmd[1..].trim().parse().unwrap();
            match self.store.delete_at(loc) {
                Err(err) => {
                    let UndoStoreErr::InvalidIndex { max_index } = err.downcast_ref::<UndoStoreErr>().unwrap();
                    Resp::Msg(format!("Invalid index max: {}", max_index))
                },
                Ok(_) => Resp::Cont,
            }
        } else if cmd == "u" {
            if self.store.can_undo() {
                self.store.undo();
                Resp::Cont
            } else {
                Resp::Msg("Cannot undo now.".to_owned())
            }
        } else if cmd == "r" {
            if self.store.can_redo() {
                self.store.redo();
                Resp::Cont
            } else {
                Resp::Msg("Cannot redo now.".to_owned())
            }
        } else if cmd == "q" {
            Resp::Quit
        } else {
            Resp::Msg(format!("??? Unknown command '{}'", cmd))
        }
    }
}

fn main() {
    let mut line_buf = String::new();
    let mut dir = env::current_dir().unwrap();
    dir.push("editor");
    let mut app = App::new(dir, None);
    loop {
        for prompt in app.prompt().iter() {
            println!("{}", prompt);
        }
        io::stdin().read_line(&mut line_buf).unwrap();
        
        match app.perform_cmd(&line_buf.trim()) {
            Resp::Cont => {},
            Resp::Msg(msg) => println!("{}", msg),
            Resp::Quit => break,
        }
        line_buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::{Cow};
    use crate::{App, Resp};
    use tempfile::tempdir;

    #[test]
    fn initial_prompt() {
        let mut dir = tempdir().unwrap().as_ref().to_path_buf();
        dir.push("editor");

        let app = App::new(dir, Some(10));
        let prompt: Vec<Cow<str>> = app.prompt();
        assert_eq!(prompt.len(), 3);
        assert_eq!(prompt[0], "Current buffer:");
        assert_eq!(prompt[1], "");
        assert_eq!(prompt[2], "Command(+s: append text, -n: remove at line n, q: quit):");
    }

    #[test]
    fn undo_redo() {
        let mut dir = tempdir().unwrap().as_ref().to_path_buf();
        dir.push("editor");

        let mut app = App::new(dir, Some(10));
        assert_eq!(app.perform_cmd("+Hello"), Resp::Cont);
        assert_eq!(app.buffer(), &vec!("Hello"));
        assert_eq!(app.perform_cmd("+World"), Resp::Cont);
        assert_eq!(app.buffer(), &vec!("Hello", "World"));

        let prompt = app.prompt();
        assert_eq!(prompt.len(), 5);
        assert_eq!(prompt[0], "Current buffer:");
        assert_eq!(prompt[1], "Hello");
        assert_eq!(prompt[2], "World");
        assert_eq!(prompt[3], "");
        assert_eq!(prompt[4], "Command(+s: append text, -n: remove at line n, u: undo, q: quit):");
        assert_eq!(app.perform_cmd("r"), Resp::Msg("Cannot redo now.".to_owned()));

        assert_eq!(app.perform_cmd("u"), Resp::Cont);
        let prompt = app.prompt();
        assert_eq!(prompt.len(), 4);
        assert_eq!(prompt[0], "Current buffer:");
        assert_eq!(prompt[1], "Hello");
        assert_eq!(prompt[2], "");
        assert_eq!(prompt[3], "Command(+s: append text, -n: remove at line n, u: undo, r: redo, q: quit):");

        assert_eq!(app.perform_cmd("r"), Resp::Cont);
        let prompt = app.prompt();
        assert_eq!(prompt.len(), 5);
        assert_eq!(prompt[0], "Current buffer:");
        assert_eq!(prompt[1], "Hello");
        assert_eq!(prompt[2], "World");
        assert_eq!(prompt[3], "");
        assert_eq!(prompt[4], "Command(+s: append text, -n: remove at line n, u: undo, q: quit):");

        assert_eq!(app.perform_cmd("u"), Resp::Cont);
        assert_eq!(app.perform_cmd("u"), Resp::Cont);
        let prompt = app.prompt();
        assert_eq!(prompt.len(), 3);
        assert_eq!(prompt[0], "Current buffer:");
        assert_eq!(prompt[1], "");
        assert_eq!(prompt[2], "Command(+s: append text, -n: remove at line n, r: redo, q: quit):");

        assert_eq!(app.perform_cmd("u"), Resp::Msg("Cannot undo now.".to_owned()));
    }

    #[test]
    fn remove() {
        let mut dir = tempdir().unwrap().as_ref().to_path_buf();
        dir.push("editor");

        let mut app = App::new(dir, Some(10));
        assert_eq!(app.perform_cmd("+Hello"), Resp::Cont);
        assert_eq!(app.buffer(), &vec!("Hello"));
        assert_eq!(app.perform_cmd("+World"), Resp::Cont);
        assert_eq!(app.buffer(), &vec!("Hello", "World"));

        assert_eq!(app.perform_cmd("-2"), Resp::Msg("Invalid index max: 1".to_owned()));
        assert_eq!(app.perform_cmd("-1"), Resp::Cont);

        let prompt = app.prompt();
        assert_eq!(prompt.len(), 4);
        assert_eq!(prompt[0], "Current buffer:");
        assert_eq!(prompt[1], "Hello");
        assert_eq!(prompt[2], "");
        assert_eq!(prompt[3], "Command(+s: append text, -n: remove at line n, u: undo, q: quit):");

        assert_eq!(app.perform_cmd("u"), Resp::Cont);

        let prompt = app.prompt();
        assert_eq!(prompt.len(), 5);
        assert_eq!(prompt[0], "Current buffer:");
        assert_eq!(prompt[1], "Hello");
        assert_eq!(prompt[2], "World");
        assert_eq!(prompt[3], "");
        assert_eq!(prompt[4], "Command(+s: append text, -n: remove at line n, u: undo, r: redo, q: quit):");

        assert_eq!(app.perform_cmd("r"), Resp::Cont);

        let prompt = app.prompt();
        assert_eq!(prompt.len(), 4);
        assert_eq!(prompt[0], "Current buffer:");
        assert_eq!(prompt[1], "Hello");
        assert_eq!(prompt[2], "");
        assert_eq!(prompt[3], "Command(+s: append text, -n: remove at line n, u: undo, q: quit):");
    }
}
