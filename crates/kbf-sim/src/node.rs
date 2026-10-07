//! What a simulated node is: a [`StateMachine`] fed by the kernel.

use std::fmt::Debug;
use std::time::Duration;

use kbf_types::{FarmTime, StateMachine};

use crate::NodeId;

/// A node the kernel can drive.
///
/// The kernel feeds the node one [`NodeInput`] at a time through
/// [`StateMachine::apply`] and, after each, collects what the node asked to send and
/// when it asked to be woken with [`Node::take_outputs`]. A node reads no clock and no
/// random source of its own: `now` and `entropy` arrive in the input, so the seed decides
/// everything. Heterogeneous clusters use an enum of node kinds as `N`.
pub trait Node: StateMachine<Input = NodeInput<Self::Msg>> {
    /// What nodes send each other. Its `Debug` form goes into the trace, so it must
    /// print the same way in every process (no hashed collections inside).
    type Msg: Clone + Debug;

    /// Takes the outputs queued by earlier `apply` calls, oldest first.
    fn take_outputs(&mut self) -> Vec<Output<Self::Msg>>;
}

/// One input to a node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeInput<M> {
    /// The virtual time at which the input happens.
    pub now: FarmTime,
    /// 64 random bits drawn from the run's seed for this input, for a node that needs a
    /// random choice (an election timeout, a peer to gossip to).
    pub entropy: u64,
    /// What happened.
    pub event: Event<M>,
}

/// What happened to a node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event<M> {
    /// The node was added to the simulation. Its first input.
    Start,
    /// A message from `from` arrived.
    Message {
        /// The sender.
        from: NodeId,
        /// The message.
        msg: M,
    },
    /// A timer the node set has fired.
    Timer {
        /// The tag the node gave the timer.
        tag: u64,
    },
}

/// Something a node asks the kernel to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output<M> {
    /// Send `msg` to `to`, through the faulty network.
    Send {
        /// The receiver.
        to: NodeId,
        /// The message.
        msg: M,
    },
    /// Deliver [`Event::Timer`] with `tag` after `after` (whole milliseconds).
    Timer {
        /// The delay.
        after: Duration,
        /// Returned in the timer event.
        tag: u64,
    },
}
