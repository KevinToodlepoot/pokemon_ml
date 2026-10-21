//! The core of the simulator: a port of the parts of Pokémon Showdown's
//! `sim/battle.ts`, `sim/pokemon.ts` and `sim/battle-queue.ts` that the
//! supported mechanics exercise, under the Champions mod. Using a move is in
//! `moves.rs`, the event system in `events.rs`, and the public entry points
//! (`Battle::new`, `choose`, legal choices) in `choice.rs`.
//!
//! The port is deliberately literal: it draws random numbers in the same
//! order Showdown does, including the draws Showdown makes for reasons that
//! have no in-game meaning (re-resolving targets when re-sorting the queue,
//! shuffling speed ties among the handlers of every event). That is what
//! makes it possible to replay a Showdown battle here from the same seed and
//! compare state after every decision. Comments name the Showdown function
//! being mirrored.

// The control flow follows Showdown's on purpose, so keep its shape even where
// clippy would fold branches or loops together.
#![allow(clippy::needless_range_loop, clippy::collapsible_if, clippy::collapsible_match, clippy::if_same_then_else)]

use crate::data::*;
use crate::env::Counted;
use crate::state::*;
use crate::trace;

/// How a player describes one Pokémon to bring.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PokemonSet {
    pub species: u16,
    pub moves: Vec<u16>,
    /// (raised, lowered) stat indices 1..=5, or (0, 0) for a neutral nature.
    pub nature: (u8, u8),
    /// Champions stat points: hp, atk, def, spa, spd, spe.
    pub stat_points: [u8; 6],
    /// Index into `data::ABILITIES`.
    pub ability: u16,
    /// Index into `data::ITEMS`; 0 is no item.
    pub item: u16,
    pub gender: Gender,
}

impl PokemonSet {
    /// Builds a set from Showdown names or ids, e.g.
    /// `PokemonSet::from_names("Garchomp", &["Earthquake", "Protect"], "Jolly", [0, 32, 0, 0, 2, 32])`.
    /// Stat points are hp, atk, def, spa, spd, spe. The set starts with no
    /// ability and no item; add them with [`PokemonSet::ability`] and
    /// [`PokemonSet::item`]. The gender is the species' fixed gender, or male
    /// if it can be either; change it with [`PokemonSet::gender`].
    pub fn from_names(
        species: &str,
        moves: &[&str],
        nature_name: &str,
        stat_points: [u8; 6],
    ) -> Result<PokemonSet, Error> {
        let species =
            species_id(&to_id(species)).ok_or_else(|| Error::BadTeam(format!("unknown species {species}")))?;
        let moves = moves
            .iter()
            .map(|m| move_id(&to_id(m)).ok_or_else(|| Error::BadTeam(format!("unknown move {m}"))))
            .collect::<Result<Vec<_>, _>>()?;
        let nature = nature(nature_name).ok_or_else(|| Error::BadTeam(format!("unknown nature {nature_name}")))?;
        let gender = SPECIES[species as usize].gender.unwrap_or(Gender::M);
        Ok(PokemonSet { species, moves, nature, stat_points, ability: ab::NOABILITY, item: it::NONE, gender })
    }

    /// Sets the ability by Showdown name or id.
    pub fn ability(mut self, name: &str) -> Result<PokemonSet, Error> {
        self.ability = ability_id(&to_id(name)).ok_or_else(|| Error::BadTeam(format!("unknown ability {name}")))?;
        Ok(self)
    }

    /// Sets the held item by Showdown name or id; the empty string is no item.
    pub fn item(mut self, name: &str) -> Result<PokemonSet, Error> {
        self.item = item_id(&to_id(name)).ok_or_else(|| Error::BadTeam(format!("unknown item {name}")))?;
        Ok(self)
    }

    pub fn gender(mut self, gender: Gender) -> PokemonSet {
        self.gender = gender;
        self
    }
}

#[derive(Debug)]
pub enum Error {
    /// The team uses something that does not exist in Champions.
    Unsupported(String),
    BadTeam(String),
    BadChoice(String),
    /// A position description that does not make sense (see `position`).
    BadState(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Unsupported(s) => write!(f, "unsupported: {s}"),
            Error::BadTeam(s) => write!(f, "bad team: {s}"),
            Error::BadChoice(s) => write!(f, "bad choice: {s}"),
            Error::BadState(s) => write!(f, "bad position: {s}"),
        }
    }
}

impl std::error::Error for Error {}

/// An entry of Showdown's `targets` arrays, which are overwritten with
/// `false` as targets drop out of a move.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Tgt {
    Mon(MonRef),
    /// Showdown's `null`: the hit landed on a substitute. Nothing more happens
    /// to the target, but the move still counts as having hit.
    Sub,
    /// Showdown's `false`.
    Gone,
}

/// What `Pokemon#takeItem` came back with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Taken {
    NoItem,
    Blocked,
    Item(u16),
}

pub(crate) const MAX_TARGETS: usize = 3;
pub(crate) type Targets = [Tgt; MAX_TARGETS];
pub(crate) type Damage = [Res; MAX_TARGETS];

/// `Battle#modify` with a modifier in 4096ths. Showdown truncates the
/// product to 32 bits before rounding.
pub(crate) fn modify(value: u32, modifier: u32) -> u32 {
    let product = (value as u64 * modifier as u64) as u32;
    ((product as u64 + 2047) / 4096) as u32
}

/// A stat after stage changes (the boost table in `Pokemon#calculateStat`).
pub(crate) fn boosted(stat: u32, stage: i32) -> u32 {
    let s = stage.clamp(-6, 6);
    if s >= 0 { stat * (2 + s as u32) / 2 } else { stat * 2 / (2 + (-s) as u32) }
}

/// `Math.round(a * num / den)` for non-negative integers.
pub(crate) fn round_div(a: u32, num: u32, den: u32) -> u32 {
    (2 * a * num + den) / (2 * den)
}

/// Champions stats for a species with a nature and stat points
/// (`Battle#spreadModify` with the Champions `statModify`): max HP, then Atk..Spe.
pub(crate) fn calc_stats(species: u16, nature: (u8, u8), stat_points: [u8; 6]) -> [u16; 6] {
    let sp = &SPECIES[species as usize];
    let mut out = [0u16; 6];
    for k in 0..6 {
        let base = sp.base[k] as u32 + stat_points[k] as u32;
        let mut v = if k == 0 { base + 75 } else { base + 20 };
        // Natures are applied with 16-bit truncation.
        if k as u8 == nature.0 && nature.0 != nature.1 {
            v = ((v * 110) & 0xFFFF) / 100;
        } else if k as u8 == nature.1 && nature.0 != nature.1 {
            v = ((v * 90) & 0xFFFF) / 100;
        }
        out[k] = v as u16;
    }
    out
}

pub(crate) fn struggle_id() -> u16 {
    move_id("struggle").expect("struggle is always in the move table")
}

/// `Battle#comparePriority` for queued actions: order low to high, priority
/// high to low, speed high to low. Negative means `a` goes first.
pub(crate) fn cmp_action(a: &Action, b: &Action) -> i64 {
    let o = a.order as i64 - b.order as i64;
    if o != 0 {
        return o;
    }
    let p = b.priority as i64 - a.priority as i64;
    if p != 0 {
        return p;
    }
    b.speed as i64 - a.speed as i64
}

impl Battle {
    // ---------------------------------------------------------------- access

    pub fn mon(&self, r: MonRef) -> &Pokemon {
        &self.sides[r.side as usize].team[r.idx as usize]
    }
    pub(crate) fn mon_mut(&mut self, r: MonRef) -> &mut Pokemon {
        &mut self.sides[r.side as usize].team[r.idx as usize]
    }
    /// The Pokémon occupying an active slot (it may be fainted).
    pub fn active(&self, side: usize, pos: usize) -> MonRef {
        MonRef { side: side as u8, idx: self.sides[side].order[pos] }
    }
    /// The Pokémon at any position in a side's current order (active or benched).
    pub fn active_at(&self, side: usize, pos: usize) -> MonRef {
        MonRef { side: side as u8, idx: self.sides[side].order[pos] }
    }
    /// Whether an active slot's occupant has actually entered the battle.
    /// (Showdown's `side.active` holds nothing until the opening switch-ins.)
    pub(crate) fn in_play(&self, r: MonRef) -> bool {
        let m = self.mon(r);
        // (After the start every active slot has an occupant, even one that is
        // neither active nor fainted: a Pokémon revived where it lies.)
        m.is_active || m.fainted || (self.started && (m.position as usize) < ACTIVE)
    }
    /// `Battle#getAllActive`.
    pub(crate) fn all_active(&self, include_fainted: bool) -> ([MonRef; 4], usize) {
        let mut out = [MonRef { side: 0, idx: 0 }; 4];
        let mut n = 0;
        for side in 0..2 {
            for pos in 0..ACTIVE {
                let r = self.active(side, pos);
                if self.in_play(r) && (include_fainted || !self.mon(r).fainted) {
                    out[n] = r;
                    n += 1;
                }
            }
        }
        (out, n)
    }
    /// `Side#allies()`: this side's active Pokémon that still have HP.
    pub(crate) fn allies_and_self(&self, side: usize) -> ([MonRef; ACTIVE], usize) {
        let mut out = [MonRef { side: 0, idx: 0 }; ACTIVE];
        let mut n = 0;
        for pos in 0..ACTIVE {
            let r = self.active(side, pos);
            if self.mon(r).is_active && self.mon(r).hp > 0 {
                out[n] = r;
                n += 1;
            }
        }
        (out, n)
    }
    /// `Pokemon#adjacentAllies` in doubles: the partner, if neither has fainted.
    pub(crate) fn adjacent_allies(&self, r: MonRef) -> ([MonRef; ACTIVE], usize) {
        let mut out = [r; ACTIVE];
        let mut n = 0;
        if self.mon(r).fainted {
            return (out, 0);
        }
        let (allies, k) = self.allies_and_self(r.side as usize);
        for &a in &allies[..k] {
            if a != r && !self.mon(a).fainted {
                out[n] = a;
                n += 1;
            }
        }
        (out, n)
    }
    pub(crate) fn is_ally(&self, a: MonRef, b: MonRef) -> bool {
        a.side == b.side
    }
    /// `Pokemon#getLocOf`.
    pub(crate) fn loc_of(&self, from: MonRef, target: MonRef) -> i8 {
        let p = self.mon(target).position as i8 + 1;
        if from.side == target.side { -p } else { p }
    }
    /// `Pokemon#getAtLoc`.
    pub(crate) fn at_loc(&self, from: MonRef, loc: i8) -> Option<MonRef> {
        let side = if loc < 0 { from.side as usize } else { 1 - from.side as usize };
        let pos = loc.unsigned_abs() as usize;
        if pos == 0 || pos > ACTIVE {
            return None;
        }
        let r = self.active(side, pos - 1);
        self.in_play(r).then_some(r)
    }
    /// `pokemon.switchFlag === true`: marked to switch by anything but a move of its own.
    pub(crate) fn switch_flag_is_true(&self, r: MonRef) -> bool {
        let m = self.mon(r);
        m.switch_flag && m.switch_move == NO_MOVE
    }
    /// `Battle#canSwitch`.
    pub(crate) fn can_switch(&self, side: usize) -> bool {
        let s = &self.sides[side];
        if s.pokemon_left == 0 {
            return false;
        }
        (ACTIVE..s.n as usize).any(|p| self.switchable(side, s.order[p]))
    }
    /// One of `Battle#possibleSwitches`: a benched Pokémon that has not fainted and is not
    /// already queued to switch in.
    fn switchable(&self, side: usize, idx: u8) -> bool {
        let mon = MonRef { side: side as u8, idx };
        !self.mon(mon).fainted
            && !self
                .queue
                .as_slice()
                .iter()
                .any(|a| matches!(a.kind, ActKind::Switch | ActKind::InstaSwitch) && a.switch_to == Some(mon))
    }
    /// Index of a Pokémon's slot in per-move hit data.
    pub(crate) fn slot_index(&self, r: MonRef) -> usize {
        r.side as usize * 2 + (self.mon(r).position as usize).min(1)
    }

    /// The volatile conditions of a Pokémon. Only a Pokémon in an active
    /// position can have any.
    pub fn vols(&self, r: MonRef) -> &Volatiles {
        static EMPTY: Volatiles = Volatiles::new(VolKind::FIRST);
        let pos = self.mon(r).position as usize;
        if pos < ACTIVE { &self.vols[r.side as usize][pos] } else { &EMPTY }
    }
    pub(crate) fn vols_mut(&mut self, r: MonRef) -> Option<&mut Volatiles> {
        let pos = self.mon(r).position as usize;
        if pos < ACTIVE { Some(&mut self.vols[r.side as usize][pos]) } else { None }
    }
    pub(crate) fn vol_mut(&mut self, r: MonRef, kind: VolKind) -> Option<&mut Volatile> {
        self.vols_mut(r).and_then(|l| l.get_mut(kind))
    }
    /// Delete a volatile outright (Showdown's `delete pokemon.volatiles[id]`): no End event.
    pub(crate) fn drop_vol(&mut self, r: MonRef, kind: VolKind) -> bool {
        self.vols_mut(r).is_some_and(|l| l.remove(kind))
    }
    /// `side * 2 + position` of a Pokémon, for `Cond::source_slot`.
    pub(crate) fn field_slot(&self, r: MonRef) -> u8 {
        r.side * 2 + self.mon(r).position
    }

    // ------------------------------------------------------------------- rng

    pub(crate) fn rand(&mut self, n: u32, what: &'static str) -> u32 {
        let v = self.rng.below(n);
        trace::rng(what, n, v);
        v
    }
    pub(crate) fn chance(&mut self, num: u32, den: u32, what: &'static str) -> bool {
        self.rand(den, what) < num
    }
    /// `PRNG#random(from, to)`.
    pub(crate) fn rand_range(&mut self, from: usize, to: usize, what: &'static str) -> usize {
        let v = self.rng.range(from as u32, to as u32);
        trace::rng_range(what, from as u32, to as u32, v);
        v as usize
    }
    /// `PRNG#shuffle` on `items[start..end]`.
    fn shuffle<T>(&mut self, items: &mut [T], mut start: usize, end: usize, what: &'static str) {
        while start + 1 < end {
            let next = self.rand_range(start, end, what);
            if start != next {
                items.swap(start, next);
            }
            start += 1;
        }
    }
    /// `Battle#speedSort`: a selection sort that shuffles each group of ties.
    pub(crate) fn speed_sort<T: Copy>(&mut self, list: &mut [T], cmp: impl Fn(&T, &T) -> i64, what: &'static str) {
        if list.len() < 2 {
            return;
        }
        let mut sorted = 0;
        debug_assert!(list.len() <= 64);
        let mut next = [0u8; 64];
        while sorted + 1 < list.len() {
            next[0] = sorted as u8;
            let mut n_next = 1;
            for i in sorted + 1..list.len() {
                let delta = cmp(&list[next[0] as usize], &list[i]);
                if delta < 0 {
                    continue;
                }
                if delta > 0 {
                    next[0] = i as u8;
                    n_next = 1;
                } else {
                    next[n_next] = i as u8;
                    n_next += 1;
                }
            }
            for (i, &index) in next[..n_next].iter().enumerate() {
                if index as usize != sorted + i {
                    list.swap(sorted + i, index as usize);
                }
            }
            if n_next > 1 {
                self.shuffle(list, sorted, sorted + n_next, what);
            }
            sorted += n_next;
        }
    }

    // ----------------------------------------------------------------- stats

    /// `Pokemon#getStat` (stat index 1 = atk .. 5 = spe).
    pub(crate) fn get_stat(&mut self, r: MonRef, stat: usize, unboosted: bool, unmodified: bool) -> u32 {
        let mut value = self.mon(r).stats[stat] as u32;
        // Download ignores Wonder Room's swap of the stats, but then reads the
        // stage of the other defence.
        let stat = match stat {
            2 if unmodified && self.field.pseudo.has(Pseudo::Wonderroom) => 4,
            4 if unmodified && self.field.pseudo.has(Pseudo::Wonderroom) => 2,
            s => s,
        };
        if !unboosted {
            let mut boosts = self.mon(r).boosts;
            if !unmodified {
                boosts = self.modify_boost(r, boosts);
            }
            value = boosted(value, boosts[stat - 1] as i32);
        }
        if !unmodified {
            const EVS: [Ev; 5] = [Ev::ModifyAtk, Ev::ModifyDef, Ev::ModifySpA, Ev::ModifySpD, Ev::ModifySpe];
            value = self.run_event(EVS[stat - 1], Some(r), None, Eff::None, Res::Num(value as i32)).num() as u32;
        }
        if stat == 5 && value > 10000 {
            value = 10000;
        }
        value
    }

    /// The `ModifyBoost` event on a table of stat stages.
    pub(crate) fn modify_boost(&mut self, r: MonRef, boosts: Boosts) -> Boosts {
        if self.event_mask & Ev::ModifyBoost.bit() == 0 {
            return boosts;
        }
        let mut e = Event::new(Ev::ModifyBoost, Some(r), None, Eff::None);
        e.boosts = boosts;
        self.run_event_ex(e, Res::Undef, false, false).1.boosts
    }

    /// `Pokemon#calculateStat`: a stat at a given stage, as seen by `stat_user`.
    pub(crate) fn calculate_stat(&mut self, r: MonRef, stat: usize, boost: i32, stat_user: MonRef) -> u32 {
        // Wonder Room swaps the raw defences before anything else (the stage and
        // the modifiers applied afterwards are still those of the stat asked for).
        let raw = match stat {
            2 if self.field.pseudo.has(Pseudo::Wonderroom) => 4,
            4 if self.field.pseudo.has(Pseudo::Wonderroom) => 2,
            s => s,
        };
        let value = self.mon(r).stats[raw] as u32;
        let mut boosts = [0i8; 7];
        boosts[stat - 1] = boost as i8;
        let boosts = self.modify_boost(stat_user, boosts);
        boosted(value, boosts[stat - 1] as i32)
    }

    /// `Pokemon#getActionSpeed` under the Champions mod: Trick Room simply negates Speed.
    pub(crate) fn action_speed(&mut self, r: MonRef) -> i32 {
        let speed = self.get_stat(r, 5, false, false) as i32;
        if self.field.pseudo.has(Pseudo::Trickroom) { -speed } else { speed }
    }
    /// `Battle#updateSpeed`.
    pub(crate) fn update_speed(&mut self) {
        let (actives, n) = self.all_active(false);
        for &r in &actives[..n] {
            let s = self.action_speed(r);
            self.mon_mut(r).speed = s;
        }
    }

    // ----------------------------------------------------- types and immunity

    /// `Pokemon#getTypes`: the types that count right now. A roosting
    /// Pokémon is not Flying (Roost's `onType`, the only listener of the Type
    /// event, applied here directly); with no type left it is Normal; a type
    /// added by Forest's Curse or Trick-or-Treat comes last.
    pub(crate) fn get_types(&self, r: MonRef, exclude_added: bool) -> ([Type; 3], usize) {
        let m = self.mon(r);
        let added = if exclude_added { Type::None } else { m.added_type };
        types_now(m.types, self.vols(r).has(VolKind::Roost), added)
    }

    /// The base types `viewer`'s side takes `r` to have. What they are, as a
    /// rule; for one of the other side's in disguise, those of the Pokémon it
    /// passes for; and where the log has said something of them (a change that
    /// names the new types, or one that names only the Pokémon they were copied
    /// from), what that came to for this side. See [`TypesSaid`].
    pub(crate) fn seen_base(&self, r: MonRef, viewer: usize) -> [Type; 2] {
        let m = self.mon(r);
        let said = m.said.seen[viewer];
        if said != NOT_SAID {
            said
        } else if r.side as usize != viewer && m.illusion != 0 {
            SPECIES[m.live.species as usize].types
        } else {
            m.types
        }
    }

    /// [`Battle::get_types`] as `viewer`'s side sees it: a type added to a
    /// Pokémon is said aloud, and a roosting one is seen to roost.
    pub(crate) fn seen_types(&self, r: MonRef, viewer: usize) -> ([Type; 3], usize) {
        types_now(self.seen_base(r, viewer), self.vols(r).has(VolKind::Roost), self.mon(r).added_type)
    }

    /// Showdown's line at the end of a turn, `-start|named|typechange|types|[silent]`,
    /// for a Pokémon whose types are not the ones its log last gave it: what each
    /// side makes of it. The line gives the types of the Pokémon that `named`
    /// is or passes for, so its own side, which knows a disguise of its own,
    /// reads it rightly. The other side takes it for the Pokémon named if that
    /// one copied another's types this turn, and for its partner if only the
    /// partner did (then `named` must be a disguise of it).
    pub(crate) fn told_types(&mut self, named: MonRef, types: [Type; 2]) {
        let other = self.active(named.side as usize, 1 - self.mon(named).position as usize);
        let partner = Some(other).filter(|&p| p != named && self.mon(p).is_active && !self.mon(p).fainted);
        for viewer in 0..2 {
            let meant = match partner {
                Some(p) if viewer == named.side as usize => {
                    if self.mon(named).illusion != 0 {
                        p
                    } else {
                        named
                    }
                }
                Some(p) if !self.mon(named).said.copied && self.mon(p).said.copied => p,
                _ => named,
            };
            let m = self.mon_mut(meant);
            m.said.seen[viewer] = types;
            if viewer == meant.side as usize {
                m.said.told = true;
            }
        }
    }

    /// `Pokemon#hasType`.
    pub(crate) fn has_type(&self, r: MonRef, t: Type) -> bool {
        let (types, n) = self.get_types(r, false);
        types[..n].contains(&t)
    }

    /// `pokemon.getTypes().join() === type`: exactly this one type.
    pub(crate) fn is_only_type(&self, r: MonRef, t: Type) -> bool {
        let (types, n) = self.get_types(r, false);
        n == 1 && types[0] == t
    }

    /// `Pokemon#setType`: new base types; an added type is lost.
    pub(crate) fn set_type(&mut self, r: MonRef, types: [Type; 2]) -> bool {
        let m = self.mon_mut(r);
        m.types = types;
        m.added_type = Type::None;
        // (Said in so many words, to both sides: `-start|pokemon|typechange|types`.)
        (m.said.seen, m.said.apparent, m.said.known) = ([types; 2], types, true);
        // Showdown setType makes these types known. A later explicit change
        // supersedes Reflect Type, without requiring a silent turn-end revelation.
        (m.said.copied, m.said.copied_foe, m.said.told) = (false, false, false);
        true
    }

    /// `dex.getImmunity` for a status-like immunity kind: whether the
    /// Pokémon's types leave it open to it.
    pub(crate) fn type_allows(&self, r: MonRef, kind: usize) -> bool {
        let (types, n) = self.get_types(r, false);
        !types[..n].iter().any(|&t| t != Type::Typeless && STATUS_IMMUNE[kind][t as usize])
    }

    /// `Pokemon#runStatusImmunity`: false means immune.
    pub(crate) fn run_status_immunity(&mut self, r: MonRef, imm: Imm) -> bool {
        if self.mon(r).fainted {
            return false;
        }
        let natural = match imm {
            Imm::Status(Status::Brn) => self.type_allows(r, 0),
            Imm::Status(Status::Par) => self.type_allows(r, 1),
            Imm::Status(Status::Psn) | Imm::Status(Status::Tox) => self.type_allows(r, 2),
            Imm::Status(Status::Slp) => self.type_allows(r, 3),
            Imm::Status(Status::Frz) => self.type_allows(r, 4),
            Imm::Status(Status::None) => return true,
            Imm::Powder => self.type_allows(r, 5),
            Imm::Trapped => self.type_allows(r, 6),
            Imm::Weather(Weather::Sandstorm) => self.type_allows(r, 8),
            Imm::Weather(_) => true,
            // The volatile called `trapped` shares its name with the immunity Ghosts have.
            Imm::Vol(VolKind::Trapped) => self.type_allows(r, 6),
            Imm::Vol(_) => true,
        };
        if !natural {
            return false;
        }
        if self.event_mask & Ev::Immunity.bit() == 0 {
            return true;
        }
        let mut e = Event::new(Ev::Immunity, Some(r), None, Eff::None);
        e.imm = Some(imm);
        self.run_event_ex(e, Res::Undef, false, false).0.truthy()
    }

    /// `Pokemon#isGrounded`. Showdown returns `null` for Levitate, which every
    /// caller treats like `false`.
    pub(crate) fn is_grounded(&mut self, r: MonRef, negate_immunity: bool) -> bool {
        if self.field.pseudo.has(Pseudo::Gravity) {
            return true;
        }
        if self.has_vol_named(r, "ingrain") || self.has_vol_named(r, "smackdown") {
            return true;
        }
        let item = if self.ignoring_item(r) { it::NONE } else { self.mon(r).item };
        if item == it::IRONBALL {
            return true;
        }
        // (A Fire/Flying type that used Burn Up and then Roost is the exception to this rule.)
        if !negate_immunity
            && self.has_type(r, Type::Flying)
            && !(self.has_type(r, Type::Typeless) && self.has_vol_named(r, "roost"))
        {
            return false;
        }
        if (self.has_ability(r, ab::LEVITATE) || self.has_ability(r, ab::EELEVATE))
            && !self.suppressing_ability(Some(r))
        {
            return false;
        }
        if self.has_vol_named(r, "magnetrise") || self.has_vol_named(r, "telekinesis") {
            return false;
        }
        item != it::AIRBALLOON
    }

    /// The ability that keeps `r` off the ground, where `Pokemon#isGrounded`
    /// answers `null`: Levitate or Eelevate on a Pokémon nothing else grounds or lifts.
    fn levitating(&mut self, r: MonRef) -> Option<u16> {
        if self.field.pseudo.has(Pseudo::Gravity)
            || self.has_vol_named(r, "ingrain")
            || self.has_vol_named(r, "smackdown")
        {
            return None;
        }
        if !self.ignoring_item(r) && self.mon(r).item == it::IRONBALL {
            return None;
        }
        if self.has_type(r, Type::Flying) && !(self.has_type(r, Type::Typeless) && self.has_vol_named(r, "roost")) {
            return None;
        }
        if self.suppressing_ability(Some(r)) {
            return None;
        }
        [ab::LEVITATE, ab::EELEVATE].into_iter().find(|&a| self.has_ability(r, a))
    }

    /// `Pokemon#runImmunity` for a move: false means the move's type cannot hit.
    pub(crate) fn run_immunity(&mut self, target: MonRef, mi: u8) -> bool {
        self.run_immunity_ex(target, mi, false)
    }

    /// `runImmunity` with its `message` flag: said aloud, an immunity that
    /// comes from Levitate names it (`-immune|pokemon|[from] ability: Levitate`).
    pub(crate) fn run_immunity_ex(&mut self, target: MonRef, mi: u8, message: bool) -> bool {
        let am = &self.am[mi as usize];
        let typ = am.typ;
        match am.ignore_immunity {
            IgnoreImm::All => return true,
            IgnoreImm::NormalFighting if matches!(typ, Type::Normal | Type::Fighting) => return true,
            _ => {}
        }
        if typ == Type::Typeless || typ == Type::None {
            return true;
        }
        // The NegateImmunity event has no listeners among modelled effects.
        if typ == Type::Ground {
            let grounded = self.is_grounded(target, false);
            if !grounded && message {
                if let Some(a) = self.levitating(target) {
                    self.show_ability(target, a);
                }
            }
            return grounded;
        }
        let (types, n) = self.get_types(target, false);
        !types[..n].iter().any(|&t| t != Type::Typeless && TYPE_CHART[typ as usize][t as usize] == 3)
    }

    /// `Pokemon#runEffectiveness`.
    pub(crate) fn run_effectiveness(&mut self, target: MonRef, mi: u8) -> i32 {
        let typ = self.am[mi as usize].typ;
        let (types, n) = self.get_types(target, false);
        let mut total = 0;
        for &t in &types[..n] {
            let mut type_mod = type_effectiveness(typ, t);
            if self.move_has_cb(mi, Ev::Effectiveness) {
                // The move's own onEffectiveness(typeMod, target, type, move).
                let mut e = Event::new(Ev::Effectiveness, Some(target), None, Eff::Move(mi));
                e.typ = t;
                let me = Eff::Move(mi);
                type_mod = self
                    .single_event_ex(
                        Ev::Effectiveness,
                        Ev::Effectiveness,
                        Pre::On,
                        me,
                        None,
                        e,
                        Res::Num(type_mod),
                        true,
                    )
                    .num();
            }
            if self.event_mask & Ev::Effectiveness.bit() != 0 {
                let mut e = Event::new(Ev::Effectiveness, Some(target), None, Eff::Move(mi));
                e.typ = t;
                type_mod = self.run_event_ex(e, Res::Num(type_mod), false, false).0.num();
            }
            total += type_mod;
        }
        total
    }

    // ------------------------------------------------------------- targeting

    /// `Battle#validTargetLoc`.
    pub(crate) fn valid_target_loc(&self, loc: i8, source: MonRef, target: Target) -> bool {
        if loc == 0 {
            return true;
        }
        let num_slots = ACTIVE as i8;
        let source_loc = self.loc_of(source, source);
        if loc.abs() > num_slots {
            return false;
        }
        let is_self = source_loc == loc;
        let is_foe = loc > 0;
        let across = -(num_slots + 1 - loc);
        let is_adjacent = if loc > 0 { (across - source_loc).abs() <= 1 } else { (loc - source_loc).abs() == 1 };
        match target {
            Target::RandomNormal | Target::Scripted | Target::Normal => is_adjacent,
            Target::AdjacentAlly => is_adjacent && !is_foe,
            Target::AdjacentAllyOrSelf => (is_adjacent && !is_foe) || is_self,
            Target::AdjacentFoe => is_adjacent && is_foe,
            Target::Any => !is_self,
            _ => false,
        }
    }

    /// `Battle#getRandomTarget`. Note that `sample` draws even from a
    /// one-element list.
    pub(crate) fn get_random_target(&mut self, user: MonRef, target: Target) -> Option<MonRef> {
        match target {
            Target::User | Target::All | Target::AllySide | Target::AllyTeam | Target::AdjacentAllyOrSelf => Some(user),
            Target::AdjacentAlly => {
                let (adj, k) = self.adjacent_allies(user);
                if k == 0 { None } else { Some(adj[self.rand(k as u32, "random ally target") as usize]) }
            }
            _ => {
                let foe = 1 - user.side as usize;
                let (foes, n) = self.allies_and_self(foe);
                if n == 0 {
                    let r = self.active(foe, 0);
                    self.in_play(r).then_some(r)
                } else {
                    Some(foes[self.rand(n as u32, "random foe target") as usize])
                }
            }
        }
    }

    /// `Battle#getTarget`.
    pub(crate) fn get_target(
        &mut self,
        user: MonRef,
        target: Target,
        loc: i8,
        original: Option<MonRef>,
    ) -> Option<MonRef> {
        self.get_target_ex(user, None, target, loc, original)
    }

    /// `Battle#getTarget` for a queued move, which may track its target (Snipe
    /// Shot), pick its targets smartly (Dragon Darts) or be a future move.
    /// `target` is the queued move's target type, which can differ from the data's.
    pub(crate) fn get_target_ex(
        &mut self,
        user: MonRef,
        move_id: Option<u16>,
        target: Target,
        loc: i8,
        original: Option<MonRef>,
    ) -> Option<MonRef> {
        let d = move_id.map(|id| &MOVES[id as usize]);
        let tracks = d.is_some_and(|d| d.tracks_target);
        let smart = d.is_some_and(|d| d.smart_target);
        let future = d.is_some_and(|d| d.flags & F_FUTUREMOVE != 0);
        if tracks || self.has_ability(user, ab::STALWART) || self.has_ability(user, ab::PROPELLERTAIL) {
            if let Some(o) = original {
                if self.mon(o).is_active {
                    return Some(o);
                }
            }
        }
        if smart {
            return match self.at_loc(user, loc) {
                Some(t) if !self.mon(t).fainted => Some(t),
                _ => self.get_random_target(user, target),
            };
        }
        let self_loc = self.loc_of(user, user);
        if matches!(target, Target::AdjacentAlly | Target::Any | Target::Normal)
            && loc == self_loc
            && !self.vols(user).has(VolKind::Twoturnmove)
        {
            // Aimed at its own position (the user has moved since choosing): only a future move goes ahead.
            return future.then_some(user);
        }
        if target != Target::RandomNormal && self.valid_target_loc(loc, user, target) {
            if let Some(t) = self.at_loc(user, loc) {
                if self.mon(t).fainted {
                    if t.side == user.side {
                        if target == Target::AdjacentAllyOrSelf {
                            return Some(user);
                        }
                        // A fainted ally is not retargeted.
                        return Some(t);
                    }
                } else {
                    return Some(t);
                }
            }
        }
        self.get_random_target(user, target)
    }

    // ----------------------------------------------------------------- queue

    /// `Battle#getActionSpeed`. For moves this re-resolves the target for the
    /// `ModifyPriority` hooks; the lookup can draw from the RNG, so it has to
    /// happen even when nothing listens.
    pub(crate) fn resolve_speed(&mut self, a: &mut Action) {
        let Some(r) = a.mon else {
            a.speed = 1;
            return;
        };
        if a.kind == ActKind::Move {
            let d = &MOVES[a.move_id as usize];
            let mut priority = d.priority as i32;
            let target = self.get_target_ex(r, Some(a.move_id), a.move_target(), a.target_loc, None);
            let own = d.events & Ev::ModifyPriority.bit() != 0;
            if own || self.event_mask & Ev::ModifyPriority.bit() != 0 {
                // Handlers see the queued move itself; give it a scratch slot.
                let saved = self.am_len;
                let mi = self.new_am(a.move_id);
                self.am[mi as usize].prankster_boosted = a.prankster;
                // The move's own onModifyPriority(priority, source, target, move) (Grassy Glide).
                priority = self
                    .single_event(
                        Ev::ModifyPriority,
                        Eff::Move(mi),
                        None,
                        Some(r),
                        target,
                        Eff::None,
                        Res::Num(priority),
                    )
                    .num();
                priority = self.run_event(Ev::ModifyPriority, Some(r), target, Eff::Move(mi), Res::Num(priority)).num();
                a.prankster = self.am[mi as usize].prankster_boosted;
                self.am_len = saved;
            }
            a.priority = priority * 10 + a.frac as i32;
            a.move_priority = priority as i8;
        }
        a.speed = self.action_speed(r);
    }

    pub(crate) fn blank_action(kind: ActKind, order: u32) -> Action {
        Action {
            kind,
            order,
            priority: 0,
            speed: 1,
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
        }
    }

    /// `BattleQueue#resolveAction` for a Mega Evolution, which is queued ahead of the Pokémon's move.
    pub(crate) fn resolve_mega(&mut self, user: MonRef) -> Action {
        let mut a = Battle::blank_action(ActKind::MegaEvo, 104);
        a.mon = Some(user);
        self.resolve_speed(&mut a);
        a
    }

    /// `BattleQueue#resolveAction` for a move.
    pub(crate) fn resolve_move(&mut self, user: MonRef, move_id: u16, loc: i8) -> Action {
        let mut a = Battle::blank_action(ActKind::Move, 200);
        a.mon = Some(user);
        a.move_id = move_id;
        a.target_loc = loc;
        if self.event_mask & Ev::FractionalPriority.bit() != 0 {
            let saved = self.am_len;
            let mi = self.new_am(move_id);
            a.frac = self.run_event(Ev::FractionalPriority, Some(user), None, Eff::Move(mi), Res::Num(0)).num() as i8;
            self.am_len = saved;
        }
        a.self_target = move_id == mv::CURSE && !self.has_type(user, Type::Ghost);
        if a.target_loc == 0 {
            if let Some(t) = self.get_random_target(user, a.move_target()) {
                a.target_loc = self.loc_of(user, t);
            }
        }
        a.orig_target = self.at_loc(user, a.target_loc);
        self.resolve_speed(&mut a);
        a
    }

    /// `BattleQueue#resolveAction` for a switch chosen at a move or switch request.
    pub(crate) fn resolve_switch(&mut self, kind: ActKind, out: MonRef, incoming: MonRef) -> Action {
        let mut a = Battle::blank_action(kind, if kind == ActKind::InstaSwitch { 3 } else { 103 });
        a.mon = Some(out);
        a.switch_to = Some(incoming);
        let m = self.mon_mut(out);
        if m.switch_flag && m.switch_move != NO_MOVE {
            a.source = ActSource::SelfSwitch(m.switch_move);
        }
        m.switch_flag = false;
        m.switch_move = NO_MOVE;
        self.resolve_speed(&mut a);
        a
    }

    /// `BattleQueue#insertChoice`: place an action where it would have sorted,
    /// picking a random spot among actions it ties with.
    pub(crate) fn insert_choice(&mut self, a: Action) -> usize {
        let mut first = None;
        let mut last = None;
        for (i, cur) in self.queue.as_slice().iter().enumerate() {
            let c = cmp_action(&a, cur);
            if c <= 0 && first.is_none() {
                first = Some(i);
            }
            if c < 0 {
                last = Some(i);
                break;
            }
        }
        match first {
            None => {
                self.queue.push(a);
                self.queue.len as usize - 1
            }
            Some(f) => {
                let l = last.unwrap_or(self.queue.len as usize);
                let index = if f == l { f } else { self.rand_range(f, l + 1, "insert choice tie") };
                self.queue.insert(index, a);
                index
            }
        }
    }

    pub(crate) fn sort_queue(&mut self) {
        let mut q = self.queue;
        let n = q.len as usize;
        self.speed_sort(&mut q.items[..n], cmp_action, "queue sort tie");
        self.queue = q;
    }

    /// `BattleQueue#cancelMove`.
    pub(crate) fn cancel_move(&mut self, r: MonRef) -> bool {
        self.queue.cancel_move(r)
    }

    /// `Battle#swapPosition`: `r` trades places with whoever is at `new_pos` on its side.
    pub(crate) fn swap_position(&mut self, r: MonRef, new_pos: usize) -> bool {
        let side = r.side as usize;
        let other = self.active(side, new_pos);
        if new_pos != 1 && self.mon(other).fainted {
            return false;
        }
        let old_pos = self.mon(r).position as usize;
        self.sides[side].order[old_pos] = other.idx;
        self.sides[side].order[new_pos] = r.idx;
        self.mon_mut(other).position = old_pos as u8;
        self.mon_mut(r).position = new_pos as u8;
        // Volatile conditions are stored by position; they go with their Pokémon.
        self.vols[side].swap(old_pos, new_pos);
        self.run_event(Ev::Swap, Some(other), Some(r), Eff::None, Res::Undef);
        self.run_event(Ev::Swap, Some(r), Some(other), Eff::None, Res::Undef);
        true
    }

    /// `BattleQueue#prioritizeAction` for the queued action at `at`: it happens next.
    pub(crate) fn prioritize_at(&mut self, at: usize, source: ActSource) {
        let n = self.queue.len as usize;
        let mut a = self.queue.items[at];
        self.queue.items.copy_within(at + 1..n, at);
        self.queue.len -= 1;
        a.source = source;
        a.order = 3;
        self.queue.insert(0, a);
    }

    /// Where in the queue `r`'s move is (`BattleQueue#willMove`).
    pub(crate) fn will_move_at(&self, r: MonRef) -> Option<usize> {
        if self.mon(r).fainted {
            return None;
        }
        self.queue.as_slice().iter().position(|a| a.kind == ActKind::Move && a.mon == Some(r))
    }

    /// `BattleQueue#willAct`.
    pub(crate) fn will_act(&self) -> bool {
        self.queue.as_slice().iter().any(|a| matches!(a.kind, ActKind::Move | ActKind::Switch | ActKind::InstaSwitch))
    }

    /// `BattleQueue#willMove`.
    pub(crate) fn will_move(&self, r: MonRef) -> bool {
        if self.mon(r).fainted {
            return false;
        }
        self.queue.as_slice().iter().any(|a| a.kind == ActKind::Move && a.mon == Some(r))
    }

    /// The move `r` is still due to use this turn (`willMove(r)?.moveid`).
    pub(crate) fn queued_move(&self, r: MonRef) -> Option<u16> {
        if self.mon(r).fainted {
            return None;
        }
        self.queue.as_slice().iter().find(|a| a.kind == ActKind::Move && a.mon == Some(r)).map(|a| a.move_id)
    }

    /// `BattleQueue#changeAction` to a move with no target given: everything
    /// the Pokémon had queued is dropped (a pending Mega Evolution included)
    /// and the new move goes where it would have sorted.
    pub(crate) fn change_action(&mut self, r: MonRef, move_id: u16) {
        self.queue.cancel(r);
        let s = self.action_speed(r);
        self.mon_mut(r).speed = s;
        let (mut actions, n) = self.resolve_move_actions(r, move_id, 0, false);
        for action in &mut actions[..n] {
            action.target_replaced = true;
        }
        // Showdown places the whole group where its first action sorts.
        let at = self.insert_choice(actions[0]);
        for (k, a) in actions[1..n].iter().enumerate() {
            self.queue.insert(at + 1 + k, *a);
        }
    }

    /// `BattleQueue#resolveAction` for a chosen move: the move's action,
    /// preceded by the actions that go with it. Showdown resolves them in the
    /// order "before-turn callback, Mega Evolution, priority charge, the
    /// move" (each may draw a random target) and queues them in the order
    /// returned here.
    pub(crate) fn resolve_move_actions(
        &mut self,
        user: MonRef,
        move_id: u16,
        loc: i8,
        mega: bool,
    ) -> ([Action; 4], usize) {
        let d = &MOVES[move_id as usize];
        let blank = Battle::blank_action(ActKind::Move, 200);
        let (mut before, mut evo, mut charge) = (None, None, None);
        if d.events & Ev::BeforeTurnCallback.bit() != 0 {
            before = Some(self.resolve_move_extra(ActKind::BeforeTurnMove, 5, user, move_id, loc));
        }
        if mega {
            evo = Some(self.resolve_mega(user));
        }
        if d.events & Ev::PriorityChargeCallback.bit() != 0 {
            charge = Some(self.resolve_move_extra(ActKind::PriorityCharge, 107, user, move_id, 0));
        }
        let the_move = self.resolve_move(user, move_id, loc);
        let mut out = [blank; 4];
        let mut n = 0;
        for a in [charge, evo, before, Some(the_move)].into_iter().flatten() {
            out[n] = a;
            n += 1;
        }
        (out, n)
    }

    /// `resolveAction` for an action that carries a move without being one:
    /// it still gets a target, picked at random if none was chosen.
    fn resolve_move_extra(&mut self, kind: ActKind, order: u32, user: MonRef, move_id: u16, loc: i8) -> Action {
        let mut a = Battle::blank_action(kind, order);
        a.mon = Some(user);
        a.move_id = move_id;
        a.target_loc = loc;
        if a.target_loc == 0 {
            if let Some(t) = self.get_random_target(user, MOVES[move_id as usize].target) {
                a.target_loc = self.loc_of(user, t);
            }
        }
        a.orig_target = self.at_loc(user, a.target_loc);
        self.resolve_speed(&mut a);
        a
    }

    // ------------------------------------------------------------- switching

    /// `Pokemon#clearVolatile` (the Champions version).
    pub(crate) fn clear_volatile(&mut self, r: MonRef, include_switch_flags: bool) {
        let m = self.mon_mut(r);
        m.boosts = [0; 7];
        if m.transformed {
            // Its own moves come back, with the PP they had.
            m.moves = m.base_moves;
            m.unknown_pp = m.base_unknown_pp;
            m.base_unknown_pp = 0;
            m.n_moves = m.base_n_moves;
            m.transformed = false;
        }
        m.ability = m.base_ability;
        // Linked volatiles (`trapped` and `trapper`) release their other halves.
        let trapped_by = self.vols(r).get(VolKind::Trapped).and_then(|v| v.source);
        let was_trapper = self.vols(r).has(VolKind::Trapper);
        if let Some(list) = self.vols_mut(r) {
            list.clear();
        }
        if trapped_by.is_some() {
            self.unlink_volatile(r, VolKind::Trapped, trapped_by);
        }
        if was_trapper {
            self.unlink_volatile(r, VolKind::Trapper, None);
        }
        let m = self.mon_mut(r);
        if include_switch_flags {
            m.switch_flag = false;
            m.switch_move = NO_MOVE;
            m.force_switch_flag = false;
        }
        m.being_called_back = false;
        m.move_this_turn = Res::Undef;
        m.move_last_turn = Res::Undef;
        m.was_attacked = false;
        m.last_attack_damage = 0;
        m.last_move = NO_MOVE;
        m.last_attempted_move = None;
        m.locked_move = NO_MOVE;
        m.newly_switched = true;
        m.hurt_this_turn = 0;
        m.times_attacked = 0;
        m.hit_by_this_turn = 0;
        m.n_damaged_by = 0;
        let base = m.base_species;
        self.set_species(r, base);
        self.mon_mut(r).said = TypesSaid::fresh(SPECIES[base as usize].types);
    }

    /// `Pokemon#setSpecies`: species, types and stats (never max HP, which
    /// is fixed when the Pokémon is created), and the cached speed back to
    /// the raw stat. The `ModifySpecies` event only has rule listeners.
    pub(crate) fn set_species(&mut self, r: MonRef, species: u16) {
        let m = self.mon_mut(r);
        let stats = calc_stats(species, m.nature, m.stat_points);
        m.species = species;
        m.types = SPECIES[species as usize].types;
        m.added_type = Type::None;
        (m.said.seen, m.said.apparent, m.said.known) = ([NOT_SAID; 2], m.types, true);
        m.stats[1..].copy_from_slice(&stats[1..]);
        m.speed = m.stats[5] as i32;
    }

    /// `Pokemon#updateMaxHp`: the species changed for good; keep the damage taken.
    fn update_max_hp(&mut self, r: MonRef) {
        let m = self.mon_mut(r);
        let new = calc_stats(m.species, m.nature, m.stat_points)[0];
        let old = m.stats[0];
        if new == old {
            return;
        }
        if m.hp > 0 {
            m.hp = (new as i32 - (old as i32 - m.hp as i32)).max(1) as u16;
        }
        m.stats[0] = new;
    }

    /// `Pokemon#formeChange(species, item, true)`: the permanent forme change
    /// of Mega Evolution. The new forme brings its own ability.
    fn mega_forme_change(&mut self, r: MonRef, species: u16) {
        // Mega Evolution counts as having acted.
        self.mon_mut(r).move_this_turn = TRUE;
        // `-mega|pokemon|Species|Stone`: the stone is named, and a Mega has the one ability.
        let stone = self.mon(r).item;
        let named = match self.mon(r).illusion {
            0 => SPECIES[species as usize].base_species,
            _ => SPECIES[self.sides[r.side as usize].team[self.shown_as(r)].species as usize].id,
        };
        let ability = crate::shown::mega_ability(stone, named);
        self.sides[r.side as usize].mega_used = true;
        self.show_item_gain(r, stone);
        let rec = self.shown_mut(r);
        (rec.ability, rec.base_ability, rec.ability_changed) = (ability, ability, false);
        self.forme_change(r, species, true, true);
    }

    /// `Pokemon#formeChange` as Champions has it. A permanent change (a Mega
    /// Evolution, Palafin's, a broken Disguise) also becomes the forme the
    /// Pokémon returns to, and unless `keep_ability` (Disguise) brings the new
    /// forme's own ability.
    pub(crate) fn forme_change(&mut self, r: MonRef, species: u16, permanent: bool, new_ability: bool) {
        self.set_species(r, species);
        if !permanent {
            return;
        }
        self.mon_mut(r).base_species = species;
        self.shown_new_forme(r, species);
        self.update_max_hp(r);
        if new_ability {
            if self.mon(r).illusion != 0 {
                // Showdown blanks the ability first, so that Illusion does not get to end.
                self.mon_mut(r).ability = ab::NOABILITY;
            }
            let ability = SPECIES[species as usize].ability0;
            self.set_ability_ex(r, ability, None, Eff::None, true);
            self.mon_mut(r).base_ability = ability;
        }
    }

    /// `Pokemon#transformInto`: take on another Pokémon's species, types, stats
    /// (not HP), stat stages, moves (5 PP each) and ability.
    pub(crate) fn transform_into(&mut self, r: MonRef, target: MonRef) -> bool {
        let t = self.mon(target);
        if t.fainted
            || self.mon(r).illusion != 0
            || t.illusion != 0
            || self.vols(target).has(VolKind::Substitute)
            || t.transformed
            || self.mon(r).transformed
        {
            return false;
        }
        let t = *self.mon(target);
        // `setSpecies(species, effect, true)`: the cached speed is left at what this
        // Pokémon's own training would give the new species, until the next update.
        self.set_species(r, t.species);
        {
            let m = self.mon_mut(r);
            m.transformed = true;
            m.types = t.types;
            m.added_type = t.added_type;
            // Each side sees on it what it sees on the one it turned into.
            m.said.seen = t.said.seen;
            m.said.apparent = t.said.apparent;
            m.said.known = r.side == target.side && t.said.known;
            m.said.copied = true;
            m.stats[1..].copy_from_slice(&t.stats[1..]);
            m.base_moves = m.moves;
            m.base_unknown_pp = m.unknown_pp;
            m.unknown_pp = 0;
            m.base_n_moves = m.n_moves;
            m.n_moves = t.n_moves;
            for k in 0..t.n_moves as usize {
                let id = t.moves[k].id;
                let pp = MOVES[id as usize].base_pp.min(5);
                m.moves[k] = MoveSlot { id, pp, maxpp: pp, disabled: false, hidden: false, used: false };
            }
            m.times_attacked = t.times_attacked;
            m.boosts = t.boosts;
        }
        // The critical-hit volatiles come along too, started afresh.
        for kind in [VolKind::Dragoncheer, VolKind::Focusenergy] {
            self.remove_volatile(r, kind);
        }
        for kind in [VolKind::Dragoncheer, VolKind::Focusenergy] {
            if let Some(theirs) = self.vols(target).get(kind).map(|v| v.data) {
                self.add_volatile(r, kind, None, Eff::None);
                if kind == VolKind::Dragoncheer {
                    if let Some(v) = self.vol_mut(r, kind) {
                        v.data = theirs;
                    }
                }
            }
        }
        // `-transform|pokemon|target|[from] ability: Imposter`: it has the ability its model has.
        if let Eff::Ability(a) = self.effect {
            self.show_ability(r, a);
        }
        let theirs = self.shown_mut(target).ability;
        self.show_ability_change(r, theirs);
        // `setAbility(ability, this, null, true, true)`: no one is asked, and an
        // ability the Pokémon already had does not start again.
        let (old, new) = (self.mon(r).ability, t.ability);
        self.single_event(Ev::End, Eff::Ability(old), Some(r), Some(r), Some(r), Eff::None, Res::Undef);
        self.mon_mut(r).ability = new;
        self.listen(ABILITIES[new as usize].events, ABILITIES[new as usize].events_pre);
        self.new_ability_state(r);
        if old != new {
            self.single_event(Ev::Start, Eff::Ability(new), Some(r), Some(r), Some(r), Eff::None, Res::Undef);
        }
        true
    }

    /// `BattleActions#runMegaEvo`.
    fn run_mega_evo(&mut self, r: MonRef) {
        let species = self.mon(r).can_mega;
        if species == NO_SPECIES {
            return;
        }
        self.mega_forme_change(r, species);
        // One Mega Evolution per side.
        let side = &mut self.sides[r.side as usize];
        for i in 0..side.n as usize {
            side.team[i].can_mega = NO_SPECIES;
        }
        self.run_event(Ev::AfterMega, Some(r), None, Eff::None, Res::Undef);
    }

    /// `BattleActions#switchIn`. `via` is the `selfSwitch` of the move that
    /// brought the switch about, if one did; `is_drag` says the Pokémon is being
    /// dragged in (Roar, Red Card), in which case its entrance happens at once.
    pub(crate) fn switch_in(
        &mut self,
        incoming: MonRef,
        pos: usize,
        at_start: bool,
        via: SelfSwitch,
        is_drag: bool,
    ) -> bool {
        if self.mon(incoming).is_active {
            return false;
        }
        let mut copied_gastro = false;
        let side = incoming.side as usize;
        if !at_start {
            let old = self.active(side, pos);
            let mut passed = None;
            if self.mon(old).hp > 0 {
                self.mon_mut(old).being_called_back = true;
                if !is_drag {
                    self.run_pursuit_activation(old);
                }
                if !self.run_event(Ev::SwitchOut, Some(old), None, Eff::None, Res::Undef).truthy() {
                    return false;
                }
                if self.mon(old).hp == 0 {
                    // Fainted before it could leave.
                    return false;
                }
                let (ability, item) = (self.mon(old).ability, self.mon(old).item);
                self.single_event(Ev::End, Eff::Ability(ability), Some(old), Some(old), None, Eff::None, Res::Undef);
                self.single_event(Ev::End, Eff::Item(item), Some(old), Some(old), None, Eff::None, Res::Undef);
                self.queue.cancel(old);
                // `Pokemon#copyVolatileFrom`: Baton Pass hands on stat stages and
                // every volatile condition that can be copied, Shed Tail just the substitute.
                if via == SelfSwitch::CopyVolatile || via == SelfSwitch::ShedTail {
                    self.clear_volatile(incoming, true);
                    let mut list = *self.vols(old);
                    let mut kept = list.take_where(|k| {
                        !k.data().no_copy && (via == SelfSwitch::CopyVolatile || k == VolKind::Substitute)
                    });
                    // Each copy is a fresh state, made for a Pokémon that is not on the field yet.
                    for i in 0..kept.as_slice().len() {
                        let kind = kept.as_slice()[i].kind;
                        self.next_uid = self.next_uid.wrapping_add(1);
                        if let Some(v) = kept.get_mut(kind) {
                            let from_uid = v.st.uid;
                            v.st.order = 0;
                            v.st.uid = self.next_uid;
                            // (A confusion or a binding goes on with what is left of it: its count with it, D97.)
                            let counted =
                                [Counted::Confusion, Counted::Trap].into_iter().find(|c| c.volatile() == Some(kind));
                            if let Some(counted) = counted {
                                self.note_count_passed(old, counted, from_uid, self.next_uid);
                            }
                        }
                    }
                    let boosts = if via == SelfSwitch::CopyVolatile { self.mon(old).boosts } else { [0; 7] };
                    passed = Some((kept, boosts));
                }
                self.clear_volatile(old, true);
            }
            let old_new_pos = self.mon(incoming).position;
            self.shown_leave(old, false);
            {
                let o = self.mon_mut(old);
                o.is_active = false;
                o.used_item_this_turn = false;
                o.stats_raised_this_turn = false;
                o.stats_lowered_this_turn = false;
                o.skip_before_switch_out = false;
                o.position = old_new_pos;
                if o.fainted {
                    o.status = Status::None;
                }
            }
            self.mon_mut(incoming).position = pos as u8;
            self.sides[side].order[pos] = incoming.idx;
            self.sides[side].order[old_new_pos as usize] = old.idx;
            if let Some((list, boosts)) = passed {
                copied_gastro = list.has(VolKind::Gastroacid);
                self.vols[side][pos] = list;
                self.mon_mut(incoming).boosts = boosts;
                for i in 0..list.as_slice().len() {
                    let kind = list.as_slice()[i].kind;
                    if self.vols(incoming).has(kind) {
                        let eff = Eff::Vol(kind);
                        self.single_event(Ev::Copy, eff, Some(incoming), Some(incoming), None, Eff::None, Res::Undef);
                    }
                }
            }
        }
        {
            let m = self.mon_mut(incoming);
            m.is_active = true;
            m.active_turns = 0;
            m.active_move_actions = 0;
            for k in 0..m.n_moves as usize {
                m.moves[k].used = false;
            }
        }
        self.new_ability_state(incoming);
        let has_item = self.mon(incoming).item != it::NONE;
        let st = self.new_state(has_item, incoming);
        self.mon_mut(incoming).item_st = st;
        self.run_event(Ev::BeforeSwitchIn, Some(incoming), None, Eff::None, Res::Undef);
        // The `switch` line.
        self.shown_enter(incoming);
        self.mon_mut(incoming).live.copied_gastro = copied_gastro;
        self.note_speed_switch(incoming);
        // `queue.insertChoice({choice: 'runSwitch', pokemon})`: the speed is
        // computed once to cache it and once more for the action.
        if is_drag {
            // So that Mold Breaker's move can still be the active one when the hazards strike.
            self.run_switch(incoming);
            return true;
        }
        let s = self.action_speed(incoming);
        self.mon_mut(incoming).speed = s;
        let mut a = Battle::blank_action(ActKind::RunSwitch, 101);
        a.mon = Some(incoming);
        a.speed = self.action_speed(incoming);
        self.insert_choice(a);
        true
    }

    /// `BattleActions#runPursuitActivation`: the BeforeSwitchOut event of a Pokémon about to
    /// leave, once (`skip_before_switch_out` is Showdown's `skipPursuit`). Champions has no
    /// Pursuit, so nothing has fainted here that would need resolving.
    fn run_pursuit_activation(&mut self, r: MonRef) {
        if self.mon(r).skip_before_switch_out {
            return;
        }
        self.run_event(Ev::BeforeSwitchOut, Some(r), None, Eff::None, Res::Undef);
        self.clear_active_move(false);
        self.each_event(Ev::Update);
        // Asked again only if the switch was called off.
        let m = self.mon_mut(r);
        m.skip_before_switch_out = !(m.hp > 0 && !m.switch_flag);
    }

    /// `BattleActions#dragIn`: a random teammate takes the place of whoever is at `pos`.
    pub(crate) fn drag_in(&mut self, side: usize, pos: usize) -> bool {
        // `getRandomSwitchable`: the draw is made even when there is only one candidate.
        let s = &self.sides[side];
        let mut bench = [0u8; MAX_TEAM];
        let mut n = 0;
        if s.pokemon_left > 0 {
            for p in ACTIVE..s.n as usize {
                if self.switchable(side, s.order[p]) {
                    bench[n] = s.order[p];
                    n += 1;
                }
            }
        }
        if n == 0 {
            return false;
        }
        let incoming = MonRef { side: side as u8, idx: bench[self.rand(n as u32, "dragged in") as usize] };
        if self.mon(incoming).is_active {
            return false;
        }
        let old = self.active(side, pos);
        if self.mon(old).hp == 0 {
            return false;
        }
        if !self.run_event(Ev::DragOut, Some(old), None, Eff::None, Res::Undef).truthy() {
            return false;
        }
        self.switch_in(incoming, pos, false, SelfSwitch::No, true)
    }

    /// `BattleActions#runSwitch`.
    pub(crate) fn run_switch(&mut self, first: MonRef) {
        let mut switchers = [first; 4];
        let mut n = 1;
        while let Some(a) = self.queue.peek() {
            if a.kind != ActKind::RunSwitch {
                break;
            }
            switchers[n] = self.queue.shift().unwrap().mon.unwrap();
            n += 1;
        }
        // Showdown sorts everyone on the field (fainted included) by cached
        // speed here; switch-in handlers then use that fixed order.
        let (actives, k) = self.all_active(true);
        let mut keyed = [(actives[0], 0i32); 4];
        for i in 0..k {
            keyed[i] = (actives[i], self.mon(actives[i]).speed);
        }
        self.speed_sort(&mut keyed[..k], |a, b| b.1 as i64 - a.1 as i64, "switch-in speed tie");
        for i in 0..k {
            let r = keyed[i].0;
            self.speed_order[i] = r.side + 2 * self.mon(r).position;
        }
        self.n_speed_order = k as u8;
        self.field_event(Ev::SwitchIn, Some(&switchers[..n]));
    }

    // -------------------------------------------------------- damage, healing

    /// `Pokemon#faint`: queue the faint; it resolves in `faint_messages`.
    pub(crate) fn faint(&mut self, r: MonRef, source: Option<MonRef>, effect: Eff) -> u32 {
        let m = self.mon_mut(r);
        if m.fainted || m.faint_queued {
            return 0;
        }
        let d = m.hp as u32;
        m.hp = 0;
        m.switch_flag = false;
        m.switch_move = NO_MOVE;
        m.faint_queued = true;
        self.faint_queue[self.n_faint as usize] = FaintEntry { target: r, source, effect };
        self.n_faint += 1;
        d
    }

    /// `Pokemon#damage`: returns the HP actually lost.
    pub(crate) fn damage_mon(&mut self, r: MonRef, d: i32, source: Option<MonRef>, effect: Eff) -> u32 {
        let hp = self.mon(r).hp as i32;
        if hp == 0 || d <= 0 {
            return 0;
        }
        if d >= hp {
            self.faint(r, source, effect);
            hp as u32
        } else {
            self.mon_mut(r).hp = (hp - d) as u16;
            d as u32
        }
    }

    /// The defaults Showdown fills in from the running event when a callback
    /// leaves `target`, `source` or `effect` out.
    fn event_defaults(
        &self,
        target: Option<MonRef>,
        source: Option<MonRef>,
        effect: Eff,
    ) -> (Option<MonRef>, Option<MonRef>, Eff) {
        (
            target.or(self.event.target),
            source.or(self.event.source),
            if effect == Eff::None { self.effect } else { effect },
        )
    }

    /// `Battle#damage`: one target, with the `Damage` event. Fractions must be
    /// floored by the caller (see `div1`).
    pub(crate) fn damage(&mut self, d: i32, target: Option<MonRef>, source: Option<MonRef>, effect: Eff) -> Res {
        let (target, source, effect) = self.event_defaults(target, source, effect);
        let mut dmg: Damage = [Res::Num(d), Res::Undef, Res::Undef];
        let tgts: Targets = [target.map_or(Tgt::Gone, Tgt::Mon), Tgt::Gone, Tgt::Gone];
        self.spread_damage(&mut dmg, &tgts, 1, source, effect);
        dmg[0]
    }

    /// `Battle#spreadDamage`.
    pub(crate) fn spread_damage(
        &mut self,
        damage: &mut Damage,
        targets: &Targets,
        n: usize,
        source: Option<MonRef>,
        effect: Eff,
    ) {
        for i in 0..n {
            let cur = damage[i];
            let Tgt::Mon(target) = targets[i] else {
                if cur.truthy() || cur == Res::Num(0) {
                    damage[i] = Res::Num(0);
                }
                continue;
            };
            let Res::Num(mut amount) = cur else {
                continue;
            };
            if self.mon(target).hp == 0 {
                damage[i] = Res::Num(0);
                continue;
            }
            if !self.mon(target).is_active {
                damage[i] = FALSE;
                continue;
            }
            if amount != 0 {
                amount = amount.max(1);
            }
            if effect != Eff::StruggleRecoil {
                if let Eff::Weather(w) = effect {
                    if !self.run_status_immunity(target, Imm::Weather(w)) {
                        damage[i] = Res::Num(0);
                        continue;
                    }
                }
                let e = Event::new(Ev::Damage, Some(target), source, effect);
                let r = if self.event_mask & Ev::Damage.bit() != 0 || effect.is_move() {
                    self.run_event_ex(e, Res::Num(amount), true, false).0
                } else {
                    Res::Num(amount)
                };
                match r {
                    Res::Num(v) => amount = v,
                    other => {
                        damage[i] = other;
                        continue;
                    }
                }
            }
            if amount != 0 {
                amount = amount.max(1);
            }
            let dealt = self.damage_mon(target, amount, source, effect) as i32;
            damage[i] = Res::Num(dealt);
            // `-damage|target|hp|[from] ability: X|[of] source`.
            if matches!(effect, Eff::Ability(_) | Eff::Item(_)) {
                self.show_from(effect, target, source.filter(|&s| s != target || matches!(effect, Eff::Ability(_))));
            }
            if dealt != 0 {
                self.mon_mut(target).hurt_this_turn = self.mon(target).hp;
            }
            if dealt != 0 {
                if let Eff::Move(mi) = effect {
                    let drain = self.am[mi as usize].d().drain;
                    if drain.0 > 0 {
                        if let Some(s) = source {
                            let amount = round_div(dealt as u32, drain.0 as u32, drain.1 as u32);
                            self.heal(amount as i32, Some(s), Some(target), Eff::Drain);
                        }
                    }
                }
            }
        }
    }

    /// `Battle#directDamage`: no `Damage` event (Struggle recoil).
    pub(crate) fn direct_damage(&mut self, d: i32, target: MonRef, source: Option<MonRef>, effect: Eff) -> u32 {
        if self.mon(target).hp == 0 || d == 0 {
            return 0;
        }
        self.damage_mon(target, d.max(1), source, effect)
    }

    /// `Battle#heal`. Fractions must already be truncated, with anything in
    /// (0, 1] raised to 1 (see `div1`).
    pub(crate) fn heal(&mut self, amount: i32, target: Option<MonRef>, source: Option<MonRef>, effect: Eff) -> Res {
        let (target, source, effect) = self.event_defaults(target, source, effect);
        let r = self.run_event(Ev::TryHeal, target, source, effect, Res::Num(amount));
        if !r.truthy() {
            return r;
        }
        let amount = r.num();
        let Some(t) = target else {
            return FALSE;
        };
        let m = self.mon_mut(t);
        if m.hp == 0 || !m.is_active || m.hp >= m.max_hp() || amount <= 0 {
            return FALSE;
        }
        let new = (m.hp as i32 + amount).min(m.max_hp() as i32);
        let healed = new - m.hp as i32;
        m.hp = new as u16;
        // `-heal|target|hp|[from] item: X`.
        if matches!(effect, Eff::Ability(_) | Eff::Item(_)) {
            self.show_from(effect, t, source.filter(|&s| s != t));
        }
        self.run_event(Ev::Heal, target, source, effect, Res::Num(healed));
        Res::Num(healed)
    }

    /// `Pokemon#sethp`: set the HP outright (at least 1, at most the maximum).
    pub(crate) fn set_hp(&mut self, r: MonRef, hp: i32) {
        let m = self.mon_mut(r);
        if m.hp == 0 {
            return;
        }
        m.hp = hp.clamp(1, m.max_hp() as i32) as u16;
    }

    /// `Pokemon#heal` used directly (no events): returns the HP restored.
    pub(crate) fn heal_mon(&mut self, r: MonRef, d: i32) -> i32 {
        let m = self.mon_mut(r);
        if m.hp == 0 || d <= 0 || m.hp >= m.max_hp() {
            return 0;
        }
        let new = (m.hp as i32 + d).min(m.max_hp() as i32);
        let healed = new - m.hp as i32;
        m.hp = new as u16;
        healed
    }

    /// `Battle#faintMessages`.
    pub(crate) fn faint_messages(&mut self, last_first: bool, force_check: bool, mut check_win: bool) -> bool {
        if self.ended {
            return false;
        }
        let length = self.n_faint as usize;
        if length == 0 {
            return force_check && self.check_win(None);
        }
        if last_first {
            self.faint_queue[..length].rotate_right(1);
        }
        let mut last = None;
        // The queue can grow while it is being drained.
        while self.n_faint > 0 {
            let left = self.n_faint as usize;
            let fd = self.faint_queue[0];
            self.faint_queue.copy_within(1..left, 0);
            self.n_faint -= 1;
            last = Some(fd);
            let r = fd.target;
            if !self.mon(r).fainted
                && self.run_event(Ev::BeforeFaint, Some(r), fd.source, fd.effect, Res::Undef).truthy()
            {
                // The `faint` line.
                self.shown_leave(r, true);
                let side = &mut self.sides[r.side as usize];
                if side.pokemon_left > 0 {
                    side.pokemon_left -= 1;
                }
                if side.total_fainted < 100 {
                    side.total_fainted += 1;
                }
                self.run_event(Ev::Faint, Some(r), fd.source, fd.effect, Res::Undef);
                let (ability, item) = (self.mon(r).ability, self.mon(r).item);
                self.single_event(Ev::End, Eff::Ability(ability), Some(r), Some(r), None, Eff::None, Res::Undef);
                self.single_event(Ev::End, Eff::Item(item), Some(r), Some(r), None, Eff::None, Res::Undef);
                self.clear_volatile(r, false);
                let m = self.mon_mut(r);
                m.fainted = true;
                m.illusion = 0;
                m.is_active = false;
                self.sides[r.side as usize].fainted_this_turn = true;
                if self.n_faint as usize >= left {
                    check_win = true;
                }
            }
        }
        if check_win && self.check_win(last.map(|fd| fd.target)) {
            return true;
        }
        if let Some(fd) = last {
            self.run_event(Ev::AfterFaint, Some(fd.target), fd.source, fd.effect, Res::Num(length as i32));
        }
        false
    }

    /// `Battle#checkWin`. If both sides run out together, the side whose
    /// Pokémon fainted last wins.
    pub(crate) fn check_win(&mut self, last_fainted: Option<MonRef>) -> bool {
        if self.ended {
            return true;
        }
        let left = [self.sides[0].pokemon_left, self.sides[1].pokemon_left];
        if left[0] == 0 && left[1] == 0 {
            self.win(last_fainted.map(|r| r.side));
            return true;
        }
        for side in 0..2 {
            if left[1 - side] == 0 {
                self.win(Some(side as u8));
                return true;
            }
        }
        false
    }

    pub(crate) fn win(&mut self, side: Option<u8>) {
        if self.ended {
            return;
        }
        self.winner = side;
        self.ended = true;
        self.request = Request::None;
    }

    /// `Battle#checkFainted`.
    fn check_fainted(&mut self) {
        for side in 0..2 {
            // Nobody to send in: the fainted stay as they are.
            if !self.can_switch(side) {
                continue;
            }
            for pos in 0..ACTIVE {
                let r = self.active(side, pos);
                let m = self.mon_mut(r);
                if m.fainted {
                    // Showdown parks the status at 'fnt' here, which has no handlers.
                    m.status = Status::None;
                    m.switch_flag = true;
                    m.switch_move = NO_MOVE;
                }
            }
        }
    }

    // ------------------------------------------------- status and volatiles

    /// `Pokemon#setStatus` for a non-empty status.
    pub(crate) fn set_status(&mut self, r: MonRef, status: Status, source: Option<MonRef>, source_effect: Eff) -> Res {
        debug_assert!(status != Status::None);
        if self.mon(r).hp == 0 || !self.mon(r).is_active {
            return FALSE;
        }
        let source = source.or(self.event.source).or(Some(r));
        let source_effect = if source_effect == Eff::None { self.effect } else { source_effect };
        if self.mon(r).status == status {
            return FALSE;
        }
        let corrosive =
            source.is_some_and(|s| self.has_ability(s, ab::CORROSION)) && matches!(status, Status::Tox | Status::Psn);
        if !corrosive && !self.run_status_immunity(r, Imm::Status(status)) {
            return FALSE;
        }
        let prev = (self.mon(r).status, self.mon(r).status_time, self.mon(r).tox_stage, self.mon(r).status_st);
        let mut e = Event::new(Ev::SetStatus, Some(r), source, source_effect);
        e.status = status;
        let result = if self.event_mask & Ev::SetStatus.bit() != 0 {
            self.run_event_ex(e, Res::Undef, false, false).0
        } else {
            TRUE
        };
        if !result.truthy() {
            return result;
        }
        let st = self.new_state(true, r);
        {
            let m = self.mon_mut(r);
            m.status = status;
            m.status_time = 0;
            m.tox_stage = 0;
            m.status_st = st;
        }
        let data = &STATUS_CONDS[status as usize];
        self.listen(data.events, data.events_pre);
        if !self
            .single_event(Ev::Start, Eff::Status(status), Some(r), Some(r), source, source_effect, Res::Undef)
            .truthy()
        {
            let m = self.mon_mut(r);
            (m.status, m.status_time, m.tox_stage, m.status_st) = prev;
            return FALSE;
        }
        // `-status|target|brn|[from] ability: Flame Body|[of] source`.
        if let (Eff::Ability(a), Some(s)) = (source_effect, source) {
            self.show_ability(s, a);
        }
        let mut e = Event::new(Ev::AfterSetStatus, Some(r), source, source_effect);
        e.status = status;
        if self.event_mask & Ev::AfterSetStatus.bit() != 0 && !self.run_event_ex(e, Res::Undef, false, false).0.truthy()
        {
            return FALSE;
        }
        TRUE
    }

    /// `Pokemon#trySetStatus`: fails if there is already a status.
    pub(crate) fn try_set_status(
        &mut self,
        r: MonRef,
        status: Status,
        source: Option<MonRef>,
        source_effect: Eff,
    ) -> Res {
        // Showdown calls `setStatus(this.status || status)`, which fails on an existing status.
        if self.mon(r).status != Status::None {
            if self.mon(r).hp == 0 || !self.mon(r).is_active {
                return FALSE;
            }
            return FALSE;
        }
        self.set_status(r, status, source, source_effect)
    }

    /// `Pokemon#cureStatus` / `clearStatus`.
    pub(crate) fn cure_status(&mut self, r: MonRef) -> bool {
        let m = self.mon_mut(r);
        if m.hp == 0 || m.status == Status::None {
            return false;
        }
        m.status = Status::None;
        m.status_time = 0;
        m.tox_stage = 0;
        let st = self.new_state(false, r);
        self.mon_mut(r).status_st = st;
        // `-curestatus|p1: Name|status`: for a Pokémon on the bench (Heal Bell), the news is all there is.
        self.shown_bench_update(r);
        true
    }

    /// `Pokemon#addVolatile`.
    pub(crate) fn add_volatile(&mut self, r: MonRef, kind: VolKind, source: Option<MonRef>, source_effect: Eff) -> Res {
        if self.mon(r).hp == 0 && !VOL_CONDS[kind as usize].affects_fainted {
            return FALSE;
        }
        let source = source.or(self.event.source).or(Some(r));
        let source_effect = if source_effect == Eff::None { self.effect } else { source_effect };
        if self.vols(r).has(kind) {
            if !self.has_cb(Eff::Vol(kind), Ev::Restart) {
                return FALSE;
            }
            return self.single_event(Ev::Restart, Eff::Vol(kind), Some(r), Some(r), source, source_effect, Res::Undef);
        }
        if !self.run_status_immunity(r, Imm::Vol(kind)) {
            return FALSE;
        }
        if self.event_mask & Ev::TryAddVolatile.bit() != 0 {
            let mut e = Event::new(Ev::TryAddVolatile, Some(r), source, source_effect);
            e.vol = Some(kind);
            let result = self.run_event_ex(e, Res::Undef, false, false).0;
            if !result.truthy() {
                return result;
            }
        }
        if self.vols_mut(r).is_none_or(|l| l.is_full()) {
            // Not on the field, or holding an absurd number of volatiles already.
            return FALSE;
        }
        let data = kind.data();
        let mut v = Volatile::new(kind);
        v.st = self.new_state(true, r);
        v.duration = data.duration;
        v.source = source;
        v.source_slot = source.map_or(NO_SLOT, |s| self.field_slot(s));
        if data.duration_cb {
            v.duration = self.cond_duration(Eff::Vol(kind), Some(r), source, source_effect);
        }
        self.vols_mut(r).unwrap().push(v);
        self.listen(data.events, data.events_pre);
        let result = self.single_event(Ev::Start, Eff::Vol(kind), Some(r), Some(r), source, source_effect, Res::Undef);
        if !result.truthy() {
            self.drop_vol(r, kind);
            return result;
        }
        TRUE
    }

    /// `Pokemon#removeVolatile`.
    pub(crate) fn remove_volatile(&mut self, r: MonRef, kind: VolKind) -> bool {
        if self.mon(r).hp == 0 || !self.vols(r).has(kind) {
            return false;
        }
        self.single_event(Ev::End, Eff::Vol(kind), Some(r), Some(r), None, Eff::None, Res::Undef);
        let link = self.vols(r).get(kind).and_then(|v| v.source);
        self.drop_vol(r, kind);
        self.unlink_volatile(r, kind, link);
        true
    }

    /// `Pokemon#addVolatile(status, source, effect, linkedStatus)` for the one
    /// linked pair there is: `trapped` on the target and `trapper` on the
    /// Pokémon holding it there. Each ends when the last Pokémon it is linked
    /// to loses its half. The links are not stored: a `trapped` is linked to
    /// its source, a `trapper` to everyone whose `trapped` names it.
    pub(crate) fn add_trapped(&mut self, r: MonRef, source: MonRef, source_effect: Eff) -> Res {
        if self.mon(source).hp == 0 {
            return FALSE;
        }
        let result = self.add_volatile(r, VolKind::Trapped, Some(source), source_effect);
        if result != TRUE {
            return result;
        }
        if !self.vols(source).has(VolKind::Trapper) {
            self.add_volatile(source, VolKind::Trapper, Some(r), source_effect);
        }
        TRUE
    }

    /// `Pokemon#removeLinkedVolatiles`, after `r` lost `kind` (whose source was `link`).
    fn unlink_volatile(&mut self, r: MonRef, kind: VolKind, link: Option<MonRef>) {
        match kind {
            VolKind::Trapped => {
                // Its trapper is released once it holds nobody else.
                let Some(source) = link else {
                    return;
                };
                if self.vols(source).has(VolKind::Trapper) && !self.anyone_trapped_by(source) {
                    self.remove_volatile(source, VolKind::Trapper);
                }
            }
            VolKind::Trapper => {
                // Everyone it was holding goes free (not itself, after its No Retreat).
                let (actives, n) = self.all_active(true);
                for &other in &actives[..n] {
                    if other != r && self.vols(other).get(VolKind::Trapped).is_some_and(|v| v.source == Some(r)) {
                        self.remove_volatile(other, VolKind::Trapped);
                    }
                }
            }
            _ => {}
        }
    }

    fn anyone_trapped_by(&self, source: MonRef) -> bool {
        // No Retreat's own `trapped` (its source is itself) is linked to nobody.
        let (actives, n) = self.all_active(true);
        actives[..n]
            .iter()
            .any(|&o| o != source && self.vols(o).get(VolKind::Trapped).is_some_and(|v| v.source == Some(source)))
    }

    // ------------------------------------------------- field and side conditions

    /// `Field#suppressingWeather`: an active Pokémon has Cloud Nine or Air Lock.
    pub(crate) fn suppressing_weather(&self) -> bool {
        for side in 0..2 {
            for pos in 0..ACTIVE {
                let r = self.active(side, pos);
                let m = self.mon(r);
                if self.in_play(r)
                    && !m.fainted
                    && !self.ignoring_ability(r)
                    && matches!(m.ability, ab::CLOUDNINE | ab::AIRLOCK)
                    && m.ability_st.a == 0
                {
                    return true;
                }
            }
        }
        false
    }

    /// `Field#effectiveWeather`.
    pub(crate) fn field_weather(&self) -> Weather {
        if self.field.weather.kind == Weather::None || self.suppressing_weather() {
            Weather::None
        } else {
            self.field.weather.kind
        }
    }

    /// `Field#isWeather`.
    pub(crate) fn is_weather(&self, w: Weather) -> bool {
        self.field_weather() == w
    }

    /// `Pokemon#effectiveWeather`: the weather as one Pokémon experiences it.
    /// While a Pokémon with Mega Sol is using a move, moves and weather
    /// effects behave as if the sun were out. (Utility Umbrella is not in Champions.)
    pub(crate) fn effective_weather(&self, _r: MonRef) -> Weather {
        let weather = self.field_weather();
        if self.active_pokemon.is_some_and(|p| self.has_ability(p, ab::MEGASOL))
            && (self.effect == Eff::Ability(ab::MEGASOL) || matches!(self.effect, Eff::Move(_) | Eff::Weather(_)))
        {
            return Weather::Sunnyday;
        }
        weather
    }

    /// `effectiveWeather(undefined, true)`: the same, said aloud. Mega Sol
    /// making sun of other weather is announced (`-activate|pokemon|ability: Mega Sol`).
    pub(crate) fn effective_weather_aloud(&mut self, r: MonRef) -> Weather {
        let weather = self.effective_weather(r);
        if weather == Weather::Sunnyday && self.field_weather() != Weather::Sunnyday {
            self.show_ability(r, ab::MEGASOL);
        }
        weather
    }

    /// `Field#setWeather`. `Res::Null` is Showdown's "blocked, say nothing".
    pub(crate) fn set_weather(&mut self, w: Weather, source: Option<MonRef>, source_effect: Eff) -> Res {
        let source_effect = if source_effect == Eff::None { self.effect } else { source_effect };
        let source = source.or(self.event.target);
        if self.field.weather.kind == w {
            return FALSE;
        }
        // The SetWeather event only has listeners among the primal weathers.
        let prev = self.field.weather;
        let data = w.data();
        let mut c = Cond::new(w);
        // A field effect's state has no target, so it takes no place in the effect order.
        c.st = self.new_state_counted(false);
        c.source = source;
        c.source_slot = source.map_or(NO_SLOT, |s| self.field_slot(s));
        c.duration = data.duration;
        if data.duration_cb {
            c.duration = self.cond_duration(Eff::Weather(w), source, source, source_effect);
        }
        self.field.weather = c;
        self.listen(data.events, data.events_pre);
        if !self.single_event_at(Ev::FieldStart, Eff::Weather(w), Holder::Field, source, source_effect).truthy() {
            self.field.weather = prev;
            return FALSE;
        }
        // `-weather|RainDance|[from] ability: Drizzle|[of] source`
        if let (Eff::Ability(a), Some(s)) = (source_effect, source) {
            self.show_ability(s, a);
        }
        self.each_event_from(Ev::WeatherChange, source_effect);
        TRUE
    }

    /// `Field#clearWeather`.
    pub(crate) fn clear_weather(&mut self) -> bool {
        let prev = self.field.weather.kind;
        if prev == Weather::None {
            return false;
        }
        self.single_event_at(Ev::FieldEnd, Eff::Weather(prev), Holder::Field, None, Eff::None);
        self.field.weather = Cond::new(Weather::None);
        let effect = self.effect;
        self.each_event_from(Ev::WeatherChange, effect);
        true
    }

    /// `Field#setTerrain`.
    pub(crate) fn set_terrain(&mut self, t: Terrain, source: Option<MonRef>, source_effect: Eff) -> bool {
        let source_effect = if source_effect == Eff::None { self.effect } else { source_effect };
        let source = source.or(self.event.target);
        if self.field.terrain.kind == t {
            return false;
        }
        let prev = self.field.terrain;
        let data = t.data();
        let mut c = Cond::new(t);
        c.st = self.new_state_counted(false);
        c.source = source;
        c.source_slot = source.map_or(NO_SLOT, |s| self.field_slot(s));
        c.duration = data.duration;
        if data.duration_cb {
            c.duration = self.cond_duration(Eff::Terrain(t), source, source, source_effect);
        }
        self.field.terrain = c;
        self.listen(data.events, data.events_pre);
        if !self.single_event_at(Ev::FieldStart, Eff::Terrain(t), Holder::Field, source, source_effect).truthy() {
            self.field.terrain = prev;
            return false;
        }
        // `-fieldstart|move: Grassy Terrain|[from] ability: Grassy Surge|[of] source`
        if let (Eff::Ability(a), Some(s)) = (source_effect, source) {
            self.show_ability(s, a);
        }
        self.each_event_from(Ev::TerrainChange, source_effect);
        true
    }

    /// `Field#clearTerrain`.
    pub(crate) fn clear_terrain(&mut self) -> bool {
        let prev = self.field.terrain.kind;
        if prev == Terrain::None {
            return false;
        }
        self.single_event_at(Ev::FieldEnd, Eff::Terrain(prev), Holder::Field, None, Eff::None);
        self.field.terrain = Cond::new(Terrain::None);
        let effect = self.effect;
        self.each_event_from(Ev::TerrainChange, effect);
        true
    }

    /// `Field#isTerrain`. (The TryTerrain event has no listeners.)
    pub(crate) fn is_terrain(&self, t: Terrain) -> bool {
        self.field.terrain.kind == t
    }

    /// `Field#addPseudoWeather`.
    pub(crate) fn add_pseudo_weather(&mut self, kind: Pseudo, source: Option<MonRef>, source_effect: Eff) -> bool {
        let source = source.or(self.event.target);
        if self.field.pseudo.has(kind) {
            if !self.has_cb(Eff::Pseudo(kind), Ev::FieldRestart) {
                return false;
            }
            return self
                .single_event_at(Ev::FieldRestart, Eff::Pseudo(kind), Holder::Field, source, source_effect)
                .truthy();
        }
        let data = kind.data();
        let mut c = Cond::new(kind);
        c.st = self.new_state_counted(false);
        c.source = source;
        c.source_slot = source.map_or(NO_SLOT, |s| self.field_slot(s));
        c.duration = data.duration;
        if data.duration_cb {
            c.duration = self.cond_duration(Eff::Pseudo(kind), source, source, source_effect);
        }
        self.field.pseudo.push(c);
        self.listen(data.events, data.events_pre);
        if !self.single_event_at(Ev::FieldStart, Eff::Pseudo(kind), Holder::Field, source, source_effect).truthy() {
            self.field.pseudo.remove(kind);
            return false;
        }
        // The PseudoWeatherChange event has no listeners.
        true
    }

    /// `Field#removePseudoWeather`.
    pub(crate) fn remove_pseudo_weather(&mut self, kind: Pseudo) -> bool {
        if !self.field.pseudo.has(kind) {
            return false;
        }
        self.single_event_at(Ev::FieldEnd, Eff::Pseudo(kind), Holder::Field, None, Eff::None);
        self.field.pseudo.remove(kind);
        true
    }

    /// `Side#addSideCondition`.
    pub(crate) fn add_side_condition(
        &mut self,
        side: usize,
        kind: SideCond,
        source: Option<MonRef>,
        source_effect: Eff,
    ) -> Res {
        let source = source.or(self.event.target);
        let holder = Holder::Side(side as u8);
        if self.sides[side].conds.has(kind) {
            if !self.has_cb(Eff::SideCond(kind), Ev::SideRestart) {
                return FALSE;
            }
            return self.single_event_at(Ev::SideRestart, Eff::SideCond(kind), holder, source, source_effect);
        }
        let data = kind.data();
        let mut c = Cond::new(kind);
        // A side condition's state has the side as its target, so it is counted.
        c.st = self.new_state_counted(true);
        c.source = source;
        c.source_slot = source.map_or(NO_SLOT, |s| self.field_slot(s));
        c.duration = data.duration;
        if data.duration_cb {
            let first = self.active(side, 0);
            c.duration = self.cond_duration(Eff::SideCond(kind), Some(first), source, source_effect);
        }
        self.sides[side].conds.push(c);
        self.listen(data.events, data.events_pre);
        if !self.single_event_at(Ev::SideStart, Eff::SideCond(kind), holder, source, source_effect).truthy() {
            self.sides[side].conds.remove(kind);
            return FALSE;
        }
        // The SideConditionStart event has no listeners.
        TRUE
    }

    /// `Side#removeSideCondition`.
    pub(crate) fn remove_side_condition(&mut self, side: usize, kind: SideCond) -> bool {
        if !self.sides[side].conds.has(kind) {
            return false;
        }
        self.single_event_at(Ev::SideEnd, Eff::SideCond(kind), Holder::Side(side as u8), None, Eff::None);
        self.sides[side].conds.remove(kind);
        true
    }

    /// `Side#addSlotCondition`.
    pub(crate) fn add_slot_condition(
        &mut self,
        side: usize,
        pos: usize,
        kind: SlotCond,
        source: Option<MonRef>,
        source_effect: Eff,
    ) -> Res {
        let source = source.or(self.event.target);
        let occupant = self.active(side, pos);
        if self.sides[side].slot_conds[pos].has(kind) {
            if !self.has_cb(Eff::SlotCond(kind), Ev::Restart) {
                return FALSE;
            }
            return self.single_event(
                Ev::Restart,
                Eff::SlotCond(kind),
                Some(occupant),
                None,
                source,
                source_effect,
                Res::Undef,
            );
        }
        let data = kind.data();
        let mut c = Cond::new(kind);
        c.st = self.new_state_counted(true);
        c.source = source;
        c.source_slot = source.map_or(NO_SLOT, |s| self.field_slot(s));
        c.duration = data.duration;
        if data.duration_cb {
            let first = self.active(side, 0);
            c.duration = self.cond_duration(Eff::SlotCond(kind), Some(first), source, source_effect);
        }
        self.sides[side].slot_conds[pos].push(c);
        self.listen(data.events, data.events_pre);
        let started = self.single_event(
            Ev::Start,
            Eff::SlotCond(kind),
            Some(occupant),
            Some(occupant),
            source,
            source_effect,
            Res::Undef,
        );
        if !started.truthy() {
            self.sides[side].slot_conds[pos].remove(kind);
            return FALSE;
        }
        TRUE
    }

    /// `Side#removeSlotCondition`.
    pub(crate) fn remove_slot_condition(&mut self, side: usize, pos: usize, kind: SlotCond) -> bool {
        if !self.sides[side].slot_conds[pos].has(kind) {
            return false;
        }
        let occupant = self.active(side, pos);
        self.single_event(Ev::End, Eff::SlotCond(kind), Some(occupant), Some(occupant), None, Eff::None, Res::Undef);
        self.sides[side].slot_conds[pos].remove(kind);
        true
    }

    // ------------------------------------------------------------ stat stages

    /// `Battle#boost` for a table whose keys are in the usual atk..evasion order.
    pub(crate) fn boost(&mut self, b: Boosts, target: Option<MonRef>, source: Option<MonRef>, effect: Eff) -> Res {
        self.boost_ordered(b, &BOOST_ORDERS[0], target, source, effect)
    }

    /// `Battle#boost` for a single stat.
    pub(crate) fn boost1(
        &mut self,
        stat: usize,
        by: i8,
        target: Option<MonRef>,
        source: Option<MonRef>,
        effect: Eff,
    ) -> Res {
        let mut b = [0i8; 7];
        b[stat] = by;
        self.boost(b, target, source, effect)
    }

    /// `Battle#boost`. `order` is the order the table's keys are applied in.
    pub(crate) fn boost_ordered(
        &mut self,
        b: Boosts,
        order: &[u8; 7],
        target: Option<MonRef>,
        source: Option<MonRef>,
        effect: Eff,
    ) -> Res {
        let (target, source, effect) = self.event_defaults(target, source, effect);
        let Some(r) = target else {
            return Res::Num(0);
        };
        if self.mon(r).hp == 0 {
            return Res::Num(0);
        }
        if !self.mon(r).is_active {
            return FALSE;
        }
        if self.sides[1 - r.side as usize].pokemon_left == 0 {
            return FALSE;
        }
        let mut b = b;
        if self.event_mask & Ev::ChangeBoost.bit() != 0 {
            let mut e = Event::new(Ev::ChangeBoost, Some(r), source, effect);
            e.boosts = b;
            b = self.run_event_ex(e, Res::Undef, false, false).1.boosts;
        }
        // getCappedBoost
        for k in 0..7 {
            let cur = self.mon(r).boosts[k];
            b[k] = (cur + b[k]).clamp(-6, 6) - cur;
        }
        if self.event_mask & Ev::TryBoost.bit() != 0 {
            let mut e = Event::new(Ev::TryBoost, Some(r), source, effect);
            e.boosts = b;
            b = self.run_event_ex(e, Res::Undef, false, false).1.boosts;
        }
        let mut success = false;
        for &k in order {
            let k = k as usize;
            if b[k] == 0 {
                continue;
            }
            let cur = self.mon(r).boosts[k];
            let new = (cur + b[k]).clamp(-6, 6);
            if new != cur {
                self.mon_mut(r).boosts[k] = new;
                if !success {
                    // `-ability|target|X|boost` for an ability raising its own holder's stats (one acting on
                    // someone else announces itself); `-boost|...|[from] item: X` for an item.
                    match effect {
                        Eff::Ability(a) if self.effect_holder == Some(Holder::Mon(r)) || self.effect != effect => {
                            self.show_ability(r, a)
                        }
                        Eff::Item(i) => self.show_item(r, i),
                        _ => {}
                    }
                }
                success = true;
                if self.event_mask & Ev::AfterEachBoost.bit() != 0 {
                    let mut e = Event::new(Ev::AfterEachBoost, Some(r), source, effect);
                    e.boosts[k] = b[k];
                    self.run_event_ex(e, Res::Undef, false, false);
                }
            }
        }
        if self.event_mask & Ev::AfterBoost.bit() != 0 {
            let mut e = Event::new(Ev::AfterBoost, Some(r), source, effect);
            e.boosts = b;
            self.run_event_ex(e, Res::Undef, false, false);
        }
        if success {
            let m = self.mon_mut(r);
            m.stats_raised_this_turn |= b.iter().any(|&x| x > 0);
            m.stats_lowered_this_turn |= b.iter().any(|&x| x < 0);
        }
        if success { TRUE } else { Res::Null }
    }

    // --------------------------------------------------- abilities and items

    /// `Pokemon#ignoringAbility`.
    pub(crate) fn ignoring_ability(&self, r: MonRef) -> bool {
        let m = self.mon(r);
        if !m.is_active {
            return true;
        }
        // An ability tied to its owner's own forme does nothing for a Pokémon transformed into it.
        if ABILITIES[m.ability as usize].flags & AF_NOTRANSFORM != 0 && m.transformed {
            return true;
        }
        if ABILITIES[m.ability as usize].flags & AF_CANTSUPPRESS != 0 {
            return false;
        }
        // (Neutralizing Gas, the other way to lose an ability, is not in Champions.)
        self.vols(r).has(VolKind::Gastroacid)
    }

    /// `Pokemon#hasAbility`.
    pub(crate) fn has_ability(&self, r: MonRef, ability: u16) -> bool {
        self.mon(r).ability == ability && !self.ignoring_ability(r)
    }

    /// `Pokemon#ignoringItem`.
    pub(crate) fn ignoring_item(&self, r: MonRef) -> bool {
        let m = self.mon(r);
        if !m.is_active {
            return true;
        }
        if self.field.pseudo.has(Pseudo::Magicroom) || self.has_vol_named(r, "embargo") {
            return true;
        }
        ITEMS[m.item as usize].flags & IF_IGNORE_KLUTZ == 0 && self.has_ability(r, ab::KLUTZ)
    }

    /// Whether a Pokémon has the volatile with this id. For the places where
    /// Showdown looks up a volatile Champions may not have: the check starts
    /// working the moment the volatile appears in the generated tables.
    pub(crate) fn has_vol_named(&self, r: MonRef, id: &str) -> bool {
        VolKind::named(id).is_some_and(|k| self.vols(r).has(k))
    }

    /// `Pokemon#hasItem`.
    pub(crate) fn has_item(&self, r: MonRef, item: u16) -> bool {
        self.mon(r).item == item && !self.ignoring_item(r)
    }

    /// `Pokemon#eatItem`.
    pub(crate) fn eat_item(&mut self, r: MonRef, force: bool, source: Option<MonRef>, source_effect: Eff) -> bool {
        let item = self.mon(r).item;
        if item == it::NONE {
            return false;
        }
        if self.mon(r).hp == 0 || !self.mon(r).is_active {
            return false;
        }
        let source_effect = if source_effect == Eff::None { self.effect } else { source_effect };
        let source = source.or(self.event.target);
        if let Eff::Item(i) = source_effect {
            if i != item && source == Some(r) {
                // An item is telling us to eat it but we no longer hold it.
                return false;
            }
        }
        let mut e = Event::new(Ev::UseItem, Some(r), None, Eff::None);
        e.item = item;
        if !self.run_event_ex(e, Res::Undef, false, false).0.truthy() {
            return false;
        }
        if !force {
            let mut e = Event::new(Ev::TryEatItem, Some(r), None, Eff::None);
            e.item = item;
            if !self.run_event_ex(e, Res::Undef, false, false).0.truthy() {
                return false;
            }
        }
        // `-enditem|pokemon|Item|[eat]`.
        self.show_item_lost(r, item);
        self.single_event(Ev::Eat, Eff::Item(item), Some(r), Some(r), source, source_effect, Res::Undef);
        let mut e = Event::new(Ev::EatItem, Some(r), source, source_effect);
        e.item = item;
        self.run_event_ex(e, Res::Undef, false, false);
        {
            let m = self.mon_mut(r);
            m.last_item = item;
            m.item = it::NONE;
            // clearEffectState keeps the state object but blanks it.
            m.item_st.order = 0;
            m.item_st.a = 0;
            m.item_st.b = 0;
            m.used_item_this_turn = true;
            m.ate_berry = true;
        }
        let mut e = Event::new(Ev::AfterUseItem, Some(r), None, Eff::None);
        e.item = item;
        self.run_event_ex(e, Res::Undef, false, false);
        true
    }

    /// `Pokemon#useItem`.
    pub(crate) fn use_item(&mut self, r: MonRef, source: Option<MonRef>, source_effect: Eff) -> bool {
        let item = self.mon(r).item;
        let is_gem = ITEMS[item as usize].flags & IF_GEM != 0;
        if (self.mon(r).hp == 0 && !is_gem) || !self.mon(r).is_active {
            return false;
        }
        if item == it::NONE {
            return false;
        }
        let source_effect = if source_effect == Eff::None { self.effect } else { source_effect };
        let source = source.or(self.event.target);
        if let Eff::Item(i) = source_effect {
            if i != item && source == Some(r) {
                return false;
            }
        }
        let mut e = Event::new(Ev::UseItem, Some(r), None, Eff::None);
        e.item = item;
        if !self.run_event_ex(e, Res::Undef, false, false).0.truthy() {
            return false;
        }
        // `-enditem|pokemon|Item`.
        self.show_item_lost(r, item);
        if let Some(b) = ITEMS[item as usize].boosts {
            self.boost(b, Some(r), source, Eff::Item(item));
        }
        self.single_event(Ev::Use, Eff::Item(item), Some(r), Some(r), source, source_effect, Res::Undef);
        {
            let m = self.mon_mut(r);
            m.last_item = item;
            m.item = it::NONE;
            m.item_st.order = 0;
            m.item_st.a = 0;
            m.item_st.b = 0;
            m.used_item_this_turn = true;
        }
        let mut e = Event::new(Ev::AfterUseItem, Some(r), None, Eff::None);
        e.item = item;
        self.run_event_ex(e, Res::Undef, false, false);
        true
    }

    /// `Pokemon#takeItem`: returns the item taken (0 if none or blocked).
    pub(crate) fn take_item(&mut self, r: MonRef, source: Option<MonRef>) -> u16 {
        match self.try_take_item(r, source) {
            Taken::Item(item) => item,
            _ => it::NONE,
        }
    }

    /// `Pokemon#takeItem`, telling "had no item" (`undefined`) from "would not let go" (`false`).
    pub(crate) fn try_take_item(&mut self, r: MonRef, source: Option<MonRef>) -> Taken {
        match self.take_item_inner(r, source) {
            it::NONE if self.mon(r).item == it::NONE => Taken::NoItem,
            it::NONE => Taken::Blocked,
            item => Taken::Item(item),
        }
    }

    /// The item's own say in being taken: `singleEvent('TakeItem', item, ...)`,
    /// which only Mega Stones answer. `keeper` is the Pokémon Showdown passes
    /// as the event's target.
    pub(crate) fn item_lets_go(&mut self, item: u16, keeper: MonRef, source: MonRef, mi: u8) -> bool {
        if item == it::NONE || !self.has_cb(Eff::Item(item), Ev::TakeItem) {
            return true;
        }
        let mut e = Event::new(Ev::TakeItem, Some(keeper), Some(source), Eff::Move(mi));
        e.item = item;
        let holder = Some(Holder::Mon(keeper));
        self.single_event_ex(Ev::TakeItem, Ev::TakeItem, Pre::On, Eff::Item(item), holder, e, Res::Undef, false)
            .truthy()
    }

    fn take_item_inner(&mut self, r: MonRef, source: Option<MonRef>) -> u16 {
        let source = source.or(Some(r));
        let item = self.mon(r).item;
        if item == it::NONE {
            return it::NONE;
        }
        let mut e = Event::new(Ev::TakeItem, Some(r), source, Eff::None);
        e.item = item;
        if !self.run_event_ex(e, Res::Undef, false, false).0.truthy() {
            return it::NONE;
        }
        {
            let m = self.mon_mut(r);
            m.item = it::NONE;
            m.item_st.order = 0;
            m.item_st.a = 0;
            m.item_st.b = 0;
        }
        self.single_event(Ev::End, Eff::Item(item), Some(r), Some(r), None, Eff::None, Res::Undef);
        item
    }

    /// `Pokemon#setItem`.
    pub(crate) fn set_item(&mut self, r: MonRef, item: u16, source: Option<MonRef>, effect: Eff) -> bool {
        if self.mon(r).hp == 0 || !self.mon(r).is_active {
            return false;
        }
        let old = self.mon(r).item;
        self.listen(ITEMS[item as usize].events, ITEMS[item as usize].events_pre);
        let st = self.new_state(item != it::NONE, r);
        {
            let m = self.mon_mut(r);
            m.item = item;
            m.item_st = st;
        }
        if old != it::NONE {
            self.single_event(Ev::End, Eff::Item(old), Some(r), Some(r), None, Eff::None, Res::Undef);
        }
        if item != it::NONE {
            self.single_event(Ev::Start, Eff::Item(item), Some(r), Some(r), source, effect, Res::Undef);
        }
        true
    }

    /// A fresh `abilityState` for the Pokémon's current ability.
    pub(crate) fn new_ability_state(&mut self, r: MonRef) {
        let st = self.new_state(true, r);
        let m = self.mon_mut(r);
        m.ability_st = st;
        m.ability_boosts = [0; 7];
    }

    /// `Pokemon#setAbility`: returns whether the ability was changed.
    pub(crate) fn set_ability(&mut self, r: MonRef, ability: u16, source: Option<MonRef>, source_effect: Eff) -> bool {
        self.set_ability_ex(r, ability, source, source_effect, false)
    }

    /// `Pokemon#setAbility` with its `isFromFormeChange` flag: a forme change
    /// replaces even abilities that cannot otherwise be replaced, and asks nobody.
    pub(crate) fn set_ability_ex(
        &mut self,
        r: MonRef,
        ability: u16,
        source: Option<MonRef>,
        source_effect: Eff,
        from_forme_change: bool,
    ) -> bool {
        if self.mon(r).hp == 0 {
            return false;
        }
        let source_effect = if source_effect == Eff::None { self.effect } else { source_effect };
        let old = self.mon(r).ability;
        if !from_forme_change {
            if (ABILITIES[ability as usize].flags | ABILITIES[old as usize].flags) & AF_CANTSUPPRESS != 0 {
                return false;
            }
            if !self.run_event(Ev::SetAbility, Some(r), source, source_effect, Res::Undef).truthy() {
                return false;
            }
        }
        self.single_event(Ev::End, Eff::Ability(old), Some(r), Some(r), source, Eff::None, Res::Undef);
        self.mon_mut(r).ability = ability;
        self.listen(ABILITIES[ability as usize].events, ABILITIES[ability as usize].events_pre);
        self.new_ability_state(r);
        if source_effect != Eff::None && !from_forme_change {
            // `-ability|pokemon|New|Old|[from] effect|[of] source`: what it had, what it has now,
            // and (when the new one was copied from someone) that the source has it too. Mummy says
            // the same in its own words: `-activate|source|ability: Mummy|pokemon|[ability] Old`.
            self.show_ability(r, old);
            self.show_ability_change(r, ability);
            if let Some(s) = source.filter(|&s| self.shown_live(s)) {
                self.show_ability(s, ability);
            }
        }
        self.single_event(Ev::Start, Eff::Ability(ability), Some(r), Some(r), source, Eff::None, Res::Undef);
        true
    }

    /// `setAbility` as a move's onHit reports it: nothing to add if the ability
    /// changed, `false` if it could not be.
    pub(crate) fn set_ability_result(&mut self, r: MonRef, ability: u16, source: Option<MonRef>) -> Res {
        if self.set_ability_ex(r, ability, source, Eff::None, false) { Res::Undef } else { FALSE }
    }

    /// `Battle#skillSwap`. Skill Swap, or (`wandering`) the Wandering Spirit of `target`, which
    /// `source` has just touched.
    pub(crate) fn skill_swap(&mut self, source: MonRef, target: MonRef, wandering: bool) -> bool {
        if self.mon(source).fainted || self.mon(target).fainted {
            return false;
        }
        let (sa, ta) = (self.mon(source).ability, self.mon(target).ability);
        if (ABILITIES[sa as usize].flags | ABILITIES[ta as usize].flags) & AF_FAILSKILLSWAP != 0 {
            return false;
        }
        // The SetAbility event has no listeners among modelled effects.
        // `-activate|source|Skill Swap|theirs|mine|[of] target`; between allies the abilities are not named.
        if self.is_ally(source, target) {
            let (mine, theirs) = (self.shown_mut(source).ability, self.shown_mut(target).ability);
            if wandering {
                // No move called Skill Swap was used, so this was a Wandering Spirit, and
                // the one that was hit had it. What it got in return is not said.
                self.show_ability(target, ab::WANDERINGSPIRIT);
                self.show_ability_change(source, ab::WANDERINGSPIRIT);
            } else {
                self.show_ability_change(source, theirs);
            }
            self.show_ability_change(target, mine);
        } else {
            self.show_ability(source, sa);
            self.show_ability(target, ta);
            self.show_ability_change(source, ta);
            self.show_ability_change(target, sa);
        }
        self.single_event(Ev::End, Eff::Ability(sa), Some(source), Some(source), None, Eff::None, Res::Undef);
        self.single_event(Ev::End, Eff::Ability(ta), Some(target), Some(target), None, Eff::None, Res::Undef);
        self.mon_mut(source).ability = ta;
        self.mon_mut(target).ability = sa;
        self.new_ability_state(source);
        self.new_ability_state(target);
        self.single_event(Ev::Start, Eff::Ability(sa), Some(target), Some(target), None, Eff::None, Res::Undef);
        self.single_event(Ev::Start, Eff::Ability(ta), Some(source), Some(source), None, Eff::None, Res::Undef);
        true
    }

    /// `Pokemon#tryTrap`.
    pub(crate) fn try_trap(&mut self, r: MonRef, hidden: bool) -> bool {
        if !self.run_status_immunity(r, Imm::Trapped) {
            return false;
        }
        if self.mon(r).trapped != Trapped::No && hidden {
            return true;
        }
        self.mon_mut(r).trapped = if hidden { Trapped::Hidden } else { Trapped::Yes };
        true
    }

    /// `Pokemon#isSemiInvulnerable`: out of reach in the middle of Fly, Dig and the like.
    pub(crate) fn is_semi_invulnerable(&self, r: MonRef) -> bool {
        let v = self.vols(r);
        v.has(VolKind::Fly)
            || v.has(VolKind::Bounce)
            || v.has(VolKind::Dive)
            || v.has(VolKind::Dig)
            || v.has(VolKind::Phantomforce)
            || self.has_vol_named(r, "shadowforce")
    }

    /// `Pokemon#getWeight`, in hectograms.
    pub(crate) fn get_weight(&mut self, r: MonRef) -> u32 {
        let base = SPECIES[self.mon(r).species as usize].weight_hg as i32;
        let w = self.run_event(Ev::ModifyWeight, Some(r), None, Eff::None, Res::Num(base)).num();
        w.max(1) as u32
    }

    /// `Pokemon#positiveBoosts`: the sum of its raised stat stages.
    pub(crate) fn positive_boosts(&self, r: MonRef) -> i32 {
        self.mon(r).boosts.iter().filter(|&&b| b > 0).map(|&b| b as i32).sum()
    }

    /// `Pokemon#getLockedMove`: the move the Pokémon has no choice but to use
    /// (the `LockMove` event), if any.
    pub(crate) fn get_locked_move(&mut self, r: MonRef) -> Option<u16> {
        if !self.listens(Ev::LockMove) {
            return None;
        }
        match self.priority_event(Ev::LockMove, Some(r), None, Eff::None, Res::Undef) {
            Res::Num(n) if n > 0 => Some((n - 1) as u16),
            _ => None,
        }
    }

    /// Where a locked move is aimed (`Side#chooseMove`): where the charging
    /// move's own volatile says, or else wherever the last move went.
    pub(crate) fn locked_move_loc(&self, r: MonRef, move_id: u16) -> i8 {
        let own = VolKind::named(MOVES[move_id as usize].id).and_then(|k| self.vols(r).get(k)).map(|v| v.st.a as i8);
        match own {
            Some(loc) if loc != 0 => loc,
            _ => self.mon(r).last_move_loc,
        }
    }

    /// `Pokemon#isAdjacent` in doubles: two different Pokémon, neither fainted.
    pub(crate) fn is_adjacent(&self, a: MonRef, b: MonRef) -> bool {
        !self.mon(a).fainted && !self.mon(b).fainted && a != b
    }

    // -------------------------------------------------------------- the turn

    /// `Battle#runAction`.
    fn run_action(&mut self, a: Action) {
        // What Emergency Exit will be asked about afterwards: the HP the
        // action's Pokémon had going in, and for the end-of-turn step, everyone's.
        let original_hp = a.mon.map_or(0, |r| self.mon(r).hp);
        let mut residual = [(MonRef { side: 0, idx: 0 }, 0u16); 4];
        let mut n_residual = 0;
        match a.kind {
            ActKind::Team => return,
            ActKind::Start => {
                for side in 0..2 {
                    for pos in 0..ACTIVE {
                        let r = self.active(side, pos);
                        self.switch_in(r, pos, true, SelfSwitch::No, false);
                    }
                }
                self.started = true;
            }
            ActKind::Move => {
                let r = a.mon.unwrap();
                if !self.mon(r).is_active || self.mon(r).fainted {
                    return;
                }
                trace::note(|| format!("move {} by p{}:{}", MOVES[a.move_id as usize].id, r.side + 1, r.idx));
                self.note_move_starts(&a);
                self.run_move(&a);
                self.note_move_ends();
            }
            ActKind::Switch | ActKind::InstaSwitch => {
                let out = a.mon.unwrap();
                let pos = self.mon(out).position as usize;
                let via = match a.source {
                    ActSource::SelfSwitch(id) => MOVES[id as usize].self_switch,
                    _ => SelfSwitch::No,
                };
                self.switch_in(a.switch_to.unwrap(), pos, false, via, false);
            }
            ActKind::Revival => {
                let (r, t) = (a.mon.unwrap(), a.switch_to.unwrap());
                let side = r.side as usize;
                self.sides[side].pokemon_left += 1;
                if (self.mon(t).position as usize) < ACTIVE {
                    // Still in its active slot: it comes back in where it lies, once the
                    // queue gets to it (Showdown adds this at the very end of the queue).
                    let back = self.resolve_switch(ActKind::InstaSwitch, t, t);
                    self.queue.push(back);
                }
                let m = self.mon_mut(t);
                m.fainted = false;
                m.faint_queued = false;
                m.status = Status::None;
                m.hp = (m.max_hp() / 2).max(1);
                // `-heal|p1: Name|hp|[from] move: Revival Blessing`
                self.shown_bench_update(t);
                let pos = self.mon(r).position as usize;
                self.remove_slot_condition(side, pos, SlotCond::Revivalblessing);
            }
            ActKind::MegaEvo => self.run_mega_evo(a.mon.unwrap()),
            ActKind::BeforeTurnMove => {
                let r = a.mon.unwrap();
                if !self.mon(r).is_active || self.mon(r).fainted {
                    return;
                }
                let Some(target) = self.get_target(r, MOVES[a.move_id as usize].target, a.target_loc, None) else {
                    return;
                };
                self.before_turn_callback(a.move_id, r, target);
            }
            ActKind::PriorityCharge => {
                let r = a.mon.unwrap();
                if !self.mon(r).is_active || self.mon(r).fainted {
                    return;
                }
                self.priority_charge_callback(a.move_id, r);
            }
            ActKind::RunSwitch => self.run_switch(a.mon.unwrap()),
            ActKind::BeforeTurn => self.each_event(Ev::BeforeTurn),
            ActKind::Residual => {
                self.clear_active_move(true);
                self.update_speed();
                let (actives, k) = self.all_active(false);
                for &r in &actives[..k] {
                    residual[n_residual] = (r, self.mon(r).hp);
                    n_residual += 1;
                }
                self.field_event(Ev::Residual, None);
            }
        }

        // Phazing (Roar and the like): whoever was marked is dragged out now.
        for side in 0..2 {
            for pos in 0..ACTIVE {
                let r = self.active(side, pos);
                if self.mon(r).force_switch_flag {
                    if self.mon(r).hp > 0 {
                        self.drag_in(side, pos);
                    }
                    self.mon_mut(r).force_switch_flag = false;
                }
            }
        }

        self.clear_active_move(false);
        self.faint_messages(false, true, true);
        if self.ended {
            return;
        }

        let next = self.queue.peek().map(|x| x.kind);
        if next.is_none() {
            self.check_fainted();
        }
        if a.kind != ActKind::Start && next != Some(ActKind::InstaSwitch) {
            self.each_event(Ev::Update);
            for &(r, hp) in &residual[..n_residual] {
                self.run_event(Ev::EmergencyExit, Some(r), None, Eff::None, Res::Num(hp as i32));
            }
        }
        if a.kind == ActKind::RunSwitch {
            // (Entry hazards may have brought it to half.)
            self.run_event(Ev::EmergencyExit, a.mon, None, Eff::None, Res::Num(original_hp as i32));
        }

        let mut any = false;
        for side in 0..2 {
            let wants = |b: &Battle| (0..ACTIVE).any(|pos| b.mon(b.active(side, pos)).switch_flag);
            let mut switching = wants(self);
            if switching && !self.can_switch(side) {
                // No one to switch to. Revival Blessing's "switch" is the exception: it asks all the same.
                let mut reviving = false;
                for pos in 0..ACTIVE {
                    if self.sides[side].slot_conds[pos].has(SlotCond::Revivalblessing) {
                        reviving = true;
                        continue;
                    }
                    let r = self.active(side, pos);
                    self.mon_mut(r).switch_flag = false;
                    self.mon_mut(r).switch_move = NO_MOVE;
                }
                switching = reviving;
            } else if switching {
                // Anyone leaving of its own accord gets its BeforeSwitchOut event now, once.
                for pos in 0..ACTIVE {
                    let r = self.active(side, pos);
                    let m = self.mon(r);
                    if m.hp > 0 && m.switch_flag && m.switch_move != mv::REVIVALBLESSING {
                        self.run_pursuit_activation(r);
                        if self.ended {
                            return;
                        }
                        if self.mon(r).fainted {
                            switching = wants(self);
                        }
                    }
                }
            }
            any |= switching;
        }
        if any {
            self.request = Request::Switch;
            return;
        }

        if self.queue.peek().is_some_and(|x| x.kind == ActKind::Move) {
            // Speed is dynamic since Gen 8: recompute and re-sort what is left.
            self.update_speed();
            let mut q = self.queue;
            for act in q.as_mut_slice() {
                if act.mon.is_some() {
                    self.resolve_speed(act);
                }
            }
            self.queue = q;
            self.sort_queue();
        }
    }

    /// `Battle#turnLoop`.
    pub(crate) fn turn_loop(&mut self) {
        self.request = Request::None;
        if !self.mid_turn {
            self.insert_choice(Battle::blank_action(ActKind::BeforeTurn, 4));
            self.queue.push(Battle::blank_action(ActKind::Residual, 300));
            self.mid_turn = true;
        }
        while let Some(a) = self.queue.shift() {
            // Moves of earlier actions are finished; their slots can be reused.
            self.am_len = 0;
            self.run_action(a);
            if self.request != Request::None || self.ended {
                return;
            }
        }
        self.end_turn();
        self.mid_turn = false;
        self.queue.clear();
    }

    /// `Battle#endTurn`.
    fn end_turn(&mut self) {
        self.turn += 1;
        for side in 0..2 {
            for pos in 0..ACTIVE {
                let r = self.active(side, pos);
                if !self.in_play(r) {
                    continue;
                }
                {
                    let turn = self.turn;
                    let m = self.mon_mut(r);
                    m.newly_switched = false;
                    m.move_last_turn = m.move_this_turn;
                    m.move_this_turn = Res::Undef;
                    if turn != 1 {
                        m.used_item_this_turn = false;
                        m.stats_raised_this_turn = false;
                        m.stats_lowered_this_turn = false;
                        m.hurt_this_turn = 0;
                    }
                    m.hit_by_this_turn = 0;
                    for k in 0..m.n_moves as usize {
                        m.moves[k].disabled = false;
                        m.moves[k].hidden = false;
                    }
                }
                // `attackedBy`: nothing in it counts as "this turn" any more, and
                // attacks by Pokémon that have left the field are forgotten.
                let mut kept = 0;
                for k in 0..self.mon(r).n_damaged_by as usize {
                    let mut a = self.mon(r).damaged_by[k];
                    if self.mon(MonRef { side: 1 - r.side, idx: a.idx }).is_active {
                        a.this_turn = false;
                        self.mon_mut(r).damaged_by[kept] = a;
                        kept += 1;
                    }
                }
                self.mon_mut(r).n_damaged_by = kept as u8;
                self.end_turn_types(r);
                self.request_prep(r);
                if self.mon(r).fainted {
                    continue;
                }
                self.mon_mut(r).active_turns += 1;
            }
            self.sides[side].fainted_last_turn = self.sides[side].fainted_this_turn;
            self.sides[side].fainted_this_turn = false;
        }
        if self.turn > 1000 {
            // Showdown's hard turn limit: the battle is a tie.
            self.win(None);
            return;
        }
        self.types_settled();
        self.request_locks();
        self.request = Request::Move;
        self.note_sealed();
    }

    /// The part of `Battle#endTurn` that keeps everyone's types on show (since
    /// generation 7 "the real type of every Pokemon is visible to all players"):
    /// a Pokémon whose types, or those of the Pokémon it passes for, are not
    /// what the log last gave them has them said.
    fn end_turn_types(&mut self, r: MonRef) {
        let m = self.mon(r);
        if !m.is_active || m.fainted {
            return;
        }
        // (`if (pokemon.getLastAttackedBy()) pokemon.knownType = true`, and then the
        // attacks of Pokémon that have left the field are forgotten.)
        let mut by = m.said.attacked_by;
        let known = m.said.known || by != 0;
        for bit in 0..2 * MAX_TEAM {
            let from = MonRef { side: (bit / MAX_TEAM) as u8, idx: (bit % MAX_TEAM) as u8 };
            if by & 1 << bit != 0 && !self.mon(from).is_active {
                by &= !(1 << bit);
            }
        }
        let m = self.mon_mut(r);
        (m.said.known, m.said.attacked_by) = (known, by);
        let seen = match m.illusion {
            0 => r,
            k => MonRef { side: r.side, idx: k - 1 },
        };
        let (types, n) = self.get_types(seen, true);
        let real = [types[0], if n > 1 { types[1] } else { Type::None }];
        if real != self.mon(seen).said.apparent {
            self.mon_mut(seen).said.apparent = real;
            self.told_types(r, real);
        }
    }

    /// With the turn's last word on types said: a Pokémon that took a foe's
    /// types this turn and has not been told what they are has, then, the ones
    /// it had before (Showdown speaks only of a change). The other side, whose
    /// Pokémon was copied, knew all along.
    fn types_settled(&mut self) {
        for side in 0..2 {
            for pos in 0..ACTIVE {
                let r = self.active(side, pos);
                let m = self.mon_mut(r);
                if !m.is_active || m.fainted {
                    continue;
                }
                if m.said.copied_foe && !m.said.told && m.illusion == 0 {
                    m.said.seen[side] = m.said.before;
                }
                (m.said.copied, m.said.copied_foe, m.said.told) = (false, false, false);
            }
        }
    }

    /// What the end of a turn works out for the coming move request about one
    /// active Pokémon: which of its moves are disabled (the flags must have been
    /// cleared first) and whether it is trapped.
    pub(crate) fn request_prep(&mut self, r: MonRef) {
        self.run_event(Ev::DisableMove, Some(r), None, Eff::None, Res::Undef);
        // Moves that disable themselves (Fake Out after the first turn).
        for k in 0..self.mon(r).n_moves as usize {
            let id = self.mon(r).moves[k].id;
            if MOVES[id as usize].events & Ev::DisableMove.bit() != 0 {
                let saved = self.am_len;
                let mi = self.new_am(id);
                self.single_event(Ev::DisableMove, Eff::Move(mi), None, Some(r), None, Eff::None, Res::Undef);
                self.am_len = saved;
            }
            // Gigaton Hammer cannot be chosen twice in a row.
            if MOVES[id as usize].flags & F_CANTUSETWICE != 0 && self.mon(r).last_move == id {
                self.mon_mut(r).moves[k].disabled = true;
                self.mon_mut(r).moves[k].hidden = false;
            }
        }
        self.mon_mut(r).trapped = Trapped::No;
        self.run_event(Ev::TrapPokemon, Some(r), None, Eff::None, Res::Undef);
        if self.type_allows(r, 6) {
            self.run_event(Ev::MaybeTrapPokemon, Some(r), None, Eff::None, Res::Undef);
        }
    }

    /// `Pokemon#getMoveRequestData`, as the move request is put together:
    /// a Pokémon locked into a move cannot switch either.
    pub(crate) fn request_locks(&mut self) {
        for side in 0..2 {
            if self.sides[side].pokemon_left == 0 {
                continue;
            }
            for pos in 0..ACTIVE {
                let r = self.active(side, pos);
                let locked = self.get_locked_move(r);
                // Showdown sends full move slots even for a fainted active Pokémon,
                // but a hard lock or Struggle lists no PP. Switch/wait requests never
                // reach this move-request preparation.
                if locked.is_none() && self.usable_moves(r) {
                    self.mon_mut(r).unknown_pp = 0;
                }
                if !self.in_play(r) {
                    continue;
                }
                self.mon_mut(r).locked_move = locked.unwrap_or(NO_MOVE);
                if locked.is_some() {
                    self.mon_mut(r).trapped = Trapped::Yes;
                }
            }
        }
    }

    // ------------------------------------------------------- active move slots

    /// `dex.getActiveMove`: a fresh mutable copy of a move in the next slot.
    pub(crate) fn new_am(&mut self, move_id: u16) -> u8 {
        let mi = self.am_len;
        assert!((mi as usize) < AM_CAP, "too many nested moves");
        self.am[mi as usize] = ActiveMove::new(move_id);
        self.am_len += 1;
        mi
    }

    /// `Battle#setActiveMove`.
    pub(crate) fn set_active_move(&mut self, mi: Option<u8>, pokemon: Option<MonRef>, target: Option<MonRef>) {
        self.active_move = mi;
        self.active_pokemon = pokemon;
        self.active_target = target.or(pokemon);
    }

    /// `Battle#clearActiveMove`.
    pub(crate) fn clear_active_move(&mut self, failed: bool) {
        if let (Some(mi), false) = (self.active_move, failed) {
            self.last_move = self.am[mi as usize].id;
        }
        self.active_move = None;
        self.active_pokemon = None;
        self.active_target = None;
    }
}

/// `dex.getEffectiveness` of one type against one type: 1, 0 or -1.
pub(crate) fn type_effectiveness(attack: Type, defend: Type) -> i32 {
    if matches!(attack, Type::Typeless | Type::None) || matches!(defend, Type::Typeless | Type::None) {
        return 0;
    }
    match TYPE_CHART[attack as usize][defend as usize] {
        1 => 1,
        2 => -1,
        _ => 0,
    }
}

/// A fraction of something as Showdown's damage and heal functions end up
/// using it: floored, but at least 1 when the numerator is positive.
pub(crate) fn div1(num: u32, den: u32) -> i32 {
    if num == 0 { 0 } else { (num / den).max(1) as i32 }
}

/// `Pokemon#getTypes` for a Pokémon with these base types: a roosting one is
/// not Flying; with no type left it is Normal; a type added by Forest's Curse
/// or Trick-or-Treat comes last.
pub(crate) fn types_now(base: [Type; 2], roosting: bool, added: Type) -> ([Type; 3], usize) {
    let mut out = [Type::None; 3];
    let mut n = 0;
    for &t in &base {
        if t != Type::None && !(roosting && t == Type::Flying) {
            out[n] = t;
            n += 1;
        }
    }
    if n == 0 {
        out[0] = Type::Normal;
        n = 1;
    }
    if added != Type::None {
        out[n] = added;
        n += 1;
    }
    (out, n)
}

/// The base types Reflect Type takes from a Pokémon that has these: as they
/// count right now, without the `???` a Burn Up leaves, and Normal if that
/// leaves nothing. (The real move fails in that last case unless a type has
/// been added; this is also used for what a side takes the types to be.)
pub(crate) fn types_copied(base: [Type; 2], roosting: bool) -> [Type; 2] {
    let (types, n) = types_now(base, roosting, Type::None);
    let mut new = [Type::None; 2];
    let mut k = 0;
    for &t in &types[..n] {
        if t != Type::Typeless && k < 2 {
            new[k] = t;
            k += 1;
        }
    }
    if k == 0 {
        new[0] = Type::Normal;
    }
    new
}
