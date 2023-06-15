use std::{io, borrow::Cow};
use serdo::{cmd::Cmd, undo_store::{UndoStore, InMemoryUndoStore}};

enum SumCmd {
    Add(i32), Mul(i32),
}

#[derive(Default)]
struct Sum {
    sum: i32,
    call_count: usize, // Let it treat as out of scope of serdo.
}

impl Cmd for SumCmd {
    type Model = Sum;

    fn undo(&self, model: &mut Self::Model) {
        match self {
            SumCmd::Add(i) => model.sum -= *i,
            SumCmd::Mul(i) => model.sum /= *i,
        }
    }

    fn redo(&self, model: &mut Self::Model) {
        match self {
            SumCmd::Add(i) => model.sum += *i,
            SumCmd::Mul(i) => model.sum *= *i,
        }
    }
}

trait Model: UndoStore<CmdType = SumCmd, ModelType = Sum, ErrType = ()> {
    fn add(&mut self, to_add: i32);
    fn mul(&mut self, to_mul: i32);
}

impl Model for InMemoryUndoStore<SumCmd, Sum, ()> {
    fn add(&mut self, to_add: i32) {
        self.irreversible_mutate(Box::new(|model| model.call_count += 1));
        self.add_cmd(SumCmd::Add(to_add));
    }

    fn mul(&mut self, to_mul: i32) {
        self.irreversible_mutate(Box::new(|model| model.call_count += 1));
        self.add_cmd(SumCmd::Mul(to_mul));
    }
}

#[derive(Debug, PartialEq)]
enum Resp {
    Cont, Msg(String), Quit,
}

struct App {
    store: Box<dyn Model>,
}

impl App {
    fn new(capacity: usize) -> Self {
        Self {
            store: Box::new(InMemoryUndoStore::<SumCmd, Sum, ()>::new(capacity)),
        }
    }

    fn sum(&self) -> i32 {self.store.model().sum}
    fn call_count(&self) -> usize {self.store.model().call_count}

    fn prompt(&self) -> Vec<Cow<str>> {
        vec!(
            format!("Current sum: {}, call count: {}", self.sum(), self.call_count()).into(),
            format!(
                "Command(+n: add number, *n: multiply number, {}{}q: quit):",
                if self.store.can_undo() { "u: undo, " } else { "" },
                if self.store.can_redo() { "r: redo, " } else { "" }
            ).into()
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

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use crate::{App, Resp};

    #[test]
    fn initial_prompt() {
        let app = App::new(10);
        let prompt: Vec<Cow<str>> = app.prompt();
        assert_eq!(prompt.len(), 2);
        assert_eq!(prompt[0], "Current sum: 0, call count: 0");
        assert_eq!(prompt[1], "Command(+n: add number, *n: multiply number, q: quit):");
    }

    #[test]
    fn add_and_mul() {
        let mut app = App::new(10);
        assert_eq!(app.perform_cmd("+3"), Resp::Cont);
        assert_eq!(app.sum(), 3);
        assert_eq!(app.perform_cmd("*4"), Resp::Cont);
        assert_eq!(app.sum(), 12);

        let prompt = app.prompt();
        assert_eq!(prompt.len(), 2);
        assert_eq!(prompt[0], "Current sum: 12, call count: 2");
        assert_eq!(prompt[1], "Command(+n: add number, *n: multiply number, u: undo, q: quit):");
        assert_eq!(app.perform_cmd("r"), Resp::Msg("Cannot redo now.".to_owned()));

        assert_eq!(app.perform_cmd("u"), Resp::Cont);
        let prompt = app.prompt();
        assert_eq!(prompt.len(), 2);
        assert_eq!(prompt[0], "Current sum: 3, call count: 2");
        assert_eq!(prompt[1], "Command(+n: add number, *n: multiply number, u: undo, r: redo, q: quit):");

        assert_eq!(app.perform_cmd("r"), Resp::Cont);
        let prompt = app.prompt();
        assert_eq!(prompt.len(), 2);
        assert_eq!(prompt[0], "Current sum: 12, call count: 2");
        assert_eq!(prompt[1], "Command(+n: add number, *n: multiply number, u: undo, q: quit):");

        assert_eq!(app.perform_cmd("u"), Resp::Cont);
        assert_eq!(app.perform_cmd("u"), Resp::Cont);
        let prompt = app.prompt();
        assert_eq!(prompt.len(), 2);
        assert_eq!(prompt[0], "Current sum: 0, call count: 2");
        assert_eq!(prompt[1], "Command(+n: add number, *n: multiply number, r: redo, q: quit):");

        assert_eq!(app.perform_cmd("u"), Resp::Msg("Cannot undo now.".to_owned()));
    }
}
