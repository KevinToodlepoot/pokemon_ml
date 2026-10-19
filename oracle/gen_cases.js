// Plays random battles in Pokémon Showdown and records, for every decision,
// the choices made, what was legal, and the full state afterwards. The Rust
// engine replays the file from the same seed and must match at every step.
//
//   node gen_cases.js --n 200 --seed 1 --out cases.jsonl [--stats stats.json] [--trace] [--only ID]
//                     [--max-turns 250] [--policy switch] [--plain] [--check-legal]
//                     [--abilities id,id] [--items id,id] [--moves id,id [--moves-per 2]] [--species id,id] [--mega-rate 0.5]
//
// --plain gives every Pokémon no ability, no item and no gender (the set-up the
// engine's first version was checked with).
'use strict';
const fs = require('fs');
const path = require('path');
const L = require('./lib.js');
const { snapshot: snapshotOf } = require('./snapshot.js');
const { PS, dex } = L;

const args = {};
for (let i = 2; i < process.argv.length; i++) {
	const a = process.argv[i];
	if (!a.startsWith('--')) continue;
	const next = process.argv[i + 1];
	if (next === undefined || next.startsWith('--')) args[a.slice(2)] = true;
	else args[a.slice(2)] = process.argv[++i];
}
const N = parseInt(args.n || '100');
const SEED = parseInt(args.seed || '1');
const OUT = args.out || path.join(__dirname, 'cases.jsonl');
const TRACE = !!args.trace;
const MAX_TURNS = parseInt(args['max-turns'] || '250');
const ONLY = args.only !== undefined ? parseInt(args.only) : null;
const POLICY = args.policy || 'mixed';
const PLAIN = !!args.plain;
// --abilities a,b and --items x,y: make every battle a themed one over just these.
const listArg = name => (typeof args[name] === 'string' ? args[name].split(',').filter(Boolean) : []);
const FORCED_THEME = (args.abilities || args.items) ? { abilities: listArg('abilities'), items: listArg('items') } : null;
// --moves a,b: every Pokémon is one that learns at least one of these, and knows up to two of them
// (--moves-per N: up to N of them, for effects that need three particular moves on one Pokémon).
const FORCED_MOVES = listArg('moves');
const MOVES_PER = args['moves-per'] !== undefined ? parseInt(args['moves-per']) : 2;
// --species a,b: at least half of every team is drawn from these.
const FORCED_SPECIES = listArg('species');
// --check-legal: at every move request, also ask Showdown itself about every
// conceivable choice and insist that `legalOptions` lists exactly the accepted ones.
const CHECK_LEGAL = !!args['check-legal'];
// --mega-rate R: chance that a species with a Mega Stone holds it (default 0.5; 0 turns Megas off).
const MEGA_RATE = args['mega-rate'] !== undefined ? parseFloat(args['mega-rate']) : 0.5;

const pool = JSON.parse(fs.readFileSync(path.join(__dirname, 'pool.json'), 'utf8'));
// A theme can only name what the engine models, or the battles would all be skipped.
if (FORCED_THEME) {
	const unknown = [
		...FORCED_THEME.abilities.filter(a => a !== 'noability' && !pool.abilities.includes(a)),
		...FORCED_THEME.items.filter(i => !pool.items.includes(i) && !(pool.megastones || []).includes(i)),
	];
	if (unknown.length) throw new Error('not in the pool: ' + unknown.join(', '));
}
{
	const known = new Set(pool.species.flatMap(sp => sp.moves));
	const unknown = FORCED_MOVES.filter(m => !known.has(m));
	if (unknown.length) throw new Error('--moves: nothing in the pool knows ' + unknown.join(', '));
}
const ALL_MOVES = [...new Set(pool.species.flatMap(s => s.moves))].sort();
const STATS = args.stats || null;
// --log: keep Showdown's battle log with every decision (what `--trace` keeps, without the random draws).
const LOG = !!args.log || TRACE;
// --open-sheets: both players have agreed to open team sheets, so the log starts with every
// Pokémon's item, ability and moves (`|showteam|`), as it does in a best-of-three.
const OPEN_SHEETS = !!args['open-sheets'];
// --requests: keep, with every decision, the request Showdown sent each player (what a
// player's own side looks like to it: exact HP, moves and PP, what it may do).
const REQUESTS = !!args.requests;
// --script FILE: instead of random battles, play the ones written out in FILE, a JSON list of
// { name, seed, rosters: [[set, ...], [set, ...]], picks: [[0, 1, 2, 3], [0, 1, 2, 3]], choices: [[p1, p2], ...] }.
// A set is { species, moves, ability, item, nature, sp, gender } by name or id; `choices` are
// Showdown's own ("move 1 2 mega, switch 3"; "" for a side that is not being asked). Each battle
// is recorded like any other, up to the last choice given, so the engine can be held to a
// particular line of play (scripts/perish_trap.json).
const SCRIPTS = args.script ? JSON.parse(fs.readFileSync(args.script, 'utf8')) : null;
function scriptedSet(set) {
	const species = dex.species.get(set.species);
	if (!species.exists) throw new Error(`unknown species ${set.species}`);
	const known = (what, thing) => { if (!thing.exists) throw new Error(`unknown ${what} in a script`); return thing.id; };
	return {
		species: species.id,
		moves: set.moves.map(m => known('move', dex.moves.get(m))),
		nature: set.nature || 'Hardy',
		sp: set.sp || [0, 0, 0, 0, 0, 0],
		ability: set.ability ? known('ability', dex.abilities.get(set.ability)) : 'noability',
		item: set.item ? known('item', dex.items.get(set.item)) : '',
		gender: set.gender || species.gender || 'M',
	};
}
// --holders FILE: for every kind of log line that names an ability or an item, count which of the
// Pokémon the line mentions really has it. This is where the table in src/observer.rs comes from.
const HOLDERS = args.holders || null;
const holderTally = {};
function watchHolders(battle) {
	const resolve = ident => {
		const m = /^(p[12])([ab]?): (.*)$/.exec(ident);
		if (!m) return null;
		const side = battle.sides[m[1] === 'p1' ? 0 : 1];
		if (m[2]) return side.active[m[2] === 'a' ? 0 : 1];
		return side.pokemon.find(p => p.name === m[3]) || null;
	};
	const note = raw => {
		const parts = [];
		for (const p of raw) {
			if (typeof p === 'string') parts.push(...p.split('|'));
			else parts.push(p);
		}
		const kind = parts[0];
		const mons = [];
		const named = [];
		parts.forEach((p, i) => {
			if (i === 0 || p === null || p === undefined) return;
			if (p instanceof PS.Pokemon) { mons.push([`arg${i}`, p]); return; }
			if (typeof p !== 'string') return;
			let m;
			if ((m = /^\[of\] (.*)$/.exec(p))) { const q = resolve(m[1]); if (q) mons.push(['of', q]); return; }
			if (/^p[12][ab]?: /.test(p)) { const q = resolve(p); if (q) mons.push([`arg${i}`, q]); return; }
			if ((m = /^(\[from\] )?(ability|item): (.*)$/.exec(p))) named.push([m[1] ? 'from' : `arg${i}`, m[2], PS.toID(m[3])]);
		});
		const str = i => (typeof parts[i] === 'string' ? PS.toID(parts[i]) : '');
		if (kind === '-ability' && str(2)) named.push(['new', 'ability', str(2)]);
		if ((kind === '-item' || kind === '-enditem') && str(2)) named.push(['arg2', 'item', str(2)]);
		for (const [form, what, id] of named) {
			const holders = mons.filter(([, p]) => (what === 'ability' ? p.ability === id || p.baseAbility === id :
				p.item === id || p.lastItem === id)).map(([label]) => label);
			const key = `${kind} ${form} ${what}`;
			const who = holders.join('+') || 'nobody';
			holderTally[key] = holderTally[key] || {};
			holderTally[key][who] = holderTally[key][who] || {};
			bump(holderTally[key][who], id);
		}
	};
	for (const fn of ['add', 'addMove']) {
		const orig = battle[fn].bind(battle);
		battle[fn] = (...parts) => { note(parts); return orig(...parts); };
	}
}
const tally = { moves: {}, events: {}, abilities: {}, items: {}, megas: {}, brought: { abilities: {}, items: {} } };
const bump = (table, key) => { table[key] = (table[key] || 0) + 1; };
function tallyLog(log) {
	for (const line of log) {
		const parts = line.split('|');
		const kind = parts[1];
		// Anything in the log that names an ability or item counts as it doing something visible.
		for (const m of line.matchAll(/ability: ([^|\]]+)/g)) bump(tally.abilities, PS.toID(m[1]));
		for (const m of line.matchAll(/item: ([^|\]]+)/g)) bump(tally.items, PS.toID(m[1]));
		if (kind === '-ability') bump(tally.abilities, PS.toID(parts[3]));
		if (kind === '-enditem' || kind === '-item') bump(tally.items, PS.toID(parts[3]));
		if (kind === '-mega') bump(tally.megas, PS.toID(parts[3]) + '>' + PS.toID(parts[4] || ''));
		if (kind === 'move') bump(tally.moves, PS.toID(parts[3]));
		else if (kind === '-status' || kind === 'cant') bump(tally.events, `${kind} ${parts[3]}`);
		else if (kind === '-activate' && parts[3]) bump(tally.events, `-activate ${parts[3]}`);
		else if (['-crit', '-miss', '-fail', '-immune', 'switch', 'faint', '-hitcount', '-heal', '-boost', '-unboost',
			'-supereffective', '-resisted', '-curestatus', '-singleturn', 'win', 'tie'].includes(kind)) bump(tally.events, kind);
	}
}
const CHOOSABLE = new Set(['normal', 'any', 'adjacentAlly', 'adjacentAllyOrSelf', 'adjacentFoe']);

function pick(rand, arr) { return arr[Math.floor(rand() * arr.length)]; }
function shuffled(rand, arr) {
	const a = arr.slice();
	for (let i = a.length - 1; i > 0; i--) {
		const j = Math.floor(rand() * (i + 1));
		[a[i], a[j]] = [a[j], a[i]];
	}
	return a;
}

/** A random legal Champions stat-point spread: 66 points, at most 32 per stat. */
function randomSpread(rand) {
	const sp = [0, 0, 0, 0, 0, 0];
	let left = 66;
	const style = rand();
	if (style < 0.5) {
		// The common shape: two stats maxed, the remainder in a third.
		const order = shuffled(rand, [0, 1, 2, 3, 4, 5]);
		sp[order[0]] = 32; sp[order[1]] = 32; sp[order[2]] = 2;
		return sp;
	}
	while (left > 0) {
		const k = Math.floor(rand() * 6);
		const add = Math.min(left, 32 - sp[k], 1 + Math.floor(rand() * 12));
		sp[k] += add; left -= add;
	}
	return sp;
}

/**
 * Ability, item and gender for a set. Showdown's simulator does not check that a
 * species can legally have an ability, so half the time any modelled ability is
 * used: that exercises abilities on bodies and movesets their real owners lack.
 */
function extras(rand, s) {
	if (PLAIN) return { ability: 'noability', item: '', gender: 'N' };
	let ability = 'noability';
	const r = rand();
	if (pool.abilities.length && r >= 0.06) {
		ability = (r < 0.5 && s.abilities.length) ? pick(rand, s.abilities) : pick(rand, pool.abilities);
		if (theme && theme.abilities.length) ability = pick(rand, theme.abilities);
	}
	let item = (pool.items.length && rand() < 0.8) ? pick(rand, pool.items) : '';
	if (item && theme && theme.items.length) item = pick(rand, theme.items);
	// Mega Stones: usually the species' own, now and then a stone it cannot use.
	const megaRoll = rand();
	if (s.megas && s.megas.length && megaRoll < MEGA_RATE) item = pick(rand, s.megas);
	else if (megaRoll > 0.98 && pool.megastones && pool.megastones.length && MEGA_RATE > 0) item = pick(rand, pool.megastones);
	const gender = s.gender || (rand() < 0.5 ? 'M' : 'F');
	return { ability, item, gender };
}

function randomSet(rand) {
	let from = FORCED_MOVES.length ? moveLearners() : pool.species;
	if (FORCED_SPECIES.length && rand() < 0.6) {
		from = pool.species.filter(sp => FORCED_SPECIES.includes(sp.id));
		if (from.length !== FORCED_SPECIES.length) throw new Error('--species: not all of these are in the pool');
	}
	const s = pick(rand, from);
	let moves = shuffled(rand, s.moves).slice(0, 4);
	if (FORCED_MOVES.length) {
		const known = shuffled(rand, FORCED_MOVES.filter(m => s.moves.includes(m))).slice(0, MOVES_PER);
		moves = [...known, ...moves.filter(m => !known.includes(m))].slice(0, 4);
	}
	// Protect is on nearly every real doubles set; make sure it is exercised.
	if (rand() < 0.35 && !moves.includes('protect') && s.moves.includes('protect')) moves[3] = 'protect';
	return { species: s.id, moves, nature: pick(rand, pool.natures), sp: randomSpread(rand), ...extras(rand, s) };
}

let learners = null;
function moveLearners() {
	if (!learners) {
		learners = pool.species.filter(s => FORCED_MOVES.some(m => s.moves.includes(m)));
		if (learners.length < 12) throw new Error(`--moves: only ${learners.length} species learn any of ${FORCED_MOVES}`);
	}
	return learners;
}

/** A set guaranteed to know `moveId`, so that every modelled move gets exercised. */
function featuredSet(rand, moveId) {
	const learners = pool.species.filter(s => s.moves.includes(moveId));
	if (!learners.length) return randomSet(rand);
	const s = pick(rand, learners);
	const moves = [moveId, ...shuffled(rand, s.moves.filter(m => m !== moveId)).slice(0, 3)];
	return { species: s.id, moves, nature: pick(rand, pool.natures), sp: randomSpread(rand), ...extras(rand, s) };
}

// A themed battle draws every ability and item from a handful, so that effects
// meet themselves and each other far more often than under uniform sampling.
let theme = null;
function pickTheme(rand) {
	theme = null;
	if (FORCED_THEME) { theme = FORCED_THEME; return; }
	if (PLAIN || rand() >= 0.3) return;
	theme = {
		abilities: Array.from({ length: 1 + Math.floor(rand() * 3) }, () => pick(rand, pool.abilities)),
		items: Array.from({ length: 1 + Math.floor(rand() * 3) }, () => pick(rand, pool.items)),
	};
}

function buildTeams(rand) {
	pickTheme(rand);
	const mode = rand();
	const team = () => {
		const out = [];
		const seen = new Set();
		while (out.length < 6) {
			const set = randomSet(rand);
			if (seen.has(set.species)) continue;
			seen.add(set.species);
			out.push(set);
		}
		return out;
	};
	if (mode < 0.03) {
		// Nobody can attack: PP runs dry and the battle is settled by Struggle.
		const passive = pool.species.map(s => ({ id: s.id, moves: s.moves.filter(m => dex.moves.get(m).category === 'Status') }))
			.filter(s => s.moves.length >= 4);
		const set = () => {
			const s = pick(rand, passive);
			const full = pool.species.find(x => x.id === s.id);
			return { species: s.id, moves: shuffled(rand, s.moves).slice(0, 4), nature: pick(rand, pool.natures), sp: randomSpread(rand), ...extras(rand, full) };
		};
		return [Array.from({ length: 6 }, set), Array.from({ length: 6 }, set)];
	}
	if (mode < 0.15) {
		// Everyone shares one species and spread: every ordering decision is a speed tie.
		const base = randomSet(rand);
		const sp = pool.species.find(s => s.id === base.species);
		const clone = () => ({ ...base, moves: shuffled(rand, sp.moves).slice(0, 4) });
		return [Array.from({ length: 6 }, clone), Array.from({ length: 6 }, clone)];
	}
	if (mode < 0.35) {
		// Mirror match: each Pokémon ties with its counterpart.
		const t = team();
		return [t, t.map(s => ({ ...s }))];
	}
	return [team(), team()];
}

function toPsSet(set, i) {
	const species = dex.species.get(set.species);
	const evs = {};
	L.STAT_IDS.forEach((k, j) => { evs[k] = set.sp[j]; });
	return {
		name: `${species.baseSpecies}${i}`, species: species.name,
		item: set.item ? dex.items.get(set.item).name : '', ability: dex.abilities.get(set.ability).name,
		moves: set.moves.map(m => dex.moves.get(m).name), nature: set.nature,
		// Gender must be fixed: an unset gender is rolled from the battle's RNG.
		gender: set.gender, evs, ivs: { hp: 31, atk: 31, def: 31, spa: 31, spd: 31, spe: 31 }, level: 50,
	};
}

/**
 * Per active slot, every choice Showdown would accept, as canonical choice strings.
 * This reads the Pokémon's real state rather than the request sent to the player,
 * because the request deliberately hides some things (a trap not yet revealed).
 */
function legalOptions(battle, side) {
	const req = side.activeRequest;
	if (!req || req.wait) return [['pass'], ['pass']];
	const bench = [];
	for (let i = side.active.length; i < side.pokemon.length; i++) if (!side.pokemon[i].fainted) bench.push(i);
	if (req.forceSwitch) {
		const need = req.forceSwitch.filter(Boolean).length;
		return req.forceSwitch.map((f, pos) => {
			if (!f) return ['pass'];
			// Revival Blessing asks for a fainted team member instead (one still in an active slot counts).
			const opts = side.slotConditions[pos]['revivalblessing'] ?
				side.pokemon.map((p, i) => (p.fainted ? `switch ${i + 1}` : null)).filter(Boolean) :
				bench.map(i => `switch ${i + 1}`);
			if (bench.length < need) opts.push('pass');
			return opts;
		});
	}
	return req.active.map((a, pos) => {
		const p = side.active[pos];
		if (p.fainted) return ['pass'];
		const opts = [];
		// Locked into a move (a rampage, the second turn of a charging move, recharging):
		// that move is all Showdown lists, and it takes no target.
		if (p.getLockedMove()) {
			opts.push('move 1');
			if (!p.trapped) throw new Error('a locked Pokémon that is not trapped');
			return opts;
		}
		const moves = p.getMoves();
		// No usable move left: any move choice Showdown accepts becomes Struggle. It lists
		// Struggle itself, except to a side's last active Pokémon with moves sealed by a
		// foe's Imprison: that one is shown its real moves and must spell out a use of one.
		if (!moves.length) {
			const shown = a.moves[0];
			const loc = CHOOSABLE.has(shown.target) && [1, 2, -1, -2].find(l => battle.validTargetLoc(l, p, shown.target));
			opts.push(loc ? `move 1 ${loc}` : 'move 1');
		}
		for (const mega of (p.canMegaEvo && moves.length ? ['', ' mega'] : [''])) {
			moves.forEach((m, j) => {
				if (m.disabled) return;
				if (CHOOSABLE.has(m.target)) {
					for (const loc of [1, 2, -1, -2]) if (battle.validTargetLoc(loc, p, m.target)) opts.push(`move ${j + 1} ${loc}${mega}`);
				} else {
					opts.push(`move ${j + 1}${mega}`);
				}
			});
		}
		if (!p.trapped) for (const i of bench) opts.push(`switch ${i + 1}`);
		return opts;
	});
}

/** Cross-checks `legalOptions` against what `Side#choose` accepts (move requests only). */
function checkLegal(battle, side, legal) {
	const req = side.activeRequest;
	if (!req || req.wait || req.forceSwitch) return;
	const universe = ['pass'];
	for (const mega of ['', ' mega']) {
		for (let m = 1; m <= 4; m++) for (const t of ['', ' 1', ' 2', ' -1', ' -2']) universe.push(`move ${m}${t}${mega}`);
	}
	for (let i = 1; i <= side.pokemon.length; i++) universe.push(`switch ${i}`);
	for (let pos = 0; pos < 2; pos++) {
		// Hold the other slot at something legal that cannot clash with a switch here.
		const other = legal[1 - pos].find(o => !o.startsWith('switch') && !o.endsWith('mega')) || legal[1 - pos][0];
		const accepted = [];
		for (const opt of universe) {
			if (opt.startsWith('switch') && opt === other) continue;
			const input = pos === 0 ? `${opt}, ${other}` : `${other}, ${opt}`;
			if (side.choose(input)) accepted.push(opt);
			side.clearChoice();
		}
		const p = side.active[pos];
		let want = legal[pos].filter(o => !(o.startsWith('switch') && o === other));
		let got = accepted;
		if (!p.fainted && p.getLockedMove()) {
			// Showdown takes the lone move with or without a Mega flag, and ignores the flag.
			got = accepted.filter(o => !o.endsWith(' mega'));
		} else if (!p.fainted && !p.getMoves().length) {
			// Out of usable moves: Showdown takes any move slot it listed and turns it into Struggle.
			// (It also ignores a Mega flag on that Struggle, so `move 1 mega` is not listed.)
			got = accepted.filter(o => !o.startsWith('move'));
			const first = accepted.find(o => o.startsWith('move'));
			if (first) got.push(first);
		}
		want = want.slice().sort(); got = got.slice().sort();
		if (want.join('|') !== got.join('|')) {
			throw new Error(`legal choices differ for ${side.id} slot ${pos + 1} (${p.species.id}, ${p.ability}, ${p.item}): ` +
				`listed [${want.join('; ')}], Showdown accepts [${got.join('; ')}]`);
		}
	}
}

/**
 * Whether Showdown takes these answers to a switch request. It counts down
 * the switches and passes it still expects as it reads the slots in order, and a
 * revival uses up a switch if one is left: so with a single replacement for a
 * fainted Pokémon and a reviver to its left, reviving and replacing are not
 * accepted together (Showdown throws).
 */
function switchComboOk(side, picks) {
	const flagged = side.active.map(p => !!p.switchFlag);
	const out = flagged.filter(Boolean).length;
	const into = side.pokemon.slice(side.active.length).filter(p => !p.fainted).length;
	let switches = Math.min(out, into);
	let passes = out - switches;
	const seen = new Set();
	for (const [pos, pick] of picks.entries()) {
		if (!flagged[pos]) continue;
		if (pick === 'pass') {
			if (!passes) return false;
			passes--;
		} else if (side.slotConditions[pos]['revivalblessing']) {
			switches = Math.max(switches - 1, 0);
		} else {
			if (!switches || seen.has(pick)) return false;
			seen.add(pick);
			switches--;
		}
	}
	return switches === 0;
}

function chooseFor(rand, options, side) {
	if (side.activeRequest && side.activeRequest.forceSwitch && side.active.some((p, pos) => side.slotConditions[pos]['revivalblessing'])) {
		for (let attempt = 0; attempt < 200; attempt++) {
			const picks = options.map(opts => pick(rand, opts));
			if (switchComboOk(side, picks)) return picks;
		}
		throw new Error('no acceptable answer to a switch request with Revival Blessing');
	}
	if (options.some(opts => opts.includes('pass') && opts.length > 1)) {
		// Fewer replacements than empty slots: hand the bench out at random, pass the rest.
		const open = shuffled(rand, options.map((opts, i) => i).filter(i => options[i].length > 1));
		const bench = shuffled(rand, options[open[0]].filter(o => o !== 'pass'));
		const picks = options.map(() => 'pass');
		open.forEach((slot, k) => { if (k < bench.length) picks[slot] = bench[k]; });
		return picks;
	}
	// Favour attacking so battles finish, but switch often enough to exercise it.
	// `--policy switch` instead switches whenever possible, which never ends a battle
	// and so runs into Showdown's 1000-turn limit.
	const attackRate = POLICY === 'switch' ? 0 : 0.88;
	for (let attempt = 0; attempt < 50; attempt++) {
		const picks = options.map(opts => {
			let moves = opts.filter(o => o.startsWith('move'));
			const others = opts.filter(o => !o.startsWith('move'));
			// Mega Evolve at the first chance about half the time, so that it happens early and late.
			const megas = moves.filter(o => o.endsWith('mega'));
			if (megas.length) moves = rand() < 0.5 ? megas : moves.filter(o => !o.endsWith('mega'));
			// (With one Pokémon on the bench both slots cannot switch: after a few tries, let them attack.)
			const rate = attempt < 10 ? attackRate : Math.max(attackRate, 0.5);
			if (moves.length && (!others.length || rand() < rate)) return pick(rand, moves);
			return pick(rand, others);
		});
		const switches = picks.filter(p => p.startsWith('switch'));
		if (new Set(switches).size !== switches.length) continue;
		// Only one Mega Evolution per side.
		if (picks.filter(p => p.endsWith('mega')).length > 1) continue;
		return picks;
	}
	throw new Error('could not find a consistent choice');
}

function stackLabel() {
	const lines = new Error().stack.split('\n').slice(3, 9)
		.map(l => (l.match(/at (?:new )?([\w.$<>]+)/) || [])[1])
		.filter(x => x && !/^(PRNG|Gen5RNG)\./.test(x) && x !== 'Battle.random' && x !== 'Battle.randomChance' && x !== 'Battle.sample');
	return lines.slice(0, 4).join(' < ');
}

function runCase(id) {
	const rand = L.mulberry32((SEED * 1000003 + id) | 0);
	const script = SCRIPTS && SCRIPTS[id];
	const [full1, full2] = script ? script.rosters.map(r => r.map(scriptedSet)) : buildTeams(rand);
	const seed = script ? (script.seed || [1, 2, 3, 4]) : [0, 0, 0, 0].map(() => Math.floor(rand() * 65536));
	const picks = script ? (script.picks || [full1, full2].map(full => full.map((_, i) => i).slice(0, 4))) :
		[full1, full2].map(full => shuffled(rand, full.map((_, i) => i)).slice(0, 4));
	// Each battle features one modelled move on a lead, cycling through all of them.
	const featured = ALL_MOVES[id % ALL_MOVES.length];
	const fside = Math.floor(id / ALL_MOVES.length) % 2;
	if (!script) [full1, full2][fside][picks[fside][0]] = featuredSet(rand, featured);
	// Likewise one modelled ability and one modelled item, on leads, so each gets its share of battles.
	if (!PLAIN && !script) {
		if (pool.abilities.length) {
			const set = [full1, full2][1 - fside][picks[1 - fside][0]];
			[full1, full2][1 - fside][picks[1 - fside][0]] = { ...set, ability: pool.abilities[id % pool.abilities.length] };
		}
		if (pool.items.length) {
			const set = [full1, full2][fside][picks[fside][1]];
			[full1, full2][fside][picks[fside][1]] = { ...set, item: pool.items[id % pool.items.length] };
		}
	}

	const battle = new PS.Battle({ formatid: L.FORMAT, seed: seed.join(',') });
	let draws = [];
	if (TRACE) {
		const orig = battle.prng.random.bind(battle.prng);
		battle.prng.random = (from, to) => {
			const v = orig(from, to);
			draws.push(`rng(${to ? `${from},${to}` : from})=${v}  ${stackLabel()}`);
			return v;
		};
	}
	if (HOLDERS) watchHolders(battle);
	battle.setPlayer('p1', { name: 'P1', team: full1.map(toPsSet) });
	battle.setPlayer('p2', { name: 'P2', team: full2.map(toPsSet) });
	if (battle.requestState !== 'teampreview') throw new Error(`expected team preview, got ${battle.requestState}`);
	// (What the server does once both players have accepted, or at once in a best-of-three.)
	if (OPEN_SHEETS) battle.showOpenTeamSheets();

	// Remember each Pokémon's pick order; Showdown reorders side.pokemon as it switches.
	battle.sides.forEach((side, s) => picks[s].forEach((orig, k) => { side.pokemon[orig].pickIndex = k; }));
	battle.sides.forEach((side, s) => {
		if (!battle.choose(side.id, `team ${picks[s].map(i => i + 1).join(',')}`)) throw new Error(side.choice.error);
	});
	const teams = [full1, full2].map((full, s) => picks[s].map(i => full[i]));
	for (const t of teams) for (const set of t) { bump(tally.brought.abilities, set.ability); if (set.item) bump(tally.brought.items, set.item); }

	let logPos = battle.log.length;
	// `teams` are the four each side brought, in the order picked; `rosters` the six it
	// registered, which Team Preview showed, with `picks[side][k]` the roster index of `teams[side][k]`.
	const out = {
		id, seed, teams, rosters: [full1, full2], picks, open_sheets: OPEN_SHEETS,
		initial: snapshotOf(battle, p => p.pickIndex), steps: [], truncated: false,
	};
	if (script && script.name) out.name = script.name;
	if (TRACE) { out.initial.draws = draws; draws = []; }
	if (LOG) out.initial.log = battle.log.slice(0);
	const requests = () => battle.sides.map(side => JSON.parse(JSON.stringify(side.activeRequest || null)));
	if (REQUESTS) out.initial.requests = requests();
	while (!battle.ended) {
		if (battle.turn > MAX_TURNS) { out.truncated = true; break; }
		if (script && script.choices && out.steps.length >= script.choices.length) break;
		const legal = battle.sides.map(side => legalOptions(battle, side));
		if (CHECK_LEGAL) battle.sides.forEach((side, s) => checkLegal(battle, side, legal[s]));
		const choices = script && script.choices ?
			script.choices[out.steps.length].map(c => (c || 'pass, pass').split(/,\s*/)) :
			legal.map((opts, s) => chooseFor(rand, opts, battle.sides[s]));
		// Decide who is being asked before anyone answers: the last answer starts the next
		// request, which would otherwise look like one this step still had to fill.
		const asked = battle.sides.map(side => !!side.activeRequest && !side.activeRequest.wait);
		battle.sides.forEach((side, s) => {
			if (!asked[s]) return;
			if (!battle.choose(side.id, choices[s].join(', '))) {
				throw new Error(`case ${id}: Showdown rejected "${choices[s].join(', ')}" for ${side.id}: ${side.choice.error}`);
			}
		});
		const after = snapshotOf(battle, p => p.pickIndex);
		if (TRACE) { after.draws = draws; draws = []; }
		if (LOG) after.log = battle.log.slice(logPos);
		if (REQUESTS) after.requests = requests();
		logPos = battle.log.length;
		out.steps.push({ choices: choices.map(c => c.join(', ')), legal, after });
	}
	if (STATS) tallyLog(battle.log);
	return out;
}

const fd = fs.openSync(OUT, 'w');
let steps = 0, truncated = 0, turns = 0;
const ids = ONLY !== null ? [ONLY] : Array.from({ length: SCRIPTS ? SCRIPTS.length : N }, (_, i) => i);
for (const id of ids) {
	const c = runCase(id);
	steps += c.steps.length;
	if (c.truncated) truncated++;
	turns += c.steps.length ? c.steps[c.steps.length - 1].after.turn : 0;
	fs.writeSync(fd, JSON.stringify(c) + '\n');
}
fs.closeSync(fd);
if (STATS) {
	const unused = ALL_MOVES.filter(m => !tally.moves[m]);
	const least = ALL_MOVES.map(m => [m, tally.moves[m] || 0]).sort((a, b) => a[1] - b[1]).slice(0, 10);
	fs.writeFileSync(STATS, JSON.stringify({
		battles: ids.length, decisions: steps, modelled_moves: ALL_MOVES.length, unused_moves: unused, least_used: least,
		events: tally.events, moves: tally.moves,
		// Mega Evolutions that happened, as "species>stone".
		megas: tally.megas,
		// For each modelled ability and item: [times brought, log lines naming it].
		abilities: Object.fromEntries(pool.abilities.map(a => [a, [tally.brought.abilities[a] || 0, tally.abilities[a] || 0]])),
		items: Object.fromEntries(pool.items.map(a => [a, [tally.brought.items[a] || 0, tally.items[a] || 0]])),
	}, null, 1));
	console.log(`moves used at least once: ${ALL_MOVES.length - unused.length}/${ALL_MOVES.length}; least used: ${least.slice(0, 5).map(x => x.join(' x')).join(', ')}`);
}
if (HOLDERS) fs.writeFileSync(HOLDERS, JSON.stringify(holderTally, null, 1));
console.log(`wrote ${ids.length} battles, ${steps} decisions, ${truncated} cut off at turn ${MAX_TURNS}, mean length ${(turns / ids.length).toFixed(1)} turns -> ${OUT}`);
