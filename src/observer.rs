//! Reads Pokémon Showdown's battle log and keeps track of what it has shown
//! about each Pokémon: the same knowledge [`Battle::shown`](crate::Battle::shown)
//! reports, worked out from the text a player receives instead of from the
//! simulator's state.
//!
//! It has two uses. Recorded battles carry Showdown's log, and at every
//! decision the engine's idea of what has been shown must equal what this
//! reader makes of the log (`difftest --shown`). And a program playing on
//! Showdown has only the log to go by: this is the reader it needs.
//!
//! ```
//! use vgc_engine::observer::Observer;
//!
//! let mut seen = Observer::new();
//! for line in [
//!     "|switch|p1a: Rex|Garchomp, L50, M|100/100",
//!     "|switch|p2a: Milotic|Milotic, L50, F|100/100",
//!     "|move|p1a: Rex|Earthquake|p2a: Milotic",
//!     "|-damage|p2a: Milotic|41/100",
//!     "|-damage|p1a: Rex|90/100|[from] item: Life Orb",
//! ] {
//!     seen.line(line).unwrap();
//! }
//! let rex = seen.shown(0).active[0].clone().unwrap();
//! assert_eq!((rex.moves, rex.item.as_deref(), rex.hp), (vec!["earthquake".to_string()], Some("lifeorb"), 90));
//! ```
//!
//! Given the log from its first line, it also takes in Team Preview (`|poke|`)
//! and open team sheets (`|showteam|`): see [`ShownSide::roster`].
//!
//! Lines are the ones every player and spectator receives. Where Showdown
//! sends a private and a public version of a line (`|split|`), give this the
//! public one, or hand the raw log to [`Observer::lines`], which picks it.
//!
//! # Who a line is about
//!
//! A line that names an ability or an item also names one or two Pokémon,
//! and Showdown has no single rule for which of them has it:
//! `|-damage|p1a: A|…|[from] item: Rocky Helmet|[of] p2a: B` is B's helmet,
//! while `|-heal|p1a: A|…|[from] item: Shell Bell|[of] p2a: B` is A's bell.
//! [`holder`] is the table of which is which. It was drawn up by asking
//! Showdown itself, on every such line of some 20,000 battles, which of the
//! Pokémon mentioned really had the thing (`oracle/gen_cases.js --holders`).

use crate::data::*;
use crate::shown::{
    Bar, ItemShown, NOTHING_SHOWN, Shown, ShownMon, ShownSide, illusionists, listed_as, mask_list, may_be_disguise,
    muddled, roster_shown,
};
use crate::state::*;

/// One Pokémon as the log names it.
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub(crate) name: String,
    pub(crate) species: u16,
    pub(crate) gender: Gender,
    /// What had been shown of it when it was last off the field.
    pub(crate) rec: Shown,
}

/// Whoever is in an active position.
#[derive(Clone, Debug)]
pub(crate) struct Live {
    /// Index of the entry it goes by.
    pub(crate) entry: usize,
    /// The species on show (a temporary forme, or the Pokémon it transformed into).
    pub(crate) species: u16,
    pub(crate) gender: Gender,
    pub(crate) rec: Shown,
    pub(crate) transformed: bool,
    /// It has fainted, and what was shown of it has been filed.
    pub(crate) gone: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ObsSide {
    pub(crate) team: Vec<Entry>,
    pub(crate) active: [Option<Live>; ACTIVE],
    /// Pokémon brought (`|teamsize|`), and how many of them have fainted.
    pub(crate) size: u8,
    pub(crate) fainted: u8,
    /// The registered team as Team Preview listed it (`|poke|`), and whether
    /// its sheet has been shown as well (`|showteam|`).
    pub(crate) roster: Vec<Listed>,
    pub(crate) open: bool,
}

/// A reader of Showdown's battle log. See the module notes.
#[derive(Clone, Debug, Default)]
pub struct Observer {
    pub(crate) sides: [ObsSide; 2],
    /// Items seen to change hands, in order, for whoever keeps track of more than the
    /// record (see [`crate::speed`]): the side, the registered Pokémon it appears to be,
    /// whether that is in doubt, the item, and whether it was lost (or else come by).
    pub(crate) item_notes: Vec<(usize, u8, bool, u16, bool)>,
    /// Items used up since the last move began, by side, name and item (see `arrived`).
    used_up: Vec<(usize, String, u16)>,
    /// The Pokémon whose Cud Chew has just been announced.
    cud_chew: Option<(usize, String)>,
    /// The move in progress is Skill Swap.
    swapping: bool,
    /// User, target and move of the last move line, for a bounced move
    /// stopped before Showdown can emit its own move line.
    current_move: Option<((usize, usize), (usize, usize), u16)>,
}

/// Which of the Pokémon a line mentions has the ability or item it names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Who {
    /// The Pokémon the line is about (its first argument).
    Subject,
    /// The Pokémon after `[of]`; the subject if there is none.
    Of,
    /// Neither: the line names the effect without saying who has it.
    Nobody,
}

/// The holder of the ability or item (`is_item`) with id `effect` that a line
/// of kind `kind` names, either as an argument (`ability: X`) or in a
/// `[from]` tag (`tagged`).
pub fn holder(kind: &str, tagged: bool, is_item: bool, effect: &str) -> Who {
    if !tagged {
        // `|-activate|p1a: A|ability: X`, `|cant|p1a: A|ability: X|Move|[of] p2a: B`,
        // `|-start|p1a: A|ability: Flash Fire`... The three abilities that guard an
        // ally say whom they guarded first.
        return if kind == "-block" { Who::Of } else { Who::Subject };
    }
    match (kind, is_item) {
        // A berry's effects carry its name, but by then it has been eaten, and not always by its holder.
        // What its holder had was said by the `-enditem` line.
        // (Checked by the caller, which knows what is a berry.)
        ("-damage", _) => Who::Of,
        ("-status" | "-weather" | "-fieldstart" | "-clearboost" | "-start" | "-sidestart", false) => Who::Of,
        ("-heal", false) if effect == "hospitality" => Who::Of,
        ("-item", false) if effect == "frisk" => Who::Of,
        // The thief's ability, on a line about its victim.
        ("-enditem", false) => Who::Nobody,
        // A flung herb cures its recipient, which never holds the item.
        ("-end", true) if effect == "mentalherb" => Who::Nobody,
        _ => Who::Subject,
    }
}

/// `p1a: Name` or `p1: Name`.
pub(crate) struct Ident<'a> {
    pub(crate) side: usize,
    pub(crate) pos: Option<usize>,
    pub(crate) name: &'a str,
}

pub(crate) fn ident(s: &str) -> Option<Ident<'_>> {
    let (who, name) = s.split_once(": ")?;
    let side = match who.get(..2)? {
        "p1" => 0,
        "p2" => 1,
        _ => return None,
    };
    let pos = match who.get(2..)? {
        "" => None,
        "a" => Some(0),
        "b" => Some(1),
        _ => return None,
    };
    Some(Ident { side, pos, name })
}

/// `Garchomp-Mega, L50, M` to species and gender.
pub(crate) fn details(s: &str) -> Result<(u16, Gender), String> {
    let mut parts = s.split(", ");
    let name = parts.next().unwrap_or("");
    let species = species_id(&to_id(name)).ok_or_else(|| format!("unknown species {name:?}"))?;
    let mut gender = Gender::N;
    for p in parts {
        match p {
            "M" => gender = Gender::M,
            "F" => gender = Gender::F,
            _ => {}
        }
    }
    Ok((species, gender))
}

/// `73/100 par`, `50/100y`, `0 fnt` to HP, bar, status and whether it has fainted.
pub(crate) fn condition(s: &str) -> Result<(u8, Bar, Status, bool), String> {
    let (hp, status) = s.split_once(' ').unwrap_or((s, ""));
    if status == "fnt" {
        return Ok((0, Bar::Red, Status::None, true));
    }
    let (num, den) = hp.split_once('/').ok_or_else(|| format!("unreadable HP {s:?}"))?;
    let num: u32 = num.parse().map_err(|_| format!("unreadable HP {s:?}"))?;
    let (den, mark) = match den.chars().last() {
        Some(c) if c.is_ascii_alphabetic() => (&den[..den.len() - 1], Some(c)),
        _ => (den, None),
    };
    let den: u32 = den.parse().map_err(|_| format!("unreadable HP {s:?}"))?;
    if den == 0 {
        return Err(format!("unreadable HP {s:?}"));
    }
    let pct = if den == 100 { num } else { (100 * num / den).max(1) };
    let bar = match mark {
        Some('g') => Bar::Green,
        Some('y') => Bar::Yellow,
        Some('r') => Bar::Red,
        _ if 2 * num > den => Bar::Green,
        _ if 5 * num > den => Bar::Yellow,
        _ => Bar::Red,
    };
    let status = match status {
        "" => Status::None,
        "brn" => Status::Brn,
        "par" => Status::Par,
        "psn" => Status::Psn,
        "tox" => Status::Tox,
        "slp" => Status::Slp,
        "frz" => Status::Frz,
        other => return Err(format!("unknown status {other:?}")),
    };
    Ok((pct as u8, bar, status, false))
}

fn ability(name: &str) -> Result<u16, String> {
    let id = to_id(name);
    if id.is_empty() {
        return Ok(ab::NOABILITY);
    }
    ability_id(&id).ok_or_else(|| format!("unknown ability {name:?}"))
}

fn item(name: &str) -> Result<u16, String> {
    item_id(&to_id(name)).ok_or_else(|| format!("unknown item {name:?}"))
}

fn a_move(name: &str) -> Result<u16, String> {
    move_id(&to_id(name)).ok_or_else(|| format!("unknown move {name:?}"))
}

impl Shown {
    /// A line names this item as the Pokémon's own. Not news if it is a berry
    /// (see [`holder`]) or the item it has just been shown to lose.
    pub(crate) fn name_item(&mut self, item: u16) {
        if item == it::NONE || ITEMS[item as usize].flags & IF_BERRY != 0 || self.item == ItemShown::Lost(item) {
            return;
        }
        self.item = ItemShown::Holds(item);
    }
}

/// One Pokémon of `|showteam|p1|…`, Showdown's packed team format:
/// `Species||Item|Ability|Move,Move|Nature||Gender|||Level|`, with the species
/// second if the Pokémon has a nickname.
fn sheet(packed: &str) -> Result<Listed, String> {
    let f: Vec<&str> = packed.split('|').collect();
    let field = |i: usize| f.get(i).copied().unwrap_or("");
    let name = if field(1).is_empty() { field(0) } else { field(1) };
    let species = species_id(&to_id(name)).ok_or_else(|| format!("unknown species {name:?}"))?;
    let mut out = Listed { species, ..Listed::NONE };
    out.item = if field(2).is_empty() { it::NONE } else { item(field(2))? };
    out.ability = ability(field(3))?;
    for name in field(4).split(',').filter(|m| !m.is_empty()) {
        if out.n_moves as usize == MAX_MOVES {
            return Err("a team sheet with more than four moves".to_string());
        }
        out.moves[out.n_moves as usize] = a_move(name)?;
        out.n_moves += 1;
    }
    out.nature = if field(5).is_empty() {
        (0, 0)
    } else {
        nature(field(5)).ok_or_else(|| format!("unknown nature {:?}", field(5)))?
    };
    out.gender = match field(7) {
        "M" => Gender::M,
        "F" => Gender::F,
        _ => Gender::N,
    };
    Ok(out)
}

impl ObsSide {
    /// The entries of the registered team that Pokémon seen so far have been taken for.
    fn taken(&self) -> u8 {
        self.team.iter().filter(|e| e.rec.listed != NOT_LISTED).fold(0, |mask, e| mask | 1 << e.rec.listed)
    }

    /// The entry of the registered team this Pokémon, appearing with these details, is taken for.
    fn list(&mut self, entry: usize, species: u16, gender: Gender) {
        if self.team[entry].rec.listed == NOT_LISTED {
            self.team[entry].rec.listed = listed_as(&self.roster, species, gender, self.taken());
        }
    }

    /// The entries of the registered team that have Illusion, as far as can be told.
    fn illusionists(&self) -> u8 {
        illusionists(&self.roster, self.open)
    }

    /// Whether whoever is in `pos` may be the side's Illusion Pokémon in the shape it shows.
    fn suspect(&self, pos: usize) -> bool {
        let Some(live) = self.active[pos].as_ref().filter(|l| !l.gone) else {
            return false;
        };
        may_be_disguise(&self.roster, self.illusionists(), live.rec.listed, |only| {
            let fainted = self.team.iter().any(|e| e.rec.listed == only && e.rec.fainted);
            let beside = (0..ACTIVE)
                .any(|p| p != pos && self.active[p].as_ref().is_some_and(|l| !l.gone && l.rec.listed == only));
            fainted || beside
        })
    }

    /// Clears the doubt about whoever on the field need no longer be doubted.
    fn recheck(&mut self) {
        for pos in 0..ACTIVE {
            if self.active[pos].as_ref().is_some_and(|l| !l.gone && l.rec.suspect) && !self.suspect(pos) {
                self.active[pos].as_mut().unwrap().rec.suspect = false;
            }
        }
    }

    fn entry(&mut self, name: &str) -> usize {
        if let Some(i) = self.team.iter().position(|e| e.name == name) {
            return i;
        }
        let mut rec = NOTHING_SHOWN;
        rec.seen = self.team.len() as u8 + 1;
        self.team.push(Entry { name: name.to_string(), species: NO_SPECIES, gender: Gender::N, rec });
        self.team.len() - 1
    }

    /// The record a line about `id` writes to: the Pokémon in that position if
    /// it is on the field, otherwise whoever goes by that name.
    fn rec(&mut self, id: &Ident) -> &mut Shown {
        if let Some(pos) = id.pos
            && self.active[pos].as_ref().is_some_and(|l| !l.gone)
        {
            return &mut self.active[pos].as_mut().unwrap().rec;
        }
        let i = self.entry(id.name);
        &mut self.team[i].rec
    }

    fn live(&mut self, id: &Ident) -> Option<&mut Live> {
        self.active[id.pos?].as_mut().filter(|l| !l.gone)
    }

    /// Whoever is in `pos` leaves the field: what was shown of it is filed under its name.
    fn leave(&mut self, pos: usize) {
        if let Some(live) = self.active[pos].take()
            && !live.gone
        {
            let mut rec = live.rec;
            rec.leave();
            rec.seen = self.team[live.entry].rec.seen;
            self.team[live.entry].rec = rec;
        }
    }
}

impl Observer {
    pub fn new() -> Observer {
        Observer::default()
    }

    /// Reads a stretch of raw log, taking the public version of the lines that have a private one.
    pub fn lines<S: AsRef<str>>(&mut self, log: &[S]) -> Result<(), String> {
        let mut skip = false;
        for line in log {
            let line = line.as_ref();
            if skip {
                skip = false;
                continue;
            }
            if line.starts_with("|split|") {
                skip = true;
                continue;
            }
            self.line(line).map_err(|e| format!("{e} in {line:?}"))?;
        }
        Ok(())
    }

    fn rec(&mut self, id: &Ident) -> &mut Shown {
        self.sides[id.side].rec(id)
    }

    /// Notes that `id` has lost `item`, or come by it, before the record is changed.
    fn note_item(&mut self, id: &Ident, item: u16, lost: bool) {
        let rec = *self.rec(id);
        self.item_notes.push((id.side, rec.listed, rec.suspect || rec.tainted, item, lost));
    }

    /// A line says `item` has reached `id` (Trick, Thief, Magician...). Such lines
    /// come after the item was handed over, and one that is used the moment it
    /// arrives (a White Herb, a terrain seed) has had its `-enditem` by then,
    /// which stands.
    fn arrived(&mut self, id: &Ident, item: u16) {
        // Only these are used as they arrive; a berry, say, waits for the next check-up.
        let used_on_arrival = matches!(
            ITEMS[item as usize].id,
            "whiteherb" | "electricseed" | "grassyseed" | "mistyseed" | "psychicseed"
        );
        let gone = used_on_arrival
            && self.used_up.iter().any(|(side, name, i)| *side == id.side && name == id.name && *i == item);
        self.note_item(id, item, false);
        if !gone {
            self.rec(id).item = ItemShown::Holds(item);
        }
    }

    /// `id` has been seen to have this move of its own. (Not while it is
    /// transformed: those are the moves of the Pokémon it copied.)
    fn own_move(&mut self, id: &Ident, mv: u16) {
        let transformed = self.sides[id.side].live(id).is_some_and(|l| l.transformed);
        if !transformed && mv != crate::battle::struggle_id() {
            self.rec(id).add_move(mv);
        }
    }

    /// `id` has used this move as its own, or been stopped from using it
    /// (`borrowed`: used it on another move's account). See [`Shown::use_move`].
    fn used_move(&mut self, id: &Ident, mv: u16, borrowed: bool) {
        let transformed = self.sides[id.side].live(id).is_some_and(|l| l.transformed);
        if transformed {
            return;
        }
        if borrowed {
            self.rec(id).borrow_move(mv);
        } else if mv != crate::battle::struggle_id() {
            self.rec(id).use_move(mv);
        }
    }

    /// Takes in the `[from]` and `[of]` tags of a line about `subject`.
    fn tags(&mut self, kind: &str, subject: &Ident, parts: &[&str]) -> Result<(), String> {
        let from = parts.iter().find_map(|p| p.strip_prefix("[from] "));
        let of = parts.iter().find_map(|p| p.strip_prefix("[of] ")).and_then(ident);
        let Some(from) = from else {
            return Ok(());
        };
        self.named(kind, true, subject, of.as_ref(), from)
    }

    /// `effect` is `ability: X` or `item: X` (anything else is ignored): note who has it.
    fn named(
        &mut self,
        kind: &str,
        tagged: bool,
        subject: &Ident,
        of: Option<&Ident>,
        effect: &str,
    ) -> Result<(), String> {
        let (is_item, name) = if let Some(name) = effect.strip_prefix("ability: ") {
            (false, name)
        } else if let Some(name) = effect.strip_prefix("item: ") {
            (true, name)
        } else {
            return Ok(());
        };
        let who = match holder(kind, tagged, is_item, &to_id(name)) {
            Who::Subject => subject,
            Who::Of => of.unwrap_or(subject),
            Who::Nobody => return Ok(()),
        };
        if is_item {
            let i = item(name)?;
            self.rec(who).name_item(i);
        } else {
            let a = ability(name)?;
            self.rec(who).set_ability(a);
        }
        Ok(())
    }

    /// Heal Block can stop a reflected Heal Pulse before its move line. This
    /// cant belongs to the bounce, not the target's original queued action.
    pub(crate) fn borrowed_cant(&self, side: usize, pos: usize, reason: &str, named: &str) -> bool {
        reason == "move: Heal Block"
            && self.current_move.is_some_and(|(user, target, mv)| {
                user != (side, pos) && target == (side, pos) && MOVES[mv as usize].id == to_id(named)
            })
    }

    /// Reads one line of the log.
    pub fn line(&mut self, line: &str) -> Result<(), String> {
        self.read(line)?;
        // Whatever it said may have settled who is who.
        self.sides.iter_mut().for_each(ObsSide::recheck);
        Ok(())
    }

    fn read(&mut self, line: &str) -> Result<(), String> {
        if let Some(rest) = line.strip_prefix("|showteam|") {
            // `|showteam|p1|Garchomp||LifeOrb|RoughSkin|Earthquake,Protect|Jolly||M|||50|]…`: an open
            // team sheet, in the order Team Preview listed the team.
            let (side, packed) = rest.split_once('|').ok_or("an empty team sheet")?;
            let side = &mut self.sides[if side == "p1" { 0 } else { 1 }];
            let sheets = packed.split(']').filter(|p| !p.is_empty()).map(sheet).collect::<Result<Vec<_>, _>>()?;
            if !side.roster.is_empty() && side.roster.len() != sheets.len() {
                return Err("a team sheet for a different number of Pokémon than Team Preview showed".to_string());
            }
            side.roster = sheets;
            side.open = true;
            return Ok(());
        }
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() < 2 || !parts[0].is_empty() {
            return Ok(());
        }
        let kind = parts[1];
        if matches!(kind, "" | "move" | "switch" | "drag" | "turn" | "upkeep") {
            self.used_up.clear();
        }
        if matches!(kind, "" | "-fail" | "switch" | "drag" | "turn" | "upkeep") {
            self.current_move = None;
        }
        let arg = |i: usize| parts.get(i).copied().unwrap_or("");
        let of = parts.iter().find_map(|p| p.strip_prefix("[of] ")).and_then(ident);
        let from = parts.iter().find_map(|p| p.strip_prefix("[from] ")).unwrap_or("");
        match kind {
            "clearpoke" => {
                // Team Preview begins.
                for side in &mut self.sides {
                    side.roster.clear();
                    side.open = false;
                }
                return Ok(());
            }
            "poke" => {
                // `|poke|p1|Garchomp, L50, M|`: one Pokémon of a registered team. A few species are
                // listed without their forme in some formats (`Zacian-*`).
                let side = if arg(2) == "p1" { 0 } else { 1 };
                let shown = arg(3);
                let name = shown.split(", ").next().unwrap_or("");
                let any_forme = name.ends_with("-*");
                let (species, gender) = details(&shown.replacen("-*", "", 1))?;
                self.sides[side].roster.push(Listed { species, any_forme, gender, ..Listed::NONE });
                return Ok(());
            }
            "teamsize" => {
                let side = if arg(2) == "p1" { 0 } else { 1 };
                self.sides[side].size = arg(3).parse().map_err(|_| "unreadable team size".to_string())?;
                return Ok(());
            }
            "switch" | "drag" => {
                let id = ident(arg(2)).ok_or("unreadable Pokémon")?;
                let pos = id.pos.ok_or("a switch with no position")?;
                let (species, gender) = details(arg(3))?;
                let (hp, bar, status, _) = condition(arg(4))?;
                let side = &mut self.sides[id.side];
                side.leave(pos);
                let i = side.entry(id.name);
                (side.team[i].species, side.team[i].gender, side.team[i].rec.species) = (species, gender, species);
                side.list(i, species, gender);
                let mut rec = side.team[i].rec;
                rec.fainted = false;
                rec.leave();
                (rec.hp, rec.bar, rec.status) = (hp, bar, status);
                side.active[pos] = Some(Live { entry: i, species, gender, rec, transformed: false, gone: false });
                let suspect = side.suspect(pos);
                side.active[pos].as_mut().unwrap().rec.suspect = suspect;
                return Ok(());
            }
            "swap" => {
                // `|swap|p1a: A|1`: A and its partner trade places (Ally Switch).
                let id = ident(arg(2)).ok_or("unreadable Pokémon")?;
                let (Some(from), Ok(to)) = (id.pos, arg(3).parse::<usize>()) else {
                    return Err("unreadable swap".to_string());
                };
                if to < ACTIVE {
                    self.sides[id.side].active.swap(from, to);
                }
                return Ok(());
            }
            "replace" => {
                // Illusion ends: this position holds, and has held since it was filled, someone else.
                let id = ident(arg(2)).ok_or("unreadable Pokémon")?;
                let pos = id.pos.ok_or("a replacement with no position")?;
                let (species, gender) = details(arg(3))?;
                let side = &mut self.sides[id.side];
                let old = side.active[pos].take().ok_or("a replacement for nobody")?;
                let before = side.team[old.entry].rec;
                let i = side.entry(id.name);
                (side.team[i].species, side.team[i].gender, side.team[i].rec.species) = (species, gender, species);
                side.list(i, species, gender);
                let mut rec = side.team[i].rec;
                rec.fainted = false;
                rec.leave();
                old.rec.learned_since(&before, &mut rec);
                // Whatever it looked like, this is what it is. (But where the Illusion Pokémon
                // cannot be told apart, what was noted under the name it used may be another's.)
                rec.suspect = false;
                rec.tainted |= muddled(&side.roster, side.illusionists());
                (rec.hp, rec.bar, rec.status) = (old.rec.hp, old.rec.bar, old.rec.status);
                side.active[pos] = Some(Live { entry: i, species, gender, rec, transformed: false, gone: false });
                return Ok(());
            }
            _ => {}
        }
        // Everything else is about the Pokémon named first, if any is. A line about the field
        // (`|-weather|Sandstorm|[from] ability: Sand Stream|[of] p1a: A`) can still name whose doing it was.
        let Some(id) = ident(arg(2)) else {
            if let (Some(source), false) = (&of, from.is_empty()) {
                self.named(kind, true, source, Some(source), from)?;
            }
            return Ok(());
        };
        match kind {
            "faint" => {
                let side = &mut self.sides[id.side];
                side.fainted += 1;
                if let Some(live) = id.pos.and_then(|p| side.active[p].as_mut()).filter(|l| !l.gone) {
                    let mut rec = live.rec;
                    (rec.hp, rec.bar, rec.status, rec.fainted) = (0, Bar::Red, Status::None, true);
                    rec.leave();
                    live.gone = true;
                    let entry = live.entry;
                    rec.seen = side.team[entry].rec.seen;
                    side.team[entry].rec = rec;
                }
            }
            "move" => {
                let mv = a_move(arg(3))?;
                self.current_move = id.pos.and_then(|p| {
                    let target = ident(arg(4))?;
                    Some(((id.side, p), (target.side, target.pos?), mv))
                });
                self.swapping = mv == mv::SKILLSWAP;
                // A move used outright is its own, and so is one Sleep Talk picks or a Round sung
                // after another. One borrowed by Copycat is not, and the later turns of a move
                // it is locked into (`[from] lockedmove`) say nothing the first did not.
                let own = match from {
                    "" | "move: Sleep Talk" => true,
                    f => f.strip_prefix("move: ").is_some_and(|caller| to_id(caller) == to_id(arg(3))),
                };
                if own {
                    self.used_move(&id, mv, false);
                } else if from.starts_with("move: ") {
                    self.used_move(&id, mv, true);
                }
                self.tags(kind, &id, &parts)?;
            }
            "cant" => {
                // `|cant|p1a: A|reason|Move`: A could not use Move. But when the reason is
                // someone's ability, A is that ability's holder, the Pokémon that could not
                // move comes after `[of]`, and its move has already had a line of its own.
                let reason = arg(3);
                let borrowed = id.pos.is_some_and(|p| self.borrowed_cant(id.side, p, reason, arg(4)));
                self.current_move = None;
                self.named(kind, false, &id, of.as_ref(), reason)?;
                let mv = arg(4);
                if !borrowed && !reason.starts_with("ability: ") && !mv.is_empty() && !mv.starts_with('[') {
                    self.used_move(&id, a_move(mv)?, false);
                }
            }
            "-singleturn" if matches!(arg(3), "move: Focus Punch" | "move: Beak Blast") => {
                // Said at the start of the turn, before the move: it has been chosen.
                self.own_move(&id, a_move(&arg(3)[6..])?);
            }
            "-prepare" if parts.contains(&"[premajor]") => {
                // `|-prepare|p1a: A|Chilly Reception|[premajor]`, likewise ahead of the move.
                self.own_move(&id, a_move(arg(3))?);
            }
            "-end" if arg(3) == "Illusion" => {
                // After `replace`: the ability that was keeping up the disguise.
                self.rec(&id).end_illusion();
            }
            "-end" if arg(3).starts_with("ability: ") => {
                // `|-end|p1a: A|ability: Flash Fire`: what an ability it had was doing has stopped,
                // usually because the ability has just been replaced. Nothing about what it has now.
            }
            "-damage" | "-heal" | "-sethp" => {
                let (hp, bar, status, fainted) = condition(arg(3))?;
                let side = &mut self.sides[id.side];
                if from == "move: Revival Blessing" && side.fainted > 0 {
                    side.fainted -= 1;
                }
                let rec = side.rec(&id);
                (rec.hp, rec.bar, rec.status, rec.fainted) = (hp, bar, status, fainted);
                self.tags(kind, &id, &parts)?;
            }
            "-status" => {
                let (_, _, status, _) = condition(&format!("1/1 {}", arg(3)))?;
                self.rec(&id).status = status;
                self.tags(kind, &id, &parts)?;
            }
            "-curestatus" => {
                self.rec(&id).status = Status::None;
                self.tags(kind, &id, &parts)?;
            }
            "-item" => {
                let i = item(arg(3))?;
                // Recycle and Pickup say what is coming, Frisk and Poltergeist what is there; the
                // rest say what has been handed over, after the fact.
                if matches!(from, "" | "move: Recycle" | "ability: Pickup" | "ability: Harvest" | "ability: Frisk") {
                    if matches!(from, "move: Recycle" | "ability: Pickup" | "ability: Harvest") {
                        self.note_item(&id, i, false);
                    }
                    self.rec(&id).item = ItemShown::Holds(i);
                } else {
                    self.arrived(&id, i);
                }
                // Taken from someone: that Pokémon has it no longer.
                if let Some(victim) = &of
                    && matches!(from, "ability: Magician" | "ability: Pickpocket" | "move: Thief" | "move: Covet")
                {
                    // The preceding explicit -enditem already recorded this loss.
                    // Repeating it can turn contradictory historical item evidence
                    // into a false original item-clause owner after class reset.
                    if self.rec(victim).item != ItemShown::Lost(i) {
                        self.note_item(victim, i, true);
                    }
                    self.rec(victim).item = ItemShown::Lost(i);
                }
                self.tags(kind, &id, &parts)?;
            }
            "-enditem" => {
                if arg(3).is_empty() {
                    // `|-enditem|p1a: A||[from] move: Fling`: what A was throwing had gone by
                    // the time it left its hand (a Lum Berry, eaten for the burn the target's
                    // Spicy Spray gave it), and that has had its own line.
                    return Ok(());
                }
                let i = item(arg(3))?;
                if self.cud_chew.take().is_some_and(|(side, name)| side == id.side && name == id.name) {
                    // Cud Chew bringing up the berry it ate before: nothing it holds now.
                    return Ok(());
                }
                if parts.contains(&"[weaken]") {
                    // A berry that halves a hit goes with two lines, `…|[eat]` and then this
                    // one. An ally's Symbiosis can pass over its own item in between, and
                    // that is not what this line is about.
                    return Ok(());
                }
                self.note_item(&id, i, true);
                self.rec(&id).item = ItemShown::Lost(i);
                if from.is_empty() || from == "gem" || from == "move: Fling" {
                    // Used by its holder, not taken from it.
                    self.used_up.push((id.side, id.name.to_string(), i));
                }
                self.tags(kind, &id, &parts)?;
            }
            "-ability" => {
                let new = ability(arg(3))?;
                if from.is_empty() {
                    // The ability announces itself.
                    self.rec(&id).set_ability(new);
                } else {
                    // It has been replaced; the line may say what it was, and where the new one came from.
                    let old = arg(4);
                    if !old.is_empty() && !old.starts_with('[') && old != "boost" {
                        let old = ability(old)?;
                        self.rec(&id).set_ability(old);
                    }
                    self.rec(&id).change_ability(new);
                    if let Some(source) = &of {
                        // (Not if it has fainted, as Receiver's source has: that was its ability
                        // at the end, which need not be the one it started with.)
                        if self.sides[source.side].live(source).is_some() {
                            self.rec(source).set_ability(new);
                        }
                    }
                }
            }
            "-activate" => {
                let effect = arg(3);
                match effect {
                    "ability: Forewarn" => {
                        if let (Some(target), Ok(mv)) = (&of, a_move(arg(4))) {
                            self.own_move(target, mv);
                        }
                    }
                    "ability: Symbiosis" => {
                        let i = item(arg(4))?;
                        self.note_item(&id, i, true);
                        self.rec(&id).item = ItemShown::Lost(i);
                        if let Some(ally) = &of {
                            // One loss caused the transfer; a second loss can be
                            // the arriving herb/seed consumed before this line.
                            // An identical replacement herb is a fresh item.
                            let losses =
                                self.used_up.iter().filter(|(s, n, _)| *s == ally.side && n == ally.name).count();
                            self.arrived(ally, i);
                            if losses == 1 {
                                self.rec(ally).item = ItemShown::Holds(i);
                            }
                            self.used_up.retain(|(s, n, _)| *s != ally.side || n != ally.name);
                        }
                    }
                    "ability: Cud Chew" => self.cud_chew = Some((id.side, id.name.to_string())),
                    "ability: Mummy" | "ability: Lingering Aroma" => {
                        // `|-activate|p1a: Holder|ability: Mummy|p2a: Attacker|[ability] What it had`
                        if let Some(target) = ident(arg(4)) {
                            if let Some(old) = parts.iter().find_map(|p| p.strip_prefix("[ability] ")) {
                                let old = ability(old)?;
                                self.rec(&target).set_ability(old);
                            }
                            let new = ability(&effect[9..])?;
                            self.rec(&target).change_ability(new);
                        }
                    }
                    "Skill Swap" => {
                        // `|-activate|p1a: A|Skill Swap|What A gets|What B gets|[of] p2a: B`,
                        // the abilities left out when A and B are on the same side.
                        if let Some(other) = &of {
                            let (mine, theirs) = (self.rec(&id).ability, self.rec(other).ability);
                            let (gets, gives) = (arg(4), arg(5));
                            if gets.is_empty() || gets.starts_with('[') {
                                if self.swapping {
                                    self.rec(&id).change_ability(theirs);
                                } else {
                                    // No Skill Swap was used: this is the Wandering Spirit of a
                                    // Pokémon its own partner has just touched. That one had
                                    // it, the partner has it now, and what went the other way
                                    // is not said.
                                    self.rec(other).set_ability(ab::WANDERINGSPIRIT);
                                    self.rec(&id).change_ability(ab::WANDERINGSPIRIT);
                                }
                                self.rec(other).change_ability(mine);
                            } else {
                                let (gets, gives) = (ability(gets)?, ability(gives)?);
                                self.rec(&id).set_ability(gives);
                                self.rec(other).set_ability(gets);
                                self.rec(&id).change_ability(gets);
                                self.rec(other).change_ability(gives);
                            }
                        }
                    }
                    "move: Spite" | "move: Eerie Spell" | "item: Leppa Berry" => {
                        // PP taken from, or given back to, one of its moves.
                        if let Ok(mv) = a_move(arg(4)) {
                            self.own_move(&id, mv);
                        }
                    }
                    "move: Poltergeist" => {
                        let i = item(arg(4))?;
                        // A later hit can name an empty slot after the first hit
                        // consumed a berry. Keep the already announced loss.
                        if i != it::NONE {
                            self.rec(&id).item = ItemShown::Holds(i);
                        }
                    }
                    _ => {}
                }
                self.named(kind, false, &id, of.as_ref(), effect)?;
                self.tags(kind, &id, &parts)?;
            }
            "-transform" => {
                self.tags(kind, &id, &parts)?;
                if let Some(target) = ident(arg(3)) {
                    let theirs = self.sides[target.side].live(&target).map(|l| (l.species, l.rec.ability));
                    if let (Some((species, ability)), Some(live)) = (theirs, self.sides[id.side].live(&id)) {
                        live.species = species;
                        live.transformed = true;
                        live.rec.change_ability(ability);
                    }
                }
            }
            "detailschange" => {
                let (species, gender) = details(arg(3))?;
                let side = &mut self.sides[id.side];
                if let Some(live) = side.live(&id) {
                    (live.species, live.gender) = (species, gender);
                    live.rec.species = species;
                    let entry = live.entry;
                    (side.team[entry].species, side.team[entry].gender) = (species, gender);
                    side.team[entry].rec.species = species;
                }
            }
            "-formechange" => {
                let (species, _) = details(arg(3))?;
                if let Some(live) = self.sides[id.side].live(&id) {
                    live.species = species;
                }
                self.tags(kind, &id, &parts)?;
            }
            "-mega" => {
                // The stone is named, and a Mega has the one ability.
                let stone = item(arg(4))?;
                let ability = crate::shown::mega_ability(stone, &to_id(arg(3)));
                self.note_item(&id, stone, false);
                if let Some(live) = self.sides[id.side].live(&id) {
                    live.rec.item = ItemShown::Holds(stone);
                    (live.rec.ability, live.rec.base_ability, live.rec.ability_changed) = (ability, ability, false);
                }
            }
            _ => {
                // `|-start|p1a: A|ability: Flash Fire`, `|-end|p1a: A|ability: Flash Fire`, `|-block|…`
                self.named(kind, false, &id, of.as_ref(), arg(3))?;
                self.tags(kind, &id, &parts)?;
            }
        }
        Ok(())
    }

    /// What the log has shown about `side`'s Pokémon so far.
    pub fn shown(&self, side: usize) -> ShownSide {
        let s = &self.sides[side];
        let mut out = ShownSide { left: s.size.saturating_sub(s.fainted), ..Default::default() };
        let mut on_field = vec![false; s.team.len()];
        for live in &s.active {
            let Some(live) = live.as_ref().filter(|l| !l.gone) else {
                out.active.push(None);
                continue;
            };
            on_field[live.entry] = true;
            let mut mon = ShownMon::from_record(&live.rec, live.species, live.gender);
            mon.transformed = live.transformed;
            out.active.push(Some(mon));
        }
        for (i, e) in s.team.iter().enumerate() {
            if !on_field[i] && e.rec.species != NO_SPECIES {
                out.bench.push(ShownMon::from_record(&e.rec, e.rec.species, e.gender));
            }
        }
        out.bench.sort_by_key(|m| m.id);
        out.unseen = s.size.saturating_sub(s.team.len() as u8);
        out.roster = roster_shown(&s.roster, s.open);
        out.illusion = mask_list(s.illusionists());
        out
    }

    /// The record of whoever is on the field at `pos`, and of each Pokémon by
    /// its place in the order of appearance: for setting the engine's records
    /// to what the log says.
    pub(crate) fn live_record(&self, side: usize, pos: usize) -> Option<Shown> {
        self.sides[side].active[pos].as_ref().filter(|l| !l.gone).map(|l| l.rec)
    }

    pub(crate) fn filed_records(&self, side: usize) -> Vec<Shown> {
        self.sides[side].team.iter().map(|e| e.rec).collect()
    }
}
