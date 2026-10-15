//! Replays battles recorded from Pokémon Showdown by `oracle/gen_cases.js`
//! and reports the first place this engine disagrees. Used by the `difftest`
//! binary and by the fixture test in `tests/`.

use crate::data::{
    ABILITIES, Gender, ITEMS, MOVES, SPECIES, SideCond, SlotCond, Terrain, Type, VolKind, Weather, ab, it,
};
use crate::observer::Observer;
use crate::position::BattleState;
use crate::shown::{ItemShown, ShownMon, ShownSide};
use crate::state::{ACTIVE, Cond, NO_SPECIES, NOT_LISTED, Res, Trapped};
use crate::{Battle, Choice, Error, PokemonSet, Request, trace};
use serde::Deserialize;

#[derive(Deserialize, serde::Serialize, Clone, Debug)]
pub struct SetJson {
    pub species: String,
    pub moves: Vec<String>,
    pub nature: String,
    pub sp: [u8; 6],
    /// Ability id; absent means no ability.
    #[serde(default)]
    pub ability: Option<String>,
    /// Item id; absent or empty means no item.
    #[serde(default)]
    pub item: Option<String>,
    /// "M", "F" or "N"; absent means genderless, which is what the recorder
    /// used before genders were recorded.
    #[serde(default)]
    pub gender: Option<String>,
}

impl SetJson {
    pub fn to_set(&self) -> Result<PokemonSet, String> {
        let moves: Vec<&str> = self.moves.iter().map(String::as_str).collect();
        let mut set =
            PokemonSet::from_names(&self.species, &moves, &self.nature, self.sp).map_err(|e| e.to_string())?;
        if let Some(a) = &self.ability {
            set = set.ability(a).map_err(|e| e.to_string())?;
        }
        if let Some(i) = &self.item {
            set = set.item(i).map_err(|e| e.to_string())?;
        }
        let g = self.gender.as_deref().unwrap_or("N");
        set = set.gender(Gender::parse(g).ok_or_else(|| format!("unknown gender {g}"))?);
        Ok(set)
    }
}

#[derive(Deserialize, PartialEq, Debug, Clone)]
pub struct MonSnap {
    pub idx: u8,
    pub species: String,
    pub hp: u16,
    pub maxhp: u16,
    pub stats: [u16; 6],
    pub status: String,
    pub time: u8,
    pub stage: u8,
    pub boosts: [i8; 7],
    pub pp: Vec<u8>,
    pub active: bool,
    pub fainted: bool,
    pub switch_flag: bool,
    pub speed: i32,
    pub vol: Vec<String>,
    // Recorded since abilities and items were modelled; absent in older files.
    #[serde(default)]
    pub ability: Option<String>,
    #[serde(default)]
    pub item: Option<String>,
    #[serde(default)]
    pub last_item: Option<String>,
    #[serde(default)]
    pub used_item: Option<bool>,
    #[serde(default)]
    pub ate_berry: Option<bool>,
    #[serde(default)]
    pub types: Option<Vec<String>>,
    /// 0 free, 1 trapped, 2 trapped by something not yet revealed.
    #[serde(default)]
    pub trapped: Option<u8>,
    #[serde(default)]
    pub disabled: Option<Vec<bool>>,
    #[serde(default)]
    pub ability_order: Option<u32>,
    #[serde(default)]
    pub item_order: Option<u32>,
    #[serde(default)]
    pub active_turns: Option<u16>,
    /// `moveThisTurnResult` and `moveLastTurnResult`, one letter each:
    /// u(ndefined), n(ull), t(rue), f(alse).
    #[serde(default)]
    pub move_result: Option<String>,
    #[serde(default)]
    pub base_species: Option<String>,
    /// Id of the Mega the Pokémon can still become; empty if none.
    #[serde(default)]
    pub can_mega: Option<String>,
    /// Id of the move last used since coming in; empty if none.
    #[serde(default)]
    pub last_move: Option<String>,
    #[serde(default)]
    pub move_actions: Option<u8>,
    #[serde(default)]
    pub newly_switched: Option<bool>,
    /// `hurtThisTurn` (0 if unhurt), `timesAttacked`, the two stat-change flags
    /// as "raised/lowered" letters, and what this turn's `attackedBy` entries
    /// amount to: "sources that did damage : slot and damage of the last foe to hit".
    #[serde(default)]
    pub hurt: Option<u16>,
    #[serde(default)]
    pub times_attacked: Option<u16>,
    #[serde(default)]
    pub stat_flags: Option<String>,
    #[serde(default)]
    pub attacked_by: Option<String>,
    /// The last hit from a foe still on the field, from this turn or an earlier one ("*").
    #[serde(default)]
    pub last_damaged: Option<String>,
    /// The ids of the moves it has now (another Pokémon's while it is transformed).
    #[serde(default)]
    pub moves: Option<String>,
    #[serde(default)]
    pub transformed: Option<bool>,
    /// Team index of the Pokémon an Illusion shows; -1 for none.
    #[serde(default)]
    pub illusion: Option<i8>,
    #[serde(default)]
    pub locked: Option<String>,
    /// The type added by Forest's Curse or Trick-or-Treat; empty if none.
    #[serde(default)]
    pub added_type: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct SideSnap {
    pub left: u8,
    pub mons: Vec<MonSnap>,
    /// Side conditions as `id:turns left:detail:effect order` (absent in older files).
    #[serde(default)]
    pub conds: Option<Vec<String>>,
    /// Slot conditions per active position, as `id:turns left:detail`.
    #[serde(default)]
    pub slots: Option<Vec<Vec<String>>>,
}

/// Weather, terrain and pseudo-weathers.
#[derive(Deserialize, PartialEq, Debug)]
pub struct FieldSnap {
    pub weather: String,
    pub weather_turns: u8,
    pub terrain: String,
    pub terrain_turns: u8,
    pub pseudo: Vec<String>,
}

/// Showdown's state at one decision point.
#[derive(Deserialize, Debug)]
pub struct Snap {
    pub turn: u16,
    pub ended: bool,
    /// 0 or 1 for a winner, -1 for a tie, absent while the battle is running.
    pub winner: Option<i8>,
    pub request: String,
    pub rng: [u16; 4],
    /// Showdown's `effectOrder` counter (absent in older files).
    #[serde(default)]
    pub effect_order: Option<u32>,
    /// `battle.lastMove` (absent in older files); empty before the first move.
    #[serde(default)]
    pub battle_last_move: Option<String>,
    #[serde(default)]
    pub field: Option<FieldSnap>,
    pub sides: [SideSnap; 2],
    /// Showdown's RNG draws during the step (only recorded with `--trace`).
    #[serde(default)]
    pub draws: Vec<String>,
    /// Showdown's battle log for the step (only recorded with `--trace`).
    #[serde(default)]
    pub log: Vec<String>,
    /// The request Showdown sent each player at this point (only recorded with `--requests`).
    #[serde(default)]
    pub requests: Vec<serde_json::Value>,
}

#[derive(Deserialize, Debug)]
pub struct Step {
    pub choices: [String; 2],
    /// Legal choices per side and slot before `choices` were made.
    pub legal: [[Vec<String>; 2]; 2],
    pub after: Snap,
}

#[derive(Deserialize, Debug)]
pub struct Case {
    pub id: u32,
    pub seed: [u16; 4],
    /// The Pokémon each side brought, in the order picked.
    pub teams: [Vec<SetJson>; 2],
    /// The teams the sides registered, which Team Preview showed, and for
    /// each Pokémon brought its index there. (Absent from battles recorded
    /// before the recorder kept them.)
    #[serde(default)]
    pub rosters: Option<[Vec<SetJson>; 2]>,
    #[serde(default)]
    pub picks: Option<[Vec<usize>; 2]>,
    /// The battle was played with open team sheets (`gen_cases.js --open-sheets`).
    #[serde(default)]
    pub open_sheets: bool,
    pub initial: Snap,
    pub steps: Vec<Step>,
}

pub enum Outcome {
    /// Matched at every decision; carries the number of decisions checked.
    Pass(usize),
    /// The case uses something the engine does not model.
    Unsupported(String),
    /// Diverged; carries a human-readable report.
    Fail(Vec<String>),
}

impl Case {
    /// The battle as it stood after the opening switch-ins.
    fn start(&self, open_team_sheets: bool) -> Result<Battle, Outcome> {
        let sets = |teams: &[Vec<SetJson>; 2]| -> Result<Vec<Vec<PokemonSet>>, Outcome> {
            let all: Result<Vec<Vec<PokemonSet>>, String> =
                teams.iter().map(|t| t.iter().map(SetJson::to_set).collect()).collect();
            all.map_err(|e| Outcome::Fail(vec![e]))
        };
        let built = match (&self.rosters, &self.picks) {
            (Some(rosters), Some(picks)) => {
                let rosters = sets(rosters)?;
                Battle::with_rosters([&rosters[0], &rosters[1]], [&picks[0], &picks[1]], open_team_sheets, self.seed)
            }
            _ => {
                let teams = sets(&self.teams)?;
                Battle::new([&teams[0], &teams[1]], self.seed)
            }
        };
        built.map_err(|e| match e {
            Error::Unsupported(what) => Outcome::Unsupported(what),
            e => Outcome::Fail(vec![e.to_string()]),
        })
    }
}

fn mon_snap(b: &Battle, side: usize, pos: usize) -> MonSnap {
    let r = b.active_at(side, pos);
    let m = b.mon(r);
    MonSnap {
        idx: r.idx,
        species: SPECIES[m.species as usize].id.to_string(),
        hp: m.hp,
        maxhp: m.max_hp(),
        stats: m.stats,
        // Showdown parks a fainted Pokémon's status in ways that no longer matter.
        status: if m.fainted { String::new() } else { m.status.id().to_string() },
        time: if m.fainted { 0 } else { m.status_time },
        stage: if m.fainted { 0 } else { m.tox_stage },
        boosts: m.boosts,
        pp: m.moves[..m.n_moves as usize].iter().map(|s| s.pp).collect(),
        active: m.is_active,
        fainted: m.fainted,
        switch_flag: m.switch_flag,
        speed: m.speed,
        vol: b.vols(r).as_slice().iter().map(vol_snap).collect(),
        ability: Some(ABILITIES[m.ability as usize].id.to_string()),
        item: Some(ITEMS[m.item as usize].id.to_string()),
        last_item: Some(ITEMS[m.last_item as usize].id.to_string()),
        used_item: Some(m.used_item_this_turn),
        ate_berry: Some(m.ate_berry),
        types: Some(m.types.iter().filter(|&&t| t != Type::None).map(|&t| type_name(t)).collect()),
        trapped: Some(match m.trapped {
            Trapped::No => 0,
            Trapped::Yes => 1,
            Trapped::Hidden => 2,
        }),
        disabled: Some(m.moves[..m.n_moves as usize].iter().map(|s| s.disabled).collect()),
        ability_order: Some(m.ability_st.order),
        item_order: Some(m.item_st.order),
        active_turns: Some(m.active_turns),
        move_result: Some([m.move_this_turn, m.move_last_turn].iter().map(|r| result_code(*r)).collect()),
        base_species: Some(SPECIES[m.base_species as usize].id.to_string()),
        can_mega: Some(if m.can_mega == NO_SPECIES {
            String::new()
        } else {
            SPECIES[m.can_mega as usize].id.to_string()
        }),
        last_move: Some(if m.last_move == crate::state::NO_MOVE {
            String::new()
        } else {
            MOVES[m.last_move as usize].id.to_string()
        }),
        move_actions: Some(m.active_move_actions),
        newly_switched: Some(m.newly_switched),
        hurt: Some(m.hurt_this_turn),
        times_attacked: Some(m.times_attacked),
        stat_flags: Some(format!(
            "{}{}",
            if m.stats_raised_this_turn { 'r' } else { '-' },
            if m.stats_lowered_this_turn { 'l' } else { '-' }
        )),
        attacked_by: Some(format!(
            "{}:{}",
            m.hit_by_this_turn,
            match m.last_damaged_by() {
                Some(a) if a.this_turn => format!("{}/{}", slot_index(a.slot), a.damage),
                _ => String::new(),
            }
        )),
        last_damaged: Some(match m.last_damaged_by() {
            Some(a) => format!("{}/{}{}", slot_index(a.slot), a.damage, if a.this_turn { "" } else { "*" }),
            None => String::new(),
        }),
        moves: Some(
            m.moves[..m.n_moves as usize].iter().map(|s| MOVES[s.id as usize].id).collect::<Vec<_>>().join(","),
        ),
        transformed: Some(m.transformed),
        illusion: Some(m.illusion as i8 - 1),
        added_type: Some(if m.added_type == Type::None { String::new() } else { type_name(m.added_type) }),
        locked: Some(if m.locked_move == crate::state::NO_MOVE {
            String::new()
        } else {
            MOVES[m.locked_move as usize].id.to_string()
        }),
    }
}

/// A type as Showdown spells it.
fn type_name(t: Type) -> String {
    if t == Type::Typeless { "???".to_string() } else { format!("{t:?}") }
}

fn result_code(r: Res) -> char {
    match r {
        Res::Undef => 'u',
        Res::Null => 'n',
        Res::Bool(true) => 't',
        Res::Bool(false) => 'f',
        _ => '?',
    }
}

/// The state a side condition carries (the recorder writes the same thing).
fn side_detail(c: &Cond<SideCond>) -> String {
    match c.kind {
        // Layers.
        SideCond::Spikes | SideCond::Toxicspikes => c.data.to_string(),
        _ => "0".to_string(),
    }
}

/// A field slot as the recording prints it: -1 for "not on the field".
fn slot_index(slot: u8) -> i32 {
    if slot == crate::state::NO_SLOT { -1 } else { slot as i32 }
}

/// The state a slot condition carries.
fn slot_detail(c: &Cond<SlotCond>) -> String {
    match c.kind {
        // HP to restore / turn counter value when the wish was made.
        SlotCond::Wish => format!("{}/{}", c.data, c.st.a),
        // The turn counter value it lands at / who sent it (side, team index).
        SlotCond::Futuremove => {
            format!("{}/{}", c.st.a, c.source.map_or(String::new(), |s| format!("{}{}", s.side, s.idx)))
        }
        _ => "0".to_string(),
    }
}

/// A volatile as `id:turns left:detail`, where the detail is whatever state
/// the volatile carries (the recorder writes the same thing).
fn vol_snap(v: &crate::state::Volatile) -> String {
    let detail = match v.kind {
        VolKind::Stall | VolKind::Allyswitch => v.data.to_string(),
        VolKind::Confusion => v.data.to_string(),
        VolKind::Choicelock | VolKind::Encore | VolKind::Disable if v.data > 0 => {
            MOVES[v.data as usize - 1].id.to_string()
        }
        VolKind::Helpinghand => v.data.to_string(),
        // Kept in half points; printed the way JavaScript prints the number.
        VolKind::Substitute => format!("{}{}", v.data / 2, if v.data % 2 == 1 { ".5" } else { "" }),
        VolKind::Dragoncheer | VolKind::Partiallytrapped => v.data.to_string(),
        VolKind::Stockpile => format!("{}/{}/{}", v.data, v.st.a, v.st.b),
        VolKind::Leechseed => v.source_slot.to_string(),
        VolKind::Lockedmove if v.data > 0 => format!("{}/{}", MOVES[v.data as usize - 1].id, v.st.a),
        VolKind::Twoturnmove if v.data > 0 => MOVES[v.data as usize - 1].id.to_string(),
        VolKind::Fly
        | VolKind::Dig
        | VolKind::Dive
        | VolKind::Bounce
        | VolKind::Phantomforce
        | VolKind::Solarbeam
        | VolKind::Solarblade
        | VolKind::Skyattack
        | VolKind::Meteorbeam
        | VolKind::Electroshot => v.st.a.to_string(),
        VolKind::Metronome => {
            let last = if v.data > 0 { MOVES[v.data as usize - 1].id } else { "-" };
            format!("{last}/{}", v.st.a)
        }
        _ => "0".to_string(),
    };
    format!("{}:{}:{}", v.kind.id(), v.duration, detail)
}

/// Every field where the engine disagrees with Showdown's snapshot.
pub fn diff(b: &Battle, want: &Snap) -> Vec<String> {
    let mut out = Vec::new();
    macro_rules! check {
        ($name:expr, $got:expr, $want:expr) => {
            if $got != $want {
                out.push(format!("{}: engine {:?}, Showdown {:?}", $name, $got, $want));
            }
        };
    }
    check!("turn", b.turn, want.turn);
    check!("ended", b.ended, want.ended);
    let winner = if b.ended { Some(b.winner.map(|w| w as i8).unwrap_or(-1)) } else { None };
    check!("winner", winner, want.winner);
    let request = match b.request {
        Request::None => "",
        Request::Move => "move",
        Request::Switch => "switch",
    };
    check!("request", request, want.request.as_str());
    check!("rng seed", b.rng.words(), want.rng);
    if let Some(eo) = want.effect_order {
        check!("effect order counter", b.effect_order, eo);
    }
    if let Some(lm) = &want.battle_last_move {
        let ours = if b.last_move == crate::state::NO_MOVE { "" } else { MOVES[b.last_move as usize].id };
        check!("the battle's last move", ours, lm.as_str());
    }
    if let Some(wf) = &want.field {
        let f = &b.field;
        let got = FieldSnap {
            weather: f.weather.kind.id().to_string(),
            weather_turns: if f.weather.kind == Weather::None { 0 } else { f.weather.duration },
            terrain: f.terrain.kind.id().to_string(),
            terrain_turns: if f.terrain.kind == Terrain::None { 0 } else { f.terrain.duration },
            pseudo: f.pseudo.as_slice().iter().map(|c| format!("{}:{}", c.kind.id(), c.duration)).collect(),
        };
        check!("field", &got, wf);
    }
    for side in 0..2 {
        let ws = &want.sides[side];
        check!(format!("p{} pokemon left", side + 1), b.sides[side].pokemon_left, ws.left);
        if let Some(wc) = &ws.conds {
            let got: Vec<String> = b.sides[side]
                .conds
                .as_slice()
                .iter()
                .map(|c| format!("{}:{}:{}:{}", c.kind.id(), c.duration, side_detail(c), c.st.order))
                .collect();
            check!(format!("p{} side conditions", side + 1), &got, wc);
        }
        if let Some(wsl) = &ws.slots {
            let got: Vec<Vec<String>> = b.sides[side]
                .slot_conds
                .iter()
                .map(|l| {
                    l.as_slice().iter().map(|c| format!("{}:{}:{}", c.kind.id(), c.duration, slot_detail(c))).collect()
                })
                .collect();
            check!(format!("p{} slot conditions", side + 1), &got, wsl);
        }
        if b.sides[side].n as usize != ws.mons.len() {
            out.push(format!("p{} team size differs", side + 1));
            continue;
        }
        for (pos, w) in ws.mons.iter().enumerate() {
            let g = mon_snap(b, side, pos);
            let who = format!("p{} position {} ({})", side + 1, pos + 1, w.species);
            check!(format!("{who} team index"), g.idx, w.idx);
            check!(format!("{who} species"), &g.species, &w.species);
            check!(format!("{who} hp"), g.hp, w.hp);
            check!(format!("{who} max hp"), g.maxhp, w.maxhp);
            check!(format!("{who} stats"), g.stats, w.stats);
            check!(format!("{who} status"), &g.status, &w.status);
            check!(format!("{who} status timer"), g.time, w.time);
            check!(format!("{who} toxic stage"), g.stage, w.stage);
            check!(format!("{who} boosts"), g.boosts, w.boosts);
            check!(format!("{who} pp"), &g.pp, &w.pp);
            check!(format!("{who} active"), g.active, w.active);
            check!(format!("{who} fainted"), g.fainted, w.fainted);
            check!(format!("{who} switch flag"), g.switch_flag, w.switch_flag);
            check!(format!("{who} cached speed"), g.speed, w.speed);
            check!(format!("{who} volatiles"), &g.vol, &w.vol);
            // Fields older recordings do not carry are only compared when present.
            macro_rules! check_opt {
                ($name:expr, $field:ident) => {
                    if w.$field.is_some() {
                        check!(format!("{who} {}", $name), &g.$field, &w.$field);
                    }
                };
            }
            check_opt!("ability", ability);
            check_opt!("item", item);
            check_opt!("last item", last_item);
            check_opt!("used item this turn", used_item);
            check_opt!("ate berry", ate_berry);
            check_opt!("types", types);
            check_opt!("disabled moves", disabled);
            check_opt!("active turns", active_turns);
            check_opt!("base species", base_species);
            check_opt!("can mega evolve", can_mega);
            check_opt!("illusion", illusion);
            if w.active {
                check_opt!("move results", move_result);
                check_opt!("last move", last_move);
                check_opt!("move actions since switching in", move_actions);
                check_opt!("newly switched", newly_switched);
                check_opt!("hurt this turn", hurt);
                check_opt!("times attacked", times_attacked);
                check_opt!("stats raised/lowered this turn", stat_flags);
                check_opt!("attacked by", attacked_by);
                check_opt!("last damaged by", last_damaged);
                check_opt!("moves", moves);
                check_opt!("transformed", transformed);
                check_opt!("added type", added_type);
                check_opt!("trapped", trapped);
                check_opt!("ability effect order", ability_order);
                check_opt!("item effect order", item_order);
            }
        }
    }
    out
}

fn diff_legal(b: &Battle, want: &[[Vec<String>; 2]; 2]) -> Vec<String> {
    let mut out = Vec::new();
    for (side, slots) in want.iter().enumerate() {
        for (pos, expected) in slots.iter().enumerate() {
            let mut got: Vec<String> = b.legal_choices(side, pos).into_iter().map(Choice::to_showdown).collect();
            let mut exp = expected.clone();
            got.sort();
            exp.sort();
            if got != exp {
                out.push(format!(
                    "legal choices for p{} slot {}: engine {:?}, Showdown {:?}",
                    side + 1,
                    pos + 1,
                    got,
                    exp
                ));
            }
        }
    }
    out
}

/// The `rng(range)=value` part of a trace line, without its label.
fn drawn(line: &str) -> &str {
    line.split("  ").next().unwrap_or(line)
}

/// With tracing on both sides, checks that the engine drew the same ranges and
/// values in the same order as Showdown, not merely that the seeds agree afterwards.
fn draws_differ(snap: &Snap) -> Option<Vec<String>> {
    if !cfg!(feature = "trace") || snap.draws.is_empty() {
        trace::take();
        return None;
    }
    let ours = trace::take();
    let ours_rng: Vec<&String> = ours.iter().filter(|l| l.starts_with("rng(")).collect();
    let same =
        ours_rng.len() == snap.draws.len() && ours_rng.iter().zip(&snap.draws).all(|(a, b)| drawn(a) == drawn(b));
    if same {
        return None;
    }
    let mut out = vec![format!("RNG draws differ: engine {}, Showdown {}", ours_rng.len(), snap.draws.len())];
    let len = ours_rng.len().max(snap.draws.len());
    for j in 0..len {
        let a = ours_rng.get(j).map(|s| s.as_str()).unwrap_or("-");
        let b = snap.draws.get(j).map(|s| s.as_str()).unwrap_or("-");
        if drawn(a) != drawn(b) {
            out.push(format!("  #{j:<3} engine   {a}"));
            out.push(format!("       showdown {b}"));
        }
    }
    Some(out)
}

/// Lines up the engine's RNG draws against Showdown's for the failing step.
fn trace_report(snap: &Snap, out: &mut Vec<String>) {
    let ours = trace::take();
    if snap.draws.is_empty() || ours.is_empty() {
        out.push(
            "(to see where the RNG streams part: record with `gen_cases.js --trace` and build with `--features trace`)"
                .into(),
        );
        return;
    }
    let ours_rng: Vec<&String> = ours.iter().filter(|l| l.starts_with("rng(")).collect();
    out.push(format!("RNG draws this step: engine {}, Showdown {}", ours_rng.len(), snap.draws.len()));
    let len = ours_rng.len().max(snap.draws.len());
    let first_bad = (0..len).find(|&i| match (ours_rng.get(i), snap.draws.get(i)) {
        (Some(a), Some(b)) => drawn(a) != drawn(b),
        _ => true,
    });
    if let Some(i) = first_bad {
        out.push(format!("first differing draw is #{i}:"));
        for j in i.saturating_sub(5)..(i + 4).min(len) {
            out.push(format!("  #{j:<3} engine   {}", ours_rng.get(j).map(|s| s.as_str()).unwrap_or("-")));
            out.push(format!("       showdown {}", snap.draws.get(j).map(|s| s.as_str()).unwrap_or("-")));
        }
    }
    out.push("engine trace:".into());
    out.extend(ours.iter().map(|l| format!("  {l}")));
    out.push("Showdown log:".into());
    let noise = |l: &&String| l.is_empty() || l.starts_with("|t:|") || l.starts_with("|split") || l.contains("uhtml");
    out.extend(snap.log.iter().filter(|l| !noise(l)).map(|l| format!("  {l}")));
}

/// What else to do at every decision while replaying.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rebuild {
    /// Nothing: just replay.
    No,
    /// Write the position down, build a battle back from it, require the two
    /// to be the same position, and carry on with the rebuilt one.
    Exact,
    /// Also build a battle from the position stripped of the simulator's
    /// bookkeeping, as if it had been written by hand, and require what
    /// `from_state` works out to be what the engine had.
    ByHand,
}

/// A position with everything removed that a battle built from a hand-written
/// description may legitimately have differently: the random numbers, the
/// order effects started in and the cached speeds.
fn loose(st: &BattleState, at_move_request: bool) -> BattleState {
    let mut s = st.clone();
    s.seed = [0; 4];
    s.rng_calls = 0;
    s.effect_order = 0;
    s.speed_order.clear();
    let strip = |c: &mut crate::position::CondState| {
        c.order = 0;
        // (Whether a field condition's handlers have ever run: it changes how
        // Showdown sorts them among ties, and cannot be known from outside.)
        c.targeted = false;
    };
    s.weather.iter_mut().for_each(strip);
    s.terrain.iter_mut().for_each(strip);
    s.pseudo_weather.iter_mut().for_each(strip);
    for side in &mut s.sides {
        side.conditions.iter_mut().for_each(strip);
        side.slot_conditions.iter_mut().flatten().for_each(|c| c.order = 0);
        for (place, m) in side.pokemon.iter_mut().enumerate() {
            m.speed = 0;
            m.status_extra.order = 0;
            m.ability_extra.order = 0;
            m.item_extra.order = 0;
            m.volatiles.iter_mut().for_each(strip);
            // (Leftover flags on moves it cannot use while transformed.)
            for mv in &mut m.base_moves {
                mv.disabled = false;
                mv.hidden = false;
            }
            if !at_move_request || place >= crate::ACTIVE || m.fainted {
                // Only a move request works these out; in between they are leftovers.
                m.trapped = Trapped::No;
                m.locked_move.clear();
                for mv in &mut m.moves {
                    mv.disabled = false;
                    mv.hidden = false;
                }
            }
        }
    }
    if s.ended {
        // (Nothing is going to continue.)
        s.mid_turn = None;
    }
    // The queue as a set: ties in it are settled by the random numbers.
    if st.ended {
        s.pending.clear();
    }
    for a in &mut s.pending {
        a.resolved = None;
    }
    s.pending.sort_by_key(|a| (a.kind.clone(), a.pokemon.map(|m| (m.side, m.pokemon)), a.move_id.clone()));
    s
}

/// The first place two positions differ, as JSON paths.
fn json_diff(path: &str, a: &serde_json::Value, b: &serde_json::Value, out: &mut Vec<String>) {
    use serde_json::Value;
    if a == b || out.len() >= 6 {
        return;
    }
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let mut keys: Vec<&String> = x.keys().chain(y.keys()).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                json_diff(
                    &format!("{path}.{k}"),
                    x.get(k).unwrap_or(&Value::Null),
                    y.get(k).unwrap_or(&Value::Null),
                    out,
                );
            }
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
            for (i, (p, q)) in x.iter().zip(y).enumerate() {
                json_diff(&format!("{path}[{i}]"), p, q, out);
            }
        }
        _ => out.push(format!("{path}: engine {a}, rebuilt {b}")),
    }
}

/// The rebuild checks at one decision. On success the battle to carry on with.
fn rebuild(b: &Battle, mode: Rebuild) -> Result<Battle, Vec<String>> {
    let st = b.to_state();
    let text = st.to_json();
    match BattleState::from_json(&text) {
        Ok(back) if back == st => {}
        Ok(_) => return Err(vec!["the position changes when written as JSON and read back".into()]),
        Err(e) => return Err(vec![format!("the position's own JSON does not read back: {e}")]),
    }
    let exact = Battle::from_state(&st).map_err(|e| vec![format!("from_state refused an exported position: {e}")])?;
    if let Some(d) = b.position_diff(&exact) {
        return Err(vec!["a battle rebuilt from the exported position differs:".into(), d]);
    }
    if mode == Rebuild::ByHand {
        let by_hand = Battle::from_state(&st.without_bookkeeping()).map_err(|e| {
            vec![
                format!("from_state refused the position without bookkeeping: {e}"),
                format!("pending: {}", serde_json::to_string(&st.pending).unwrap_or_default()),
            ]
        })?;
        let at_move = b.request == Request::Move;
        let (want, got) = (loose(&st, at_move), loose(&by_hand.to_state(), at_move));
        if want != got {
            let mut out = vec!["a battle built from the position without bookkeeping differs:".to_string()];
            let (x, y) = (serde_json::to_value(&want).unwrap(), serde_json::to_value(&got).unwrap());
            json_diff("", &x, &y, &mut out);
            return Err(out);
        }
        // (Working the bookkeeping out draws random numbers of its own; they are not the battle's.)
        trace::take();
    }
    Ok(exact)
}

/// Replays one recorded battle and compares state after every decision.
pub fn check_case(c: &Case) -> Outcome {
    check_case_with(c, Rebuild::No)
}

/// [`check_case`], optionally rebuilding the battle from its exported position at every decision.
pub fn check_case_with(c: &Case, mode: Rebuild) -> Outcome {
    trace::take();
    let mut b = match c.start(c.open_sheets) {
        Ok(b) => b,
        Err(outcome) => return outcome,
    };
    let d = diff(&b, &c.initial);
    if !d.is_empty() {
        let mut out = vec!["state differs after the opening switch-ins".to_string()];
        out.extend(d);
        trace_report(&c.initial, &mut out);
        return Outcome::Fail(out);
    }
    if let Some(lines) = draws_differ(&c.initial) {
        let mut out = vec!["same state, different RNG draws during the opening switch-ins".to_string()];
        out.extend(lines);
        return Outcome::Fail(out);
    }
    for (i, step) in c.steps.iter().enumerate() {
        trace::take();
        let turn = b.turn;
        let header = |what: &str| {
            format!("{what} at decision {i} (turn {turn}), choices p1 [{}] p2 [{}]", step.choices[0], step.choices[1])
        };
        if mode != Rebuild::No {
            match rebuild(&b, mode) {
                Ok(rebuilt) => b = rebuilt,
                Err(mut out) => {
                    out.insert(0, header("rebuilding the position failed"));
                    return Outcome::Fail(out);
                }
            }
        }
        let dl = diff_legal(&b, &step.legal);
        if !dl.is_empty() {
            let mut out = vec![header("legal choices differ")];
            out.extend(dl);
            return Outcome::Fail(out);
        }
        let (Some(p1), Some(p2)) = (Choice::parse_side(&step.choices[0]), Choice::parse_side(&step.choices[1])) else {
            return Outcome::Fail(vec![header("unreadable choice")]);
        };
        if let Err(e) = b.choose([p1, p2]) {
            return Outcome::Fail(vec![header("engine rejected the choice"), e.to_string()]);
        }
        let d = diff(&b, &step.after);
        if !d.is_empty() {
            let mut out = vec![header("state differs")];
            out.extend(d);
            trace_report(&step.after, &mut out);
            return Outcome::Fail(out);
        }
        if let Some(lines) = draws_differ(&step.after) {
            let mut out = vec![header("same state, different RNG draws")];
            out.extend(lines);
            return Outcome::Fail(out);
        }
    }
    if mode != Rebuild::No {
        // The final position too (usually a finished battle).
        if let Err(mut out) = rebuild(&b, mode) {
            out.insert(0, "rebuilding the final position failed".to_string());
            return Outcome::Fail(out);
        }
    }
    Outcome::Pass(c.steps.len())
}

/// What `check_shown` found over a set of battles: every way the engine's
/// idea of what has been shown differed from what the log says, and every
/// time the log's reader believed something untrue, each with how often and
/// where it first happened.
#[derive(Default, Debug)]
pub struct ShownTally {
    pub decisions: usize,
    pub mismatches: std::collections::BTreeMap<String, (usize, String)>,
    pub untrue: std::collections::BTreeMap<String, (usize, String)>,
    /// On sides that brought a Pokémon with Illusion (and, with closed
    /// sheets, only Pokémon that may have it), at each decision, the
    /// Pokémon on the field that were really in disguise and were marked as
    /// possibly so; those that were themselves and marked all the same; and
    /// those that were themselves and known to be.
    pub disguised_marked: usize,
    pub genuine_marked: usize,
    pub genuine_known: usize,
}

impl ShownTally {
    fn note(map: &mut std::collections::BTreeMap<String, (usize, String)>, key: String, at: &str) {
        let e = map.entry(key).or_insert_with(|| (0, at.to_string()));
        e.0 += 1;
    }
}

fn shown_mon_diffs(what: &str, ours: &ShownMon, theirs: &ShownMon, rosters: bool, out: &mut Vec<String>) {
    let mut field = |name: &str, a: String, b: String| {
        if a != b {
            out.push(format!("{what} {name}: engine {a}, log {b}"));
        }
    };
    if rosters {
        field("listed", format!("{:?}", ours.listed), format!("{:?}", theirs.listed));
        field("maybe a disguise", ours.maybe_disguise.to_string(), theirs.maybe_disguise.to_string());
    }
    field("id", ours.id.to_string(), theirs.id.to_string());
    field("species", ours.species.clone(), theirs.species.clone());
    field("gender", ours.gender.clone(), theirs.gender.clone());
    field("status", ours.status.clone(), theirs.status.clone());
    field("fainted", ours.fainted.to_string(), theirs.fainted.to_string());
    field("transformed", ours.transformed.to_string(), theirs.transformed.to_string());
    field("item", format!("{:?}", ours.item), format!("{:?}", theirs.item));
    field("item lost", ours.item_lost.clone(), theirs.item_lost.clone());
    field("ability", format!("{:?}", ours.ability), format!("{:?}", theirs.ability));
    field("base ability", format!("{:?}", ours.base_ability), format!("{:?}", theirs.base_ability));
    if (ours.hp, ours.bar) != (theirs.hp, theirs.bar) {
        out.push(format!("{what} hp differs"));
    }
    for m in &theirs.moves {
        if !ours.moves.contains(m) {
            out.push(format!("{what} move {m}: only in the log"));
        }
    }
    for m in &ours.moves {
        if !theirs.moves.contains(m) {
            out.push(format!("{what} move {m}: only in the engine"));
        }
    }
}

/// `rosters`: the battle was recorded with the teams as registered, so what
/// each side knows of the other's registered team can be compared too.
fn shown_diffs(ours: &ShownSide, theirs: &ShownSide, rosters: bool) -> Vec<String> {
    let mut out = Vec::new();
    if rosters {
        if ours.roster.len() != theirs.roster.len() {
            out.push(format!("roster: engine lists {}, log {}", ours.roster.len(), theirs.roster.len()));
        }
        for (j, (a, b)) in ours.roster.iter().zip(&theirs.roster).enumerate() {
            if (&a.species, &a.gender) != (&b.species, &b.gender) {
                out.push(format!("roster {j}: engine {} {}, log {} {}", a.species, a.gender, b.species, b.gender));
            }
            match (&a.sheet, &b.sheet) {
                (Some(a), Some(b)) => {
                    let mut field = |name: &str, a: &str, b: &str| {
                        if a != b {
                            out.push(format!("sheet {name}: engine {a}, log {b}"));
                        }
                    };
                    field("item", &a.item, &b.item);
                    field("ability", &a.ability, &b.ability);
                    field("nature", &a.nature, &b.nature);
                    field("moves", &a.moves.join(","), &b.moves.join(","));
                }
                (None, None) => {}
                (a, _) => out.push(format!("sheet: only in the {}", if a.is_some() { "engine" } else { "log" })),
            }
        }
        if ours.illusion != theirs.illusion {
            out.push(format!("who has Illusion: engine {:?}, log {:?}", ours.illusion, theirs.illusion));
        }
    }
    if ours.unseen != theirs.unseen {
        out.push(format!("unseen: engine {}, log {}", ours.unseen, theirs.unseen));
    }
    if ours.left != theirs.left {
        out.push(format!("left: engine {}, log {}", ours.left, theirs.left));
    }
    for (a, b) in ours.active.iter().zip(&theirs.active) {
        match (a, b) {
            (Some(a), Some(b)) => shown_mon_diffs("active", a, b, rosters, &mut out),
            (None, None) => {}
            (a, _) => {
                out.push(format!("active: {} only in the engine", if a.is_some() { "someone" } else { "nobody" }))
            }
        }
    }
    if ours.bench.len() != theirs.bench.len() {
        out.push(format!("bench: engine has {}, log {}", ours.bench.len(), theirs.bench.len()));
    }
    for (a, b) in ours.bench.iter().zip(&theirs.bench) {
        shown_mon_diffs("bench", a, b, rosters, &mut out);
    }
    out
}

/// Sets the engine's records of what has been shown to the log reader's.
fn adopt_shown(b: &mut Battle, obs: &Observer) -> Result<(), String> {
    for side in 0..2 {
        let filed = obs.filed_records(side);
        let s = &mut b.sides[side];
        if filed.len() != s.n_seen as usize {
            return Err(format!("the engine has shown {} Pokémon of side {side}, the log {}", s.n_seen, filed.len()));
        }
        for rec in filed {
            let Some(a) = (0..s.n as usize).find(|&a| s.shown[a].seen == rec.seen) else {
                return Err("the orders of appearance differ".to_string());
            };
            s.shown[a] = rec;
        }
        for pos in 0..ACTIVE.min(s.n as usize) {
            let idx = s.order[pos] as usize;
            let m = &mut s.team[idx];
            match (obs.live_record(side, pos), m.is_active && m.live.seen != 0) {
                (Some(rec), true) => m.live = rec,
                (None, false) => {}
                _ => return Err("who is on the field differs".to_string()),
            }
        }
    }
    Ok(())
}

/// Whether the other side can know which of `side`'s Pokémon have Illusion:
/// always with open team sheets, and with closed ones as long as only
/// Pokémon that may have it under the regulation do. (The recorder also
/// hands Illusion to Pokémon that may not, and those battles say nothing
/// about closed sheets.)
fn illusion_knowable(b: &Battle, side: usize) -> bool {
    let s = &b.sides[side];
    let public = b.shown(side).illusion;
    // (By the ability each was registered with: a Mega Evolution replaces it.)
    s.roster[..s.n_roster as usize]
        .iter()
        .enumerate()
        .all(|(j, l)| l.ability != ab::ILLUSION || l.brought == NOT_LISTED || public.contains(&(j as u8)))
}

/// Whether what the log reader believes about the Pokémon is true of them.
///
/// In an `honest` battle (one without Illusion) that goes for every record.
/// In one with Illusion it goes for the records that are not marked as
/// possibly a disguise's, on a side whose Illusion Pokémon can be known:
/// what is noted there has to be about the Pokémon it is filed under.
fn untrue_beliefs(b: &Battle, honest: bool, out: &mut Vec<String>) {
    for side in 0..2 {
        let s = &b.sides[side];
        if !honest && !illusion_knowable(b, side) {
            continue;
        }
        for a in 0..s.n as usize {
            let m = &s.team[a];
            let live = m.is_active && m.live.seen != 0;
            let rec = if live { &m.live } else { &s.shown[a] };
            if rec.seen == 0 || (!honest && (rec.suspect || rec.tainted)) {
                continue;
            }
            let own =
                if m.transformed { &m.base_moves[..m.base_n_moves as usize] } else { &m.moves[..m.n_moves as usize] };
            for &mv in rec.moves() {
                if !own.iter().any(|slot| slot.id == mv) {
                    out.push(format!("move {}: not one of its own", MOVES[mv as usize].id));
                }
            }
            match rec.item {
                ItemShown::Holds(i) if m.item != i => {
                    out.push(format!("item {}: it holds {}", ITEMS[i as usize].id, ITEMS[m.item as usize].id))
                }
                ItemShown::Lost(i) if m.item != it::NONE => {
                    out.push(format!("item {}: lost, but it holds {}", ITEMS[i as usize].id, ITEMS[m.item as usize].id))
                }
                _ => {}
            }
            let name = |x: u16| ABILITIES[x as usize].id;
            let current = if live || m.fainted { m.ability } else { m.base_ability };
            // A Pokémon with Illusion that Mega Evolved in disguise. The log put the Mega
            // Evolution down to the Pokémon it looked like, so nothing says that this
            // one's Illusion has been replaced for good. (Only the recorder builds such a
            // Pokémon: none that may have Illusion has a Mega Stone under the regulation.)
            let mega = ITEMS.iter().flat_map(|i| i.mega.iter()).any(|&(_, to)| to == m.species);
            if mega && rec.base_ability == ab::ILLUSION {
                continue;
            }
            if rec.ability != crate::shown::UNKNOWN && rec.ability != current && !m.fainted {
                out.push(format!("ability {}: it has {}", name(rec.ability), name(current)));
            }
            if rec.base_ability != crate::shown::UNKNOWN && rec.base_ability != m.base_ability {
                out.push(format!("base ability {}: it is {}", name(rec.base_ability), name(m.base_ability)));
            }
        }
    }
}

/// Whether what the record says about who is who is true: a Pokémon on the
/// field that is in fact disguised must be marked as possibly so, and one
/// that is itself must point at its own entry of the registered team.
/// (The first where the other side can know which Pokémon have Illusion:
/// see `illusion_knowable`.)
fn untrue_identities(b: &Battle, out: &mut Vec<String>, tally: &mut [usize; 3]) {
    for side in 0..2 {
        let s = &b.sides[side];
        let roster = &s.roster[..s.n_roster as usize];
        let brought_one = roster.iter().any(|l| l.ability == ab::ILLUSION && l.brought != NOT_LISTED);
        let knowable = illusion_knowable(b, side);
        for pos in 0..ACTIVE.min(s.n as usize) {
            let idx = s.order[pos] as usize;
            let m = &s.team[idx];
            if !m.is_active || m.live.seen == 0 {
                continue;
            }
            let name = SPECIES[m.set_species as usize].id;
            if brought_one && knowable {
                let which = if m.illusion != 0 { 0 } else { 1 + usize::from(!m.live.suspect) };
                tally[which] += 1;
            }
            if m.illusion != 0 {
                if knowable && !m.live.suspect {
                    out.push(format!("{name} is in disguise and is not marked as possibly one"));
                }
                continue;
            }
            // (A team with the same Pokémon twice gives no way to tell which entry is which.)
            let twins = roster.iter().filter(|l| l.species == m.set_species).count() > 1;
            let own = roster.iter().position(|l| l.brought == idx as u8);
            if !twins && own.map(|j| j as u8) != Some(m.live.listed) {
                out.push(format!("{name} is listed as entry {} of its team, and is {own:?}", m.live.listed));
            }
        }
    }
}

/// Replays a battle recorded with its log (`gen_cases.js --log`) and checks,
/// at every decision, that what the engine says has been shown is what a
/// reader of the log makes of it. Differences are counted in `tally` and the
/// engine's records set to the log's, so that one missing rule is counted
/// where it applies rather than at every decision after it.
pub fn check_shown(c: &Case, tally: &mut ShownTally) -> Outcome {
    check_shown_with(c, Rebuild::No, tally)
}

/// [`check_shown`], also rebuilding the battle from its exported position
/// before every decision (see [`Rebuild`]): what has been shown is part of
/// a position and has to survive the trip.
pub fn check_shown_with(c: &Case, mode: Rebuild, tally: &mut ShownTally) -> Outcome {
    check_shown_as(c, mode, false, tally)
}

/// [`check_shown_with`]; with `closed`, a battle recorded with open team
/// sheets is replayed as if the sheets had stayed closed: the engine is told
/// so, and the log's reader is not given the lines that show them.
pub fn check_shown_as(c: &Case, mode: Rebuild, closed: bool, tally: &mut ShownTally) -> Outcome {
    trace::take();
    let mut b = match c.start(c.open_sheets && !closed) {
        Ok(b) => b,
        Err(outcome) => return outcome,
    };
    if c.initial.log.is_empty() {
        return Outcome::Fail(vec!["the battle was recorded without its log (gen_cases.js --log)".to_string()]);
    }
    // Recorded with the teams as registered? Then the reader gets Team Preview too.
    let rosters = c.rosters.is_some() && c.picks.is_some();
    let honest = !b.sides.iter().any(|s| s.team[..s.n as usize].iter().any(|m| m.base_ability == ab::ILLUSION));
    let mut obs = Observer::new();
    let mut compare = |b: &mut Battle, obs: &mut Observer, log: &[String], at: String| -> Result<(), Vec<String>> {
        let skipped = |line: &&String| {
            (closed && line.starts_with("|showteam|"))
                || (!rosters && ["|poke|", "|clearpoke", "|showteam|"].iter().any(|k| line.starts_with(k)))
        };
        let log: Vec<&String> = log.iter().filter(|line| !skipped(line)).collect();
        obs.lines(&log).map_err(|e| vec![format!("{at}: the log could not be read: {e}")])?;
        for side in 0..2 {
            for d in shown_diffs(&b.shown(side), &obs.shown(side), rosters) {
                ShownTally::note(&mut tally.mismatches, d, &at);
            }
        }
        adopt_shown(b, obs).map_err(|e| vec![format!("{at}: {e}")])?;
        let mut untrue = Vec::new();
        if honest || rosters {
            untrue_beliefs(b, honest, &mut untrue);
        }
        if rosters {
            let mut counts = [0; 3];
            untrue_identities(b, &mut untrue, &mut counts);
            tally.disguised_marked += counts[0];
            tally.genuine_marked += counts[1];
            tally.genuine_known += counts[2];
        }
        for u in untrue {
            ShownTally::note(&mut tally.untrue, u, &at);
        }
        tally.decisions += 1;
        Ok(())
    };
    if !diff(&b, &c.initial).is_empty() {
        return Outcome::Fail(vec!["state differs after the opening switch-ins".to_string()]);
    }
    if let Err(out) = compare(&mut b, &mut obs, &c.initial.log, format!("case {} at the start", c.id)) {
        return Outcome::Fail(out);
    }
    for (i, step) in c.steps.iter().enumerate() {
        if mode != Rebuild::No {
            match rebuild(&b, mode) {
                Ok(rebuilt) => b = rebuilt,
                Err(mut out) => {
                    out.insert(0, format!("rebuilding the position failed at decision {i}"));
                    return Outcome::Fail(out);
                }
            }
        }
        let (Some(p1), Some(p2)) = (Choice::parse_side(&step.choices[0]), Choice::parse_side(&step.choices[1])) else {
            return Outcome::Fail(vec![format!("unreadable choice at decision {i}")]);
        };
        if let Err(e) = b.choose([p1, p2]) {
            return Outcome::Fail(vec![format!("engine rejected the choice at decision {i}"), e.to_string()]);
        }
        if !diff(&b, &step.after).is_empty() {
            return Outcome::Fail(vec![format!("state differs at decision {i}")]);
        }
        if let Err(out) = compare(&mut b, &mut obs, &step.after.log, format!("case {} decision {i}", c.id)) {
            return Outcome::Fail(out);
        }
    }
    Outcome::Pass(c.steps.len())
}

// ------------------------------------------------- following from the log

/// How [`check_follow`] has gone over the battles it was given.
#[derive(Default, Debug)]
pub struct FollowTally {
    /// Observations compared, and how many differed anywhere.
    pub observations: usize,
    pub wrong: usize,
    /// The same for the observations a network acts on: the ones where the player has
    /// more than one thing it can do. (The rest are a side waiting for the other to
    /// send in a replacement, or with one legal action.)
    pub choices: usize,
    pub wrong_choices: usize,
    /// The first difference of each battle that has one.
    pub first_wrong: Vec<String>,
    /// By feature: how often it differed, and one place it did.
    pub by_feature: std::collections::BTreeMap<String, (usize, String)>,
    /// Observations passed over: a move was disabled by something the player has not
    /// been shown (a foe's Imprison). Showdown then lists the move as usable until the
    /// player tries it, so its request and the simulator's own legal actions part ways.
    pub hidden_disables: usize,
    /// Print every difference as it is found.
    pub verbose: bool,
}

/// A name for each number of an observation's feature buffer, by part and column.
fn feature_name(index: usize) -> String {
    use crate::obs::{ACT_F, ACT_VOLATILES, FIELD_F, MON_F, MOVE_F, layout};
    let (parts, _) = layout();
    let (name, start, _) = parts.iter().rev().find(|p| index >= p.1).expect("an index inside the buffer");
    let at = index - start;
    match *name {
        "field" => {
            let names = ["phase", "phase", "phase", "open sheets", "turn"];
            let n_pseudo = crate::data::Pseudo::ALL.len();
            let n_side = SideCond::ALL.len();
            let per_side = n_side + 5;
            let col = at % FIELD_F;
            if col < 5 {
                format!("field: {}", names[col])
            } else if col < 10 {
                "field: weather".into()
            } else if col == 10 {
                "field: weather timer".into()
            } else if col < 16 {
                "field: terrain".into()
            } else if col == 16 {
                "field: terrain timer".into()
            } else if col < 17 + n_pseudo {
                format!("field: {}", crate::data::Pseudo::ALL[col - 17].id())
            } else if col < 17 + 2 * n_pseudo {
                format!("field: {} turns", crate::data::Pseudo::ALL[col - 17 - n_pseudo].id())
            } else if col < 17 + 2 * n_pseudo + 2 * per_side {
                let k = (col - 17 - 2 * n_pseudo) % per_side;
                if k < n_side {
                    format!("side: {}", SideCond::ALL[k].id())
                } else {
                    format!(
                        "side: {}",
                        [
                            "tailwind turns",
                            "safeguard turns",
                            "reflect timer",
                            "light screen timer",
                            "aurora veil timer"
                        ][k - n_side]
                    )
                }
            } else {
                let k = col - 17 - 2 * n_pseudo - 2 * per_side;
                format!(
                    "field: {}",
                    ["own left", "their left", "their unseen", "own mega", "their mega"].get(k).unwrap_or(&"?")
                )
            }
        }
        "mon_feats" => {
            let own = if at / MON_F < crate::obs::ROSTER { "own" } else { "their" };
            let col = at % MON_F;
            let what = match col {
                0 => "exists",
                1 => "own",
                2 => "brought",
                3 => "left behind",
                4 => "seen",
                5 => "active",
                6 => "fainted",
                7 => "hp",
                8..=14 => "status",
                15 => "tox",
                16..=22 => "stats",
                23 => "item known",
                24 => "ability known",
                25 => "moves known",
                26 => "can mega",
                27 => "maybe disguise",
                28 => "transformed",
                29..=31 => "gender",
                32..=41 => "nature",
                42..=45 => "pp",
                46 => "speed low",
                47 => "speed high",
                _ => "speed items",
            };
            format!("{own} mon: {what}")
        }
        "act_feats" => {
            let own = if at / ACT_F < ACTIVE { "own" } else { "their" };
            let col = at % ACT_F;
            let n_vol = VolKind::ALL.len();
            let what = match col {
                0 => "there".to_string(),
                1 => "hp".to_string(),
                2..=8 => "status".to_string(),
                9..=15 => "boosts".to_string(),
                16..=33 => "types".to_string(),
                34 => "first turn".to_string(),
                35 => "active turns".to_string(),
                36 => "trapped".to_string(),
                37 => "locked".to_string(),
                c if c < ACT_VOLATILES + n_vol => format!("vol {}", VolKind::ALL[c - ACT_VOLATILES].id()),
                c if c == ACT_VOLATILES + n_vol => "perish count".to_string(),
                c if c == ACT_VOLATILES + n_vol + 1 => "protect streak".to_string(),
                c if c < ACT_VOLATILES + n_vol + 2 + SlotCond::ALL.len() => {
                    format!("slot {}", SlotCond::ALL[c - ACT_VOLATILES - n_vol - 2].id())
                }
                _ => "moves first".to_string(),
            };
            format!("{own} position: {what}")
        }
        _ => format!("move token: {}", ["there", "pp", "disabled", "forced"][at % MOVE_F]),
    }
}

fn id_name(index: usize) -> String {
    use crate::obs::{ACT_IDS, MON_IDS, layout};
    let (_, parts) = layout();
    let (name, start, _) = parts.iter().rev().find(|p| index >= p.1).expect("an index inside the buffer");
    let at = index - start;
    match *name {
        "mon_ids" => {
            let own = if at / MON_IDS < crate::obs::ROSTER { "own" } else { "their" };
            let what = ["species", "item", "lost item", "ability", "move", "move", "move", "move"][at % MON_IDS];
            format!("{own} mon id: {what}")
        }
        "act_ids" => {
            let own = if at / ACT_IDS < ACTIVE { "own" } else { "their" };
            format!("{own} position id: {}", ["token", "last move"][at % ACT_IDS])
        }
        "move_ids" => "move token id".to_string(),
        _ => format!("info: {}", ["phase", "joint actions", "turn", "acting"][at]),
    }
}

/// Whether one of `side`'s Pokémon on the field is barred from a move or from switching by
/// something its player has not been shown (a foe's Imprison, a Shadow Tag not yet announced).
/// Showdown then offers the choice all the same and refuses it when it is tried; the game
/// does not offer it.
pub fn hidden_disable(game: &crate::env::Game, side: usize) -> bool {
    let Some(b) = game.battle() else { return false };
    b.request == Request::Move
        && (0..ACTIVE).any(|pos| {
            let m = b.mon(b.active(side, pos));
            let last = crate::obs::last_to_choose(b, side, pos);
            m.is_active
                && !m.fainted
                && (m.moves[..m.n_moves as usize].iter().any(|slot| slot.disabled && slot.hidden)
                    || (m.trapped == Trapped::Hidden && last && b.living_bench(side) > 0))
        })
}

/// What a recorded battle told its players at one decision: the log up to it
/// and the request each side was sent.
#[derive(Deserialize, serde::Serialize, Clone, Debug)]
pub struct Told {
    #[serde(default)]
    pub ended: bool,
    pub log: Vec<String>,
    pub requests: Vec<serde_json::Value>,
}

#[derive(Deserialize, serde::Serialize, Clone, Debug)]
pub struct ToldStep {
    pub choices: [String; 2],
    pub after: Told,
}

/// A recorded battle as [`check_follow`] needs it: the teams, what was
/// chosen, and what the players were told. Any battle recorded with
/// `gen_cases.js --log --requests` reads as one (the rest of what the
/// recorder writes is passed over), and one written back out is a small
/// fraction of the size.
#[derive(Deserialize, serde::Serialize, Clone, Debug)]
pub struct FollowCase {
    pub id: u32,
    pub seed: [u16; 4],
    pub rosters: [Vec<SetJson>; 2],
    pub picks: [Vec<usize>; 2],
    #[serde(default)]
    pub open_sheets: bool,
    pub initial: Told,
    pub steps: Vec<ToldStep>,
}

/// Plays a recorded battle in a [`Game`](crate::env::Game) and, beside it,
/// has a [`Follower`](crate::follow::Follower) for each side read the log
/// and the requests Showdown sent that side. At every decision the
/// follower's observation and legal actions must be the game's. Differences
/// are counted in `tally`; the outcome is a failure only if a follower could
/// not read something.
// (The two sides, and the two versions of an observation, are indexed side by side.)
#[allow(clippy::needless_range_loop)]
pub fn check_follow(c: &FollowCase, tally: &mut FollowTally) -> Outcome {
    use crate::env::{Game, OBS_M};
    use crate::follow::Follower;
    use crate::obs::{OBS_F, OBS_I};
    let mut differed = false;
    let mut compare = |game: &Game, followers: &[Follower; 2], step: usize| -> Result<(), String> {
        let (mut f, mut i, mut m) = (
            [vec![0f32; OBS_F], vec![0f32; OBS_F]],
            [vec![0i16; OBS_I], vec![0i16; OBS_I]],
            [vec![0u8; OBS_M], vec![0u8; OBS_M]],
        );
        for side in 0..2 {
            if hidden_disable(game, side) {
                tally.hidden_disables += 1;
                continue;
            }
            game.observe(side, &mut f[0], &mut i[0], &mut m[0]);
            followers[side].observe(&mut f[1], &mut i[1], &mut m[1])?;
            tally.observations += 1;
            let at = format!("battle {} step {step} side {side}", c.id);
            let mut wrong = false;
            let verbose = tally.verbose;
            let mut note = |name: String, a: String, b: String| {
                if verbose {
                    println!("{at}: {name}: the game has {a}, the follower {b}");
                }
                if !differed {
                    differed = true;
                    tally.first_wrong.push(format!("{at}: {name}: the game has {a}, the follower {b}"));
                }
                let entry = tally.by_feature.entry(name).or_insert((0, String::new()));
                entry.0 += 1;
                if entry.1.is_empty() {
                    entry.1 = format!("{at}: the game has {a}, the follower {b}");
                }
                wrong = true;
            };
            for k in 0..OBS_F {
                if (f[0][k] - f[1][k]).abs() > 1e-6 {
                    note(feature_name(k), format!("{} (at {k})", f[0][k]), f[1][k].to_string());
                }
            }
            for k in 0..OBS_I {
                if i[0][k] != i[1][k] {
                    note(id_name(k), format!("{} (at {k})", i[0][k]), i[1][k].to_string());
                }
            }
            if m[0] != m[1] {
                let (a, b) = (m[0].iter().filter(|&&x| x != 0).count(), m[1].iter().filter(|&&x| x != 0).count());
                note("legal actions".to_string(), format!("{a} set"), format!("{b} set"));
            }
            tally.wrong += wrong as usize;
            if i[0][OBS_I - crate::obs::INFO + 3] != 0 {
                tally.choices += 1;
                tally.wrong_choices += wrong as usize;
                if wrong && verbose {
                    println!("{at}: (the player has a choice to make here)");
                }
            }
        }
        Ok(())
    };
    walk_follow(c, &mut compare)
}

/// What [`check_export`] found over recorded battles.
#[derive(Default, Debug)]
pub struct ExportTally {
    /// Decisions at which a follower's [`game_state`](crate::follow::Follower::game_state) was asked for.
    pub asked: usize,
    /// Exports refused, by reason.
    pub refused: std::collections::BTreeMap<String, usize>,
    /// Exports that did not load, or whose game could not list a side's choices.
    pub broken: usize,
    /// Exports that observe differently from the follower that made them.
    pub observed_differently: usize,
    /// Exports whose counts of hidden-length effects differ from the game's, Pokémon by Pokémon.
    pub counts_differ: usize,
    pub first: Vec<String>,
}

/// Follows a recorded battle as [`check_follow`] does and, at every decision of
/// each side (where nothing barred is hidden from it), takes the follower's
/// [`game_state`](crate::follow::Follower::game_state): it must load, list both
/// sides' choices, observe for that side as the follower does, and count the
/// hidden-length effects as the game does (each Pokémon found by its sheet entry,
/// since the follower numbers the other side's in the order they appeared).
pub fn check_export(c: &FollowCase, tally: &mut ExportTally) -> Outcome {
    use crate::env::{Game, OBS_M};
    use crate::obs::{OBS_F, OBS_I};
    // A team with a species twice (which no ladder allows, and the harness will not follow): the
    // other side's Pokémon cannot be told apart by their sheets, so there is no export to check.
    let twice = c.rosters.iter().any(|team| {
        let species: Vec<&str> = team.iter().map(|s| s.species.as_str()).collect();
        species.iter().enumerate().any(|(k, a)| species[..k].contains(a))
    });
    if twice {
        return Outcome::Unsupported("a team with a species twice".into());
    }
    let mut visit = |game: &Game, followers: &[crate::follow::Follower; 2], step: usize| -> Result<(), String> {
        for (side, follower) in followers.iter().enumerate() {
            if hidden_disable(game, side) || follower.phase().is_none() {
                continue;
            }
            tally.asked += 1;
            let at = format!("battle {} step {step} side {side}", c.id);
            let note = |tally: &mut ExportTally, what: String| {
                if tally.first.len() < 20 {
                    tally.first.push(format!("{at}: {what}"));
                }
            };
            let st = match follower.game_state() {
                Ok(st) => st,
                Err(why) => {
                    *tally.refused.entry(why).or_insert(0) += 1;
                    continue;
                }
            };
            let export = match Game::from_state(&st) {
                Ok(g) => g,
                Err(e) => {
                    tally.broken += 1;
                    note(tally, format!("does not load: {e}"));
                    continue;
                }
            };
            let Some(b) = export.battle() else { continue };
            let listed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (export.legal(0), export.legal(1))));
            if listed.is_err() {
                tally.broken += 1;
                note(tally, "its game cannot list a side's choices".into());
                continue;
            }
            let (mut f, mut i, mut m) = (
                [vec![0f32; OBS_F], vec![0f32; OBS_F]],
                [vec![0i16; OBS_I], vec![0i16; OBS_I]],
                [vec![0u8; OBS_M], vec![0u8; OBS_M]],
            );
            export.observe(side, &mut f[0], &mut i[0], &mut m[0]);
            follower.observe(&mut f[1], &mut i[1], &mut m[1])?;
            if f[0] != f[1] || i[0] != i[1] || m[0] != m[1] {
                tally.observed_differently += 1;
                note(tally, "observes differently from its follower".into());
            }
            // The counts, Pokémon by Pokémon: the follower's other side is numbered by appearance.
            let Some(truth) = game.battle() else { continue };
            let (mine, theirs) = (export.counts(), game.counts());
            let mut differ = false;
            for s in 0..2 {
                for idx in 0..b.sides[s].n as usize {
                    let j = (0..b.sides[s].n_roster as usize).find(|&j| b.sides[s].roster[j].brought == idx as u8);
                    let Some(t) = j.map(|j| truth.sides[s].roster[j].brought) else { continue };
                    let t = t as usize;
                    if t >= truth.sides[s].n as usize {
                        continue;
                    }
                    let a = [mine.sleep[s][idx], mine.confusion[s][idx], mine.rampage[s][idx], mine.trap[s][idx]];
                    let z = [theirs.sleep[s][t], theirs.confusion[s][t], theirs.rampage[s][t], theirs.trap[s][t]];
                    if a != z {
                        differ = true;
                        note(tally, format!("counts of side {s} #{idx}: the follower's {a:?}, the game's {z:?}"));
                    }
                }
            }
            tally.counts_differ += differ as usize;
        }
        Ok(())
    };
    walk_follow(c, &mut visit)
}

/// Plays a recorded battle in a [`Game`](crate::env::Game) beside a
/// [`Follower`](crate::follow::Follower) for each side, as [`check_follow`]
/// does, and calls `visit` at every decision (Team Preview, then each request)
/// with the game, the followers and the step. An error from `visit` fails the
/// battle.
#[allow(clippy::needless_range_loop)]
pub fn walk_follow(
    c: &FollowCase,
    visit: &mut dyn FnMut(&crate::env::Game, &[crate::follow::Follower; 2], usize) -> Result<(), String>,
) -> Outcome {
    use crate::env::Game;
    use crate::follow::Follower;
    let (rosters, picks) = (&c.rosters, &c.picks);
    let sets: Result<Vec<Vec<PokemonSet>>, String> =
        rosters.iter().map(|t| t.iter().map(SetJson::to_set).collect()).collect();
    let sets = match sets {
        Ok(sets) => sets,
        Err(e) => return Outcome::Fail(vec![e]),
    };
    if c.initial.requests.len() != 2 {
        return Outcome::Unsupported("a battle recorded without its requests".into());
    }
    let mut game = match Game::new([sets[0].clone(), sets[1].clone()], c.open_sheets) {
        Ok(game) => game,
        Err(Error::Unsupported(what)) => return Outcome::Unsupported(what),
        Err(e) => return Outcome::Fail(vec![e.to_string()]),
    };
    // The recorder gives each Pokémon a name of its own, which the first request has
    // for those brought. (Its teams can have a species twice, which no ladder allows.)
    let names = |side: usize| -> Vec<String> {
        let mut out = vec![String::new(); sets[side].len()];
        let listed = c.initial.requests[side]["side"]["pokemon"].as_array();
        for (k, mon) in listed.into_iter().flatten().enumerate() {
            let name = mon["ident"].as_str().and_then(|i| i.split_once(": ")).map(|(_, n)| n.to_string());
            if let (Some(name), Some(&j)) = (name, picks[side].get(k)) {
                out[j] = name;
            }
        }
        out
    };
    let mut followers = match (Follower::new(0, sets[0].clone()), Follower::new(1, sets[1].clone())) {
        (Ok(a), Ok(b)) => [a.with_names(names(0)), b.with_names(names(1))],
        (Err(e), _) | (_, Err(e)) => return Outcome::Fail(vec![e]),
    };
    // The recorder's teams do not all keep the item clause, and the game knows which do.
    let clause = crate::format::Format::current().team.item_clause == 1;
    for side in 0..2 {
        let team = &sets[1 - side];
        let once =
            team.iter().enumerate().all(|(k, a)| a.item == it::NONE || team[..k].iter().all(|b| b.item != a.item));
        followers[side].trust_item_clause(clause && once);
    }
    let mut decisions = 0;
    // With closed sheets a player takes the other team for a legal one. The recorder also
    // hands Illusion to Pokémon the regulation gives no such ability, and what a player
    // makes of those battles is not what the game, which knows better, makes of them.
    if !c.open_sheets {
        let legal = |set: &PokemonSet| {
            let rule = crate::format::Format::current().rule(set.species);
            set.ability != ab::ILLUSION || rule.is_some_and(|r| r.abilities.contains(&ab::ILLUSION))
        };
        if !sets.iter().flatten().all(legal) {
            return Outcome::Unsupported("an Illusion the regulation does not allow, with closed sheets".into());
        }
    }
    let result = (|| -> Result<(), String> {
        // Team Preview: everything up to the battle's start.
        let cut = c.initial.log.iter().position(|l| l.starts_with("|teamsize|")).unwrap_or(0);
        for side in 0..2 {
            followers[side].lines(&Follower::own_lines(side, &c.initial.log[..cut]))?;
        }
        if cut > 0 {
            visit(&game, &followers, 0)?;
        }
        game.start([&picks[0], &picks[1]], c.seed).map_err(|e| e.to_string())?;
        for side in 0..2 {
            followers[side].lines(&Follower::own_lines(side, &c.initial.log[cut..]))?;
            followers[side].request(&c.initial.requests[side].to_string())?;
        }
        visit(&game, &followers, 0)?;
        decisions += 1;
        for (k, step) in c.steps.iter().enumerate() {
            let parse = |s: &str| Choice::parse_side(s).ok_or_else(|| format!("unreadable choice {s:?}"));
            let choices = [parse(&step.choices[0])?, parse(&step.choices[1])?];
            // What the game takes as actions, each follower must turn back into Showdown's words.
            for side in 0..2 {
                let action = |pos: usize| game.action_of(side, choices[side][pos]).ok_or("a choice with no action");
                if hidden_disable(&game, side) {
                    // (What was chosen is still what its follower goes on from.)
                    if let (Some(first), Some(second)) = (action(0).ok(), action(1).ok()) {
                        let _ = followers[side].choice([first, second]);
                    }
                    continue;
                }
                let reply = followers[side].choice([action(0)?, action(1)?])?;
                if Choice::parse_side(&reply) != Some(choices[side]) {
                    return Err(format!(
                        "battle {} step {}: side {side} chose {:?}, and its follower would have sent {reply:?}",
                        c.id,
                        k + 1,
                        step.choices[side]
                    ));
                }
            }
            game.choose(choices).map_err(|e| e.to_string())?;
            for side in 0..2 {
                followers[side].lines(&Follower::own_lines(side, &step.after.log))?;
                let request = step.after.requests.get(side).ok_or("a step recorded without its requests")?;
                followers[side].request(&request.to_string())?;
            }
            if step.after.ended {
                if !game.ended() || !followers.iter().all(|f| f.ended() && f.winner() == game.winner()) {
                    return Err(format!("battle {}: the followers and the game disagree about how it ended", c.id));
                }
                break;
            }
            visit(&game, &followers, k + 1)?;
            decisions += 1;
        }
        Ok(())
    })();
    match result {
        Ok(()) => Outcome::Pass(decisions),
        Err(e) => Outcome::Fail(vec![e]),
    }
}
