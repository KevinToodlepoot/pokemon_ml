// Shared helpers for the Showdown oracle scripts.
'use strict';
const path = require('path');
const PS = require(path.join(process.env.PS_DIR || path.join(__dirname, 'pokemon-showdown'), 'dist', 'sim'));

const MOD = 'champions';
const FORMAT = 'gen9championsvgc2026regmc';
const dex = PS.Dex.mod(MOD);

const STAT_IDS = ['hp', 'atk', 'def', 'spa', 'spd', 'spe'];
const BOOST_IDS = ['atk', 'def', 'spa', 'spd', 'spe', 'accuracy', 'evasion'];
const STATUSES = ['brn', 'par', 'psn', 'tox', 'slp', 'frz'];

// Move properties every move object carries; they are either read directly by
// the generator or irrelevant to battle mechanics.
const BASE_KEYS = new Set([
	'name', 'id', 'fullname', 'effectType', 'exists', 'num', 'gen', 'isNonstandard', 'duration', 'noCopy',
	'affectsFainted', 'status', 'weather', 'sourceEffect', 'type', 'target', 'basePower', 'accuracy', 'critRatio',
	'baseMoveType', 'secondary', 'secondaries', 'priority', 'category',
	'overrideOffensiveStat', 'overrideOffensivePokemon', 'overrideDefensiveStat', 'overrideDefensivePokemon',
	'ignoreNegativeOffensive', 'ignorePositiveDefensive', 'ignoreOffensive', 'ignoreDefensive', 'ignoreImmunity',
	'pp', 'noPPBoosts', 'isZ', 'isMax', 'flags', 'selfSwitch', 'ignoreAbility', 'damage', 'spreadHit', 'forceSTAB',
	'volatileStatus', 'maxMove', 'zMove', 'contestType', 'desc', 'shortDesc', 'realMove', 'nonGhostTarget',
	'pressureTarget',
]);
// Extra declarative properties the Rust engine implements.
const EXTRA_OK = new Set([
	'boosts', 'self', 'drain', 'recoil', 'heal', 'thawsTarget', 'willCrit', 'ignoreEvasion', 'multihit',
	'stallingMove', 'struggleRecoil', 'condition', 'sideCondition', 'slotCondition', 'pseudoWeather', 'terrain',
	'breaksProtect', 'selfdestruct', 'selfBoost', 'hasCrashDamage', 'mindBlownRecoil',
	'tracksTarget', 'sleepUsable', 'ohko', 'multiaccuracy', 'tags', 'smartTarget', 'callsMove', 'forceSwitch', 'hasSheerForceBoost',
]);
const TARGETS_OK = new Set([
	'normal', 'any', 'adjacentFoe', 'allAdjacentFoes', 'allAdjacent', 'self', 'adjacentAlly', 'adjacentAllyOrSelf', 'allies',
	'randomNormal', 'all', 'allySide', 'foeSide', 'scripted', 'allyTeam',
]);
const BAD_FLAGS = ['pledgecombo'];
// Moves whose script callbacks (onHit, onTry, basePowerCallback, ...) all have a
// hand-written body in src/movecbs.rs. A move with callbacks that is not listed
// here is rejected by `Battle::new`.
const HAND_MOVES = new Set(('protect detect struggle auroraveil ' +
	// weather, terrain, screens and hazards
	'blizzard hurricane thunder weatherball growth moonlight morningsun synthesis expandingforce risingvoltage ' +
	'grassyglide terrainpulse steelroller icespinner brickbreak psychicfangs defog rapidspin mortalspin tidyup ' +
	'courtchange magneticflux haze ' +
	// moves that depend on the flow of the turn, and the volatile conditions
	'fakeout firstimpression followme ragepowder helpinghand wideguard quickguard endure banefulbunker kingsshield ' +
	'spikyshield disable attract substitute leechseed magnetrise noretreat octolock electrify gastroacid lockon ' +
	'perishsong yawn stockpile spitup swallow destinybond sparklingaria block meanlook jawlock throatchop ' +
	'spiritshackle ' +
	// moves that take more than one turn
	'fly dig dive bounce phantomforce solarbeam solarblade skyattack meteorbeam electroshot uproar ' +
	// moves prepared before the turn, fixed damage, crashes and self-destruction
	'focuspunch beakblast counter mirrorcoat superfang endeavor finalgambit mistyexplosion highjumpkick axekick ' +
	'supercellslam steelbeam ' +
	// base power that depends on the state of the battle
	'acrobatics assurance avalanche electroball eruption waterspout flail reversal gyroball hardpress hex ' +
	'infernalparade lastrespects payback powertrip storedpower ragefist stompingtantrum temperflare watershuriken ' +
	'barbbarrage venoshock facade ficklebeam gravapple lashout grassknot lowkick heatcrash heavyslam ' +
	'burningjealousy alluringvoice fellstinger ' +
	// HP, stat stages and PP
	'healbell healpulse painsplit strengthsap bellydrum clangoroussoul acupressure psychup topsyturvy clearsmog ' +
	'guardsplit powersplit guardswap powerswap speedswap spite rest direclaw triattack eeriespell ' +
	// abilities and types changed by moves
	'skillswap entrainment roleplay simplebeam worryseed soak magicpowder forestscurse trickortreat reflecttype ' +
	'burnup doubleshock ' +
	// moves that take, swap or eat items
	'knockoff thief covet trick switcheroo bugbite pluck corrosivegas recycle poltergeist belch stuffcheeks ' +
	'teatime ' +
	// assorted
	'suckerpunch upperhand lastresort flyingpress freezedry ragingbull snore ceaselessedge stoneaxe ' +
	'tripleaxel pollenpuff shellsidearm beatup curse metalburst comeuppance ' +
	// the order of the turn, and moves that use other moves
	'afteryou quash round instruct copycat sleeptalk ' +
	'futuresight fling allyswitch ' +
	// switching
	'partingshot chillyreception batonpass shedtail healingwish revivalblessing transform aurawheel').split(' '));
// Callback-like move properties, mapped to the event the Rust engine files them under.
const MOVE_CALLBACKS = {
	basePowerCallback: 'BasePowerCallback', damageCallback: 'DamageCallback', beforeMoveCallback: 'BeforeMoveCallback',
	beforeTurnCallback: 'BeforeTurnCallback', priorityChargeCallback: 'PriorityChargeCallback',
};

/** The script callbacks of a move as event names (`onTry` -> `Try`). */
function moveCallbacks(m) {
	const out = [];
	for (const k of Object.keys(m)) {
		if (typeof m[k] !== 'function') continue;
		if (MOVE_CALLBACKS[k]) out.push(MOVE_CALLBACKS[k]);
		else if (k.startsWith('on')) out.push(k.slice(2));
		else throw new Error(`${m.id}: unexpected function property ${k}`);
	}
	// Fling gives itself an onHit while it runs (the thrown item's effect).
	if (m.id === 'fling') out.push('Hit');
	return out;
}

/** Which condition list a condition id belongs to, or null if it is not modelled. */
function condClass(id) {
	id = PS.toID(id);
	if (VOLATILES.includes(id)) return 'volatile';
	if (SIDE_CONDS.includes(id)) return 'side';
	if (SLOT_CONDS.includes(id)) return 'slot';
	if (PSEUDO_WEATHERS.includes(id)) return 'pseudo';
	if (WEATHERS.includes(id)) return 'weather';
	if (TERRAINS.includes(id)) return 'terrain';
	return null;
}

function hitEffectReasons(e, where, why, moveId) {
	for (const k in e) {
		if (k === 'chance' || k === 'dustproof') continue;
		if (k === 'boosts') continue;
		if (k === 'status') {
			if (!STATUSES.includes(e.status)) why.push(`${where}.status=${e.status}`);
		} else if (k === 'volatileStatus') {
			if (condClass(e.volatileStatus) !== 'volatile') why.push(`${where}.volatile=${e.volatileStatus}`);
		} else if (k === 'onHit') {
			if (!HAND_MOVES.has(moveId)) why.push(`${where}.onHit`);
		} else if (k === 'self') {
			for (const sk in e.self) if (sk !== 'boosts') why.push(`${where}.self.${sk}`);
		} else {
			why.push(`${where}.${k}`);
		}
	}
}

/** Returns [] if the Rust engine models every effect of this move, else the reasons it does not. */
function unsupportedReasons(m) {
	const why = [];
	if (m === RECHARGE) return ['placeholder'];
	const hand = HAND_MOVES.has(m.id);
	if (PENDING_MOVES.has(m.id)) why.push('pending');
	for (const k in m) {
		// (A mod can blank out a property it inherits: Champions removes Belch's onDisableMove.)
		if (BASE_KEYS.has(k) || EXTRA_OK.has(k) || m[k] === undefined) continue;
		if (typeof m[k] === 'function') {
			if (!hand) why.push('fn:' + k);
			continue;
		}
		// Ordering metadata of a callback (onTryHitPriority and the like) goes with the callback.
		if (/^on.*(Priority|Order|SubOrder)$/.test(k) && hand) continue;
		why.push('key:' + k);
	}
	if (!TARGETS_OK.has(m.target)) why.push('target:' + m.target);
	if (m.status && !STATUSES.includes(m.status)) why.push('status:' + m.status);
	if (m.volatileStatus && condClass(m.volatileStatus) !== 'volatile') why.push('volatile:' + m.volatileStatus);
	if (m.sideCondition && condClass(m.sideCondition) !== 'side') why.push('sideCondition:' + m.sideCondition);
	if (m.slotCondition && condClass(m.slotCondition) !== 'slot') why.push('slotCondition:' + m.slotCondition);
	if (m.pseudoWeather && condClass(m.pseudoWeather) !== 'pseudo') why.push('pseudoWeather:' + m.pseudoWeather);
	if (m.weather && condClass(m.weather) !== 'weather') why.push('weather:' + m.weather);
	if (m.terrain && condClass(m.terrain) !== 'terrain') why.push('terrain:' + m.terrain);
	// A move's own `condition` block is the definition of the condition named after the move.
	if (m.condition && !condClass(m.id)) why.push('condition');
	if (m.damage && m.damage !== 'level') why.push('damage:' + m.damage);
	if (m.isZ || m.isMax) why.push('zmax');
	if (m.forceSTAB) why.push('forceSTAB');
	if (m.ignoreImmunity && m.ignoreImmunity !== true) why.push('ignoreImmunity:map');
	// (A future move never hits on the turn it is used; the hit that lands later has its own data.)
	if (m.ignoreImmunity === true && m.category !== 'Status' && !m.flags.futuremove) why.push('ignoreImmunity');
	if (m.ignoreNegativeOffensive || m.ignorePositiveDefensive || m.ignoreOffensive) why.push('ignoreBoosts');
	if (m.overrideDefensivePokemon) why.push('overrideDefensivePokemon');
	for (const f of BAD_FLAGS) if (m.flags[f]) why.push('flag:' + f);
	if (m.self) {
		for (const sk in m.self) {
			if (sk === 'boosts' || sk === 'chance') continue;
			if (sk === 'volatileStatus' && condClass(m.self.volatileStatus) === 'volatile') continue;
			if (sk === 'onHit' && hand) continue;
			why.push('self.' + sk);
		}
	}
	if (m.secondaries) for (const s of m.secondaries) hitEffectReasons(s, 'sec', why, m.id);
	return why;
}

/** Species that can be brought to a Champions battle (Megas are reached in battle, not brought). */
function legalSpecies() {
	return dex.species.all().filter(s => s.exists && !s.isNonstandard && s.tier !== 'Illegal');
}

function learnableMoves(species) {
	// Champions learnsets are stored per species; formes without their own entry use the base species'.
	let ls = dex.species.getLearnsetData(species.id);
	if (!(ls && ls.learnset)) ls = dex.species.getLearnsetData(dex.species.get(species.baseSpecies).id);
	if (!(ls && ls.learnset)) return [];
	return Object.keys(ls.learnset).filter(id => {
		const m = dex.moves.get(id);
		return m.exists && !m.isNonstandard;
	}).sort();
}

// What Showdown queues for a Pokémon that must recharge: a move id with no entry in the
// dex, which resolves to a blank move (no type, no category, no target). It is never
// carried out, but it is sorted into the queue and given a target like any other.
// For the purposes that leaves, a blank target behaves like 'randomNormal'.
const RECHARGE = {
	id: 'recharge', name: 'Recharge', exists: false, type: '???', category: 'Physical', basePower: 0, accuracy: true,
	pp: 1, noPPBoosts: true, priority: 0, target: 'randomNormal', flags: {}, critRatio: 1,
};

/** Every move the generated Rust table contains, in table order. */
function tableMoves() {
	const ids = new Set(dex.moves.all().filter(m => m.exists && !m.isNonstandard).map(m => m.id));
	for (const s of legalSpecies()) for (const m of learnableMoves(s)) ids.add(m);
	ids.add('struggle');
	ids.add(RECHARGE.id);
	return [...ids].sort().map(id => (id === RECHARGE.id ? RECHARGE : dex.moves.get(id)));
}

/** Every species the generated Rust table contains, in table order. */
function tableSpecies() {
	return dex.species.all().filter(s => s.exists && !s.isNonstandard).sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
}

/** Small deterministic generator for the harness itself (never the battle's RNG). */
function mulberry32(a) {
	return function () {
		a |= 0; a = (a + 0x6D2B79F5) | 0;
		let t = Math.imul(a ^ (a >>> 15), 1 | a);
		t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
		return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
	};
}

// ---- abilities, items and conditions ------------------------------------------

// Conditions the Rust engine implements. Each list is one Rust enum, in this order;
// every callback of a listed condition has a hand-written body in src/conditions.rs.
// Volatile conditions sit on a Pokémon (`VolKind`).
const VOLATILES = ['protect', 'stall', 'flinch', 'confusion', 'choicelock', 'gem', 'metronome', 'flashfire', 'unburden',
	'followme', 'ragepowder', 'helpinghand', 'endure', 'banefulbunker', 'kingsshield', 'spikyshield',
	'taunt', 'encore', 'disable', 'torment', 'imprison', 'attract', 'healblock', 'substitute',
	'aquaring', 'ingrain', 'leechseed', 'focusenergy', 'dragoncheer', 'magnetrise', 'minimize', 'noretreat', 'trapped',
	'trapper', 'octolock', 'powertrick', 'smackdown', 'saltcure', 'syrupbomb', 'throatchop', 'charge', 'destinybond',
	'electrify', 'gastroacid', 'lockon', 'perishsong', 'yawn', 'glaiverush', 'stockpile', 'sparklingaria',
	'partiallytrapped',
	// moves that take more than one turn
	'lockedmove', 'mustrecharge', 'twoturnmove', 'uproar', 'fly', 'dig', 'dive', 'bounce', 'phantomforce',
	'solarbeam', 'solarblade', 'skyattack', 'meteorbeam', 'electroshot',
	'focuspunch', 'beakblast', 'counter', 'mirrorcoat', 'roost', 'gigatonhammer', 'curse', 'fling', 'allyswitch', 'chillyreception'];
// Volatiles that are only a marker: Showdown has no condition data for them at all.
// (A charging move puts a volatile named after itself on its user, condition or not.)
// (So does a move that cannot be used twice in a row, when it is tried twice in a row.)
const BARE_VOLATILES = new Set(['sparklingaria', 'solarbeam', 'solarblade', 'skyattack', 'meteorbeam', 'electroshot',
	'gigatonhammer']);
// Side conditions sit on one side of the field (`SideCond`).
const SIDE_CONDS = ['tailwind', 'reflect', 'lightscreen', 'auroraveil', 'safeguard', 'spikes', 'toxicspikes', 'stealthrock', 'stickyweb',
	'wideguard', 'quickguard'];
// Slot conditions sit on one active position of a side (`SlotCond`).
const SLOT_CONDS = ['wish', 'futuremove', 'healingwish', 'revivalblessing'];
// Pseudo-weathers sit on the whole field (`Pseudo`).
const PSEUDO_WEATHERS = ['trickroom', 'gravity', 'magicroom', 'wonderroom', 'fairylock'];
// `Weather` and `Terrain`; index 0 of each Rust enum is "none".
const WEATHERS = ['raindance', 'sunnyday', 'sandstorm', 'snowscape'];
const TERRAINS = ['electricterrain', 'grassyterrain', 'mistyterrain', 'psychicterrain'];
// Moves that pass every declarative check but are held back until the engine part
// they need has been written and fuzzed.
const PENDING_MOVES = new Set([]);

// Abilities and items whose every callback has a hand-written body in the Rust
// engine (src/abilities.rs, src/items.rs). Everything else is rejected by
// `Battle::new`. DEFERRED_* records why something is not here yet.
// Abilities: every ability a Champions species can have, except the ones listed
// here with the mechanic they wait for.
const DEFERRED_ABILITIES = {};
const defer = (why, ids) => { for (const id of ids.split(' ')) DEFERRED_ABILITIES[id] = why; };
const SUPPORTED_ABILITIES = new Set(['noability']);
// Modelled effects with a part that never comes up, because it reacts to something
// Champions does not have or only writes to the battle log. They are exact for every
// battle the engine accepts; this list says what to revisit if Showdown's Champions
// data grows.
const DORMANT_PARTS = {
	abilities: {
		anticipation: 'only writes to the battle log',
		battlebond: 'does nothing: Greninja-Bond is not in Champions',
		embodyaspectcornerstone: 'needs Terastallization, which Champions does not have',
		embodyaspecthearthflame: 'needs Terastallization, which Champions does not have',
		embodyaspectteal: 'needs Terastallization, which Champions does not have',
		embodyaspectwellspring: 'needs Terastallization, which Champions does not have',
		forewarn: 'only writes to the battle log (its random pick is still drawn, from the same number of candidates)',
		frisk: 'only writes to the battle log',
		gluttony: 'only matters for pinch berries, which are not in Champions',
		gulpmissile: 'does nothing: Cramorant is not in Champions',
		iceface: 'does nothing: Eiscue is not in Champions',
		parentalbond: 'its Secret Power special case (not in Champions)',
		shieldsdown: 'does nothing: Minior is not in Champions',
		terashell: 'does nothing: Terapagos is not in Champions',
	},
	items: {
	},
};
// Items: everything except the ones listed here with the mechanic they wait for. (A Mega Stone is
// accepted on any Pokémon; whether the Mega it leads to is modelled is checked when the battle is built.)
const DEFERRED_ITEMS = {};
const SUPPORTED_ITEMS = new Set(['']);
for (const item of dex.items.all()) {
	if (!item.exists || item.isNonstandard) continue;
	if (!DEFERRED_ITEMS[item.id]) SUPPORTED_ITEMS.add(item.id);
}

/** Event names, in the order of the Rust `Ev` enum (src/data.rs is the single source). */
const EVENTS = (() => {
	const src = require('fs').readFileSync(path.join(__dirname, '..', 'src', 'data.rs'), 'utf8');
	const body = src.match(/pub enum Ev \{([\s\S]*?)\n\}/)[1];
	return body.split('\n').map(l => l.replace(/\/\/.*$/, '').trim().replace(/,$/, '')).filter(Boolean);
})();
const PREFIXES = ['Ally', 'Foe', 'Any', 'Source'];
const META = /(Priority|Order|SubOrder)$/;

/**
 * The event callbacks of an ability, item or condition, with the ordering
 * metadata `Battle#resolvePriority` would give them. `cls` says where a
 * condition sits: 'volatile', 'status', 'side', 'slot', 'pseudo', 'weather' or 'terrain'.
 */
function callbacks(effect, cls) {
	const out = [];
	const subOrderDefault = () => {
		// `effectTypeOrder` in resolvePriority; statuses and terrains are not in it.
		if (cls === 'volatile') return 2;
		if (cls === 'slot') return 3;
		if (cls === 'side') return 4;
		// A pseudo-weather's state has no target until one of its handlers has run in an
		// event, so its default flips from 2 to 5 during the battle; 0 = let the engine decide.
		if (cls === 'pseudo') return 0;
		if (cls === 'weather') return 5;
		if (cls === 'status' || cls === 'terrain') return 0;
		if (effect.effectType === 'Ability') {
			if (effect.name === 'Poison Touch' || effect.name === 'Perish Body') return 6;
			if (effect.name === 'Stall') return 9;
			return 7;
		}
		if (effect.effectType === 'Item') return 8;
		throw new Error(`no default subOrder for ${effect.id} (${cls})`);
	};
	const meta = key => ({
		order: effect[key + 'Order'] || 0,
		priority: effect[key + 'Priority'] || 0,
		subOrder: effect[key + 'SubOrder'] || subOrderDefault(),
	});
	const parse = rest => {
		if (EVENTS.includes(rest)) return [rest, 'On'];
		for (const p of PREFIXES) {
			if (rest.startsWith(p) && EVENTS.includes(rest.slice(p.length))) return [rest.slice(p.length), p];
		}
		return [null, 'On'];
	};
	for (const key of Object.keys(effect)) {
		if (!key.startsWith('on') || effect[key] === undefined) continue;
		// `onXPriority: 5` is ordering metadata for `onX`. But `onFractionalPriority` and
		// `onModifyPriority` are events in their own right, and may even be constants
		// (Stall's `onFractionalPriority: -0.1`).
		if (typeof effect[key] !== 'function' && META.test(key) && (parse(key.slice(2).replace(META, ''))[0] || /^on(Side|Field)?Residual/.test(key))) continue;
		const [ev, pre] = parse(key.slice(2));
		if (!ev) { out.push({ key, unknown: true }); continue; }
		out.push({ key, ev, pre, kind: 'Fn', constant: typeof effect[key] === 'function' ? undefined : effect[key], ...meta(key) });
	}
	const has = key => effect[key] !== undefined;
	if (['Ability', 'Item'].includes(effect.effectType) && has('onStart') && !has('onSwitchIn') && !has('onAnySwitchIn')) {
		out.push({ key: 'onSwitchIn', ev: 'SwitchIn', pre: 'On', kind: 'StartAlias', ...meta('onSwitchIn') });
	}
	// An effect with a duration takes part in the Residual event even without a callback
	// there, so that the duration counts down (in the order its onXResidualOrder gives).
	const residual = { side: 'SideResidual', pseudo: 'FieldResidual', weather: 'FieldResidual', terrain: 'FieldResidual' }[cls] || 'Residual';
	if ((effect.duration || effect.durationCallback) && !has('on' + residual)) {
		out.push({ key: 'on' + residual, ev: residual, pre: 'On', kind: 'DurationOnly', ...meta('on' + residual) });
	}
	return out;
}

/** Every ability the generated Rust table contains, in table order. */
function tableAbilities() {
	const ids = new Set(dex.abilities.all().filter(a => a.exists && !a.isNonstandard).map(a => a.id));
	for (const s of dex.species.all()) {
		if (!s.exists || s.isNonstandard) continue;
		for (const k in s.abilities) ids.add(PS.toID(s.abilities[k]));
	}
	ids.add('noability');
	return [...ids].sort().map(id => dex.abilities.get(id));
}

/** Abilities some Champions species (Megas included) can have. */
// (SUPPORTED_ABILITIES is filled in below, once legalAbilities exists.)
function legalAbilities() {
	const ids = new Set();
	for (const s of dex.species.all()) {
		if (!s.exists || s.isNonstandard) continue;
		for (const k in s.abilities) ids.add(PS.toID(s.abilities[k]));
	}
	return [...ids].sort();
}

/** Every held item the generated Rust table contains, in table order (index 0 is "no item"). */
function tableItems() {
	return dex.items.all().filter(i => i.exists && !i.isNonstandard).sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
}

for (const id of legalAbilities()) if (!DEFERRED_ABILITIES[id]) SUPPORTED_ABILITIES.add(id);
// Abilities no Champions species has, but which a move can hand out (Simple Beam).
const MOVE_GIVEN_ABILITIES = ['simple'];
for (const id of MOVE_GIVEN_ABILITIES) SUPPORTED_ABILITIES.add(id);
if (process.env.VGC_ABILITIES !== undefined) {
	// Debugging aid: model only the listed abilities.
	const only = new Set(process.env.VGC_ABILITIES.split(',').filter(Boolean));
	for (const id of [...SUPPORTED_ABILITIES]) if (id !== 'noability' && !only.has(id)) SUPPORTED_ABILITIES.delete(id);
}

module.exports = {
	PS, dex, MOD, FORMAT, STAT_IDS, BOOST_IDS, STATUSES, EVENTS, HAND_MOVES,
	VOLATILES, BARE_VOLATILES, SIDE_CONDS, SLOT_CONDS, PSEUDO_WEATHERS, WEATHERS, TERRAINS, condClass, moveCallbacks,
	SUPPORTED_ABILITIES, SUPPORTED_ITEMS, DEFERRED_ABILITIES, DEFERRED_ITEMS, DORMANT_PARTS,
	unsupportedReasons, legalSpecies, learnableMoves, tableMoves, tableSpecies, mulberry32,
	callbacks, tableAbilities, legalAbilities, tableItems,
};
