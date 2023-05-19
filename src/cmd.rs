pub trait Cmd {
    type Model;
    type RedoResp;
    type RedoErr;

    fn undo(&self, model: &mut Self::Model);
    fn redo(&mut self, model: &mut Self::Model) -> Result<Self::RedoResp, Self::RedoErr>;
}

#[cfg(feature = "persistence")]
pub trait SerializableCmd: Cmd + serde::Serialize + serde::de::DeserializeOwned {
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
        type RedoResp = ();
        type RedoErr = ();

        fn undo(&self, model: &mut Self::Model) {
            match self {
                SumAction::Add(i) => model.0 -= *i,
                SumAction::Sub(i) => model.0 += *i,
            }
        }

        fn redo(&mut self, model: &mut Self::Model) -> Result<Self::RedoResp, Self::RedoErr> {
            match self {
                SumAction::Add(i) => {
                    model.0 += *i;
                    Ok(())
                },
                SumAction::Sub(i) => {
                    model.0 -= *i;
                    Ok(())
                }
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