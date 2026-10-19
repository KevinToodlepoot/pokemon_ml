// Showdown's state at one decision point, in the terms `replay::Snap` reads and `replay::diff`
// compares with the engine's. Shared by the oracle's recorder (gen_cases.js) and any other
// recorder of Showdown battles, so that there is one schema.
//
//   const { snapshot } = require('./snapshot.js');
//   snapshot(battle, p => teamIndex(p))
//
// `index(pokemon)` gives a Pokémon's team index: its place in the order the side picked its
// team at Team Preview (Showdown reorders `side.pokemon` as Pokémon switch, and keeps no such
// number itself). Needs nothing but the live battle: no Showdown module is loaded here.
'use strict';

const STAT_IDS = ['hp', 'atk', 'def', 'spa', 'spd', 'spe'];
const BOOST_IDS = ['atk', 'def', 'spa', 'spd', 'spe', 'accuracy', 'evasion'];
const SLOTS = ['p1a', 'p1b', 'p2a', 'p2b'];

function snapshot(battle, index) {
	// The side that won (by its player's name), -1 for a tie, null while the battle runs.
	const winner = !battle.ended ? null : battle.sides.findIndex(side => side.name === battle.winner);
	return {
		turn: battle.turn,
		ended: battle.ended,
		winner,
		request: battle.requestState || '',
		rng: battle.prng.getSeed().split(',').map(Number),
		effect_order: battle.effectOrder,
		battle_last_move: battle.lastMove ? battle.lastMove.id : '',
		field: {
			weather: battle.field.weather,
			weather_turns: battle.field.weatherState.duration || 0,
			terrain: battle.field.terrain,
			terrain_turns: battle.field.terrainState.duration || 0,
			pseudo: Object.keys(battle.field.pseudoWeather).map(id => `${id}:${battle.field.pseudoWeather[id].duration || 0}`),
		},
		sides: battle.sides.map(side => ({
			left: side.pokemonLeft,
			// Side conditions as id:turns left:detail:effect order, in the order they were added.
			conds: Object.keys(side.sideConditions).map(id => {
				const st = side.sideConditions[id];
				return `${id}:${st.duration || 0}:${condDetail(id, st, index)}:${st.effectOrder || 0}`;
			}),
			slots: side.slotConditions.map(slot => Object.keys(slot).map(id => `${id}:${slot[id].duration || 0}:${condDetail(id, slot[id], index)}`)),
			mons: side.pokemon.map(p => ({
				idx: index(p),
				species: p.species.id,
				hp: p.hp,
				maxhp: p.maxhp,
				stats: [p.maxhp, ...STAT_IDS.slice(1).map(k => p.storedStats[k])],
				status: p.fainted ? '' : p.status,
				time: p.fainted ? 0 : (p.statusState.time || 0),
				stage: p.fainted ? 0 : (p.statusState.stage || 0),
				boosts: BOOST_IDS.map(k => p.boosts[k]),
				pp: p.moveSlots.map(m => m.pp),
				active: p.isActive,
				fainted: p.fainted,
				switch_flag: !!p.switchFlag,
				speed: p.speed,
				vol: Object.keys(p.volatiles).map(id => `${id}:${p.volatiles[id].duration || 0}:${volDetail(id, p.volatiles[id])}`),
				ability: p.ability,
				item: p.item,
				last_item: p.lastItem,
				used_item: !!p.usedItemThisTurn,
				ate_berry: !!p.ateBerry,
				types: p.types,
				trapped: p.trapped ? (p.trapped === 'hidden' ? 2 : 1) : 0,
				disabled: p.moveSlots.map(m => !!m.disabled),
				ability_order: p.abilityState.effectOrder || 0,
				item_order: p.itemState.effectOrder || 0,
				active_turns: p.activeTurns,
				move_result: [p.moveThisTurnResult, p.moveLastTurnResult].map(resultCode).join(''),
				base_species: p.baseSpecies.id,
				can_mega: p.canMegaEvo ? battle.dex.toID(p.canMegaEvo) : '',
				last_move: p.lastMove ? p.lastMove.id : '',
				move_actions: Math.min(p.activeMoveActions, 255),
				newly_switched: !!p.newlySwitched,
				hurt: p.hurtThisTurn || 0,
				times_attacked: p.timesAttacked,
				stat_flags: (p.statsRaisedThisTurn ? 'r' : '-') + (p.statsLoweredThisTurn ? 'l' : '-'),
				attacked_by: attackedBySummary(p, index),
				last_damaged: lastDamagedSummary(p),
				moves: p.moveSlots.map(m => m.id).join(','),
				transformed: !!p.transformed,
				illusion: p.illusion ? index(p.illusion) : -1,
				added_type: p.addedType || '',
			})),
		})),
	};
}

/** What this turn's entries of `attackedBy` amount to, as the Rust engine keeps it. */
function attackedBySummary(p, index) {
	let mask = 0;
	for (const a of p.attackedBy) {
		if (a.thisTurn && a.damage > 0) mask |= 1 << (a.source.side.n * 6 + index(a.source));
	}
	const last = p.getLastDamagedBy(true);
	const slot = last && last.thisTurn ? `${SLOTS.indexOf(last.slot)}/${last.damage}` : '';
	return `${mask}:${slot}`;
}

/** The last entry of `attackedBy` that Metal Burst would answer, whatever turn it is from. */
function lastDamagedSummary(p) {
	const last = p.getLastDamagedBy(true);
	if (!last) return '';
	return `${SLOTS.indexOf(last.slot)}/${last.damage}${last.thisTurn ? '' : '*'}`;
}

/** The state a side or slot condition carries, as the Rust engine prints it. */
function condDetail(id, state, index) {
	switch (id) {
	case 'spikes': case 'toxicspikes': return state.layers || 0;
	case 'wish': return `${Math.floor(state.hp)}/${state.startingTurn}`;
	case 'futuremove': return `${state.endingTurn}/${state.source.side.n}${index(state.source)}`;
	default: return 0;
	}
}

/** The state a volatile carries, as the Rust engine prints it. */
function volDetail(id, state) {
	switch (id) {
	case 'stall': case 'allyswitch': return state.counter || 0;
	case 'choicelock': case 'encore': case 'disable': return state.move || 0;
	case 'substitute': return state.hp;
	case 'dragoncheer': return state.hasDragonType ? 1 : 0;
	case 'partiallytrapped': return state.boundDivisor;
	case 'stockpile': return `${state.layers}/${state.def}/${state.spd}`;
	case 'lockedmove': return `${state.move}/${state.trueDuration}`;
	case 'twoturnmove': return state.move;
	case 'fly': case 'dig': case 'dive': case 'bounce': case 'phantomforce': case 'solarbeam': case 'solarblade':
	case 'skyattack': case 'meteorbeam': case 'electroshot': return state.targetLoc || 0;
	case 'leechseed': return SLOTS.indexOf(state.sourceSlot);
	case 'helpinghand': return Math.round(Math.log(state.multiplier) / Math.log(1.5));
	case 'confusion': return state.time || 0;
	case 'metronome': return `${state.lastMove || '-'}/${state.numConsecutive || 0}`;
	default: return 0;
	}
}

function resultCode(v) {
	return v === undefined ? 'u' : v === null ? 'n' : v === true ? 't' : v === false ? 'f' : '?';
}

module.exports = { snapshot };
