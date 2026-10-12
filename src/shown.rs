//! What a battle has shown about each Pokémon.
//!
//! The engine knows everything: every move, item, ability and exact HP on
//! both sides. A player does not. This module keeps, for every Pokémon, what
//! everyone watching the battle has been told about it, and
//! [`Battle::shown`] hands that out:
//!
//! ```
//! use vgc_engine::{Battle, Choice, PokemonSet};
//!
//! let set = |species: &str, moves: &[&str]| PokemonSet::from_names(species, moves, "Hardy", [0; 6]).unwrap();
//! let ours = [
//!     set("Incineroar", &["Fake Out", "Flare Blitz"]).ability("Intimidate").unwrap(),
//!     set("Garchomp", &["Earthquake", "Protect"]).item("Life Orb").unwrap(),
//!     set("Milotic", &["Scald"]),
//! ];
//! let theirs = [set("Sylveon", &["Hyper Voice"]), set("Arcanine", &["Protect"]), set("Snorlax", &["Body Slam"])];
//! let mut battle = Battle::new([&ours, &theirs], [1, 2, 3, 4]).unwrap();
//!
//! // Before anyone has moved: the leads are on the field, a third Pokémon has
//! // not been seen, and the only thing learned is the ability that announced itself.
//! let shown = battle.shown(0);
//! assert_eq!(shown.unseen, 1);
//! let incineroar = shown.active[0].as_ref().unwrap();
//! assert_eq!(incineroar.ability.as_deref(), Some("intimidate"));
//! assert!(incineroar.moves.is_empty());
//! let garchomp = shown.active[1].as_ref().unwrap();
//! assert_eq!((garchomp.item.as_deref(), garchomp.ability.as_deref()), (None, None));
//!
//! // Garchomp uses Earthquake: now that move is known, and so is the Life Orb that hurt it.
//! let protect = Choice::Move { slot: 0, target: 0, mega: false };
//! let fake_out = Choice::Move { slot: 0, target: 1, mega: false };
//! battle.choose([[fake_out, protect], [protect, protect]]).unwrap();
//! let shown = battle.shown(0);
//! let garchomp = shown.active[1].as_ref().unwrap();
//! assert_eq!(garchomp.moves, ["earthquake"]);
//! assert_eq!(garchomp.item.as_deref(), Some("lifeorb"));
//! ```
//!
//! "Shown" means exactly what Pokémon Showdown's battle log says, and the
//! engine is checked against a reader of that log ([`crate::observer`]) at
//! every decision of every recorded battle. What follows from that:
//!
//! * Nothing is worked out. A move that did less damage than it should have
//!   does not reveal an Assault Vest, a Pokémon that outsped another does not
//!   reveal a Choice Scarf, and a Frisk that found nothing does not reveal an
//!   empty hand. Only what the log names counts.
//! * Everything the log names counts, including lines a player's screen does
//!   not dwell on: in Champions a Regenerator heals with a line of its own
//!   when it leaves, and Natural Cure says what it cured.
//! * Under Illusion, what the disguised Pokémon shows is credited to the
//!   Pokémon it is disguised as, since that is who everyone thinks it is.
//!   When the disguise breaks, what was shown since it came in moves over to
//!   the Pokémon it really is. A disguise that is never broken is never
//!   corrected.
//! * HP is the percentage Showdown reports (rounded down, never 0 for a
//!   Pokémon that has not fainted), with the colour of the bar.
//! * In one place the plain reading would be wrong, and is not followed. A
//!   move borrowed with Copycat can carry on over the next turns (Uproar,
//!   Fly), and a line that then names it, because Disable or a Choice item
//!   stopped it, reads as if the move were the Pokémon's own. The first such
//!   line after a borrowing is passed over.
//!
//! What is public anyway is not repeated here: stat stages, volatile
//! conditions, the weather and each side's conditions are in the [`Battle`]
//! itself. (Their timers are not public, though: the turns a Reflect has
//! left give away a Light Clay.) PP is not tracked.
//!
//! # Before the battle: Team Preview and team sheets
//!
//! Some things are known before anything is shown. Team Preview lists the
//! species each player registered, and with open team sheets, which
//! Showdown's Champions formats offer and force in a best-of-three, each
//! player also has the other's sheet: every registered Pokémon's item,
//! ability, moves and nature. [`Battle::with_rosters`] says what was
//! registered and whether the sheets are open, and [`ShownSide::roster`]
//! hands it out. A Pokémon that has been seen points at the entry it appears
//! to be ([`ShownMon::listed`]).
//!
//! The sheet and the record are kept apart: the sheet is what a Pokémon came
//! with, the record what the battle has shown of it since. What stays hidden
//! with open sheets is the stat points, and which of the registered Pokémon
//! were brought.
//!
//! # Illusion
//!
//! "The entry it appears to be" is as far as it goes on a team with an
//! Illusion Pokémon, which comes in looking like a team-mate.
//! [`ShownSide::illusion`] lists the registered Pokémon that have Illusion,
//! as far as their opponent can know, and [`ShownMon::maybe_disguise`] marks
//! every Pokémon that might be one of them in disguise. The mark comes off
//! only when that is ruled out: the Illusion Pokémon has fainted, or has
//! stood on the field as itself beside the Pokémon in question. Nothing
//! cleverer is tried (a move that is on one sheet and not the other): that
//! is inference, and left to whoever reads the record.

use serde::{Deserialize, Serialize};

use crate::data::*;
use crate::state::*;

/// The most moves remembered for one Pokémon. Four are its own; more can pile
/// up on a Pokémon that an Illusion user has passed itself off as.
pub(crate) const SHOWN_MOVES: usize = 8;
/// "Not shown" for an ability.
pub(crate) const UNKNOWN: u16 = u16::MAX;

/// The colour of an HP bar: green above half, yellow above a fifth, red below.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Bar {
    #[default]
    Green,
    Yellow,
    Red,
}

/// What has been shown of a held item.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ItemShown {
    Unknown,
    Holds(u16),
    /// Holds nothing now, having lost this.
    Lost(u16),
}

/// What everyone has been shown about one Pokémon (as it appears: see the module notes on Illusion).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Shown {
    /// 0 until it has appeared; then 1 for the first Pokémon its side showed, 2 for the second, and so on.
    pub seen: u8,
    /// The species it was when last on the field under its own name (a Mega stays a Mega).
    pub species: u16,
    pub fainted: bool,
    /// HP and status as they were when it was last on the field. (While it is on the field they are read off the Pokémon.)
    pub hp: u8,
    pub bar: Bar,
    pub status: Status,
    pub moves: [u16; SHOWN_MOVES],
    pub n_moves: u8,
    pub item: ItemShown,
    /// The ability it has now and the one it returns to on leaving the field; `UNKNOWN` if not shown.
    pub ability: u16,
    pub base_ability: u16,
    /// Its ability has been changed since it came in, so the next one shown is not the one it returns to.
    pub ability_changed: bool,
    /// On the field: the move it last used that was not its own (Copycat's pick),
    /// until it is seen to use anything else; `NO_MOVE` otherwise. See [`Shown::use_move`].
    pub borrowed: u16,
    /// The entry of its side's registered team it appears to be (`NOT_LISTED`: none fits).
    pub listed: u8,
    /// On the field now, and it may be the side's Illusion Pokémon in this shape.
    pub suspect: bool,
    /// It left the field while that was still open, so part of what is noted
    /// here may be about the Illusion Pokémon instead.
    pub tainted: bool,
    /// Baton Pass copied suppression; a hidden unsuppressible ability may
    /// silently remove it without giving the opponent that information.
    pub copied_gastro: bool,
}

pub(crate) const NOTHING_SHOWN: Shown = Shown {
    seen: 0,
    species: NO_SPECIES,
    fainted: false,
    hp: 100,
    bar: Bar::Green,
    status: Status::None,
    moves: [NO_MOVE; SHOWN_MOVES],
    n_moves: 0,
    item: ItemShown::Unknown,
    ability: UNKNOWN,
    base_ability: UNKNOWN,
    ability_changed: false,
    borrowed: NO_MOVE,
    listed: NOT_LISTED,
    suspect: false,
    tainted: false,
    copied_gastro: false,
};

impl Shown {
    pub(crate) fn moves(&self) -> &[u16] {
        &self.moves[..self.n_moves as usize]
    }

    pub(crate) fn add_move(&mut self, id: u16) {
        if self.moves().contains(&id) || self.n_moves as usize == SHOWN_MOVES {
            return;
        }
        self.moves[self.n_moves as usize] = id;
        self.n_moves += 1;
    }

    /// A line has it using `id` as a move of its own, or failing to: a `move`
    /// line that is not put down to another move, or a `cant` line.
    ///
    /// That shows the move, with one exception. A move it borrowed (Copycat
    /// picking Uproar, or Fly) can carry on over the following turns, and
    /// when something then stops it, the line that says so reads like any
    /// other: `|cant|p1a: A|Disable|Uproar`, or, from a Choice item,
    /// `|move|p1a: A|Fly||[still]`. So the first such line to name the move it
    /// last borrowed shows nothing. (If the move was its own as well, the
    /// next use shows it.)
    pub(crate) fn use_move(&mut self, id: u16) {
        let carried_on = self.borrowed == id;
        self.borrowed = NO_MOVE;
        if !carried_on {
            self.add_move(id);
        }
    }

    /// A line has it using `id` on another move's account (`[from] move: Copycat`).
    pub(crate) fn borrow_move(&mut self, id: u16) {
        self.borrowed = id;
    }

    /// A line credits it with `ability`.
    pub(crate) fn set_ability(&mut self, ability: u16) {
        // Unless its ability has been replaced since it came in, and by what is known: then
        // this is the old one still finishing what it had begun. (Poison Touch poisons on
        // the second hit of a Double Hit whose first swapped it for Wandering Spirit.)
        if self.ability_changed && self.ability != UNKNOWN && self.ability != ability {
            return;
        }
        self.ability = ability;
        if !self.ability_changed {
            self.base_ability = ability;
        }
    }

    /// The ability has been replaced; `ability` is the new one if that was shown.
    pub(crate) fn change_ability(&mut self, ability: u16) {
        self.ability = ability;
        self.ability_changed = true;
    }

    /// `-end|pokemon|Illusion`, after its disguise has dropped: Illusion is what
    /// was keeping the disguise up. Unless the Pokémon is a Mega: then the
    /// disguise dropped because Mega Evolution replaced Illusion for good.
    /// (No Pokémon that may have Illusion has a Mega Stone under the regulation.)
    pub(crate) fn end_illusion(&mut self) {
        let mega = ITEMS.iter().flat_map(|i| i.mega.iter()).any(|&(_, to)| to == self.species);
        if !mega {
            self.set_ability(ab::ILLUSION);
        }
    }

    /// Leaving the field: the ability goes back to its own. And if it was
    /// never settled whether this was the Pokémon it looked like, it never will be.
    pub(crate) fn leave(&mut self) {
        self.copied_gastro = false;
        self.ability = self.base_ability;
        self.ability_changed = false;
        self.borrowed = NO_MOVE;
        self.tainted |= self.suspect;
        self.suspect = false;
    }

    /// What `self` has that `before` did not: what was learned in between.
    pub(crate) fn learned_since(&self, before: &Shown, into: &mut Shown) {
        for &m in self.moves() {
            if !before.moves().contains(&m) {
                into.add_move(m);
            }
        }
        if self.item != before.item {
            into.item = self.item;
        }
        if self.base_ability != before.base_ability {
            into.base_ability = self.base_ability;
        }
        if self.ability != before.base_ability || self.ability_changed {
            into.ability = self.ability;
            into.ability_changed = self.ability_changed;
        }
    }
}

/// Which entries of a registered team have Illusion, as far as their opponent
/// can tell, as a bit mask. With open team sheets the sheet says. Without, it
/// is the Pokémon whose species may have it under the regulation (Zoroark):
/// a team is taken to be a legal one.
pub(crate) fn illusionists(roster: &[Listed], open: bool) -> u8 {
    let mut mask = 0;
    for (j, listed) in roster.iter().enumerate() {
        let has = if open {
            listed.ability == ab::ILLUSION
        } else {
            let rule = crate::format::Format::current().rule(listed.species);
            rule.is_some_and(|r| r.abilities.contains(&ab::ILLUSION))
        };
        if has {
            mask |= 1 << j;
        }
    }
    mask
}

/// The entry of a registered team that a Pokémon appearing as `species` is
/// taken for: the first one of that species (and gender, if one fits) that no
/// other Pokémon of the side has been taken for (`taken`, a mask of entries).
pub(crate) fn listed_as(roster: &[Listed], species: u16, gender: Gender, taken: u8) -> u8 {
    let base = |sp: u16| SPECIES[sp as usize].base_species;
    for pass in 0..4 {
        for (j, listed) in roster.iter().enumerate() {
            if taken & (1 << j) != 0 || listed.species == NO_SPECIES {
                continue;
            }
            let same = listed.species == species || (listed.any_forme && base(listed.species) == base(species));
            let fits = match pass {
                0 => same && listed.gender == gender,
                1 => same,
                2 => base(listed.species) == base(species) && listed.gender == gender,
                _ => base(listed.species) == base(species),
            };
            if fits {
                return j as u8;
            }
        }
    }
    NOT_LISTED
}

/// Whether a team's Illusion Pokémon cannot be told apart: there is more than
/// one, or there are two of the same Pokémon and one of them has it. (Neither
/// is a team the regulation allows.) Then every Pokémon of the team stays in doubt.
pub(crate) fn muddled(roster: &[Listed], illusionists: u8) -> bool {
    match illusionists.count_ones() {
        0 => false,
        1 => {
            let base = |l: &Listed| SPECIES[l.species as usize].base_species;
            let species = base(&roster[illusionists.trailing_zeros() as usize]);
            roster.iter().filter(|l| l.species != NO_SPECIES && base(l) == species).count() > 1
        }
        _ => true,
    }
}

/// Whether a Pokémon that looks like entry `listed` of its side's registered
/// team (`roster`) may be an Illusion Pokémon in that shape. `illusionists`
/// is the mask of entries that have Illusion; `accounted` says whether the
/// only such Pokémon is known to be somewhere else: fainted, or on the field
/// as itself.
pub(crate) fn may_be_disguise(
    roster: &[Listed],
    illusionists: u8,
    listed: u8,
    accounted: impl FnOnce(u8) -> bool,
) -> bool {
    if illusionists == 0 {
        return false;
    }
    if muddled(roster, illusionists) {
        return true;
    }
    let only = illusionists.trailing_zeros() as u8;
    // It looks like the Illusion Pokémon itself: a disguise is always someone else.
    listed != only && !accounted(only)
}

/// What `-mega|pokemon|Species|Stone` says about the ability: a Mega has
/// just the one, and the stone and the species name the Mega. (Under
/// Illusion the species named is the disguise's; then the stone alone has to
/// do, and if it makes Megas with different abilities the line says nothing.)
pub(crate) fn mega_ability(stone: u16, named: &str) -> u16 {
    let pairs = ITEMS[stone as usize].mega;
    let fits = |from: u16| SPECIES[from as usize].base_species == named || SPECIES[from as usize].id == named;
    let any_fit = pairs.iter().any(|&(from, _)| fits(from));
    let mut ability = UNKNOWN;
    for &(from, to) in pairs {
        if any_fit && !fits(from) {
            continue;
        }
        let a = SPECIES[to as usize].ability0;
        if ability != UNKNOWN && ability != a {
            return UNKNOWN;
        }
        ability = a;
    }
    ability
}

/// HP as Showdown reports it to the other player in Champions: a whole
/// percentage, rounded down but never to 0, and the colour of the bar.
pub(crate) fn hp_shown(hp: u16, max: u16) -> (u8, Bar) {
    if hp == 0 || max == 0 {
        return (0, Bar::Red);
    }
    let (hp, max) = (hp as u32, max as u32);
    let pct = (100 * hp / max).max(1);
    let bar = if hp * 2 > max {
        Bar::Green
    } else if hp * 5 > max {
        Bar::Yellow
    } else {
        Bar::Red
    };
    (pct as u8, bar)
}

/// One Pokémon as everyone watching knows it.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ShownMon {
    /// A handle that stays with this Pokémon for the whole battle: 1 for the
    /// first Pokémon its side showed, 2 for the second, and so on. (It says
    /// nothing about the order the team was brought in.)
    pub id: u8,
    /// The species on show, as an id: the Mega after Mega Evolution, the
    /// copied Pokémon after Transform, the disguise under Illusion.
    pub species: String,
    /// "M", "F" or "N".
    pub gender: String,
    /// HP as a whole percentage, rounded down; 0 only once fainted. For a
    /// Pokémon on the bench, as it was when it left the field.
    pub hp: u8,
    pub bar: Bar,
    /// "brn", "par", "psn", "tox", "slp", "frz" or empty.
    pub status: String,
    pub fainted: bool,
    /// Moves it has been seen to have, as ids, in the order they came to light.
    pub moves: Vec<String>,
    /// Its item if that has been shown: an id, or empty for "holds nothing". `None`: not shown.
    pub item: Option<String>,
    /// The item it is known to have lost (used, eaten, knocked off, stolen); empty if none.
    pub item_lost: String,
    /// The ability it has now, if shown.
    pub ability: Option<String>,
    /// The ability it returns to when it leaves the field, if shown.
    pub base_ability: Option<String>,
    /// It has taken another Pokémon's shape (and `species` is that Pokémon's).
    pub transformed: bool,
    /// The Pokémon of the registered team ([`ShownSide::roster`]) it appears
    /// to be, as an index; `None` if none fits.
    pub listed: Option<u8>,
    /// Its side has a Pokémon with Illusion, and this may be that Pokémon in
    /// disguise, or part of what is noted here may be about that Pokémon
    /// from a time it was. `false` means it is who it appears to be: its side
    /// has no Illusion Pokémon, or that one is accounted for (it has
    /// fainted, or it stood on the field as itself beside this one).
    pub maybe_disguise: bool,
}

/// One Pokémon of the team a player registered, as its opponent knows it.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ListedMon {
    /// As Team Preview showed it, as an id.
    pub species: String,
    /// "M", "F" or "N".
    pub gender: String,
    /// Its team sheet, when the sheets are open.
    pub sheet: Option<Sheet>,
    /// With closed sheets: moves its opponent has worked out it knows without
    /// seeing it use them, as ids. They are moves of the opponent's own that its
    /// Imprison sealed, which is something only that player is told.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub told: Vec<String>,
}

/// What an open team sheet says of one Pokémon besides its species: all but its stat points.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Sheet {
    /// An id; empty for no item.
    pub item: String,
    pub ability: String,
    pub moves: Vec<String>,
    /// An id. All five natures that change nothing are given as "hardy".
    pub nature: String,
}

/// One side of the field as everyone watching knows it.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ShownSide {
    /// Who is in each position on the field; `None` for a position whose Pokémon has fainted.
    pub active: Vec<Option<ShownMon>>,
    /// The Pokémon that have been on the field and are not now, in the order they first appeared.
    pub bench: Vec<ShownMon>,
    /// How many of the Pokémon brought have not appeared yet.
    pub unseen: u8,
    /// How many have not fainted.
    pub left: u8,
    /// The team this side registered, in the order registered: the species
    /// Team Preview showed and, with open team sheets, each one's sheet. It
    /// says nothing of which were brought. A Pokémon that has been seen
    /// points here with [`ShownMon::listed`].
    pub roster: Vec<ListedMon>,
    /// The Pokémon of `roster` that have Illusion, by index. With open team
    /// sheets that is what the sheets say. Without, it is those whose species
    /// can have it under the regulation (a Zoroark, which Team Preview
    /// shows): the team is taken to be a legal one.
    pub illusion: Vec<u8>,
}

impl ShownSide {
    /// The sheet of the Pokémon `mon` appears to be, with open team sheets.
    /// (Mind [`ShownMon::maybe_disguise`].)
    pub fn sheet_of(&self, mon: &ShownMon) -> Option<&Sheet> {
        self.roster.get(mon.listed? as usize)?.sheet.as_ref()
    }
}

/// A side's registered team as its opponent knows it.
pub(crate) fn roster_shown(roster: &[Listed], open: bool) -> Vec<ListedMon> {
    let listed = |l: &Listed| ListedMon {
        species: SPECIES[l.species as usize].id.to_string(),
        gender: l.gender.id().to_string(),
        sheet: open.then(|| Sheet {
            item: if l.item == it::NONE { String::new() } else { ITEMS[l.item as usize].id.to_string() },
            ability: ABILITIES[l.ability as usize].id.to_string(),
            moves: l.moves().iter().map(|&m| MOVES[m as usize].id.to_string()).collect(),
            nature: crate::position::nature_name(l.nature).to_string(),
        }),
        told: l.told().iter().map(|&m| MOVES[m as usize].id.to_string()).collect(),
    };
    roster.iter().map(listed).collect()
}

/// A bit mask of roster entries as a list of indices.
pub(crate) fn mask_list(mask: u8) -> Vec<u8> {
    (0..8).filter(|j| mask & (1 << j) != 0).collect()
}

pub(crate) fn ability_name(a: u16) -> Option<String> {
    (a != UNKNOWN).then(|| if a == ab::NOABILITY { String::new() } else { ABILITIES[a as usize].id.to_string() })
}

impl ShownMon {
    pub(crate) fn from_record(rec: &Shown, species: u16, gender: Gender) -> ShownMon {
        let (item, item_lost) = match rec.item {
            ItemShown::Unknown => (None, String::new()),
            ItemShown::Holds(i) => (Some(ITEMS[i as usize].id.to_string()), String::new()),
            ItemShown::Lost(i) => (Some(String::new()), ITEMS[i as usize].id.to_string()),
        };
        ShownMon {
            id: rec.seen,
            species: SPECIES[species as usize].id.to_string(),
            gender: gender.id().to_string(),
            hp: rec.hp,
            bar: rec.bar,
            status: rec.status.id().to_string(),
            fainted: rec.fainted,
            moves: rec.moves().iter().map(|&m| MOVES[m as usize].id.to_string()).collect(),
            item,
            item_lost,
            ability: ability_name(rec.ability),
            base_ability: ability_name(rec.base_ability),
            transformed: false,
            listed: (rec.listed != NOT_LISTED).then_some(rec.listed),
            maybe_disguise: rec.suspect || rec.tainted,
        }
    }
}

impl Battle {
    /// The roster entry of the one Pokémon of `side` on the field with Imprison up, if
    /// what it seals can be put down to it: there is just the one, it is who it looks
    /// like, and its moves are its own.
    pub(crate) fn imprisoner(&self, side: usize) -> Option<usize> {
        let s = &self.sides[side];
        let mut found = None;
        for pos in 0..ACTIVE.min(s.n as usize) {
            let r = self.active(side, pos);
            if self.has_live(r) && !self.mon(r).fainted && self.vols(r).has(VolKind::Imprison) {
                if found.is_some() {
                    return None;
                }
                found = Some(r);
            }
        }
        let m = self.mon(found?);
        let rec = &m.live;
        let own = !m.transformed && !rec.suspect && !rec.tainted && (rec.listed as usize) < s.n_roster as usize;
        own.then_some(rec.listed as usize)
    }

    /// What `viewer`'s side can work out about the other's moves from its own move
    /// request, when the sheets are closed: a move of its own that it is told it
    /// cannot choose, with nothing in plain sight to account for that, has been
    /// sealed by an opposing Imprison, and whoever has Imprison up knows the move.
    /// Each with that Pokémon's roster entry.
    ///
    /// A player is told so for all but the last of its Pokémon to choose (Showdown
    /// keeps that one in the dark). And it is only put down to Imprison where no
    /// other condition on the Pokémon could be behind it, whichever move that
    /// condition bars: nothing here depends on what is not on show.
    /// `strict`: this is the game itself, which can check the conclusion.
    pub(crate) fn sealed_moves(&self, viewer: usize, strict: bool) -> Vec<(usize, u16)> {
        let mut out = Vec::new();
        if self.open_team_sheets || self.request != Request::Move {
            return out;
        }
        let Some(entry) = self.imprisoner(1 - viewer) else { return out };
        let gravity = self.field.pseudo.has(Pseudo::Gravity);
        for pos in 0..ACTIVE.min(self.sides[viewer].n as usize) {
            let r = self.active(viewer, pos);
            let (m, vols) = (self.mon(r), self.vols(r));
            if !m.is_active || m.fainted || m.locked_move != NO_MOVE || !self.usable_moves(r) {
                continue;
            }
            // What bars one move in particular, whichever it is.
            let held = [VolKind::Disable, VolKind::Encore, VolKind::Torment].iter().any(|&k| vols.has(k))
                || ITEMS[m.item as usize].flags & IF_CHOICE != 0;
            if held {
                continue;
            }
            let last = crate::obs::last_to_choose(self, viewer, pos);
            for slot in &m.moves[..m.n_moves as usize] {
                let d = &MOVES[slot.id as usize];
                let told = slot.disabled && slot.pp > 0 && !(slot.hidden && last);
                let accounted = (vols.has(VolKind::Taunt) && d.category == Category::Status)
                    || (vols.has(VolKind::Healblock) && d.flags & F_HEAL != 0)
                    || (vols.has(VolKind::Throatchop) && d.flags & F_SOUND != 0)
                    || (gravity && d.flags & F_GRAVITY != 0)
                    || d.events & Ev::DisableMove.bit() != 0
                    || d.flags & F_CANTUSETWICE != 0;
                if told && !accounted {
                    debug_assert!(!strict || slot.hidden, "{} is barred, and not by Imprison", d.name);
                    out.push((entry, slot.id));
                }
            }
        }
        out
    }

    /// At a move request: each side notes what [`Battle::sealed_moves`] tells it.
    pub(crate) fn note_sealed(&mut self) {
        for viewer in 0..2 {
            for (entry, sealed) in self.sealed_moves(viewer, true) {
                self.sides[1 - viewer].roster[entry].tell(sealed);
            }
        }
    }

    /// The team index of the Pokémon `r` passes for (itself, unless Illusion says otherwise).
    pub(crate) fn shown_as(&self, r: MonRef) -> usize {
        let m = self.mon(r);
        if m.illusion != 0 { m.illusion as usize - 1 } else { r.idx as usize }
    }

    /// Whether `r` is on the field and has not fainted, as far as the log has said.
    pub(crate) fn shown_live(&self, r: MonRef) -> bool {
        self.has_live(r)
    }

    /// Whether `r` is on the field with a record of its own for this stay.
    fn has_live(&self, r: MonRef) -> bool {
        let m = self.mon(r);
        m.is_active && m.live.seen != 0
    }

    /// The record that something shown about `r` right now is written to.
    pub(crate) fn shown_mut(&mut self, r: MonRef) -> &mut Shown {
        if self.has_live(r) {
            &mut self.mon_mut(r).live
        } else {
            let a = self.shown_as(r);
            &mut self.sides[r.side as usize].shown[a]
        }
    }

    /// Changes the record of what `r` has shown. (With the `trace` feature,
    /// counts the use of the place this was asked from: see `trace::shown_sites`.)
    #[track_caller]
    fn show(&mut self, r: MonRef, what: &'static str, change: impl FnOnce(&mut Shown)) {
        let rec = self.shown_mut(r);
        #[cfg(feature = "trace")]
        let before = *rec;
        change(rec);
        #[cfg(feature = "trace")]
        crate::trace::shown_site(std::panic::Location::caller(), what, before != *rec);
        let _ = what;
    }

    /// `r` has been seen to have this move. (Not while it is transformed:
    /// those are the moves of the Pokémon it copied.)
    #[track_caller]
    pub(crate) fn show_move(&mut self, r: MonRef, move_id: u16) {
        if move_id == NO_MOVE || move_id == crate::battle::struggle_id() || self.mon(r).transformed {
            return;
        }
        self.show(r, MOVES[move_id as usize].id, |rec| rec.add_move(move_id));
    }

    /// `r` has used this move as its own, or been stopped from using it: see [`Shown::use_move`].
    #[track_caller]
    pub(crate) fn show_move_used(&mut self, r: MonRef, move_id: u16) {
        if move_id == NO_MOVE {
            return;
        }
        self.note_move_named(r, move_id);
        if move_id == crate::battle::struggle_id() || self.mon(r).transformed {
            return;
        }
        self.show(r, MOVES[move_id as usize].id, |rec| rec.use_move(move_id));
    }

    /// `r` has used this move on another move's account (Copycat's pick).
    pub(crate) fn show_move_borrowed(&mut self, r: MonRef, move_id: u16) {
        if move_id != NO_MOVE && !self.mon(r).transformed {
            self.shown_mut(r).borrow_move(move_id);
        }
    }

    /// `r`'s ability has been named.
    #[track_caller]
    pub(crate) fn show_ability(&mut self, r: MonRef, ability: u16) {
        self.show(r, ABILITIES[ability as usize].id, |rec| rec.set_ability(ability));
    }

    /// The ability the running callback belongs to has been named, on its holder.
    #[track_caller]
    pub(crate) fn show_own_ability(&mut self) {
        if let (Eff::Ability(a), Some(Holder::Mon(h))) = (self.effect, self.effect_holder) {
            self.show_ability(h, a);
        }
    }

    /// `r`'s ability was replaced in plain sight; `ability` is the new one, or `UNKNOWN`.
    #[track_caller]
    pub(crate) fn show_ability_change(&mut self, r: MonRef, ability: u16) {
        self.show(r, "", |rec| rec.change_ability(ability));
    }

    /// A line names `item` as `r`'s own (`[from] item: X`, `item: X`).
    #[track_caller]
    pub(crate) fn show_item(&mut self, r: MonRef, item: u16) {
        self.show(r, ITEMS[item as usize].id, |rec| rec.name_item(item));
    }

    /// `r` has been shown to come by `item` (`-item`).
    #[track_caller]
    pub(crate) fn show_item_gain(&mut self, r: MonRef, item: u16) {
        if item != it::NONE {
            self.note_item(r, item, false);
            self.show(r, ITEMS[item as usize].id, |rec| rec.item = ItemShown::Holds(item));
        }
    }

    /// `r` has been shown to hold `item`, which it held already: Frisk has found
    /// it, or it has announced itself. (Nothing has changed hands, so there is
    /// nothing to note for [`crate::speed`].)
    #[track_caller]
    pub(crate) fn show_item_held(&mut self, r: MonRef, item: u16) {
        if item != it::NONE {
            self.show(r, ITEMS[item as usize].id, |rec| rec.item = ItemShown::Holds(item));
        }
    }

    /// The line that says `item` has reached `r` comes after it was handed over,
    /// and something may have happened to it in between (a White Herb used at
    /// once has had its own `-enditem`): then that stands.
    #[track_caller]
    pub(crate) fn show_item_arrived(&mut self, r: MonRef, item: u16) {
        // (Noted even if it is gone again already: a White Herb handed to a Pokémon with a
        // lowered stat is used up before the log gets to say it arrived.)
        self.note_item(r, item, false);
        if self.mon(r).item == item {
            self.show_item_gain(r, item);
        }
    }

    /// `r` has been shown to lose `item`, and holds nothing now.
    #[track_caller]
    pub(crate) fn show_item_lost(&mut self, r: MonRef, item: u16) {
        if item != it::NONE {
            self.note_item(r, item, true);
            self.show(r, ITEMS[item as usize].id, |rec| rec.item = ItemShown::Lost(item));
        }
    }

    /// A log line with `[from] effect`, as Showdown's `damage`, `heal` and the
    /// status messages write them: the ability or item is shown on whoever has
    /// it. That is the holder of the running callback when the effect is its
    /// own; for an effect named from elsewhere, the Pokémon the line points at
    /// (`[of] source` if it has one, else its subject).
    #[track_caller]
    pub(crate) fn show_from(&mut self, effect: Eff, target: MonRef, of: Option<MonRef>) {
        let holder = match self.effect_holder {
            Some(Holder::Mon(h)) if effect == self.effect => h,
            _ => of.unwrap_or(target),
        };
        match effect {
            Eff::Ability(a) => self.show_ability(holder, a),
            Eff::Item(i) => self.show_item(holder, i),
            _ => {}
        }
    }

    /// The `switch` or `drag` line: `r` appears, as whoever it passes for.
    pub(crate) fn shown_enter(&mut self, r: MonRef) {
        let a = self.shown_as(r);
        let side = &mut self.sides[r.side as usize];
        if side.shown[a].seen == 0 {
            side.n_seen += 1;
            side.shown[a].seen = side.n_seen;
        }
        side.shown[a].species = side.team[a].species;
        if side.shown[a].listed == NOT_LISTED {
            let (species, gender) = (side.team[a].species, side.team[a].gender);
            side.shown[a].listed = listed_as(&side.roster[..side.n_roster as usize], species, gender, side.taken());
        }
        let mut live = side.shown[a];
        live.fainted = false;
        live.leave();
        self.mon_mut(r).live = live;
        self.mon_mut(r).live.suspect = self.shown_suspect(r);
        self.shown_recheck(r.side as usize);
    }

    /// The entries of `side`'s registered team that have Illusion, as far as the other side can tell.
    pub(crate) fn shown_illusionists(&self, side: usize) -> u8 {
        let s = &self.sides[side];
        illusionists(&s.roster[..s.n_roster as usize], self.open_team_sheets)
    }

    /// Whether `r`, on the field, may be its side's Illusion Pokémon in the shape it shows.
    fn shown_suspect(&self, r: MonRef) -> bool {
        let s = &self.sides[r.side as usize];
        let listed = self.mon(r).live.listed;
        may_be_disguise(&s.roster[..s.n_roster as usize], self.shown_illusionists(r.side as usize), listed, |only| {
            let fainted =
                (0..s.n as usize).any(|a| s.shown[a].seen != 0 && s.shown[a].listed == only && s.shown[a].fainted);
            let beside = (0..s.n as usize).any(|a| {
                a != r.idx as usize && s.team[a].is_active && s.team[a].live.seen != 0 && s.team[a].live.listed == only
            });
            fainted || beside
        })
    }

    /// Something has happened that can settle who is who on `side`: a Pokémon
    /// came in, fainted or dropped a disguise.
    /// Whoever is on the field and need no longer be doubted is cleared.
    pub(crate) fn shown_recheck(&mut self, side: usize) {
        for pos in 0..ACTIVE.min(self.sides[side].n as usize) {
            let r = MonRef { side: side as u8, idx: self.sides[side].order[pos] };
            if self.has_live(r) && self.mon(r).live.suspect && !self.shown_suspect(r) {
                self.mon_mut(r).live.suspect = false;
            }
        }
    }

    /// HP and status of `r` as the log has them now.
    pub(crate) fn shown_condition(&self, r: MonRef) -> (u8, Bar, Status) {
        let m = self.mon(r);
        let (hp, bar) = hp_shown(m.hp, m.max_hp());
        (hp, bar, if m.hp == 0 { Status::None } else { m.status })
    }

    /// `r` leaves the field (called back, dragged out or fainted): what was
    /// shown during its stay is filed under the Pokémon it passed for.
    pub(crate) fn shown_leave(&mut self, r: MonRef, fainted: bool) {
        if !self.has_live(r) {
            return;
        }
        let (hp, bar, status) = self.shown_condition(r);
        let a = self.shown_as(r);
        let mut live = self.mon(r).live;
        (live.hp, live.bar, live.status, live.fainted) = (hp, bar, status, fainted);
        if fainted {
            (live.hp, live.bar, live.status) = (0, Bar::Red, Status::None);
        }
        live.leave();
        // Its place in the order of appearance belongs to the name, which another Pokémon may share.
        live.seen = self.sides[r.side as usize].shown[a].seen;
        self.sides[r.side as usize].shown[a] = live;
        self.mon_mut(r).live = NOTHING_SHOWN;
        if fainted {
            self.shown_recheck(r.side as usize);
        }
    }

    /// Illusion ends (`replace`): the Pokémon in this position is, and has
    /// been since it came in, someone else. `was` is the team index it passed for.
    pub(crate) fn shown_unmask(&mut self, r: MonRef, was: usize) {
        if !self.has_live(r) {
            return;
        }
        let side = &mut self.sides[r.side as usize];
        let live = side.team[r.idx as usize].live;
        let before = side.shown[was];
        if side.shown[r.idx as usize].seen == 0 {
            side.n_seen += 1;
            side.shown[r.idx as usize].seen = side.n_seen;
        }
        side.shown[r.idx as usize].species = side.team[r.idx as usize].species;
        if side.shown[r.idx as usize].listed == NOT_LISTED {
            let (species, gender) = (side.team[r.idx as usize].species, side.team[r.idx as usize].gender);
            let taken = side.taken();
            side.shown[r.idx as usize].listed =
                listed_as(&side.roster[..side.n_roster as usize], species, gender, taken);
        }
        let mut own = side.shown[r.idx as usize];
        own.fainted = false;
        own.leave();
        live.learned_since(&before, &mut own);
        // Whatever it looked like, this is what it is. (But where the Illusion Pokémon
        // cannot be told apart, what was noted under the name it used may be another's.)
        own.suspect = false;
        side.team[r.idx as usize].live = own;
        if muddled(
            &self.sides[r.side as usize].roster[..self.sides[r.side as usize].n_roster as usize],
            self.shown_illusionists(r.side as usize),
        ) {
            self.mon_mut(r).live.tainted = true;
        }
        self.shown_recheck(r.side as usize);
    }

    /// `-end|pokemon|Illusion`: see [`Shown::end_illusion`].
    #[track_caller]
    pub(crate) fn show_illusion_ended(&mut self, r: MonRef) {
        self.show(r, "illusion", Shown::end_illusion);
    }

    /// A benched (or fainted) Pokémon's HP or status changed in plain sight.
    pub(crate) fn shown_bench_update(&mut self, r: MonRef) {
        if self.has_live(r) {
            return;
        }
        let (hp, bar, status) = self.shown_condition(r);
        let fainted = self.mon(r).fainted;
        let a = self.shown_as(r);
        let side = &mut self.sides[r.side as usize];
        if side.shown[a].seen == 0 {
            side.n_seen += 1;
            side.shown[a].seen = side.n_seen;
        }
        let species = side.team[a].species;
        let rec = &mut side.shown[a];
        if rec.species == NO_SPECIES {
            rec.species = species;
        }
        (rec.hp, rec.bar, rec.status, rec.fainted) = (hp, bar, status, fainted);
    }

    /// `detailschange`: `r` has changed forme for good. (Under Illusion the line shows the disguise, unchanged.)
    pub(crate) fn shown_new_forme(&mut self, r: MonRef, species: u16) {
        if self.mon(r).illusion == 0 {
            self.shown_mut(r).species = species;
        }
    }

    /// What everyone watching has been shown about `side`'s Pokémon.
    pub fn shown(&self, side: usize) -> ShownSide {
        let s = &self.sides[side];
        let mut out = ShownSide { left: s.pokemon_left, ..Default::default() };
        let mut on_field = [false; MAX_TEAM];
        for pos in 0..ACTIVE.min(s.n as usize) {
            let r = MonRef { side: side as u8, idx: s.order[pos] };
            if !self.has_live(r) {
                out.active.push(None);
                continue;
            }
            let m = self.mon(r);
            let a = self.shown_as(r);
            on_field[a] = true;
            // A disguise is the Pokémon it copies as that one looked when the disguise came in.
            let species = if m.illusion != 0 { m.live.species } else { m.species };
            let mut mon = ShownMon::from_record(&m.live, species, s.team[a].gender);
            let (hp, bar, status) = self.shown_condition(r);
            (mon.hp, mon.bar, mon.status) = (hp, bar, status.id().to_string());
            mon.fainted = false;
            mon.transformed = m.transformed;
            out.active.push(Some(mon));
        }
        while out.active.len() < ACTIVE {
            out.active.push(None);
        }
        let mut bench = Vec::new();
        for (a, rec) in s.shown.iter().enumerate().take(s.n as usize) {
            if rec.seen == 0 {
                out.unseen += 1;
            } else if !on_field[a] {
                bench.push(ShownMon::from_record(rec, rec.species, s.team[a].gender));
            }
        }
        bench.sort_by_key(|m| m.id);
        out.bench = bench;
        out.roster = roster_shown(&s.roster[..s.n_roster as usize], self.open_team_sheets);
        out.illusion = mask_list(self.shown_illusionists(side));
        out
    }

    /// Whether the players have each other's team sheets ([`Battle::with_rosters`]).
    pub fn open_team_sheets(&self) -> bool {
        self.open_team_sheets
    }
}
