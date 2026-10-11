//! Showdown's event system (`Battle#runEvent`, `singleEvent`, `eachEvent`,
//! `fieldEvent`, `findEventHandlers`, `resolvePriority`).
//!
//! Which effect listens to which event, and with what order and priority,
//! comes from tables generated out of Showdown's data (`tables.rs`). The
//! bodies of the callbacks are written by hand in `conditions.rs`,
//! `abilities.rs` and `items.rs`. Collecting handlers through the tables,
//! rather than hard-coding who can react where, matters for more than
//! convenience: Showdown sorts the handlers of every event and shuffles the
//! ones that tie, so a handler that does nothing still changes the random
//! number stream if it ties with another one.

// The nesting mirrors Showdown's code; keep it rather than folding conditions together.
#![allow(clippy::collapsible_if, clippy::collapsible_match)]

use crate::data::*;
use crate::state::*;
use crate::trace;

const INLINE_HANDLERS: usize = 12;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Handler {
    pub eff: Eff,
    /// The effect holder the callback sees. For a side or field condition
    /// run once per Pokémon (hazards at switch-in, Grassy Terrain's healing)
    /// this is that Pokémon, not the side or field the condition sits on.
    pub holder: Holder,
    /// `holder` is such a stand-in.
    pub custom: bool,
    /// The event whose body runs (`Start` for an `onStart` run at switch-in).
    pub ev: Ev,
    pub pre: Pre,
    pub has_cb: bool,
    pub order: u32,
    pub priority: i16,
    /// Holder's cached speed times four (switch-in handlers subtract their
    /// place in the speed order, which Showdown expresses in quarters); zero
    /// when the holder is a side or the field.
    pub speed: i32,
    pub sub_order: u8,
    pub effect_order: u32,
    /// `abilityState.effectOrder` of a Pokémon holder, used by `compareRedirectOrder`.
    pub ability_order: u32,
    pub uid: u16,
    /// Detached field timers still expire before Showdown checks handler identity.
    pub field_duration: u8,
    /// Position of the handler's target in a multi-target event.
    pub index: u8,
}

const BLANK: Handler = Handler {
    eff: Eff::None,
    holder: Holder::Field,
    custom: false,
    ev: Ev::Start,
    pre: Pre::On,
    has_cb: false,
    order: 0,
    priority: 0,
    speed: 0,
    sub_order: 0,
    effect_order: 0,
    ability_order: 0,
    uid: 0,
    field_duration: 0,
    index: 0,
};

/// The handlers collected for one event. Nearly every event has only a few,
/// so they live in a small inline array; the rare larger set spills to the heap.
pub(crate) struct HList {
    n: usize,
    inline: [Handler; INLINE_HANDLERS],
    spill: Vec<Handler>,
}

impl HList {
    fn new() -> HList {
        HList { n: 0, inline: [BLANK; INLINE_HANDLERS], spill: Vec::new() }
    }
    fn push(&mut self, h: Handler) {
        if self.spill.is_empty() && self.n < INLINE_HANDLERS {
            self.inline[self.n] = h;
        } else {
            if self.spill.is_empty() {
                self.spill.extend_from_slice(&self.inline[..self.n]);
            }
            self.spill.push(h);
        }
        self.n += 1;
    }
    fn insert_front(&mut self, h: Handler) {
        if self.spill.is_empty() && self.n < INLINE_HANDLERS {
            self.inline.copy_within(0..self.n, 1);
            self.inline[0] = h;
        } else {
            if self.spill.is_empty() {
                self.spill.extend_from_slice(&self.inline[..self.n]);
            }
            self.spill.insert(0, h);
        }
        self.n += 1;
    }
    fn as_mut_slice(&mut self) -> &mut [Handler] {
        if self.spill.is_empty() { &mut self.inline[..self.n] } else { &mut self.spill[..] }
    }
    fn get(&self, k: usize) -> Handler {
        if self.spill.is_empty() { self.inline[k] } else { self.spill[k] }
    }
}

/// `Battle#comparePriority` for handlers.
fn cmp_priority(a: &Handler, b: &Handler) -> i64 {
    let ord = |h: &Handler| if h.order == 0 { 1i64 << 32 } else { h.order as i64 };
    let o = ord(a) - ord(b);
    if o != 0 {
        return o;
    }
    let p = b.priority as i64 - a.priority as i64;
    if p != 0 {
        return p;
    }
    let s = b.speed as i64 - a.speed as i64;
    if s != 0 {
        return s;
    }
    let so = a.sub_order as i64 - b.sub_order as i64;
    if so != 0 {
        return so;
    }
    a.effect_order as i64 - b.effect_order as i64
}

/// `Battle.compareLeftToRightOrder`.
fn cmp_left_to_right(a: &Handler, b: &Handler) -> std::cmp::Ordering {
    let ord = |h: &Handler| if h.order == 0 { 1i64 << 32 } else { h.order as i64 };
    ord(a).cmp(&ord(b)).then(b.priority.cmp(&a.priority)).then(a.index.cmp(&b.index))
}

/// `Battle.compareRedirectOrder`. The last key only applies between two
/// handlers whose holders are Pokémon.
fn cmp_redirect(a: &Handler, b: &Handler) -> std::cmp::Ordering {
    let both_mons = matches!(a.holder, Holder::Mon(_)) && matches!(b.holder, Holder::Mon(_));
    b.priority.cmp(&a.priority).then(b.speed.cmp(&a.speed)).then(if both_mons {
        a.ability_order.cmp(&b.ability_order)
    } else {
        std::cmp::Ordering::Equal
    })
}

fn find_cb(cbs: &'static [CbInfo], ev: Ev, pre: Pre) -> Option<&'static CbInfo> {
    cbs.iter().find(|c| c.ev == ev && c.pre == pre)
}

/// The callback of a condition for `ev` with `pre`, after the cheap bit-set test.
fn cond_cb(c: &'static CondData, ev: Ev, pre: Pre) -> Option<&'static CbInfo> {
    let mask = if pre == Pre::On { c.events } else { c.events_pre };
    if mask & ev.bit() == 0 {
        return None;
    }
    find_cb(c.cbs, ev, pre)
}

impl Battle {
    /// `Battle#initEffectState` for an effect starting on a Pokémon.
    pub(crate) fn new_state(&mut self, has_id: bool, holder: MonRef) -> EffState {
        let counted = has_id && self.mon(holder).is_active;
        self.new_state_counted(counted)
    }

    /// `Battle#initEffectState`: `counted` says whether the state takes the
    /// next `effectOrder` (an effect with an id on an active Pokémon or on a
    /// side) or gets 0 (anything else, including everything on the field).
    pub(crate) fn new_state_counted(&mut self, counted: bool) -> EffState {
        let order = if counted {
            let o = self.effect_order;
            self.effect_order += 1;
            o
        } else {
            0
        };
        self.next_uid = self.next_uid.wrapping_add(1);
        EffState { order, uid: self.next_uid, a: 0, b: 0 }
    }

    /// Record that an effect with these callbacks is now somewhere in the
    /// battle. Events nobody has ever listened to are skipped outright.
    pub(crate) fn listen(&mut self, events: u128, events_pre: u128) {
        self.event_mask |= events | events_pre;
        self.event_mask_pre |= events_pre;
    }

    /// Whether `eff` has an unprefixed callback for `ev`.
    pub(crate) fn has_cb(&self, eff: Eff, ev: Ev) -> bool {
        let of = |cbs: &'static [CbInfo]| find_cb(cbs, ev, Pre::On).is_some_and(|c| c.kind == CbKind::Fn);
        match eff {
            Eff::Status(s) => of(STATUS_CONDS[s as usize].cbs),
            Eff::Vol(v) => of(v.data().cbs),
            Eff::Ability(a) => of(ABILITIES[a as usize].cbs),
            Eff::Item(i) => of(ITEMS[i as usize].cbs),
            Eff::SideCond(k) => of(k.data().cbs),
            Eff::SlotCond(k) => of(k.data().cbs),
            Eff::Pseudo(k) => of(k.data().cbs),
            Eff::Weather(k) => of(k.data().cbs),
            Eff::Terrain(k) => of(k.data().cbs),
            Eff::Move(mi) => self.move_has_cb(mi, ev),
            _ => false,
        }
    }

    /// `Battle#resolvePriority`.
    fn resolve(
        &self,
        eff: Eff,
        cb: &CbInfo,
        holder: Holder,
        custom: bool,
        st: EffState,
        default_sub_order: u8,
    ) -> Handler {
        let mut speed = 0;
        let mut ability_order = 0;
        let mut effect_order = 0;
        if matches!(cb.ev, Ev::SwitchIn | Ev::BeforeSwitchIn | Ev::RedirectTarget) {
            // Hazards on one side, or two redirecting abilities, tie on everything
            // else; they go in the order they were created.
            effect_order = st.order;
        }
        if let Holder::Mon(r) = holder {
            let m = self.mon(r);
            speed = m.speed * 4;
            ability_order = m.ability_st.order;
            if matches!(cb.ev, Ev::SwitchIn | Ev::BeforeSwitchIn) {
                // Speed ties between switch-in handlers were settled when the
                // Pokémon were sorted; use that fixed order.
                let fpv = r.side + 2 * m.position;
                let idx = self.speed_order[..self.n_speed_order as usize].iter().position(|&v| v == fpv);
                speed -= idx.map_or(-1, |i| i as i32);
            }
            if eff == Eff::Ability(ab::MAGICBOUNCE) && cb.ev == Ev::TryHitSide && cb.pre == Pre::Ally {
                // Showdown special-cases this one handler to use the raw Speed stat.
                speed = m.stats[5] as i32 * 4;
            }
        }
        Handler {
            eff,
            holder,
            custom,
            ev: if cb.kind == CbKind::StartAlias { Ev::Start } else { cb.ev },
            pre: cb.pre,
            has_cb: cb.kind != CbKind::DurationOnly,
            order: cb.order,
            priority: cb.priority,
            speed,
            sub_order: if cb.sub_order != 0 { cb.sub_order } else { default_sub_order },
            effect_order,
            ability_order,
            uid: st.uid,
            field_duration: match eff {
                Eff::Terrain(_) => self.field.terrain.duration,
                Eff::Weather(_) => self.field.weather.duration,
                _ => 0,
            },
            index: 0,
        }
    }

    /// `Battle#findPokemonEventHandlers`.
    fn find_pokemon_handlers(&self, mon: MonRef, ev: Ev, pre: Pre, get_duration: bool, out: &mut HList) {
        let m = self.mon(mon);
        let bit = ev.bit();
        let holder = Holder::Mon(mon);
        if m.status != Status::None {
            if let Some(cb) = cond_cb(&STATUS_CONDS[m.status as usize], ev, pre) {
                out.push(self.resolve(Eff::Status(m.status), cb, holder, false, m.status_st, 0));
            }
        }
        for v in self.vols(mon).as_slice() {
            if let Some(cb) = cond_cb(v.kind.data(), ev, pre) {
                if cb.kind == CbKind::DurationOnly && !(get_duration && v.duration > 0) {
                    continue;
                }
                out.push(self.resolve(Eff::Vol(v.kind), cb, holder, false, v.st, 2));
            }
        }
        let a = &ABILITIES[m.ability as usize];
        if (if pre == Pre::On { a.events } else { a.events_pre }) & bit != 0 {
            if let Some(cb) = find_cb(a.cbs, ev, pre) {
                out.push(self.resolve(Eff::Ability(m.ability), cb, holder, false, m.ability_st, 7));
            }
        }
        let i = &ITEMS[m.item as usize];
        if (if pre == Pre::On { i.events } else { i.events_pre }) & bit != 0 {
            if let Some(cb) = find_cb(i.cbs, ev, pre) {
                out.push(self.resolve(Eff::Item(m.item), cb, holder, false, m.item_st, 8));
            }
        }
        if (m.position as usize) < ACTIVE {
            for c in self.sides[mon.side as usize].slot_conds[m.position as usize].as_slice() {
                if let Some(cb) = cond_cb(c.kind.data(), ev, pre) {
                    if cb.kind == CbKind::DurationOnly && !(get_duration && c.duration > 0) {
                        continue;
                    }
                    out.push(self.resolve(Eff::SlotCond(c.kind), cb, holder, false, c.st, 3));
                }
            }
        }
    }

    /// `Battle#findSideEventHandlers`. With `custom`, the handlers are run
    /// for that Pokémon instead of for the side.
    fn find_side_handlers(
        &self,
        side: usize,
        ev: Ev,
        pre: Pre,
        get_duration: bool,
        custom: Option<MonRef>,
        out: &mut HList,
    ) {
        let holder = custom.map_or(Holder::Side(side as u8), Holder::Mon);
        for c in self.sides[side].conds.as_slice() {
            if let Some(cb) = cond_cb(c.kind.data(), ev, pre) {
                if cb.kind == CbKind::DurationOnly && !(get_duration && c.duration > 0) {
                    continue;
                }
                out.push(self.resolve(Eff::SideCond(c.kind), cb, holder, custom.is_some(), c.st, 4));
            }
        }
    }

    /// `Battle#findFieldEventHandlers`: pseudo-weathers, then weather, then terrain.
    fn find_field_handlers(&self, ev: Ev, get_duration: bool, custom: Option<MonRef>, out: &mut HList) {
        let holder = custom.map_or(Holder::Field, Holder::Mon);
        let is_custom = custom.is_some();
        for c in self.field.pseudo.as_slice() {
            if let Some(cb) = cond_cb(c.kind.data(), ev, Pre::On) {
                if cb.kind == CbKind::DurationOnly && !(get_duration && c.duration > 0) {
                    continue;
                }
                // A pseudo-weather sorts as a plain condition until one of its
                // handlers has run in an event, and as a field condition after.
                let default = if c.targeted { 5 } else { 2 };
                out.push(self.resolve(Eff::Pseudo(c.kind), cb, holder, is_custom, c.st, default));
            }
        }
        let w = &self.field.weather;
        if w.kind != Weather::None {
            if let Some(cb) = cond_cb(w.kind.data(), ev, Pre::On) {
                if !(cb.kind == CbKind::DurationOnly && !(get_duration && w.duration > 0)) {
                    out.push(self.resolve(Eff::Weather(w.kind), cb, holder, is_custom, w.st, 5));
                }
            }
        }
        let t = &self.field.terrain;
        if t.kind != Terrain::None {
            if let Some(cb) = cond_cb(t.kind.data(), ev, Pre::On) {
                if !(cb.kind == CbKind::DurationOnly && !(get_duration && t.duration > 0)) {
                    out.push(self.resolve(Eff::Terrain(t.kind), cb, holder, is_custom, t.st, 0));
                }
            }
        }
    }

    /// `Battle#findEventHandlers` for a Pokémon target (or none). Nothing
    /// modelled listens to the few events Showdown aims at a side.
    fn find_event_handlers(&self, target: Option<MonRef>, ev: Ev, source: Option<MonRef>, out: &mut HList) {
        // Events normally run through `eachEvent` never have prefixed handlers.
        let prefixed = !matches!(ev, Ev::BeforeTurn | Ev::Update | Ev::Weather | Ev::WeatherChange | Ev::TerrainChange)
            && self.event_mask_pre & ev.bit() != 0;
        let mut side_target = None;
        if let Some(t) = target {
            if self.mon(t).is_active || source.is_some_and(|s| self.mon(s).is_active) {
                self.find_pokemon_handlers(t, ev, Pre::On, false, out);
                if prefixed {
                    let (allies, n) = self.allies_and_self(t.side as usize);
                    for &a in &allies[..n] {
                        self.find_pokemon_handlers(a, ev, Pre::Ally, false, out);
                        self.find_pokemon_handlers(a, ev, Pre::Any, false, out);
                    }
                    let (foes, n) = self.allies_and_self(1 - t.side as usize);
                    for &f in &foes[..n] {
                        self.find_pokemon_handlers(f, ev, Pre::Foe, false, out);
                        self.find_pokemon_handlers(f, ev, Pre::Any, false, out);
                    }
                }
                // The event bubbles up to the target's side.
                side_target = Some(t.side as usize);
            }
        }
        if let Some(s) = source {
            if prefixed {
                self.find_pokemon_handlers(s, ev, Pre::Source, false, out);
            }
        }
        if let Some(ts) = side_target {
            for side in 0..2 {
                if side == ts {
                    self.find_side_handlers(side, ev, Pre::On, false, None, out);
                } else if prefixed {
                    self.find_side_handlers(side, ev, Pre::Foe, false, None, out);
                }
                if prefixed {
                    self.find_side_handlers(side, ev, Pre::Any, false, None, out);
                }
            }
        }
        self.find_field_handlers(ev, false, None, out);
    }

    /// `Battle#suppressingAbility`: a Mold Breaker-style move is in flight
    /// against `target`.
    pub(crate) fn suppressing_ability(&self, target: Option<MonRef>) -> bool {
        let Some(ap) = self.active_pokemon else {
            return false;
        };
        let Some(mi) = self.active_move else {
            return false;
        };
        self.mon(ap).is_active && Some(ap) != target && self.am[mi as usize].ignore_ability
    }

    /// The suppression rules `runEvent` applies to each handler. Returns
    /// whether the handler is skipped.
    fn handler_suppressed(&self, h: &Handler, ev: Ev) -> bool {
        match (h.eff, h.holder) {
            (Eff::Status(s), Holder::Mon(r)) => {
                if self.mon(r).status != s {
                    return true;
                }
            }
            (Eff::Ability(a), Holder::Mon(r)) => {
                if ABILITIES[a as usize].flags & AF_BREAKABLE != 0 && self.suppressing_ability(Some(r)) {
                    return true;
                }
                if ev != Ev::End && self.ignoring_ability(r) {
                    return true;
                }
            }
            (Eff::Item(_), Holder::Mon(r)) => {
                if !matches!(ev, Ev::Start | Ev::SwitchIn | Ev::TakeItem) && self.ignoring_item(r) {
                    return true;
                }
            }
            _ => {}
        }
        // Air Lock and Cloud Nine switch off the weather's own handlers and
        // everything that reacts to the Weather event.
        (matches!(h.eff, Eff::Weather(_)) || ev == Ev::Weather)
            && !matches!(ev, Ev::Residual | Ev::End)
            && self.suppressing_weather()
    }

    fn call_handler(&mut self, h: &Handler) -> Res {
        let parent = (self.effect, self.effect_holder);
        self.effect = h.eff;
        self.effect_holder = Some(h.holder);
        // `this.effectState.target = effectHolder`: running a handler inside an event leaves
        // its mark on the condition's state.
        if let Eff::Pseudo(kind) = h.eff {
            if let Some(c) = self.field.pseudo.get_mut(kind) {
                c.targeted = true;
            }
        }
        // For a condition on a position the holder is the Pokémon standing there, and that
        // matters: see `field_event`. (Only Healing Wish has a handler that runs this way,
        // `onSwap`, when Ally Switch moves a Pokémon onto it.)
        if let (Eff::SlotCond(kind), Holder::Mon(r)) = (h.eff, h.holder) {
            let (side, pos) = (r.side as usize, self.mon(r).position as usize);
            if let Some(c) = self.sides[side].slot_conds.get_mut(pos).and_then(|cs| cs.get_mut(kind)) {
                c.targeted = true;
            }
        }
        let r = self.dispatch(h.eff, h.ev, h.pre, Some(h.holder));
        (self.effect, self.effect_holder) = parent;
        r
    }

    /// The hand-written body of one callback. `holder` is what the effect
    /// sits on (`this.effectState.target`).
    fn dispatch(&mut self, eff: Eff, ev: Ev, pre: Pre, holder: Option<Holder>) -> Res {
        match (eff, holder) {
            (Eff::Move(mi), _) => self.move_cb(mi, ev),
            (Eff::Status(s), Some(Holder::Mon(h))) => self.status_cb(s, ev, h),
            (Eff::Vol(v), Some(Holder::Mon(h))) => self.vol_cb(v, ev, pre, h),
            (Eff::Ability(a), Some(Holder::Mon(h))) => self.ability_cb(a, ev, pre, h),
            (Eff::Item(i), Some(Holder::Mon(h))) => self.item_cb(i, ev, pre, h),
            (Eff::SlotCond(k), Some(Holder::Mon(h))) => self.slot_cb(k, ev, h),
            // A side condition run for one Pokémon is on that Pokémon's side.
            (Eff::SideCond(k), Some(Holder::Side(s))) => self.side_cb(k, ev, pre, s as usize),
            (Eff::SideCond(k), Some(Holder::Mon(h))) => self.side_cb(k, ev, pre, h.side as usize),
            (Eff::Pseudo(k), _) => self.pseudo_cb(k, ev),
            (Eff::Weather(k), _) => self.weather_cb(k, ev),
            (Eff::Terrain(k), _) => self.terrain_cb(k, ev),
            _ => Res::Undef,
        }
    }

    /// `Battle#runEvent` with a single target (or none).
    pub(crate) fn run_event(
        &mut self,
        ev: Ev,
        target: Option<MonRef>,
        source: Option<MonRef>,
        effect: Eff,
        relay: Res,
    ) -> Res {
        if self.event_mask & ev.bit() == 0 {
            return if relay == Res::Undef { TRUE } else { relay };
        }
        self.run_event_ex(Event::new(ev, target, source, effect), relay, false, false).0
    }

    /// Whether any effect in this battle listens to `ev` at all.
    pub(crate) fn listens(&self, ev: Ev) -> bool {
        self.event_mask & ev.bit() != 0
    }

    /// `runEvent` given a prepared event (for relay variables that are not
    /// plain numbers); returns the event as the handlers left it.
    pub(crate) fn run_event_ex(&mut self, e: Event, relay: Res, on_effect: bool, fast_exit: bool) -> (Res, Event) {
        let ev = e.id.expect("event without id");
        let own = on_effect && self.has_cb(e.effect, ev);
        if self.event_mask & ev.bit() == 0 && !own {
            return (if relay == Res::Undef { TRUE } else { relay }, e);
        }
        let mut hl = HList::new();
        if self.event_mask & ev.bit() != 0 {
            self.find_event_handlers(e.target, ev, e.source, &mut hl);
        }
        if own {
            // The causing effect's own callback goes first.
            let holder = e.target.expect("onEffect without a target");
            let m = self.mon(holder);
            let h = Handler {
                eff: e.effect,
                holder: Holder::Mon(holder),
                ev,
                has_cb: true,
                speed: m.speed * 4,
                ability_order: m.ability_st.order,
                sub_order: match e.effect {
                    Eff::Ability(_) => 7,
                    Eff::Item(_) => 8,
                    Eff::Vol(_) => 2,
                    _ => 0,
                },
                ..BLANK
            };
            hl.insert_front(h);
        }
        let mut relays = [Res::Undef; 1];
        let r = self.run_handlers(e, ev, &mut hl, relay, fast_exit, None, &mut relays);
        (r.0, r.1)
    }

    /// `runEvent` with an array of targets. `relays` holds the per-target relay
    /// variables on entry (`Res::Undef` for "none": Showdown starts those at
    /// `true`) and the per-target results on return.
    pub(crate) fn run_event_multi(
        &mut self,
        ev: Ev,
        targets: &[MonRef],
        source: Option<MonRef>,
        effect: Eff,
        relays: &mut [Res],
        has_relay: bool,
    ) {
        if !has_relay {
            for r in relays.iter_mut() {
                *r = TRUE;
            }
        }
        if self.event_mask & ev.bit() == 0 {
            return;
        }
        let mut hl = HList::new();
        for (i, &t) in targets.iter().enumerate() {
            let start = hl.n;
            self.find_event_handlers(Some(t), ev, source, &mut hl);
            for h in &mut hl.as_mut_slice()[start..] {
                h.index = i as u8;
            }
        }
        let e = Event::new(ev, None, source, effect);
        let relay = if has_relay { Res::Num(0) } else { Res::Undef };
        self.run_handlers(e, ev, &mut hl, relay, false, Some(targets), relays);
    }

    #[allow(clippy::too_many_arguments)]
    fn run_handlers(
        &mut self,
        e: Event,
        ev: Ev,
        hl: &mut HList,
        relay: Res,
        fast_exit: bool,
        targets: Option<&[MonRef]>,
        target_relays: &mut [Res],
    ) -> (Res, Event) {
        let n = hl.n;
        if n == 0 {
            // Nobody listens: the relay variable comes straight back.
            return (if relay == Res::Undef { TRUE } else { relay }, e);
        }
        if matches!(ev, Ev::Invulnerability | Ev::TryHit | Ev::DamagingHit) {
            hl.as_mut_slice().sort_by(cmp_left_to_right);
        } else if fast_exit {
            hl.as_mut_slice().sort_by(cmp_redirect);
        } else {
            self.speed_sort(hl.as_mut_slice(), cmp_priority, "event handler tie");
        }
        let has_relay = relay != Res::Undef;
        let mut relay = if has_relay { relay } else { TRUE };

        let parent = self.event;
        self.event = e;
        self.event.modifier = 4096;
        self.event_depth += 1;
        debug_assert!(self.event_depth < 12, "event stack too deep in {ev:?}");

        for k in 0..n {
            let h = hl.get(k);
            if let Some(ts) = targets {
                let i = h.index as usize;
                let cur = target_relays[i];
                if !cur.truthy() && !(cur == Res::Num(0) && ev == Ev::DamagingHit) {
                    continue;
                }
                self.event.target = Some(ts[i]);
                if has_relay {
                    relay = cur;
                }
            }
            if self.handler_suppressed(&h, ev) {
                continue;
            }
            self.event.relay = relay;
            let ret = if h.has_cb { self.call_handler(&h) } else { Res::Undef };
            if ret != Res::Undef {
                relay = ret;
                if !relay.truthy() || fast_exit {
                    if targets.is_some() {
                        target_relays[h.index as usize] = relay;
                        if target_relays.iter().all(|v| !v.truthy()) {
                            break;
                        }
                    } else {
                        break;
                    }
                }
            }
        }

        self.event_depth -= 1;
        if let Res::Num(v) = relay {
            if v >= 0 && !matches!(ev, Ev::ModifySTAB | Ev::FractionalPriority) {
                relay = Res::Num(crate::battle::modify(v as u32, self.event.modifier) as i32);
            }
        }
        let after = self.event;
        self.event = parent;
        (relay, after)
    }

    /// `Battle#priorityEvent`: stops at the first handler that returns anything.
    pub(crate) fn priority_event(
        &mut self,
        ev: Ev,
        target: Option<MonRef>,
        source: Option<MonRef>,
        effect: Eff,
        relay: Res,
    ) -> Res {
        if self.event_mask & ev.bit() == 0 {
            return if relay == Res::Undef { TRUE } else { relay };
        }
        self.run_event_ex(Event::new(ev, target, source, effect), relay, false, true).0
    }

    /// `Battle#singleEvent`: run one effect's own callback for an event.
    /// `holder` is the Pokémon the effect is on, if it is on one.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn single_event(
        &mut self,
        ev: Ev,
        eff: Eff,
        holder: Option<MonRef>,
        target: Option<MonRef>,
        source: Option<MonRef>,
        source_effect: Eff,
        relay: Res,
    ) -> Res {
        self.single_event_ex(
            ev,
            ev,
            Pre::On,
            eff,
            holder.map(Holder::Mon),
            Event::new(ev, target, source, source_effect),
            relay,
            false,
        )
    }

    /// `singleEvent` for an effect on a side or on the field: the event's
    /// target is that side (or nothing, for the field).
    pub(crate) fn single_event_at(
        &mut self,
        ev: Ev,
        eff: Eff,
        holder: Holder,
        source: Option<MonRef>,
        source_effect: Eff,
    ) -> Res {
        let mut e = Event::new(ev, None, source, source_effect);
        match holder {
            Holder::Mon(r) => e.target = Some(r),
            Holder::Side(s) => e.target_side = Some(s),
            Holder::Field => {}
        }
        self.single_event_ex(ev, ev, Pre::On, eff, Some(holder), e, Res::Undef, false)
    }

    /// `singleEvent` in full: `ev` is the event the suppression rules see,
    /// `body` the callback that runs (they differ when an `onStart` runs
    /// during SwitchIn), and `custom` says the caller supplied the callback.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn single_event_ex(
        &mut self,
        ev: Ev,
        body: Ev,
        pre: Pre,
        eff: Eff,
        holder: Option<Holder>,
        e: Event,
        relay: Res,
        custom: bool,
    ) -> Res {
        let relay_var = if relay == Res::Undef { TRUE } else { relay };
        if let (Eff::Status(s), Some(t)) = (eff, e.target) {
            if self.mon(t).status != s {
                return relay_var;
            }
        }
        match eff {
            Eff::Ability(a) => {
                if ev == Ev::SwitchIn
                    && ABILITIES[a as usize].flags & AF_BREAKABLE != 0
                    && self.suppressing_ability(e.target)
                {
                    return relay_var;
                }
                if ev != Ev::End && e.target.is_some_and(|t| self.ignoring_ability(t)) {
                    return relay_var;
                }
            }
            Eff::Item(_) => {
                if !matches!(ev, Ev::Start | Ev::TakeItem | Ev::SetAbility)
                    && e.target.is_some_and(|t| self.ignoring_item(t))
                {
                    return relay_var;
                }
            }
            Eff::Weather(_) => {
                if !matches!(ev, Ev::FieldStart | Ev::FieldResidual | Ev::FieldEnd) && self.suppressing_weather() {
                    return relay_var;
                }
            }
            _ => {}
        }
        if !custom && !self.has_cb(eff, body) {
            return relay_var;
        }
        let parent = (self.effect, self.effect_holder, self.event);
        self.effect = eff;
        self.effect_holder = holder;
        self.event = e;
        self.event.relay = relay_var;
        self.event_depth += 1;
        debug_assert!(self.event_depth < 12, "event stack too deep in single {ev:?}");
        let ret = self.dispatch(eff, body, pre, holder.or(e.target.map(Holder::Mon)));
        self.event_depth -= 1;
        (self.effect, self.effect_holder, self.event) = parent;
        if ret == Res::Undef { relay_var } else { ret }
    }

    /// `Battle#eachEvent`: run an event on every active Pokémon in speed order.
    pub(crate) fn each_event(&mut self, ev: Ev) {
        let effect = self.effect;
        self.each_event_from(ev, effect);
    }

    /// `eachEvent` with the causing effect given (it defaults to the running one).
    pub(crate) fn each_event_from(&mut self, ev: Ev, effect: Eff) {
        let effect = if effect == Eff::None { self.effect } else { effect };
        let (actives, n) = self.all_active(false);
        let mut keyed = [(actives[0], 0i32); 4];
        for i in 0..n {
            keyed[i] = (actives[i], self.mon(actives[i]).speed);
        }
        self.speed_sort(&mut keyed[..n], |a, b| b.1 as i64 - a.1 as i64, trace::each_label(ev));
        if self.event_mask & ev.bit() != 0 {
            for &(r, _) in &keyed[..n] {
                self.run_event_ex(Event::new(ev, Some(r), None, effect), Res::Undef, false, false);
            }
        }
        if ev == Ev::Weather {
            self.each_event(Ev::Update);
        }
    }

    /// The identity of the live instance of the effect a handler was collected for.
    fn live_uid(&self, h: &Handler) -> Option<u16> {
        match (h.eff, h.holder) {
            (Eff::Ability(_), Holder::Mon(r)) => Some(self.mon(r).ability_st.uid),
            (Eff::Item(_), Holder::Mon(r)) => Some(self.mon(r).item_st.uid),
            (Eff::Status(_), Holder::Mon(r)) => Some(self.mon(r).status_st.uid),
            (Eff::Vol(kind), Holder::Mon(r)) => self.vols(r).get(kind).map(|v| v.st.uid),
            (Eff::SlotCond(kind), Holder::Mon(r)) => {
                let pos = self.mon(r).position as usize;
                if pos < ACTIVE {
                    self.sides[r.side as usize].slot_conds[pos].get(kind).map(|c| c.st.uid)
                } else {
                    None
                }
            }
            (Eff::SideCond(kind), Holder::Side(s)) => self.sides[s as usize].conds.get(kind).map(|c| c.st.uid),
            (Eff::SideCond(kind), Holder::Mon(r)) => self.sides[r.side as usize].conds.get(kind).map(|c| c.st.uid),
            (Eff::Pseudo(kind), _) => self.field.pseudo.get(kind).map(|c| c.st.uid),
            (Eff::Weather(kind), _) => (self.field.weather.kind == kind).then_some(self.field.weather.st.uid),
            (Eff::Terrain(kind), _) => (self.field.terrain.kind == kind).then_some(self.field.terrain.st.uid),
            _ => None,
        }
    }

    /// The Residual countdown of a handler's effect: returns true if the
    /// duration ran out and the effect was ended.
    fn tick_duration(&mut self, h: &Handler) -> bool {
        let uid = h.uid;
        // Count down, and report whether the instance has just expired.
        fn tick<K: Copy>(c: Option<&mut Cond<K>>, uid: u16) -> bool {
            match c {
                Some(c) if c.st.uid == uid && c.duration > 0 => {
                    c.duration -= 1;
                    c.duration == 0
                }
                _ => false,
            }
        }
        match (h.eff, h.holder) {
            (Eff::Vol(kind), Holder::Mon(r)) => {
                let Some(list) = self.vols_mut(r) else {
                    return false;
                };
                if tick(list.get_mut(kind), uid) {
                    self.remove_volatile(r, kind);
                    return true;
                }
            }
            (Eff::SlotCond(kind), Holder::Mon(r)) => {
                let pos = self.mon(r).position as usize;
                if pos < ACTIVE && tick(self.sides[r.side as usize].slot_conds[pos].get_mut(kind), uid) {
                    self.remove_slot_condition(r.side as usize, pos, kind);
                    return true;
                }
            }
            (Eff::SideCond(kind), Holder::Side(s)) => {
                if tick(self.sides[s as usize].conds.get_mut(kind), uid) {
                    self.remove_side_condition(s as usize, kind);
                    return true;
                }
            }
            (Eff::Pseudo(kind), Holder::Field) => {
                if tick(self.field.pseudo.get_mut(kind), uid) {
                    self.remove_pseudo_weather(kind);
                    return true;
                }
            }
            (Eff::Weather(kind), Holder::Field) => {
                let w = &mut self.field.weather;
                let detached_expiry = (w.kind != kind || w.st.uid != uid) && h.field_duration == 1;
                if detached_expiry || (w.kind == kind && tick(Some(w), uid)) {
                    self.clear_weather();
                    return true;
                }
            }
            (Eff::Terrain(kind), Holder::Field) => {
                let t = &mut self.field.terrain;
                let detached_expiry = (t.kind != kind || t.st.uid != uid) && h.field_duration == 1;
                if detached_expiry || (t.kind == kind && tick(Some(t), uid)) {
                    self.clear_terrain();
                    return true;
                }
            }
            // Statuses, abilities and items never carry a duration; a side or
            // field condition run for one Pokémon does not count down there.
            _ => {}
        }
        false
    }

    /// `Battle#fieldEvent`, used for Residual and SwitchIn: every effect on
    /// the field takes its turn in one global order.
    pub(crate) fn field_event(&mut self, ev: Ev, targets: Option<&[MonRef]>) {
        let get_duration = ev == Ev::Residual;
        let mut hl = HList::new();
        // `onFieldResidual` / `onSideResidual`: the condition's own turn, on the
        // field or side itself. (Nothing has an onFieldSwitchIn or onSideSwitchIn.)
        if ev == Ev::Residual {
            self.find_field_handlers(Ev::FieldResidual, true, None, &mut hl);
        }
        for side in 0..2 {
            if ev == Ev::Residual {
                self.find_side_handlers(side, Ev::SideResidual, Pre::On, true, None, &mut hl);
            }
            for pos in 0..ACTIVE {
                let active = self.active(side, pos);
                if !self.in_play(active) {
                    continue;
                }
                if ev == Ev::SwitchIn && self.event_mask_pre & ev.bit() != 0 {
                    self.find_pokemon_handlers(active, ev, Pre::Any, false, &mut hl);
                }
                if targets.is_some_and(|ts| !ts.contains(&active)) {
                    continue;
                }
                self.find_pokemon_handlers(active, ev, Pre::On, get_duration, &mut hl);
                // `onResidual` / `onSwitchIn` of side and field conditions, once per Pokémon.
                self.find_side_handlers(side, ev, Pre::On, false, Some(active), &mut hl);
                self.find_field_handlers(ev, false, Some(active), &mut hl);
            }
        }
        let n = hl.n;
        self.speed_sort(
            hl.as_mut_slice(),
            cmp_priority,
            if get_duration { "residual handler tie" } else { "switch-in handler tie" },
        );
        for k in 0..n {
            let h = hl.get(k);
            if let Holder::Mon(r) = h.holder {
                if self.mon(r).fainted && !matches!(h.eff, Eff::SlotCond(_)) {
                    continue;
                }
            }
            if ev == Ev::Residual && !h.custom && self.tick_duration(&h) {
                if self.ended {
                    return;
                }
                continue;
            }
            // The effect may have been removed or replaced by an earlier handler.
            // (Showdown does not check slot conditions.)
            if !matches!(h.eff, Eff::SlotCond(_)) && self.live_uid(&h) != Some(h.uid) {
                continue;
            }
            // What it does do to them is a slip. Once a slot condition's handler has run
            // inside an event (`call_handler`), its state names a Pokémon as its target, and
            // the check above then looks for the condition among that Pokémon's volatiles,
            // does not find it, and passes it over. So a Healing Wish that Ally Switch has
            // moved a healthy Pokémon onto no longer heals whoever switches in there; it
            // waits for the next Ally Switch.
            if let (Eff::SlotCond(kind), Holder::Mon(r)) = (h.eff, h.holder) {
                let (side, pos) = (r.side as usize, self.mon(r).position as usize);
                let slot = self.sides[side].slot_conds.get(pos).and_then(|cs| cs.get(kind));
                if slot.is_some_and(|c| c.targeted) {
                    continue;
                }
            }
            if h.has_cb {
                // The event is named after what the handler sits on.
                let (event_id, mut e) = match (h.holder, ev) {
                    (Holder::Side(_), Ev::Residual) => {
                        (Ev::SideResidual, Event::new(Ev::SideResidual, None, None, Eff::None))
                    }
                    (Holder::Field, Ev::Residual) => {
                        (Ev::FieldResidual, Event::new(Ev::FieldResidual, None, None, Eff::None))
                    }
                    _ => (ev, Event::new(ev, None, None, Eff::None)),
                };
                match h.holder {
                    Holder::Mon(r) => e.target = Some(r),
                    Holder::Side(s) => e.target_side = Some(s),
                    Holder::Field => {}
                }
                self.single_event_ex(event_id, h.ev, h.pre, h.eff, Some(h.holder), e, Res::Undef, true);
            }
            self.faint_messages(false, false, true);
            if self.ended {
                return;
            }
        }
    }

    // ------------------------------------------------------------- modifiers

    /// `Battle#chainModify` with a fraction: multiply the running event's modifier.
    pub(crate) fn chain_modify(&mut self, num: u32, den: u32) -> Res {
        let next = num * 4096 / den;
        self.event.modifier = (self.event.modifier * next + 2048) >> 12;
        Res::Undef
    }

    /// `Battle#finalModify`: apply the accumulated modifier now and reset it.
    pub(crate) fn final_modify(&mut self, value: u32) -> u32 {
        let v = crate::battle::modify(value, self.event.modifier);
        self.event.modifier = 4096;
        v
    }
}
