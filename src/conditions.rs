//! Event callbacks of status conditions and volatile conditions, ported from
//! Showdown's `data/conditions.ts` (with the Champions overrides) and from the
//! `condition` blocks of moves.

// The nesting mirrors Showdown's code; keep it rather than folding conditions together.
#![allow(clippy::collapsible_if, clippy::collapsible_match)]

use crate::battle::div1;
use crate::data::*;
use crate::state::*;

impl Battle {
    /// Callbacks of the major status conditions. `holder` has the status.
    pub(crate) fn status_cb(&mut self, status: Status, ev: Ev, holder: MonRef) -> Res {
        match (status, ev) {
            // Showdown's onStart for brn, par and psn only writes to the log.
            (Status::Brn | Status::Par | Status::Psn, Ev::Start) => Res::Undef,

            (Status::Brn, Ev::Residual) => {
                let d = div1(self.mon(holder).max_hp() as u32, 16);
                self.damage(d, None, None, Eff::None);
                Res::Undef
            }
            (Status::Psn, Ev::Residual) => {
                let d = div1(self.mon(holder).max_hp() as u32, 8);
                self.damage(d, None, None, Eff::None);
                Res::Undef
            }

            (Status::Tox, Ev::Start | Ev::SwitchIn) => {
                self.mon_mut(holder).tox_stage = 0;
                Res::Undef
            }
            (Status::Tox, Ev::Residual) => {
                let m = self.mon_mut(holder);
                if m.tox_stage < 15 {
                    m.tox_stage += 1;
                }
                let d = (m.max_hp() as i32 / 16).max(1) * m.tox_stage as i32;
                self.damage(d, None, None, Eff::None);
                Res::Undef
            }

            // Paralysis halves Speed after every other modifier.
            (Status::Par, Ev::ModifySpe) => {
                let mut spe = self.final_modify(self.event.relay.num() as u32);
                if !self.has_ability(holder, ab::QUICKFEET) {
                    spe = spe * 50 / 100;
                }
                Res::Num(spe as i32)
            }
            // Champions: full paralysis is 1 in 8.
            (Status::Par, Ev::BeforeMove) => {
                if self.chance(1, 8, "full paralysis") {
                    return FALSE;
                }
                Res::Undef
            }

            // Champions: asleep for 2 turns one time in three, otherwise 3.
            (Status::Slp, Ev::Start) => {
                let t = [2u8, 3, 3][self.rand(3, "sleep turns") as usize];
                self.mon_mut(holder).status_time = t;
                self.note_count_began(self.mon(holder).status_st.uid, t, false);
                Res::Undef
            }
            (Status::Slp, Ev::BeforeMove) => {
                let early = self.has_ability(holder, ab::EARLYBIRD);
                let m = self.mon_mut(holder);
                if early {
                    m.status_time = m.status_time.saturating_sub(1);
                }
                m.status_time = m.status_time.saturating_sub(1);
                if m.status_time == 0 {
                    self.cure_status(holder);
                    return Res::Undef;
                }
                // Snore and Sleep Talk go ahead anyway.
                if matches!(self.event.effect, Eff::Move(mi) if self.am[mi as usize].d().sleep_usable) {
                    return Res::Undef;
                }
                FALSE
            }

            // Champions: frozen for at most 3 turns.
            (Status::Frz, Ev::Start) => {
                self.mon_mut(holder).status_time = 3;
                Res::Undef
            }
            (Status::Frz, Ev::BeforeMove) => {
                // A move that thaws its user goes ahead; Burn Up only does from a Fire type.
                let burn_up_fizzles = matches!(self.event.effect, Eff::Move(mi) if self.am[mi as usize].id == mv::BURNUP)
                    && !self.has_type(holder, Type::Fire);
                if self.event_move_flags() & F_DEFROST != 0 && !burn_up_fizzles {
                    return Res::Undef;
                }
                let m = self.mon_mut(holder);
                m.status_time = m.status_time.saturating_sub(1);
                // The thaw roll is skipped once the timer has run out.
                if m.status_time == 0 || self.chance(1, 4, "thaw") {
                    self.cure_status(holder);
                    return Res::Undef;
                }
                FALSE
            }
            // Using a defrosting move thaws the user.
            (Status::Frz, Ev::ModifyMove) => {
                if self.event_move_flags() & F_DEFROST != 0 {
                    self.cure_status(holder);
                }
                Res::Undef
            }
            (Status::Frz, Ev::AfterMoveSecondary) => {
                if let (Eff::Move(mi), Some(t)) = (self.event.effect, self.event.target) {
                    if self.am[mi as usize].d().thaws_target {
                        self.cure_status(t);
                    }
                }
                Res::Undef
            }
            // A damaging Fire move thaws its target.
            (Status::Frz, Ev::DamagingHit) => {
                if let (Eff::Move(mi), Some(t)) = (self.event.effect, self.event.target) {
                    let am = &self.am[mi as usize];
                    if am.typ == Type::Fire && am.category != Category::Status {
                        self.cure_status(t);
                    }
                }
                Res::Undef
            }
            _ => unreachable!("no body for {status:?} {ev:?}"),
        }
    }

    /// A `cant|pokemon|reason|Move` line: the move `who` was about to use is named.
    #[track_caller]
    fn show_attempted(&mut self, who: Option<MonRef>) {
        if let (Eff::Move(mi), Some(who)) = (self.event.effect, who) {
            let id = self.am[mi as usize].id;
            self.show_move_used(who, id);
        }
    }

    /// Flags of the move the running event is about (0 if it is not about a move).
    pub(crate) fn event_move_flags(&self) -> u32 {
        match self.event.effect {
            Eff::Move(mi) => self.am[mi as usize].flags,
            _ => 0,
        }
    }

    /// Callbacks of volatile conditions. `holder` has the volatile.
    pub(crate) fn vol_cb(&mut self, kind: VolKind, ev: Ev, pre: Pre, holder: MonRef) -> Res {
        if pre != Pre::On {
            return self.vol_cb_prefixed(kind, ev, pre, holder);
        }
        let e = self.event;
        match (kind, ev) {
            (VolKind::Flinch, Ev::BeforeMove) => {
                self.run_event(Ev::Flinch, Some(holder), None, Eff::None, Res::Undef);
                FALSE
            }

            (VolKind::Protect, Ev::Start) => Res::Undef,
            (VolKind::Protect, Ev::TryHit) => {
                let (Eff::Move(mi), Some(source)) = (self.event.effect, self.event.source) else {
                    return Res::Undef;
                };
                let target = self.event.target.unwrap_or(holder);
                if self.bypasses_protect(mi, source, target, true) {
                    return Res::Undef;
                }
                self.am[mi as usize].smart_target = false;
                self.protect_unlocks(source);
                Res::NotFail
            }

            // The consecutive-Protect counter.
            (VolKind::Stall, Ev::Start) => {
                if let Some(v) = self.vol_mut(holder, VolKind::Stall) {
                    v.data = 3;
                }
                Res::Undef
            }
            (VolKind::Stall, Ev::StallMove) => {
                let counter = self.vols(holder).get(VolKind::Stall).map_or(1, |v| v.data.max(1)) as u32;
                let success = self.chance(1, counter, "consecutive protect");
                if !success {
                    self.drop_vol(holder, VolKind::Stall);
                }
                Res::Bool(success)
            }
            (VolKind::Stall, Ev::Restart) => {
                if let Some(v) = self.vol_mut(holder, VolKind::Stall) {
                    if v.data < 729 {
                        v.data *= 3;
                    }
                    v.duration = 2;
                }
                Res::Undef
            }

            // ---- Ally Switch's counter. `data` is the denominator of its next chance to work.
            (VolKind::Allyswitch, Ev::Start) => {
                if let Some(v) = self.vol_mut(holder, kind) {
                    v.data = 3;
                }
                Res::Undef
            }
            // onRestart(pokemon)
            (VolKind::Allyswitch, Ev::Restart) => {
                let counter = self.vols(holder).get(kind).map_or(1, |v| v.data.max(1)) as u32;
                if !self.chance(1, counter, "consecutive ally switch") {
                    self.drop_vol(holder, kind);
                    return FALSE;
                }
                if let Some(v) = self.vol_mut(holder, kind) {
                    if v.data < 729 {
                        v.data *= 3;
                    }
                    v.duration = 2;
                }
                Res::Undef
            }

            (VolKind::Confusion, Ev::Start) => {
                // Lasts 2 to 5 of the holder's move attempts (3 to 5 from Axe Kick).
                let min = if self.eff_is_named(self.event.effect, "axekick") { 3 } else { 2 };
                let t = self.rand_range(min, 6, "confusion turns");
                if let Some(v) = self.vol_mut(holder, VolKind::Confusion) {
                    v.data = t as u16;
                    let uid = v.st.uid;
                    self.note_count_began(uid, t as u8, false);
                }
                Res::Undef
            }
            (VolKind::Confusion, Ev::End) => Res::Undef,
            (VolKind::Confusion, Ev::BeforeMove) => {
                let Some(v) = self.vol_mut(holder, VolKind::Confusion) else {
                    return Res::Undef;
                };
                v.data = v.data.saturating_sub(1);
                if v.data == 0 {
                    self.remove_volatile(holder, VolKind::Confusion);
                    return Res::Undef;
                }
                if !self.chance(33, 100, "confusion self-hit") {
                    return Res::Undef;
                }
                self.active_target = Some(holder);
                let d = self.confusion_damage(holder, 40);
                self.damage(d, Some(holder), Some(holder), Eff::Confused);
                FALSE
            }

            (VolKind::Choicelock, Ev::Start) => {
                let Some(mi) = self.active_move else {
                    return FALSE;
                };
                let am = &self.am[mi as usize];
                if am.has_bounced {
                    return FALSE;
                }
                let id = am.id;
                if let Some(v) = self.vol_mut(holder, VolKind::Choicelock) {
                    v.data = id + 1;
                }
                Res::Undef
            }
            (VolKind::Choicelock, Ev::BeforeMove) => {
                if ITEMS[self.mon(holder).item as usize].flags & IF_CHOICE == 0 {
                    self.remove_volatile(holder, VolKind::Choicelock);
                    return Res::Undef;
                }
                let locked = self.vols(holder).get(VolKind::Choicelock).map_or(0, |v| v.data);
                if let Eff::Move(mi) = self.event.effect {
                    let am = &self.am[mi as usize];
                    if !self.ignoring_item(holder) && am.id + 1 != locked && am.id != mv::STRUGGLE {
                        // Fails, and no PP is lost. Showdown writes the `move` line itself here,
                        // and nothing tells it from any other move that failed. (This may be
                        // the second turn of a move Copycat borrowed: see `Shown::use_move`.)
                        self.show_attempted(Some(holder));
                        self.mon_mut(holder).last_attempted_move = Some(self.am[mi as usize].id);
                        return FALSE;
                    }
                }
                Res::Undef
            }
            (VolKind::Choicelock, Ev::DisableMove) => {
                let locked = self.vols(holder).get(VolKind::Choicelock).map_or(0, |v| v.data);
                let m = self.mon(holder);
                let has_move = m.moves[..m.n_moves as usize].iter().any(|s| s.id + 1 == locked);
                if ITEMS[m.item as usize].flags & IF_CHOICE == 0 || !has_move {
                    self.remove_volatile(holder, VolKind::Choicelock);
                    return Res::Undef;
                }
                if self.ignoring_item(holder) {
                    return Res::Undef;
                }
                self.disable_slots_where(holder, |s| s.id + 1 != locked);
                Res::Undef
            }

            (VolKind::Gem, Ev::BasePower) => self.chain_modify(5325, 4096),

            // Flash Fire's boost, once a Fire move has been absorbed.
            (VolKind::Flashfire, Ev::Start | Ev::End) => Res::Undef,
            (VolKind::Flashfire, Ev::ModifyAtk | Ev::ModifySpA) => {
                let fire = matches!(self.event.effect, Eff::Move(mi) if self.am[mi as usize].typ == Type::Fire);
                if fire && self.has_ability(holder, ab::FLASHFIRE) {
                    return self.chain_modify(6144, 4096);
                }
                Res::Undef
            }

            // Unburden's doubled Speed while the holder has no item.
            (VolKind::Unburden, Ev::ModifySpe) => {
                if self.mon(holder).item == it::NONE && !self.ignoring_ability(holder) {
                    return self.chain_modify(2, 1);
                }
                Res::Undef
            }

            // The Metronome item's counter of consecutive uses of one move.
            (VolKind::Metronome, Ev::Start) => {
                if let Some(v) = self.vol_mut(holder, VolKind::Metronome) {
                    v.data = 0;
                    v.st.a = 0;
                }
                Res::Undef
            }
            (VolKind::Metronome, Ev::TryMove) => {
                if !self.has_item(holder, it::METRONOME) {
                    self.remove_volatile(holder, VolKind::Metronome);
                    return Res::Undef;
                }
                let Eff::Move(mi) = self.event.effect else {
                    return Res::Undef;
                };
                if self.am[mi as usize].d().calls_move {
                    return Res::Undef;
                }
                let id = self.am[mi as usize].id + 1;
                let last_ok = self.mon(holder).move_last_turn.truthy();
                // The second turn of a charging move counts as a use of its own.
                let charged = self.vols(holder).has(VolKind::Twoturnmove);
                if let Some(v) = self.vol_mut(holder, VolKind::Metronome) {
                    if v.data == id && last_ok {
                        v.st.a += 1;
                    } else if charged {
                        v.st.a = if v.data != id { 1 } else { v.st.a + 1 };
                    } else {
                        v.st.a = 0;
                    }
                    v.data = id;
                }
                Res::Undef
            }
            (VolKind::Metronome, Ev::ModifyDamage) => {
                const MODS: [u32; 6] = [4096, 4915, 5734, 6553, 7372, 8192];
                let n = self.vols(holder).get(VolKind::Metronome).map_or(0, |v| v.st.a).clamp(0, 5);
                self.chain_modify(MODS[n as usize], 4096)
            }

            // ---- Follow Me / Rage Powder draw the foes' single-target moves (see `vol_cb_prefixed`).
            (VolKind::Followme | VolKind::Ragepowder, Ev::Start) => Res::Undef,

            // ---- Helping Hand. `data` counts how many times it has been given this turn.
            (VolKind::Helpinghand, Ev::Start) => {
                if let Some(v) = self.vol_mut(holder, VolKind::Helpinghand) {
                    v.data = 1;
                }
                Res::Undef
            }
            (VolKind::Helpinghand, Ev::Restart) => {
                if let Some(v) = self.vol_mut(holder, VolKind::Helpinghand) {
                    v.data += 1;
                }
                Res::Undef
            }
            // onBasePower(basePower): 1.5 for each Helping Hand.
            (VolKind::Helpinghand, Ev::BasePower) => {
                let n = self.vols(holder).get(VolKind::Helpinghand).map_or(1, |v| v.data.clamp(1, 8)) as u32;
                self.chain_modify(3u32.pow(n), 2u32.pow(n))
            }

            // ---- Endure
            (VolKind::Endure, Ev::Start) => Res::Undef,
            // onDamage(damage, target, source, effect)
            (VolKind::Endure, Ev::Damage) => {
                let hp = self.mon(holder).hp as i32;
                if e.effect.is_move() && e.relay.num() >= hp {
                    return Res::Num(hp - 1);
                }
                Res::Undef
            }

            // ---- Baneful Bunker / King's Shield / Spiky Shield: Protect, with a
            // penalty for moves that make contact.
            (VolKind::Banefulbunker | VolKind::Kingsshield | VolKind::Spikyshield, Ev::Start) => Res::Undef,
            // onTryHit(target, source, move)
            (VolKind::Banefulbunker | VolKind::Kingsshield | VolKind::Spikyshield, Ev::TryHit) => {
                let (Eff::Move(mi), Some(source)) = (e.effect, e.source) else {
                    return Res::Undef;
                };
                let target = e.target.unwrap_or(holder);
                // King's Shield lets status moves through.
                if self.bypasses_protect(mi, source, target, kind != VolKind::Kingsshield) {
                    return Res::Undef;
                }
                self.am[mi as usize].smart_target = false;
                self.protect_unlocks(source);
                if self.makes_contact(mi) {
                    match kind {
                        VolKind::Banefulbunker => {
                            let saved = self.am_len;
                            let bunker = self.new_am(mv::BANEFULBUNKER);
                            self.try_set_status(source, Status::Psn, Some(target), Eff::Move(bunker));
                            self.am_len = saved;
                        }
                        VolKind::Kingsshield => {
                            let saved = self.am_len;
                            let shield = self.new_am(mv::KINGSSHIELD);
                            self.boost1(ATK, -1, Some(source), Some(target), Eff::Move(shield));
                            self.am_len = saved;
                        }
                        _ => {
                            let d = div1(self.mon(source).max_hp() as u32, 8);
                            self.damage(d, Some(source), Some(target), Eff::None);
                        }
                    }
                }
                Res::NotFail
            }
            // onHit only reacts to Z-Moves and Max Moves.
            (VolKind::Banefulbunker | VolKind::Kingsshield | VolKind::Spikyshield, Ev::Hit) => Res::Undef,

            // ---- Taunt
            // onStart(target): a Pokémon that has already moved this turn is taunted for an extra turn.
            (VolKind::Taunt, Ev::Start) => {
                if self.mon(holder).active_turns > 0 && !self.will_move(holder) {
                    if let Some(v) = self.vol_mut(holder, VolKind::Taunt) {
                        v.duration += 1;
                    }
                }
                Res::Undef
            }
            (VolKind::Taunt, Ev::End) => Res::Undef,
            // onDisableMove(pokemon)
            (VolKind::Taunt, Ev::DisableMove) => {
                self.disable_moves_where(holder, |d| d.category == Category::Status && d.id != "mefirst");
                Res::Undef
            }
            // onBeforeMove(attacker, defender, move)
            (VolKind::Taunt, Ev::BeforeMove) => {
                if let Eff::Move(mi) = e.effect {
                    let am = &self.am[mi as usize];
                    if am.category == Category::Status && am.d().id != "mefirst" {
                        // `cant|attacker|move: Taunt|Move`
                        self.show_attempted(Some(holder));
                        return FALSE;
                    }
                }
                Res::Undef
            }

            // ---- Encore. `data` is the move the holder is held to (table index plus one).
            // onStart(target)
            (VolKind::Encore, Ev::Start) => {
                let last = self.mon(holder).last_move;
                if last == NO_MOVE {
                    return FALSE;
                }
                let pp = self.move_slot(holder, last).map(|s| s.pp);
                if MOVES[last as usize].flags & F_FAILENCORE != 0 || pp.is_none_or(|pp| pp == 0) {
                    return FALSE;
                }
                if let Some(v) = self.vol_mut(holder, VolKind::Encore) {
                    v.data = last + 1;
                }
                match self.queued_move(holder) {
                    // Already moved this turn: the Encore lasts a turn longer.
                    None => {
                        if let Some(v) = self.vol_mut(holder, VolKind::Encore) {
                            v.duration += 1;
                        }
                    }
                    // Still to move with something else: its action is replaced.
                    Some(chosen) => {
                        if chosen != last && !self.has_item(holder, it::MENTALHERB) {
                            self.change_action(holder, last);
                        }
                    }
                }
                Res::Undef
            }
            // onResidual(target): ends when the move runs out of PP.
            (VolKind::Encore, Ev::Residual) => {
                let locked = self.vols(holder).get(VolKind::Encore).map_or(0, |v| v.data);
                let pp = if locked == 0 { None } else { self.move_slot(holder, locked - 1).map(|s| s.pp) };
                if pp.is_none_or(|pp| pp == 0) {
                    self.remove_volatile(holder, VolKind::Encore);
                }
                Res::Undef
            }
            (VolKind::Encore, Ev::End) => Res::Undef,
            // onDisableMove(pokemon)
            (VolKind::Encore, Ev::DisableMove) => {
                let locked = self.vols(holder).get(VolKind::Encore).map_or(0, |v| v.data);
                if locked == 0 || self.move_slot(holder, locked - 1).is_none() {
                    return Res::Undef;
                }
                self.disable_slots_where(holder, |s| s.id != locked - 1);
                Res::Undef
            }

            // ---- Disable. `data` is the disabled move (table index plus one).
            // onStart(pokemon, source, effect)
            (VolKind::Disable, Ev::Start) => {
                // The turn it lands on counts if the holder has yet to move, or is moving right now.
                let moving_now = self.active_pokemon == Some(holder) && self.active_move.is_some();
                if self.will_move(holder) || moving_now {
                    if let Some(v) = self.vol_mut(holder, VolKind::Disable) {
                        v.duration -= 1;
                    }
                }
                let last = self.mon(holder).last_move;
                if last == NO_MOVE {
                    return FALSE;
                }
                if self.move_slot(holder, last).is_some_and(|s| s.pp == 0) {
                    return FALSE;
                }
                if let Some(v) = self.vol_mut(holder, VolKind::Disable) {
                    v.data = last + 1;
                }
                Res::Undef
            }
            (VolKind::Disable, Ev::End) => Res::Undef,
            // onBeforeMove(attacker, defender, move)
            (VolKind::Disable, Ev::BeforeMove) => {
                let locked = self.vols(holder).get(VolKind::Disable).map_or(0, |v| v.data);
                if let Eff::Move(mi) = e.effect {
                    let am = &self.am[mi as usize];
                    if am.id + 1 == locked && am.flags & F_CANTUSETWICE == 0 {
                        // `cant|attacker|Disable|Move`
                        self.show_attempted(Some(holder));
                        return FALSE;
                    }
                }
                Res::Undef
            }
            // onDisableMove(pokemon)
            (VolKind::Disable, Ev::DisableMove) => {
                let locked = self.vols(holder).get(VolKind::Disable).map_or(0, |v| v.data);
                self.disable_slots_where(holder, |s| s.id + 1 == locked);
                Res::Undef
            }

            // ---- Torment: the same move cannot be chosen twice in a row.
            (VolKind::Torment, Ev::Start | Ev::End) => Res::Undef,
            // onDisableMove(pokemon)
            (VolKind::Torment, Ev::DisableMove) => {
                let last = self.mon(holder).last_move;
                if last != NO_MOVE && last != mv::STRUGGLE {
                    self.disable_slots_where(holder, |s| s.id == last);
                }
                Res::Undef
            }

            // ---- Imprison works on the foes (see `vol_cb_prefixed`).
            (VolKind::Imprison, Ev::Start) => Res::Undef,

            // ---- Attract
            // onStart(pokemon, source, effect)
            (VolKind::Attract, Ev::Start) => {
                let Some(source) = e.source else {
                    return FALSE;
                };
                if !opposite_genders(self.mon(holder).gender, self.mon(source).gender) {
                    return FALSE;
                }
                // (The Attract event only has Destiny Knot listening, which is not in Champions.)
                Res::Undef
            }
            // onUpdate(pokemon): ends when the Pokémon it is attracted to leaves.
            (VolKind::Attract, Ev::Update) => {
                let source = self.vols(holder).get(VolKind::Attract).and_then(|v| v.source);
                if source.is_some_and(|s| !self.mon(s).is_active) {
                    self.remove_volatile(holder, VolKind::Attract);
                }
                Res::Undef
            }
            // onBeforeMove(pokemon, target, move)
            (VolKind::Attract, Ev::BeforeMove) => {
                if self.chance(1, 2, "attract") {
                    return FALSE;
                }
                Res::Undef
            }
            (VolKind::Attract, Ev::End) => Res::Undef,

            // ---- Heal Block
            // onStart(pokemon, source)
            (VolKind::Healblock, Ev::Start) => {
                if let Some(source) = e.source {
                    self.mon_mut(source).move_this_turn = TRUE;
                }
                Res::Undef
            }
            // onDisableMove(pokemon)
            (VolKind::Healblock, Ev::DisableMove) => {
                self.disable_moves_where(holder, |d| d.flags & F_HEAL != 0);
                Res::Undef
            }
            // onBeforeMove(pokemon, target, move) and onModifyMove(move, pokemon)
            (VolKind::Healblock, Ev::BeforeMove | Ev::ModifyMove) => {
                if self.event_move_flags() & F_HEAL != 0 {
                    // `cant|pokemon|move: Heal Block|Move`
                    if self.event_move().is_none_or(|mi| !matches!(self.am[mi as usize].source_effect, Eff::Ability(_)))
                    {
                        self.show_attempted(Some(holder));
                    }
                    return FALSE;
                }
                Res::Undef
            }
            (VolKind::Healblock, Ev::End) => Res::Undef,
            // onTryHeal(damage, target, source, effect): nothing heals. (Its special
            // message for Pollen Puff aimed at an ally does not change the outcome.)
            (VolKind::Healblock, Ev::TryHeal) => {
                if self.eff_is_named(e.effect, "pollenpuff") && e.source.is_some_and(|s| s != holder) {
                    let m = self.mon(holder);
                    if m.hp != m.max_hp() {
                        return Res::Null;
                    }
                }
                FALSE
            }
            // onRestart(target, source, effect)
            (VolKind::Healblock, Ev::Restart) => {
                if self.eff_is_named(e.effect, "psychicnoise") {
                    return Res::Undef;
                }
                if let Some(source) = e.source {
                    if !self.mon(source).move_this_turn.truthy() {
                        self.mon_mut(source).move_this_turn = FALSE;
                    }
                }
                Res::Undef
            }

            // ---- Substitute. `data` is the HP the substitute has left, in half points.
            // onStart(target, source, effect)
            (VolKind::Substitute, Ev::Start) => {
                let hp = self.mon(holder).max_hp() / 4;
                if let Some(v) = self.vol_mut(holder, VolKind::Substitute) {
                    v.data = 2 * hp;
                }
                // A substitute frees its maker from Wrap and the like, without the usual ending.
                if let Some(k) = VolKind::named("partiallytrapped") {
                    self.drop_vol(holder, k);
                }
                Res::Undef
            }
            // onTryPrimaryHit(target, source, move): the substitute takes the hit.
            (VolKind::Substitute, Ev::TryPrimaryHit) => {
                let (Eff::Move(mi), Some(source)) = (e.effect, e.source) else {
                    return Res::Undef;
                };
                let target = holder;
                let am = &self.am[mi as usize];
                if target == source || am.flags & F_BYPASSSUB != 0 || am.infiltrates {
                    return Res::Undef;
                }
                let r = self.move_damage(source, target, mi);
                if !r.hit() {
                    // No damage to deal (a status move, an immunity): the move fails.
                    return Res::Null;
                }
                // Everything here is in half points: Metal Burst's damage can end in one.
                let left = self.vols(target).get(VolKind::Substitute).map_or(0, |v| v.data) as i32;
                let damage = (2 * r.num() + self.am[mi as usize].half_damage as i32).min(left);
                if let Some(v) = self.vol_mut(target, VolKind::Substitute) {
                    v.data = (left - damage) as u16;
                }
                if left - damage <= 0 {
                    self.remove_volatile(target, VolKind::Substitute);
                }
                if damage > 0 {
                    self.apply_recoil_halves(damage as u32, mi, source);
                }
                let drain = self.am[mi as usize].d().drain;
                if drain.0 > 0 {
                    let amount = (damage as u32 * drain.0 as u32).div_ceil(2 * drain.1 as u32);
                    self.heal(amount as i32, Some(source), Some(target), Eff::Drain);
                }
                let me = Eff::Move(mi);
                // (No handler reads the amount.)
                self.single_event(Ev::AfterSubDamage, me, None, Some(target), Some(source), me, Res::Num(damage / 2));
                self.run_event(Ev::AfterSubDamage, Some(target), Some(source), me, Res::Num(damage / 2));
                HIT_SUBSTITUTE
            }
            (VolKind::Substitute, Ev::End) => Res::Undef,

            // ---- Aqua Ring, Ingrain: onResidual(pokemon) heals 1/16.
            (VolKind::Aquaring | VolKind::Ingrain, Ev::Start) => Res::Undef,
            (VolKind::Aquaring | VolKind::Ingrain, Ev::Residual) => {
                let amount = div1(self.mon(holder).max_hp() as u32, 16);
                self.heal(amount, None, None, Eff::None);
                Res::Undef
            }
            // Ingrain: onTrapPokemon(pokemon), and onDragOut(pokemon) refuses to be forced out.
            (VolKind::Ingrain, Ev::TrapPokemon) => {
                self.try_trap(holder, false);
                Res::Undef
            }
            (VolKind::Ingrain, Ev::DragOut) => Res::Null,

            // ---- Leech Seed: drains into whoever stands where its user stood.
            (VolKind::Leechseed, Ev::Start) => Res::Undef,
            // onResidual(pokemon)
            (VolKind::Leechseed, Ev::Residual) => {
                let slot = self.vols(holder).get(VolKind::Leechseed).map_or(NO_SLOT, |v| v.source_slot);
                let Some(target) = self.at_slot(slot) else {
                    return Res::Undef;
                };
                if self.mon(target).fainted || self.mon(target).hp == 0 {
                    return Res::Undef;
                }
                let d = div1(self.mon(holder).max_hp() as u32, 8);
                let dealt = self.damage(d, Some(holder), Some(target), Eff::None);
                if dealt.truthy() {
                    self.heal(dealt.num(), Some(target), Some(holder), Eff::None);
                }
                Res::Undef
            }

            // ---- Focus Energy, Dragon Cheer: mutually exclusive critical-hit boosts.
            (VolKind::Focusenergy, Ev::Start) => {
                if self.vols(holder).has(VolKind::Dragoncheer) {
                    return FALSE;
                }
                Res::Undef
            }
            // onModifyCritRatio(critRatio)
            (VolKind::Focusenergy, Ev::ModifyCritRatio) => Res::Num(e.relay.num() + 2),
            // onStart(target, source, effect). `data` is whether the holder was a Dragon when cheered.
            (VolKind::Dragoncheer, Ev::Start) => {
                if self.vols(holder).has(VolKind::Focusenergy) {
                    return FALSE;
                }
                let dragon = self.has_type(holder, Type::Dragon);
                if let Some(v) = self.vol_mut(holder, VolKind::Dragoncheer) {
                    v.data = dragon as u16;
                }
                Res::Undef
            }
            (VolKind::Dragoncheer, Ev::ModifyCritRatio) => {
                let dragon = self.vols(holder).get(VolKind::Dragoncheer).is_some_and(|v| v.data != 0);
                Res::Num(e.relay.num() + if dragon { 2 } else { 1 })
            }

            // ---- Magnet Rise (the levitation itself is in `is_grounded`)
            (VolKind::Magnetrise, Ev::Start | Ev::End) => Res::Undef,
            // onImmunity(type) answers for the Ground type, which nothing asks it about.
            (VolKind::Magnetrise, Ev::Immunity) => Res::Undef,

            // ---- Minimize: moves that punish it never miss and hit twice as hard.
            // onRestart: () => null
            (VolKind::Minimize, Ev::Restart) => Res::Null,
            // onAccuracy(accuracy, target, source, move)
            (VolKind::Minimize, Ev::Accuracy) => {
                if self.event_move_flags() & F_MINIMIZE != 0 {
                    return TRUE;
                }
                e.relay
            }

            // ---- No Retreat (Champions): onStart(pokemon). It traps through "trapped", and stays
            // on a Pokémon already trapped (unannounced), so it cannot be used again.
            (VolKind::Noretreat, Ev::Start) => {
                self.add_volatile(holder, VolKind::Trapped, None, Eff::None);
                Res::Undef
            }
            // ---- The "trapped" of Block, Mean Look, Jaw Lock and No Retreat
            (VolKind::Trapped, Ev::Start) => Res::Undef,
            // onTrapPokemon(pokemon)
            (VolKind::Trapped, Ev::TrapPokemon) => {
                self.try_trap(holder, false);
                Res::Undef
            }

            // ---- Octolock
            (VolKind::Octolock, Ev::Start) => Res::Undef,
            // onResidual(pokemon)
            (VolKind::Octolock, Ev::Residual) => {
                let source = self.vols(holder).get(kind).and_then(|v| v.source);
                if let Some(s) = source {
                    let m = self.mon(s);
                    if !m.is_active || m.hp == 0 || m.active_turns == 0 {
                        self.drop_vol(holder, kind);
                        return Res::Undef;
                    }
                }
                let saved = self.am_len;
                let octolock = self.new_am(mv::OCTOLOCK);
                let mut b = [0i8; 7];
                b[DEF] = -1;
                b[SPD] = -1;
                self.boost(b, Some(holder), source, Eff::Move(octolock));
                self.am_len = saved;
                Res::Undef
            }
            // onTrapPokemon(pokemon)
            (VolKind::Octolock, Ev::TrapPokemon) => {
                let source = self.vols(holder).get(kind).and_then(|v| v.source);
                if source.is_some_and(|s| self.mon(s).is_active) {
                    self.try_trap(holder, false);
                }
                Res::Undef
            }

            // ---- Power Trick: Attack and Defense change places, and back again when it ends.
            (VolKind::Powertrick, Ev::Start | Ev::Copy | Ev::End) => {
                self.mon_mut(holder).stats.swap(ATK + 1, DEF + 1);
                Res::Undef
            }
            // onRestart(pokemon)
            (VolKind::Powertrick, Ev::Restart) => {
                self.remove_volatile(holder, VolKind::Powertrick);
                Res::Undef
            }

            // ---- Smack Down: only sticks to something that was off the ground.
            // onStart(pokemon)
            (VolKind::Smackdown, Ev::Start) => {
                let mut applies = self.has_type(holder, Type::Flying)
                    || self.has_ability(holder, ab::LEVITATE)
                    || self.has_ability(holder, ab::EELEVATE);
                if self.has_item(holder, it::IRONBALL)
                    || self.has_vol_named(holder, "ingrain")
                    || self.field.pseudo.has(Pseudo::Gravity)
                {
                    applies = false;
                }
                applies |= self.smack_out_of_the_air(holder);
                for id in ["magnetrise", "telekinesis"] {
                    if let Some(k) = VolKind::named(id) {
                        applies |= self.drop_vol(holder, k);
                    }
                }
                if !applies {
                    return FALSE;
                }
                Res::Undef
            }
            // onRestart(pokemon)
            (VolKind::Smackdown, Ev::Restart) => {
                self.smack_out_of_the_air(holder);
                Res::Undef
            }

            // ---- Chilly Reception: onBeforeMove(source, target, move) announces the move before
            // anything can stop it: `-prepare|source|Chilly Reception|[premajor]`
            (VolKind::Chillyreception, Ev::BeforeMove) => {
                if matches!(e.effect, Eff::Move(mi) if self.am[mi as usize].id == mv::CHILLYRECEPTION) {
                    self.show_move(holder, mv::CHILLYRECEPTION);
                }
                Res::Undef
            }

            // ---- Fling: onUpdate(pokemon). The thrown item is gone, whether or not it hit anything.
            (VolKind::Fling, Ev::Update) => {
                let item = self.mon(holder).item;
                self.set_item(holder, it::NONE, None, Eff::None);
                // `-enditem|pokemon|Item|[from] move: Fling`
                self.show_item_lost(holder, item);
                let m = self.mon_mut(holder);
                m.last_item = item;
                m.used_item_this_turn = true;
                let mut ee = Event::new(Ev::AfterUseItem, Some(holder), None, Eff::None);
                ee.item = item;
                self.run_event_ex(ee, Res::Undef, false, false);
                self.remove_volatile(holder, VolKind::Fling);
                Res::Undef
            }

            // ---- Curse (a Ghost's): onResidual(pokemon) takes a quarter of its HP.
            (VolKind::Curse, Ev::Start) => Res::Undef,
            (VolKind::Curse, Ev::Residual) => {
                let d = div1(self.mon(holder).max_hp() as u32, 4);
                self.damage(d, None, None, Eff::None);
                Res::Undef
            }

            // ---- Salt Cure
            (VolKind::Saltcure, Ev::Start | Ev::End) => Res::Undef,
            // onResidual(pokemon)
            (VolKind::Saltcure, Ev::Residual) => {
                let weak = self.has_type(holder, Type::Water) || self.has_type(holder, Type::Steel);
                let d = div1(self.mon(holder).max_hp() as u32, if weak { 8 } else { 16 });
                self.damage(d, None, None, Eff::None);
                Res::Undef
            }

            // ---- Syrup Bomb
            (VolKind::Syrupbomb, Ev::Start | Ev::End) => Res::Undef,
            // onUpdate(pokemon): ends when its user leaves.
            (VolKind::Syrupbomb, Ev::Update) => {
                let source = self.vols(holder).get(kind).and_then(|v| v.source);
                if source.is_some_and(|s| !self.mon(s).is_active) {
                    self.remove_volatile(holder, kind);
                }
                Res::Undef
            }
            // onResidual(pokemon)
            (VolKind::Syrupbomb, Ev::Residual) => {
                let source = self.vols(holder).get(kind).and_then(|v| v.source);
                self.boost1(SPE, -1, Some(holder), source, Eff::None);
                Res::Undef
            }

            // ---- Throat Chop: no sound moves.
            (VolKind::Throatchop, Ev::Start | Ev::End) => Res::Undef,
            // onDisableMove(pokemon)
            (VolKind::Throatchop, Ev::DisableMove) => {
                self.disable_moves_where(holder, |d| d.flags & F_SOUND != 0);
                Res::Undef
            }
            // onBeforeMove(pokemon, target, move) and onModifyMove(move, pokemon, target)
            (VolKind::Throatchop, Ev::BeforeMove | Ev::ModifyMove) => {
                if self.event_move_flags() & F_SOUND != 0 {
                    return FALSE;
                }
                Res::Undef
            }

            // ---- Charge: the next Electric move is doubled.
            (VolKind::Charge, Ev::Start | Ev::Restart | Ev::End) => Res::Undef,
            // onBasePower(basePower, attacker, defender, move)
            (VolKind::Charge, Ev::BasePower) => {
                if matches!(e.effect, Eff::Move(mi) if self.am[mi as usize].typ == Type::Electric) {
                    return self.chain_modify(2, 1);
                }
                Res::Undef
            }
            // onMoveAborted(pokemon, target, move) and onAfterMove(pokemon, target, move)
            (VolKind::Charge, Ev::MoveAborted | Ev::AfterMove) => {
                if let Eff::Move(mi) = e.effect {
                    let am = &self.am[mi as usize];
                    if am.typ == Type::Electric && am.id != mv::CHARGE {
                        self.remove_volatile(holder, VolKind::Charge);
                    }
                }
                Res::Undef
            }

            // ---- Destiny Bond: until the holder's next move, whoever knocks it out goes down too.
            (VolKind::Destinybond, Ev::Start) => Res::Undef,
            // onFaint(target, source, effect)
            (VolKind::Destinybond, Ev::Faint) => {
                let Some(source) = e.source else {
                    return Res::Undef;
                };
                if self.is_ally(holder, source) {
                    return Res::Undef;
                }
                if matches!(e.effect, Eff::Move(mi) if self.am[mi as usize].flags & F_FUTUREMOVE == 0) {
                    self.faint(source, None, Eff::None);
                }
                Res::Undef
            }
            // onBeforeMove(pokemon, target, move)
            (VolKind::Destinybond, Ev::BeforeMove) => {
                if matches!(e.effect, Eff::Move(mi) if self.am[mi as usize].id == mv::DESTINYBOND) {
                    return Res::Undef;
                }
                self.remove_volatile(holder, VolKind::Destinybond);
                Res::Undef
            }
            // onMoveAborted(pokemon, target, move)
            (VolKind::Destinybond, Ev::MoveAborted) => {
                self.remove_volatile(holder, VolKind::Destinybond);
                Res::Undef
            }

            // ---- Electrify: the holder's move this turn becomes Electric.
            (VolKind::Electrify, Ev::Start) => Res::Undef,
            // onModifyType(move)
            (VolKind::Electrify, Ev::ModifyType) => {
                if let Eff::Move(mi) = e.effect {
                    if self.am[mi as usize].id != mv::STRUGGLE {
                        self.am[mi as usize].typ = Type::Electric;
                    }
                }
                Res::Undef
            }

            // ---- Gastro Acid (what it suppresses is in `ignoring_ability`)
            // onStart(pokemon): the ability gets to say goodbye first.
            (VolKind::Gastroacid, Ev::Start) => {
                let ability = self.mon(holder).ability;
                self.single_event(
                    Ev::End,
                    Eff::Ability(ability),
                    Some(holder),
                    Some(holder),
                    Some(holder),
                    Eff::Vol(kind),
                    Res::Undef,
                );
                Res::Undef
            }
            // onCopy(pokemon)
            (VolKind::Gastroacid, Ev::Copy) => {
                if ABILITIES[self.mon(holder).ability as usize].flags & AF_CANTSUPPRESS != 0 {
                    self.remove_volatile(holder, kind);
                }
                Res::Undef
            }

            // ---- Perish Song: the holder faints when the count runs out.
            // onEnd(target)
            (VolKind::Perishsong, Ev::End) => {
                self.faint(holder, None, Eff::None);
                Res::Undef
            }
            // onResidual only announces the count.
            (VolKind::Perishsong, Ev::Residual) => Res::Undef,

            // ---- Yawn: asleep at the end of the next turn.
            (VolKind::Yawn, Ev::Start) => Res::Undef,
            // onEnd(target)
            (VolKind::Yawn, Ev::End) => {
                let source = self.vols(holder).get(kind).and_then(|v| v.source);
                self.try_set_status(holder, Status::Slp, source, Eff::None);
                Res::Undef
            }

            // ---- Glaive Rush: until the holder's next move, everything hits it, twice as hard.
            (VolKind::Glaiverush, Ev::Start) => Res::Undef,
            // onAccuracy()
            (VolKind::Glaiverush, Ev::Accuracy) => TRUE,
            // onBeforeMove(pokemon)
            (VolKind::Glaiverush, Ev::BeforeMove) => {
                self.remove_volatile(holder, kind);
                Res::Undef
            }

            // ---- Stockpile. `data` is the number of layers; `st.a` and `st.b` are
            // minus the Defense and Sp. Def stages it actually managed to raise.
            // onStart(target) and onRestart(target)
            (VolKind::Stockpile, Ev::Start | Ev::Restart) => {
                if ev == Ev::Restart && self.vols(holder).get(kind).is_some_and(|v| v.data >= 3) {
                    return FALSE;
                }
                if let Some(v) = self.vol_mut(holder, kind) {
                    if ev == Ev::Start {
                        v.data = 1;
                        v.st.a = 0;
                        v.st.b = 0;
                    } else {
                        v.data += 1;
                    }
                }
                let before = self.mon(holder).boosts;
                let mut b = [0i8; 7];
                b[DEF] = 1;
                b[SPD] = 1;
                self.boost(b, Some(holder), Some(holder), Eff::None);
                let after = self.mon(holder).boosts;
                if let Some(v) = self.vol_mut(holder, kind) {
                    if before[DEF] != after[DEF] {
                        v.st.a -= 1;
                    }
                    if before[SPD] != after[SPD] {
                        v.st.b -= 1;
                    }
                }
                Res::Undef
            }
            // onEnd(target): the stages it raised come back off.
            (VolKind::Stockpile, Ev::End) => {
                let (def, spd) = self.vols(holder).get(kind).map_or((0, 0), |v| (v.st.a, v.st.b));
                if def != 0 || spd != 0 {
                    let mut b = [0i8; 7];
                    b[DEF] = def as i8;
                    b[SPD] = spd as i8;
                    self.boost(b, Some(holder), Some(holder), Eff::None);
                }
                Res::Undef
            }

            // ---- Wrap, Fire Spin and the other binding moves. `data` is the damage divisor.
            // onStart(pokemon, source)
            (VolKind::Partiallytrapped, Ev::Start) => {
                let band = e.source.is_some_and(|s| self.has_item(s, it::BINDINGBAND));
                if let Some(v) = self.vol_mut(holder, kind) {
                    v.data = if band { 6 } else { 8 };
                    let (uid, length) = (v.st.uid, v.duration);
                    self.note_count_began(uid, length, false);
                }
                Res::Undef
            }
            // onResidual(pokemon)
            (VolKind::Partiallytrapped, Ev::Residual) => {
                let (source, divisor) = self.vols(holder).get(kind).map_or((None, 8), |v| (v.source, v.data.max(1)));
                if let Some(s) = source {
                    let m = self.mon(s);
                    if !m.is_active || m.hp == 0 || m.active_turns == 0 {
                        self.drop_vol(holder, kind);
                        return Res::Undef;
                    }
                }
                let d = div1(self.mon(holder).max_hp() as u32, divisor as u32);
                self.damage(d, None, None, Eff::None);
                Res::Undef
            }
            (VolKind::Partiallytrapped, Ev::End) => Res::Undef,
            // onTrapPokemon(pokemon)
            (VolKind::Partiallytrapped, Ev::TrapPokemon) => {
                let source = self.vols(holder).get(kind).and_then(|v| v.source);
                if source.is_some_and(|s| self.mon(s).is_active) {
                    self.try_trap(holder, false);
                }
                Res::Undef
            }

            // ============================== moves that take more than one turn
            // ---- A rampage (Outrage, Thrash, ...). `data` is the move (table index
            // plus one); `st.a` counts down the two or three turns it really lasts.
            // onStart(target, source, effect)
            (VolKind::Lockedmove, Ev::Start) => {
                let turns = self.rand_range(2, 4, "rampage turns") as i16;
                let id = match e.effect {
                    Eff::Move(mi) => self.am[mi as usize].id + 1,
                    _ => 0,
                };
                if let Some(v) = self.vol_mut(holder, kind) {
                    v.st.a = turns;
                    v.data = id;
                    let uid = v.st.uid;
                    self.note_count_began(uid, turns as u8, false);
                }
                Res::Undef
            }
            // onRestart(): each use renews the lock while turns remain.
            (VolKind::Lockedmove, Ev::Restart) => {
                if let Some(v) = self.vol_mut(holder, kind) {
                    if v.st.a >= 2 {
                        v.duration = 2;
                    }
                }
                Res::Undef
            }
            // onResidual(target)
            (VolKind::Lockedmove, Ev::Residual) => {
                if self.mon(holder).status == Status::Slp {
                    self.drop_vol(holder, kind);
                } else if let Some(v) = self.vol_mut(holder, kind) {
                    v.st.a -= 1;
                }
                Res::Undef
            }
            // onAfterMove(pokemon)
            (VolKind::Lockedmove, Ev::AfterMove) => {
                if self.vols(holder).get(kind).is_some_and(|v| v.duration == 1) {
                    self.remove_volatile(holder, kind);
                }
                Res::Undef
            }
            // onEnd(target): a rampage that ran its course leaves the user confused.
            (VolKind::Lockedmove, Ev::End) => {
                if self.vols(holder).get(kind).is_some_and(|v| v.st.a > 1) {
                    return Res::Undef;
                }
                self.add_volatile(holder, VolKind::Confusion, None, Eff::None);
                Res::Undef
            }
            // onLockMove(pokemon)
            (VolKind::Lockedmove | VolKind::Twoturnmove, Ev::LockMove) => {
                Res::Num(self.vols(holder).get(kind).map_or(0, |v| v.data) as i32)
            }

            // ---- Recharging after Hyper Beam and the like.
            (VolKind::Mustrecharge, Ev::Start) => Res::Undef,
            // onBeforeMove(pokemon): the turn is spent.
            (VolKind::Mustrecharge, Ev::BeforeMove) => {
                self.remove_volatile(holder, kind);
                if let Some(k) = VolKind::named("truant") {
                    self.remove_volatile(holder, k);
                }
                Res::Null
            }
            // onLockMove: 'recharge'
            (VolKind::Mustrecharge, Ev::LockMove) => Res::Num(mv::RECHARGE as i32 + 1),

            // ---- The first turn of a charging move. `data` is the move (table
            // index plus one). The user also gets a volatile named after the move,
            // whose `st.a` remembers where the move was aimed.
            // onStart(attacker, defender, effect)
            (VolKind::Twoturnmove, Ev::Start) => {
                let Eff::Move(mi) = e.effect else {
                    return Res::Undef;
                };
                let id = self.am[mi as usize].id;
                let Some(own) = VolKind::named(MOVES[id as usize].id) else {
                    return FALSE;
                };
                if let Some(v) = self.vol_mut(holder, kind) {
                    v.data = id + 1;
                }
                self.add_volatile(holder, own, None, Eff::None);
                let mut defender = e.source;
                let mut loc = self.mon(holder).last_move_loc;
                // Called by another move, it remembers the target it was given.
                if self.am[mi as usize].source_effect != Eff::None && MOVES[id as usize].target != Target::User {
                    if defender.is_some_and(|d| self.mon(d).fainted) {
                        let foe = 1 - holder.side as usize;
                        let pick = self.rand(ACTIVE as u32, "charging move target") as usize;
                        defender = Some(self.active(foe, pick));
                    }
                    if let Some(d) = defender {
                        loc = self.loc_of(holder, d);
                    }
                }
                if let Some(v) = self.vol_mut(holder, own) {
                    v.st.a = loc as i16;
                }
                self.run_event(Ev::PrepareHit, Some(holder), defender, Eff::Move(mi), Res::Undef);
                Res::Undef
            }
            // onEnd(target)
            (VolKind::Twoturnmove, Ev::End) => {
                let id = self.vols(holder).get(kind).map_or(0, |v| v.data);
                if id > 0 {
                    if let Some(own) = VolKind::named(MOVES[id as usize - 1].id) {
                        self.remove_volatile(holder, own);
                    }
                }
                Res::Undef
            }
            // onMoveAborted(pokemon)
            (VolKind::Twoturnmove, Ev::MoveAborted) => {
                self.remove_volatile(holder, kind);
                Res::Undef
            }

            // ---- Out of reach: Fly, Bounce, Dig, Dive, Phantom Force.
            // onInvulnerability(target, source, move): `false` unless the move is one that reaches.
            (
                VolKind::Fly | VolKind::Bounce | VolKind::Dig | VolKind::Dive | VolKind::Phantomforce,
                Ev::Invulnerability,
            ) => {
                let reaches: &[&str] = match kind {
                    VolKind::Fly | VolKind::Bounce => {
                        &["gust", "twister", "skyuppercut", "thunder", "hurricane", "smackdown", "thousandarrows"]
                    }
                    VolKind::Dig => &["earthquake", "magnitude"],
                    VolKind::Dive => &["surf", "whirlpool"],
                    _ => &[],
                };
                if matches!(e.effect, Eff::Move(mi) if reaches.contains(&self.am[mi as usize].d().id)) {
                    return Res::Undef;
                }
                FALSE
            }
            // onImmunity(type, pokemon): underground and underwater, the weather does not reach.
            (VolKind::Dig | VolKind::Dive, Ev::Immunity) => {
                if matches!(e.imm, Some(Imm::Weather(Weather::Sandstorm))) {
                    return FALSE;
                }
                Res::Undef
            }

            // ---- Focus Punch's focus. `data` is set once it is lost.
            // onStart(pokemon): `-singleturn|pokemon|move: Focus Punch`, at the start of the turn,
            // says which move it has chosen.
            (VolKind::Focuspunch | VolKind::Beakblast, Ev::Start) => {
                self.show_move(holder, if kind == VolKind::Focuspunch { mv::FOCUSPUNCH } else { mv::BEAKBLAST });
                Res::Undef
            }
            // onHit(pokemon, source, move): any damaging move breaks it.
            (VolKind::Focuspunch, Ev::Hit) => {
                if matches!(e.effect, Eff::Move(mi) if self.am[mi as usize].category != Category::Status) {
                    if let Some(v) = self.vol_mut(holder, kind) {
                        v.data = 1;
                    }
                }
                Res::Undef
            }
            // onTryAddVolatile(status, pokemon): it cannot flinch meanwhile.
            (VolKind::Focuspunch, Ev::TryAddVolatile) => {
                if e.vol == Some(VolKind::Flinch) {
                    return Res::Null;
                }
                Res::Undef
            }
            // ---- Beak Blast's heated beak: onHit(target, source, move) burns on contact.
            (VolKind::Beakblast, Ev::Hit) => {
                if let (Eff::Move(mi), Some(source)) = (e.effect, e.source) {
                    if self.makes_contact(mi) {
                        self.try_set_status(source, Status::Brn, Some(holder), Eff::None);
                    }
                }
                Res::Undef
            }

            // ---- Counter, Mirror Coat. `st.a` is the field slot (plus one) of the
            // foe that hit the holder, `data` twice the damage it did.
            (VolKind::Counter | VolKind::Mirrorcoat, Ev::Start) => {
                if let Some(v) = self.vol_mut(holder, kind) {
                    v.st.a = 0;
                    v.data = 0;
                }
                Res::Undef
            }
            // onRedirectTarget(target, source, source2, move): the move goes back where the hit came from.
            (VolKind::Counter | VolKind::Mirrorcoat, Ev::RedirectTarget) => {
                let own = if kind == VolKind::Counter { mv::COUNTER } else { mv::MIRRORCOAT };
                if !matches!(e.effect, Eff::Move(mi) if self.am[mi as usize].id == own) {
                    return Res::Undef;
                }
                let slot = self.vols(holder).get(kind).map_or(0, |v| v.st.a);
                if e.target != Some(holder) || slot == 0 {
                    return Res::Undef;
                }
                match self.at_slot(slot as u8 - 1) {
                    Some(r) => Res::Mon(r),
                    None => Res::Undef,
                }
            }
            // onDamagingHit(damage, target, source, move)
            (VolKind::Counter | VolKind::Mirrorcoat, Ev::DamagingHit) => {
                let (Eff::Move(mi), Some(source)) = (e.effect, e.source) else {
                    return Res::Undef;
                };
                let wanted = if kind == VolKind::Counter { Category::Physical } else { Category::Special };
                if !self.is_ally(source, holder) && self.am[mi as usize].category == wanted {
                    let slot = self.field_slot(source);
                    let damage = e.relay.num();
                    if let Some(v) = self.vol_mut(holder, kind) {
                        v.st.a = slot as i16 + 1;
                        v.data = (2 * damage).clamp(0, u16::MAX as i32) as u16;
                    }
                }
                Res::Undef
            }

            // ---- Roost: the user is not Flying this turn. That is its onType(types, pokemon),
            // which `get_types` applies directly.
            (VolKind::Roost, Ev::Start | Ev::Type) => Res::Undef,

            // ---- Uproar: three turns of it, and nobody sleeps meanwhile (see `vol_cb_prefixed`).
            (VolKind::Uproar, Ev::Start | Ev::End) => Res::Undef,
            // onResidual(target)
            (VolKind::Uproar, Ev::Residual) => {
                if self.vols(holder).has(VolKind::Throatchop) {
                    self.remove_volatile(holder, kind);
                    return Res::Undef;
                }
                if self.mon(holder).last_move == mv::STRUGGLE {
                    self.drop_vol(holder, kind);
                }
                Res::Undef
            }
            // onLockMove: 'uproar'
            (VolKind::Uproar, Ev::LockMove) => Res::Num(mv::UPROAR as i32 + 1),
            _ => unreachable!("no body for {kind:?} {ev:?}"),
        }
    }

    /// Callbacks a volatile has on events about other Pokémon (`onFoe...`).
    /// `holder` has the volatile; the event's target is the other Pokémon.
    fn vol_cb_prefixed(&mut self, kind: VolKind, ev: Ev, pre: Pre, holder: MonRef) -> Res {
        let e = self.event;
        match (kind, ev, pre) {
            // onFoeRedirectTarget(target, source, source2, move): `source` is the Pokémon using the move.
            (VolKind::Followme | VolKind::Ragepowder, Ev::RedirectTarget, Pre::Foe) => {
                let (Some(user), Eff::Move(mi)) = (e.target, e.effect) else {
                    return Res::Undef;
                };
                if kind == VolKind::Ragepowder && !self.run_status_immunity(user, Imm::Powder) {
                    return Res::Undef;
                }
                let loc = self.loc_of(user, holder);
                if self.valid_target_loc(loc, user, self.am[mi as usize].target) {
                    self.am[mi as usize].smart_target = false;
                    return Res::Mon(holder);
                }
                Res::Undef
            }

            // Imprison: the foes cannot choose or use any move its user knows.
            // onFoeDisableMove(pokemon)
            (VolKind::Imprison, Ev::DisableMove, Pre::Foe) => {
                let (Some(pokemon), Some(source)) = (e.target, self.vols(holder).get(kind).and_then(|v| v.source))
                else {
                    return Res::Undef;
                };
                let known = self.mon(source).moves;
                let n = self.mon(source).n_moves as usize;
                self.disable_slots_hidden_where(pokemon, |s| {
                    s.id != mv::STRUGGLE && known[..n].iter().any(|k| k.id == s.id)
                });
                Res::Undef
            }
            // onFoeBeforeMove(attacker, defender, move)
            (VolKind::Imprison, Ev::BeforeMove, Pre::Foe) => {
                let (Eff::Move(mi), Some(source)) = (e.effect, self.vols(holder).get(kind).and_then(|v| v.source))
                else {
                    return Res::Undef;
                };
                let id = self.am[mi as usize].id;
                if id != mv::STRUGGLE && self.move_slot(source, id).is_some() {
                    // `cant|attacker|move: Imprison|Move`
                    self.show_attempted(e.target);
                    // Which says, to anyone who did not know, that the sealer has the move.
                    if !self.open_team_sheets
                        && let Some(entry) = self.imprisoner(holder.side as usize)
                    {
                        self.sides[holder.side as usize].roster[entry].tell(id);
                    }
                    return FALSE;
                }
                Res::Undef
            }

            // Minimize, Glaive Rush: onSourceModifyDamage. The holder is the one being hit.
            (VolKind::Minimize, Ev::ModifyDamage, Pre::Source) => {
                if self.event_move_flags() & F_MINIMIZE != 0 {
                    return self.chain_modify(2, 1);
                }
                Res::Undef
            }
            (VolKind::Glaiverush, Ev::ModifyDamage, Pre::Source) => self.chain_modify(2, 1),

            // The moves that reach a Pokémon in mid-move hit it twice as hard:
            // onSourceModifyDamage (Fly, Dig, Dive) or onSourceBasePower (Bounce).
            (VolKind::Fly | VolKind::Dig | VolKind::Dive, Ev::ModifyDamage, Pre::Source)
            | (VolKind::Bounce, Ev::BasePower, Pre::Source) => {
                let doubled: &[&str] = match kind {
                    VolKind::Fly | VolKind::Bounce => &["gust", "twister"],
                    VolKind::Dig => &["earthquake", "magnitude"],
                    _ => &["surf", "whirlpool"],
                };
                if matches!(e.effect, Eff::Move(mi) if doubled.contains(&self.am[mi as usize].d().id)) {
                    return self.chain_modify(2, 1);
                }
                Res::Undef
            }

            // Uproar: onAnySetStatus(status, pokemon). Nobody falls asleep.
            (VolKind::Uproar, Ev::SetStatus, Pre::Any) => {
                if e.status == Status::Slp {
                    return Res::Null;
                }
                Res::Undef
            }

            // Lock-On: the holder's moves cannot miss the Pokémon it locked on to.
            // onSourceInvulnerability(target, source, move) and onSourceAccuracy(accuracy, target, source, move)
            (VolKind::Lockon, Ev::Invulnerability | Ev::Accuracy, Pre::Source) => {
                let locked = self.vols(holder).get(kind).and_then(|v| v.source);
                if matches!(e.effect, Eff::Move(_)) && e.source == Some(holder) && e.target == locked {
                    return if ev == Ev::Accuracy { TRUE } else { Res::Num(0) };
                }
                Res::Undef
            }
            _ => unreachable!("no body for {kind:?} {ev:?} {pre:?}"),
        }
    }

    /// Smack Down's check for a target in the air mid-move (Fly, Bounce): it
    /// comes down and loses its turn.
    fn smack_out_of_the_air(&mut self, r: MonRef) -> bool {
        let mut hit = false;
        for id in ["fly", "bounce"] {
            if let Some(k) = VolKind::named(id) {
                hit |= self.remove_volatile(r, k);
            }
        }
        if hit {
            self.cancel_move(r);
            if let Some(k) = VolKind::named("twoturnmove") {
                self.remove_volatile(r, k);
            }
        }
        hit
    }

    /// `Battle#getAtSlot`: whoever is in a field position now.
    pub(crate) fn at_slot(&self, slot: u8) -> Option<MonRef> {
        if slot == NO_SLOT {
            return None;
        }
        Some(self.active((slot / 2) as usize, (slot % 2) as usize))
    }

    /// What every Protect-like condition does to an attacker it stops: a
    /// rampage (Outrage and the like) on its last turn ends without confusing.
    pub(crate) fn protect_unlocks(&mut self, source: MonRef) {
        if let Some(k) = VolKind::named("lockedmove") {
            if self.vols(source).get(k).is_some_and(|v| v.duration == 2) {
                self.drop_vol(source, k);
            }
        }
    }

    /// The move slot holding `move_id`, if the Pokémon knows it (`Pokemon#getMoveData`).
    pub(crate) fn move_slot(&self, r: MonRef, move_id: u16) -> Option<&MoveSlot> {
        let m = self.mon(r);
        m.moves[..m.n_moves as usize].iter().find(|s| s.id == move_id)
    }

    /// `Pokemon#disableMove` for every move slot matching `pred`.
    pub(crate) fn disable_slots_where(&mut self, r: MonRef, pred: impl Fn(&MoveSlot) -> bool) {
        let m = self.mon_mut(r);
        for k in 0..m.n_moves as usize {
            if pred(&m.moves[k]) {
                m.moves[k].disabled = true;
                m.moves[k].hidden = false;
            }
        }
    }

    /// `Pokemon#disableMove(id, true)`: disabled without its player being told,
    /// unless something else has disabled the move openly already.
    pub(crate) fn disable_slots_hidden_where(&mut self, r: MonRef, pred: impl Fn(&MoveSlot) -> bool) {
        let m = self.mon_mut(r);
        for k in 0..m.n_moves as usize {
            if pred(&m.moves[k]) && !m.moves[k].disabled {
                m.moves[k].disabled = true;
                m.moves[k].hidden = true;
            }
        }
    }

    /// `Pokemon#disableMove` for every move whose data matches `pred`.
    pub(crate) fn disable_moves_where(&mut self, r: MonRef, pred: impl Fn(&MoveData) -> bool) {
        self.disable_slots_where(r, |s| pred(&MOVES[s.id as usize]));
    }

    /// `durationCallback` of the conditions that have one: how long the
    /// condition lasts, given who it is on and who caused it.
    pub(crate) fn cond_duration(
        &mut self,
        eff: Eff,
        _target: Option<MonRef>,
        source: Option<MonRef>,
        source_effect: Eff,
    ) -> u8 {
        let holds = |b: &Battle, item: u16| source.is_some_and(|s| b.has_item(s, item));
        // (The Persistent ability, which lengthens some of these, is not in Champions.)
        match eff {
            // durationCallback(target, source): 4 or 5 turns of damage.
            // (Grip Claw, which would make it 7, is not in Champions.)
            Eff::Vol(VolKind::Partiallytrapped) => self.rand_range(5, 7, "binding move turns") as u8,
            // durationCallback(target, source, effect): two turns from Psychic Noise.
            Eff::Vol(VolKind::Healblock) => {
                if self.eff_is_named(source_effect, "psychicnoise") {
                    2
                } else {
                    5
                }
            }
            // durationCallback(source, effect): 8 turns with the matching rock.
            Eff::Weather(Weather::Raindance) => {
                if holds(self, it::DAMPROCK) {
                    8
                } else {
                    5
                }
            }
            Eff::Weather(Weather::Sunnyday) => {
                if holds(self, it::HEATROCK) {
                    8
                } else {
                    5
                }
            }
            Eff::Weather(Weather::Sandstorm) => {
                if holds(self, it::SMOOTHROCK) {
                    8
                } else {
                    5
                }
            }
            Eff::Weather(Weather::Snowscape) => {
                if holds(self, it::ICYROCK) {
                    8
                } else {
                    5
                }
            }
            Eff::Terrain(_) => {
                if holds(self, it::TERRAINEXTENDER) {
                    8
                } else {
                    5
                }
            }
            Eff::Pseudo(Pseudo::Trickroom | Pseudo::Gravity | Pseudo::Magicroom | Pseudo::Wonderroom) => 5,
            // durationCallback(target, source, effect)
            Eff::SideCond(SideCond::Tailwind) => 4,
            Eff::SideCond(SideCond::Safeguard) => 5,
            Eff::SideCond(SideCond::Reflect | SideCond::Lightscreen | SideCond::Auroraveil) => {
                if holds(self, it::LIGHTCLAY) { 8 } else { 5 }
            }
            _ => unreachable!("no durationCallback body for {eff:?}"),
        }
    }

    /// Callbacks of side conditions. `side` is the side the condition is on.
    /// `Cond::data` holds the layers of Spikes and Toxic Spikes.
    pub(crate) fn side_cb(&mut self, kind: SideCond, ev: Ev, pre: Pre, side: usize) -> Res {
        let e = self.event;
        match (kind, ev, pre) {
            // ---- Wide Guard, Quick Guard: one-turn protection for the whole side.
            (SideCond::Wideguard | SideCond::Quickguard, Ev::SideStart, Pre::On) => Res::Undef,
            // onTryHit(target, source, move)
            (SideCond::Wideguard | SideCond::Quickguard, Ev::TryHit, Pre::On) => {
                let (Some(target), Some(source), Eff::Move(mi)) = (e.target, e.source, e.effect) else {
                    return Res::Undef;
                };
                let am = &self.am[mi as usize];
                let covered = if kind == SideCond::Wideguard {
                    matches!(am.target, Target::AllAdjacent | Target::AllAdjacentFoes)
                } else {
                    am.priority > 0
                };
                if !covered || self.bypasses_protect(mi, source, target, true) {
                    return Res::Undef;
                }
                self.protect_unlocks(source);
                Res::NotFail
            }

            // ---- Spikes, Toxic Spikes: layers
            // onSideStart(side)
            (SideCond::Spikes | SideCond::Toxicspikes, Ev::SideStart, Pre::On) => {
                if let Some(c) = self.sides[side].conds.get_mut(kind) {
                    c.data = 1;
                }
                Res::Undef
            }
            // onSideRestart(side)
            (SideCond::Spikes | SideCond::Toxicspikes, Ev::SideRestart, Pre::On) => {
                let max = if kind == SideCond::Spikes { 3 } else { 2 };
                match self.sides[side].conds.get_mut(kind) {
                    Some(c) if c.data < max => {
                        c.data += 1;
                        Res::Undef
                    }
                    _ => FALSE,
                }
            }
            // Every other onSideStart(side) and onSideEnd(side) only writes to the log.
            (_, Ev::SideStart | Ev::SideEnd, Pre::On) => Res::Undef,

            // ---- Tailwind: onModifySpe(spe, pokemon)
            (SideCond::Tailwind, Ev::ModifySpe, Pre::On) => self.chain_modify(2, 1),

            // ---- Reflect, Light Screen, Aurora Veil: onAnyModifyDamage(damage, source, target, move).
            // `e.target` is the attacker and `e.source` the Pokémon being hit.
            (SideCond::Reflect | SideCond::Lightscreen | SideCond::Auroraveil, Ev::ModifyDamage, Pre::Any) => {
                let (Some(attacker), Some(defender), Eff::Move(mi)) = (e.target, e.source, e.effect) else {
                    return Res::Undef;
                };
                if defender == attacker || defender.side as usize != side {
                    return Res::Undef;
                }
                let category = self.am[mi as usize].category;
                let covered = match kind {
                    SideCond::Reflect => category == Category::Physical,
                    SideCond::Lightscreen => category == Category::Special,
                    // Aurora Veil steps aside where the matching screen is also up.
                    _ => {
                        let conds = &self.sides[defender.side as usize].conds;
                        !(conds.has(SideCond::Reflect) && category == Category::Physical
                            || conds.has(SideCond::Lightscreen) && category == Category::Special)
                    }
                };
                if !covered {
                    return Res::Undef;
                }
                let am = &self.am[mi as usize];
                if !am.hit_data[self.slot_index(defender)].crit && !am.infiltrates {
                    // The doubles value; in singles it would be a half.
                    return self.chain_modify(2732, 4096);
                }
                Res::Undef
            }

            // ---- Safeguard
            // onSetStatus(status, target, source, effect)
            (SideCond::Safeguard, Ev::SetStatus, Pre::On) => {
                let (Some(target), Some(source)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                if e.effect == Eff::None || self.eff_is_named(e.effect, "yawn") {
                    return Res::Undef;
                }
                if let Eff::Move(mi) = e.effect {
                    if self.am[mi as usize].infiltrates && !self.is_ally(target, source) {
                        return Res::Undef;
                    }
                }
                if target != source {
                    return Res::Null;
                }
                Res::Undef
            }
            // onTryAddVolatile(status, target, source, effect)
            (SideCond::Safeguard, Ev::TryAddVolatile, Pre::On) => {
                let (Some(target), Some(source)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                if e.effect == Eff::None {
                    return Res::Undef;
                }
                if let Eff::Move(mi) = e.effect {
                    if self.am[mi as usize].infiltrates && !self.is_ally(target, source) {
                        return Res::Undef;
                    }
                }
                let blocked = e.vol.is_some_and(|v| v == VolKind::Confusion || v.id() == "yawn");
                if blocked && target != source {
                    return Res::Null;
                }
                Res::Undef
            }

            // ---- Entry hazards: onSwitchIn(pokemon), run for each Pokémon that comes in.
            (SideCond::Spikes, Ev::SwitchIn, Pre::On) => {
                let Some(pokemon) = e.target else {
                    return Res::Undef;
                };
                if !self.is_grounded(pokemon, false) {
                    return Res::Undef;
                }
                let layers = self.sides[side].conds.get(kind).map_or(1, |c| c.data) as u32;
                let d = div1([0u32, 3, 4, 6][layers.min(3) as usize] * self.mon(pokemon).max_hp() as u32, 24);
                self.damage(d, None, None, Eff::None);
                Res::Undef
            }
            (SideCond::Toxicspikes, Ev::SwitchIn, Pre::On) => {
                let Some(pokemon) = e.target else {
                    return Res::Undef;
                };
                if !self.is_grounded(pokemon, false) {
                    return Res::Undef;
                }
                if self.has_type(pokemon, Type::Poison) {
                    // A grounded Poison type soaks them up.
                    self.remove_side_condition(pokemon.side as usize, SideCond::Toxicspikes);
                } else if self.has_type(pokemon, Type::Steel) {
                } else {
                    let layers = self.sides[side].conds.get(kind).map_or(1, |c| c.data);
                    let status = if layers >= 2 { Status::Tox } else { Status::Psn };
                    // Showdown names the foe's first active slot as the source.
                    let source = self.active(1 - pokemon.side as usize, 0);
                    self.try_set_status(pokemon, status, Some(source), Eff::None);
                }
                Res::Undef
            }
            (SideCond::Stealthrock, Ev::SwitchIn, Pre::On) => {
                let Some(pokemon) = e.target else {
                    return Res::Undef;
                };
                // The damage scales with how a Rock move would do against the Pokémon.
                let saved = self.am_len;
                let rock = self.new_am(mv::STEALTHROCK);
                let type_mod = self.run_effectiveness(pokemon, rock).clamp(-6, 6);
                self.am_len = saved;
                let hp = self.mon(pokemon).max_hp() as u32;
                let d = if type_mod >= 0 { div1(hp << type_mod, 8) } else { div1(hp, 8 << -type_mod) };
                self.damage(d, None, None, Eff::None);
                Res::Undef
            }
            (SideCond::Stickyweb, Ev::SwitchIn, Pre::On) => {
                let Some(pokemon) = e.target else {
                    return Res::Undef;
                };
                if !self.is_grounded(pokemon, false) {
                    return Res::Undef;
                }
                // The drop counts as coming from the move Sticky Web, used by the
                // Pokémon in the foe's first slot.
                let source = self.active(1 - pokemon.side as usize, 0);
                let saved = self.am_len;
                let web = self.new_am(mv::STICKYWEB);
                self.boost1(SPE, -1, Some(pokemon), Some(source), Eff::Move(web));
                self.am_len = saved;
                Res::Undef
            }

            _ => unreachable!("no body for {kind:?} {ev:?} {pre:?}"),
        }
    }

    /// Whether an effect is the condition with this id (looked up by name, as Showdown does,
    /// so that an id Champions lacks simply never matches).
    pub(crate) fn eff_is_named(&self, eff: Eff, id: &str) -> bool {
        match eff {
            Eff::Vol(v) => v.id() == id,
            Eff::Move(mi) => self.am[mi as usize].d().id == id,
            _ => false,
        }
    }

    /// Callbacks of slot conditions. `holder` is the Pokémon in the position.
    pub(crate) fn slot_cb(&mut self, kind: SlotCond, ev: Ev, holder: MonRef) -> Res {
        let e = self.event;
        let side = holder.side as usize;
        let pos = self.mon(holder).position as usize;
        match (kind, ev) {
            // ---- Wish. `data` is the HP it will restore, `st.a` the turn counter when it was made.
            // onStart(pokemon, source)
            (SlotCond::Wish, Ev::Start) => {
                let hp = e.source.map_or(0, |s| self.mon(s).max_hp() / 2);
                let turn = self.overflowed_turn_count();
                if let Some(c) = self.sides[side].slot_conds[pos].get_mut(kind) {
                    c.data = hp;
                    c.st.a = turn;
                }
                Res::Undef
            }
            // onResidual(target): comes true at the end of the next turn.
            (SlotCond::Wish, Ev::Residual) => {
                let Some(c) = self.sides[side].slot_conds[pos].get(kind) else {
                    return Res::Undef;
                };
                if self.overflowed_turn_count() <= c.st.a {
                    return Res::Undef;
                }
                // The position the wisher stood in, on this side.
                let made_at = if c.source_slot == NO_SLOT { pos } else { (c.source_slot & 1) as usize };
                self.remove_slot_condition(side, made_at, kind);
                Res::Undef
            }
            // onEnd(target): whoever stands there now is healed.
            (SlotCond::Wish, Ev::End) => {
                let Some(target) = e.target else {
                    return Res::Undef;
                };
                if !self.mon(target).fainted {
                    let hp = self.sides[side].slot_conds[pos].get(kind).map_or(0, |c| c.data);
                    self.heal(hp as i32, Some(target), Some(target), Eff::None);
                }
                Res::Undef
            }
            // ---- Healing Wish: waits on the slot for someone it can help.
            // onSwitchIn(target)
            (SlotCond::Healingwish, Ev::SwitchIn) => {
                self.single_event(
                    Ev::Swap,
                    Eff::SlotCond(kind),
                    Some(holder),
                    Some(holder),
                    None,
                    Eff::None,
                    Res::Undef,
                );
                Res::Undef
            }
            // onSwap(target): also when a Pokémon is moved into the slot by Ally Switch.
            (SlotCond::Healingwish, Ev::Swap) => {
                let target = e.target.unwrap_or(holder);
                let m = self.mon(target);
                if !m.fainted && (m.hp < m.max_hp() || m.status != Status::None) {
                    let max = m.max_hp() as i32;
                    self.heal_mon(target, max);
                    self.cure_status(target);
                    let (side, pos) = (target.side as usize, self.mon(target).position as usize);
                    self.remove_slot_condition(side, pos, kind);
                }
                Res::Undef
            }

            // ---- A future move (Future Sight). `st.a` is the turn counter value it lands at,
            // `source` the Pokémon that used it, wherever that is by then.
            // onStart(target)
            (SlotCond::Futuremove, Ev::Start) => {
                let ending = self.turn as i16 - 1 + 2;
                if let Some(c) = self.sides[side].slot_conds[pos].get_mut(kind) {
                    c.st.a = ending;
                }
                Res::Undef
            }
            // onResidual(target): the counter is one byte, so a hit due at 256 or later never comes.
            (SlotCond::Futuremove, Ev::Residual) => {
                let Some(c) = self.sides[side].slot_conds[pos].get(kind) else {
                    return Res::Undef;
                };
                if self.overflowed_turn_count() < c.st.a {
                    return Res::Undef;
                }
                self.remove_slot_condition(side, pos, kind);
                Res::Undef
            }
            // onEnd(target): time is up; whoever stands in the slot now is hit.
            (SlotCond::Futuremove, Ev::End) => {
                let Some(target) = e.target else {
                    return Res::Undef;
                };
                let Some(source) = self.sides[side].slot_conds[pos].get(kind).and_then(|c| c.source) else {
                    return Res::Undef;
                };
                if self.mon(target).fainted || target == source {
                    return Res::Undef;
                }
                // The hit is built from a few plain fields of the move: no callbacks,
                // and none of the original's indifference to type immunity.
                let hit = self.new_am(mv::FUTURESIGHT);
                {
                    let infiltrates = self.has_ability(source, ab::INFILTRATOR);
                    let am = &mut self.am[hit as usize];
                    am.bare = true;
                    am.flags = F_METRONOME | F_FUTUREMOVE;
                    am.ignore_immunity = IgnoreImm::No;
                    am.infiltrates = infiltrates;
                }
                self.set_active_move(Some(hit), Some(source), Some(target));
                self.try_spread_move_hit(&[target], source, hit);
                if self.mon(source).is_active
                    && !self.mon(source).force_switch_flag
                    && self.has_item(source, it::LIFEORB)
                {
                    // Asked directly, as "the move" itself, but its callback still
                    // skips recoil when Red Card has already scheduled a drag-out.
                    let d = div1(self.mon(source).max_hp() as u32, 10);
                    self.damage(d, Some(source), Some(source), Eff::Item(it::LIFEORB));
                }
                self.active_move = None;
                self.check_win(None);
                Res::Undef
            }
            _ => unreachable!("no body for {kind:?} {ev:?}"),
        }
    }

    /// `Battle#getOverflowedTurnCount`: the turn counter as the games keep it, in one byte.
    pub(crate) fn overflowed_turn_count(&self) -> i16 {
        ((self.turn as i32 - 1) & 0xFF) as i16
    }

    /// Callbacks of pseudo-weathers.
    pub(crate) fn pseudo_cb(&mut self, kind: Pseudo, ev: Ev) -> Res {
        let e = self.event;
        match (kind, ev) {
            // ---- Trick Room, Magic Room, Wonder Room: using the move again ends the room.
            // onFieldRestart(target, source)
            (Pseudo::Trickroom | Pseudo::Magicroom | Pseudo::Wonderroom, Ev::FieldRestart) => {
                self.remove_pseudo_weather(kind);
                Res::Undef
            }
            // onFieldStart / onFieldEnd only write to the log.
            (Pseudo::Trickroom | Pseudo::Wonderroom | Pseudo::Fairylock, Ev::FieldStart) => Res::Undef,
            (_, Ev::FieldEnd) => Res::Undef,

            // ---- Magic Room: onFieldStart(target, source). Every held item stops working.
            (Pseudo::Magicroom, Ev::FieldStart) => {
                let (actives, n) = self.all_active(false);
                for &mon in &actives[..n] {
                    let item = self.mon(mon).item;
                    self.single_event(Ev::End, Eff::Item(item), Some(mon), Some(mon), None, Eff::None, Res::Undef);
                }
                Res::Undef
            }

            // ---- Wonder Room: onModifyMove(move, source, target). Body Press follows the swap.
            (Pseudo::Wonderroom, Ev::ModifyMove) => {
                if let Eff::Move(mi) = e.effect {
                    let am = &mut self.am[mi as usize];
                    am.off_stat = match am.off_stat {
                        2 => 4,
                        4 => 2,
                        s => s,
                    };
                }
                Res::Undef
            }

            // ---- Fairy Lock: onTrapPokemon(pokemon)
            (Pseudo::Fairylock, Ev::TrapPokemon) => {
                if let Some(pokemon) = e.target {
                    self.try_trap(pokemon, false);
                }
                Res::Undef
            }

            // ---- Gravity
            // onFieldStart(target, source): everyone comes down.
            (Pseudo::Gravity, Ev::FieldStart) => {
                let (actives, n) = self.all_active(false);
                for &pokemon in &actives[..n] {
                    let mut flying = false;
                    for id in ["bounce", "fly"] {
                        if let Some(k) = VolKind::named(id) {
                            flying |= self.remove_volatile(pokemon, k);
                        }
                    }
                    if flying {
                        self.cancel_move(pokemon);
                        if let Some(k) = VolKind::named("twoturnmove") {
                            self.remove_volatile(pokemon, k);
                        }
                    }
                    // (Sky Drop is not in Champions.)
                    for id in ["magnetrise", "telekinesis"] {
                        if let Some(k) = VolKind::named(id) {
                            self.drop_vol(pokemon, k);
                        }
                    }
                }
                Res::Undef
            }
            // onModifyAccuracy(accuracy)
            (Pseudo::Gravity, Ev::ModifyAccuracy) => {
                if !matches!(e.relay, Res::Num(_)) {
                    return Res::Undef;
                }
                self.chain_modify(6840, 4096)
            }
            // onDisableMove(pokemon)
            (Pseudo::Gravity, Ev::DisableMove) => {
                if let Some(pokemon) = e.target {
                    self.disable_moves_where(pokemon, |d| d.flags & F_GRAVITY != 0);
                }
                Res::Undef
            }
            // onBeforeMove(pokemon, target, move) and onModifyMove(move, pokemon, target)
            (Pseudo::Gravity, Ev::BeforeMove | Ev::ModifyMove) => {
                if self.event_move_flags() & F_GRAVITY != 0 {
                    // `cant|pokemon|move: Gravity|Move`
                    self.show_attempted(e.target);
                    return FALSE;
                }
                Res::Undef
            }
            _ => unreachable!("no body for {kind:?} {ev:?}"),
        }
    }

    /// Callbacks of weather conditions.
    pub(crate) fn weather_cb(&mut self, kind: Weather, ev: Ev) -> Res {
        let e = self.event;
        let mtype = match e.effect {
            Eff::Move(mi) => Some(self.am[mi as usize].typ),
            _ => None,
        };
        match (kind, ev) {
            // onFieldStart(field, source, effect) and onFieldEnd() only write to the log.
            (_, Ev::FieldStart | Ev::FieldEnd) => Res::Undef,

            // onFieldResidual(): the weather's turn; everyone gets the Weather event.
            (Weather::Raindance | Weather::Sunnyday, Ev::FieldResidual) => {
                self.each_event(Ev::Weather);
                Res::Undef
            }
            (Weather::Sandstorm | Weather::Snowscape, Ev::FieldResidual) => {
                if self.is_weather(kind) {
                    self.each_event(Ev::Weather);
                }
                Res::Undef
            }

            // onWeatherModifyDamage(damage, attacker, defender, move)
            (Weather::Raindance, Ev::WeatherModifyDamage) => {
                let Some(defender) = e.source else {
                    return Res::Undef;
                };
                if self.effective_weather(defender) != Weather::Raindance {
                    return Res::Undef;
                }
                match mtype {
                    Some(Type::Water) => self.chain_modify(3, 2),
                    Some(Type::Fire) => self.chain_modify(1, 2),
                    _ => Res::Undef,
                }
            }
            (Weather::Sunnyday, Ev::WeatherModifyDamage) => self.sun_modify_damage(),

            // onImmunity(type, pokemon): nothing freezes in the sun.
            (Weather::Sunnyday, Ev::Immunity) => {
                let Some(pokemon) = e.target else {
                    return Res::Undef;
                };
                if self.effective_weather(pokemon) != Weather::Sunnyday {
                    return Res::Undef;
                }
                if e.imm == Some(Imm::Status(Status::Frz)) {
                    return FALSE;
                }
                Res::Undef
            }

            // onModifySpD(spd, pokemon): Rock types in a sandstorm.
            (Weather::Sandstorm, Ev::ModifySpD) => {
                let Some(pokemon) = e.target else {
                    return Res::Undef;
                };
                if self.has_type(pokemon, Type::Rock) && self.effective_weather(pokemon) == Weather::Sandstorm {
                    return Res::Num(crate::battle::modify(e.relay.num() as u32, 6144) as i32);
                }
                Res::Undef
            }
            // onModifyDef(def, pokemon): Ice types in snow.
            (Weather::Snowscape, Ev::ModifyDef) => {
                let Some(pokemon) = e.target else {
                    return Res::Undef;
                };
                if self.has_type(pokemon, Type::Ice) && self.effective_weather(pokemon) == Weather::Snowscape {
                    return Res::Num(crate::battle::modify(e.relay.num() as u32, 6144) as i32);
                }
                Res::Undef
            }

            // onWeather(target): sandstorm damage.
            (Weather::Sandstorm, Ev::Weather) => {
                if let Some(target) = e.target {
                    let d = div1(self.mon(target).max_hp() as u32, 16);
                    self.damage(d, None, None, Eff::None);
                }
                Res::Undef
            }
            _ => unreachable!("no body for {kind:?} {ev:?}"),
        }
    }

    /// Sunny Day's `onWeatherModifyDamage(damage, attacker, defender, move)`,
    /// which Mega Sol also calls as its own.
    pub(crate) fn sun_modify_damage(&mut self) -> Res {
        let e = self.event;
        let (Some(attacker), Some(defender), Eff::Move(mi)) = (e.target, e.source, e.effect) else {
            return Res::Undef;
        };
        if self.am[mi as usize].d().id == "hydrosteam" && self.effective_weather(attacker) == Weather::Sunnyday {
            return self.chain_modify(3, 2);
        }
        if self.effective_weather(defender) != Weather::Sunnyday {
            return Res::Undef;
        }
        match self.am[mi as usize].typ {
            Type::Fire => self.chain_modify(3, 2),
            Type::Water => self.chain_modify(1, 2),
            _ => Res::Undef,
        }
    }

    /// Callbacks of terrains.
    pub(crate) fn terrain_cb(&mut self, kind: Terrain, ev: Ev) -> Res {
        let e = self.event;
        let mtype = match e.effect {
            Eff::Move(mi) => Some(self.am[mi as usize].typ),
            _ => None,
        };
        // "On the ground and not in the middle of Fly or Dig."
        let grounded = |b: &mut Battle, r: Option<MonRef>| {
            r.is_some_and(|r| b.is_grounded(r, false) && !b.is_semi_invulnerable(r))
        };
        match (kind, ev) {
            // onFieldStart(field, source, effect) and onFieldEnd() only write to the log.
            (_, Ev::FieldStart | Ev::FieldEnd) => Res::Undef,

            // ---- Electric Terrain
            // onSetStatus(status, target, source, effect)
            (Terrain::Electricterrain, Ev::SetStatus) => {
                if e.status == Status::Slp && grounded(self, e.target) {
                    return FALSE;
                }
                Res::Undef
            }
            // onTryAddVolatile(status, target)
            (Terrain::Electricterrain, Ev::TryAddVolatile) => {
                if !grounded(self, e.target) {
                    return Res::Undef;
                }
                if e.vol.is_some_and(|v| v.id() == "yawn") {
                    return Res::Null;
                }
                Res::Undef
            }
            // onBasePower(basePower, attacker, defender, move)
            (Terrain::Electricterrain, Ev::BasePower) => {
                if mtype == Some(Type::Electric) && grounded(self, e.target) {
                    return self.chain_modify(5325, 4096);
                }
                Res::Undef
            }

            // ---- Grassy Terrain
            // onBasePower(basePower, attacker, defender, move)
            (Terrain::Grassyterrain, Ev::BasePower) => {
                let weakened = matches!(e.effect, Eff::Move(mi)
                    if matches!(self.am[mi as usize].d().id, "earthquake" | "bulldoze" | "magnitude"));
                if weakened && grounded(self, e.source) {
                    return self.chain_modify(1, 2);
                }
                // (The attacker only has to be on the ground, not out of Fly or Dig.)
                if mtype == Some(Type::Grass) && e.target.is_some_and(|a| self.is_grounded(a, false)) {
                    return self.chain_modify(5325, 4096);
                }
                Res::Undef
            }
            // onResidual(pokemon): run once for each active Pokémon.
            (Terrain::Grassyterrain, Ev::Residual) => {
                if let Some(pokemon) = e.target {
                    if grounded(self, Some(pokemon)) {
                        let amount = div1(self.mon(pokemon).max_hp() as u32, 16);
                        self.heal(amount, Some(pokemon), Some(pokemon), Eff::None);
                    }
                }
                Res::Undef
            }

            // ---- Misty Terrain
            // onSetStatus(status, target, source, effect)
            (Terrain::Mistyterrain, Ev::SetStatus) => {
                if !grounded(self, e.target) {
                    return Res::Undef;
                }
                FALSE
            }
            // onTryAddVolatile(status, target, source, effect)
            (Terrain::Mistyterrain, Ev::TryAddVolatile) => {
                if !grounded(self, e.target) {
                    return Res::Undef;
                }
                if e.vol == Some(VolKind::Confusion) {
                    return Res::Null;
                }
                Res::Undef
            }
            // onBasePower(basePower, attacker, defender, move)
            (Terrain::Mistyterrain, Ev::BasePower) => {
                if mtype == Some(Type::Dragon) && grounded(self, e.source) {
                    return self.chain_modify(1, 2);
                }
                Res::Undef
            }

            // ---- Psychic Terrain
            // onTryHit(target, source, effect): priority moves fail against grounded foes.
            (Terrain::Psychicterrain, Ev::TryHit) => {
                let (Some(target), Some(source), Eff::Move(mi)) = (e.target, e.source, e.effect) else {
                    return Res::Undef;
                };
                let am = &self.am[mi as usize];
                // (Showdown compares against 0.1 because Quick Claw-style boosts are fractions.)
                if am.priority <= 0 || am.target == Target::User {
                    return Res::Undef;
                }
                if self.is_semi_invulnerable(target) || self.is_ally(target, source) {
                    return Res::Undef;
                }
                if !self.is_grounded(target, false) {
                    return Res::Undef;
                }
                Res::Null
            }
            // onBasePower(basePower, attacker, defender, move)
            (Terrain::Psychicterrain, Ev::BasePower) => {
                if mtype == Some(Type::Psychic) && grounded(self, e.target) {
                    return self.chain_modify(5325, 4096);
                }
                Res::Undef
            }
            _ => unreachable!("no body for {kind:?} {ev:?}"),
        }
    }

    /// `BattleActions#getConfusionDamage`.
    fn confusion_damage(&mut self, r: MonRef, base_power: u32) -> i32 {
        let (atk_stage, def_stage) = (self.mon(r).boosts[ATK] as i32, self.mon(r).boosts[DEF] as i32);
        let attack = self.calculate_stat(r, 1, atk_stage, r);
        let defense = self.calculate_stat(r, 2, def_stage, r);
        let level = self.mon(r).level as u32;
        let base = (2 * level / 5 + 2).wrapping_mul(base_power).wrapping_mul(attack) / defense.max(1) / 50 + 2;
        let dmg = base & 0xFFFF;
        let roll = self.rand(16, "confusion damage roll");
        (dmg.wrapping_mul(100 - roll) / 100).max(1) as i32
    }
}

/// Attract only works between a male and a female.
pub(crate) fn opposite_genders(a: Gender, b: Gender) -> bool {
    matches!((a, b), (Gender::M, Gender::F) | (Gender::F, Gender::M))
}
