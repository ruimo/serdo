pub trait Undo<M, C: Cmd<Model = M> + ?Sized, E>: Sized {
    fn undo(cmd: &C, model: &mut M) -> Result<Self, E> where Self: Sized;
}

pub trait Redo<M, C: Cmd<Model = M> + ?Sized, E>: Sized {
    fn redo(cmd: &mut C, model: &mut M) -> Result<Self, E> where Self: Sized;
}

pub trait Cmd {
    type Model;

    fn undo<E, U: Undo<Self::Model, Self, E>>(&self, model: &mut Self::Model) -> Result<U, E> {
        U::undo(self, model)
    }

    fn redo<E, R: Redo<Self::Model, Self, E>>(&mut self, model: &mut Self::Model) -> Result<R, E> {
        R::redo(self, model)
    }

    fn restore(&mut self, model: &mut Self::Model);
}

#[cfg(feature = "persistence")]
pub trait SerializableCmd: Cmd + serde::Serialize + serde::de::DeserializeOwned {
}

#[cfg(test)]
mod tests {
    use super::{Cmd, Undo, Redo};

    enum SumAction {
        Add(i32), Sub(i32),
    }

    struct Sum(i32);

    impl Undo<Sum, SumAction, ()> for () {
        fn undo(cmd: &SumAction, model: &mut Sum) -> Result<Self, ()> {
            match cmd {
                SumAction::Add(i) => {
                    model.0 -= i;
                },
                SumAction::Sub(i) => {
                    model.0 += i;
                }
            }
            Ok(())
        }
    }

    impl Redo<Sum, SumAction, ()> for () {
        fn redo(cmd: &mut SumAction, model: &mut Sum) -> Result<Self, ()> {
            match cmd {
                SumAction::Add(i) => {
                    model.0 += *i;
                },
                SumAction::Sub(i) => {
                    model.0 -= *i;
                },
            }
            Ok(())
        }
    }

    impl Cmd for SumAction {
        type Model = Sum;

        fn restore(&mut self, model: &mut Self::Model) {
            let _ = self.redo::<(), ()>(model);
        }
    }

    #[test]
    fn do_action() {
        let mut action = SumAction::Add(3);
        let mut model = Sum(100);

        action.undo::<(), ()>(&mut model);
        assert_eq!(model.0, 97);

        let action = SumAction::Sub(3);
        action.undo::<(), ()>(&mut model);
        assert_eq!(model.0, 100);
    }
}