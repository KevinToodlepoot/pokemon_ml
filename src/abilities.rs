//! Event callbacks of abilities, ported from Showdown's `data/abilities.ts`
//! with the Champions overrides.
//!
//! Each arm is one Showdown callback, named in the comment above it together
//! with its parameter list. Inside a callback `self.event` holds what Showdown
//! passes as arguments (the first Pokémon parameter is `e.target`, the second
//! `e.source`, the effect is `e.effect`, a numeric relay value is `relay`),
//! and `holder` is the Pokémon with the ability (`this.effectState.target`).
//! Callbacks that only write to Showdown's battle log are empty here.

#![allow(clippy::collapsible_if, clippy::needless_range_loop)]

use crate::battle::{div1, modify};
use crate::data::*;
use crate::state::*;

/// Moves whose type the "-ate" abilities leave alone.
const NO_MODIFY_TYPE: [&str; 7] =
    ["judgment", "multiattack", "naturalgift", "revelationdance", "technoblast", "terrainpulse", "weatherball"];

/// The volatiles Aroma Veil blocks. None is modelled yet; matching by name
/// means the ability starts working the moment one is.
const AROMA_VEIL: [&str; 6] = ["attract", "disable", "encore", "healblock", "taunt", "torment"];

/// The type a pinch ability (Blaze and friends) boosts at low HP.
fn pinch_type(ability: u16) -> Option<Type> {
    Some(match ability {
        ab::BLAZE => Type::Fire,
        ab::OVERGROW => Type::Grass,
        ab::TORRENT => Type::Water,
        ab::SWARM => Type::Bug,
        _ => return None,
    })
}

/// The type an "-ate" ability turns Normal moves into.
fn ate_type(ability: u16) -> Option<Type> {
    Some(match ability {
        ab::AERILATE => Type::Flying,
        ab::PIXILATE => Type::Fairy,
        ab::REFRIGERATE => Type::Ice,
        ab::DRAGONIZE => Type::Dragon,
        _ => return None,
    })
}

/// The status an immunity ability blocks and cures (`onSetStatus` + `onUpdate`).
fn status_guard(ability: u16) -> Option<&'static [Status]> {
    Some(match ability {
        ab::IMMUNITY => &[Status::Psn, Status::Tox],
        ab::INSOMNIA | ab::VITALSPIRIT => &[Status::Slp],
        ab::LIMBER => &[Status::Par],
        ab::THERMALEXCHANGE | ab::WATERBUBBLE => &[Status::Brn],
        _ => return None,
    })
}

/// Abilities that heal a quarter of max HP instead of being hit by one type.
fn absorb_heal(ability: u16) -> Option<Type> {
    Some(match ability {
        ab::EARTHEATER => Type::Ground,
        ab::VOLTABSORB => Type::Electric,
        ab::WATERABSORB | ab::DRYSKIN => Type::Water,
        _ => return None,
    })
}

/// Abilities that raise a stat instead of being hit by one type: (type, stat).
fn absorb_boost(ability: u16) -> Option<(Type, usize)> {
    Some(match ability {
        ab::LIGHTNINGROD => (Type::Electric, SPA),
        ab::MOTORDRIVE => (Type::Electric, SPE),
        ab::SAPSIPPER => (Type::Grass, ATK),
        _ => return None,
    })
}

/// Contact abilities that may inflict a status on the attacker.
fn contact_status(ability: u16) -> Option<Status> {
    Some(match ability {
        ab::FLAMEBODY => Status::Brn,
        ab::POISONPOINT => Status::Psn,
        ab::STATIC => Status::Par,
        _ => return None,
    })
}

/// Base-power boosts keyed on a move flag: (flag, numerator over 4096).
fn flag_boost(ability: u16) -> Option<(u32, u32)> {
    Some(match ability {
        ab::IRONFIST => (F_PUNCH, 4915),
        ab::MEGALAUNCHER => (F_PULSE, 6144),
        ab::SHARPNESS => (F_SLICING, 6144),
        ab::STRONGJAW => (F_BITE, 6144),
        ab::TOUGHCLAWS => (F_CONTACT, 5325),
        _ => return None,
    })
}

impl Battle {
    fn clear_negative(&mut self) {
        for b in self.event.boosts.iter_mut() {
            if *b < 0 {
                *b = 0;
            }
        }
    }

    /// The shared `onTryBoost` of the abilities Intimidate cannot touch.
    fn block_intimidate(&mut self) -> Res {
        if self.event.effect == Eff::Ability(ab::INTIMIDATE) && self.event.boosts[ATK] != 0 {
            self.event.boosts[ATK] = 0;
            // `-fail|target|unboost|atk|[from] ability: Inner Focus`
            self.show_own_ability();
        }
        Res::Undef
    }

    /// Showdown's `(effect as Move)?.status`: the effect is a move whose own
    /// effect is a status condition, or the stand-in Synchronize passes one on
    /// with. An ability that blocks a status only says so then.
    pub(crate) fn is_status_move(&self, effect: Eff) -> bool {
        match effect {
            Eff::Move(mi) => self.am[mi as usize].d().status != Status::None,
            Eff::Ability(a) => a == ab::SYNCHRONIZE,
            _ => false,
        }
    }

    /// Showdown's `!(effect as ActiveMove).secondaries`: anything but a move
    /// with secondary effects. An ability that blocks a stat drop only says so then.
    pub(crate) fn no_secondaries(&self, effect: Eff) -> bool {
        match effect {
            Eff::Move(mi) => !self.am[mi as usize].has_secs,
            _ => true,
        }
    }

    /// Showdown's `effect.id === 'octolock'`: the drops Octolock deals at the end of
    /// each turn, which it applies as the move.
    fn is_octolock(&self, effect: Eff) -> bool {
        matches!(effect, Eff::Move(mi) if self.am[mi as usize].id == mv::OCTOLOCK)
    }

    /// `Pokemon#getBestStat(true, true)`: index 1..=5 of the highest raw stat.
    fn best_stat(&self, r: MonRef) -> usize {
        let m = self.mon(r);
        let mut best = 1;
        let mut value = 0;
        for k in 1..=5 {
            if m.stats[k] > value {
                best = k;
                value = m.stats[k];
            }
        }
        best
    }

    /// Runs one ability callback.
    pub(crate) fn ability_cb(&mut self, ability: u16, ev: Ev, pre: Pre, holder: MonRef) -> Res {
        let e = self.event;
        let relay = e.relay;
        let mi = self.event_move();
        let mtype = mi.map(|m| self.am[m as usize].typ);
        let mflags = mi.map_or(0, |m| self.am[m as usize].flags);
        let mcat = mi.map(|m| self.am[m as usize].category);

        // ---- families of abilities that share a callback -----------------------
        if let Some(t) = pinch_type(ability) {
            // onModifyAtk / onModifySpA(atk, attacker, defender, move)
            let m = self.mon(holder);
            if mtype == Some(t) && m.hp as u32 * 3 <= m.max_hp() as u32 {
                return self.chain_modify(6144, 4096);
            }
            return Res::Undef;
        }
        if let Some(t) = ate_type(ability) {
            let Some(m) = mi else {
                return Res::Undef;
            };
            return match ev {
                // onModifyType(move, pokemon)
                Ev::ModifyType => {
                    let am = &mut self.am[m as usize];
                    if am.typ == Type::Normal && !NO_MODIFY_TYPE.contains(&am.d().id) {
                        am.typ = t;
                        am.type_changer_boosted = Eff::Ability(ability);
                    }
                    Res::Undef
                }
                // onBasePower(basePower, pokemon, target, move)
                Ev::BasePower => {
                    if self.am[m as usize].type_changer_boosted == Eff::Ability(ability) {
                        return self.chain_modify(4915, 4096);
                    }
                    Res::Undef
                }
                _ => unreachable!("no body for {} {ev:?}", ABILITIES[ability as usize].id),
            };
        }
        if let (Some(guarded), Ev::Update | Ev::SetStatus) = (status_guard(ability), ev) {
            return match ev {
                // onUpdate(pokemon)
                Ev::Update => {
                    if guarded.contains(&self.mon(holder).status) {
                        // `-activate|pokemon|ability: Limber`
                        self.show_ability(holder, ability);
                        self.cure_status(holder);
                    }
                    Res::Undef
                }
                // onSetStatus(status, target, source, effect)
                _ => {
                    if guarded.contains(&e.status) {
                        if self.is_status_move(e.effect) {
                            // `-immune|target|[from] ability: Limber`
                            self.show_ability(holder, ability);
                        }
                        return FALSE;
                    }
                    Res::Undef
                }
            };
        }
        if let (Some(t), Ev::TryHit, Pre::On) = (absorb_heal(ability), ev, pre) {
            // onTryHit(target, source, move)
            if e.target != e.source && mtype == Some(t) {
                let amount = div1(self.mon(holder).max_hp() as u32, 4);
                if !self.heal(amount, None, None, Eff::None).truthy() {
                    // `-immune|target|[from] ability: Water Absorb`
                    self.show_ability(holder, ability);
                }
                return Res::Null;
            }
            return Res::Undef;
        }
        if let (Some((t, stat)), Ev::TryHit, Pre::On) = (absorb_boost(ability), ev, pre) {
            // onTryHit(target, source, move)
            if e.target != e.source && mtype == Some(t) {
                if !self.boost1(stat, 1, None, None, Eff::None).truthy() {
                    // `-immune|target|[from] ability: Lightning Rod`
                    self.show_ability(holder, ability);
                }
                return Res::Null;
            }
            return Res::Undef;
        }
        if let (Some(status), Ev::DamagingHit) = (contact_status(ability), ev) {
            // onDamagingHit(damage, target, source, move)
            if let (Some(m), Some(source)) = (mi, e.source) {
                if self.makes_contact(m) && self.chance(3, 10, "contact ability") {
                    self.try_set_status(source, status, e.target, Eff::None);
                }
            }
            return Res::Undef;
        }
        if let (Some((flag, num)), Ev::BasePower) = (flag_boost(ability), ev) {
            // onBasePower(basePower, attacker, defender, move)
            if mflags & flag != 0 {
                return self.chain_modify(num, 4096);
            }
            return Res::Undef;
        }

        match (ability, ev, pre) {
            // ================================================= weather abilities
            // ---- Drizzle, Drought, Sand Stream, Snow Warning: onStart(source)
            (ab::DRIZZLE, Ev::Start, Pre::On) => {
                self.set_weather(Weather::Raindance, None, Eff::None);
                Res::Undef
            }
            (ab::DROUGHT, Ev::Start, Pre::On) => {
                self.set_weather(Weather::Sunnyday, None, Eff::None);
                Res::Undef
            }
            (ab::SANDSTREAM, Ev::Start, Pre::On) => {
                self.set_weather(Weather::Sandstorm, None, Eff::None);
                Res::Undef
            }
            (ab::SNOWWARNING, Ev::Start, Pre::On) => {
                self.set_weather(Weather::Snowscape, None, Eff::None);
                Res::Undef
            }
            // ---- Sand Spit: onDamagingHit(damage, target, source, move)
            (ab::SANDSPIT, Ev::DamagingHit, Pre::On) => {
                self.set_weather(Weather::Sandstorm, None, Eff::None);
                Res::Undef
            }

            // ---- Cloud Nine, Air Lock. `ability_st.a` is `abilityState.ending`:
            // set while the ability is on its way out, so that it no longer suppresses.
            // onSwitchIn(pokemon) runs onStart(pokemon)
            (ab::CLOUDNINE | ab::AIRLOCK, Ev::SwitchIn | Ev::Start, Pre::On) => {
                if ev == Ev::SwitchIn {
                    // `-ability|pokemon|Cloud Nine`
                    self.show_ability(holder, ability);
                }
                self.mon_mut(holder).ability_st.a = 0;
                self.each_event_from(Ev::WeatherChange, Eff::Ability(ability));
                Res::Undef
            }
            // onEnd(pokemon)
            (ab::CLOUDNINE | ab::AIRLOCK, Ev::End, Pre::On) => {
                self.mon_mut(holder).ability_st.a = 1;
                self.each_event_from(Ev::WeatherChange, Eff::Ability(ability));
                Res::Undef
            }

            // ---- Speed doubled in a weather: onModifySpe(spe, pokemon)
            (ab::CHLOROPHYLL, Ev::ModifySpe, Pre::On) => {
                if self.effective_weather(holder) == Weather::Sunnyday {
                    return self.chain_modify(2, 1);
                }
                Res::Undef
            }
            (ab::SWIFTSWIM, Ev::ModifySpe, Pre::On) => {
                if self.effective_weather(holder) == Weather::Raindance {
                    return self.chain_modify(2, 1);
                }
                Res::Undef
            }
            (ab::SANDRUSH, Ev::ModifySpe, Pre::On) => {
                if self.is_weather(Weather::Sandstorm) {
                    return self.chain_modify(2, 1);
                }
                Res::Undef
            }
            (ab::SLUSHRUSH, Ev::ModifySpe, Pre::On) => {
                if self.is_weather(Weather::Snowscape) {
                    return self.chain_modify(2, 1);
                }
                Res::Undef
            }
            // ---- Sandstorm immunity: onImmunity(type, pokemon)
            (ab::SANDRUSH | ab::SANDFORCE | ab::SANDVEIL, Ev::Immunity, Pre::On) => {
                if e.imm == Some(Imm::Weather(Weather::Sandstorm)) {
                    return FALSE;
                }
                Res::Undef
            }
            // ---- Ice Body, Snow Cloak: onImmunity only covers hail, which is not in Champions.
            (ab::ICEBODY | ab::SNOWCLOAK, Ev::Immunity, Pre::On) => Res::Undef,
            // ---- Sand Force: onBasePower(basePower, attacker, defender, move)
            (ab::SANDFORCE, Ev::BasePower, Pre::On) => {
                if self.is_weather(Weather::Sandstorm) && matches!(mtype, Some(Type::Rock | Type::Ground | Type::Steel))
                {
                    return self.chain_modify(5325, 4096);
                }
                Res::Undef
            }
            // ---- Sand Veil, Snow Cloak: onModifyAccuracy(accuracy)
            (ab::SANDVEIL, Ev::ModifyAccuracy, Pre::On) => {
                if matches!(relay, Res::Num(_)) && self.is_weather(Weather::Sandstorm) {
                    return self.chain_modify(3277, 4096);
                }
                Res::Undef
            }
            (ab::SNOWCLOAK, Ev::ModifyAccuracy, Pre::On) => {
                if matches!(relay, Res::Num(_)) && self.is_weather(Weather::Snowscape) {
                    return self.chain_modify(3277, 4096);
                }
                Res::Undef
            }

            // ---- Dry Skin (its onTryHit is the Water Absorb one)
            // onSourceBasePower(basePower, attacker, defender, move)
            (ab::DRYSKIN, Ev::BasePower, Pre::Source) => {
                if mtype == Some(Type::Fire) {
                    return self.chain_modify(5, 4);
                }
                Res::Undef
            }
            // onWeather(target, source, effect)
            (ab::DRYSKIN, Ev::Weather, Pre::On) => {
                let Eff::Weather(w) = e.effect else {
                    return Res::Undef;
                };
                if self.effective_weather(holder) != w {
                    return Res::Undef;
                }
                let eighth = div1(self.mon(holder).max_hp() as u32, 8);
                if w == Weather::Raindance {
                    self.heal(eighth, None, None, Eff::None);
                } else if w == Weather::Sunnyday {
                    self.damage(eighth, Some(holder), Some(holder), Eff::None);
                }
                Res::Undef
            }
            // ---- Rain Dish: onWeather(target, source, effect)
            (ab::RAINDISH, Ev::Weather, Pre::On) => {
                let Eff::Weather(w) = e.effect else {
                    return Res::Undef;
                };
                if self.effective_weather(holder) == w && w == Weather::Raindance {
                    let amount = div1(self.mon(holder).max_hp() as u32, 16);
                    self.heal(amount, None, None, Eff::None);
                }
                Res::Undef
            }
            // ---- Ice Body: onWeather(target, source, effect)
            (ab::ICEBODY, Ev::Weather, Pre::On) => {
                if e.effect == Eff::Weather(Weather::Snowscape) {
                    let amount = div1(self.mon(holder).max_hp() as u32, 16);
                    self.heal(amount, None, None, Eff::None);
                }
                Res::Undef
            }
            // ---- Solar Power
            // onModifySpA(spa, pokemon)
            (ab::SOLARPOWER, Ev::ModifySpA, Pre::On) => {
                if self.effective_weather(holder) == Weather::Sunnyday {
                    return self.chain_modify(3, 2);
                }
                Res::Undef
            }
            // onWeather(target, source, effect)
            (ab::SOLARPOWER, Ev::Weather, Pre::On) => {
                let Eff::Weather(w) = e.effect else {
                    return Res::Undef;
                };
                if self.effective_weather(holder) == w && w == Weather::Sunnyday {
                    let d = div1(self.mon(holder).max_hp() as u32, 8);
                    self.damage(d, Some(holder), Some(holder), Eff::None);
                }
                Res::Undef
            }
            // ---- Hydration: onResidual(pokemon)
            (ab::HYDRATION, Ev::Residual, Pre::On) => {
                if self.mon(holder).status != Status::None && self.effective_weather(holder) == Weather::Raindance {
                    // `-activate|pokemon|ability: Hydration`
                    self.show_ability(holder, ability);
                    self.cure_status(holder);
                }
                Res::Undef
            }
            // ---- Leaf Guard
            // onSetStatus(status, target, source, effect)
            (ab::LEAFGUARD, Ev::SetStatus, Pre::On) => {
                if self.effective_weather(holder) == Weather::Sunnyday {
                    if self.is_status_move(e.effect) {
                        self.show_ability(holder, ability);
                    }
                    return FALSE;
                }
                Res::Undef
            }
            // onTryAddVolatile(status, target): Yawn in the sun
            (ab::LEAFGUARD, Ev::TryAddVolatile, Pre::On) => {
                if e.vol.is_some_and(|v| v.id() == "yawn") && self.effective_weather(holder) == Weather::Sunnyday {
                    self.show_ability(holder, ability);
                    return Res::Null;
                }
                Res::Undef
            }
            // ---- Mega Sol: onWeatherModifyDamage(damage, attacker, defender, move).
            // It runs Sunny Day's handler as its own and returns the damage it was
            // given, which ends the event before the real weather has a say.
            (ab::MEGASOL, Ev::WeatherModifyDamage, Pre::On) => {
                self.sun_modify_damage();
                relay
            }
            // ============================================ forme changes
            // ---- Stance Change: onModifyMove(move, attacker, defender). Blade forme to attack, Shield forme behind King's Shield.
            (ab::STANCECHANGE, Ev::ModifyMove, Pre::On) => {
                let Some(m) = mi else {
                    return Res::Undef;
                };
                let mon = self.mon(holder);
                if SPECIES[mon.species as usize].base_species != "aegislash" || mon.transformed {
                    return Res::Undef;
                }
                let am = &self.am[m as usize];
                if am.category == Category::Status && am.id != mv::KINGSSHIELD {
                    return Res::Undef;
                }
                let want = if am.id == mv::KINGSSHIELD { "aegislash" } else { "aegislashblade" };
                if SPECIES[mon.species as usize].id != want {
                    if let Some(species) = species_id(want) {
                        // (Champions' `formeChange` names the ability behind a change only when it is
                        // handed one, and this is not: `-formechange|pokemon|Aegislash-Blade`.)
                        self.forme_change(holder, species, false, false);
                    }
                }
                Res::Undef
            }
            // ---- Hunger Switch: onResidual(pokemon). Morpeko changes mode at the end of every turn.
            (ab::HUNGERSWITCH, Ev::Residual, Pre::On) => {
                let species = &SPECIES[self.mon(holder).species as usize];
                if species.base_species != "morpeko" {
                    return Res::Undef;
                }
                let want = if species.id == "morpeko" { "morpekohangry" } else { "morpeko" };
                if let Some(species) = species_id(want) {
                    self.forme_change(holder, species, false, false);
                }
                Res::Undef
            }
            // ---- Disguise. `ability_st.a` is `abilityState.busted`.
            // onDamage(damage, target, source, effect): the first hit from a move does nothing.
            (ab::DISGUISE, Ev::Damage, Pre::On) => {
                if e.effect.is_move() && SPECIES[self.mon(holder).species as usize].id == "mimikyu" {
                    // `-activate|target|ability: Disguise`
                    self.show_ability(holder, ability);
                    self.mon_mut(holder).ability_st.a = 1;
                    return Res::Num(0);
                }
                Res::Undef
            }
            // onCriticalHit(target, source, move) and onEffectiveness(typeMod, target, type, move):
            // the hit the disguise will take is neither critical nor more or less effective.
            (ab::DISGUISE, Ev::CriticalHit | Ev::Effectiveness, Pre::On) => {
                let (Some(m), Some(target)) = (mi, e.target) else {
                    return Res::Undef;
                };
                if ev == Ev::Effectiveness && self.am[m as usize].category == Category::Status {
                    return Res::Undef;
                }
                if SPECIES[self.mon(target).species as usize].id != "mimikyu" {
                    return Res::Undef;
                }
                let am = &self.am[m as usize];
                let hit_sub =
                    self.vols(target).has(VolKind::Substitute) && am.flags & F_BYPASSSUB == 0 && !am.infiltrates;
                if hit_sub || !self.run_immunity(target, m) {
                    return Res::Undef;
                }
                if ev == Ev::CriticalHit { FALSE } else { Res::Num(0) }
            }
            // onUpdate(pokemon): the disguise is gone for good, at the cost of an eighth of its HP.
            (ab::DISGUISE, Ev::Update, Pre::On) => {
                if SPECIES[self.mon(holder).species as usize].id == "mimikyu" && self.mon(holder).ability_st.a != 0 {
                    if let Some(species) = species_id("mimikyubusted") {
                        self.forme_change(holder, species, true, false);
                        let d = div1(self.mon(holder).max_hp() as u32, 8);
                        self.damage(d, Some(holder), Some(holder), Eff::Species);
                    }
                }
                Res::Undef
            }
            // ---- Zero to Hero: onSwitchOut(pokemon). Palafin comes back as a hero.
            (ab::ZEROTOHERO, Ev::SwitchOut, Pre::On) => {
                let mon = self.mon(holder);
                if SPECIES[mon.base_species as usize].base_species == "palafin"
                    && SPECIES[mon.species as usize].id != "palafinhero"
                {
                    if let Some(species) = species_id("palafinhero") {
                        self.forme_change(holder, species, true, true);
                    }
                }
                Res::Undef
            }
            // onSwitchIn(pokemon): `-activate|pokemon|ability: Zero to Hero`, the first time it
            // comes in as a hero. (Once said, the ability is known, so saying it again changes nothing.)
            (ab::ZEROTOHERO, Ev::SwitchIn, Pre::On) => {
                let mon = self.mon(holder);
                if SPECIES[mon.base_species as usize].base_species == "palafin"
                    && SPECIES[mon.species as usize].id == "palafinhero"
                {
                    self.show_ability(holder, ability);
                }
                Res::Undef
            }
            // ---- Abilities of species that are not in Champions (Eiscue, Cramorant,
            // Minior, Greninja's bonded forme). Every callback first checks for that
            // species, so on anything else they do nothing at all.
            (
                ab::ICEFACE,
                Ev::Start | Ev::Damage | Ev::CriticalHit | Ev::Effectiveness | Ev::Update | Ev::WeatherChange,
                Pre::On,
            )
            | (ab::GULPMISSILE, Ev::DamagingHit, Pre::On)
            | (ab::GULPMISSILE, Ev::TryPrimaryHit, Pre::Source)
            | (ab::SHIELDSDOWN, Ev::Start | Ev::Residual | Ev::SetStatus | Ev::TryAddVolatile, Pre::On)
            | (ab::BATTLEBOND, Ev::AfterFaint, Pre::Source)
            | (ab::BATTLEBOND, Ev::ModifyMove, Pre::On) => Res::Undef,

            // ============================================ Illusion, Imposter
            // ---- Illusion. `Pokemon::illusion` is who the holder looks like.
            // onBeforeSwitchIn(pokemon): the last Pokémon in the party that can still fight.
            (ab::ILLUSION, Ev::BeforeSwitchIn, Pre::On) => {
                let s = &self.sides[holder.side as usize];
                let own = self.mon(holder).position as usize;
                let mut looks_like = 0;
                for p in (own + 1..s.n as usize).rev() {
                    if !s.team[s.order[p] as usize].fainted {
                        looks_like = s.order[p] + 1;
                        break;
                    }
                }
                self.mon_mut(holder).illusion = looks_like;
                Res::Undef
            }
            // onDamagingHit(damage, target, source, move): a damaging hit ends it.
            (ab::ILLUSION, Ev::DamagingHit, Pre::On) => {
                if self.mon(holder).illusion != 0 {
                    let me = e.effect;
                    self.single_event(
                        Ev::End,
                        Eff::Ability(ab::ILLUSION),
                        Some(holder),
                        Some(holder),
                        e.source,
                        me,
                        Res::Undef,
                    );
                }
                Res::Undef
            }
            // onEnd(pokemon): not while it is being called back, so that it leaves disguised.
            (ab::ILLUSION, Ev::End, Pre::On) => {
                if !self.mon(holder).being_called_back && self.mon(holder).illusion != 0 {
                    let was = self.shown_as(holder);
                    self.mon_mut(holder).illusion = 0;
                    // `replace`, then `-end|pokemon|Illusion`.
                    self.shown_unmask(holder, was);
                    self.show_illusion_ended(holder);
                }
                Res::Undef
            }
            // onFaint(pokemon)
            (ab::ILLUSION, Ev::Faint, Pre::On) => {
                self.mon_mut(holder).illusion = 0;
                Res::Undef
            }
            // ---- Imposter: onSwitchIn(pokemon). Transforms into the foe straight across.
            (ab::IMPOSTER, Ev::SwitchIn, Pre::On) => {
                let across = ACTIVE - 1 - self.mon(holder).position as usize;
                let target = self.active(1 - holder.side as usize, across);
                if self.in_play(target) {
                    self.transform_into(holder, target);
                }
                Res::Undef
            }

            // ---- Forecast
            // onStart(pokemon)
            (ab::FORECAST, Ev::Start, Pre::On) => {
                let ev = Event::new(Ev::WeatherChange, Some(holder), None, Eff::None);
                self.single_event_ex(
                    Ev::WeatherChange,
                    Ev::WeatherChange,
                    Pre::On,
                    Eff::Ability(ability),
                    Some(Holder::Mon(holder)),
                    ev,
                    Res::Undef,
                    false,
                );
                Res::Undef
            }
            // onWeatherChange(pokemon)
            (ab::FORECAST, Ev::WeatherChange, Pre::On) => {
                let m = self.mon(holder);
                if SPECIES[m.base_species as usize].base_species != "castform" {
                    return Res::Undef;
                }
                let want = match self.effective_weather(holder) {
                    Weather::Sunnyday => "castformsunny",
                    Weather::Raindance => "castformrainy",
                    Weather::Snowscape => "castformsnowy",
                    _ => "castform",
                };
                if self.mon(holder).is_active && SPECIES[self.mon(holder).species as usize].id != want {
                    if let Some(forme) = species_id(want) {
                        // A temporary forme change: types and stats, not the ability.
                        self.set_species(holder, forme);
                        // `-formechange|pokemon|Castform-Sunny|[msg]|[from] ability: Forecast`
                        self.show_ability(holder, ability);
                    }
                }
                Res::Undef
            }

            // ================================================= terrain abilities
            // ---- Electric / Grassy / Psychic Surge: onStart(source)
            (ab::ELECTRICSURGE, Ev::Start, Pre::On) => {
                self.set_terrain(Terrain::Electricterrain, None, Eff::None);
                Res::Undef
            }
            (ab::GRASSYSURGE, Ev::Start, Pre::On) => {
                self.set_terrain(Terrain::Grassyterrain, None, Eff::None);
                Res::Undef
            }
            (ab::PSYCHICSURGE, Ev::Start, Pre::On) => {
                self.set_terrain(Terrain::Psychicterrain, None, Eff::None);
                Res::Undef
            }
            // ---- Seed Sower: onDamagingHit(damage, target, source, move)
            (ab::SEEDSOWER, Ev::DamagingHit, Pre::On) => {
                self.set_terrain(Terrain::Grassyterrain, None, Eff::None);
                Res::Undef
            }
            // ---- Grass Pelt: onModifyDef(pokemon)
            (ab::GRASSPELT, Ev::ModifyDef, Pre::On) => {
                if self.is_terrain(Terrain::Grassyterrain) {
                    return self.chain_modify(3, 2);
                }
                Res::Undef
            }
            // ---- Surge Surfer: onModifySpe(spe)
            (ab::SURGESURFER, Ev::ModifySpe, Pre::On) => {
                if self.is_terrain(Terrain::Electricterrain) {
                    return self.chain_modify(2, 1);
                }
                Res::Undef
            }
            // ---- Mimicry
            // onStart(pokemon)
            (ab::MIMICRY, Ev::Start, Pre::On) => {
                let ev = Event::new(Ev::TerrainChange, Some(holder), None, Eff::None);
                self.single_event_ex(
                    Ev::TerrainChange,
                    Ev::TerrainChange,
                    Pre::On,
                    Eff::Ability(ability),
                    Some(Holder::Mon(holder)),
                    ev,
                    Res::Undef,
                    false,
                );
                Res::Undef
            }
            // onTerrainChange(pokemon): take the terrain's type, or the species' own without one.
            (ab::MIMICRY, Ev::TerrainChange, Pre::On) => {
                let types = match self.field.terrain.kind {
                    Terrain::Electricterrain => [Type::Electric, Type::None],
                    Terrain::Grassyterrain => [Type::Grass, Type::None],
                    Terrain::Mistyterrain => [Type::Fairy, Type::None],
                    Terrain::Psychicterrain => [Type::Psychic, Type::None],
                    Terrain::None => SPECIES[self.mon(holder).base_species as usize].types,
                };
                // `oldTypes.join() === types.join()`: the types that count now, in order.
                let (current, n) = self.get_types(holder, false);
                let wanted: Vec<Type> = types.iter().copied().filter(|&t| t != Type::None).collect();
                if current[..n] != wanted[..] && self.set_type(holder, types) {
                    // `-start|pokemon|typechange|…|[from] ability: Mimicry`, or `-activate|pokemon|ability: Mimicry`.
                    self.show_ability(holder, ability);
                }
                Res::Undef
            }

            // ---- Adaptability: onModifySTAB(stab, source, target, move)
            (ab::ADAPTABILITY, Ev::ModifySTAB, Pre::On) => {
                if let (Some(t), Some(source)) = (mtype, e.target) {
                    if self.has_type(source, t) {
                        return Res::Num(if relay == Res::Num(8192) { 9216 } else { 8192 });
                    }
                }
                Res::Undef
            }

            // ---- Aftermath: onDamagingHit(damage, target, source, move)
            (ab::AFTERMATH, Ev::DamagingHit, Pre::On) => {
                if let (Some(m), Some(target), Some(source)) = (mi, e.target, e.source) {
                    if self.mon(target).hp == 0 && self.makes_contact(m) {
                        let d = div1(self.mon(source).max_hp() as u32, 4);
                        self.damage(d, Some(source), Some(target), Eff::None);
                    }
                }
                Res::Undef
            }

            // ---- Analytic: onBasePower(basePower, pokemon)
            (ab::ANALYTIC, Ev::BasePower, Pre::On) => {
                let (actives, n) = self.all_active(false);
                let boosted = !actives[..n].iter().any(|&t| Some(t) != e.target && self.will_move(t));
                if boosted {
                    return self.chain_modify(5325, 4096);
                }
                Res::Undef
            }

            // ---- Anger Point: onHit(target, source, move)
            (ab::ANGERPOINT, Ev::Hit, Pre::On) => {
                if let (Some(m), Some(target)) = (mi, e.target) {
                    if self.mon(target).hp > 0 && self.am[m as usize].hit_data[self.slot_index(target)].crit {
                        self.boost1(ATK, 12, Some(target), Some(target), Eff::None);
                    }
                }
                Res::Undef
            }

            // ---- Pressure, Mold Breaker: onStart(pokemon) announces the ability.
            (ab::PRESSURE | ab::MOLDBREAKER, Ev::Start, Pre::On) => {
                self.show_ability(holder, ability);
                Res::Undef
            }
            // ---- Fairy Aura: onStart(pokemon) announces it, unless it is being suppressed.
            (ab::FAIRYAURA, Ev::Start, Pre::On) => {
                if !self.suppressing_ability(Some(holder)) {
                    self.show_ability(holder, ability);
                }
                Res::Undef
            }
            // ---- Anticipation: onStart(pokemon). It shudders if a foe has a move that
            // would be super effective, or a one-hit knockout.
            (ab::ANTICIPATION, Ev::Start, Pre::On) => {
                let (foes, n) = self.allies_and_self(1 - holder.side as usize);
                for &f in &foes[..n] {
                    let m = *self.mon(f);
                    for slot in &m.moves[..m.n_moves as usize] {
                        let d = &MOVES[slot.id as usize];
                        if d.category == Category::Status {
                            continue;
                        }
                        // `dex.getImmunity` and `dex.getEffectiveness` go by the types alone.
                        let (types, k) = self.get_types(holder, false);
                        let chart = |t: Type| TYPE_CHART[d.typ as usize][t as usize];
                        let immune = types[..k].iter().any(|&t| t != Type::Typeless && chart(t) == 3);
                        let effectiveness: i32 = types[..k]
                            .iter()
                            .map(|&t| if t == Type::Typeless { 0 } else { crate::battle::type_effectiveness(d.typ, t) })
                            .sum();
                        if (!immune && effectiveness > 0) || d.ohko != Ohko::No {
                            self.show_ability(holder, ability);
                            return Res::Undef;
                        }
                    }
                }
                Res::Undef
            }
            // ---- Frisk: onStart(pokemon). `-item|target|Item|[from] ability: Frisk|[of] pokemon` for every foe holding something.
            (ab::FRISK, Ev::Start, Pre::On) => {
                let (foes, n) = self.allies_and_self(1 - holder.side as usize);
                for &f in &foes[..n] {
                    let item = self.mon(f).item;
                    if item != it::NONE {
                        self.show_item_held(f, item);
                        self.show_ability(holder, ability);
                    }
                }
                Res::Undef
            }
            // ---- Screen Cleaner: onStart(pokemon). Both sides lose their screens.
            (ab::SCREENCLEANER, Ev::Start, Pre::On) => {
                for cond in [SideCond::Reflect, SideCond::Lightscreen, SideCond::Auroraveil] {
                    for side in [holder.side as usize, 1 - holder.side as usize] {
                        if self.remove_side_condition(side, cond) {
                            // `-activate|pokemon|ability: Screen Cleaner`
                            self.show_ability(holder, ability);
                        }
                    }
                }
                Res::Undef
            }
            // ---- Toxic Debris: onDamagingHit(damage, target, source, move).
            // A physical hit scatters Toxic Spikes on the attacker's side.
            (ab::TOXICDEBRIS, Ev::DamagingHit, Pre::On) => {
                let (Some(target), Some(source)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                let side = if self.is_ally(source, target) { 1 - source.side as usize } else { source.side as usize };
                let layers = self.sides[side].conds.get(SideCond::Toxicspikes).map(|c| c.data);
                if mcat == Some(Category::Physical) && layers.is_none_or(|l| l < 2) {
                    // `-activate|target|ability: Toxic Debris`
                    self.show_ability(holder, ability);
                    self.add_side_condition(side, SideCond::Toxicspikes, Some(target), Eff::None);
                }
                Res::Undef
            }
            // ---- Embody Aspect: onStart only acts on a Terastallized Ogerpon; Champions has no Terastallization.
            (
                ab::EMBODYASPECTCORNERSTONE
                | ab::EMBODYASPECTHEARTHFLAME
                | ab::EMBODYASPECTTEAL
                | ab::EMBODYASPECTWELLSPRING,
                Ev::Start,
                Pre::On,
            ) => Res::Undef,
            // ---- Gluttony only matters for berries that are not in Champions.
            (ab::GLUTTONY, Ev::Start | Ev::Damage, Pre::On) => Res::Undef,
            // ---- Heavy Metal / Light Metal: onModifyWeight(weighthg)
            (ab::HEAVYMETAL, Ev::ModifyWeight, Pre::On) => Res::Num(relay.num() * 2),
            (ab::LIGHTMETAL, Ev::ModifyWeight, Pre::On) => Res::Num(relay.num() / 2),
            // ---- Emergency Exit, Wimp Out: onEmergencyExit(originalHp, target). Leaves when brought to half.
            (ab::EMERGENCYEXIT | ab::WIMPOUT, Ev::EmergencyExit, Pre::On) => {
                let Some(target) = e.target else {
                    return Res::Undef;
                };
                let m = self.mon(target);
                let (hp, max, original) = (m.hp as i32, m.max_hp() as i32, relay.num());
                if hp == 0 || 2 * hp > max || 2 * original <= max {
                    return Res::Undef;
                }
                if !self.can_switch(target.side as usize) || m.force_switch_flag || m.switch_flag {
                    return Res::Undef;
                }
                let m = self.mon_mut(target);
                m.switch_flag = true;
                m.switch_move = NO_MOVE;
                // `-activate|target|ability: Emergency Exit`
                self.show_ability(holder, ability);
                Res::Undef
            }

            // ---- Suction Cups: onDragOut. `-activate|pokemon|ability: Suction Cups`
            (ab::SUCTIONCUPS, Ev::DragOut, Pre::On) => {
                self.show_ability(holder, ability);
                Res::Null
            }

            // ---- Armor Tail / Queenly Majesty: onFoeTryMove(target, source, move).
            // Here `target` is the Pokémon using the move and `source` its target.
            (ab::ARMORTAIL | ab::QUEENLYMAJESTY, Ev::TryMove, Pre::Foe) => {
                let (Some(m), Some(aimed_at)) = (mi, e.source) else {
                    return Res::Undef;
                };
                let am = &self.am[m as usize];
                // Moves aimed at the foe's side or at the whole field are let through
                // (Perish Song being the exception that is stopped).
                let all = am.target == Target::All;
                if am.target == Target::FoeSide
                    || (all && !matches!(am.d().id, "perishsong" | "flowershield" | "rototiller"))
                {
                    return Res::Undef;
                }
                if (self.is_ally(aimed_at, holder) || all) && am.priority > 0 {
                    // `cant|holder|ability: Armor Tail|Move|[of] user`
                    self.show_ability(holder, ability);
                    return FALSE;
                }
                Res::Undef
            }

            // ---- Aroma Veil: onAllyTryAddVolatile(status, target, source, effect)
            (ab::AROMAVEIL, Ev::TryAddVolatile, Pre::Ally) => {
                if e.vol.is_some_and(|v| AROMA_VEIL.contains(&v.id())) {
                    if matches!(e.effect, Eff::Move(_)) {
                        // `-block|target|ability: Aroma Veil|[of] holder`
                        self.show_ability(holder, ability);
                    }
                    return Res::Null;
                }
                Res::Undef
            }

            // ---- Aura Guard: onSourceModifyDamage(damage, source, target, move)
            (ab::AURAGUARD, Ev::ModifyDamage, Pre::Source) => {
                if mflags & F_CONTACT != 0 {
                    return self.chain_modify(2048, 4096);
                }
                Res::Undef
            }

            // ---- Battle Armor / Shell Armor: onCriticalHit: false
            (ab::BATTLEARMOR | ab::SHELLARMOR, Ev::CriticalHit, Pre::On) => FALSE,

            // ---- Berserk (Champions). `ability_st.a` is `checkedBerserk`: 0 unset, 1 true, 2 false.
            // onDamage(damage, target, source, effect)
            (ab::BERSERK, Ev::Damage, Pre::On) => {
                let single_hit_move = match e.effect {
                    Eff::Move(m) => self.am[m as usize].multihit == (0, 0),
                    Eff::Confused => true,
                    _ => false,
                };
                self.mon_mut(holder).ability_st.a = if single_hit_move { 2 } else { 1 };
                Res::Undef
            }
            // onTryEatItem(item)
            (ab::BERSERK, Ev::TryEatItem, Pre::On) => {
                if e.item == it::SITRUSBERRY || e.item == it::ORANBERRY {
                    return match self.mon(holder).ability_st.a {
                        1 => TRUE,
                        2 => FALSE,
                        _ => Res::Undef,
                    };
                }
                TRUE
            }
            // onAfterMoveSecondary(target, source, move)
            (ab::BERSERK, Ev::AfterMoveSecondary, Pre::On) => {
                self.mon_mut(holder).ability_st.a = 1;
                let (Some(m), Some(target), Some(source)) = (mi, e.target, e.source) else {
                    return Res::Undef;
                };
                let am = &self.am[m as usize];
                let t = self.mon(target);
                if source == target || t.hp == 0 || am.total_damage == 0 || !t.was_attacked {
                    return Res::Undef;
                }
                let damage =
                    if am.multihit != (0, 0) && !am.smart_target { am.total_damage } else { t.last_attack_damage };
                let (hp, max) = (t.hp as i32, t.max_hp() as i32);
                if hp * 2 <= max && (hp + damage) * 2 > max {
                    self.boost1(SPA, 1, Some(target), Some(target), Eff::None);
                }
                Res::Undef
            }

            // ---- Big Pecks / Hyper Cutter / Keen Eye / Illuminate: onTryBoost(boost, target, source, effect)
            (ab::BIGPECKS | ab::HYPERCUTTER | ab::KEENEYE | ab::ILLUMINATE, Ev::TryBoost, Pre::On) => {
                if e.source.is_some() && e.target == e.source {
                    return Res::Undef;
                }
                let stat = match ability {
                    ab::BIGPECKS => DEF,
                    ab::HYPERCUTTER => ATK,
                    _ => ACC,
                };
                if self.event.boosts[stat] < 0 {
                    self.event.boosts[stat] = 0;
                    // `-fail|target|unboost|def|[from] ability: Big Pecks`, unless the drop was a move's side effect.
                    // Nor does Big Pecks say anything about Octolock's drops at the end of a turn.
                    let octolock = ability == ab::BIGPECKS && self.is_octolock(e.effect);
                    if self.no_secondaries(e.effect) && !octolock {
                        self.show_ability(holder, ability);
                    }
                }
                Res::Undef
            }
            // ---- Keen Eye / Illuminate: onModifyMove(move)
            (ab::KEENEYE | ab::ILLUMINATE, Ev::ModifyMove, Pre::On) => {
                if let Some(m) = mi {
                    self.am[m as usize].ignore_evasion = true;
                }
                Res::Undef
            }

            // ---- Bulletproof: onTryHit(pokemon, target, move)
            (ab::BULLETPROOF, Ev::TryHit, Pre::On) => {
                if mflags & F_BULLET != 0 {
                    // `-immune|pokemon|[from] ability: Bulletproof`
                    self.show_ability(holder, ability);
                    return Res::Null;
                }
                Res::Undef
            }

            // ---- Cheek Pouch: onEatItem(item, pokemon)
            (ab::CHEEKPOUCH, Ev::EatItem, Pre::On) => {
                let amount = div1(self.mon(holder).max_hp() as u32, 3);
                self.heal(amount, None, None, Eff::None);
                Res::Undef
            }

            // ---- Clear Body / White Smoke: onTryBoost(boost, target, source, effect)
            (ab::CLEARBODY | ab::WHITESMOKE, Ev::TryBoost, Pre::On) => {
                if e.source.is_some() && e.target == e.source {
                    return Res::Undef;
                }
                // `-fail|target|unboost|[from] ability: Clear Body`, unless the drop was a move's
                // side effect or Octolock's at the end of a turn.
                if e.boosts.iter().any(|&b| b < 0) && self.no_secondaries(e.effect) && !self.is_octolock(e.effect) {
                    self.show_ability(holder, ability);
                }
                self.clear_negative();
                Res::Undef
            }

            // ---- Competitive / Defiant: onAfterEachBoost(boost, target, source, effect)
            (ab::COMPETITIVE | ab::DEFIANT, Ev::AfterEachBoost, Pre::On) => {
                let (Some(target), Some(source)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                if self.is_ally(target, source) {
                    return Res::Undef;
                }
                if e.boosts.iter().any(|&b| b < 0) {
                    let stat = if ability == ab::DEFIANT { ATK } else { SPA };
                    self.boost1(stat, 2, Some(target), Some(target), Eff::None);
                }
                Res::Undef
            }

            // ---- Compound Eyes: onSourceModifyAccuracy(accuracy)
            (ab::COMPOUNDEYES, Ev::ModifyAccuracy, Pre::Source) => {
                if matches!(relay, Res::Num(_)) {
                    return self.chain_modify(5325, 4096);
                }
                Res::Undef
            }

            // ---- Contrary: onChangeBoost(boost, target, source, effect)
            // ---- Simple: onChangeBoost(boost, target, source, effect). Every stat change is doubled.
            (ab::SIMPLE, Ev::ChangeBoost, Pre::On) => {
                for b in self.event.boosts.iter_mut() {
                    *b = b.saturating_mul(2);
                }
                Res::Undef
            }
            (ab::CONTRARY, Ev::ChangeBoost, Pre::On) => {
                for b in self.event.boosts.iter_mut() {
                    *b = -*b;
                }
                Res::Undef
            }

            // ---- Cud Chew. `ability_st.a` is the berry to eat again, `.b` the turns left.
            // onEatItem(item, pokemon, source, effect)
            (ab::CUDCHEW, Ev::EatItem, Pre::On) => {
                // (Not a berry the holder took from someone with Bug Bite or Pluck.)
                let stolen =
                    matches!(e.effect, Eff::Move(m) if matches!(self.am[m as usize].id, mv::BUGBITE | mv::PLUCK));
                if ITEMS[e.item as usize].flags & IF_BERRY != 0 && !stolen {
                    let last_action = self.queue.peek().is_none();
                    let st = &mut self.mon_mut(holder).ability_st;
                    st.a = e.item as i16;
                    st.b = if last_action { 1 } else { 2 };
                }
                Res::Undef
            }
            // onResidual(pokemon)
            (ab::CUDCHEW, Ev::Residual, Pre::On) => {
                let st = self.mon(holder).ability_st;
                if st.a == 0 || self.mon(holder).hp == 0 {
                    return Res::Undef;
                }
                self.mon_mut(holder).ability_st.b -= 1;
                if self.mon(holder).ability_st.b <= 0 {
                    let item = st.a as u16;
                    // `-activate|pokemon|ability: Cud Chew`. (The `-enditem|pokemon|Berry|[eat]` after
                    // it is the berry it ate before, coming up again: nothing it holds now.)
                    self.show_ability(holder, ability);
                    if self
                        .single_event(Ev::Eat, Eff::Item(item), None, Some(holder), None, Eff::None, Res::Undef)
                        .truthy()
                    {
                        let mut ee = Event::new(Ev::EatItem, Some(holder), None, Eff::None);
                        ee.item = item;
                        self.run_event_ex(ee, Res::Undef, false, false);
                    }
                    let m = self.mon_mut(holder);
                    m.ate_berry = true;
                    m.ability_st.a = 0;
                    m.ability_st.b = 0;
                }
                Res::Undef
            }

            // ---- Curious Medicine: onStart(pokemon)
            (ab::CURIOUSMEDICINE, Ev::Start, Pre::On) => {
                let (allies, n) = self.adjacent_allies(holder);
                for &a in &allies[..n] {
                    self.mon_mut(a).boosts = [0; 7];
                    // `-clearboost|ally|[from] ability: Curious Medicine|[of] pokemon`
                    self.show_ability(holder, ability);
                }
                Res::Undef
            }

            // ---- Damp
            // onAnyTryMove(target, source, effect): nobody blows up.
            (ab::DAMP, Ev::TryMove, Pre::Any) => {
                if mi.is_some_and(|m| {
                    matches!(self.am[m as usize].d().id, "explosion" | "mindblown" | "mistyexplosion" | "selfdestruct")
                }) {
                    // `cant|holder|ability: Damp|Move|[of] user`
                    self.show_ability(holder, ability);
                    return FALSE;
                }
                Res::Undef
            }
            // onAnyDamage(damage, target, source, effect)
            (ab::DAMP, Ev::Damage, Pre::Any) => {
                if e.effect == Eff::Ability(ab::AFTERMATH) {
                    return FALSE;
                }
                Res::Undef
            }

            // ---- Eelevate: onSourceAfterFaint(length, target, source, effect)
            (ab::EELEVATE, Ev::AfterFaint, Pre::Source) => {
                if let (true, Some(source)) = (e.effect.is_move(), e.source) {
                    let stat = self.best_stat(source) - 1;
                    self.boost1(stat, relay.num() as i8, Some(source), None, Eff::None);
                }
                Res::Undef
            }

            // ---- Cursed Body: onDamagingHit(damage, target, source, move)
            (ab::CURSEDBODY, Ev::DamagingHit, Pre::On) => {
                let (Some(m), Some(source)) = (mi, e.source) else {
                    return Res::Undef;
                };
                if self.has_vol_named(source, "disable") {
                    return Res::Undef;
                }
                let am = &self.am[m as usize];
                if am.flags & F_FUTUREMOVE == 0
                    && am.id != mv::STRUGGLE
                    && self.chance(3, 10, "cursed body")
                    && self.add_volatile(source, VolKind::Disable, Some(holder), Eff::None).truthy()
                {
                    // `-start|source|Disable|Move|[from] ability: Cursed Body|[of] holder`
                    self.show_ability(holder, ability);
                }
                Res::Undef
            }

            // ---- Cute Charm: onDamagingHit(damage, target, source, move)
            (ab::CUTECHARM, Ev::DamagingHit, Pre::On) => {
                let (Some(m), Some(source)) = (mi, e.source) else {
                    return Res::Undef;
                };
                if self.makes_contact(m)
                    && self.chance(3, 10, "cute charm")
                    && self.add_volatile(source, VolKind::Attract, Some(holder), Eff::None).truthy()
                {
                    // `-start|source|Attract|[from] ability: Cute Charm|[of] holder`
                    self.show_ability(holder, ability);
                }
                Res::Undef
            }

            // ---- Electromorphosis: onDamagingHit(damage, target, source, move)
            (ab::ELECTROMORPHOSIS, Ev::DamagingHit, Pre::On) => {
                if self.add_volatile(holder, VolKind::Charge, None, Eff::None).truthy() {
                    // `-start|pokemon|Charge|Move|[from] ability: Electromorphosis`
                    self.show_ability(holder, ability);
                }
                Res::Undef
            }

            // ---- Effect Spore: onDamagingHit(damage, target, source, move)
            (ab::EFFECTSPORE, Ev::DamagingHit, Pre::On) => {
                let (Some(m), Some(source)) = (mi, e.source) else {
                    return Res::Undef;
                };
                if self.makes_contact(m) && self.run_status_immunity(source, Imm::Powder) {
                    let r = self.rand(100, "effect spore");
                    let status = if r < 11 {
                        Status::Slp
                    } else if r < 21 {
                        Status::Par
                    } else if r < 30 {
                        Status::Psn
                    } else {
                        Status::None
                    };
                    if status != Status::None {
                        self.try_set_status(source, status, e.target, Eff::None);
                    }
                }
                Res::Undef
            }

            // ---- Fairy Aura: onAnyBasePower(basePower, source, target, move)
            (ab::FAIRYAURA, Ev::BasePower, Pre::Any) => {
                let Some(m) = mi else {
                    return Res::Undef;
                };
                if e.target == e.source || mcat == Some(Category::Status) || mtype != Some(Type::Fairy) {
                    return Res::Undef;
                }
                let booster = self.am[m as usize].aura_booster;
                if !booster.is_some_and(|b| self.has_ability(b, ab::FAIRYAURA)) {
                    self.am[m as usize].aura_booster = Some(holder);
                }
                if self.am[m as usize].aura_booster != Some(holder) {
                    return Res::Undef;
                }
                self.chain_modify(5448, 4096)
            }

            // ---- Filter / Solid Rock: onSourceModifyDamage(damage, source, target, move)
            (ab::FILTER | ab::SOLIDROCK, Ev::ModifyDamage, Pre::Source) => {
                if let (Some(m), Some(target)) = (mi, e.source) {
                    if self.am[m as usize].hit_data[self.slot_index(target)].type_mod > 0 {
                        return self.chain_modify(3072, 4096);
                    }
                }
                Res::Undef
            }

            // ---- Fire Mane: onModifyAtk / onModifySpA(atk, attacker, defender, move)
            (ab::FIREMANE, Ev::ModifyAtk | Ev::ModifySpA, Pre::On) => {
                if mtype == Some(Type::Fire) {
                    return self.chain_modify(6144, 4096);
                }
                Res::Undef
            }

            // ---- Flash Fire
            // onTryHit(target, source, move)
            (ab::FLASHFIRE, Ev::TryHit, Pre::On) => {
                if let (Some(m), true) = (mi, e.target != e.source && mtype == Some(Type::Fire)) {
                    self.am[m as usize].accuracy = 0;
                    self.add_volatile(holder, VolKind::Flashfire, None, Eff::None);
                    // `-start|target|ability: Flash Fire`, or `-immune|target|[from] ability: Flash Fire` if it already burns.
                    self.show_ability(holder, ability);
                    return Res::Null;
                }
                Res::Undef
            }
            // onEnd(pokemon)
            (ab::FLASHFIRE, Ev::End, Pre::On) => {
                self.remove_volatile(holder, VolKind::Flashfire);
                Res::Undef
            }

            // ---- Flower Veil
            // onAllyTryBoost(boost, target, source, effect)
            (ab::FLOWERVEIL, Ev::TryBoost, Pre::Ally) => {
                let Some(target) = e.target else {
                    return Res::Undef;
                };
                if (e.source.is_some() && e.target == e.source) || !self.has_type(target, Type::Grass) {
                    return Res::Undef;
                }
                if e.boosts.iter().any(|&b| b < 0) && self.no_secondaries(e.effect) {
                    // `-block|target|ability: Flower Veil|[of] holder`
                    self.show_ability(holder, ability);
                }
                self.clear_negative();
                Res::Undef
            }
            // onAllySetStatus(status, target, source, effect)
            (ab::FLOWERVEIL, Ev::SetStatus, Pre::Ally) => {
                let Some(target) = e.target else {
                    return Res::Undef;
                };
                // (Sleep from a Yawn that got through is not its business.)
                if self.has_type(target, Type::Grass)
                    && e.source.is_some()
                    && e.target != e.source
                    && e.effect != Eff::None
                    && e.effect != Eff::Vol(VolKind::Yawn)
                {
                    let said = match e.effect {
                        Eff::Ability(a) => a == ab::SYNCHRONIZE,
                        Eff::Move(m) => !self.am[m as usize].has_secs,
                        _ => false,
                    };
                    if said {
                        self.show_ability(holder, ability);
                    }
                    return Res::Null;
                }
                Res::Undef
            }
            // onAllyTryAddVolatile(status, target): blocks Yawn (Flower Veil only for Grass types).
            (ab::FLOWERVEIL | ab::SWEETVEIL, Ev::TryAddVolatile, Pre::Ally) => {
                let grass_only = ability == ab::FLOWERVEIL;
                if e.vol == Some(VolKind::Yawn)
                    && (!grass_only || e.target.is_some_and(|t| self.has_type(t, Type::Grass)))
                {
                    // `-block|target|ability: Sweet Veil|[of] holder`
                    self.show_ability(holder, ability);
                    return Res::Null;
                }
                Res::Undef
            }

            // ---- Fluffy: onSourceModifyDamage(damage, source, target, move)
            (ab::FLUFFY, Ev::ModifyDamage, Pre::Source) => {
                let mut num = 4096;
                if mtype == Some(Type::Fire) {
                    num *= 2;
                }
                if mflags & F_CONTACT != 0 {
                    num /= 2;
                }
                self.chain_modify(num, 4096)
            }

            // ---- Forewarn: onStart(pokemon). Showdown picks at random among the
            // foes' strongest moves; only the draw matters here.
            (ab::FOREWARN, Ev::Start, Pre::On) => {
                let (foes, n) = self.allies_and_self(1 - holder.side as usize);
                let mut best = 1;
                let mut count = 0;
                let mut warned = [(holder, NO_MOVE); 2 * MAX_MOVES];
                for &f in &foes[..n] {
                    let m = self.mon(f);
                    for slot in &m.moves[..m.n_moves as usize] {
                        let d = &MOVES[slot.id as usize];
                        let mut bp = d.base_power as u32;
                        if d.ohko != Ohko::No {
                            bp = 150;
                        }
                        if matches!(slot.id, mv::COUNTER | mv::METALBURST | mv::MIRRORCOAT) {
                            bp = 120;
                        }
                        if bp == 1 || (bp == 0 && d.category != Category::Status) {
                            bp = 80;
                        }
                        if bp > best {
                            best = bp;
                            count = 0;
                        }
                        if bp == best {
                            warned[count as usize] = (f, slot.id);
                            count += 1;
                        }
                    }
                }
                if count > 0 {
                    // `-activate|pokemon|ability: Forewarn|Move|[of] foe`
                    let (foe, mv) = warned[self.rand(count, "forewarn") as usize];
                    self.show_ability(holder, ability);
                    self.show_move(foe, mv);
                }
                Res::Undef
            }

            // ---- Friend Guard: onAnyModifyDamage(damage, source, target, move)
            (ab::FRIENDGUARD, Ev::ModifyDamage, Pre::Any) => {
                if let Some(target) = e.source {
                    if target != holder && self.is_ally(target, holder) {
                        return self.chain_modify(3072, 4096);
                    }
                }
                Res::Undef
            }

            // ---- Fur Coat: onModifyDef(def)
            (ab::FURCOAT, Ev::ModifyDef, Pre::On) => self.chain_modify(2, 1),

            // ---- Gale Wings: onModifyPriority(priority, pokemon, target, move)
            (ab::GALEWINGS, Ev::ModifyPriority, Pre::On) => {
                let m = self.mon(holder);
                if mtype == Some(Type::Flying) && m.hp == m.max_hp() {
                    return Res::Num(relay.num() + 1);
                }
                Res::Undef
            }

            // ---- Good as Gold: onTryHit(target, source, move)
            (ab::GOODASGOLD, Ev::TryHit, Pre::On) => {
                if mcat == Some(Category::Status) && e.target != e.source {
                    // `-immune|target|[from] ability: Good as Gold`
                    self.show_ability(holder, ability);
                    return Res::Null;
                }
                Res::Undef
            }

            // ---- Gooey: onDamagingHit(damage, target, source, move)
            (ab::GOOEY, Ev::DamagingHit, Pre::On) => {
                if let (Some(m), Some(source)) = (mi, e.source) {
                    if self.makes_contact(m) {
                        // `-ability|target|Gooey`
                        self.show_ability(holder, ability);
                        self.boost1(SPE, -1, Some(source), e.target, Eff::None);
                    }
                }
                Res::Undef
            }

            // ---- Guard Dog
            // onDragOut(pokemon): `-activate|pokemon|ability: Guard Dog`
            (ab::GUARDDOG, Ev::DragOut, Pre::On) => {
                self.show_ability(holder, ability);
                Res::Null
            }
            // onTryBoost(boost, target, source, effect)
            (ab::GUARDDOG, Ev::TryBoost, Pre::On) => {
                if e.effect == Eff::Ability(ab::INTIMIDATE) && self.event.boosts[ATK] != 0 {
                    self.event.boosts[ATK] = 0;
                    self.boost1(ATK, 1, e.target, e.target, Eff::None);
                }
                Res::Undef
            }

            // ---- Guts: onModifyAtk(atk, pokemon)
            (ab::GUTS, Ev::ModifyAtk, Pre::On) => {
                if self.mon(holder).status != Status::None {
                    return self.chain_modify(6144, 4096);
                }
                Res::Undef
            }

            // ---- Harvest: onResidual(pokemon). In sun it always works (and no roll is made).
            (ab::HARVEST, Ev::Residual, Pre::On) => {
                if self.is_weather(Weather::Sunnyday) || self.chance(1, 2, "harvest") {
                    let m = self.mon(holder);
                    if m.hp > 0 && m.item == it::NONE && ITEMS[m.last_item as usize].flags & IF_BERRY != 0 {
                        let item = m.last_item;
                        self.set_item(holder, item, None, Eff::None);
                        self.mon_mut(holder).last_item = it::NONE;
                        // `-item|pokemon|Berry|[from] ability: Harvest`
                        self.show_item_gain(holder, item);
                        self.show_ability(holder, ability);
                    }
                }
                Res::Undef
            }

            // ---- Healer (Champions: one chance in two): onResidual(pokemon)
            (ab::HEALER, Ev::Residual, Pre::On) => {
                let (allies, n) = self.adjacent_allies(holder);
                for &a in &allies[..n] {
                    if self.mon(a).status != Status::None && self.chance(1, 2, "healer") {
                        // `-activate|pokemon|ability: Healer`
                        self.show_ability(holder, ability);
                        self.cure_status(a);
                    }
                }
                Res::Undef
            }

            // ---- Heatproof
            // onSourceModifyAtk / onSourceModifySpA(atk, attacker, defender, move)
            (ab::HEATPROOF, Ev::ModifyAtk | Ev::ModifySpA, Pre::Source) => {
                if mtype == Some(Type::Fire) {
                    return self.chain_modify(2048, 4096);
                }
                Res::Undef
            }
            // onDamage(damage, target, source, effect): burn damage is halved.
            (ab::HEATPROOF, Ev::Damage, Pre::On) => {
                if e.effect == Eff::Status(Status::Brn) {
                    return Res::Num(relay.num() / 2);
                }
                Res::Undef
            }

            // ---- Hospitality: onStart(pokemon)
            (ab::HOSPITALITY, Ev::Start, Pre::On) => {
                let (allies, n) = self.adjacent_allies(holder);
                for &a in &allies[..n] {
                    let amount = div1(self.mon(a).max_hp() as u32, 4);
                    self.heal(amount, Some(a), Some(holder), Eff::None);
                }
                Res::Undef
            }

            // ---- Huge Power / Pure Power: onModifyAtk(atk)
            (ab::HUGEPOWER | ab::PUREPOWER, Ev::ModifyAtk, Pre::On) => self.chain_modify(2, 1),

            // ---- Hustle
            // onModifyAtk(atk): applied to the stat directly, not chained.
            (ab::HUSTLE, Ev::ModifyAtk, Pre::On) => Res::Num(modify(relay.num() as u32, 6144) as i32),
            // onSourceModifyAccuracy(accuracy, target, source, move)
            (ab::HUSTLE, Ev::ModifyAccuracy, Pre::Source) => {
                if mcat == Some(Category::Physical) && matches!(relay, Res::Num(_)) {
                    return self.chain_modify(3277, 4096);
                }
                Res::Undef
            }

            // ---- Infiltrator: onModifyMove(move)
            (ab::INFILTRATOR, Ev::ModifyMove, Pre::On) => {
                if let Some(m) = mi {
                    self.am[m as usize].infiltrates = true;
                }
                Res::Undef
            }

            // ---- Innards Out: onDamagingHit(damage, target, source, move)
            (ab::INNARDSOUT, Ev::DamagingHit, Pre::On) => {
                if let (Some(m), Some(target), Some(source)) = (mi, e.target, e.source) {
                    if self.mon(target).hp == 0 {
                        // (The earlier hits count too, except for Dragon Darts.)
                        let am = &self.am[m as usize];
                        let d = relay.num() + if am.smart_target { 0 } else { am.total_damage };
                        self.damage(d, Some(source), Some(target), Eff::None);
                    }
                }
                Res::Undef
            }

            // ---- Inner Focus
            // onTryAddVolatile(status, pokemon)
            (ab::INNERFOCUS, Ev::TryAddVolatile, Pre::On) => {
                if e.vol == Some(VolKind::Flinch) {
                    return Res::Null;
                }
                Res::Undef
            }
            // onTryBoost(boost, target, source, effect)
            (ab::INNERFOCUS | ab::OBLIVIOUS | ab::OWNTEMPO | ab::SCRAPPY, Ev::TryBoost, Pre::On) => {
                self.block_intimidate()
            }

            // ---- Insomnia / Vital Spirit / Purifying Salt: onTryAddVolatile(status, target) blocks Yawn.
            (ab::INSOMNIA | ab::VITALSPIRIT | ab::PURIFYINGSALT, Ev::TryAddVolatile, Pre::On) => {
                if e.vol == Some(VolKind::Yawn) {
                    // `-immune|target|[from] ability: Insomnia`
                    self.show_ability(holder, ability);
                    return Res::Null;
                }
                Res::Undef
            }

            // ---- Intimidate: onStart(pokemon)
            (ab::INTIMIDATE, Ev::Start, Pre::On) => {
                let (foes, n) = self.allies_and_self(1 - holder.side as usize);
                if n > 0 {
                    // `-ability|pokemon|Intimidate|boost`
                    self.show_ability(holder, ability);
                }
                for &f in &foes[..n] {
                    if !self.has_vol_named(f, "substitute") {
                        self.boost1(ATK, -1, Some(f), Some(holder), Eff::None);
                    }
                }
                Res::Undef
            }

            // ---- Justified: onDamagingHit(damage, target, source, move)
            (ab::JUSTIFIED, Ev::DamagingHit, Pre::On) => {
                if mtype == Some(Type::Dark) {
                    self.boost1(ATK, 1, None, None, Eff::None);
                }
                Res::Undef
            }

            // ---- Klutz: onStart(pokemon) ends the held item's effect.
            (ab::KLUTZ, Ev::Start, Pre::On) => {
                let item = self.mon(holder).item;
                self.single_event(Ev::End, Eff::Item(item), Some(holder), Some(holder), None, Eff::None, Res::Undef);
                Res::Undef
            }

            // ---- Libero / Protean: onPrepareHit(source, target, move). Once per stay on the field.
            (ab::LIBERO | ab::PROTEAN, Ev::PrepareHit, Pre::On) => {
                let Some(m) = mi else {
                    return Res::Undef;
                };
                let am = &self.am[m as usize];
                if self.mon(holder).ability_st.a != 0
                    || am.has_bounced
                    || am.flags & F_FUTUREMOVE != 0
                    || am.d().calls_move
                {
                    return Res::Undef;
                }
                let t = self.am[m as usize].typ;
                if t != Type::Typeless && t != Type::None && !self.is_only_type(holder, t) {
                    self.set_type(holder, [t, Type::None]);
                    self.mon_mut(holder).ability_st.a = 1;
                    // `-start|source|typechange|Type|[from] ability: Protean`
                    self.show_ability(holder, ability);
                }
                Res::Undef
            }

            // ---- Lightning Rod: onAnyRedirectTarget(target, source, source2, move)
            (ab::LIGHTNINGROD, Ev::RedirectTarget, Pre::Any) => {
                let (Some(m), Some(user)) = (mi, e.target) else {
                    return Res::Undef;
                };
                if mtype != Some(Type::Electric) {
                    return Res::Undef;
                }
                let t = match self.am[m as usize].target {
                    Target::RandomNormal | Target::AdjacentFoe => Target::Normal,
                    other => other,
                };
                let loc = self.loc_of(user, holder);
                if self.valid_target_loc(loc, user, t) {
                    self.am[m as usize].smart_target = false;
                    if relay != Res::Mon(holder) {
                        // `-activate|holder|ability: Lightning Rod`
                        self.show_ability(holder, ability);
                    }
                    return Res::Mon(holder);
                }
                Res::Undef
            }

            // ---- Liquid Ooze: onSourceTryHeal(damage, target, source, effect)
            (ab::LIQUIDOOZE, Ev::TryHeal, Pre::Source) => {
                // Draining moves, Leech Seed and Strength Sap.
                if matches!(e.effect, Eff::Drain | Eff::Vol(VolKind::Leechseed))
                    || self.eff_is_named(e.effect, "strengthsap")
                {
                    self.damage(relay.num(), None, None, Eff::None);
                    return Res::Num(0);
                }
                Res::Undef
            }

            // ---- Liquid Voice: onModifyType(move, pokemon)
            (ab::LIQUIDVOICE, Ev::ModifyType, Pre::On) => {
                if let (Some(m), true) = (mi, mflags & F_SOUND != 0) {
                    self.am[m as usize].typ = Type::Water;
                }
                Res::Undef
            }

            // ---- Long Reach: onModifyMove(move)
            (ab::LONGREACH, Ev::ModifyMove, Pre::On) => {
                if let Some(m) = mi {
                    self.am[m as usize].flags &= !F_CONTACT;
                }
                Res::Undef
            }

            // ---- Magic Bounce: onTryHit(target, source, move)
            (ab::MAGICBOUNCE, Ev::TryHit, Pre::On) => {
                let (Some(m), Some(target), Some(source)) = (mi, e.target, e.source) else {
                    return Res::Undef;
                };
                let am = &self.am[m as usize];
                if target == source
                    || am.has_bounced
                    || am.flags & F_REFLECTABLE == 0
                    || self.is_semi_invulnerable(target)
                {
                    return Res::Undef;
                }
                let id = am.id;
                let bounced = self.new_am(id);
                self.am[bounced as usize].has_bounced = true;
                self.am[bounced as usize].prankster_boosted = false;
                self.use_move(bounced, target, Some(source), Eff::None);
                Res::Null
            }
            // onAllyTryHitSide(target, source, move): a move aimed at the holder's side.
            (ab::MAGICBOUNCE, Ev::TryHitSide, Pre::Ally) => {
                let (Some(m), Some(target), Some(source)) = (mi, e.target, e.source) else {
                    return Res::Undef;
                };
                let am = &self.am[m as usize];
                if self.is_ally(target, source)
                    || am.has_bounced
                    || am.flags & F_REFLECTABLE == 0
                    || self.is_semi_invulnerable(target)
                {
                    return Res::Undef;
                }
                let id = am.id;
                let bounced = self.new_am(id);
                self.am[bounced as usize].has_bounced = true;
                self.am[bounced as usize].prankster_boosted = false;
                self.use_move(bounced, holder, Some(source), Eff::None);
                // So that a second Magic Bounce on the side does not bounce it again.
                self.am[m as usize].has_bounced = true;
                Res::Null
            }
            (ab::SAPSIPPER, Ev::TryHitSide, Pre::Ally) => {
                let (Some(target), Some(source)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                if source == holder || !self.is_ally(target, source) {
                    return Res::Undef;
                }
                if mtype == Some(Type::Grass) {
                    self.boost1(ATK, 1, Some(holder), None, Eff::None);
                }
                Res::Undef
            }
            // Soundproof's version: `-immune|holder|[from] ability: Soundproof`
            (ab::SOUNDPROOF, Ev::TryHitSide, Pre::Ally) => {
                if mflags & F_SOUND != 0 {
                    self.show_ability(holder, ability);
                }
                Res::Undef
            }

            // ---- Magic Guard: onDamage(damage, target, source, effect)
            (ab::MAGICGUARD, Ev::Damage, Pre::On) => {
                if !e.effect.is_move() {
                    // Warding off another ability's damage names that ability: `-activate|source|ability: Rough Skin`
                    if let (Eff::Ability(theirs), Some(source)) = (e.effect, e.source) {
                        self.show_ability(source, theirs);
                    }
                    return FALSE;
                }
                Res::Undef
            }

            // ---- Magician: onAfterMoveSecondarySelf(source, target, move)
            (ab::MAGICIAN, Ev::AfterMoveSecondarySelf, Pre::On) => {
                let (Some(m), Some(source)) = (mi, e.target) else {
                    return Res::Undef;
                };
                let am = &self.am[m as usize];
                let s = self.mon(source);
                if self.switch_flag_is_true(source)
                    || !am.has_hit_targets
                    || am.id == mv::FLING
                    || s.item != it::NONE
                    || self.vols(source).has(VolKind::Gem)
                    || am.category == Category::Status
                {
                    return Res::Undef;
                }
                // Showdown speed-sorts the hit targets in place, ties shuffled.
                let n = am.n_hit_targets as usize;
                let mut keyed = [(source, 0i32); 3];
                for i in 0..n {
                    keyed[i] = (am.hit_targets[i], self.mon(am.hit_targets[i]).speed);
                }
                self.speed_sort(&mut keyed[..n], |a, b| b.1 as i64 - a.1 as i64, "magician target tie");
                for i in 0..n {
                    self.am[m as usize].hit_targets[i] = keyed[i].0;
                }
                for &(pokemon, _) in &keyed[..n] {
                    if pokemon == source {
                        continue;
                    }
                    let taken = self.take_item(pokemon, Some(source));
                    if taken == it::NONE {
                        continue;
                    }
                    if !self.set_item(source, taken, None, Eff::None) {
                        // Give it straight back without the usual item hooks.
                        self.mon_mut(pokemon).item = taken;
                        continue;
                    }
                    // `-item|source|Item|[from] ability: Magician|[of] pokemon`
                    self.show_item_lost(pokemon, taken);
                    self.show_item_arrived(source, taken);
                    self.show_ability(holder, ability);
                    return Res::Undef;
                }
                Res::Undef
            }

            // ---- Magma Armor
            // onUpdate(pokemon)
            (ab::MAGMAARMOR, Ev::Update, Pre::On) => {
                if self.mon(holder).status == Status::Frz {
                    // `-activate|pokemon|ability: Magma Armor`
                    self.show_ability(holder, ability);
                    self.cure_status(holder);
                }
                Res::Undef
            }
            // onImmunity(type, pokemon)
            (ab::MAGMAARMOR, Ev::Immunity, Pre::On) => {
                if e.imm == Some(Imm::Status(Status::Frz)) {
                    return FALSE;
                }
                Res::Undef
            }

            // ---- Marvel Scale: onModifyDef(def, pokemon)
            (ab::MARVELSCALE, Ev::ModifyDef, Pre::On) => {
                if self.mon(holder).status != Status::None {
                    return self.chain_modify(6144, 4096);
                }
                Res::Undef
            }

            // ---- Merciless: onModifyCritRatio(critRatio, source, target)
            (ab::MERCILESS, Ev::ModifyCritRatio, Pre::On) => {
                if e.source.is_some_and(|t| matches!(self.mon(t).status, Status::Psn | Status::Tox)) {
                    return Res::Num(5);
                }
                Res::Undef
            }

            // ---- Minus / Plus: onModifySpA(spa, pokemon)
            (ab::MINUS | ab::PLUS, Ev::ModifySpA, Pre::On) => {
                let (allies, n) = self.allies_and_self(holder.side as usize);
                for &a in &allies[..n] {
                    if a != holder && (self.has_ability(a, ab::MINUS) || self.has_ability(a, ab::PLUS)) {
                        return self.chain_modify(6144, 4096);
                    }
                }
                Res::Undef
            }

            // ---- Mirror Armor: onTryBoost(boost, target, source, effect)
            (ab::MIRRORARMOR, Ev::TryBoost, Pre::On) => {
                let (Some(target), Some(source)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                if target == source || e.effect == Eff::Ability(ab::MIRRORARMOR) {
                    return Res::Undef;
                }
                for k in 0..7 {
                    let b = self.event.boosts[k];
                    if b < 0 {
                        if self.mon(target).boosts[k] == -6 {
                            continue;
                        }
                        self.event.boosts[k] = 0;
                        if self.mon(source).hp > 0 {
                            // `-ability|target|Mirror Armor`
                            self.show_ability(holder, ability);
                            self.boost1(k, b, Some(source), Some(target), Eff::None);
                        }
                    }
                }
                Res::Undef
            }

            // ---- Mold Breaker: onModifyMove(move)
            (ab::MOLDBREAKER, Ev::ModifyMove, Pre::On) => {
                if let Some(m) = mi {
                    self.am[m as usize].ignore_ability = true;
                }
                Res::Undef
            }

            // ---- Moody: onResidual(pokemon)
            (ab::MOODY, Ev::Residual, Pre::On) => {
                let boosts = self.mon(holder).boosts;
                let mut stats = [0usize; 5];
                let mut n = 0;
                for k in 0..5 {
                    if boosts[k] < 6 {
                        stats[n] = k;
                        n += 1;
                    }
                }
                let plus = if n > 0 { Some(stats[self.rand(n as u32, "moody raise") as usize]) } else { None };
                n = 0;
                for k in 0..5 {
                    if boosts[k] > -6 && Some(k) != plus {
                        stats[n] = k;
                        n += 1;
                    }
                }
                let minus = if n > 0 { Some(stats[self.rand(n as u32, "moody lower") as usize]) } else { None };
                // The raised stat is applied first, whatever its position.
                let mut b = [0i8; 7];
                let mut order = [0u8; 7];
                let mut w = 0;
                for (stat, by) in [(plus, 2), (minus, -1)] {
                    if let Some(k) = stat {
                        b[k] = by;
                        order[w] = k as u8;
                        w += 1;
                    }
                }
                for k in 0..7u8 {
                    if !order[..w].contains(&k) {
                        order[w] = k;
                        w += 1;
                    }
                }
                self.boost_ordered(b, &order, Some(holder), Some(holder), Eff::None);
                Res::Undef
            }

            // ---- Moxie: onSourceAfterFaint(length, target, source, effect)
            (ab::MOXIE, Ev::AfterFaint, Pre::Source) => {
                if e.effect.is_move() {
                    self.boost1(ATK, relay.num() as i8, e.source, None, Eff::None);
                }
                Res::Undef
            }

            // ---- Multiscale: onSourceModifyDamage(damage, source, target, move)
            (ab::MULTISCALE, Ev::ModifyDamage, Pre::Source) => {
                let m = self.mon(holder);
                if m.hp >= m.max_hp() {
                    return self.chain_modify(2048, 4096);
                }
                Res::Undef
            }

            // ---- Mummy: onDamagingHit(damage, target, source, move)
            (ab::MUMMY, Ev::DamagingHit, Pre::On) => {
                let (Some(m), Some(source)) = (mi, e.source) else {
                    return Res::Undef;
                };
                let theirs = self.mon(source).ability;
                if ABILITIES[theirs as usize].flags & AF_CANTSUPPRESS != 0 || theirs == ab::MUMMY {
                    return Res::Undef;
                }
                if self.makes_contact(m) {
                    self.set_ability(source, ab::MUMMY, e.target, Eff::None);
                }
                Res::Undef
            }

            // ---- Natural Cure (Champions): onSwitchOut(pokemon)
            (ab::NATURALCURE, Ev::SwitchOut, Pre::On) => {
                if self.cure_status(holder) {
                    // `-curestatus|pokemon|status|[from] ability: Natural Cure|[silent]`
                    self.show_ability(holder, ability);
                }
                Res::Undef
            }

            // ---- No Guard
            // onAnyInvulnerability(target, source, move)
            (ab::NOGUARD, Ev::Invulnerability, Pre::Any) => {
                if mi.is_some() && (e.source == Some(holder) || e.target == Some(holder)) {
                    return Res::Num(0);
                }
                Res::Undef
            }
            // onAnyAccuracy(accuracy, target, source, move)
            (ab::NOGUARD, Ev::Accuracy, Pre::Any) => {
                if mi.is_some() && (e.source == Some(holder) || e.target == Some(holder)) {
                    return TRUE;
                }
                relay
            }

            // ---- Oblivious (its Intimidate block is with Inner Focus)
            // onUpdate(pokemon)
            (ab::OBLIVIOUS, Ev::Update, Pre::On) => {
                if self.vols(holder).has(VolKind::Attract) || self.vols(holder).has(VolKind::Taunt) {
                    // `-activate|pokemon|ability: Oblivious`
                    self.show_ability(holder, ability);
                }
                self.remove_volatile(holder, VolKind::Attract);
                self.remove_volatile(holder, VolKind::Taunt);
                Res::Undef
            }
            // onImmunity(type, pokemon)
            (ab::OBLIVIOUS, Ev::Immunity, Pre::On) => {
                if e.imm == Some(Imm::Vol(VolKind::Attract)) {
                    return FALSE;
                }
                Res::Undef
            }
            // onTryHit(pokemon, target, move)
            (ab::OBLIVIOUS, Ev::TryHit, Pre::On) => {
                if mi.is_some_and(|m| matches!(self.am[m as usize].id, mv::ATTRACT | mv::TAUNT)) {
                    // `-immune|pokemon|[from] ability: Oblivious`
                    self.show_ability(holder, ability);
                    return Res::Null;
                }
                Res::Undef
            }

            // ---- Opportunist. `ability_boosts` holds the copies waiting to be applied,
            // `ability_st.a` whether Showdown's `effectState.boosts` exists at all.
            // onFoeAfterBoost(boost, target, source, effect)
            (ab::OPPORTUNIST, Ev::AfterBoost, Pre::Foe) => {
                if e.effect == Eff::Ability(ab::OPPORTUNIST) {
                    return Res::Undef;
                }
                let m = self.mon_mut(holder);
                m.ability_st.a = 1;
                for k in 0..7 {
                    if e.boosts[k] > 0 {
                        m.ability_boosts[k] += e.boosts[k];
                    }
                }
                Res::Undef
            }
            // onAnySwitchIn / onAnyAfterMega / onAnyAfterTerastallization / onAnyAfterMove / onResidual
            (ab::OPPORTUNIST, Ev::SwitchIn | Ev::AfterMega | Ev::AfterTerastallization | Ev::AfterMove, Pre::Any)
            | (ab::OPPORTUNIST, Ev::Residual, Pre::On) => {
                if self.mon(holder).ability_st.a == 0 {
                    return Res::Undef;
                }
                let b = self.mon(holder).ability_boosts;
                self.boost(b, Some(holder), None, Eff::None);
                let m = self.mon_mut(holder);
                m.ability_st.a = 0;
                m.ability_boosts = [0; 7];
                Res::Undef
            }
            // onEnd()
            (ab::OPPORTUNIST, Ev::End, Pre::On) => {
                let m = self.mon_mut(holder);
                m.ability_st.a = 0;
                m.ability_boosts = [0; 7];
                Res::Undef
            }

            // ---- Overcoat
            // onImmunity(type, pokemon)
            (ab::OVERCOAT, Ev::Immunity, Pre::On) => {
                if matches!(e.imm, Some(Imm::Powder | Imm::Weather(Weather::Sandstorm))) {
                    return FALSE;
                }
                Res::Undef
            }
            // onTryHit(target, source, move)
            (ab::OVERCOAT, Ev::TryHit, Pre::On) => {
                if mflags & F_POWDER != 0 && e.target != e.source && self.type_allows(holder, 5) {
                    // `-immune|target|[from] ability: Overcoat`
                    self.show_ability(holder, ability);
                    return Res::Null;
                }
                Res::Undef
            }

            // ---- Own Tempo
            // onUpdate(pokemon)
            (ab::OWNTEMPO, Ev::Update, Pre::On) => {
                if self.vols(holder).has(VolKind::Confusion) {
                    // `-activate|pokemon|ability: Own Tempo`
                    self.show_ability(holder, ability);
                    self.remove_volatile(holder, VolKind::Confusion);
                }
                Res::Undef
            }
            // onTryAddVolatile(status, pokemon)
            (ab::OWNTEMPO, Ev::TryAddVolatile, Pre::On) => {
                if e.vol == Some(VolKind::Confusion) {
                    return Res::Null;
                }
                Res::Undef
            }
            // onHit(target, source, move): `-immune|target|confusion|[from] ability: Own Tempo` to a move that would confuse.
            (ab::OWNTEMPO, Ev::Hit, Pre::On) => {
                if mi.is_some_and(|m| self.am[m as usize].d().volatile == Some(VolKind::Confusion)) {
                    self.show_ability(holder, ability);
                }
                Res::Undef
            }

            // ---- Parental Bond
            // onPrepareHit(source, target, move)
            (ab::PARENTALBOND, Ev::PrepareHit, Pre::On) => {
                let Some(m) = mi else {
                    return Res::Undef;
                };
                let am = &mut self.am[m as usize];
                if am.category == Category::Status
                    || am.multihit != (0, 0)
                    || am.flags & (F_NOPARENTALBOND | F_CHARGE | F_FUTUREMOVE) != 0
                    || am.spread_hit
                {
                    return Res::Undef;
                }
                am.multihit = (2, 2);
                am.parental_bond = true;
                Res::Undef
            }
            // onSourceModifySecondaries only concerns Secret Power, which is not in Champions.
            (ab::PARENTALBOND, Ev::ModifySecondaries, Pre::Source) => Res::Undef,

            // ---- Pickpocket: onAfterMoveSecondary(target, source, move)
            (ab::PICKPOCKET, Ev::AfterMoveSecondary, Pre::On) => {
                let (Some(target), Some(source)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                if source == target || mflags & F_CONTACT == 0 {
                    return Res::Undef;
                }
                // (A user on its way out through its own move's switch can still be robbed.)
                if self.mon(target).item != it::NONE
                    || self.mon(target).switch_flag
                    || self.mon(target).force_switch_flag
                    || self.switch_flag_is_true(source)
                {
                    return Res::Undef;
                }
                let taken = self.take_item(source, Some(target));
                if taken == it::NONE {
                    return Res::Undef;
                }
                if !self.set_item(target, taken, None, Eff::None) {
                    self.mon_mut(source).item = taken;
                } else {
                    // `-enditem|source|Item|[silent]`, `-item|target|Item|[from] ability: Pickpocket|[of] source`
                    self.show_item_lost(source, taken);
                    self.show_item_arrived(target, taken);
                    self.show_ability(holder, ability);
                }
                Res::Undef
            }

            // ---- Pickup: onResidual(pokemon)
            (ab::PICKUP, Ev::Residual, Pre::On) => {
                if self.mon(holder).item != it::NONE {
                    return Res::Undef;
                }
                let (actives, n) = self.all_active(false);
                let mut cands = [holder; 4];
                let mut k = 0;
                for &t in &actives[..n] {
                    let m = self.mon(t);
                    if m.last_item != it::NONE && m.used_item_this_turn && self.is_adjacent(holder, t) {
                        cands[k] = t;
                        k += 1;
                    }
                }
                if k == 0 {
                    return Res::Undef;
                }
                let from = cands[self.rand(k as u32, "pickup") as usize];
                let item = self.mon(from).last_item;
                self.mon_mut(from).last_item = it::NONE;
                // `-item|pokemon|Item|[from] ability: Pickup`
                self.show_item_gain(holder, item);
                self.show_ability(holder, ability);
                self.set_item(holder, item, None, Eff::None);
                Res::Undef
            }

            // ---- Piercing Drill / Unseen Fist (Champions): onHitProtect(source, target, move).
            // Contact moves go through Protect for a quarter of the damage.
            (ab::PIERCINGDRILL | ab::UNSEENFIST, Ev::HitProtect, Pre::On) => {
                if let (Some(m), Some(target), true) = (mi, e.source, mflags & F_CONTACT != 0) {
                    let slot = self.slot_index(target);
                    self.am[m as usize].hit_data[slot].bypass_protect = true;
                    return FALSE;
                }
                Res::Undef
            }

            // ---- Poison Heal: onDamage(damage, target, source, effect)
            (ab::POISONHEAL, Ev::Damage, Pre::On) => {
                if matches!(e.effect, Eff::Status(Status::Psn) | Eff::Status(Status::Tox)) {
                    let amount = div1(self.mon(holder).max_hp() as u32, 8);
                    self.heal(amount, None, None, Eff::None);
                    return FALSE;
                }
                Res::Undef
            }

            // ---- Poison Touch: onSourceDamagingHit(damage, target, source, move)
            (ab::POISONTOUCH, Ev::DamagingHit, Pre::Source) => {
                let (Some(m), Some(target)) = (mi, e.target) else {
                    return Res::Undef;
                };
                // Shield Dust blocks it even though it is not a secondary effect.
                if self.has_ability(target, ab::SHIELDDUST) {
                    return Res::Undef;
                }
                if self.makes_contact(m) && self.chance(3, 10, "poison touch") {
                    self.try_set_status(target, Status::Psn, e.source, Eff::None);
                }
                Res::Undef
            }

            // ---- Prankster: onModifyPriority(priority, pokemon, target, move)
            (ab::PRANKSTER, Ev::ModifyPriority, Pre::On) => {
                if let (Some(m), Some(Category::Status)) = (mi, mcat) {
                    self.am[m as usize].prankster_boosted = true;
                    return Res::Num(relay.num() + 1);
                }
                Res::Undef
            }

            // ---- Pressure: onDeductPP(target, source)
            (ab::PRESSURE, Ev::DeductPP, Pre::On) => {
                if let (Some(target), Some(source)) = (e.target, e.source) {
                    if self.is_ally(target, source) {
                        return Res::Undef;
                    }
                }
                Res::Num(1)
            }

            // ---- Punk Rock
            // onBasePower(basePower, attacker, defender, move)
            (ab::PUNKROCK, Ev::BasePower, Pre::On) => {
                if mflags & F_SOUND != 0 {
                    return self.chain_modify(5325, 4096);
                }
                Res::Undef
            }
            // onSourceModifyDamage(damage, source, target, move)
            (ab::PUNKROCK, Ev::ModifyDamage, Pre::Source) => {
                if mflags & F_SOUND != 0 {
                    return self.chain_modify(2048, 4096);
                }
                Res::Undef
            }

            // ---- Purifying Salt
            // onSetStatus(status, target, source, effect)
            (ab::PURIFYINGSALT, Ev::SetStatus, Pre::On) => {
                if self.is_status_move(e.effect) {
                    // `-immune|target|[from] ability: Purifying Salt`
                    self.show_ability(holder, ability);
                }
                FALSE
            }
            // onSourceModifyAtk / onSourceModifySpA(atk, attacker, defender, move)
            (ab::PURIFYINGSALT, Ev::ModifyAtk | Ev::ModifySpA, Pre::Source) => {
                if mtype == Some(Type::Ghost) {
                    return self.chain_modify(2048, 4096);
                }
                Res::Undef
            }

            // ---- Quick Draw: onFractionalPriority(priority, pokemon, target, move)
            (ab::QUICKDRAW, Ev::FractionalPriority, Pre::On) => {
                if mcat != Some(Category::Status) && self.chance(3, 10, "quick draw") {
                    // `-activate|pokemon|ability: Quick Draw`
                    self.show_ability(holder, ability);
                    return Res::Num(1);
                }
                Res::Undef
            }

            // ---- Quick Feet: onModifySpe(spe, pokemon)
            (ab::QUICKFEET, Ev::ModifySpe, Pre::On) => {
                if self.mon(holder).status != Status::None {
                    return self.chain_modify(6144, 4096);
                }
                Res::Undef
            }

            // ---- Rattled
            // onDamagingHit(damage, target, source, move)
            (ab::RATTLED, Ev::DamagingHit, Pre::On) => {
                if matches!(mtype, Some(Type::Dark | Type::Bug | Type::Ghost)) {
                    self.boost1(SPE, 1, None, None, Eff::None);
                }
                Res::Undef
            }
            // onAfterBoost(boost, target, source, effect)
            (ab::RATTLED, Ev::AfterBoost, Pre::On) => {
                if e.effect == Eff::Ability(ab::INTIMIDATE) && e.boosts[ATK] != 0 {
                    self.boost1(SPE, 1, None, None, Eff::None);
                }
                Res::Undef
            }

            // ---- Receiver: onAllyFaint(target)
            (ab::RECEIVER, Ev::Faint, Pre::Ally) => {
                let Some(fainted) = e.target else {
                    return Res::Undef;
                };
                if self.mon(holder).hp == 0 {
                    return Res::Undef;
                }
                let theirs = self.mon(fainted).ability;
                if ABILITIES[theirs as usize].flags & AF_NORECEIVER != 0 || theirs == ab::NOABILITY {
                    return Res::Undef;
                }
                self.set_ability(holder, theirs, Some(fainted), Eff::None);
                Res::Undef
            }

            // ---- Reckless: onBasePower(basePower, attacker, defender, move)
            (ab::RECKLESS, Ev::BasePower, Pre::On) => {
                if mi.is_some_and(|m| self.am[m as usize].d().recoil.0 > 0 || self.am[m as usize].d().has_crash_damage)
                {
                    return self.chain_modify(4915, 4096);
                }
                Res::Undef
            }

            // ---- Regenerator (Champions): onSwitchOut(pokemon)
            (ab::REGENERATOR, Ev::SwitchOut, Pre::On) => {
                let amount = self.mon(holder).max_hp() as i32 / 3;
                if self.heal_mon(holder, amount) != 0 {
                    // `-heal|pokemon|hp|[from] ability: Regenerator|[silent]`
                    self.show_ability(holder, ability);
                }
                Res::Undef
            }

            // ---- Ripen. `ability_st.a` is `berryWeaken`.
            // onTryHeal(damage, target, source, effect)
            (ab::RIPEN, Ev::TryHeal, Pre::On) => {
                if let Eff::Item(i) = e.effect {
                    if i == it::LEFTOVERS {
                        // `-activate|target|ability: Ripen`
                        self.show_ability(holder, ability);
                    }
                    if ITEMS[i as usize].flags & IF_BERRY != 0 {
                        return self.chain_modify(2, 1);
                    }
                }
                Res::Undef
            }
            // onChangeBoost(boost, target, source, effect)
            (ab::RIPEN, Ev::ChangeBoost, Pre::On) => {
                if let Eff::Item(i) = e.effect {
                    if ITEMS[i as usize].flags & IF_BERRY != 0 {
                        for b in self.event.boosts.iter_mut() {
                            *b *= 2;
                        }
                    }
                }
                Res::Undef
            }
            // onSourceModifyDamage(damage, source, target, move)
            (ab::RIPEN, Ev::ModifyDamage, Pre::Source) => {
                if self.mon(holder).ability_st.a != 0 {
                    self.mon_mut(holder).ability_st.a = 0;
                    return self.chain_modify(2048, 4096);
                }
                Res::Undef
            }
            // onTryEatItem(item, pokemon): `-activate|pokemon|ability: Ripen`
            (ab::RIPEN, Ev::TryEatItem, Pre::On) => {
                self.show_ability(holder, ability);
                Res::Undef
            }
            // onEatItem(item, pokemon)
            (ab::RIPEN, Ev::EatItem, Pre::On) => {
                self.mon_mut(holder).ability_st.a = crate::items::is_resist_berry(e.item) as i16;
                Res::Undef
            }

            // ---- Rivalry: onBasePower(basePower, attacker, defender, move)
            (ab::RIVALRY, Ev::BasePower, Pre::On) => {
                let (Some(attacker), Some(defender)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                let (a, d) = (self.mon(attacker).gender, self.mon(defender).gender);
                if a != Gender::N && d != Gender::N {
                    return if a == d { self.chain_modify(5120, 4096) } else { self.chain_modify(3072, 4096) };
                }
                Res::Undef
            }

            // ---- Rock Head: onDamage(damage, target, source, effect)
            (ab::ROCKHEAD, Ev::Damage, Pre::On) => {
                if e.effect == Eff::Recoil {
                    let struggling = self.active_move.is_some_and(|m| self.am[m as usize].id == mv::STRUGGLE);
                    if !struggling {
                        return Res::Null;
                    }
                }
                Res::Undef
            }

            // ---- Rough Skin: onDamagingHit(damage, target, source, move)
            (ab::ROUGHSKIN, Ev::DamagingHit, Pre::On) => {
                if let (Some(m), Some(target), Some(source)) = (mi, e.target, e.source) {
                    if self.makes_contact(m) {
                        let d = div1(self.mon(source).max_hp() as u32, 8);
                        self.damage(d, Some(source), Some(target), Eff::None);
                    }
                }
                Res::Undef
            }

            // ---- Run Away (Champions): onTrapPokemon / onMaybeTrapPokemon(pokemon)
            (ab::RUNAWAY, Ev::TrapPokemon, Pre::On) => {
                self.mon_mut(holder).trapped = Trapped::No;
                Res::Undef
            }
            (ab::RUNAWAY, Ev::MaybeTrapPokemon, Pre::On) => Res::Undef,

            // ---- Scrappy: onModifyMove(move)
            (ab::SCRAPPY, Ev::ModifyMove, Pre::On) => {
                if let Some(m) = mi {
                    if self.am[m as usize].ignore_immunity == IgnoreImm::No {
                        self.am[m as usize].ignore_immunity = IgnoreImm::NormalFighting;
                    }
                }
                Res::Undef
            }

            // ---- Serene Grace: onModifyMove(move)
            (ab::SERENEGRACE, Ev::ModifyMove, Pre::On) => {
                if let Some(m) = mi {
                    let am = &mut self.am[m as usize];
                    if am.has_secs {
                        for s in am.secs[..am.n_secs as usize].iter_mut() {
                            s.chance *= 2;
                        }
                    }
                    am.self_chance = am.self_chance.saturating_mul(2);
                }
                Res::Undef
            }

            // ---- Shadow Tag
            // onFoeTrapPokemon(pokemon)
            (ab::SHADOWTAG, Ev::TrapPokemon, Pre::Foe) => {
                if let Some(pokemon) = e.target {
                    if !self.has_ability(pokemon, ab::SHADOWTAG) && self.is_adjacent(pokemon, holder) {
                        self.try_trap(pokemon, true);
                    }
                }
                Res::Undef
            }
            // onFoeMaybeTrapPokemon only decides what the player is told.
            (ab::SHADOWTAG, Ev::MaybeTrapPokemon, Pre::Foe) => Res::Undef,

            // ---- Shed Skin: onResidual(pokemon)
            (ab::SHEDSKIN, Ev::Residual, Pre::On) => {
                let m = self.mon(holder);
                if m.hp > 0 && m.status != Status::None && self.chance(33, 100, "shed skin") {
                    // `-activate|pokemon|ability: Shed Skin`
                    self.show_ability(holder, ability);
                    self.cure_status(holder);
                }
                Res::Undef
            }

            // ---- Sheer Force
            // onModifyMove(move, pokemon)
            (ab::SHEERFORCE, Ev::ModifyMove, Pre::On) => {
                if let Some(m) = mi {
                    let am = &mut self.am[m as usize];
                    if am.has_secs && !am.d().sheer_force_boost {
                        am.has_secs = false;
                        am.n_secs = 0;
                        // Not a secondary effect, but removed with them.
                        am.self_boosts = None;
                        am.self_chance = 0;
                        am.has_sheer_force = true;
                    }
                }
                Res::Undef
            }
            // onBasePower(basePower, pokemon, target, move)
            (ab::SHEERFORCE, Ev::BasePower, Pre::On) => {
                if mi.is_some_and(|m| self.am[m as usize].has_sheer_force || self.am[m as usize].d().sheer_force_boost)
                {
                    return self.chain_modify(5325, 4096);
                }
                Res::Undef
            }

            // ---- Shield Dust: onModifySecondaries(secondaries): only those with a `self` part survive.
            (ab::SHIELDDUST, Ev::ModifySecondaries, Pre::On) => {
                if let Some(m) = mi {
                    let am = &self.am[m as usize];
                    for k in 0..am.n_secs as usize {
                        if am.secs[k].self_boosts.is_none() {
                            self.event.secs &= !(1 << k);
                        }
                    }
                }
                Res::Undef
            }

            // ---- Skill Link: onModifyMove(move)
            (ab::SKILLLINK, Ev::ModifyMove, Pre::On) => {
                if let Some(m) = mi {
                    let am = &mut self.am[m as usize];
                    if am.multihit.0 != am.multihit.1 {
                        am.multihit = (am.multihit.1, am.multihit.1);
                    }
                    am.multiaccuracy = false;
                }
                Res::Undef
            }

            // ---- Sniper: onModifyDamage(damage, source, target, move)
            (ab::SNIPER, Ev::ModifyDamage, Pre::On) => {
                if let (Some(m), Some(target)) = (mi, e.source) {
                    if self.am[m as usize].hit_data[self.slot_index(target)].crit {
                        return self.chain_modify(6144, 4096);
                    }
                }
                Res::Undef
            }

            // ---- Soundproof: onTryHit(target, source, move)
            (ab::SOUNDPROOF, Ev::TryHit, Pre::On) => {
                if e.target != e.source && mflags & F_SOUND != 0 {
                    // `-immune|target|[from] ability: Soundproof`
                    self.show_ability(holder, ability);
                    return Res::Null;
                }
                Res::Undef
            }

            // ---- Speed Boost: onResidual(pokemon)
            (ab::SPEEDBOOST, Ev::Residual, Pre::On) => {
                if self.mon(holder).active_turns > 0 {
                    self.boost1(SPE, 1, None, None, Eff::None);
                }
                Res::Undef
            }

            // ---- Spicy Spray: onDamagingHit(damage, target, source, move)
            (ab::SPICYSPRAY, Ev::DamagingHit, Pre::On) => {
                if let Some(source) = e.source {
                    self.try_set_status(source, Status::Brn, e.target, Eff::None);
                }
                Res::Undef
            }

            // ---- Stakeout: onModifyAtk / onModifySpA(atk, attacker, defender)
            (ab::STAKEOUT, Ev::ModifyAtk | Ev::ModifySpA, Pre::On) => {
                if e.source.is_some_and(|d| self.mon(d).active_turns == 0) {
                    return self.chain_modify(2, 1);
                }
                Res::Undef
            }

            // ---- Stall: onFractionalPriority: -0.1
            (ab::STALL, Ev::FractionalPriority, Pre::On) => Res::Num(-1),

            // ---- Stalwart: onModifyMove(move)
            (ab::STALWART, Ev::ModifyMove, Pre::On) => {
                if let Some(m) = mi {
                    self.am[m as usize].tracks_target = self.am[m as usize].target != Target::Scripted;
                }
                Res::Undef
            }

            // ---- Stamina: onDamagingHit(damage, target, source, effect)
            (ab::STAMINA, Ev::DamagingHit, Pre::On) => {
                self.boost1(DEF, 1, None, None, Eff::None);
                Res::Undef
            }

            // ---- Steadfast: onFlinch(pokemon)
            (ab::STEADFAST, Ev::Flinch, Pre::On) => {
                self.boost1(SPE, 1, None, None, Eff::None);
                Res::Undef
            }

            // ---- Steely Spirit: onAllyBasePower(basePower, attacker, defender, move)
            (ab::STEELYSPIRIT, Ev::BasePower, Pre::Ally) => {
                if mtype == Some(Type::Steel) {
                    return self.chain_modify(6144, 4096);
                }
                Res::Undef
            }

            // ---- Stench: onModifyMove(move)
            (ab::STENCH, Ev::ModifyMove, Pre::On) => {
                if let Some(m) = mi {
                    self.add_flinch_secondary(m);
                }
                Res::Undef
            }

            // ---- Sticky Hold: onTakeItem(item, pokemon, source)
            (ab::STICKYHOLD, Ev::TakeItem, Pre::On) => {
                if self.mon(holder).hp == 0 {
                    return Res::Undef;
                }
                // Nobody else takes it, and Knock Off does not remove it either.
                let knock_off = self.active_move.is_some_and(|m| self.am[m as usize].id == mv::KNOCKOFF);
                if (e.source.is_some() && e.source != Some(holder)) || knock_off {
                    // `-activate|pokemon|ability: Sticky Hold`
                    self.show_ability(holder, ability);
                    return FALSE;
                }
                Res::Undef
            }

            // ---- Sturdy
            // onTryHit(pokemon, target, move): immune to one-hit knockouts.
            (ab::STURDY, Ev::TryHit, Pre::On) => {
                if mi.is_some_and(|m| self.am[m as usize].d().ohko != Ohko::No) {
                    // `-immune|pokemon|[from] ability: Sturdy`
                    self.show_ability(holder, ability);
                    return Res::Null;
                }
                Res::Undef
            }
            // onDamage(damage, target, source, effect)
            (ab::STURDY, Ev::Damage, Pre::On) => {
                let m = self.mon(holder);
                if m.hp == m.max_hp() && relay.num() >= m.hp as i32 && e.effect.is_move() {
                    let left = m.hp as i32 - 1;
                    // `-ability|target|Sturdy`
                    self.show_ability(holder, ability);
                    return Res::Num(left);
                }
                Res::Undef
            }

            // ---- Super Luck: onModifyCritRatio(critRatio)
            (ab::SUPERLUCK, Ev::ModifyCritRatio, Pre::On) => Res::Num(relay.num() + 1),

            // ---- Supersweet Syrup: onStart(pokemon), once per battle.
            (ab::SUPERSWEETSYRUP, Ev::Start, Pre::On) => {
                if self.mon(holder).syrup_triggered {
                    return Res::Undef;
                }
                self.mon_mut(holder).syrup_triggered = true;
                // `-ability|pokemon|Supersweet Syrup`
                self.show_ability(holder, ability);
                let (foes, n) = self.allies_and_self(1 - holder.side as usize);
                for &f in &foes[..n] {
                    if !self.has_vol_named(f, "substitute") {
                        self.boost1(EVA, -1, Some(f), Some(holder), Eff::None);
                    }
                }
                Res::Undef
            }

            // ---- Supreme Overlord. `ability_st.a` is the number of fallen allies counted at switch-in.
            // onStart(pokemon)
            (ab::SUPREMEOVERLORD, Ev::Start, Pre::On) => {
                let fallen = self.sides[holder.side as usize].total_fainted.min(5);
                self.mon_mut(holder).ability_st.a = fallen as i16;
                if fallen > 0 {
                    // `-activate|pokemon|ability: Supreme Overlord`
                    self.show_ability(holder, ability);
                }
                Res::Undef
            }
            (ab::SUPREMEOVERLORD, Ev::End, Pre::On) => Res::Undef,
            // onBasePower(basePower, attacker, defender, move)
            (ab::SUPREMEOVERLORD, Ev::BasePower, Pre::On) => {
                const POW_MOD: [u32; 6] = [4096, 4506, 4915, 5325, 5734, 6144];
                let fallen = self.mon(holder).ability_st.a;
                if fallen > 0 {
                    return self.chain_modify(POW_MOD[fallen as usize], 4096);
                }
                Res::Undef
            }

            // ---- Sweet Veil: onAllySetStatus(status, target, source, effect)
            (ab::SWEETVEIL, Ev::SetStatus, Pre::Ally) => {
                if e.status == Status::Slp {
                    // `-block|target|ability: Sweet Veil|[of] holder`
                    self.show_ability(holder, ability);
                    return Res::Null;
                }
                Res::Undef
            }

            // ---- Symbiosis: onAllyAfterUseItem(item, pokemon)
            (ab::SYMBIOSIS, Ev::AfterUseItem, Pre::Ally) => {
                let Some(pokemon) = e.target else {
                    return Res::Undef;
                };
                if self.mon(pokemon).switch_flag {
                    return Res::Undef;
                }
                let mine = self.take_item(holder, None);
                if mine == it::NONE {
                    return Res::Undef;
                }
                if !self.set_item(pokemon, mine, None, Eff::None) {
                    self.mon_mut(holder).item = mine;
                } else {
                    // `-activate|holder|ability: Symbiosis|Item|[of] pokemon`
                    self.show_ability(holder, ability);
                    self.show_item_lost(holder, mine);
                    self.show_item_arrived(pokemon, mine);
                }
                Res::Undef
            }

            // ---- Synchronize: onAfterSetStatus(status, target, source, effect)
            (ab::SYNCHRONIZE, Ev::AfterSetStatus, Pre::On) => {
                let (Some(target), Some(source)) = (e.target, e.source) else {
                    return Res::Undef;
                };
                // Poison from Toxic Spikes is not passed back (its "source" is only nominal).
                if source == target
                    || e.effect == Eff::SideCond(SideCond::Toxicspikes)
                    || matches!(e.status, Status::Slp | Status::Frz)
                {
                    return Res::Undef;
                }
                // `-activate|target|ability: Synchronize`
                self.show_ability(holder, ability);
                self.try_set_status(source, e.status, Some(target), Eff::Ability(ab::SYNCHRONIZE));
                Res::Undef
            }

            // ---- Tangled Feet: onModifyAccuracy(accuracy, target)
            (ab::TANGLEDFEET, Ev::ModifyAccuracy, Pre::On) => {
                if matches!(relay, Res::Num(_)) && self.vols(holder).has(VolKind::Confusion) {
                    return self.chain_modify(2048, 4096);
                }
                Res::Undef
            }

            // ---- Technician: onBasePower(basePower, attacker, defender, move)
            (ab::TECHNICIAN, Ev::BasePower, Pre::On) => {
                if modify(relay.num() as u32, self.event.modifier) <= 60 {
                    return self.chain_modify(6144, 4096);
                }
                Res::Undef
            }

            // ---- Telepathy: onTryHit(target, source, move)
            (ab::TELEPATHY, Ev::TryHit, Pre::On) => {
                if let (Some(target), Some(source)) = (e.target, e.source) {
                    if target != source && self.is_ally(target, source) && mcat != Some(Category::Status) {
                        // `-activate|target|ability: Telepathy`
                        self.show_ability(holder, ability);
                        return Res::Null;
                    }
                }
                Res::Undef
            }

            // ---- Thermal Exchange: onDamagingHit(damage, target, source, move)
            (ab::THERMALEXCHANGE, Ev::DamagingHit, Pre::On) => {
                if mtype == Some(Type::Fire) {
                    self.boost1(ATK, 1, None, None, Eff::None);
                }
                Res::Undef
            }

            // ---- Thick Fat: onSourceModifyAtk / onSourceModifySpA(atk, attacker, defender, move)
            (ab::THICKFAT, Ev::ModifyAtk | Ev::ModifySpA, Pre::Source) => {
                if matches!(mtype, Some(Type::Ice | Type::Fire)) {
                    return self.chain_modify(2048, 4096);
                }
                Res::Undef
            }

            // ---- Trace. `ability_st.a` is `effectState.seek`.
            // onStart(pokemon)
            (ab::TRACE, Ev::Start, Pre::On) => {
                let (foes, n) = self.allies_and_self(1 - holder.side as usize);
                let blank = foes[..n].iter().any(|&f| self.mon(f).ability == ab::NOABILITY);
                self.mon_mut(holder).ability_st.a = !blank as i16;
                if !blank {
                    self.single_event(
                        Ev::Update,
                        Eff::Ability(ab::TRACE),
                        Some(holder),
                        Some(holder),
                        None,
                        Eff::None,
                        Res::Undef,
                    );
                }
                Res::Undef
            }
            // onUpdate(pokemon)
            (ab::TRACE, Ev::Update, Pre::On) => {
                if self.mon(holder).ability_st.a == 0 {
                    return Res::Undef;
                }
                let (foes, n) = self.allies_and_self(1 - holder.side as usize);
                let mut cands = [holder; ACTIVE];
                let mut k = 0;
                for &f in &foes[..n] {
                    let a = self.mon(f).ability;
                    if ABILITIES[a as usize].flags & AF_NOTRACE == 0 && a != ab::NOABILITY {
                        cands[k] = f;
                        k += 1;
                    }
                }
                if k == 0 {
                    return Res::Undef;
                }
                let target = cands[self.rand(k as u32, "trace") as usize];
                let ability = self.mon(target).ability;
                self.set_ability(holder, ability, Some(target), Eff::None);
                Res::Undef
            }

            // ---- Unaware: onAnyModifyBoost(boosts, pokemon)
            (ab::UNAWARE, Ev::ModifyBoost, Pre::Any) => {
                let pokemon = e.target;
                if pokemon == Some(holder) {
                    return Res::Undef;
                }
                if Some(holder) == self.active_pokemon && pokemon == self.active_target {
                    self.event.boosts[DEF] = 0;
                    self.event.boosts[SPD] = 0;
                    self.event.boosts[EVA] = 0;
                }
                if pokemon == self.active_pokemon && Some(holder) == self.active_target {
                    self.event.boosts[ATK] = 0;
                    self.event.boosts[DEF] = 0;
                    self.event.boosts[SPA] = 0;
                    self.event.boosts[ACC] = 0;
                }
                Res::Undef
            }

            // ---- Unburden
            // onAfterUseItem(item, pokemon)
            (ab::UNBURDEN, Ev::AfterUseItem, Pre::On) => {
                if e.target == Some(holder) {
                    self.add_volatile(holder, VolKind::Unburden, None, Eff::None);
                }
                Res::Undef
            }
            // onTakeItem(item, pokemon)
            (ab::UNBURDEN, Ev::TakeItem, Pre::On) => {
                self.add_volatile(holder, VolKind::Unburden, None, Eff::None);
                Res::Undef
            }
            // onEnd(pokemon)
            (ab::UNBURDEN, Ev::End, Pre::On) => {
                self.remove_volatile(holder, VolKind::Unburden);
                Res::Undef
            }

            // ---- Unnerve. `ability_st.a` is `effectState.unnerved`.
            (ab::UNNERVE, Ev::Start, Pre::On) => {
                if self.mon(holder).ability_st.a == 0 {
                    // `-ability|pokemon|Unnerve`
                    self.show_ability(holder, ability);
                }
                self.mon_mut(holder).ability_st.a = 1;
                Res::Undef
            }
            (ab::UNNERVE, Ev::End, Pre::On) => {
                self.mon_mut(holder).ability_st.a = 0;
                Res::Undef
            }
            // onFoeTryEatItem()
            (ab::UNNERVE, Ev::TryEatItem, Pre::Foe) => Res::Bool(self.mon(holder).ability_st.a == 0),

            // ---- Wandering Spirit: onDamagingHit(damage, target, source, move)
            (ab::WANDERINGSPIRIT, Ev::DamagingHit, Pre::On) => {
                if let (Some(m), Some(target), Some(source)) = (mi, e.target, e.source) {
                    if self.makes_contact(m) {
                        self.skill_swap(source, target, true);
                    }
                }
                Res::Undef
            }

            // ---- Water Bubble
            // onSourceModifyAtk / onSourceModifySpA(atk, attacker, defender, move)
            (ab::WATERBUBBLE, Ev::ModifyAtk | Ev::ModifySpA, Pre::Source) => {
                if mtype == Some(Type::Fire) {
                    return self.chain_modify(2048, 4096);
                }
                Res::Undef
            }
            // onModifyAtk / onModifySpA(atk, attacker, defender, move)
            (ab::WATERBUBBLE, Ev::ModifyAtk | Ev::ModifySpA, Pre::On) => {
                if mtype == Some(Type::Water) {
                    return self.chain_modify(2, 1);
                }
                Res::Undef
            }

            // ---- Weak Armor: onDamagingHit(damage, target, source, move)
            (ab::WEAKARMOR, Ev::DamagingHit, Pre::On) => {
                if mcat == Some(Category::Physical) {
                    let mut b = [0i8; 7];
                    b[DEF] = -1;
                    b[SPE] = 2;
                    self.boost(b, e.target, e.target, Eff::None);
                }
                Res::Undef
            }

            _ => unreachable!("no body for ability {} {ev:?} {pre:?}", ABILITIES[ability as usize].id),
        }
    }
}
