//! Follows a battle on Pokémon Showdown from what one player is sent, and
//! turns it into the observation a model was trained on.
//!
//! In training the observation is written from the simulator's own state
//! ([`crate::obs`]). On Showdown there is no such state: a player gets a log
//! of what happens, line by line, and at every decision a request listing
//! its own Pokémon and what it may do. [`Follower`] takes those two in and
//! keeps what they add up to: who stands where, stat stages, the conditions
//! each Pokémon and side is under and for how long, what each of the other
//! side's Pokémon has shown, and what the order of moves says of its Speed.
//! From that it rebuilds a [`Battle`] that is right in everything the
//! observation reads, and writes the observation and the legal actions from
//! it with the same code the training games use.
//!
//! ```no_run
//! # use vgc_engine::follow::Follower;
//! # use vgc_engine::env::OBS_M;
//! # use vgc_engine::obs::{OBS_F, OBS_I};
//! # let (team, side) = (Vec::new(), 0);
//! let mut me = Follower::new(side, team).unwrap();
//! let (mut f, mut i, mut mask) = (vec![0.0; OBS_F], vec![0; OBS_I], vec![0; OBS_M]);
//! // For every message of the battle room: the log lines, then the request.
//! me.line("|turn|1").unwrap();
//! me.request(r#"{"active":[],"side":{"pokemon":[]}}"#).unwrap();
//! if me.observe(&mut f, &mut i, &mut mask).unwrap() > 1 {
//!     // ...a model picks `actions` from the observation...
//!     let reply = me.choice([0, 46]).unwrap(); // "move 1, pass"
//! }
//! ```
//!
//! `tests/follow.rs` checks it against recorded Showdown battles: at every
//! decision, the observation rebuilt from the log and the request must be
//! the one the simulator gives for the same battle.
//!
//! # What cannot be had from the log
//!
//! A few things the simulator knows are not said anywhere, and the
//! observation leaves them out for both sides so that training and play see
//! the same: conditions the game keeps to itself (a Choice lock, a flinch
//! waiting to happen), and the exact turns a rampage has left.
//!
//! # Where it and the simulator part
//!
//! Showdown keeps one thing from the last Pokémon of a side to choose: that
//! it is barred from a move or from switching by something not yet shown (a
//! foe's Imprison, a Shadow Tag that has not announced itself). It lists the
//! choice all the same, refuses it if it is made, and sends the request
//! again put right. The follower offers what Showdown lists, and takes the
//! corrected request like any other; the simulator, in training, does not
//! offer the choice in the first place.
//!
//! # Public move information
//!
//! A failed move's line can hide both its target and whether it began. The
//! observation therefore gives the last visibly attempted queued move, not
//! the last move used for mechanics. A hidden Choice refusal is an attempt
//! too, even though it consumes no PP and does not update mechanical last_move.
//!
//! When a hidden random target could have charged different Pressure costs,
//! remaining PP is unknown (-1 in the observation) until an exact move request
//! supplies it. This also covers wait requests and Pokémon leaving the field.
//!
//! And one thing it is told late. A player's own Pokémon with Illusion comes
//! in under another's name in its own log too. The follower knows which
//! Pokémon it is from its HP, which its side is shown exactly, or from having
//! sent it in; where it was dragged in and has the very HP of the one it
//! passes for, the follower takes it for that one until the next request,
//! which says who stands where.
//!
//! # Types
//!
//! Showdown says what a Pokémon's types become whenever they change, with
//! one exception: after Reflect Type the log names the Pokémon copied and
//! not the types. If that Pokémon is in disguise, its own side knows what
//! was taken and the other side sees the types of the disguise. At the end
//! of the turn Showdown says the new types if they are not the ones its log
//! last gave (and says nothing after copying a partner). The follower keeps
//! the types as its side has been told them, the simulator keeps both
//! sides' (`Battle::seen_types`), and the two go by the same rules.

use serde_json::Value;

use crate::battle::{PokemonSet, calc_stats, types_copied};
use crate::data::*;
use crate::env::{N_PREVIEW, OBS_M, choice_of, masks_of, preview_table};
use crate::obs::{INFO, OBS_I, Phase, ROSTER, Timers, is_mega, observe_battle, observe_preview};
use crate::observer::{Observer, ident};
use crate::shown::{ItemShown, UNKNOWN};
use crate::speed::{self, Event, FieldSeen, Queued, Seen, Speeds};
use crate::state::*;

/// When a condition seen on a Pokémon goes away without the log saying so.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ends {
    /// It lasts until the log ends it, or the Pokémon leaves.
    Said,
    /// At the end of the turn.
    Turn,
    /// When the Pokémon next moves.
    Move,
    /// After this many more turns have ended.
    After(u8),
}

#[derive(Clone, Copy, Debug)]
struct Vol {
    kind: VolKind,
    ends: Ends,
    /// What the engine keeps in the condition's `data` and `duration`, where the observation reads them.
    data: u16,
    duration: u8,
    /// The stay (see [`Spot::stay`]) of the Pokémon keeping it up, if it ends when that one leaves.
    source: u32,
}

/// One position on the field: what holds for whoever stands there, from the log.
#[derive(Clone, Debug)]
struct Spot {
    /// A number for this stay on the field: a new one every time a Pokémon comes in.
    stay: u32,
    boosts: [i8; 7],
    vols: Vec<Vol>,
    /// Its types, if something has said what they are: as this side takes them (a
    /// Reflect Type names only the Pokémon copied, which may be in disguise).
    types: Option<[Type; 2]>,
    added: Type,
    /// This turn it has taken another Pokémon's types (Reflect Type, Transform), the last
    /// time by Reflect Type from a foe; and Showdown has since said what they are.
    copied: bool,
    copied_foe: bool,
    told: bool,
    /// What its types were taken to be before the first of those.
    before: [Type; 2],
    active_turns: u16,
    last_move: u16,
    last_move_loc: Option<i8>,
    last_attempted_move: Option<u16>,
    tox: u8,
    /// Badly poisoned during the end of this turn: the poison has not built up yet.
    tox_new: bool,
    /// Queued moves it has begun this turn.
    begun: u8,
    /// Its move was put ahead or behind by another (After You, Quash, Round), as of this many moves begun.
    reordered: Option<usize>,
    /// A Speed Swap has been used while it has stood here: its Speed may not be its own.
    swap_seen: bool,
    /// Its move this turn may not be the one it chose: an Encore came first, as of this many moves begun.
    overridden: Option<usize>,
    /// Its move went ahead of its bracket, and said so (Quick Claw, Quick Draw, Custap Berry).
    quick: bool,
    /// It has been seen to lose an item since it came in (Unburden asks).
    lost_item: bool,
    /// Something has been said of its ability since the last request.
    ability_touched: bool,
    /// Own ability knowledge after an allied swap, before the next exact request.
    own_abilities: Option<Vec<u16>>,
    /// The item the log has given it, or left it without (`it::NONE`), since the last request.
    item_since: Option<u16>,
    /// It has said it has Pressure, as its holder does on coming in or coming by it. (A
    /// Pokémon passing for one that has it says nothing.)
    pressing: bool,
    /// The move its Choice item holds it to, where that can be told: its first since it
    /// came in with the item at work. (`None`: none, or not known.)
    choice: Option<u16>,
    /// It has been seen to use a move its Choice item would have held it from: whatever
    /// the reason, the item is not taken to be at work on it.
    no_hold: bool,
    /// The opposing Pokémon that have damaged it with a move since it came in and are
    /// active at turn cleanup, oldest first: identity and the original attacking slot.
    hit_by: Vec<((usize, usize), usize)>,
    /// Instruct has been used on it: its next move is the repeat, put in ahead of the queue.
    instructed: bool,
    /// The forme it has changed into on the field, where the log has said so (a request
    /// goes on giving the forme it came in as).
    forme: Option<u16>,
    /// It came in on the left with none of its side left in reserve: the one case in
    /// which an Illusion copies the Pokémon standing beside it.
    last_two: bool,
}

impl Spot {
    fn new(stay: u32) -> Spot {
        Spot {
            stay,
            boosts: [0; 7],
            vols: Vec::new(),
            types: None,
            added: Type::None,
            copied: false,
            copied_foe: false,
            told: false,
            before: [Type::None; 2],
            active_turns: 0,
            last_move: NO_MOVE,
            last_move_loc: None,
            last_attempted_move: None,
            tox: 0,
            tox_new: false,
            begun: 0,
            reordered: None,
            swap_seen: false,
            overridden: None,
            quick: false,
            lost_item: false,
            ability_touched: false,
            own_abilities: None,
            item_since: None,
            pressing: false,
            choice: None,
            no_hold: false,
            hit_by: Vec::new(),
            instructed: false,
            forme: None,
            last_two: false,
        }
    }

    fn has(&self, kind: VolKind) -> bool {
        self.vols.iter().any(|v| v.kind == kind)
    }

    fn add(&mut self, kind: VolKind, ends: Ends) -> bool {
        if self.has(kind) {
            return false;
        }
        self.vols.push(Vol { kind, ends, data: 0, duration: 0, source: 0 });
        true
    }

    fn remove(&mut self, kind: VolKind) {
        self.vols.retain(|v| v.kind != kind);
    }

    /// A protecting move worked: the next one in a row is a third as likely to.
    fn stalled(&mut self) {
        match self.vols.iter_mut().find(|v| v.kind == VolKind::Stall) {
            Some(v) => (v.data, v.ends) = ((v.data * 3).min(729), Ends::After(2)),
            None => self.vols.push(Vol { kind: VolKind::Stall, ends: Ends::After(2), data: 3, duration: 0, source: 0 }),
        }
    }

    /// Brought down to earth, by Smack Down or by Gravity: out of the air if it had flown
    /// or bounced up there, and that move is off; and out of a Magnet Rise. (A move it is
    /// charging on the ground, Sky Attack say, goes on.)
    fn grounded(&mut self) {
        let airborne = |v: &Vol| matches!(v.kind, VolKind::Fly | VolKind::Bounce);
        let up = self.vols.iter().any(airborne);
        self.vols.retain(|v| !airborne(v) && v.kind != VolKind::Magnetrise && !(up && v.kind == VolKind::Twoturnmove));
    }
}

/// The two-turn moves' own conditions, by the move's id.
const CHARGING: [VolKind; 10] = [
    VolKind::Fly,
    VolKind::Dig,
    VolKind::Dive,
    VolKind::Bounce,
    VolKind::Phantomforce,
    VolKind::Solarbeam,
    VolKind::Solarblade,
    VolKind::Skyattack,
    VolKind::Meteorbeam,
    VolKind::Electroshot,
];

#[derive(Clone, Debug, Default)]
struct SideTrack {
    /// Each condition, how many layers of it, and how many turns have ended since it went up.
    conds: Vec<(SideCond, u8, u16)>,
    /// Conditions on each position, with how many more turns' ends they last (0: until said).
    slots: [Vec<(SlotCond, u8)>; ACTIVE],
}

#[derive(Clone, Debug)]
struct FieldTrack {
    weather: Weather,
    weather_for: u16,
    terrain: Terrain,
    terrain_for: u16,
    /// The weather, or the terrain, went up while this turn was ending: too late for the
    /// turn's end to count against it.
    weather_late: bool,
    terrain_late: bool,
    /// Field conditions and the turns they have left.
    pseudo: Vec<(Pseudo, u8)>,
    sides: [SideTrack; 2],
}

/// One Pokémon of the follower's own, of those it brought.
#[derive(Clone, Debug)]
struct Own {
    /// Its place in the registered team.
    entry: usize,
    /// The name Showdown knows it by.
    name: String,
    /// The PP of its own moves as last known: move, PP left, most PP.
    pp: Vec<(u16, u8, u8)>,
    /// The same for the moves it has copied, while it is transformed.
    copied: Vec<(u16, u8, u8)>,
    /// Normal and transformed moves with a hidden, indeterminate Pressure cost.
    unknown_pp: [Vec<u16>; 2],
}

#[derive(Clone, Debug)]
struct ReqMove {
    id: u16,
    /// PP left and most PP, where the request gives them.
    pp: Option<(u8, u8)>,
    disabled: bool,
}

#[derive(Clone, Debug, Default)]
struct ReqActive {
    moves: Vec<ReqMove>,
    trapped: bool,
    maybe_trapped: bool,
}

#[derive(Clone, Debug)]
struct ReqMon {
    name: String,
    species: u16,
    hp: u16,
    max_hp: u16,
    status: Status,
    fainted: bool,
    active: bool,
    moves: Vec<u16>,
    ability: u16,
    base_ability: u16,
    item: u16,
    reviving: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Asked {
    Preview,
    Move,
    Switch,
    /// The other side is choosing replacements.
    Wait,
}

#[derive(Clone, Debug)]
struct Req {
    asked: Asked,
    mons: Vec<ReqMon>,
    active: Vec<ReqActive>,
    force: [bool; ACTIVE],
}

/// One of the follower's own Pokémon at one moment: what settles how fast it moves.
#[derive(Clone, Debug)]
struct OwnNow {
    seen: Seen,
    /// Its ability, or the ones it may have.
    abilities: Vec<u16>,
    item: u16,
    /// It has lost an item since it came in.
    lost_item: bool,
    /// Whether its known ability, item and field conditions put it on the ground.
    grounded: Option<bool>,
    /// Its Speed stat.
    stat: u16,
}

impl OwnNow {
    /// What `f` makes of it, if that is the same whichever ability it has.
    fn agreed<T: PartialEq + Copy>(&self, f: impl Fn(u16) -> Option<T>) -> Option<T> {
        let first = f(*self.abilities.first()?)?;
        self.abilities.iter().all(|&a| f(a) == Some(first)).then_some(first)
    }
}

/// A move just shown that its user's Choice item holds it from, to be taken back if the
/// next line says it failed: the move it had last used before, and the PP counted for
/// this one (whose, whether of a moveset it copied, which move, how many).
#[derive(Clone, Debug)]
struct Refusal {
    side: usize,
    pos: usize,
    last_move: u16,
    last_move_loc: Option<i8>,
    paid: Option<(usize, bool, u16, u8, bool)>,
}

/// A queued move that has begun since the last decision.
#[derive(Clone, Debug)]
struct Begun {
    side: usize,
    stay: u32,
    r: MonRef,
    /// The move the log named for it (`NO_MOVE`: none).
    named: u16,
    /// It had begun a move already this turn.
    again: bool,
    /// The move is the repeat Instruct called for, which was never in the queue with the rest.
    instructed: bool,
    /// All that has been said of it is that its user is asleep.
    asleep: bool,
    reordered: Option<usize>,
    /// A nameless cant tentatively consumed Round; a later Snore can disprove it.
    round_before: Option<Option<usize>>,
    overridden: Option<usize>,
    /// Encore is still active as this move begins; replacement can outlive it.
    encored: bool,
    quick: bool,
    seen: [Seen; 4],
    field: FieldSeen,
    /// The follower's own two at that moment: which stay, and what it knew of each.
    own: [(u32, Option<OwnNow>); ACTIVE],
}

/// What a move in progress has set up that a `-fail` line takes back.
#[derive(Clone, Copy, Debug)]
enum Undo {
    Vol(VolKind),
    Slot(usize, usize, SlotCond),
    /// The protect streak as it stood before the move.
    Stall(Option<Vol>),
}

#[derive(Clone, Debug)]
struct Mover {
    side: usize,
    pos: usize,
    stay: u32,
    /// Where it aimed, if at one position.
    target: Option<(usize, usize)>,
    undo: Option<Undo>,
    /// Its move is one that protects, which works less often in a row.
    stalling: bool,
    /// The protection that move puts up: the log calls every kind "Protect".
    shield: Option<VolKind>,
}

/// Follows one player's side of a battle on Showdown. See the module notes.
#[derive(Clone, Debug)]
pub struct Follower {
    side: usize,
    team: Vec<PokemonSet>,
    /// The names the team was registered under, where they were given.
    nicknames: Vec<String>,
    /// Whether the other team is taken to keep the item clause, if that has been said.
    item_clause: Option<bool>,
    stats: [[u16; 6]; ROSTER],
    reader: Observer,
    names: [String; 2],
    spots: [[Spot; ACTIVE]; 2],
    field: FieldTrack,
    turn: u16,
    stays: u32,
    own: Vec<Own>,
    /// The follower's own Pokémon at each position: its team index.
    own_at: [usize; ACTIVE],
    req: Option<Req>,
    battle: Option<Battle>,
    /// The battle as it stood when this turn's moves were chosen.
    turn_start: Option<Battle>,
    timers: Timers,
    speeds: Option<Speeds>,
    begun: Vec<Begun>,
    /// Items seen to change hands since the last decision, each with how many moves had begun by then.
    items: Vec<(usize, Event)>,
    mover: Option<Mover>,
    /// The move in the line just read, if its user's Choice item cannot have let it begin.
    refusable: Option<Refusal>,
    /// The moves chosen with the last reply, by team index, and where each was aimed.
    chosen: Vec<(usize, u16, i8)>,
    /// The Pokémon of its own that the last reply sent in, by position: one that comes in
    /// under another's name (Illusion) is known by this until the next request says so.
    sent: [Option<usize>; ACTIVE],
    /// By side: it has Mega Evolved.
    megaed: [bool; 2],
    /// The turn is ending: what the log says now comes of the end-of-turn effects.
    ending: bool,
    /// Pokémon of the other side whose number changed since the last decision (a disguise dropped): from, to.
    renumbered: Vec<Vec<(usize, usize, bool)>>,
    /// A Round may have promoted one next action, as of this many moves begun.
    rounds: Vec<usize>,
    /// The Pokémon that has just woken or thawed to move, with the status it had.
    roused: Option<(usize, usize, Status)>,
    started: bool,
    ended: bool,
    winner: Option<usize>,
}

fn err<T>(what: impl Into<String>) -> Result<T, String> {
    Err(what.into())
}

/// Whether a `move` line is a move from the queue: one used outright, a later
/// turn of one, a Round another Round called forward, or the move Instruct
/// has repeated. (Not one that another move or an ability calls: Sleep Talk's
/// pick, a bounced Taunt.)
fn queued_move(name: &str, from: &str) -> bool {
    from.is_empty()
        || from == "lockedmove"
        || from == "move: Instruct"
        || from.strip_prefix("move: ").is_some_and(|caller| to_id(caller) == to_id(name))
}

/// The effect a line names, without the kind it says it is (`move: Taunt`, `ability: Flash Fire`).
fn effect_id(effect: &str) -> String {
    let name = effect
        .split_once(": ")
        .map_or(effect, |(kind, name)| if matches!(kind, "move" | "ability" | "item") { name } else { effect });
    to_id(name)
}

fn stat_index(name: &str) -> Option<usize> {
    ["atk", "def", "spa", "spd", "spe", "accuracy", "evasion"].iter().position(|&s| s == name)
}

fn type_named(name: &str) -> Type {
    const TYPES: [(&str, Type); 18] = [
        ("normal", Type::Normal),
        ("fighting", Type::Fighting),
        ("flying", Type::Flying),
        ("poison", Type::Poison),
        ("ground", Type::Ground),
        ("rock", Type::Rock),
        ("bug", Type::Bug),
        ("ghost", Type::Ghost),
        ("steel", Type::Steel),
        ("fire", Type::Fire),
        ("water", Type::Water),
        ("grass", Type::Grass),
        ("electric", Type::Electric),
        ("psychic", Type::Psychic),
        ("ice", Type::Ice),
        ("dragon", Type::Dragon),
        ("dark", Type::Dark),
        ("fairy", Type::Fairy),
    ];
    let id = to_id(name);
    if name == "???" {
        return Type::Typeless;
    }
    TYPES.iter().find(|(n, _)| *n == id).map_or(Type::None, |&(_, t)| t)
}

/// `141/169 par`, `0 fnt`: HP, most HP, status and whether it has fainted, from a request.
fn own_condition(s: &str) -> Result<(u16, u16, Status, bool), String> {
    let (hp, status) = s.split_once(' ').unwrap_or((s, ""));
    if status == "fnt" {
        return Ok((0, 0, Status::None, true));
    }
    let bad = || format!("unreadable condition {s:?}");
    let (num, den) = hp.split_once('/').ok_or_else(bad)?;
    let status = match status {
        "" => Status::None,
        "brn" => Status::Brn,
        "par" => Status::Par,
        "psn" => Status::Psn,
        "tox" => Status::Tox,
        "slp" => Status::Slp,
        "frz" => Status::Frz,
        _ => return Err(bad()),
    };
    Ok((num.parse().map_err(|_| bad())?, den.parse().map_err(|_| bad())?, status, false))
}

impl Req {
    fn parse(text: &str) -> Result<Option<Req>, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| format!("unreadable request: {e}"))?;
        if v.is_null() {
            return Ok(None);
        }
        let s = |v: &Value| v.as_str().unwrap_or("").to_string();
        let mut mons = Vec::new();
        for p in v["side"]["pokemon"].as_array().map_or(&[][..], |a| a) {
            let name = s(&p["ident"]).split_once(": ").map_or(String::new(), |(_, n)| n.to_string());
            let (species, _) = crate::observer::details(&s(&p["details"]))?;
            let (hp, max_hp, status, fainted) = own_condition(&s(&p["condition"]))?;
            let ability = |v: &Value| {
                let id = to_id(&s(v));
                if id.is_empty() { Ok(ab::NOABILITY) } else { ability_id(&id).ok_or(format!("unknown ability {id}")) }
            };
            let mut moves = Vec::new();
            for m in p["moves"].as_array().map_or(&[][..], |a| a) {
                moves.push(move_id(&to_id(&s(m))).ok_or_else(|| format!("unknown move {m}"))?);
            }
            mons.push(ReqMon {
                name,
                species,
                hp,
                max_hp,
                status,
                fainted,
                active: p["active"].as_bool().unwrap_or(false),
                moves,
                ability: if p["ability"].is_null() { ability(&p["baseAbility"])? } else { ability(&p["ability"])? },
                base_ability: ability(&p["baseAbility"])?,
                item: item_id(&to_id(&s(&p["item"]))).ok_or_else(|| format!("unknown item {}", p["item"]))?,
                reviving: p["reviving"].as_bool().unwrap_or(false),
            });
        }
        let mut active = Vec::new();
        for a in v["active"].as_array().map_or(&[][..], |a| a) {
            let mut out = ReqActive {
                trapped: a["trapped"].as_bool().unwrap_or(false),
                maybe_trapped: a["maybeTrapped"].as_bool().unwrap_or(false),
                ..Default::default()
            };
            for m in a["moves"].as_array().map_or(&[][..], |a| a) {
                let id = move_id(&to_id(&s(&m["id"]))).ok_or_else(|| format!("unknown move {}", m["id"]))?;
                let pp = match (m["pp"].as_u64(), m["maxpp"].as_u64()) {
                    (Some(pp), Some(max)) => Some((pp.min(255) as u8, max.min(255) as u8)),
                    _ => None,
                };
                out.moves.push(ReqMove { id, pp, disabled: m["disabled"].as_bool().unwrap_or(false) });
            }
            active.push(out);
        }
        let mut force = [false; ACTIVE];
        let asked = if v["teamPreview"].as_bool() == Some(true) {
            Asked::Preview
        } else if let Some(list) = v["forceSwitch"].as_array() {
            for (k, flag) in list.iter().take(ACTIVE).enumerate() {
                force[k] = flag.as_bool().unwrap_or(false);
            }
            Asked::Switch
        } else if v["wait"].as_bool() == Some(true) {
            Asked::Wait
        } else {
            Asked::Move
        };
        Ok(Some(Req { asked, mons, active, force }))
    }
}

impl Follower {
    /// A follower for the player on `side` (0 for `p1`), who registered
    /// `team`: its six Pokémon in the order they were sent to Showdown.
    pub fn new(side: usize, team: Vec<PokemonSet>) -> Result<Follower, String> {
        if side > 1 || team.len() != ROSTER {
            return err(format!("a follower plays side 0 or 1 with a team of {ROSTER}"));
        }
        let mut stats = [[0u16; 6]; ROSTER];
        for (j, set) in team.iter().enumerate() {
            stats[j] = calc_stats(set.species, set.nature, set.stat_points);
        }
        Ok(Follower {
            side,
            team,
            nicknames: Vec::new(),
            item_clause: None,
            stats,
            reader: Observer::new(),
            names: [String::new(), String::new()],
            spots: std::array::from_fn(|_| std::array::from_fn(|_| Spot::new(0))),
            field: FieldTrack {
                weather: Weather::None,
                weather_for: 0,
                terrain: Terrain::None,
                terrain_for: 0,
                weather_late: false,
                terrain_late: false,
                pseudo: Vec::new(),
                sides: Default::default(),
            },
            turn: 0,
            stays: 0,
            own: Vec::new(),
            own_at: [0, 1],
            req: None,
            battle: None,
            turn_start: None,
            timers: Timers::default(),
            speeds: None,
            begun: Vec::new(),
            items: Vec::new(),
            mover: None,
            refusable: None,
            chosen: Vec::new(),
            sent: [None; ACTIVE],
            megaed: [false; 2],
            ending: false,
            renumbered: Vec::new(),
            rounds: Vec::new(),
            roused: None,
            started: false,
            ended: false,
            winner: None,
        })
    }

    /// Says what each Pokémon of the team was named when it was registered.
    /// Needed only for a team with two of a species, which no ladder allows:
    /// otherwise the species says which is which.
    pub fn with_names(mut self, names: Vec<String>) -> Follower {
        self.nicknames = names;
        self
    }

    /// Says whether the other side's team can be counted on to hold each item
    /// once at most. On Showdown it can wherever the format has an item
    /// clause, which is what is assumed if nothing is said. (Battles set up
    /// by hand may break it.)
    pub fn trust_item_clause(&mut self, on: bool) {
        self.item_clause = Some(on);
    }

    /// From a battle's whole log, as the server keeps it, the lines this
    /// follower's player is sent: of each pair that has a private and a
    /// public version, the one meant for it. (A player's own connection
    /// delivers just these.)
    pub fn own_lines<S: AsRef<str>>(side: usize, log: &[S]) -> Vec<&str> {
        let mut out = Vec::new();
        let mut k = 0;
        while k < log.len() {
            let line = log[k].as_ref();
            if let Some(whose) = line.strip_prefix("|split|") {
                let mine = whose == ["p1", "p2"][side];
                if let Some(pick) = log.get(k + if mine { 1 } else { 2 }) {
                    out.push(pick.as_ref());
                }
                k += 3;
            } else {
                out.push(line);
                k += 1;
            }
        }
        out
    }

    pub fn side(&self) -> usize {
        self.side
    }

    pub fn turn(&self) -> u16 {
        self.turn
    }

    pub fn ended(&self) -> bool {
        self.ended
    }

    /// The side that won, once the battle has ended; `None` for a tie.
    pub fn winner(&self) -> Option<usize> {
        self.winner
    }

    /// What the last request asks for; `None` before the first, and when it asks nothing of this player.
    pub fn phase(&self) -> Option<Phase> {
        match self.req.as_ref()?.asked {
            Asked::Preview => Some(Phase::Preview),
            Asked::Move => Some(Phase::Move),
            Asked::Switch | Asked::Wait => Some(Phase::Switch),
        }
    }

    /// The battle as rebuilt at the last request: right in what the observation reads, and no further.
    pub fn battle(&self) -> Option<&Battle> {
        self.battle.as_ref()
    }

    fn present(&self, side: usize, pos: usize) -> bool {
        self.reader.sides[side].active[pos].as_ref().is_some_and(|l| !l.gone)
    }

    /// From when in this stretch the Pokémon at a position has had the move `held` in the
    /// queue, if it has been caught by an Encore before moving: the move an Encore holds a
    /// Pokémon to is put in the queue in place of the one it chose, as a fresh action.
    /// `None`: all along (no Encore; or its own Pokémon, which had chosen that very move).
    fn queued_since(&self, side: usize, pos: usize, held: u16) -> Option<usize> {
        let caught = self.spots[side][pos].overridden?;
        let chose = self.chosen.iter().find(|c| side == self.side && c.0 == self.own_at[pos]).map(|c| c.1);
        (chose != Some(held)).then_some(caught)
    }

    /// Whether the follower's own Pokémon at a position is in disguise: the log has it
    /// under another's name.
    fn own_disguised(&self, pos: usize) -> bool {
        let us = &self.reader.sides[self.side];
        match (us.active[pos].as_ref().filter(|l| !l.gone), self.own.get(self.own_at[pos])) {
            (Some(live), Some(own)) => us.team.get(live.entry).is_some_and(|e| e.name != own.name),
            _ => false,
        }
    }

    /// The base types this side takes the Pokémon at a position to have: what the log has
    /// said of them, or else its species' own. (The species it is, for one of its own; the
    /// one it looks like, for one of the other side's.)
    fn seen_base(&self, side: usize, pos: usize) -> [Type; 2] {
        if let Some(types) = self.spots[side][pos].types {
            return types;
        }
        let own = self.own.get(self.own_at[pos]).filter(|_| side == self.side && self.own_disguised(pos));
        let species = match own {
            Some(own) => Some(self.team[own.entry].species),
            None => self.spots[side][pos].forme.or(self.reader.sides[side].active[pos].as_ref().map(|l| l.species)),
        };
        species.map_or([Type::None; 2], |sp| SPECIES[sp as usize].types)
    }

    /// Whether the Pokémon at a position is known to hold a Choice item that is doing
    /// its work: not in a Magic Room, and not on a Pokémon that may have Klutz.
    fn choice_at_work(&self, side: usize, pos: usize) -> bool {
        if self.field.pseudo.iter().any(|p| p.0 == Pseudo::Magicroom) || self.spots[side][pos].no_hold {
            return false;
        }
        let (item, klutz) = if side == self.side {
            match self.own_now(pos) {
                Some(now) => (now.item, now.abilities.contains(&ab::KLUTZ)),
                None => return false,
            }
        } else {
            let s = self.seen(side, pos);
            let Some(listed) = self.reader.sides[side].roster.get(s.listed as usize).filter(|_| s.present) else {
                return false;
            };
            // An item it has been seen with is the item of whoever stands there. One that is
            // only on a team sheet is its own if it is who it looks like, and if the sheets
            // say which is which (two of a species on a team, which no ladder allows, and
            // they do not).
            let twice = self.reader.sides[side].roster.iter().filter(|l| l.species == listed.species).count() > 1;
            let item = match s.item {
                ItemShown::Holds(item) => item,
                ItemShown::Lost(_) => it::NONE,
                ItemShown::Unknown if self.open() && !twice && !s.doubt => listed.item,
                ItemShown::Unknown => return false,
            };
            (item, speed::may_have(&s, listed, self.open(), ab::KLUTZ))
        };
        ITEMS[item as usize].flags & IF_CHOICE != 0 && !klutz
    }

    /// A hidden target can still be determined from public redirection and
    /// the user's private powder immunities. This runs only in the client.
    fn redirected_pp_target(
        &self,
        pos: usize,
        used: u16,
        target: Target,
        aimed: (usize, usize),
    ) -> Option<(usize, usize)> {
        let b = self.rebuild().ok()?;
        let own = self.own_now(pos)?;
        own.agreed(|ability| {
            let mut b = b;
            let user = b.active(self.side, pos);
            (b.mon_mut(user).ability, b.mon_mut(user).item) = (ability, own.item);
            let mi = b.new_am(used);
            b.am[mi as usize].target = target;
            let at = b.active(aimed.0, aimed.1);
            Some(match b.priority_event(Ev::RedirectTarget, Some(user), Some(user), Eff::Move(mi), Res::Mon(at)) {
                Res::Mon(r) => {
                    (r.side as usize, (0..ACTIVE).find(|&p| b.active(r.side as usize, p) == r).unwrap_or(aimed.1))
                }
                _ => aimed,
            })
        })
    }

    /// The team index the rebuilt battle gives whoever stands at a position.
    fn mon_ref(&self, side: usize, pos: usize) -> MonRef {
        if side == self.side {
            return MonRef { side: side as u8, idx: self.own_at[pos] as u8 };
        }
        MonRef { side: side as u8, idx: self.their_index(pos) as u8 }
    }

    /// The other side's Pokémon are numbered in the order they first appeared.
    /// Two on the field can go by one name (an Illusion beside the Pokémon it
    /// copies); the second then gets a number past all the others.
    fn their_index(&self, pos: usize) -> usize {
        let opp = 1 - self.side;
        let s = &self.reader.sides[opp];
        let Some(here) = s.active[pos].as_ref() else {
            return MAX_TEAM - 1;
        };
        let copy = match s.active[1 - pos].as_ref() {
            Some(other) if other.entry == here.entry => {
                // A residual faint need not reveal Illusion. Of two sharing a name, the
                // copy is the one the record has its doubts about, if it doubts just one.
                // Otherwise it goes by who came first. Illusion copies the last healthy
                // Pokémon to its user's right, which as a rule is one in reserve: so the
                // copy was there first, and the Pokémon itself came in beside it. Only one
                // that came in on the left, with nobody left in reserve, can have copied the
                // Pokémon already standing on its right; then the later one is taken for it.
                let (mine, beside) = (&self.spots[opp][pos], &self.spots[opp][1 - pos]);
                let later = mine.stay > beside.stay;
                // (A Pokémon in disguise cannot Transform: one that has is the Pokémon itself.)
                if here.transformed != other.transformed {
                    other.transformed
                } else if here.rec.suspect != other.rec.suspect {
                    here.rec.suspect
                } else if if later { mine.last_two } else { beside.last_two } {
                    later
                } else {
                    !later
                }
            }
            _ => false,
        };
        if copy { s.team.len().min(MAX_TEAM - 1) } else { here.entry.min(MAX_TEAM - 1) }
    }

    // ------------------------------------------------------------ the log

    /// Takes in one line of the battle's log, as this player is sent it.
    pub fn line(&mut self, line: &str) -> Result<(), String> {
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() < 2 || !parts[0].is_empty() {
            return Ok(());
        }
        let kind = parts[1];
        let arg = |i: usize| parts.get(i).copied().unwrap_or("");
        let from = parts.iter().find_map(|p| p.strip_prefix("[from]")).map_or("", str::trim);
        // A queued move begins: note how things stand before the reader takes the line in.
        let who = ident(arg(2)).and_then(|id| id.pos.map(|pos| (id.side, pos)));
        if kind == "cant" && who.is_some_and(|(side, pos)| self.reader.borrowed_cant(side, pos, arg(3), arg(4))) {
            self.reader.line(line)?;
            return Ok(());
        }
        if let Some((side, pos)) = who {
            let begins = match kind {
                "move" => queued_move(arg(3), from),
                "cant" => !arg(3).starts_with("ability: "),
                _ => false,
            };
            if begins {
                // (The later turns of a move it is locked into say nothing the first did not.)
                let named = match kind {
                    "move" if from != "lockedmove" => move_id(&to_id(arg(3))),
                    "move" => None,
                    // A cant naming the attempted move gives its priority too,
                    // including a Focus Punch whose focus was lost after preparation.
                    _ => Some(arg(4)).filter(|m| !m.is_empty() && !m.starts_with('[')).and_then(|m| move_id(&to_id(m))),
                };
                // A sleeping Pokémon that goes on to use Sleep Talk has both lines for the one move.
                let named = named.unwrap_or(NO_MOVE);
                let stay = self.spots[side][pos].stay;
                // An instructed sleeping action may continue with Snore/Sleep
                // Talk. A subsequent wake instead starts the original queued
                // action, rather than completing the failed instruction.
                let instruction_continues = self.roused.is_none()
                    && matches!(named, mv::SNORE | mv::SLEEPTALK)
                    && named == self.spots[side][pos].last_move;
                match self.begun.last_mut() {
                    Some(last)
                        if last.asleep
                            && (!last.instructed || instruction_continues)
                            && last.stay == stay
                            && (kind == "move" || (kind == "cant" && named != NO_MOVE && arg(3) != "slp")) =>
                    {
                        if from != "move: Round"
                            && let Some(before) = last.round_before.take()
                        {
                            last.reordered = before;
                            self.spots[side][pos].reordered = before;
                        }
                        (last.named, last.asleep) = (named, false);
                    }
                    _ => {
                        self.begin(side, pos, named, (kind == "move").then_some(from == "move: Round"));
                        self.begun.last_mut().unwrap().asleep = kind == "cant" && arg(3) == "slp";
                    }
                }
            }
            if kind == "move" && to_id(arg(3)) == "round" && !arg(4).is_empty() && !parts.contains(&"[notarget]") {
                // Only an executed Round with a target reaches onTry and promotes
                // one next action. A cant or targetless Round stops the chain.
                self.rounds.clear();
                self.rounds.push(self.begun.len());
            }
        }
        // Confusion is checked after waking but before the move line. The
        // queue was sorted while asleep, including Quick Feet's Speed bonus.
        if !(kind == "-curestatus" && parts.contains(&"[msg]"))
            && !(matches!(kind, "-activate" | "-end") && arg(3) == "confusion")
            && !(kind == "-activate" && matches!(arg(3), "move: Struggle" | "move: Attract"))
        {
            self.roused = None;
        }
        let held = |me: &Follower| -> [Option<(ItemShown, u16, bool)>; 4] {
            std::array::from_fn(|k| {
                me.reader.live_record(k / 2, k % 2).map(|rec| (rec.item, rec.ability, rec.ability_changed))
            })
        };
        // The public Skill Swap line omits allied abilities. This player already
        // knows its own two: preserve their knowledge before the reader erases it.
        let own_swap = if kind == "-activate" && matches!(effect_id(arg(3)).as_str(), "skillswap" | "wanderingspirit") {
            let other = parts
                .iter()
                .find_map(|p| p.strip_prefix("[of]").and_then(|p| ident(p.trim())))
                .and_then(|id| id.pos.map(|pos| (id.side, pos)));
            match (who, other) {
                (Some((s, p)), Some((t, q))) if s == self.side && t == s && p != q => {
                    Some((p, q, self.own_now(p).map(|n| n.abilities), self.own_now(q).map(|n| n.abilities)))
                }
                _ => None,
            }
        } else {
            None
        };
        // Poison and burn tick after weather's residual event. A weather ability
        // inherited on the ensuing faint starts too late to age at this upkeep.
        if kind == "-damage" && matches!(from, "brn" | "psn" | "tox") {
            self.ending = true;
        }
        let before = held(self);
        // The other side's Pokémon as they are numbered now, each with its stay on the field.
        let opp = 1 - self.side;
        let were: [(u32, usize); ACTIVE] =
            std::array::from_fn(|pos| (self.spots[opp][pos].stay, self.their_index(pos)));
        self.reader.line(line)?;
        let after = held(self);
        self.track(kind, &parts, from)?;
        // One of them that is still where it stood may go by another number now. A disguise
        // dropped, and it is numbered anew, as the one beside it that it was passing for may
        // be. Or the Pokémon it passes for has come in beside it, and takes that number.
        // What was noted of either goes by the new numbers.
        // (And whether a Speed Swap has been used while each has stood there.)
        let moved: Vec<(usize, usize, bool)> = (0..ACTIVE)
            .filter(|&pos| self.spots[opp][pos].stay == were[pos].0)
            .map(|pos| (were[pos].1, self.their_index(pos), self.spots[opp][pos].swap_seen))
            .filter(|(was, now, _)| was != now)
            .collect();
        if !moved.is_empty() {
            let renumber = |r: &mut MonRef| {
                if r.side == opp as u8
                    && let Some(&(_, now, _)) = moved.iter().find(|m| m.0 == r.idx as usize)
                {
                    r.idx = now as u8;
                }
            };
            for begun in &mut self.begun {
                renumber(&mut begun.r);
                begun.seen.iter_mut().for_each(|seen| renumber(&mut seen.r));
            }
            for (_, event) in &mut self.items {
                if let Event::Switch { r } = event {
                    renumber(r);
                }
            }
            self.renumbered.push(moved);
        }
        if !matches!(kind, "switch" | "drag" | "replace" | "swap") {
            for k in 0..4 {
                let (Some(was), Some(is)) = (before[k], after[k]) else { continue };
                if was.0 != is.0 && matches!(is.0, ItemShown::Lost(_)) {
                    self.spots[k / 2][k % 2].lost_item = true;
                }
                if was.0 != is.0 {
                    // (A Choice item that goes takes its hold with it; one that comes starts afresh.)
                    self.spots[k / 2][k % 2].choice = None;
                }
                if (was.1, was.2) != (is.1, is.2) {
                    self.spots[k / 2][k % 2].ability_touched = true;
                    self.spots[k / 2][k % 2].own_abilities = None;
                }
            }
        }
        if let Some((p, q, first, second)) = own_swap {
            self.spots[self.side][p].own_abilities = second;
            self.spots[self.side][q].own_abilities = first;
        }
        let at = self.begun.len();
        for (side, listed, doubt, item, lost) in self.reader.item_notes.drain(..) {
            self.items.push((at, Event::Item { side: side as u8, listed, doubt, item, lost }));
        }
        Ok(())
    }

    /// Takes in a stretch of log.
    pub fn lines<S: AsRef<str>>(&mut self, lines: &[S]) -> Result<(), String> {
        for line in lines {
            self.line(line.as_ref()).map_err(|e| format!("{e} in {:?}", line.as_ref()))?;
        }
        Ok(())
    }

    /// How a position looks to everyone, for working out who was faster.
    fn seen(&self, side: usize, pos: usize) -> Seen {
        let r = self.mon_ref(side, pos);
        let mut s = Seen::nobody(r);
        let Some(live) = self.reader.sides[side].active[pos].as_ref().filter(|l| !l.gone) else {
            return s;
        };
        let spot = &self.spots[side][pos];
        s.present = true;
        s.listed = live.rec.listed;
        s.doubt = live.rec.suspect || live.transformed;
        s.species = live.species;
        s.stage = spot.boosts[SPE];
        s.status = live.rec.status;
        s.hp_full = live.rec.hp == 100;
        s.item = live.rec.item;
        s.ability = live.rec.ability;
        s.ability_changed = live.rec.ability_changed;
        s.gastro_acid = spot.has(VolKind::Gastroacid);
        s
    }

    fn field_seen(&self) -> FieldSeen {
        // Cloud Nine and Air Lock say so when they come in.
        let calm = (0..2).any(|side| {
            (0..ACTIVE).any(|pos| {
                let known = self.reader.live_record(side, pos).map_or(UNKNOWN, |rec| rec.ability);
                matches!(known, ab::CLOUDNINE | ab::AIRLOCK) && !self.spots[side][pos].has(VolKind::Gastroacid)
            })
        });
        let has = |side: usize, kind: SideCond| self.field.sides[side].conds.iter().any(|c| c.0 == kind);
        FieldSeen {
            trick_room: self.field.pseudo.iter().any(|p| p.0 == Pseudo::Trickroom),
            magic_room: self.field.pseudo.iter().any(|p| p.0 == Pseudo::Magicroom),
            weather: if calm { Weather::None } else { self.field.weather },
            terrain: self.field.terrain,
            tailwind: [has(0, SideCond::Tailwind), has(1, SideCond::Tailwind)],
        }
    }

    /// What the follower knows of its own Pokémon at `pos` that settles how fast it moves.
    fn own_now(&self, pos: usize) -> Option<OwnNow> {
        let req = self.req.as_ref()?;
        let own = self.own.get(self.own_at[pos])?;
        let mon = req.mons.iter().find(|m| m.name == own.name)?;
        let live = self.reader.sides[self.side].active[pos].as_ref().filter(|l| !l.gone)?;
        let mut seen = self.seen(self.side, pos);
        // (Whether the other side doubts who it is makes no difference to its Speed.)
        seen.doubt = live.transformed;
        let set = &self.team[own.entry];
        // The species it is now: as the request had it, or the forme it has taken since.
        let species = self.spots[self.side][pos].forme.filter(|_| !is_mega(mon.species)).unwrap_or(mon.species);
        let rec = &live.rec;
        // The private request identifies its actual item, even when an Illusion alias's
        // public record belongs to a different Pokémon. Only changes since that request
        // can override it; the position's item_since is reset on entry and on requests.
        let item = self.spots[self.side][pos].item_since.unwrap_or(mon.item);
        let stat = calc_stats(species, set.nature, set.stat_points)[5];
        let mut abilities = Vec::new();
        if let Some(known) = &self.spots[self.side][pos].own_abilities {
            abilities.clone_from(known);
        } else if !self.spots[self.side][pos].ability_touched {
            // Nothing has been said of it since the request, which has it exactly. (For one
            // that has come in since, that is the ability it came in with.)
            abilities.push(mon.ability);
        } else if rec.ability != UNKNOWN {
            abilities.push(rec.ability);
        } else {
            // Its ability has been traded for its partner's and the log does not say which
            // is which now: either, then, which is good enough where both come to the same.
            for p in 0..ACTIVE {
                let partner = self.own.get(self.own_at[p])?;
                abilities.push(req.mons.iter().find(|m| m.name == partner.name)?.ability);
            }
        }
        // Opposing grounding can be hidden. Own grounding is exact unless an allied
        // ability swap left several possibilities that disagree about it.
        let grounded = if self.field.terrain == Terrain::Grassyterrain {
            self.rebuild().ok().and_then(|b| {
                let values: Vec<_> = abilities
                    .iter()
                    .map(|&ability| {
                        let mut b = b;
                        let r = b.active(self.side, pos);
                        (b.mon_mut(r).ability, b.mon_mut(r).item) = (ability, item);
                        b.is_grounded(r, false)
                    })
                    .collect();
                values.first().copied().filter(|g| values.iter().all(|v| v == g))
            })
        } else {
            None
        };
        Some(OwnNow { seen, abilities, item, grounded, lost_item: self.spots[self.side][pos].lost_item, stat })
    }

    /// A queued move of whoever is at a position begins.
    fn begin(&mut self, side: usize, pos: usize, named: u16, round_called: Option<bool>) {
        let at = self.begun.len();
        let field = self.field_seen();
        let mut seen = [self.seen(0, 0), self.seen(0, 1), self.seen(1, 0), self.seen(1, 1)];
        if let Some((s, p, status)) = self.roused
            && (s, p) == (side, pos)
        {
            // It woke up to move: the queue was sorted while it slept.
            seen[2 * side + pos].status = status;
        }
        let roused = self.roused.filter(|&(s, p, _)| (s, p) == (side, pos) && s == self.side);
        let own = std::array::from_fn(|p| {
            let mut now = self.own_now(p);
            if let (Some((_, _, status)), Some(now)) = (roused.filter(|r| r.1 == p), now.as_mut()) {
                now.seen.status = status;
            }
            (self.spots[self.side][p].stay, now)
        });
        let mut round_before = None;
        if round_called == Some(true) || (round_called.is_none() && matches!(named, mv::ROUND | NO_MOVE)) {
            // A Round calls the next one waiting in the queue forward: this one, if a Round
            // has been used since this was the move it had queued.
            let from = self.queued_since(side, pos, named);
            let spot = &mut self.spots[side][pos];
            let called = self.rounds.iter().copied().find(|&since| from.is_none_or(|t| since > t));
            if let Some(since) = round_called.filter(|&called| called).map(|_| at).or(called) {
                if round_called.is_none() && named == NO_MOVE {
                    round_before = Some(spot.reordered);
                }
                spot.reordered = Some(spot.reordered.map_or(since, |t| t.min(since)));
            }
        }
        // Promotion is consumed by that one action, even when it cannot move.
        self.rounds.clear();
        if named == mv::SPEEDSWAP {
            for (s, p) in (0..2).flat_map(|s| (0..ACTIVE).map(move |p| (s, p))) {
                self.spots[s][p].swap_seen |= self.present(s, p);
            }
        }
        let spot = &mut self.spots[side][pos];
        self.begun.push(Begun {
            side,
            stay: spot.stay,
            r: MonRef { side: 0, idx: 0 },
            named,
            again: spot.begun > 0,
            instructed: spot.instructed,
            asleep: false,
            reordered: spot.reordered,
            round_before,
            overridden: spot.overridden,
            encored: spot.has(VolKind::Encore),
            quick: spot.quick,
            seen,
            field,
            own,
        });
        if spot.instructed {
            spot.instructed = false;
        } else {
            spot.begun += 1;
        }
        self.begun[at].r = self.mon_ref(side, pos);
    }

    fn spot(&mut self, side: usize, pos: usize) -> &mut Spot {
        &mut self.spots[side][pos]
    }

    fn identity(&self, side: usize, pos: usize) -> (usize, usize) {
        let entry = if side == self.side {
            self.own_at[pos]
        } else {
            self.reader.sides[side].active[pos].as_ref().map_or(usize::MAX, |live| live.entry)
        };
        (side, entry)
    }

    /// A Pokémon comes in at a position.
    fn enter(&mut self, side: usize, pos: usize, name: &str, health: &str, from: &str) {
        self.stays += 1;
        let mut fresh = Spot::new(self.stays);
        let team = &self.reader.sides[side];
        let brought = if team.size == 0 { 4 } else { team.size };
        fresh.last_two = pos == 0 && brought.saturating_sub(team.fainted) <= 2;
        let old = &self.spots[side][pos];
        if from.contains("Baton Pass") {
            fresh.boosts = old.boosts;
            fresh.vols = old.vols.iter().copied().filter(|v| !v.kind.data().no_copy).collect();
        } else if from.contains("Shed Tail") {
            fresh.vols = old.vols.iter().copied().filter(|v| v.kind == VolKind::Substitute).collect();
        }
        let left = old.stay;
        self.spots[side][pos] = fresh;
        // What the one that left was keeping up on others goes with it.
        for spot in self.spots.iter_mut().flatten() {
            spot.vols.retain(|v| v.source == 0 || v.source != left);
        }
        if side == self.side {
            // Which of its own this is: the one of that name, unless it is one with Illusion
            // under another's name. Its HP, which its own side is shown exactly, tells them
            // apart unless the two have the same; so does the follower's own reply, if that
            // is what sent it in; and so does the one named standing beside it already.
            let named = self.own.iter().position(|o| o.name == name);
            let beside = self.present(side, 1 - pos).then_some(self.own_at[1 - pos]);
            let health = health.split(' ').next().and_then(|h| h.split_once('/'));
            let health = health.and_then(|(hp, max)| Some((hp.parse::<u16>().ok()?, max.parse::<u16>().ok()?)));
            let fits = |idx: usize| {
                // (As the last request had it: nothing changes on the bench.)
                let mon = self.req.as_ref().and_then(|r| r.mons.iter().find(|m| m.name == self.own[idx].name));
                match (mon, health) {
                    (Some(mon), Some(health)) => !mon.fainted && !mon.active && (mon.hp, mon.max_hp) == health,
                    _ => true,
                }
            };
            let disguise = |idx: usize| self.team[self.own[idx].entry].ability == ab::ILLUSION && Some(idx) != named;
            let could: Vec<usize> = (0..self.own.len())
                .filter(|&idx| (Some(idx) == named || disguise(idx)) && Some(idx) != beside && fits(idx))
                .collect();
            let sent = self.sent[pos].take().filter(|idx| could.contains(idx));
            let only = Some(could.as_slice()).filter(|c| c.len() == 1).map(|c| c[0]);
            if let Some(idx) = only.or(sent).or(named) {
                self.own_at[pos] = idx;
            }
        }
        self.items.push((self.begun.len(), Event::Switch { r: self.mon_ref(side, pos) }));
    }

    /// Where the Pokémon a line names stands: its side and position.
    fn place_of(&self, who: &str) -> Option<(usize, usize)> {
        let o = ident(who)?;
        // (A Pokémon that has fainted where it stood is named without its position.)
        let fallen = |p: &usize| {
            let live = self.reader.sides[o.side].active[*p].as_ref();
            live.is_some_and(|l| l.gone && self.reader.sides[o.side].team[l.entry].name == o.name)
        };
        o.pos.or((0..ACTIVE).find(fallen)).map(|p| (o.side, p))
    }

    fn side_of(who: &str) -> Option<usize> {
        match who.get(..2)? {
            "p1" => Some(0),
            "p2" => Some(1),
            _ => None,
        }
    }

    fn track(&mut self, kind: &str, parts: &[&str], from: &str) -> Result<(), String> {
        let arg = |i: usize| parts.get(i).copied().unwrap_or("");
        let refusable = self.refusable.take();
        if let Some(r) = refusable.as_ref().filter(|_| kind != "-fail") {
            // (The move thought to be held back did not fail: see `no_hold`.)
            let spot = &mut self.spots[r.side][r.pos];
            (spot.choice, spot.no_hold) = (None, true);
        }
        match kind {
            "player" => {
                if let Some(side) = Follower::side_of(arg(2))
                    && !arg(3).is_empty()
                {
                    self.names[side] = arg(3).to_string();
                }
                return Ok(());
            }
            "start" => {
                self.started = true;
                return Ok(());
            }
            "turn" => {
                self.turn = arg(2).parse().map_err(|_| "unreadable turn".to_string())?;
                for side in 0..2 {
                    for pos in 0..ACTIVE {
                        let present = self.present(side, pos);
                        let spot = &mut self.spots[side][pos];
                        if present {
                            spot.active_turns += 1;
                        }
                        (spot.begun, spot.reordered, spot.overridden, spot.quick) = (0, None, None, false);
                        // Showdown says at the end of a turn what a Pokémon's types have
                        // become, if they have changed: one of the follower's own that took
                        // a foe's types and has heard nothing has the ones it had.
                        let unchanged = side == self.side && spot.copied_foe && !spot.told;
                        (spot.copied, spot.copied_foe, spot.told) = (false, false, false);
                        if present && unchanged && !self.own_disguised(pos) {
                            let spot = &mut self.spots[side][pos];
                            spot.types = Some(spot.before);
                        }
                    }
                }
                // Who hit whom is kept for as long as the one that hit is on the field.
                let here: Vec<(usize, usize)> = (0..2)
                    .flat_map(|s| (0..ACTIVE).map(move |p| (s, p)))
                    .filter(|&(s, p)| self.present(s, p))
                    .map(|(s, p)| self.identity(s, p))
                    .collect();
                for spot in self.spots.iter_mut().flatten() {
                    spot.hit_by.retain(|(stay, _)| here.contains(stay));
                }
                // (A Revival Blessing nobody was brought back with is over with its turn.)
                for slots in self.field.sides.iter_mut().flat_map(|s| s.slots.iter_mut()) {
                    slots.retain(|c| c.0 != SlotCond::Revivalblessing);
                }
                self.rounds.clear();
                self.mover = None;
                return Ok(());
            }
            "upkeep" => {
                self.upkeep();
                return Ok(());
            }
            "win" => {
                self.ended = true;
                self.winner = self.names.iter().position(|n| n == arg(2));
                return Ok(());
            }
            "tie" => {
                self.ended = true;
                return Ok(());
            }
            "-weather" => {
                let id = to_id(arg(2));
                if id == "none" {
                    self.field.weather = Weather::None;
                    self.ending = true;
                } else if parts.contains(&"[upkeep]") {
                    self.ending = true;
                } else {
                    let weather = Weather::named(&id).or(match id.as_str() {
                        "snow" | "hail" => Some(Weather::Snowscape),
                        _ => None,
                    });
                    if let Some(weather) = weather
                        && weather != self.field.weather
                    {
                        (self.field.weather, self.field.weather_for) = (weather, 0);
                        self.field.weather_late = self.ending;
                    }
                }
                return Ok(());
            }
            "-fieldstart" | "-fieldend" => {
                let id = effect_id(arg(2));
                let start = kind == "-fieldstart";
                if let Some(terrain) = Terrain::named(&id) {
                    if start {
                        (self.field.terrain, self.field.terrain_for) = (terrain, 0);
                        self.field.terrain_late = self.ending;
                    } else if self.field.terrain == terrain {
                        self.field.terrain = Terrain::None;
                    }
                } else if let Some(pseudo) = Pseudo::named(&id) {
                    self.field.pseudo.retain(|p| p.0 != pseudo);
                    if start {
                        self.field.pseudo.push((pseudo, pseudo.data().duration));
                    }
                }
                return Ok(());
            }
            "-fieldactivate" => {
                if effect_id(arg(2)) == "fairylock" {
                    self.field.pseudo.retain(|p| p.0 != Pseudo::Fairylock);
                    self.field.pseudo.push((Pseudo::Fairylock, Pseudo::Fairylock.data().duration));
                }
                return Ok(());
            }
            "-sidestart" | "-sideend" => {
                let (Some(side), Some(cond)) = (Follower::side_of(arg(2)), SideCond::named(&effect_id(arg(3)))) else {
                    return Ok(());
                };
                let conds = &mut self.field.sides[side].conds;
                if kind == "-sideend" {
                    conds.retain(|c| c.0 != cond);
                } else if let Some(c) = conds.iter_mut().find(|c| c.0 == cond) {
                    c.1 += 1;
                } else {
                    conds.push((cond, 1, 0));
                }
                return Ok(());
            }
            "-swapsideconditions" => {
                let [a, b] = &mut self.field.sides;
                std::mem::swap(&mut a.conds, &mut b.conds);
                return Ok(());
            }
            "-clearallboost" => {
                self.spots.iter_mut().flatten().for_each(|s| s.boosts = [0; 7]);
                return Ok(());
            }
            _ => {}
        }
        let Some(id) = ident(arg(2)) else {
            return Ok(());
        };
        let side = id.side;
        if kind == "-heal" && effect_id(from) == "revivalblessing" {
            // The Pokémon picked has been revived: nobody is choosing any more.
            self.field.sides[side].slots.iter_mut().for_each(|s| s.retain(|c| c.0 != SlotCond::Revivalblessing));
        }
        let Some(pos) = id.pos else {
            return Ok(());
        };
        let (third, fourth) = (self.place_of(arg(3)), self.place_of(arg(4)));
        let of =
            parts.iter().find_map(|p| p.strip_prefix("[of] ")).and_then(ident).and_then(|o| o.pos.map(|p| (o.side, p)));
        match kind {
            "switch" | "drag" => self.enter(side, pos, id.name, arg(4), from),
            "replace" => {
                // A disguise is broken: one of its own is known by its name again.
                if side == self.side
                    && let Some(idx) = self.own.iter().position(|o| o.name == id.name)
                {
                    self.own_at[pos] = idx;
                }
            }
            "swap" => {
                if let Ok(to) = arg(3).parse::<usize>()
                    && to < ACTIVE
                    && to != pos
                {
                    // (What is set on a position, a Wish on its way, stays where it was set.)
                    self.spots[side].swap(pos, to);
                    if side == self.side {
                        self.own_at.swap(pos, to);
                    }
                }
            }
            "-mega" => {
                self.megaed[side] = true;
                if side == self.side {
                    // Its own Mega, whatever disguise the log shows it under.
                    let species = self.own.get(self.own_at[pos]).map(|own| self.team[own.entry].species);
                    let stone = item_id(&to_id(arg(4)));
                    let mega = species.zip(stone).and_then(|(species, stone)| {
                        ITEMS[stone as usize].mega.iter().find(|&&(from, _)| from == species).map(|&(_, to)| to)
                    });
                    if mega.is_some() {
                        self.spot(side, pos).forme = mega;
                    }
                }
            }
            "faint" => {
                let left = self.spots[side][pos].stay;
                for spot in self.spots.iter_mut().flatten() {
                    spot.vols.retain(|v| v.source == 0 || v.source != left);
                }
            }
            "detailschange" | "-formechange" => {
                // A new forme has its own types.
                let forme = species_id(&to_id(arg(3).split(',').next().unwrap_or("")));
                let spot = self.spot(side, pos);
                (spot.types, spot.added) = (None, Type::None);
                if kind == "-formechange" && forme.is_some() {
                    spot.forme = forme;
                }
            }
            "move" => {
                let Some(used) = move_id(&to_id(arg(3))) else {
                    return Ok(());
                };
                let queued = queued_move(arg(3), from);
                let stay = self.spots[side][pos].stay;
                let mut undo = None;
                // A Choice item holds its Pokémon to one move. It can come to choose another
                // all the same (the item was switched off by a Magic Room when it chose, and is
                // not now): Showdown then shows the move and that it failed, with nothing to
                // tell it from any other that failed, but the move never began. It is not the
                // move the Pokémon last used, and it cost no PP.
                let held_to = self.spots[side][pos].choice;
                let at_work = queued && used != mv::STRUGGLE && self.choice_at_work(side, pos);
                let doubtful = at_work && parts.contains(&"[still]") && held_to.is_some_and(|held| held != used);
                let before = self.spots[side][pos].last_move;
                let before_loc = self.spots[side][pos].last_move_loc;
                let instructed = self.begun.last().is_some_and(|b| b.stay == stay && b.instructed);
                let selected_loc = if instructed {
                    before_loc
                } else {
                    self.chosen.iter().find(|c| c.0 == self.own_at[pos]).map(|c| c.2)
                };
                let replaced = queued && self.queued_since(side, pos, used).is_some();
                let mut paid = None;
                if at_work && !doubtful && held_to.is_some_and(|held| held != used) {
                    // It went ahead with a move it was thought to be held from: the item is
                    // not doing its work after all (a Klutz not yet shown, say).
                    let spot = self.spot(side, pos);
                    (spot.choice, spot.no_hold) = (None, true);
                }
                if at_work && held_to.is_none() {
                    self.spot(side, pos).choice = Some(used);
                }
                if queued {
                    let mine = side == self.side;
                    let spot = self.spot(side, pos);
                    spot.last_move = used;
                    if mine {
                        spot.last_move_loc = if replaced { Some(0) } else { selected_loc };
                    }
                    spot.last_attempted_move = Some(used);
                    // (A charging move's own condition goes as it strikes; that it is a
                    // two-turn move stays on the books until the turn is over.)
                    spot.vols.retain(|v| {
                        v.ends != Ends::Move && v.kind != VolKind::Mustrecharge && !CHARGING.contains(&v.kind)
                    });
                }
                // Which of the user's moves pays for this one: itself if it was chosen, and the
                // move that called it (Copycat, Sleep Talk) for what Pressure takes on top.
                let caller = from.strip_prefix("move: ").and_then(|c| move_id(&to_id(c)));
                let payer = match (queued, caller) {
                    (true, _) if from != "lockedmove" => Some((used, 1)),
                    (false, Some(c)) => Some((c, 0)),
                    _ => None,
                };
                if let Some((paying, base)) = payer
                    && side == self.side
                {
                    // One PP, in case it leaves the field before the next request says so;
                    // two if it is aimed at a Pokémon with Pressure, which says so on coming in.
                    let mut aimed: Vec<(usize, usize)> = Vec::new();
                    let data = &MOVES[used as usize];
                    // Ghost Curse rerolls an allied/missing selected target to a random foe.
                    // A failed execution can omit that foe from the public move line.
                    let curse = used == mv::CURSE;
                    let ghost = curse
                        && (self.seen_base(side, pos).contains(&Type::Ghost)
                            || self.spots[side][pos].added == Type::Ghost);
                    let chosen_loc = selected_loc;
                    let target = if curse && !ghost {
                        Target::User
                    } else if ghost && (!queued || replaced || chosen_loc.is_none_or(|loc| loc <= 0)) {
                        Target::RandomNormal
                    } else {
                        data.target
                    };
                    let expanding = used == mv::EXPANDINGFORCE
                        && self.field.terrain == Terrain::Psychicterrain
                        && self.rebuild().is_ok_and(|mut b| {
                            let user = b.active(side, pos);
                            b.is_grounded(user, false)
                        });
                    let wide = data.flags & F_MUSTPRESSURE != 0
                        || expanding
                        || matches!(target, Target::All | Target::AllAdjacent | Target::AllAdjacentFoes);
                    if wide {
                        // A move on the whole field, on everyone across it, or one of the few
                        // (Imprison, Spikes) that pay to every opponent: whether or not it lands.
                        aimed.extend([(1 - side, 0), (1 - side, 1)]);
                    } else if target == Target::FoeSide || parts.contains(&"[notarget]") {
                        // (Any other move laid on the far side of the field pays nothing extra.
                        // Nor does one left with nobody to aim at, whoever the line goes on to name.)
                    } else if let Some(target) = fourth {
                        aimed.push(target);
                        // Dragon Darts goes for both of the other side while two are standing,
                        // and pays to both; the line names only one of them (the partner of the
                        // one it was aimed at).
                        let partner = (target.0, 1 - target.1);
                        if data.smart_target && target.0 != side && self.present(partner.0, partner.1) {
                            aimed.push(partner);
                        }
                    } else if queued
                        && !replaced
                        && matches!(target, Target::Normal | Target::Any | Target::AdjacentFoe)
                        && let Some(loc) = chosen_loc
                        && loc != 0
                    {
                        if loc < 0 {
                            // Follow Me/Rage Powder can redirect an attack
                            // originally selected against the user's ally too.
                            aimed.push((side, (-loc - 1) as usize));
                        } else {
                            // With nobody left where it was aimed, it goes for the other one.
                            let there = loc as usize - 1;
                            let other = !self.present(1 - side, there) && self.present(1 - side, 1 - there);
                            aimed.push((1 - side, if other { 1 - there } else { there }));
                        }
                    }
                    if fourth.is_none()
                        && aimed.len() == 1
                        && !data.tracks_target
                        && matches!(target, Target::Normal | Target::Any | Target::AdjacentFoe | Target::RandomNormal)
                        && self.spots[1 - side].iter().any(|s| s.has(VolKind::Followme) || s.has(VolKind::Ragepowder))
                        && let Some(redirected) = self.redirected_pp_target(pos, used, target, aimed[0])
                    {
                        aimed[0] = redirected;
                    }
                    let presses = |&(s, p): &(usize, usize)| {
                        s != side
                            && self.present(s, p)
                            && self.spots[s][p].pressing
                            && !self.spots[s][p].has(VolKind::Gastroacid)
                            && self.reader.live_record(s, p).is_some_and(|rec| rec.ability == ab::PRESSURE)
                    };
                    // A move for one target that its user did not pick one for: one that answers
                    // whoever hit it, one that goes for anybody, one called by another move.
                    let unaimed = match target {
                        Target::Scripted | Target::RandomNormal => true,
                        Target::Normal | Target::Any | Target::AdjacentFoe => {
                            !queued || replaced || (instructed && chosen_loc.is_none_or(|loc| loc == 0))
                        }
                        _ => false,
                    };
                    let mut uncertain = false;
                    if aimed.is_empty() && unaimed && parts.contains(&"[still]") {
                        // It did nothing to be seen, and the line names nobody. Metal Burst and
                        // Comeuppance were still aimed where the last blow came from, if someone
                        // stands there. Otherwise the game picked one of the other side at
                        // random and does not say which: counted only where it makes no
                        // difference.
                        let foes: Vec<(usize, usize)> =
                            (0..ACTIVE).map(|p| (1 - side, p)).filter(|&(s, p)| self.present(s, p)).collect();
                        let back = self.spots[side][pos].hit_by.last().map(|&(_, p)| (1 - side, p));
                        match back.filter(|at| matches!(used, mv::METALBURST | mv::COMEUPPANCE) && foes.contains(at)) {
                            Some(at) => aimed.push(at),
                            None if !foes.is_empty() && foes.iter().all(presses) => aimed.push(foes[0]),
                            None => uncertain = foes.iter().any(presses),
                        }
                    }
                    let pressed = aimed.iter().filter(|at| presses(at)).count() as u8;
                    let idx = self.own_at[pos];
                    let copied = self.reader.sides[side].active[pos].as_ref().is_some_and(|l| l.transformed);
                    if let Some(own) = self.own.get_mut(idx) {
                        if copied && !own.copied.iter().any(|slot| slot.0 == paying) {
                            let pp = MOVES[paying as usize].base_pp.min(5);
                            own.copied.push((paying, pp, pp));
                        }
                        let was_unknown = own.unknown_pp[copied as usize].contains(&paying);
                        if uncertain && !was_unknown {
                            own.unknown_pp[copied as usize].push(paying);
                        }
                        let known = if copied { &mut own.copied } else { &mut own.pp };
                        if let Some(slot) = known.iter_mut().find(|s| s.0 == paying) {
                            let taken = (base + pressed).min(slot.1);
                            slot.1 -= taken;
                            paid = Some((idx, copied, paying, taken, was_unknown));
                        }
                    }
                }
                if doubtful {
                    self.refusable = Some(Refusal { side, pos, last_move: before, last_move_loc: before_loc, paid });
                }
                match used {
                    mv::WIDEGUARD | mv::QUICKGUARD => {
                        // Over a side already covered it says nothing, and still counts
                        // towards the user's run of protecting moves.
                        let cond = if used == mv::WIDEGUARD { SideCond::Wideguard } else { SideCond::Quickguard };
                        if self.field.sides[side].conds.iter().any(|c| c.0 == cond) {
                            let spot = self.spot(side, pos);
                            undo = Some(Undo::Stall(spot.vols.iter().find(|v| v.kind == VolKind::Stall).copied()));
                            spot.stalled();
                        }
                    }
                    mv::MINIMIZE => {
                        if self.spot(side, pos).add(VolKind::Minimize, Ends::Said) {
                            undo = Some(Undo::Vol(VolKind::Minimize));
                        }
                    }
                    mv::WISH | mv::HEALINGWISH | mv::REVIVALBLESSING => {
                        let (cond, turns) = match used {
                            mv::WISH => (SlotCond::Wish, 2),
                            mv::HEALINGWISH => (SlotCond::Healingwish, 0),
                            _ => (SlotCond::Revivalblessing, 0),
                        };
                        let slots = &mut self.field.sides[side].slots[pos];
                        if !slots.iter().any(|s| s.0 == cond) {
                            slots.push((cond, turns));
                            undo = Some(Undo::Slot(side, pos, cond));
                        }
                    }
                    _ if MOVES[used as usize].stalling_move
                        && MOVES[used as usize].volatile.is_some_and(|up| self.spots[side][pos].has(up)) =>
                    {
                        // Used again in a turn its protection is already up (Instruct): if it
                        // works, nothing more is said, and the run is one longer.
                        let spot = self.spot(side, pos);
                        undo = Some(Undo::Stall(spot.vols.iter().find(|v| v.kind == VolKind::Stall).copied()));
                        spot.stalled();
                    }
                    _ => {}
                }
                let stalling = MOVES[used as usize].stalling_move;
                let shield = MOVES[used as usize].volatile.filter(|_| stalling);
                self.mover = Some(Mover { side, pos, stay, target: fourth, undo, stalling, shield });
            }
            "cant" => {
                if arg(3) == "move: Imprison"
                    && side == self.side
                    && !self.open()
                    && let Some(sealed) = move_id(&to_id(arg(4)))
                {
                    // One of its own was stopped by an Imprison: whoever has that up knows
                    // the move. (If just one of them has, and its moves are its own.)
                    let opp = 1 - side;
                    let up: Vec<usize> = (0..ACTIVE)
                        .filter(|&p| self.present(opp, p) && self.spots[opp][p].has(VolKind::Imprison))
                        .collect();
                    let entry =
                        match (up.as_slice(), self.reader.sides[opp].active.get(up.first().copied().unwrap_or(0))) {
                            (&[_], Some(Some(live))) if !live.transformed && !live.rec.suspect && !live.rec.tainted => {
                                Some(live.rec.listed as usize)
                            }
                            _ => None,
                        };
                    if let Some(listed) = entry.and_then(|e| self.reader.sides[opp].roster.get_mut(e)) {
                        listed.tell(sealed);
                    }
                }
                if !arg(3).starts_with("ability: ") {
                    // (Out of PP for Destiny Bond itself: the one already up is left standing.)
                    let again = arg(3) == "nopp" && to_id(arg(4)) == "destinybond";
                    self.spot(side, pos).vols.retain(|v| {
                        (v.ends != Ends::Move || (again && v.kind == VolKind::Destinybond))
                            && !matches!(v.kind, VolKind::Twoturnmove | VolKind::Mustrecharge)
                            && !CHARGING.contains(&v.kind)
                    });
                }
            }
            "-fail" => {
                // Whether anybody else has yet to move this turn: not one that has moved, or
                // that came in during it.
                let to_act = (0..2).flat_map(|s| (0..ACTIVE).map(move |p| (s, p))).any(|(s, p)| {
                    (s, p) != (side, pos)
                        && self.present(s, p)
                        && self.spots[s][p].begun == 0
                        && self.spots[s][p].active_turns > 0
                });
                let refused = refusable.filter(|r| (r.side, r.pos) == (side, pos));
                if let Some(mover) = &mut self.mover
                    && (mover.side, mover.pos) == (side, pos)
                {
                    match mover.undo.take() {
                        Some(Undo::Vol(kind)) => self.spots[side][pos].remove(kind),
                        Some(Undo::Slot(s, p, cond)) => self.field.sides[s].slots[p].retain(|c| c.0 != cond),
                        Some(Undo::Stall(before)) => {
                            let spot = &mut self.spots[side][pos];
                            spot.remove(VolKind::Stall);
                            spot.vols.extend(before);
                        }
                        None => {}
                    }
                    if mover.stalling && to_act && refused.is_none() {
                        // A protecting move that fails ends the run. (Not one that fails for
                        // coming last in the turn: there was nothing to protect from, the odds
                        // were never put to the test, and the run stands.)
                        self.spots[side][pos].remove(VolKind::Stall);
                    }
                }
                if let Some(refused) = refused {
                    // Held to another move by its Choice item: this one never began.
                    self.spot(side, pos).last_move = refused.last_move;
                    self.spot(side, pos).last_move_loc = refused.last_move_loc;
                    if let Some((idx, copied, paying, taken, was_unknown)) = refused.paid
                        && let Some(own) = self.own.get_mut(idx)
                    {
                        if !was_unknown {
                            own.unknown_pp[copied as usize].retain(|&id| id != paying);
                        }
                        let known = if copied { &mut own.copied } else { &mut own.pp };
                        if let Some(slot) = known.iter_mut().find(|s| s.0 == paying) {
                            slot.1 += taken;
                        }
                    }
                }
            }
            "-boost" | "-unboost" | "-setboost" => {
                if let (Some(stat), Ok(n)) = (stat_index(arg(3)), arg(4).parse::<i8>()) {
                    let b = &mut self.spot(side, pos).boosts[stat];
                    *b = match kind {
                        "-boost" => (*b + n).min(6),
                        "-unboost" => (*b - n).max(-6),
                        _ => n,
                    };
                }
            }
            "-clearboost" => self.spot(side, pos).boosts = [0; 7],
            "-clearpositiveboost" => self.spot(side, pos).boosts.iter_mut().for_each(|b| *b = (*b).min(0)),
            "-clearnegativeboost" => self.spot(side, pos).boosts.iter_mut().for_each(|b| *b = (*b).max(0)),
            "-invertboost" => self.spot(side, pos).boosts.iter_mut().for_each(|b| *b = -*b),
            "-copyboost" => {
                if let Some((s, p)) = third {
                    self.spots[side][pos].boosts = self.spots[s][p].boosts;
                    if from.ends_with("Psych Up") {
                        // What it had done for its critical hits goes too, for whatever the other has.
                        for kind in [VolKind::Focusenergy, VolKind::Dragoncheer] {
                            self.spots[side][pos].remove(kind);
                            if self.spots[s][p].has(kind) {
                                self.spots[side][pos].add(kind, Ends::Said);
                            }
                        }
                    }
                }
            }
            "-swapboost" => {
                if let Some((s, p)) = third {
                    let stats: Vec<usize> = match arg(4) {
                        "" => (0..7).collect(),
                        list if list.starts_with('[') => (0..7).collect(),
                        list => list.split(", ").filter_map(stat_index).collect(),
                    };
                    for k in stats {
                        let (a, b) = (self.spots[side][pos].boosts[k], self.spots[s][p].boosts[k]);
                        (self.spots[side][pos].boosts[k], self.spots[s][p].boosts[k]) = (b, a);
                    }
                }
            }
            "-transform" => {
                if let Some((s, p)) = third {
                    let theirs = self.spots[s][p].clone();
                    if side == self.side && !self.own.is_empty() {
                        // Every successful Transform creates new five-PP slots, even if
                        // it left and reentered without an intervening private request.
                        let own = &mut self.own[self.own_at[pos]];
                        own.copied.clear();
                        own.unknown_pp[1].clear();
                    }
                    let species = self.reader.sides[s].active[p].as_ref().map(|l| l.species);
                    let spot = self.spot(side, pos);
                    spot.choice = None;
                    spot.boosts = theirs.boosts;
                    spot.types = theirs.types.or(species.map(|sp| SPECIES[sp as usize].types));
                    spot.added = theirs.added;
                    spot.copied = true;
                    // Transform replaces these volatiles without separate end lines.
                    for kind in [VolKind::Dragoncheer, VolKind::Focusenergy] {
                        spot.remove(kind);
                        if let Some(vol) = theirs.vols.iter().find(|v| v.kind == kind) {
                            spot.vols.push(*vol);
                        }
                    }
                }
            }
            "-status" => {
                if arg(3) == "tox" {
                    let spot = self.spot(side, pos);
                    (spot.tox, spot.tox_new) = (0, from.starts_with("item:"));
                }
            }
            "-curestatus" => {
                // Only waking/thawing happens inside BeforeMove. A berry's
                // paralysis cure must not masquerade as waking from sleep.
                if parts.contains(&"[msg]") && matches!(arg(3), "slp" | "frz") {
                    let status = if arg(3) == "frz" { Status::Frz } else { Status::Slp };
                    self.roused = Some((side, pos, status));
                }
            }
            "-start" => self.started_on(side, pos, arg(3), parts, of),
            "-end" => {
                let at = self.begun.len();
                let reset_types = if arg(3) == "typechange"
                    && self.reader.live_record(side, pos).is_some_and(|r| r.ability == ab::MIMICRY)
                {
                    let base = if side == self.side {
                        self.own
                            .get(self.own_at[pos])
                            .and_then(|o| self.req.as_ref()?.mons.iter().find(|m| m.name == o.name).map(|m| m.species))
                    } else {
                        self.reader.sides[side].active[pos]
                            .as_ref()
                            .and_then(|l| self.reader.sides[side].team.get(l.entry))
                            .map(|m| m.species)
                    };
                    self.spots[side][pos].forme.filter(|&s| is_mega(s)).or(base).map(|s| SPECIES[s as usize].types)
                } else {
                    None
                };
                let spot = self.spot(side, pos);
                let effect = arg(3);
                if parts.contains(&"[partiallytrapped]") {
                    // The one that held it has gone: an Octolock ends under its own name, a
                    // binding move under the move's. (It can be under both, from two Pokémon.)
                    spot.remove(if effect == "Octolock" { VolKind::Octolock } else { VolKind::Partiallytrapped });
                } else if effect == "typechange" {
                    spot.types = reset_types;
                    spot.added = Type::None;
                    (spot.copied, spot.copied_foe, spot.told) = (false, false, false);
                } else if effect == "Stockpile" {
                    spot.remove(VolKind::Stockpile);
                } else if let Some(kind) = VolKind::named(&effect_id(effect)) {
                    spot.remove(kind);
                    // An immediate herb cure prevents replacement. A later cure (e.g.
                    // receiving a herb via Trick) cannot undo an already changed action.
                    if kind == VolKind::Encore && spot.begun == 0 && spot.overridden == Some(at) {
                        spot.overridden = None;
                    }
                } else if matches!(effect_id(effect).as_str(), "futuresight" | "doomdesire") {
                    self.field.sides[side].slots[pos].retain(|c| c.0 != SlotCond::Futuremove);
                    self.ending = true;
                }
            }
            "-singleturn" | "-singlemove" => {
                let effect = effect_id(arg(3));
                let ends = if kind == "-singleturn" { Ends::Turn } else { Ends::Move };
                if let Some(cond) = SideCond::named(&effect) {
                    // Wide Guard and Quick Guard cover the side.
                    let conds = &mut self.field.sides[side].conds;
                    if !conds.iter().any(|c| c.0 == cond) {
                        conds.push((cond, 1, 0));
                    }
                } else if let Some(vol) = VolKind::named(&effect) {
                    // (Spiky Shield, Baneful Bunker and King's Shield go up under Protect's name.)
                    let own = self.mover.as_ref().filter(|m| (m.side, m.pos) == (side, pos)).and_then(|m| m.shield);
                    let vol = if vol == VolKind::Protect { own.unwrap_or(vol) } else { vol };
                    self.spot(side, pos).add(vol, ends);
                } else if effect == "instruct" {
                    self.spot(side, pos).instructed = true;
                }
                // A move that protects makes the next one less likely to work.
                let stalling = move_id(&effect).is_some_and(|m| MOVES[m as usize].stalling_move)
                    || matches!(effect.as_str(), "wideguard" | "quickguard");
                if stalling {
                    self.spot(side, pos).stalled();
                }
            }
            "-activate" => {
                let effect = arg(3);
                let name = effect_id(effect);
                let begun = self.begun.len();
                if effect == "trapped" {
                    // Kept there by whoever is moving; or, if that is itself (Jaw Lock holds
                    // both), by the one it moved on.
                    let source = match &self.mover {
                        Some(m) if (m.side, m.pos) == (side, pos) => m.target.map_or(0, |(s, p)| self.spots[s][p].stay),
                        Some(m) => m.stay,
                        None => 0,
                    };
                    let spot = self.spot(side, pos);
                    if spot.add(VolKind::Trapped, Ends::Said) {
                        spot.vols.last_mut().unwrap().source = source;
                    }
                } else if effect.starts_with("move: ")
                    && move_id(&name).is_some_and(|m| MOVES[m as usize].volatile == Some(VolKind::Partiallytrapped))
                {
                    self.spot(side, pos).add(VolKind::Partiallytrapped, Ends::Said);
                } else if matches!(name.as_str(), "lockon" | "mindreader") {
                    self.spot(side, pos).add(VolKind::Lockon, Ends::After(2));
                } else if matches!(name.as_str(), "afteryou" | "quash") {
                    let spot = self.spot(side, pos);
                    spot.reordered = Some(spot.reordered.map_or(begun, |t| t.min(begun)));
                } else if matches!(name.as_str(), "quickclaw" | "quickdraw" | "custapberry") {
                    // Instruct resolves a fresh move and can roll its item again;
                    // that roll did not promote the original queued action.
                    if !self.spots[side][pos].instructed {
                        self.spot(side, pos).quick = true;
                    }
                } else if name == "symbiosis" {
                    // This transfer is announced only by -activate, not -item.
                    // Keep the donor and recipient's private-item overrides current.
                    self.spot(side, pos).item_since = Some(it::NONE);
                    if let Some((s, p)) = of
                        && let Some(item) = item_id(&to_id(arg(4)))
                    {
                        let consumed =
                            self.reader.live_record(s, p).is_some_and(|rec| rec.item == ItemShown::Lost(item));
                        self.spot(s, p).item_since = Some(if consumed { it::NONE } else { item });
                    }
                } else if matches!(name.as_str(), "skillswap" | "wanderingspirit" | "mummy" | "lingeringaroma") {
                    // Abilities change hands (and one of them says so again if it is Pressure):
                    // both Pokémon's for a swap, the attacker's for one that rubs off.
                    let swap = matches!(name.as_str(), "skillswap" | "wanderingspirit");
                    for (s, p) in of.or(fourth).into_iter().chain(swap.then_some((side, pos))) {
                        let spot = &mut self.spots[s][p];
                        spot.own_abilities = None;
                        (spot.pressing, spot.lost_item) = (false, false);
                    }
                } else if name == "feint" || parts.contains(&"[broken]") {
                    // Its protection is broken, and with it the run of protecting moves.
                    self.field.sides[side].conds.retain(|c| !matches!(c.0, SideCond::Quickguard | SideCond::Wideguard));
                    self.spot(side, pos).vols.retain(|v| {
                        !matches!(
                            v.kind,
                            VolKind::Protect
                                | VolKind::Banefulbunker
                                | VolKind::Kingsshield
                                | VolKind::Spikyshield
                                | VolKind::Stall
                        )
                    });
                } else if name == "gravity" {
                    self.spot(side, pos).grounded();
                } else if matches!(name.as_str(), "spite" | "eeriespell" | "leppaberry") && side == self.side {
                    // PP taken from one of its moves, or given back.
                    let restored = if self.reader.live_record(side, pos).is_some_and(|rec| rec.ability == ab::RIPEN)
                        && !self.spots[side][pos].has(VolKind::Gastroacid)
                    {
                        20
                    } else {
                        10
                    };
                    let n: u8 = arg(5).parse().unwrap_or(if name == "leppaberry" { restored } else { 0 });
                    if let (Some(id), Some(own)) = (move_id(&to_id(arg(4))), self.own.get_mut(self.own_at[pos])) {
                        let known = if own.copied.is_empty() { &mut own.pp } else { &mut own.copied };
                        if let Some(slot) = known.iter_mut().find(|s| s.0 == id) {
                            slot.1 =
                                if name == "leppaberry" { (slot.1 + n).min(slot.2) } else { slot.1.saturating_sub(n) };
                        }
                    }
                }
            }
            "-enditem" => {
                // The observer filters empty Fling, Cud Chew and duplicate [weaken]
                // messages: none removes the item currently held after a transfer.
                if self.reader.item_notes.is_empty() {
                    return Ok(());
                }
                if effect_id(arg(3)) == "custapberry" {
                    self.spot(side, pos).quick = true;
                }
                let spot = self.spot(side, pos);
                (spot.choice, spot.item_since) = (None, Some(it::NONE));
            }
            "-waiting" => {
                // A Pledge waits for its partner, which goes next.
                if let Some((s, p)) = third {
                    let begun = self.begun.len();
                    let spot = self.spot(s, p);
                    spot.reordered = Some(spot.reordered.map_or(begun, |t| t.min(begun)));
                }
            }
            "-endability" => {
                // Gastro Acid says nothing more; an ability replaced names it.
                if arg(3).is_empty() || arg(3).starts_with('[') {
                    self.spot(side, pos).add(VolKind::Gastroacid, Ends::Said);
                }
                let spot = self.spot(side, pos);
                (spot.pressing, spot.lost_item) = (false, false);
            }
            "-item" => {
                // (A Choice item that comes, even in place of another, starts afresh.)
                let item = item_id(&to_id(arg(3))).map(|item| {
                    // White Herb/terrain seeds can be consumed before the arrival line.
                    if self.reader.live_record(side, pos).is_some_and(|rec| rec.item == ItemShown::Lost(item)) {
                        it::NONE
                    } else {
                        item
                    }
                });
                let spot = self.spot(side, pos);
                spot.item_since = item.or(spot.item_since);
                if !from.is_empty() && !from.contains("Frisk") {
                    // (Not an item it had all along coming to light: a Choice item that
                    // comes, even in place of another, starts afresh.)
                    (spot.choice, spot.no_hold) = (None, false);
                }
                let taken =
                    ["move: Thief", "move: Covet", "ability: Pickpocket", "ability: Magician", "ability: Symbiosis"];
                if let Some((s, p)) = of.filter(|_| taken.contains(&from)) {
                    // Taken from, or handed over by, the Pokémon named last.
                    let spot = &mut self.spots[s][p];
                    (spot.choice, spot.item_since) = (None, Some(it::NONE));
                }
            }
            "-mustrecharge" => {
                self.spot(side, pos).add(VolKind::Mustrecharge, Ends::Said);
            }
            "-ability" => {
                let spot = self.spot(side, pos);
                spot.own_abilities = None;
                spot.pressing = to_id(arg(3)) == "pressure";
                if !from.is_empty() {
                    // It has come by this ability in place of its own (Trace, Role Play, Worry
                    // Seed): an Unburden at work ends with the ability, whatever replaces it.
                    spot.lost_item = false;
                }
            }
            "-prepare" => {
                if !parts.contains(&"[premajor]") {
                    let spot = self.spot(side, pos);
                    spot.add(VolKind::Twoturnmove, Ends::After(2));
                    if let Some(own) = VolKind::named(&to_id(arg(3))) {
                        spot.add(own, Ends::Said);
                    }
                }
            }
            "-anim" => {
                // The move goes off at once after all (Power Herb, Solar Beam in the sun).
                self.spot(side, pos).vols.retain(|v| v.kind != VolKind::Twoturnmove && !CHARGING.contains(&v.kind));
            }
            "-damage" if from.is_empty() && !self.ending => {
                // A blow from a move of the other side's: Metal Burst and Comeuppance go back
                // to where the last one came from.
                if let Some(m) =
                    self.mover.as_ref().filter(|m| m.side != side && self.spots[m.side][m.pos].stay == m.stay)
                {
                    let by = (self.identity(m.side, m.pos), m.pos);
                    self.spot(side, pos).hit_by.push(by);
                }
            }
            "-damage" if from == "confusion" => {
                if self.spots[side][pos].instructed {
                    // Showdown emits no move/cant for this failed repeat. Consume it
                    // independently so the later chosen action remains in normal order.
                    self.begin(side, pos, NO_MOVE, Some(false));
                }
                // It hurt itself in place of moving: a move it was charging is off.
                self.spot(side, pos).vols.retain(|v| {
                    v.ends != Ends::Move
                        && !matches!(v.kind, VolKind::Twoturnmove | VolKind::Mustrecharge)
                        && !CHARGING.contains(&v.kind)
                });
            }
            "-heal" => {
                let cond = match effect_id(from).as_str() {
                    "wish" => Some(SlotCond::Wish),
                    "healingwish" | "lunardance" => Some(SlotCond::Healingwish),
                    _ => None,
                };
                if let Some(cond) = cond {
                    self.field.sides[side].slots[pos].retain(|c| c.0 != cond);
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// `-start|pokemon|effect`: a condition begins on a Pokémon.
    fn started_on(&mut self, side: usize, pos: usize, effect: &str, parts: &[&str], of: Option<(usize, usize)>) {
        let arg = |i: usize| parts.get(i).copied().unwrap_or("");
        let name = effect_id(effect);
        let begun = self.spots[side][pos].begun;
        let at = self.begun.len();
        match effect {
            "typechange" if arg(4).starts_with('[') || arg(4).is_empty() => {
                // Reflect Type: the types of the Pokémon named after `[of]`, as they are now
                // (a roosting one is not Flying) and as far as this side can see them. If
                // that Pokémon is in disguise, they are another's: its own side knows better.
                if let Some((s, p)) = of {
                    let theirs = &self.spots[s][p];
                    let roosting = theirs.vols.iter().any(|v| v.kind == VolKind::Roost);
                    let (types, added) = (types_copied(self.seen_base(s, p), roosting), theirs.added);
                    let before = self.seen_base(side, pos);
                    let spot = self.spot(side, pos);
                    if !spot.copied {
                        spot.before = before;
                    }
                    (spot.types, spot.added) = (Some(types), added);
                    (spot.copied, spot.copied_foe) = (true, s != side);
                }
            }
            "typechange" if parts.contains(&"[silent]") => {
                // Said at the end of a turn of a Pokémon whose types are not the ones the
                // log last gave it: the types of the Pokémon named, or of the one it passes
                // for. Of its own Pokémon the follower knows which; of the other side's, the
                // one meant is the one that copied another's types this turn.
                if !self.present(side, pos) {
                    return;
                }
                let mut types = [Type::None; 2];
                for (k, t) in arg(4).split('/').take(2).enumerate() {
                    types[k] = type_named(t);
                }
                let partner = Some(1 - pos).filter(|&p| self.present(side, p));
                let meant = match partner {
                    Some(p) if side == self.side => {
                        if self.own_disguised(pos) {
                            p
                        } else {
                            pos
                        }
                    }
                    Some(p) if !self.spots[side][pos].copied && self.spots[side][p].copied => p,
                    _ => pos,
                };
                let spot = self.spot(side, meant);
                (spot.types, spot.told) = (Some(types), true);
                if meant == pos {
                    // (Its added type, if it has one, is said again next.)
                    spot.added = Type::None;
                }
            }
            "typechange" => {
                let mut types = [Type::None; 2];
                for (k, t) in arg(4).split('/').take(2).enumerate() {
                    types[k] = type_named(t);
                }
                let spot = self.spot(side, pos);
                (spot.types, spot.added) = (Some(types), Type::None);
                // This explicit change supersedes Reflect Type's pending settlement.
                (spot.copied, spot.copied_foe) = (false, false);
            }
            "typeadd" => self.spot(side, pos).added = type_named(arg(4)),
            _ if effect.starts_with("perish") => {
                let left: u8 = effect[6..].parse().unwrap_or(3);
                let duration = if parts.contains(&"[silent]") { left + 1 } else { left };
                let spot = self.spot(side, pos);
                spot.add(VolKind::Perishsong, Ends::Said);
                if let Some(v) = spot.vols.iter_mut().find(|v| v.kind == VolKind::Perishsong) {
                    v.duration = duration;
                }
            }
            _ if effect.starts_with("stockpile") => {
                self.spot(side, pos).add(VolKind::Stockpile, Ends::Said);
            }
            _ if matches!(name.as_str(), "futuresight" | "doomdesire") => {
                // On whoever stands where it was aimed, in two turns' time.
                let target = self.mover.as_ref().filter(|m| (m.side, m.pos) == (side, pos)).and_then(|m| m.target);
                if let Some((s, p)) = target.or(of) {
                    let slots = &mut self.field.sides[s].slots[p];
                    if !slots.iter().any(|c| c.0 == SlotCond::Futuremove) {
                        slots.push((SlotCond::Futuremove, 3));
                    }
                }
            }
            _ => {
                if let Some(kind) = VolKind::named(&name) {
                    let spot = self.spot(side, pos);
                    spot.add(kind, Ends::Said);
                    if kind == VolKind::Encore && begun == 0 {
                        spot.overridden = Some(at);
                    }
                    let spot = self.spot(side, pos);
                    if kind == VolKind::Smackdown {
                        spot.grounded();
                    }
                }
            }
        }
    }

    /// A turn ends.
    fn upkeep(&mut self) {
        if self.field.weather != Weather::None && !self.field.weather_late {
            self.field.weather_for += 1;
        }
        if self.field.terrain != Terrain::None && !self.field.terrain_late {
            self.field.terrain_for += 1;
        }
        (self.field.weather_late, self.field.terrain_late, self.ending) = (false, false, false);
        self.field.pseudo.iter_mut().for_each(|p| p.1 = p.1.saturating_sub(1));
        self.field.pseudo.retain(|p| p.1 > 0);
        for side in 0..2 {
            let track = &mut self.field.sides[side];
            track.conds.retain(|c| !matches!(c.0, SideCond::Wideguard | SideCond::Quickguard));
            track.conds.iter_mut().for_each(|c| c.2 += 1);
            for slots in track.slots.iter_mut() {
                slots.retain_mut(|c| {
                    if c.1 == 0 {
                        return true;
                    }
                    c.1 -= 1;
                    c.1 > 0
                });
            }
            for pos in 0..ACTIVE {
                let status = self.reader.live_record(side, pos).map(|rec| rec.status);
                let spot = &mut self.spots[side][pos];
                if status == Some(Status::Tox) && !spot.tox_new {
                    spot.tox = (spot.tox + 1).min(15);
                }
                spot.tox_new = false;
                spot.vols.retain_mut(|v| match &mut v.ends {
                    Ends::Turn => false,
                    Ends::After(n) => {
                        *n -= 1;
                        *n > 0
                    }
                    _ => true,
                });
            }
        }
    }

    // ------------------------------------------------------- the request

    /// Takes in a request (the JSON after `|request|`), which makes this a
    /// decision: the battle is rebuilt as it now stands. Give the log up to
    /// the request first. (Showdown may send the request before the lines
    /// that lead up to it: hold it back until those have come.)
    pub fn request(&mut self, json: &str) -> Result<(), String> {
        let Some(req) = Req::parse(json)? else {
            self.req = None;
            return Ok(());
        };
        if req.asked == Asked::Preview || !self.started {
            self.req = Some(req);
            return Ok(());
        }
        if self.own.is_empty() {
            // The first request of the battle lists the Pokémon brought, in the order picked.
            for mon in &req.mons {
                let base = SPECIES[mon.species as usize].base_species;
                let free = |j: &usize| !self.own.iter().any(|o| o.entry == *j);
                let entry = (0..self.team.len())
                    .filter(free)
                    .find(|&j| self.nicknames.get(j) == Some(&mon.name))
                    .or((0..self.team.len())
                        .filter(free)
                        .find(|&j| SPECIES[self.team[j].species as usize].base_species == base))
                    .ok_or_else(|| format!("{} is not on the team this follower was given", mon.name))?;
                let pp =
                    self.team[entry].moves.iter().map(|&m| (m, MOVES[m as usize].pp, MOVES[m as usize].pp)).collect();
                self.own.push(Own {
                    entry,
                    name: mon.name.clone(),
                    pp,
                    copied: Vec::new(),
                    unknown_pp: [Vec::new(), Vec::new()],
                });
            }
        }
        // Who stands where, and the PP the request gives.
        for (k, mon) in req.mons.iter().enumerate() {
            let idx = self.own.iter().position(|o| o.name == mon.name).ok_or("a request for an unknown Pokémon")?;
            if k < ACTIVE {
                self.own_at[k] = idx;
            }
            let live = if k < ACTIVE { self.reader.sides[self.side].active[k].as_ref() } else { None };
            let transformed = live.is_some_and(|l| l.transformed && !l.gone);
            if k < ACTIVE && self.spots[self.side][k].choice.is_some_and(|held| !mon.moves.contains(&held)) {
                // The private request names the current moveset, even after
                // Transform and before the copied PP cache is filled.
                self.spots[self.side][k].choice = None;
            }
            let own = &mut self.own[idx];
            if !transformed {
                own.copied.clear();
                own.unknown_pp[1].clear();
            }
            if let (Some(active), true) = (req.active.get(k), k < ACTIVE) {
                for m in &active.moves {
                    let Some((pp, max)) = m.pp else {
                        if transformed && !own.copied.iter().any(|slot| slot.0 == m.id) {
                            let pp = MOVES[m.id as usize].base_pp.min(5);
                            own.copied.push((m.id, pp, pp));
                        }
                        continue;
                    };
                    own.unknown_pp[transformed as usize].retain(|&id| id != m.id);
                    let known = if transformed { &mut own.copied } else { &mut own.pp };
                    match known.iter_mut().find(|s| s.0 == m.id) {
                        Some(slot) => (slot.1, slot.2) = (pp, max),
                        None if transformed => known.push((m.id, pp, max)),
                        None => {}
                    }
                }
            }
        }
        self.req = Some(req);
        self.spots.iter_mut().flatten().for_each(|spot| {
            (spot.ability_touched, spot.item_since, spot.own_abilities) = (false, None, None);
        });
        let mut b = self.rebuild()?;
        // What this request says of the other side's moves: one of its own that is barred
        // for no reason on show has been sealed by an Imprison, whose user knows it.
        let mut news = false;
        for (entry, sealed) in b.sealed_moves(self.side, false) {
            if let Some(listed) = self.reader.sides[1 - self.side].roster.get_mut(entry) {
                news |= listed.tell(sealed);
            }
        }
        if news {
            b = self.rebuild()?;
        }
        self.digest(&b);
        if b.request == Request::Move {
            self.turn_start = Some(b);
        }
        self.battle = Some(b);
        Ok(())
    }

    /// The registered teams as sets: the follower's own, and the other side's as far as it is known.
    fn rosters(&self) -> [Vec<PokemonSet>; 2] {
        let theirs = self.reader.sides[1 - self.side]
            .roster
            .iter()
            .map(|l| PokemonSet {
                species: l.species,
                moves: l.moves().to_vec(),
                nature: l.nature,
                stat_points: [0; 6],
                ability: l.ability,
                item: l.item,
                gender: l.gender,
            })
            .collect();
        let mut out = [self.team.clone(), theirs];
        if self.side == 1 {
            out.swap(0, 1);
        }
        out
    }

    fn open(&self) -> bool {
        self.reader.sides[1 - self.side].open
    }

    /// The battle as the log and the last request have it.
    fn rebuild(&self) -> Result<Battle, String> {
        let req = self.req.as_ref().ok_or("no request to go by")?;
        let (me, opp) = (self.side, 1 - self.side);
        let mut b = Battle::blank([1, 2, 3, 4]);
        b.started = true;
        b.turn = self.turn.max(1);
        b.request = if req.asked == Asked::Move { Request::Move } else { Request::Switch };
        b.mid_turn = b.request == Request::Switch;
        b.open_team_sheets = self.open();
        // Nothing is skipped for want of a listener.
        (b.event_mask, b.event_mask_pre) = (u128::MAX, u128::MAX);

        // ---- the field
        b.field.weather.kind = self.field.weather;
        b.field.terrain.kind = self.field.terrain;
        for &(kind, left) in &self.field.pseudo {
            let mut c = Cond::new(kind);
            c.duration = left;
            b.field.pseudo.push(c);
        }
        for side in 0..2 {
            let track = &self.field.sides[side];
            for &(kind, layers, since) in &track.conds {
                if b.sides[side].conds.has(kind) {
                    continue;
                }
                let mut c = Cond::new(kind);
                c.duration = (kind.data().duration as u16).saturating_sub(since).min(255) as u8;
                c.data = layers as u16;
                b.sides[side].conds.push(c);
            }
            for pos in 0..ACTIVE {
                for &(kind, _) in &track.slots[pos] {
                    if !b.sides[side].slot_conds[pos].has(kind) {
                        b.sides[side].slot_conds[pos].push(Cond::new(kind));
                    }
                }
            }
        }

        (b.sides[0].mega_used, b.sides[1].mega_used) = (self.megaed[0], self.megaed[1]);

        // ---- the follower's own side, from the request
        let megaed = self.megaed[me] || req.mons.iter().any(|m| is_mega(m.species));
        let us = &self.reader.sides[me];
        b.sides[me].n = req.mons.len().min(MAX_TEAM) as u8;
        for (k, mon) in req.mons.iter().take(MAX_TEAM).enumerate() {
            let idx = self.own.iter().position(|o| o.name == mon.name).ok_or("a request for an unknown Pokémon")?;
            let own = &self.own[idx];
            let set = &self.team[own.entry];
            let mut m = b.new_mon(set, k).map_err(|e| e.to_string())?;
            let live = if k < ACTIVE { us.active[k].as_ref().filter(|l| !l.gone) } else { None };
            m.transformed = live.is_some_and(|l| l.transformed);
            // The species it is now: what the request says, or what the log has it change
            // into on the field (another Pokémon's shape, a forme of its own).
            // (A forme taken on the field is dropped on fainting, as on leaving.)
            let stands = k < ACTIVE && !mon.fainted && !is_mega(mon.species);
            let forme = if stands { self.spots[me][k].forme } else { None };
            m.species = match live {
                Some(l) if l.transformed => l.species,
                _ => forme.unwrap_or(mon.species),
            };
            m.base_species = mon.species;
            m.types = SPECIES[m.species as usize].types;
            if !m.transformed {
                m.stats = calc_stats(m.species, set.nature, set.stat_points);
            }
            m.stats[0] = if mon.fainted { self.stats[own.entry][0] } else { mon.max_hp };
            (m.hp, m.status, m.fainted) = (mon.hp, mon.status, mon.fainted);
            m.faint_queued = mon.fainted;
            (m.ability, m.base_ability, m.item) = (mon.ability, mon.base_ability, mon.item);
            let stone = ITEMS[mon.item as usize].mega.iter().find(|&&(from, _)| from == set.species);
            m.can_mega = match stone {
                Some(&(_, mega)) if !megaed => mega,
                _ => NO_SPECIES,
            };
            // Its moves: as the request has them, with the PP last seen.
            let active = if k < ACTIVE { req.active.get(k) } else { None };
            let listed = active.filter(|a| a.moves.iter().all(|slot| slot.pp.is_some()) && !a.moves.is_empty());
            m.n_moves = 0;
            match listed {
                Some(active) => {
                    for slot in active.moves.iter().take(MAX_MOVES) {
                        let (pp, maxpp) = slot.pp.unwrap();
                        m.moves[m.n_moves as usize] =
                            MoveSlot { id: slot.id, pp, maxpp, disabled: slot.disabled, hidden: false, used: false };
                        m.n_moves += 1;
                    }
                }
                None => {
                    for &id in mon.moves.iter().take(MAX_MOVES) {
                        let known = if m.transformed { &own.copied } else { &own.pp };
                        let initial_pp =
                            if m.transformed { MOVES[id as usize].base_pp.min(5) } else { MOVES[id as usize].pp };
                        let (pp, maxpp) =
                            known.iter().find(|s| s.0 == id).map_or((initial_pp, initial_pp), |s| (s.1, s.2));
                        m.moves[m.n_moves as usize] =
                            MoveSlot { id, pp, maxpp, disabled: false, hidden: false, used: false };
                        m.n_moves += 1;
                    }
                }
            }
            for k in 0..m.n_moves as usize {
                if own.unknown_pp[m.transformed as usize].contains(&m.moves[k].id) {
                    m.unknown_pp |= 1 << k;
                }
            }
            if let Some(active) = active {
                // ("Maybe trapped" is all the last one to choose is told when a foe might be
                // holding it in by an ability not yet shown: it may try to switch, and is told
                // then. Until it is, it is free as far as its player knows.)
                let _ = active.maybe_trapped;
                m.trapped = if active.trapped { Trapped::Yes } else { Trapped::No };
                // One move listed with no PP to its name: it is locked into that, or can only struggle.
                if listed.is_none() && active.moves.len() == 1 {
                    let only = &active.moves[0];
                    if only.id == mv::STRUGGLE {
                        m.moves[..m.n_moves as usize].iter_mut().for_each(|s| s.disabled = true);
                    } else {
                        m.locked_move = only.id;
                    }
                }
            }
            m.position = k as u8;
            m.is_active = k < ACTIVE && mon.active && !mon.fainted;
            if m.is_active && m.ability == ab::UNBURDEN && m.item == it::NONE && self.spots[me][k].lost_item {
                // Unburden is at work, which the engine keeps as a condition of its own.
                b.vols[me][k].push(Cond::new(VolKind::Unburden));
            }
            m.switch_flag = k < ACTIVE && req.asked == Asked::Switch && req.force[k];
            if k < ACTIVE && mon.reviving && !b.sides[me].slot_conds[k].has(SlotCond::Revivalblessing) {
                b.sides[me].slot_conds[k].push(Cond::new(SlotCond::Revivalblessing));
            }
            // What the other side has been shown of it.
            if let Some(entry) = us.team.iter().find(|e| e.name == mon.name) {
                b.sides[me].shown[idx] = entry.rec;
            }
            if m.is_active
                && let Some(live) = live
            {
                m.live = live.rec;
            }
            b.sides[me].team[idx] = m;
            b.sides[me].order[k] = idx as u8;
        }
        let side = &mut b.sides[me];
        side.total_fainted = req.mons.iter().filter(|m| m.fainted).count() as u8;
        side.pokemon_left = side.n - side.total_fainted;
        side.n_seen = us.team.len() as u8;
        side.n_roster = self.team.len() as u8;
        for (j, set) in self.team.iter().enumerate() {
            let mut listed = Listed {
                species: set.species,
                gender: set.gender,
                item: set.item,
                ability: set.ability,
                n_moves: set.moves.len().min(MAX_MOVES) as u8,
                nature: set.nature,
                ..Listed::NONE
            };
            for (k, &id) in set.moves.iter().take(MAX_MOVES).enumerate() {
                listed.moves[k] = id;
            }
            listed.brought = self.own.iter().position(|o| o.entry == j).map_or(NOT_LISTED, |idx| idx as u8);
            side.roster[j] = listed;
        }

        // ---- the other side, from what it has shown
        let them = &self.reader.sides[opp];
        let side = &mut b.sides[opp];
        side.n = if them.size == 0 { 4 } else { them.size.min(MAX_TEAM as u8) };
        side.n = side.n.max(them.team.len().min(MAX_TEAM) as u8).max(ACTIVE as u8);
        side.pokemon_left = side.n.saturating_sub(them.fainted);
        side.total_fainted = them.fainted;
        side.n_seen = them.team.len().min(MAX_TEAM) as u8;
        side.n_roster = them.roster.len().min(MAX_ROSTER) as u8;
        for (j, listed) in them.roster.iter().take(MAX_ROSTER).enumerate() {
            side.roster[j] = *listed;
        }
        for (idx, entry) in them.team.iter().take(MAX_TEAM).enumerate() {
            let mut m = Battle::blank_mon();
            let species = if entry.rec.species == NO_SPECIES { entry.species } else { entry.rec.species };
            (m.species, m.base_species, m.set_species) = (species, species, species);
            m.types = SPECIES[species as usize].types;
            m.gender = entry.gender;
            // Its HP is known as a percentage: out of a hundred, then.
            m.stats = [100; 6];
            (m.hp, m.status, m.fainted) = (entry.rec.hp as u16, entry.rec.status, entry.rec.fainted);
            m.faint_queued = m.fainted;
            side.team[idx] = m;
            side.shown[idx] = entry.rec;
        }
        // Those not yet seen stand behind, in the places that are left.
        for idx in them.team.len()..MAX_TEAM {
            let mut m = Battle::blank_mon();
            (m.stats, m.hp, m.fainted) = ([100; 6], 100, false);
            side.team[idx] = m;
        }
        let mut order: Vec<usize> = Vec::new();
        for pos in 0..ACTIVE {
            let Some(live) = them.active[pos].as_ref() else {
                continue;
            };
            let idx = self.their_index(pos);
            order.push(idx);
            if live.gone {
                continue;
            }
            let entry = &them.team[live.entry];
            if idx != live.entry {
                // The second of two that go by one name: a Pokémon of its own, passing for the first.
                let mut m = Battle::blank_mon();
                m.gender = entry.gender;
                m.stats = [100; 6];
                m.illusion = live.entry as u8 + 1;
                side.team[idx] = m;
            }
            let m = &mut side.team[idx];
            m.fainted = false;
            m.faint_queued = false;
            m.is_active = true;
            (m.species, m.base_species) = (live.species, live.species);
            m.types = SPECIES[live.species as usize].types;
            m.transformed = live.transformed;
            (m.hp, m.status) = (live.rec.hp as u16, live.rec.status);
            m.live = live.rec;
            if live.rec.ability != UNKNOWN {
                (m.ability, m.base_ability) = (live.rec.ability, live.rec.ability);
            }
            if let ItemShown::Holds(item) = live.rec.item {
                m.item = item;
            }
        }
        for idx in 0..MAX_TEAM {
            if !order.contains(&idx) {
                order.push(idx);
            }
        }
        for (k, &idx) in order.iter().take(MAX_TEAM).enumerate() {
            side.order[k] = idx as u8;
            side.team[idx].position = k as u8;
        }

        // ---- what the log says of each position
        for s in 0..2 {
            for pos in 0..ACTIVE {
                let spot = &self.spots[s][pos];
                let r = b.active(s, pos);
                if !b.mon(r).is_active {
                    continue;
                }
                {
                    let m = b.mon_mut(r);
                    m.boosts = spot.boosts;
                    if let Some(types) = spot.types {
                        (m.types, m.said.seen) = (types, [types; 2]);
                    }
                    m.added_type = spot.added;
                    m.active_turns = spot.active_turns;
                    m.last_move = spot.last_move;
                    m.last_attempted_move = spot.last_attempted_move;
                    m.tox_stage = if m.status == Status::Tox { spot.tox } else { 0 };
                }
                for v in &spot.vols {
                    if b.vols[s][pos].has(v.kind) || b.vols[s][pos].is_full() {
                        continue;
                    }
                    let mut c = Cond::new(v.kind);
                    (c.data, c.duration) = (v.data, v.duration);
                    b.vols[s][pos].push(c);
                }
            }
        }
        Ok(b)
    }

    /// The timers the observation reads, from the turns counted.
    fn count_timers(&self) -> Timers {
        let mut screens = [[None; 3]; 2];
        for (side, row) in screens.iter_mut().enumerate() {
            for (k, kind) in [SideCond::Reflect, SideCond::Lightscreen, SideCond::Auroraveil].into_iter().enumerate() {
                row[k] = self.field.sides[side].conds.iter().find(|c| c.0 == kind).map(|c| c.2);
            }
        }
        Timers::counted(
            (self.field.weather != Weather::None).then_some(self.field.weather_for),
            (self.field.terrain != Terrain::None).then_some(self.field.terrain_for),
            screens,
            self.turn.max(1),
        )
    }

    /// Where within its bracket the follower's own Pokémon's move went this
    /// turn: settled when it was chosen, by its item or ability.
    fn own_frac(&self, begun: &Begun, chosen: u16) -> Option<i8> {
        let mut b = *self.turn_start.as_ref()?;
        if begun.named == NO_MOVE || !b.mon(begun.r).is_active {
            return None;
        }
        let a = b.resolve_move(begun.r, chosen, 0);
        // A Quick Claw is rolled for, and says so when it works; what always holds a move back does not.
        Some(if begun.quick { 1 } else { a.frac.min(0) })
    }

    /// A move as it stood in the queue when the `at`th of this stretch began.
    fn queued(&self, j: usize, at: usize) -> Queued {
        let s = &self.begun[j];
        let mut q = Queued {
            r: s.r,
            order: 200,
            priority: 0,
            frac: s.quick as i8,
            speed: 0,
            move_id: if s.named == NO_MOVE { 0 } else { s.named },
            encored: s.encored,
        };
        if s.again || s.instructed || s.reordered.is_some_and(|t| t <= at) {
            q.order = 3;
        }
        if s.side == self.side {
            // Its own choice it knows, where the reply was made here; otherwise an Encore
            // leaves the move it queued in doubt.
            // (Once an Encore has caught it, the move in the queue is the one it is held to.)
            match self.chosen.iter().find(|c| c.0 == s.r.idx as usize) {
                _ if s.overridden.is_some_and(|t| t <= at) => {}
                // (With every move barred, by an Imprison it was not told the reach of, what
                // went into the queue when it chose was Struggle.)
                Some(_) if s.named == mv::STRUGGLE => q.move_id = mv::STRUGGLE,
                Some(&(_, chosen, _)) => q.move_id = chosen,
                None if s.overridden.is_some() => q.order = 3,
                None => {}
            }
            // Speed and priority as they stood when the `at`th began: the queue is sorted
            // again after every move, with whatever has changed (a Mega's new ability, say).
            let then = &self.begun[at];
            let now = then.own.iter().find(|o| o.0 == s.stay).and_then(|o| o.1.as_ref());
            let speed =
                now.and_then(|n| n.agreed(|a| speed::own_speed(&n.seen, a, n.item, n.lost_item, n.stat, &then.field)));
            let priority =
                now.and_then(|n| n.agreed(|a| speed::own_priority(&n.seen, a, &then.field, q.move_id, n.grounded)));
            match (speed, priority, self.own_frac(s, q.move_id)) {
                (Some(speed), Some(priority), Some(frac)) => {
                    (q.speed, q.priority, q.frac) = (speed, priority + frac as i32, frac)
                }
                _ => q.order = 0,
            }
        }
        q
    }

    /// At a decision: what the moves since the last one say of the other side's Speed.
    fn digest(&mut self, b: &Battle) {
        let first = self.speeds.is_none();
        if first {
            let rosters = self.rosters();
            let mut speeds = Speeds::new([&rosters[0], &rosters[1]], self.open());
            speeds.watch_as(self.side);
            if let Some(on) = self.item_clause {
                speeds.set_clause(1 - self.side, on);
            }
            self.speeds = Some(speeds);
        }
        let mut events = Vec::with_capacity(self.begun.len() + self.items.len());
        for i in 0..=self.begun.len() {
            events.extend(self.items.iter().filter(|(at, _)| *at == i).map(|(_, e)| *e));
            if i == self.begun.len() {
                break;
            }
            let me = self.queued(i, i);
            let mut rest = [me; 3];
            let mut n_rest = 0;
            for j in i + 1..self.begun.len() {
                let later = &self.begun[j];
                let listed = rest[..n_rest].iter().any(|q| q.r == later.r);
                if later.stay != self.begun[i].stay && !later.instructed && !listed && n_rest < 3 {
                    rest[n_rest] = self.queued(j, i);
                    n_rest += 1;
                }
            }
            events.push(Event::Start {
                me,
                rest,
                n_rest: n_rest as u8,
                seen: self.begun[i].seen,
                field: self.begun[i].field,
                named: self.begun[i].named,
            });
        }
        let speeds = self.speeds.as_mut().unwrap();
        for moved in self.renumbered.drain(..) {
            speeds.renumber(1 - self.side, &moved);
        }
        speeds.digest(&events, b);
        self.begun.clear();
        self.items.clear();
        // Where a turn goes on after this (a Pokémon is replaced in the middle of it), what
        // was counted from one of the moves just digested holds from the next one on: a
        // Pokémon held back by Quash still is, one caught by an Encore still is, and a
        // Round has still been used.
        for side in 0..2 {
            for pos in 0..ACTIVE {
                let spot = &mut self.spots[side][pos];
                (spot.reordered, spot.overridden) = (spot.reordered.map(|_| 0), spot.overridden.map(|_| 0));
            }
        }
        for since in &mut self.rounds {
            *since = 0;
        }
        if b.request == Request::Move {
            self.chosen.clear();
        }
        self.timers = self.count_timers();
    }

    // ------------------------------------------------------- deciding

    /// Writes this player's observation into buffers of [`OBS_F`](crate::obs::OBS_F),
    /// [`OBS_I`] and [`OBS_M`], as [`crate::env::Game::observe`] does for a
    /// training game, and returns how many joint actions are legal: more
    /// than one means there is a choice to make.
    pub fn observe(&self, f: &mut [f32], i: &mut [i16], mask: &mut [u8]) -> Result<usize, String> {
        assert_eq!(mask.len(), OBS_M, "a mask buffer of the wrong size");
        let n = match &self.battle {
            None => {
                let rosters = self.rosters();
                if rosters[1 - self.side].len() != ROSTER {
                    return err("Team Preview has not shown six Pokémon on the other side");
                }
                let mut speeds = Speeds::new([&rosters[0], &rosters[1]], self.open());
                speeds.watch_as(self.side);
                observe_preview([&rosters[0], &rosters[1]], &speeds, &self.stats, self.open(), self.side, f, i);
                mask.fill(0);
                N_PREVIEW
            }
            Some(b) => {
                let speeds = self.speeds.as_ref().ok_or("no request has been taken in")?;
                observe_battle(b, &self.timers, speeds, &self.stats, self.side, f, i);
                masks_of(b, self.side, mask)
            }
        };
        let info = &mut i[OBS_I - INFO..];
        info[1] = n.min(i16::MAX as usize) as i16;
        info[3] = (n > 1) as i16;
        Ok(n)
    }

    /// The reply to send Showdown (after `/choose `) for a pair of actions
    /// (at Team Preview, the first of the pair alone counts).
    pub fn choice(&mut self, actions: [usize; 2]) -> Result<String, String> {
        // (A replacement sent in while a turn is under way leaves the moves chosen for
        // that turn as they were: some have yet to be used.)
        if !self.req.as_ref().is_some_and(|r| matches!(r.asked, Asked::Switch | Asked::Wait)) {
            self.chosen.clear();
        }
        self.sent = [None; ACTIVE];
        let Some(b) = &self.battle else {
            let pick = preview_table().get(actions[0]).ok_or("not a Team Preview action")?;
            // The four brought, then the two left behind: Showdown takes the first four.
            return Ok(format!("team {}", pick.iter().map(|j| (j + 1).to_string()).collect::<String>()));
        };
        let mut out = Vec::new();
        for (pos, &action) in actions.iter().enumerate() {
            let choice = choice_of(b, self.side, action).ok_or_else(|| format!("{action} is not an action"))?;
            if !b.legal_choices(self.side, pos).contains(&choice) {
                return err(format!("action {action} is not open to position {pos}"));
            }
            if let Choice::Move { slot, target, .. } = choice {
                let r = b.active(self.side, pos);
                let m = b.mon(r);
                let id = if m.locked_move != NO_MOVE {
                    m.locked_move
                } else if !b.usable_moves(r) {
                    mv::STRUGGLE
                } else {
                    m.moves[slot as usize].id
                };
                self.chosen.push((r.idx as usize, id, target));
            }
            if let Choice::Switch { to } = choice {
                self.sent[pos] = Some(b.sides[self.side].order[to as usize] as usize);
            }
            out.push(choice.to_showdown());
        }
        Ok(out.join(", "))
    }
}
