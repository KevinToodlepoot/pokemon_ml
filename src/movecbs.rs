//! Script callbacks of moves (`onTry`, `onHit`, `basePowerCallback`, ...),
//! ported from Showdown's `data/moves.ts` with the Champions overrides.
//!
//! Which move has which callback comes from the generated move table
//! (`MoveData::events`); this file holds the bodies, one arm per callback,
//! named in the comment above it together with its parameter list. Inside a
//! callback `self.event` holds what Showdown passes as arguments.

#![allow(clippy::collapsible_if)]

use crate::battle::modify;
use crate::battle::types_copied;
use crate::battle::{Taken, div1, round_div};
use crate::data::*;
use crate::state::*;

/// The entry hazards, in the order Showdown's moves clear them.
const HAZARDS: [SideCond; 4] = [SideCond::Spikes, SideCond::Toxicspikes, SideCond::Stealthrock, SideCond::Stickyweb];
const SCREENS: [SideCond; 3] = [SideCond::Reflect, SideCond::Lightscreen, SideCond::Auroraveil];

impl Battle {
    /// Rapid Spin and Mortal Spin's clean-up of the user's side:
    /// onAfterHit(target, pokemon, move) and onAfterSubDamage(damage, target, pokemon, move).
    fn spin_away(&mut self, mi: u8, pokemon: MonRef, need_hp: bool) {
        if self.am[mi as usize].has_sheer_force || (need_hp && self.mon(pokemon).hp == 0) {
            return;
        }
        if let Some(k) = VolKind::named("leechseed") {
            self.remove_volatile(pokemon, k);
        }
        for cond in HAZARDS {
            self.remove_side_condition(pokemon.side as usize, cond);
        }
        if let Some(k) = VolKind::named("partiallytrapped") {
            self.remove_volatile(pokemon, k);
        }
    }

    /// `move.beforeTurnCallback(pokemon, target)`. Showdown calls it directly
    /// rather than through an event, so nothing is in the event context.
    pub(crate) fn before_turn_callback(&mut self, move_id: u16, pokemon: MonRef, _target: MonRef) {
        match move_id {
            // Counter, Mirror Coat: start watching for the hit to return.
            mv::COUNTER => {
                self.add_volatile(pokemon, VolKind::Counter, None, Eff::None);
            }
            mv::MIRRORCOAT => {
                self.add_volatile(pokemon, VolKind::Mirrorcoat, None, Eff::None);
            }
            _ => unreachable!("no beforeTurnCallback body for {}", MOVES[move_id as usize].id),
        }
    }

    /// `move.priorityChargeCallback(pokemon)`, also called directly.
    pub(crate) fn priority_charge_callback(&mut self, move_id: u16, pokemon: MonRef) {
        match move_id {
            // Focus Punch tightens its focus, Beak Blast heats up.
            mv::FOCUSPUNCH => {
                self.add_volatile(pokemon, VolKind::Focuspunch, None, Eff::None);
            }
            mv::BEAKBLAST => {
                self.add_volatile(pokemon, VolKind::Beakblast, None, Eff::None);
            }
            // Chilly Reception: the user is seen getting ready to tell its joke.
            mv::CHILLYRECEPTION => {
                self.add_volatile(pokemon, VolKind::Chillyreception, None, Eff::None);
            }
            _ => unreachable!("no priorityChargeCallback body for {}", MOVES[move_id as usize].id),
        }
    }

    /// `move.beforeMoveCallback(pokemon, target, move)`: true means the move does not happen.
    pub(crate) fn before_move_callback(&mut self, mi: u8, pokemon: MonRef) -> bool {
        match self.am[mi as usize].id {
            // Focus Punch: the focus was lost if anything hit the user this turn.
            mv::FOCUSPUNCH => self.vols(pokemon).get(VolKind::Focuspunch).is_some_and(|v| v.data != 0),
            id => unreachable!("no beforeMoveCallback body for {}", MOVES[id as usize].id),
        }
    }

    /// The crash of a move that failed to land: onMoveFail(target, source, move).
    fn crash(&mut self, source: Option<MonRef>, amount: i32) {
        if let Some(source) = source {
            self.damage(amount, Some(source), Some(source), Eff::Crash);
        }
    }

    /// Runs one script callback of the move in slot `mi`.
    pub(crate) fn move_cb(&mut self, mi: u8, ev: Ev) -> Res {
        let e = self.event;
        let id = self.am[mi as usize].id;
        match (id, ev) {
            // ---- Protect, Detect and the other moves that fail more often when repeated
            // onPrepareHit(pokemon)
            (
                mv::PROTECT | mv::DETECT | mv::ENDURE | mv::BANEFULBUNKER | mv::KINGSSHIELD | mv::SPIKYSHIELD,
                Ev::PrepareHit,
            ) => {
                if !self.will_act() {
                    return FALSE;
                }
                self.run_event(Ev::StallMove, e.target, None, Eff::None, Res::Undef)
            }
            // onHit(pokemon)
            (
                mv::PROTECT | mv::DETECT | mv::ENDURE | mv::BANEFULBUNKER | mv::KINGSSHIELD | mv::SPIKYSHIELD,
                Ev::Hit,
            ) => {
                if let Some(t) = e.target {
                    self.add_volatile(t, VolKind::Stall, None, Eff::None);
                }
                Res::Undef
            }

            // ---- Wide Guard, Quick Guard
            // onTry(): fails if nothing else is left to act this turn.
            (mv::WIDEGUARD | mv::QUICKGUARD, Ev::Try) => Res::Bool(self.will_act()),
            // onHitSide(side, source): counts as a use of Protect for the user's next one.
            (mv::WIDEGUARD | mv::QUICKGUARD, Ev::HitSide) => {
                if let Some(source) = e.source {
                    self.add_volatile(source, VolKind::Stall, None, Eff::None);
                }
                Res::Undef
            }

            // ---- Fake Out, First Impression: only on the user's first turn out.
            // onTry(source)
            (mv::FAKEOUT | mv::FIRSTIMPRESSION, Ev::Try) => {
                if e.target.is_some_and(|source| self.mon(source).active_move_actions > 1) {
                    return FALSE;
                }
                Res::Undef
            }
            // onDisableMove(pokemon)
            (mv::FAKEOUT | mv::FIRSTIMPRESSION, Ev::DisableMove) => {
                if let Some(pokemon) = e.target {
                    if self.mon(pokemon).active_move_actions > 0 {
                        self.disable_slots_where(pokemon, |s| s.id == id);
                    }
                }
                Res::Undef
            }

            // ---- Follow Me, Rage Powder: onTry(source). Fail in singles only.
            (mv::FOLLOWME | mv::RAGEPOWDER, Ev::Try) => TRUE,

            // ---- Helping Hand: onTryHit(target). The ally must still have a move to make.
            (mv::HELPINGHAND, Ev::TryHit) => {
                if e.target.is_some_and(|t| !self.mon(t).newly_switched && !self.will_move(t)) {
                    return FALSE;
                }
                Res::Undef
            }

            // ---- Disable: onTryHit(target). The target must have used a move.
            (mv::DISABLE, Ev::TryHit) => {
                if e.target.is_some_and(|t| matches!(self.mon(t).last_move, NO_MOVE | mv::STRUGGLE)) {
                    return FALSE;
                }
                Res::Undef
            }

            // ============================================ base power that depends on the battle
            // basePowerCallback(pokemon, target, move): `e.target` is the user, `e.source` its target.
            (
                mv::ACROBATICS
                | mv::ASSURANCE
                | mv::AVALANCHE
                | mv::HEX
                | mv::INFERNALPARADE
                | mv::PAYBACK
                | mv::STOMPINGTANTRUM
                | mv::TEMPERFLARE,
                Ev::BasePowerCallback,
            ) => {
                let (Some(pokemon), Some(target)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                let doubled = match id {
                    // No held item.
                    mv::ACROBATICS => self.mon(pokemon).item == it::NONE,
                    // The target has already been hurt this turn.
                    mv::ASSURANCE => self.mon(target).hurt_this_turn != 0,
                    // The target has damaged the user this turn.
                    mv::AVALANCHE => {
                        let bit = 1u16 << (target.side as usize * MAX_TEAM + target.idx as usize);
                        self.mon(pokemon).hit_by_this_turn & bit != 0
                    }
                    // The target has a status condition.
                    mv::HEX | mv::INFERNALPARADE => {
                        self.mon(target).status != Status::None || self.has_ability(target, ab::COMATOSE)
                    }
                    // The target has already moved this turn.
                    mv::PAYBACK => !(self.mon(target).newly_switched || self.will_move(target)),
                    // The user's last move failed.
                    _ => self.mon(pokemon).move_last_turn == FALSE,
                };
                let bp = self.am[mi as usize].base_power as i32;
                Res::Num(if doubled { bp * 2 } else { bp })
            }
            // Electro Ball: the faster the user is than the target.
            (mv::ELECTROBALL, Ev::BasePowerCallback) => {
                let (Some(pokemon), Some(target)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                let (mine, theirs) = (self.get_stat(pokemon, 5, false, false), self.get_stat(target, 5, false, false));
                let ratio = mine.checked_div(theirs).map_or(0, |q| q.min(4));
                Res::Num([40, 60, 80, 120, 150][ratio as usize])
            }
            // Gyro Ball: the slower the user is than the target.
            (mv::GYROBALL, Ev::BasePowerCallback) => {
                let (Some(pokemon), Some(target)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                let (mine, theirs) = (self.get_stat(pokemon, 5, false, false), self.get_stat(target, 5, false, false));
                let power = (25 * theirs).checked_div(mine).map_or(1, |q| (q + 1).min(150));
                Res::Num(power as i32)
            }
            // Eruption, Water Spout: in proportion to the user's HP (never less than 1).
            (mv::ERUPTION | mv::WATERSPOUT, Ev::BasePowerCallback) => {
                let Some(pokemon) = e.target else {
                    return Res::Undef;
                };
                let m = self.mon(pokemon);
                let bp = self.am[mi as usize].base_power as u32 * m.hp as u32 / m.max_hp() as u32;
                Res::Num(bp.max(1) as i32)
            }
            // Flail, Reversal: the less HP the user has.
            (mv::FLAIL | mv::REVERSAL, Ev::BasePowerCallback) => {
                let Some(pokemon) = e.target else {
                    return Res::Undef;
                };
                let m = self.mon(pokemon);
                let ratio = (m.hp as u32 * 48 / m.max_hp() as u32).max(1);
                Res::Num(match ratio {
                    0..=1 => 200,
                    2..=4 => 150,
                    5..=9 => 100,
                    10..=16 => 80,
                    17..=32 => 40,
                    _ => 20,
                })
            }
            // Hard Press: the more HP the target has, up to 100.
            (mv::HARDPRESS, Ev::BasePowerCallback) => {
                let Some(target) = e.source else {
                    return Res::Undef;
                };
                let m = self.mon(target);
                let part = m.hp as u32 * 4096 / m.max_hp() as u32;
                let bp = (100 * (100 * part) + 2048 - 1) / 4096 / 100;
                Res::Num(bp.max(1) as i32)
            }
            // Last Respects: 50 more for every ally that has fainted.
            (mv::LASTRESPECTS, Ev::BasePowerCallback) => match e.target {
                Some(pokemon) => Res::Num(50 + 50 * self.sides[pokemon.side as usize].total_fainted as i32),
                None => Res::Undef,
            },
            // Power Trip, Stored Power: 20 more for every stage the user's stats are raised.
            (mv::POWERTRIP | mv::STOREDPOWER, Ev::BasePowerCallback) => match e.target {
                Some(pokemon) => Res::Num(self.am[mi as usize].base_power as i32 + 20 * self.positive_boosts(pokemon)),
                None => Res::Undef,
            },
            // Rage Fist: 50 more for every hit the user has taken.
            (mv::RAGEFIST, Ev::BasePowerCallback) => match e.target {
                Some(pokemon) => Res::Num((50 + 50 * self.mon(pokemon).times_attacked as i32).min(350)),
                None => Res::Undef,
            },
            // Water Shuriken: its bonus is for Ash-Greninja, which is not in Champions.
            (mv::WATERSHURIKEN, Ev::BasePowerCallback) => Res::Num(self.am[mi as usize].base_power as i32),
            // Grass Knot, Low Kick: the heavier the target.
            (mv::GRASSKNOT | mv::LOWKICK, Ev::BasePowerCallback) => {
                let Some(target) = e.source else {
                    return Res::Undef;
                };
                Res::Num(match self.get_weight(target) {
                    2000.. => 120,
                    1000.. => 100,
                    500.. => 80,
                    250.. => 60,
                    100.. => 40,
                    _ => 20,
                })
            }
            // Heat Crash, Heavy Slam: the heavier the user is than the target.
            (mv::HEATCRASH | mv::HEAVYSLAM, Ev::BasePowerCallback) => {
                let (Some(pokemon), Some(target)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                let theirs = self.get_weight(target);
                let mine = self.get_weight(pokemon);
                Res::Num(if mine >= theirs * 5 {
                    120
                } else if mine >= theirs * 4 {
                    100
                } else if mine >= theirs * 3 {
                    80
                } else if mine >= theirs * 2 {
                    60
                } else {
                    40
                })
            }
            // onTryHit of the weight moves only fails against Dynamax.
            (mv::GRASSKNOT | mv::LOWKICK | mv::HEATCRASH | mv::HEAVYSLAM, Ev::TryHit) => Res::Undef,

            // onBasePower(basePower, pokemon, target): doubled (or more) in the right circumstances.
            // Barb Barrage, Venoshock: against a poisoned target.
            (mv::BARBBARRAGE | mv::VENOSHOCK, Ev::BasePower) => {
                if e.source.is_some_and(|t| matches!(self.mon(t).status, Status::Psn | Status::Tox)) {
                    return self.chain_modify(2, 1);
                }
                Res::Undef
            }
            // Facade: with a status other than sleep.
            (mv::FACADE, Ev::BasePower) => {
                if e.target.is_some_and(|p| !matches!(self.mon(p).status, Status::None | Status::Slp)) {
                    return self.chain_modify(2, 1);
                }
                Res::Undef
            }
            // Fickle Beam: three times in ten.
            (mv::FICKLEBEAM, Ev::BasePower) => {
                if self.chance(3, 10, "fickle beam") {
                    return self.chain_modify(2, 1);
                }
                Res::Undef
            }
            // Grav Apple: under Gravity.
            (mv::GRAVAPPLE, Ev::BasePower) => {
                if self.field.pseudo.has(Pseudo::Gravity) {
                    return self.chain_modify(3, 2);
                }
                Res::Undef
            }
            // Lash Out: if the user's stats were lowered this turn.
            (mv::LASHOUT, Ev::BasePower) => {
                if e.target.is_some_and(|p| self.mon(p).stats_lowered_this_turn) {
                    return self.chain_modify(2, 1);
                }
                Res::Undef
            }

            // Burning Jealousy, Alluring Voice: their secondary's onHit(target, source, move)
            // punishes a target whose stats went up this turn.
            (mv::BURNINGJEALOUSY | mv::ALLURINGVOICE, Ev::SecondaryHit) => {
                let Some(target) = e.target else {
                    return Res::Undef;
                };
                if self.mon(target).stats_raised_this_turn {
                    if id == mv::BURNINGJEALOUSY {
                        self.try_set_status(target, Status::Brn, e.source, Eff::Move(mi));
                    } else {
                        self.add_volatile(target, VolKind::Confusion, e.source, Eff::Move(mi));
                    }
                }
                Res::Undef
            }
            // Fell Stinger: onAfterMoveSecondarySelf(pokemon, target, move). +3 Attack for a knockout.
            (mv::FELLSTINGER, Ev::AfterMoveSecondarySelf) => {
                if let (Some(pokemon), Some(target)) = (e.target, e.source) {
                    if self.mon(target).fainted || self.mon(target).hp == 0 {
                        self.boost1(ATK, 3, Some(pokemon), Some(pokemon), Eff::Move(mi));
                    }
                }
                Res::Undef
            }

            // ============================================ HP, stat stages and PP
            // ---- Heal Bell: onHit(target, source). Cures the whole team, benched included,
            // except allies that cannot hear it.
            (mv::HEALBELL, Ev::Hit) => {
                let (Some(target), Some(source)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                let side = target.side;
                let mut success = false;
                for idx in 0..self.sides[side as usize].n {
                    let ally = MonRef { side, idx };
                    let ignored = self.am[mi as usize].ignore_ability && self.active_pokemon != Some(ally);
                    if ally != source && !ignored {
                        // `-immune|ally|[from] ability: Soundproof`
                        if let Some(deaf) =
                            [ab::SOUNDPROOF, ab::GOODASGOLD].into_iter().find(|&a| self.has_ability(ally, a))
                        {
                            self.show_ability(ally, deaf);
                            continue;
                        }
                    }
                    success |= self.cure_status(ally);
                }
                Res::Bool(success)
            }
            // ---- Heal Pulse: onHit(target, source). Half the target's HP, more with Mega Launcher.
            (mv::HEALPULSE, Ev::Hit) => {
                let (Some(target), Some(source)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                let max = self.mon(target).max_hp() as u32;
                let amount =
                    if self.has_ability(source, ab::MEGALAUNCHER) { modify(max, 3072) } else { max.div_ceil(2) };
                if self.heal(amount as i32, None, None, Eff::None).truthy() { TRUE } else { Res::NotFail }
            }
            // ---- Pollen Puff: heals an ally instead of hurting it.
            // onTryHit(target, source, move)
            (mv::POLLENPUFF, Ev::TryHit) => {
                if let (Some(target), Some(source)) = (e.target, e.source) {
                    if self.is_ally(source, target) {
                        self.am[mi as usize].base_power = 0;
                        self.am[mi as usize].infiltrates = true;
                    }
                }
                Res::Undef
            }
            // onTryMove(source, target, move): not while the user's healing is blocked.
            (mv::POLLENPUFF, Ev::TryMove) => {
                if let (Some(source), Some(target)) = (e.target, e.source) {
                    if self.is_ally(source, target) && self.vols(source).has(VolKind::Healblock) {
                        return FALSE;
                    }
                }
                Res::Undef
            }
            // onHit(target, source, move)
            (mv::POLLENPUFF, Ev::Hit) => {
                if let (Some(target), Some(source)) = (e.target, e.source) {
                    if self.is_ally(source, target) {
                        let amount = self.mon(target).max_hp() as i32 / 2;
                        if !self.heal(amount, None, None, Eff::None).truthy() {
                            return Res::NotFail;
                        }
                    }
                }
                Res::Undef
            }
            // ---- Pain Split: onHit(target, pokemon). Both end up with the average of their HP.
            (mv::PAINSPLIT, Ev::Hit) => {
                let (Some(target), Some(pokemon)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                let (theirs, mine) = (self.mon(target).hp as i32, self.mon(pokemon).hp as i32);
                let average = ((theirs + mine) / 2).max(1);
                self.set_hp(target, average);
                self.set_hp(pokemon, average);
                Res::Undef
            }
            // ---- Strength Sap: onHit(target, source). Heals by the target's Attack, then lowers it.
            (mv::STRENGTHSAP, Ev::Hit) => {
                let (Some(target), Some(source)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                if self.mon(target).boosts[ATK] == -6 {
                    return FALSE;
                }
                let atk = self.get_stat(target, 1, false, true);
                let lowered = self.boost1(ATK, -1, Some(target), Some(source), Eff::None).truthy();
                let healed = self.heal(atk as i32, Some(source), Some(target), Eff::None).truthy();
                Res::Bool(healed || lowered)
            }
            // ---- Belly Drum: onHit(target). Half its HP for maximum Attack.
            (mv::BELLYDRUM, Ev::Hit) => {
                let Some(target) = e.target else {
                    return Res::Undef;
                };
                let m = self.mon(target);
                if m.hp as u32 * 2 <= m.max_hp() as u32 || m.boosts[ATK] >= 6 || m.max_hp() == 1 {
                    return FALSE;
                }
                let cost = m.max_hp() as i32 / 2;
                self.direct_damage(cost, target, e.source, Eff::Move(mi));
                self.boost1(ATK, 12, Some(target), None, Eff::None);
                Res::Undef
            }
            // ---- Clangorous Soul: a third of its HP for +1 in everything.
            // onTry(source)
            (mv::CLANGOROUSSOUL, Ev::Try) => {
                if let Some(source) = e.target {
                    let m = self.mon(source);
                    if m.hp as u32 * 100 <= m.max_hp() as u32 * 33 || m.max_hp() == 1 {
                        return FALSE;
                    }
                }
                Res::Undef
            }
            // onTryHit(pokemon, target, move): the boosts come first; without them, no cost.
            (mv::CLANGOROUSSOUL, Ev::TryHit) => {
                let Some(boosts) = self.am[mi as usize].boosts else {
                    return Res::Undef;
                };
                if !self.boost(boosts, None, None, Eff::None).truthy() {
                    return Res::Null;
                }
                self.am[mi as usize].boosts = None;
                Res::Undef
            }
            // onHit(pokemon)
            (mv::CLANGOROUSSOUL, Ev::Hit) => {
                if let Some(pokemon) = e.target {
                    let cost = self.mon(pokemon).max_hp() as i32 * 33 / 100;
                    self.direct_damage(cost, pokemon, e.source, Eff::Move(mi));
                }
                Res::Undef
            }
            // ---- Acupressure: onHit(target). +2 to a random stat that can still go up.
            (mv::ACUPRESSURE, Ev::Hit) => {
                let Some(target) = e.target else {
                    return Res::Undef;
                };
                let boosts = self.mon(target).boosts;
                let open: Vec<usize> = (0..7).filter(|&k| boosts[k] < 6).collect();
                if open.is_empty() {
                    return FALSE;
                }
                let stat = open[self.rand(open.len() as u32, "acupressure") as usize];
                self.boost1(stat, 2, None, None, Eff::None);
                Res::Undef
            }
            // ---- Psych Up: onHit(target, source). Copies the target's stat stages.
            (mv::PSYCHUP, Ev::Hit) => {
                let (Some(target), Some(source)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                self.mon_mut(source).boosts = self.mon(target).boosts;
                for kind in [VolKind::Dragoncheer, VolKind::Focusenergy] {
                    self.remove_volatile(source, kind);
                }
                for kind in [VolKind::Dragoncheer, VolKind::Focusenergy] {
                    if let Some(theirs) = self.vols(target).get(kind).map(|v| v.data) {
                        self.add_volatile(source, kind, None, Eff::None);
                        if kind == VolKind::Dragoncheer {
                            if let Some(v) = self.vol_mut(source, kind) {
                                v.data = theirs;
                            }
                        }
                    }
                }
                Res::Undef
            }
            // ---- Topsy-Turvy: onHit(target). Every stat stage changes sign.
            (mv::TOPSYTURVY, Ev::Hit) => {
                let Some(target) = e.target else {
                    return Res::Undef;
                };
                let m = self.mon_mut(target);
                if m.boosts.iter().all(|&b| b == 0) {
                    return FALSE;
                }
                for b in m.boosts.iter_mut() {
                    *b = -*b;
                }
                Res::Undef
            }
            // ---- Clear Smog: onHit(target). Resets the target's stat stages.
            (mv::CLEARSMOG, Ev::Hit) => {
                if let Some(target) = e.target {
                    self.mon_mut(target).boosts = [0; 7];
                }
                Res::Undef
            }
            // ---- Guard Split, Power Split: onHit(target, source). Average the two stats.
            (mv::GUARDSPLIT | mv::POWERSPLIT, Ev::Hit) => {
                let (Some(target), Some(source)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                let stats = if id == mv::GUARDSPLIT { [DEF + 1, SPD + 1] } else { [ATK + 1, SPA + 1] };
                for k in stats {
                    let average = (self.mon(target).stats[k] as u32 + self.mon(source).stats[k] as u32) / 2;
                    self.mon_mut(target).stats[k] = average as u16;
                    self.mon_mut(source).stats[k] = average as u16;
                }
                Res::Undef
            }
            // ---- Guard Swap, Power Swap: onHit(target, source). Trade those stat stages.
            (mv::GUARDSWAP | mv::POWERSWAP, Ev::Hit) => {
                let (Some(target), Some(source)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                let stats = if id == mv::GUARDSWAP { [DEF, SPD] } else { [ATK, SPA] };
                for k in stats {
                    let (theirs, mine) = (self.mon(target).boosts[k], self.mon(source).boosts[k]);
                    self.mon_mut(source).boosts[k] = theirs;
                    self.mon_mut(target).boosts[k] = mine;
                }
                Res::Undef
            }
            // ---- Speed Swap: onHit(target, source). Trade the Speed stats themselves.
            (mv::SPEEDSWAP, Ev::Hit) => {
                if let (Some(target), Some(source)) = (e.target, e.source) {
                    self.note_speed_swap(source, target);
                    let (theirs, mine) = (self.mon(target).stats[5], self.mon(source).stats[5]);
                    self.mon_mut(target).stats[5] = mine;
                    self.mon_mut(source).stats[5] = theirs;
                }
                Res::Undef
            }
            // ---- Spite: onHit(target). Four PP off the target's last move.
            (mv::SPITE, Ev::Hit) => {
                let Some(target) = e.target else {
                    return Res::Undef;
                };
                let last = self.mon(target).last_move;
                if last == NO_MOVE || self.deduct_pp(target, last, 4) == 0 {
                    return FALSE;
                }
                // `-activate|target|move: Spite|Move|4`
                self.show_move(target, last);
                Res::Undef
            }
            // ---- Eerie Spell: its secondary's onHit(target). Three PP off the target's last move.
            (mv::EERIESPELL, Ev::SecondaryHit) => {
                if let Some(target) = e.target {
                    let last = self.mon(target).last_move;
                    if self.mon(target).hp > 0 && last != NO_MOVE && self.deduct_pp(target, last, 3) != 0 {
                        // `-activate|target|move: Eerie Spell|Move|3`
                        self.show_move(target, last);
                    }
                }
                Res::Undef
            }
            // ---- Rest
            // onTry(source): not if asleep, at full HP, or unable to sleep.
            (mv::REST, Ev::Try) => {
                let Some(source) = e.target else {
                    return Res::Undef;
                };
                let m = self.mon(source);
                if m.status == Status::Slp || self.has_ability(source, ab::COMATOSE) {
                    return FALSE;
                }
                if m.hp == m.max_hp() {
                    return Res::Null;
                }
                // `-fail|source|[from] ability: Insomnia`
                if let Some(awake) = [ab::INSOMNIA, ab::VITALSPIRIT].into_iter().find(|&a| self.has_ability(source, a))
                {
                    self.show_ability(source, awake);
                    return Res::Null;
                }
                Res::Undef
            }
            // onHit(target, source, move): asleep for exactly three turns, and fully healed.
            (mv::REST, Ev::Hit) => {
                let Some(target) = e.target else {
                    return Res::Undef;
                };
                let result = self.set_status(target, Status::Slp, e.source, Eff::Move(mi));
                if !result.truthy() {
                    return result;
                }
                self.mon_mut(target).status_time = 3;
                self.note_count_began(self.mon(target).status_st.uid, 3, true);
                let max = self.mon(target).max_hp() as i32;
                self.heal(max, None, None, Eff::None);
                Res::Undef
            }
            // ---- Dire Claw, Tri Attack: their secondary's onHit(target, source). One of three statuses.
            (mv::DIRECLAW | mv::TRIATTACK, Ev::SecondaryHit) => {
                let Some(target) = e.target else {
                    return Res::Undef;
                };
                let options = if id == mv::DIRECLAW {
                    [Status::Psn, Status::Par, Status::Slp]
                } else {
                    [Status::Brn, Status::Par, Status::Frz]
                };
                let status = options[self.rand(3, "random status") as usize];
                self.try_set_status(target, status, e.source, Eff::Secondary);
                Res::Undef
            }

            // ============================================ abilities changed by moves
            // ---- Skill Swap: onHit(target, source, move)
            (mv::SKILLSWAP, Ev::Hit) => match (e.target, e.source) {
                (Some(target), Some(source)) => Res::Bool(self.skill_swap(source, target, false)),
                _ => Res::Undef,
            },
            // ---- Entrainment: the target gets the user's ability.
            // onTryHit(target, source)
            (mv::ENTRAINMENT, Ev::TryHit) => {
                let (Some(target), Some(source)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                let (theirs, mine) = (self.mon(target).ability, self.mon(source).ability);
                if target == source
                    || theirs == mine
                    || ABILITIES[theirs as usize].flags & AF_CANTSUPPRESS != 0
                    || ABILITIES[theirs as usize].id == "truant"
                    || ABILITIES[mine as usize].flags & AF_NOENTRAIN != 0
                {
                    return FALSE;
                }
                Res::Undef
            }
            // onHit(target, source)
            (mv::ENTRAINMENT, Ev::Hit) => {
                let (Some(target), Some(source)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                let ability = self.mon(source).ability;
                self.set_ability_result(target, ability, Some(source))
            }
            // ---- Role Play: the user gets the target's ability.
            // onTryHit(target, source)
            (mv::ROLEPLAY, Ev::TryHit) => {
                let (Some(target), Some(source)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                let (theirs, mine) = (self.mon(target).ability, self.mon(source).ability);
                if theirs == mine
                    || ABILITIES[theirs as usize].flags & AF_FAILROLEPLAY != 0
                    || ABILITIES[mine as usize].flags & AF_CANTSUPPRESS != 0
                {
                    return FALSE;
                }
                Res::Undef
            }
            // onHit(target, source)
            (mv::ROLEPLAY, Ev::Hit) => {
                let (Some(target), Some(source)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                let ability = self.mon(target).ability;
                self.set_ability_result(source, ability, Some(target))
            }
            // ---- Simple Beam, Worry Seed: the target's ability becomes Simple or Insomnia.
            // onTryHit(target)
            (mv::SIMPLEBEAM | mv::WORRYSEED, Ev::TryHit) => {
                let Some(target) = e.target else {
                    return Res::Undef;
                };
                let theirs = &ABILITIES[self.mon(target).ability as usize];
                if theirs.flags & AF_CANTSUPPRESS != 0
                    || (id == mv::SIMPLEBEAM && matches!(theirs.id, "simple" | "truant"))
                {
                    return FALSE;
                }
                Res::Undef
            }
            // Worry Seed: onTryImmunity(target)
            (mv::WORRYSEED, Ev::TryImmunity) => {
                let immune = e
                    .target
                    .is_some_and(|t| matches!(ABILITIES[self.mon(t).ability as usize].id, "truant" | "insomnia"));
                Res::Bool(!immune)
            }
            // onHit(target, source)
            (mv::SIMPLEBEAM | mv::WORRYSEED, Ev::Hit) => {
                let Some(target) = e.target else {
                    return Res::Undef;
                };
                let ability = if id == mv::SIMPLEBEAM { ab::SIMPLE } else { ab::INSOMNIA };
                let result = self.set_ability_result(target, ability, None);
                if result != Res::Undef {
                    return result;
                }
                if id == mv::WORRYSEED && self.mon(target).status == Status::Slp {
                    self.cure_status(target);
                }
                Res::Undef
            }

            // ============================================ types changed by moves
            // ---- Soak, Magic Powder: onHit(target). The target becomes pure Water or Psychic.
            (mv::SOAK | mv::MAGICPOWDER, Ev::Hit) => {
                let Some(target) = e.target else {
                    return Res::Undef;
                };
                let t = if id == mv::SOAK { Type::Water } else { Type::Psychic };
                if self.is_only_type(target, t) || !self.set_type(target, [t, Type::None]) {
                    return if id == mv::SOAK { Res::Null } else { FALSE };
                }
                Res::Undef
            }
            // ---- Forest's Curse, Trick-or-Treat: onHit(target). Adds Grass or Ghost to the target's types.
            (mv::FORESTSCURSE | mv::TRICKORTREAT, Ev::Hit) => {
                let Some(target) = e.target else {
                    return Res::Undef;
                };
                let t = if id == mv::FORESTSCURSE { Type::Grass } else { Type::Ghost };
                if self.has_type(target, t) {
                    return FALSE;
                }
                self.mon_mut(target).added_type = t;
                // A Curse the new Ghost has queued from the right-hand slot is re-aimed.
                if id == mv::TRICKORTREAT && self.mon(target).position == 1 && !self.mon(target).fainted {
                    let q = &mut self.queue;
                    for k in 0..q.len as usize {
                        let a = &mut q.items[k];
                        if a.kind == ActKind::Move && a.mon == Some(target) {
                            if MOVES[a.move_id as usize].id == "curse" {
                                a.target_loc = -1;
                            }
                            break;
                        }
                    }
                }
                Res::Undef
            }
            // ---- Reflect Type: onHit(target, source). The user takes on the target's types.
            (mv::REFLECTTYPE, Ev::Hit) => {
                let (Some(target), Some(source)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                let (types, n) = self.get_types(target, true);
                let mut new = [Type::None; 2];
                let mut k = 0;
                for &t in &types[..n] {
                    if t != Type::Typeless && k < 2 {
                        new[k] = t;
                        k += 1;
                    }
                }
                let added = self.mon(target).added_type;
                if k == 0 {
                    if added == Type::None {
                        return FALSE;
                    }
                    new[0] = Type::Normal;
                }
                // `-start|source|typechange|[from] move: Reflect Type|[of] target`: the log
                // names the Pokémon, not the types. Each side takes them for the ones it
                // sees on the target, which for a disguise only its own side sees rightly.
                let roosting = self.vols(target).has(VolKind::Roost);
                let seen = [0, 1].map(|viewer| types_copied(self.seen_base(target, viewer), roosting));
                let before = self.seen_base(source, source.side as usize);
                let was = self.mon(source).said;
                self.set_type(source, new);
                // (`source.knownType = target.isAlly(source) && target.knownType`, and
                // while that is off Showdown's own note of its types stays as it was.)
                let ally = source.side == target.side;
                let known = ally && self.mon(target).said.known;
                let m = self.mon_mut(source);
                m.added_type = added;
                m.said.seen = seen;
                m.said.known = known;
                if !known {
                    m.said.apparent = was.apparent;
                }
                if !was.copied {
                    m.said.before = before;
                }
                (m.said.copied, m.said.copied_foe) = (true, !ally);
                Res::Undef
            }
            // ---- Burn Up, Double Shock: only a Fire (Electric) type can use it, and it stops being one.
            // onTryMove(pokemon, target, move)
            (mv::BURNUP | mv::DOUBLESHOCK, Ev::TryMove) => {
                let t = if id == mv::BURNUP { Type::Fire } else { Type::Electric };
                if e.target.is_some_and(|p| self.has_type(p, t)) {
                    return Res::Undef;
                }
                Res::Null
            }
            // self.onHit(pokemon): that type becomes "???".
            (mv::BURNUP | mv::DOUBLESHOCK, Ev::SelfHit) => {
                let Some(pokemon) = e.target else {
                    return Res::Undef;
                };
                let lost = if id == mv::BURNUP { Type::Fire } else { Type::Electric };
                let (types, n) = self.get_types(pokemon, true);
                let mut new = [Type::None; 2];
                for (k, &t) in types[..n.min(2)].iter().enumerate() {
                    new[k] = if t == lost { Type::Typeless } else { t };
                }
                self.set_type(pokemon, new);
                Res::Undef
            }

            // ============================================ moves that take, swap or eat items
            // ---- Knock Off
            // onBasePower(basePower, source, target, move): stronger if there is an item to remove.
            (mv::KNOCKOFF, Ev::BasePower) => {
                let Some(target) = e.source else {
                    return Res::Undef;
                };
                let item = self.mon(target).item;
                if !self.item_lets_go(item, target, target, mi) {
                    return Res::Undef;
                }
                if item != it::NONE {
                    return self.chain_modify(3, 2);
                }
                Res::Undef
            }
            // onAfterHit(target, source)
            (mv::KNOCKOFF, Ev::AfterHit) => {
                if let Some(target) = e.target {
                    let item = self.take_item(target, None);
                    // `-enditem|target|Item|[from] move: Knock Off`
                    self.show_item_lost(target, item);
                }
                Res::Undef
            }
            // ---- Thief, Covet: onAfterHit(target, source, move). An empty-handed user takes the item.
            (mv::THIEF | mv::COVET, Ev::AfterHit) => {
                let (Some(target), Some(source)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                if self.mon(source).item != it::NONE || self.vols(source).has(VolKind::Gem) {
                    return Res::Undef;
                }
                if let Taken::Item(item) = self.try_take_item(target, Some(source)) {
                    if !self.item_lets_go(item, source, target, mi) || !self.set_item(source, item, None, Eff::None) {
                        // It goes straight back.
                        self.mon_mut(target).item = item;
                    } else {
                        // `-enditem|target|Item|[silent]`, `-item|source|Item|[from] move: Thief|[of] target`
                        self.show_item_lost(target, item);
                        self.show_item_arrived(source, item);
                    }
                }
                Res::Undef
            }
            // ---- Trick, Switcheroo: the two swap items.
            // onTryImmunity(target)
            (mv::TRICK | mv::SWITCHEROO, Ev::TryImmunity) => {
                Res::Bool(e.target.is_some_and(|t| !self.has_ability(t, ab::STICKYHOLD)))
            }
            // onHit(target, source, move)
            (mv::TRICK | mv::SWITCHEROO, Ev::Hit) => {
                let (Some(target), Some(source)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                let yours = self.try_take_item(target, Some(source));
                let mine = self.try_take_item(source, None);
                let (your_item, my_item) = (
                    if let Taken::Item(i) = yours { i } else { it::NONE },
                    if let Taken::Item(i) = mine { i } else { it::NONE },
                );
                let mut fail =
                    yours == Taken::Blocked || mine == Taken::Blocked || (your_item == it::NONE && my_item == it::NONE);
                if !fail {
                    fail = (my_item != it::NONE && !self.item_lets_go(my_item, target, source, mi))
                        || (your_item != it::NONE && !self.item_lets_go(your_item, source, target, mi));
                }
                if fail {
                    if your_item != it::NONE {
                        self.mon_mut(target).item = your_item;
                    }
                    if my_item != it::NONE {
                        self.mon_mut(source).item = my_item;
                    }
                    return FALSE;
                }
                // Each ends up with the other's item (`-item`), or with nothing (`-enditem`).
                // (An item used up the moment it arrives, a White Herb say, has had its `-enditem` by then.)
                if my_item != it::NONE {
                    self.set_item(target, my_item, None, Eff::None);
                    self.show_item_arrived(target, my_item);
                } else {
                    self.show_item_lost(target, your_item);
                }
                if your_item != it::NONE {
                    self.set_item(source, your_item, None, Eff::None);
                    self.show_item_arrived(source, your_item);
                } else {
                    self.show_item_lost(source, my_item);
                }
                Res::Undef
            }
            // ---- Bug Bite, Pluck: onHit(target, source, move). The user eats the target's berry.
            (mv::BUGBITE | mv::PLUCK, Ev::Hit) => {
                let (Some(target), Some(source)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                let item = self.mon(target).item;
                if self.mon(source).hp > 0
                    && ITEMS[item as usize].flags & IF_BERRY != 0
                    && self.try_take_item(target, Some(source)) == Taken::Item(item)
                {
                    // `-enditem|target|Berry|[from] stealeat`
                    self.show_item_lost(target, item);
                    let me = Eff::Move(mi);
                    if self
                        .single_event(
                            Ev::Eat,
                            Eff::Item(item),
                            Some(source),
                            Some(source),
                            Some(source),
                            me,
                            Res::Undef,
                        )
                        .truthy()
                    {
                        let mut ee = Event::new(Ev::EatItem, Some(source), Some(source), me);
                        ee.item = item;
                        self.run_event_ex(ee, Res::Undef, false, false);
                    }
                    if self.has_cb(Eff::Item(item), Ev::Eat) {
                        self.mon_mut(source).ate_berry = true;
                    }
                }
                Res::Undef
            }
            // ---- Corrosive Gas: onHit(target, source). The target's item is gone.
            (mv::CORROSIVEGAS, Ev::Hit) => {
                if let Some(target) = e.target {
                    let item = self.take_item(target, e.source);
                    // `-enditem|target|Item|[from] move: Corrosive Gas`
                    self.show_item_lost(target, item);
                }
                Res::Undef
            }
            // ---- Recycle: onHit(pokemon, source, move). The last item used comes back.
            (mv::RECYCLE, Ev::Hit) => {
                let Some(pokemon) = e.target else {
                    return Res::Undef;
                };
                let last = self.mon(pokemon).last_item;
                if self.mon(pokemon).item != it::NONE || last == it::NONE {
                    return FALSE;
                }
                self.mon_mut(pokemon).last_item = it::NONE;
                // `-item|pokemon|Item|[from] move: Recycle`
                self.show_item_gain(pokemon, last);
                self.set_item(pokemon, last, e.source, Eff::Move(mi));
                Res::Undef
            }
            // ---- Poltergeist: onTry(source, target). The target must hold something.
            (mv::POLTERGEIST, Ev::Try) => Res::Bool(e.source.is_some_and(|t| self.mon(t).item != it::NONE)),
            // onTryHit(target, source, move): `-activate|target|move: Poltergeist|Item`
            (mv::POLTERGEIST, Ev::TryHit) => {
                if let Some(target) = e.target {
                    let item = self.mon(target).item;
                    self.show_item_held(target, item);
                }
                Res::Undef
            }
            // ---- Belch: onTry(source). Only after eating a berry.
            (mv::BELCH, Ev::Try) => Res::Bool(e.target.is_some_and(|s| self.mon(s).ate_berry)),
            // ---- Stuff Cheeks: eat the held berry for +2 Defense.
            // onTry(source)
            (mv::STUFFCHEEKS, Ev::Try) => {
                Res::Bool(e.target.is_some_and(|s| ITEMS[self.mon(s).item as usize].flags & IF_BERRY != 0))
            }
            // onHit(pokemon)
            (mv::STUFFCHEEKS, Ev::Hit) => {
                let Some(pokemon) = e.target else {
                    return Res::Undef;
                };
                if !self.boost1(DEF, 2, None, None, Eff::None).truthy() {
                    return Res::Null;
                }
                self.eat_item(pokemon, true, None, Eff::None);
                Res::Undef
            }
            // ---- Teatime: onHitField(target, source, move). Everyone eats their berry.
            (mv::TEATIME, Ev::HitField) => {
                let me = Eff::Move(mi);
                let (actives, n) = self.all_active(false);
                let mut eaters = [actives[0]; 4];
                let mut k = 0;
                for &pokemon in &actives[..n] {
                    if self.run_event(Ev::Invulnerability, Some(pokemon), e.source, me, Res::Undef) == FALSE {
                        continue;
                    }
                    if self.run_event(Ev::TryHit, Some(pokemon), e.source, me, Res::Undef).truthy()
                        && ITEMS[self.mon(pokemon).item as usize].flags & IF_BERRY != 0
                    {
                        eaters[k] = pokemon;
                        k += 1;
                    }
                }
                if k == 0 {
                    return Res::NotFail;
                }
                for &pokemon in &eaters[..k] {
                    self.eat_item(pokemon, true, None, Eff::None);
                }
                Res::Undef
            }

            // ============================================ assorted
            // ---- Sucker Punch, Upper Hand: onTry(source, target). Only against a
            // target that is about to attack (with priority, for Upper Hand).
            (mv::SUCKERPUNCH | mv::UPPERHAND, Ev::Try) => {
                let Some(target) = e.source else {
                    return FALSE;
                };
                let queued = if self.mon(target).fainted {
                    None
                } else {
                    self.queue.as_slice().iter().find(|a| a.kind == ActKind::Move && a.mon == Some(target)).copied()
                };
                let Some(action) = queued else {
                    return FALSE;
                };
                let theirs = &MOVES[action.move_id as usize];
                let fails = if id == mv::SUCKERPUNCH {
                    (theirs.category == Category::Status && theirs.id != "mefirst")
                        || self.vols(target).has(VolKind::Mustrecharge)
                } else {
                    action.move_priority <= 0 || theirs.category == Category::Status
                };
                if fails {
                    return FALSE;
                }
                Res::Undef
            }
            // ---- Last Resort: onTry(source). Only once every other move has been used.
            (mv::LASTRESORT, Ev::Try) => {
                let Some(source) = e.target else {
                    return FALSE;
                };
                let m = self.mon(source);
                let slots = &m.moves[..m.n_moves as usize];
                if slots.len() < 2 {
                    return FALSE;
                }
                let mut has_it = false;
                for s in slots {
                    if s.id == mv::LASTRESORT {
                        has_it = true;
                    } else if !s.used {
                        return FALSE;
                    }
                }
                Res::Bool(has_it)
            }
            // ---- Flying Press: onEffectiveness(typeMod, target, type, move). Fighting and Flying at once.
            (mv::FLYINGPRESS, Ev::Effectiveness) => {
                Res::Num(e.relay.num() + crate::battle::type_effectiveness(Type::Flying, e.typ))
            }
            // ---- Freeze-Dry: onEffectiveness(typeMod, target, type). Super effective on Water.
            (mv::FREEZEDRY, Ev::Effectiveness) => {
                if e.typ == Type::Water {
                    return Res::Num(1);
                }
                Res::Undef
            }
            // ---- Raging Bull
            // onTryHit(pokemon): breaks the target's screens.
            (mv::RAGINGBULL, Ev::TryHit) => {
                if let Some(pokemon) = e.target {
                    for screen in SCREENS {
                        self.remove_side_condition(pokemon.side as usize, screen);
                    }
                }
                Res::Undef
            }
            // onModifyType(move, pokemon): the type of the Paldean Tauros using it.
            (mv::RAGINGBULL, Ev::ModifyType) => {
                if let Some(pokemon) = e.target {
                    let t = match SPECIES[self.mon(pokemon).species as usize].name {
                        "Tauros-Paldea-Combat" => Some(Type::Fighting),
                        "Tauros-Paldea-Blaze" => Some(Type::Fire),
                        "Tauros-Paldea-Aqua" => Some(Type::Water),
                        _ => None,
                    };
                    if let Some(t) = t {
                        self.am[mi as usize].typ = t;
                    }
                }
                Res::Undef
            }
            // ---- Snore: onTry(source). Only while asleep.
            (mv::SNORE, Ev::Try) => Res::Bool(
                e.target.is_some_and(|s| self.mon(s).status == Status::Slp || self.has_ability(s, ab::COMATOSE)),
            ),
            // ---- Ceaseless Edge, Stone Axe: a hit leaves Spikes or Stealth Rock on the foe's side.
            // onAfterHit(target, source, move) and onAfterSubDamage(damage, target, source, move)
            (mv::CEASELESSEDGE | mv::STONEAXE, Ev::AfterHit | Ev::AfterSubDamage) => {
                let Some(source) = e.source else {
                    return Res::Undef;
                };
                if self.am[mi as usize].has_sheer_force || (ev == Ev::AfterSubDamage && self.mon(source).hp == 0) {
                    return Res::Undef;
                }
                let hazard = if id == mv::CEASELESSEDGE { SideCond::Spikes } else { SideCond::Stealthrock };
                // (Showdown's default source for it is the Pokémon that was hit.)
                self.add_side_condition(1 - source.side as usize, hazard, e.target, Eff::None);
                Res::Undef
            }
            // ============================================ switching
            // ---- Parting Shot: onHit(target, source, move). No switch unless something was lowered.
            (mv::PARTINGSHOT, Ev::Hit) => {
                let (Some(target), Some(source)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                let mut b = [0i8; 7];
                b[ATK] = -1;
                b[SPA] = -1;
                let success = self.boost(b, Some(target), Some(source), Eff::None).truthy();
                if !success && !self.has_ability(target, ab::MIRRORARMOR) {
                    self.am[mi as usize].self_switch = SelfSwitch::No;
                }
                Res::Undef
            }

            // ---- Baton Pass: onHit(target). Nothing to pass to: the move does nothing.
            (mv::BATONPASS, Ev::Hit) => {
                if e.target.is_some_and(|t| !self.can_switch(t.side as usize)) {
                    return Res::NotFail;
                }
                Res::Undef
            }
            // self.onHit(source): leaving this way does not set off Pursuit and the like.
            (mv::BATONPASS | mv::SHEDTAIL, Ev::SelfHit) => {
                if let Some(source) = e.target {
                    self.mon_mut(source).skip_before_switch_out = true;
                }
                Res::Undef
            }
            // ---- Shed Tail: a substitute for whoever comes in, at the price of half the user's HP.
            // onTryHit(source)
            (mv::SHEDTAIL, Ev::TryHit) => {
                let Some(source) = e.target else {
                    return Res::Undef;
                };
                let m = self.mon(source);
                if !self.can_switch(source.side as usize)
                    || self.vols(source).has(VolKind::Substitute)
                    || m.hp as u32 <= (m.max_hp() as u32).div_ceil(2)
                {
                    return Res::NotFail;
                }
                Res::Undef
            }
            // onHit(target)
            (mv::SHEDTAIL, Ev::Hit) => {
                if let Some(target) = e.target {
                    let half = (self.mon(target).max_hp() as i32 + 1) / 2;
                    self.direct_damage(half, target, e.source, Eff::Move(mi));
                }
                Res::Undef
            }

            // ---- Healing Wish: onTryHit(source). Pointless with no one to come in.
            (mv::HEALINGWISH, Ev::TryHit) => {
                if e.target.is_some_and(|s| !self.can_switch(s.side as usize)) {
                    return Res::NotFail;
                }
                Res::Undef
            }

            // ---- Revival Blessing: onTryHit(source). Fails with no one to bring back.
            (mv::REVIVALBLESSING, Ev::TryHit) => {
                let Some(source) = e.target else {
                    return Res::Undef;
                };
                let s = &self.sides[source.side as usize];
                if !s.team[..s.n as usize].iter().any(|m| m.fainted) {
                    return FALSE;
                }
                Res::Undef
            }

            // ---- Transform: onHit(target, pokemon)
            (mv::TRANSFORM, Ev::Hit) => match (e.target, e.source) {
                (Some(target), Some(pokemon)) => Res::Bool(self.transform_into(pokemon, target)),
                _ => FALSE,
            },
            // ---- Aura Wheel
            // onTry(source): only Morpeko, in either mode, can use it.
            (mv::AURAWHEEL, Ev::Try) => {
                let ok = e.target.is_some_and(|s| SPECIES[self.mon(s).species as usize].base_species == "morpeko");
                if ok { Res::Undef } else { Res::Null }
            }
            // onModifyType(move, pokemon): Dark when Morpeko is hangry.
            (mv::AURAWHEEL, Ev::ModifyType) => {
                let hangry = e.target.is_some_and(|p| SPECIES[self.mon(p).species as usize].id == "morpekohangry");
                self.am[mi as usize].typ = if hangry { Type::Dark } else { Type::Electric };
                Res::Undef
            }

            // ============================================ Ally Switch
            // onPrepareHit(pokemon): like Protect, it is less and less likely to work when repeated.
            (mv::ALLYSWITCH, Ev::PrepareHit) => match e.target {
                Some(pokemon) => self.add_volatile(pokemon, VolKind::Allyswitch, None, Eff::None),
                None => FALSE,
            },
            // onHit(pokemon): the user and its ally trade places.
            (mv::ALLYSWITCH, Ev::Hit) => {
                let Some(pokemon) = e.target else {
                    return Res::Undef;
                };
                let new_pos = if self.mon(pokemon).position == 0 { ACTIVE - 1 } else { 0 };
                if self.mon(self.active(pokemon.side as usize, new_pos)).fainted {
                    return Res::NotFail;
                }
                self.swap_position(pokemon, new_pos);
                Res::Undef
            }

            // ============================================ a hit two turns from now
            // ---- Future Sight: onTry(source, target). Nothing happens yet; the hit is left waiting on the target's slot.
            (mv::FUTURESIGHT, Ev::Try) => {
                let (Some(source), Some(target)) = (e.target, e.source) else {
                    return FALSE;
                };
                let (side, pos) = (target.side as usize, self.mon(target).position as usize);
                if !self.add_slot_condition(side, pos, SlotCond::Futuremove, Some(source), Eff::None).truthy() {
                    return FALSE;
                }
                if let Some(c) = self.sides[side].slot_conds[pos].get_mut(SlotCond::Futuremove) {
                    c.source = Some(source);
                }
                Res::NotFail
            }

            // ============================================ Fling
            // onPrepareHit(target, source, move): pick the item up; what it does depends on the item.
            (mv::FLING, Ev::PrepareHit) => {
                let Some(source) = e.source else {
                    return FALSE;
                };
                // `ignoringItem(true)`: Klutz counts here whatever the item.
                if self.field.pseudo.has(Pseudo::Magicroom)
                    || self.has_vol_named(source, "embargo")
                    || self.has_ability(source, ab::KLUTZ)
                {
                    return FALSE;
                }
                let item = self.mon(source).item;
                if !self.item_lets_go(item, source, source, mi) {
                    return FALSE;
                }
                let fling = ITEMS[item as usize].fling;
                if fling.power == 0 {
                    return FALSE;
                }
                let me = Eff::Move(mi);
                self.am[mi as usize].base_power = fling.power as u16;
                self.am[mi as usize].fling_item = item;
                if ITEMS[item as usize].flags & IF_BERRY != 0 {
                    // The target eats it (onHit); a thrower with Cud Chew notes the berry all the same.
                    if self.has_ability(source, ab::CUDCHEW) {
                        let mut ee = Event::new(Ev::EatItem, Some(source), Some(source), me);
                        ee.item = item;
                        let holder = Some(Holder::Mon(source));
                        self.single_event_ex(
                            Ev::EatItem,
                            Ev::EatItem,
                            Pre::On,
                            Eff::Ability(ab::CUDCHEW),
                            holder,
                            ee,
                            Res::Undef,
                            false,
                        );
                    }
                } else if !fling.effect {
                    let am = &mut self.am[mi as usize];
                    if !am.has_secs {
                        am.has_secs = true;
                        am.n_secs = 0;
                    }
                    if fling.status != Status::None || fling.flinch {
                        let n = am.n_secs as usize;
                        am.secs[n] = Secondary {
                            chance: 0,
                            status: fling.status,
                            boosts: None,
                            volatile: if fling.status == Status::None { Some(VolKind::Flinch) } else { None },
                            self_boosts: None,
                            on_hit: false,
                        };
                        am.n_secs += 1;
                    }
                }
                self.add_volatile(source, VolKind::Fling, None, Eff::None);
                Res::Undef
            }
            // The onHit Fling gives itself: a berry is eaten by what it hits, a herb works on it.
            (mv::FLING, Ev::Hit) => {
                let (Some(foe), Some(source)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                let item = self.am[mi as usize].fling_item;
                let me = Eff::Move(mi);
                if ITEMS[item as usize].flags & IF_BERRY != 0 {
                    if self
                        .single_event(Ev::Eat, Eff::Item(item), Some(foe), Some(foe), Some(source), me, Res::Undef)
                        .truthy()
                    {
                        let mut ee = Event::new(Ev::EatItem, Some(foe), Some(source), me);
                        ee.item = item;
                        self.run_event_ex(ee, Res::Undef, false, false);
                    }
                    if self.has_cb(Eff::Item(item), Ev::Eat) {
                        self.mon_mut(foe).ate_berry = true;
                    }
                } else if item == it::MENTALHERB {
                    // effect(pokemon): if it is under any of the six, all six are lifted.
                    let has = |b: &Battle, name: &str| b.vols(foe).as_slice().iter().any(|v| v.kind.id() == name);
                    if crate::items::MENTAL_HERB.iter().any(|name| has(self, name)) {
                        for name in crate::items::MENTAL_HERB {
                            if let Some(k) = VolKind::named(name) {
                                self.remove_volatile(foe, k);
                            }
                        }
                    }
                } else if item == it::WHITEHERB {
                    // effect(pokemon): lowered stats go back to normal.
                    for b in self.mon_mut(foe).boosts.iter_mut() {
                        if *b < 0 {
                            *b = 0;
                        }
                    }
                }
                Res::Undef
            }

            // ============================================ changing the order of the turn
            // ---- After You: onHit(target). The target moves next.
            (mv::AFTERYOU, Ev::Hit) => match e.target.and_then(|t| self.will_move_at(t)) {
                Some(at) => {
                    self.prioritize_at(at, ActSource::None);
                    Res::Undef
                }
                None => FALSE,
            },
            // ---- Quash: onHit(target). The target moves after everything else.
            (mv::QUASH, Ev::Hit) => match e.target.and_then(|t| self.will_move_at(t)) {
                Some(at) => {
                    self.queue.items[at].order = 201;
                    Res::Undef
                }
                None => FALSE,
            },
            // ---- Round
            // onTry(source, target, move): the next Round waiting in the queue, anyone's, follows at once.
            (mv::ROUND, Ev::Try) => {
                let next = self.queue.as_slice().iter().position(|a| {
                    matches!(a.kind, ActKind::Move | ActKind::BeforeTurnMove | ActKind::PriorityCharge)
                        && a.mon.is_some()
                        && a.move_id == mv::ROUND
                });
                if let Some(at) = next {
                    let ignore_ability = self.am[mi as usize].ignore_ability;
                    self.prioritize_at(at, ActSource::Round { ignore_ability });
                }
                Res::Undef
            }
            // basePowerCallback(target, source, move): doubled when a Round called it.
            (mv::ROUND, Ev::BasePowerCallback) => {
                let am = &self.am[mi as usize];
                let called = matches!(am.source_effect, Eff::Move(s) if self.am[s as usize].id == mv::ROUND);
                Res::Num(am.base_power as i32 * if called { 2 } else { 1 })
            }
            // ---- Instruct: onHit(target, source). The target uses its last move again, right now.
            (mv::INSTRUCT, Ev::Hit) => {
                let Some(target) = e.target else {
                    return Res::Undef;
                };
                let last = self.mon(target).last_move;
                if last == NO_MOVE {
                    return FALSE;
                }
                let flags = MOVES[last as usize].flags;
                let out_of_pp = self.move_slot(target, last).is_some_and(|slot| slot.pp == 0);
                if flags & (F_FAILINSTRUCT | F_CHARGE | F_RECHARGE) != 0
                    || self.vols(target).has(VolKind::Beakblast)
                    || self.vols(target).has(VolKind::Focuspunch)
                    || out_of_pp
                {
                    return FALSE;
                }
                // Showdown resolves the move as if it had been chosen (a Quick Claw
                // can go off again) and puts the first resulting action at the head
                // of the queue: for Counter and Mirror Coat that is their start-of-turn
                // step, not the move.
                let loc = self.mon(target).last_move_loc;
                let (actions, _) = self.resolve_move_actions(target, last, loc, false);
                self.queue.insert(0, actions[0]);
                self.prioritize_at(0, ActSource::None);
                Res::Undef
            }

            // ============================================ moves that use another move
            // ---- Copycat: onHit(pokemon). The last move anyone used.
            (mv::COPYCAT, Ev::Hit) => {
                let Some(pokemon) = e.target else {
                    return Res::Undef;
                };
                let last = self.last_move;
                if last == NO_MOVE {
                    return Res::Undef;
                }
                if MOVES[last as usize].flags & F_FAILCOPYCAT != 0 {
                    return FALSE;
                }
                self.call_move(last, pokemon);
                Res::Undef
            }
            // ---- Sleep Talk
            // onTry(source)
            (mv::SLEEPTALK, Ev::Try) => {
                let asleep =
                    e.target.is_some_and(|s| self.mon(s).status == Status::Slp || self.has_ability(s, ab::COMATOSE));
                Res::Bool(asleep)
            }
            // onHit(pokemon): one of its other moves, at random.
            (mv::SLEEPTALK, Ev::Hit) => {
                let Some(pokemon) = e.target else {
                    return Res::Undef;
                };
                let mut moves = [0u16; MAX_MOVES];
                let mut n = 0;
                let mon = self.mon(pokemon);
                for slot in &mon.moves[..mon.n_moves as usize] {
                    if MOVES[slot.id as usize].flags & (F_NOSLEEPTALK | F_CHARGE) == 0 {
                        moves[n] = slot.id;
                        n += 1;
                    }
                }
                if n == 0 {
                    return FALSE;
                }
                let pick = moves[self.rand(n as u32, "sleep talk") as usize];
                self.call_move(pick, pokemon);
                Res::Undef
            }

            // ---- Shell Side Arm
            // onModifyMove(move, pokemon, target): physical, and making contact, if that does more.
            (mv::SHELLSIDEARM, Ev::ModifyMove) => {
                let (Some(pokemon), Some(target)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                let atk = self.get_stat(pokemon, ATK + 1, false, true);
                let spa = self.get_stat(pokemon, SPA + 1, false, true);
                let def = self.get_stat(target, DEF + 1, false, true);
                let spd = self.get_stat(target, SPD + 1, false, true);
                let base = 2 * self.mon(pokemon).level as u32 / 5 + 2;
                let physical = base * 90 * atk / def / 50;
                let special = base * 90 * spa / spd / 50;
                if physical > special || (physical == special && self.chance(1, 2, "shell side arm")) {
                    self.am[mi as usize].category = Category::Physical;
                    self.am[mi as usize].flags |= F_CONTACT;
                }
                Res::Undef
            }
            // onPrepareHit, onHit and onAfterSubDamage only announce which it was.
            (mv::SHELLSIDEARM, Ev::PrepareHit | Ev::Hit | Ev::AfterSubDamage) => Res::Undef,

            // ---- Beat Up: one hit from each member of the team that is fit to fight.
            // onModifyMove(move, pokemon)
            (mv::BEATUP, Ev::ModifyMove) => {
                let Some(pokemon) = e.target else {
                    return Res::Undef;
                };
                let s = &self.sides[pokemon.side as usize];
                let mut powers = [0u8; MAX_TEAM];
                let mut n = 0;
                for p in 0..s.n as usize {
                    let ally = &s.team[s.order[p] as usize];
                    if s.order[p] == pokemon.idx || (!ally.fainted && ally.status == Status::None) {
                        // The species it was brought as, whatever it has become since.
                        powers[n] = 5 + SPECIES[ally.set_species as usize].base[ATK + 1] / 10;
                        n += 1;
                    }
                }
                let am = &mut self.am[mi as usize];
                am.beat_up = powers;
                am.beat_up_used = 0;
                am.multihit = (n as u8, n as u8);
                Res::Undef
            }
            // basePowerCallback(pokemon, target, move): the next member's turn.
            (mv::BEATUP, Ev::BasePowerCallback) => {
                let am = &mut self.am[mi as usize];
                let k = am.beat_up_used as usize;
                am.beat_up_used += 1;
                Res::Num(am.beat_up[k.min(MAX_TEAM - 1)] as i32)
            }

            // ---- Curse
            // onModifyMove(move, source, target): a stat move for most, a hex for Ghosts.
            (mv::CURSE, Ev::ModifyMove) => {
                let Some(source) = e.target else {
                    return Res::Undef;
                };
                if !self.has_type(source, Type::Ghost) {
                    self.am[mi as usize].target = Target::User;
                } else if e.source.is_none_or(|t| t != source && self.is_ally(source, t)) {
                    self.am[mi as usize].target = Target::RandomNormal;
                }
                Res::Undef
            }
            // onTryHit(target, source, move)
            (mv::CURSE, Ev::TryHit) => {
                if let (Some(target), Some(source)) = (e.target, e.source) {
                    if self.has_type(source, Type::Ghost) && self.vols(target).has(VolKind::Curse) {
                        return FALSE;
                    }
                }
                Res::Undef
            }
            // onHit(target, source)
            (mv::CURSE, Ev::Hit) => {
                let (Some(mut target), Some(source)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                if !self.has_type(source, Type::Ghost) {
                    // {spe: -1, atk: 1, def: 1}, in that order.
                    let mut b = [0i8; 7];
                    b[SPE] = -1;
                    b[ATK] = 1;
                    b[DEF] = 1;
                    let order = [SPE as u8, ATK as u8, DEF as u8, SPA as u8, SPD as u8, 5, 6];
                    return Res::Bool(self.boost_ordered(b, &order, Some(source), Some(source), Eff::None).truthy());
                }
                let half = div1(self.mon(source).max_hp() as u32, 2);
                self.direct_damage(half, source, Some(source), Eff::Move(mi));
                if self.is_ally(source, target) {
                    match self.get_random_target(source, Target::Normal) {
                        Some(random) => target = random,
                        None => return FALSE,
                    }
                }
                self.drop_vol(target, VolKind::Curse);
                self.add_volatile(target, VolKind::Curse, None, Eff::None);
                Res::Undef
            }

            // ---- Triple Axel: basePowerCallback(pokemon, target, move). 20, 40, 60.
            (mv::TRIPLEAXEL, Ev::BasePowerCallback) => Res::Num(20 * self.am[mi as usize].hit as i32),

            // ============================================ fixed damage
            // ---- Counter, Mirror Coat
            // onTry(source): fails unless something hit the user with the right kind of move.
            (mv::COUNTER | mv::MIRRORCOAT, Ev::Try) => {
                let kind = if id == mv::COUNTER { VolKind::Counter } else { VolKind::Mirrorcoat };
                match e.target.and_then(|s| self.vols(s).get(kind)) {
                    Some(v) if v.st.a != 0 => Res::Undef,
                    _ => FALSE,
                }
            }
            // damageCallback(pokemon): twice what was taken.
            (mv::COUNTER | mv::MIRRORCOAT, Ev::DamageCallback) => {
                let kind = if id == mv::COUNTER { VolKind::Counter } else { VolKind::Mirrorcoat };
                match e.target.and_then(|p| self.vols(p).get(kind)) {
                    Some(v) => Res::Num((v.data as i32).max(1)),
                    None => Res::Num(0),
                }
            }
            // ---- Metal Burst, Comeuppance: one and a half times the last hit a foe landed this turn.
            // onTry(source)
            (mv::METALBURST | mv::COMEUPPANCE, Ev::Try) => match e.target.and_then(|s| self.mon(s).last_damaged_by()) {
                Some(a) if a.this_turn => Res::Undef,
                _ => FALSE,
            },
            // onModifyTarget(targetRelayVar, source, target, move): aimed at the slot the hit came from.
            (mv::METALBURST | mv::COMEUPPANCE, Ev::ModifyTarget) => {
                match e.target.and_then(|s| self.mon(s).last_damaged_by()).and_then(|a| self.at_slot(a.slot)) {
                    Some(t) => Res::Mon(t),
                    None => Res::Undef,
                }
            }
            // damageCallback(pokemon): `damage * 1.5 || 1`, which Showdown leaves unrounded.
            (mv::METALBURST | mv::COMEUPPANCE, Ev::DamageCallback) => {
                match e.target.and_then(|p| self.mon(p).last_damaged_by()) {
                    Some(a) if a.damage == 0 => Res::Num(1),
                    Some(a) => {
                        self.am[mi as usize].half_damage = a.damage % 2 == 1;
                        Res::Num(a.damage as i32 * 3 / 2)
                    }
                    None => Res::Num(0),
                }
            }
            // ---- Super Fang: damageCallback(pokemon, target). Half the target's HP.
            (mv::SUPERFANG, Ev::DamageCallback) => Res::Num(e.source.map_or(0, |t| (self.mon(t).hp as i32 / 2).max(1))),
            // ---- Endeavor: brings the target down to the user's HP.
            // onTryImmunity(target, pokemon)
            (mv::ENDEAVOR, Ev::TryImmunity) => match (e.target, e.source) {
                (Some(target), Some(pokemon)) => Res::Bool(self.mon(pokemon).hp < self.mon(target).hp),
                _ => FALSE,
            },
            // damageCallback(pokemon, target)
            (mv::ENDEAVOR, Ev::DamageCallback) => match (e.target, e.source) {
                (Some(pokemon), Some(target)) => Res::Num(self.mon(target).hp as i32 - self.mon(pokemon).hp as i32),
                _ => Res::Num(0),
            },
            // ---- Final Gambit: damageCallback(pokemon). All the user's HP, and the user with it.
            (mv::FINALGAMBIT, Ev::DamageCallback) => {
                let Some(pokemon) = e.target else {
                    return Res::Num(0);
                };
                let damage = self.mon(pokemon).hp as i32;
                self.faint(pokemon, None, Eff::None);
                Res::Num(damage)
            }

            // ============================================ crashes and explosions
            // onMoveFail(target, source, move): half the user's HP for missing.
            (mv::HIGHJUMPKICK | mv::AXEKICK | mv::SUPERCELLSLAM, Ev::MoveFail) => {
                let amount = e.source.map_or(0, |s| div1(self.mon(s).max_hp() as u32, 2));
                self.crash(e.source, amount);
                Res::Undef
            }
            // ---- Steel Beam: onMoveFail(target, source, move). It costs half either way.
            (mv::STEELBEAM, Ev::MoveFail) => {
                if self.am[mi as usize].multihit != (0, 0) {
                    return Res::Undef;
                }
                let amount = e.source.map_or(0, |s| round_div(self.mon(s).max_hp() as u32, 1, 2) as i32);
                self.crash(e.source, amount);
                Res::Undef
            }
            // ---- Misty Explosion: onBasePower(basePower, source). Stronger on Misty Terrain.
            (mv::MISTYEXPLOSION, Ev::BasePower) => {
                if self.is_terrain(Terrain::Mistyterrain) && e.target.is_some_and(|s| self.is_grounded(s, false)) {
                    return self.chain_modify(3, 2);
                }
                Res::Undef
            }

            // ============================================ moves prepared before they are used
            // ---- Beak Blast: onAfterMove(pokemon). The beak cools down.
            (mv::BEAKBLAST, Ev::AfterMove) => {
                if let Some(pokemon) = e.target {
                    self.remove_volatile(pokemon, VolKind::Beakblast);
                }
                Res::Undef
            }

            // ============================================ moves that take two turns
            // onTryMove(attacker, defender, move): the first use only prepares
            // (`null` ends the move there); the second, a turn later, strikes.
            (
                mv::FLY
                | mv::DIG
                | mv::DIVE
                | mv::BOUNCE
                | mv::PHANTOMFORCE
                | mv::SOLARBEAM
                | mv::SOLARBLADE
                | mv::SKYATTACK
                | mv::METEORBEAM
                | mv::ELECTROSHOT,
                Ev::TryMove,
            ) => {
                let Some(attacker) = e.target else {
                    return Res::Undef;
                };
                let Some(own) = VolKind::named(MOVES[id as usize].id) else {
                    return Res::Undef;
                };
                if self.remove_volatile(attacker, own) {
                    return Res::Undef;
                }
                match id {
                    // No charging turn in the sun.
                    mv::SOLARBEAM | mv::SOLARBLADE => {
                        if self.effective_weather_aloud(attacker) == Weather::Sunnyday {
                            return Res::Undef;
                        }
                    }
                    // These raise Sp. Atk as they charge; Electro Shot skips the wait in rain.
                    mv::METEORBEAM | mv::ELECTROSHOT => {
                        self.boost1(SPA, 1, Some(attacker), Some(attacker), Eff::Move(mi));
                        if id == mv::ELECTROSHOT && self.effective_weather(attacker) == Weather::Raindance {
                            return Res::Undef;
                        }
                    }
                    _ => {}
                }
                // (The ChargeMove event only has Power Herb listening, which is not in Champions.)
                self.add_volatile(attacker, VolKind::Twoturnmove, e.source, Eff::None);
                Res::Null
            }
            // Solar Beam, Solar Blade: onBasePower(basePower, pokemon, target). Halved in bad weather.
            (mv::SOLARBEAM | mv::SOLARBLADE, Ev::BasePower) => {
                let weather = e.target.map_or(Weather::None, |p| self.effective_weather(p));
                if matches!(weather, Weather::Raindance | Weather::Sandstorm | Weather::Snowscape) {
                    return self.chain_modify(1, 2);
                }
                Res::Undef
            }

            // ---- Uproar: onTryHit(target). Wakes everyone up.
            (mv::UPROAR, Ev::TryHit) => {
                let Some(target) = e.target else {
                    return Res::Undef;
                };
                let side = target.side as usize;
                for pos in 0..ACTIVE {
                    for s in [side, 1 - side] {
                        let r = self.active(s, pos);
                        if self.in_play(r) && self.mon(r).status == Status::Slp {
                            self.cure_status(r);
                        }
                    }
                }
                Res::Undef
            }

            // ============================================ moves that set up a volatile
            // ---- Leech Seed: onTryImmunity(target). Grass types are immune.
            (mv::LEECHSEED, Ev::TryImmunity) => Res::Bool(e.target.is_some_and(|t| !self.has_type(t, Type::Grass))),

            // ---- Magnet Rise: onTry(source, target, move)
            (mv::MAGNETRISE, Ev::Try) => {
                if let Some(target) = e.source {
                    if self.has_vol_named(target, "smackdown") || self.has_vol_named(target, "ingrain") {
                        return FALSE;
                    }
                }
                if self.field.pseudo.has(Pseudo::Gravity) {
                    return Res::Null;
                }
                Res::Undef
            }

            // ---- No Retreat: onTry(source, target, move). Fails if already used.
            (mv::NORETREAT, Ev::Try) => {
                let Some(source) = e.target else {
                    return Res::Undef;
                };
                if self.vols(source).has(VolKind::Noretreat) {
                    return FALSE;
                }
                Res::Undef
            }

            // ---- Octolock: onTryImmunity(target). Ghosts cannot be trapped.
            (mv::OCTOLOCK, Ev::TryImmunity) => Res::Bool(e.target.is_some_and(|t| self.type_allows(t, 6))),

            // ---- Electrify: onTryHit(target). The target must still have a move to make.
            (mv::ELECTRIFY, Ev::TryHit) => {
                if e.target.is_some_and(|t| !self.will_move(t) && self.mon(t).active_turns > 0) {
                    return FALSE;
                }
                Res::Undef
            }

            // ---- Gastro Acid: onTryHit(target). Some abilities cannot be suppressed.
            (mv::GASTROACID, Ev::TryHit) => {
                if e.target.is_some_and(|t| ABILITIES[self.mon(t).ability as usize].flags & AF_CANTSUPPRESS != 0) {
                    return FALSE;
                }
                Res::Undef
            }

            // ---- Lock-On
            // onTryHit(target, source)
            (mv::LOCKON, Ev::TryHit) => {
                if e.source.is_some_and(|s| self.vols(s).has(VolKind::Lockon)) {
                    return FALSE;
                }
                Res::Undef
            }
            // onHit(target, source)
            (mv::LOCKON, Ev::Hit) => {
                if let (Some(target), Some(source)) = (e.target, e.source) {
                    self.add_volatile(source, VolKind::Lockon, Some(target), Eff::None);
                }
                Res::Undef
            }

            // ---- Perish Song: onHitField(target, source, move). Everyone who hears it.
            (mv::PERISHSONG, Ev::HitField) => {
                let me = Eff::Move(mi);
                let mut result = false;
                let (actives, n) = self.all_active(false);
                for &pokemon in &actives[..n] {
                    // Out of reach, or deaf to it (Soundproof answers `null`).
                    let unreachable = self.run_event(Ev::Invulnerability, Some(pokemon), e.source, me, Res::Undef)
                        == FALSE
                        || self.run_event(Ev::TryHit, Some(pokemon), e.source, me, Res::Undef) == Res::Null;
                    if unreachable {
                        result = true;
                    } else if !self.vols(pokemon).has(VolKind::Perishsong) {
                        self.add_volatile(pokemon, VolKind::Perishsong, None, Eff::None);
                        result = true;
                    }
                }
                if !result {
                    return FALSE;
                }
                Res::Undef
            }

            // ---- Yawn: onTryHit(target). Not on something that cannot fall asleep.
            (mv::YAWN, Ev::TryHit) => {
                let Some(target) = e.target else {
                    return Res::Undef;
                };
                if self.mon(target).status != Status::None
                    || !self.run_status_immunity(target, Imm::Status(Status::Slp))
                {
                    return FALSE;
                }
                Res::Undef
            }

            // ---- Stockpile, Spit Up, Swallow
            // onTry(source): three layers at most.
            (mv::STOCKPILE, Ev::Try) => {
                if e.target.is_some_and(|s| self.vols(s).get(VolKind::Stockpile).is_some_and(|v| v.data >= 3)) {
                    return FALSE;
                }
                Res::Undef
            }
            // onTry(source): needs something stockpiled.
            (mv::SPITUP | mv::SWALLOW, Ev::Try) => {
                Res::Bool(e.target.is_some_and(|s| self.vols(s).has(VolKind::Stockpile)))
            }
            // basePowerCallback(pokemon)
            (mv::SPITUP, Ev::BasePowerCallback) => {
                match e.target.and_then(|p| self.vols(p).get(VolKind::Stockpile)).map(|v| v.data) {
                    Some(layers) if layers > 0 => Res::Num(layers as i32 * 100),
                    _ => FALSE,
                }
            }
            // onAfterMove(pokemon)
            (mv::SPITUP, Ev::AfterMove) => {
                if let Some(pokemon) = e.target {
                    self.remove_volatile(pokemon, VolKind::Stockpile);
                }
                Res::Undef
            }
            // onHit(pokemon): a quarter, half or all of its HP.
            (mv::SWALLOW, Ev::Hit) => {
                let Some(pokemon) = e.target else {
                    return Res::Undef;
                };
                let layers = self.vols(pokemon).get(VolKind::Stockpile).map_or(1, |v| v.data.clamp(1, 3));
                let part = [1024, 2048, 4096][layers as usize - 1];
                let amount = crate::battle::modify(self.mon(pokemon).max_hp() as u32, part);
                let success = self.heal(amount as i32, None, None, Eff::None).truthy();
                self.remove_volatile(pokemon, VolKind::Stockpile);
                if success { TRUE } else { Res::NotFail }
            }

            // ---- Destiny Bond: onPrepareHit(pokemon). Fails if it is still in effect from last time.
            (mv::DESTINYBOND, Ev::PrepareHit) => {
                Res::Bool(e.target.is_some_and(|p| !self.remove_volatile(p, VolKind::Destinybond)))
            }

            // ---- Sparkling Aria: onAfterMove(source, target, move). Cures the burns of
            // those it hit; its secondary only marks them.
            (mv::SPARKLINGARIA, Ev::AfterMove) => {
                let Some(source) = e.target else {
                    return Res::Undef;
                };
                let am = self.am[mi as usize];
                if self.mon(source).fainted || !am.has_hit_targets || am.has_sheer_force {
                    let (actives, n) = self.all_active(true);
                    for &pokemon in &actives[..n] {
                        self.drop_vol(pokemon, VolKind::Sparklingaria);
                    }
                    return Res::Undef;
                }
                let n = am.n_hit_targets as usize;
                for &pokemon in &am.hit_targets[..n] {
                    if pokemon != source
                        && self.mon(pokemon).is_active
                        && (self.remove_volatile(pokemon, VolKind::Sparklingaria) || n > 1)
                        && self.mon(pokemon).status == Status::Brn
                    {
                        self.cure_status(pokemon);
                    }
                }
                Res::Undef
            }

            // ---- Block, Mean Look: onHit(target, source, move)
            (mv::BLOCK | mv::MEANLOOK, Ev::Hit) => match (e.target, e.source) {
                (Some(target), Some(source)) => self.add_trapped(target, source, Eff::Move(mi)),
                _ => Res::Undef,
            },
            // ---- Jaw Lock: onHit(target, source, move). Both are trapped.
            (mv::JAWLOCK, Ev::Hit) => {
                if let (Some(target), Some(source)) = (e.target, e.source) {
                    self.add_trapped(source, target, Eff::Move(mi));
                    self.add_trapped(target, source, Eff::Move(mi));
                }
                Res::Undef
            }
            // ---- Spirit Shackle: its secondary's onHit(target, source, move)
            (mv::SPIRITSHACKLE, Ev::SecondaryHit) => {
                if let (Some(target), Some(source)) = (e.target, e.source) {
                    if self.mon(source).is_active {
                        self.add_trapped(target, source, Eff::Move(mi));
                    }
                }
                Res::Undef
            }
            // ---- Throat Chop: its secondary's onHit(target)
            (mv::THROATCHOP, Ev::SecondaryHit) => {
                if let Some(target) = e.target {
                    self.add_volatile(target, VolKind::Throatchop, None, Eff::Secondary);
                }
                Res::Undef
            }

            // ---- Substitute
            // onTryHit(source): fails with a substitute already up, or without the HP to make one.
            (mv::SUBSTITUTE, Ev::TryHit) => {
                let Some(source) = e.target else {
                    return Res::Undef;
                };
                let m = self.mon(source);
                if self.vols(source).has(VolKind::Substitute) || m.hp as u32 * 4 <= m.max_hp() as u32 || m.max_hp() == 1
                {
                    return Res::NotFail;
                }
                Res::Undef
            }
            // onHit(target): the substitute costs a quarter of the user's HP.
            (mv::SUBSTITUTE, Ev::Hit) => {
                if let Some(target) = e.target {
                    let cost = self.mon(target).max_hp() as i32 / 4;
                    self.direct_damage(cost, target, e.source, Eff::Move(mi));
                }
                Res::Undef
            }

            // ---- Attract: onTryImmunity(target, source)
            (mv::ATTRACT, Ev::TryImmunity) => match (e.target, e.source) {
                (Some(t), Some(s)) => {
                    Res::Bool(crate::conditions::opposite_genders(self.mon(t).gender, self.mon(s).gender))
                }
                _ => FALSE,
            },

            // ---- Aurora Veil: onTry(). Only in snow.
            (mv::AURORAVEIL, Ev::Try) => Res::Bool(self.is_weather(Weather::Snowscape)),

            // ============================================ weather-dependent moves
            // ---- Blizzard: onModifyMove(move). Never misses in snow.
            (mv::BLIZZARD, Ev::ModifyMove) => {
                if self.is_weather(Weather::Snowscape) {
                    self.am[mi as usize].accuracy = 0;
                }
                Res::Undef
            }
            // ---- Thunder, Hurricane: onModifyMove(move, pokemon, target)
            (mv::THUNDER | mv::HURRICANE, Ev::ModifyMove) => {
                match e.source.map(|t| self.effective_weather(t)) {
                    Some(Weather::Raindance) => self.am[mi as usize].accuracy = 0,
                    Some(Weather::Sunnyday) => self.am[mi as usize].accuracy = 50,
                    _ => {}
                }
                Res::Undef
            }
            // ---- Weather Ball
            // onModifyType(move, pokemon)
            (mv::WEATHERBALL, Ev::ModifyType) => {
                if let Some(pokemon) = e.target {
                    match self.effective_weather(pokemon) {
                        Weather::Sunnyday => self.am[mi as usize].typ = Type::Fire,
                        Weather::Raindance => self.am[mi as usize].typ = Type::Water,
                        Weather::Sandstorm => self.am[mi as usize].typ = Type::Rock,
                        Weather::Snowscape => self.am[mi as usize].typ = Type::Ice,
                        Weather::None => {}
                    }
                }
                Res::Undef
            }
            // onModifyMove(move, pokemon)
            (mv::WEATHERBALL, Ev::ModifyMove) => {
                if e.target.is_some_and(|p| self.effective_weather(p) != Weather::None) {
                    self.am[mi as usize].base_power *= 2;
                }
                Res::Undef
            }
            // ---- Growth: onModifyMove(move, pokemon). +2/+2 in the sun.
            (mv::GROWTH, Ev::ModifyMove) => {
                if e.target.is_some_and(|p| self.effective_weather(p) == Weather::Sunnyday) {
                    let mut b = [0i8; 7];
                    b[ATK] = 2;
                    b[SPA] = 2;
                    self.am[mi as usize].boosts = Some(b);
                }
                Res::Undef
            }
            // ---- Moonlight, Morning Sun, Synthesis: onHit(pokemon)
            (mv::MOONLIGHT | mv::MORNINGSUN | mv::SYNTHESIS, Ev::Hit) => {
                let Some(pokemon) = e.target else {
                    return Res::Undef;
                };
                // 0.667, 0.25 and 0.5 as Showdown's `modify` sees them.
                let factor = match self.effective_weather_aloud(pokemon) {
                    Weather::Sunnyday => 2732,
                    Weather::None => 2048,
                    _ => 1024,
                };
                let amount = modify(self.mon(pokemon).max_hp() as u32, factor);
                if self.heal(amount as i32, None, None, Eff::None).truthy() { TRUE } else { Res::NotFail }
            }

            // ============================================ terrain-dependent moves
            // ---- Expanding Force
            // onBasePower(basePower, source)
            (mv::EXPANDINGFORCE, Ev::BasePower) => {
                if self.is_terrain(Terrain::Psychicterrain) && e.target.is_some_and(|s| self.is_grounded(s, false)) {
                    return self.chain_modify(3, 2);
                }
                Res::Undef
            }
            // onModifyMove(move, source, target): it becomes a spread move.
            (mv::EXPANDINGFORCE, Ev::ModifyMove) => {
                if self.is_terrain(Terrain::Psychicterrain) && e.target.is_some_and(|s| self.is_grounded(s, false)) {
                    self.am[mi as usize].target = Target::AllAdjacentFoes;
                }
                Res::Undef
            }
            // ---- Rising Voltage: basePowerCallback(source, target, move)
            (mv::RISINGVOLTAGE, Ev::BasePowerCallback) => {
                let bp = self.am[mi as usize].base_power as i32;
                if self.is_terrain(Terrain::Electricterrain) && e.source.is_some_and(|t| self.is_grounded(t, false)) {
                    return Res::Num(bp * 2);
                }
                Res::Num(bp)
            }
            // ---- Grassy Glide: onModifyPriority(priority, source, target, move)
            (mv::GRASSYGLIDE, Ev::ModifyPriority) => {
                if self.is_terrain(Terrain::Grassyterrain) && e.target.is_some_and(|s| self.is_grounded(s, false)) {
                    return Res::Num(e.relay.num() + 1);
                }
                Res::Undef
            }
            // ---- Terrain Pulse
            // onModifyType(move, pokemon)
            (mv::TERRAINPULSE, Ev::ModifyType) => {
                if !e.target.is_some_and(|p| self.is_grounded(p, false)) {
                    return Res::Undef;
                }
                match self.field.terrain.kind {
                    Terrain::Electricterrain => self.am[mi as usize].typ = Type::Electric,
                    Terrain::Grassyterrain => self.am[mi as usize].typ = Type::Grass,
                    Terrain::Mistyterrain => self.am[mi as usize].typ = Type::Fairy,
                    Terrain::Psychicterrain => self.am[mi as usize].typ = Type::Psychic,
                    Terrain::None => {}
                }
                Res::Undef
            }
            // onModifyMove(move, pokemon)
            (mv::TERRAINPULSE, Ev::ModifyMove) => {
                if self.field.terrain.kind != Terrain::None && e.target.is_some_and(|p| self.is_grounded(p, false)) {
                    self.am[mi as usize].base_power *= 2;
                }
                Res::Undef
            }
            // ---- Steel Roller: onTry() fails without a terrain; onHit() and onAfterSubDamage() remove it.
            (mv::STEELROLLER, Ev::Try) => Res::Bool(self.field.terrain.kind != Terrain::None),
            (mv::STEELROLLER, Ev::Hit | Ev::AfterSubDamage) => {
                self.clear_terrain();
                Res::Undef
            }
            // ---- Ice Spinner
            // onAfterHit(target, source)
            (mv::ICESPINNER, Ev::AfterHit) => {
                self.clear_terrain();
                Res::Undef
            }
            // onAfterSubDamage(damage, target, source)
            (mv::ICESPINNER, Ev::AfterSubDamage) => {
                if e.source.is_some_and(|s| self.mon(s).hp > 0) {
                    self.clear_terrain();
                }
                Res::Undef
            }

            // ============================================ screens and hazards
            // ---- Brick Break, Psychic Fangs: onTryHit(pokemon). The screens go before the hit.
            (mv::BRICKBREAK | mv::PSYCHICFANGS, Ev::TryHit) => {
                if let Some(target) = e.target {
                    for cond in SCREENS {
                        self.remove_side_condition(target.side as usize, cond);
                    }
                }
                Res::Undef
            }
            // ---- Defog: onHit(target, source, move)
            (mv::DEFOG, Ev::Hit) => {
                let (Some(target), Some(source)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                let mut success = false;
                if !self.has_vol_named(target, "substitute") || self.am[mi as usize].infiltrates {
                    success = self.boost1(EVA, -1, None, None, Eff::None).truthy();
                }
                // The target's side loses its screens and Safeguard too; only a hazard
                // counts as the move having done something.
                for cond in SCREENS.into_iter().chain([SideCond::Safeguard]) {
                    self.remove_side_condition(target.side as usize, cond);
                }
                for side in [target.side as usize, source.side as usize] {
                    for cond in HAZARDS {
                        if self.remove_side_condition(side, cond) {
                            success = true;
                        }
                    }
                }
                self.clear_terrain();
                Res::Bool(success)
            }
            // ---- Rapid Spin, Mortal Spin
            // onAfterHit(target, pokemon, move)
            (mv::RAPIDSPIN | mv::MORTALSPIN, Ev::AfterHit) => {
                if let Some(pokemon) = e.source {
                    self.spin_away(mi, pokemon, false);
                }
                Res::Undef
            }
            // onAfterSubDamage(damage, target, pokemon, move)
            (mv::RAPIDSPIN | mv::MORTALSPIN, Ev::AfterSubDamage) => {
                if let Some(pokemon) = e.source {
                    self.spin_away(mi, pokemon, true);
                }
                Res::Undef
            }
            // ---- Tidy Up: onHit(pokemon)
            (mv::TIDYUP, Ev::Hit) => {
                let Some(pokemon) = e.target else {
                    return Res::Undef;
                };
                let mut success = false;
                if let Some(k) = VolKind::named("substitute") {
                    let (actives, n) = self.all_active(false);
                    for &active in &actives[..n] {
                        if self.remove_volatile(active, k) {
                            success = true;
                        }
                    }
                }
                for side in [pokemon.side as usize, 1 - pokemon.side as usize] {
                    for cond in HAZARDS {
                        if self.remove_side_condition(side, cond) {
                            success = true;
                        }
                    }
                }
                let mut b = [0i8; 7];
                b[ATK] = 1;
                b[SPE] = 1;
                let boosted = self.boost(b, Some(pokemon), Some(pokemon), Eff::None).truthy();
                Res::Bool(boosted || success)
            }
            // ---- Court Change: onHitField(target, source). The two sides trade conditions.
            (mv::COURTCHANGE, Ev::HitField) => {
                let Some(source) = e.source else {
                    return Res::Undef;
                };
                // Everything a side can have except the one-turn guards.
                let swaps = |k: SideCond| {
                    matches!(
                        k,
                        SideCond::Tailwind
                            | SideCond::Reflect
                            | SideCond::Lightscreen
                            | SideCond::Auroraveil
                            | SideCond::Safeguard
                            | SideCond::Spikes
                            | SideCond::Toxicspikes
                            | SideCond::Stealthrock
                            | SideCond::Stickyweb
                    )
                };
                let own = source.side as usize;
                let from_own = self.sides[own].conds.take_where(swaps);
                let from_foe = self.sides[1 - own].conds.take_where(swaps);
                if from_own.as_slice().is_empty() && from_foe.as_slice().is_empty() {
                    return FALSE;
                }
                for c in from_own.as_slice() {
                    self.sides[1 - own].conds.push(*c);
                }
                for c in from_foe.as_slice() {
                    self.sides[own].conds.push(*c);
                }
                Res::Undef
            }
            // ---- Magnetic Flux: onHitSide(side, source, move)
            (mv::MAGNETICFLUX, Ev::HitSide) => {
                let Some(side) = e.target_side else {
                    return Res::Undef;
                };
                let (allies, n) = self.allies_and_self(side as usize);
                let mut any_target = false;
                let mut did = false;
                let mut b = [0i8; 7];
                b[DEF] = 1;
                b[SPD] = 1;
                for &ally in &allies[..n] {
                    if self.has_ability(ally, ab::PLUS) || self.has_ability(ally, ab::MINUS) {
                        any_target = true;
                        did = self.boost(b, Some(ally), e.source, Eff::Move(mi)).truthy() || did;
                    }
                }
                if !any_target {
                    return FALSE;
                }
                Res::Bool(did)
            }
            // ---- Haze: onHitField()
            (mv::HAZE, Ev::HitField) => {
                let (actives, n) = self.all_active(false);
                for &pokemon in &actives[..n] {
                    self.mon_mut(pokemon).boosts = [0; 7];
                }
                Res::Undef
            }

            // ---- Struggle: onModifyMove(move, pokemon, target)
            (mv::STRUGGLE, Ev::ModifyMove) => {
                self.am[mi as usize].typ = Type::Typeless;
                Res::Undef
            }

            _ => unreachable!("no body for move {} {ev:?}", MOVES[id as usize].id),
        }
    }
}
