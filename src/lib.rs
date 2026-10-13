//! A fast Pokémon Champions doubles battle engine, checked turn by turn
//! against Pokémon Showdown.
//!
//! Start with [`Battle::new`], read [`Battle::request`], and answer with
//! [`Battle::choose`]. [`Battle::legal_choices`] lists what each slot may do.

mod abilities;
pub mod battle;
mod choice;
mod conditions;
pub mod data;
pub mod env;
mod events;
pub mod follow;
pub mod format;
mod items;
mod movecbs;
mod moves;
pub mod obs;
pub mod observer;
pub mod position;
#[cfg(feature = "python")]
mod python;
pub mod replay;
#[rustfmt::skip]
mod tables;
pub mod rng;
pub mod search;
pub mod shown;
pub mod speed;
pub mod state;
pub mod teams;
pub mod trace;

pub use battle::{Error, PokemonSet};
pub use data::{Gender, VolKind};
pub use state::{ACTIVE, Battle, Choice, MAX_TEAM, MonRef, Pokemon, Request, Side};
