use std::io;
use serdo::{cmd::Cmd, undo_store::{UndoStore, InMemoryUndoStore}};

enum SumCmd {
    Add(i32), Mul(i32),
}

#[derive(Default)]
struct Sum(i32);

impl Cmd for SumCmd {
    type Model = Sum;

    fn undo(&self, model: &mut Self::Model) {
        match self {
            SumCmd::Add(i) => model.0 -= *i,
            SumCmd::Mul(i) => model.0 /= *i,
        }
    }

    fn redo(&self, model: &mut Self::Model) {
        match self {
            SumCmd::Add(i) => model.0 += *i,
            SumCmd::Mul(i) => model.0 *= *i,
        }
    }
}

trait Model {
    fn add(&mut self, to_add: i32);
    fn mul(&mut self, to_mul: i32);
}

impl Model for InMemoryUndoStore<SumCmd, Sum> {
    fn add(&mut self, to_add: i32) {
        self.add_cmd(SumCmd::Add(to_add));
    }

    fn mul(&mut self, to_mul: i32) {
        self.add_cmd(SumCmd::Mul(to_mul));
    }
}

enum Resp {
    Cont, Msg(String), Quit,
}

struct App {
    store: InMemoryUndoStore<SumCmd, Sum>,
}

impl App {
    fn new(capacity: usize) -> Self {
        Self {
            store: InMemoryUndoStore::new(capacity),
        }
    }

    fn prompt(&self) -> Vec<String> {
        vec!(
            format!("Current sum: {:?}", self.store.model().0),
            format!(
                "Command(+n: add number, *n: multiply number, {}{}q: quit):",
                if self.store.can_undo() { "u: undo, " } else { "" },
                if self.store.can_redo() { "r: redo, " } else { "" }
            )
        )
    }

    fn perform_cmd(&mut self, cmd: &str) -> Resp {
        if cmd.starts_with("+") {
            let num: i32 = cmd[1..].trim().parse().unwrap();
            self.store.add(num);
            Resp::Cont
        } else if cmd.starts_with("*") {
            let num: i32 = cmd[1..].trim().parse().unwrap();
            self.store.mul(num);
            Resp::Cont
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
    let mut app = App::new(10);
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
