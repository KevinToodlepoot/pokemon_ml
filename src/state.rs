//! Battle state. Everything here is plain fixed-size data so a `Battle` can be
//! copied cheaply (search wants thousands of copies per decision).

use crate::data::{
    Boosts, Category, Ev, Gender, MOVES, MoveData, Pseudo, Secondary, SelfSwitch, SideCond, SlotCond, Status, Target,
    Terrain, Type, VolKind, Weather,
};
use crate::rng::Rng;
use crate::shown::Shown;

/// Largest team a side can bring into battle.
pub const MAX_TEAM: usize = 6;
/// Active Pokémon per side (doubles).
pub const ACTIVE: usize = 2;
pub const MAX_MOVES: usize = 4;
/// The most Pokémon a player registers (of which up to `MAX_TEAM` are brought).
pub const MAX_ROSTER: usize = 6;
/// "No entry" where an index into a side's registered team is kept.
pub(crate) const NOT_LISTED: u8 = u8::MAX;
/// Most volatile conditions one Pokémon can hold at once. Far more than any
/// real game state reaches; adding one beyond it fails like an immunity would.
pub const VOL_CAP: usize = 20;
/// "No position" in `Cond::source_slot`.
pub const NO_SLOT: u8 = u8::MAX;

/// "No species" in fields that hold an optional species index.
/// "No move" in fields that hold a move table index.
pub const NO_MOVE: u16 = u16::MAX;
pub const NO_SPECIES: u16 = u16::MAX;

/// A Pokémon identified by side and by its fixed index in that side's team.
/// The index never changes, unlike its field position.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MonRef {
    pub side: u8,
    pub idx: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MoveSlot {
    pub id: u16,
    pub pp: u8,
    pub maxpp: u8,
    /// Cannot be chosen this turn (Choice lock and the like). Recomputed every turn.
    pub disabled: bool,
    /// Disabled only by something its player has not been shown (a foe's
    /// Imprison). Showdown still lists such a move in the request it sends.
    pub hidden: bool,
    /// Used at least once since the Pokémon came in (Last Resort).
    pub used: bool,
}

/// Bookkeeping Showdown attaches to every effect instance (`EffectState`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct EffState {
    /// Showdown's `effectOrder`: a battle-wide counter value taken when the
    /// effect started on an active Pokémon, 0 otherwise. Breaks ties between
    /// switch-in and redirection handlers.
    pub order: u32,
    /// Identity of this instance. Showdown skips a queued handler when the
    /// state object it was collected for has since been replaced.
    pub uid: u16,
    /// Scratch values whose meaning depends on the effect (documented where used).
    pub a: i16,
    pub b: i16,
}

/// One instance of a condition: a volatile on a Pokémon, a condition on a
/// side or on one of its positions, a pseudo-weather, the weather or the
/// terrain. `K` is the enum naming the condition.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cond<K: Copy> {
    pub kind: K,
    /// Turns left; 0 means the condition has no duration.
    pub duration: u8,
    /// Whatever the condition keeps in its state, documented where the
    /// condition is written (`conditions.rs`). For the first volatiles:
    /// `stall`: 1-in-`data` chance the next protecting move works.
    /// `choicelock`: the move table index the holder is locked into, plus one.
    /// `confusion`: turns of confusion left.
    /// `metronome`: the last move used (table index plus one); `st.a` counts consecutive uses.
    pub data: u16,
    /// The Pokémon that caused the condition (`effectState.source`).
    pub source: Option<MonRef>,
    /// Where that Pokémon stood at the time, as side * 2 + position (`effectState.sourceSlot`).
    pub source_slot: u8,
    /// Set once one of the condition's handlers has run inside an event.
    /// Showdown sorts a pseudo-weather's handlers differently from then on.
    pub(crate) targeted: bool,
    pub st: EffState,
}

impl<K: Copy> Cond<K> {
    pub(crate) const fn new(kind: K) -> Cond<K> {
        Cond {
            kind,
            duration: 0,
            data: 0,
            source: None,
            source_slot: NO_SLOT,
            targeted: false,
            st: EffState { order: 0, uid: 0, a: 0, b: 0 },
        }
    }
}

/// Conditions in the order they were added. Showdown iterates them in
/// insertion order, and that order decides how ties between their handlers
/// come out, so it is part of the state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CondList<K: Copy, const N: usize> {
    len: u8,
    items: [Cond<K>; N],
}

impl<K: Copy, const N: usize> CondList<K, N> {
    /// An empty list; `blank` only fills the unused storage.
    pub(crate) const fn new(blank: K) -> Self {
        CondList { len: 0, items: [Cond::new(blank); N] }
    }
}

impl<K: Copy + PartialEq, const N: usize> CondList<K, N> {
    pub fn as_slice(&self) -> &[Cond<K>] {
        &self.items[..self.len as usize]
    }
    pub fn get(&self, kind: K) -> Option<&Cond<K>> {
        self.as_slice().iter().find(|v| v.kind == kind)
    }
    pub fn get_mut(&mut self, kind: K) -> Option<&mut Cond<K>> {
        let n = self.len as usize;
        self.items[..n].iter_mut().find(|v| v.kind == kind)
    }
    pub fn has(&self, kind: K) -> bool {
        self.get(kind).is_some()
    }
    pub fn is_full(&self) -> bool {
        self.len as usize == N
    }
    pub(crate) fn push(&mut self, v: Cond<K>) {
        debug_assert!(!self.has(v.kind));
        self.items[self.len as usize] = v;
        self.len += 1;
    }
    pub(crate) fn remove(&mut self, kind: K) -> bool {
        let n = self.len as usize;
        match self.items[..n].iter().position(|v| v.kind == kind) {
            Some(i) => {
                self.items.copy_within(i + 1..n, i);
                self.len -= 1;
                true
            }
            None => false,
        }
    }
    pub(crate) fn clear(&mut self) {
        self.len = 0;
    }
    /// The conditions, for editing in place.
    pub(crate) fn as_mut_slice(&mut self) -> &mut [Cond<K>] {
        let n = self.len as usize;
        &mut self.items[..n]
    }
    /// Blank the unused storage, so that two lists holding the same conditions are the same data.
    pub(crate) fn tidy(&mut self, blank: K) {
        for i in self.len as usize..N {
            self.items[i] = Cond::new(blank);
        }
    }
    /// Remove every condition `pred` picks and return them, both lists keeping their order.
    pub(crate) fn take_where(&mut self, pred: impl Fn(K) -> bool) -> Self {
        let mut taken = Self { len: 0, items: self.items };
        let mut kept = 0;
        for i in 0..self.len as usize {
            let c = self.items[i];
            if pred(c.kind) {
                taken.items[taken.len as usize] = c;
                taken.len += 1;
            } else {
                self.items[kept] = c;
                kept += 1;
            }
        }
        self.len = kept as u8;
        taken
    }
}

/// A volatile condition on a Pokémon.
pub type Volatile = Cond<VolKind>;
/// The volatiles of the Pokémon in one active position. Only Pokémon on the
/// field have any, so they are stored per position rather than per Pokémon.
pub type Volatiles = CondList<VolKind, VOL_CAP>;
pub type SideConds = CondList<SideCond, { crate::data::N_SIDE_CONDS }>;
pub type SlotConds = CondList<SlotCond, { crate::data::N_SLOT_CONDS }>;
pub type PseudoWeathers = CondList<Pseudo, { crate::data::N_PSEUDO }>;

/// Conditions on the whole field (Showdown's `Field`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Field {
    /// `kind` is `Weather::None` when there is no weather.
    pub weather: Cond<Weather>,
    pub terrain: Cond<Terrain>,
    pub pseudo: PseudoWeathers,
}

/// `Pokemon#trapped`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trapped {
    #[default]
    No,
    Yes,
    /// Trapped by something the player has not been shown yet (Shadow Tag).
    Hidden,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pokemon {
    /// Current species (index into `data::SPECIES`); changes on Mega Evolution.
    pub species: u16,
    /// The species the Pokémon returns to when it leaves the field. Mega
    /// Evolution changes this too, so a Mega stays a Mega.
    pub base_species: u16,
    /// The species it was brought to the battle as (`set.species`).
    pub(crate) set_species: u16,
    /// Has taken another Pokémon's shape with Transform or Imposter; undone on leaving the field.
    pub transformed: bool,
    /// Its own moves, set aside while it is transformed.
    pub(crate) base_moves: [MoveSlot; MAX_MOVES],
    pub(crate) base_n_moves: u8,
    /// Illusion: the team index, plus one, of the Pokémon it is disguised as (0 for none).
    pub(crate) illusion: u8,
    /// The Mega this Pokémon can still evolve into (`NO_SPECIES` if none).
    pub can_mega: u16,
    /// The set's nature and stat points, kept to recompute stats when the species changes.
    pub(crate) nature: (u8, u8),
    pub(crate) stat_points: [u8; 6],
    /// Current types; an effect can change them until the Pokémon leaves the field.
    /// (Read them through `Battle::get_types`, which knows about Roost.)
    pub types: [Type; 2],
    /// A third type added by Forest's Curse or Trick-or-Treat (`Type::None` if none).
    pub added_type: Type,
    /// What each side has been told of its types, which is not always what they are.
    pub(crate) said: TypesSaid,
    pub level: u8,
    pub gender: Gender,
    /// Unboosted stats: max HP, Atk, Def, SpA, SpD, Spe.
    pub stats: [u16; 6],
    pub hp: u16,
    pub status: Status,
    /// Sleep / freeze turn counter (Showdown's `statusState.time`).
    pub status_time: u8,
    /// Toxic damage stage (Showdown's `statusState.stage`).
    pub tox_stage: u8,
    pub status_st: EffState,
    /// Stat stages: atk, def, spa, spd, spe, accuracy, evasion.
    pub boosts: [i8; 7],
    pub moves: [MoveSlot; MAX_MOVES],
    pub n_moves: u8,
    /// Current ability; `base_ability` is restored when the Pokémon leaves the field.
    pub ability: u16,
    pub base_ability: u16,
    pub ability_st: EffState,
    /// Stat stages an ability is holding on to (Opportunist's pending copies).
    pub(crate) ability_boosts: [i8; 7],
    /// Supersweet Syrup has already gone off once this battle.
    pub(crate) syrup_triggered: bool,
    /// Whether anything has attacked this Pokémon since it came in, and the
    /// damage of the latest attack (Showdown's `getLastAttackedBy`).
    pub(crate) was_attacked: bool,
    pub(crate) last_attack_damage: i32,
    /// Held item; 0 is none.
    pub item: u16,
    pub item_st: EffState,
    /// The item most recently used up or eaten.
    pub last_item: u16,
    pub used_item_this_turn: bool,
    pub ate_berry: bool,
    /// Index into the side's current order; positions below `ACTIVE` are on the field.
    pub position: u8,
    pub is_active: bool,
    pub fainted: bool,
    pub faint_queued: bool,
    /// Must be replaced at the next switch request.
    pub switch_flag: bool,
    /// The move whose `selfSwitch` set `switch_flag` (`NO_MOVE` for any other
    /// cause): Showdown stores the move's id in the flag itself, and some
    /// effects only count a flag that is plainly `true`.
    pub(crate) switch_move: u16,
    /// About to be dragged out by Roar, Red Card and the like (`forceSwitchFlag`).
    pub(crate) force_switch_flag: bool,
    /// The `BeforeSwitchOut` event has already run for the switch that is coming.
    pub(crate) skip_before_switch_out: bool,
    /// In the middle of being switched out (`beingCalledBack`).
    pub(crate) being_called_back: bool,
    /// Cannot switch out this turn.
    pub trapped: Trapped,
    /// Full turns spent on the field since switching in.
    pub active_turns: u16,
    /// Showdown's `moveThisTurnResult` / `moveLastTurnResult`: `Undef` (did not
    /// move), `Null`, or a boolean for whether the move did anything.
    pub(crate) move_this_turn: Res,
    pub(crate) move_last_turn: Res,
    /// Speed as last cached by Showdown's `updateSpeed`; several orderings read
    /// this stale value rather than the live stat.
    pub speed: i32,
    /// The move this Pokémon last used since coming in (`NO_MOVE` if none), and where it aimed it.
    pub last_move: u16,
    /// Last queued move named in the public log, including a Choice-item refusal.
    /// Separate from `last_move`, which mechanics use. None for imported legacy states.
    pub last_attempted_move: Option<u16>,
    /// Move slots whose remaining PP cannot be recovered from the public log.
    /// Exact mechanical PP is still kept in `moves`; the next move request clears these bits.
    pub unknown_pp: u8,
    /// The corresponding mask for its original moves during Transform.
    pub base_unknown_pp: u8,
    pub(crate) last_move_loc: i8,
    /// How many times it has started to move since coming in (Fake Out works while this is at most 1).
    pub active_move_actions: u8,
    /// Came in this turn, or has not had a turn yet.
    pub(crate) newly_switched: bool,
    /// HP left after the last time it took damage this turn (0 if unhurt), `Pokemon#hurtThisTurn`.
    pub(crate) hurt_this_turn: u16,
    /// Hits taken from moves since coming in (Rage Fist).
    pub times_attacked: u16,
    pub(crate) stats_raised_this_turn: bool,
    pub(crate) stats_lowered_this_turn: bool,
    /// What Showdown's `attackedBy` list is read for. Who has damaged this
    /// Pokémon this turn (bit `side * MAX_TEAM + team index`), and, oldest
    /// first, the latest damaging hit from each foe that has stayed on the
    /// field since (Metal Burst answers the last of them).
    pub(crate) hit_by_this_turn: u16,
    pub(crate) damaged_by: [DamagedBy; MAX_TEAM],
    pub(crate) n_damaged_by: u8,
    /// The move it is locked into for the coming turn (`NO_MOVE` if free):
    /// the next turn of a rampage, the second turn of a charging move, or the
    /// `recharge` placeholder. Worked out when the request is made.
    pub locked_move: u16,
    /// What has been shown of it during this stay on the field (see `shown.rs`); `seen` is 0 when there is none.
    pub(crate) live: Shown,
}

/// What a Pokémon's types are taken to be, and what Showdown has said of them.
///
/// Types are public: Showdown names them whenever they change. Two things keep
/// that from being the whole story. A Pokémon in disguise is shown with the
/// types of the one it passes for. And Reflect Type (or Transform) says only
/// which Pokémon was copied, whose types the two sides may see differently;
/// when what it took is not what the log last gave it, Showdown says so at the
/// end of the turn (`-start|…|typechange|…|[silent]`), and otherwise never.
/// [`Battle::seen_types`] reads this; `Battle::end_turn` keeps Showdown's side
/// of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TypesSaid {
    /// By the side looking: the base types it takes this Pokémon to have, or
    /// [`NOT_SAID`] where nothing has been said and they are what they seem.
    pub(crate) seen: [[Type; 2]; 2],
    /// Showdown's `apparentType`: the base types its log has given this Pokémon.
    pub(crate) apparent: [Type; 2],
    /// Showdown's `knownType`: off from when it copies a foe's types until something attacks it.
    pub(crate) known: bool,
    /// This turn it has taken another Pokémon's types (Reflect Type, Transform)...
    pub(crate) copied: bool,
    /// ...the last time by Reflect Type from a foe...
    pub(crate) copied_foe: bool,
    /// ...and its own side has since been told what they are.
    pub(crate) told: bool,
    /// What its own side took its base types to be before the first of those.
    pub(crate) before: [Type; 2],
    /// Showdown's `attackedBy`, as far as "is there anything in it" goes: who has aimed
    /// a move at it and stayed on the field since (bit `side * MAX_TEAM + team index`).
    pub(crate) attacked_by: u16,
}

/// Nothing said of a Pokémon's types that it does not show anyway.
pub(crate) const NOT_SAID: [Type; 2] = [Type::None; 2];

impl TypesSaid {
    /// A Pokémon as it comes in, or changes forme: it has its species' types and everyone knows it.
    pub(crate) const fn fresh(types: [Type; 2]) -> TypesSaid {
        TypesSaid {
            seen: [NOT_SAID; 2],
            apparent: types,
            known: true,
            copied: false,
            copied_foe: false,
            told: false,
            before: NOT_SAID,
            attacked_by: 0,
        }
    }
}

impl Pokemon {
    pub fn max_hp(&self) -> u16 {
        self.stats[0]
    }
    /// `Pokemon#getLastDamagedBy(true)`.
    pub(crate) fn last_damaged_by(&self) -> Option<DamagedBy> {
        self.n_damaged_by.checked_sub(1).map(|i| self.damaged_by[i as usize])
    }
}

/// One entry of Showdown's `attackedBy` that Metal Burst can read: a hit
/// from a foe that did a number of damage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DamagedBy {
    /// The attacker's index in its team.
    pub idx: u8,
    /// The field slot it attacked from.
    pub slot: u8,
    pub damage: u16,
    pub this_turn: bool,
}

pub(crate) const NO_DAMAGED_BY: DamagedBy = DamagedBy { idx: 0, slot: 0, damage: 0, this_turn: false };

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Side {
    pub team: [Pokemon; MAX_TEAM],
    /// Number of Pokémon brought.
    pub n: u8,
    /// `order[position]` is the team index of the Pokémon at that position.
    pub order: [u8; MAX_TEAM],
    pub pokemon_left: u8,
    /// How many of this side's Pokémon have fainted so far.
    pub total_fainted: u8,
    /// One of its Pokémon has Mega Evolved, which a side does once a battle. (Everyone
    /// sees it happen, even where an Illusion hides what the Pokémon became.)
    pub(crate) mega_used: bool,
    /// Whether one of its Pokémon fainted this turn, and last turn.
    pub(crate) fainted_this_turn: bool,
    pub(crate) fainted_last_turn: bool,
    /// Conditions on the whole side (Tailwind, screens, hazards).
    pub conds: SideConds,
    /// Conditions on each active position (Wish).
    pub slot_conds: [SlotConds; ACTIVE],
    /// What has been shown of each Pokémon, by team index, as of when it was last off the field,
    /// and how many have appeared.
    pub(crate) shown: [Shown; MAX_TEAM],
    pub(crate) n_seen: u8,
    /// The team this side registered, in the order registered: what Team Preview showed.
    pub(crate) roster: [Listed; MAX_ROSTER],
    pub(crate) n_roster: u8,
}

/// One Pokémon of the team a player registered. Team Preview shows its
/// species and gender; open team sheets add its item, ability, moves and
/// nature. (Never its stat points, and never whether it was brought.)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Listed {
    pub species: u16,
    /// Team Preview gave the Pokémon without its forme (`Zacian-*`). Showdown
    /// does that for a few species in some formats; not in Champions.
    pub any_forme: bool,
    pub gender: Gender,
    pub item: u16,
    pub ability: u16,
    pub moves: [u16; MAX_MOVES],
    pub n_moves: u8,
    pub nature: (u8, u8),
    /// Its team index if it was brought, else `NOT_LISTED`. (The engine's own
    /// knowledge: nothing that is shown depends on it.)
    pub brought: u8,
    /// Moves the other side has worked out it knows without seeing it use them,
    /// when the sheets are closed: ones of their own that its Imprison sealed
    /// (see [`Battle::sealed_moves`]).
    pub told: [u16; MAX_MOVES],
    pub n_told: u8,
}

impl Side {
    /// The entries of the registered team that Pokémon seen so far have been taken for, as a bit mask.
    pub(crate) fn taken(&self) -> u8 {
        let mut mask = 0;
        for rec in &self.shown[..self.n as usize] {
            if rec.seen != 0 && rec.listed != NOT_LISTED {
                mask |= 1 << rec.listed;
            }
        }
        mask
    }
}

impl Listed {
    pub(crate) const NONE: Listed = Listed {
        species: NO_SPECIES,
        any_forme: false,
        gender: Gender::N,
        item: 0,
        ability: crate::data::ab::NOABILITY,
        moves: [NO_MOVE; MAX_MOVES],
        n_moves: 0,
        nature: (0, 0),
        brought: NOT_LISTED,
        told: [NO_MOVE; MAX_MOVES],
        n_told: 0,
    };

    pub(crate) fn moves(&self) -> &[u16] {
        &self.moves[..self.n_moves as usize]
    }

    pub(crate) fn told(&self) -> &[u16] {
        &self.told[..self.n_told as usize]
    }

    /// Notes a move it has been worked out to know. Whether that is news.
    pub(crate) fn tell(&mut self, move_id: u16) -> bool {
        if self.told().contains(&move_id) || self.n_told as usize >= MAX_MOVES {
            return false;
        }
        self.told[self.n_told as usize] = move_id;
        self.n_told += 1;
        true
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Request {
    /// The battle is running or over; nobody is being asked anything.
    None,
    /// Both sides choose moves or switches for the turn.
    #[default]
    Move,
    /// One or both sides replace fainted Pokémon.
    Switch,
}

/// One active slot's decision.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Choice {
    Pass,
    /// `slot` is the move slot (0-based). `target` is Showdown's target
    /// location: +1/+2 are the foe's slots, -1/-2 your own, 0 for moves that
    /// do not take a target.
    /// `mega`: Mega Evolve before moving.
    Move {
        slot: u8,
        target: i8,
        mega: bool,
    },
    /// Switch to the Pokémon currently at this position (0-based) in the side's order.
    Switch {
        to: u8,
    },
}

impl Choice {
    /// A move choice without Mega Evolution.
    pub fn mv(slot: u8, target: i8) -> Choice {
        Choice::Move { slot, target, mega: false }
    }

    /// The choice in Showdown's notation: `move 2 1`, `move 3`, `move 1 2 mega`, `switch 4`, `pass`.
    pub fn to_showdown(self) -> String {
        match self {
            Choice::Pass => "pass".into(),
            Choice::Move { slot, target, mega } => {
                let mut s = format!("move {}", slot + 1);
                if target != 0 {
                    s.push_str(&format!(" {target}"));
                }
                if mega {
                    s.push_str(" mega");
                }
                s
            }
            Choice::Switch { to } => format!("switch {}", to + 1),
        }
    }

    /// Parses one slot's choice from Showdown's notation.
    pub fn parse(s: &str) -> Option<Choice> {
        let mut parts: Vec<&str> = s.split_whitespace().collect();
        let mega = parts.last() == Some(&"mega");
        if mega {
            parts.pop();
        }
        let mut parts = parts.into_iter();
        let kind = parts.next()?;
        let mut num = || parts.next().and_then(|p| p.parse::<i32>().ok());
        if mega && kind != "move" {
            return None;
        }
        match kind {
            "pass" => Some(Choice::Pass),
            "move" => {
                let slot = num()?;
                let target = num().unwrap_or(0);
                (1..=MAX_MOVES as i32).contains(&slot).then_some(Choice::Move {
                    slot: (slot - 1) as u8,
                    target: target as i8,
                    mega,
                })
            }
            "switch" => {
                let to = num()?;
                (1..=MAX_TEAM as i32).contains(&to).then_some(Choice::Switch { to: (to - 1) as u8 })
            }
            _ => None,
        }
    }

    /// Parses a side's choice, e.g. `move 1 2, switch 3`.
    pub fn parse_side(s: &str) -> Option<[Choice; ACTIVE]> {
        let mut out = [Choice::Pass; ACTIVE];
        let mut n = 0;
        for part in s.split(',') {
            if n == ACTIVE {
                return None;
            }
            out[n] = Choice::parse(part.trim())?;
            n += 1;
        }
        (n == ACTIVE).then_some(out)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ActKind {
    Team,
    Start,
    InstaSwitch,
    BeforeTurn,
    /// Revival Blessing bringing back the Pokémon its user picked.
    Revival,
    /// A move's `beforeTurnCallback`, at the very start of the turn (Counter).
    BeforeTurnMove,
    RunSwitch,
    Switch,
    MegaEvo,
    /// A move's `priorityChargeCallback`, before any move is used (Focus Punch).
    PriorityCharge,
    Move,
    Residual,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Action {
    pub kind: ActKind,
    pub order: u32,
    /// Priority in tenths: ten times the move's priority plus `frac`.
    pub priority: i32,
    pub speed: i32,
    pub mon: Option<MonRef>,
    /// Move id for `Move`.
    pub move_id: u16,
    pub target_loc: i8,
    /// Incoming Pokémon for `Switch` / `InstaSwitch`.
    pub switch_to: Option<MonRef>,
    /// Showdown's `fractionalPriority` in tenths (Quick Claw and the like).
    pub frac: i8,
    /// The move's priority after `ModifyPriority` (Showdown stores it on the queued move).
    pub move_priority: i8,
    /// Set once Prankster has raised this move's priority; it stays set.
    pub prankster: bool,
    /// The Pokémon in the targeted slot when the move was chosen.
    pub orig_target: Option<MonRef>,
    /// Champions: Curse queued by a Pokémon that was not a Ghost at the time
    /// is a move used on oneself from then on, whatever its user becomes.
    pub self_target: bool,
    /// What put this move at the head of the queue, if it matters to the move (`action.sourceEffect`).
    pub source: ActSource,
    /// Encore replaced this action and discarded its selected target.
    pub target_replaced: bool,
}

/// `action.sourceEffect` of a queued move.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ActSource {
    None,
    /// Another Pokémon's Round called it forward; `ignore_ability` is that move's.
    Round {
        ignore_ability: bool,
    },
    /// A switch brought about by this move of the Pokémon leaving (U-turn, Baton Pass).
    SelfSwitch(u16),
}

impl Action {
    /// The target type of the queued move (`action.move.target`).
    pub(crate) fn move_target(&self) -> Target {
        if self.self_target { Target::User } else { MOVES[self.move_id as usize].target }
    }
}

pub(crate) const QUEUE_CAP: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Queue {
    pub len: u8,
    pub items: [Action; QUEUE_CAP],
}

impl Queue {
    pub fn new() -> Self {
        let blank = Action {
            kind: ActKind::Residual,
            order: 0,
            priority: 0,
            speed: 0,
            mon: None,
            move_id: 0,
            target_loc: 0,
            switch_to: None,
            frac: 0,
            move_priority: 0,
            prankster: false,
            orig_target: None,
            self_target: false,
            source: ActSource::None,
            target_replaced: false,
        };
        Queue { len: 0, items: [blank; QUEUE_CAP] }
    }
    pub fn as_slice(&self) -> &[Action] {
        &self.items[..self.len as usize]
    }
    pub fn as_mut_slice(&mut self) -> &mut [Action] {
        let n = self.len as usize;
        &mut self.items[..n]
    }
    pub fn push(&mut self, a: Action) {
        self.items[self.len as usize] = a;
        self.len += 1;
    }
    pub fn insert(&mut self, at: usize, a: Action) {
        let n = self.len as usize;
        self.items.copy_within(at..n, at + 1);
        self.items[at] = a;
        self.len += 1;
    }
    pub fn shift(&mut self) -> Option<Action> {
        if self.len == 0 {
            return None;
        }
        let a = self.items[0];
        let n = self.len as usize;
        self.items.copy_within(1..n, 0);
        self.len -= 1;
        Some(a)
    }
    pub fn peek(&self) -> Option<&Action> {
        self.as_slice().first()
    }
    pub fn clear(&mut self) {
        self.len = 0;
    }
    /// Showdown's `cancelMove`: drop the queued move of `mon`.
    pub fn cancel_move(&mut self, mon: MonRef) -> bool {
        let n = self.len as usize;
        match self.items[..n].iter().position(|a| a.kind == ActKind::Move && a.mon == Some(mon)) {
            Some(i) => {
                self.items.copy_within(i + 1..n, i);
                self.len -= 1;
                true
            }
            None => false,
        }
    }
    /// Showdown's `cancelAction`: drop every queued action belonging to `mon`.
    pub fn cancel(&mut self, mon: MonRef) {
        let mut w = 0;
        for r in 0..self.len as usize {
            // Its switch-in effects (`runSwitch`) still happen.
            if self.items[r].mon != Some(mon) || self.items[r].kind == ActKind::RunSwitch {
                self.items[w] = self.items[r];
                w += 1;
            }
        }
        self.len = w as u8;
    }
}

/// Any effect that can cause something or own an event handler (Showdown's `Effect`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Eff {
    None,
    /// A move in use; the index is its slot in `Battle::am`.
    Move(u8),
    Status(Status),
    Vol(VolKind),
    Ability(u16),
    Item(u16),
    Weather(Weather),
    Terrain(Terrain),
    Pseudo(Pseudo),
    SideCond(SideCond),
    SlotCond(SlotCond),
    /// The pseudo-conditions Showdown names `recoil`, `drain` and `strugglerecoil`.
    Recoil,
    Drain,
    StruggleRecoil,
    /// The stand-in move Showdown uses for confusion self-damage: it counts
    /// as a move for effects that ask, but has no data of its own.
    Confused,
    /// A species as the cause of damage: Mimikyu's disguise breaking.
    Species,
    /// A condition named after a move, as the cause of damage to its own user:
    /// the crash of a High Jump Kick that missed, the price of Steel Beam. It
    /// is neither a move nor recoil to the effects that ask.
    Crash,
    /// A secondary effect's own data, which Showdown passes along as "the
    /// effect" when a secondary's `onHit` causes something without saying
    /// what did it. It is not a move (so Infiltrator does not carry over).
    Secondary,
}

impl Eff {
    /// Showdown's `effect.effectType === 'Move'`.
    pub fn is_move(self) -> bool {
        matches!(self, Eff::Move(_) | Eff::Confused)
    }
}

/// What an effect sits on (Showdown's `effectHolder`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Holder {
    Mon(MonRef),
    Side(u8),
    Field,
}

/// Showdown callbacks return numbers, booleans, `undefined`, `null` or `''`
/// (`NOT_FAIL`), and the caller branches on which one it got. This is that value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Res {
    Undef,
    NotFail,
    Null,
    Bool(bool),
    Num(i32),
    Mon(MonRef),
}

pub(crate) const TRUE: Res = Res::Bool(true);
pub(crate) const FALSE: Res = Res::Bool(false);
/// Showdown's `HIT_SUBSTITUTE`, which is the number 0.
pub(crate) const HIT_SUBSTITUTE: Res = Res::Num(0);

impl Res {
    pub fn truthy(self) -> bool {
        match self {
            Res::Bool(b) => b,
            Res::Num(n) => n != 0,
            Res::Mon(_) => true,
            _ => false,
        }
    }
    fn rank(self) -> u8 {
        match self {
            Res::Undef => 0,
            Res::NotFail => 1,
            Res::Null => 2,
            Res::Bool(_) => 3,
            Res::Num(_) | Res::Mon(_) => 4,
        }
    }
    /// "Counts as a hit": truthy, or exactly 0 damage.
    pub fn hit(self) -> bool {
        self.truthy() || self == Res::Num(0)
    }
    pub fn num(self) -> i32 {
        match self {
            Res::Num(n) => n,
            _ => 0,
        }
    }
    /// `BattleActions#combineResults`, branch for branch.
    #[allow(clippy::if_same_then_else)]
    pub fn combine(self, right: Res) -> Res {
        let left = self;
        if left.rank() > right.rank() {
            left
        } else if left.truthy() && !right.truthy() && right != Res::Num(0) {
            left
        } else if let (Res::Num(a), Res::Num(b)) = (left, right) {
            Res::Num(a + b)
        } else {
            right
        }
    }
}

/// Kinds of thing `Pokemon#runStatusImmunity` can be asked about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Imm {
    Status(Status),
    Vol(VolKind),
    Powder,
    Trapped,
    /// Damage from a weather (only sandstorm deals any).
    Weather(Weather),
}

/// The event being run (Showdown's `battle.event`), including the values that
/// travel with it as its relay variable.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Event {
    /// `None` outside any event.
    pub id: Option<Ev>,
    pub target: Option<MonRef>,
    /// The side, for the few events whose target is a side rather than a Pokémon.
    pub target_side: Option<u8>,
    pub source: Option<MonRef>,
    /// The effect that caused the event (`sourceEffect`).
    pub effect: Eff,
    /// Accumulated `chainModify` multiplier in 4096ths.
    pub modifier: u32,
    /// The relay variable when it is a number or boolean.
    pub relay: Res,
    /// Relay for the stat-stage events.
    pub boosts: Boosts,
    /// Relay for `SetStatus` / `AfterSetStatus`.
    pub status: Status,
    /// Relay for `TryAddVolatile`.
    pub vol: Option<VolKind>,
    /// Relay for the item events.
    pub item: u16,
    /// Relay for `Immunity`.
    pub imm: Option<Imm>,
    /// The defending type an `Effectiveness` event is about.
    pub typ: Type,
    /// Relay for `ModifySecondaries`: bit i set = secondary i still applies.
    pub secs: u8,
}

impl Event {
    pub const NONE: Event = Event {
        id: None,
        target: None,
        target_side: None,
        source: None,
        effect: Eff::None,
        modifier: 4096,
        relay: Res::Undef,
        boosts: [0; 7],
        status: Status::None,
        vol: None,
        item: 0,
        imm: None,
        typ: Type::None,
        secs: 0,
    };
    pub fn new(id: Ev, target: Option<MonRef>, source: Option<MonRef>, effect: Eff) -> Event {
        Event { id: Some(id), target, source, effect, ..Event::NONE }
    }
}

/// Showdown's `move.ignoreImmunity`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum IgnoreImm {
    No,
    All,
    /// Scrappy: Normal and Fighting moves hit Ghost types.
    NormalFighting,
}

/// Per-target results of a move (`Pokemon#getMoveHitData`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct HitData {
    pub crit: bool,
    pub type_mod: i8,
    pub bypass_protect: bool,
}

pub(crate) const MAX_SECS: usize = 3;

/// The mutable per-use copy of a move (Showdown's `ActiveMove`). Handlers
/// change its type, flags, secondaries and so on while it is being used.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ActiveMove {
    pub id: u16,
    /// PP payer whose randomly selected target is not otherwise publicly identifiable.
    /// Used only when Showdown strips the target from a failed move's log.
    pub hidden_pp_slot: Option<u8>,
    pub target_replaced: bool,
    pub typ: Type,
    pub category: Category,
    pub base_power: u16,
    /// 0 = never checks accuracy.
    pub accuracy: u8,
    pub priority: i8,
    pub target: Target,
    pub flags: u32,
    pub crit_ratio: u8,
    /// `None` is Showdown's `undefined`: roll for a critical hit.
    pub will_crit: Option<bool>,
    pub multihit: (u8, u8),
    /// The stat changes the move makes to its target (Growth doubles its own in the sun).
    pub boosts: Option<Boosts>,
    pub secs: [Secondary; MAX_SECS],
    pub n_secs: u8,
    /// Whether `secondaries` still exists (Sheer Force deletes it).
    pub has_secs: bool,
    pub self_boosts: Option<Boosts>,
    pub self_chance: u8,
    pub ignore_ability: bool,
    pub ignore_immunity: IgnoreImm,
    pub ignore_evasion: bool,
    pub ignore_defensive: bool,
    /// `overrideOffensiveStat` (0 = the category's own); Wonder Room can flip it.
    pub off_stat: u8,
    pub has_sheer_force: bool,
    pub prankster_boosted: bool,
    pub tracks_target: bool,
    /// Still set to split its hits between two targets (`smartTarget`); anything that stops one of them clears it.
    pub smart_target: bool,
    pub multiaccuracy: bool,
    pub infiltrates: bool,
    pub has_bounced: bool,
    /// The ability that changed this move's type and boosts it (Pixilate and so on).
    pub type_changer_boosted: Eff,
    /// The effect that called this move, if it was not chosen directly.
    pub source_effect: Eff,
    pub spread_hit: bool,
    pub self_dropped: bool,
    pub total_damage: i32,
    pub hit: u8,
    pub last_hit: bool,
    /// Indexed by side * 2 + position.
    pub hit_data: [HitData; 4],
    /// Parental Bond made this a two-hit move (`multihitType`).
    pub parental_bond: bool,
    /// The Pokémon whose Fairy Aura boosts this move.
    pub aura_booster: Option<MonRef>,
    /// The targets the move ended up hitting (`hitTargets`), once known.
    pub hit_targets: [MonRef; 3],
    pub n_hit_targets: u8,
    pub has_hit_targets: bool,
    /// Beat Up's `allies`: the base power of each hit still to come, and how many have been used.
    pub beat_up: [u8; MAX_TEAM],
    pub beat_up_used: u8,
    /// The damage a `damageCallback` just returned has another half a point
    /// to it (Metal Burst's one and a half times); a substitute counts it.
    pub half_damage: bool,
    /// Plain move data with no script callbacks: the hit of a future move.
    pub bare: bool,
    /// `selfSwitch`, which Parting Shot loses when it fails to lower anything.
    pub self_switch: SelfSwitch,
    /// The item Fling is throwing (0 before it has picked one up).
    pub fling_item: u16,
}

impl ActiveMove {
    pub fn d(&self) -> &'static MoveData {
        &MOVES[self.id as usize]
    }
    /// `dex.getActiveMove`: a fresh copy of the move's data.
    pub fn new(id: u16) -> ActiveMove {
        let d = &MOVES[id as usize];
        let blank = Secondary {
            chance: 0,
            status: Status::None,
            boosts: None,
            volatile: None,
            self_boosts: None,
            on_hit: false,
        };
        let mut secs = [blank; MAX_SECS];
        for (i, s) in d.secondaries.iter().enumerate() {
            secs[i] = *s;
        }
        ActiveMove {
            id,
            hidden_pp_slot: None,
            target_replaced: false,
            typ: d.typ,
            category: d.category,
            base_power: d.base_power,
            accuracy: d.accuracy,
            priority: d.priority,
            target: d.target,
            flags: d.flags,
            crit_ratio: d.crit_ratio,
            will_crit: if d.will_crit { Some(true) } else { None },
            multihit: d.multihit,
            boosts: d.boosts,
            secs,
            n_secs: d.secondaries.len() as u8,
            has_secs: !d.secondaries.is_empty(),
            self_boosts: d.self_boosts,
            self_chance: d.self_chance,
            ignore_ability: false,
            // Showdown resolves the default (status moves ignore type immunity) when it loads the move.
            ignore_immunity: if d.ignore_immunity { IgnoreImm::All } else { IgnoreImm::No },
            ignore_evasion: d.ignore_evasion,
            ignore_defensive: d.ignore_defensive,
            off_stat: d.off_stat,
            has_sheer_force: false,
            prankster_boosted: false,
            tracks_target: d.tracks_target,
            smart_target: d.smart_target,
            multiaccuracy: d.multiaccuracy,
            infiltrates: false,
            has_bounced: false,
            type_changer_boosted: Eff::None,
            source_effect: Eff::None,
            spread_hit: false,
            self_dropped: false,
            total_damage: 0,
            hit: 0,
            last_hit: false,
            hit_data: [HitData { crit: false, type_mod: 0, bypass_protect: false }; 4],
            parental_bond: false,
            aura_booster: None,
            hit_targets: [MonRef { side: 0, idx: 0 }; 3],
            n_hit_targets: 0,
            has_hit_targets: false,
            beat_up: [0; MAX_TEAM],
            beat_up_used: 0,
            half_damage: false,
            bare: false,
            self_switch: d.self_switch,
            fling_item: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FaintEntry {
    pub target: MonRef,
    pub source: Option<MonRef>,
    pub effect: Eff,
}

/// Active-move slots kept per action; nested move use (a bounced move) takes another slot.
pub(crate) const AM_CAP: usize = 6;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Battle {
    pub rng: Rng,
    pub sides: [Side; 2],
    pub field: Field,
    /// Volatile conditions, indexed by side and active position.
    pub(crate) vols: [[Volatiles; ACTIVE]; 2],
    pub turn: u16,
    pub request: Request,
    pub ended: bool,
    /// Winning side once `ended`; `None` there means a tie.
    pub winner: Option<u8>,
    pub(crate) queue: Queue,
    pub(crate) faint_queue: [FaintEntry; 8],
    pub(crate) n_faint: u8,
    pub(crate) mid_turn: bool,
    /// Showdown's `effectOrder` counter.
    pub(crate) effect_order: u32,
    pub(crate) next_uid: u16,
    /// Events some effect in this battle can listen to; the rest are skipped outright.
    pub(crate) event_mask: u128,
    /// The subset something listens to from the side (onAlly, onFoe, onAny, onSource).
    pub(crate) event_mask_pre: u128,
    pub(crate) event: Event,
    /// The effect whose handler is running (`battle.effect`) and the Pokémon it is on.
    pub(crate) effect: Eff,
    pub(crate) effect_holder: Option<Holder>,
    pub(crate) event_depth: u8,
    pub(crate) am: [ActiveMove; AM_CAP],
    pub(crate) am_len: u8,
    /// `battle.activeMove` as a slot in `am`, with `activePokemon` and `activeTarget`.
    pub(crate) active_move: Option<u8>,
    pub(crate) active_pokemon: Option<MonRef>,
    pub(crate) active_target: Option<MonRef>,
    /// `battle.lastMove`: the last move anyone got as far as using (`NO_MOVE` before the first).
    pub(crate) last_move: u16,
    /// The opening switch-ins have happened: every active slot has an occupant.
    pub(crate) started: bool,
    /// Field positions (side + 2 * position) in the speed order fixed at the last switch-in.
    pub(crate) speed_order: [u8; 4],
    pub(crate) n_speed_order: u8,
    /// Both players' team sheets are open: see [`Battle::with_rosters`].
    pub(crate) open_team_sheets: bool,
}
