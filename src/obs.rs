//! What a player sees, as arrays: the input to a model.
//!
//! One observation is one side's view of a game at a decision, written into
//! three flat buffers that a caller owns (so that thousands can be filled in
//! place, in parallel, with nothing allocated):
//!
//! * `f32` features, [`OBS_F`] of them: the field, twelve Pokémon, the four
//!   positions on the field and the eight moves the side's own two can pick;
//! * `i16` ids, [`OBS_I`]: species, items, abilities and moves as table
//!   indices for an embedding to look up, and four numbers about the decision;
//! * `u8` masks, [`OBS_M`]: which actions are legal (see [`crate::env`]).
//!
//! [`layout`] says where each part lies. The side's own team is given in
//! full. Of the other side it is given what [`crate::shown`] has: what Team
//! Preview and the team sheets said, and what the battle has shown since.
//! Nothing is read from the opponent's Pokémon beyond that, bar what is in
//! plain sight on the field (stat stages, the conditions a Pokémon is under).
//!
//! The Pokémon come in the order their teams were registered: the viewer's
//! six, then the opponent's six. An opposing Pokémon that has appeared is
//! put where the Pokémon it appears to be was registered
//! ([`crate::shown::ShownMon::listed`]), so a token is one Pokémon from Team
//! Preview to the end.
//!
//! # Ids
//!
//! An id of 0 is "nothing here", 1 is "there is something and the viewer
//! does not know what", and a table index `k` is given as `k + 2`. So item
//! `2` is "holds nothing" (the item table's first entry) while item `1` is
//! "holds something unknown, or nothing".
//!
//! # Speed
//!
//! Each Pokémon comes with the lowest and highest its Speed can be and what
//! is known of an item that changes it, and each position with who goes first
//! against each position across the field. For the viewer's own Pokémon
//! these are facts. For the other side's they are what the order of moves has
//! left possible ([`crate::speed`]).
//!
//! # Move information
//!
//! Active move ids are the last visibly attempted queued moves, including
//! Choice-item refusals. Own PP is normalized to [0, 1], or -1 when a failed
//! move's hidden random target leaves its Pressure cost uncertain. An exact
//! move request restores the count; wait and switch requests do not.
//!
//! # Timers
//!
//! How long a weather, a terrain or a screen has left is not public: an item
//! the viewer may not have seen makes them last 8 turns for 5. How long they
//! have been up is, and that is what is given ([`Timers`]). Conditions no
//! item extends (Trick Room, Tailwind) are given by the turns they have left.

use std::sync::OnceLock;

use crate::battle::{PokemonSet, calc_stats};
use crate::data::*;
use crate::shown::{ItemShown, Shown, UNKNOWN};
use crate::speed::{self, Belief, First, Speeds};
use crate::state::*;

/// Pokémon a player registers. The environment plays the regulation's six.
pub const ROSTER: usize = 6;
/// Pokémon tokens: the viewer's six, then the opponent's.
pub const MONS: usize = 2 * ROSTER;
/// Ids per Pokémon: species, item, the item it has lost, ability, four moves.
pub const MON_IDS: usize = 8;
/// Positions on the field: the viewer's two, then the opponent's two.
pub const ACTIVES: usize = 2 * ACTIVE;
/// Ids per position: the Pokémon token standing there plus one (0: nobody), and its last visibly attempted move.
pub const ACT_IDS: usize = 2;
/// Moves the viewer's two active Pokémon can pick from, four each.
pub const MOVE_TOKENS: usize = ACTIVE * MAX_MOVES;
/// Numbers about the decision: phase, how many joint actions are legal, the turn, whether there is a choice to make.
pub const INFO: usize = 4;

pub const FIELD_F: usize = 64;
pub const MON_F: usize = 52;
pub const ACT_F: usize = 125;
pub const MOVE_F: usize = 4;
/// Where a position's volatile conditions begin in its features, one flag for each [`VolKind`].
pub const ACT_VOLATILES: usize = 38;

pub const OBS_F: usize = FIELD_F + MONS * MON_F + ACTIVES * ACT_F + MOVE_TOKENS * MOVE_F;
pub const OBS_I: usize = MONS * MON_IDS + ACTIVES * ACT_IDS + MOVE_TOKENS + INFO;

pub const ID_NONE: i16 = 0;
pub const ID_UNKNOWN: i16 = 1;
pub const ID_BASE: i16 = 2;

fn id(index: u16) -> i16 {
    index as i16 + ID_BASE
}

/// What a game is waiting for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Phase {
    /// Team Preview: each side picks four of its six.
    Preview = 0,
    /// Both sides choose for the turn.
    Move = 1,
    /// One or both sides replace Pokémon.
    Switch = 2,
}

/// A named part of an observation: where it begins in its buffer, and its shape.
pub type Part = (&'static str, usize, Vec<usize>);

/// Where each part of an observation lies: the parts of the feature buffer, then those of the id buffer.
pub fn layout() -> (Vec<Part>, Vec<Part>) {
    let mut f = Vec::new();
    let mut at = 0;
    for (name, shape) in [
        ("field", vec![FIELD_F]),
        ("mon_feats", vec![MONS, MON_F]),
        ("act_feats", vec![ACTIVES, ACT_F]),
        ("move_feats", vec![MOVE_TOKENS, MOVE_F]),
    ] {
        f.push((name, at, shape.clone()));
        at += shape.iter().product::<usize>();
    }
    debug_assert_eq!(at, OBS_F);
    let mut i = Vec::new();
    let mut at = 0;
    for (name, shape) in [
        ("mon_ids", vec![MONS, MON_IDS]),
        ("act_ids", vec![ACTIVES, ACT_IDS]),
        ("move_ids", vec![MOVE_TOKENS]),
        ("info", vec![INFO]),
    ] {
        i.push((name, at, shape.clone()));
        at += shape.iter().product::<usize>();
    }
    debug_assert_eq!(at, OBS_I);
    (f, i)
}

/// Volatile conditions that are left out, for either side: the ones the
/// game keeps to itself. An item or ability that has not shown itself keeps
/// some (a Choice lock, Unburden). Some are the engine's own notes during a
/// turn (Counter's memory of the last hit). And some are things no line of
/// Showdown's log says, so that a player following a battle from the log
/// could not be given them: a flinch that has yet to stop a move, how far
/// into a rampage a Pokémon is. What matters of those to a player's own
/// Pokémon comes with its moves instead: which are disabled, and whether it
/// is locked into one.
pub const HIDDEN_VOLATILES: [VolKind; 16] = [
    VolKind::Flinch,
    VolKind::Choicelock,
    VolKind::Gem,
    VolKind::Metronome,
    VolKind::Unburden,
    VolKind::Trapper,
    VolKind::Sparklingaria,
    VolKind::Lockedmove,
    VolKind::Focuspunch,
    VolKind::Beakblast,
    VolKind::Counter,
    VolKind::Mirrorcoat,
    VolKind::Gigatonhammer,
    VolKind::Fling,
    VolKind::Allyswitch,
    VolKind::Chillyreception,
];

fn megas() -> &'static [bool] {
    static MEGAS: OnceLock<Vec<bool>> = OnceLock::new();
    MEGAS.get_or_init(|| {
        let mut out = vec![false; SPECIES.len()];
        for &(_, to) in ITEMS.iter().flat_map(|i| i.mega.iter()) {
            out[to as usize] = true;
        }
        out
    })
}

/// Whether `species` is a Mega Evolution.
pub fn is_mega(species: u16) -> bool {
    megas().get(species as usize).copied().unwrap_or(false)
}

fn mega_stone_for(item: u16, species: u16) -> bool {
    ITEMS[item as usize].mega.iter().any(|&(from, _)| from == species)
}

// ------------------------------------------------------------------ timers

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct Timer {
    /// 0: not there.
    kind: u8,
    /// Which instance of the condition this is: the engine numbers every one it starts.
    uid: u16,
    began: u16,
    left: u8,
}

impl Timer {
    fn see(&mut self, kind: u8, uid: u16, left: u8, now: u16) {
        // New: another condition, or the same one put up again since the last decision.
        if kind != self.kind || uid != self.uid {
            (self.kind, self.uid) = (kind, uid);
            self.began = now;
        }
        self.left = left;
    }

    /// Turns the condition has been up for, as someone counting would have it.
    fn elapsed(&self, turn: u16) -> f32 {
        if self.kind == 0 {
            return 0.0;
        }
        let counted = turn.saturating_sub(self.began) as i32;
        if self.left == 0 {
            return counted as f32;
        }
        // It lasts 5 turns or 8, so it has been up for one of two lengths of time, three
        // turns apart. The count is right to within a turn (it cannot tell a condition that
        // began before the end-of-turn step from one that began after), which settles it.
        let short = 5 - self.left as i32;
        let long = 8 - self.left as i32;
        let pick = if short < 0 || (long - counted).abs() < (short - counted).abs() { long } else { short };
        pick.max(0) as f32
    }
}

/// When the weather, the terrain and each side's screens began, kept from
/// decision to decision so that what is given of them is how long they have
/// been up and not how long they have left. Part of a game's state, next to
/// its [`Battle`].
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Timers {
    weather: Timer,
    terrain: Timer,
    screens: [[Timer; 3]; 2],
}

const SCREENS: [SideCond; 3] = [SideCond::Reflect, SideCond::Lightscreen, SideCond::Auroraveil];

impl Timers {
    /// From a count kept some other way: how many turns have ended since the
    /// weather, the terrain and each side's Reflect, Light Screen and Aurora
    /// Veil went up (`None`: not there), with `turn` the turn it is now.
    pub fn counted(weather: Option<u16>, terrain: Option<u16>, screens: [[Option<u16>; 3]; 2], turn: u16) -> Timers {
        let timer = |count: Option<u16>| match count {
            Some(n) => Timer { kind: 1, uid: 0, began: turn.saturating_sub(n), left: 0 },
            None => Timer::default(),
        };
        Timers { weather: timer(weather), terrain: timer(terrain), screens: screens.map(|side| side.map(timer)) }
    }

    /// To be called at every decision, the first included.
    pub fn update(&mut self, b: &Battle) {
        // What has turned up since the last decision began during the turn before, if a
        // new turn has started, and during this one otherwise.
        let now = if b.request == Request::Move && b.turn > 1 { b.turn - 1 } else { b.turn };
        let w = &b.field.weather;
        self.weather.see(w.kind as u8, w.st.uid, w.duration, now);
        let t = &b.field.terrain;
        self.terrain.see(t.kind as u8, t.st.uid, t.duration, now);
        let before = self.screens;
        for side in 0..2 {
            for (k, &kind) in SCREENS.iter().enumerate() {
                match b.sides[side].conds.get(kind) {
                    Some(c) => {
                        // Court Change transfers the same condition instance,
                        // including its age. Recover its timer from either side.
                        if let Some(timer) =
                            before.iter().map(|row| row[k]).find(|timer| timer.kind != 0 && timer.uid == c.st.uid)
                        {
                            self.screens[side][k] = timer;
                        }
                        self.screens[side][k].see(1, c.st.uid, c.duration, now);
                    }
                    None => self.screens[side][k] = Timer::default(),
                }
            }
        }
    }
}

// A replacement can be on the field before its queued SwitchIn callback
// runs. Toxic's previous-stay counter is not public state for the new stay.
fn public_toxic_stage(b: &Battle, r: MonRef) -> u8 {
    if b.queue.as_slice().iter().any(|a| a.kind == crate::state::ActKind::RunSwitch && a.mon == Some(r)) {
        0
    } else {
        b.mon(r).tox_stage
    }
}

// ------------------------------------------------------------------ writing

struct W<'a> {
    buf: &'a mut [f32],
    at: usize,
}

impl W<'_> {
    fn put(&mut self, v: f32) {
        self.buf[self.at] = v;
        self.at += 1;
    }
    fn flag(&mut self, on: bool) {
        self.put(on as u8 as f32);
    }
    fn one_hot(&mut self, n: usize, k: usize) {
        debug_assert!(k < n);
        self.buf[self.at + k] = 1.0;
        self.at += n;
    }
    fn skip(&mut self, n: usize) {
        self.at += n;
    }
}

/// One Pokémon token, gathered before it is written.
struct Tok {
    own: bool,
    brought: bool,
    left_behind: bool,
    seen: bool,
    active: bool,
    fainted: bool,
    hp: f32,
    status: Status,
    tox: u8,
    stats: Option<[u16; 6]>,
    item_known: bool,
    ability_known: bool,
    moves_known: u8,
    can_mega: bool,
    maybe_disguise: bool,
    transformed: bool,
    gender: Gender,
    nature: Option<(u8, u8)>,
    pp: [f32; MAX_MOVES],
    /// The lowest and highest its Speed stat can be, and what is known of an item that changes it.
    speed: (u32, u32),
    speed_items: [bool; 4],
    species: i16,
    item: i16,
    lost: i16,
    ability: i16,
    moves: [i16; MAX_MOVES],
}

impl Tok {
    fn new(own: bool, species: u16, gender: Gender) -> Tok {
        Tok {
            own,
            brought: false,
            left_behind: false,
            seen: false,
            active: false,
            fainted: false,
            hp: 1.0,
            status: Status::None,
            tox: 0,
            stats: None,
            item_known: false,
            ability_known: false,
            moves_known: 0,
            can_mega: false,
            maybe_disguise: false,
            transformed: false,
            gender,
            nature: None,
            pp: [0.0; MAX_MOVES],
            speed: (0, 0),
            speed_items: [false; 4],
            species: id(species),
            item: ID_UNKNOWN,
            lost: ID_NONE,
            ability: ID_UNKNOWN,
            moves: [ID_UNKNOWN; MAX_MOVES],
        }
    }

    /// Its own side's Pokémon: the Speed stat is known.
    fn speed_known(&mut self, stat: u16, item: u16) {
        let (speed, items) = speed::own(stat, item);
        (self.speed, self.speed_items) = ((speed, speed), items);
    }

    /// The other side's: what the order of moves has left possible.
    fn speed_believed(&mut self, belief: &Belief, species: u16) {
        self.speed = belief.range(species);
        self.speed_items = belief.items();
    }

    /// Everything a team sheet says.
    fn sheet(&mut self, item: u16, ability: u16, moves: &[u16], nature: (u8, u8)) {
        self.item = id(item);
        self.ability = id(ability);
        self.moves = [ID_NONE; MAX_MOVES];
        for (k, &m) in moves.iter().take(MAX_MOVES).enumerate() {
            self.moves[k] = id(m);
        }
        self.nature = Some(nature);
        (self.item_known, self.ability_known, self.moves_known) = (true, true, MAX_MOVES as u8);
    }

    fn write(&self, f: &mut [f32], i: &mut [i16]) {
        let mut w = W { buf: f, at: 0 };
        w.flag(true);
        w.flag(self.own);
        w.flag(self.brought);
        w.flag(self.left_behind);
        w.flag(self.seen);
        w.flag(self.active);
        w.flag(self.fainted);
        w.put(self.hp);
        w.one_hot(7, self.status as usize);
        w.put(self.tox as f32 / 15.0);
        match self.stats {
            Some(stats) => {
                stats.iter().for_each(|&s| w.put(s as f32 / 200.0));
                w.flag(true);
            }
            None => w.skip(7),
        }
        w.flag(self.item_known);
        w.flag(self.ability_known);
        w.put(self.moves_known as f32 / MAX_MOVES as f32);
        w.flag(self.can_mega);
        w.flag(self.maybe_disguise);
        w.flag(self.transformed);
        w.one_hot(3, self.gender as usize);
        match self.nature {
            Some((up, down)) if up != 0 => {
                w.one_hot(5, up as usize - 1);
                w.one_hot(5, down as usize - 1);
            }
            _ => w.skip(10),
        }
        self.pp.iter().for_each(|&p| w.put(p));
        w.put(self.speed.0 as f32 / 200.0);
        w.put(self.speed.1 as f32 / 200.0);
        self.speed_items.iter().for_each(|&on| w.flag(on));
        debug_assert_eq!(w.at, MON_F);
        i[0] = self.species;
        i[1] = self.item;
        i[2] = self.lost;
        i[3] = self.ability;
        i[4..8].copy_from_slice(&self.moves);
    }
}

fn shown_item(tok: &mut Tok, rec: &Shown) {
    match rec.item {
        ItemShown::Unknown => {}
        ItemShown::Holds(item) => (tok.item, tok.lost, tok.item_known) = (id(item), ID_NONE, true),
        ItemShown::Lost(item) => (tok.item, tok.lost, tok.item_known) = (id(it::NONE), id(item), true),
    }
}

/// The parts of one observation.
struct Parts<'a> {
    field: &'a mut [f32],
    mon_feats: &'a mut [f32],
    act_feats: &'a mut [f32],
    move_feats: &'a mut [f32],
    mon_ids: &'a mut [i16],
    act_ids: &'a mut [i16],
    move_ids: &'a mut [i16],
    info: &'a mut [i16],
}

fn parts<'a>(f: &'a mut [f32], i: &'a mut [i16]) -> Parts<'a> {
    assert!(f.len() == OBS_F && i.len() == OBS_I, "an observation buffer of the wrong size");
    f.fill(0.0);
    i.fill(0);
    let (field, f) = f.split_at_mut(FIELD_F);
    let (mon_feats, f) = f.split_at_mut(MONS * MON_F);
    let (act_feats, move_feats) = f.split_at_mut(ACTIVES * ACT_F);
    let (mon_ids, i) = i.split_at_mut(MONS * MON_IDS);
    let (act_ids, i) = i.split_at_mut(ACTIVES * ACT_IDS);
    let (move_ids, info) = i.split_at_mut(MOVE_TOKENS);
    Parts { field, mon_feats, act_feats, move_feats, mon_ids, act_ids, move_ids, info }
}

impl Parts<'_> {
    fn mon(&mut self, token: usize, tok: &Tok) {
        tok.write(
            &mut self.mon_feats[token * MON_F..(token + 1) * MON_F],
            &mut self.mon_ids[token * MON_IDS..(token + 1) * MON_IDS],
        );
    }
}

/// Team Preview from `view`'s side: its own six in full, and the other six as
/// Team Preview shows them, with their sheets if those are open. `stats` are
/// the viewer's own Pokémon's.
pub fn observe_preview(
    rosters: [&[PokemonSet]; 2],
    speeds: &Speeds,
    stats: &[[u16; 6]; ROSTER],
    open: bool,
    view: usize,
    f: &mut [f32],
    i: &mut [i16],
) {
    let mut p = parts(f, i);
    let mut w = W { buf: p.field, at: 0 };
    w.one_hot(3, Phase::Preview as usize);
    w.flag(open);
    w.skip(FIELD_F - 9);
    // Everyone is still to come: four each, none of them seen.
    (0..3).for_each(|_| w.put(1.0));
    w.skip(2);
    debug_assert_eq!(w.at, FIELD_F);
    for (j, set) in rosters[view].iter().take(ROSTER).enumerate() {
        let mut tok = Tok::new(true, set.species, set.gender);
        tok.sheet(set.item, set.ability, &set.moves, set.nature);
        tok.stats = Some(stats[j]);
        tok.can_mega = mega_stone_for(set.item, set.species);
        tok.pp = [0.0; MAX_MOVES];
        tok.pp[..set.moves.len().min(MAX_MOVES)].fill(1.0);
        tok.speed_known(stats[j][5], set.item);
        p.mon(j, &tok);
    }
    for (j, set) in rosters[1 - view].iter().take(ROSTER).enumerate() {
        let mut tok = Tok::new(false, set.species, set.gender);
        if open {
            tok.sheet(set.item, set.ability, &set.moves, set.nature);
            tok.can_mega = mega_stone_for(set.item, set.species);
        }
        tok.speed_believed(speeds.belief(1 - view, j), set.species);
        p.mon(ROSTER + j, &tok);
    }
    p.info[0] = Phase::Preview as i16;
    p.info[3] = 1;
}

/// A battle from `view`'s side. `stats` are the viewer's own Pokémon's as
/// registered (for the two left behind, which the battle does not hold).
/// Whether nobody on `side` chooses after the Pokémon at `pos`: the one Showdown keeps
/// in the dark about what an unseen ability or move bars it from.
pub(crate) fn last_to_choose(b: &Battle, side: usize, pos: usize) -> bool {
    (pos + 1..ACTIVE.min(b.sides[side].n as usize)).all(|p| {
        let ally = b.mon(b.active(side, p));
        !ally.is_active || ally.fainted
    })
}

/// The last two numbers of `info` (how many joint actions are legal, and
/// whether there is a choice to make) are left for whoever works out the
/// legal actions.
pub fn observe_battle(
    b: &Battle,
    timers: &Timers,
    speeds: &Speeds,
    stats: &[[u16; 6]; ROSTER],
    view: usize,
    f: &mut [f32],
    i: &mut [i16],
) {
    let mut p = parts(f, i);
    let opp = 1 - view;
    let (us, them) = (&b.sides[view], &b.sides[opp]);
    let megaed = |s: &Side| s.mega_used;
    let phase = if b.request == Request::Switch { Phase::Switch } else { Phase::Move };

    // ---- the field
    let mut w = W { buf: p.field, at: 0 };
    w.one_hot(3, phase as usize);
    w.flag(b.open_team_sheets);
    w.put(b.turn.min(40) as f32 / 40.0);
    w.one_hot(5, b.field.weather.kind as usize);
    w.put(timers.weather.elapsed(b.turn) / 8.0);
    w.one_hot(5, b.field.terrain.kind as usize);
    w.put(timers.terrain.elapsed(b.turn) / 8.0);
    for kind in Pseudo::ALL {
        w.flag(b.field.pseudo.has(kind));
    }
    for kind in Pseudo::ALL {
        w.put(b.field.pseudo.get(kind).map_or(0.0, |c| c.duration as f32 / 5.0));
    }
    for side in [view, opp] {
        let conds = &b.sides[side].conds;
        for kind in SideCond::ALL {
            w.put(match (conds.get(kind), kind) {
                (None, _) => 0.0,
                (Some(c), SideCond::Spikes) => c.data as f32 / 3.0,
                (Some(c), SideCond::Toxicspikes) => c.data as f32 / 2.0,
                (Some(_), _) => 1.0,
            });
        }
        w.put(conds.get(SideCond::Tailwind).map_or(0.0, |c| c.duration as f32 / 4.0));
        w.put(conds.get(SideCond::Safeguard).map_or(0.0, |c| c.duration as f32 / 5.0));
        for k in 0..3 {
            w.put(timers.screens[side][k].elapsed(b.turn) / 8.0);
        }
    }
    w.put(us.pokemon_left as f32 / 4.0);
    w.put(them.pokemon_left as f32 / 4.0);
    w.put((them.n - them.n_seen) as f32 / 4.0);
    w.flag(megaed(us));
    w.flag(megaed(them));
    debug_assert_eq!(w.at, FIELD_F);

    // ---- the viewer's own six
    let mut own_token = [0usize; MAX_TEAM];
    for (j, listed) in us.roster[..us.n_roster as usize].iter().enumerate().take(ROSTER) {
        let mut tok = Tok::new(true, listed.species, listed.gender);
        if listed.brought == NOT_LISTED {
            tok.sheet(listed.item, listed.ability, listed.moves(), listed.nature);
            tok.stats = Some(stats[j]);
            tok.left_behind = true;
            tok.speed_known(stats[j][5], listed.item);
            p.mon(j, &tok);
            continue;
        }
        let idx = listed.brought as usize;
        own_token[idx] = j;
        let m = &us.team[idx];
        tok.species = id(m.species);
        tok.brought = true;
        tok.seen = us.shown[idx].seen != 0 || (m.is_active && m.live.seen != 0);
        tok.fainted = m.fainted;
        tok.active = m.is_active && !m.fainted;
        tok.hp = m.hp as f32 / m.max_hp().max(1) as f32;
        // (A status a fainted Pokémon had is neither here nor there.)
        tok.status = if m.fainted { Status::None } else { m.status };
        // (The count starts over whenever it comes in.)
        tok.tox = if m.is_active && m.status == Status::Tox {
            public_toxic_stage(b, MonRef { side: view as u8, idx: idx as u8 })
        } else {
            0
        };
        // Its stats as its player is told them: its own, for the forme it is in. What a
        // battle does to them is not said (after Transform or Power Split they are partly
        // another Pokémon's).
        let mut known = calc_stats(if m.transformed { m.base_species } else { m.species }, m.nature, m.stat_points);
        known[0] = m.stats[0];
        tok.stats = Some(known);
        tok.item = id(m.item);
        // The item it was seen to lose, as the other side knows it.
        let rec = if m.is_active && m.live.seen != 0 { &m.live } else { &us.shown[idx] };
        tok.lost = match rec.item {
            ItemShown::Lost(item) if m.item == it::NONE => id(item),
            _ => ID_NONE,
        };
        tok.ability = id(m.ability);
        (tok.item_known, tok.ability_known, tok.moves_known) = (true, true, MAX_MOVES as u8);
        tok.moves = [ID_NONE; MAX_MOVES];
        for (k, slot) in m.moves[..m.n_moves as usize].iter().enumerate() {
            tok.moves[k] = id(slot.id);
            tok.pp[k] = if m.unknown_pp & (1 << k) != 0 { -1.0 } else { slot.pp as f32 / slot.maxpp.max(1) as f32 };
        }
        tok.can_mega = m.can_mega != NO_SPECIES;
        tok.transformed = m.transformed;
        tok.gender = m.gender;
        tok.nature = Some(m.nature);
        tok.speed_known(known[5], m.item);
        p.mon(j, &tok);
    }

    // ---- the opponent's six, as far as they are known
    let roster = &them.roster[..them.n_roster as usize];
    let mut filled = [false; ROSTER];
    let mut opp_token = [None; ACTIVE];
    let mut on_field = [false; MAX_TEAM];
    let they_megaed = megaed(them);
    // Those on the field first, then the rest of those that have appeared. `a` is the team
    // index of the Pokémon each appears to be. (Two on the field can appear to be the same
    // one, when an Illusion Pokémon stands beside the Pokémon it copies. They share that
    // one's token, which takes the first's HP; each position has its own HP as well.)
    let standing =
        (0..ACTIVE.min(them.n as usize)).map(|pos| (pos, b.active(opp, pos))).filter(|&(_, r)| b.shown_live(r));
    let seen = standing.map(|(pos, r)| (b.shown_as(r), Some((pos, r)))).collect::<Vec<_>>();
    seen.iter().for_each(|&(a, _)| on_field[a] = true);
    let benched = (0..them.n as usize).filter(|&a| !on_field[a]).map(|a| (a, None));
    for (a, live) in seen.iter().copied().chain(benched) {
        let rec = match live {
            Some((_, r)) => &b.mon(r).live,
            None => &them.shown[a],
        };
        if rec.seen == 0 || rec.listed == NOT_LISTED || rec.listed as usize >= roster.len().min(ROSTER) {
            continue;
        }
        let j = rec.listed as usize;
        if let Some((pos, _)) = live {
            opp_token[pos] = Some(ROSTER + j);
        }
        if filled[j] {
            continue;
        }
        let listed = &roster[j];
        let mut tok = Tok::new(false, rec.species, listed.gender);
        if b.open_team_sheets {
            tok.sheet(listed.item, listed.ability, listed.moves(), listed.nature);
        } else {
            for (k, &m) in rec.moves().iter().take(MAX_MOVES).enumerate() {
                tok.moves[k] = id(m);
            }
            tok.moves_known = rec.n_moves.min(MAX_MOVES as u8);
            // And the moves it has not used that it is known to have: ones of the viewer's
            // own that its Imprison sealed.
            for &m in listed.told() {
                let n = tok.moves_known as usize;
                if n < MAX_MOVES && !tok.moves[..n].contains(&id(m)) {
                    tok.moves[n] = id(m);
                    tok.moves_known += 1;
                }
            }
        }
        shown_item(&mut tok, rec);
        if rec.ability != UNKNOWN {
            (tok.ability, tok.ability_known) = (id(rec.ability), true);
        }
        (tok.brought, tok.seen) = (true, true);
        tok.maybe_disguise = rec.suspect || rec.tainted;
        match live {
            Some((_, r)) => {
                let m = b.mon(r);
                // A disguise is the Pokémon it copies, as that one looked when the disguise came in.
                tok.species = id(if m.illusion != 0 { rec.species } else { m.species });
                let (hp, _, status) = b.shown_condition(r);
                (tok.hp, tok.status, tok.active) = (hp as f32 / 100.0, status, true);
                tok.tox = if status == Status::Tox { public_toxic_stage(b, r) } else { 0 };
                tok.transformed = m.transformed;
            }
            None => (tok.hp, tok.status, tok.fainted) = (rec.hp as f32 / 100.0, rec.status, rec.fainted),
        }
        // A Mega Stone in plain sight, on a side that has not used one yet.
        let species = (tok.species - ID_BASE) as u16;
        tok.can_mega = !they_megaed && tok.item >= ID_BASE && mega_stone_for((tok.item - ID_BASE) as u16, species);
        tok.speed_believed(speeds.belief(opp, j), species);
        filled[j] = true;
        p.mon(ROSTER + j, &tok);
    }
    for (j, listed) in roster.iter().enumerate().take(ROSTER) {
        if filled[j] {
            continue;
        }
        let mut tok = Tok::new(false, listed.species, listed.gender);
        if b.open_team_sheets {
            tok.sheet(listed.item, listed.ability, listed.moves(), listed.nature);
            tok.can_mega = !they_megaed && mega_stone_for(listed.item, listed.species);
        }
        // Once all the Pokémon brought have appeared, the rest are known to have stayed behind.
        tok.left_behind = them.n_seen == them.n;
        tok.speed_believed(speeds.belief(opp, j), listed.species);
        p.mon(ROSTER + j, &tok);
    }

    // ---- the four positions
    // Who goes first of each of the viewer's two and each of the other two, as far as the viewer can tell.
    let mut firsts = [[None; ACTIVE]; ACTIVE];
    for (mine, row) in firsts.iter_mut().enumerate() {
        for (theirs, first) in row.iter_mut().enumerate() {
            *first = speeds.first(b, view, mine, theirs);
        }
    }
    for (slot, (side, pos)) in [(view, 0), (view, 1), (opp, 0), (opp, 1)].into_iter().enumerate() {
        let own = side == view;
        if pos >= b.sides[side].n as usize {
            continue;
        }
        let r = b.active(side, pos);
        let m = b.mon(r);
        let (there, token) = if own {
            (m.is_active && !m.fainted, Some(own_token[r.idx as usize]))
        } else {
            (b.shown_live(r), opp_token[pos])
        };
        if !there {
            continue;
        }
        p.act_ids[slot * ACT_IDS] = token.map_or(0, |t| t as i16 + 1);
        let attempted = m.last_attempted_move.unwrap_or(m.last_move);
        p.act_ids[slot * ACT_IDS + 1] = if attempted == NO_MOVE { ID_NONE } else { id(attempted) };
        let mut w = W { buf: &mut p.act_feats[slot * ACT_F..(slot + 1) * ACT_F], at: 0 };
        w.flag(true);
        // HP and status here too: its own exactly, the other side's as the bar shows it.
        if own {
            w.put(m.hp as f32 / m.max_hp().max(1) as f32);
            w.one_hot(7, m.status as usize);
        } else {
            let (hp, _, status) = b.shown_condition(r);
            w.put(hp as f32 / 100.0);
            w.one_hot(7, status as usize);
        }
        m.boosts.iter().for_each(|&s| w.put(s as f32 / 6.0));
        // Its types as this side has been told them: for one of the other side's in
        // disguise, those of the Pokémon it passes for.
        let (types, n) = b.seen_types(r, view);
        for &t in &types[..n] {
            if (t as usize) < 18 {
                w.buf[w.at + t as usize] = 1.0;
            }
        }
        w.skip(18);
        w.flag(m.active_turns == 0);
        w.put(m.active_turns.min(5) as f32 / 5.0);
        // Kept from switching, as its player is told when it chooses a move: not if there is
        // nobody to switch to. And whether that move is chosen for it.
        let choosing = own && phase == Phase::Move;
        // (Held in by an ability nobody has shown, the last of a side to choose is not told:
        // it would give the secret away. It finds out by trying.)
        let last = last_to_choose(b, side, pos);
        let held = m.trapped == Trapped::Yes || (m.trapped == Trapped::Hidden && !last);
        w.flag(choosing && held && b.living_bench(side) > 0);
        w.flag(choosing && m.locked_move != NO_MOVE);
        let vols = b.vols(r);
        debug_assert_eq!(w.at, ACT_VOLATILES);
        for v in vols.as_slice() {
            if !HIDDEN_VOLATILES.contains(&v.kind) {
                w.buf[w.at + v.kind as usize] = 1.0;
            }
        }
        if !own && m.live.copied_gastro && m.live.ability == crate::shown::UNKNOWN && !b.open_team_sheets {
            // Suppression's silent removal by a hidden ability is not public.
            w.buf[w.at + VolKind::Gastroacid as usize] = 1.0;
        }
        w.skip(N_VOLATILES);
        w.put(vols.get(VolKind::Perishsong).map_or(0.0, |v| v.duration as f32 / 3.0));
        // How unlikely another Protect is to work: 0 after none, 2/3 after one, 8/9 after two.
        w.put(vols.get(VolKind::Stall).map_or(0.0, |v| 1.0 - 1.0 / v.data.max(1) as f32));
        for kind in SlotCond::ALL {
            w.flag(b.sides[side].slot_conds[pos].has(kind));
        }
        // Against each position across the field, with moves of the same priority: this one
        // goes first, goes second, or the viewer cannot tell.
        for (across, row) in firsts.iter().enumerate() {
            match if own { firsts[pos][across] } else { row[pos] } {
                None => w.skip(3),
                Some(First::Unknown) => w.one_hot(3, 2),
                Some(who) => w.one_hot(3, ((who == First::Mine) != own) as usize),
            }
        }
        debug_assert_eq!(w.at, ACT_F);
    }

    // ---- the moves the viewer's two can pick, as Showdown's request lists them (when
    // moves are being chosen: a request for replacements lists none)
    for pos in 0..ACTIVE.min(us.n as usize) {
        let r = b.active(view, pos);
        let m = b.mon(r);
        if !m.is_active || m.fainted || phase != Phase::Move {
            continue;
        }
        let mut put = |k: usize, mv: u16, pp: f32, disabled: bool, forced: bool| {
            let t = pos * MAX_MOVES + k;
            p.move_ids[t] = id(mv);
            p.move_feats[t * MOVE_F..(t + 1) * MOVE_F].copy_from_slice(&[
                1.0,
                pp,
                disabled as u8 as f32,
                forced as u8 as f32,
            ]);
        };
        if m.locked_move != NO_MOVE {
            // Locked into a move: that move alone, and nothing to pick.
            put(0, m.locked_move, 1.0, false, true);
        } else if !b.usable_moves(r) {
            put(0, crate::battle::struggle_id(), 1.0, false, true);
        } else {
            // A move disabled by something not yet shown (a foe's Imprison) is listed as
            // usable for the last Pokémon to choose, as Showdown lists it: to say otherwise
            // would give the secret away.
            let last = last_to_choose(b, view, pos);
            for (k, slot) in m.moves[..m.n_moves as usize].iter().enumerate() {
                let disabled = slot.pp == 0 || (slot.disabled && !(slot.hidden && last));
                put(k, slot.id, slot.pp as f32 / slot.maxpp.max(1) as f32, disabled, false);
            }
        }
    }

    p.info[0] = phase as i16;
    p.info[2] = b.turn.min(i16::MAX as u16) as i16;
}

// ------------------------------------------------------------- dex tables

/// A table of static features, one row per id (rows 0 and 1, "nothing" and "unknown", are zero).
pub struct Table {
    pub rows: usize,
    pub cols: usize,
    pub data: Vec<f32>,
}

fn table<T>(entries: &[T], row: impl Fn(&T, &mut Vec<f32>)) -> Table {
    let mut data = Vec::new();
    let mut cols = 0;
    for (k, e) in entries.iter().enumerate() {
        let before = data.len();
        row(e, &mut data);
        if k == 0 {
            cols = data.len();
            // The two rows that stand for no entry.
            data = [vec![0.0; 2 * cols], data].concat();
        } else {
            assert_eq!(data.len() - before, cols, "rows of different lengths");
        }
    }
    Table { rows: entries.len() + ID_BASE as usize, cols, data }
}

fn hot(out: &mut Vec<f32>, n: usize, k: usize) {
    let at = out.len();
    out.resize(at + n, 0.0);
    if k < n {
        out[at + k] = 1.0;
    }
}

fn boosts(out: &mut Vec<f32>, b: Option<Boosts>) {
    out.extend(b.unwrap_or([0; 7]).iter().map(|&s| s as f32 / 3.0));
}

/// Species: types, base stats, weight, whether it is a Mega Evolution.
pub fn species_table() -> Table {
    let megas = megas();
    let all: Vec<(usize, &SpeciesData)> = SPECIES.iter().enumerate().collect();
    table(&all, |&(k, s), out| {
        let at = out.len();
        out.resize(at + 18, 0.0);
        for t in s.types {
            if (t as usize) < 18 {
                out[at + t as usize] = 1.0;
            }
        }
        out.extend(s.base.iter().map(|&v| v as f32 / 200.0));
        out.push((s.weight_hg as f32 / 10.0).ln_1p() / 7.0);
        out.push(megas[k] as u8 as f32);
    })
}

/// Moves: type, category, power, accuracy, priority, who it hits, flags and what it does.
pub fn move_table() -> Table {
    table(&MOVES, |m, out| {
        hot(out, 19, if m.typ == Type::Typeless { 18 } else { m.typ as usize });
        hot(out, 3, m.category as usize);
        out.push(m.base_power as f32 / 150.0);
        out.push(if m.accuracy == 0 { 1.0 } else { m.accuracy as f32 / 100.0 });
        out.push((m.accuracy == 0) as u8 as f32);
        out.push(m.priority as f32 / 5.0);
        hot(out, 15, m.target as usize);
        out.push(matches!(m.target, Target::AllAdjacentFoes | Target::AllAdjacent) as u8 as f32);
        out.extend((0..32).map(|bit| (m.flags >> bit & 1) as f32));
        out.push(m.pp as f32 / 32.0);
        for (num, den) in [m.drain, m.recoil, m.heal] {
            out.push(if den == 0 { 0.0 } else { num as f32 / den as f32 });
        }
        out.push(m.multihit.1 as f32 / 5.0);
        out.push(m.crit_ratio as f32 / 3.0);
        for flag in [
            m.will_crit,
            m.stalling_move,
            m.breaks_protect,
            m.self_switch != SelfSwitch::No,
            m.force_switch,
            m.ohko != Ohko::No,
            m.fixed_damage != FixedDamage::No,
            m.selfdestruct != SelfDestruct::No,
            m.sleep_usable,
            m.calls_move,
            m.ignore_defensive,
            m.volatile.is_some(),
            m.self_volatile.is_some(),
        ] {
            out.push(flag as u8 as f32);
        }
        hot(out, 7, m.status as usize);
        boosts(out, m.boosts);
        boosts(out, m.self_boosts);
        // The likeliest of its secondary effects.
        let sec = m.secondaries.iter().max_by_key(|s| if s.chance == 0 { 100 } else { s.chance });
        out.push(sec.map_or(0.0, |s| if s.chance == 0 { 1.0 } else { s.chance as f32 / 100.0 }));
        hot(out, 7, sec.map_or(0, |s| s.status as usize));
        out.push(sec.is_some_and(|s| s.volatile == Some(VolKind::Flinch)) as u8 as f32);
        boosts(out, sec.and_then(|s| s.boosts));
        boosts(out, sec.and_then(|s| s.self_boosts));
        hot(out, 5, m.weather as usize);
        hot(out, 5, m.terrain as usize);
        hot(out, N_SIDE_CONDS, m.side_condition.map_or(usize::MAX, |c| c as usize));
        hot(out, N_PSEUDO, m.pseudo_weather.map_or(usize::MAX, |c| c as usize));
    })
}

/// Items: what kind of item it is, and how hard it hits when thrown.
pub fn item_table() -> Table {
    table(&ITEMS, |item, out| {
        for flag in [IF_BERRY, IF_GEM, IF_CHOICE] {
            out.push((item.flags & flag != 0) as u8 as f32);
        }
        out.push(!item.mega.is_empty() as u8 as f32);
        out.push(item.boosts.is_some() as u8 as f32);
        out.push(item.fling.power as f32 / 130.0);
    })
}

/// How many ids there are of each kind, the two reserved ones included:
/// species, items, abilities, moves.
pub fn vocab() -> [usize; 4] {
    let base = ID_BASE as usize;
    [SPECIES.len() + base, ITEMS.len() + base, ABILITIES.len() + base, MOVES.len() + base]
}
