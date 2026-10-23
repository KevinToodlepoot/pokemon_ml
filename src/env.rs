//! Games for a model to play: Team Preview and the battle as one run of
//! decisions, actions as numbers, and many games stepped at once.
//!
//! A [`Game`] starts at Team Preview, where each side picks which four of its
//! six Pokémon to bring and which two lead, and goes on as a [`Battle`]. At
//! every decision both sides give an action and get an observation
//! ([`crate::obs`]). [`VecEnv`] holds many games, plays teams from a
//! [`Pool`] against each other, starts a new game when one ends and fills
//! the observations of all of them in parallel.
//!
//! # Actions
//!
//! At Team Preview an action is one number below [`N_PREVIEW`] (90): which
//! pair leads, and which pair of the other four waits behind
//! ([`preview_table`]).
//!
//! In battle a side gives two numbers below [`N_ACTIONS`] (47), one for each
//! of its positions on the field:
//!
//! | action | meaning |
//! |---|---|
//! | `10 * move + 2 * target + mega` | use move 0 to 3. Target 0: the move takes none; 1, 2: the opponent's first and second position; 3, 4: one's own first and second. `mega` 1: Mega Evolve first. |
//! | `40 + j` | switch to the Pokémon registered `j`th, 0 to 5 |
//! | `46` | pass: nothing to do in this position |
//!
//! A switch names the Pokémon and not where it stands on the bench, so that
//! it means the same thing all game, and the same thing as the Pokémon's
//! token in the observation. What is legal comes with each observation as a
//! mask: `N_ACTIONS` bytes for the first position, then for each of its
//! actions `N_ACTIONS` bytes for the second position given the first (the
//! two cannot switch to the same Pokémon, or both Mega Evolve).

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::battle::{Error, PokemonSet, calc_stats, struggle_id};
use crate::data::*;
use crate::format::ShowdownSet;
use crate::obs::{OBS_F, OBS_I, Phase, ROSTER, TimerCounts, Timers, observe_battle, observe_preview};
use crate::position::BattleState;
use crate::rng::Rng;
use crate::speed::{self, Speeds};
use crate::state::*;
use crate::teams::{Pool, Sampler, Variation};

/// Actions for one position on the field.
pub const N_ACTIONS: usize = 47;
/// Ways to pick two leads and two more from six.
pub const N_PREVIEW: usize = 90;
/// Bytes of legal-action mask per observation.
pub const OBS_M: usize = N_ACTIONS + N_ACTIONS * N_ACTIONS;

const SWITCH: usize = 40;
const PASS: usize = 46;
const TARGETS: [i8; 5] = [0, 1, 2, -1, -2];

/// The four roster entries each Team Preview action brings: the two leads, then the two behind.
pub fn preview_table() -> &'static [[u8; 4]; N_PREVIEW] {
    static TABLE: std::sync::OnceLock<[[u8; 4]; N_PREVIEW]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut out = [[0u8; 4]; N_PREVIEW];
        let mut n = 0;
        for a in 0..6u8 {
            for b in a + 1..6 {
                let rest: Vec<u8> = (0..6).filter(|&x| x != a && x != b).collect();
                for (k, &c) in rest.iter().enumerate() {
                    for &d in &rest[k + 1..] {
                        out[n] = [a, b, c, d];
                        n += 1;
                    }
                }
            }
        }
        debug_assert_eq!(n, N_PREVIEW);
        out
    })
}

/// A player that needs no model, to measure one against.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Baseline {
    /// Any legal action, each as likely as the next.
    Random,
    /// The strongest attack it has for the targets in front of it, going by
    /// power, type and accuracy alone. It switches only when it must, and
    /// brings a random four.
    Greedy,
    /// Tries its choices out in the simulator, one turn ahead, and keeps the
    /// one that leaves it best off: Pokémon and HP left, its own against the
    /// other side's. It plays each against what the greedy player would
    /// answer (twice) and against a random answer. It sees everything: the
    /// other side's stats, items and moves are the real ones, which no
    /// player has. Only the dice are not: each try rolls its own. A bar to
    /// clear, then, and no model of fair play. Replacements and Team Preview
    /// it picks at random.
    Lookahead,
}

/// How far the effects with a hidden random length have run, as both players
/// can count them from the log (D97): by side and team index, the runs of the
/// effect its current instance has made, the turn it began included. A run is
/// a `cant` line of a Pokémon asleep (two with Early Bird at work), a move
/// attempt while confused (`-activate`), and a turn's end with a rampage's
/// lock or a binding move on it. Each count is the length the effect rolled
/// less what is left of it, so every length the rest of the game can have is
/// one the count allows; a confusion or a binding Baton Pass hands on keeps
/// its count, as Showdown copies what is left of it. Part of a game's state,
/// next to its [`Battle`], for whoever has to draw the hidden length afresh (a
/// search from a position one side knows).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Counts {
    pub sleep: [[u8; MAX_TEAM]; 2],
    pub confusion: [[u8; MAX_TEAM]; 2],
    pub rampage: [[u8; MAX_TEAM]; 2],
    pub trap: [[u8; MAX_TEAM]; 2],
    /// Its sleep is Rest's, which the log says (`-status|X|slp|[from] move: Rest`): three turns, no
    /// draw (Showdown's Rest sets `time = startTime = 3`), whatever it has done since.
    pub rest: [[bool; MAX_TEAM]; 2],
    /// The sleep counts the other side is shown, by side and the team index of the Pokémon each name
    /// is (D96), where a disguise (Illusion) makes them other than `sleep`: plus one, and 0 where it is
    /// the Pokémon's own ([`Counts::sleep_told`]). A Pokémon in disguise sleeps under the name it passes
    /// for; once the disguise is told (`replace`), the runs it made under that name this stay are its
    /// own, and the name's count goes back to what it was before them.
    #[serde(skip_serializing_if = "is_default")]
    pub told_sleep: [[u8; MAX_TEAM]; 2],
    /// By side and team index: what a Pokémon in disguise has slept under the name it passes for this
    /// stay (D96).
    #[serde(skip_serializing_if = "is_default")]
    pub borrowed: [[Borrowed; MAX_TEAM]; 2],
}

/// What a Pokémon in disguise has slept under the name it passes for, this stay ([`Counts::borrowed`]):
/// the name (by team index), the runs, the name's count before them, and whether the sleep began this
/// stay.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Borrowed {
    pub record: Option<u8>,
    pub runs: u8,
    pub before: u8,
    pub began: bool,
}

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

/// The effects [`Counts`] counts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Counted {
    Sleep,
    Confusion,
    Rampage,
    Trap,
}

impl Counted {
    const ALL: [Counted; 4] = [Counted::Sleep, Counted::Confusion, Counted::Rampage, Counted::Trap];

    /// The volatile condition it is (sleep is a status).
    pub(crate) fn volatile(self) -> Option<VolKind> {
        match self {
            Counted::Sleep => None,
            Counted::Confusion => Some(VolKind::Confusion),
            Counted::Rampage => Some(VolKind::Lockedmove),
            Counted::Trap => Some(VolKind::Partiallytrapped),
        }
    }

    /// The instance `r` holds and what is left of it: the sleep's turns, the confusion's attempts,
    /// the rampage's true turns and the binding's turns. Volatiles only on the field.
    fn instance(self, b: &Battle, r: MonRef) -> Option<(u16, i16)> {
        let m = b.mon(r);
        match self.volatile() {
            None => (m.status == Status::Slp).then_some((m.status_st.uid, m.status_time as i16)),
            Some(kind) => {
                if !m.is_active || m.fainted {
                    return None;
                }
                b.vols(r).get(kind).map(|c| {
                    let left = match self {
                        Counted::Confusion => c.data as i16,
                        Counted::Rampage => c.st.a,
                        _ => c.duration as i16,
                    };
                    (c.st.uid, left)
                })
            }
        }
    }

    fn table(self, counts: &mut Counts) -> &mut [[u8; MAX_TEAM]; 2] {
        match self {
            Counted::Sleep => &mut counts.sleep,
            Counted::Confusion => &mut counts.confusion,
            Counted::Rampage => &mut counts.rampage,
            Counted::Trap => &mut counts.trap,
        }
    }

    fn count(self, counts: &Counts, r: MonRef) -> u8 {
        let table = match self {
            Counted::Sleep => &counts.sleep,
            Counted::Confusion => &counts.confusion,
            Counted::Rampage => &counts.rampage,
            Counted::Trap => &counts.trap,
        };
        table[r.side as usize][r.idx as usize]
    }
}

impl Counts {
    /// The sleep count the other side is shown for `side`'s Pokémon of team index `a`, by its name (D96).
    pub(crate) fn sleep_told(&self, side: usize, a: usize) -> u8 {
        match self.told_sleep[side][a] {
            0 => self.sleep[side][a],
            n => n - 1,
        }
    }

    /// Advances the counts from the battle as it was at the last decision to the battle now, with what
    /// the step between reported ([`speed::Event::Began`], [`speed::Event::Passed`]): every instance
    /// alive now has a length it began with, from the last decision (its count and what was left), from
    /// the roll that began it in the step, or from the Pokémon that passed it on, and its count is that
    /// length less what is left.
    fn update(&mut self, before: &Battle, after: &Battle, events: &[speed::Event]) {
        // (uid, the length it began with, whether it is Rest's sleep)
        let mut began: Vec<(u16, i16, bool)> = Vec::new();
        // The length an instance alive at the last decision began with: its count and what was left.
        let start_before = |counts: &Counts, kind: Counted, r: MonRef, uid: u16| -> Option<i16> {
            let (was, left) = kind.instance(before, r)?;
            (was == uid).then(|| kind.count(counts, r) as i16 + left)
        };
        for event in events {
            match *event {
                speed::Event::Began { uid, length, rest } => {
                    began.retain(|e| e.0 != uid);
                    began.push((uid, length as i16, rest));
                }
                speed::Event::Passed { from, kind, from_uid, to_uid, .. } => {
                    let start = began
                        .iter()
                        .find(|e| e.0 == from_uid)
                        .map(|e| e.1)
                        .or_else(|| start_before(self, kind, from, from_uid));
                    if let Some(start) = start {
                        began.retain(|e| e.0 != to_uid);
                        began.push((to_uid, start, false));
                    }
                }
                _ => {}
            }
        }
        let mut next = *self;
        // By side and team index: a sleep began in the step.
        let mut fresh = [[false; MAX_TEAM]; 2];
        for side in 0..2 {
            for idx in 0..MAX_TEAM {
                let r = MonRef { side: side as u8, idx: idx as u8 };
                for kind in Counted::ALL {
                    let now = kind.instance(after, r);
                    let new = now.and_then(|(uid, _)| began.iter().find(|e| e.0 == uid));
                    let n = match now {
                        None => 0,
                        Some((uid, left)) => {
                            let start = start_before(self, kind, r, uid).or_else(|| new.map(|e| e.1));
                            start.map_or(0, |start| (start - left).clamp(0, 255) as u8)
                        }
                    };
                    kind.table(&mut next)[side][idx] = n;
                    if kind == Counted::Sleep {
                        // (Rest's mark goes with the sleep it began, and with nothing else.)
                        let same = now.is_some_and(|(uid, _)| start_before(self, kind, r, uid).is_some());
                        fresh[side][idx] = now.is_some() && !same;
                        next.rest[side][idx] = match (now, new) {
                            (None, _) => false,
                            (Some(_), Some(e)) if !same => e.2,
                            _ => same && self.rest[side][idx],
                        };
                    }
                }
            }
        }
        // The sleep counts by name (D96): each Pokémon's runs in the step go to the name it goes by now, or
        // went by at the last decision if it has left the field since.
        let mut told = [[0u8; MAX_TEAM]; 2];
        for (side, row) in told.iter_mut().enumerate() {
            for (a, n) in row.iter_mut().enumerate() {
                *n = self.sleep_told(side, a);
            }
        }
        for side in 0..2 {
            for idx in 0..(after.sides[side].n as usize).min(MAX_TEAM) {
                let r = MonRef { side: side as u8, idx: idx as u8 };
                let on = |b: &Battle| b.mon(r).is_active && !b.mon(r).fainted;
                let (was, now) = (on(before).then(|| before.shown_as(r)), on(after).then(|| after.shown_as(r)));
                let borrowed = &mut next.borrowed[side][idx];
                if was.is_none() && now.is_some() {
                    // (A new stay.)
                    *borrowed = Borrowed::default();
                }
                // The disguise is told: it stood under another name at the last decision, and as itself now.
                if let (Some(w), Some(n)) = (was, now)
                    && w != idx
                    && n == idx
                {
                    if borrowed.record == Some(w as u8) {
                        told[side][w] = borrowed.before;
                        told[side][idx] =
                            if borrowed.began { borrowed.runs } else { told[side][idx].saturating_add(borrowed.runs) };
                    }
                    *borrowed = Borrowed::default();
                }
                let j = now.or(was).unwrap_or(idx);
                let (c0, c1) = (self.sleep[side][idx], next.sleep[side][idx]);
                let runs = if fresh[side][idx] { c1 } else { c1.saturating_sub(c0) };
                if after.mon(r).status != Status::Slp {
                    if before.mon(r).status == Status::Slp && after.mon(r).fainted && j != idx {
                        // Fainted in disguise, asleep: nothing woke under the name. Where the faint told the
                        // disguise (`replace` before `faint`: the name's record is not the one shown fainted),
                        // the runs it made under the borrowed name are its own and the name's count goes back
                        // to what it was before them; where it fell under the name without a word (a residual
                        // faint, Showdown's `onFaint` clearing the Illusion silently), they stay with the name,
                        // as the log showed them.
                        if borrowed.record == Some(j as u8) && !after.sides[side].shown[j].fainted {
                            told[side][j] = borrowed.before;
                        }
                        borrowed.runs = 0;
                    } else if before.mon(r).status == Status::Slp {
                        // Awake: the name it woke under shows no sleep.
                        told[side][j] = 0;
                        borrowed.runs = 0;
                    }
                } else if j != idx {
                    if borrowed.record != Some(j as u8) {
                        *borrowed = Borrowed { record: Some(j as u8), runs: 0, before: told[side][j], began: false };
                    }
                    if fresh[side][idx] {
                        (borrowed.runs, borrowed.began, told[side][j]) = (0, true, 0);
                    }
                    borrowed.runs = borrowed.runs.saturating_add(runs);
                    told[side][j] = told[side][j].saturating_add(runs);
                } else {
                    told[side][idx] = if fresh[side][idx] { runs } else { told[side][idx].saturating_add(runs) };
                }
            }
        }
        for side in 0..2 {
            for a in 0..MAX_TEAM {
                next.told_sleep[side][a] =
                    if told[side][a] == next.sleep[side][a] { 0 } else { told[side][a].saturating_add(1) };
            }
        }
        *self = next;
    }
}

/// What the player whose position an export is cannot tell (D89, D94), as its follower writes it
/// beside the battle ([`GameState::untold`]): each entry names a Pokémon by side and team index, as
/// the battle numbers them, and says what the game may have instead of what the battle shows. A
/// search from the position draws one reading in every world (`vgcbot.belief`). A game keeps the
/// list it was given and writes it back out; nothing in the engine reads it, and a step clears it
/// (what was drawn is then the game's own).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Untold {
    /// A rampage that may have run out without a word on its second turn (the confusion its end
    /// brings is taken silently: the holder is already confused, has Own Tempo, stands grounded in
    /// Misty Terrain). The battle has it locked for one turn more; it may be over.
    Rampage { side: u8, team_index: u8 },
    /// PP a move paid to Pressure behind a blanked target (`|move|X|Sucker Punch||[still]`), with a
    /// Pressure Pokémon and one without where it could have been aimed: `payments` times one PP
    /// or two. The battle counts one each, and has `pp` left.
    PressurePp { side: u8, team_index: u8, move_id: String, payments: u8, pp: u8 },
    /// Where a move of the player's own went after Champions' Encore put it in again with no
    /// target, with two of the other side standing: one of `locs`. Either the Pokémon Showdown drew
    /// fainted first (the battle has the place the log named), or the move failed with no
    /// animation, which takes the target off its line (the battle has none).
    EncoreTarget { side: u8, team_index: u8, locs: Vec<i8> },
    /// Stats the other side's Pokémon split or swapped with its partner (Power Split, Guard Split,
    /// Speed Swap) this stay: they come from two spreads the player does not know, and a position
    /// that holds them is refused (as D77 refuses one split with the player's own).
    StatsShared { side: u8, team_index: u8 },
    /// Where the player's own charging move is aimed when another move called it (Copycat) with two of
    /// the other side standing: Showdown drew one (`getRandomTarget`) and the charging turn's line names
    /// nobody. One of `locs`; the battle has the first.
    ChargeTarget { side: u8, team_index: u8, locs: Vec<i8> },
    /// Whether a status move of the other side's worked, its line naming no target and failing with no
    /// word of why (`|-fail|X`), with a Substitute up on some of the Pokémon it may have been aimed at and
    /// not all: one took it (it worked) or it failed at another. This turn's result (`move_this_turn`) or
    /// last turn's (`move_last_turn`); the battle has it failed.
    MoveResult { side: u8, team_index: u8, this_turn: bool },
    /// Which of two Pokémon that used up the same item this turn Pickup took it from (Showdown samples
    /// one, and the log names neither): that one's last item is gone. The battle has both keep it.
    ItemTaken { side: u8, team_index: u8, other_side: u8, other_index: u8 },
}

/// An exact HP the player whose export this is worked out for one of the other side's Pokémon (a
/// Pain Split or an Endeavor with one of its own, D77), with the percentage and bar (`green`,
/// `yellow`, `red`) it showed then, which only some max HPs can show: what belief draws its HP
/// stat points from; `now` if it still has that HP. Kept and cleared as [`Untold`] is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HpSeen {
    pub side: u8,
    pub team_index: u8,
    pub hp: u16,
    pub pct: u8,
    pub bar: String,
    pub now: bool,
}

/// A [`Game`] written down: the position with everything a game keeps next to
/// it. The two teams as registered (six a side, with their stat points, which
/// a [`BattleState`] has only for the Pokémon brought), whether the sheets are
/// open, the battle if Team Preview is over, and the bookkeeping the
/// observation reads: how long the field conditions have been up
/// ([`TimerCounts`]), what each side has been shown of the other's Speed
/// ([`Speeds`]) and the public counts of the hidden-length effects
/// ([`Counts`]). The last three may be left out of a position written by
/// hand, and are then taken as if nothing had been seen before this turn.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GameState {
    pub rosters: [Vec<ShowdownSet>; 2],
    pub open: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battle: Option<BattleState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timers: Option<TimerCounts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speeds: Option<Speeds>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counts: Option<Counts>,
    /// In an export, what its player cannot tell ([`Untold`]).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub untold: Vec<Untold>,
    /// In an export, the other side's exact HP its player has worked out ([`HpSeen`]).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hp_seen: Vec<HpSeen>,
}

impl GameState {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("a game state is always serialisable")
    }
    pub fn from_json(text: &str) -> Result<GameState, Error> {
        serde_json::from_str(text).map_err(|e| Error::BadState(format!("unreadable game state: {e}")))
    }
}

/// One game: Team Preview, then the battle.
#[derive(Clone, Debug)]
pub struct Game {
    rosters: [Vec<PokemonSet>; 2],
    /// Each registered Pokémon's stats.
    stats: [[[u16; 6]; ROSTER]; 2],
    open: bool,
    battle: Option<Battle>,
    timers: Timers,
    speeds: Speeds,
    counts: Counts,
    /// What the player whose export this was cannot tell, as given ([`Untold`]), and the HP it worked
    /// out ([`HpSeen`]); cleared by a step.
    untold: Vec<Untold>,
    hp_seen: Vec<HpSeen>,
    /// What the engine noted during the last step, for `speeds` (kept to reuse the space).
    events: Vec<speed::Event>,
}

impl Game {
    /// A game between two teams of six, at Team Preview. `open`: the players have each other's team sheets.
    pub fn new(rosters: [Vec<PokemonSet>; 2], open: bool) -> Result<Game, Error> {
        let mut stats = [[[0u16; 6]; ROSTER]; 2];
        for side in 0..2 {
            if rosters[side].len() != ROSTER {
                return Err(Error::BadTeam(format!(
                    "side {} registers {} Pokémon; a game is played with {ROSTER}",
                    side + 1,
                    rosters[side].len()
                )));
            }
            for (j, set) in rosters[side].iter().enumerate() {
                stats[side][j] = calc_stats(set.species, set.nature, set.stat_points);
            }
        }
        let speeds = Speeds::new([&rosters[0], &rosters[1]], open);
        Ok(Game {
            rosters,
            stats,
            open,
            battle: None,
            timers: Timers::default(),
            speeds,
            counts: Counts::default(),
            untold: Vec::new(),
            hp_seen: Vec::new(),
            events: Vec::new(),
        })
    }

    /// The game written down, battle and bookkeeping included. See [`GameState`].
    pub fn to_state(&self) -> GameState {
        let rosters = [
            self.rosters[0].iter().map(ShowdownSet::from_set).collect(),
            self.rosters[1].iter().map(ShowdownSet::from_set).collect(),
        ];
        match &self.battle {
            None => GameState { rosters, open: self.open, ..GameState::default() },
            Some(b) => GameState {
                rosters,
                open: self.open,
                battle: Some(b.to_state()),
                timers: Some(self.timers.counts(b.turn)),
                speeds: Some(self.speeds),
                counts: Some(self.counts),
                untold: self.untold.clone(),
                hp_seen: self.hp_seen.clone(),
            },
        }
    }

    /// A game taken up from a [`GameState`]: at Team Preview if it has no
    /// battle (with the Speed beliefs it carries), else at the battle's
    /// request. Bookkeeping left out is taken as
    /// if nothing had been seen before: the field conditions as just put up,
    /// each side's Speed as the sheets allow, the hidden-length effects as
    /// just begun.
    pub fn from_state(st: &GameState) -> Result<Game, Error> {
        let sets = |team: &[ShowdownSet]| team.iter().map(|s| s.to_set()).collect::<Result<Vec<_>, _>>();
        let mut g = Game::new([sets(&st.rosters[0])?, sets(&st.rosters[1])?], st.open)?;
        // The Speed each side has been shown of the other's, carried even by a state without a
        // battle (a probe of what the beliefs allow, D72).
        if let Some(sp) = &st.speeds {
            g.speeds = *sp;
            g.speeds.strict = false;
        }
        let Some(bs) = &st.battle else {
            return Ok(g);
        };
        if bs.open_team_sheets != st.open {
            return Err(Error::BadState("the battle and the game disagree on whether the sheets are open".into()));
        }
        let b = Battle::from_state(bs)?;
        g.timers = match &st.timers {
            Some(c) => {
                let mut t = Timers::counted(c.weather, c.terrain, c.screens, b.turn);
                t.adopt(&b);
                t
            }
            None => {
                let mut t = Timers::default();
                t.update(&b);
                t
            }
        };
        g.speeds.resume(&b);
        g.counts = st.counts.unwrap_or_default();
        g.untold.clone_from(&st.untold);
        g.hp_seen.clone_from(&st.hp_seen);
        g.battle = Some(b);
        Ok(g)
    }

    pub fn timers(&self) -> &Timers {
        &self.timers
    }

    pub fn counts(&self) -> &Counts {
        &self.counts
    }

    pub fn phase(&self) -> Phase {
        match &self.battle {
            None => Phase::Preview,
            Some(b) if b.request == Request::Switch => Phase::Switch,
            Some(_) => Phase::Move,
        }
    }

    /// What each side can tell of the other's Speed.
    pub fn speeds(&self) -> &Speeds {
        &self.speeds
    }

    /// The Speed stat points the Pokémon `side` registered `entry`th can still have, as its
    /// opponent can tell, with the nature it was registered with and whatever item it holds now.
    pub fn speed_points(&self, side: usize, entry: usize) -> Vec<u8> {
        self.speeds.belief(side, entry).points(self.rosters[side][entry].nature).collect()
    }

    /// Has [`Speeds`] check itself against the truth as the game goes, and panic on a difference: for tests.
    pub fn check_speeds(&mut self) {
        self.speeds.strict = true;
    }

    /// The battle, once Team Preview is over.
    pub fn battle(&self) -> Option<&Battle> {
        self.battle.as_ref()
    }

    pub fn rosters(&self) -> [&[PokemonSet]; 2] {
        [&self.rosters[0], &self.rosters[1]]
    }

    pub fn open_team_sheets(&self) -> bool {
        self.open
    }

    pub fn ended(&self) -> bool {
        self.battle.as_ref().is_some_and(|b| b.ended)
    }

    /// The side that won, once the game has ended; `None` for a tie.
    pub fn winner(&self) -> Option<usize> {
        self.battle.as_ref().and_then(|b| b.winner).map(|w| w as usize)
    }

    /// The number a choice goes by.
    pub fn action_of(&self, side: usize, choice: Choice) -> Option<usize> {
        action_of(self.battle.as_ref()?, side, choice)
    }

    /// The choice a number stands for, whether or not it is legal now.
    pub fn choice_of(&self, side: usize, action: usize) -> Option<Choice> {
        choice_of(self.battle.as_ref()?, side, action)
    }

    /// Every pair of actions `side` may give now.
    pub fn legal(&self, side: usize) -> Vec<[usize; 2]> {
        let Some(b) = &self.battle else {
            return (0..N_PREVIEW).map(|a| [a, 0]).collect();
        };
        let act = |c: Choice| self.action_of(side, c).expect("a legal choice has a number");
        b.joint_choices(side).into_iter().map(|c| [act(c[0]), act(c[1])]).collect()
    }

    /// Fills `mask` ([`OBS_M`] bytes) with what `side` may do and returns how
    /// many pairs of actions that is. At Team Preview the mask stays empty:
    /// every one of the [`N_PREVIEW`] picks is allowed.
    pub fn masks(&self, side: usize, mask: &mut [u8]) -> usize {
        assert_eq!(mask.len(), OBS_M, "a mask buffer of the wrong size");
        mask.fill(0);
        let Some(b) = &self.battle else {
            return N_PREVIEW;
        };
        masks_of(b, side, mask)
    }

    /// Team Preview ends: each side brings the Pokémon it names, as places in
    /// its registered team, in the order it wants them (the first two lead).
    /// `seed` seeds the battle that starts. ([`Game::act`] does this from a
    /// Team Preview action, which names the leads in the order registered.)
    pub fn start(&mut self, picks: [&[usize]; 2], seed: [u16; 4]) -> Result<(), Error> {
        let rosters = [&self.rosters[0][..], &self.rosters[1][..]];
        let b = speed::record(&mut self.events, || Battle::with_rosters(rosters, picks, self.open, seed))?;
        self.timers = Timers::default();
        self.timers.update(&b);
        self.speeds.digest(&self.events, &b);
        self.counts = Counts::default();
        self.untold.clear();
        self.hp_seen.clear();
        self.battle = Some(b);
        Ok(())
    }

    /// Both sides choose, in the battle's own terms. ([`Game::act`] does this from actions.)
    pub fn choose(&mut self, choices: [[Choice; ACTIVE]; 2]) -> Result<(), Error> {
        let b = self.battle.as_mut().ok_or_else(|| Error::BadChoice("the battle has not started".into()))?;
        let before = *b;
        speed::record(&mut self.events, || b.choose(choices))?;
        self.timers.update(b);
        self.speeds.digest(&self.events, b);
        self.counts.update(&before, b, &self.events);
        self.untold.clear();
        self.hp_seen.clear();
        Ok(())
    }

    /// Both sides act. At Team Preview only the first number of each side
    /// counts, and `seed` seeds the battle that starts.
    pub fn act(&mut self, actions: [[usize; 2]; 2], seed: [u16; 4]) -> Result<(), Error> {
        if self.battle.is_none() {
            let mut picks = [[0usize; 4]; 2];
            for side in 0..2 {
                let pick = preview_table().get(actions[side][0]).ok_or_else(|| {
                    Error::BadChoice(format!("p{}: {} is not a Team Preview action", side + 1, actions[side][0]))
                })?;
                picks[side] = pick.map(|j| j as usize);
            }
            return self.start([&picks[0], &picks[1]], seed);
        }
        let mut choices = [[Choice::Pass; ACTIVE]; 2];
        for side in 0..2 {
            for pos in 0..ACTIVE {
                choices[side][pos] = self.choice_of(side, actions[side][pos]).ok_or_else(|| {
                    Error::BadChoice(format!("p{}: {} is not an action", side + 1, actions[side][pos]))
                })?;
            }
        }
        self.choose(choices)
    }

    /// Gives the battle a fresh random number generator from four seed words,
    /// for playing the same position out under different dice (a search cell);
    /// `false` at Team Preview, where [`Game::act`] seeds the battle itself.
    pub fn reseed(&mut self, words: [u16; 4]) -> bool {
        match &mut self.battle {
            Some(b) => {
                b.rng = Rng::from_words(words);
                true
            }
            None => false,
        }
    }

    /// Whether the turn has been played to its end: at a move request, at a
    /// request for replacing the fainted at the end of the turn, or once the
    /// battle is over. `false` at a switch request in the middle of a turn
    /// (U-turn, Eject Button), where moves are still to run.
    pub fn turn_over(&self) -> bool {
        self.battle.as_ref().is_none_or(|b| b.turn_over())
    }

    /// `view`'s side of the game, into buffers of [`OBS_F`], [`OBS_I`] and [`OBS_M`].
    pub fn observe(&self, view: usize, f: &mut [f32], i: &mut [i16], mask: &mut [u8]) {
        match &self.battle {
            None => observe_preview(self.rosters(), &self.speeds, &self.stats[view], self.open, view, f, i),
            Some(b) => observe_battle(b, &self.timers, &self.speeds, &self.counts, &self.stats[view], view, f, i),
        }
        let n = self.masks(view, mask);
        let info = &mut i[OBS_I - crate::obs::INFO..];
        info[1] = n.min(i16::MAX as usize) as i16;
        info[3] = (n > 1) as i16;
    }

    /// What a [`Baseline`] player does on `side`.
    pub fn baseline(&self, side: usize, kind: Baseline, rng: &mut Rng) -> [usize; 2] {
        let Some(b) = &self.battle else {
            return [rng.below(N_PREVIEW as u32) as usize, 0];
        };
        if kind == Baseline::Random || b.request != Request::Move {
            let legal = self.legal(side);
            return legal[rng.below(legal.len() as u32) as usize];
        }
        let act = |c: Choice| self.action_of(side, c).expect("a legal choice has a number");
        let best = if kind == Baseline::Lookahead { lookahead(b, side, rng) } else { greedy(b, side, rng) };
        [act(best[0]), act(best[1])]
    }
}

/// The roster entry of the Pokémon at a position of a side's current order.
fn entry_at(b: &Battle, side: usize, position: usize) -> Option<usize> {
    let s = &b.sides[side];
    let idx = *s.order.get(position)?;
    s.roster[..s.n_roster as usize].iter().position(|l| l.brought == idx)
}

/// The number a choice goes by in a battle.
pub(crate) fn action_of(b: &Battle, side: usize, choice: Choice) -> Option<usize> {
    Some(match choice {
        Choice::Pass => PASS,
        Choice::Move { slot, target, mega } => {
            let t = TARGETS.iter().position(|&x| x == target)?;
            10 * slot as usize + 2 * t + mega as usize
        }
        Choice::Switch { to } => SWITCH + entry_at(b, side, to as usize)?,
    })
}

/// The choice a number stands for in a battle, whether or not it is legal now.
pub(crate) fn choice_of(b: &Battle, side: usize, action: usize) -> Option<Choice> {
    Some(match action {
        PASS => Choice::Pass,
        a if a < SWITCH => Choice::Move { slot: (a / 10) as u8, target: TARGETS[a % 10 / 2], mega: a % 2 == 1 },
        a if a < PASS => {
            let s = &b.sides[side];
            let brought = s.roster.get(a - SWITCH)?.brought;
            if brought == NOT_LISTED {
                return None;
            }
            Choice::Switch { to: s.team[brought as usize].position }
        }
        _ => return None,
    })
}

/// Fills `mask` with what `side` may do in a battle and returns how many pairs of actions that is.
pub(crate) fn masks_of(b: &Battle, side: usize, mask: &mut [u8]) -> usize {
    assert_eq!(mask.len(), OBS_M, "a mask buffer of the wrong size");
    mask.fill(0);
    if b.ended {
        return 0;
    }
    let act = |c: Choice| action_of(b, side, c).expect("a legal choice has a number");
    let first: Vec<(Choice, usize)> = b.legal_choices(side, 0).into_iter().map(|c| (c, act(c))).collect();
    let second: Vec<(Choice, usize)> = b.legal_choices(side, 1).into_iter().map(|c| (c, act(c))).collect();
    let mut n = 0;
    for &(x, ax) in &first {
        for &(y, ay) in &second {
            if b.pair_ok(side, &[x, y]) {
                mask[ax] = 1;
                mask[N_ACTIONS * (1 + ax) + ay] = 1;
                n += 1;
            }
        }
    }
    n
}

/// What the greedy player does on a turn.
fn greedy(b: &Battle, side: usize, rng: &mut Rng) -> [Choice; ACTIVE] {
    let mut best = [Choice::Pass; ACTIVE];
    for pos in 0..ACTIVE {
        let mut top = f32::NEG_INFINITY;
        for c in b.legal_choices(side, pos) {
            let mut trial = best;
            trial[pos] = c;
            if pos == 1 && !b.pair_ok(side, &trial) {
                continue;
            }
            // A coin's worth of noise settles ties.
            let score = greedy_score(b, side, pos, c) + rng.below(1000) as f32 * 1e-4;
            if score > top {
                (top, best[pos]) = (score, c);
            }
        }
    }
    best
}

/// How well off `side` is: a point for each Pokémon it has left and up to another for that
/// one's HP, less the same for the other side. A battle that is over outweighs any of that.
fn standing(b: &Battle, side: usize) -> f32 {
    if b.ended {
        return match b.winner {
            Some(w) if w as usize == side => 100.0,
            Some(_) => -100.0,
            None => 0.0,
        };
    }
    let worth = |s: usize| -> f32 {
        let team = &b.sides[s].team[..b.sides[s].n as usize];
        team.iter().filter(|m| !m.fainted).map(|m| 1.0 + m.hp as f32 / m.max_hp().max(1) as f32).sum()
    };
    worth(side) - worth(1 - side)
}

/// What the look-ahead player does on a turn.
fn lookahead(b: &Battle, side: usize, rng: &mut Rng) -> [Choice; ACTIVE] {
    let foe = 1 - side;
    let anything = b.joint_choices(foe);
    let replies = [greedy(b, foe, rng), greedy(b, foe, rng), anything[rng.below(anything.len() as u32) as usize]];
    // How things stand after a turn of `mine` against each reply, with dice of the try's own.
    let value = |mine: [Choice; ACTIVE], rng: &mut Rng| -> f32 {
        let mut total = 0.0;
        for reply in replies {
            let mut after = *b;
            after.rng = Rng::from_words(seed_of(rng));
            let mut choices = [[Choice::Pass; ACTIVE]; 2];
            (choices[side], choices[foe]) = (mine, reply);
            if after.choose(choices).is_err() {
                return f32::NEG_INFINITY;
            }
            total += standing(&after, side);
        }
        total
    };
    // Each position's choices tried beside the other's greedy pick, to find the four worth pairing up.
    let base = greedy(b, side, rng);
    let mut short: [Vec<(f32, Choice)>; ACTIVE] = [Vec::new(), Vec::new()];
    for (pos, list) in short.iter_mut().enumerate() {
        for c in b.legal_choices(side, pos) {
            let mut trial = base;
            trial[pos] = c;
            if b.pair_ok(side, &trial) {
                list.push((value(trial, rng), c));
            }
        }
        list.sort_by(|x, y| y.0.total_cmp(&x.0));
        list.truncate(4);
    }
    let (mut best, mut top) = (base, f32::NEG_INFINITY);
    for &(_, x) in &short[0] {
        for &(_, y) in &short[1] {
            if b.pair_ok(side, &[x, y]) {
                let v = value([x, y], rng);
                if v > top {
                    (top, best) = (v, [x, y]);
                }
            }
        }
    }
    best
}

/// How much the greedy player likes a choice: roughly the damage a move promises.
fn greedy_score(b: &Battle, side: usize, pos: usize, choice: Choice) -> f32 {
    let Choice::Move { slot, target, mega } = choice else {
        return if choice == Choice::Pass { 0.0 } else { 0.5 };
    };
    let r = b.active(side, pos);
    let m = b.mon(r);
    let id = if m.locked_move != NO_MOVE {
        m.locked_move
    } else if b.usable_moves(r) {
        m.moves[slot as usize].id
    } else {
        struggle_id()
    };
    let d = &MOVES[id as usize];
    if d.category == Category::Status {
        return 1.0;
    }
    let (types, n) = b.get_types(r, false);
    let stab = if types[..n].contains(&d.typ) { 1.5 } else { 1.0 };
    let power = if d.base_power == 0 { 60.0 } else { d.base_power as f32 };
    let accuracy = if d.accuracy == 0 { 1.0 } else { d.accuracy as f32 / 100.0 };
    // How well the move's type does against whoever stands at a position, going by the
    // types that Pokémon is seen to have. Nothing for an empty position.
    let against = |s: usize, p: usize| -> f32 {
        let t = b.active(s, p);
        let tm = b.mon(t);
        if p >= b.sides[s].n as usize || !tm.is_active || tm.fainted {
            return 0.0;
        }
        let (seen, n) = if s != side && tm.illusion != 0 {
            let sp = SPECIES[tm.live.species as usize].types;
            ([sp[0], sp[1], Type::None], 2)
        } else {
            b.get_types(t, false)
        };
        let mut mult = 1.0;
        for &def in &seen[..n] {
            if (d.typ as usize) < 18 && (def as usize) < 18 {
                mult *= [1.0, 2.0, 0.5, 0.0][TYPE_CHART[d.typ as usize][def as usize] as usize];
            }
        }
        mult
    };
    let foe = 1 - side;
    let reach = match (target, d.target) {
        (1 | 2, _) => against(foe, target as usize - 1),
        // Never at a partner on purpose.
        (-1 | -2, _) => 0.0,
        (_, Target::AllAdjacentFoes) => 0.75 * (against(foe, 0) + against(foe, 1)),
        (_, Target::AllAdjacent) => 0.75 * (against(foe, 0) + against(foe, 1) - against(side, 1 - pos)),
        _ => 0.5 * (against(foe, 0) + against(foe, 1)),
    };
    2.0 + power * stab * accuracy * reach * if mega { 1.05 } else { 1.0 }
}

/// How a [`VecEnv`] plays.
#[derive(Clone, Debug)]
pub struct Config {
    /// Games running at once.
    pub envs: usize,
    pub seed: u64,
    /// The share of games played with open team sheets.
    pub open_sheets: f64,
    /// How the pool's teams are varied from game to game.
    pub variation: Variation,
    /// A game still going after this many turns is stopped and scored as a tie.
    pub max_turns: u16,
    /// Worker threads; 0 for one per core.
    pub threads: usize,
    /// Whether the pool's teams are checked against the regulation (and a
    /// team that breaks it refused). Off, any team the engine can simulate is
    /// played as it is: for pools of deliberately illegal teams, made up to
    /// teach a model what the regulation's sets do not cover.
    pub check_teams: bool,
    /// The side of a split pool this plays (`Pool::side`): a pool that says it is another side is
    /// refused, so that teams held out for evaluation are never played by mistake. `None` plays
    /// whatever side the pool is; a pool that names no side (one never split) is always played.
    pub side: Option<String>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            envs: 256,
            seed: 1,
            open_sheets: 0.5,
            variation: Variation::default(),
            max_turns: 100,
            threads: 0,
            check_teams: true,
            side: Some("train".to_string()),
        }
    }
}

/// Totals since a [`VecEnv`] was made.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Stats {
    /// Games finished.
    pub games: u64,
    /// Decisions taken (Team Preview counts as one).
    pub decisions: u64,
    /// Turns played in the games that finished.
    pub turns: u64,
    /// Games that ended in a tie or were stopped at the turn limit.
    pub ties: u64,
    /// Games the first side won.
    pub first_wins: u64,
}

impl Stats {
    fn add(self, o: Stats) -> Stats {
        Stats {
            games: self.games + o.games,
            decisions: self.decisions + o.decisions,
            turns: self.turns + o.turns,
            ties: self.ties + o.ties,
            first_wins: self.first_wins + o.first_wins,
        }
    }
}

/// Many games at once. Observations are indexed game by game, the first
/// side's then the second's: observation `2 * game + side`.
pub struct VecEnv {
    games: Vec<Game>,
    rngs: Vec<Rng>,
    sampler: Sampler,
    config: Config,
    workers: rayon::ThreadPool,
    stats: Stats,
}

fn fresh(sampler: &Sampler, config: &Config, rng: &mut Rng) -> Game {
    let a = sampler.sample(rng, &config.variation).team;
    let b = sampler.sample(rng, &config.variation).team;
    let open = config.open_sheets > 0.0 && (rng.below(1_000_000) as f64) < config.open_sheets * 1_000_000.0;
    Game::new([a, b], open).expect("the pool's teams have six Pokémon")
}

fn seed_of(rng: &mut Rng) -> [u16; 4] {
    [rng.below(65536) as u16, rng.below(65536) as u16, rng.below(65536) as u16, rng.below(65536) as u16]
}

impl VecEnv {
    /// `config.envs` games between teams of `pool`, all at Team Preview.
    pub fn new(pool: &Pool, config: Config) -> Result<VecEnv, Error> {
        if let (Some(theirs), Some(ours)) = (&pool.side, &config.side)
            && theirs != ours
        {
            return Err(Error::BadTeam(format!(
                "the pool is the {theirs:?} side of a split, and this plays the {ours:?} side only \
                 (pass the side to play it)"
            )));
        }
        let sampler = Sampler::with_check(pool, config.check_teams)?;
        if let Some(short) = pool.teams.iter().find(|t| t.team.len() != ROSTER) {
            return Err(Error::BadTeam(format!(
                "{}'s team has {} Pokémon, not {ROSTER}",
                short.player,
                short.team.len()
            )));
        }
        let mut rngs: Vec<Rng> = (0..config.envs as u64)
            .map(|k| {
                // splitmix64, so that neighbouring seeds give unrelated games
                let mut z = config.seed.wrapping_add((k + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                Rng { seed: z ^ (z >> 31), calls: 0 }
            })
            .collect();
        let games = rngs.iter_mut().map(|rng| fresh(&sampler, &config, rng)).collect();
        let workers = rayon::ThreadPoolBuilder::new()
            .num_threads(config.threads)
            .build()
            .map_err(|e| Error::BadTeam(format!("cannot start worker threads: {e}")))?;
        Ok(VecEnv { games, rngs, sampler, config, workers, stats: Stats::default() })
    }

    pub fn len(&self) -> usize {
        self.games.len()
    }

    pub fn is_empty(&self) -> bool {
        self.games.is_empty()
    }

    pub fn games(&self) -> &[Game] {
        &self.games
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn threads(&self) -> usize {
        self.workers.current_num_threads()
    }

    fn check(&self, f: &[f32], i: &[i16], mask: &[u8]) {
        let n = 2 * self.games.len();
        assert!(
            f.len() == n * OBS_F && i.len() == n * OBS_I && mask.len() == n * OBS_M,
            "observation buffers must hold {n} observations"
        );
    }

    /// Every game from both sides, as it stands.
    pub fn observe(&self, f: &mut [f32], i: &mut [i16], mask: &mut [u8]) {
        self.check(f, i, mask);
        self.workers.install(|| {
            (
                self.games.par_iter(),
                f.par_chunks_mut(2 * OBS_F),
                i.par_chunks_mut(2 * OBS_I),
                mask.par_chunks_mut(2 * OBS_M),
            )
                .into_par_iter()
                .with_min_len(8)
                .for_each(|(game, f, i, mask)| observe_both(game, f, i, mask));
        });
    }

    /// Both sides of every game act (`actions`: four numbers a game, the
    /// first side's two then the second's), and the observations of what
    /// follows are filled in. A game that ends gives its result in `reward`
    /// (two numbers a game: 1 for the winner, -1 for the loser, 0 each for a
    /// tie) and sets `done`; a new game has then already taken its place,
    /// and the observation is that game's Team Preview.
    pub fn step(
        &mut self,
        actions: &[i32],
        f: &mut [f32],
        i: &mut [i16],
        mask: &mut [u8],
        reward: &mut [f32],
        done: &mut [u8],
    ) -> Result<(), Error> {
        self.check(f, i, mask);
        let n = self.games.len();
        assert!(actions.len() == 4 * n && reward.len() == 2 * n && done.len() == n, "one set of actions a game");
        let (sampler, config) = (&self.sampler, &self.config);
        let stepped = self.workers.install(|| {
            (
                self.games.par_iter_mut(),
                self.rngs.par_iter_mut(),
                actions.par_chunks(4),
                f.par_chunks_mut(2 * OBS_F),
                i.par_chunks_mut(2 * OBS_I),
                mask.par_chunks_mut(2 * OBS_M),
                reward.par_chunks_mut(2),
                done.par_iter_mut(),
            )
                .into_par_iter()
                .with_min_len(8)
                .map(|(game, rng, a, f, i, mask, reward, done)| -> Result<Stats, Error> {
                    let number = |x: i32| usize::try_from(x).unwrap_or(usize::MAX);
                    let seed = seed_of(rng);
                    game.act([[number(a[0]), number(a[1])], [number(a[2]), number(a[3])]], seed)?;
                    let mut stats = Stats { decisions: 1, ..Stats::default() };
                    reward.fill(0.0);
                    *done = 0;
                    let b = game.battle.as_ref().expect("a game that has been acted on has a battle");
                    if b.ended || b.turn > config.max_turns {
                        *done = 1;
                        stats.games = 1;
                        stats.turns = b.turn as u64;
                        match b.winner.filter(|_| b.ended) {
                            Some(w) => {
                                reward[w as usize] = 1.0;
                                reward[1 - w as usize] = -1.0;
                                stats.first_wins = (w == 0) as u64;
                            }
                            None => stats.ties = 1,
                        }
                        *game = fresh(sampler, config, rng);
                    }
                    observe_both(game, f, i, mask);
                    Ok(stats)
                })
                .try_reduce(Stats::default, |a, b| Ok(a.add(b)))
        })?;
        self.stats = self.stats.add(stepped);
        Ok(())
    }

    /// What a [`Baseline`] player would do on each side of every game (four numbers a game, as [`VecEnv::step`] takes them).
    pub fn baseline(&mut self, kind: Baseline, actions: &mut [i32]) {
        assert_eq!(actions.len(), 4 * self.games.len(), "one set of actions a game");
        self.workers.install(|| {
            (self.games.par_iter(), self.rngs.par_iter_mut(), actions.par_chunks_mut(4))
                .into_par_iter()
                .with_min_len(8)
                .for_each(|(game, rng, out)| {
                    for side in 0..2 {
                        let a = game.baseline(side, kind, rng);
                        out[2 * side] = a[0] as i32;
                        out[2 * side + 1] = a[1] as i32;
                    }
                });
        });
    }
}

fn observe_both(game: &Game, f: &mut [f32], i: &mut [i16], mask: &mut [u8]) {
    let (f0, f1) = f.split_at_mut(OBS_F);
    let (i0, i1) = i.split_at_mut(OBS_I);
    let (m0, m1) = mask.split_at_mut(OBS_M);
    game.observe(0, f0, i0, m0);
    game.observe(1, f1, i1, m1);
}
