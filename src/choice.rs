//! The public entry points: building a battle, listing legal choices, and
//! submitting them (Showdown's `Side#chooseMove` / `chooseSwitch` validation
//! and `Battle#commitChoices`).

#![allow(clippy::needless_range_loop, clippy::collapsible_if)]

use crate::battle::*;
use crate::data::*;
use crate::rng::Rng;
use crate::state::*;

impl Battle {
    /// A Pokémon with nothing filled in.
    pub(crate) fn blank_mon() -> Pokemon {
        let blank_slot = MoveSlot { id: 0, pp: 0, maxpp: 0, disabled: false, hidden: false, used: false };
        Pokemon {
            species: 0,
            base_species: 0,
            set_species: 0,
            transformed: false,
            base_moves: [MoveSlot { id: 0, pp: 0, maxpp: 0, disabled: false, hidden: false, used: false }; MAX_MOVES],
            base_n_moves: 0,
            illusion: 0,
            can_mega: NO_SPECIES,
            nature: (0, 0),
            stat_points: [0; 6],
            types: [Type::None; 2],
            added_type: Type::None,
            said: TypesSaid::fresh([Type::None; 2]),
            level: 50,
            gender: Gender::N,
            stats: [0; 6],
            hp: 0,
            status: Status::None,
            status_time: 0,
            tox_stage: 0,
            status_st: EffState::default(),
            boosts: [0; 7],
            moves: [blank_slot; MAX_MOVES],
            n_moves: 0,
            ability: ab::NOABILITY,
            base_ability: ab::NOABILITY,
            ability_st: EffState::default(),
            ability_boosts: [0; 7],
            syrup_triggered: false,
            was_attacked: false,
            last_attack_damage: 0,
            item: it::NONE,
            item_st: EffState::default(),
            last_item: it::NONE,
            used_item_this_turn: false,
            ate_berry: false,
            position: 0,
            is_active: false,
            fainted: true,
            faint_queued: false,
            switch_flag: false,
            switch_move: NO_MOVE,
            force_switch_flag: false,
            skip_before_switch_out: false,
            being_called_back: false,
            trapped: Trapped::No,
            active_turns: 0,
            move_this_turn: Res::Undef,
            move_last_turn: Res::Undef,
            last_move: NO_MOVE,
            last_attempted_move: None,
            unknown_pp: 0,
            base_unknown_pp: 0,
            last_move_loc: 0,
            active_move_actions: 0,
            newly_switched: true,
            hurt_this_turn: 0,
            times_attacked: 0,
            stats_raised_this_turn: false,
            stats_lowered_this_turn: false,
            hit_by_this_turn: 0,
            damaged_by: [NO_DAMAGED_BY; MAX_TEAM],
            n_damaged_by: 0,
            locked_move: NO_MOVE,
            speed: 0,
            live: crate::shown::NOTHING_SHOWN,
        }
    }

    /// A battle with no Pokémon in it yet.
    pub(crate) fn blank(seed: [u16; 4]) -> Battle {
        let blank_side = Side {
            team: [Battle::blank_mon(); MAX_TEAM],
            n: 0,
            order: [0, 1, 2, 3, 4, 5],
            pokemon_left: 0,
            total_fainted: 0,
            mega_used: false,
            fainted_this_turn: false,
            fainted_last_turn: false,
            conds: SideConds::new(SideCond::FIRST),
            slot_conds: [SlotConds::new(SlotCond::FIRST); ACTIVE],
            shown: [crate::shown::NOTHING_SHOWN; MAX_TEAM],
            n_seen: 0,
            roster: [Listed::NONE; MAX_ROSTER],
            n_roster: 0,
        };
        let nobody = MonRef { side: 0, idx: 0 };
        Battle {
            rng: Rng::from_words(seed),
            sides: [blank_side; 2],
            field: Field {
                weather: Cond::new(Weather::None),
                terrain: Cond::new(Terrain::None),
                pseudo: PseudoWeathers::new(Pseudo::FIRST),
            },
            vols: [[Volatiles::new(VolKind::FIRST); ACTIVE]; 2],
            turn: 0,
            request: Request::None,
            ended: false,
            winner: None,
            queue: Queue::new(),
            faint_queue: [FaintEntry { target: nobody, source: None, effect: Eff::None }; 8],
            n_faint: 0,
            mid_turn: true,
            effect_order: 0,
            next_uid: 0,
            event_mask: 0,
            event_mask_pre: 0,
            event: Event::NONE,
            effect: Eff::None,
            effect_holder: None,
            event_depth: 0,
            am: [ActiveMove::new(0); AM_CAP],
            am_len: 0,
            active_move: None,
            last_move: NO_MOVE,
            started: false,
            active_pokemon: None,
            active_target: None,
            speed_order: [0; 4],
            n_speed_order: 0,
            open_team_sheets: false,
        }
    }

    /// A Pokémon as it is at the start of a battle, from the set it was brought as.
    /// Registers the events its ability and item listen to.
    pub(crate) fn new_mon(&mut self, set: &PokemonSet, position: usize) -> Result<Pokemon, Error> {
        let sp = SPECIES.get(set.species as usize).ok_or_else(|| Error::BadTeam("unknown species".into()))?;
        if sp.illegal {
            return Err(Error::Unsupported(format!("{} is not in Champions", sp.name)));
        }
        if set.moves.is_empty() || set.moves.len() > MAX_MOVES {
            return Err(Error::BadTeam(format!("{} needs 1 to {MAX_MOVES} moves", sp.name)));
        }
        if set.stat_points.iter().any(|&p| p > 32) || set.stat_points.iter().map(|&p| p as u32).sum::<u32>() > 66 {
            return Err(Error::BadTeam(format!("{}: at most 32 stat points per stat and 66 in total", sp.name)));
        }
        let ability = ABILITIES.get(set.ability as usize).ok_or_else(|| Error::BadTeam("unknown ability".into()))?;
        if !ability.supported {
            return Err(Error::Unsupported(format!("ability {}", ability.name)));
        }
        let item = ITEMS.get(set.item as usize).ok_or_else(|| Error::BadTeam("unknown item".into()))?;
        if !item.supported {
            return Err(Error::Unsupported(format!("item {}", item.name)));
        }
        self.event_mask |= ability.events | ability.events_pre | item.events | item.events_pre;
        self.event_mask_pre |= ability.events_pre | item.events_pre;
        let mut mon = Battle::blank_mon();
        // `canMegaEvo`: holding the Mega Stone of this very species.
        if let Some(&(_, mega)) = item.mega.iter().find(|&&(from, _)| from == set.species) {
            let mega_ability = &ABILITIES[SPECIES[mega as usize].ability0 as usize];
            if !mega_ability.supported {
                return Err(Error::Unsupported(format!(
                    "{} (its ability, {}, is not modelled)",
                    SPECIES[mega as usize].name, mega_ability.name
                )));
            }
            self.event_mask |= mega_ability.events | mega_ability.events_pre;
            self.event_mask_pre |= mega_ability.events_pre;
            mon.can_mega = mega;
        }
        mon.species = set.species;
        mon.base_species = set.species;
        mon.set_species = set.species;
        mon.nature = set.nature;
        mon.stat_points = set.stat_points;
        mon.types = sp.types;
        mon.added_type = Type::None;
        mon.said = TypesSaid::fresh(sp.types);
        mon.gender = set.gender;
        mon.ability = set.ability;
        mon.base_ability = set.ability;
        mon.item = set.item;
        mon.fainted = false;
        mon.position = position as u8;
        // Champions stats: base + stat points + 75 for HP, + 20 otherwise.
        mon.stats = calc_stats(set.species, set.nature, set.stat_points);
        mon.hp = mon.stats[0];
        mon.speed = mon.stats[5] as i32;
        for (k, &id) in set.moves.iter().enumerate() {
            let d = MOVES.get(id as usize).ok_or_else(|| Error::BadTeam("unknown move".into()))?;
            if !d.supported {
                return Err(Error::Unsupported(format!("move {}", d.name)));
            }
            mon.moves[k] = MoveSlot { id, pp: d.pp, maxpp: d.pp, disabled: false, hidden: false, used: false };
        }
        mon.n_moves = set.moves.len() as u8;
        Ok(mon)
    }

    /// Builds a battle from the four Pokémon each side picked at team preview,
    /// in the order picked (the first two lead), and plays the opening
    /// switch-ins. `seed` is Showdown's four 16-bit seed words.
    ///
    /// Nothing is said here about the teams the players registered, so each
    /// side's registered team is taken to be what it brought, and the team
    /// sheets to be closed. [`Battle::with_rosters`] says otherwise.
    pub fn new(teams: [&[PokemonSet]; 2], seed: [u16; 4]) -> Result<Battle, Error> {
        let all: [Vec<usize>; 2] = [(0..teams[0].len()).collect(), (0..teams[1].len()).collect()];
        Battle::with_rosters(teams, [&all[0], &all[1]], false, seed)
    }

    /// Builds a battle from the teams the players registered. `rosters` are
    /// those teams (up to six Pokémon each), which Team Preview has shown to
    /// both players, and `picks[side]` the Pokémon that side brings, as
    /// indices into its roster in the order picked (the first two lead).
    ///
    /// With `open_team_sheets`, each player has also been given the other's
    /// team sheet: every registered Pokémon's item, ability, moves and
    /// nature, though not its stat points, and not which of them are
    /// brought. Showdown's Champions formats do this when both players
    /// agree, and always in a best-of-three. It changes nothing about how the
    /// battle plays, only what [`Battle::shown`] has to say from the start.
    pub fn with_rosters(
        rosters: [&[PokemonSet]; 2],
        picks: [&[usize]; 2],
        open_team_sheets: bool,
        seed: [u16; 4],
    ) -> Result<Battle, Error> {
        let mut b = Battle::blank(seed);
        b.open_team_sheets = open_team_sheets;
        for s in 0..2 {
            let (roster, picks) = (rosters[s], picks[s]);
            if roster.len() > MAX_ROSTER {
                return Err(Error::BadTeam(format!(
                    "side {} registers {} Pokémon; at most {MAX_ROSTER}",
                    s + 1,
                    roster.len()
                )));
            }
            if picks.len() < ACTIVE || picks.len() > MAX_TEAM {
                return Err(Error::BadTeam(format!(
                    "side {} brings {} Pokémon; need {ACTIVE} to {MAX_TEAM}",
                    s + 1,
                    picks.len()
                )));
            }
            for (j, set) in roster.iter().enumerate() {
                if set.moves.len() > MAX_MOVES {
                    return Err(Error::BadTeam(format!("side {}: a Pokémon with more than {MAX_MOVES} moves", s + 1)));
                }
                let mut listed = Listed {
                    species: set.species,
                    gender: set.gender,
                    item: set.item,
                    ability: set.ability,
                    n_moves: set.moves.len() as u8,
                    nature: set.nature,
                    ..Listed::NONE
                };
                listed.moves[..set.moves.len()].copy_from_slice(&set.moves);
                b.sides[s].roster[j] = listed;
            }
            b.sides[s].n_roster = roster.len() as u8;
            for (i, &j) in picks.iter().enumerate() {
                if j >= roster.len() || b.sides[s].roster[j].brought != NOT_LISTED {
                    return Err(Error::BadTeam(format!(
                        "side {}: the Pokémon brought must be different members of its roster",
                        s + 1
                    )));
                }
                b.sides[s].roster[j].brought = i as u8;
                b.sides[s].team[i] = b.new_mon(&roster[j], i)?;
            }
            b.sides[s].n = picks.len() as u8;
            b.sides[s].pokemon_left = picks.len() as u8;
        }

        // Team preview picks are queued as 'team' actions and sorted like any
        // others, so equal-speed Pokémon picked in the same slot draw from the RNG.
        let mut picks = [Battle::blank_action(ActKind::Team, 1); 2 * MAX_TEAM];
        let mut n = 0;
        for s in 0..2 {
            for i in 0..b.sides[s].n as usize {
                let r = MonRef { side: s as u8, idx: i as u8 };
                picks[n].priority = -(i as i32);
                picks[n].speed = b.action_speed(r);
                n += 1;
            }
        }
        b.speed_sort(&mut picks[..n], cmp_action, "team preview tie");

        b.queue.push(Battle::blank_action(ActKind::Start, 2));
        b.turn_loop();
        Ok(b)
    }

    /// Whether a move slot can be chosen (`Pokemon#getMoves`: PP left and not disabled).
    pub(crate) fn slot_usable(s: &MoveSlot) -> bool {
        s.pp > 0 && !s.disabled
    }

    pub(crate) fn usable_moves(&self, r: MonRef) -> bool {
        let m = self.mon(r);
        m.moves[..m.n_moves as usize].iter().any(Battle::slot_usable)
    }

    /// The target type Showdown's move request shows for a move
    /// (`Pokemon#getMoves`), which is what a choice of target is checked against.
    fn request_target(&self, r: MonRef, move_id: u16) -> Target {
        match move_id {
            // For anything but a Ghost, Curse is a move used on oneself.
            mv::CURSE if !self.has_type(r, Type::Ghost) => Target::User,
            // Heal Block keeps Pollen Puff off allies.
            mv::POLLENPUFF if self.vols(r).has(VolKind::Healblock) => Target::AdjacentFoe,
            _ => MOVES[move_id as usize].target,
        }
    }

    /// `Pokemon#isLastActive`: no living ally stands to its right.
    fn is_last_active(&self, r: MonRef) -> bool {
        let m = self.mon(r);
        m.is_active && (m.position as usize + 1..ACTIVE).all(|p| self.mon(self.active(r.side as usize, p)).fainted)
    }

    /// The one move choice listed for a Pokémon with no usable move, which
    /// Showdown turns into Struggle. Normally that is `move 1`. But Showdown does
    /// not tell a side's last active Pokémon which of its moves a foe's Imprison
    /// has sealed, so there it lists the real moves, and the choice is spelled
    /// like a use of the first one, target included. (Showdown accepts a use of
    /// any move it listed there, `struggle_ok`; this is the one the engine, the
    /// oracle's recorder and the masks name for it.)
    fn struggle_choice(&self, r: MonRef) -> Choice {
        let m = self.mon(r);
        let listed = self.is_last_active(r) && m.moves[..m.n_moves as usize].iter().any(|s| s.hidden && s.pp > 0);
        if listed {
            let t = self.request_target(r, m.moves[0].id);
            if t.is_chosen() {
                if let Some(loc) = [1i8, 2, -1, -2].into_iter().find(|&loc| self.valid_target_loc(loc, r, t)) {
                    return Choice::mv(0, loc);
                }
            }
        }
        Choice::mv(0, 0)
    }

    /// Whether Showdown takes `move <slot+1> <target>` from a Pokémon with no
    /// usable move (`Side#chooseMove`: with no move to use, any move slot the
    /// request lists is accepted, its target checked against that move's target
    /// type, and the choice becomes Struggle; a Mega flag is ignored). Shown
    /// Struggle alone, the Pokémon can only say `move 1`. Shown its real moves
    /// (its side's last active, a foe's Imprison sealing some of them unsaid), it
    /// may name any of them: `move 4` for a sealed Protect under Encore, as a
    /// player going by the request does. The engine lists `struggle_choice`
    /// alone, but takes what Showdown takes.
    fn struggle_ok(&self, r: MonRef, slot: u8, target: i8) -> bool {
        let m = self.mon(r);
        let listed = self.is_last_active(r) && m.moves[..m.n_moves as usize].iter().any(|s| s.hidden && s.pp > 0);
        if !listed {
            return slot == 0 && target == 0;
        }
        if slot >= m.n_moves {
            return false;
        }
        let t = self.request_target(r, m.moves[slot as usize].id);
        if t.is_chosen() { target != 0 && self.valid_target_loc(target, r, t) } else { target == 0 }
    }

    /// Every choice the given active slot may make at the current request.
    pub fn legal_choices(&self, side: usize, pos: usize) -> Vec<Choice> {
        let mut out = Vec::new();
        let r = self.active(side, pos);
        let m = self.mon(r);
        let s = &self.sides[side];
        let bench = |out: &mut Vec<Choice>| {
            for p in ACTIVE..s.n as usize {
                if !s.team[s.order[p] as usize].fainted {
                    out.push(Choice::Switch { to: p as u8 });
                }
            }
        };
        match self.request {
            Request::None => {}
            Request::Move => {
                if m.fainted {
                    out.push(Choice::Pass);
                    return out;
                }
                if m.locked_move != NO_MOVE {
                    // Locked into a move: Showdown lists that move alone, with no target to pick.
                    out.push(Choice::mv(0, 0));
                } else if !self.usable_moves(r) {
                    out.push(self.struggle_choice(r));
                } else {
                    // Any usable move can be combined with Mega Evolution.
                    let megas: &[bool] = if m.can_mega != NO_SPECIES { &[false, true] } else { &[false] };
                    for (i, slot) in m.moves[..m.n_moves as usize].iter().enumerate() {
                        if !Battle::slot_usable(slot) {
                            continue;
                        }
                        let t = self.request_target(r, slot.id);
                        for &mega in megas {
                            if t.is_chosen() {
                                for loc in [1i8, 2, -1, -2] {
                                    if self.valid_target_loc(loc, r, t) {
                                        out.push(Choice::Move { slot: i as u8, target: loc, mega });
                                    }
                                }
                            } else {
                                out.push(Choice::Move { slot: i as u8, target: 0, mega });
                            }
                        }
                    }
                }
                if m.trapped == Trapped::No {
                    bench(&mut out);
                }
            }
            Request::Switch => {
                if !m.switch_flag {
                    out.push(Choice::Pass);
                    return out;
                }
                if self.reviving(side, pos) {
                    // Revival Blessing: pick a fainted team member (one still in an active slot counts).
                    for p in 0..s.n as usize {
                        if s.team[s.order[p] as usize].fainted {
                            out.push(Choice::Switch { to: p as u8 });
                        }
                    }
                } else {
                    bench(&mut out);
                }
                if self.living_bench(side) < self.open_slots(side) {
                    // More empty slots than replacements: one of them stays empty.
                    out.push(Choice::Pass);
                }
            }
        }
        out
    }

    pub(crate) fn living_bench(&self, side: usize) -> usize {
        let s = &self.sides[side];
        (ACTIVE..s.n as usize).filter(|&p| !s.team[s.order[p] as usize].fainted).count()
    }

    /// Whether the Pokémon in this slot is being asked whom to revive.
    fn reviving(&self, side: usize, pos: usize) -> bool {
        self.sides[side].slot_conds[pos].has(SlotCond::Revivalblessing)
    }

    fn open_slots(&self, side: usize) -> usize {
        (0..ACTIVE).filter(|&p| self.mon(self.active(side, p)).switch_flag).count()
    }

    /// Whether one slot's choice is legal on its own (the allocation-free
    /// counterpart of `legal_choices`).
    fn slot_ok(&self, side: usize, pos: usize, c: Choice) -> bool {
        let r = self.active(side, pos);
        let m = self.mon(r);
        let s = &self.sides[side];
        let bench_ok =
            |to: u8| (ACTIVE..s.n as usize).contains(&(to as usize)) && !s.team[s.order[to as usize] as usize].fainted;
        match self.request {
            Request::None => false,
            Request::Move => match c {
                Choice::Pass => m.fainted,
                _ if m.fainted => false,
                Choice::Switch { to } => m.trapped == Trapped::No && bench_ok(to),
                Choice::Move { slot, target, mega } => {
                    if m.locked_move != NO_MOVE {
                        return c == Choice::mv(0, 0);
                    }
                    if !self.usable_moves(r) {
                        // Struggle, however the choice spells it; Showdown ignores a Mega flag here.
                        let _ = mega;
                        return self.struggle_ok(r, slot, target);
                    }
                    if slot >= m.n_moves || !Battle::slot_usable(&m.moves[slot as usize]) {
                        return false;
                    }
                    if mega && m.can_mega == NO_SPECIES {
                        return false;
                    }
                    let t = self.request_target(r, m.moves[slot as usize].id);
                    if t.is_chosen() { target != 0 && self.valid_target_loc(target, r, t) } else { target == 0 }
                }
            },
            Request::Switch => match c {
                Choice::Pass => !m.switch_flag || self.living_bench(side) < self.open_slots(side),
                Choice::Switch { to } if self.reviving(side, pos) => {
                    m.switch_flag && to < s.n && s.team[s.order[to as usize] as usize].fainted
                }
                Choice::Switch { to } => m.switch_flag && bench_ok(to),
                Choice::Move { .. } => false,
            },
        }
    }

    /// The constraints that tie a side's two slots together: no two slots
    /// switching to the same Pokémon, only one Mega Evolution, and at a switch
    /// request every open slot filled while replacements last.
    #[inline(always)]
    pub(crate) fn pair_ok(&self, side: usize, c: &[Choice; ACTIVE]) -> bool {
        if let (Choice::Switch { to: a }, Choice::Switch { to: b }) = (c[0], c[1]) {
            if a == b {
                return false;
            }
        }
        if let (Choice::Move { mega: true, .. }, Choice::Move { mega: true, .. }) = (c[0], c[1]) {
            return false;
        }
        self.request != Request::Switch || self.switch_pair_ok(side, c)
    }

    /// The part of `pair_ok` that only a switch request needs.
    #[inline(never)]
    fn switch_pair_ok(&self, side: usize, c: &[Choice; ACTIVE]) -> bool {
        // Showdown counts down the switches and passes it still expects as it
        // reads the slots in order. A revival uses up a switch if one is left,
        // so with one replacement for a fainted Pokémon and a reviver to its
        // left, reviving and replacing cannot be chosen together (Showdown throws).
        let out = self.open_slots(side);
        let mut switches = out.min(self.living_bench(side));
        let mut passes = out - switches;
        for p in 0..ACTIVE {
            if !self.mon(self.active(side, p)).switch_flag {
                continue;
            }
            match c[p] {
                Choice::Pass => {
                    if passes == 0 {
                        return false;
                    }
                    passes -= 1;
                }
                Choice::Switch { .. } if self.reviving(side, p) => switches = switches.saturating_sub(1),
                Choice::Switch { .. } => {
                    if switches == 0 {
                        return false;
                    }
                    switches -= 1;
                }
                Choice::Move { .. } => return false,
            }
        }
        switches == 0
    }

    /// Whether a side's two slot choices are legal together.
    pub fn joint_ok(&self, side: usize, c: &[Choice; ACTIVE]) -> bool {
        (0..ACTIVE).all(|pos| self.slot_ok(side, pos, c[pos])) && self.pair_ok(side, c)
    }

    /// Every legal pair of slot choices for a side at the current request.
    pub fn joint_choices(&self, side: usize) -> Vec<[Choice; ACTIVE]> {
        let a = self.legal_choices(side, 0);
        let b = self.legal_choices(side, 1);
        let mut out = Vec::with_capacity(a.len() * b.len());
        for &x in &a {
            for &y in &b {
                let c = [x, y];
                if self.pair_ok(side, &c) {
                    out.push(c);
                }
            }
        }
        out
    }

    /// Whether the turn has been played to its end. A switch request in the
    /// middle of a turn (U-turn, Eject Button) still has the end of the turn
    /// queued; a request for replacing the fainted after it does not.
    pub fn turn_over(&self) -> bool {
        self.ended
            || self.request != Request::Switch
            || !self.queue.as_slice().iter().any(|a| a.kind == ActKind::Residual)
    }

    /// Submits both sides' choices and runs the battle to the next request (or the end).
    /// A side that is not being asked anything passes in both slots.
    pub fn choose(&mut self, choices: [[Choice; ACTIVE]; 2]) -> Result<(), Error> {
        if self.ended || self.request == Request::None {
            return Err(Error::BadChoice("the battle is not waiting for a choice".into()));
        }
        let request = self.request;
        for side in 0..2 {
            if !self.joint_ok(side, &choices[side]) {
                return Err(Error::BadChoice(format!("p{}: {:?} is not legal here", side + 1, choices[side])));
            }
        }

        // commitChoices
        self.update_speed();
        let old = self.queue;
        self.queue.clear();
        self.am_len = 0;
        for side in 0..2 {
            for pos in 0..ACTIVE {
                let r = self.active(side, pos);
                match choices[side][pos] {
                    Choice::Pass => {}
                    Choice::Move { slot, target, mega } => {
                        // A locked move goes where it was aimed. Struggle picks its
                        // own target, whatever the choice said (and whichever slot it
                        // named, `struggle_ok`).
                        let locked = self.mon(r).locked_move;
                        let usable = self.usable_moves(r);
                        let (id, target) = if locked != NO_MOVE {
                            (locked, self.locked_move_loc(r, locked))
                        } else if usable {
                            (self.mon(r).moves[slot as usize].id, target)
                        } else {
                            (struggle_id(), 0)
                        };
                        // The move, and ahead of it whatever goes with it (a Mega Evolution, ...).
                        // Showdown's `chooseMove` returns before it reads a Mega flag on a locked
                        // move or on Struggle, so the flag does nothing there.
                        let (actions, n) = self.resolve_move_actions(r, id, target, mega && locked == NO_MOVE && usable);
                        for a in &actions[..n] {
                            self.queue.push(*a);
                        }
                    }
                    Choice::Switch { to } if request == Request::Switch && self.reviving(side, pos) => {
                        // Not a switch at all: the chosen Pokémon is brought back.
                        let mut a = Battle::blank_action(ActKind::Revival, 6);
                        a.mon = Some(r);
                        a.switch_to = Some(self.active_at(side, to as usize));
                        self.mon_mut(r).switch_flag = false;
                        self.mon_mut(r).switch_move = NO_MOVE;
                        self.resolve_speed(&mut a);
                        self.queue.push(a);
                    }
                    Choice::Switch { to } => {
                        let incoming = self.active_at(side, to as usize);
                        let kind = if request == Request::Switch { ActKind::InstaSwitch } else { ActKind::Switch };
                        let a = self.resolve_switch(kind, r, incoming);
                        self.queue.push(a);
                    }
                }
            }
        }
        self.sort_queue();
        for a in old.as_slice() {
            self.queue.push(*a);
        }
        self.turn_loop();
        Ok(())
    }
}
