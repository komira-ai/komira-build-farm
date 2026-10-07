//! The shape every pure core shares: inputs in, effects out.

use crate::{Answer, ControlRecord, Refusal, StartLease, Waiting};

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
/// Variants arrive with the cores that emit them; today that is the scheduler
/// (`kbf-sched`). Effects are carried out in list order.
///
/// Exhaustive on purpose: a caller matches every variant, so a new effect is a compile
/// error in each place that carries effects out, not an arm that logs and drops it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Append the record to the control log, and feed it back to the machine once it is
    /// committed. Nothing that depends on the record happens before then.
    Commit(ControlRecord),
    /// Send a `Start` for a committed lease to its worker.
    Start(StartLease),
    /// Answer every waiter of a finished operation.
    Answer(Answer),
    /// Tell an operation's waiters why it waits (or that it no longer waits for a
    /// worker that can run it).
    Waiting(Waiting),
    /// Answer every waiter of an operation the scheduler refused to run.
    Refuse(Refusal),
}

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
