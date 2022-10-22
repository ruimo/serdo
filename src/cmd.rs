use serde::{de::DeserializeOwned, Serialize};

pub trait Cmd {
    type Model;

    fn undo(&self, model: &mut Self::Model);
    fn redo(&self, model: &mut Self::Model);
}

pub trait SerializableCmd: Cmd + Serialize + DeserializeOwned {
}

#[cfg(test)]
mod tests {
    use super::Cmd;

    enum SumAction {
        Add(i32), Sub(i32),
    }

    struct Sum(i32);

    impl Cmd for SumAction {
        type Model = Sum;

        fn undo(&self, model: &mut Self::Model) {
            match self {
                SumAction::Add(i) => model.0 -= *i,
                SumAction::Sub(i) => model.0 += *i,
            }
        }

        fn redo(&self, model: &mut Self::Model) {
            match self {
                SumAction::Add(i) => model.0 += *i,
                SumAction::Sub(i) => model.0 -= *i,
            }
        }
    }

    #[test]
    fn do_action() {
        let action = SumAction::Add(3);
        let mut model = Sum(100);

        action.undo(&mut model);
        assert_eq!(model.0, 97);

        let action = SumAction::Sub(3);
        action.undo(&mut model);
        assert_eq!(model.0, 100);
    }
}