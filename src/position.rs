//! A battle position as data: [`Battle::to_state`] writes one down and
//! [`Battle::from_state`] builds a battle from one.
//!
//! This is how a battle is started from the middle: describe what is on the
//! field, hand it to `from_state`, and play on (or search) from there. The
//! description can be written by hand, edited from an exported one, or read
//! from JSON; every field has a default, so only what differs from "nothing
//! has happened yet" needs saying.
//!
//! ```
//! use vgc_engine::position::{BattleState, CondState, PokemonState, SideState};
//! use vgc_engine::Battle;
//!
//! let mon = |species: &str, moves: &[&str]| PokemonState::new(species, moves);
//! let mut ours = SideState::new(vec![
//!     mon("Incineroar", &["Fake Out", "Flare Blitz", "Parting Shot", "Protect"]).ability("Intimidate"),
//!     mon("Garchomp", &["Earthquake", "Rock Slide", "Protect"]).item("Life Orb"),
//!     mon("Rillaboom", &["Grassy Glide", "Wood Hammer"]),
//! ]);
//! ours.pokemon[0].hp_percent = Some(42.0);
//! ours.pokemon[0].boosts[0] = -1;
//! ours.conditions.push(CondState::new("tailwind").turns(2));
//! let theirs = SideState::new(vec![
//!     mon("Pelipper", &["Hurricane", "Protect"]).ability("Drizzle"),
//!     mon("Archaludon", &["Electro Shot", "Draco Meteor"]),
//! ]);
//! let state = BattleState { turn: 4, sides: [ours, theirs], weather: Some(CondState::new("raindance").turns(3)), ..Default::default() };
//! let battle = Battle::from_state(&state).unwrap();
//! assert_eq!(battle.turn, 4);
//! ```
//!
//! Two kinds of field are in here. Most say something about the game that a
//! player could, at least in principle, know: HP, stat stages, who is
//! taunted for how long. The rest is bookkeeping the simulator keeps (the
//! order effects started in, cached speeds, which moves the coming request
//! disables). An exported position carries the bookkeeping, and a battle
//! rebuilt from it continues exactly as the original would, random numbers
//! included. A hand-written position leaves it out (`prepared: false`) and
//! `from_state` works it out.
//!
//! A position is taken where a battle waits for choices: at the start of a
//! turn, or at a switch request, which may come in the middle of a turn (then
//! `pending` holds the actions still to run).

use serde::{Deserialize, Serialize};

use crate::battle::{Error, PokemonSet, calc_stats};
use crate::data::*;
use crate::shown::{Bar, ItemShown, NOTHING_SHOWN, SHOWN_MOVES, Shown, ShownMon, UNKNOWN};
use crate::state::*;

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

fn bad(msg: impl Into<String>) -> Error {
    Error::BadState(msg.into())
}

/// A Pokémon named by its side (0 or 1) and its place in that side's
/// [`SideState::pokemon`] list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MonId {
    pub side: u8,
    pub pokemon: u8,
}

/// What each side has been told of a Pokémon's types: after a Reflect Type
/// the log names the Pokémon copied, not the types, and one of the two may be
/// in disguise.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TypesSaidState {
    /// The base types side 0, then side 1, takes it to have. Empty: nothing said.
    pub seen: [Vec<String>; 2],
    /// The base types Showdown's log last gave it (`apparentType`).
    pub apparent: Vec<String>,
    /// Showdown's `knownType`.
    pub known: bool,
    /// This turn it has copied another Pokémon's types; the last time by Reflect
    /// Type from a foe; and its own side has since been told what they are.
    pub copied: bool,
    pub copied_foe: bool,
    pub told: bool,
    /// What its own side took its base types to be before the first of those.
    pub before: Vec<String>,
    /// Who has aimed a move at it and stayed on the field since.
    pub attacked_by: Vec<MonId>,
}

/// One move a Pokémon knows.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MoveState {
    /// Showdown name or id.
    pub id: String,
    /// PP left; all of it if absent.
    #[serde(skip_serializing_if = "is_default")]
    pub pp: Option<u8>,
    /// Maximum PP; the move's own if absent (5 for a move copied by Transform).
    #[serde(skip_serializing_if = "is_default")]
    pub max_pp: Option<u8>,
    /// Used at least once since the Pokémon came in (Last Resort asks).
    #[serde(skip_serializing_if = "is_default")]
    pub used: bool,
    /// Bookkeeping: cannot be chosen at the coming request.
    #[serde(skip_serializing_if = "is_default")]
    pub disabled: bool,
    /// Bookkeeping: disabled by something its player has not been shown.
    #[serde(skip_serializing_if = "is_default")]
    pub hidden: bool,
}

impl MoveState {
    pub fn new(id: &str) -> MoveState {
        MoveState { id: id.to_string(), ..Default::default() }
    }
    pub fn pp(mut self, pp: u8) -> MoveState {
        self.pp = Some(pp);
        self
    }
}

/// A condition: a volatile on a Pokémon (Taunt, Substitute, a Choice lock), a
/// condition on a side (Tailwind, Stealth Rock) or on one of its positions
/// (Wish), a pseudo-weather (Trick Room), the weather or the terrain.
///
/// `value`, `a` and `b` are whatever the condition keeps count of; the
/// constructors below fill them in for the common ones:
///
/// | Condition | `value` | Other |
/// |---|---|---|
/// | `substitute` | HP left, in half points | |
/// | `confusion` | move attempts left | |
/// | `stall` (the Protect streak) | the next Protect works 1 time in `value` | `turns` 2 |
/// | `choicelock`, `encore`, `disable` | | `move_id` |
/// | `lockedmove` (Outrage) | | `move_id`; `a` turns left |
/// | `twoturnmove` | | `move_id`; the move's own volatile (`fly`) holds the target location in `a` |
/// | `stockpile` | layers | `a`, `b`: Defense and Sp. Def stages to give back |
/// | `partiallytrapped` | damage divisor (8, or 6 with Binding Band) | `source` |
/// | `perishsong`, `taunt`, `yawn`, ... | | `turns` |
/// | `leechseed` | | `source_slot`: who is healed |
/// | `spikes`, `toxicspikes` | layers | |
/// | `wish` | HP it restores | `a`: turn counter when made |
/// | `futuremove` | | `a`: turn counter it lands at; `source` |
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CondState {
    /// Showdown id: `taunt`, `tailwind`, `raindance`, `trickroom`.
    pub id: String,
    /// Turns left, where 0 means it has no timer. Absent: the condition's usual duration.
    #[serde(skip_serializing_if = "is_default")]
    pub turns: Option<u8>,
    #[serde(skip_serializing_if = "is_default")]
    pub value: u16,
    /// The move a condition is about (a Choice lock, Encore, Disable, a rampage, a charging move).
    #[serde(skip_serializing_if = "is_default")]
    pub move_id: String,
    /// The Pokémon that caused it.
    #[serde(skip_serializing_if = "is_default")]
    pub source: Option<MonId>,
    /// Where that Pokémon stood: side * 2 + position, or -1 for nowhere.
    /// Absent: where `source` stands now.
    #[serde(skip_serializing_if = "is_default")]
    pub source_slot: Option<i8>,
    #[serde(skip_serializing_if = "is_default")]
    pub a: i16,
    #[serde(skip_serializing_if = "is_default")]
    pub b: i16,
    /// Bookkeeping: the counter value when it started (ties between handlers go by it).
    #[serde(skip_serializing_if = "is_default")]
    pub order: u32,
    /// Bookkeeping: one of its handlers has run inside an event.
    ///
    /// For a Healing Wish waiting on a position it is more than bookkeeping:
    /// Ally Switch has moved a Pokémon onto it that it had nothing to heal,
    /// and since then (a slip of Showdown's) it does nothing for a Pokémon
    /// that switches in there, only for one that another Ally Switch brings.
    #[serde(skip_serializing_if = "is_default")]
    pub targeted: bool,
}

impl CondState {
    pub fn new(id: &str) -> CondState {
        CondState { id: to_id(id), ..Default::default() }
    }
    pub fn turns(mut self, turns: u8) -> CondState {
        self.turns = Some(turns);
        self
    }
    pub fn value(mut self, value: u16) -> CondState {
        self.value = value;
        self
    }
    pub fn source(mut self, side: u8, pokemon: u8) -> CondState {
        self.source = Some(MonId { side, pokemon });
        self
    }
    /// A substitute with this much HP left.
    pub fn substitute(hp: u16) -> CondState {
        CondState::new("substitute").value(2 * hp)
    }
    /// Confusion with this many move attempts left (it starts at 2 to 5).
    pub fn confusion(attempts: u16) -> CondState {
        CondState::new("confusion").value(attempts)
    }
    /// Locked into a move by a Choice item.
    pub fn choice_lock(move_id: &str) -> CondState {
        CondState { move_id: to_id(move_id), ..CondState::new("choicelock") }
    }
    /// Held to a move by Encore for this many more turns.
    pub fn encore(move_id: &str, turns: u8) -> CondState {
        CondState { move_id: to_id(move_id), ..CondState::new("encore").turns(turns) }
    }
    /// A move disabled for this many more turns.
    pub fn disable(move_id: &str, turns: u8) -> CondState {
        CondState { move_id: to_id(move_id), ..CondState::new("disable").turns(turns) }
    }
    /// Between the two turns of a two-turn move (in the air with Fly, charged
    /// up for Solar Beam), aimed at this target location: the two volatiles it takes.
    pub fn charging(move_id: &str, target: i8) -> [CondState; 2] {
        let id = to_id(move_id);
        [
            CondState { move_id: id.clone(), ..CondState::new("twoturnmove").turns(1) },
            CondState { a: target as i16, ..CondState::new(&id).turns(1) },
        ]
    }
    /// Has protected `streak` turns running (1 or more): the next attempt works 1 time in 3^streak.
    pub fn protect_streak(streak: u32) -> CondState {
        CondState::new("stall").turns(2).value(3u16.saturating_pow(streak).min(729))
    }
}

/// What an ability, an item or a status keeps besides its name.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Extra {
    /// Bookkeeping: the counter value when the effect started.
    #[serde(skip_serializing_if = "is_default")]
    pub order: u32,
    /// What the effect remembers. Abilities: Disguise, 1 once broken; Supreme
    /// Overlord, allies fallen when it came in; Cud Chew, `b` turns until the
    /// berry in `item` is eaten again.
    #[serde(skip_serializing_if = "is_default")]
    pub a: i16,
    #[serde(skip_serializing_if = "is_default")]
    pub b: i16,
    /// The item the state refers to (Cud Chew's berry).
    #[serde(skip_serializing_if = "is_default")]
    pub item: String,
}

/// How a Pokémon's move went (`moveThisTurnResult`); Stomping Tantrum and Temper Flare ask.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MoveOutcome {
    /// Did not move.
    #[default]
    None,
    /// Moved, with nothing to succeed or fail at.
    Null,
    Failed,
    Succeeded,
}

/// A damaging hit a Pokémon took from a foe that is still on the field (Metal Burst answers the last).
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HitState {
    /// The attacker's place in the other side's `pokemon` list.
    pub pokemon: u8,
    /// The slot it attacked from: side * 2 + position. Absent if it was not
    /// on the field when the hit landed (a Future Sight).
    pub slot: Option<u8>,
    pub damage: u16,
    pub this_turn: bool,
}

/// One Pokémon as it is now.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PokemonState {
    /// Its species now, Showdown name or id: the Mega if it has Mega Evolved,
    /// the other Pokémon's species if it is transformed.
    pub species: String,
    /// The species it goes back to on leaving the field. Absent: `species`
    /// (or Aegislash and Morpeko's ordinary formes for their other ones).
    /// Required for a transformed Pokémon.
    #[serde(skip_serializing_if = "is_default")]
    pub base_species: Option<String>,
    /// The species it was brought as. Absent: worked out from `base_species` (a Mega's ordinary forme).
    #[serde(skip_serializing_if = "is_default")]
    pub original_species: Option<String>,
    /// Nature by name; neutral if empty.
    #[serde(skip_serializing_if = "is_default")]
    pub nature: String,
    /// Champions stat points: hp, atk, def, spa, spd, spe.
    #[serde(skip_serializing_if = "is_default")]
    pub stat_points: [u8; 6],
    /// "M", "F" or "N". Absent: the species' fixed gender, or male.
    #[serde(skip_serializing_if = "is_default")]
    pub gender: Option<String>,
    /// Max HP, Atk, Def, SpA, SpD, Spe before stat stages. Absent: computed
    /// from species, nature and stat points. Needed after Transform, Power
    /// Trick, Power Split and the like.
    #[serde(skip_serializing_if = "is_default")]
    pub stats: Option<[u16; 6]>,
    /// HP now. Absent: `hp_percent` of the maximum, or full.
    #[serde(skip_serializing_if = "is_default")]
    pub hp: Option<u16>,
    /// HP as a percentage, for a Pokémon whose exact HP is not known (rounded up).
    #[serde(skip_serializing_if = "is_default")]
    pub hp_percent: Option<f32>,
    #[serde(skip_serializing_if = "is_default")]
    pub fainted: bool,
    /// "brn", "par", "psn", "tox", "slp", "frz" or empty.
    #[serde(skip_serializing_if = "is_default")]
    pub status: String,
    /// Turns asleep or frozen so far (sleep ends when it reaches 2 or 3, freeze at 3).
    #[serde(skip_serializing_if = "is_default")]
    pub status_turns: u8,
    /// How far toxic poison has built up.
    #[serde(skip_serializing_if = "is_default")]
    pub toxic_stage: u8,
    #[serde(skip_serializing_if = "is_default")]
    pub status_extra: Extra,
    /// Stat stages: atk, def, spa, spd, spe, accuracy, evasion.
    #[serde(skip_serializing_if = "is_default")]
    pub boosts: [i8; 7],
    /// The moves it has now.
    pub moves: Vec<MoveState>,
    /// Has taken another Pokémon's shape (Transform, Imposter).
    #[serde(skip_serializing_if = "is_default")]
    pub transformed: bool,
    /// Its own moves, while it is transformed.
    #[serde(skip_serializing_if = "is_default")]
    pub base_moves: Vec<MoveState>,
    /// Illusion: the place in this side's list of the Pokémon it is disguised as.
    #[serde(skip_serializing_if = "is_default")]
    pub illusion: Option<u8>,
    /// The Mega it can still evolve into, as a species id; empty for none.
    /// Absent: by its item, unless its side has already Mega Evolved.
    #[serde(skip_serializing_if = "is_default")]
    pub can_mega: Option<String>,
    /// Its types now. Absent: the species' own.
    #[serde(skip_serializing_if = "is_default")]
    pub types: Option<Vec<String>>,
    /// A third type from Forest's Curse or Trick-or-Treat.
    #[serde(skip_serializing_if = "is_default")]
    pub added_type: String,
    /// What has been said of its types, where that is more than what they are.
    /// Absent: both sides take them for what they are (or for those of the
    /// Pokémon it is disguised as), and nothing has attacked it.
    #[serde(skip_serializing_if = "is_default")]
    pub types_said: Option<TypesSaidState>,
    /// Its ability now; empty for none.
    #[serde(skip_serializing_if = "is_default")]
    pub ability: String,
    /// The ability it gets back on leaving the field. Absent: `ability`.
    #[serde(skip_serializing_if = "is_default")]
    pub base_ability: Option<String>,
    #[serde(skip_serializing_if = "is_default")]
    pub ability_extra: Extra,
    /// Stat stages Opportunist is waiting to copy.
    #[serde(skip_serializing_if = "is_default")]
    pub ability_boosts: [i8; 7],
    /// Supersweet Syrup has gone off already this battle.
    #[serde(skip_serializing_if = "is_default")]
    pub syrup_triggered: bool,
    /// Its held item now; empty for none.
    #[serde(skip_serializing_if = "is_default")]
    pub item: String,
    #[serde(skip_serializing_if = "is_default")]
    pub item_extra: Extra,
    /// The item it last used up or ate (Recycle, Harvest).
    #[serde(skip_serializing_if = "is_default")]
    pub last_item: String,
    #[serde(skip_serializing_if = "is_default")]
    pub used_item_this_turn: bool,
    /// Has eaten a berry this battle (Belch).
    #[serde(skip_serializing_if = "is_default")]
    pub ate_berry: bool,
    /// Volatile conditions; only a Pokémon on the field has any.
    #[serde(skip_serializing_if = "is_default")]
    pub volatiles: Vec<CondState>,
    /// Must be replaced at the coming switch request.
    #[serde(skip_serializing_if = "is_default")]
    pub switch_flag: bool,
    /// The move of its own that is switching it out (U-turn, Baton Pass), if one is.
    #[serde(skip_serializing_if = "is_default")]
    pub switch_move: String,
    /// Full turns on the field since it came in. Absent: 1 on the field, 0 on the bench.
    #[serde(skip_serializing_if = "is_default")]
    pub active_turns: Option<u16>,
    /// Came in this turn.
    #[serde(skip_serializing_if = "is_default")]
    pub newly_switched: bool,
    /// Times it has started to move since coming in (Fake Out and First
    /// Impression work at 0). Absent: 1 on the field, 0 on the bench.
    #[serde(skip_serializing_if = "is_default")]
    pub move_actions: Option<u8>,
    /// The move it last used since coming in, and where it aimed it.
    #[serde(skip_serializing_if = "is_default")]
    pub last_move: String,
    /// Last publicly attempted move; absent in legacy state files.
    #[serde(skip_serializing_if = "is_default")]
    pub last_attempted_move: Option<String>,
    /// Bit mask of move slots whose PP the player is not currently told.
    #[serde(skip_serializing_if = "is_default")]
    pub unknown_pp: u8,
    #[serde(skip_serializing_if = "is_default")]
    pub base_unknown_pp: u8,
    #[serde(skip_serializing_if = "is_default")]
    pub last_move_target: i8,
    #[serde(skip_serializing_if = "is_default")]
    pub move_this_turn: MoveOutcome,
    #[serde(skip_serializing_if = "is_default")]
    pub move_last_turn: MoveOutcome,
    /// Hits taken from moves since it came in (Rage Fist).
    #[serde(skip_serializing_if = "is_default")]
    pub times_attacked: u16,
    /// Something has attacked it since it came in, and the damage of the latest attack.
    #[serde(skip_serializing_if = "is_default")]
    pub was_attacked: bool,
    #[serde(skip_serializing_if = "is_default")]
    pub last_attack_damage: i32,
    /// HP left after it last took damage this turn; 0 if unhurt.
    #[serde(skip_serializing_if = "is_default")]
    pub hurt_this_turn: u16,
    #[serde(skip_serializing_if = "is_default")]
    pub stats_raised_this_turn: bool,
    #[serde(skip_serializing_if = "is_default")]
    pub stats_lowered_this_turn: bool,
    /// Who has damaged it this turn.
    #[serde(skip_serializing_if = "is_default")]
    pub hit_by_this_turn: Vec<MonId>,
    /// Damaging hits from foes still on the field, oldest first.
    #[serde(skip_serializing_if = "is_default")]
    pub damaged_by: Vec<HitState>,
    /// Bookkeeping: whether it counts as on the field. Absent: in one of the first two places and not fainted.
    #[serde(skip_serializing_if = "is_default")]
    pub is_active: Option<bool>,
    /// Bookkeeping: its fainting has been dealt with or is about to be. Absent: as `fainted`.
    #[serde(skip_serializing_if = "is_default")]
    pub faint_queued: Option<bool>,
    /// Bookkeeping: its speed as last cached.
    #[serde(skip_serializing_if = "is_default")]
    pub speed: i32,
    /// Bookkeeping: cannot switch at the coming request.
    #[serde(skip_serializing_if = "is_default")]
    pub trapped: Trapped,
    /// Bookkeeping: the move it must use at the coming request.
    #[serde(skip_serializing_if = "is_default")]
    pub locked_move: String,
    /// Bookkeeping: switch-related flags that are only set while a switch is under way.
    #[serde(skip_serializing_if = "is_default")]
    pub force_switch_flag: bool,
    #[serde(skip_serializing_if = "is_default")]
    pub skip_before_switch_out: bool,
    #[serde(skip_serializing_if = "is_default")]
    pub being_called_back: bool,
    /// Bookkeeping: its number in the order the team was brought. Absent: its place in the list.
    #[serde(skip_serializing_if = "is_default")]
    pub team_index: Option<u8>,
    /// What the other player has been shown of it, as of when it was last
    /// off the field. Absent: nothing, except that a Pokémon on the field has
    /// been seen.
    #[serde(skip_serializing_if = "is_default")]
    pub shown: Option<ShownState>,
    /// The same for its present stay on the field, which is where anything
    /// shown now is noted (under Illusion, as the Pokémon it passes for).
    /// Absent: as `shown`.
    #[serde(skip_serializing_if = "is_default")]
    pub shown_now: Option<ShownState>,
}

/// What a battle has shown of one Pokémon: what the other player knows about
/// it. See [`crate::shown`].
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShownState {
    /// Its place in the order its side's Pokémon first appeared: 1, 2, ...
    /// 0: not seen yet (a Pokémon on the field is given the next number).
    #[serde(skip_serializing_if = "is_default")]
    pub id: u8,
    /// The species it was when last on the field under its own name. Empty: its species now.
    #[serde(skip_serializing_if = "is_default")]
    pub species: String,
    /// Moves it has been seen to have.
    #[serde(skip_serializing_if = "is_default")]
    pub moves: Vec<String>,
    /// Its item, if shown; empty for "holds nothing".
    #[serde(skip_serializing_if = "is_default")]
    pub item: Option<String>,
    /// The item it was shown to lose, when it holds nothing.
    #[serde(skip_serializing_if = "is_default")]
    pub item_lost: String,
    /// The ability it has now, and the one it returns to off the field, if shown.
    #[serde(skip_serializing_if = "is_default")]
    pub ability: Option<String>,
    #[serde(skip_serializing_if = "is_default")]
    pub base_ability: Option<String>,
    /// Its ability has been replaced since it came in.
    #[serde(skip_serializing_if = "is_default")]
    pub ability_changed: bool,
    /// HP as a whole percentage, bar colour and status when it was last on the field. Absent: full and healthy.
    #[serde(skip_serializing_if = "is_default")]
    pub hp: Option<u8>,
    #[serde(skip_serializing_if = "is_default")]
    pub bar: Bar,
    #[serde(skip_serializing_if = "is_default")]
    pub status: String,
    #[serde(skip_serializing_if = "is_default")]
    pub fainted: bool,
    /// On the field: the move it last borrowed (Copycat's pick), until it uses another.
    #[serde(skip_serializing_if = "is_default")]
    pub borrowed: String,
    /// The entry of its side's registered team ([`SideState::roster`]) it
    /// appears to be; -1 for none. Absent: its own, once it has been seen.
    #[serde(skip_serializing_if = "is_default")]
    pub listed: Option<i8>,
    /// On the field, and it may be the side's Illusion Pokémon in this shape
    /// (see [`crate::shown::ShownMon::maybe_disguise`]).
    #[serde(skip_serializing_if = "is_default")]
    pub suspect: bool,
    /// It left the field while that was still open.
    #[serde(skip_serializing_if = "is_default")]
    pub tainted: bool,
    #[serde(default, skip_serializing_if = "is_default")]
    pub copied_gastro: bool,
}

/// One Pokémon of the team a side registered: what Team Preview showed and,
/// with open team sheets, what its sheet says.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ListedState {
    pub species: String,
    /// "M", "F" or "N". Absent: the species' fixed gender, or male.
    #[serde(skip_serializing_if = "is_default")]
    pub gender: Option<String>,
    #[serde(skip_serializing_if = "is_default")]
    pub item: String,
    #[serde(skip_serializing_if = "is_default")]
    pub ability: String,
    #[serde(skip_serializing_if = "is_default")]
    pub moves: Vec<String>,
    #[serde(skip_serializing_if = "is_default")]
    pub nature: String,
    /// Its place in [`SideState::pokemon`] if it was brought.
    #[serde(skip_serializing_if = "is_default")]
    pub brought: Option<u8>,
    /// With closed sheets: moves the other side has worked out it knows without
    /// seeing it use them (see [`crate::shown::ListedMon::told`]).
    #[serde(skip_serializing_if = "is_default")]
    pub told: Vec<String>,
}

impl ShownState {
    fn of(rec: &Shown) -> ShownState {
        let view = ShownMon::from_record(rec, 0, Gender::N);
        ShownState {
            id: rec.seen,
            species: if rec.species == NO_SPECIES {
                String::new()
            } else {
                SPECIES[rec.species as usize].id.to_string()
            },
            moves: view.moves,
            item: view.item,
            item_lost: view.item_lost,
            ability: view.ability,
            base_ability: view.base_ability,
            ability_changed: rec.ability_changed,
            hp: Some(rec.hp),
            bar: rec.bar,
            status: view.status,
            fainted: rec.fainted,
            borrowed: move_name(rec.borrowed),
            listed: Some(if rec.listed == NOT_LISTED { -1 } else { rec.listed as i8 }),
            suspect: rec.suspect,
            tainted: rec.tainted,
            copied_gastro: rec.copied_gastro,
        }
    }

    fn record(&self) -> Result<Shown, Error> {
        let mut rec = NOTHING_SHOWN;
        rec.seen = self.id;
        if !self.species.is_empty() {
            rec.species = find_species(&self.species)?;
        }
        if self.moves.len() > SHOWN_MOVES {
            return Err(bad(format!("at most {SHOWN_MOVES} moves can have been shown of one Pokémon")));
        }
        for m in &self.moves {
            rec.add_move(find_move(m)?);
        }
        rec.item = match &self.item {
            None => ItemShown::Unknown,
            Some(i) if !i.is_empty() => ItemShown::Holds(find_item(i)?),
            Some(_) if !self.item_lost.is_empty() => ItemShown::Lost(find_item(&self.item_lost)?),
            Some(_) => return Err(bad("a Pokémon shown to hold nothing was shown to lose something: item_lost")),
        };
        let ability = |name: &Option<String>| name.as_ref().map_or(Ok(UNKNOWN), |a| find_ability(a));
        rec.ability = ability(&self.ability)?;
        rec.base_ability = ability(&self.base_ability)?;
        rec.ability_changed = self.ability_changed;
        rec.hp = self.hp.unwrap_or(100);
        rec.bar = self.bar;
        rec.status = STATUSES
            .into_iter()
            .find(|st| st.id() == self.status)
            .ok_or_else(|| bad(format!("unknown status {}", self.status)))?;
        rec.fainted = self.fainted;
        rec.listed = match self.listed {
            Some(j) if (0..MAX_ROSTER as i8).contains(&j) => j as u8,
            Some(-1) | None => NOT_LISTED,
            Some(j) => return Err(bad(format!("no entry {j} in a registered team"))),
        };
        rec.borrowed = find_move_or_none(&self.borrowed)?;
        rec.suspect = self.suspect;
        rec.tainted = self.tainted;
        rec.copied_gastro = self.copied_gastro;
        Ok(rec)
    }
}

impl PokemonState {
    /// A healthy Pokémon with these moves, no ability, no item and a neutral nature.
    pub fn new(species: &str, moves: &[&str]) -> PokemonState {
        PokemonState {
            species: species.to_string(),
            moves: moves.iter().map(|m| MoveState::new(m)).collect(),
            ..Default::default()
        }
    }
    /// The state of a Pokémon that has just been brought, from its set.
    pub fn from_set(set: &PokemonSet) -> PokemonState {
        PokemonState {
            species: SPECIES[set.species as usize].id.to_string(),
            moves: set.moves.iter().map(|&m| MoveState::new(MOVES[m as usize].id)).collect(),
            nature: nature_name(set.nature).to_string(),
            stat_points: set.stat_points,
            gender: Some(set.gender.id().to_string()),
            ability: ABILITIES[set.ability as usize].id.to_string(),
            item: ITEMS[set.item as usize].id.to_string(),
            ..Default::default()
        }
    }
    pub fn ability(mut self, ability: &str) -> PokemonState {
        self.ability = ability.to_string();
        self
    }
    pub fn item(mut self, item: &str) -> PokemonState {
        self.item = item.to_string();
        self
    }
    pub fn nature(mut self, nature: &str, stat_points: [u8; 6]) -> PokemonState {
        self.nature = nature.to_string();
        self.stat_points = stat_points;
        self
    }
    pub fn status(mut self, status: &str) -> PokemonState {
        self.status = status.to_string();
        self
    }
    pub fn volatile(mut self, cond: CondState) -> PokemonState {
        self.volatiles.push(cond);
        self
    }
}

/// One side of the field.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SideState {
    /// The Pokémon it brought, as they are placed now: the first two are on
    /// the field (left, then right), the rest on the bench in party order. A
    /// fainted Pokémon with no replacement stays in its place.
    pub pokemon: Vec<PokemonState>,
    /// Tailwind, screens, hazards and the like.
    #[serde(skip_serializing_if = "is_default")]
    pub conditions: Vec<CondState>,
    /// Conditions on the left and right positions (Wish, a Future Sight on its way).
    #[serde(skip_serializing_if = "is_default")]
    pub slot_conditions: [Vec<CondState>; ACTIVE],
    /// How many of its Pokémon have fainted in all (Last Respects, Supreme
    /// Overlord). Absent: those that are fainted now.
    #[serde(skip_serializing_if = "is_default")]
    pub total_fainted: Option<u8>,
    #[serde(skip_serializing_if = "is_default")]
    pub fainted_this_turn: bool,
    /// One of its Pokémon fainted last turn (Retaliate).
    #[serde(skip_serializing_if = "is_default")]
    pub fainted_last_turn: bool,
    /// Bookkeeping. Absent: those that are not fainted.
    #[serde(skip_serializing_if = "is_default")]
    pub pokemon_left: Option<u8>,
    /// The team it registered, in the order registered: what Team Preview
    /// showed the other player, and what its sheet says if the sheets are
    /// open ([`BattleState::open_team_sheets`]). It matters only to what each
    /// side has been shown. Absent: the Pokémon in `pokemon`, as they are now.
    #[serde(skip_serializing_if = "is_default")]
    pub roster: Vec<ListedState>,
}

impl SideState {
    pub fn new(pokemon: Vec<PokemonState>) -> SideState {
        SideState { pokemon, ..Default::default() }
    }
}

/// An action still to run this turn, at a switch request in the middle of one.
///
/// By hand, list the moves still to come: who (`pokemon`), what (`move_id`)
/// and where (`target`, as in a choice). They are put in order as if just
/// chosen, and the end of the turn is added after them. If only the end of
/// the turn is left (the last Pokémon to move used U-turn), list that:
/// [`ActionState::end_of_turn`]. No pending actions at all means the turn is
/// over, and the request is for replacing the fainted.
///
/// What goes with a move at the start of a turn (a Mega Evolution, Focus
/// Punch's focusing) is taken to have happened already. An exported position
/// lists everything left in the queue, with the ordering already worked out
/// in `resolved`.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ActionState {
    /// "move" unless said otherwise. An exported queue also has "residual"
    /// (the end of the turn) and may have "megaevo", "switch" and others.
    #[serde(skip_serializing_if = "is_default")]
    pub kind: String,
    #[serde(skip_serializing_if = "is_default")]
    pub pokemon: Option<MonId>,
    #[serde(skip_serializing_if = "is_default")]
    pub move_id: String,
    /// Target location as in a choice: 1 and 2 are the foe's positions, -1 and -2 the user's side.
    #[serde(skip_serializing_if = "is_default")]
    pub target: i8,
    /// The Pokémon coming in, for a switch.
    #[serde(skip_serializing_if = "is_default")]
    pub switch_to: Option<MonId>,
    /// Bookkeeping: where the action stands in the queue. Absent: worked out as if just chosen.
    #[serde(skip_serializing_if = "is_default")]
    pub resolved: Option<Resolved>,
}

impl ActionState {
    /// A move still to be made this turn.
    pub fn mv(side: u8, pokemon: u8, move_id: &str, target: i8) -> ActionState {
        ActionState {
            kind: "move".into(),
            pokemon: Some(MonId { side, pokemon }),
            move_id: to_id(move_id),
            target,
            ..Default::default()
        }
    }
    /// The end of the turn (weather damage, Leftovers and so on) is still to come.
    pub fn end_of_turn() -> ActionState {
        ActionState { kind: "residual".into(), ..Default::default() }
    }
}

/// The ordering values of a queued action (Showdown's `resolveAction`).
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Resolved {
    pub order: u32,
    pub priority: i32,
    pub speed: i32,
    pub fractional_priority: i8,
    pub move_priority: i8,
    pub prankster: bool,
    pub original_target: Option<MonId>,
    pub self_target: bool,
    /// An Encore replacement discarded the originally selected target.
    pub target_replaced: bool,
    /// Called forward by a Round; the value is that Round's `ignoreAbility`.
    pub round: Option<bool>,
    /// A switch caused by this move of the Pokémon leaving.
    pub self_switch: String,
}

/// A whole position. See the [module documentation](self).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BattleState {
    /// The turn about to be played (1 for the first).
    pub turn: u16,
    /// The random number generator's state, as Showdown's four seed words.
    pub seed: [u16; 4],
    /// What the battle is waiting for.
    pub request: Request,
    pub sides: [SideState; 2],
    #[serde(skip_serializing_if = "is_default")]
    pub weather: Option<CondState>,
    #[serde(skip_serializing_if = "is_default")]
    pub terrain: Option<CondState>,
    /// Trick Room, Gravity and the other conditions on the whole field.
    #[serde(skip_serializing_if = "is_default")]
    pub pseudo_weather: Vec<CondState>,
    /// The last move anyone used (Copycat).
    #[serde(skip_serializing_if = "is_default")]
    pub last_move: String,
    /// At a switch request in the middle of a turn: what is still to run.
    #[serde(skip_serializing_if = "is_default")]
    pub pending: Vec<ActionState>,
    #[serde(skip_serializing_if = "is_default")]
    pub ended: bool,
    #[serde(skip_serializing_if = "is_default")]
    pub winner: Option<u8>,
    /// The players have each other's team sheets (see [`Battle::with_rosters`]).
    #[serde(skip_serializing_if = "is_default")]
    pub open_team_sheets: bool,
    /// Whether the bookkeeping fields are filled in (an exported position). If
    /// not, `from_state` works them out: disabled moves, trapping, locked
    /// moves, cached speeds and the order effects started in.
    #[serde(skip_serializing_if = "is_default")]
    pub prepared: bool,
    /// Bookkeeping: the effect-order counter.
    #[serde(skip_serializing_if = "is_default")]
    pub effect_order: u32,
    /// Bookkeeping: field positions in the speed order fixed at the last switch-in.
    #[serde(skip_serializing_if = "is_default")]
    pub speed_order: Vec<u8>,
    /// Bookkeeping: in the middle of a turn. Absent: at a switch request.
    #[serde(skip_serializing_if = "is_default")]
    pub mid_turn: Option<bool>,
    /// Bookkeeping: random numbers drawn so far.
    #[serde(skip_serializing_if = "is_default")]
    pub rng_calls: u32,
}

impl Default for BattleState {
    fn default() -> Self {
        BattleState {
            turn: 1,
            seed: [1, 2, 3, 4],
            request: Request::Move,
            sides: Default::default(),
            weather: None,
            terrain: None,
            pseudo_weather: Vec::new(),
            last_move: String::new(),
            pending: Vec::new(),
            ended: false,
            winner: None,
            open_team_sheets: false,
            prepared: false,
            effect_order: 0,
            speed_order: Vec::new(),
            mid_turn: None,
            rng_calls: 0,
        }
    }
}

impl BattleState {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("a position is always serialisable")
    }
    pub fn from_json(text: &str) -> Result<BattleState, Error> {
        serde_json::from_str(text).map_err(|e| bad(e.to_string()))
    }
    /// The same position without the simulator's bookkeeping: what a
    /// hand-written description of it would say. `from_state` works the
    /// rest out again.
    pub fn without_bookkeeping(&self) -> BattleState {
        let mut s = self.clone();
        s.prepared = false;
        s.effect_order = 0;
        s.speed_order.clear();
        s.mid_turn = None;
        s.rng_calls = 0;
        let strip = |c: &mut CondState| {
            c.order = 0;
            c.targeted = false;
        };
        s.weather.iter_mut().for_each(strip);
        s.terrain.iter_mut().for_each(strip);
        s.pseudo_weather.iter_mut().for_each(strip);
        for side in &mut s.sides {
            side.pokemon_left = None;
            side.conditions.iter_mut().for_each(strip);
            // (For a condition on a position `targeted` is not bookkeeping: see `CondState`.)
            side.slot_conditions.iter_mut().flatten().for_each(|c| c.order = 0);
            for (p, m) in side.pokemon.iter_mut().enumerate() {
                m.speed = 0;
                m.trapped = Trapped::No;
                m.locked_move.clear();
                m.is_active = None;
                m.faint_queued = None;
                if m.fainted {
                    // (That the fainted are to be replaced goes without saying.)
                    m.switch_flag = false;
                    m.switch_move.clear();
                }
                m.status_extra.order = 0;
                m.ability_extra.order = 0;
                m.item_extra.order = 0;
                m.volatiles.iter_mut().for_each(strip);
                for mv in m.moves.iter_mut().chain(m.base_moves.iter_mut()) {
                    mv.disabled = false;
                    mv.hidden = false;
                }
                // (The numbering of the team is kept: it is part of how sources are named.)
                let _ = p;
            }
        }
        if s.ended {
            s.pending.clear();
        }
        // Moves and the end of the turn are listed as they would be by hand.
        // Anything else that is queued stays as it is.
        for a in &mut s.pending {
            if a.kind == "move" || a.kind == "residual" {
                a.resolved = None;
            }
        }
        s
    }
}

// ------------------------------------------------------------------ names

const NATURES: [&str; 21] = [
    "hardy", "lonely", "brave", "adamant", "naughty", "bold", "relaxed", "impish", "lax", "timid", "hasty", "jolly",
    "naive", "modest", "mild", "quiet", "rash", "calm", "gentle", "sassy", "careful",
];

pub(crate) fn nature_name(n: (u8, u8)) -> &'static str {
    NATURES.iter().copied().find(|name| nature(name) == Some(n)).unwrap_or("hardy")
}

const TYPES: [Type; 18] = [
    Type::Normal,
    Type::Fighting,
    Type::Flying,
    Type::Poison,
    Type::Ground,
    Type::Rock,
    Type::Bug,
    Type::Ghost,
    Type::Steel,
    Type::Fire,
    Type::Water,
    Type::Grass,
    Type::Electric,
    Type::Psychic,
    Type::Ice,
    Type::Dragon,
    Type::Dark,
    Type::Fairy,
];

fn type_name(t: Type) -> String {
    match t {
        Type::None => String::new(),
        Type::Typeless => "???".to_string(),
        t => format!("{t:?}"),
    }
}

fn parse_type(name: &str) -> Result<Type, Error> {
    match name {
        "" => Ok(Type::None),
        "???" => Ok(Type::Typeless),
        _ => TYPES
            .iter()
            .copied()
            .find(|t| format!("{t:?}").eq_ignore_ascii_case(name))
            .ok_or_else(|| bad(format!("unknown type {name}"))),
    }
}

const STATUSES: [Status; 7] =
    [Status::None, Status::Brn, Status::Par, Status::Psn, Status::Tox, Status::Slp, Status::Frz];

fn find_species(name: &str) -> Result<u16, Error> {
    species_id(&to_id(name)).ok_or_else(|| bad(format!("unknown species {name}")))
}
fn find_move(name: &str) -> Result<u16, Error> {
    move_id(&to_id(name)).ok_or_else(|| bad(format!("unknown move {name}")))
}
fn find_ability(name: &str) -> Result<u16, Error> {
    if name.is_empty() {
        return Ok(ab::NOABILITY);
    }
    let id = ability_id(&to_id(name)).ok_or_else(|| bad(format!("unknown ability {name}")))?;
    if !ABILITIES[id as usize].supported {
        return Err(Error::Unsupported(format!("ability {}", ABILITIES[id as usize].name)));
    }
    Ok(id)
}
fn find_item(name: &str) -> Result<u16, Error> {
    let id = item_id(&to_id(name)).ok_or_else(|| bad(format!("unknown item {name}")))?;
    if !ITEMS[id as usize].supported {
        return Err(Error::Unsupported(format!("item {}", ITEMS[id as usize].name)));
    }
    Ok(id)
}
fn ability_name(a: u16) -> String {
    if a == ab::NOABILITY { String::new() } else { ABILITIES[a as usize].id.to_string() }
}
fn move_name(m: u16) -> String {
    if m == NO_MOVE { String::new() } else { MOVES[m as usize].id.to_string() }
}
fn find_move_or_none(name: &str) -> Result<u16, Error> {
    if name.is_empty() { Ok(NO_MOVE) } else { find_move(name) }
}

/// The species a Mega was before it evolved, if `species` is a Mega.
fn mega_origin(species: u16) -> Option<u16> {
    ITEMS.iter().flat_map(|i| i.mega.iter()).find(|&&(_, to)| to == species).map(|&(from, _)| from)
}

/// Formes that last only while the Pokémon is on the field, and what they revert to.
const PASSING_FORMES: [(&str, &str); 2] = [("aegislashblade", "aegislash"), ("morpekohangry", "morpeko")];
/// Formes that last for the battle, and the species brought.
const LASTING_FORMES: [(&str, &str); 2] = [("mimikyubusted", "mimikyu"), ("palafinhero", "palafin")];

fn forme_origin(table: &[(&str, &str)], species: u16) -> Option<u16> {
    let id = SPECIES[species as usize].id;
    table.iter().find(|(forme, _)| *forme == id).and_then(|(_, base)| species_id(base))
}

/// The kinds of condition, by the enum that names them.
trait Kind: Copy + PartialEq {
    fn kind_id(self) -> &'static str;
    fn kind_named(id: &str) -> Option<Self>;
    fn kind_data(self) -> &'static CondData;
    /// Whether the condition's `data` is a move (table index plus one).
    fn holds_move(self) -> bool {
        false
    }
}

macro_rules! kind {
    ($t:ty) => {
        impl Kind for $t {
            fn kind_id(self) -> &'static str {
                self.id()
            }
            fn kind_named(id: &str) -> Option<Self> {
                <$t>::named(id)
            }
            fn kind_data(self) -> &'static CondData {
                self.data()
            }
        }
    };
}
kind!(SideCond);
kind!(SlotCond);
kind!(Pseudo);
kind!(Weather);
kind!(Terrain);

impl Kind for VolKind {
    fn kind_id(self) -> &'static str {
        self.id()
    }
    fn kind_named(id: &str) -> Option<Self> {
        VolKind::named(id)
    }
    fn kind_data(self) -> &'static CondData {
        self.data()
    }
    fn holds_move(self) -> bool {
        matches!(
            self,
            VolKind::Choicelock
                | VolKind::Metronome
                | VolKind::Encore
                | VolKind::Disable
                | VolKind::Lockedmove
                | VolKind::Twoturnmove
        )
    }
}

const ACTION_KINDS: [(ActKind, &str); 12] = [
    (ActKind::Team, "team"),
    (ActKind::Start, "start"),
    (ActKind::InstaSwitch, "instaswitch"),
    (ActKind::BeforeTurn, "beforeturn"),
    (ActKind::Revival, "revival"),
    (ActKind::BeforeTurnMove, "beforeturnmove"),
    (ActKind::RunSwitch, "runswitch"),
    (ActKind::Switch, "switch"),
    (ActKind::MegaEvo, "megaevo"),
    (ActKind::PriorityCharge, "prioritycharge"),
    (ActKind::Move, "move"),
    (ActKind::Residual, "residual"),
];

fn outcome(r: Res) -> MoveOutcome {
    match r {
        Res::Undef => MoveOutcome::None,
        Res::Bool(true) => MoveOutcome::Succeeded,
        Res::Bool(false) => MoveOutcome::Failed,
        _ => MoveOutcome::Null,
    }
}

fn outcome_res(o: MoveOutcome) -> Res {
    match o {
        MoveOutcome::None => Res::Undef,
        MoveOutcome::Null => Res::Null,
        MoveOutcome::Failed => FALSE,
        MoveOutcome::Succeeded => TRUE,
    }
}

// ----------------------------------------------------------------- export

impl Battle {
    fn mon_id(&self, r: MonRef) -> MonId {
        MonId { side: r.side, pokemon: self.mon(r).position }
    }

    fn cond_state<K: Kind>(&self, c: &Cond<K>) -> CondState {
        let holds_move = c.kind.holds_move();
        CondState {
            id: c.kind.kind_id().to_string(),
            turns: Some(c.duration),
            value: if holds_move { 0 } else { c.data },
            move_id: if holds_move && c.data > 0 { MOVES[c.data as usize - 1].id.to_string() } else { String::new() },
            source: c.source.map(|s| self.mon_id(s)),
            source_slot: match (c.source, c.source_slot) {
                (None, NO_SLOT) => None,
                (_, NO_SLOT) => Some(-1),
                (_, slot) => Some(slot as i8),
            },
            a: c.st.a,
            b: c.st.b,
            order: c.st.order,
            targeted: c.targeted,
        }
    }

    fn move_states(slots: &[MoveSlot]) -> Vec<MoveState> {
        slots
            .iter()
            .map(|s| MoveState {
                id: MOVES[s.id as usize].id.to_string(),
                pp: Some(s.pp),
                max_pp: Some(s.maxpp),
                used: s.used,
                disabled: s.disabled,
                hidden: s.hidden,
            })
            .collect()
    }

    fn pokemon_state(&self, r: MonRef) -> PokemonState {
        let m = self.mon(r);
        let foe = 1 - r.side as usize;
        let on_field = (m.position as usize) < ACTIVE;
        let mut hit_by = Vec::new();
        for side in 0..2u8 {
            for idx in 0..self.sides[side as usize].n {
                if m.hit_by_this_turn & (1 << (side as usize * MAX_TEAM + idx as usize)) != 0 {
                    hit_by.push(self.mon_id(MonRef { side, idx }));
                }
            }
        }
        let ability_item = if m.ability == ab::CUDCHEW && m.ability_st.a > 0 {
            ITEMS[m.ability_st.a as usize].id.to_string()
        } else {
            String::new()
        };
        PokemonState {
            species: SPECIES[m.species as usize].id.to_string(),
            base_species: Some(SPECIES[m.base_species as usize].id.to_string()),
            original_species: Some(SPECIES[m.set_species as usize].id.to_string()),
            nature: nature_name(m.nature).to_string(),
            stat_points: m.stat_points,
            gender: Some(m.gender.id().to_string()),
            stats: Some(m.stats),
            hp: Some(m.hp),
            hp_percent: None,
            fainted: m.fainted,
            status: m.status.id().to_string(),
            status_turns: m.status_time,
            toxic_stage: m.tox_stage,
            status_extra: Extra { order: m.status_st.order, a: m.status_st.a, b: m.status_st.b, item: String::new() },
            boosts: m.boosts,
            moves: Battle::move_states(&m.moves[..m.n_moves as usize]),
            transformed: m.transformed,
            base_moves: if m.transformed {
                Battle::move_states(&m.base_moves[..m.base_n_moves as usize])
            } else {
                Vec::new()
            },
            illusion: m.illusion.checked_sub(1).map(|idx| self.sides[r.side as usize].team[idx as usize].position),
            can_mega: Some(if m.can_mega == NO_SPECIES {
                String::new()
            } else {
                SPECIES[m.can_mega as usize].id.to_string()
            }),
            types: Some(m.types.iter().filter(|&&t| t != Type::None).map(|&t| type_name(t)).collect()),
            added_type: type_name(m.added_type),
            types_said: (m.said != TypesSaid::fresh(m.types)).then(|| {
                let names =
                    |types: [Type; 2]| types.iter().filter(|&&t| t != Type::None).map(|&t| type_name(t)).collect();
                TypesSaidState {
                    seen: m.said.seen.map(names),
                    apparent: names(m.said.apparent),
                    known: m.said.known,
                    copied: m.said.copied,
                    copied_foe: m.said.copied_foe,
                    told: m.said.told,
                    before: names(m.said.before),
                    attacked_by: (0..2 * MAX_TEAM)
                        .filter(|bit| m.said.attacked_by & 1 << bit != 0)
                        .map(|bit| MonId {
                            side: (bit / MAX_TEAM) as u8,
                            pokemon: self.sides[bit / MAX_TEAM].team[bit % MAX_TEAM].position,
                        })
                        .collect(),
                }
            }),
            ability: ability_name(m.ability),
            base_ability: Some(ability_name(m.base_ability)),
            ability_extra: Extra {
                order: m.ability_st.order,
                a: if ability_item.is_empty() { m.ability_st.a } else { 0 },
                b: m.ability_st.b,
                item: ability_item,
            },
            ability_boosts: m.ability_boosts,
            syrup_triggered: m.syrup_triggered,
            item: ITEMS[m.item as usize].id.to_string(),
            item_extra: Extra { order: m.item_st.order, a: m.item_st.a, b: m.item_st.b, item: String::new() },
            last_item: ITEMS[m.last_item as usize].id.to_string(),
            used_item_this_turn: m.used_item_this_turn,
            ate_berry: m.ate_berry,
            volatiles: if on_field {
                self.vols[r.side as usize][m.position as usize].as_slice().iter().map(|v| self.cond_state(v)).collect()
            } else {
                Vec::new()
            },
            switch_flag: m.switch_flag,
            switch_move: move_name(m.switch_move),
            active_turns: Some(m.active_turns),
            newly_switched: m.newly_switched,
            move_actions: Some(m.active_move_actions),
            last_move: move_name(m.last_move),
            last_attempted_move: m.last_attempted_move.map(move_name),
            unknown_pp: m.unknown_pp,
            base_unknown_pp: m.base_unknown_pp,
            last_move_target: m.last_move_loc,
            move_this_turn: outcome(m.move_this_turn),
            move_last_turn: outcome(m.move_last_turn),
            times_attacked: m.times_attacked,
            was_attacked: m.was_attacked,
            last_attack_damage: m.last_attack_damage,
            hurt_this_turn: m.hurt_this_turn,
            stats_raised_this_turn: m.stats_raised_this_turn,
            stats_lowered_this_turn: m.stats_lowered_this_turn,
            hit_by_this_turn: hit_by,
            damaged_by: m.damaged_by[..m.n_damaged_by as usize]
                .iter()
                .map(|d| HitState {
                    pokemon: self.sides[foe].team[d.idx as usize].position,
                    slot: (d.slot != NO_SLOT).then_some(d.slot),
                    damage: d.damage,
                    this_turn: d.this_turn,
                })
                .collect(),
            is_active: Some(m.is_active),
            faint_queued: Some(m.faint_queued),
            speed: m.speed,
            trapped: m.trapped,
            locked_move: move_name(m.locked_move),
            force_switch_flag: m.force_switch_flag,
            skip_before_switch_out: m.skip_before_switch_out,
            being_called_back: m.being_called_back,
            team_index: Some(r.idx),
            shown: {
                let rec = &self.sides[r.side as usize].shown[r.idx as usize];
                (*rec != NOTHING_SHOWN).then(|| ShownState::of(rec))
            },
            shown_now: (m.is_active && m.live.seen != 0).then(|| ShownState::of(&m.live)),
        }
    }

    /// Writes the position down. The battle must be waiting for choices (or over).
    ///
    /// A battle built back from the result with [`Battle::from_state`]
    /// continues exactly as this one would.
    pub fn to_state(&self) -> BattleState {
        assert!(
            self.ended || self.request != Request::None,
            "a position can only be taken while the battle waits for choices"
        );
        let side_state = |s: usize| {
            let side = &self.sides[s];
            SideState {
                pokemon: (0..side.n as usize)
                    .map(|p| self.pokemon_state(MonRef { side: s as u8, idx: side.order[p] }))
                    .collect(),
                conditions: side.conds.as_slice().iter().map(|c| self.cond_state(c)).collect(),
                slot_conditions: std::array::from_fn(|p| {
                    side.slot_conds[p].as_slice().iter().map(|c| self.cond_state(c)).collect()
                }),
                total_fainted: Some(side.total_fainted),
                fainted_this_turn: side.fainted_this_turn,
                fainted_last_turn: side.fainted_last_turn,
                pokemon_left: Some(side.pokemon_left),
                roster: side.roster[..side.n_roster as usize]
                    .iter()
                    .map(|l| ListedState {
                        species: SPECIES[l.species as usize].id.to_string(),
                        gender: Some(l.gender.id().to_string()),
                        item: ITEMS[l.item as usize].id.to_string(),
                        ability: ABILITIES[l.ability as usize].id.to_string(),
                        moves: l.moves().iter().map(|&m| MOVES[m as usize].id.to_string()).collect(),
                        nature: nature_name(l.nature).to_string(),
                        brought: (l.brought != NOT_LISTED).then(|| side.team[l.brought as usize].position),
                        told: l.told().iter().map(|&m| MOVES[m as usize].id.to_string()).collect(),
                    })
                    .collect(),
            }
        };
        let pending = self
            .queue
            .as_slice()
            .iter()
            .map(|a| ActionState {
                kind: ACTION_KINDS.iter().find(|(k, _)| *k == a.kind).map(|(_, n)| n.to_string()).unwrap_or_default(),
                pokemon: a.mon.map(|r| self.mon_id(r)),
                move_id: if matches!(a.kind, ActKind::Move | ActKind::BeforeTurnMove | ActKind::PriorityCharge) {
                    MOVES[a.move_id as usize].id.to_string()
                } else {
                    String::new()
                },
                target: a.target_loc,
                switch_to: a.switch_to.map(|r| self.mon_id(r)),
                resolved: Some(Resolved {
                    order: a.order,
                    priority: a.priority,
                    speed: a.speed,
                    fractional_priority: a.frac,
                    move_priority: a.move_priority,
                    prankster: a.prankster,
                    original_target: a.orig_target.map(|r| self.mon_id(r)),
                    self_target: a.self_target,
                    target_replaced: a.target_replaced,
                    round: match a.source {
                        ActSource::Round { ignore_ability } => Some(ignore_ability),
                        _ => None,
                    },
                    self_switch: match a.source {
                        ActSource::SelfSwitch(m) => MOVES[m as usize].id.to_string(),
                        _ => String::new(),
                    },
                }),
            })
            .collect();
        BattleState {
            turn: self.turn,
            seed: self.rng.words(),
            request: self.request,
            sides: [side_state(0), side_state(1)],
            weather: (self.field.weather.kind != Weather::None).then(|| self.cond_state(&self.field.weather)),
            terrain: (self.field.terrain.kind != Terrain::None).then(|| self.cond_state(&self.field.terrain)),
            pseudo_weather: self.field.pseudo.as_slice().iter().map(|c| self.cond_state(c)).collect(),
            last_move: move_name(self.last_move),
            pending,
            ended: self.ended,
            winner: self.winner,
            open_team_sheets: self.open_team_sheets,
            prepared: true,
            effect_order: self.effect_order,
            speed_order: self.speed_order[..self.n_speed_order as usize].to_vec(),
            mid_turn: Some(self.mid_turn),
            rng_calls: self.rng.calls,
        }
    }
}

// ----------------------------------------------------------------- import

/// Turns the names in a position into the engine's references.
struct Names {
    /// `order[side][place in the list]` is the team index.
    order: [[u8; MAX_TEAM]; 2],
    n: [usize; 2],
}

impl Names {
    fn mon(&self, id: MonId) -> Result<MonRef, Error> {
        if id.side > 1 || id.pokemon as usize >= self.n[id.side as usize] {
            return Err(bad(format!("no Pokémon {} on side {}", id.pokemon, id.side)));
        }
        Ok(MonRef { side: id.side, idx: self.order[id.side as usize][id.pokemon as usize] })
    }
    fn mon_opt(&self, id: Option<MonId>) -> Result<Option<MonRef>, Error> {
        id.map(|i| self.mon(i)).transpose()
    }
}

impl Battle {
    fn cond_from<K: Kind>(&mut self, names: &Names, c: &CondState, what: &str) -> Result<Cond<K>, Error> {
        let kind = K::kind_named(&to_id(&c.id)).ok_or_else(|| bad(format!("unknown {what} {}", c.id)))?;
        let data = kind.kind_data();
        let mut out = Cond::new(kind);
        out.duration = c.turns.unwrap_or(data.duration);
        out.data = if kind.holds_move() {
            if c.move_id.is_empty() && kind.kind_id() != "metronome" {
                return Err(bad(format!("{} needs move_id", c.id)));
            }
            if c.move_id.is_empty() { 0 } else { find_move(&c.move_id)? + 1 }
        } else if c.value == 0 {
            // Left unsaid: what the condition starts with.
            match kind.kind_id() {
                "stall" | "allyswitch" | "confusion" => 3,
                "partiallytrapped" => 8,
                "helpinghand" | "stockpile" | "spikes" | "toxicspikes" => 1,
                _ => 0,
            }
        } else {
            c.value
        };
        // (A source on the bench has a slot past the field's four; Leech Seed's is looked up.)
        let slots = if kind.kind_id() == "leechseed" { 4 } else { 2 * MAX_TEAM as i8 };
        if c.source_slot.is_some_and(|slot| slot >= slots) {
            return Err(bad(format!("{}: source_slot is side * 2 + position", c.id)));
        }
        out.source = names.mon_opt(c.source)?;
        out.source_slot = match (c.source_slot, out.source) {
            (Some(slot), _) if slot < 0 => NO_SLOT,
            (Some(slot), _) => slot as u8,
            (None, Some(s)) if (self.mon(s).position as usize) < ACTIVE => self.field_slot(s),
            (None, _) => NO_SLOT,
        };
        out.targeted = c.targeted;
        out.st.order = c.order;
        out.st.a = c.a;
        out.st.b = c.b;
        self.listen(data.events, data.events_pre);
        Ok(out)
    }

    fn move_slots(list: &[MoveState], copied: bool) -> Result<([MoveSlot; MAX_MOVES], u8), Error> {
        if list.is_empty() || list.len() > MAX_MOVES {
            return Err(bad(format!("a Pokémon has 1 to {MAX_MOVES} moves")));
        }
        let mut out = [MoveSlot { id: 0, pp: 0, maxpp: 0, disabled: false, hidden: false, used: false }; MAX_MOVES];
        for (k, m) in list.iter().enumerate() {
            let id = find_move(&m.id)?;
            let d = &MOVES[id as usize];
            if !d.supported {
                return Err(Error::Unsupported(format!("move {}", d.name)));
            }
            let maxpp = m.max_pp.unwrap_or(if copied { d.base_pp.min(5) } else { d.pp });
            out[k] =
                MoveSlot { id, pp: m.pp.unwrap_or(maxpp), maxpp, disabled: m.disabled, hidden: m.hidden, used: m.used };
        }
        Ok((out, list.len() as u8))
    }

    /// The parts of a Pokémon that do not refer to anyone else.
    fn mon_from(&mut self, ps: &PokemonState, place: usize, side_has_mega: bool) -> Result<Pokemon, Error> {
        let species = find_species(&ps.species)?;
        let base_species = match &ps.base_species {
            Some(name) => find_species(name)?,
            None if ps.transformed => {
                return Err(bad(format!("{}: a transformed Pokémon needs base_species", ps.species)));
            }
            None => forme_origin(&PASSING_FORMES, species).unwrap_or(species),
        };
        let original = match &ps.original_species {
            Some(name) => find_species(name)?,
            None => mega_origin(base_species).or(forme_origin(&LASTING_FORMES, base_species)).unwrap_or(base_species),
        };
        let nature_pair = if ps.nature.is_empty() {
            (0, 0)
        } else {
            nature(&ps.nature).ok_or_else(|| bad(format!("unknown nature {}", ps.nature)))?
        };
        let ability = find_ability(&ps.ability)?;
        let base_ability = match &ps.base_ability {
            Some(name) => find_ability(name)?,
            None => ability,
        };
        let item = find_item(&ps.item)?;
        let gender = match &ps.gender {
            Some(g) => Gender::parse(g).ok_or_else(|| bad(format!("unknown gender {g}")))?,
            None => SPECIES[original as usize].gender.unwrap_or(Gender::M),
        };
        let own_moves = if ps.transformed { &ps.base_moves } else { &ps.moves };
        let set = PokemonSet {
            species: original,
            moves: own_moves.iter().map(|m| find_move(&m.id)).collect::<Result<Vec<_>, _>>()?,
            nature: nature_pair,
            stat_points: ps.stat_points,
            ability: base_ability,
            item,
            gender,
        };
        // (Validates the set, computes its stats and registers its listeners.)
        let mut mon = self.new_mon(&set, place)?;
        self.listen(ABILITIES[ability as usize].events, ABILITIES[ability as usize].events_pre);
        mon.species = species;
        mon.base_species = base_species;
        mon.transformed = ps.transformed;
        mon.can_mega = match &ps.can_mega {
            Some(name) if name.is_empty() => NO_SPECIES,
            Some(name) => find_species(name)?,
            None if side_has_mega || base_species != original => NO_SPECIES,
            None => mon.can_mega,
        };
        if mon.can_mega != NO_SPECIES {
            let a = &ABILITIES[SPECIES[mon.can_mega as usize].ability0 as usize];
            self.listen(a.events, a.events_pre);
        }
        mon.types = match &ps.types {
            Some(list) if list.len() <= 2 => {
                let mut t = [Type::None; 2];
                for (k, name) in list.iter().enumerate() {
                    t[k] = parse_type(name)?;
                }
                t
            }
            Some(_) => return Err(bad("a Pokémon has at most two types (and one added)")),
            None => SPECIES[species as usize].types,
        };
        mon.added_type = parse_type(&ps.added_type)?;
        mon.said = TypesSaid::fresh(mon.types);
        if let Some(said) = &ps.types_said {
            let pair = |names: &Vec<String>| -> Result<[Type; 2], Error> {
                let mut t = [Type::None; 2];
                for (k, name) in names.iter().take(2).enumerate() {
                    t[k] = parse_type(name)?;
                }
                Ok(t)
            };
            mon.said.seen = [pair(&said.seen[0])?, pair(&said.seen[1])?];
            mon.said.apparent = pair(&said.apparent)?;
            (mon.said.known, mon.said.copied) = (said.known, said.copied);
            (mon.said.copied_foe, mon.said.told) = (said.copied_foe, said.told);
            mon.said.before = pair(&said.before)?;
        }
        mon.stats = match ps.stats {
            Some(stats) if stats.contains(&0) => return Err(bad(format!("{}: a stat of 0", ps.species))),
            Some(stats) => stats,
            None => {
                let mut stats = calc_stats(species, nature_pair, ps.stat_points);
                stats[0] = calc_stats(base_species, nature_pair, ps.stat_points)[0];
                stats
            }
        };
        mon.fainted = ps.fainted || ps.hp == Some(0);
        mon.hp = if mon.fainted {
            0
        } else if let Some(hp) = ps.hp {
            hp
        } else if let Some(pct) = ps.hp_percent {
            ((mon.stats[0] as f32 * pct / 100.0).ceil() as u16).clamp(1, mon.stats[0])
        } else {
            mon.stats[0]
        };
        if mon.hp > mon.stats[0] {
            return Err(bad(format!("{}: {} HP of {}", ps.species, mon.hp, mon.stats[0])));
        }
        mon.status = STATUSES
            .iter()
            .copied()
            .find(|s| s.id() == ps.status)
            .ok_or_else(|| bad(format!("unknown status {}", ps.status)))?;
        let sd = &STATUS_CONDS[mon.status as usize];
        self.listen(sd.events, sd.events_pre);
        mon.status_time = ps.status_turns;
        mon.tox_stage = ps.toxic_stage;
        mon.status_st = EffState { order: ps.status_extra.order, uid: 0, a: ps.status_extra.a, b: ps.status_extra.b };
        if ps.boosts.iter().any(|b| !(-6..=6).contains(b)) {
            return Err(bad(format!("{}: stat stages run from -6 to 6", ps.species)));
        }
        mon.boosts = ps.boosts;
        (mon.moves, mon.n_moves) = Battle::move_slots(&ps.moves, ps.transformed)?;
        if ps.transformed {
            (mon.base_moves, mon.base_n_moves) = Battle::move_slots(&ps.base_moves, false)?;
        }
        mon.ability = ability;
        mon.base_ability = base_ability;
        let cud = if ps.ability_extra.item.is_empty() { None } else { Some(find_item(&ps.ability_extra.item)?) };
        if ability == ab::CUDCHEW && cud.is_none() && ps.ability_extra.a != 0 {
            return Err(bad("Cud Chew's berry is named in ability_extra.item"));
        }
        mon.ability_st = EffState {
            order: ps.ability_extra.order,
            uid: 0,
            a: cud.map_or(ps.ability_extra.a, |i| i as i16),
            b: ps.ability_extra.b,
        };
        mon.ability_boosts = ps.ability_boosts;
        mon.syrup_triggered = ps.syrup_triggered;
        mon.item = item;
        mon.item_st = EffState { order: ps.item_extra.order, uid: 0, a: ps.item_extra.a, b: ps.item_extra.b };
        mon.last_item = find_item(&ps.last_item)?;
        mon.used_item_this_turn = ps.used_item_this_turn;
        mon.ate_berry = ps.ate_berry;
        let on_field = place < ACTIVE;
        mon.position = place as u8;
        mon.is_active = ps.is_active.unwrap_or(on_field && !mon.fainted);
        mon.faint_queued = ps.faint_queued.unwrap_or(mon.fainted);
        mon.switch_flag = ps.switch_flag;
        mon.switch_move = find_move_or_none(&ps.switch_move)?;
        mon.force_switch_flag = ps.force_switch_flag;
        mon.skip_before_switch_out = ps.skip_before_switch_out;
        mon.being_called_back = ps.being_called_back;
        mon.trapped = ps.trapped;
        mon.active_turns = ps.active_turns.unwrap_or(mon.is_active as u16);
        mon.newly_switched = ps.newly_switched;
        mon.active_move_actions = ps.move_actions.unwrap_or(mon.is_active as u8);
        mon.last_move = find_move_or_none(&ps.last_move)?;
        mon.last_attempted_move = ps.last_attempted_move.as_deref().map(find_move_or_none).transpose()?;
        if ps.unknown_pp >= 1 << MAX_MOVES || ps.base_unknown_pp >= 1 << MAX_MOVES {
            return Err(Error::BadState("unknown_pp contains a nonexistent move slot".into()));
        }
        mon.unknown_pp = ps.unknown_pp;
        mon.base_unknown_pp = ps.base_unknown_pp;
        mon.last_move_loc = ps.last_move_target;
        mon.move_this_turn = outcome_res(ps.move_this_turn);
        mon.move_last_turn = outcome_res(ps.move_last_turn);
        mon.times_attacked = ps.times_attacked;
        mon.was_attacked = ps.was_attacked;
        mon.last_attack_damage = ps.last_attack_damage;
        mon.hurt_this_turn = ps.hurt_this_turn;
        mon.stats_raised_this_turn = ps.stats_raised_this_turn;
        mon.stats_lowered_this_turn = ps.stats_lowered_this_turn;
        mon.locked_move = find_move_or_none(&ps.locked_move)?;
        mon.speed = ps.speed;
        if !ps.volatiles.is_empty() && !on_field {
            return Err(bad(format!("{}: only a Pokémon on the field has volatile conditions", ps.species)));
        }
        Ok(mon)
    }

    /// Builds a battle from a position. See the [module documentation](crate::position).
    pub fn from_state(st: &BattleState) -> Result<Battle, Error> {
        let mut b = Battle::blank(st.seed);
        b.rng.calls = st.rng_calls;
        if st.turn == 0 {
            return Err(bad("turns are counted from 1"));
        }
        b.turn = st.turn;
        b.started = true;
        b.ended = st.ended;
        b.winner = st.winner;
        b.request = if st.ended { Request::None } else { st.request };
        if !st.ended && st.request == Request::None {
            return Err(bad("a position is taken at a move or switch request"));
        }
        b.mid_turn = st.mid_turn.unwrap_or(st.request == Request::Switch);
        b.last_move = find_move_or_none(&st.last_move)?;
        b.open_team_sheets = st.open_team_sheets;

        let mut names = Names { order: [[0, 1, 2, 3, 4, 5]; 2], n: [0; 2] };
        for (s, side) in st.sides.iter().enumerate() {
            let n = side.pokemon.len();
            if !(ACTIVE..=MAX_TEAM).contains(&n) {
                return Err(bad(format!("side {} has {n} Pokémon; need {ACTIVE} to {MAX_TEAM}", s + 1)));
            }
            names.n[s] = n;
            let mut seen = [false; MAX_TEAM];
            for (p, ps) in side.pokemon.iter().enumerate() {
                let idx = ps.team_index.unwrap_or(p as u8) as usize;
                if idx >= n || seen[idx] {
                    return Err(bad(format!("side {}: team_index must number its Pokémon 0 to {}", s + 1, n - 1)));
                }
                seen[idx] = true;
                names.order[s][p] = idx as u8;
            }
            b.sides[s].n = n as u8;
            b.sides[s].order = names.order[s];
        }

        // The Pokémon themselves, then whatever refers from one to another.
        for (s, side) in st.sides.iter().enumerate() {
            let side_has_mega = side.pokemon.iter().any(|ps| {
                let now = ps.base_species.as_deref().unwrap_or(&ps.species);
                species_id(&to_id(now)).is_some_and(|sp| mega_origin(sp).is_some())
            });
            for (p, ps) in side.pokemon.iter().enumerate() {
                let mon = b.mon_from(ps, p, side_has_mega)?;
                b.sides[s].team[names.order[s][p] as usize] = mon;
            }
            b.sides[s].mega_used = side_has_mega;
            let fainted = side.pokemon.iter().filter(|ps| ps.fainted || ps.hp == Some(0)).count() as u8;
            b.sides[s].total_fainted = side.total_fainted.unwrap_or(fainted);
            b.sides[s].pokemon_left = side.pokemon_left.unwrap_or(side.pokemon.len() as u8 - fainted);
            b.sides[s].fainted_this_turn = side.fainted_this_turn;
            b.sides[s].fainted_last_turn = side.fainted_last_turn;
        }
        for (s, side) in st.sides.iter().enumerate() {
            for (p, ps) in side.pokemon.iter().enumerate() {
                let r = MonRef { side: s as u8, idx: names.order[s][p] };
                if let Some(place) = ps.illusion {
                    let as_whom = names.mon(MonId { side: s as u8, pokemon: place })?;
                    b.mon_mut(r).illusion = as_whom.idx + 1;
                }
                if let Some(said) = &ps.types_said {
                    let mut bits = 0u16;
                    for &id in &said.attacked_by {
                        let by = names.mon(id)?;
                        bits |= 1 << (by.side as usize * MAX_TEAM + by.idx as usize);
                    }
                    b.mon_mut(r).said.attacked_by = bits;
                }
                let mut bits = 0u16;
                for &id in &ps.hit_by_this_turn {
                    let by = names.mon(id)?;
                    bits |= 1 << (by.side as usize * MAX_TEAM + by.idx as usize);
                }
                b.mon_mut(r).hit_by_this_turn = bits;
                if ps.damaged_by.len() > MAX_TEAM {
                    return Err(bad("damaged_by lists each foe at most once"));
                }
                for (k, hit) in ps.damaged_by.iter().enumerate() {
                    let by = names.mon(MonId { side: 1 - s as u8, pokemon: hit.pokemon })?;
                    if hit.slot.is_some_and(|slot| slot >= 4) {
                        return Err(bad("damaged_by: slot is side * 2 + position"));
                    }
                    b.mon_mut(r).damaged_by[k] = DamagedBy {
                        idx: by.idx,
                        slot: hit.slot.unwrap_or(NO_SLOT),
                        damage: hit.damage,
                        this_turn: hit.this_turn,
                    };
                }
                b.mon_mut(r).n_damaged_by = ps.damaged_by.len() as u8;
                if p < ACTIVE {
                    if ps.volatiles.len() > VOL_CAP {
                        return Err(bad("too many volatile conditions on one Pokémon"));
                    }
                    for v in &ps.volatiles {
                        let mut c: Cond<VolKind> = b.cond_from(&names, v, "volatile condition")?;
                        if c.kind == VolKind::Substitute && c.data == 0 {
                            // A fresh one: a quarter of its maker's HP, in half points.
                            c.data = 2 * (b.mon(r).max_hp() / 4);
                        }
                        if b.vols[s][p].has(c.kind) {
                            return Err(bad(format!("{} listed twice on one Pokémon", v.id)));
                        }
                        b.vols[s][p].push(c);
                    }
                }
            }
            for c in &side.conditions {
                let c: Cond<SideCond> = b.cond_from(&names, c, "side condition")?;
                if b.sides[s].conds.has(c.kind) {
                    return Err(bad(format!("{} listed twice on one side", c.kind.id())));
                }
                b.sides[s].conds.push(c);
            }
            for p in 0..ACTIVE {
                for c in &side.slot_conditions[p] {
                    let c: Cond<SlotCond> = b.cond_from(&names, c, "slot condition")?;
                    if b.sides[s].slot_conds[p].has(c.kind) {
                        return Err(bad(format!("{} listed twice on one position", c.kind.id())));
                    }
                    b.sides[s].slot_conds[p].push(c);
                }
            }
        }
        if let Some(w) = &st.weather {
            b.field.weather = b.cond_from(&names, w, "weather")?;
        }
        if let Some(t) = &st.terrain {
            b.field.terrain = b.cond_from(&names, t, "terrain")?;
        }
        for c in &st.pseudo_weather {
            let c: Cond<Pseudo> = b.cond_from(&names, c, "field condition")?;
            if b.field.pseudo.has(c.kind) {
                return Err(bad(format!("{} listed twice", c.kind.id())));
            }
            b.field.pseudo.push(c);
        }

        b.effect_order = st.effect_order;
        if st.speed_order.len() > 4 {
            return Err(bad("speed_order lists at most the four field positions"));
        }
        if st.speed_order.iter().any(|&slot| slot >= 4) {
            return Err(bad("speed_order: positions are side + 2 * position"));
        }
        b.speed_order[..st.speed_order.len()].copy_from_slice(&st.speed_order);
        b.n_speed_order = st.speed_order.len() as u8;
        b.number_states();
        if !st.prepared {
            // The turn is over once its end has been played: nothing by hand
            // is pending, and no end of turn is in the queue.
            let turn_over = st.pending.iter().all(|a| a.resolved.is_some() && a.kind != "residual");
            b.check_by_hand(turn_over)?;
            b.work_out_bookkeeping();
        }
        b.queue_from(st, &names)?;
        // Last, so that nothing the working-out above did (a Quick Claw rolled again for a
        // pending move, say) is taken for something the battle showed.
        b.shown_from(st, &names)?;
        Ok(b)
    }

    /// The team a side registered: as given, or else the Pokémon it has, as they are now.
    fn roster_from(&mut self, s: usize, side: &SideState, names: &Names) -> Result<(), Error> {
        let n = self.sides[s].n as usize;
        let mut roster = [Listed::NONE; MAX_ROSTER];
        if side.roster.is_empty() {
            for (p, entry) in roster.iter_mut().enumerate().take(n) {
                let idx = names.order[s][p];
                let m = &self.sides[s].team[idx as usize];
                *entry = Listed {
                    species: m.set_species,
                    gender: m.gender,
                    item: m.item,
                    ability: m.base_ability,
                    n_moves: m.base_n_moves,
                    nature: m.nature,
                    brought: idx,
                    ..Listed::NONE
                };
                for k in 0..m.base_n_moves as usize {
                    entry.moves[k] = m.base_moves[k].id;
                }
            }
            (self.sides[s].roster, self.sides[s].n_roster) = (roster, n as u8);
            return Ok(());
        }
        if side.roster.len() > MAX_ROSTER {
            return Err(bad(format!("side {}: a registered team has at most {MAX_ROSTER} Pokémon", s + 1)));
        }
        let mut brought = [false; MAX_TEAM];
        for (entry, ls) in roster.iter_mut().zip(&side.roster) {
            let species = find_species(&ls.species)?;
            let gender = match &ls.gender {
                Some(g) => Gender::parse(g).ok_or_else(|| bad(format!("unknown gender {g}")))?,
                None => SPECIES[species as usize].gender.unwrap_or(Gender::M),
            };
            if ls.moves.len() > MAX_MOVES {
                return Err(bad(format!("{}: a team sheet has at most {MAX_MOVES} moves", ls.species)));
            }
            *entry = Listed {
                species,
                gender,
                item: if ls.item.is_empty() { it::NONE } else { find_item(&ls.item)? },
                ability: if ls.ability.is_empty() { ab::NOABILITY } else { find_ability(&ls.ability)? },
                n_moves: ls.moves.len() as u8,
                nature: if ls.nature.is_empty() {
                    (0, 0)
                } else {
                    nature(&ls.nature).ok_or_else(|| bad(format!("unknown nature {}", ls.nature)))?
                },
                ..Listed::NONE
            };
            for (k, m) in ls.moves.iter().enumerate() {
                entry.moves[k] = find_move(m)?;
            }
            for m in &ls.told {
                entry.tell(find_move(m)?);
            }
            if let Some(p) = ls.brought {
                if p as usize >= n || std::mem::replace(&mut brought[p as usize], true) {
                    return Err(bad(format!(
                        "side {}: roster entries must be brought as different Pokémon of the side",
                        s + 1
                    )));
                }
                entry.brought = names.order[s][p as usize];
            }
        }
        (self.sides[s].roster, self.sides[s].n_roster) = (roster, side.roster.len() as u8);
        Ok(())
    }

    /// What each side has been shown: as given, and for a Pokémon on the field
    /// with nothing said, that it has been seen.
    fn shown_from(&mut self, st: &BattleState, names: &Names) -> Result<(), Error> {
        for (s, side) in st.sides.iter().enumerate() {
            self.roster_from(s, side, names)?;
            self.sides[s].shown = [NOTHING_SHOWN; MAX_TEAM];
            for mon in self.sides[s].team.iter_mut() {
                mon.live = NOTHING_SHOWN;
            }
            let mut taken = [false; MAX_TEAM + 1];
            for (p, ps) in side.pokemon.iter().enumerate() {
                let idx = names.order[s][p] as usize;
                // Said nothing of which entry of the registered team it looks like: its own.
                let own = (0..self.sides[s].n_roster).find(|&j| self.sides[s].roster[j as usize].brought == idx as u8);
                let listed = |shown: &ShownState, rec: &mut Shown| {
                    if shown.listed.is_none() && rec.seen != 0 {
                        rec.listed = own.unwrap_or(NOT_LISTED);
                    }
                };
                if let Some(shown) = &ps.shown {
                    let mut rec = shown.record()?;
                    listed(shown, &mut rec);
                    if rec.seen as usize > side.pokemon.len() || (rec.seen != 0 && taken[rec.seen as usize]) {
                        return Err(bad(format!(
                            "side {}: shown.id must number the Pokémon seen from 1, each once",
                            s + 1
                        )));
                    }
                    taken[rec.seen as usize] = true;
                    self.sides[s].shown[idx] = rec;
                }
                if let Some(shown) = &ps.shown_now {
                    if !self.sides[s].team[idx].is_active {
                        return Err(bad(format!("{}: only a Pokémon on the field has shown_now", ps.species)));
                    }
                    let mut rec = shown.record()?;
                    listed(shown, &mut rec);
                    self.sides[s].team[idx].live = rec;
                }
            }
            // Whoever is on the field has been seen, as whoever it passes for.
            for p in 0..ACTIVE.min(side.pokemon.len()) {
                let r = MonRef { side: s as u8, idx: names.order[s][p] };
                if !self.mon(r).is_active {
                    continue;
                }
                let a = self.shown_as(r);
                let side = &mut self.sides[s];
                if side.shown[a].seen == 0 {
                    let free = (1..=MAX_TEAM).find(|&id| !taken[id]).unwrap_or(MAX_TEAM);
                    taken[free] = true;
                    side.shown[a].seen = free as u8;
                    let own = (0..side.n_roster).find(|&j| side.roster[j as usize].brought == a as u8);
                    side.shown[a].listed = own.unwrap_or(NOT_LISTED);
                }
                if side.shown[a].species == NO_SPECIES {
                    side.shown[a].species = side.team[a].species;
                }
                if side.team[r.idx as usize].live.seen == 0 {
                    let mut live = side.shown[a];
                    live.fainted = false;
                    side.team[r.idx as usize].live = live;
                }
            }
            let side = &mut self.sides[s];
            for a in 0..side.n as usize {
                // (A Pokémon seen and now on the bench was last seen as what it is.)
                if side.shown[a].seen != 0 && side.shown[a].species == NO_SPECIES {
                    side.shown[a].species = side.team[a].species;
                }
            }
            side.n_seen = (0..side.n as usize).map(|a| side.shown[a].seen).max().unwrap_or(0);
        }
        Ok(())
    }

    /// A hand-written position has to be one a battle can be in.
    fn check_by_hand(&mut self, turn_over: bool) -> Result<(), Error> {
        if self.ended {
            return Ok(());
        }
        let mut anyone_to_replace = false;
        for s in 0..2 {
            if self.sides[s].pokemon_left == 0 {
                return Err(bad(format!("side {} has no Pokémon left; that battle is over (ended: true)", s + 1)));
            }
            let can_switch = self.can_switch(s);
            for p in 0..ACTIVE {
                let r = self.active(s, p);
                let m = self.mon(r);
                match self.request {
                    Request::Move if m.fainted && can_switch => {
                        return Err(bad(format!(
                            "side {}: a fainted Pokémon is on the field with replacements left; that is a switch request",
                            s + 1
                        )));
                    }
                    Request::Move if m.switch_flag => {
                        return Err(bad("switch_flag at a move request"));
                    }
                    // Once the turn is over the fainted are replaced, said or not.
                    // (In the middle of one they wait.)
                    Request::Switch if m.fainted && can_switch && turn_over => {
                        self.mon_mut(r).switch_flag = true;
                        anyone_to_replace = true;
                    }
                    Request::Switch if m.switch_flag => anyone_to_replace = true,
                    _ => {}
                }
            }
        }
        if self.request == Request::Switch && !anyone_to_replace {
            return Err(bad("a switch request with nobody to replace (switch_flag)"));
        }
        Ok(())
    }

    /// Every effect state gets an identity of its own.
    fn number_states(&mut self) {
        let mut uid = 0u16;
        let mut next = || {
            uid += 1;
            uid
        };
        for s in 0..2 {
            for i in 0..self.sides[s].n as usize {
                let m = &mut self.sides[s].team[i];
                m.status_st.uid = next();
                m.ability_st.uid = next();
                m.item_st.uid = next();
            }
            for p in 0..ACTIVE {
                self.vols[s][p].as_mut_slice().iter_mut().for_each(|c| c.st.uid = next());
                self.sides[s].slot_conds[p].as_mut_slice().iter_mut().for_each(|c| c.st.uid = next());
            }
            self.sides[s].conds.as_mut_slice().iter_mut().for_each(|c| c.st.uid = next());
        }
        self.field.weather.st.uid = next();
        self.field.terrain.st.uid = next();
        self.field.pseudo.as_mut_slice().iter_mut().for_each(|c| c.st.uid = next());
        self.next_uid = uid;
    }

    /// What a hand-written position leaves out: the order effects started in,
    /// cached speeds, and what the coming request allows.
    fn work_out_bookkeeping(&mut self) {
        // Effects on the field in a fixed order: left to right, first side first.
        let mut order = self.effect_order;
        let mut next = || {
            order += 1;
            order
        };
        for s in 0..2 {
            for p in 0..ACTIVE {
                let r = self.active(s, p);
                if !self.mon(r).is_active {
                    continue;
                }
                let m = self.mon_mut(r);
                m.ability_st.order = next();
                if m.item != it::NONE {
                    m.item_st.order = next();
                }
                if m.status != Status::None {
                    m.status_st.order = next();
                }
                self.vols[s][p].as_mut_slice().iter_mut().for_each(|c| c.st.order = next());
            }
            self.sides[s].conds.as_mut_slice().iter_mut().for_each(|c| c.st.order = next());
            for p in 0..ACTIVE {
                self.sides[s].slot_conds[p].as_mut_slice().iter_mut().for_each(|c| c.st.order = next());
            }
        }
        self.effect_order = order;
        if self.ended {
            return;
        }
        self.update_speed();
        let (actives, n) = self.all_active(true);
        let mut keyed: Vec<(u8, i32)> =
            actives[..n].iter().map(|&r| (r.side + 2 * self.mon(r).position, self.mon(r).speed)).collect();
        keyed.sort_by_key(|&(_, speed)| -speed);
        for (i, &(slot, _)) in keyed.iter().enumerate() {
            self.speed_order[i] = slot;
        }
        self.n_speed_order = n as u8;
        if self.request != Request::Move {
            return;
        }
        for s in 0..2 {
            for p in 0..ACTIVE {
                let r = self.active(s, p);
                if !self.in_play(r) {
                    continue;
                }
                let m = self.mon_mut(r);
                for k in 0..m.n_moves as usize {
                    m.moves[k].disabled = false;
                    m.moves[k].hidden = false;
                }
                self.request_prep(r);
            }
        }
        self.request_locks();
    }

    fn queue_from(&mut self, st: &BattleState, names: &Names) -> Result<(), Error> {
        if st.pending.len() > QUEUE_CAP {
            return Err(bad("too many pending actions"));
        }
        let mut by_hand = false;
        for a in &st.pending {
            let kind_name = if a.kind.is_empty() { "move" } else { a.kind.as_str() };
            let kind = ACTION_KINDS
                .iter()
                .find(|(_, n)| *n == kind_name)
                .map(|(k, _)| *k)
                .ok_or_else(|| bad(format!("unknown kind of action {kind_name}")))?;
            let mon = names.mon_opt(a.pokemon)?;
            let Some(res) = &a.resolved else {
                // Written by hand: the end of the turn, or a move, ordered as if it had just been chosen.
                by_hand = true;
                if kind == ActKind::Residual {
                    self.queue.push(Battle::blank_action(ActKind::Residual, 300));
                    continue;
                }
                let (ActKind::Move, Some(user)) = (kind, mon) else {
                    return Err(bad("a pending action written by hand is a move by a Pokémon, or the end of the turn"));
                };
                let id = find_move(&a.move_id)?;
                let act = self.resolve_move(user, id, a.target);
                self.queue.push(act);
                continue;
            };
            let mut act = Battle::blank_action(kind, res.order);
            act.priority = res.priority;
            act.speed = res.speed;
            act.mon = mon;
            act.move_id = if a.move_id.is_empty() { 0 } else { find_move(&a.move_id)? };
            act.target_loc = a.target;
            act.switch_to = names.mon_opt(a.switch_to)?;
            act.frac = res.fractional_priority;
            act.move_priority = res.move_priority;
            act.prankster = res.prankster;
            act.orig_target = names.mon_opt(res.original_target)?;
            act.self_target = res.self_target;
            act.target_replaced = res.target_replaced;
            act.source = match (res.round, res.self_switch.is_empty()) {
                (Some(ignore_ability), _) => ActSource::Round { ignore_ability },
                (None, false) => ActSource::SelfSwitch(find_move(&res.self_switch)?),
                (None, true) => ActSource::None,
            };
            self.queue.push(act);
        }
        if by_hand {
            self.sort_queue();
            self.am_len = 0;
            if !self.queue.as_slice().iter().any(|a| a.kind == ActKind::Residual) {
                // With moves still to come, so is the end of the turn.
                self.queue.push(Battle::blank_action(ActKind::Residual, 300));
            }
        }
        Ok(())
    }

    /// The battle with everything that is scratch space or mere identity
    /// blanked, so that two battles in the same position print the same.
    /// Used to check that a position survives being written down and rebuilt.
    #[doc(hidden)]
    pub fn canonical(&self) -> Battle {
        let mut b = *self;
        let blank = Battle::blank([0; 4]);
        b.rng.calls = 0;
        b.next_uid = 0;
        b.event_mask = 0;
        b.event_mask_pre = 0;
        b.event = blank.event;
        b.effect = blank.effect;
        b.effect_holder = None;
        b.event_depth = 0;
        b.am = blank.am;
        b.am_len = 0;
        b.active_move = None;
        b.active_pokemon = None;
        b.active_target = None;
        b.faint_queue = blank.faint_queue;
        for i in b.queue.len as usize..QUEUE_CAP {
            b.queue.items[i] = blank.queue.items[i];
        }
        for i in b.n_speed_order as usize..4 {
            b.speed_order[i] = 0;
        }
        fn tidy<K: Copy + PartialEq, const N: usize>(list: &mut CondList<K, N>, blank: K) {
            list.as_mut_slice().iter_mut().for_each(|c| c.st.uid = 0);
            list.tidy(blank);
        }
        for s in 0..2 {
            let n = b.sides[s].n as usize;
            for i in 0..MAX_TEAM {
                if i >= n {
                    b.sides[s].team[i] = blank.sides[0].team[0];
                    continue;
                }
                let m = &mut b.sides[s].team[i];
                m.status_st.uid = 0;
                m.ability_st.uid = 0;
                m.item_st.uid = 0;
                let blank_slot = blank.sides[0].team[0].moves[0];
                for k in m.n_moves as usize..MAX_MOVES {
                    m.moves[k] = blank_slot;
                }
                if !m.transformed {
                    m.base_moves = [blank_slot; MAX_MOVES];
                    m.base_n_moves = 0;
                }
                for k in m.base_n_moves as usize..MAX_MOVES {
                    m.base_moves[k] = blank_slot;
                }
                for k in m.n_damaged_by as usize..MAX_TEAM {
                    m.damaged_by[k] = NO_DAMAGED_BY;
                }
            }
            for i in n..MAX_TEAM {
                b.sides[s].order[i] = i as u8;
            }
            for p in 0..ACTIVE {
                tidy(&mut b.vols[s][p], VolKind::FIRST);
                tidy(&mut b.sides[s].slot_conds[p], SlotCond::FIRST);
            }
            tidy(&mut b.sides[s].conds, SideCond::FIRST);
        }
        b.field.weather.st.uid = 0;
        b.field.terrain.st.uid = 0;
        tidy(&mut b.field.pseudo, Pseudo::FIRST);
        b
    }

    /// The first difference between two battles' positions, if there is one.
    #[doc(hidden)]
    pub fn position_diff(&self, other: &Battle) -> Option<String> {
        let (x, y) = (self.canonical(), other.canonical());
        if x == y {
            return None;
        }
        let (a, b) = (format!("{x:#?}"), format!("{y:#?}"));
        // Report the first line that differs, with the lines that say where it is.
        let (la, lb): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
        let at = la.iter().zip(&lb).position(|(x, y)| x != y).unwrap_or(la.len().min(lb.len()));
        let mut path: Vec<&str> = Vec::new();
        let indent = |l: &str| l.len() - l.trim_start().len();
        let mut depth = la.get(at).map_or(0, |l| indent(l));
        for l in la[..at].iter().rev() {
            if indent(l) < depth && (l.trim_end().ends_with('{') || l.trim_end().ends_with('[')) {
                depth = indent(l);
                path.push(l.trim());
            }
        }
        path.reverse();
        Some(format!(
            "{}\n  here:  {}\n  there: {}",
            path.join(" > "),
            la.get(at).map_or("(end)", |l| l.trim()),
            lb.get(at).map_or("(end)", |l| l.trim())
        ))
    }
}
