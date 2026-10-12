//! What the order of moves has shown about each Pokémon's Speed.
//!
//! A player never sees the other side's Speed, but sees who moves first, and
//! knows its own Pokémon's Speed exactly. So every time one of theirs and one
//! of its own move in the same priority bracket, it learns which is faster.
//! [`Speeds`] keeps what follows from that, for every Pokémon of both teams
//! as the other side knows it.
//!
//! # What is kept
//!
//! Not a range of numbers but the set of ways the Pokémon can have been
//! built that are still possible: every combination of
//!
//! * the 33 amounts of stat points it can have in Speed (0 to 32),
//! * the three things its nature can do to Speed (lower, nothing, raise),
//! * the three things its item can do (nothing, Choice Scarf, Iron Ball).
//!
//! It starts as everything the rules allow (narrowed by an open team sheet,
//! which gives nature and item) and each observation strikes out the
//! combinations that would have moved in the other order. A range of Speed
//! follows from the set ([`Speeds::range`]); so does "it must be holding a
//! Choice Scarf", when nothing else is left that explains what was seen.
//! Because the set is about how the Pokémon was built and not about a
//! number, Mega Evolution needs nothing special: the same points and nature
//! give the new forme's Speed.
//!
//! Items narrow it as well. An item that shows itself is the Pokémon's one
//! item, so it rules out the other two classes. And under the regulation's
//! item clause a team has each item once: when one Pokémon is found to have
//! been registered with the Choice Scarf (it is proved to hold one, or loses
//! one, having been handed nothing), the Scarf is struck from every
//! team-mate that still holds what it was registered with.
//!
//! # What counts as an observation
//!
//! When a move starts, it was at the head of the queue, which Showdown sorts
//! by priority and then by Speed as things stand at that moment (re-sorted
//! after every action). So it was at least as fast as every move of the same
//! priority still waiting. That is used when both moves go on to be named in
//! the log (a Pokémon that flinches does not show what it chose, and so not
//! its priority), and when everything else that enters the comparison is
//! public: stat stages, paralysis, Tailwind, Trick Room, the weather or
//! terrain an ability reads.
//!
//! It is passed over whenever something the watcher has not been shown could
//! be at work: an ability the Pokémon might legally have that changes Speed
//! or priority in the conditions of the moment (Swift Swim in rain, Prankster
//! on a status move, Stall, Unburden once the item is gone, Klutz), a
//! Pokémon that may be an Illusion or has transformed, a Speed Swap. A
//! comparison passed over costs a little knowledge; one wrongly used would
//! leave a set without the truth in it. `tests/speed.rs` plays thousands of
//! games with every legal ability and item and checks that it never does.
//!
//! Nothing here reads what the watcher has not been shown, bar the fields
//! marked as the truth, which only the self-check ([`Speeds::strict`]) reads.

// Natures, item classes and events are looked up by index in several arrays at once.
#![allow(clippy::needless_range_loop)]

use std::cell::{Cell, RefCell};

use crate::battle::{PokemonSet, boosted, modify};
use crate::data::*;
use crate::format::Format;
use crate::shown::{ItemShown, UNKNOWN};
use crate::state::*;

/// Amounts of stat points a stat can have: 0 to 32.
const POINTS: u32 = 33;
const EVERY_AMOUNT: u64 = (1 << POINTS) - 1;
/// What a nature does to Speed: 0 lowers it, 1 leaves it, 2 raises it.
const NATURES: usize = 3;
/// What an item does to Speed: 0 nothing, 1 Choice Scarf, 2 Iron Ball.
const CLASSES: usize = 3;
const SCARF: usize = 1;
const IRON_BALL: usize = 2;
/// Each class's multiplier, in 4096ths.
const CLASS_MOD: [u32; CLASSES] = [4096, 6144, 2048];

fn class_of(item: u16) -> usize {
    match item {
        it::CHOICESCARF => SCARF,
        it::IRONBALL => IRON_BALL,
        _ => 0,
    }
}

fn nature_class(nature: (u8, u8)) -> usize {
    if nature.0 == nature.1 {
        1
    } else if nature.0 == 5 {
        2
    } else if nature.1 == 5 {
        0
    } else {
        1
    }
}

/// What a Pokémon's own side knows of its Speed, in the terms of [`Belief::range`]
/// and [`Belief::items`]: its Speed stat with what its item does to it, and the item.
pub fn own(stat: u16, item: u16) -> (u32, [bool; 4]) {
    let class = class_of(item);
    (modify(stat as u32, CLASS_MOD[class]), [class == SCARF, class == SCARF, class == IRON_BALL, class == IRON_BALL])
}

/// The Speed stat of a species with a nature class and an amount of stat points (`calc_stats`, for Speed alone).
pub fn raw_speed(species: u16, nature: usize, points: u32) -> u32 {
    let v = SPECIES[species as usize].base[5] as u32 + 20 + points;
    match nature {
        0 => ((v * 90) & 0xFFFF) / 100,
        2 => ((v * 110) & 0xFFFF) / 100,
        _ => v,
    }
}

/// The ways one Pokémon's Speed can still have come about: a bit for each amount of stat points, by nature and item class.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Belief {
    bits: [[u64; CLASSES]; NATURES],
}

impl Belief {
    fn everything() -> Belief {
        Belief { bits: [[EVERY_AMOUNT; CLASSES]; NATURES] }
    }

    fn is_empty(&self) -> bool {
        self.bits.iter().flatten().all(|&b| b == 0)
    }

    fn keep_nature(&mut self, nature: usize) {
        for n in 0..NATURES {
            if n != nature {
                self.bits[n] = [0; CLASSES];
            }
        }
    }

    fn keep_class(&mut self, class: usize) {
        for row in &mut self.bits {
            for (k, bits) in row.iter_mut().enumerate() {
                if k != class {
                    *bits = 0;
                }
            }
        }
    }

    fn drop_class(&mut self, class: usize) {
        for row in &mut self.bits {
            row[class] = 0;
        }
    }

    /// The item has changed hands: whatever class it was, the points and the nature
    /// that were possible with any of them are possible with whatever it holds now.
    fn forget_class(&mut self) {
        for row in &mut self.bits {
            let any = row.iter().fold(0, |a, &b| a | b);
            *row = [any; CLASSES];
        }
    }

    /// Whether this combination is still possible.
    pub fn has(&self, nature: (u8, u8), points: u8, item: u16) -> bool {
        self.bits[nature_class(nature)][class_of(item)] & (1 << points) != 0
    }

    /// The lowest and highest Speed `species` can have with this still possible, counting
    /// what its item does: the stat itself, half again with a Choice Scarf, halved by an
    /// Iron Ball. (Speed as it behaves: whether a Pokémon is fast or holds a Choice Scarf
    /// comes to the same thing for who moves first.)
    pub fn range(&self, species: u16) -> (u32, u32) {
        let (mut lo, mut hi) = (u32::MAX, 0);
        for n in 0..NATURES {
            for k in 0..CLASSES {
                let bits = self.bits[n][k];
                if bits != 0 {
                    lo = lo.min(modify(raw_speed(species, n, bits.trailing_zeros()), CLASS_MOD[k]));
                    hi = hi.max(modify(raw_speed(species, n, 63 - bits.leading_zeros()), CLASS_MOD[k]));
                }
            }
        }
        if lo > hi { (0, 0) } else { (lo, hi) }
    }

    fn class_possible(&self, class: usize) -> bool {
        self.bits.iter().any(|row| row[class] != 0)
    }

    /// Whether the item can still be a Choice Scarf, and whether it must be; the same for an Iron Ball.
    pub fn items(&self) -> [bool; 4] {
        let only =
            |class: usize| self.class_possible(class) && (0..CLASSES).all(|k| k == class || !self.class_possible(k));
        [self.class_possible(SCARF), only(SCARF), self.class_possible(IRON_BALL), only(IRON_BALL)]
    }

    /// Strikes out every combination `keep` does not accept. `keep` must go one way with
    /// the stat points (more of them never makes a Pokémon slower), which lets each run of
    /// amounts be cut at one place found by halving, in place of trying all 33.
    fn retain(&mut self, keep: impl Fn(usize, u32, usize) -> bool) {
        for n in 0..NATURES {
            for k in 0..CLASSES {
                let bits = self.bits[n][k];
                if bits == 0 {
                    continue;
                }
                let (mut lo, mut hi) = (bits.trailing_zeros(), 63 - bits.leading_zeros());
                self.bits[n][k] = match (keep(n, lo, k), keep(n, hi, k)) {
                    (true, true) => bits,
                    (false, false) => 0,
                    // Kept from some amount up: the least that is.
                    (false, true) => {
                        while lo < hi {
                            let mid = (lo + hi) / 2;
                            if keep(n, mid, k) { hi = mid } else { lo = mid + 1 }
                        }
                        bits & !((1 << lo) - 1)
                    }
                    // Kept up to some amount: the most that is.
                    (true, false) => {
                        while lo < hi {
                            let mid = (lo + hi).div_ceil(2);
                            if keep(n, mid, k) { lo = mid } else { hi = mid - 1 }
                        }
                        bits & ((1 << (lo + 1)) - 1)
                    }
                };
            }
        }
    }
}

// ----------------------------------------------------- what the engine notes

/// One Pokémon on the field at one moment, as everyone watching sees it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Seen {
    pub r: MonRef,
    pub present: bool,
    /// The entry of its side's registered team it appears to be.
    pub listed: u8,
    /// It may be an Illusion, or has transformed: its Speed is not its own.
    pub doubt: bool,
    /// It has transformed: its Speed is another Pokémon's, which its own side does not know either.
    pub transformed: bool,
    /// The species on show.
    pub species: u16,
    pub stage: i8,
    pub status: Status,
    pub hp_full: bool,
    pub item: ItemShown,
    /// Its ability, if shown (`UNKNOWN` if not).
    pub ability: u16,
    pub ability_changed: bool,
    pub gastro_acid: bool,
    /// The truth, for [`Speeds::strict`] alone.
    true_item: u16,
}

impl Seen {
    /// Nobody there.
    pub(crate) fn nobody(r: MonRef) -> Seen {
        Seen {
            r,
            present: false,
            listed: NOT_LISTED,
            doubt: false,
            transformed: false,
            species: 0,
            stage: 0,
            status: Status::None,
            hp_full: true,
            item: ItemShown::Unknown,
            ability: UNKNOWN,
            ability_changed: false,
            gastro_acid: false,
            true_item: it::NONE,
        }
    }
}

/// The field at one moment.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FieldSeen {
    pub trick_room: bool,
    pub magic_room: bool,
    pub weather: Weather,
    pub terrain: Terrain,
    pub tailwind: [bool; 2],
}

/// A move waiting in the queue, as the queue is sorted.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Queued {
    pub r: MonRef,
    pub order: u32,
    /// Ten times the priority, plus the fractional priority of a Quick Claw and the like.
    pub priority: i32,
    pub frac: i8,
    /// Speed as the queue sorts by it: negated under Trick Room.
    pub speed: i32,
    pub move_id: u16,
    /// For a move that is starting: its user is under an Encore.
    pub encored: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Event {
    /// A queued move has reached the head of the queue and starts.
    Start {
        me: Queued,
        /// The moves still waiting.
        rest: [Queued; 3],
        n_rest: u8,
        seen: [Seen; 4],
        field: FieldSeen,
        /// The move the log went on to name for it (`NO_MOVE`: none, as when it flinches).
        named: u16,
    },
    /// The log has a Pokémon lose its item (`lost`) or come by one.
    Item {
        side: u8,
        listed: u8,
        doubt: bool,
        item: u16,
        lost: bool,
    },
    /// A new stay can reuse the roster number of a departing disguise.
    Switch {
        r: MonRef,
    },
    Swap {
        first: MonRef,
        second: MonRef,
    },
}

thread_local! {
    static ON: Cell<bool> = const { Cell::new(false) };
    static LOG: RefCell<Vec<Event>> = const { RefCell::new(Vec::new()) };
    /// The Pokémon whose queued move is under way, and where its entry is in the log.
    static UNDER_WAY: Cell<Option<(MonRef, usize)>> = const { Cell::new(None) };
}

/// Runs `f` with the engine noting what [`Speeds::digest`] reads, into `events` (emptied first).
pub(crate) fn record<T>(events: &mut Vec<Event>, f: impl FnOnce() -> T) -> T {
    events.clear();
    LOG.with(|log| std::mem::swap(&mut *log.borrow_mut(), events));
    ON.with(|on| on.set(true));
    let out = f();
    ON.with(|on| on.set(false));
    UNDER_WAY.with(|u| u.set(None));
    LOG.with(|log| std::mem::swap(&mut *log.borrow_mut(), events));
    out
}

fn on() -> bool {
    ON.with(|on| on.get())
}

pub(crate) fn seen(b: &Battle, r: MonRef) -> Seen {
    let m = b.mon(r);
    let live = &m.live;
    Seen {
        r,
        present: b.shown_live(r) && !m.fainted,
        listed: live.listed,
        doubt: live.suspect || m.transformed,
        transformed: m.transformed,
        species: if m.illusion != 0 { live.species } else { m.species },
        stage: m.boosts[SPE],
        status: m.status,
        hp_full: m.hp == m.max_hp(),
        item: live.item,
        ability: live.ability,
        ability_changed: live.ability_changed,
        gastro_acid: b.vols(r).has(VolKind::Gastroacid)
            || (live.copied_gastro && live.ability == UNKNOWN && !b.open_team_sheets),
        true_item: m.item,
    }
}

pub(crate) fn field_seen(b: &Battle) -> FieldSeen {
    FieldSeen {
        trick_room: b.field.pseudo.has(Pseudo::Trickroom),
        magic_room: b.field.pseudo.has(Pseudo::Magicroom),
        weather: b.field_weather(),
        terrain: b.field.terrain.kind,
        tailwind: [b.sides[0].conds.has(SideCond::Tailwind), b.sides[1].conds.has(SideCond::Tailwind)],
    }
}

fn queued(a: &Action) -> Queued {
    Queued {
        r: a.mon.unwrap_or(MonRef { side: 0, idx: 0 }),
        order: a.order,
        priority: a.priority,
        frac: a.frac,
        speed: a.speed,
        move_id: a.move_id,
        encored: false,
    }
}

impl Battle {
    pub(crate) fn note_speed_switch(&self, r: MonRef) {
        if on() {
            LOG.with(|log| log.borrow_mut().push(Event::Switch { r }));
        }
    }

    /// A queued move is about to run. (Called by the turn loop.)
    pub(crate) fn note_move_starts(&self, a: &Action) {
        if !on() {
            return;
        }
        let mut me = queued(a);
        me.encored = self.vols(me.r).has(VolKind::Encore);
        let mut rest = [me; 3];
        let mut n_rest = 0;
        for other in self.queue.as_slice() {
            if other.kind == ActKind::Move && other.mon.is_some() && n_rest < 3 {
                rest[n_rest] = queued(other);
                n_rest += 1;
            }
        }
        let at = |side: usize, pos: usize| seen(self, self.active(side, pos));
        let event = Event::Start {
            me,
            rest,
            n_rest: n_rest as u8,
            seen: [at(0, 0), at(0, 1), at(1, 0), at(1, 1)],
            field: field_seen(self),
            named: NO_MOVE,
        };
        LOG.with(|log| {
            let mut log = log.borrow_mut();
            UNDER_WAY.with(|u| u.set(Some((me.r, log.len()))));
            log.push(event);
        });
    }

    /// The move under way is over.
    pub(crate) fn note_move_ends(&self) {
        if on() {
            UNDER_WAY.with(|u| u.set(None));
        }
    }

    /// The log names `move_id` as a move `r` used or tried to use.
    pub(crate) fn note_move_named(&self, r: MonRef, move_id: u16) {
        if !on() {
            return;
        }
        if let Some((who, at)) = UNDER_WAY.with(|u| u.get())
            && who == r
        {
            LOG.with(|log| {
                if let Some(Event::Start { named, .. }) = log.borrow_mut().get_mut(at)
                    && *named == NO_MOVE
                {
                    *named = move_id;
                }
            });
        }
    }

    /// A successful Speed Swap exchanged these stats, including a called move.
    pub(crate) fn note_speed_swap(&self, first: MonRef, second: MonRef) {
        if on() {
            LOG.with(|log| log.borrow_mut().push(Event::Swap { first, second }));
        }
    }

    /// The log has `r` lose `item`, or come by it.
    pub(crate) fn note_item(&mut self, r: MonRef, item: u16, lost: bool) {
        if !on() {
            return;
        }
        let rec = *self.shown_mut(r);
        let event = Event::Item { side: r.side, listed: rec.listed, doubt: rec.suspect || rec.tainted, item, lost };
        LOG.with(|log| log.borrow_mut().push(event));
    }
}

// ------------------------------------------------------------ reading them

/// What is known of an ability.
enum Know {
    Is(u16),
    /// Not shown: one of those the species may have.
    Among(&'static [u16]),
    Any,
}

impl Know {
    fn is(&self, ability: u16) -> bool {
        matches!(self, Know::Is(a) if *a == ability)
    }
    fn could_be(&self, ability: u16) -> bool {
        match self {
            Know::Is(a) => *a == ability,
            Know::Among(list) => list.contains(&ability),
            Know::Any => true,
        }
    }
}

fn know(s: &Seen, listed: &Listed, open: bool) -> Know {
    if s.gastro_acid {
        return Know::Is(ab::NOABILITY);
    }
    know_of(s.ability, s.ability_changed, s.species, listed, open)
}

/// Whether, for all that has been shown, a Pokémon may have `ability`.
pub(crate) fn may_have(s: &Seen, listed: &Listed, open: bool, ability: u16) -> bool {
    know(s, listed, open).could_be(ability)
}

/// The same from the three things it rests on: the ability shown, whether it has been replaced, and the species.
fn know_of(ability: u16, changed: bool, species: u16, listed: &Listed, open: bool) -> Know {
    if ability != UNKNOWN {
        return Know::Is(ability);
    }
    if changed {
        return Know::Any;
    }
    if crate::obs::is_mega(species) {
        return Know::Is(SPECIES[species as usize].ability0);
    }
    if open {
        return Know::Is(listed.ability);
    }
    match Format::current().rule(listed.species) {
        Some(rule) => Know::Among(&rule.abilities),
        None => Know::Any,
    }
}

/// Everything public that turns a Speed stat into the Speed the queue sorts by, the item aside.
struct Mods {
    stage: i32,
    /// In 4096ths.
    chain: u32,
    /// Paralysis: halved after everything else.
    halve: bool,
    /// Whether items do anything (not in a Magic Room).
    items: bool,
    trick_room: bool,
}

impl Mods {
    fn speed(&self, raw: u32, class: usize) -> i32 {
        let chain = if self.items { (self.chain * CLASS_MOD[class] + 2048) >> 12 } else { self.chain };
        let mut v = modify(boosted(raw, self.stage), chain);
        if self.halve {
            v = v * 50 / 100;
        }
        let v = v.min(10000) as i32;
        if self.trick_room { -v } else { v }
    }
}

/// `None`: something the watcher has not been shown may be changing this Pokémon's Speed.
fn mods(s: &Seen, known: &Know, field: &FieldSeen, belief: &Belief) -> Option<Mods> {
    if !s.present || s.doubt {
        return None;
    }
    let mut chain = 4096u32;
    let mut times = |num: u32| chain = (chain * num + 2048) >> 12;
    if field.tailwind[s.r.side as usize] {
        times(8192);
    }
    let ailing = s.status != Status::None;
    for (ability, active, num) in [
        (ab::CHLOROPHYLL, field.weather == Weather::Sunnyday, 8192),
        (ab::SWIFTSWIM, field.weather == Weather::Raindance, 8192),
        (ab::SANDRUSH, field.weather == Weather::Sandstorm, 8192),
        (ab::SLUSHRUSH, field.weather == Weather::Snowscape, 8192),
        (ab::SURGESURFER, field.terrain == Terrain::Electricterrain, 8192),
        (ab::QUICKFEET, ailing, 6144),
    ] {
        if !active {
            continue;
        }
        if known.is(ability) {
            times(num);
        } else if known.could_be(ability) {
            return None;
        }
    }
    // Unburden lasts until its holder leaves the field, and the record of a lost item lasts longer.
    if matches!(s.item, ItemShown::Lost(_)) && known.could_be(ab::UNBURDEN) {
        return None;
    }
    // Klutz makes nothing of a Choice Scarf, so it matters unless there is known to be none.
    if known.could_be(ab::KLUTZ) && (belief.class_possible(SCARF) || belief.class_possible(IRON_BALL)) {
        return None;
    }
    Some(Mods {
        stage: s.stage as i32,
        chain,
        halve: s.status == Status::Par && !known.is(ab::QUICKFEET),
        items: !field.magic_room,
        trick_room: field.trick_room,
    })
}

/// The Speed the queue sorts a Pokémon by, worked out by its own side from what
/// it knows of it (ability, item and Speed stat) and what is in plain sight.
/// `lost_item`: it has lost an item since it came in, which is when Unburden
/// starts. `None` if something this does not model is at work (see [`mods`]).
pub(crate) fn own_speed(
    s: &Seen,
    ability: u16,
    item: u16,
    lost_item: bool,
    stat: u16,
    field: &FieldSeen,
) -> Option<i32> {
    let ability = if s.gastro_acid { ab::NOABILITY } else { ability };
    // (Its own Klutz it knows about: the item then does nothing for its Speed.)
    let class = if ability == ab::KLUTZ { 0 } else { class_of(item) };
    let mut belief = Belief { bits: [[0; CLASSES]; NATURES] };
    belief.bits[1][class] = 1;
    // Whether Unburden is at work its own side can tell, where a watcher has to pass.
    let mut seen = *s;
    seen.item = ItemShown::Unknown;
    let mut m = mods(&seen, &Know::Is(ability), field, &belief)?;
    if ability == ab::UNBURDEN && lost_item && item == it::NONE {
        m.chain = (m.chain * 8192 + 2048) >> 12;
    }
    Some(m.speed(stat as u32, class))
}

/// Ten times the priority a move has from a Pokémon, worked out by its own
/// side from what it knows (its ability now, and the one it began the turn
/// with). `None` where [`priority`] cannot say.
pub(crate) fn own_priority(
    s: &Seen,
    ability: u16,
    field: &FieldSeen,
    move_id: u16,
    grounded: Option<bool>,
) -> Option<i32> {
    let known = Know::Is(if s.gastro_acid { ab::NOABILITY } else { ability });
    // (Whether it goes last in its bracket, as with Stall, its side knows too: that is
    // worked out apart, from how things stood when it chose.)
    if move_id == mv::GRASSYGLIDE && field.terrain == Terrain::Grassyterrain {
        let mut ordinary = *field;
        ordinary.terrain = Terrain::None;
        let bonus = 10 * grounded? as i32;
        return bracket(s, &known, &ordinary, move_id).map(|p| p + bonus);
    }
    bracket(s, &known, field, move_id)
}

/// Ten times the priority `move_id` has from this Pokémon, as far as the
/// watcher can tell; `None` if an ability it has not shown could change it.
/// `at_start` is the ability it began the turn with: where a move goes
/// within its bracket is settled when it is chosen, so a Sableye that Mega
/// Evolves, or has its ability taken, still moves last that turn if it had Stall.
fn priority(s: &Seen, known: &Know, at_start: &Know, field: &FieldSeen, move_id: u16) -> Option<i32> {
    // Stall goes last in its bracket and says nothing.
    if known.could_be(ab::STALL) || at_start.could_be(ab::STALL) {
        return None;
    }
    bracket(s, known, field, move_id)
}

/// Ten times the priority bracket `move_id` goes in from this Pokémon; `None` if an
/// ability it has not shown could change it.
fn bracket(s: &Seen, known: &Know, field: &FieldSeen, move_id: u16) -> Option<i32> {
    let d = &MOVES[move_id as usize];
    let mut p = d.priority as i32;
    if move_id == mv::GRASSYGLIDE && field.terrain == Terrain::Grassyterrain {
        return None;
    }
    for (ability, applies) in
        [(ab::PRANKSTER, d.category == Category::Status), (ab::GALEWINGS, d.typ == Type::Flying && s.hp_full)]
    {
        if !applies {
            continue;
        }
        if known.is(ability) {
            p += 1;
        } else if known.could_be(ability) {
            return None;
        }
    }
    Some(10 * p)
}

/// What each side can tell of the other's Speed, kept from decision to
/// decision. Part of a game's state, next to its [`Battle`].
#[derive(Clone, Copy, Debug)]
pub struct Speeds {
    /// By side and registered Pokémon: what that side's opponent knows of it.
    belief: [[Belief; MAX_ROSTER]; 2],
    /// By side and team index: on the field since a Speed Swap was used, so its Speed may not be its own.
    swapped: [[bool; MAX_TEAM]; 2],
    /// By side and team index: what was known of its ability when the turn began (the
    /// ability shown, whether it had been replaced, and the species it then was).
    began: [[(u16, bool, u16); MAX_TEAM]; 2],
    /// By side and team index: under an Encore when the turn began.
    encored: [[bool; MAX_TEAM]; 2],
    /// By side: whether its team can be counted on to have each item once at most (the
    /// regulation's item clause, on a team that keeps it).
    clause: [bool; 2],
    /// By side and registered Pokémon: an item has come to it from elsewhere, so what it
    /// holds is no longer what it was registered with.
    handed: [[bool; MAX_ROSTER]; 2],
    /// By side and item class: the Pokémon that was registered with that item, once known (`NOT_LISTED` until then).
    owner: [[u8; CLASSES]; 2],
    /// Each active Pokémon's Speed as the queue would sort by it now. Its own side knows it.
    own: [[i32; ACTIVE]; 2],
    /// Kept for one side alone, which this is: nothing is worked out about its own
    /// Pokémon. (A player following a battle from Showdown's log has no more to go on.)
    watcher: Option<u8>,
    /// Check every observation against the truth as it is used and panic on a
    /// difference: for tests. (It reads what a player is not shown.)
    pub strict: bool,
}

/// Who moves first of two Pokémon using moves of the same priority, as far as one side can tell.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum First {
    Mine,
    Theirs,
    Unknown,
}

impl Speeds {
    /// Before anything has been seen: everything the rules allow, narrowed by
    /// the team sheets if they are `open` (a sheet gives nature and item).
    pub fn new(rosters: [&[PokemonSet]; 2], open: bool) -> Speeds {
        let mut belief = [[Belief::everything(); MAX_ROSTER]; 2];
        if open {
            for side in 0..2 {
                for (j, set) in rosters[side].iter().take(MAX_ROSTER).enumerate() {
                    belief[side][j].keep_nature(nature_class(set.nature));
                    belief[side][j].keep_class(class_of(set.item));
                }
            }
        }
        // The item clause is a fact about a legal team. A team put together by hand that
        // breaks it gets no conclusions drawn from it.
        let once = |team: &[PokemonSet]| {
            team.iter().enumerate().all(|(k, a)| a.item == it::NONE || team[..k].iter().all(|b| b.item != a.item))
        };
        let clause = Format::current().team.item_clause == 1;
        Speeds {
            belief,
            clause: [clause && once(rosters[0]), clause && once(rosters[1])],
            handed: [[false; MAX_ROSTER]; 2],
            owner: [[NOT_LISTED; CLASSES]; 2],
            swapped: [[false; MAX_TEAM]; 2],
            began: [[(UNKNOWN, true, 0); MAX_TEAM]; 2],
            encored: [[false; MAX_TEAM]; 2],
            own: [[0; ACTIVE]; 2],
            watcher: None,
            strict: false,
        }
    }

    /// Says whether `side`'s team can be counted on to have each item once at most. (By
    /// default it is, if the regulation says so and the team as given here keeps to it.)
    pub(crate) fn set_clause(&mut self, side: usize, on: bool) {
        self.clause[side] = on;
    }

    /// Each Pokémon of `side` numbered `from` goes by the number `to` from now on, and
    /// what was noted of it this turn with it. (A follower of Showdown's log numbers the
    /// other side's Pokémon as they appear. One that drops a disguise appears anew, and
    /// one in disguise gives its number up when the Pokémon it passes for comes in.)
    ///
    /// With each goes whether a Speed Swap has been used while it has stood there. The
    /// note under its old number does not say: a Pokémon in disguise may have come in
    /// under the number of one that had just left, whose note it was.
    pub(crate) fn renumber(&mut self, side: usize, moved: &[(usize, usize, bool)]) {
        // (All at once: two that stand side by side can trade numbers. A number given up
        // and not taken by the other is nobody's until someone comes in under it.)
        let (began, encored) = (self.began[side], self.encored[side]);
        for &(from, _, _) in moved {
            (self.began[side][from], self.swapped[side][from], self.encored[side][from]) =
                ((UNKNOWN, true, 0), false, false);
        }
        for &(from, to, swapped) in moved {
            (self.began[side][to], self.swapped[side][to], self.encored[side][to]) =
                (began[from], swapped, encored[from]);
        }
    }

    /// Keeps track for `side` alone: what it can tell of the other side's Speed.
    pub(crate) fn watch_as(&mut self, side: usize) {
        self.watcher = Some(side as u8);
    }

    /// What `side`'s opponent knows of the Speed of the Pokémon `side` registered `entry`th.
    pub fn belief(&self, side: usize, entry: usize) -> &Belief {
        &self.belief[side][entry]
    }

    /// The Speed the queue would sort `side`'s Pokémon at `pos` by, as things stand.
    pub fn own(&self, side: usize, pos: usize) -> i32 {
        self.own[side][pos]
    }

    /// Takes in what the engine noted since the last decision, and the battle as it now stands.
    pub(crate) fn digest(&mut self, events: &[Event], b: &Battle) {
        // Which Pokémon's Speed was not its own when each event happened.
        let mut swapped_at = Vec::with_capacity(events.len());
        for (j, event) in events.iter().enumerate() {
            swapped_at.push(self.swapped);
            match *event {
                Event::Switch { r } => self.swapped[r.side as usize][r.idx as usize] = false,
                Event::Swap { first, second } => {
                    for r in [first, second] {
                        self.swapped[r.side as usize][r.idx as usize] = true;
                    }
                }
                Event::Item { side, listed, doubt, item, lost } => {
                    let Some(belief) = self.belief[side as usize].get_mut(listed as usize) else { continue };
                    // A Mega Stone that comes to light was there all along: nothing takes one away.
                    let stone = !ITEMS[item as usize].mega.is_empty();
                    // An item it loses was its item for everything seen so far, unless it was
                    // handed the item in this same step (and the log has yet to say so).
                    let just_handed = events.iter().any(|e| {
                        matches!(*e, Event::Item { side: s2, listed: l2, item: i2, lost: false, .. }
                            if (s2, l2) == (side, listed) && ITEMS[i2 as usize].mega.is_empty())
                    });
                    if !doubt && (stone || (lost && !just_handed)) {
                        let mut kept = *belief;
                        kept.keep_class(class_of(item));
                        if !kept.is_empty() {
                            *belief = kept;
                            // And if nothing was ever handed to it, it was registered with the item.
                            if lost && !self.handed[side as usize][listed as usize] && class_of(item) != 0 {
                                self.owner[side as usize][class_of(item)] = listed;
                            }
                        } else {
                            assert!(
                                !self.strict,
                                "an item shown that nothing seen allows: {event:?}\n{belief:?}\n{:#?}\n{:#?}",
                                &events[..j],
                                b.shown(side as usize)
                            );
                        }
                    }
                    if !stone {
                        belief.forget_class();
                        self.handed[side as usize][listed as usize] |= !lost;
                        if doubt {
                            // It may be the side's Illusion Pokémon that the item went to or from.
                            let masked = b.shown_illusionists(side as usize);
                            for (j, other) in self.belief[side as usize].iter_mut().enumerate() {
                                if masked & (1 << j) != 0 {
                                    other.forget_class();
                                    self.handed[side as usize][j] = true;
                                }
                            }
                        }
                    }
                }
                Event::Start { .. } => {
                    // This one moved after each of those that started before it.
                    for i in 0..j {
                        self.compare(events, i, j, &swapped_at[i], b);
                    }
                }
            }
        }
        self.settle(b);
    }

    /// Entry `i` started before entry `j`: what that says of each one's Speed, to the other's side.
    fn compare(&mut self, events: &[Event], i: usize, j: usize, swapped: &[[bool; MAX_TEAM]; 2], b: &Battle) {
        let (
            Event::Start { me: first, rest, n_rest, seen, field, named: first_named },
            Event::Start { me: later, named: second_named, .. },
        ) = (&events[i], &events[j])
        else {
            return;
        };
        // An instructed/promoted action does not establish the ordering of
        // the original queued action, even if it happens to name the same move.
        if later.order != 200 {
            return;
        }
        // The second as it stood in the queue when the first went.
        let Some(second) = rest[..*n_rest as usize].iter().find(|q| q.r == later.r) else { return };
        let ordinary = |q: &Queued, named: u16| q.order == 200 && named != NO_MOVE && named == q.move_id;
        if first.r.side == second.r.side
            || second.move_id != later.move_id
            || !ordinary(first, *first_named)
            || !ordinary(second, *second_named)
        {
            return;
        }
        for (x, other, x_first) in [(first, second, true), (second, first, false)] {
            // What `other`'s side learns about `x`. It knows its own Pokémon's priority and Speed.
            let side = x.r.side as usize;
            if self.watcher.is_some_and(|w| w as usize == side) {
                continue;
            }
            let Some(s) = seen.iter().find(|s| s.r == x.r) else { continue };
            if !s.present || s.doubt || s.listed == NOT_LISTED || swapped[side][x.r.idx as usize] {
                continue;
            }
            let roster = &b.sides[side].roster[..b.sides[side].n_roster as usize];
            let Some(listed) = roster.get(s.listed as usize) else { continue };
            // An item changing hands in between: the set is no longer about the same thing.
            let moved = events[i..j].iter().any(
                |e| matches!(e, Event::Item { side: es, listed: el, .. } if *es as usize == side && *el == s.listed),
            );
            // A Quick Claw or a Quick Draw going off is announced, and puts the move ahead of its bracket.
            if moved || x.frac > 0 || other.frac != 0 {
                continue;
            }
            // An Encore that caught it this turn before it moved: the move it then used is
            // not the one it chose, and where it stood in the queue went by the one it chose.
            let at_start = if x_first { first } else { later };
            if at_start.encored && !self.encored[side][x.r.idx as usize] {
                continue;
            }
            // A Pokémon that has transformed or had its Speed swapped moves at a Speed its own
            // side cannot put a number to.
            if seen.iter().any(|o| o.r == other.r && o.transformed)
                || swapped[other.r.side as usize][other.r.idx as usize]
            {
                continue;
            }
            let known = know(s, listed, b.open_team_sheets);
            let belief = self.belief[side][s.listed as usize];
            // The ability it began the turn with, which it may have lost since.
            let (ability, changed, species) = self.began[side][x.r.idx as usize];
            let at_start = know_of(ability, changed, species, listed, b.open_team_sheets);
            let (Some(m), Some(p)) =
                (mods(s, &known, field, &belief), priority(s, &known, &at_start, field, x.move_id))
            else {
                continue;
            };
            if p != other.priority {
                continue;
            }
            if self.strict {
                let truth = b.mon(x.r);
                let raw = raw_speed(s.species, nature_class(truth.nature), truth.stat_points[5] as u32);
                let speed = m.speed(raw, class_of(s.true_item));
                assert!(
                    speed == x.speed && p == x.priority,
                    "a comparison read wrongly: {} worked out at speed {speed} and priority {p}, the queue had {} and {}\n{s:?}\n{field:?}",
                    SPECIES[s.species as usize].name,
                    x.speed,
                    x.priority
                );
            }
            let mut narrowed = belief;
            narrowed.retain(|n, points, k| {
                let speed = m.speed(raw_speed(s.species, n, points), k);
                if x_first { speed >= other.speed } else { speed <= other.speed }
            });
            if narrowed.is_empty() {
                assert!(!self.strict, "an order of moves that nothing allows: {:?}", events[i]);
                continue;
            }
            self.belief[side][s.listed as usize] = narrowed;
        }
    }

    /// At a decision: what the record of what has been shown says of each item, who is still on the field, and each Pokémon's own Speed now.
    fn settle(&mut self, b: &Battle) {
        for side in 0..2 {
            let s = &b.sides[side];
            // A live alias supersedes its filed record. In particular, a trade
            // made under Illusion must not be narrowed again by the copied
            // Pokémon's old item on the bench: the log cannot identify the trader.
            let live_listed = s
                .team
                .iter()
                .filter(|m| m.is_active && m.live.seen != 0)
                .filter(|m| (m.live.listed as usize) < MAX_ROSTER)
                .fold(0u8, |mask, m| mask | (1 << m.live.listed));
            // (Every place on the team, brought or not: a follower of Showdown's log keeps a
            // Pokémon it knows to be in disguise in one past the last it has seen.)
            for a in 0..MAX_TEAM {
                let m = &s.team[a];
                if !m.is_active {
                    self.swapped[side][a] = false;
                }
                if b.request == Request::Move {
                    // A turn begins. (Unknown and replaced, for one not on the field: nothing is assumed of it.)
                    let live = m.is_active && m.live.seen != 0;
                    self.began[side][a] = if live {
                        (
                            m.live.ability,
                            m.live.ability_changed,
                            seen(b, MonRef { side: side as u8, idx: a as u8 }).species,
                        )
                    } else {
                        (UNKNOWN, true, 0)
                    };
                    self.encored[side][a] =
                        m.is_active && b.vols(MonRef { side: side as u8, idx: a as u8 }).has(VolKind::Encore);
                }
                // The record of the Pokémon this one appears to be.
                let rec = if m.is_active && m.live.seen != 0 { &m.live } else { &s.shown[a] };
                if rec.seen == 0 || rec.suspect || rec.tainted || rec.listed as usize >= s.n_roster as usize {
                    continue;
                }
                if !(m.is_active && m.live.seen != 0) && live_listed & (1 << rec.listed) != 0 {
                    continue;
                }
                // The record of an Illusion Pokémon's own item goes stale when it trades items in
                // disguise (the trade is put down to the Pokémon it looked like). What its
                // item does is then left open.
                if b.shown_illusionists(side) & (1 << rec.listed) != 0 {
                    continue;
                }
                let class = match rec.item {
                    ItemShown::Unknown => continue,
                    ItemShown::Holds(item) => class_of(item),
                    ItemShown::Lost(_) => 0,
                };
                let mut kept = self.belief[side][rec.listed as usize];
                kept.keep_class(class);
                if !kept.is_empty() {
                    self.belief[side][rec.listed as usize] = kept;
                } else {
                    assert!(!self.strict, "an item on record that nothing seen allows");
                }
            }
        }
        // The item clause: a team has each item once. Once it is known which Pokémon was
        // registered with the Choice Scarf (or the Iron Ball), no other was. That holds for
        // every Pokémon still holding what it was registered with, or nothing.
        for side in 0..2 {
            if !self.clause[side] {
                continue;
            }
            for class in [SCARF, IRON_BALL] {
                for j in 0..MAX_ROSTER {
                    if !self.handed[side][j] && self.belief[side][j].items()[2 * class - 1] {
                        self.owner[side][class] = j as u8;
                    }
                }
                let owner = self.owner[side][class] as usize;
                if owner >= MAX_ROSTER {
                    continue;
                }
                for j in (0..MAX_ROSTER).filter(|&j| j != owner && !self.handed[side][j]) {
                    let mut kept = self.belief[side][j];
                    kept.drop_class(class);
                    if !kept.is_empty() {
                        self.belief[side][j] = kept;
                    } else {
                        assert!(!self.strict, "two Pokémon of a team with the same item, by what was seen");
                    }
                }
            }
        }
        // Speeds as the queue would have them, worked out on a copy: the battle itself is left as it is.
        let mut copy = *b;
        if !copy.ended {
            copy.update_speed();
        }
        for side in 0..2 {
            for pos in 0..ACTIVE.min(copy.sides[side].n as usize) {
                self.own[side][pos] = copy.mon(copy.active(side, pos)).speed;
            }
        }
    }

    /// Who moves first, as far as `view`'s side can tell: its Pokémon at
    /// `mine`, or the other side's at `theirs`, if both use moves of the same
    /// priority with things as they stand. `None` if either position is empty.
    ///
    /// This goes by Speed alone. A Quick Claw (or a Quick Draw) that may put a
    /// move ahead of a faster Pokémon's one time in five changes nothing here:
    /// its holder is treated as if it held nothing.
    pub fn first(&self, b: &Battle, view: usize, mine: usize, theirs: usize) -> Option<First> {
        let opp = 1 - view;
        if mine >= b.sides[view].n as usize || theirs >= b.sides[opp].n as usize {
            return None;
        }
        let me = b.mon(b.active(view, mine));
        let r = b.active(opp, theirs);
        let s = seen(b, r);
        if !me.is_active || me.fainted || !s.present {
            return None;
        }
        if me.transformed || self.swapped[view][b.active(view, mine).idx as usize] {
            // Its Speed is another Pokémon's, which its side has not been told.
            return Some(First::Unknown);
        }
        let roster = &b.sides[opp].roster[..b.sides[opp].n_roster as usize];
        let (Some(listed), false) = (roster.get(s.listed as usize), self.swapped[opp][r.idx as usize]) else {
            return Some(First::Unknown);
        };
        let belief = &self.belief[opp][s.listed as usize];
        let known = know(&s, listed, b.open_team_sheets);
        let Some(m) = mods(&s, &known, &field_seen(b), belief) else {
            return Some(First::Unknown);
        };
        let (mut slowest, mut fastest) = (i32::MAX, i32::MIN);
        for n in 0..NATURES {
            for k in 0..CLASSES {
                let bits = belief.bits[n][k];
                if bits != 0 {
                    // Speed rises with stat points, so the ends of each run are enough.
                    for points in [bits.trailing_zeros(), 63 - bits.leading_zeros()] {
                        let speed = m.speed(raw_speed(s.species, n, points), k);
                        (slowest, fastest) = (slowest.min(speed), fastest.max(speed));
                    }
                }
            }
        }
        let own = self.own[view][mine];
        Some(if slowest > own {
            First::Theirs
        } else if fastest < own {
            First::Mine
        } else {
            First::Unknown
        })
    }
}
