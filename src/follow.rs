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
use crate::env::{Counts, GameState, N_PREVIEW, OBS_M, Untold, choice_of, masks_of, preview_table};
use crate::format::ShowdownSet;
use crate::obs::{
    INFO, OBS_I, Phase, ROSTER, TIMED_SLOTS, TIMED_VOLATILES, Timers, is_mega, observe_battle, observe_preview,
    slot_kept,
};
use crate::observer::{Observer, ident};
use crate::position::vol_holds_move;
use crate::shown::{ItemShown, NOTHING_SHOWN, Shown, ShownSide, UNKNOWN, hp_shown};
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
    /// The move it is about (an Encore's, a Disable's, a charging move's), where the log says: for
    /// [`Follower::game_state`], which the observation does not read. `NO_MOVE`: none, or not known.
    about: u16,
    /// For [`Follower::game_state`] too: the turns it has left as the engine counts them (its
    /// `duration`), where it has a set length the log does not end; 0 if none or not known.
    left: u8,
    /// For [`Follower::game_state`] as well: the Pokémon that put it up (side, team index) and
    /// the place it stood, where the engine reads it (Leech Seed heals whoever stands there; Yawn's,
    /// Attract's, a binding move's user). Kept apart from `source`, by which the follower ends what
    /// a Pokémon leaving was keeping up.
    by: Option<(usize, usize, usize)>,
    /// Stockpile's stages to give back, Defense and Special Defense (the engine's `st.a`, `st.b`).
    given: (i16, i16),
    /// When it went up, in the order of what has happened to this Pokémon during its stay
    /// (`Spot::next_seq`): the engine keeps its conditions in that order.
    seq: u32,
}

impl Vol {
    /// The turns it has left as the engine counts them (its `duration`), where the follower
    /// knows: a set length counted down since the log began it, or one it ends by itself; 0
    /// if none or not known.
    fn turns_left(&self) -> u8 {
        match self.ends {
            _ if self.left > 0 => self.left,
            Ends::After(n) => n,
            _ => 0,
        }
    }
}

/// A frozen Pokémon's `status_time`, as the engine keeps it: three attempts at most, less those
/// made (`conditions.rs`, Champions).
fn frozen_time(frozen: u8) -> u8 {
    3u8.saturating_sub(frozen).max(1)
}

/// One position on the field: what holds for whoever stands there, from the log.
#[derive(Clone, Debug)]
struct Spot {
    /// A number for this stay on the field: a new one every time a Pokémon comes in.
    stay: u32,
    boosts: [i8; 7],
    vols: Vec<Vol>,
    /// How far its confusion and a binding move on it have run, as [`Counts`] keeps them (D97: every
    /// run the log has shown, the turn each began included). Kept with the stay, not by team index:
    /// two Pokémon on the field can go by one name (Illusion).
    confused: u8,
    bound: u8,
    /// Cud Chew's berry, to be eaten again at a turn's end (its countdown: one at any decision a search
    /// starts from, the turn's end having come since it was eaten), and whether its `-activate` has just
    /// said it is eaten again (that eating sets no last item).
    cud: Option<u16>,
    cud_again: bool,
    /// Where its charging move is aimed when another move called it (Copycat): at one of the other side's
    /// Showdown drew then (`getRandomTarget`), which the charging turn's line does not name; `Some(None)`
    /// with two standing, which its player is not told (D89; [`Untold::ChargeTarget`]).
    charge_aim: Option<Option<i8>>,
    /// Its sleep this stay under the name it goes by (D96): that name's count before, the runs it made,
    /// whether the sleep began this stay, and the name's number (`their_name`; its own number, where it is
    /// not taken for the copy of a look-alike beside it). A disguise told (`replace`) takes them to its own
    /// number.
    sleep_here: Option<(u8, u8, bool, u8)>,
    /// The position it stood in as the turn began, where its action was resolved (`Battle#resolveAction`): a
    /// non-Ghost Curse is aimed there, though an Ally Switch has moved it since.
    turn_pos: Option<usize>,
    /// Whether it was a Ghost as the turn began, as far as this side can see: what decides where its Curse goes
    /// (`Battle#resolveAction`, before a Soak or a Mega Evolution this turn changes it).
    turn_ghost: Option<bool>,
    /// Its Berserk was last struck by a confusion self-hit, which leaves its berry check off (Showdown's
    /// `checkedBerserk` false: the engine's `ability_extra.a` 2) until any other blow.
    berserk_off: bool,
    /// Its types are not on show (Showdown's `knownType` false): it took a foe's with Reflect Type, and has
    /// not been attacked at a turn's end since (nor had its types set otherwise, which shows them).
    known_off: bool,
    /// Its Trace is looking for an ability to take (the engine's `ability_extra.a`, Showdown's `seek`): set
    /// as it starts, unless a foe on the field then has no ability; worked out at the next request, once
    /// everyone coming in has (`trace_due`).
    trace_seek: bool,
    /// Who of the other side had no ability as it came in, where that is when its Trace started (`onStart`, in its
    /// own `runSwitch`): none for a lead, whose Trace starts with the rest of the leads in.
    trace_blank: Option<bool>,
    /// Its Ripen's `berryWeaken`: it ate a resist berry outside a hit (one flung at it), which halves the next hit's
    /// damage once more.
    ripen_weaken: bool,
    trace_due: bool,
    /// It came in by Baton Pass from a Pokémon under Gastro Acid (the engine's `copied_gastro`).
    copied_gastro: bool,
    /// Its Emergency Exit has gone off (its `switchFlag`, said by `-activate|X|ability: Emergency Exit`).
    exiting: bool,
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
    /// Its Disguise broke during this stay (the ability's state `a`, which a later stay starts afresh).
    busted: bool,
    /// What this stay has filed under the number it went by, for [`Follower::game_state`]: the other
    /// side's PP spent (and given back), the item used up (and what was filed before), the first move
    /// used (and when), the doubts over Pressure's PP (by move, how many). If a disguise drops (`|replace|`),
    /// it is all its own: the Zoroark spent the PP, used the item, and its Choice item held it to that move.
    spent: Vec<(u16, i16)>,
    doubted: Vec<(u16, u8)>,
    used: Option<((u16, bool), (u16, bool))>,
    first: Option<(u16, u32)>,
    /// It has used the move a rampage locks it into this turn (or begun one). A rampage is renewed
    /// only by its move: a turn without it (a confusion self-hit, a `cant`) lets it lapse. And when
    /// the rampage began (its lock goes up then).
    rampage_moved: bool,
    rampage_seq: u32,
    /// Its move was put ahead or behind by another (After You, Quash, Round), as of this many moves begun.
    reordered: Option<usize>,
    /// A Speed Swap has been used while it has stood here: its Speed may not be its own.
    swap_seen: bool,
    /// This stay it has split or swapped stats with a Pokémon whose stats this side cannot know (two of the
    /// other side's, or one of its own with one of the other side's: Power Split, Guard Split, Speed Swap),
    /// so its stats come from a spread this side does not know ([`Untold::StatsShared`]).
    stats_shared: bool,
    /// Its own: its stats this stay where they are not its sheet's for its forme (Power Trick's swap, a split
    /// or swap with another of its own), as Showdown keeps them (`storedStats`); and whether Baton Pass handed
    /// it a Power Trick, which swaps the sheet's. A forme change works them out afresh (`setSpecies`).
    stored: Option<[u16; 6]>,
    trick_copied: bool,
    genuine_form: bool,
    /// Its move this turn may not be the one it chose: an Encore came first, as of this many moves begun.
    overridden: Option<usize>,
    /// Its move went ahead of its bracket, and said so (Quick Claw, Quick Draw, Custap Berry).
    quick: bool,
    /// The same for the move an Encore put in its place: Champions' Encore resolves that move
    /// afresh, and its Quick Claw is rolled again (and says so) after the Encore's line.
    quick_again: bool,
    /// It has been seen to lose an item since it came in (Unburden asks), and when (an Unburden's
    /// condition goes up then).
    lost_item: bool,
    unburden_seq: u32,
    /// Something has been said of its ability since the last request.
    ability_touched: bool,
    /// Own ability knowledge after an allied swap, before the next exact request.
    own_abilities: Option<Vec<u16>>,
    /// The other side's: the ability it has had this stay since a Skill Swap with its partner,
    /// which Showdown does not name (`|-activate|p2a: A|Skill Swap|||[of] p2b: B`), as this side
    /// can work it out: what the partner was shown to have, or else its sheet's (its Mega's).
    their_ability: Option<u16>,
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
    /// Queued moves it has begun since it came in, a `cant` included: the engine's
    /// `active_move_actions` (Fake Out works while that is at most 1).
    actions: u8,
    // What the engine keeps of its stay that the observation does not read, for
    // [`Follower::game_state`]: the moves it has paid PP for (Last Resort), the hits it has
    // taken from moves (Rage Fist counts them; a substitute's too), whether it has been hit,
    // the damage of the last hit (exact for the follower's own, which it is shown), the latest
    // hit from each of the other side's Pokémon still on the field (whose number, from where,
    // how much), how its move went this turn and the turn before (`None`: it did not move;
    // Stomping Tantrum asks), where its last move was aimed, and whether its stats were
    // lowered or raised this turn (a lead's Intimidate counts into the first turn).
    paid: Vec<u16>,
    hits: u16,
    attacked: bool,
    /// Who has aimed a move at it that reached it, and stayed on the field since (bit `side *
    /// MAX_TEAM + team index`): the engine's `TypesSaid::attacked_by`; and by bit, the stay of the
    /// attacker that set it (a disguise dropping takes only its own stay's, `refile_disguise`).
    attacked_by: u16,
    attacked_stay: [u32; 2 * MAX_TEAM],
    last_damage: i32,
    /// The hits it took from the other side, as the engine's `damaged_by`: who (by number), from where,
    /// how much, and the stay of the attacker that landed it.
    damaged_by: Vec<(usize, usize, u16, u32)>,
    result_now: Option<Option<bool>>,
    result_last: Option<Option<bool>>,
    /// This turn's result and last turn's are untold (D89; [`Untold::MoveResult`]): a status move of the
    /// other side's whose line named no target failed with no word of why, with a Substitute up on some of
    /// the Pokémon it may have been aimed at (taken by one, the move worked).
    result_doubt: [bool; 2],
    aimed: i8,
    lowered: bool,
    raised: bool,
    /// It has used up an item this turn (one used as the battle began counts into the first).
    used_item: bool,
    /// Its Unnerve has announced itself during this stay (the ability's state `a`).
    unnerved: bool,
    /// The move its Choice item holds it to, for [`Follower::game_state`]: as `choice`, but kept
    /// when an item is only shown (a Poltergeist, a Frisk), and let go only when the item goes or
    /// another comes. And how many conditions it had when the lock took hold (it comes after those).
    lock: Option<u16>,
    choice_seq: u32,
    /// The last number given out by `next_seq`.
    seq: u32,
    /// Its side's fainted when it came in (Supreme Overlord counts them, at most five), and
    /// whether its Protean or Libero has changed its type during this stay (once a stay).
    fallen: u8,
    protean: bool,
}

impl Spot {
    fn new(stay: u32) -> Spot {
        Spot {
            stay,
            boosts: [0; 7],
            vols: Vec::new(),
            confused: 0,
            bound: 0,
            cud: None,
            cud_again: false,
            charge_aim: None,
            turn_ghost: None,
            sleep_here: None,
            turn_pos: None,
            berserk_off: false,
            known_off: false,
            trace_seek: false,
            trace_blank: None,
            ripen_weaken: false,
            trace_due: false,
            copied_gastro: false,
            exiting: false,
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
            rampage_moved: false,
            rampage_seq: 0,
            busted: false,
            spent: Vec::new(),
            doubted: Vec::new(),
            used: None,
            first: None,
            reordered: None,
            swap_seen: false,
            stats_shared: false,
            stored: None,
            trick_copied: false,
            genuine_form: false,
            overridden: None,
            quick: false,
            quick_again: false,
            lost_item: false,
            unburden_seq: 0,
            ability_touched: false,
            own_abilities: None,
            their_ability: None,
            item_since: None,
            pressing: false,
            choice: None,
            no_hold: false,
            hit_by: Vec::new(),
            instructed: false,
            forme: None,
            last_two: false,
            actions: 0,
            paid: Vec::new(),
            hits: 0,
            attacked: false,
            attacked_by: 0,
            attacked_stay: [0; 2 * MAX_TEAM],
            last_damage: 0,
            damaged_by: Vec::new(),
            result_now: None,
            result_doubt: [false; 2],
            result_last: None,
            aimed: 0,
            lowered: false,
            raised: false,
            used_item: false,
            unnerved: false,
            lock: None,
            choice_seq: 0,
            seq: 0,
            fallen: 0,
            protean: false,
        }
    }

    fn has(&self, kind: VolKind) -> bool {
        self.vols.iter().any(|v| v.kind == kind)
    }

    /// The next number in the order of what happens to it during its stay (see `Vol::seq`).
    fn next_seq(&mut self) -> u32 {
        self.seq += 1;
        self.seq
    }

    fn add(&mut self, kind: VolKind, ends: Ends) -> bool {
        if self.has(kind) {
            return false;
        }
        let seq = self.next_seq();
        self.vols.push(Vol {
            kind,
            ends,
            data: 0,
            duration: 0,
            source: 0,
            about: NO_MOVE,
            left: 0,
            by: None,
            given: (0, 0),
            seq,
        });
        true
    }

    fn remove(&mut self, kind: VolKind) {
        self.vols.retain(|v| v.kind != kind);
    }

    /// A protecting move worked: the next one in a row is a third as likely to.
    fn stalled(&mut self) {
        if let Some(v) = self.vols.iter_mut().find(|v| v.kind == VolKind::Stall) {
            (v.data, v.ends) = ((v.data * 3).min(729), Ends::After(2));
            return;
        }
        let seq = self.next_seq();
        self.vols.push(Vol {
            kind: VolKind::Stall,
            ends: Ends::After(2),
            data: 3,
            duration: 0,
            source: 0,
            about: NO_MOVE,
            left: 0,
            by: None,
            given: (0, 0),
            seq,
        });
    }

    /// PP of the other side's spent (or, negative, given back) during this stay.
    fn spend(&mut self, id: u16, n: i16) {
        match self.spent.iter_mut().find(|s| s.0 == id) {
            Some(s) => s.1 += n,
            None => self.spent.push((id, n)),
        }
    }

    /// It has lost an item: an Unburden's condition goes up now, after those it has (Unburden's
    /// `onTakeItem` and `onAfterUseItem`), and stays while the ability does, whatever item comes after.
    fn lose_item(&mut self) {
        if !self.lost_item {
            let seq = self.next_seq();
            (self.lost_item, self.unburden_seq) = (true, seq);
        }
    }

    /// Ally Switch's own condition, as Protect's run is: each repeat a third as likely to work
    /// (`allyswitch`: a counter of 3, tripled on each repeat up to 729, kept two turns).
    fn ally_switched(&mut self) {
        if let Some(v) = self.vols.iter_mut().find(|v| v.kind == VolKind::Allyswitch) {
            (v.data, v.ends) = ((v.data * 3).min(729), Ends::After(2));
            return;
        }
        let seq = self.next_seq();
        self.vols.push(Vol {
            kind: VolKind::Allyswitch,
            ends: Ends::After(2),
            data: 3,
            duration: 0,
            source: 0,
            about: NO_MOVE,
            left: 0,
            by: None,
            given: (0, 0),
            seq,
        });
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
/// The most stat points one stat may carry in Champions (the format's cap per stat).
const MAX_HP_POINTS: u8 = 32;

pub(crate) const CHARGING: [VolKind; 10] = [
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

/// The kinds of protocol line the follower or its reader takes something from.
const READ: [&str; 59] = [
    "-ability", "-activate", "-anim", "-block", "-boost", "-clearallboost", "-clearboost", "-clearnegativeboost",
    "-clearpositiveboost", "-copyboost", "-curestatus", "-damage", "-end", "-endability", "-enditem", "-fail",
    "-fieldactivate", "-fieldend", "-fieldstart", "-formechange", "-heal", "-immune", "-invertboost", "-item", "-mega",
    "-miss", "-mustrecharge", "-prepare", "-setboost", "-sethp", "-sideend", "-sidestart", "-singlemove",
    "-singleturn", "-start", "-status", "-swapboost", "-swapsideconditions", "-transform", "-unboost", "-waiting",
    "-weather", "cant", "clearpoke", "detailschange", "drag", "faint", "move", "player", "poke", "replace", "showteam",
    "start", "swap", "switch", "teamsize", "tie", "turn", "upkeep",
];

/// The kinds it passes over, and why there is nothing in them to take (D84). Any other kind
/// is counted ([`Follower::unknown`]) rather than ignored.
const PASSED_OVER: [(&str, &str); 26] = [
    ("", "a blank line between parts of a turn"),
    ("split", "a private line and its public copy follow; each side is given its own (`Follower::own_lines`)"),
    ("t:", "the time"),
    ("gametype", "the format, known beforehand"),
    ("gen", "the format, known beforehand"),
    ("tier", "the format, known beforehand"),
    ("rule", "the format, known beforehand"),
    ("rated", "the format, known beforehand"),
    ("title", "the room"),
    ("teampreview", "Team Preview begins; the request says what to choose"),
    ("j", "a user joins the room"),
    ("l", "a user leaves the room"),
    ("n", "a user changes name"),
    ("c", "chat"),
    ("raw", "a message for people"),
    ("html", "a message for people"),
    ("uhtml", "a message for people"),
    ("uhtmlchange", "a message for people"),
    ("-supereffective", "how well a move hit: the damage line has what it did"),
    ("-resisted", "how well a move hit: the damage line has what it did"),
    ("-crit", "how well a move hit: the damage line has what it did"),
    ("-hitcount", "how many times a move hit: each hit's damage line is there"),
    ("-hint", "a message for people"),
    ("-ohko", "a one-hit KO: the damage and faint lines say it"),
    ("-zbroken", "a hit through a protection (Piercing Drill): the ability line before it and the damage after say it"),
    ("inactive", "the battle timer"),
];

/// Whether the follower knows what to make of a kind of protocol line: something to take, or
/// nothing on purpose.
fn known_kind(kind: &str) -> bool {
    READ.contains(&kind) || PASSED_OVER.iter().any(|(k, _)| *k == kind)
}

#[derive(Clone, Debug, Default)]
struct SideTrack {
    /// Each condition, how many layers of it, and how many turns have ended since it went up.
    conds: Vec<(SideCond, u8, u16)>,
    /// The screens of those that went up from a Pokémon holding Light Clay: eight turns, not five.
    lasting: Vec<SideCond>,
    /// Conditions on each position, with how many more turns' ends they last (0: until said).
    slots: [Vec<(SlotCond, u8, Option<Maker>)>; ACTIVE],
    /// By position: Ally Switch has moved a Pokémon onto its Healing Wish and the wish stayed, healing nothing:
    /// its `onSwap` has run inside an event (`runEvent('Swap')`) with that Pokémon as holder, which marks the
    /// condition (`targeted`), and since then the engine passes it over for whoever switches in there
    /// (Showdown's slip; only another Ally Switch brings it into play). A Pokémon switched or dragged in meets it
    /// through `fieldEvent('SwitchIn')`, whose `singleEvent` leaves no mark.
    wish_met: [bool; ACTIVE],
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
    /// How many turns the weather and the terrain were set up for: eight if whoever set them held
    /// the rock for that weather or a Terrain Extender, else five.
    weather_len: u8,
    terrain_len: u8,
}

/// One Pokémon of the follower's own, of those it brought.
/// Who made a slot condition (Wish, Healing Wish, Revival Blessing, a future move): side, team index
/// (as [`Follower::game_state`] numbers it) and the field slot it stood in.
#[derive(Clone, Copy, Debug)]
struct Maker {
    side: usize,
    idx: usize,
    slot: usize,
}

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
    quick_again: bool,
    seen: [Seen; 4],
    field: FieldSeen,
    /// The follower's own two at that moment: which stay, and what it knew of each.
    own: [(u32, Option<OwnNow>); ACTIVE],
}

impl Begun {
    /// Whether its move stood ahead of its bracket when the `at`th move of the stretch began: the
    /// one it chose by a roll said before an Encore replaced it, the replacement by its own roll.
    fn quick_at(&self, at: usize) -> bool {
        match self.overridden {
            Some(t) if t <= at => self.quick_again,
            _ => self.quick,
        }
    }
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
    animation_pp: Option<(usize, bool, u16, u8, bool)>,
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
    /// The turn's end has begun (`|upkeep|`) and the next `|turn|` has not come: a switch
    /// request now replaces the fainted, rather than coming in the middle of the turn.
    upkept: bool,
    /// By side and team index (the follower's own numbering for its side, the reader's for the
    /// other): the status a Pokémon fainted with and its toxic stage. Showdown's `checkFainted`
    /// clears a fainted Pokémon's status only on a side with someone to send in (since
    /// `4379269`), so one that stays where it fell keeps it. Nothing reads it; the game has it.
    faint_status: [[Option<(Status, u8)>; MAX_TEAM]; 2],
    /// By side: the Pokémon sent in to replace the fainted since the turn's `upkeep`. Showdown asks for
    /// them only on a side with someone to send in, which is `checkFainted`'s condition, judged before
    /// they came in (R2's gate 3 at `63752e0`, game 3974: Perish Song felled both of a side at the
    /// turn's end, after `upkeep`, with one left to send in; the one asleep kept its sleep in the
    /// export, counted once the replacement stood beside it).
    arrived: [u8; 2],
    /// How far the effects with a hidden length have run, as the log counts them (see
    /// [`Counts`]), by side and the team index of [`Follower::game_state`].
    counts: Counts,
    /// By side: a Pokémon fainted this turn, and in the turn before (Retaliate reads it).
    fainted_now: [bool; 2],
    fainted_last: [bool; 2],
    /// The PP the other side's Pokémon have spent, by the number each goes by in the order of
    /// appearance: move and PP (Pressure's included).
    their_pp: Vec<Vec<(u16, u8)>>,
    /// The last move that ran, either side's (Copycat's pick); `NO_MOVE` if none yet.
    last_move: u16,
    /// By side and the team index of [`Follower::game_state`]: the item each last used up or
    /// ate (Recycle), and whether that was a berry it ate (Belch).
    used_up: [[(u16, bool); MAX_TEAM]; 2],
    /// By side and team index: it has eaten a berry this battle (Belch asks; Showdown never clears
    /// `ateBerry`), its own or one it stole and ate (Bug Bite, Pluck).
    ate: [[bool; MAX_TEAM]; 2],
    /// By side and team index: its Supersweet Syrup has gone off, which it does once a battle (the
    /// engine's `syrup_triggered`; Showdown's `syrupTriggered`, set as the ability says so).
    syrup: [[bool; MAX_TEAM]; 2],
    /// By side and team index: the move a rampage locks it into, while it does.
    rampage: [[u16; MAX_TEAM]; 2],
    /// By side and team index: the turns' ends a rampage has seen since it began. It lasts two or three
    /// turns, whenever the decisions fall (one in the middle of its first turn, a U-turn's, moves its count).
    rampage_turns: [[u8; MAX_TEAM]; 2],
    /// By side and team index: the turns it has tried to move while frozen (Champions thaws a
    /// Pokémon after three at most, so what is left of a freeze is public).
    frozen: [[u8; MAX_TEAM]; 2],
    /// By side and team index: its rampage's second turn was used where the end it may then have
    /// come to would have been told to nobody (the confusion it brings taken silently): the lock it
    /// is kept in for a third turn may be over (D89, D94; [`Untold::Rampage`]).
    rampage_silent: [[bool; MAX_TEAM]; 2],
    /// The other side's PP paid to Pressure behind a blanked target, one each or two (D89, D94;
    /// [`Untold::PressurePp`]): by the number each goes by, the move and how many times.
    pressure_doubt: Vec<(usize, u16, u8)>,
    /// By own team index: where Champions' Encore's draw sent its last move is not told (D89, D94;
    /// [`Untold::EncoreTarget`]), until it moves again.
    encore_doubt: [bool; MAX_TEAM],
    /// By own position: an Encore put its move in again this turn before it moved.
    encore_queued: [Option<EncoreQueued>; ACTIVE],
    /// The own position whose move, put in again by an Encore with both of the other side standing
    /// and none fallen since, has a line that names nobody: a later line of the move may say where.
    encore_unnamed: Option<usize>,
    /// Pickup took the item two Pokémon had used up this turn from one of them (`this.sample`), and the log
    /// names neither (D89; [`Untold::ItemTaken`]): the two (side, number) and the item, while both keep it
    /// as their last item here.
    taken_doubt: Vec<((usize, usize), (usize, usize), u16)>,
    /// A Substitute has taken the status move under way at one of its targets (a spread move's `-fail`).
    subs_took: bool,
    /// This line is a move a Choice lock refuses (`choicelock`'s `onBeforeMove`, which writes the move's line,
    /// blanked, and `-fail`): chosen while Magic Room kept the lock asleep, and the room gone before it moved. It
    /// never runs: no PP, no last move, no `TryMove`.
    refused: bool,
    /// The move under way has turned Berserk's berry check back on at its targets (once, as the next action
    /// begins).
    berserk_done: bool,
    /// The move under way is Dragon Darts and its darts have yet to be settled ([`Follower::hits_settle`]), and the
    /// place its line named.
    darts_open: bool,
    darts_aim: Option<(usize, usize)>,
    /// The move under way is one whose later hits can miss (Triple Axel, Population Bomb), and a later hit missed
    /// (`-hitcount` fewer than its most): Showdown has put its targets back in the hit's list before the roll, so the
    /// hit loop's last list has every target, one a substitute took the hits for too.
    hits_cut: bool,
    /// The move under way was sent back by Magic Bounce or Magic Coat: at the user of the one it came from.
    bounced_at: Option<(usize, usize)>,
    /// The move under way is Copycat, and the move it copies (the battle's last before it).
    copycat_copies: Option<u16>,
    /// This line is a target's Magic Bounce sending the move under way back, stopped at once by a condition of its own
    /// (`cant|T|move: Throat Chop`, `useMove`'s `ModifyMove` before any line of the move): no action of its own.
    bounce_stopped: bool,
    /// The move under way was called by another (Copycat, Sleep Talk): aimed at random, `getRandomTarget`, which
    /// its own side is not told either.
    called_now: bool,
    /// The last line was a Synchronize passing a status on (`-activate|X|ability: Synchronize`): a `-fail` of that
    /// status next is its, not the move's (the status was already there).
    synced: bool,
    /// The move under way is aimed at its own user (on itself, its side's Pokémon, or itself by choice), which the
    /// hit loop then counts among its targets.
    self_aimed: bool,
    /// Exact HP of the other side's this side has worked out (a Pain Split or an Endeavor with one
    /// of its own, whose HP it is told exactly), with the percentage and bar it showed then: by the
    /// number each goes by ([`crate::env::HpSeen`]).
    /// (And the stay that saw it: a disguise that drops takes its own, `refile_disguise`.)
    hp_seen: Vec<(usize, u16, u8, crate::shown::Bar, u32)>,
    /// The other side's: those of them whose HP that still is (the number, the HP).
    hp_now: Vec<(usize, u16)>,
    /// A Pain Split's two `-sethp` lines, the first until the second comes.
    pain_split: Option<(usize, usize, String)>,
    /// An Endeavor under way (its user, and the place it was aimed at), until its hit or the next move.
    endeavor: Option<((usize, usize), (usize, usize))>,
    /// The HP of the follower's own Pokémon at each position, as its log last gave it exactly.
    own_hp: [Option<u16>; ACTIVE],
    /// The move under way (its user), and what it has come to so far at the Pokémon it was aimed
    /// at: hit, missed or failed, blocked by a protection. Each of those it reached, with what was
    /// kept of it before (attacked, by whom), to take back where it turns out not to have.
    under_way: Option<((usize, usize), bool, bool, bool)>,
    reached: Vec<((usize, usize), bool, u16)>,
    /// A rampage the move under way would begin (side, team index), until it is seen to land:
    /// one that hits nobody (an immune target, a miss) locks nobody in.
    rampage_pending: Option<(usize, usize)>,
    /// A move line that will reach `TryMove` unless the next line says otherwise (a charging turn's
    /// `-prepare` with nothing let off, Burn Up's failure, Armor Tail's or Damp's veto): who, the move,
    /// whether its charge was up, and whether a `-prepare` has come (for the Metronome item).
    try_pending: Option<((usize, usize), u16, bool, bool)>,
    /// The Pokémon whose move or `cant` line the last line was, if it was one: its place and its stay (Ally Switch
    /// may put another there before the next line), and whether it was a `cant`.
    moved_last: Option<((usize, usize), u32, bool)>,
    /// The move under way named no target (`[still]`: it failed with no animation).
    blanked: bool,
    /// The move under way's hits at each position: on the Pokémon, on its substitute, and whether the last
    /// fell on the substitute. Showdown adds every hit of a move to a target's `timesAttacked` (Rage Fist)
    /// as the move is done, if its last hit fell on the Pokémon itself (one, for Dragon Darts').
    hits_now: [[(u16, u16, bool); ACTIVE]; 2],
    /// A future move landing at a position, with who made it: its blow is its maker's (filed in the hit
    /// loop as any other, `timesAttacked`, the last blow and who attacked).
    future_hit: Option<((usize, usize), Maker)>,
    /// For a move whose one effect is to change its targets' stat stages: those it changed a stage of
    /// (Showdown's `boost` gives a target where nothing changed no success: the move fails there).
    boosted_at: Vec<(usize, usize)>,
    /// Its own leads' items have started (the Metronome item's condition): at its first request, the
    /// first time it knows what they hold.
    leads_started: bool,
    /// The two Pokémon of the last Trick or Switcheroo (user, target): each `-item` it gives one says
    /// the other's item was taken.
    tricked: Option<((usize, usize), (usize, usize))>,
    /// Hits each place has taken from the move under way.
    move_hits: [[u8; ACTIVE]; 2],
    /// By side and team index: the blow that knocked it out, if a move's (the attacker's side, number
    /// and place, the damage, the hits). Showdown records it after the faint has cleared the Pokémon's
    /// record, so one brought back by Revival Blessing comes in with it.
    ko_hit: [[Option<(usize, usize, usize, u16, u8)>; MAX_TEAM]; 2],
    /// Faints on each side so far (Showdown's `totalFainted`, which a revival does not take back):
    /// Last Respects and Supreme Overlord count them.
    faints: [u8; 2],
    /// The Pokémon that has just stockpiled: its Defense and Special Defense lines that follow say
    /// which stages it will give back.
    stockpiled: Option<(usize, usize)>,
    /// The move under way named its user's own place among its targets (Life Dew: `[spread]` with
    /// the user in it), so a `-fail` naming the user is its own share failing, not the move's.
    spread_self: bool,
    /// The next hit at this place did no damage (the blow fell on a Disguise).
    zero_hit: Option<(usize, usize)>,
    /// For a status move that only raises its user's stats: whether a stage it named changed
    /// (`Some(true)`), or every one it named was already at its limit (`Some(false)`: `boost`
    /// returns null, and the move fails).
    boosted: Option<bool>,
    /// The move under way puts up a condition on its user that it did not have (Charge, Minimize), which
    /// makes it work whatever its stages do.
    own_cond_new: bool,
    /// Protocol lines of a kind the follower does not know, by kind (D84): reported, not ignored.
    unknown: std::collections::BTreeMap<String, usize>,
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
/// A move whose effect prints a failure and then returns `NOT_FAIL`, which Showdown counts as the move
/// working (by the failure's form: Ally Switch failing its odds is a failure, failing for want of a
/// partner is not).
/// A status move whose effects are to change its targets' stat stages or give them a status, and nothing
/// else (Scary Face, Sweet Scent, Toxic Thread): it works at a target only where a stage moves or the
/// status lands.
fn only_boosts(m: u16) -> bool {
    MOVES.get(m as usize).is_some_and(|d| {
        d.category == Category::Status
            && d.boosts.is_some()
            && d.volatile.is_none()
            && d.self_volatile.is_none()
            && d.self_boosts.is_none()
            && d.events == 0
            && !matches!(d.target, Target::User | Target::AllySide | Target::AllyTeam | Target::All | Target::FoeSide)
    })
}

/// Whether a status move `m` has failed at a target where something was kept off it: unless a stage it
/// changed there (`moved`) has already made it work and what was kept off is not its own status.
fn status_kept_off(m: u16, moved: bool) -> bool {
    MOVES.get(m as usize).is_some_and(|d| d.category == Category::Status && (!moved || d.status != Status::None))
}

pub(crate) fn not_fail_on_hit(m: u16, form: &str) -> bool {
    let Some(d) = MOVES.get(m as usize) else { return false };
    matches!(
        (d.id, form),
        ("allyswitch", "move: Ally Switch")
            | ("purify" | "doodle", "")
            | ("corrosivegas", "move: Corrosive Gas")
            | ("swallow" | "healpulse" | "floralhealing" | "pollenpuff", "heal")
            | ("moonlight" | "morningsun" | "synthesis" | "shoreup", "heal")
    )
}

/// An own Pokémon's move Champions' Encore put in again before it moved, with no target: Showdown
/// draws one (`BattleQueue#resolveAction`).
#[derive(Clone, Copy, Debug)]
struct EncoreQueued {
    /// One of the other side has fainted since.
    fell: bool,
    /// The place drawn when it could be only one (one of the other side standing, or none).
    forced: Option<i8>,
}

/// An ability that takes in a move aimed at its holder (`onTryHit` returns null, the move fails
/// there): it says so by boosting or healing, or Flash Fire by starting.
fn absorbs(ability: &str) -> bool {
    matches!(
        to_id(ability).as_str(),
        "flashfire"
            | "waterabsorb"
            | "voltabsorb"
            | "dryskin"
            | "eartheater"
            | "lightningrod"
            | "stormdrain"
            | "motordrive"
            | "sapsipper"
            | "wellbakedbody"
            | "windrider"
    )
}

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
                weather_len: 5,
                terrain_len: 5,
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
            animation_pp: None,
            chosen: Vec::new(),
            sent: [None; ACTIVE],
            megaed: [false; 2],
            ending: false,
            renumbered: Vec::new(),
            rounds: Vec::new(),
            roused: None,
            upkept: false,
            faint_status: [[None; MAX_TEAM]; 2],
            arrived: [0; 2],
            counts: Counts::default(),
            fainted_now: [false; 2],
            fainted_last: [false; 2],
            their_pp: Vec::new(),
            last_move: NO_MOVE,
            used_up: [[(it::NONE, false); MAX_TEAM]; 2],
            ate: [[false; MAX_TEAM]; 2],
            syrup: [[false; MAX_TEAM]; 2],
            rampage: [[NO_MOVE; MAX_TEAM]; 2],
            rampage_turns: [[0; MAX_TEAM]; 2],
            frozen: [[0; MAX_TEAM]; 2],
            rampage_silent: [[false; MAX_TEAM]; 2],
            pressure_doubt: Vec::new(),
            encore_doubt: [false; MAX_TEAM],
            encore_queued: [None; ACTIVE],
            encore_unnamed: None,
            taken_doubt: Vec::new(),
            subs_took: false,
            refused: false,
            berserk_done: false,
            darts_open: false,
            darts_aim: None,
            self_aimed: false,
            synced: false,
            bounced_at: None,
            copycat_copies: None,
            bounce_stopped: false,
            called_now: false,
            hits_cut: false,
            hp_seen: Vec::new(),
            hp_now: Vec::new(),
            pain_split: None,
            endeavor: None,
            own_hp: [None; ACTIVE],
            under_way: None,
            reached: Vec::new(),
            rampage_pending: None,
            try_pending: None,
            moved_last: None,
            blanked: false,
            hits_now: [[(0, 0, false); ACTIVE]; 2],
            future_hit: None,
            boosted_at: Vec::new(),
            own_cond_new: false,
            leads_started: false,
            tricked: None,
            move_hits: [[0; ACTIVE]; 2],
            ko_hit: [[None; MAX_TEAM]; 2],
            faints: [0; 2],
            stockpiled: None,
            spread_self: false,
            zero_hit: None,
            boosted: None,
            unknown: std::collections::BTreeMap::new(),
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

    /// Protocol lines taken in so far of a kind the follower does not know, by kind (D84).
    pub fn unknown(&self) -> &std::collections::BTreeMap<String, usize> {
        &self.unknown
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
    /// What this side has been shown of `side`'s Pokémon (the record the
    /// observation reads: HP percentage and bar, status, moves, item, ability).
    pub fn shown(&self, side: usize) -> ShownSide {
        self.reader.shown(side)
    }

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
        !self.spots[side][pos].no_hold
            && self.item_at_work(side, pos).is_some_and(|item| ITEMS[item as usize].flags & IF_CHOICE != 0)
    }

    /// The item the Pokémon at `side`'s `pos` holds where it is at work (not under Magic Room, not with
    /// Klutz), as this side knows it; `None` if it holds none, or this side cannot tell.
    fn item_at_work(&self, side: usize, pos: usize) -> Option<u16> {
        if self.field.pseudo.iter().any(|p| p.0 == Pseudo::Magicroom) {
            return None;
        }
        self.item_known(side, pos).filter(|&(item, klutz)| item != it::NONE && !klutz).map(|(item, _)| item)
    }

    /// Whether the Pokémon at `side`'s `pos` holds a Choice item, at work or not: a Choice lock made
    /// before Magic Room or Klutz stays, asleep, while the item is a Choice item (Showdown's
    /// `choicelock` checks `getItem().isChoice`, and only its `onDisableMove` minds `ignoringItem`).
    fn choice_held(&self, side: usize, pos: usize) -> bool {
        !self.spots[side][pos].no_hold
            && self.item_known(side, pos).is_some_and(|(item, _)| ITEMS[item as usize].flags & IF_CHOICE != 0)
    }

    /// The item this side knows the Pokémon at `side`'s `pos` to hold, and whether it may have Klutz.
    fn item_known(&self, side: usize, pos: usize) -> Option<(u16, bool)> {
        Some(if side == self.side {
            match self.own_now(pos) {
                Some(now) => (now.item, now.abilities.contains(&ab::KLUTZ)),
                None => return None,
            }
        } else {
            let s = self.seen(side, pos);
            let Some(listed) = self.reader.sides[side].roster.get(s.listed as usize).filter(|_| s.present) else {
                return None;
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
                ItemShown::Unknown => return None,
            };
            // (Since a Skill Swap with its partner, which names neither ability: the one worked out then.)
            let klutz = match self.spots[side][pos].their_ability {
                Some(a) if s.ability == UNKNOWN && !s.gastro_acid => a == ab::KLUTZ,
                _ => speed::may_have(&s, listed, self.open(), ab::KLUTZ),
            };
            (item, klutz)
        })
    }

    /// The Metronome item's condition: put up as the item starts on a Pokémon coming in or taking it
    /// (`items.ts` Metronome `onStart`; not where Klutz or Magic Room keeps the item from working).
    fn metronome_starts(&mut self, side: usize, pos: usize) {
        if self.item_at_work(side, pos) == Some(it::METRONOME) {
            self.spot(side, pos).add(VolKind::Metronome, Ends::Said);
        }
    }

    /// A move whose one effect is to change its targets' stat stages fails at each target it changed none
    /// of (all at their limit; kept off by Clear Body, Mirror Armor...): settled as the move ends (the next
    /// move, a Pokémon that cannot move, the turn's end), as `keep`'s `missed` would at each. Showdown's
    /// `boost` gives a target where no stage moved no success.
    fn boosts_settle(&mut self) {
        let Some((user, hit, _, blocked)) = self.under_way else { return };
        // Pollen Puff at an ally it does not heal (at full HP) leaves it out of the hit loop, without a word
        // (`onHit` returns `NOT_FAIL`): not attacked.
        if self.last_move == mv::POLLENPUFF {
            let unhealed: Vec<usize> = (0..self.reached.len())
                .filter(|&k| self.reached[k].0.0 == user.0 && !self.boosted_at.contains(&self.reached[k].0))
                .collect();
            for k in unhealed.into_iter().rev() {
                let ((s, p), attacked, by) = self.reached.remove(k);
                (self.spots[s][p].attacked, self.spots[s][p].attacked_by) = (attacked, by);
            }
        }
        if !only_boosts(self.last_move) {
            return;
        }
        let unmoved: Vec<usize> =
            (0..self.reached.len()).filter(|&k| !self.boosted_at.contains(&self.reached[k].0)).collect();
        if unmoved.is_empty() {
            return;
        }
        for k in unmoved.into_iter().rev() {
            let ((s, p), attacked, by) = self.reached.remove(k);
            (self.spots[s][p].attacked, self.spots[s][p].attacked_by) = (attacked, by);
        }
        self.under_way = Some((user, hit, true, blocked));
        // (A stage moved at its user, Howl's, makes it work too; so does a Substitute's share, whose null Champions
        // counts as a success.)
        let own = self.self_aimed && self.boosted == Some(true);
        let outcome = if hit || own || self.subs_took || !self.reached.is_empty() { Some(true) } else { Some(false) };
        self.spots[user.0][user.1].result_now = Some(outcome);
    }

    /// The move under way is done with its hits: each target whose last hit fell on it, not its substitute,
    /// has been attacked as many times as the move hit it, its substitute's share too (one for a Dragon
    /// Darts dart): Showdown's `timesAttacked += hit - 1` after the hit loop.
    fn hits_settle(&mut self) {
        // (Dragon Darts is `smartTarget` only where its darts went to two Pokémon; at one, two hits of a move.)
        let struck = self.hits_now.iter().flatten().filter(|h| h.0 + h.1 > 0).count();
        let darts = MOVES.get(self.last_move as usize).is_some_and(|d| d.smart_target);
        let smart = struck > 1 && darts;
        // (Dragon Darts whose darts struck nothing, not even a substitute, failed: immune where they went, or with
        // the one aimed at protected and its partner immune; and where one of the two was out of reach, Dig or Fly,
        // which `smartTarget` drops without a word as a failure, `hitStepInvulnerabilityEvent`, a Protect at the
        // other does not leave it at nothing. Settled once, as the move is done.)
        let unreachable = |me: &Follower, (s, p): (usize, usize)| {
            me.present(s, p)
                && me.spots[s][p].vols.iter().any(|v| {
                    matches!(v.kind, VolKind::Fly | VolKind::Bounce | VolKind::Dive | VolKind::Dig | VolKind::Phantomforce)
                })
        };
        let hidden_miss = self.darts_aim.is_some_and(|(s, p)| unreachable(self, (s, p)) || unreachable(self, (s, 1 - p)));
        if std::mem::take(&mut self.darts_open)
            && struck == 0
            && let Some((user, ..)) = self.under_way
        {
            let now = self.spots[user.0][user.1].result_now;
            if now == Some(Some(true)) || (now == Some(None) && hidden_miss) {
                self.spots[user.0][user.1].result_now = Some(Some(false));
            }
            // It reached nobody: no dart landed (two Protects, the first unsaid, come to nothing too).
            for ((s, p), attacked, by) in std::mem::take(&mut self.reached) {
                (self.spots[s][p].attacked, self.spots[s][p].attacked_by) = (attacked, by);
            }
        }
        // (Its user, for a dart a substitute took: `smartTarget` keeps every target the darts went to; and for hits a
        // substitute took before a later one missed.)
        let cut = std::mem::take(&mut self.hits_cut);
        let user = self.under_way.map(|u| u.0).filter(|_| smart || cut);
        for s in 0..2 {
            for p in 0..ACTIVE {
                let (on, sub, last_sub) = std::mem::take(&mut self.hits_now[s][p]);
                if on > 0 && !last_sub {
                    let spot = &mut self.spots[s][p];
                    spot.hits = spot.hits.saturating_add(if smart { 1 } else { on + sub });
                } else if let Some((us, up)) = user.filter(|_| on == 0 && sub > 0) {
                    // A dart its substitute took: still one of the move's targets after the hit loop (Dragon Darts
                    // keeps both, `targetsCopy = targets`), so attacked, once, by its user, for nothing. Hits its
                    // substitute took before a later one missed: attacked as many times, for nothing (`spreadDamage`
                    // gives a target taken out of the list 0).
                    let (by, stay) = (self.mon_ref(us, up).idx as usize, self.spots[us][up].stay);
                    let bit = 1 << (us * MAX_TEAM + by);
                    let spot = &mut self.spots[s][p];
                    (spot.hits, spot.attacked) = (spot.hits.saturating_add(if smart { 1 } else { sub }), true);
                    spot.attacked_by |= bit;
                    spot.attacked_stay[bit.trailing_zeros() as usize] = stay;
                    if us != s {
                        spot.damaged_by.retain(|d| d.0 != by);
                        spot.damaged_by.push((by, us * ACTIVE + up, 0, stay));
                    }
                }
            }
        }
    }

    /// Settles a move line's `TryMove` ([`Follower::try_pending`]) by the line after it: a charging turn
    /// (`-prepare`, its user's `-boost`, and no `-anim` to let it off: the move's own `onTryMove` returns
    /// null), Burn Up's or Double Shock's failure, Pollen Puff's Heal Block, or Armor Tail's, Queenly
    /// Majesty's or Damp's veto stop it before the item's handler; anything else lets it run.
    fn try_resolve(&mut self, kind: &str, parts: &[&str]) {
        let Some((at, used, charged, prepared)) = self.try_pending else { return };
        let arg = |i: usize| parts.get(i).copied().unwrap_or("");
        let me = |w: &str| self.place_of(w) == Some(at);
        let of = parts.iter().find_map(|p| p.strip_prefix("[of] ")).is_some_and(|w| me(w.trim()));
        let go = match kind {
            "-prepare" if me(arg(2)) => {
                self.try_pending = Some((at, used, charged, true));
                return;
            }
            "-boost" | "-enditem" if prepared && me(arg(2)) => return,
            "-anim" if prepared && me(arg(2)) => true,
            "-fail" if me(arg(2)) && matches!(arg(3), "move: Burn Up" | "move: Double Shock") => false,
            // (Pollen Puff's own `onTryMove`, its user under Heal Block. The same line from its target's Heal Block,
            // `onTryHeal`, comes after `TryMove`.)
            "cant" if me(arg(2)) && arg(3) == "move: Heal Block" && self.spots[at.0][at.1].has(VolKind::Healblock) => false,
            "cant" if of && matches!(arg(3), "ability: Armor Tail" | "ability: Queenly Majesty" | "ability: Damp") => {
                false
            }
            _ => !prepared,
        };
        self.try_pending = None;
        if go {
            self.metronome_tries(at.0, at.1, used, charged);
        }
    }

    /// A move of the Pokémon at `side`'s `pos` has reached `TryMove`, where the Metronome item's
    /// condition counts (`onTryMove`, priority -2): it goes if the item is not at work any more; a
    /// move that calls another is left out (the one called counts); the same move again after a
    /// last turn that worked adds one, a charging move's second turn starts at one (or adds one), and
    /// anything else starts again at nothing. (`about` is the last move, `given.0` the run.)
    fn metronome_tries(&mut self, side: usize, pos: usize, used: u16, charged: bool) {
        if !self.spots[side][pos].has(VolKind::Metronome) {
            return;
        }
        if self.item_at_work(side, pos) != Some(it::METRONOME) {
            self.spot(side, pos).vols.retain(|v| v.kind != VolKind::Metronome);
            return;
        }
        if MOVES[used as usize].calls_move {
            return;
        }
        let spot = &mut self.spots[side][pos];
        let last_ok = spot.result_last == Some(Some(true));
        let Some(v) = spot.vols.iter_mut().find(|v| v.kind == VolKind::Metronome) else { return };
        v.given.0 = if v.about == used && last_ok {
            v.given.0 + 1
        } else if charged {
            if v.about != used { 1 } else { v.given.0 + 1 }
        } else {
            0
        };
        v.about = used;
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
                if mine.genuine_form != beside.genuine_form {
                    beside.genuine_form
                } else if here.transformed != other.transformed {
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

    /// The number of the name the other side's Pokémon at `pos` goes by: its own, or, for the one of two
    /// on the field that look the same taken for the copy, the number of the one it passes for. The
    /// other side's sleep counts are kept by name (D96, `Counts::update`): what `counts.sleep` holds under
    /// a number is the runs the log has shown under that name in the current sleep, by whoever made them.
    fn their_name(&self, pos: usize) -> usize {
        let s = &self.reader.sides[1 - self.side];
        s.active[pos].as_ref().map_or(MAX_TEAM - 1, |here| here.entry.min(MAX_TEAM - 1))
    }

    // ------------------------------------------------------------ the log

    /// Takes in one line of the battle's log, as this player is sent it.
    pub fn line(&mut self, line: &str) -> Result<(), String> {
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() < 2 || !parts[0].is_empty() {
            return Ok(());
        }
        let kind = parts[1];
        if !known_kind(kind) {
            *self.unknown.entry(kind.to_string()).or_insert(0) += 1;
        }
        let arg = |i: usize| parts.get(i).copied().unwrap_or("");
        let from = parts.iter().find_map(|p| p.strip_prefix("[from]")).map_or("", str::trim);
        // A queued move begins: note how things stand before the reader takes the line in.
        let who = ident(arg(2)).and_then(|id| id.pos.map(|pos| (id.side, pos)));
        // (A `cant` right after the same Pokémon's own move line is that move's action failing, Heal Block
        // stopping it in `onTryMove`, and one right after its `cant` is the same `BeforeMove` (asleep, then
        // flinched): not one more action. Showdown counts one a move.)
        let this = who.map(|(s, p)| ((s, p), self.spots[s][p].stay));
        let last = self.moved_last;
        let same = this.is_some() && this == last.map(|l| (l.0, l.1));
        // (After its own `cant`, a `cant` that names the move it tried is that action's continuation, which the
        // begun action takes in itself: asleep, then Imprison stopping its Sleep Talk.)
        let names_move = !arg(4).is_empty() && !arg(4).starts_with('[');
        // (After its move line, only a condition stopping the move inside it is the same action's: Heal Block at
        // `TryMove`, or Heal Block, Throat Chop or Gravity at `ModifyMove` of a move another called. One out of PP,
        // flinched or paralysed there is its next action, after an Instruct's.)
        // (Nor after the action an Instruct put in, which is one of its own: its own action comes next.)
        let instructed_last = self.begun.last().is_some_and(|b| b.instructed && this.is_some_and(|t| b.stay == t.1));
        let same_action = kind == "cant"
            && same
            && !instructed_last
            && if last.is_some_and(|l| l.2) {
                !names_move
            } else {
                matches!(arg(3), "move: Heal Block" | "move: Throat Chop" | "move: Gravity")
            };
        // (Not an ability's veto of another's move, `cant|H|ability: Armor Tail|M|[of] X`: the line names its holder.
        // A line about the same Pokémon in between, Attract's `-activate`, is part of the same `BeforeMove`.)
        self.bounce_stopped = kind == "cant"
            && matches!(arg(3), "move: Throat Chop" | "move: Heal Block" | "move: Gravity")
            && MOVES.get(self.last_move as usize).is_some_and(|d| d.flags & F_REFLECTABLE != 0)
            && who.is_some_and(|w| {
                self.under_way.is_some_and(|u| u.0 != w)
                    && self.reached.iter().any(|r| r.0 == w)
                    && self.ability_at(w.0, w.1) == ab::MAGICBOUNCE
            });
        let own_cant = kind == "cant" && !arg(3).starts_with("ability: ") && !self.bounce_stopped;
        // (Copycat's move stopped inside its action, a Throat Chop at `ModifyMove`: the battle's last move is the one it
        // copied, used all the same.)
        if same_action
            && !last.is_some_and(|l| l.2)
            && let Some(copied) = self.copycat_copies.take()
        {
            self.last_move = copied;
        }
        // (A move sent back by Magic Bounce or Magic Coat is not its holder's action, `useMove` without `runMove`:
        // its own `cant` after it is one.)
        let bounced = matches!(from, "ability: Magic Bounce" | "move: Magic Coat");
        self.moved_last = if kind == "move" && bounced {
            None
        } else if kind == "move" || own_cant {
            this.map(|(at, stay)| (at, stay, kind == "cant"))
        } else if same {
            last
        } else {
            None
        };
        if kind == "cant" && who.is_some_and(|(side, pos)| self.reader.borrowed_cant(side, pos, arg(3), arg(4))) {
            self.reader.line(line)?;
            return Ok(());
        }
        if let Some((side, pos)) = who {
            let begins = match kind {
                "move" => queued_move(arg(3), from),
                "cant" => !arg(3).starts_with("ability: ") && !same_action && !self.bounce_stopped,
                // (A confusion self-hit after its own `cant` is the same `BeforeMove`: asleep and confused.)
                "-damage" if from == "confusion" => !(same && last.is_some_and(|l| l.2)),
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
        // The other side's allied Skill Swap names no abilities: what each had, as far as this side
        // knows it, before the reader forgets them (for the export: `complete`).
        let their_swap = if kind == "-activate" && effect_id(arg(3)) == "skillswap" && arg(4).is_empty() {
            let other = parts
                .iter()
                .find_map(|p| p.strip_prefix("[of]").and_then(|p| ident(p.trim())))
                .and_then(|id| id.pos.map(|pos| (id.side, pos)));
            match (who, other) {
                (Some((s, p)), Some((t, q))) if s != self.side && t == s && p != q => {
                    Some((p, q, self.their_ability_now(p), self.their_ability_now(q)))
                }
                _ => None,
            }
        } else {
            None
        };
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
        if kind == "-formechange"
            && let Some((s, p)) = who
            && let Some(species) = species_id(&to_id(arg(3).split(',').next().unwrap_or("")))
            && self.reader.sides[s].active[p].as_ref().is_some_and(|l| l.species != species)
        {
            // Illusion never changes its disguise's form. A visibly changed
            // form identifies the genuine Pokémon when two share an alias.
            self.spots[s][p].genuine_form = true;
        }
        let own_items: [u16; ACTIVE] = std::array::from_fn(|p| self.own_now(p).map_or(it::NONE, |n| n.item));
        // A Pokémon going down: the status it falls with, before the reader writes it off (a
        // `|faint|` with no `0 fnt` before it, as Perish Song's, still has it then).
        let fainting =
            kind == "faint" || (matches!(kind, "-damage" | "-heal" | "-sethp") && arg(3).starts_with("0 fnt"));
        if fainting
            && let Some((s, p)) = who
            && let Some(key) = self.faint_key(s, p)
            && let Some(rec) = self.reader.live_record(s, p)
            && (kind != "faint" || rec.status != Status::None)
        {
            self.faint_status[s][key] = (rec.status != Status::None).then_some((rec.status, self.spots[s][p].tox));
        }
        // One that fell where it stands and is replaced there has its status cleared (`switchIn`).
        if matches!(kind, "switch" | "drag")
            && let Some((s, p)) = who
            && self.reader.sides[s].active[p].as_ref().is_some_and(|l| l.gone)
        {
            if self.upkept {
                self.arrived[s] = self.arrived[s].saturating_add(1);
            }
            if let Some(key) = self.faint_key(s, p) {
                self.faint_status[s][key] = None;
            }
        }
        // (A move's `TryMove`, settled by this line, came before what this line says happened, the reader's taking it
        // in included: a Magic Room it put up, an Ally Switch's swap.)
        self.try_resolve(kind, &parts);
        self.refused = kind == "move"
            && from.is_empty()
            && arg(4).is_empty()
            && parts.contains(&"[still]")
            && who.is_some_and(|(side, pos)| {
                let used = move_id(&to_id(arg(3)));
                // (A lock on a move it has no more, after Transform, is gone: `onDisableMove`, `hasMove`.)
                let has = |m: u16| {
                    let transformed = self.reader.sides[side].active[pos].as_ref().is_some_and(|l| l.transformed);
                    if side == self.side {
                        self.req.as_ref().and_then(|r| r.active.get(pos)).is_some_and(|a| a.moves.iter().any(|x| x.id == m))
                    } else {
                        !transformed
                    }
                };
                self.spots[side][pos].lock.is_some_and(|l| Some(l) != used && used != Some(mv::STRUGGLE) && has(l))
                    && self.choice_at_work(side, pos)
            });
        self.reader.line(line)?;
        let after = held(self);
        // (The hits a move made are filed as it is done, before anyone it hit is dragged out or replaced.)
        if matches!(kind, "move" | "cant" | "upkeep" | "turn" | "" | "switch" | "drag" | "replace" | "faint")
            && from != "ability: Magic Bounce"
        {
            self.hits_settle();
        }
        self.track(kind, &parts, from)?;
        self.keep(kind, &parts, from);
        self.synced = kind == "-activate" && arg(3) == "ability: Synchronize";
        // The moves are over at the blank line before the turn's end (`residual`: `this.add('')`, `clearActiveMove`),
        // once what they did is settled above: a stage Moody moves then is not the last move's. (The blank line that
        // opens a turn finds nothing under way.)
        if kind.is_empty() {
            // (Whom they reached is settled with them: nothing after takes it back.)
            (self.under_way, self.reached) = (None, Vec::new());
        }
        // An item it comes by starts, as one it comes in with does (the Metronome item's condition); and the item
        // of a disguise shown for what it is started as it came in, under the name it borrowed.
        // A Mega Evolution starts its new ability, as a switch-in does (Trace's `onStart`, judged on who stands beside
        // it now).
        if kind == "-mega"
            && let Some((side, pos)) = who
        {
            let blank = (0..ACTIVE).any(|q| self.present(1 - side, q) && self.ability_at(1 - side, q) == ab::NOABILITY);
            (self.spots[side][pos].trace_due, self.spots[side][pos].trace_blank) = (true, Some(blank));
        }
        if matches!(kind, "-item" | "replace")
            && let Some((side, pos)) = who
        {
            let had = self.spots[side][pos].has(VolKind::Metronome);
            self.metronome_starts(side, pos);
            // (A disguise's moves under the name it borrowed counted all the same: the last one, run afresh.)
            let last = self.spots[side][pos].last_move;
            if kind == "replace"
                && !had
                && last != NO_MOVE
                && let Some(v) = self.spots[side][pos].vols.iter_mut().find(|v| v.kind == VolKind::Metronome)
            {
                (v.about, v.given.0) = (last, 0);
            }
        }
        // A future move's landing is its next blow (after what it says of the blow, and what goes off at its
        // target first: Endure's `-activate`, Sturdy's `-ability`, a Focus Sash's or a resist berry's `-enditem`), or
        // nothing (an `-immune`, a substitute taking it).
        let at_target = who.is_some() && self.future_hit.is_some_and(|f| Some(f.0) == who);
        if !matches!(kind, "-end" | "split" | "-damage" | "-supereffective" | "-resisted" | "-crit")
            && !(kind == "-activate" && at_target && !arg(3).ends_with("Substitute"))
            && !(matches!(kind, "-ability" | "-enditem") && at_target)
        {
            self.future_hit = None;
        }
        // Any blow but a confusion self-hit turns Berserk's berry check back on (`onDamage`, or
        // `onAfterMoveSecondary` after a move's).
        if kind == "-damage"
            && from != "confusion"
            && let Some((side, pos)) = who
        {
            self.spots[side][pos].berserk_off = false;
        }
        // Its own Choice lock goes with its item, as it knows the item (a Choice item that goes takes its
        // hold with it; one that comes starts afresh), at a place it still stands.
        if !matches!(kind, "switch" | "drag" | "replace" | "swap") {
            for (p, &was) in own_items.iter().enumerate() {
                if self.own_now(p).map_or(it::NONE, |n| n.item) != was {
                    (self.spots[self.side][p].lock, self.spots[self.side][p].first) = (None, None);
                }
            }
        }
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
        let mut refiled = Vec::new();
        if kind == "replace" {
            // A disguise dropped where it stood: what the stay filed under the number it passed for is the
            // Pokémon's own.
            let mut sleeps = Vec::new();
            for pos in 0..ACTIVE {
                let now = self.their_index(pos);
                if self.spots[opp][pos].stay == were[pos].0 && were[pos].1 != now {
                    self.refile_disguise(pos, were[pos].1, now);
                    let was = were[pos].1;
                    if let Some(here) = self.spots[opp][pos].sleep_here.take()
                        && was < MAX_TEAM
                        && now < MAX_TEAM
                    {
                        sleeps.push((was, now, here));
                    }
                    refiled.push(now);
                }
            }
            // Its sleep this stay too (D96): the runs it made under the name it passed for are its own, and
            // that name's count is what it was before them. One that takes the name's own number was the
            // Pokémon itself all along, taken for the copy of the one just shown as a disguise: its runs
            // stand under the name already (`their_name`), and stay. A number past the team that either
            // leaves is nobody's now (cleared first: the disguise shown may be listed at that number).
            for &(was, now, (_, _, _, name)) in &sleeps {
                if was != now && was != name as usize {
                    self.counts.sleep[opp][was] = 0;
                }
            }
            for &(_, now, (before, runs, began, name)) in &sleeps {
                if now != name as usize {
                    let own = self.counts.sleep[opp][now];
                    self.counts.sleep[opp][now] = if began { runs } else { own.saturating_add(runs) };
                    self.counts.sleep[opp][name as usize] = before;
                }
            }
        }
        if !moved.is_empty() {
            // What goes with a Pokémon's status (its turns asleep, frozen) goes with it to its new number:
            // a Pokémon passed for by a disguise beside it slept under a number of its own until then.
            let (sleep, frozen) = (self.counts.sleep[opp], self.frozen[opp]);
            // So does a rampage's lock, with how far it has run (the rotating seed 20261026, battle 1873: a
            // Raging Fury begun in a disguise, the disguise dropping on its second turn): filed under the number
            // it went by when the move began. The number it leaves holds none, unless another takes it.
            let (rampage, turns, silent, run) =
                (self.rampage[opp], self.rampage_turns[opp], self.rampage_silent[opp], self.counts.rampage[opp]);
            for &(was, now, _) in &moved {
                if was < MAX_TEAM && now < MAX_TEAM {
                    self.frozen[opp][now] = frozen[was];
                    if !refiled.contains(&now) {
                        self.counts.sleep[opp][now] = sleep[was];
                    }
                    self.rampage[opp][now] = rampage[was];
                    self.rampage_turns[opp][now] = turns[was];
                    self.rampage_silent[opp][now] = silent[was];
                    self.counts.rampage[opp][now] = run[was];
                    if let Some((s, i)) = self.rampage_pending.as_mut()
                        && (*s, *i) == (opp, was)
                    {
                        *i = now;
                    }
                }
            }
            for &(was, _, _) in &moved {
                if was < MAX_TEAM && !moved.iter().any(|m| m.1 == was) {
                    (self.rampage[opp][was], self.counts.rampage[opp][was]) = (NO_MOVE, 0);
                    (self.rampage_turns[opp][was], self.rampage_silent[opp][was]) = (0, false);
                }
            }
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
                match event {
                    Event::Switch { r } => renumber(r),
                    Event::Swap { first, second } => {
                        renumber(first);
                        renumber(second);
                    }
                    _ => {}
                }
            }
            self.renumbered.push(moved);
        }
        if !matches!(kind, "switch" | "drag" | "replace" | "swap") {
            for k in 0..4 {
                let (Some(was), Some(is)) = (before[k], after[k]) else { continue };
                if was.0 != is.0 && matches!(is.0, ItemShown::Lost(_)) {
                    self.spots[k / 2][k % 2].lose_item();
                }
                if was.0 != is.0 {
                    // (A Choice item that goes takes its hold with it; one that comes starts afresh.)
                    self.spots[k / 2][k % 2].choice = None;
                }
                let changed = match (was.0, is.0) {
                    (_, ItemShown::Lost(_)) => true,
                    (ItemShown::Holds(a), ItemShown::Holds(b)) => a != b,
                    _ => false,
                };
                // (The other side's: its own Choice lock goes with its item as its requests show it,
                // `complete`; what is shown of it can be a disguise's, Illusion under a shared name.)
                if changed && k / 2 != self.side {
                    (self.spots[k / 2][k % 2].lock, self.spots[k / 2][k % 2].first) = (None, None);
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
        if let Some((p, q, first, second)) = their_swap {
            let opp = 1 - self.side;
            (self.spots[opp][p].their_ability, self.spots[opp][q].their_ability) = (second, first);
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
    /// The number [`Follower::faint_status`] files the Pokémon at a position under.
    fn faint_key(&self, side: usize, pos: usize) -> Option<usize> {
        let key = if side == self.side {
            Some(self.own_at[pos])
        } else {
            self.reader.sides[side].active[pos].as_ref().map(|l| l.entry)
        };
        key.filter(|&k| k < MAX_TEAM)
    }

    /// `Battle#checkFainted`, at a turn's end and once the Pokémon replacing the fainted are in:
    /// on a side with someone left to send in, the fainted where they fell lose their status.
    fn check_fainted(&mut self) {
        for side in 0..2 {
            let s = &self.reader.sides[side];
            let size = if s.size == 0 { 4 } else { s.size as usize };
            let standing = (0..ACTIVE).filter(|&p| s.active[p].as_ref().is_some_and(|l| !l.gone)).count();
            // (A replacement that has come in since `upkeep` says there was someone to send in: Showdown's
            // condition is read before the replacements, and a faint after `upkeep`, Perish Song's, is checked
            // only once they stand.)
            if self.arrived[side] == 0 && size <= s.fainted as usize + standing {
                continue;
            }
            for pos in 0..ACTIVE {
                let fell = self.reader.sides[side].active[pos].as_ref().is_some_and(|l| {
                    l.gone && self.reader.sides[side].team.get(l.entry).is_some_and(|e| e.rec.fainted)
                });
                if fell && let Some(key) = self.faint_key(side, pos) {
                    self.faint_status[side][key] = None;
                }
            }
        }
    }

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
            quick_again: spot.quick_again,
            encored: spot.has(VolKind::Encore),
            quick: spot.quick,
            seen,
            field,
            own,
        });
        spot.actions = spot.actions.saturating_add(1);
        if spot.instructed {
            spot.instructed = false;
        } else {
            spot.begun += 1;
        }
        self.begun[at].r = self.mon_ref(side, pos);
    }

    /// An Illusion has dropped at `pos` on the other side: the PP, the item used up and the Choice lock
    /// this stay filed under the number it passed for (`was`) go to its own (`now`).
    fn refile_disguise(&mut self, pos: usize, was: usize, now: usize) {
        let opp = 1 - self.side;
        if self.their_pp.len() <= was.max(now) {
            self.their_pp.resize(was.max(now) + 1, Vec::new());
        }
        let spent = std::mem::take(&mut self.spots[opp][pos].spent);
        for &(id, n) in &spent {
            if let Some(s) = self.their_pp[was].iter_mut().find(|s| s.0 == id) {
                s.1 = (s.1 as i16 - n).clamp(0, 255) as u8;
            }
            match self.their_pp[now].iter_mut().find(|s| s.0 == id) {
                Some(s) => s.1 = (s.1 as i16 + n).clamp(0, 255) as u8,
                None if n > 0 => self.their_pp[now].push((id, n.min(255) as u8)),
                None => {}
            }
        }
        self.spots[opp][pos].spent = spent;
        // The doubts over Pressure's PP it left, as the PP it spent.
        for &(id, n) in &self.spots[opp][pos].doubted {
            if let Some(d) = self.pressure_doubt.iter_mut().find(|d| (d.0, d.1) == (was, id)) {
                d.2 = d.2.saturating_sub(n);
            }
            match self.pressure_doubt.iter_mut().find(|d| (d.0, d.1) == (now, id)) {
                Some(d) => d.2 = d.2.saturating_add(n),
                None => self.pressure_doubt.push((now, id, n)),
            }
        }
        self.pressure_doubt.retain(|d| d.2 > 0);
        // Who it attacked this stay: filed under the number it passed for, which is its own. (What the
        // Pokémon it passed for did under that number, in a stay of its own, stays with it.)
        let stay = self.spots[opp][pos].stay;
        let (from, to) = (opp * MAX_TEAM + was, opp * MAX_TEAM + now);
        for spot in self.spots.iter_mut().flatten() {
            if spot.attacked_by & 1 << from != 0 && spot.attacked_stay[from] == stay {
                spot.attacked_by = (spot.attacked_by & !(1 << from)) | 1 << to;
                spot.attacked_stay[to] = stay;
            }
        }
        // (The hits each of this side's Pokémon took name the other side's attacker by its number.)
        for spot in self.spots[1 - opp].iter_mut() {
            for hit in spot.damaged_by.iter_mut().filter(|d| d.0 == was && d.3 == stay) {
                hit.0 = now;
            }
        }
        if let Some((filed, before)) = self.spots[opp][pos].used {
            (self.used_up[opp][now], self.used_up[opp][was]) = (filed, before);
        }
        // The exact HP this stay saw of it, which was its own.
        for e in self.hp_seen.iter_mut().filter(|e| e.0 == was && e.4 == stay) {
            e.0 = now;
        }
        for n in self.hp_now.iter_mut().filter(|n| n.0 == was) {
            n.0 = now;
        }
        // The blow that knocked it out, if it fell in disguise.
        if let Some(ko) = self.ko_hit[opp].get_mut(was).and_then(Option::take) {
            self.ko_hit[opp][now] = Some(ko);
        }
        // Its Choice lock, judged again with the item of who it really is.
        if self.spots[opp][pos].lock.is_none()
            && let Some((first, seq)) = self.spots[opp][pos].first
            && self.choice_at_work(opp, pos)
        {
            (self.spots[opp][pos].lock, self.spots[opp][pos].choice_seq) = (Some(first), seq);
        }
    }

    /// Whether the Pokémon at `side`'s `pos` holds anyone else on the field there (their `trapped` names it).
    fn holds_anyone(&self, side: usize, pos: usize) -> bool {
        let stay = self.spots[side][pos].stay;
        (0..2).flat_map(|s| (0..ACTIVE).map(move |p| (s, p))).any(|(s, p)| {
            (s, p) != (side, pos)
                && self.present(s, p)
                && self.spots[s][p].vols.iter().any(|v| v.kind == VolKind::Trapped && v.source == stay)
        })
    }

    /// Whether a status move of the Pokémon at `side`'s `pos` whose line named no target, failing with no
    /// word of why, was taken by its target's Substitute (`onTryPrimaryHit`, a success in Champions' hit loop)
    /// or failed (the move's own `onTryHit`, which runs first, and an effect that comes to nothing, print the
    /// same `-fail`). Each Pokémon it may have been aimed at (for its own side, the one it was) gives what it
    /// would have come to there: worked where a Substitute is up and the move's own `TryHit` passes (either,
    /// where that cannot be told), failed anywhere else. `None`: some of them give one and some the other
    /// (D89's D2 case).
    fn sub_took(&self, side: usize, pos: usize) -> Option<bool> {
        let d = MOVES.get(self.last_move as usize)?;
        if d.category != Category::Status
            || d.flags & F_BYPASSSUB != 0
            || !matches!(
                d.target,
                Target::Normal | Target::AdjacentFoe | Target::Any | Target::AdjacentAlly | Target::AdjacentAllyOrSelf
            )
        {
            return Some(false);
        }
        // (Its user's own Substitute does not take a move on itself: `target === source`.)
        let behind = |(s, p): (usize, usize)| (s, p) != (side, pos) && self.spots[s][p].has(VolKind::Substitute);
        let aims: Vec<(usize, usize)> = if let Some(t) = self.bounced_at.filter(|_| self.under_way.is_some_and(|u| u.0 == (side, pos))) {
            // (Sent back: at the one that used it.)
            vec![t]
        } else if side == self.side && !self.called_now {
            let loc = self.spots[side][pos].last_move_loc.filter(|&l| l != 0)?;
            let at = if loc > 0 { (1 - side, (loc - 1) as usize) } else { (side, (-loc - 1) as usize) };
            // (Drawn off by a Follow Me or a Rage Powder, which takes even one aimed at its partner: there.)
            let at = if self.spots[1 - side].iter().any(|s| s.has(VolKind::Followme) || s.has(VolKind::Ragepowder))
                && self.present(at.0, at.1)
            {
                self.redirected_pp_target(pos, self.last_move, d.target, at).unwrap_or(at)
            } else {
                at
            };
            if self.present(at.0, at.1) {
                vec![at]
            } else if at.0 != side {
                // (Aimed at an empty place of the other side's: Showdown aims it at one standing there instead,
                // `getRandomTarget`, which with one standing is that one.)
                let standing: Vec<(usize, usize)> = (0..ACTIVE).map(|p| (at.0, p)).filter(|&(s, p)| self.present(s, p)).collect();
                if standing.len() != 1 {
                    return Some(false);
                }
                standing
            } else {
                return Some(false);
            }
        } else {
            (0..2)
                .flat_map(|s| (0..ACTIVE).map(move |p| (s, p)))
                .filter(|&(s, p)| ((s, p) != (side, pos) || d.target == Target::AdjacentAllyOrSelf) && self.present(s, p))
                // (One a Copycat or a Sleep Talk called is aimed at random at a foe, `getRandomTarget`.)
                .filter(|&(s, _)| match d.target {
                    Target::AdjacentAlly | Target::AdjacentAllyOrSelf => s == side,
                    Target::AdjacentFoe => s != side,
                    _ => s != side || !self.called_now,
                })
                .collect()
        };
        // Whether it may have failed at `t`, which has no Substitute, with no word naming `t` (`-fail|user` and
        // `[still]`, as a Substitute taking it says): its own `onTryHit`; an effect that did nothing silently (a
        // condition `t` has already, a status where `t` has one, `trySetStatus`); or an `onHit` and the like, which
        // this side cannot judge. Anything else it would have done at `t` the log would say (a stage, a status
        // given): not where it went.
        let silent = |t: (usize, usize)| -> bool {
            let risky = Ev::Hit.bit() | Ev::Try.bit() | Ev::PrepareHit.bit() | Ev::TryImmunity.bit();
            self.try_hit_fails(self.last_move, (side, pos), t) != Some(false)
                || d.events & risky != 0
                || (d.status != Status::None && self.reader.live_record(t.0, t.1).is_none_or(|r| r.status != Status::None))
                || d.volatile.is_some_and(|v| self.spots[t.0][t.1].has(v))
        };
        let (mut worked, mut failed) = (false, false);
        for t in aims {
            match (self.try_hit_fails(self.last_move, (side, pos), t), behind(t)) {
                (Some(true), _) => failed = true,
                (Some(false), true) => worked = true,
                (None, true) => (worked, failed) = (true, true),
                (_, false) if silent(t) => failed = true,
                (_, false) => {}
            }
        }
        match (worked, failed) {
            (true, false) => Some(true),
            (true, true) => None,
            _ => Some(false),
        }
    }

    /// Whether the status move `mv` of the Pokémon at `user` would fail at its own `onTryHit` at `target`, as
    /// this side can tell: `Some(false)` for a move with none. Yawn at one with a status (nothing is immune to
    /// sleep by type); Entrainment, Simple Beam, Worry Seed, Gastro Acid by the abilities (on the sheets, or
    /// shown since); Lock-On on a user already locked on. `None` where it cannot (an ability it has not been
    /// shown, Electrify's whether the target has moved).
    fn try_hit_fails(&self, mv: u16, user: (usize, usize), target: (usize, usize)) -> Option<bool> {
        let d = MOVES.get(mv as usize)?;
        if d.events & Ev::TryHit.bit() == 0 {
            return Some(false);
        }
        // (As this side knows it, No Ability included: none where it cannot tell.)
        let ability = |(s, p): (usize, usize)| {
            if s == self.side {
                // (After an allied swap the log does not say which of the two it has: only where both agree.)
                self.own_now(p).and_then(|now| now.abilities.first().copied().filter(|a| now.abilities.iter().all(|b| b == a)))
            } else {
                self.their_ability_now(p)
            }
        };
        let cant = |a: u16| ABILITIES[a as usize].flags & AF_CANTSUPPRESS != 0;
        match d.id {
            "yawn" => {
                // (As the log has it, on either side.)
                self.reader.live_record(target.0, target.1).map(|r| r.status != Status::None)
            }
            "entrainment" => {
                let (theirs, mine) = (ability(target)?, ability(user)?);
                Some(
                    target == user
                        || theirs == mine
                        || cant(theirs)
                        || ABILITIES[theirs as usize].id == "truant"
                        || ABILITIES[mine as usize].flags & AF_NOENTRAIN != 0,
                )
            }
            "simplebeam" | "worryseed" => {
                let theirs = ability(target)?;
                Some(cant(theirs) || (d.id == "simplebeam" && matches!(ABILITIES[theirs as usize].id, "simple" | "truant")))
            }
            "gastroacid" => Some(cant(ability(target)?)),
            "lockon" => Some(self.spots[user.0][user.1].has(VolKind::Lockon)),
            // (At one that has acted this turn and was on the field before it: `!willMove && activeTurns`.)
            "electrify" => Some(self.spots[target.0][target.1].begun != 0 && self.spots[target.0][target.1].active_turns > 0),
            _ => None,
        }
    }

    /// Its own Pokémon at `pos`: its stats as Showdown keeps them (`storedStats`), from its sheet for the forme
    /// it now has ([`Spot::stored`]); none while it is transformed, or with nobody there.
    fn own_stored(&self, pos: usize) -> Option<[u16; 6]> {
        let spot = &self.spots[self.side][pos];
        if let Some(st) = spot.stored {
            return Some(st);
        }
        let own = self.own.get(self.own_at[pos])?;
        let live = self.reader.sides[self.side].active[pos].as_ref()?;
        if live.transformed {
            return None;
        }
        let set = &self.team[own.entry];
        let species = if self.own_disguised(pos) { set.species } else { spot.forme.unwrap_or(live.species) };
        let mut st = calc_stats(species, set.nature, set.stat_points);
        if spot.trick_copied {
            st.swap(1, 2);
        }
        Some(st)
    }

    /// Power Trick starts or ends on the Pokémon at `side`'s `pos`: its Attack and Defense change places.
    fn trick_swap(&mut self, side: usize, pos: usize) {
        if side == self.side
            && let Some(mut st) = self.own_stored(pos)
        {
            st.swap(1, 2);
            self.spots[side][pos].stored = Some(st);
        }
    }

    /// Power Split, Guard Split (the stats `keys` averaged, rounded down) or Speed Swap (`[5]`, swapped) between the
    /// Pokémon at `side`'s `pos` and the one at `with`: between two of its own, worked out; with one of the other
    /// side's, or between two of theirs, a stat this side cannot know ([`Spot::stats_shared`]).
    fn mix_stats(&mut self, side: usize, pos: usize, with: (usize, usize), keys: &[usize]) {
        let (s, p) = with;
        // (Mixed with one whose stats are already beyond it, a stat of its own is too.)
        let known = !self.spots[side][pos].stats_shared && !self.spots[s][p].stats_shared;
        if side == self.side && s == self.side && known {
            let (Some(mut a), Some(mut b)) = (self.own_stored(pos), self.own_stored(p)) else { return };
            for &k in keys {
                if keys == [5] {
                    (a[k], b[k]) = (b[k], a[k]);
                } else {
                    let avg = ((a[k] as u32 + b[k] as u32) / 2) as u16;
                    (a[k], b[k]) = (avg, avg);
                }
            }
            (self.spots[side][pos].stored, self.spots[s][p].stored) = (Some(a), Some(b));
        } else {
            (self.spots[side][pos].stats_shared, self.spots[s][p].stats_shared) = (true, true);
        }
    }

    /// The Pokémon at `side`'s `pos` as the maker of a slot condition.
    fn maker(&self, side: usize, pos: usize) -> Maker {
        Maker { side, idx: self.mon_ref(side, pos).idx as usize, slot: side * ACTIVE + pos }
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

    /// Ally Switch has moved a Pokémon onto `side`'s `pos`: a Healing Wish waiting there has run its handler inside
    /// an event with it as holder ([`SideTrack::wish_met`]; if it heals, it goes, and the mark with it).
    fn wish_met(&mut self, side: usize, pos: usize) {
        if self.field.sides[side].slots[pos].iter().any(|c| c.0 == SlotCond::Healingwish) {
            self.field.sides[side].wish_met[pos] = true;
        }
    }

    /// A Pokémon comes in at a position.
    fn enter(&mut self, side: usize, pos: usize, name: &str, health: &str, from: &str) {
        self.stays += 1;
        // (Where an Encore's draw sent the move of one of its own that leaves is read by nothing any more: its last
        // move goes with it, `clearVolatile`, and the next one it uses aims afresh.)
        if side == self.side
            && let Some(&old) = self.own_at.get(pos)
            && old < MAX_TEAM
        {
            self.encore_doubt[old] = false;
        }
        let mut fresh = Spot::new(self.stays);
        let team = &self.reader.sides[side];
        let brought = if team.size == 0 { 4 } else { team.size };
        fresh.last_two = pos == 0 && brought.saturating_sub(team.fainted) <= 2;
        let old = &self.spots[side][pos];
        if from.contains("Baton Pass") {
            fresh.boosts = old.boosts;
            fresh.vols = old.vols.iter().copied().filter(|v| !v.kind.data().no_copy).collect();
            // A binding or a confusion handed on goes on with what is left of it (`copyVolatileFrom`), and so
            // does its count (D97).
            (fresh.bound, fresh.confused) = (old.bound, old.confused);
            fresh.copied_gastro = old.has(VolKind::Gastroacid);
            fresh.trick_copied = old.has(VolKind::Powertrick);
            if team.active[pos].as_ref().is_some_and(|l| {
                l.rec.ability != UNKNOWN && ABILITIES[l.rec.ability as usize].flags & AF_CANTSUPPRESS != 0
            }) {
                fresh.remove(VolKind::Gastroacid);
            }
        } else if from.contains("Shed Tail") {
            fresh.vols = old.vols.iter().copied().filter(|v| v.kind == VolKind::Substitute).collect();
        }
        // (What is passed on keeps its order, ahead of anything that goes up after.)
        fresh.seq = fresh.vols.iter().map(|v| v.seq).max().unwrap_or(0);
        let left = old.stay;
        fresh.fallen = self.faints[side];
        self.spots[side][pos] = fresh;
        // Suppression copied by Baton Pass is removed for an unsuppressible
        // ability, even when the opponent's ability is known only from sheets.
        if from.contains("Baton Pass") {
            let rec = self.reader.live_record(side, pos);
            let ability = if side == self.side {
                self.req.as_ref().and_then(|r| r.mons.iter().find(|m| m.name == name)).map(|m| m.ability)
            } else {
                rec.and_then(|r| {
                    if r.ability != UNKNOWN {
                        Some(r.ability)
                    } else {
                        self.reader.sides[side].roster.get(r.listed as usize).filter(|_| self.open()).map(|l| l.ability)
                    }
                })
            };
            if ability.is_some_and(|a| a != UNKNOWN && ABILITIES[a as usize].flags & AF_CANTSUPPRESS != 0) {
                self.spots[side][pos].remove(VolKind::Gastroacid);
            }
        }
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
        // Back from a faint (Revival Blessing): with the blow that knocked it out, which Showdown filed
        // after the faint had cleared the rest.
        // (Not one Baton Pass or Shed Tail brings in: `copyVolatileFrom` clears it first.)
        let idx = self.mon_ref(side, pos).idx as usize;
        let passed = from.contains("Baton Pass") || from.contains("Shed Tail");
        if let Some((attacker, by, slot, damage, hits)) =
            self.ko_hit[side].get_mut(idx).and_then(Option::take).filter(|_| !passed)
        {
            let spot = &mut self.spots[side][pos];
            (spot.hits, spot.attacked) = (hits as u16, true);
            spot.attacked_by |= 1 << (attacker * MAX_TEAM + by);
            if attacker != side {
                spot.damaged_by.push((by, slot, damage, 0));
            }
        }
        self.items.push((self.begun.len(), Event::Switch { r: self.mon_ref(side, pos) }));
        // Its item starts as it comes in (the Metronome item's condition; after anything Baton Pass handed on).
        self.metronome_starts(side, pos);
        self.spots[side][pos].trace_due = true;
        if self.turn > 0 {
            let blank = (0..ACTIVE).any(|q| self.present(1 - side, q) && self.ability_at(1 - side, q) == ab::NOABILITY);
            self.spots[side][pos].trace_blank = Some(blank);
        }
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
                self.encore_queued = [None; ACTIVE];
                self.check_fainted();
                self.arrived = [0; 2];
                self.upkept = false;
                (self.fainted_last, self.fainted_now) = (self.fainted_now, [false; 2]);
                for side in 0..2 {
                    for pos in 0..ACTIVE {
                        let present = self.present(side, pos);
                        let spot = &mut self.spots[side][pos];
                        if present {
                            spot.active_turns += 1;
                        }
                        (spot.begun, spot.reordered, spot.overridden, spot.quick) = (0, None, None, false);
                        spot.quick_again = false;
                        spot.rampage_moved = false;
                        // Showdown says at the end of a turn what a Pokémon's types have
                        // become, if they have changed: one of the follower's own that took
                        // a foe's types and has heard nothing has the ones it had.
                        let unchanged = side == self.side && spot.copied_foe && !spot.told;
                        (spot.copied, spot.copied_foe, spot.told) = (false, false, false);
                        // (`if (pokemon.getLastAttackedBy()) pokemon.knownType = true`.)
                        if spot.attacked_by != 0 {
                            spot.known_off = false;
                        }
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
                self.upkept = true;
                self.arrived = [0; 2];
                self.check_fainted();
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
                        let rock = match weather {
                            Weather::Raindance => it::DAMPROCK,
                            Weather::Sunnyday => it::HEATROCK,
                            Weather::Sandstorm => it::SMOOTHROCK,
                            _ => it::ICYROCK,
                        };
                        // (The rock lengthens it only at work, `hasItem`: not under Klutz or Magic Room.)
                        let by = self.setter(parts);
                        let long = by.is_some_and(|(s, p)| self.item_at(s, p) == rock && !self.item_ignored(s, p));
                        self.field.weather_len = if long { 8 } else { 5 };
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
                        let by = self.setter(parts);
                        let long = by.is_some_and(|(s, p)| self.item_at(s, p) == it::TERRAINEXTENDER && !self.item_ignored(s, p));
                        self.field.terrain_len = if long { 8 } else { 5 };
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
                let screen = matches!(cond, SideCond::Reflect | SideCond::Lightscreen | SideCond::Auroraveil);
                let clay = screen
                    && kind == "-sidestart"
                    && self.mover.as_ref().is_some_and(|m| self.item_at(m.side, m.pos) == it::LIGHTCLAY && !self.item_ignored(m.side, m.pos));
                let track = &mut self.field.sides[side];
                if kind == "-sideend" {
                    track.conds.retain(|c| c.0 != cond);
                    track.lasting.retain(|&c| c != cond);
                } else if let Some(c) = track.conds.iter_mut().find(|c| c.0 == cond) {
                    c.1 += 1;
                } else {
                    track.conds.push((cond, 1, 0));
                    if clay {
                        track.lasting.push(cond);
                    }
                }
                return Ok(());
            }
            "-swapsideconditions" => {
                let [a, b] = &mut self.field.sides;
                std::mem::swap(&mut a.conds, &mut b.conds);
                std::mem::swap(&mut a.lasting, &mut b.lasting);
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
            // One that fainted where it still stands goes straight back in there, and the switch finds it in its
            // own place with HP again: `clearVolatile` on it takes the blow that knocked it out with the rest.
            if let Some((s, p)) = self.place_of(arg(2)) {
                let idx = self.mon_ref(s, p).idx as usize;
                if let Some(ko) = self.ko_hit[s].get_mut(idx) {
                    *ko = None;
                }
            }
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
                    for p in [pos, to] {
                        self.wish_met(side, p);
                    }
                    if side == self.side {
                        self.own_at.swap(pos, to);
                        self.own_hp.swap(pos, to);
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
                self.fainted_now[side] = true;
                if side != self.side {
                    for caught in self.encore_queued.iter_mut().flatten() {
                        caught.fell = true;
                    }
                }
                self.faints[side] = self.faints[side].saturating_add(1);
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
                // (A new forme has its own stats, worked out afresh: a split before it is gone, and Power Trick's
                // swap.)
                (spot.stats_shared, spot.stored, spot.trick_copied) = (false, None, false);
                if kind == "-formechange" && forme.is_some() {
                    spot.forme = forme;
                }
            }
            "move" => {
                self.animation_pp = None;
                let Some(used) = move_id(&to_id(arg(3))) else {
                    return Ok(());
                };
                if self.refused {
                    // (Refused by its Choice lock: it never ran, and failed; it was its attempt, though. `MoveAborted`
                    // takes a charge off, as a `cant` does.)
                    let spot = self.spot(side, pos);
                    (spot.result_now, spot.last_attempted_move) = (Some(Some(false)), Some(used));
                    spot.vols.retain(|v| {
                        v.ends != Ends::Move
                            && !matches!(v.kind, VolKind::Twoturnmove | VolKind::Mustrecharge)
                            && !CHARGING.contains(&v.kind)
                    });
                    return Ok(());
                }
                let queued = queued_move(arg(3), from);
                if side == self.side && from != "lockedmove" {
                    // A charging move another called is aimed where Showdown drew: the one of the other side
                    // standing, or either of two.
                    let charging = VolKind::named(MOVES[used as usize].id).is_some_and(|k| CHARGING.contains(&k));
                    let standing: Vec<usize> = (0..ACTIVE).filter(|&p| self.present(1 - side, p)).collect();
                    self.spots[side][pos].charge_aim = (charging && from.starts_with("move: ")).then_some(match standing[..] {
                        [p] => Some(p as i8 + 1),
                        [] => Some(1),
                        _ => None,
                    });
                }
                // Champions' No Retreat on one already trapped, or on a Ghost that cannot be: its volatile
                // goes on without a word (no `-start`), and a second one fails.
                if used == mv::NORETREAT && !parts.contains(&"[still]") {
                    let ghost =
                        self.seen_base(side, pos).contains(&Type::Ghost) || self.spots[side][pos].added == Type::Ghost;
                    if self.spots[side][pos].has(VolKind::Trapped) || ghost {
                        self.spot(side, pos).add(VolKind::Noretreat, Ends::Said);
                    }
                }
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
                // (Instruct repeats the last move at the place it went, `lastMoveTargetLoc`: a line of another move is
                // the Pokémon's own action.)
                let instructed = self.begun.last().is_some_and(|b| b.stay == stay && b.instructed) && used == before;
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
                    let spot = self.spot(side, pos);
                    spot.choice = Some(used);
                    if spot.lock.is_none() {
                        (spot.lock, spot.choice_seq) = (Some(used), spot.next_seq());
                    }
                }
                if queued && used != mv::STRUGGLE && self.spots[side][pos].first.is_none() {
                    let spot = self.spot(side, pos);
                    spot.first = Some((used, spot.next_seq()));
                }
                // Where Champions' Encore's draw sent its own move (D89): with one of the other side
                // standing, there; with two, not told if the one drawn fainted first (the log names the
                // other) or the line names nobody (a move that failed with no animation: `[still]`
                // blanks its target, `Battle#attrLastMove`). Until it moves again.
                let mut drawn = None;
                if side == self.side {
                    let single =
                        matches!(MOVES[used as usize].target, Target::Normal | Target::AdjacentFoe | Target::Any);
                    let queued_by_encore = self.encore_queued[pos].take().filter(|_| replaced);
                    drawn = queued_by_encore.and_then(|q| q.forced);
                    // (Where the draw had two places to go: a move at one of the other side's.)
                    // (A Curse its user is no Ghost for is aimed at itself: nothing drawn.)
                    let on_itself = used == mv::CURSE
                        && !self.spots[side][pos].turn_ghost.unwrap_or_else(|| self.ghost_now(side, pos));
                    let queued_by_encore = queued_by_encore.filter(|_| single && !on_itself);
                    self.encore_unnamed = queued_by_encore
                        .is_some_and(|q| q.forced.is_none() && !q.fell && arg(4).is_empty())
                        .then_some(pos);
                    let idx = self.own_at[pos];
                    if idx < MAX_TEAM {
                        self.encore_doubt[idx] =
                            queued_by_encore.is_some_and(|q| q.forced.is_none() && (q.fell || arg(4).is_empty()));
                    }
                }
                let self_curse = used == mv::CURSE && !self.spots[side][pos].turn_ghost.unwrap_or_else(|| self.ghost_now(side, pos));
                if queued {
                    let mine = side == self.side;
                    let spot = self.spot(side, pos);
                    spot.last_move = used;
                    if mine {
                        // A move it is locked into (a charging move's second turn, a rampage) goes where the
                        // lock says, the place chosen when it began (`Side#chooseMove`), not where the log
                        // shows it land.
                        spot.last_move_loc = if self_curse {
                            // (A Curse its user is no Ghost for: aimed at itself, `selfTarget`, at the place it was
                            // resolved from as the turn began; an Encore putting it in again draws no target for it.)
                            Some(-(spot.turn_pos.unwrap_or(pos) as i8 + 1))
                        } else if replaced {
                            Some(drawn.unwrap_or(0))
                        } else if from == "lockedmove" {
                            before_loc
                        } else {
                            selected_loc
                        };
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
                            if fourth.is_none() && VolKind::named(data.id).is_some_and(|k| CHARGING.contains(&k)) {
                                self.animation_pp =
                                    Some((idx, copied, paying, taken.saturating_sub(base), was_unknown));
                            }
                        }
                    }
                }
                if let Some((paying, base)) = payer
                    && side != self.side
                {
                    // The other side's PP, as far as its opponent can count it: one a move, and one
                    // more for each Pokémon it is aimed at that has said it has Pressure.
                    let data = &MOVES[used as usize];
                    let wide = data.flags & F_MUSTPRESSURE != 0
                        || matches!(data.target, Target::All | Target::AllAdjacent | Target::AllAdjacentFoes);
                    let mut aimed: Vec<(usize, usize)> = Vec::new();
                    if wide {
                        aimed.extend([(1 - side, 0), (1 - side, 1)]);
                    } else if data.target != Target::FoeSide && !parts.contains(&"[notarget]") {
                        aimed.extend(fourth);
                        // (Dragon Darts pays to both of the other side while two stand, as its own side's does.)
                        if let Some(t) = fourth
                            && data.smart_target
                            && t.0 != side
                            && self.present(t.0, 1 - t.1)
                        {
                            aimed.push((t.0, 1 - t.1));
                        }
                    }
                    let pressed = aimed
                        .iter()
                        .filter(|&&(s, p)| {
                            s != side
                                && self.present(s, p)
                                && self.spots[s][p].pressing
                                && !self.spots[s][p].has(VolKind::Gastroacid)
                                && self.reader.live_record(s, p).is_some_and(|rec| rec.ability == ab::PRESSURE)
                        })
                        .count() as u8;
                    // Behind a blanked target (a failed Sucker Punch), with a Pressure Pokémon and one without
                    // where it could have been aimed, whether it paid a PP more is not told (D89).
                    // (Counter, Mirror Coat, Metal Burst and a move that goes for anybody are aimed by the game at
                    // random, at one of this side's; Metal Burst and Comeuppance at the last blow's maker if it stands.)
                    let random = matches!(data.target, Target::Scripted | Target::RandomNormal);
                    let back = self.spots[side][pos].hit_by.last().map(|&(_, p)| (1 - side, p)).filter(|&(s, p)| {
                        matches!(used, mv::METALBURST | mv::COMEUPPANCE) && self.present(s, p)
                    });
                    let blanked = arg(4).is_empty()
                        && parts.contains(&"[still]")
                        && back.is_none()
                        && matches!(
                            data.target,
                            Target::Normal | Target::AdjacentFoe | Target::Any | Target::Scripted | Target::RandomNormal
                        );
                    let pressed = match back {
                        Some((s, p)) if arg(4).is_empty() && parts.contains(&"[still]") => (self.spots[s][p].pressing
                            && !self.spots[s][p].has(VolKind::Gastroacid)
                            && self.reader.live_record(s, p).is_some_and(|rec| rec.ability == ab::PRESSURE))
                            as u8,
                        _ => pressed,
                    };
                    let mut pressing: Vec<bool> = (0..ACTIVE)
                        .filter(|&p| self.present(1 - side, p))
                        .map(|p| {
                            self.spots[1 - side][p].pressing
                                && !self.spots[1 - side][p].has(VolKind::Gastroacid)
                                && self.reader.live_record(1 - side, p).is_some_and(|rec| rec.ability == ab::PRESSURE)
                        })
                        .collect();
                    // (Or at its partner, which a move at one target may be aimed at, and Pressure never charges for;
                    // a charge, whose line is always blank, even at its partner's empty place, `[notarget]` at its
                    // release.)
                    let charge = VolKind::named(data.id).is_some_and(|k| CHARGING.contains(&k));
                    if data.target != Target::AdjacentFoe && !random && (charge || self.present(side, 1 - pos)) {
                        pressing.push(false);
                    }
                    let doubt = blanked && pressing.contains(&true) && pressing.contains(&false);
                    // (Every place it could have been aimed has Pressure: one more, told.)
                    let pressed = if blanked && !pressing.is_empty() && !pressing.contains(&false) { 1 } else { pressed };
                    if let Some(entry) = self.reader.sides[side].active[pos].as_ref().map(|l| l.entry) {
                        // (Not while transformed: it pays from the moves it copied, which are not counted, and a root is
                        // refused then, D93.)
                        let copying = self.reader.sides[side].active[pos].as_ref().is_some_and(|l| l.transformed);
                        if doubt && !copying {
                            match self.pressure_doubt.iter_mut().find(|d| (d.0, d.1) == (entry, paying)) {
                                Some(d) => d.2 = d.2.saturating_add(1),
                                None => self.pressure_doubt.push((entry, paying, 1)),
                            }
                            let doubted = &mut self.spots[side][pos].doubted;
                            match doubted.iter_mut().find(|d| d.0 == paying) {
                                Some(d) => d.1 = d.1.saturating_add(1),
                                None => doubted.push((paying, 1)),
                            }
                        }
                        if self.their_pp.len() <= entry {
                            self.their_pp.resize(entry + 1, Vec::new());
                        }
                        // (Transformed, it pays from the moves it copied, not its own: Showdown's `moveSlots`.)
                        let transformed = self.reader.sides[side].active[pos].as_ref().is_some_and(|l| l.transformed);
                        let spent = &mut self.their_pp[entry];
                        // (No lower than none left: `deductPP` stops at 0, so a Leppa Berry after restores from there.)
                        let most = MOVES[paying as usize].pp;
                        match spent.iter_mut().find(|s| s.0 == paying) {
                            _ if transformed => {}
                            Some(s) => s.1 = s.1.saturating_add(base + pressed).min(most),
                            None => spent.push((paying, (base + pressed).min(most))),
                        }
                        self.spots[side][pos].spend(paying, (base + pressed) as i16);
                    }
                }
                if doubtful {
                    self.refusable = Some(Refusal { side, pos, last_move: before, last_move_loc: before_loc, paid });
                }
                match used {
                    // Its condition goes up as the move is prepared, before it can fail for want of a
                    // partner; the odds failing take it down again (`-fail` below).
                    mv::ALLYSWITCH => self.spot(side, pos).ally_switched(),
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
                        let maker = self.maker(side, pos);
                        let slots = &mut self.field.sides[side].slots[pos];
                        if !slots.iter().any(|s| s.0 == cond) {
                            slots.push((cond, turns, Some(maker)));
                            undo = Some(Undo::Slot(side, pos, cond));
                            if cond == SlotCond::Healingwish {
                                self.field.sides[side].wish_met[pos] = false;
                            }
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
            // (A target's bounce stopped at once: no action of its own, and nothing of its own let go.)
            "cant" if self.bounce_stopped => {}
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
                    if self.last_move == mv::ALLYSWITCH && arg(3) != "move: Ally Switch" {
                        // Ally Switch's odds failed (its own, or one Copycat called): its condition is gone. (With
                        // no partner to trade places with it fails naming itself, and the condition stays.)
                        self.spots[side][pos].remove(VolKind::Allyswitch);
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
                        // (Dragon Cheer's `hasDragonType` as it was on the other.)
                        for kind in [VolKind::Focusenergy, VolKind::Dragoncheer] {
                            self.spots[side][pos].remove(kind);
                            if let Some(data) = self.spots[s][p].vols.iter().find(|v| v.kind == kind).map(|v| v.data) {
                                self.spots[side][pos].add(kind, Ends::Said);
                                if let Some(v) = self.spots[side][pos].vols.iter_mut().find(|v| v.kind == kind) {
                                    v.data = data;
                                }
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
                // (A Choice lock on a move it no longer has goes at the next request, `onDisableMove`, `hasMove`; the
                // next move with the item at work locks it anew.)
                self.spots[side][pos].lock = None;
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
                        if let Some(&vol) = theirs.vols.iter().find(|v| v.kind == kind) {
                            let seq = spot.next_seq();
                            spot.vols.push(Vol { seq, ..vol });
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
                    // It lands: its blow, next, is its user's, wherever that is now.
                    let maker = self.field.sides[side].slots[pos].iter().find(|c| c.0 == SlotCond::Futuremove).and_then(|c| c.2);
                    self.future_hit = maker.map(|m| ((side, pos), m));
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
                if effect.starts_with("ability: ")
                    && ability_id(&name).is_some_and(|a| ABILITIES[a as usize].flags & AF_CANTSUPPRESS != 0)
                {
                    self.spot(side, pos).remove(VolKind::Gastroacid);
                }
                if name == "trick" {
                    // (Switcheroo says so as Trick does.)
                    self.tricked = of.map(|of| ((side, pos), of));
                }
                let begun = self.begun.len();
                if effect == "trapped" {
                    // Kept there by whoever is moving; or, if that is itself (Jaw Lock holds
                    // both), by the one it moved on.
                    let own = self.mover.as_ref().is_some_and(|m| (m.side, m.pos) == (side, pos));
                    // (Champions' No Retreat: its own condition goes up first, and keeps it there itself:
                    // `noretreat`'s `onStart` adds `trapped`, then says No Retreat.)
                    let retreat = own && self.last_move == mv::NORETREAT;
                    let source = match &self.mover {
                        Some(m) if own => m.target.map_or(0, |(s, p)| self.spots[s][p].stay),
                        Some(m) => m.stay,
                        None => 0,
                    };
                    // Whoever holds it there has a `trapper` linked to it, put up when it comes to hold anyone
                    // (`Battle::add_trapped`), naming the first it holds, and gone once it holds nobody; not
                    // with No Retreat's own, which names nobody else.
                    let holder = (0..2)
                        .flat_map(|s2| (0..ACTIVE).map(move |p2| (s2, p2)))
                        .find(|&(s2, p2)| (s2, p2) != (side, pos) && self.spots[s2][p2].stay == source)
                        .filter(|&(s2, p2)| self.present(s2, p2));
                    let holding = holder.is_some_and(|(s2, p2)| self.holds_anyone(s2, p2));
                    // (Its holder as it stands now: the engine keeps the place it held it from.)
                    let by = holder.map(|(s2, p2)| (s2, self.mon_ref(s2, p2).idx as usize, s2 * ACTIVE + p2));
                    let spot = self.spot(side, pos);
                    if retreat {
                        spot.add(VolKind::Noretreat, Ends::Said);
                    }
                    if spot.add(VolKind::Trapped, Ends::Said) {
                        let v = spot.vols.last_mut().unwrap();
                        (v.source, v.by) = (source, by);
                        let held = (side, self.mon_ref(side, pos).idx as usize, side * ACTIVE + pos);
                        if let Some((s2, p2)) = holder.filter(|_| !holding) {
                            let spot = self.spot(s2, p2);
                            spot.remove(VolKind::Trapper);
                            spot.add(VolKind::Trapper, Ends::Said);
                            spot.vols.last_mut().unwrap().by = Some(held);
                        }
                    }
                } else if effect.starts_with("move: ")
                    && move_id(&name).is_some_and(|m| MOVES[m as usize].volatile == Some(VolKind::Partiallytrapped))
                {
                    self.spot(side, pos).add(VolKind::Partiallytrapped, Ends::Said);
                } else if matches!(name.as_str(), "lockon" | "mindreader") {
                    // On its user, aimed at the `[of]` Pokémon, which its next move cannot miss.
                    let aim = of.map(|(s, p)| (s, self.mon_ref(s, p).idx as usize, s * ACTIVE + p));
                    let spot = self.spot(side, pos);
                    spot.add(VolKind::Lockon, Ends::After(2));
                    if let Some(v) = spot.vols.iter_mut().find(|v| v.kind == VolKind::Lockon) {
                        v.by = aim;
                    }
                } else if name == "speedswap" {
                    if let Some((s, p)) = of.or(fourth) {
                        self.items
                            .push((begun, Event::Swap { first: self.mon_ref(side, pos), second: self.mon_ref(s, p) }));
                        self.spots[side][pos].swap_seen = true;
                        self.spots[s][p].swap_seen = true;
                        self.mix_stats(side, pos, (s, p), &[5]);
                    }
                } else if matches!(name.as_str(), "powersplit" | "guardsplit") {
                    if let Some((s, p)) = of.or(fourth) {
                        self.mix_stats(side, pos, (s, p), if name == "powersplit" { &[1, 3] } else { &[2, 4] });
                    }
                } else if matches!(name.as_str(), "afteryou" | "quash") {
                    let spot = self.spot(side, pos);
                    spot.reordered = Some(spot.reordered.map_or(begun, |t| t.min(begun)));
                } else if matches!(name.as_str(), "quickclaw" | "quickdraw" | "custapberry") {
                    // Instruct resolves a fresh move and can roll its item again;
                    // that roll did not promote the original queued action. Nor does a roll for the
                    // move an Encore has just put in its place: it goes with that one.
                    if self.spots[side][pos].overridden.is_some() {
                        self.spot(side, pos).quick_again = true;
                    } else if !self.spots[side][pos].instructed {
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
                } else if matches!(name.as_str(), "spite" | "eeriespell" | "leppaberry") {
                    // PP taken from one of its moves, or given back: either side's (D84).
                    let restored = if self.reader.live_record(side, pos).is_some_and(|rec| rec.ability == ab::RIPEN)
                        && !self.spots[side][pos].has(VolKind::Gastroacid)
                    {
                        20
                    } else {
                        10
                    };
                    let leppa = name == "leppaberry";
                    let n: u8 = arg(5).parse().unwrap_or(if leppa { restored } else { 0 });
                    let id = move_id(&to_id(arg(4)));
                    if side == self.side {
                        if let (Some(id), Some(own)) = (id, self.own.get_mut(self.own_at[pos])) {
                            let known = if own.copied.is_empty() { &mut own.pp } else { &mut own.copied };
                            if let Some(slot) = known.iter_mut().find(|s| s.0 == id) {
                                slot.1 = if leppa { (slot.1 + n).min(slot.2) } else { slot.1.saturating_sub(n) };
                            }
                        }
                    } else if let (Some(id), Some(entry)) =
                        (id, self.reader.sides[side].active[pos].as_ref().map(|l| l.entry))
                    {
                        // The other side's, kept as PP spent.
                        if self.their_pp.len() <= entry {
                            self.their_pp.resize(entry + 1, Vec::new());
                        }
                        let spent = &mut self.their_pp[entry];
                        let most = MOVES[id as usize].pp;
                        match spent.iter_mut().find(|s| s.0 == id) {
                            Some(s) => s.1 = if leppa { s.1.saturating_sub(n) } else { s.1.saturating_add(n).min(most) },
                            None if !leppa => spent.push((id, n.min(most))),
                            None => {}
                        }
                        self.spots[side][pos].spend(id, if leppa { -(n as i16) } else { n as i16 });
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
                if matches!(from, "move: Trick" | "move: Switcheroo")
                    && let Some((a, b)) = self.tricked
                {
                    // It got this from the other of the two: the other's item was taken (an Unburden goes up).
                    let (s2, p2) = if (side, pos) == a { b } else { a };
                    self.spots[s2][p2].lose_item();
                }
                let spot = self.spot(side, pos);
                spot.item_since = item.or(spot.item_since);
                if !from.is_empty() && !from.contains("Frisk") {
                    // (Not an item it had all along coming to light: a Choice item that
                    // comes, even in place of another, starts afresh.)
                    (spot.choice, spot.no_hold, spot.lock) = (None, false, None);
                }
                let taken =
                    ["move: Thief", "move: Covet", "ability: Pickpocket", "ability: Magician", "ability: Symbiosis"];
                if let Some((s, p)) = of.filter(|_| taken.contains(&from)) {
                    // Taken from, or handed over by, the Pokémon named last.
                    let spot = &mut self.spots[s][p];
                    (spot.choice, spot.item_since) = (None, Some(it::NONE));
                }
                // Its last item is back (Recycle, Harvest: `lastItem = ''`). One Pickup picks up is the last item
                // of a Pokémon beside it that used one up this turn, which has it no more.
                if matches!(from, "move: Recycle" | "ability: Harvest") {
                    let idx = self.mon_ref(side, pos).idx as usize;
                    self.used_up[side][idx] = (it::NONE, false);
                }
                if from == "ability: Pickup"
                    && let Some(item) = item
                {
                    let from: Vec<(usize, usize)> = (0..2)
                        .flat_map(|s| (0..ACTIVE).map(move |p| (s, p)))
                        .filter(|&(s, p)| (s, p) != (side, pos) && self.present(s, p) && self.spots[s][p].used_item)
                        .filter(|&(s, p)| self.used_up[s][self.mon_ref(s, p).idx as usize].0 == item)
                        .collect();
                    match from[..] {
                        [(s, p)] => {
                            let idx = self.mon_ref(s, p).idx as usize;
                            self.used_up[s][idx] = (it::NONE, false);
                        }
                        // (From one of two: each keeps it here.)
                        [(s, p), (s2, p2)] => {
                            let pair = ((s, self.mon_ref(s, p).idx as usize), (s2, self.mon_ref(s2, p2).idx as usize));
                            self.taken_doubt.retain(|t| ![t.0, t.1].iter().any(|x| *x == pair.0 || *x == pair.1));
                            self.taken_doubt.push((pair.0, pair.1, item));
                        }
                        _ => {}
                    }
                }
            }
            "-mustrecharge" => {
                self.spot(side, pos).add(VolKind::Mustrecharge, Ends::Said);
            }
            "-ability" => {
                if to_id(arg(3)) == "supersweetsyrup" {
                    let idx = self.mon_ref(side, pos).idx as usize;
                    self.syrup[side][idx] = true;
                }
                if ability_id(&to_id(arg(3))).is_some_and(|a| ABILITIES[a as usize].flags & AF_CANTSUPPRESS != 0) {
                    self.spot(side, pos).remove(VolKind::Gastroacid);
                }
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
                    if let Some(own) = VolKind::named(&to_id(arg(3)))
                        && spot.add(own, Ends::Said)
                        && let Some(v) = spot.vols.iter_mut().find(|v| v.kind == own)
                    {
                        // As long as the charging turn lasts: the engine's `duration`.
                        v.left = own.data().duration;
                    }
                }
            }
            "-anim" => {
                // An instant charging move's animation publicly resolves a
                // target omitted by its move line (including Encore rerolls).
                if side == self.side
                    && let Some((idx, copied, paying, old, was_unknown)) = self.animation_pp.take()
                    && let Some((s, p)) = fourth
                {
                    let pressed = (s != side
                        && self.present(s, p)
                        && self.spots[s][p].pressing
                        && !self.spots[s][p].has(VolKind::Gastroacid)
                        && self.reader.live_record(s, p).is_some_and(|rec| rec.ability == ab::PRESSURE))
                        as u8;
                    if let Some(own) = self.own.get_mut(idx) {
                        let known = if copied { &mut own.copied } else { &mut own.pp };
                        if let Some(slot) = known.iter_mut().find(|v| v.0 == paying) {
                            slot.1 = (slot.1 + old).min(slot.2).saturating_sub(pressed);
                        }
                        if !was_unknown {
                            own.unknown_pp[copied as usize].retain(|v| *v != paying);
                        }
                    }
                }
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
                // The action was registered before the self-damage changed HP.
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
                    let known_off = s != side || self.spots[s][p].known_off;
                    let spot = self.spot(side, pos);
                    (spot.types, spot.added) = (Some(types), added);
                    (spot.copied, spot.copied_foe, spot.known_off) = (true, s != side, known_off);
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
                // This explicit change supersedes Reflect Type's pending settlement, and shows its types
                // (`setType`: `knownType = true`).
                (spot.copied, spot.copied_foe, spot.known_off) = (false, false, false);
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
                // Its layers, as the line counts them; the stages it raises come next.
                let layers: u16 = effect.trim_start_matches("stockpile").parse().unwrap_or(1);
                let spot = self.spot(side, pos);
                spot.add(VolKind::Stockpile, Ends::Said);
                if let Some(v) = spot.vols.iter_mut().find(|v| v.kind == VolKind::Stockpile) {
                    v.data = layers;
                }
                self.stockpiled = Some((side, pos));
            }
            _ if matches!(name.as_str(), "futuresight" | "doomdesire") => {
                // On whoever stands where it was aimed, in two turns' time.
                let target = self.mover.as_ref().filter(|m| (m.side, m.pos) == (side, pos)).and_then(|m| m.target);
                if let Some((s, p)) = target.or(of) {
                    // (The line names its user, which the hit comes from wherever it is by then.)
                    let maker = self.maker(side, pos);
                    let slots = &mut self.field.sides[s].slots[p];
                    if !slots.iter().any(|c| c.0 == SlotCond::Futuremove) {
                        slots.push((SlotCond::Futuremove, 3, Some(maker)));
                    }
                }
            }
            _ => {
                if let Some(kind) = VolKind::named(&name) {
                    let spot = self.spot(side, pos);
                    spot.add(kind, Ends::Said);
                    if kind == VolKind::Encore && begun == 0 {
                        spot.overridden = Some(at);
                        if side == self.side {
                            // Showdown draws the move's target now (`getRandomTarget`, by the move it repeats:
                            // its user for one on itself or its side, its partner for one on an ally, none
                            // without one; else one of the other side's standing, `Side#randomFoe`, with nobody
                            // standing its first place).
                            let foe = 1 - side;
                            let standing: Vec<usize> = (0..ACTIVE).filter(|&p| self.present(foe, p)).collect();
                            let ally = 1 - pos;
                            let forced = match MOVES.get(self.spots[side][pos].last_move as usize).map(|d| d.target) {
                                Some(
                                    Target::User
                                    | Target::All
                                    | Target::AllySide
                                    | Target::AllyTeam
                                    | Target::AdjacentAllyOrSelf,
                                ) => Some(-(pos as i8 + 1)),
                                Some(Target::AdjacentAlly) => Some(if self.present(side, ally) { -(ally as i8 + 1) } else { 0 }),
                                _ => match standing[..] {
                                    [p] => Some(p as i8 + 1),
                                    [] => Some(1),
                                    _ => None,
                                },
                            };
                            self.encore_queued[pos] = Some(EncoreQueued { fell: false, forced });
                        }
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
        // (A move line's `TryMove` with nothing after it ran; a move that only changes stages is over.)
        self.try_resolve("", &[]);
        self.boosts_settle();
        self.hits_settle();
        let Some(req) = Req::parse(json)? else {
            self.req = None;
            return Ok(());
        };
        if req.asked == Asked::Preview || !self.started {
            self.req = Some(req);
            return Ok(());
        }
        // (Replacements are asked for with the queue empty: `checkFainted` has run, and runs again
        // for one that fell coming in.)
        if req.asked == Asked::Switch && self.upkept {
            self.check_fainted();
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
        // Trace starting on whoever came in (`onStart`: `seek` unless a foe on the field has no ability).
        for s in 0..2 {
            for p in 0..ACTIVE {
                if std::mem::take(&mut self.spots[s][p].trace_due) && self.ability_at(s, p) == ab::TRACE {
                    let blank = self.spots[s][p].trace_blank.unwrap_or_else(|| {
                        (0..ACTIVE).any(|q| self.present(1 - s, q) && self.ability_at(1 - s, q) == ab::NOABILITY)
                    });
                    self.spots[s][p].trace_seek = !blank;
                }
            }
        }
        if !self.leads_started {
            // Its own leads came in before it knew what they hold.
            self.leads_started = true;
            for pos in 0..ACTIVE {
                if self.present(self.side, pos) {
                    self.metronome_starts(self.side, pos);
                }
            }
        }
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

    /// The ability the other side's Pokémon at `pos` has now, as this side can tell: as shown, or as
    /// worked out after an allied Skill Swap, or else (with the sheets open) its sheet's, its Mega's.
    fn their_ability_now(&self, pos: usize) -> Option<u16> {
        let opp = 1 - self.side;
        let live = self.reader.sides[opp].active[pos].as_ref()?;
        if live.rec.ability != UNKNOWN {
            return Some(live.rec.ability);
        }
        if let Some(a) = self.spots[opp][pos].their_ability {
            return Some(a);
        }
        let sheet = self.reader.sides[opp].roster.get(live.rec.listed as usize).filter(|_| self.open())?;
        Some(if is_mega(live.species) { SPECIES[live.species as usize].ability0 } else { sheet.ability })
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
                for &(kind, turns, maker) in &track.slots[pos] {
                    if !b.sides[side].slot_conds[pos].has(kind) {
                        let mut c = Cond::new(kind);
                        // The turn it keeps, as the observation reads it back (`obs::slot_left`).
                        if TIMED_SLOTS.iter().any(|t| t.0 == kind) {
                            c.st.a = slot_kept(&b, kind, turns as i32);
                        }
                        // Who made it, and where it stood: the engine heals half its maker's max HP
                        // (Wish, stored as it was made; the other side's maker's is drawn by belief), lets
                        // a future move strike from its user and puts a Wish back where it was made.
                        // (The heal itself is written once the stats are: `complete`.)
                        if let Some(Maker { side: ms, idx, slot }) = maker {
                            (c.source, c.source_slot) = (Some(MonRef { side: ms as u8, idx: idx as u8 }), slot as u8);
                        }
                        c.targeted = kind == SlotCond::Healingwish && track.wish_met[pos];
                        b.sides[side].slot_conds[pos].push(c);
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
                // (Standing, its stats as Showdown keeps them where they are not its sheet's: Power Trick's swap, a
                // split with another of its own.)
                if k < ACTIVE && !mon.fainted && self.own_at[k] == idx {
                    let spot = &self.spots[me][k];
                    if let Some(st) = spot.stored {
                        m.stats = st;
                    } else if spot.trick_copied {
                        m.stats.swap(1, 2);
                    }
                }
            }
            m.stats[0] = if mon.fainted { self.stats[own.entry][0] } else { mon.max_hp };
            (m.hp, m.status, m.fainted) = (mon.hp, mon.status, mon.fainted);
            m.faint_queued = mon.fainted;
            // (Fallen where it stands, on a side that had nobody to send in: it keeps its status.)
            if mon.fainted
                && k < ACTIVE
                && let Some((status, tox)) = self.faint_status[me][idx]
            {
                (m.status, m.tox_stage) = (status, if status == Status::Tox { tox } else { 0 });
            }
            (m.ability, m.base_ability, m.item) = (mon.ability, mon.base_ability, mon.item);
            if k < ACTIVE
                && self.spots[me][k].ability_touched
                && let Some(live) = live
                && live.rec.ability != UNKNOWN
            {
                m.ability = live.rec.ability;
            }
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
            if m.is_active
                && m.ability == ab::UNBURDEN
                && m.item == it::NONE
                && self.spots[me][k].lost_item
                && !self.spots[me][k].has(VolKind::Gastroacid)
            {
                // Unburden is at work, which the engine keeps as a condition of its own. (Not under Gastro Acid,
                // as for the other side's below.)
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
        side.pokemon_left = side.n - req.mons.iter().filter(|m| m.fainted).count() as u8;
        side.total_fainted = self.faints[me];
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
        side.total_fainted = self.faints[opp];
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
            let fell_here = them.active.iter().any(|l| l.as_ref().is_some_and(|l| l.gone && l.entry == idx));
            if m.fainted
                && fell_here
                && let Some((status, tox)) = self.faint_status[opp][idx]
            {
                (m.status, m.tox_stage) = (status, if status == Status::Tox { tox } else { 0 });
            }
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
                    // A `trapper` goes once nobody it held is still held (`Battle::unlink_volatile`).
                    if v.kind == VolKind::Trapper && !self.holds_anyone(s, pos) {
                        continue;
                    }
                    let mut c = Cond::new(v.kind);
                    (c.data, c.duration) = (v.data, v.duration);
                    if TIMED_VOLATILES.contains(&v.kind) {
                        c.duration = v.turns_left();
                    }
                    if v.kind == VolKind::Stockpile {
                        (c.st.a, c.st.b) = v.given;
                    }
                    b.vols[s][pos].push(c);
                }
            }
        }
        // How long each frozen Pokémon has stayed frozen (the observation reads it off its turns).
        for s in 0..2 {
            for idx in 0..b.sides[s].n as usize {
                let m = &mut b.sides[s].team[idx];
                if m.status == Status::Frz {
                    m.status_time = frozen_time(self.frozen[s][idx]);
                }
            }
        }
        Ok(b)
    }

    /// Keeps, line by line, what [`Follower::game_state`] needs that the observation does not
    /// read: how each Pokémon's moves have gone, the hits it has taken, the counts of the
    /// hidden-length effects, the items used up. Runs after [`Follower::track`] has taken the line in.
    fn keep(&mut self, kind: &str, parts: &[&str], from: &str) {
        let arg = |i: usize| parts.get(i).copied().unwrap_or("");
        // (Not at the line of a move Magic Bounce sends back: what the move itself did at its other targets
        // comes after.)
        if matches!(kind, "move" | "cant" | "upkeep" | "turn" | "") && from != "ability: Magic Bounce" {
            self.boosts_settle();
        }
        // Berserk's berry check is on again at every target the move reached (`onAfterMoveSecondary`, which runs at
        // the hit loop's targets, a status move's included), as the move ends: by the next action (a confusion's
        // `-activate` begins one).
        let next_action = matches!(kind, "move" | "cant" | "upkeep" | "turn" | "") || (kind == "-activate" && arg(3) == "confusion");
        if next_action && from != "ability: Magic Bounce" && self.under_way.is_some() && !self.berserk_done {
            for &((s, p), ..) in &self.reached {
                self.spots[s][p].berserk_off = false;
            }
            // (Its user too, for a move aimed at itself that worked there: Protect, Swords Dance, Howl at itself.)
            if let Some((user, ..)) = self.under_way
                && self.self_aimed
                && if only_boosts(self.last_move) {
                    self.boosted_at.contains(&user)
                } else {
                    self.spots[user.0][user.1].result_now == Some(Some(true))
                }
            {
                self.spots[user.0][user.1].berserk_off = false;
            }
            self.berserk_done = true;
        }
        // Where its own move Champions' Encore put in again went, when its line named nobody but a
        // later line of the move names it (D89): `-anim` (Power Herb, a charging move let off in sun
        // or rain), a heal that fails on one at full HP (`-fail|T|heal`), Armor Tail or Queenly
        // Majesty (`cant|H|ability: ...|M|[of] T`). (Not after one of the other side fell: then
        // Showdown drew again, and the line names the second draw, not the one the game keeps.)
        if let Some((user, ..)) = self.under_way
            && user.0 == self.side
            && self.encore_unnamed == Some(user.1)
        {
            let told = match kind {
                "-anim" if self.place_of(arg(2)) == Some(user) => self.place_of(arg(4)),
                "-fail" if arg(3) == "heal" => self.place_of(arg(2)),
                "cant" => parts.iter().find_map(|x| x.strip_prefix("[of] ")).and_then(|w| self.place_of(w)),
                _ => None,
            };
            if let Some((_, p)) = told.filter(|t| t.0 != self.side) {
                self.encore_unnamed = None;
                self.spots[user.0][user.1].last_move_loc = Some(p as i8 + 1);
                let idx = self.own_at[user.1];
                if idx < MAX_TEAM {
                    self.encore_doubt[idx] = false;
                }
            }
        }
        match kind {
            "turn" => {
                // `Battle#endTurn` has run: this turn's results become last turn's, a stat change
                // made while the first turn was being set up counts into it, and hits from
                // Pokémon no longer on the field are forgotten.
                let first = self.turn == 1;
                let here: Vec<(usize, usize)> = (0..2)
                    .flat_map(|s| (0..ACTIVE).map(move |p| (s, p)))
                    .filter(|&(s, p)| self.present(s, p))
                    .map(|(s, p)| (s, self.mon_ref(s, p).idx as usize))
                    .collect();
                let ghosts: Vec<Vec<bool>> = (0..2)
                    .map(|s| (0..ACTIVE).map(|p| self.ghost_now(s, p)).collect())
                    .collect();
                for (s, row) in self.spots.iter_mut().enumerate() {
                    for (p, spot) in row.iter_mut().enumerate() {
                        spot.turn_pos = Some(p);
                        spot.turn_ghost = Some(ghosts[s][p]);
                        spot.result_last = spot.result_now.take();
                        spot.result_doubt = [false, spot.result_doubt[0]];
                        if !first {
                            (spot.lowered, spot.raised, spot.used_item) = (false, false, false);
                        }
                        spot.damaged_by.retain(|&(idx, _, _, _)| here.contains(&(1 - s, idx)));
                        for bit in 0..2 * MAX_TEAM {
                            if spot.attacked_by & 1 << bit != 0 && !here.contains(&(bit / MAX_TEAM, bit % MAX_TEAM)) {
                                spot.attacked_by &= !(1 << bit);
                            }
                        }
                    }
                }
                return;
            }
            "upkeep" => {
                (self.under_way, self.reached) = (None, Vec::new());
                for s in 0..2 {
                    for p in 0..ACTIVE {
                        if !self.present(s, p) {
                            continue;
                        }
                        let idx = self.mon_ref(s, p).idx as usize;
                        if self.rampage[s][idx] != NO_MOVE && !self.spots[s][p].rampage_moved {
                            // It did not use the move it was locked into (it hurt itself in confusion,
                            // or could not move): nothing renewed the lock, which lapses now, with
                            // nothing said if it is already confused.
                            (self.rampage[s][idx], self.counts.rampage[s][idx]) = (NO_MOVE, 0);
                        }
                        if self.rampage[s][idx] != NO_MOVE
                            && self.reader.live_record(s, p).is_some_and(|r| r.status == Status::Slp)
                        {
                            // Asleep at the turn's end: the lock's residual (after every ordered one, Yawn's
                            // included) drops it without a word.
                            (self.rampage[s][idx], self.counts.rampage[s][idx]) = (NO_MOVE, 0);
                        }
                        if self.rampage[s][idx] != NO_MOVE {
                            self.counts.rampage[s][idx] = self.counts.rampage[s][idx].saturating_add(1);
                            self.rampage_turns[s][idx] = self.rampage_turns[s][idx].saturating_add(1);
                            if self.rampage_turns[s][idx] >= 3 {
                                // Its third turn: a rampage lasts two or three, so it is over now, whether
                                // or not anything was said (a holder already confused is told nothing).
                                (self.rampage[s][idx], self.counts.rampage[s][idx]) = (NO_MOVE, 0);
                            }
                        }
                        let spot = &mut self.spots[s][p];
                        if spot.has(VolKind::Partiallytrapped) {
                            spot.bound = spot.bound.saturating_add(1);
                        }
                        for v in self.spots[s][p].vols.iter_mut() {
                            v.left = v.left.saturating_sub(1);
                        }
                    }
                }
                return;
            }
            _ => {}
        }
        let Some((side, pos)) = self.place_of(arg(2)) else {
            return;
        };
        // The follower's own HP, exactly as its log has it.
        // (A faint is `0 fnt`, with no maximum.)
        let shown_hp = |s: &str| {
            let hp = s.split(' ').next()?;
            if hp == "0" {
                return Some(0);
            }
            hp.split_once('/').and_then(|(hp, _)| hp.parse::<u16>().ok())
        };
        let before = self.own_hp[pos];
        if side == self.side {
            let at = if matches!(kind, "switch" | "drag" | "replace") { 4 } else { 3 };
            if matches!(kind, "switch" | "drag" | "replace" | "-damage" | "-heal" | "-sethp")
                && let Some(hp) = shown_hp(arg(at))
            {
                self.own_hp[pos] = Some(hp);
            }
            if kind == "faint" {
                self.own_hp[pos] = Some(0);
            }
        }
        self.hp_told(kind, side, pos, parts, from);
        let idx = self.mon_ref(side, pos).idx as usize;
        let mover = self.mover.as_ref().map(|m| (m.side, m.pos)).filter(|&m| m != (side, pos));
        let single = |m: u16| {
            MOVES.get(m as usize).is_some_and(|d| {
                matches!(
                    d.target,
                    Target::Normal
                        | Target::Any
                        | Target::AdjacentFoe
                        | Target::AdjacentAlly
                        | Target::AdjacentAllyOrSelf
                )
            })
        };
        // The move under way came to nothing at this target: missed or failed (`Some(false)`), or
        // blocked (`None`: Protect's `NOT_FAIL`). It did not reach it, which takes back what its
        // start put down there; and its result is how things now stand (a move at one target
        // failed or was blocked; one at several failed if it only missed, came to nothing if it
        // was only blocked, and worked if it hit anybody).
        // It did not reach this one after all: what its start put down there is taken back.
        let unreach = |me: &mut Follower, (s, p): (usize, usize)| {
            if let Some(k) = me.reached.iter().position(|r| r.0 == (s, p)) {
                let (_, attacked, by) = me.reached.remove(k);
                (me.spots[s][p].attacked, me.spots[s][p].attacked_by) = (attacked, by);
            }
        };
        let missed = |me: &mut Follower, (s, p): (usize, usize), result: Option<bool>| {
            unreach(me, (s, p));
            // A move on the whole field (Perish Song, Haze) is not undone by one Pokémon it does not
            // touch (`tryMoveHit`'s field path counts a target that shrugs it off as done).
            if MOVES.get(me.last_move as usize).is_some_and(|d| {
                matches!(d.target, Target::All | Target::AllySide | Target::FoeSide | Target::AllyTeam)
            }) {
                return;
            }
            if let Some((s, i)) = me.rampage_pending.take()
                && me.under_way.is_some_and(|u| !u.1)
            {
                me.rampage[s][i] = NO_MOVE;
            }
            let Some((user, hit, failed, blocked)) = me.under_way.as_mut() else { return };
            match result {
                Some(_) => *failed = true,
                None => *blocked = true,
            }
            let (user, hit, failed, blocked) = (*user, *hit, *failed, *blocked);
            // (Judged on the move under way, which for one sent back or called is not the user's last.)
            // (A target the move still reaches is one it has not been undone at: Showdown's
            // `trySpreadMoveHit` succeeds if any target is left after every step, and otherwise fails if
            // any step failed outright, or comes to nothing.)
            let outcome = if single(me.last_move) {
                result
            } else if hit || me.subs_took || !me.reached.is_empty() {
                // (A Substitute's share counts as a success in Champions, `HIT_SUBSTITUTE`.)
                Some(true)
            } else if failed {
                Some(false)
            } else if blocked {
                None
            } else {
                Some(true)
            };
            me.spots[user.0][user.1].result_now = Some(outcome);
        };
        match kind {
            "move" => {
                let Some(used) = move_id(&to_id(arg(3))) else { return };
                if self.refused {
                    (self.under_way, self.try_pending) = (None, None);
                    self.reached.clear();
                    return;
                }
                // Every move line goes on to `TryMove` (a bounced move as its bouncer's), unless it had no target
                // at all, which returns before it (`useMoveInner`, its line's target blank or `null`; a `[notarget]`
                // naming one comes later, from `trySpreadMoveHit` finding nobody left there).
                if !parts.contains(&"[notarget]") || !matches!(arg(4), "" | "null") {
                    let charged = self.spots[side][pos].has(VolKind::Twoturnmove);
                    self.try_pending = Some(((side, pos), used, charged, false));
                }
                let bounced = from == "ability: Magic Bounce" || from == "move: Magic Coat";
                // Where a spread move is sent back, Showdown writes each move's `[spread]` list after its hit loop on
                // the last move line, which by then is this one: the move sent back's first (if it had two targets),
                // then the one under way's (if it had two: the bouncer's partner too).
                let spreads: Vec<&str> = parts.iter().filter_map(|p| p.strip_prefix("[spread]").map(str::trim)).collect();
                let (mut own_spread, mut their_spread) = (spreads.first().copied(), None);
                if bounced && let Some((user, ..)) = self.under_way {
                    let wide = matches!(MOVES[used as usize].target, Target::AllAdjacentFoes | Target::AllAdjacent);
                    let (theirs, back) = (wide && self.present(side, 1 - pos), wide && self.present(user.0, 1 - user.1));
                    (own_spread, their_spread) = match (back, theirs, spreads.as_slice()) {
                        (true, true, [a, b]) => (Some(*a), Some(*b)),
                        (true, false, [a]) => (Some(*a), None),
                        (false, true, [b]) => (None, Some(*b)),
                        _ => (None, None),
                    };
                }
                self.bounced_at = if bounced { self.under_way.map(|u| u.0) } else { None };
                self.called_now = matches!(from, "move: Copycat" | "move: Sleep Talk");
                if bounced {
                    // It sends back the move under way, which never reached it (its `onTryHit` returns
                    // null: that move fails there). One on a side or the field fails outright.
                    missed(self, (side, pos), Some(false));
                    if let Some((user, ..)) = self.under_way
                        && MOVES.get(self.last_move as usize).is_some_and(|d| {
                            matches!(d.target, Target::FoeSide | Target::AllySide | Target::All | Target::AllyTeam)
                        })
                    {
                        self.spots[user.0][user.1].result_now = Some(Some(false));
                    }
                    // The one under way goes on at the others it was aimed at, after this one: those its list names
                    // were attacked by its user, and it worked.
                    if let Some(((us, up), ..)) = self.under_way
                        && let Some(list) = their_spread
                    {
                        let (by, stay) = (self.mon_ref(us, up).idx as usize, self.spots[us][up].stay);
                        let bit = 1 << (us * MAX_TEAM + by);
                        let mut hit = false;
                        let named: Vec<(usize, usize)> = list
                            .split(',')
                            .map(str::trim)
                            .filter_map(|t| Some((Follower::side_of(t)?, t.as_bytes().get(2)?.wrapping_sub(b'a') as usize)))
                            .collect();
                        // (Those its line named that the list leaves out it did not reach: Mirror Armor, a stat change
                        // that did nothing.)
                        for t in self.reached.iter().map(|r| r.0).filter(|t| !named.contains(t)).collect::<Vec<_>>() {
                            unreach(self, t);
                        }
                        for (s, p) in named {
                            if p < ACTIVE && (s, p) != (side, pos) && self.present(s, p) {
                                // (One its line named was marked then.)
                                if !self.reached.iter().any(|r| r.0 == (s, p)) {
                                    let spot = &mut self.spots[s][p];
                                    (spot.attacked, spot.attacked_by) = (true, spot.attacked_by | bit);
                                    spot.attacked_stay[bit.trailing_zeros() as usize] = stay;
                                }
                                hit = true;
                            }
                        }
                        // (Its list is every target left after its hit loop: none left, it failed, for the bouncer's null
                        // counts as a failure, `hitStepTryHitEvent`'s `hitResults[i] || false`.)
                        self.spots[us][up].result_now = Some(Some(hit));
                    }
                }
                (self.blanked, self.boosted_at) = (arg(4).is_empty() && parts.contains(&"[still]"), Vec::new());
                self.spots[side][pos].result_doubt[0] = false;
                (self.subs_took, self.berserk_done) = (false, false);
                (self.darts_open, self.darts_aim) = (MOVES[used as usize].smart_target, self.place_of(arg(4)));
                self.self_aimed = self.place_of(arg(4)) == Some((side, pos))
                    && matches!(MOVES[used as usize].target, Target::User | Target::Allies | Target::AdjacentAllyOrSelf);
                self.own_cond_new = MOVES[used as usize].target == Target::User
                    && MOVES[used as usize].volatile.is_some_and(|v| !self.spots[side][pos].has(v));
                self.copycat_copies = (used == mv::COPYCAT).then_some(self.last_move);
                self.last_move = used;
                (self.zero_hit, self.boosted, self.stockpiled) = (None, None, None);
                self.move_hits = [[0; ACTIVE]; 2];
                let queued = queued_move(arg(3), from);
                let caller = from.strip_prefix("move: ").and_then(|c| move_id(&to_id(c)));
                let paying = if queued && from != "lockedmove" { Some(used) } else if queued { None } else { caller };
                let target = self.place_of(arg(4));
                let spot = &mut self.spots[side][pos];
                if let Some(paying) = paying
                    && !spot.paid.contains(&paying)
                {
                    spot.paid.push(paying);
                }
                if queued {
                    spot.result_now = Some(Some(!parts.contains(&"[notarget]")));
                    spot.aimed = match target {
                        Some((s, p)) if s != side => p as i8 + 1,
                        // (At its own side, its partner or itself: a move on the user or its side
                        // has the user's own place.)
                        Some((_, p)) => -(p as i8 + 1),
                        None => 0,
                    };
                }
                // A Curse its user is no Ghost for is aimed at the user itself, whatever was chosen and
                // whatever the line names (`Battle#resolveAction`: `selfTarget`, on its types as the turn began).
                let self_curse = queued
                    && used == mv::CURSE
                    && !self.spots[side][pos].turn_ghost.unwrap_or_else(|| self.ghost_now(side, pos));
                if self_curse {
                    let at = self.spots[side][pos].turn_pos.unwrap_or(pos);
                    self.spots[side][pos].aimed = -(at as i8 + 1);
                }
                if bounced {
                    // (Using it anew sets its user's result afresh, `useMove`.)
                    self.spots[side][pos].result_now = Some(Some(true));
                }
                // A condition on a side already up there fails without a word (`addSideCondition` returns
                // false), or with its layers all laid (Spikes three, Toxic Spikes two).
                if let Some(c) = MOVES[used as usize].side_condition {
                    let at = if MOVES[used as usize].target == Target::FoeSide { 1 - side } else { side };
                    let most = match c {
                        SideCond::Spikes => 3,
                        SideCond::Toxicspikes => 2,
                        _ => 1,
                    };
                    if self.field.sides[at].conds.iter().any(|k| k.0 == c && k.1 >= most) {
                        self.spots[side][pos].result_now = Some(Some(false));
                    }
                }
                self.under_way = Some(((side, pos), false, false, false));
                self.reached.clear();
                let spread = own_spread;
                let own_slot = format!("{}{}", ["p1", "p2"][side], (b'a' + pos as u8) as char);
                self.spread_self = spread.is_some_and(|list| list.split(',').any(|t| t.trim() == own_slot));
                // Whom it reaches: the one named, or those a spread move lists.
                if !parts.contains(&"[notarget]") {
                    let bit = 1 << (side * MAX_TEAM + idx);
                    let mut reached: Vec<(usize, usize)> = match spread {
                        // (A spread move that hit nobody lists nobody: it was aimed at all it could reach,
                        // each of which a line says it did not, Protect, an absorbing ability...)
                        Some(list) if list.trim().is_empty() => (0..2)
                            .flat_map(|s| (0..ACTIVE).map(move |p| (s, p)))
                            .filter(|&(s, _)| MOVES[used as usize].target == Target::AllAdjacent || s != side)
                            .collect(),
                        Some(list) => list
                            .split(',')
                            .filter_map(|t| {
                                let t = t.trim();
                                let s = Follower::side_of(t)?;
                                let p = (t.as_bytes().get(2)? - b'a') as usize;
                                (p < ACTIVE).then_some((s, p))
                            })
                            .collect(),
                        None => target.into_iter().collect(),
                    };
                    reached.retain(|&t| t != (side, pos) && self.present(t.0, t.1));
                    // (A spread status move whose condition one of them has already did nothing there, without a word,
                    // as the others went on: `addVolatile` returns false, and the hit loop leaves it out. Teeter Dance
                    // at one confused.)
                    if spread.is_some() && MOVES[used as usize].category == Category::Status {
                        if let Some(v) = MOVES[used as usize].volatile {
                            reached.retain(|&(s, p)| !self.spots[s][p].has(v));
                        }
                    }
                    // (A move on a side or the whole field reaches nobody: it never goes through the hit loop,
                    // where Showdown files who attacked whom, `gotAttacked`.)
                    if matches!(MOVES[used as usize].target, Target::FoeSide | Target::AllySide | Target::All | Target::AllyTeam) {
                        reached.clear();
                    }
                    let stay = self.spots[side][pos].stay;
                    for (s, p) in reached {
                        let spot = &mut self.spots[s][p];
                        self.reached.push(((s, p), spot.attacked, spot.attacked_by));
                        spot.attacked_by |= bit;
                        spot.attacked_stay[bit.trailing_zeros() as usize] = stay;
                        spot.attacked = true;
                    }
                }
                self.rampage_pending = None;
                // (A move another calls, Copycat's, locks its user as well: Showdown's `useMove` adds the
                // move's own lock; only a Dancer's copy, an external move, does not, and no Dancer is in
                // Champions.)
                let called = caller.is_some() && from.starts_with("move: ");
                if (queued || called)
                    && from != "lockedmove"
                    && MOVES[used as usize].self_volatile == Some(VolKind::Lockedmove)
                {
                    // A rampage begins (or begins again), if the move lands.
                    (self.rampage[side][idx], self.counts.rampage[side][idx]) = (used, 0);
                    (self.rampage_turns[side][idx], self.rampage_silent[side][idx]) = (0, false);
                    self.rampage_pending = Some((side, idx));
                    let spot = &mut self.spots[side][pos];
                    (spot.rampage_moved, spot.rampage_seq) = (true, spot.next_seq());
                } else if from == "lockedmove" && self.rampage[side][idx] != NO_MOVE {
                    // A later turn of it: renewed if the move lands, over if it misses or is blocked.
                    self.rampage_pending = Some((side, idx));
                    self.spots[side][pos].rampage_moved = true;
                    // Its second turn may be its last (it lasts two or three), and then the confusion its
                    // end brings says so, unless it is taken without a word; a third turn says it was not.
                    self.rampage_silent[side][idx] =
                        self.rampage_turns[side][idx] == 1 && self.confusion_unsaid(side, pos);
                }
                // With no one left to aim at (`[notarget]`) the move fails before it can hit, as a miss does:
                // no lock begins, and one under way is not renewed (`useMoveInner`'s early return).
                if parts.contains(&"[notarget]")
                    && let Some((s, i)) = self.rampage_pending.take()
                {
                    (self.rampage[s][i], self.counts.rampage[s][i]) = (NO_MOVE, 0);
                }
            }
            "cant" if arg(3).starts_with("ability: ") && parts.iter().any(|p| p.starts_with("[of] ")) => {
                // Dazzling, Queenly Majesty, Armor Tail, Damp: the line names the holder, and the move that
                // failed is the `[of]` Pokémon's.
                let of = parts.iter().find_map(|p| p.strip_prefix("[of] ")).and_then(|p| self.place_of(p.trim()));
                if let Some((s, p)) = of {
                    self.spots[s][p].result_now = Some(Some(false));
                }
            }
            // Pollen Puff at an ally under Heal Block and short of full HP: the ally's Heal Block stops the heal
            // (`onTryHeal`, which says so under the user's name, Showdown's FIXME), and the move has worked, its `onHit`
            // returning `NOT_FAIL` there, which leaves the ally out of the hit loop. (Under Heal Block itself, the
            // user's move fails at `onTryMove`, below.)
            "cant"
                if arg(3) == "move: Heal Block"
                    && to_id(arg(4)) == "pollenpuff"
                    && !self.spots[side][pos].has(VolKind::Healblock) =>
            {
                self.spots[side][pos].result_now = Some(Some(true));
                if self.present(side, 1 - pos) {
                    unreach(self, (side, 1 - pos));
                }
            }
            // A target's Magic Bounce sent the move back and a condition of its own stopped that at once: the move
            // under way failed there (the bounce's `onTryHit` null counts as a failure, `hitStepTryHitEvent`), and so
            // did the move sent back.
            "cant" if self.bounce_stopped => {
                missed(self, (side, pos), Some(false));
                self.spots[side][pos].result_now = Some(Some(false));
            }
            "cant" => {
                self.spots[side][pos].result_now = Some(if arg(3) == "recharge" { None } else { Some(false) });
                if arg(3) == "frz" {
                    self.frozen[side][idx] = self.frozen[side][idx].saturating_add(1);
                }
                if arg(3) == "slp" {
                    // (Early Bird wakes it twice as fast, unless Gastro Acid has suppressed it.)
                    let early =
                        self.ability_at(side, pos) == ab::EARLYBIRD && !self.spots[side][pos].has(VolKind::Gastroacid);
                    let runs = 1 + early as u8;
                    // The run is told under the name it goes by (D96): one of two on the field that look the
                    // same, taken for the copy and numbered past the one it passes for, adds its run to that
                    // name's count as well as to its own, as the game does (`Counts::update`).
                    let name = if side == self.side { idx } else { self.their_name(pos) };
                    let was = self.counts.sleep[side][name];
                    let here = self.spots[side][pos].sleep_here.get_or_insert((was, 0, false, name as u8));
                    here.1 = here.1.saturating_add(runs);
                    self.counts.sleep[side][idx] = self.counts.sleep[side][idx].saturating_add(runs);
                    if name != idx {
                        self.counts.sleep[side][name] = was.saturating_add(runs);
                    }
                }
            }
            "-fail" => {
                // (A failure an ability or item reports, Oblivious against an Intimidate say, is not
                // the move's; nor is one after the turn's moves are done.)
                let doing = self.under_way.is_some_and(|u| u.0 == (side, pos));
                // A move whose effect prints a failure and then returns `NOT_FAIL` has still worked
                // (Showdown counts it a success): a heal at full HP, a Baton Pass with nobody to pass to.
                let not_fail = not_fail_on_hit(self.last_move, arg(3));
                // (A status the move would have given, kept off by an ability or Uproar: the move fails there;
                // so does a move on its user that an ability of its own stops, Insomnia against Rest. Not one
                // passed back by Synchronize, which is not the move's.)
                let d = MOVES.get(self.last_move as usize);
                let status = matches!(arg(3), "slp" | "par" | "brn" | "psn" | "tox" | "frz")
                    && d.is_some_and(|d| d.category == Category::Status);
                // (Nor one a Synchronize reports, passing a status on to one that has one already: `-fail|X|status`, or
                // `-fail|X` for another status, whose `[still]` blanks the last move line, `Pokemon#setStatus`.)
                if self.synced {
                    return;
                }
                let on_itself = d.is_some_and(|d| d.target == Target::User) && from.starts_with("ability: ");
                // A status move at all the Pokémon beside it whose line named nobody: each `-fail` is a Substitute
                // taking it at one target (`onTryPrimaryHit`'s, the user's), and the move goes on at the others
                // (where it failed at all of them, at `TryHit`, nothing follows; Showdown fails a spread move only
                // where no target passed).
                let spread = arg(3).is_empty()
                    && self.blanked
                    && doing
                    && from.is_empty()
                    && MOVES.get(self.last_move as usize).is_some_and(|d| {
                        d.category == Category::Status
                            && d.flags & F_BYPASSSUB == 0
                            && matches!(d.target, Target::AllAdjacentFoes | Target::AllAdjacent)
                    });
                // (With one Pokémon beside it to aim at, a spread move lists nobody, `spreadHit` being off: that one.)
                let lone = (spread && self.reached.is_empty() && !self.subs_took)
                    .then(|| {
                        let all_adjacent = MOVES[self.last_move as usize].target == Target::AllAdjacent;
                        let mut aims = (0..2)
                            .flat_map(|s| (0..ACTIVE).map(move |p| (s, p)))
                            .filter(|&(s, p)| (s, p) != (side, pos) && self.present(s, p))
                            .filter(|&(s, _)| all_adjacent || s != side);
                        match (aims.next(), aims.next()) {
                            (Some(t), None) => Some(t),
                            _ => None,
                        }
                    })
                    .flatten()
                    .filter(|&(s, p)| self.spots[s][p].has(VolKind::Substitute));
                let behind = self
                    .reached
                    .iter()
                    .map(|r| r.0)
                    .find(|&(s, p)| self.spots[s][p].has(VolKind::Substitute))
                    .or(lone);
                if spread && (behind.is_some() || self.subs_took) {
                    if let Some(t) = behind {
                        unreach(self, t);
                        self.subs_took = true;
                    }
                    // (Taken by a Substitute everywhere: each null counts as 0 in Champions' hit loop, a success.)
                    if self.reached.is_empty() {
                        self.spots[side][pos].result_now = Some(Some(true));
                    }
                    return;
                }
                let sub = arg(3).is_empty() && self.blanked && doing;
                let sub = sub.then(|| self.sub_took(side, pos)).flatten();
                // (Its own side cannot tell either where a Copycat or a Sleep Talk sent it at random.)
                let undecided =
                    arg(3).is_empty() && self.blanked && doing && (side != self.side || self.called_now) && sub.is_none();
                if doing
                    && (from.is_empty() || status || on_itself)
                    && self.mover.as_ref().is_some_and(|m| (m.side, m.pos) == (side, pos))
                {
                    if not_fail {
                    } else if matches!(arg(3), "move: Burn Up" | "move: Double Shock" | "move: Teatime") {
                        // Its own `onTryMove` (Teatime's `onHitField`, with no berry anywhere) returns null:
                        // nothing came of it, without failing.
                        self.spots[side][pos].result_now = Some(None);
                    } else if sub == Some(true) {
                        // A status move its target's Substitute took (`onTryPrimaryHit` returns null, which
                        // Champions' hit loop counts as 0): it worked, at nobody.
                        self.spots[side][pos].result_now = Some(Some(true));
                        for ((s, p), attacked, by) in std::mem::take(&mut self.reached) {
                            (self.spots[s][p].attacked, self.spots[s][p].attacked_by) = (attacked, by);
                        }
                    } else if self.spread_self && !arg(3).is_empty() {
                        // Its own share of a move on its side failed (Life Dew at full HP): the move goes
                        // on at the others.
                        missed(self, (side, pos), Some(false));
                    } else {
                        self.spots[side][pos].result_now = Some(Some(false));
                        // (Or a Substitute took it: either, for some may have had one up and some not.)
                        self.spots[side][pos].result_doubt[0] = undecided;
                        // It reached nobody.
                        for ((s, p), attacked, by) in std::mem::take(&mut self.reached) {
                            (self.spots[s][p].attacked, self.spots[s][p].attacked_by) = (attacked, by);
                        }
                    }
                } else if !doing
                    && arg(3) == "heal"
                    && self.blanked
                    && from.is_empty()
                    && !self.ending
                    && self.reached.is_empty()
                    && self.under_way.is_some()
                {
                    // A heal whose line named no target fails at the one this line names (a heal at full HP):
                    // as at a named target.
                    if !not_fail && let Some((user, ..)) = self.under_way {
                        self.spots[user.0][user.1].result_now = Some(Some(false));
                    }
                } else if !doing
                    && ((from.is_empty() && !matches!(arg(3), "slp" | "par" | "brn" | "psn" | "tox" | "frz")) || status)
                    && !self.ending
                    && self.reached.iter().any(|r| r.0 == (side, pos))
                {
                    // It failed at this target alone (a status it already has, a heal at full HP, Soak):
                    // the target is dropped and was not reached; the move fails if nobody is left. (One
                    // whose effect returns `NOT_FAIL` is only not reached; Corrosive Gas, which says it failed and
                    // returns nothing, still reached it: its `onHit` ran there.)
                    if not_fail && arg(3) == "move: Corrosive Gas" {
                    } else if not_fail {
                        unreach(self, (side, pos));
                    } else {
                        missed(self, (side, pos), Some(false));
                    }
                }
            }
            "-miss" => {
                let at = self.place_of(arg(3));
                if let Some(t) = at {
                    missed(self, t, Some(false));
                } else if single(self.spots[side][pos].last_move) {
                    self.spots[side][pos].result_now = Some(Some(false));
                }
            }
            // (One naming the move's own user is not the move's: Synchronize passing a status back.)
            "-immune" if self.under_way.is_some_and(|u| u.0 == (side, pos)) => {}
            // (As above: Own Tempo keeping Swagger's confusion off a target whose Attack it raised.)
            "-immune" if !status_kept_off(self.last_move, self.boosted_at.contains(&(side, pos)))
                && MOVES.get(self.last_move as usize).is_some_and(|d| d.category == Category::Status) => {}
            // (Only at a target the move reached, or one its line named nobody for: an Intimidate's `-immune`
            // in the middle of a move is not the move's.)
            "-immune"
                if self.reached.iter().any(|r| r.0 == (side, pos)) || (self.blanked && self.reached.is_empty()) =>
            {
                missed(self, (side, pos), Some(false))
            }
            "-immune" => {}
            // Pollen Puff struck an ally more than once (Parental Bond): `gotAttacked` reads the last hit's targets,
            // which leave out an ally the last hit healed nothing (`onHit` returns `NOT_FAIL` at full HP).
            "-hitcount"
                if MOVES.get(self.last_move as usize).is_some_and(|d| {
                    d.multiaccuracy && arg(3).parse::<u8>().is_ok_and(|n| n < d.multihit.1)
                }) =>
            {
                self.hits_cut = true;
            }
            "-hitcount" if self.last_move == mv::POLLENPUFF && self.under_way.is_some_and(|u| u.0.0 == side) => {
                let hits = arg(3).parse::<usize>().unwrap_or(1);
                if self.boosted_at.iter().filter(|&&t| t == (side, pos)).count() < hits {
                    unreach(self, (side, pos));
                    self.boosted_at.retain(|&t| t != (side, pos));
                }
            }
            // Hurt itself in confusion: its move failed (`BeforeMove` returned false).
            "-damage" if from == "confusion" => {
                let spot = &mut self.spots[side][pos];
                (spot.result_now, spot.berserk_off) = (Some(Some(false)), true);
            }
            "-damage" if from.is_empty() && self.future_hit.is_some_and(|f| f.0 == (side, pos)) => {
                // A future move's blow: from its maker, at the place it stands now, fainted there included (its
                // `position` is a place on the field until it is replaced); none once it has left the field
                // (`NO_SLOT`).
                let Some((_, m)) = self.future_hit.take() else { return };
                let at = (0..ACTIVE).find(|&p| {
                    self.reader.sides[m.side].active[p].is_some() && self.mon_ref(m.side, p).idx as usize == m.idx
                });
                let slot = at.map_or(NO_SLOT as usize, |p| m.side * ACTIVE + p);
                let bit = 1 << (m.side * MAX_TEAM + m.idx);
                let spot = &mut self.spots[side][pos];
                spot.hits = spot.hits.saturating_add(1);
                (spot.attacked, spot.last_damage) = (true, 1);
                spot.attacked_by |= bit;
                if m.side != side {
                    spot.damaged_by.retain(|d| d.0 != m.idx);
                    spot.damaged_by.push((m.idx, slot, 1, 0));
                }
            }
            "-damage" if from.is_empty() && !self.ending => {
                if let Some((user, hit, _, _)) = self.under_way.as_mut()
                    && Some(*user) == mover
                {
                    *hit = true;
                    let user = *user;
                    self.spots[user.0][user.1].result_now = Some(Some(true));
                    self.rampage_pending = None;
                }
                if let Some((s, p)) = mover {
                    let by = self.mon_ref(s, p).idx as usize;
                    let damage = match (side == self.side, before, self.own_hp[pos]) {
                        // (The blow fell on its Disguise: a hit, of nothing.)
                        _ if self.zero_hit.take() == Some((side, pos)) => 0,
                        (true, Some(was), Some(now)) => was.saturating_sub(now),
                        _ => 1,
                    };
                    let stay = self.spots[s][p].stay;
                    // A blow on the Pokémon after one of the same move broke its substitute: the move reached it after
                    // all (the hit loop's last hit is the one `gotAttacked` reads). So did one whose line was blanked
                    // after it was written (a Synchronize's `[still]`), which names nobody.
                    if (self.hits_now[side][pos].1 > 0 || self.blanked)
                        && (side, pos) != (s, p)
                        && !self.reached.iter().any(|r| r.0 == (side, pos))
                    {
                        let bit = 1 << (s * MAX_TEAM + by);
                        let spot = &mut self.spots[side][pos];
                        self.reached.push(((side, pos), spot.attacked, spot.attacked_by));
                        spot.attacked_by |= bit;
                        spot.attacked_stay[bit.trailing_zeros() as usize] = stay;
                    }
                    let now = &mut self.hits_now[side][pos];
                    (now.0, now.2) = (now.0.saturating_add(1), false);
                    let spot = &mut self.spots[side][pos];
                    (spot.attacked, spot.last_damage) = (true, damage as i32);
                    // (Ripen's halving is spent by the hit's damage, `onSourceModifyDamage`.)
                    spot.ripen_weaken = false;
                    if s != side {
                        spot.damaged_by.retain(|d| d.0 != by);
                        spot.damaged_by.push((by, s * ACTIVE + p, damage, stay));
                    }
                    self.move_hits[side][pos] = self.move_hits[side][pos].saturating_add(1);
                    if arg(3).starts_with("0 fnt") {
                        self.ko_hit[side][idx] = Some((s, by, s * ACTIVE + p, damage, self.move_hits[side][pos]));
                    }
                }
            }
            "-status" => {
                // A status the move under way gave where its line names nobody (a Synchronize's `[still]` blanked it
                // after it was written): it reached that one.
                if from.is_empty()
                    && self.blanked
                    && !self.ending
                    && let Some((user, ..)) = self.under_way
                    && user != (side, pos)
                    && !self.reached.iter().any(|r| r.0 == (side, pos))
                {
                    let (by, stay) = (self.mon_ref(user.0, user.1).idx as usize, self.spots[user.0][user.1].stay);
                    let bit = 1 << (user.0 * MAX_TEAM + by);
                    let spot = &mut self.spots[side][pos];
                    self.reached.push(((side, pos), spot.attacked, spot.attacked_by));
                    (spot.attacked, spot.attacked_by) = (true, spot.attacked_by | bit);
                    spot.attacked_stay[bit.trailing_zeros() as usize] = stay;
                }
                // A status the move under way gave: it worked there.
                if from.is_empty() && self.reached.iter().any(|r| r.0 == (side, pos)) {
                    self.boosted_at.push((side, pos));
                }
                if arg(3) == "slp" {
                    // A sleep begun under a name starts that name's count over (D96); the count before it is
                    // kept against the disguise being told.
                    let name = if side == self.side { idx } else { self.their_name(pos) };
                    let was = self.counts.sleep[side][name];
                    let here = self.spots[side][pos].sleep_here.get_or_insert((was, 0, false, name as u8));
                    (here.1, here.2) = (0, true);
                    if name != idx {
                        self.counts.sleep[side][name] = 0;
                    }
                }
                self.counts.sleep[side][idx] = 0;
                self.frozen[side][idx] = 0;
                self.counts.rest[side][idx] = arg(3) == "slp" && from == "move: Rest";
            }
            "-curestatus" => {
                (self.counts.sleep[side][idx], self.counts.rest[side][idx]) = (0, false);
                if let Some(here) = self.spots[side][pos].sleep_here.as_mut() {
                    here.1 = 0;
                }
                // Awake: the name it woke under shows no sleep (D96).
                if arg(3) == "slp" && side != self.side {
                    let name = self.their_name(pos);
                    if name != idx {
                        self.counts.sleep[side][name] = 0;
                    }
                }
            }
            "-boost" | "-unboost" => {
                if kind == "-boost" {
                    self.spots[side][pos].raised = true;
                } else {
                    self.spots[side][pos].lowered = true;
                }
                if self.stockpiled == Some((side, pos)) && from.is_empty() && arg(4) != "0" {
                    // A stage Stockpile raised, to be given back when the stockpile goes (whatever its size).
                    if let Some(v) = self.spots[side][pos].vols.iter_mut().find(|v| v.kind == VolKind::Stockpile) {
                        match arg(3) {
                            "def" => v.given.0 -= 1,
                            "spd" => v.given.1 -= 1,
                            _ => {}
                        }
                    }
                }
                // A stage a move that only changes its targets' stats moved at one of them: it worked there.
                if from.is_empty() && arg(4) != "0" && self.reached.iter().any(|r| r.0 == (side, pos)) {
                    self.boosted_at.push((side, pos));
                }
                // A status move that only raises its user's stats fails if every stage it names is at its
                // limit (`|0`: `boost` returns null); one stage that moves makes it work.
                // (Howl's too, on its user and its ally: a stage moved at either makes it work.)
                let own = self.under_way.is_some_and(|u| {
                    u.0 == (side, pos) || (u.0.0 == side && MOVES[self.last_move as usize].target == Target::Allies)
                }) && from.is_empty()
                    && MOVES.get(self.last_move as usize).is_some_and(|d| {
                        d.category == Category::Status
                            && matches!(d.target, Target::User | Target::Allies)
                            && d.boosts.is_some()
                    });
                if own && self.boosted != Some(true) {
                    // (One that also puts a condition on its user works if that condition is new: Charge,
                    // Minimize the first time.)
                    let moved = arg(4) != "0" || self.own_cond_new;
                    self.boosted = Some(moved);
                    if let Some((user, ..)) = self.under_way {
                        self.spots[user.0][user.1].result_now = Some(Some(moved));
                    }
                }
            }
            "-enditem" if !parts.contains(&"[weaken]") => {
                // A berry flung (its `-enditem` comes after the blow, as the Fling condition's `onUpdate`) was
                // eaten by whatever the blow fell on (not a substitute), if eating it does anything. (Fling keeps
                // the item it took at `PrepareHit`: one its user ate first, a Spicy Spray's burn cured, still goes,
                // and the line names none.)
                let flung = if arg(3).is_empty() && from == "move: Fling" {
                    Some(self.used_up[side][idx].0).filter(|&i| i != it::NONE)
                } else {
                    item_id(&to_id(arg(3)))
                };
                let eaten_flung = from == "move: Fling"
                    && flung.is_some_and(|i| ITEMS[i as usize].flags & IF_BERRY != 0 && crate::events::item_has_cb(i, Ev::Eat));
                if eaten_flung {
                    for s2 in 0..2 {
                        for p2 in 0..ACTIVE {
                            let (on, _, last_sub) = self.hits_now[s2][p2];
                            if on > 0 && !last_sub {
                                let t = self.mon_ref(s2, p2).idx as usize;
                                self.ate[s2][t] = true;
                                // (Eaten, `EatItem`: a Cud Chew there keeps it to eat again.)
                                if self.ability_at(s2, p2) == ab::CUDCHEW {
                                    self.spots[s2][p2].cud = flung;
                                }
                                // (And a Ripen there notes whether it was a resist berry, `onEatItem`.)
                                if self.ability_at(s2, p2) == ab::RIPEN {
                                    self.spots[s2][p2].ripen_weaken = flung.is_some_and(crate::items::is_resist_berry);
                                }
                            }
                        }
                    }
                }
                if let Some(item) = item_id(&to_id(arg(3))) {
                    // Taken, swapped away or burnt up by another Pokémon's move or ability, it was not used
                    // up (Fling uses it): `Pokemon#takeItem` sets no `lastItem`. Nor is an Air Balloon that
                    // pops (its holder's item is simply gone).
                    let taken = from.starts_with("stealeat")
                        || matches!(
                            from,
                            "move: Knock Off"
                                | "move: Thief"
                                | "move: Covet"
                                | "move: Incinerate"
                                | "move: Corrosive Gas"
                                | "move: Trick"
                                | "move: Switcheroo"
                                | "ability: Pickpocket"
                        )
                        || (item == it::AIRBALLOON && from.is_empty());
                    let before = self.used_up[side][idx];
                    let again = std::mem::take(&mut self.spots[side][pos].cud_again);
                    // A berry it eats (or flings: Fling's `onPrepareHit` lets Cud Chew have it) Cud Chew keeps
                    // to eat again at a turn's end; not one eaten again, nor one Bug Bite or Pluck stole.
                    let chewed = ITEMS[item as usize].flags & IF_BERRY != 0
                        && (parts.contains(&"[eat]") || from == "move: Fling")
                        && !again
                        && self.ability_at(side, pos) == ab::CUDCHEW;
                    if chewed {
                        self.spots[side][pos].cud = Some(item);
                    }
                    if parts.contains(&"[eat]") && again {
                        // Eaten again: no last item, no item used this turn (`onResidual`'s own `-enditem`).
                        self.ate[side][idx] = true;
                    } else if parts.contains(&"[eat]") {
                        self.used_up[side][idx] = (item, true);
                        self.ate[side][idx] = true;
                    } else if !taken {
                        self.used_up[side][idx] = (item, false);
                    }
                    if self.used_up[side][idx] != before {
                        let spot = &mut self.spots[side][pos];
                        spot.used = Some((self.used_up[side][idx], spot.used.map_or(before, |u| u.1)));
                    }
                    // A berry stolen and eaten (Bug Bite, Pluck): the thief has eaten a berry, if it is one
                    // that is eaten (`if (item.onEat) source.ateBerry = true`).
                    let eaten = self.battle.as_ref().is_some_and(|b| b.has_cb(Eff::Item(item), Ev::Eat));
                    if from.starts_with("stealeat")
                        && eaten
                        && let Some((s2, p2)) =
                            parts.iter().find_map(|p| p.strip_prefix("[of] ")).and_then(|p| self.place_of(p.trim()))
                    {
                        let thief = self.mon_ref(s2, p2).idx as usize;
                        self.ate[s2][thief] = true;
                    }
                    if !taken && !again {
                        self.spots[side][pos].used_item = true;
                    }
                }
            }
            "-ability" if from.is_empty() && to_id(arg(3)) == "unnerve" => self.spots[side][pos].unnerved = true,
            "-ability" if arg(4) == "boost" && absorbs(arg(3)) && self.reached.iter().any(|r| r.0 == (side, pos)) => {
                // Taken in by its ability (Lightning Rod, Storm Drain, Sap Sipper...: `onTryHit` returns
                // null), the move never reached it and fails there.
                missed(self, (side, pos), Some(false));
            }
            "-heal"
                if from.strip_prefix("ability: ").is_some_and(absorbs)
                    && self.reached.iter().any(|r| r.0 == (side, pos))
                    && parts.iter().find_map(|p| p.strip_prefix("[of] ")).and_then(|p| self.place_of(p.trim()))
                        == self.under_way.map(|u| u.0) =>
            {
                // Water Absorb, Volt Absorb, Dry Skin, Earth Eater, taking in the move under way (the line
                // names its user; Dry Skin's heal in the rain names nobody): as above.
                missed(self, (side, pos), Some(false));
            }
            // (A `[silent]` one is an end-of-turn effect's, Leech Seed's drain.)
            "-heal" if from.is_empty() && !self.ending && !parts.contains(&"[silent]") => {
                // The move's own heal at one of its targets (Life Dew, Heal Pulse): it worked there.
                if let Some((user, hit, _, _)) = self.under_way.as_mut()
                    && (*user == (side, pos) || self.reached.iter().any(|r| r.0 == (side, pos)))
                {
                    *hit = true;
                    self.boosted_at.push((side, pos));
                    let user = *user;
                    self.spots[user.0][user.1].result_now = Some(Some(true));
                }
            }
            "-anim" => {
                // Where a move went when its line names nobody (a charging move let off at once in sun or
                // rain: `[still]` blanks the target, `-anim` names it) or a second place (Dragon Darts'
                // second dart): reached there as at any named target. A charging move that goes off is
                // under way after all.
                if let Some((user, ..)) = self.under_way
                    && user == (side, pos)
                    && let Some(t) = self.place_of(arg(4))
                    && t != user
                    && self.present(t.0, t.1)
                    && !self.reached.iter().any(|r| r.0 == t)
                {
                    let bit = 1 << (side * MAX_TEAM + idx);
                    let stay = self.spots[side][pos].stay;
                    let spot = &mut self.spots[t.0][t.1];
                    self.reached.push((t, spot.attacked, spot.attacked_by));
                    spot.attacked_by |= bit;
                    spot.attacked_stay[bit.trailing_zeros() as usize] = stay;
                    spot.attacked = true;
                }
                if self.under_way.is_some_and(|u| u.0 == (side, pos))
                    && self.spots[side][pos].result_now == Some(None)
                {
                    self.spots[side][pos].result_now = Some(Some(true));
                }
            }
            "-start"
                if arg(3) == "typechange"
                    && (from.ends_with("Protean") || from.ends_with("Libero")) =>
            {
                self.spots[side][pos].protean = true;
            }
            // A status the move would have given, kept off at this target (Safeguard, Misty or Electric
            // Terrain), or a veil keeping the move off it: the move fails there.
            // (A status move only, and not where a stage it changed there has already made it work, Swagger's
            // confusion kept off; unless what was kept off is the move's own status, Toxic Thread's poison,
            // which fails it there whatever else it did: `runMoveEffects` gives up at `move.status`.)
            "-activate"
                if matches!(arg(3), "move: Safeguard" | "move: Misty Terrain" | "move: Electric Terrain")
                    && self.reached.iter().any(|r| r.0 == (side, pos))
                    && status_kept_off(self.last_move, self.boosted_at.contains(&(side, pos))) =>
            {
                missed(self, (side, pos), Some(false));
            }
            // (Not Parting Shot, whose stages are its own `onHit`'s and which does not fail with them: the block keeps
            // the stages off, and the move has still reached its target.)
            "-block"
                if arg(3).starts_with("ability: ")
                    && status_kept_off(self.last_move, self.boosted_at.contains(&(side, pos)))
                    && MOVES.get(self.last_move as usize).is_none_or(|d| d.id != "partingshot")
                    && (self.reached.iter().any(|r| r.0 == (side, pos))
                        || self.under_way.is_some_and(|u| u.0 == (side, pos))) =>
            {
                // (On its user, a move on itself: Sweet Veil against Rest.)
                if self.under_way.is_some_and(|u| u.0 == (side, pos)) {
                    self.spots[side][pos].result_now = Some(Some(false));
                } else {
                    missed(self, (side, pos), Some(false));
                }
            }
            // An ability swapped in comes with a state of its own: Protean has not gone off, and Supreme Overlord
            // counts its side's fallen as it starts.
            "-activate" if arg(3).ends_with("Skill Swap") => {
                (self.spots[side][pos].protean, self.spots[side][pos].fallen) = (false, self.faints[side]);
                if let Some((s, p)) = parts.iter().find_map(|p| p.strip_prefix("[of] ")).and_then(|p| self.place_of(p.trim())) {
                    (self.spots[s][p].protean, self.spots[s][p].fallen) = (false, self.faints[s]);
                }
            }
            "-activate" if arg(3) == "ability: Emergency Exit" => self.spots[side][pos].exiting = true,
            "-activate" if arg(3) == "ability: Cud Chew" => {
                let spot = &mut self.spots[side][pos];
                (spot.cud, spot.cud_again) = (None, true);
            }
            "-activate" => {
                let name = effect_id(arg(3));
                if name == "confusion" {
                    let spot = &mut self.spots[side][pos];
                    spot.confused = spot.confused.saturating_add(1);
                } else if name == "disguise" && mover.is_some() {
                    // The blow falls on the Disguise: the `-damage` line that follows is the hit, of nothing.
                    // And the Disguise has broken during this stay.
                    self.zero_hit = Some((side, pos));
                    self.spots[side][pos].busted = true;
                } else if name == "substitute" && parts.contains(&"[damage]") {
                    // A hit on a substitute is not a hit on the Pokémon (`HIT_SUBSTITUTE` takes it out of
                    // the targets): it was not reached, and the move worked. (Its damage was worked out all the
                    // same, which spends Ripen's halving.)
                    self.spots[side][pos].ripen_weaken = false;
                    if mover.is_some() {
                        unreach(self, (side, pos));
                        let now = &mut self.hits_now[side][pos];
                        (now.1, now.2) = (now.1.saturating_add(1), true);
                    }
                } else if matches!(
                    name.as_str(),
                    "protect" | "detect" | "maxguard" | "wideguard" | "quickguard" | "craftyshield" | "matblock"
                ) || arg(3) == "move: Protect"
                {
                    missed(self, (side, pos), None);
                } else if (name == "psychicterrain" || arg(3) == "ability: Telepathy")
                    && self.under_way.is_some_and(|u| u.0 != (side, pos))
                {
                    // Psychic Terrain stops a priority move, Telepathy an ally's (`onTryHit` null): failed there.
                    missed(self, (side, pos), Some(false));
                } else if arg(3).starts_with("move: ")
                    && move_id(&name).is_some_and(|m| MOVES[m as usize].volatile == Some(VolKind::Partiallytrapped))
                {
                    self.spots[side][pos].bound = 0;
                    let of = parts.iter().find_map(|p| p.strip_prefix("[of] ")).and_then(|p| self.place_of(p.trim()));
                    if let Some((s, p)) = of.or(mover) {
                        let by = (s, self.mon_ref(s, p).idx as usize, s * ACTIVE + p);
                        // Its damage each turn: an eighth, or a sixth from a binder whose Binding Band is at work.
                        let band = self.item_at_work(s, p) == Some(it::BINDINGBAND);
                        let vols = &mut self.spots[side][pos].vols;
                        if let Some(v) = vols.iter_mut().find(|v| v.kind == VolKind::Partiallytrapped) {
                            (v.by, v.data) = (Some(by), if band { 6 } else { 8 });
                        }
                    }
                }
            }
            "-start" if arg(3) == "ability: Flash Fire" && self.reached.iter().any(|r| r.0 == (side, pos)) => {
                // Taken in by Flash Fire: as an absorbing ability above.
                missed(self, (side, pos), Some(false));
            }
            // A future move put up: nobody is hit now (its `onTry` returns null), only when it lands.
            "-start" if matches!(arg(3), "move: Future Sight" | "move: Doom Desire") => {
                let targets: Vec<(usize, usize)> = self.reached.iter().map(|r| r.0).collect();
                for t in targets {
                    unreach(self, t);
                }
            }
            "-start" => {
                let name = effect_id(arg(3));
                if name == "powertrick" {
                    self.trick_swap(side, pos);
                }
                let Some(vol) = VolKind::named(&name) else { return };
                // A move on its user that puts up its own condition there has worked (Charge again: its
                // `onRestart` says so too), whatever its stages did.
                if self.under_way.is_some_and(|u| u.0 == (side, pos))
                    && MOVES.get(self.last_move as usize).is_some_and(|d| d.target == Target::User && d.volatile == Some(vol))
                {
                    self.boosted = Some(true);
                    self.spots[side][pos].result_now = Some(Some(true));
                }
                if vol == VolKind::Confusion {
                    if arg(4) == "[fatigue]" || parts.contains(&"[fatigue]") {
                        self.rampage[side][idx] = NO_MOVE;
                    }
                    self.spots[side][pos].confused = 0;
                }
                let of = parts.iter().find_map(|p| p.strip_prefix("[of] ")).and_then(|p| self.place_of(p.trim()));
                let source = of.or(mover).map(|(s, p)| (s, self.mon_ref(s, p).idx as usize, s * ACTIVE + p));
                let spot = &self.spots[side][pos];
                let (last, begun, turns) = (spot.last_move, spot.begun, spot.active_turns);
                let moving = self.mover.as_ref().is_some_and(|m| (m.side, m.pos) == (side, pos));
                let base = vol.data().duration;
                let left = match vol {
                    VolKind::Taunt => base + (turns > 0 && begun > 0) as u8,
                    VolKind::Encore => base + (begun > 0) as u8,
                    VolKind::Disable => base.saturating_sub((begun == 0 || moving) as u8),
                    // (Psychic Noise's is put up without a word of where it came from: the move under way says.)
                    VolKind::Healblock
                        if from.contains("Psychic Noise")
                            || arg(3).contains("Psychic Noise")
                            || self.last_move == mv::PSYCHICNOISE =>
                    {
                        2
                    }
                    _ => base,
                };
                let about = match vol {
                    VolKind::Encore => last,
                    VolKind::Disable => move_id(&to_id(arg(4))).unwrap_or(NO_MOVE),
                    _ => NO_MOVE,
                };
                if let Some(v) = self.spots[side][pos].vols.iter_mut().find(|v| v.kind == vol) {
                    if v.left == 0 {
                        v.left = left;
                    }
                    if about != NO_MOVE {
                        v.about = about;
                    }
                    if matches!(
                        vol,
                        VolKind::Leechseed
                            | VolKind::Yawn
                            | VolKind::Attract
                            | VolKind::Imprison
                            | VolKind::Octolock
                            | VolKind::Syrupbomb
                            | VolKind::Substitute
                    ) {
                        // (Imprison and a substitute are their holder's own: a substitute keeps its maker
                        // when Shed Tail or Baton Pass hands it on, and is as strong as that one's HP.)
                        v.by = if matches!(vol, VolKind::Imprison | VolKind::Substitute) {
                            Some((side, idx, side * ACTIVE + pos))
                        } else {
                            source
                        };
                    }
                }
                // Dragon Cheer does more for a Dragon: whether it was one when cheered (the condition's `data`).
                if vol == VolKind::Dragoncheer {
                    // (Copied by Psych Up, `[silent]`: as it was on the Pokémon it came from, `hasDragonType`.)
                    let copied = (self.last_move == mv::PSYCHUP && parts.contains(&"[silent]"))
                        .then(|| self.mover.as_ref().and_then(|m| m.target))
                        .flatten()
                        .and_then(|(s, p)| self.spots[s][p].vols.iter().find(|v| v.kind == vol).map(|v| v.data != 0));
                    let dragon = copied.unwrap_or_else(|| {
                        self.seen_base(side, pos).contains(&Type::Dragon) || self.spots[side][pos].added == Type::Dragon
                    });
                    if let Some(v) = self.spots[side][pos].vols.iter_mut().find(|v| v.kind == vol) {
                        v.data = dragon as u16;
                    }
                }
            }
            "-mustrecharge" => {
                if let Some(v) = self.spots[side][pos].vols.iter_mut().find(|v| v.kind == VolKind::Mustrecharge) {
                    v.left = VolKind::Mustrecharge.data().duration.max(2);
                }
            }
            "-prepare" => {
                if let Some(m) = move_id(&to_id(arg(3)))
                    && let Some(v) = self.spots[side][pos].vols.iter_mut().find(|v| v.kind == VolKind::Twoturnmove)
                {
                    v.about = m;
                }
                // A charging turn comes to nothing, without failing.
                self.spots[side][pos].result_now = Some(None);
            }
            "-end" => {
                let name = effect_id(arg(3));
                if name == "powertrick" {
                    self.trick_swap(side, pos);
                }
                if matches!(name.as_str(), "futuresight" | "doomdesire") {
                    // A future move lands: what follows is its hit, not the move last under way's.
                    (self.under_way, self.reached) = (None, Vec::new());
                }
                if name == "substitute" && mover.is_some() && !parts.contains(&"[silent]") {
                    // The blow broke the substitute: still not a hit on the Pokémon.
                    unreach(self, (side, pos));
                    let now = &mut self.hits_now[side][pos];
                    (now.1, now.2) = (now.1.saturating_add(1), true);
                }
                if name == "confusion" {
                    self.spots[side][pos].confused = 0;
                }
            }
            "switch" | "drag" => {
                // A new stay: what was kept of the last one at this position went with it (the
                // counts kept with the stay went with the `Spot`, `Follower::enter`).
                self.rampage[side][idx] = NO_MOVE;
            }
            _ => {}
        }
    }

    /// Whether the Pokémon at a position is a Ghost now, as this side sees it.
    fn ghost_now(&self, side: usize, pos: usize) -> bool {
        self.seen_base(side, pos).contains(&Type::Ghost) || self.spots[side][pos].added == Type::Ghost
    }

    /// The ability the Pokémon at a position is taken to have: one of its own as it knows it,
    /// one of the other side's as shown, else as its sheet says.
    /// An exact HP of the other side's, where a line lets this side work one out (D77): both Pokémon of
    /// a Pain Split end at the same HP (each kept to its max HP, Showdown `painsplit`), and an Endeavor
    /// leaves its target at its user's HP; with one of them this side's own, whose HP it is told to
    /// the point, the other's is known too (while its bar is short of full, which would only bound
    /// its max HP). Any other line that moves that HP, or a switch, ends what is known of it now.
    fn hp_told(&mut self, kind: &str, side: usize, pos: usize, parts: &[&str], from: &str) {
        let foe = 1 - self.side;
        let cond = parts.get(3).copied().unwrap_or("");
        if kind == "move" {
            // (An Endeavor's hit is the line after its own; any other move ends the wait for one.)
            self.endeavor = (move_id(&to_id(cond)) == Some(mv::ENDEAVOR))
                .then(|| parts.get(4).and_then(|t| self.place_of(t)).map(|t| ((side, pos), t)))
                .flatten();
            return;
        }
        if side == foe {
            if matches!(kind, "switch" | "drag" | "replace") {
                // (Who left may have healed on the way out, Regenerator's: nothing is known of the side now.)
                self.hp_now.clear();
            } else if matches!(kind, "-damage" | "-heal" | "-sethp" | "faint")
                && let Some(entry) = self.reader.sides[foe].active[pos].as_ref().map(|l| l.entry)
            {
                self.hp_now.retain(|n| n.0 != entry);
            }
        }
        let exact = |c: &str| -> Option<(u16, u16)> {
            let (hp, max) = c.split(' ').next()?.split_once('/')?;
            Some((hp.parse().ok()?, max.trim_end_matches(|ch: char| ch.is_ascii_alphabetic()).parse().ok()?))
        };
        let told = |c: &str| crate::observer::condition(c).ok().map(|(pct, bar, _, _)| (pct, bar));
        let mut learnt = None;
        if kind == "-sethp" && from == "move: Pain Split" {
            match self.pain_split.take() {
                None => self.pain_split = Some((side, pos, cond.to_string())),
                Some((s, p, first)) => {
                    let pair = [(s, p, first), (side, pos, cond.to_string())];
                    if let (Some(own), Some(theirs)) =
                        (pair.iter().find(|x| x.0 == self.side), pair.iter().find(|x| x.0 == foe))
                        && let Some((hp, max)) = exact(&own.2)
                        && hp < max
                    {
                        learnt = Some((theirs.1, hp, told(&theirs.2)));
                    }
                }
            }
        } else if kind == "-damage"
            && from.is_empty()
            && let Some((m, aimed)) = self.endeavor.take()
            && aimed == (side, pos)
        {
            if side == foe && m.0 == self.side {
                // This side's Endeavor: its target is left at the user's HP.
                learnt = self.own_hp[m.1].map(|hp| (pos, hp, told(cond)));
            } else if side == self.side && m.0 == foe {
                // The other side's on this side's: the user's HP is what this one is left at.
                let shown = self.reader.live_record(foe, m.1).map(|r| (r.hp, r.bar));
                learnt = exact(cond).map(|(hp, _)| (m.1, hp, shown));
            }
        }
        if let Some((at, hp, Some((pct, bar)))) = learnt
            && pct > 0
            && pct < 100
            && hp > 0
            && let Some(entry) = self.reader.sides[foe].active[at].as_ref().map(|l| l.entry)
        {
            self.hp_seen.push((entry, hp, pct, bar, self.spots[foe][at].stay));
            self.hp_now.retain(|n| n.0 != entry);
            self.hp_now.push((entry, hp));
        }
    }

    /// Whether a confusion coming to the Pokémon at a position now, as a rampage's end brings it, would
    /// be taken without a word: it is already confused, has Own Tempo, or stands on the ground in
    /// Misty Terrain (its grounding as this side can work it out; where it cannot, it may).
    fn confusion_unsaid(&self, side: usize, pos: usize) -> bool {
        let spot = &self.spots[side][pos];
        if spot.has(VolKind::Confusion)
            || (self.ability_at(side, pos) == ab::OWNTEMPO && !spot.has(VolKind::Gastroacid))
        {
            return true;
        }
        self.field.terrain == Terrain::Mistyterrain
            && self.rebuild().map_or(true, |mut b| {
                let r = b.active(side, pos);
                b.is_grounded(r, false)
            })
    }

    fn ability_at(&self, side: usize, pos: usize) -> u16 {
        if side == self.side {
            return self.own_now(pos).and_then(|now| now.abilities.first().copied()).unwrap_or(ab::NOABILITY);
        }
        // (As shown, or as worked out after an allied Skill Swap, or its sheet's, or its Mega's.)
        self.their_ability_now(pos).unwrap_or(ab::NOABILITY)
    }

    /// Who set up what a line says has come about: the Pokémon after `[of]`, else the one moving.
    fn setter(&self, parts: &[&str]) -> Option<(usize, usize)> {
        let of = parts.iter().find_map(|p| p.strip_prefix("[of] ")).and_then(|p| self.place_of(p.trim()));
        of.or(self.mover.as_ref().map(|m| (m.side, m.pos)))
    }

    /// Whether the item of the Pokémon at a position does nothing now (Showdown's `ignoringItem`, which `hasItem`
    /// minds): under Magic Room, or with Klutz (as far as this side knows it).
    fn item_ignored(&self, side: usize, pos: usize) -> bool {
        self.field.pseudo.iter().any(|p| p.0 == Pseudo::Magicroom) || self.ability_at(side, pos) == ab::KLUTZ
    }

    /// The item the Pokémon at a position is taken to hold: one of its own as the request and the
    /// log since have it; one of the other side's as shown, or else as its sheet says.
    fn item_at(&self, side: usize, pos: usize) -> u16 {
        if side == self.side {
            if let Some(now) = self.own_now(pos) {
                return now.item;
            }
            // (Before the first request, as the battle begins: the set's own, by the name it goes by.)
            let us = &self.reader.sides[side];
            let name = us.active[pos].as_ref().and_then(|l| us.team.get(l.entry)).map(|e| e.name.as_str());
            let set = (0..self.team.len()).find(|&j| {
                let nick = self.nicknames.get(j).filter(|n| !n.is_empty());
                let species = SPECIES[self.team[j].species as usize].name;
                name.is_some_and(|n| nick.is_some_and(|k| k == n) || to_id(n) == to_id(species))
            });
            return set.map_or(it::NONE, |j| self.team[j].item);
        }
        let Some(live) = self.reader.sides[side].active[pos].as_ref().filter(|l| !l.gone) else {
            return it::NONE;
        };
        match live.rec.item {
            ItemShown::Holds(item) => item,
            ItemShown::Lost(_) => it::NONE,
            ItemShown::Unknown => {
                self.reader.sides[side].roster.get(live.rec.listed as usize).map_or(it::NONE, |l| l.item)
            }
        }
    }

    // ------------------------------------------------ the game, as this side knows it

    /// This player's side of the battle as a [`GameState`], for a search from
    /// what this player knows: a search over belief worlds, which draw what it
    /// cannot (the other side's stat points, the Pokémon it has not shown, the
    /// lengths the game keeps to itself, the dice). The battle the observation
    /// reads ([`Follower::battle`]) is completed from the open sheet, the log
    /// and the request: the other side's Pokémon take their sheets' moves,
    /// items, abilities and natures and stat points of 0 (stats to match, HP
    /// the least that shows their percentage and bar); those not yet seen are
    /// the sheet's entries not yet seen, in sheet order; conditions get the
    /// move they are about; the counts of hidden-length effects are the log's.
    ///
    /// The battle goes out without the simulator's bookkeeping
    /// ([`BattleState::without_bookkeeping`]), which the engine works out again
    /// from the position, as it does for every belief world: so where the
    /// request offers a choice Showdown would refuse once tried (a switch out of
    /// a Shadow Tag not yet shown), the state does not.
    ///
    /// Refused, with the reason: before the battle's first request and after
    /// its end; closed sheets (nothing to complete the other side from); a
    /// Pokémon of the other side that may be in disguise (who it is is not
    /// known); a transformed Pokémon; a switch in the middle of a turn (the
    /// rest of the turn is not here to play out); waiting while the other side
    /// chooses (nothing to decide, and who it replaces is not said).
    pub fn game_state(&self) -> Result<GameState, String> {
        let rosters = self.rosters();
        let sets = |team: &[PokemonSet]| team.iter().map(ShowdownSet::from_set).collect::<Vec<_>>();
        let open = self.open();
        let req = self.req.as_ref().ok_or("no request yet")?;
        if !open {
            return err("closed team sheets");
        }
        if rosters[1 - self.side].len() != ROSTER {
            return err("the other side's team sheet does not list six");
        }
        let teams = [sets(&rosters[0]), sets(&rosters[1])];
        if req.asked == Asked::Preview {
            return Ok(GameState { rosters: teams, open, ..GameState::default() });
        }
        if self.ended {
            return err("battle over");
        }
        if req.asked == Asked::Wait {
            // (The other side is choosing: this side has nothing to decide, and its request does not
            // say who on the other side is being replaced, as after an Emergency Exit at the turn's end.)
            return err("waiting for the other side");
        }
        if req.asked == Asked::Switch && !self.upkept {
            return err("a switch in the middle of a turn");
        }
        let mut b = self.battle.ok_or("no battle rebuilt")?;
        self.complete(&mut b, &rosters[1 - self.side])?;
        let foe = 1 - self.side;
        let hp_seen = self
            .hp_seen
            .iter()
            .map(|&(entry, hp, pct, bar, _)| crate::env::HpSeen {
                side: foe as u8,
                team_index: entry as u8,
                hp,
                pct,
                bar: format!("{bar:?}").to_lowercase(),
                now: self.hp_now.contains(&(entry, hp))
                    && (0..ACTIVE).any(|p| {
                        self.present(foe, p)
                            && self.reader.sides[foe].active[p].as_ref().is_some_and(|l| l.entry == entry)
                    }),
            })
            .collect();
        // (The PP a doubtful move has left as the export counts it, read off the battle completed.)
        let mut untold = self.untold(req.asked == Asked::Move);
        for u in &mut untold {
            if let Untold::PressurePp { side, team_index, move_id, pp, .. } = u {
                let m = b.mon(MonRef { side: *side, idx: *team_index });
                *pp = m.moves[..m.n_moves as usize]
                    .iter()
                    .find(|s| MOVES[s.id as usize].id == move_id.as_str())
                    .map_or(0, |s| s.pp);
            }
        }
        Ok(GameState {
            untold,
            hp_seen,
            rosters: teams,
            open,
            // Without the bookkeeping the engine works out again from the position (who is trapped,
            // which moves are barred, what a Pokémon is locked into): where Showdown's request only
            // says a Pokémon may be trapped by something not yet shown (a Mega's Shadow Tag), the
            // engine knows from the sheet, and a search should not try what Showdown would refuse.
            battle: Some(b.to_state().without_bookkeeping()),
            timers: Some(self.timers.counts(b.turn)),
            speeds: self.speeds,
            counts: Some(self.counts_now(&b)),
        })
    }

    /// What this side cannot tell at this decision (D89, D94), for [`Follower::game_state`]: each
    /// [`Untold`] by side and the battle's team index. `moving`: at a move request, whose moves say
    /// whether its own Pokémon is still locked into a rampage.
    fn untold(&self, moving: bool) -> Vec<Untold> {
        let mut out = Vec::new();
        for side in 0..2 {
            for idx in 0..MAX_TEAM {
                // (While its holder stays: one that has left the field is let go of its lock.)
                let here = (0..ACTIVE).any(|p| self.present(side, p) && self.mon_ref(side, p).idx as usize == idx);
                if self.rampage[side][idx] != NO_MOVE
                    && self.rampage_turns[side][idx] == 2
                    && self.rampage_silent[side][idx]
                    && here
                    && !(side == self.side && moving)
                {
                    out.push(Untold::Rampage { side: side as u8, team_index: idx as u8 });
                }
            }
        }
        for &(entry, mv, payments) in &self.pressure_doubt {
            out.push(Untold::PressurePp {
                side: (1 - self.side) as u8,
                team_index: entry as u8,
                move_id: MOVES[mv as usize].id.to_string(),
                payments,
                pp: 0,
            });
        }
        for (s, pos) in (0..2).flat_map(|s| (0..ACTIVE).map(move |pos| (s, pos))) {
            if self.spots[s][pos].stats_shared && self.present(s, pos) {
                let idx = self.mon_ref(s, pos).idx;
                out.push(Untold::StatsShared { side: s as u8, team_index: idx });
            }
        }
        for &((s, idx), (s2, idx2), item) in &self.taken_doubt {
            // (While both still have it as their last item.)
            if self.used_up[s][idx].0 == item && self.used_up[s2][idx2].0 == item {
                out.push(Untold::ItemTaken {
                    side: s as u8,
                    team_index: idx as u8,
                    other_side: s2 as u8,
                    other_index: idx2 as u8,
                });
            }
        }
        // (Either side's: its own too where a Copycat or a Sleep Talk aimed the move at random.)
        for (s, pos) in (0..2).flat_map(|s| (0..ACTIVE).map(move |pos| (s, pos))).filter(|&(s, pos)| self.present(s, pos)) {
            let idx = self.mon_ref(s, pos).idx;
            for (k, this_turn) in [(0, true), (1, false)] {
                if self.spots[s][pos].result_doubt[k] {
                    out.push(Untold::MoveResult { side: s as u8, team_index: idx, this_turn });
                }
            }
        }
        for pos in (0..ACTIVE).filter(|&pos| {
            let spot = &self.spots[self.side][pos];
            self.present(self.side, pos) && spot.charge_aim == Some(None) && spot.vols.iter().any(|v| CHARGING.contains(&v.kind))
        }) {
            // (Showdown drew one of the two standing on the other side: its places are 1 and 2.)
            let idx = self.own_at[pos];
            out.push(Untold::ChargeTarget { side: self.side as u8, team_index: idx as u8, locs: vec![1, 2] });
        }
        // (While it stands: a fainted one's is read by nothing.)
        let standing = |idx: usize| (0..ACTIVE).any(|pos| self.own_at[pos] == idx && self.present(self.side, pos));
        for idx in (0..MAX_TEAM).filter(|&idx| self.encore_doubt[idx] && standing(idx)) {
            // (Showdown drew one of the two standing on the other side: its places are 1 and 2.)
            out.push(Untold::EncoreTarget { side: self.side as u8, team_index: idx as u8, locs: vec![1, 2] });
        }
        out
    }

    /// Completes the rebuilt battle for [`Follower::game_state`]. `sheets`: the other side's six.
    fn complete(&self, b: &mut Battle, sheets: &[PokemonSet]) -> Result<(), String> {
        let (me, opp) = (self.side, 1 - self.side);
        let b_request_switch = b.request == Request::Switch;
        let them = &self.reader.sides[opp];
        for s in 0..2 {
            for pos in 0..ACTIVE {
                let Some(live) = self.reader.sides[s].active[pos].as_ref().filter(|l| !l.gone) else { continue };
                if live.transformed {
                    return err("Transform");
                }
                if s == opp && (self.their_index(pos) != live.entry || live.rec.suspect) {
                    return err("Illusion");
                }
            }
        }
        // Which sheet entry each Pokémon seen is, and with that which were brought.
        let seen = them.team.len().min(MAX_TEAM);
        let n = b.sides[opp].n as usize;
        let record = |idx: usize| -> Shown {
            let live = (0..ACTIVE).find_map(|pos| them.active[pos].as_ref().filter(|l| !l.gone && l.entry == idx));
            live.map_or(them.team[idx].rec, |l| l.rec)
        };
        let mut entry_of = [usize::MAX; MAX_TEAM];
        for (idx, entry) in entry_of.iter_mut().enumerate().take(seen) {
            let rec = record(idx);
            if rec.tainted {
                return err("Illusion");
            }
            let j = rec.listed as usize;
            if rec.listed == NOT_LISTED || j >= sheets.len() || b.sides[opp].roster[j].brought != NOT_LISTED {
                return err("a Pokémon seen is not on its sheet, or on it twice");
            }
            b.sides[opp].roster[j].brought = idx as u8;
            *entry = j;
        }
        // Those not yet seen: the entries not yet seen, in sheet order, as they were brought.
        let free: Vec<usize> = (0..sheets.len()).filter(|&j| b.sides[opp].roster[j].brought == NOT_LISTED).collect();
        let mut free = free.into_iter();
        for (idx, entry) in entry_of.iter_mut().enumerate().take(n).skip(seen) {
            let j = free.next().ok_or("more Pokémon brought than the sheet has")?;
            let position = b.sides[opp].team[idx].position;
            let mut m = b.new_mon(&sheets[j], position as usize).map_err(|e| e.to_string())?;
            m.position = position;
            m.hp = m.stats[0];
            if self.megaed[opp] {
                m.can_mega = NO_SPECIES;
            }
            b.sides[opp].team[idx] = m;
            b.sides[opp].shown[idx] = NOTHING_SHOWN;
            b.sides[opp].roster[j].brought = idx as u8;
            *entry = j;
        }
        // Those seen: their sheets, with what the log has shown changed since.
        for idx in 0..seen {
            let set = &sheets[entry_of[idx]];
            let fresh = b.new_mon(set, 0).map_err(|e| e.to_string())?;
            let rec = record(idx);
            let m = &mut b.sides[opp].team[idx];
            m.set_species = set.species;
            // (A forme taken on the field, Stance Change's, is not the one it goes back to; a Mega
            // and a broken Disguise are.)
            let lasting =
                is_mega(m.species) || matches!(SPECIES[m.species as usize].id, "mimikyubusted" | "palafinhero");
            m.base_species = if lasting { m.species } else { set.species };
            (m.nature, m.stat_points) = (set.nature, [0; 6]);
            (m.moves, m.n_moves) = (fresh.moves, fresh.n_moves);
            if let Some(spent) = self.their_pp.get(idx) {
                for slot in m.moves[..m.n_moves as usize].iter_mut() {
                    if let Some(&(_, used)) = spent.iter().find(|s| s.0 == slot.id) {
                        slot.pp = slot.maxpp.saturating_sub(used);
                    }
                }
            }
            // Stat points of 0, but for HP: at exactly 20% or 50% the bar's colour (red at a fifth,
            // yellow at a half) needs a max HP that can show it, so the least HP points whose max HP
            // shows the percentage and bar recorded are taken (S3.4).
            let (points, hp) = if m.fainted {
                ([0; 6], 0)
            } else {
                (0..=MAX_HP_POINTS)
                    .find_map(|pts| {
                        let points = [pts, 0, 0, 0, 0, 0];
                        let max = calc_stats(m.species, set.nature, points)[0];
                        (1..=max).find(|&h| hp_shown(h, max) == (rec.hp, rec.bar)).map(|h| (points, h))
                    })
                    .unwrap_or_else(|| {
                        let max = calc_stats(m.species, set.nature, [0; 6])[0];
                        ([0; 6], (1..=max).find(|&h| hp_shown(h, max).0 == rec.hp).unwrap_or(max))
                    })
            };
            m.stat_points = points;
            m.stats = calc_stats(m.species, set.nature, points);
            m.hp = hp;
            // Its ability: as shown, or its Mega's, or its sheet's.
            let own = if is_mega(m.species) { SPECIES[m.species as usize].ability0 } else { set.ability };
            // (Since a Skill Swap with its partner, which names neither ability: the one worked out then.)
            let swapped = (m.is_active && (m.position as usize) < ACTIVE)
                .then(|| self.spots[opp][m.position as usize].their_ability)
                .flatten();
            m.ability = if rec.ability != UNKNOWN { rec.ability } else { swapped.unwrap_or(own) };
            m.base_ability = if rec.base_ability != UNKNOWN { rec.base_ability } else { own };
            m.item = match rec.item {
                ItemShown::Holds(item) => item,
                ItemShown::Lost(_) => it::NONE,
                ItemShown::Unknown => set.item,
            };
            // (What it has used up is kept as the log tells it, below: an item knocked off is not.)
            let stone = ITEMS[m.item as usize].mega.iter().find(|&&(from, _)| from == set.species);
            m.can_mega = match stone {
                Some(&(_, mega)) if !self.megaed[opp] && !is_mega(m.species) => mega,
                _ => NO_SPECIES,
            };
        }
        // The field: how long the weather, the terrain and screens from Light Clay have left.
        if b.field.weather.kind != Weather::None {
            b.field.weather.duration = self.field.weather_len.saturating_sub(self.field.weather_for.min(255) as u8);
        }
        if b.field.terrain.kind != Terrain::None {
            b.field.terrain.duration = self.field.terrain_len.saturating_sub(self.field.terrain_for.min(255) as u8);
        }
        for side in 0..2 {
            let track = &self.field.sides[side];
            for c in b.sides[side].conds.as_mut_slice() {
                // (Layers are kept only by the conditions that have them.)
                if !matches!(c.kind, SideCond::Spikes | SideCond::Toxicspikes) {
                    c.data = 0;
                }
            }
            for &kind in &track.lasting {
                let since = track.conds.iter().find(|c| c.0 == kind).map_or(0, |c| c.2);
                if let Some(c) = b.sides[side].conds.get_mut(kind) {
                    c.duration = 8u16.saturating_sub(since).min(255) as u8;
                }
            }
            b.sides[side].fainted_last_turn = self.fainted_last[side];
            b.sides[side].fainted_this_turn = self.fainted_now[side];
        }
        let moving = self.req.as_ref().is_some_and(|r| r.asked == Asked::Move);
        // What the log says of each position the observation does not read.
        for s in 0..2 {
            for pos in 0..ACTIVE {
                let r = b.active(s, pos);
                if !b.mon(r).is_active {
                    continue;
                }
                let spot = &self.spots[s][pos];
                b.mon_mut(r).active_move_actions = spot.actions;
                if moving {
                    // (The turn's end has been and gone: nobody on the field is new to it now.)
                    b.mon_mut(r).newly_switched = false;
                }
                // A Choice lock: of its own, as the request has it (every move barred but the one the
                // item holds it to, with a Choice item at work); of the other side's, as the log has
                // shown it. The move held to is the first it used with the item at work (`lock`), not
                // the last it used: locked into a move that is then Disabled, it Struggles, and the
                // lock is still on the first.
                let asked = self.req.as_ref().and_then(|r| r.active.get(pos));
                let one_move = asked.is_some_and(|a| a.moves.len() == 1);
                let magic_room = self.field.pseudo.iter().any(|p| p.0 == Pseudo::Magicroom);
                let held = if s != me || !moving || one_move || magic_room {
                    // As the log has it: at a replacement request there is no move request to read it from
                    // (it outlasts the turn's end, and holds the next turn); a request with one move to offer
                    // (to recharge, a charging move's second turn, a rampage) shows nothing of it; and under Magic
                    // Room a lock made before stays without barring anything.
                    spot.lock.filter(|_| self.choice_held(s, pos))
                } else {
                    let asked = self.req.as_ref().and_then(|r| r.active.get(pos));
                    let item = self.own_now(pos).map_or(it::NONE, |now| now.item);
                    let to = spot.lock.unwrap_or(spot.last_move);
                    let choice = ITEMS[item as usize].flags & IF_CHOICE != 0 && to != NO_MOVE && to != mv::STRUGGLE;
                    asked.filter(|_| choice).and_then(|a| {
                        let open: Vec<u16> = a.moves.iter().filter(|m| !m.disabled).map(|m| m.id).collect();
                        // (Locked into a move it cannot use, no PP left or Disabled, it is offered Struggle alone.)
                        let only_struggle = a.moves.len() == 1 && a.moves[0].id == mv::STRUGGLE;
                        // Locked into a move by the move itself (a charging turn, a rampage), it is offered that
                        // move alone, and the request says nothing of the item: the item took hold as the move
                        // was first used with it at work (`onModifyMove`), which is `lock`.
                        let locked_in = a.moves.len() == 1 && spot.lock == Some(a.moves[0].id);
                        (only_struggle || locked_in || (a.moves.len() > 1 && open == [to])).then_some(to)
                    })
                };
                // A rampage: of its own, as the request has it (locked into a rampage move); at a
                // replacement request, which has no move request to say, as the log has it.
                let own_rampage = (s == me)
                    .then(|| if moving { b.mon(r).locked_move } else { self.rampage[s][r.idx as usize] })
                    .filter(|&m| m != NO_MOVE && MOVES[m as usize].self_volatile == Some(VolKind::Lockedmove));
                let idx = r.idx as usize;
                {
                    let m = b.mon_mut(r);
                    let n = m.n_moves as usize;
                    for slot in m.moves[..n].iter_mut() {
                        slot.used = spot.paid.contains(&slot.id);
                    }
                    (m.times_attacked, m.was_attacked) = (spot.hits, spot.attacked);
                    m.last_attack_damage = spot.last_damage;
                    m.n_damaged_by = 0;
                    for &(by, slot, damage, _) in spot.damaged_by.iter().take(MAX_TEAM) {
                        let k = m.n_damaged_by as usize;
                        m.damaged_by[k] = DamagedBy { idx: by as u8, slot: slot as u8, damage, this_turn: !moving };
                        m.n_damaged_by += 1;
                    }
                    let result = |r: Option<Option<bool>>| match r {
                        None => Res::Undef,
                        Some(None) => Res::Null,
                        Some(Some(ok)) => Res::Bool(ok),
                    };
                    // This turn's and the last: at a move request this turn has not begun (`result_now`
                    // was moved into `result_last` at `|turn|`); at a replacement request at the turn's
                    // end it is the turn just played, which `end_turn` makes the last as the next begins.
                    (m.move_last_turn, m.move_this_turn) = (result(spot.result_last), result(spot.result_now));
                    m.last_move_loc = match spot.last_move_loc {
                        Some(loc) if s == me && loc != 0 => loc,
                        _ => spot.aimed,
                    };
                    m.said.attacked_by = spot.attacked_by;
                    m.said.known = !spot.known_off && (m.said.known || spot.attacked_by != 0);
                    // What a Reflect Type this turn leaves for the turn's end to settle (`Battle::types_settled`, after
                    // a replacement request): whether it took another's types, a foe's the last time, whether its side
                    // has been told them since, and what that side took them to be before.
                    (m.said.copied, m.said.copied_foe, m.said.told) = (spot.copied, spot.copied_foe, spot.told);
                    if spot.copied {
                        m.said.before = spot.before;
                    }
                    (m.stats_lowered_this_turn, m.stats_raised_this_turn) = (spot.lowered, spot.raised);
                    m.used_item_this_turn = spot.used_item;
                    // An ability's own state, where the log has shown it: an Unnerve that has
                    // announced itself, a Disguise that is broken.
                    let unnerved = spot.unnerved && m.ability == ab::UNNERVE;
                    let protean = spot.protean && matches!(m.ability, ab::PROTEAN | ab::LIBERO);
                    if unnerved || protean || spot.busted {
                        m.ability_st.a = 1;
                    }
                    if m.ability == ab::SUPREMEOVERLORD {
                        m.ability_st.a = spot.fallen.min(5) as i16;
                    }
                    if m.ability == ab::CUDCHEW
                        && let Some(berry) = spot.cud
                    {
                        (m.ability_st.a, m.ability_st.b) = (berry as i16, 1);
                    }
                    if m.ability == ab::BERSERK && spot.berserk_off {
                        m.ability_st.a = 2;
                    }
                    if m.ability == ab::RIPEN && spot.ripen_weaken {
                        m.ability_st.a = 1;
                    }
                    if m.ability == ab::TRACE && spot.trace_seek {
                        m.ability_st.a = 1;
                    }
                    m.live.copied_gastro = spot.copied_gastro;
                    // Leaving of its own accord (an Emergency Exit), it has had its before-switch-out step by a
                    // replacement request (`runPursuitActivation`: `skipPursuit`).
                    m.switch_flag |= spot.exiting;
                    if b_request_switch && m.hp > 0 && m.switch_flag {
                        m.skip_before_switch_out = true;
                    }
                    // Its types as they now stand, as everyone has them (a Mega's are its own); but while a foe's types
                    // it took this turn are not on show, Showdown's `apparentType` stays what it was until the turn's
                    // end says them.
                    m.said.apparent = if spot.copied && spot.known_off { spot.before } else { m.types };
                }
                // One of its own in disguise: the Pokémon it passes for.
                if s == me && self.own_disguised(pos) {
                    let us = &self.reader.sides[me];
                    let name = us.active[pos].as_ref().and_then(|l| us.team.get(l.entry)).map(|e| e.name.as_str());
                    if let Some(k) = name.and_then(|n| self.own.iter().position(|o| o.name == n)) {
                        b.mon_mut(r).illusion = k as u8 + 1;
                    }
                }
                // Unburden's condition, which stays while the ability does once an item has gone, whatever
                // item comes after (it only doubles Speed with none).
                // (Not under Gastro Acid: suppressing the ability ends it, Unburden's `onEnd`, and while suppressed
                // nothing puts it up again.)
                let unburden = spot.lost_item && b.mon(r).ability == ab::UNBURDEN && !spot.has(VolKind::Gastroacid);
                let loc = b.mon(r).last_move_loc;
                let vols = &mut b.vols[s][pos];
                // A Choice lock, an Unburden and a rampage's lock, each where it took hold among the
                // conditions, which the engine keeps in the order they went up (each condition's `seq`).
                let mut put: Vec<(u32, Cond<VolKind>)> = Vec::new();
                if let Some(held) = held
                    && !vols.has(VolKind::Choicelock)
                {
                    let mut c = Cond::new(VolKind::Choicelock);
                    c.data = held + 1;
                    put.push((spot.choice_seq, c));
                }
                if unburden {
                    put.push((spot.unburden_seq, Cond::new(VolKind::Unburden)));
                }
                let rampage = if s == me { own_rampage } else { Some(self.rampage[s][idx]).filter(|&m| m != NO_MOVE) };
                if let Some(m) = rampage
                    && !vols.has(VolKind::Lockedmove)
                {
                    let mut c = Cond::new(VolKind::Lockedmove);
                    (c.data, c.duration) = (m + 1, 1);
                    put.push((spot.rampage_seq, c));
                }
                if !put.is_empty() {
                    put.sort_by_key(|p| p.0);
                    let seq_of = |kind: VolKind| spot.vols.iter().find(|v| v.kind == kind).map(|v| v.seq);
                    let mut all: Vec<(Option<u32>, Cond<VolKind>)> = vols
                        .as_slice()
                        .iter()
                        .filter(|c| c.kind != VolKind::Unburden || !unburden)
                        .map(|c| (seq_of(c.kind), *c))
                        .collect();
                    for (seq, c) in put {
                        // After the last condition that went up before it.
                        let at = all.iter().rposition(|(s, _)| s.is_some_and(|s| s < seq)).map_or(0, |k| k + 1);
                        all.insert(at, (Some(seq), c));
                    }
                    vols.clear();
                    for (_, c) in all {
                        vols.push(c);
                    }
                }
                // A charging move remembers where it was aimed: the place last chosen, or where its caller's
                // draw went (with two standing, untold: the first place stands for either).
                let aim = if s == me { spot.charge_aim } else { None };
                for c in vols.as_mut_slice() {
                    if CHARGING.contains(&c.kind) {
                        c.st.a = aim.map_or(loc, |a| a.unwrap_or(1)) as i16;
                    }
                }
                for v in &spot.vols {
                    let Some(c) = vols.get_mut(v.kind) else { continue };
                    if vol_holds_move(v.kind) {
                        c.data = if v.about == NO_MOVE { 0 } else { v.about + 1 };
                    }
                    if v.kind == VolKind::Metronome {
                        c.st.a = v.given.0;
                    }
                    if c.duration == 0 {
                        c.duration = v.turns_left();
                    }
                    // Whoever keeps it up, where the engine asks (Leech Seed's healer, a binding
                    // move's user, Yawn's, Attract's, Imprison's, Octolock's: it holds only while
                    // its user is on the field).
                    let read = matches!(
                        v.kind,
                        VolKind::Leechseed
                            | VolKind::Partiallytrapped
                            | VolKind::Yawn
                            | VolKind::Attract
                            | VolKind::Imprison
                            | VolKind::Trapped
                            | VolKind::Trapper
                            | VolKind::Octolock
                            | VolKind::Syrupbomb
                            | VolKind::Substitute
                            | VolKind::Lockon
                    );
                    if read && let Some((s2, idx2, slot)) = v.by {
                        (c.source, c.source_slot) = (Some(MonRef { side: s2 as u8, idx: idx2 as u8 }), slot as u8);
                    } else if read && v.source != 0 {
                        let at = (0..2)
                            .flat_map(|s2| (0..ACTIVE).map(move |p2| (s2, p2)))
                            .find(|&(s2, p2)| self.spots[s2][p2].stay == v.source && self.present(s2, p2));
                        if let Some((s2, p2)) = at {
                            (c.source, c.source_slot) = (Some(self.mon_ref(s2, p2)), (s2 * ACTIVE + p2) as u8);
                        }
                    }
                }
                // What the engine needs a move for and the log has not said is left out.
                // (The Metronome item's condition with no move yet has none to say.)
                let unsaid: Vec<VolKind> = vols
                    .as_slice()
                    .iter()
                    .filter(|c| vol_holds_move(c.kind) && c.data == 0 && c.kind != VolKind::Metronome)
                    .map(|c| c.kind)
                    .collect();
                for kind in unsaid {
                    vols.remove(kind);
                }
            }
        }
        // What each has used up or eaten, the last move that ran, and the counts of the effects
        // with a hidden length, for those still under them.
        for s in 0..2 {
            for idx in 0..b.sides[s].n as usize {
                let (item, _) = self.used_up[s][idx];
                let m = &mut b.sides[s].team[idx];
                if item != it::NONE {
                    m.last_item = item;
                }
                m.ate_berry = self.ate[s][idx];
                m.syrup_triggered = self.syrup[s][idx];
                if s == opp && m.said.apparent == [Type::None; 2] {
                    m.said = TypesSaid::fresh(m.types);
                }
            }
        }
        b.last_move = self.last_move;
        for s in 0..2 {
            for idx in 0..b.sides[s].n as usize {
                let m = &mut b.sides[s].team[idx];
                if m.status == Status::Frz {
                    m.status_time = frozen_time(self.frozen[s][idx]);
                }
            }
        }
        // A Wish heals half its maker's max HP, stored as it was made (`conditions.rs` Wish's start): one's
        // own exactly; the other side's from its sheet, which belief draws afresh (`_rebuild_wishes`).
        for s in 0..2 {
            for pos in 0..ACTIVE {
                let made = b.sides[s].slot_conds[pos].get(SlotCond::Wish).and_then(|c| c.source);
                if let Some(r) = made.filter(|r| (r.idx as usize) < b.sides[r.side as usize].n as usize) {
                    let hp = b.mon(r).max_hp() / 2;
                    if let Some(c) = b.sides[s].slot_conds[pos].get_mut(SlotCond::Wish) {
                        c.data = hp;
                    }
                }
            }
        }
        Ok(())
    }

    /// The counts of the hidden-length effects as [`Counts`] keeps them: none for an effect a
    /// Pokémon is not under now.
    fn counts_now(&self, b: &Battle) -> Counts {
        let mut c = self.counts;
        (c.confusion, c.trap) = ([[0; MAX_TEAM]; 2], [[0; MAX_TEAM]; 2]);
        for s in 0..2 {
            for idx in 0..MAX_TEAM {
                let r = MonRef { side: s as u8, idx: idx as u8 };
                let m = b.mon(r);
                let here = idx < b.sides[s].n as usize && m.is_active && !m.fainted;
                if idx >= b.sides[s].n as usize || m.status != Status::Slp {
                    (c.sleep[s][idx], c.rest[s][idx]) = (0, false);
                }
                if !(here && b.vols(r).has(VolKind::Lockedmove)) {
                    c.rampage[s][idx] = 0;
                }
            }
            // Confusion and binding, from the stay at each position, onto whoever stands there.
            for pos in 0..ACTIVE.min(b.sides[s].n as usize) {
                let r = b.active(s, pos);
                let m = b.mon(r);
                if !m.is_active || m.fainted {
                    continue;
                }
                let spot = &self.spots[s][pos];
                if b.vols(r).has(VolKind::Confusion) {
                    c.confusion[s][r.idx as usize] = spot.confused;
                }
                if b.vols(r).has(VolKind::Partiallytrapped) {
                    c.trap[s][r.idx as usize] = spot.bound;
                }
            }
        }
        // (The other side's sleep counts are kept by name already, `their_name`: one of two on the field that
        // look the same, taken for the copy, makes its runs under the name it passes for as well as under its
        // own number, so the name's count is what the log has shown under it, by whoever made them (D96).
        // Until S4.5a the copy's own count stood in for the name's here, which put an awake copy's 0 on a
        // name asleep beside it.)
        c
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
    fn own_frac(&self, begun: &Begun, chosen: u16, at: usize) -> Option<i8> {
        let mut b = *self.turn_start.as_ref()?;
        if !b.mon(begun.r).is_active {
            return None;
        }
        let a = b.resolve_move(begun.r, chosen, 0);
        // A Quick Claw is rolled for, and says so when it works; what always holds a move back does not.
        Some(if begun.quick_at(at) { 1 } else { a.frac.min(0) })
    }

    /// A move as it stood in the queue when the `at`th of this stretch began.
    fn queued(&self, j: usize, at: usize) -> Queued {
        let s = &self.begun[j];
        let mut q = Queued {
            r: s.r,
            order: 200,
            priority: 0,
            frac: s.quick_at(at) as i8,
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
            match (speed, priority, self.own_frac(s, q.move_id, at)) {
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
                // (The observation gives a sleep count only to a Pokémon shown asleep, and the
                // rebuilt battle leaves the other side's statuses to what it was shown.)
                let mut counts = self.counts_now(b);
                counts.sleep = self.counts.sleep;
                observe_battle(b, &self.timers, speeds, &counts, &self.stats, self.side, f, i);
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
