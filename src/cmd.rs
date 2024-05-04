pub trait Cmd {
    type Model;

    fn undo(&self, model: &mut Self::Model);
    fn redo(&self, model: &mut Self::Model);

    // If two commands can be merged, merge them and return the merged command.
    fn merge<T: Cmd>(&self, _other: &T) -> Option<T> {
        None
    }
}

#[cfg(feature = "persistence")]
pub trait SerializableCmd: Cmd + serde::Serialize + serde::de::DeserializeOwned {
}

#[cfg(test)]
mod tests {
    use super::Cmd;

    enum SumCmd {
        Add(i32), Sub(i32),
    }

    struct Sum(i32);

    impl Cmd for SumCmd {
        type Model = Sum;

        fn undo(&self, model: &mut Self::Model) {
            match self {
                SumCmd::Add(i) => model.0 -= *i,
                SumCmd::Sub(i) => model.0 += *i,
            }
        }

        fn redo(&self, model: &mut Self::Model) {
            match self {
                SumCmd::Add(i) => model.0 += *i,
                SumCmd::Sub(i) => model.0 -= *i,
            }
        }
    }

    #[test]
    fn do_action() {
        let action = SumCmd::Add(3);
        let mut model = Sum(100);

        action.undo(&mut model);
        assert_eq!(model.0, 97);

        let action = SumCmd::Sub(3);
        action.undo(&mut model);
        assert_eq!(model.0, 100);
    }
}