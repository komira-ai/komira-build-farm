//! The shape every pure core shares: inputs in, effects out.

/// A deterministic state machine.
///
/// `apply` changes the state and returns the effects the change calls for. It does no
/// I/O and reads no clock or random source: time arrives inside `Input` as a
/// [`FarmTime`](crate::FarmTime). Applying the same inputs in the same order to equal
/// states yields equal states and equal effects, which is what lets every replica apply
/// committed log entries and lets a simulation seed replay exactly. Effects are carried
/// out by the caller, never by the machine.
pub trait StateMachine {
    /// One input: a committed command, a report, a tick carrying the farm time.
    type Input;

    /// Applies `input` and returns the effects it calls for, in the order they should
    /// be carried out.
    fn apply(&mut self, input: Self::Input) -> Vec<Effect>;
}

/// Something a [`StateMachine`] asks its caller to do.
///
/// This is a placeholder with no variants yet, so no value of it can exist and every
/// `apply` returns an empty list. Variants (starting an action on a worker, raising an
/// alert, launching a node) arrive with the cores that emit them.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FarmTime;

    /// A minimal machine: tracks the latest farm time it has seen.
    #[derive(Debug, Default, PartialEq)]
    struct Latest(FarmTime);

    impl StateMachine for Latest {
        type Input = FarmTime;

        fn apply(&mut self, input: FarmTime) -> Vec<Effect> {
            self.0 = self.0.max(input);
            Vec::new()
        }
    }

    /// Catches: a trait shape that cannot be implemented by a plain owned value or
    /// cannot be driven from a list of inputs (the simulator's use). Also shows replay:
    /// the same inputs give the same state.
    #[test]
    fn same_inputs_replay_to_same_state() {
        let inputs = [5, 3, 9, 7].map(FarmTime::from_millis);
        let run = || {
            let mut m = Latest::default();
            let effects: Vec<Effect> = inputs.iter().flat_map(|&t| m.apply(t)).collect();
            (m, effects)
        };
        let (a, effects) = run();
        assert_eq!(a, Latest(FarmTime::from_millis(9)));
        assert!(effects.is_empty());
        assert_eq!(run(), (a, effects));
    }
}
