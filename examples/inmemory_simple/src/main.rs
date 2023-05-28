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

fn main() {
    let mut line_buf = String::new();
    let mut store: InMemoryUndoStore<SumCmd, Sum> = InMemoryUndoStore::new(10);
    loop {
        println!("Current sum: {:?}", store.model().0);
        println!(
            "Command(+n: add number, *n: multiply number, {}{}q: quit):",
            if store.can_undo() { "u: undo, " } else { "" },
            if store.can_redo() { "r: redo, " } else { "" }
        );
        io::stdin().read_line(&mut line_buf).unwrap();
        
        let cmd = line_buf.trim();
        if cmd.starts_with("+") {
            let num: i32 = cmd[1..].trim().parse().unwrap();
            store.add(num);
        } else if line_buf.starts_with("*") {
            let num: i32 = cmd[1..].trim().parse().unwrap();
            store.mul(num);
        } else if cmd == "u" {
            if store.can_undo() {
                store.undo();
            } else {
                println!("Cannot undo now.");
            }
        } else if cmd == "r" {
            if store.can_redo() {
                store.redo();
            } else {
                println!("Cannot redo now.");
            }
        } else if cmd == "q" {
            break;
        } else {
            println!("??? Unknown command '{}'", cmd);
        }
        line_buf.clear();
    }
}
