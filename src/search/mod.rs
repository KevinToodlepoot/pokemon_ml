//! Search from a position: one turn ahead, as the simultaneous-move game it is.
//!
//! A position is a [`Game`]: the true state of a game being played, or a
//! belief about one built from a single player's information. From it, each
//! side has a list of joint actions (a pair of actions, one per position on
//! the field: [`Game::legal`], in whatever order the caller thinks likely).
//! A **cell** is one pair of them played for one turn under one set of dice
//! ([`cell`]): copy the game, reseed it, act, play any switch in the middle
//! of the turn out, and hand the leaf to a value function. A **matrix** of
//! cells, averaged over the dice, is a zero-sum game whose equilibrium
//! ([`solve`]) says what each side should mix between and what the position
//! is worth. The **double oracle** ([`oracle`]) builds that matrix from the
//! likeliest action of each side outwards: solve, look for an action outside
//! the matrix that does better against the other side's mix, add it, solve
//! again, until neither side can improve or a budget runs out.
//!
//! The value function is the caller's: a hand score of the leaf
//! ([`hand_value`]), or a network over the leaf's observation. Cells are
//! simulated in parallel on the engine; values come back in batches. The
//! [`Search`] object asks for cells ([`Search::wanted`]), takes their values
//! ([`Search::supply`]) and moves on ([`Search::advance`]), so that a driver
//! can batch leaves across many searches at once; [`run`] is such a driver.
//! Everything is deterministic under its seed.

pub mod cell;
pub mod oracle;
pub mod solve;

pub use cell::{CellId, Follow, Leaves, NO_PLAN, NO_PLANS, Plan, SUMMARY, SimOpts, hand_value, pool, simulate, simulate_planned, words};
pub use oracle::{Budget, Candidates, Cap, CellStat, Report, Search, Status, heuristic_order, run};
pub use solve::{SolveOpts, Solution, best_col, best_row, exploitability, solve};

#[allow(unused_imports)]
use crate::env::Game;
