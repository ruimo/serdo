use std::io;
use serdo::{cmd::{Cmd, Redo, Undo}, undo_store::{UndoStore, InMemoryUndoStore, InMemoryStoreErr}};

enum AddDivCmd {
    Add(i32), Div(i32),
}

#[derive(Default)]
struct Sum(i32);

enum DivCmdErr {
    DivByZero,
}

impl Redo<Sum, AddDivCmd, DivCmdErr> for () {
    fn redo(cmd: &mut AddDivCmd, model: &mut Sum) -> Result<Self, DivCmdErr> {
        match cmd {
            AddDivCmd::Add(i) => {
                model.0 += *i;
                Ok(())
            },
            AddDivCmd::Div(i) => {
                if *i == 0 {
                    Err(DivCmdErr::DivByZero)
                } else {
                    model.0 /= *i;
                    Ok(())
                }
            }
        }
    }
}

impl Undo<Sum, AddDivCmd, ()> for () {
    fn undo(cmd: &AddDivCmd, model: &mut Sum) -> Result<Self, ()> {
        match cmd {
            AddDivCmd::Add(i) => model.0 -= *i,
            AddDivCmd::Div(i) => model.0 *= *i,
        }

        Ok(())
    }
}

trait Model {
    fn add(&mut self, to_add: i32);
    fn div(&mut self, to_div: i32) -> Result<(), DivCmdErr>;
}

impl Cmd for AddDivCmd {
    type Model = Sum;

    fn restore(&mut self, model: &mut Self::Model) {
        let _:Result<(), DivCmdErr> = self.redo(model);
    }
}

impl Model for InMemoryUndoStore<AddDivCmd, Sum> {
    fn add(&mut self, to_add: i32) {
        let _: Result<Result<(), DivCmdErr>, InMemoryStoreErr> = self.add_cmd(AddDivCmd::Add(to_add));
    }

    fn div(&mut self, to_div: i32) -> Result<(), DivCmdErr> {
        let result: Result<Result<(), DivCmdErr>, InMemoryStoreErr> = self.add_cmd(AddDivCmd::Div(to_div));
        result.unwrap()
    }
}

fn main() {
    let mut line_buf = String::new();
    let mut store: InMemoryUndoStore<AddDivCmd, Sum> = InMemoryUndoStore::new(10);
    loop {
        println!("Current sum: {:?}", store.model().0);
        println!(
            "Command(+n: add number, /n: divide number, {}{}q: quit):",
            if store.can_undo().unwrap() { "u: undo, " } else { "" },
            if store.can_redo().unwrap() { "r: redo, " } else { "" }
        );
        io::stdin().read_line(&mut line_buf).unwrap();
        
        let cmd = line_buf.trim();
        if cmd.starts_with("+") {
            let num: i32 = cmd[1..].trim().parse().unwrap();
            store.add(num);
        } else if line_buf.starts_with("/") {
            let num: i32 = cmd[1..].trim().parse().unwrap();
            match store.div(num) {
                Ok(_) => {},
                Err(DivCmdErr::DivByZero) => {
                    println!("Divide by zero.")
                },
            };
        } else if cmd == "u" {
            if store.can_undo().unwrap() {
                match store.undo() {
                    Ok(_) => {},
                    Err(InMemoryStoreErr::CannotUndoRedo) => {
                        println!("Cannot undo.");
                    }
                }
            } else {
                println!("Cannot undo now.");
            }
        } else if cmd == "r" {
            if store.can_redo().unwrap() {
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

