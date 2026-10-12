// Writes formats/<format id>.json: what a regulation allows, as Pokémon Showdown's own
// team validator sees it.
//
//   node oracle/gen_format.js                       the regulation the engine is built for
//   node oracle/gen_format.js gen9championsvgc2026regmb [more ids]
//
// Nothing here knows the rules. Every entry is the answer to a question put to the
// validator: is this species accepted, and with this move, this ability, this item, this
// gender? The team rules are read from the format's rule table. `scripts/check-teams.sh`
// then checks the engine's reading of the file against the validator on whole teams.
'use strict';
const fs = require('fs');
const path = require('path');
const L = require('./lib.js');
const { PS } = L;
const toID = PS.toID;

const ids = process.argv.slice(2).filter(a => !a.startsWith('--'));
if (!ids.length) ids.push(L.FORMAT);

const commit = (() => {
	// The checkout `lib.js` loaded (PS_DIR or oracle/pokemon-showdown); `git` handles a
	// submodule checkout, whose .git is a file, as well as a plain clone.
	try {
		const dir = process.env.PS_DIR || path.join(__dirname, 'pokemon-showdown');
		return require('child_process').execFileSync('git', ['-C', dir, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
	} catch {
		return 'unknown';
	}
})();

/** Everything about an effect that could matter to the simulator, as comparable text. */
function fingerprint(x) {
	return JSON.stringify(x, (key, v) => {
		if (typeof v === 'function') return v.toString();
		if (NOT_MECHANICS.includes(key)) return undefined;
		return v;
	});
}

/** Not mechanics: availability, descriptions and where a thing can be found. */
const NOT_MECHANICS = ['isNonstandard', 'tier', 'doublesTier', 'natDexTier', 'desc', 'shortDesc', 'gen', 'exists', 'rating', 'inherit'];

function describeDifference(kind, a, b) {
	const out = [];
	for (const k of new Set([...Object.keys(a), ...Object.keys(b)])) {
		if (NOT_MECHANICS.includes(k)) continue;
		if (fingerprint(a[k]) !== fingerprint(b[k])) {
			const show = v => (typeof v === 'function' ? '(a different script)' : JSON.stringify(v));
			out.push(`${kind} ${a.id}: ${k} is ${show(b[k])} here, ${show(a[k])} in the engine`);
		}
	}
	return out;
}

for (const id of ids) {
	const format = PS.Dex.formats.get(id);
	if (!format.exists) throw new Error(`no such format: ${id}`);
	const dex = PS.Dex.forFormat(format);
	const validator = PS.TeamValidator.get(format);
	const rules = PS.Dex.formats.getRuleTable(format);

	const ok = set => validator.validateSet(set, {}) === null;
	const blank = species => ({
		name: species.name, species: species.name, item: '', ability: '', moves: [], nature: 'Hardy',
		evs: { hp: 1, atk: 0, def: 0, spa: 0, spd: 0, spe: 0 }, level: 50, gender: '',
	});

	const allMoves = dex.moves.all().filter(m => m.id !== 'struggle');
	const allAbilities = dex.abilities.all();
	const allItems = dex.items.all();
	const species = [];
	const refused = {};
	const itemUsers = new Map();
	for (const sp of dex.species.all()) {
		// A set this species is accepted with, if there is one: any of its abilities, any move it learns.
		const learnset = dex.species.getLearnsetData(sp.id).learnset || {};
		const candidates = [...Object.keys(learnset), 'protect', 'transform', 'sketch'];
		let base = null;
		let why = '';
		search:
		for (const ability of [...Object.values(sp.abilities), '']) {
			for (const move of candidates.slice(0, 3).concat(candidates.slice(-3))) {
				const set = { ...blank(sp), ability, moves: [move] };
				const problems = validator.validateSet(set, {});
				if (!problems) {
					// Accepted, but as something else (a Gigantamax or in-battle forme): not a species to bring.
					if (toID(set.species) === sp.id) base = { ability, move };
					else why = `is brought as ${set.species}`;
					break search;
				}
				why = problems[0].replace(sp.name, 'it').replace(/^it /, '');
			}
		}
		if (!base) {
			const reason = why.replace(/\(.*?\)/g, '').replace(/ +/g, ' ').slice(0, 60);
			refused[reason] = (refused[reason] || 0) + 1;
			continue;
		}
		const withMove = m => ok({ ...blank(sp), ability: base.ability, moves: [m] });
		const withAbility = a => ok({ ...blank(sp), ability: a, moves: [base.move] });
		const withItem = i => ok({ ...blank(sp), ability: base.ability, moves: [base.move], item: i });
		// The validator does not refuse a gender; it corrects it. So: which genders does it leave alone?
		// (An unspecified one is settled by the species' gender ratio when the battle starts.)
		const genders = new Set();
		for (const g of ['M', 'F', 'N']) {
			const set = { ...blank(sp), ability: base.ability, moves: [base.move], gender: g };
			if (validator.validateSet(set, {}) !== null) continue;
			if (set.gender === g) genders.add(g);
			else if (set.gender) genders.add(set.gender);
			else if (sp.genderRatio.M > 0 || sp.genderRatio.F > 0) (sp.genderRatio.M > 0 && genders.add('M'), sp.genderRatio.F > 0 && genders.add('F'));
		}
		const items = allItems.filter(i => withItem(i.name)).map(i => i.id);
		for (const i of items) itemUsers.set(i, (itemUsers.get(i) || 0) + 1);
		species.push({
			id: sp.id,
			// Species Clause goes by Pokédex number.
			num: sp.num,
			moves: allMoves.filter(m => withMove(m.name)).map(m => m.id).sort(),
			abilities: allAbilities.filter(a => withAbility(a.name)).map(a => a.id).sort(),
			genders: ['M', 'F', 'N'].filter(g => genders.has(g)),
			megas: allItems.filter(i => i.megaStone && i.megaStone[sp.name]).map(i => i.id).filter(i => items.includes(i)),
			_items: items,
		});
	}
	// Items anyone may hold; what only some may hold is listed with them.
	const common = [...itemUsers].filter(([, n]) => n === species.length).map(([i]) => i).sort();
	const commonSet = new Set(common);
	for (const s of species) {
		const extra = s._items.filter(i => !commonSet.has(i));
		const missing = common.filter(i => !s._items.includes(i));
		if (missing.length) throw new Error(`${s.id} may not hold ${missing.slice(0, 5)}, which everyone else may`);
		delete s._items;
		if (extra.length) s.items = extra.sort();
	}

	// Does this regulation change anything the simulator computes with, or only what may be brought?
	const differences = [];
	if (dex.currentMod !== L.dex.currentMod) {
		const seenMoves = new Set(species.flatMap(s => s.moves));
		const seenAbilities = new Set(species.flatMap(s => s.abilities));
		for (const s of species) differences.push(...describeDifference('species', L.dex.species.get(s.id), dex.species.get(s.id)));
		for (const m of seenMoves) differences.push(...describeDifference('move', L.dex.moves.get(m), dex.moves.get(m)));
		for (const a of seenAbilities) differences.push(...describeDifference('ability', L.dex.abilities.get(a), dex.abilities.get(a)));
		for (const i of [...common, ...species.flatMap(s => s.items || [])]) {
			differences.push(...describeDifference('item', L.dex.items.get(i), dex.items.get(i)));
		}
		for (const s of species) {
			for (const megaStone of s.megas) {
				const megaName = dex.items.get(megaStone).megaStone[dex.species.get(s.id).name];
				differences.push(...describeDifference('species', L.dex.species.get(megaName), dex.species.get(megaName)));
			}
		}
		// (A mod that inherits its scripts has them filled in from its parent.)
		const scripts = m => fingerprint(PS.Dex.mod(m).data.Scripts);
		if (scripts(dex.currentMod) !== scripts(L.dex.currentMod)) differences.push('the battle scripts differ');
		const conds = m => fingerprint(PS.Dex.mod(m).data.Conditions);
		if (conds(dex.currentMod) !== conds(L.dex.currentMod)) differences.push('the conditions (statuses, weather) differ');
	}

	const out = {
		id: format.id,
		name: format.name,
		showdown_commit: commit,
		// The data the engine's tables were generated from, and this regulation's.
		engine_mod: L.dex.currentMod,
		mod: dex.currentMod,
		game_type: format.gameType,
		team: {
			min_size: rules.minTeamSize,
			max_size: rules.maxTeamSize,
			picked: rules.pickedTeamSize,
			level: rules.adjustLevel,
			species_clause: rules.has('speciesclause'),
			// How many Pokémon may hold the same item; 0 for no limit.
			item_clause: rules.has('itemclause') ? Number(rules.valueRules.get('itemclause') || 1) : 0,
			stat_points_total: rules.evLimit,
			stat_points_per_stat: 32,
		},
		differences: [...new Set(differences)].sort(),
		items: common,
		species,
	};
	// One species to a line: the file is meant to be diffed between regulations.
	const head = { ...out, species: undefined };
	let text = JSON.stringify(head, null, '\t').replace(/\n}$/, ',\n\t"species": [\n');
	text += species.map(s => '\t\t' + JSON.stringify(s)).join(',\n') + '\n\t]\n}\n';
	const dir = path.join(__dirname, '..', 'formats');
	fs.mkdirSync(dir, { recursive: true });
	fs.writeFileSync(path.join(dir, `${format.id}.json`), text);
	console.log(`${format.name}: ${species.length} species, ${common.length} items anyone may hold, ` +
		`${differences.length} differences from the engine's data -> formats/${format.id}.json`);
	const why = Object.entries(refused).sort((a, b) => b[1] - a[1]).slice(0, 6).map(([r, n]) => `${n} ${r}`).join('; ');
	console.log(`  not to be brought: ${why}`);
}
