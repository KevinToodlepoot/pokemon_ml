#!/usr/bin/env python3
"""Mutation test: does the comparison with Showdown have teeth?

Injects one small bug at a time into a copy of the engine (a wrong multiplier,
an inverted condition, a dropped line), rebuilds, replays recorded Showdown
battles and reports whether the replay noticed. A bug that goes unnoticed is
either an equivalent change or a gap in what the recorded battles exercise.

    scripts/mutation_test.py CASES.jsonl [MORE.jsonl ...] [--only TEXT]... [--range FROM:TO]
                             [--handlers [REF] | --handlers-only [REF]] [--work NAME]
    scripts/mutation_test.py --check

--handlers adds one mutation for every callback body of a move, ability, item
or condition ("the callback does nothing"); with a git ref, only for callbacks
that ref does not have. --work names a separate build directory under target/,
so that several slices can run at once.

Record a few thousand battles first (oracle/gen_cases.js); small corpora miss
the rarer effects. Corpora are tried in the order given, so put batches built
around the effects being mutated first (`gen_cases.js --moves ...`); --range
runs a slice of the list, for giving different slices different corpora. Each mutation is a (name, file, old text, new text) entry;
when the source changes under an entry it is reported as BAD PATTERN and
should be updated rather than deleted.
"""
import os
import re
import shutil
import subprocess
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
WORK = os.path.join(REPO, 'target', 'mutation')
A, I, B, M, C, E = 'src/abilities.rs', 'src/items.rs', 'src/battle.rs', 'src/moves.rs', 'src/conditions.rs', 'src/events.rs'
V = 'src/movecbs.rs'
CH = 'src/choice.rs'
MUT = [
 # --- everything after the volatile conditions: scripted moves, switching, formes
 ('healing wish still heals a switch-in after ally switch', E, '                if slot.is_some_and(|c| c.targeted) {\n                    continue;\n                }\n', ''),
 ('ally switch leaves no mark on healing wish', E, '            if let Some(c) = self.sides[side].slot_conds.get_mut(pos).and_then(|cs| cs.get_mut(kind)) {\n                c.targeted = true;\n            }\n', ''),
 ('reckless ignores crash moves', A, 'self.am[m as usize].d().recoil.0 > 0 || self.am[m as usize].d().has_crash_damage)', 'self.am[m as usize].d().recoil.0 > 0)'),
 ('sheer force passes over electro shot', A, 'self.am[m as usize].has_sheer_force || self.am[m as usize].d().sheer_force_boost', 'self.am[m as usize].has_sheer_force'),
 ('cud chew keeps a stolen berry', A, 'if ITEMS[e.item as usize].flags & IF_BERRY != 0 && !stolen {', 'if ITEMS[e.item as usize].flags & IF_BERRY != 0 {'),
 ('magician steals after fling', A, '                    || am.id == mv::FLING\n', ''),
 ('burn up thaws anyone', C, 'if self.event_move_flags() & F_DEFROST != 0 && !burn_up_fizzles {', 'if self.event_move_flags() & F_DEFROST != 0 {'),
 ('attack history never ages', B, '                    if self.mon(MonRef { side: 1 - r.side, idx: a.idx }).is_active {', '                    if true {'),
 ('attack history stays this turn', B, '                        a.this_turn = false;\n', ''),
 ('metal burst 1.5 -> 2', V, 'Res::Num(a.damage as i32 * 3 / 2)', 'Res::Num(a.damage as i32 * 2)'),
 ('metal burst loses its half point', V, 'self.am[mi as usize].half_damage = a.damage % 2 == 1;', 'self.am[mi as usize].half_damage = false;'),
 ('metal burst picks a random target', M, '            if let Res::Mon(t) = self.single_event(Ev::ModifyTarget, me, None, Some(pokemon), target, me, Res::Undef) {\n                target = Some(t);\n                pick = false;\n            }', '            self.single_event(Ev::ModifyTarget, me, None, Some(pokemon), target, me, Res::Undef);'),
 ('substitute starts with whole hp doubled wrongly', C, '                    v.data = 2 * hp;', '                    v.data = hp;'),
 ('curse aims like any move', B, 'a.self_target = move_id == mv::CURSE && !self.has_type(user, Type::Ghost);', 'a.self_target = false;'),
 ('curse costs a quarter', V, 'let half = div1(self.mon(source).max_hp() as u32, 2);\n                self.direct_damage(half, source, Some(source), Eff::Move(mi));', 'let half = div1(self.mon(source).max_hp() as u32, 4);\n                self.direct_damage(half, source, Some(source), Eff::Move(mi));'),
 ('curse request targets like a ghost', CH, 'mv::CURSE if !self.has_type(r, Type::Ghost) => Target::User,', 'mv::CURSE if false => Target::User,'),
 ('pollen puff heal block still targets allies', CH, 'mv::POLLENPUFF if self.vols(r).has(VolKind::Healblock) => Target::AdjacentFoe,', 'mv::POLLENPUFF if false => Target::AdjacentFoe,'),
 ('beat up counts the fainted', V, 'if s.order[p] == pokemon.idx || (!ally.fainted && ally.status == Status::None) {', 'if s.order[p] == pokemon.idx || ally.status == Status::None {'),
 ('beat up uses the current species', V, 'powers[n] = 5 + SPECIES[ally.set_species as usize].base[ATK + 1] / 10;', 'powers[n] = 5 + SPECIES[ally.species as usize].base[ATK + 1] / 10;'),
 ('shell side arm never rolls the tie', V, 'if physical > special || (physical == special && self.chance(1, 2, "shell side arm")) {', 'if physical > special {'),
 ('dragon darts counts as a spread move', M, '        if n > 1 && !self.am[m].smart_target {', '        if n > 1 {'),
 ('dragon darts hits one target twice', M, '        let smart = self.am[m].smart_target && n > 1;', '        let smart = false;'),
 ('dragon darts keeps splitting after a failure', M, '        if *any_failure {\n            self.am[mi as usize].smart_target = false;\n        }', ''),
 ('protect does not stop the split', C, '                self.am[mi as usize].smart_target = false;\n                self.protect_unlocks(source);\n                Res::NotFail', '                self.protect_unlocks(source);\n                Res::NotFail'),
 ('dragon darts second target can be the user', M, 'Some(t2) if t2 != user && self.mon(t2).hp > 0 => {', 'Some(t2) if self.mon(t2).hp > 0 => {'),
 ('prioritised action keeps its order', B, '        a.source = source;\n        a.order = 3;', '        a.source = source;'),
 ('quash order 201 -> 199', V, 'self.queue.items[at].order = 201;', 'self.queue.items[at].order = 199;'),
 ('round is not doubled', V, 'Res::Num(am.base_power as i32 * if called { 2 } else { 1 })', 'Res::Num(am.base_power as i32)'),
 ('round forgets who called', M, '        if let ActSource::Round { ignore_ability } = a.source {', '        if let (ActSource::Round { ignore_ability }, false) = (a.source, true) {'),
 ('instruct ignores empty pp', V, '                    || out_of_pp\n', ''),
 ('instruct repeats recharge moves', V, 'if flags & (F_FAILINSTRUCT | F_CHARGE | F_RECHARGE) != 0', 'if flags & (F_FAILINSTRUCT | F_CHARGE) != 0'),
 ('copycat copies anything', V, '                if MOVES[last as usize].flags & F_FAILCOPYCAT != 0 {\n                    return FALSE;\n                }\n', ''),
 ('battle last move never set', B, '            self.last_move = self.am[mi as usize].id;', ''),
 ('called move gets no random target', M, '        if pick && target.is_none() {\n            target = self.get_random_target(pokemon, base_target);\n        }', ''),
 ('called move escapes pressure', M, 'if (source_effect == Eff::None || caller.is_some()) && self.listens(Ev::DeductPP) {', 'if source_effect == Eff::None && self.listens(Ev::DeductPP) {'),
 ('pressure charges the called move', M, 'let id = caller.unwrap_or(self.am[m].id);', 'let id = self.am[m].id;'),
 ('sleep talk can pick charging moves', V, 'if MOVES[slot.id as usize].flags & (F_NOSLEEPTALK | F_CHARGE) == 0 {', 'if MOVES[slot.id as usize].flags & F_NOSLEEPTALK == 0 {'),
 ('called multi-hit stops when asleep', M, 'if !self.am[m].d().sleep_usable && !called_asleep {', 'if !self.am[m].d().sleep_usable {'),
 ('protean changes type for sleep talk', A, '                    || am.d().calls_move\n', ''),
 ('future sight lands a turn early', C, 'let ending = self.turn as i16 - 1 + 2;', 'let ending = self.turn as i16 - 1 + 1;'),
 ('future sight ignores type immunity', C, '                    am.ignore_immunity = IgnoreImm::No;\n', ''),
 ('future sight hit runs the move callbacks', C, '                    am.bare = true;\n', ''),
 ('future sight life orb chip missing', C, '                    && self.has_item(source, it::LIFEORB)\n                {', '                    && false\n                {'),
 ('life orb chips on the turn future sight is used', M, '        if !self.suppressing_secondaries() && self.am[m].flags & F_FUTUREMOVE == 0 {', '        if !self.suppressing_secondaries() {'),
 ('future sight cannot aim at a fainted ally', M, 'if self.mon(t).fainted && self.am[mi as usize].flags & F_FUTUREMOVE == 0 {', 'if self.mon(t).fainted {'),
 ('future sight aimed at itself fails', B, '            return future.then_some(user);', '            return None;'),
 ('fling works under klutz', V, '                    || self.has_ability(source, ab::KLUTZ)\n                {\n                    return FALSE;', '                {\n                    return FALSE;'),
 ('fling keeps the item', C, '                self.set_item(holder, it::NONE, None, Eff::None);\n                // `-enditem|pokemon|Item|[from] move: Fling`', '                // `-enditem|pokemon|Item|[from] move: Fling`'),
 ('fling power fixed', V, 'self.am[mi as usize].base_power = fling.power as u16;', 'self.am[mi as usize].base_power = 30;'),
 ('flung berry is not eaten', V, '                    if self.has_cb(Eff::Item(item), Ev::Eat) {\n                        self.mon_mut(foe).ate_berry = true;\n                    }\n                } else if item == it::MENTALHERB {', '                } else if item == it::MENTALHERB {'),
 ('ally switch leaves volatiles behind', B, '        self.vols[side].swap(old_pos, new_pos);\n', ''),
 ('ally switch always works', C, 'if !self.chance(1, counter, "consecutive ally switch") {', 'if !self.chance(1, 1, "consecutive ally switch") {'),
 ('self-switch flag forgets the move', M, '            m.switch_flag = true;\n            m.switch_move = id;', '            m.switch_flag = true;'),
 ('did-anything ignores the damage', M, '        let mut any = damage[0];\n        for d in &damage[1..n] {\n            any = any.combine(*d);\n        }', '        let mut any = Res::Undef;'),
 ('u-turn redoes the switch-out event', B, '        if self.mon(r).skip_before_switch_out {\n            return;\n        }\n', ''),
 ('before-switch-out event not marked done', B, '        m.skip_before_switch_out = !(m.hp > 0 && !m.switch_flag);', '        m.skip_before_switch_out = false;'),
 ('parting shot always switches', V, '                if !success && !self.has_ability(target, ab::MIRRORARMOR) {\n                    self.am[mi as usize].self_switch = SelfSwitch::No;\n                }', ''),
 ('roar works with no one to drag in', M, '                if eff.primary && self.am[mi as usize].d().force_switch {\n                    did = did.combine(Res::Bool(self.can_switch(t.side as usize)));\n                }', ''),
 ('dragged pokemon enters with the others', B, "        if is_drag {\n            // So that Mold Breaker's move can still be the active one when the hazards strike.\n            self.run_switch(incoming);\n            return true;\n        }", ''),
 ('drag skips the second DragOut check', B, '        if !self.run_event(Ev::DragOut, Some(old), None, Eff::None, Res::Undef).truthy() {\n            return false;\n        }\n        self.switch_in(incoming, pos, false, SelfSwitch::No, true)', '        self.switch_in(incoming, pos, false, SelfSwitch::No, true)'),
 ('eject button ignores other exits', I, '                if actives[..n].iter().any(|&p| self.switch_flag_is_true(p)) {\n                    return Res::Undef;\n                }', ''),
 ('eject button counts a u-turn as an exit', B, '        m.switch_flag && m.switch_move == NO_MOVE\n', '        m.switch_flag\n'),
 ('red card drags without asking', I, '                    && self.run_event(Ev::DragOut, Some(source), Some(target), Eff::Move(mi), Res::Undef).truthy()\n', ''),
 ('life orb chips a red-carded user', I, '                    && !self.mon(source).force_switch_flag\n', ''),
 ('shell bell heals a red-carded user', I, 'if total > 0 && !self.mon(pokemon).force_switch_flag {', 'if total > 0 {'),
 ('emergency exit threshold ignores the hp before', A, 'if hp == 0 || 2 * hp > max || 2 * original <= max {', 'if hp == 0 || 2 * hp > max {'),
 ('emergency exit not asked after recoil', M, '        self.run_event(Ev::EmergencyExit, Some(user), Some(user), Eff::None, Res::Num(hp_before as i32));', ''),
 ('emergency exit not asked after the hit loop', M, '                        self.run_event(Ev::EmergencyExit, Some(t), Some(user), Eff::None, Res::Num(before));', ''),
 ('emergency exit not asked after rocky helmet', M, '                self.run_event(Ev::EmergencyExit, Some(user), None, Eff::None, Res::Num(user_hp_before as i32));', ''),
 ('emergency exit not asked at the end of the turn', B, '                self.run_event(Ev::EmergencyExit, Some(r), None, Eff::None, Res::Num(hp as i32));', ''),
 ('emergency exit not asked after hazards', B, '            self.run_event(Ev::EmergencyExit, a.mon, None, Eff::None, Res::Num(original_hp as i32));', ''),
 ('baton pass copies everything', B, '!k.data().no_copy && (via == SelfSwitch::CopyVolatile || k == VolKind::Substitute)', 'via == SelfSwitch::CopyVolatile || k == VolKind::Substitute'),
 ('shed tail passes stat stages', B, 'let boosts = if via == SelfSwitch::CopyVolatile { self.mon(old).boosts } else { [0; 7] };', 'let boosts = self.mon(old).boosts;'),
 ('passed volatiles get no copy event', B, '                        self.single_event(Ev::Copy, eff, Some(incoming), Some(incoming), None, Eff::None, Res::Undef);', ''),
 ('shed tail costs a quarter', V, 'let half = (self.mon(target).max_hp() as i32 + 1) / 2;', 'let half = (self.mon(target).max_hp() as i32 + 1) / 4;'),
 ('healing wish heals the healthy too', C, 'if !m.fainted && (m.hp < m.max_hp() || m.status != Status::None) {', 'if !m.fainted {'),
 ('revival restores full hp', B, 'm.hp = (m.max_hp() / 2).max(1);', 'm.hp = m.max_hp();'),
 ('revival does not count the pokemon back in', B, '                self.sides[side].pokemon_left += 1;\n', ''),
 ('revived active pokemon never returns', B, '                    let back = self.resolve_switch(ActKind::InstaSwitch, t, t);\n                    self.queue.push(back);\n', ''),
 ('revival request lapses without a bench', B, '                switching = reviving;', '                switching = false;'),
 ('reviver may switch for real', CH, '                Choice::Switch { .. } if self.reviving(side, p) => switches = switches.saturating_sub(1),\n', ''),
 ('stance change on status moves', A, 'if am.category == Category::Status && am.id != mv::KINGSSHIELD {', 'if false {'),
 ('disguise blocks only once... never busts', A, '                    self.mon_mut(holder).ability_st.a = 1;\n                    return Res::Num(0);', '                    return Res::Num(0);'),
 ('busted disguise costs nothing', A, '                        self.damage(d, Some(holder), Some(holder), Eff::Species);', ''),
 ('busted disguise is not permanent', A, '                        self.forme_change(holder, species, true, false);\n                        let d = div1', '                        self.forme_change(holder, species, false, false);\n                        let d = div1'),
 ('zero to hero keeps the old ability state', A, '                        self.forme_change(holder, species, true, true);\n                    }\n                }\n                Res::Undef\n            }\n            // onSwitchIn(pokemon): `-activate', '                        self.forme_change(holder, species, true, false);\n                    }\n                }\n                Res::Undef\n            }\n            // onSwitchIn(pokemon): `-activate'),
 ('mega evolution ends an illusion', B, '                self.mon_mut(r).ability = ab::NOABILITY;\n', ''),
 ('illusion picks the first party member', A, 'for p in (own + 1..s.n as usize).rev() {', 'for p in own + 1..s.n as usize {'),
 ('illusion survives a hit', A, '                if !self.mon(holder).being_called_back && self.mon(holder).illusion != 0 {\n                    let was = self.shown_as(holder);\n                    self.mon_mut(holder).illusion = 0;\n                    // `replace`, then `-end|pokemon|Illusion`.\n                    self.shown_unmask(holder, was);\n                    self.show_illusion_ended(holder);\n                }', ''),
 ('illusion ends on switching out', A, '                if !self.mon(holder).being_called_back && self.mon(holder).illusion != 0 {', '                if self.mon(holder).illusion != 0 {'),
 ('imposter copies the foe in front', A, 'let across = ACTIVE - 1 - self.mon(holder).position as usize;', 'let across = self.mon(holder).position as usize;'),
 ('transform through a substitute', B, '            || self.vols(target).has(VolKind::Substitute)\n            || t.transformed', '            || t.transformed'),
 ('transform into an illusion', B, '            || self.mon(r).illusion != 0\n            || t.illusion != 0\n', '            || self.mon(r).illusion != 0\n'),
 ('transform copies full pp', B, 'let pp = MOVES[id as usize].base_pp.min(5);', 'let pp = MOVES[id as usize].pp;'),
 ('transform keeps its own stats', B, '            m.stats[1..].copy_from_slice(&t.stats[1..]);\n            m.base_moves', '            m.base_moves'),
 ('transform skips the stat stages', B, '            m.times_attacked = t.times_attacked;\n            m.boosts = t.boosts;', '            m.times_attacked = t.times_attacked;'),
 ('transform restarts a shared ability', B, '        if old != new {\n            self.single_event(Ev::Start, Eff::Ability(new), Some(r), Some(r), Some(r), Eff::None, Res::Undef);\n        }', '        self.single_event(Ev::Start, Eff::Ability(new), Some(r), Some(r), Some(r), Eff::None, Res::Undef);'),
 ('transformed moves survive switching', B, '            m.moves = m.base_moves;\n            m.unknown_pp = m.base_unknown_pp;\n            m.base_unknown_pp = 0;\n            m.n_moves = m.base_n_moves;\n', '            m.unknown_pp = m.base_unknown_pp;\n            m.base_unknown_pp = 0;\n'),
 ('forme-bound ability works when transformed', B, '        if ABILITIES[m.ability as usize].flags & AF_NOTRANSFORM != 0 && m.transformed {\n            return true;\n        }\n', ''),
 ('aura wheel is always electric', V, 'self.am[mi as usize].typ = if hangry { Type::Dark } else { Type::Electric };', 'self.am[mi as usize].typ = Type::Electric;'),
 ('vetoed move draws no target', M, '            self.get_random_target(pokemon, Target::Normal);\n            return FALSE;', '            return FALSE;'),
 # --- turn flow, volatile conditions, Substitute
 ('fake out works every turn', V, 'if e.target.is_some_and(|source| self.mon(source).active_move_actions > 1) {', 'if e.target.is_some_and(|source| self.mon(source).active_move_actions > 100) {'),
 ('fake out never disabled', V, 'if self.mon(pokemon).active_move_actions > 0 {', 'if self.mon(pokemon).active_move_actions > 100 {'),
 ('move actions not reset on switch', B, '            m.active_turns = 0;\n            m.active_move_actions = 0;\n', '            m.active_turns = 0;\n'),
 ('follow me does not redirect', C, '                if self.valid_target_loc(loc, user, self.am[mi as usize].target) {\n                    self.am[mi as usize].smart_target = false;\n                    return Res::Mon(holder);\n                }', '                if self.valid_target_loc(loc, user, self.am[mi as usize].target) {\n                    return Res::Undef;\n                }'),
 ('rage powder pulls grass types', C, 'if kind == VolKind::Ragepowder && !self.run_status_immunity(user, Imm::Powder) {', 'if false && !self.run_status_immunity(user, Imm::Powder) {'),
 ('helping hand 1.5 -> 1.3', C, 'self.chain_modify(3u32.pow(n), 2u32.pow(n))', 'self.chain_modify(13u32.pow(n), 10u32.pow(n))'),
 ('helping hand works on a pokemon that moved', V, 'if e.target.is_some_and(|t| !self.mon(t).newly_switched && !self.will_move(t)) {', 'if e.target.is_some_and(|t| !self.mon(t).newly_switched && !self.will_move(t) && false) {'),
 ('newly switched never cleared', B, '                    m.newly_switched = false;\n', ''),
 ('wide guard blocks single-target moves', C, 'matches!(am.target, Target::AllAdjacent | Target::AllAdjacentFoes)', 'matches!(am.target, Target::AllAdjacent | Target::AllAdjacentFoes | Target::Normal)'),
 ('quick guard blocks everything', C, '                    am.priority > 0\n                };', '                    am.priority >= 0\n                };'),
 ('wide guard does not add to the protect counter', V, '            (mv::WIDEGUARD | mv::QUICKGUARD, Ev::HitSide) => {\n                if let Some(source) = e.source {\n                    self.add_volatile(source, VolKind::Stall, None, Eff::None);\n                }', '            (mv::WIDEGUARD | mv::QUICKGUARD, Ev::HitSide) => {'),
 ('feint leaves protect up', M, '                    if let Some(k) = VolKind::named(name) {\n                        broke |= self.remove_volatile(t, k);\n                    }', '                    if let Some(_k) = VolKind::named(name) {}'),
 ('feint leaves the stall counter', M, '                if broke {\n                    self.drop_vol(t, VolKind::Stall);\n                }', ''),
 ('endure leaves 2 hp', C, 'return Res::Num(hp - 1);', 'return Res::Num(hp - 2);'),
 ('baneful bunker poisons non-contact', C, '                self.protect_unlocks(source);\n                if self.makes_contact(mi) {\n                    match kind {', '                self.protect_unlocks(source);\n                if true {\n                    match kind {'),
 ("king's shield blocks status moves", C, 'if self.bypasses_protect(mi, source, target, kind != VolKind::Kingsshield) {', 'if self.bypasses_protect(mi, source, target, true) {'),
 ('spiky shield 1/8 -> 1/16', C, '                            let d = div1(self.mon(source).max_hp() as u32, 8);\n                            self.damage(d, Some(source), Some(target), Eff::None);', '                            let d = div1(self.mon(source).max_hp() as u32, 16);\n                            self.damage(d, Some(source), Some(target), Eff::None);'),
 ('taunt never extended', C, '                if self.mon(holder).active_turns > 0 && !self.will_move(holder) {\n                    if let Some(v) = self.vol_mut(holder, VolKind::Taunt) {\n                        v.duration += 1;\n                    }\n                }', ''),
 ('taunt blocks attacks', C, 'if am.category == Category::Status && am.d().id != "mefirst" {', 'if am.d().id != "mefirst" {'),
 ('taunt does not disable status moves', C, 'self.disable_moves_where(holder, |d| d.category == Category::Status && d.id != "mefirst");', ''),
 ('encore works without pp', C, 'if MOVES[last as usize].flags & F_FAILENCORE != 0 || pp.is_none_or(|pp| pp == 0) {', 'if MOVES[last as usize].flags & F_FAILENCORE != 0 || pp.is_none() {'),
 ('encore ignores failencore', C, 'if MOVES[last as usize].flags & F_FAILENCORE != 0 || pp.is_none_or(|pp| pp == 0) {', 'if pp.is_none_or(|pp| pp == 0) {'),
 ('encore does not change the queued move', C, '                        if chosen != last && !self.has_item(holder, it::MENTALHERB) {\n                            self.change_action(holder, last);\n                        }', ''),
 ('encore ignores mental herb', C, 'if chosen != last && !self.has_item(holder, it::MENTALHERB) {', 'if chosen != last {'),
 ('encore never extended', C, '                    None => {\n                        if let Some(v) = self.vol_mut(holder, VolKind::Encore) {\n                            v.duration += 1;\n                        }\n                    }', '                    None => {}'),
 ('disable never shortened', C, '                if self.will_move(holder) || moving_now {\n                    if let Some(v) = self.vol_mut(holder, VolKind::Disable) {\n                        v.duration -= 1;\n                    }\n                }', ''),
 ('disable ignores moving now', C, 'if self.will_move(holder) || moving_now {', 'if self.will_move(holder) {'),
 ('disable works on a move without pp', C, '                if self.move_slot(holder, last).is_some_and(|s| s.pp == 0) {\n                    return FALSE;\n                }', ''),
 ('torment disables nothing', C, '                    self.disable_slots_where(holder, |s| s.id == last);\n', ''),
 ('last move not recorded', M, '        self.mon_mut(pokemon).last_move = a.move_id;\n', ''),
 ('last move survives switching', B, '        m.last_move = NO_MOVE;\n        m.last_attempted_move = None;\n', '        m.last_attempted_move = None;\n'),
 ('imprison does not stop the move', C, '                if id != mv::STRUGGLE && self.move_slot(source, id).is_some() {\n                    // `cant|attacker|move: Imprison|Move`', '                if false {\n                    // `cant|attacker|move: Imprison|Move`'),
 ('imprison disables openly', C, '                self.disable_slots_hidden_where(pokemon, |s| {', '                self.disable_slots_where(pokemon, |s| {'),
 ('struggle spelled plainly when moves are hidden', CH, 'let listed = self.is_last_active(r) && m.moves[..m.n_moves as usize].iter().any(|s| s.hidden && s.pp > 0);', 'let listed = false && m.moves[..m.n_moves as usize].iter().any(|s| s.hidden && s.pp > 0);'),
 ('struggle spelling ignores which slot is last', CH, 'let listed = self.is_last_active(r) && m.moves[..m.n_moves as usize].iter().any(|s| s.hidden && s.pp > 0);', 'let listed = m.moves[..m.n_moves as usize].iter().any(|s| s.hidden && s.pp > 0);'),
 ('attract ignores gender', C, '                if !opposite_genders(self.mon(holder).gender, self.mon(source).gender) {\n                    return FALSE;\n                }', ''),
 ('attract 1/2 -> 1/3', C, 'if self.chance(1, 2, "attract") {', 'if self.chance(1, 3, "attract") {'),
 ('attract outlasts its source', C, '                if source.is_some_and(|s| !self.mon(s).is_active) {\n                    self.remove_volatile(holder, VolKind::Attract);\n                }', ''),
 ('heal block lets healing through', C, '                        return Res::Null;\n                    }\n                }\n                FALSE\n            }\n            // onRestart(target, source, effect)', '                        return Res::Null;\n                    }\n                }\n                Res::Undef\n            }\n            // onRestart(target, source, effect)'),
 ('heal block from psychic noise lasts 5', C, '                if self.eff_is_named(source_effect, "psychicnoise") {\n                    2', '                if self.eff_is_named(source_effect, "psychicnoise") {\n                    5'),
 ('cursed body 30% -> 50%', A, 'self.chance(3, 10, "cursed body")', 'self.chance(5, 10, "cursed body")'),
 ('cute charm without contact', A, 'if self.makes_contact(m)\n                    && self.chance(3, 10, "cute charm")', 'if self.chance(3, 10, "cute charm")'),
 ('oblivious keeps taunt', A, '                self.remove_volatile(holder, VolKind::Attract);\n                self.remove_volatile(holder, VolKind::Taunt);', '                self.remove_volatile(holder, VolKind::Attract);'),
 ('substitute has a third of the hp', C, '                let hp = self.mon(holder).max_hp() / 4;\n                if let Some(v) = self.vol_mut(holder, VolKind::Substitute) {', '                let hp = self.mon(holder).max_hp() / 3;\n                if let Some(v) = self.vol_mut(holder, VolKind::Substitute) {'),
 ('substitute costs nothing', V, '                    self.direct_damage(cost, target, e.source, Eff::Move(mi));\n', ''),
 ('substitute at a quarter hp', V, 'm.hp as u32 * 4 <= m.max_hp() as u32 || m.max_hp() == 1', 'm.hp as u32 * 4 < m.max_hp() as u32 || m.max_hp() == 1'),
 ('substitute blocks sound moves', C, 'if target == source || am.flags & F_BYPASSSUB != 0 || am.infiltrates {', 'if target == source || am.infiltrates {'),
 ('substitute blocks infiltrator', C, 'if target == source || am.flags & F_BYPASSSUB != 0 || am.infiltrates {', 'if target == source || am.flags & F_BYPASSSUB != 0 {'),
 ('status moves pass a substitute', C, '                if !r.hit() {\n                    // No damage to deal (a status move, an immunity): the move fails.\n                    return Res::Null;\n                }', '                if !r.hit() {\n                    return Res::Undef;\n                }'),
 ('no recoil from hitting a substitute', C, '                if damage > 0 {\n                    self.apply_recoil_halves(damage as u32, mi, source);\n                }\n', ''),
 ('no drain from hitting a substitute', C, '                    self.heal(amount as i32, Some(source), Some(target), Eff::Drain);\n                }\n                let me = Eff::Move(mi);', '                }\n                let me = Eff::Move(mi);'),
 ('substitute drain rounds down', C, 'let amount = (damage as u32 * drain.0 as u32).div_ceil(2 * drain.1 as u32);', 'let amount = damage as u32 * drain.0 as u32 / (2 * drain.1 as u32);'),
 ('secondaries reach behind a substitute', M, '            if damage[i] == HIT_SUBSTITUTE {\n                damage[i] = TRUE;\n                targets[i] = Tgt::Sub;\n            }', '            if damage[i] == HIT_SUBSTITUTE {\n                damage[i] = TRUE;\n            }'),
 ('no secondary roll behind a substitute', M, '        for i in 0..n {\n            if targets[i] == Tgt::Gone {\n                continue;\n            }\n            let count = self.am[mi as usize].n_secs as usize;', '        for i in 0..n {\n            if targets[i] == Tgt::Gone || targets[i] == Tgt::Sub {\n                continue;\n            }\n            let count = self.am[mi as usize].n_secs as usize;'),
 ('intimidate goes through a substitute', A, '                    if !self.has_vol_named(f, "substitute") {\n                        self.boost1(ATK, -1, Some(f), Some(holder), Eff::None);\n                    }', '                    self.boost1(ATK, -1, Some(f), Some(holder), Eff::None);'),
 ('resist berry eaten behind a substitute', I, '                        if hit_sub {\n                            return Res::Undef;\n                        }\n', ''),
 ('air balloon survives a hit on the substitute', I, '(it::AIRBALLOON, Ev::DamagingHit | Ev::AfterSubDamage, Pre::On) => {\n                let Some(target) = e.target else {\n                    return Res::Undef;\n                };\n                if !matches!(e.effect, Eff::Move(_)) {', '(it::AIRBALLOON, Ev::DamagingHit | Ev::AfterSubDamage, Pre::On) => {\n                let Some(target) = e.target else {\n                    return Res::Undef;\n                };\n                if ev == Ev::AfterSubDamage || !matches!(e.effect, Eff::Move(_)) {'),
 ('aqua ring 1/16 -> 1/8', C, '            (VolKind::Aquaring | VolKind::Ingrain, Ev::Residual) => {\n                let amount = div1(self.mon(holder).max_hp() as u32, 16);', '            (VolKind::Aquaring | VolKind::Ingrain, Ev::Residual) => {\n                let amount = div1(self.mon(holder).max_hp() as u32, 8);'),
 ('ingrain does not trap', C, '            (VolKind::Ingrain, Ev::TrapPokemon) => {\n                self.try_trap(holder, false);', '            (VolKind::Ingrain, Ev::TrapPokemon) => {'),
 ('leech seed 1/8 -> 1/16', C, '                let d = div1(self.mon(holder).max_hp() as u32, 8);\n                let dealt = self.damage(d, Some(holder), Some(target), Eff::None);', '                let d = div1(self.mon(holder).max_hp() as u32, 16);\n                let dealt = self.damage(d, Some(holder), Some(target), Eff::None);'),
 ('leech seed does not heal', C, '                if dealt.truthy() {\n                    self.heal(dealt.num(), Some(target), Some(holder), Eff::None);\n                }', ''),
 ('leech seed works on grass', V, 'Res::Bool(e.target.is_some_and(|t| !self.has_type(t, Type::Grass)))', 'Res::Bool(e.target.is_some())'),
 ('big root ignores leech seed', I, 'Eff::Drain | Eff::Vol(VolKind::Leechseed | VolKind::Ingrain | VolKind::Aquaring)', 'Eff::Drain | Eff::Vol(VolKind::Ingrain | VolKind::Aquaring)'),
 ('liquid ooze ignores leech seed', A, 'if matches!(e.effect, Eff::Drain | Eff::Vol(VolKind::Leechseed))', 'if matches!(e.effect, Eff::Drain)'),
 ('focus energy +2 -> +1', C, '(VolKind::Focusenergy, Ev::ModifyCritRatio) => Res::Num(e.relay.num() + 2),', '(VolKind::Focusenergy, Ev::ModifyCritRatio) => Res::Num(e.relay.num() + 1),'),
 ('dragon cheer same for everyone', C, 'Res::Num(e.relay.num() + if dragon { 2 } else { 1 })', 'Res::Num(e.relay.num() + 1)'),
 ('focus energy stacks with dragon cheer', C, '                if self.vols(holder).has(VolKind::Dragoncheer) {\n                    return FALSE;\n                }', ''),
 ('minimize not punished', C, '                if self.event_move_flags() & F_MINIMIZE != 0 {\n                    return self.chain_modify(2, 1);\n                }', ''),
 ('no retreat does not trap', C, '            (VolKind::Noretreat, Ev::Start) => {\n                self.add_volatile(holder, VolKind::Trapped, None, Eff::None);', '            (VolKind::Noretreat, Ev::Start) => {'),
 ('no retreat repeats', V, '                if self.vols(source).has(VolKind::Noretreat) {\n                    return FALSE;\n                }', ''),
 ('trapped outlasts the trapper', B, '        if was_trapper {\n            self.unlink_volatile(r, VolKind::Trapper, None);\n        }', ''),
 ('ghosts can be trapped by mean look', B, '            Imm::Vol(VolKind::Trapped) => self.type_allows(r, 6),\n', ''),
 ('octolock lowers only defense', C, '                b[DEF] = -1;\n                b[SPD] = -1;\n                self.boost(b, Some(holder), source, Eff::Move(octolock));', '                b[DEF] = -1;\n                self.boost(b, Some(holder), source, Eff::Move(octolock));'),
 ('octolock outlasts its user', C, '                    if !m.is_active || m.hp == 0 || m.active_turns == 0 {\n                        self.drop_vol(holder, kind);\n                        return Res::Undef;\n                    }\n                }\n                let saved = self.am_len;', '                    if false {\n                        self.drop_vol(holder, kind);\n                        return Res::Undef;\n                    }\n                }\n                let saved = self.am_len;'),
 ('power trick does not swap back', C, '            (VolKind::Powertrick, Ev::Start | Ev::Copy | Ev::End) => {\n                self.mon_mut(holder).stats.swap(ATK + 1, DEF + 1);\n                Res::Undef\n            }', '            (VolKind::Powertrick, Ev::Start | Ev::Copy) => {\n                self.mon_mut(holder).stats.swap(ATK + 1, DEF + 1);\n                Res::Undef\n            }\n            (VolKind::Powertrick, Ev::End) => Res::Undef,'),
 ('smack down grounds everything', C, '                if !applies {\n                    return FALSE;\n                }\n                Res::Undef\n            }\n            // onRestart(pokemon)\n            (VolKind::Smackdown, Ev::Restart) => {', '                Res::Undef\n            }\n            // onRestart(pokemon)\n            (VolKind::Smackdown, Ev::Restart) => {'),
 ('salt cure same for water types', C, 'let d = div1(self.mon(holder).max_hp() as u32, if weak { 8 } else { 16 });', 'let d = div1(self.mon(holder).max_hp() as u32, 16);'),
 ('syrup bomb outlasts its user', C, '                if source.is_some_and(|s| !self.mon(s).is_active) {\n                    self.remove_volatile(holder, kind);\n                }\n                Res::Undef\n            }\n            // onResidual(pokemon)\n            (VolKind::Syrupbomb, Ev::Residual) => {', '                Res::Undef\n            }\n            // onResidual(pokemon)\n            (VolKind::Syrupbomb, Ev::Residual) => {'),
 ('throat chop does not stop sound moves', C, '            (VolKind::Throatchop, Ev::BeforeMove | Ev::ModifyMove) => {\n                if self.event_move_flags() & F_SOUND != 0 {\n                    return FALSE;\n                }', '            (VolKind::Throatchop, Ev::BeforeMove | Ev::ModifyMove) => {'),
 ('charge x2 -> x1.5', C, '                if matches!(e.effect, Eff::Move(mi) if self.am[mi as usize].typ == Type::Electric) {\n                    return self.chain_modify(2, 1);', '                if matches!(e.effect, Eff::Move(mi) if self.am[mi as usize].typ == Type::Electric) {\n                    return self.chain_modify(3, 2);'),
 ('charge is never used up', C, '                    if am.typ == Type::Electric && am.id != mv::CHARGE {\n                        self.remove_volatile(holder, VolKind::Charge);\n                    }', ''),
 ('destiny bond takes allies', C, '                if self.is_ally(holder, source) {\n                    return Res::Undef;\n                }\n                if matches!(e.effect, Eff::Move(mi) if self.am[mi as usize].flags & F_FUTUREMOVE == 0) {', '                if matches!(e.effect, Eff::Move(mi) if self.am[mi as usize].flags & F_FUTUREMOVE == 0) {'),
 ('destiny bond never wears off', C, '                self.remove_volatile(holder, VolKind::Destinybond);\n                Res::Undef\n            }\n            // onMoveAborted(pokemon, target, move)', '                Res::Undef\n            }\n            // onMoveAborted(pokemon, target, move)'),
 ('destiny bond can be repeated', V, 'Res::Bool(e.target.is_some_and(|p| !self.remove_volatile(p, VolKind::Destinybond)))', 'Res::Bool(e.target.is_some())'),
 ('electrify changes struggle', C, '                    if self.am[mi as usize].id != mv::STRUGGLE {\n                        self.am[mi as usize].typ = Type::Electric;\n                    }', '                    self.am[mi as usize].typ = Type::Electric;'),

 ('gastro acid suppresses nothing', B, '        self.vols(r).has(VolKind::Gastroacid)\n    }', '        false\n    }'),
 ('gastro acid skips the ability end', C, '            (VolKind::Gastroacid, Ev::Start) => {\n                let ability = self.mon(holder).ability;\n                self.single_event(\n                    Ev::End,', '            (VolKind::Gastroacid, Ev::Start) => {\n                let ability = self.mon(holder).ability;\n                self.single_event(\n                    Ev::Copy,'),
 ('lock-on locks everyone', C, 'if matches!(e.effect, Eff::Move(_)) && e.source == Some(holder) && e.target == locked {', 'if matches!(e.effect, Eff::Move(_)) && e.source == Some(holder) {'),
 ('perish song spares the singer', V, '                    } else if !self.vols(pokemon).has(VolKind::Perishsong) {\n                        self.add_volatile(pokemon, VolKind::Perishsong, None, Eff::None);', '                    } else if !self.vols(pokemon).has(VolKind::Perishsong) && Some(pokemon) != e.source {\n                        self.add_volatile(pokemon, VolKind::Perishsong, None, Eff::None);'),
 ('perish song ignores soundproof', V, '                        || self.run_event(Ev::TryHit, Some(pokemon), e.source, me, Res::Undef) == Res::Null;\n                    if unreachable {', '                        || false;\n                    if unreachable {'),
 ('yawn does nothing', C, '                self.try_set_status(holder, Status::Slp, source, Eff::None);\n', ''),
 ('yawn on a statused target', V, 'if self.mon(target).status != Status::None\n                    || !self.run_status_immunity(target, Imm::Status(Status::Slp))\n                {', 'if !self.run_status_immunity(target, Imm::Status(Status::Slp)) {'),
 ('insomnia lets yawn in', A, '            (ab::INSOMNIA | ab::VITALSPIRIT | ab::PURIFYINGSALT, Ev::TryAddVolatile, Pre::On) => {\n                if e.vol == Some(VolKind::Yawn) {\n                    // `-immune|target|[from] ability: Insomnia`\n                    self.show_ability(holder, ability);\n                    return Res::Null;\n                }', '            (ab::INSOMNIA | ab::VITALSPIRIT | ab::PURIFYINGSALT, Ev::TryAddVolatile, Pre::On) => {'),
 ('glaive rush x2 -> x1.5', C, '(VolKind::Glaiverush, Ev::ModifyDamage, Pre::Source) => self.chain_modify(2, 1),', '(VolKind::Glaiverush, Ev::ModifyDamage, Pre::Source) => self.chain_modify(3, 2),'),
 ('glaive rush never wears off', C, '            (VolKind::Glaiverush, Ev::BeforeMove) => {\n                self.remove_volatile(holder, kind);', '            (VolKind::Glaiverush, Ev::BeforeMove) => {'),
 ('stockpile boosts are kept', C, '                if def != 0 || spd != 0 {\n                    let mut b = [0i8; 7];\n                    b[DEF] = def as i8;', '                if false {\n                    let mut b = [0i8; 7];\n                    b[DEF] = def as i8;'),
 ('spit up 100 -> 80 per layer', V, 'Some(layers) if layers > 0 => Res::Num(layers as i32 * 100),', 'Some(layers) if layers > 0 => Res::Num(layers as i32 * 80),'),
 ('swallow always heals half', V, 'let part = [1024, 2048, 4096][layers as usize - 1];', 'let part = [2048, 2048, 2048][layers as usize - 1];'),
 ('spit up keeps the stockpile', V, '            (mv::SPITUP, Ev::AfterMove) => {\n                if let Some(pokemon) = e.target {\n                    self.remove_volatile(pokemon, VolKind::Stockpile);\n                }', '            (mv::SPITUP, Ev::AfterMove) => {'),
 ('sparkling aria cures nothing', V, '                        && self.mon(pokemon).status == Status::Brn\n                    {\n                        self.cure_status(pokemon);\n                    }', '                        && self.mon(pokemon).status == Status::Brn\n                    {\n                    }'),
 ('binding moves 1/8 -> 1/16', C, '                    v.data = if band { 6 } else { 8 };', '                    v.data = if band { 6 } else { 16 };'),
 ('binding band does nothing', C, '                    v.data = if band { 6 } else { 8 };', '                    v.data = 8;'),
 ('binding moves last 5 to 7 turns', C, 'self.rand_range(5, 7, "binding move turns") as u8', 'self.rand_range(5, 8, "binding move turns") as u8'),
 ('binding outlasts its user', C, '                    if !m.is_active || m.hp == 0 || m.active_turns == 0 {\n                        self.drop_vol(holder, kind);\n                        return Res::Undef;\n                    }\n                }\n                let d = div1(self.mon(holder).max_hp() as u32, divisor as u32);', '                    if false {\n                        self.drop_vol(holder, kind);\n                        return Res::Undef;\n                    }\n                }\n                let d = div1(self.mon(holder).max_hp() as u32, divisor as u32);'),
 ('substitute does not free from binding', C, '                if let Some(k) = VolKind::named("partiallytrapped") {\n                    self.drop_vol(holder, k);\n                }\n                Res::Undef\n            }\n            // onTryPrimaryHit(target, source, move): the substitute takes the hit.', '                Res::Undef\n            }\n            // onTryPrimaryHit(target, source, move): the substitute takes the hit.'),
 ('jaw lock traps one side', V, '                    self.add_trapped(source, target, Eff::Move(mi));\n                    self.add_trapped(target, source, Eff::Move(mi));', '                    self.add_trapped(target, source, Eff::Move(mi));'),
 ('electromorphosis does nothing', A, '                if self.add_volatile(holder, VolKind::Charge, None, Eff::None).truthy() {\n                    // `-start|pokemon|Charge|Move|[from] ability: Electromorphosis`\n                    self.show_ability(holder, ability);\n                }\n', ''),
 ("a move's own after-move callback is skipped", M, '        if self.move_has_cb(mv, Ev::AfterMove) {\n            let me = Eff::Move(mv);\n            self.single_event(Ev::AfterMove, me, None, Some(pokemon), target, me, Res::Undef);\n        }', ''),
 # --- weather, terrain and other field conditions
 ('rain water boost 1.5 -> 1.25', C, "Some(Type::Water) => self.chain_modify(3, 2),\n                    Some(Type::Fire) => self.chain_modify(1, 2),", "Some(Type::Water) => self.chain_modify(5, 4),\n                    Some(Type::Fire) => self.chain_modify(1, 2),"),
 ('sun weakens fire', C, "Type::Fire => self.chain_modify(3, 2),\n            Type::Water => self.chain_modify(1, 2),", "Type::Fire => self.chain_modify(1, 2),\n            Type::Water => self.chain_modify(1, 2),"),
 ('sandstorm damage 1/16 -> 1/8', C, "(Weather::Sandstorm, Ev::Weather) => {\n                if let Some(target) = e.target {\n                    let d = div1(self.mon(target).max_hp() as u32, 16);", "(Weather::Sandstorm, Ev::Weather) => {\n                if let Some(target) = e.target {\n                    let d = div1(self.mon(target).max_hp() as u32, 8);"),
 ('sandstorm hurts rock types', B, "Imm::Weather(Weather::Sandstorm) => self.type_allows(r, 8),", "Imm::Weather(Weather::Sandstorm) => true,"),
 ('sand spd boost for everyone', C, "if self.has_type(pokemon, Type::Rock) && self.effective_weather(pokemon) == Weather::Sandstorm {", "if self.effective_weather(pokemon) == Weather::Sandstorm {"),
 ('snow def boost 1.5 -> 1.3', C, "if self.has_type(pokemon, Type::Ice) && self.effective_weather(pokemon) == Weather::Snowscape {\n                    return Res::Num(crate::battle::modify(e.relay.num() as u32, 6144) as i32);", "if self.has_type(pokemon, Type::Ice) && self.effective_weather(pokemon) == Weather::Snowscape {\n                    return Res::Num(crate::battle::modify(e.relay.num() as u32, 5325) as i32);"),
 ('sun does not stop freezing', C, "if e.imm == Some(Imm::Status(Status::Frz)) {\n                    return FALSE;\n                }\n                Res::Undef\n            }\n\n            // onModifySpD", "if e.imm == Some(Imm::Status(Status::Frz)) {\n                    return Res::Undef;\n                }\n                Res::Undef\n            }\n\n            // onModifySpD"),
 ('weather lasts 6 turns', C, "Eff::Weather(Weather::Raindance) => {\n                if holds(self, it::DAMPROCK) {\n                    8\n                } else {\n                    5\n                }", "Eff::Weather(Weather::Raindance) => {\n                if holds(self, it::DAMPROCK) {\n                    8\n                } else {\n                    6\n                }"),
 ('heat rock 8 -> 7', C, "if holds(self, it::HEATROCK) {\n                    8", "if holds(self, it::HEATROCK) {\n                    7"),
 ('same weather can be set again', B, "        if self.field.weather.kind == w {\n            return FALSE;\n        }\n", ""),
 ('weather state takes an effect order', B, "        // A field effect's state has no target, so it takes no place in the effect order.\n        c.st = self.new_state_counted(false);", "        c.st = self.new_state_counted(true);"),
 ('cloud nine does not suppress', B, "&& matches!(m.ability, ab::CLOUDNINE | ab::AIRLOCK)", "&& matches!(m.ability, ab::AIRLOCK)"),
 ('no weather event update', E, "        if ev == Ev::Weather {\n            self.each_event(Ev::Update);\n        }\n", ""),
 ('mega sol ignored', B, "if self.active_pokemon.is_some_and(|p| self.has_ability(p, ab::MEGASOL))", "if self.active_pokemon.is_some_and(|p| self.has_ability(p, ab::NOABILITY))"),
 ('electric terrain boost 1.3 -> 1.5', C, "if mtype == Some(Type::Electric) && grounded(self, e.target) {\n                    return self.chain_modify(5325, 4096);", "if mtype == Some(Type::Electric) && grounded(self, e.target) {\n                    return self.chain_modify(6144, 4096);"),
 ('electric terrain lets sleep through', C, "if e.status == Status::Slp && grounded(self, e.target) {\n                    return FALSE;", "if e.status == Status::Slp && grounded(self, e.target) {\n                    return Res::Undef;"),
 ('grassy heal 1/16 -> 1/8', C, "if grounded(self, Some(pokemon)) {\n                        let amount = div1(self.mon(pokemon).max_hp() as u32, 16);", "if grounded(self, Some(pokemon)) {\n                        let amount = div1(self.mon(pokemon).max_hp() as u32, 8);"),
 ('grassy does not weaken earthquake', C, "if weakened && grounded(self, e.source) {\n                    return self.chain_modify(1, 2);", "if weakened && grounded(self, e.source) {\n                    return Res::Undef;"),
 ('misty protects the airborne', C, "(Terrain::Mistyterrain, Ev::SetStatus) => {\n                if !grounded(self, e.target) {\n                    return Res::Undef;\n                }", "(Terrain::Mistyterrain, Ev::SetStatus) => {"),
 ('misty dragon 0.5 -> 0.75', C, "if mtype == Some(Type::Dragon) && grounded(self, e.source) {\n                    return self.chain_modify(1, 2);", "if mtype == Some(Type::Dragon) && grounded(self, e.source) {\n                    return self.chain_modify(3, 4);"),
 ('psychic terrain blocks allies too', C, "if self.is_semi_invulnerable(target) || self.is_ally(target, source) {", "if self.is_semi_invulnerable(target) {"),
 ('psychic terrain blocks all moves', C, "if am.priority <= 0 || am.target == Target::User {", "if am.target == Target::User {"),
 ('terrain extender 8 -> 7', C, "if holds(self, it::TERRAINEXTENDER) {\n                    8", "if holds(self, it::TERRAINEXTENDER) {\n                    7"),
 ('trick room does nothing', B, "if self.field.pseudo.has(Pseudo::Trickroom) { -speed } else { speed }", "speed"),
 ('trick room cannot be undone', C, "(Pseudo::Trickroom | Pseudo::Magicroom | Pseudo::Wonderroom, Ev::FieldRestart) => {\n                self.remove_pseudo_weather(kind);", "(Pseudo::Trickroom | Pseudo::Magicroom | Pseudo::Wonderroom, Ev::FieldRestart) => {"),
 ('pseudo-weather always sorts as field', E, "let default = if c.targeted { 5 } else { 2 };", "let default = 5;"),
 ('gravity accuracy', C, "self.chain_modify(6840, 4096)", "self.chain_modify(6144, 4096)"),
 ('gravity does not ground', B, "        if self.field.pseudo.has(Pseudo::Gravity) {\n            return true;\n        }\n", ""),
 ('magic room leaves items on', B, "if self.field.pseudo.has(Pseudo::Magicroom) || self.has_vol_named(r, \"embargo\") {", "if self.has_vol_named(r, \"embargo\") {"),
 ('wonder room does not swap', B, "            2 if self.field.pseudo.has(Pseudo::Wonderroom) => 4,\n            4 if self.field.pseudo.has(Pseudo::Wonderroom) => 2,", "            2 if false => 4,\n            4 if false => 2,"),
 ('fairy lock does not trap', C, "(Pseudo::Fairylock, Ev::TrapPokemon) => {\n                if let Some(pokemon) = e.target {\n                    self.try_trap(pokemon, false);\n                }", "(Pseudo::Fairylock, Ev::TrapPokemon) => {"),
 ('tailwind x2 -> x1.5', C, "(SideCond::Tailwind, Ev::ModifySpe, Pre::On) => self.chain_modify(2, 1),", "(SideCond::Tailwind, Ev::ModifySpe, Pre::On) => self.chain_modify(3, 2),"),
 ('tailwind lasts 5 turns', C, "Eff::SideCond(SideCond::Tailwind) => 4,", "Eff::SideCond(SideCond::Tailwind) => 5,"),
 ('screens halve as in singles', C, "return self.chain_modify(2732, 4096);", "return self.chain_modify(2048, 4096);"),
 ('screens stop critical hits', C, "if !am.hit_data[self.slot_index(defender)].crit && !am.infiltrates {", "if !am.infiltrates {"),
 ('reflect covers special moves', C, "SideCond::Reflect => category == Category::Physical,", "SideCond::Reflect => true,"),
 ('aurora veil stacks with reflect', C, "!(conds.has(SideCond::Reflect) && category == Category::Physical\n                            || conds.has(SideCond::Lightscreen) && category == Category::Special)", "!(conds.has(SideCond::Lightscreen) && category == Category::Special)"),
 ('light clay 8 -> 7', C, "if holds(self, it::LIGHTCLAY) { 8 } else { 5 }", "if holds(self, it::LIGHTCLAY) { 7 } else { 5 }"),
 ('safeguard blocks self-inflicted status', C, "                if target != source {\n                    return Res::Null;\n                }\n                Res::Undef\n            }\n            // onTryAddVolatile(status, target, source, effect)\n            (SideCond::Safeguard", "                return Res::Null;\n            }\n            // onTryAddVolatile(status, target, source, effect)\n            (SideCond::Safeguard"),
 ('spikes ignore layers', C, "let d = div1([0u32, 3, 4, 6][layers.min(3) as usize] * self.mon(pokemon).max_hp() as u32, 24);", "let d = div1([0u32, 3, 3, 3][layers.min(3) as usize] * self.mon(pokemon).max_hp() as u32, 24);"),
 ('spikes hit the airborne', C, "(SideCond::Spikes, Ev::SwitchIn, Pre::On) => {\n                let Some(pokemon) = e.target else {\n                    return Res::Undef;\n                };\n                if !self.is_grounded(pokemon, false) {\n                    return Res::Undef;\n                }", "(SideCond::Spikes, Ev::SwitchIn, Pre::On) => {\n                let Some(pokemon) = e.target else {\n                    return Res::Undef;\n                };"),
 ('fourth layer of spikes', C, "let max = if kind == SideCond::Spikes { 3 } else { 2 };", "let max = if kind == SideCond::Spikes { 4 } else { 2 };"),
 ('toxic spikes always badly poison', C, "let status = if layers >= 2 { Status::Tox } else { Status::Psn };", "let status = if layers >= 1 { Status::Tox } else { Status::Psn };"),
 ('poison types leave toxic spikes', C, "                    self.remove_side_condition(pokemon.side as usize, SideCond::Toxicspikes);\n", ""),
 ('stealth rock ignores type', C, "let type_mod = self.run_effectiveness(pokemon, rock).clamp(-6, 6);", "let type_mod = self.run_effectiveness(pokemon, rock).clamp(0, 0);"),
 ('sticky web -1 -> -2', C, "self.boost1(SPE, -1, Some(pokemon), Some(source), Eff::Move(web));", "self.boost1(SPE, -2, Some(pokemon), Some(source), Eff::Move(web));"),
 ('side condition takes no effect order', B, "        // A side condition's state has the side as its target, so it is counted.\n        c.st = self.new_state_counted(true);", "        c.st = self.new_state_counted(false);"),
 ('wish heals a third', C, "let hp = e.source.map_or(0, |s| self.mon(s).max_hp() / 2);", "let hp = e.source.map_or(0, |s| self.mon(s).max_hp() / 3);"),
 ('wish comes true at once', C, "if self.overflowed_turn_count() <= c.st.a {", "if self.overflowed_turn_count() < c.st.a {"),
 # --- abilities and items that depend on the field
 ('swift swim x2 -> x1.5', A, "(ab::SWIFTSWIM, Ev::ModifySpe, Pre::On) => {\n                if self.effective_weather(holder) == Weather::Raindance {\n                    return self.chain_modify(2, 1);", "(ab::SWIFTSWIM, Ev::ModifySpe, Pre::On) => {\n                if self.effective_weather(holder) == Weather::Raindance {\n                    return self.chain_modify(3, 2);"),
 ('drought brings rain', A, "(ab::DROUGHT, Ev::Start, Pre::On) => {\n                self.set_weather(Weather::Sunnyday, None, Eff::None);", "(ab::DROUGHT, Ev::Start, Pre::On) => {\n                self.set_weather(Weather::Raindance, None, Eff::None);"),
 ('solar power costs 1/16', A, "if self.effective_weather(holder) == w && w == Weather::Sunnyday {\n                    let d = div1(self.mon(holder).max_hp() as u32, 8);", "if self.effective_weather(holder) == w && w == Weather::Sunnyday {\n                    let d = div1(self.mon(holder).max_hp() as u32, 16);"),
 ('rain dish heals 1/8', A, "if self.effective_weather(holder) == w && w == Weather::Raindance {\n                    let amount = div1(self.mon(holder).max_hp() as u32, 16);", "if self.effective_weather(holder) == w && w == Weather::Raindance {\n                    let amount = div1(self.mon(holder).max_hp() as u32, 8);"),
 ('sand veil 3277 -> 3686', A, "(ab::SANDVEIL, Ev::ModifyAccuracy, Pre::On) => {\n                if matches!(relay, Res::Num(_)) && self.is_weather(Weather::Sandstorm) {\n                    return self.chain_modify(3277, 4096);", "(ab::SANDVEIL, Ev::ModifyAccuracy, Pre::On) => {\n                if matches!(relay, Res::Num(_)) && self.is_weather(Weather::Sandstorm) {\n                    return self.chain_modify(3686, 4096);"),
 ('harvest rolls in the sun', A, "if self.is_weather(Weather::Sunnyday) || self.chance(1, 2, \"harvest\") {", "if self.chance(1, 2, \"harvest\") {"),
 ('grass pelt 1.5 -> 1.3', A, "(ab::GRASSPELT, Ev::ModifyDef, Pre::On) => {\n                if self.is_terrain(Terrain::Grassyterrain) {\n                    return self.chain_modify(3, 2);", "(ab::GRASSPELT, Ev::ModifyDef, Pre::On) => {\n                if self.is_terrain(Terrain::Grassyterrain) {\n                    return self.chain_modify(5325, 4096);"),
 ('mimicry keeps its type', A, 'if current[..n] != wanted[..] && self.set_type(holder, types) {', 'if false {'),
 ('screen cleaner spares the foe', A, 'for side in [holder.side as usize, 1 - holder.side as usize] {\n                        if self.remove_side_condition(side, cond) {', 'for side in [holder.side as usize] {\n                        if self.remove_side_condition(side, cond) {'),
 ('toxic debris on special hits', A, "if mcat == Some(Category::Physical) && layers.is_none_or(|l| l < 2) {", "if layers.is_none_or(|l| l < 2) {"),
 ('synchronize passes toxic spikes poison', A, "                    || e.effect == Eff::SideCond(SideCond::Toxicspikes)\n", ""),
 ('armor tail stops field moves', A, "if am.target == Target::FoeSide\n                    || (all && !matches!(am.d().id, \"perishsong\" | \"flowershield\" | \"rototiller\"))\n                {", "if am.target == Target::FoeSide {"),
 ('seed does not boost', B, "        if let Some(b) = ITEMS[item as usize].boosts {\n            self.boost(b, Some(r), source, Eff::Item(item));\n        }\n", ""),
 ('seed used without its terrain', I, "if !self.ignoring_item(holder) && self.is_terrain(terrain) {", "if !self.ignoring_item(holder) {"),
 # --- moves that depend on the field
 ('weather ball not doubled', V, "if e.target.is_some_and(|p| self.effective_weather(p) != Weather::None) {\n                    self.am[mi as usize].base_power *= 2;\n                }", ""),
 ('thunder misses in rain', V, "Some(Weather::Raindance) => self.am[mi as usize].accuracy = 0,", "Some(Weather::Raindance) => {}"),
 ('moonlight heals half in rain', V, "                    _ => 1024,\n                };\n                let amount = modify(self.mon(pokemon).max_hp() as u32, factor);", "                    _ => 2048,\n                };\n                let amount = modify(self.mon(pokemon).max_hp() as u32, factor);"),
 ('expanding force stays single target', V, "                    self.am[mi as usize].target = Target::AllAdjacentFoes;\n", ""),
 ('rising voltage not doubled', V, "return Res::Num(bp * 2);", "return Res::Num(bp);"),
 ('grassy glide no priority', V, "return Res::Num(e.relay.num() + 1);", "return Res::Num(e.relay.num());"),
 ('steel roller works without terrain', V, "(mv::STEELROLLER, Ev::Try) => Res::Bool(self.field.terrain.kind != Terrain::None),", "(mv::STEELROLLER, Ev::Try) => TRUE,"),
 ('brick break leaves screens', V, "                    for cond in SCREENS {\n                        self.remove_side_condition(target.side as usize, cond);\n                    }", ""),
 ('defog leaves own hazards', V, "for side in [target.side as usize, source.side as usize] {", "for side in [target.side as usize] {"),
 ('floettite goes by base species', I, "if d.flags & IF_MEGA_BY_FORME != 0 {", "if false {"),
 ('rapid spin under sheer force', V, "if self.am[mi as usize].has_sheer_force || (need_hp && self.mon(pokemon).hp == 0) {", "if need_hp && self.mon(pokemon).hp == 0 {"),
 ('court change one way', V, "                for c in from_foe.as_slice() {\n                    self.sides[own].conds.push(*c);\n                }", ""),
 ('haze spares the user', V, "                for &pokemon in &actives[..n] {\n                    self.mon_mut(pokemon).boosts = [0; 7];\n                }", "                for &pokemon in &actives[..n.min(1)] {\n                    self.mon_mut(pokemon).boosts = [0; 7];\n                }"),
 ('aurora veil without snow', V, "(mv::AURORAVEIL, Ev::Try) => Res::Bool(self.is_weather(Weather::Snowscape)),", "(mv::AURORAVEIL, Ev::Try) => TRUE,"),
 ('hazards cost no extra pp under pressure', M, "            if self.am[m].flags & F_MUSTPRESSURE != 0 {", "            if false {"),
 ('field move hits despite failed weather', M, "result = damage.hit() || damage == Res::Undef;", "result = true;"),
 # --- Mega Evolution
 ('mega keeps the old ability', B, '            self.set_ability_ex(r, ability, None, Eff::None, true);\n            self.mon_mut(r).base_ability = ability;', '            self.mon_mut(r).base_ability = ability;'),
 ('mega ability asks permission', B, "self.set_ability_ex(r, ability, None, Eff::None, true);", "self.set_ability_ex(r, ability, None, Eff::None, false);"),
 ('mega keeps the old stats', B, "        m.stats[1..].copy_from_slice(&stats[1..]);\n", ""),
 ('mega keeps the old types', B, '        m.types = SPECIES[species as usize].types;\n        m.added_type = Type::None;\n', '        m.added_type = Type::None;\n'),
 ('mega reverts on switching out', B, "        self.mon_mut(r).base_species = species;\n", ""),
 ('second mega allowed', B, "            side.team[i].can_mega = NO_SPECIES;", "            side.team[i].can_mega = side.team[i].can_mega;"),
 ('mega does not count as acting', B, "        self.mon_mut(r).move_this_turn = TRUE;\n", ""),
 ('mega after moves', B, "Battle::blank_action(ActKind::MegaEvo, 104)", "Battle::blank_action(ActKind::MegaEvo, 204)"),
 ('mega stone can be taken', I, "            return Res::Bool(!own);", "            return Res::Bool(true);"),
 # --- abilities
 ('hugepower x2 -> x1.5', A, "(ab::HUGEPOWER | ab::PUREPOWER, Ev::ModifyAtk, Pre::On) => self.chain_modify(2, 1),", "(ab::HUGEPOWER | ab::PUREPOWER, Ev::ModifyAtk, Pre::On) => self.chain_modify(3, 2),"),
 ('ironfist 4915 -> 5325', A, "ab::IRONFIST => (F_PUNCH, 4915),", "ab::IRONFIST => (F_PUNCH, 5325),"),
 ('megalauncher 1.5 -> 1.3', A, "ab::MEGALAUNCHER => (F_PULSE, 6144),", "ab::MEGALAUNCHER => (F_PULSE, 5325),"),
 ('strongjaw flag', A, "ab::STRONGJAW => (F_BITE, 6144),", "ab::STRONGJAW => (F_PUNCH, 6144),"),
 ('toughclaws 5325 -> 4915', A, "ab::TOUGHCLAWS => (F_CONTACT, 5325),", "ab::TOUGHCLAWS => (F_CONTACT, 4915),"),
 ('sharpness flag', A, "ab::SHARPNESS => (F_SLICING, 6144),", "ab::SHARPNESS => (F_BITE, 6144),"),
 ('technician threshold 60 -> 70', A, "<= 60 {", "<= 70 {"),
 ('blaze type', A, "ab::BLAZE => Type::Fire,", "ab::BLAZE => Type::Water,"),
 ('pinch threshold 1/3 -> 1/2', A, "m.hp as u32 * 3 <= m.max_hp() as u32", "m.hp as u32 * 2 <= m.max_hp() as u32"),
 ('adaptability 2 -> 1.75', A, "{ 9216 } else { 8192 }", "{ 9216 } else { 7168 }"),
 # ("analytic counts itself" is an equivalent change: the user's own action has already left the queue.)
 ('analytic 1.3 -> 1.2', A, "if boosted {\n                    return self.chain_modify(5325, 4096);", "if boosted {\n                    return self.chain_modify(4915, 4096);"),
 ('analytic never boosted', A, "let boosted = !actives[..n]", "let boosted = actives[..n]"),
 ('merciless ratio', A, "return Res::Num(5);", "return Res::Num(2);"),
 ('superluck +1 -> +2', A, "(ab::SUPERLUCK, Ev::ModifyCritRatio, Pre::On) => Res::Num(relay.num() + 1),", "(ab::SUPERLUCK, Ev::ModifyCritRatio, Pre::On) => Res::Num(relay.num() + 2),"),
 ('stall sign', A, "(ab::STALL, Ev::FractionalPriority, Pre::On) => Res::Num(-1),", "(ab::STALL, Ev::FractionalPriority, Pre::On) => Res::Num(1),"),
 ('quickfeet 1.5 -> 2', A, "(ab::QUICKFEET, Ev::ModifySpe, Pre::On) => {\n                if self.mon(holder).status != Status::None {\n                    return self.chain_modify(6144, 4096);", "(ab::QUICKFEET, Ev::ModifySpe, Pre::On) => {\n                if self.mon(holder).status != Status::None {\n                    return self.chain_modify(8192, 4096);"),
 ('stakeout inverted', A, "self.mon(d).active_turns == 0", "self.mon(d).active_turns > 0"),
 ('unaware keeps evasion', A, "                    self.event.boosts[EVA] = 0;\n", ""),
 ('unaware keeps attack', A, "                    self.event.boosts[ATK] = 0;\n                    self.event.boosts[DEF] = 0;\n                    self.event.boosts[SPA] = 0;", "                    self.event.boosts[DEF] = 0;\n                    self.event.boosts[SPA] = 0;"),
 ('hustle atk', A, "modify(relay.num() as u32, 6144)", "modify(relay.num() as u32, 5325)"),
 ('hustle accuracy', A, "if mcat == Some(Category::Physical) && matches!(relay, Res::Num(_)) {\n                    return self.chain_modify(3277, 4096);", "if mcat == Some(Category::Physical) && matches!(relay, Res::Num(_)) {\n                    return self.chain_modify(3686, 4096);"),
 ('keeneye ignores nothing', A, "self.am[m as usize].ignore_evasion = true;", "self.am[m as usize].ignore_evasion = false;"),
 ('contrary off', A, "*b = -*b;", "*b = *b;"),
 ('magicguard only recoil', A, "(ab::MAGICGUARD, Ev::Damage, Pre::On) => {\n                if !e.effect.is_move() {", "(ab::MAGICGUARD, Ev::Damage, Pre::On) => {\n                if e.effect == Eff::Recoil {"),
 ('battlearmor off', A, "(ab::BATTLEARMOR | ab::SHELLARMOR, Ev::CriticalHit, Pre::On) => FALSE,", "(ab::BATTLEARMOR | ab::SHELLARMOR, Ev::CriticalHit, Pre::On) => Res::Undef,"),
 ('-ate boost 1.2 -> 1.3', A, "if self.am[m as usize].type_changer_boosted == Eff::Ability(ability) {\n                        return self.chain_modify(4915, 4096);", "if self.am[m as usize].type_changer_boosted == Eff::Ability(ability) {\n                        return self.chain_modify(5325, 4096);"),
 ('pixilate type', A, "ab::PIXILATE => Type::Fairy,", "ab::PIXILATE => Type::Ice,"),
 ('fluffy contact /2 -> /4', A, "                    num /= 2;", "                    num /= 4;"),
 ('heatproof burn /2 -> /3', A, "return Res::Num(relay.num() / 2);", "return Res::Num(relay.num() / 3);"),
 ('stickyhold off', A, '                    // `-activate|pokemon|ability: Sticky Hold`\n                    self.show_ability(holder, ability);\n                    return FALSE;', '                    // `-activate|pokemon|ability: Sticky Hold`\n                    self.show_ability(holder, ability);\n                    return Res::Undef;'),
 ('minus/plus counts itself', A, "if a != holder && (self.has_ability(a, ab::MINUS)", "if (self.has_ability(a, ab::MINUS)"),
 ('firemane type', A, "(ab::FIREMANE, Ev::ModifyAtk | Ev::ModifySpA, Pre::On) => {\n                if mtype == Some(Type::Fire) {", "(ab::FIREMANE, Ev::ModifyAtk | Ev::ModifySpA, Pre::On) => {\n                if mtype == Some(Type::Water) {"),
 ('intimidate -1 -> -2', A, "self.boost1(ATK, -1, Some(f), Some(holder), Eff::None);", "self.boost1(ATK, -2, Some(f), Some(holder), Eff::None);"),
 ('sturdy leaves 2 hp', A, 'let left = m.hp as i32 - 1;', 'let left = m.hp as i32 - 2;'),
 ('regenerator 1/3 -> 1/4', A, "let amount = self.mon(holder).max_hp() as i32 / 3;", "let amount = self.mon(holder).max_hp() as i32 / 4;"),
 ('moody +2 -> +1', A, "[(plus, 2), (minus, -1)]", "[(plus, 1), (minus, -1)]"),
 ('synchronize passes freeze', A, "matches!(e.status, Status::Slp | Status::Frz)", "matches!(e.status, Status::Slp)"),
 ('wandering spirit contact inverted', A, "(ab::WANDERINGSPIRIT, Ev::DamagingHit, Pre::On) => {\n                if let (Some(m), Some(target), Some(source)) = (mi, e.target, e.source) {\n                    if self.makes_contact(m) {", "(ab::WANDERINGSPIRIT, Ev::DamagingHit, Pre::On) => {\n                if let (Some(m), Some(target), Some(source)) = (mi, e.target, e.source) {\n                    if !self.makes_contact(m) {"),
 ('shield dust off', A, "self.event.secs &= !(1 << k);", "self.event.secs &= 0xff;"),
 ('skill link min hits', A, "am.multihit = (am.multihit.1, am.multihit.1);", "am.multihit = (am.multihit.0, am.multihit.0);"),
 ('serene grace off', A, "s.chance *= 2;", "s.chance *= 1;"),
 ('sheer force no boost', A, "am.has_sheer_force = true;", "am.has_sheer_force = false;"),
 ('sniper 1.5 -> 2', A, "if self.am[m as usize].hit_data[self.slot_index(target)].crit {\n                        return self.chain_modify(6144, 4096);", "if self.am[m as usize].hit_data[self.slot_index(target)].crit {\n                        return self.chain_modify(8192, 4096);"),
 ('multiscale 0.5 -> 0.75', A, "if m.hp >= m.max_hp() {\n                    return self.chain_modify(2048, 4096);", "if m.hp >= m.max_hp() {\n                    return self.chain_modify(3072, 4096);"),
 ('friend guard 0.75 -> 0.5', A, "if target != holder && self.is_ally(target, holder) {\n                        return self.chain_modify(3072, 4096);", "if target != holder && self.is_ally(target, holder) {\n                        return self.chain_modify(2048, 4096);"),
 ('thick fat only ice', A, "matches!(mtype, Some(Type::Ice | Type::Fire))", "matches!(mtype, Some(Type::Ice))"),
 ('rivalry swapped', A, "return if a == d {", "return if a != d {"),
 ('supreme overlord table', A, "[4096, 4506, 4915, 5325, 5734, 6144]", "[4096, 4915, 4915, 5325, 5734, 6144]"),
 ('shadow tag off', A, "self.try_trap(pokemon, true);", ""),
 ('pressure 1 -> 2', A, "                Res::Num(1)\n            }\n\n            // ---- Punk Rock", "                Res::Num(2)\n            }\n\n            // ---- Punk Rock"),
 ('opportunist copies nothing', A, "m.ability_boosts[k] += e.boosts[k];", "m.ability_boosts[k] += 0;"),
 ('trace picks first', A, "let target = cands[self.rand(k as u32, \"trace\") as usize];", "self.rand(k as u32, \"trace\");\n                let target = cands[0];"),
 ('filter 0.75 -> 0.5', A, "(ab::FILTER | ab::SOLIDROCK, Ev::ModifyDamage, Pre::Source) => {\n                if let (Some(m), Some(target)) = (mi, e.source) {\n                    if self.am[m as usize].hit_data[self.slot_index(target)].type_mod > 0 {\n                        return self.chain_modify(3072, 4096);", "(ab::FILTER | ab::SOLIDROCK, Ev::ModifyDamage, Pre::Source) => {\n                if let (Some(m), Some(target)) = (mi, e.source) {\n                    if self.am[m as usize].hit_data[self.slot_index(target)].type_mod > 0 {\n                        return self.chain_modify(2048, 4096);"),
 ('tangled feet 0.5 -> 0.75', A, "self.vols(holder).has(VolKind::Confusion) {\n                    return self.chain_modify(2048, 4096);", "self.vols(holder).has(VolKind::Confusion) {\n                    return self.chain_modify(3072, 4096);"),
 ('aura guard 0.5 -> 0.75', A, "(ab::AURAGUARD, Ev::ModifyDamage, Pre::Source) => {\n                if mflags & F_CONTACT != 0 {\n                    return self.chain_modify(2048, 4096);", "(ab::AURAGUARD, Ev::ModifyDamage, Pre::Source) => {\n                if mflags & F_CONTACT != 0 {\n                    return self.chain_modify(3072, 4096);"),
 ('fairy aura 5448 -> 5325', A, "self.chain_modify(5448, 4096)", "self.chain_modify(5325, 4096)"),
 ('long reach keeps contact', A, "self.am[m as usize].flags &= !F_CONTACT;", "self.am[m as usize].flags &= !F_SOUND;"),
 ('no guard accuracy', A, "                    return TRUE;\n                }\n                relay\n", "                    return relay;\n                }\n                relay\n"),
 ('galewings any hp', A, "if mtype == Some(Type::Flying) && m.hp == m.max_hp() {", "if mtype == Some(Type::Flying) {"),
 ('prankster +1 -> +2', A, "self.am[m as usize].prankster_boosted = true;\n                    return Res::Num(relay.num() + 1);", "self.am[m as usize].prankster_boosted = true;\n                    return Res::Num(relay.num() + 2);"),
 ('quick draw odds', A, "self.chance(3, 10, \"quick draw\")", "self.chance(4, 10, \"quick draw\")"),
 ('steely spirit 1.5 -> 1.3', A, "(ab::STEELYSPIRIT, Ev::BasePower, Pre::Ally) => {\n                if mtype == Some(Type::Steel) {\n                    return self.chain_modify(6144, 4096);", "(ab::STEELYSPIRIT, Ev::BasePower, Pre::Ally) => {\n                if mtype == Some(Type::Steel) {\n                    return self.chain_modify(5325, 4096);"),
 ('reckless 1.2 -> 1.3', A, 'd().has_crash_damage)\n                {\n                    return self.chain_modify(4915, 4096);', 'd().has_crash_damage)\n                {\n                    return self.chain_modify(5325, 4096);'),
 ('scrappy off', A, "self.am[m as usize].ignore_immunity = IgnoreImm::NormalFighting;", "self.am[m as usize].ignore_immunity = IgnoreImm::No;"),
 ('liquid voice type', A, "self.am[m as usize].typ = Type::Water;", "self.am[m as usize].typ = Type::Ice;"),
 ('marvel scale 1.5 -> 2', A, "(ab::MARVELSCALE, Ev::ModifyDef, Pre::On) => {\n                if self.mon(holder).status != Status::None {\n                    return self.chain_modify(6144, 4096);", "(ab::MARVELSCALE, Ev::ModifyDef, Pre::On) => {\n                if self.mon(holder).status != Status::None {\n                    return self.chain_modify(8192, 4096);"),
 ('fur coat 2 -> 1.5', A, "(ab::FURCOAT, Ev::ModifyDef, Pre::On) => self.chain_modify(2, 1),", "(ab::FURCOAT, Ev::ModifyDef, Pre::On) => self.chain_modify(3, 2),"),
 ('guts 1.5 -> 2', A, "(ab::GUTS, Ev::ModifyAtk, Pre::On) => {\n                if self.mon(holder).status != Status::None {\n                    return self.chain_modify(6144, 4096);", "(ab::GUTS, Ev::ModifyAtk, Pre::On) => {\n                if self.mon(holder).status != Status::None {\n                    return self.chain_modify(8192, 4096);"),
 ('compound eyes 1.3 -> 1.2', A, "(ab::COMPOUNDEYES, Ev::ModifyAccuracy, Pre::Source) => {\n                if matches!(relay, Res::Num(_)) {\n                    return self.chain_modify(5325, 4096);", "(ab::COMPOUNDEYES, Ev::ModifyAccuracy, Pre::Source) => {\n                if matches!(relay, Res::Num(_)) {\n                    return self.chain_modify(4915, 4096);"),
 ('queenly majesty blocks nothing', A, '                    // `cant|holder|ability: Armor Tail|Move|[of] user`\n                    self.show_ability(holder, ability);\n                    return FALSE;', '                    // `cant|holder|ability: Armor Tail|Move|[of] user`\n                    self.show_ability(holder, ability);\n                    return Res::Undef;'),
 ('stench/kings rock 10% -> 20%', I, "                chance: 10,", "                chance: 20,"),
 # --- abilities checked directly in the simulator core
 ('prankster hits Dark types', M, "&& !self.type_allows(t, 7)", "&& false"),
 ('parental bond second hit 0.25 -> 0.5', M, "dmg = modify(dmg, 1024);\n        }\n        if self.listens(Ev::WeatherModifyDamage)", "dmg = modify(dmg, 2048);\n        }\n        if self.listens(Ev::WeatherModifyDamage)"),
 ('guts still halved by burn', M, "            && !self.has_ability(user, ab::GUTS)\n", ""),
 ('klutz off', B, "ITEMS[m.item as usize].flags & IF_IGNORE_KLUTZ == 0 && self.has_ability(r, ab::KLUTZ)", "false"),
 ('corrosion off', B, "if !corrosive && !self.run_status_immunity(r, Imm::Status(status)) {", "if !self.run_status_immunity(r, Imm::Status(status)) {"),
 ('levitate off', B, "(self.has_ability(r, ab::LEVITATE) || self.has_ability(r, ab::EELEVATE))", "self.has_ability(r, ab::EELEVATE)"),
 ('stalwart off', B, 'if tracks || self.has_ability(user, ab::STALWART) || self.has_ability(user, ab::PROPELLERTAIL) {', 'if tracks {'),
 ('early bird off', C, "                if early {", "                if early && false {"),
 ('quick feet still halved by paralysis', C, "if !self.has_ability(holder, ab::QUICKFEET) {", "if true {"),
 ('mold breaker off', E, "if ABILITIES[a as usize].flags & AF_BREAKABLE != 0 && self.suppressing_ability(Some(r)) {", "if false {"),
 ('sheer force keeps life orb recoil', M, '        if !self.suppressing_secondaries() && self.am[m].flags & F_FUTUREMOVE == 0 {', '        if self.am[m].flags & F_FUTUREMOVE == 0 {'),
 ('flash fire boost 1.5 -> 2', C, "if fire && self.has_ability(holder, ab::FLASHFIRE) {\n                    return self.chain_modify(6144, 4096);", "if fire && self.has_ability(holder, ab::FLASHFIRE) {\n                    return self.chain_modify(8192, 4096);"),
 ('unburden x2 -> x1.5', C, "!self.ignoring_ability(holder) {\n                    return self.chain_modify(2, 1);", "!self.ignoring_ability(holder) {\n                    return self.chain_modify(3, 2);"),
 # --- items
 ('charcoal type', I, "it::CHARCOAL => Type::Fire,", "it::CHARCOAL => Type::Water,"),
 ('type booster 4915 -> 5324', I, "if self.event_move().is_some_and(|mi| self.am[mi as usize].typ == t) {\n                return self.chain_modify(4915, 4096);", "if self.event_move().is_some_and(|mi| self.am[mi as usize].typ == t) {\n                return self.chain_modify(5324, 4096);"),
 ('resist berry 0.5 -> 0.75', I, "if self.eat_item(target, false, None, Eff::None) {\n                            return self.chain_modify(2048, 4096);", "if self.eat_item(target, false, None, Eff::None) {\n                            return self.chain_modify(3072, 4096);"),
 ('chilan needs super effective', I, "(item == it::CHILANBERRY || am.hit_data[slot].type_mod > 0)", "(am.hit_data[slot].type_mod > 0)"),
 ('expert belt 4915 -> 5324', I, "if self.am[mi as usize].hit_data[slot].type_mod > 0 {\n                        return self.chain_modify(4915, 4096);", "if self.am[mi as usize].hit_data[slot].type_mod > 0 {\n                        return self.chain_modify(5324, 4096);"),
 ('muscle band 4505 -> 4915', I, "Category::Physical) {\n                    return self.chain_modify(4505, 4096);", "Category::Physical) {\n                    return self.chain_modify(4915, 4096);"),
 ('wise glasses 4505 -> 4915', I, "Category::Special) {\n                    return self.chain_modify(4505, 4096);", "Category::Special) {\n                    return self.chain_modify(4915, 4096);"),
 ('scope lens +1 -> +2', I, "(it::SCOPELENS, Ev::ModifyCritRatio, Pre::On) => Res::Num(relay.num() + 1),", "(it::SCOPELENS, Ev::ModifyCritRatio, Pre::On) => Res::Num(relay.num() + 2),"),
 ('wide lens 4505 -> 4915', I, "(it::WIDELENS, Ev::ModifyAccuracy, Pre::Source) => {\n                if matches!(relay, Res::Num(_)) {\n                    return self.chain_modify(4505, 4096);", "(it::WIDELENS, Ev::ModifyAccuracy, Pre::Source) => {\n                if matches!(relay, Res::Num(_)) {\n                    return self.chain_modify(4915, 4096);"),
 ('zoom lens inverted', I, "e.target.is_some_and(|t| !self.will_move(t))", "e.target.is_some_and(|t| self.will_move(t))"),
 ('bright powder 3686 -> 3277', I, "return self.chain_modify(3686, 4096);", "return self.chain_modify(3277, 4096);"),
 ('choice scarf 1.5 -> 1.3', I, "(it::CHOICESCARF, Ev::ModifySpe, Pre::On) => self.chain_modify(6144, 4096),", "(it::CHOICESCARF, Ev::ModifySpe, Pre::On) => self.chain_modify(5325, 4096),"),
 ('iron ball speed 0.5 -> 0.75', I, "(it::IRONBALL, Ev::ModifySpe, Pre::On) => self.chain_modify(2048, 4096),", "(it::IRONBALL, Ev::ModifySpe, Pre::On) => self.chain_modify(3072, 4096),"),
 ('iron ball does not ground', B, "if item == it::IRONBALL {", "if false {"),
 ('air balloon does not lift', B, "item != it::AIRBALLOON", "true"),
 ('leek +2 -> +1', I, "return Res::Num(relay.num() + 2);", "return Res::Num(relay.num() + 1);"),
 ('light ball species', I, 'if base == "pikachu" {', 'if base == "raichu" {'),
 ('life orb 5324 -> 5325', I, "(it::LIFEORB, Ev::ModifyDamage, Pre::On) => self.chain_modify(5324, 4096),", "(it::LIFEORB, Ev::ModifyDamage, Pre::On) => self.chain_modify(5325, 4096),"),
 ('life orb recoil 1/10 -> 1/8', I, "let d = div1(self.mon(source).max_hp() as u32, 10);", "let d = div1(self.mon(source).max_hp() as u32, 8);"),
 ('metronome first step', C, "[4096, 4915, 5734, 6553, 7372, 8192]", "[4096, 5000, 5734, 6553, 7372, 8192]"),
 ('big root 1.3 -> 1.5', I, '                    || self.eff_is_named(e.effect, "strengthsap")\n                {\n                    return self.chain_modify(5324, 4096);', '                    || self.eff_is_named(e.effect, "strengthsap")\n                {\n                    return self.chain_modify(6144, 4096);'),
 ('quick claw odds', I, 'self.chance(1, 5, "quick claw")', 'self.chance(1, 4, "quick claw")'),
 ('focus band odds', I, 'self.chance(1, 10, "focus band")', 'self.chance(1, 9, "focus band")'),
 ('focus sash leaves 2 hp', I, "if self.use_item(target, None, Eff::None) {\n                        return Res::Num(self.mon(target).hp as i32 - 1);", "if self.use_item(target, None, Eff::None) {\n                        return Res::Num(self.mon(target).hp as i32 - 2);"),
 ('shed shell traps', I, "(it::SHEDSHELL, Ev::TrapPokemon, Pre::On) => {\n                self.mon_mut(holder).trapped = Trapped::No;", "(it::SHEDSHELL, Ev::TrapPokemon, Pre::On) => {\n                self.mon_mut(holder).trapped = Trapped::Yes;"),
 ('leppa 10 -> 5', I, "{ 20 } else { 10 }", "{ 20 } else { 5 }"),
 ('shell bell 1/8 -> 1/4', I, "div1(total as u32, 8)", "div1(total as u32, 4)"),
 ('rocky helmet 1/6 -> 1/8', I, "div1(self.mon(source).max_hp() as u32, 6)", "div1(self.mon(source).max_hp() as u32, 8)"),
 ('leftovers 1/16 -> 1/8', I, "(it::LEFTOVERS, Ev::Residual, Pre::On) => {\n                let amount = div1(self.mon(holder).max_hp() as u32, 16);", "(it::LEFTOVERS, Ev::Residual, Pre::On) => {\n                let amount = div1(self.mon(holder).max_hp() as u32, 8);"),
 ('gem 1.3 -> 1.5', C, "(VolKind::Gem, Ev::BasePower) => self.chain_modify(5325, 4096),", "(VolKind::Gem, Ev::BasePower) => self.chain_modify(6144, 4096),"),
 ('white herb sets +1', I, "                    if *b < 0 {\n                        *b = 0;", "                    if *b < 0 {\n                        *b = 1;"),
 ('persim does nothing', I, "(it::PERSIMBERRY, Ev::Eat, Pre::On) => {\n                self.remove_volatile(holder, VolKind::Confusion);", "(it::PERSIMBERRY, Ev::Eat, Pre::On) => {"),
 ('sitrus 1/4 -> 1/3', I, "let amount = if item == it::ORANBERRY { 10 } else { div1(self.mon(holder).max_hp() as u32, 4) };\n                self.heal(", "let amount = if item == it::ORANBERRY { 10 } else { div1(self.mon(holder).max_hp() as u32, 3) };\n                self.heal("),
 ('choice lock off', C, '                self.disable_slots_where(holder, |s| s.id + 1 != locked);\n', ''),
]

def sh(cmd, **kw):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True, **kw)

ARM = re.compile(r'^            \((mv|ab|it|VolKind|SlotCond|SideCond|Status|Pseudo|Weather|Terrain)::')

def callback_arms(text):
    """The match arms that are callback bodies: (header text, is a block)."""
    out = []
    lines = text.split('\n')
    i = 0
    while i < len(lines):
        if ARM.match(lines[i]) or lines[i] == '            (':
            j = i
            while not re.search(r'=> (\{|.*,)$', lines[j]) and j - i < 12:
                j += 1
            header = '\n'.join(lines[i:j + 1])
            if lines[j].endswith('=> {'):
                # (A block that only says `Res::Undef` is already switched off.)
                k = j + 1
                body = []
                while k < len(lines) and lines[k] != '            }':
                    if not lines[k].strip().startswith('//'):
                        body.append(lines[k].strip())
                    k += 1
                if body != ['Res::Undef']:
                    out.append((header, True))
            elif '=> ' in lines[j] and not lines[j].endswith('=> Res::Undef,'):
                out.append((header, False))
            i = j + 1
        else:
            i += 1
    return out

# Mutations that were tried and cannot be observed in Champions, kept out of MUT:
#  - Sheer Force stripping the secondaries of a move marked hasSheerForceBoost (Electro Shot):
#    the move has none when Sheer Force looks. King's Rock adds one, but later.
#  - A pivoting move setting its switch flag on a user with no HP left, or Dragon Tail forcing a
#    switch from a user with no HP left: nothing takes the user's HP before that point (Rocky
#    Helmet, recoil and Life Orb all come after).
#  - Curse applying its three stat changes in another order: nothing reacts to a self-inflicted
#    change stat by stat.
#  - Volatiles passed by Baton Pass keeping their effect order: the order only separates
#    switch-in and redirection handlers, and the only passable volatiles with one (Follow Me,
#    Rage Powder) last for the turn they were used in, when their user cannot also Baton Pass.
#  - Stance Change working for a transformed Pokémon: an ability that cannot be copied by
#    Transform is ignored altogether while its holder is transformed.
#  - A hit on Mimikyu's disguise counting as critical: the hit does no damage either way.
#  - A permanent forme change keeping the old maximum HP: the two that exist in Champions
#    (Mimikyu's disguise breaking, Palafin's Hero forme) do not change base HP.

# Callbacks that do nothing observable in Champions, so that switching them off changes nothing.
EQUIVALENT = {
    # Returns the move's own base power except for Greninja-Ash, which Champions does not have.
    '            (mv::WATERSHURIKEN, Ev::BasePowerCallback)',
    # Zeroes two fields of a volatile that has just been created, where they are zero already.
    '            (VolKind::Counter | VolKind::Mirrorcoat, Ev::Start)',
    # The core clears the illusion of a fainted Pokémon itself (Battle#faintMessages).
    '            (ab::ILLUSION, Ev::Faint, Pre::On)',
    # Marks the user as having had its BeforeSwitchOut event, which the core does for anyone
    # about to switch, and which nothing in Champions listens to in any case (no Pursuit).
    '            (mv::BATONPASS | mv::SHEDTAIL, Ev::SelfHit)',
    # Zeroes the counter of a volatile that has just been created.
    '            (VolKind::Metronome, Ev::Start)',
    # Returns true where nothing is returned otherwise (the move only fails in singles).
    '            (mv::FOLLOWME | mv::RAGEPOWDER, Ev::Try)',
    # Refuses a fourth Stockpile, which the volatile's own onRestart refuses as well.
    '            (mv::STOCKPILE, Ev::Try)',
    # Heal Block only comes from Psychic Noise in Champions. Its onStart marks the user's move
    # as successful, which the move's own result does right after; its onRestart returns at
    # once for Psychic Noise.
    '            (VolKind::Healblock, Ev::Start)',
    '            (VolKind::Healblock, Ev::Restart)',
    # These two only end the effects of held items, and no item in Champions has an onEnd.
    '            (Pseudo::Magicroom, Ev::FieldStart)',
    '            (ab::KLUTZ, Ev::Start, Pre::On)',
    # Reacts to an ally's Grass move aimed at its own side; Champions has no such move.
    '            (ab::SAPSIPPER, Ev::TryHitSide, Pre::Ally)',
    # Doubles the stat changes of berries; Champions has no berry that changes stats.
    '            (ab::RIPEN, Ev::ChangeBoost, Pre::On)',
    # Clears state that is set afresh whenever the ability starts and is never read while the
    # ability is off; Opportunist's pending boosts are always applied before it can end.
    '            (ab::OPPORTUNIST, Ev::End, Pre::On)',
    '            (ab::UNNERVE, Ev::End, Pre::On)',
}

def handler_off_entries(since=None):
    """One mutation per callback: the callback does nothing and returns nothing.

    With `since` (a git ref), only callbacks that ref does not have.
    """
    entries = []
    for f in (V, C, A, I):
        arms = callback_arms(open(os.path.join(REPO, f)).read())
        known = set()
        if since:
            known = {h for h, _ in callback_arms(sh(f'git show {since}:{f}', cwd=REPO).stdout)}
        for header, block in arms:
            if header in known or any(header.startswith(e) for e in EQUIVALENT):
                continue
            name = 'off: ' + ' '.join(header.split())[:-5].strip()
            if block:
                off = header + '\n                if std::hint::black_box(true) {\n                    return Res::Undef;\n                }'
            else:
                off = header[:header.rindex('=> ')] + '=> Res::Undef,'
            entries.append((name, f, header, off))
    return entries

def main():
    args = sys.argv[1:]
    only = []
    while '--only' in args:
        i = args.index('--only'); only.append(args[i + 1]); del args[i:i + 2]
    if '--handlers' in args:
        # Add a mutation for every callback (or, with a ref, every callback newer than it).
        i = args.index('--handlers')
        since = args[i + 1] if i + 1 < len(args) and not args[i + 1].startswith('-') and not args[i + 1].endswith('.jsonl') else None
        del args[i:i + (2 if since else 1)]
        MUT.extend(handler_off_entries(since))
    if '--handlers-only' in args:
        i = args.index('--handlers-only')
        since = args[i + 1] if i + 1 < len(args) and not args[i + 1].startswith('-') and not args[i + 1].endswith('.jsonl') else None
        del args[i:i + (2 if since else 1)]
        MUT[:] = handler_off_entries(since)
    work = WORK
    if '--work' in args:
        i = args.index('--work'); work = os.path.join(REPO, 'target', args[i + 1]); del args[i:i + 2]
    lo, hi = 0, len(MUT)
    if '--range' in args:
        i = args.index('--range'); lo, hi = (int(x) for x in args[i + 1].split(':')); del args[i:i + 2]
    if '--check' in args:
        # Only confirm that every entry still matches the source exactly once.
        bad = 0
        for name, f, old, _ in MUT:
            n = open(os.path.join(REPO, f)).read().count(old)
            if n != 1:
                bad += 1
                print(f'BAD PATTERN ({n} matches): {name}')
        print(f'{len(MUT)} entries, {bad} to repair')
        return
    corpora = [os.path.abspath(c) for c in args]
    if not corpora:
        sys.exit(__doc__)
    os.makedirs(work, exist_ok=True)
    for d in ('src', 'tests'):
        shutil.rmtree(os.path.join(work, d), ignore_errors=True)
        shutil.copytree(os.path.join(REPO, d), os.path.join(work, d), copy_function=shutil.copy)
    for f in ('Cargo.toml', 'Cargo.lock', 'rustfmt.toml'):
        shutil.copy(os.path.join(REPO, f), os.path.join(work, f))
    # (`formats` is compiled in: src/format.rs includes the regulation files.)
    for d in ('examples', 'formats'):
        shutil.rmtree(os.path.join(work, d), ignore_errors=True)
        shutil.copytree(os.path.join(REPO, d), os.path.join(work, d), copy_function=shutil.copy)
    results = []
    # baseline must pass
    r = sh('cargo build --release --bin difftest 2>&1 | tail -1', cwd=work)
    for c in corpora:
        r = sh(f'./target/release/difftest {c} --quiet', cwd=work)
        if r.returncode != 0:
            print('BASELINE FAILS on', c, r.stdout[-300:]); return
    print('baseline passes on', len(corpora), 'corpora', flush=True)
    for name, f, old, new in MUT[lo:hi]:
        if only and not any(o in name for o in only):
            continue
        path = os.path.join(work, f)
        src = open(path).read()
        n = src.count(old)
        if n != 1:
            print(f'BAD PATTERN ({n} matches): {name}', flush=True); results.append((name, 'bad')); continue
        open(path, 'w').write(src.replace(old, new))
        b = sh('cargo build --release --bin difftest 2>&1 | grep -E "^error" -A6', cwd=work)
        if b.stdout.strip():
            print(f'DOES NOT COMPILE: {name}\n{b.stdout[:400]}', flush=True); results.append((name, 'nocompile'))
        else:
            caught = None
            for c in corpora:
                r = sh(f'./target/release/difftest {c} --quiet', cwd=work)
                if r.returncode != 0:
                    line = [l for l in r.stdout.splitlines() if 'diverged' in l or 'panicked' in l]
                    caught = (os.path.basename(c), line[-1] if line else 'crash')
                    break
            if caught:
                print(f'caught   {name:<44} {caught[0]}: {caught[1][:90]}', flush=True); results.append((name, 'caught'))
            else:
                print(f'MISSED   {name}', flush=True); results.append((name, 'missed'))
        open(path, 'w').write(src)
    tally = {}
    for _, r in results: tally[r] = tally.get(r, 0) + 1
    print('summary:', tally)
    print('missed:', [n for n, r in results if r == 'missed'])

if __name__ == '__main__':
    main()
