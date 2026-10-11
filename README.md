# vgc-engine

A fast battle engine for Pokémon Champions doubles (VGC), written in Rust and
checked decision by decision against Pokémon Showdown. It is meant as the
simulator underneath a self-play bot: copyable fixed-size state, no allocation
on the common paths of the turn loop, and a harness that proves each mechanic
matches Showdown before anything is trained on it.

**Status: every move, ability and held item a Champions Pokémon can have is
modelled**: all 510 moves in the Champions learnsets, 225 abilities, 85 held
items and 82 Mega Evolutions, each checked against Showdown as described
below. Any team that is legal in the format can be played, from turn 1 or
[from any position in the middle of a battle](#starting-from-the-middle-of-a-battle),
and the engine keeps track of
[what each player has been shown](#what-each-side-has-been-shown) of the
other's team. On top of it sit a [training environment](#training-a-bot)
that steps thousands of games at once and hands each side's view to a model
as arrays, a first, small self-play learner in PyTorch, and a
[client](#playing-on-showdown) that plays what it has learned on a Pokémon
Showdown server.

## Quick start

```sh
cargo test --release                       # includes recorded Showdown battles
cargo run --release --example random_battle
```

```rust
use vgc_engine::{Battle, PokemonSet};

// The four Pokémon each side picked at team preview; the first two lead.
let p1 = [
    PokemonSet::from_names("Arcanine", &["Flare Blitz", "Protect"], "Adamant", [32, 32, 0, 0, 2, 0])?
        .ability("Intimidate")?
        .item("Sitrus Berry")?,
    /* ... */
];
let mut battle = Battle::new([&p1, &p2], [1, 2, 3, 4])?;   // seed: Showdown's four 16-bit words

while !battle.ended {
    // battle.request says what is being asked: Request::Move at the start of a
    // turn, Request::Switch when someone has to be replaced. The latter can come
    // in the middle of a turn (after U-turn or Eject Button) as well as at its end.
    let c1 = pick(battle.joint_choices(0));   // every legal pair of slot choices for side 0
    let c2 = pick(battle.joint_choices(1));
    battle.choose([c1, c2])?;
}
println!("{:?}", battle.winner);              // Some(0), Some(1) or None for a tie
```

`Battle` is `Copy`: cloning a position for search is a plain memory copy
(about 8 kB). Choices use Showdown's conventions (`Choice::to_showdown`
prints `move 2 1`, `move 1 2 mega`, `switch 3`, `pass`), so they can be sent
to a Showdown server unchanged. A Pokémon holding its Mega Stone is offered
every move a second time with `mega: true`; a side can Mega Evolve once.
`Battle::new` returns `Error::Unsupported`, naming the culprit, for anything
that does not exist in Champions (Tinted Lens, a forme of a
species the game lacks) rather than guessing how it would behave.

A set built with `from_names` has no ability and no item until you add them,
and takes the species' fixed gender or male. `Battle::new` does not ask
whether a species can legally have an ability, move or item; neither does
Showdown's simulator. That is a separate question, answered per regulation:
see [Legal teams and regulations](#legal-teams-and-regulations).

## Starting from the middle of a battle

A bot searching from a live game does not start at turn 1. `Battle::from_state`
builds a battle from a description of the position, and `Battle::to_state`
writes one down:

```rust
use vgc_engine::position::{BattleState, CondState, PokemonState, SideState};

let mon = |species: &str, moves: &[&str]| PokemonState::new(species, moves);
let mut incineroar = mon("Incineroar", &["Fake Out", "Flare Blitz", "Parting Shot", "Protect"]).ability("Intimidate");
incineroar.hp_percent = Some(38.0);
incineroar.boosts[0] = -1;
let garchomp = mon("Garchomp", &["Earthquake", "Rock Slide", "Protect"])
    .item("Choice Scarf")
    .volatile(CondState::choice_lock("Rock Slide"));
let mut ours = SideState::new(vec![incineroar, garchomp, mon("Milotic", &["Scald", "Recover"]).status("par")]);
ours.conditions.push(CondState::new("tailwind").turns(1));
let theirs = SideState::new(vec![/* the first two are on the field, the rest on the bench */]);

let state = BattleState {
    turn: 6,
    sides: [ours, theirs],
    weather: Some(CondState::new("raindance").turns(2)),
    ..Default::default()
};
let battle = Battle::from_state(&state)?;       // waiting for turn 6's choices
```

Every field has a default, so a description only says what differs from
"nothing has happened": HP (exact or as a percentage), status and its
counters, stat stages, PP, current item, ability, types and forme, volatile
conditions with their timers (Taunt, Encore, a substitute's HP, a Choice
lock), side conditions, weather, terrain and Trick Room with the turns left,
who has fainted. `BattleState` is plain serde data: `to_json` and `from_json`
carry it across a process boundary, which is how a Python bot will hand over
a position. `cargo run --release --example position` builds one and does a
one-ply search from it; `src/position.rs` documents every field.

Three kinds of position can be described:

- **The start of a turn** (`request: Move`, the default).
- **Replacing the fainted at the end of a turn** (`request: Switch`): mark
  them `fainted`.
- **A switch in the middle of a turn** (after U-turn, Eject Button,
  Emergency Exit): set `switch_flag` on the Pokémon leaving and list the
  moves still to come in `pending`.

What to know before relying on it:

- **Exported positions are exact.** `from_state(to_state(b))` continues as `b`
  would, random numbers included. An exported position carries the
  simulator's bookkeeping (which moves the coming request disables, cached
  speeds, the order effects started in) and says so with `prepared: true`.
- **Hand-written positions leave the bookkeeping out**, and `from_state`
  works it out: disabled moves, trapping and locked moves come out as the
  engine would have had them. Two things cannot be known from outside and get
  a neutral default: the order in which effects started, with a flag Showdown
  keeps per field condition (both only settle ties between simultaneous
  effects), and the state of the random number generator (give a `seed`).
- **After editing an exported position**, set `prepared = false` (or call
  `without_bookkeeping()`), or the stale bookkeeping is believed.
- **Hidden information is yours to fill in.** A position has no "unknown":
  every Pokémon needs a species, moves and stats. Sampling the opponent's
  unrevealed moves, items, spreads and bench from a prior is not here yet.
- A Pokémon in the middle of a battle is taken to have been on the field for
  a turn and to have moved (so Fake Out is not offered); say `move_actions:
  Some(0)` and `active_turns: Some(0)` for one that has just come in.

How this is checked is under [How it is checked](#how-it-is-checked).

## Legal teams and regulations

What a regulation allows is data, one file per regulation in `formats/`,
and `format::Format` checks teams against it:

```rust
use vgc_engine::format::Format;

let format = Format::current();                     // Reg M-C, the one the engine is built for
for problem in format.check_team(&team) {           // all six, as registered
    println!("{problem}");                          // "Garchomp cannot learn Moonblast"
}
let rule = format.rule(garchomp.species).unwrap();  // its legal moves, abilities, genders, Mega Stones
let random = format.random_team(&mut rng);          // six legal Pokémon, drawn uniformly
```

The file lists every species that can be brought with its legal moves,
abilities and genders, the items anyone may hold, and the team rules (for
Reg M-C: bring six and pick four, no two Pokémon with the same Pokédex number,
no item held twice, at most 32 stat points in a stat and 66 in all).

**Nothing in the file is written by hand.** `oracle/gen_format.js` asks
Showdown's own team validator, one question at a time: is this species
accepted, and with this move, this ability, this item? So the file is what
Showdown accepts, by construction, and a new regulation needs no rules
transcribed.

**Moving to another regulation:**

```sh
node oracle/gen_format.js gen9championsvgc2026regmb     # any format id Showdown knows
scripts/check-teams.sh 5000 1 gen9championsvgc2026regmb # verdicts against Showdown's, both ways
```

```rust
let reg_mb = Format::from_json(&std::fs::read_to_string("formats/gen9championsvgc2026regmb.json")?)?;
```

The file also records whether the regulation changes anything the simulator
computes with, as opposed to what may be brought. Reg M-B, the previous
regulation, is in the repository as the example: 29 fewer species, 18 fewer
items, and two moves with different PP (Wish and Strength Sap), which the file
lists under `differences` and `Format::simulated_exactly` reports. Teams can
be checked against such a regulation, but battles under it would be off by
those differences until the engine's tables are regenerated from that data.

For a regulation newer than the pinned Showdown: move the pin
(`scripts/setup-oracle.sh`), regenerate the tables (`node oracle/gen_data.js`;
a new move or ability stops the build by name until its callback is written,
and `scripts/fuzz.sh` checks it), set the format id in `oracle/lib.js` and the
file name in `Format::current`, and generate its file. `Format::from_json`
refuses a file that names anything the engine's tables lack.

What the validator does not do, where Showdown's does:

- **It reports, it does not repair.** Showdown accepts a Mega written as the
  species (and turns it into the ordinary forme holding its stone) and
  corrects an impossible gender. Here both are violations.
- **Levels.** Everything is level 50. Showdown also refuses a Pokémon whose
  stated level is too low for it to have evolved; there is no level to state
  here.
- **Nicknames**, and Showdown's reminder that a Pokémon with no stat points at
  all and a Serious nature looks unfinished. (Teams written out for Showdown
  use Hardy for a neutral nature, for that reason.)
- **Picking four of the six.** `Battle::new` takes the Pokémon picked; team
  preview is not modelled.

## Teams from tournaments

Training needs teams people actually play. `scripts/scrape_teams.py` collects
the team sheets of a tournament from its Victory Road page, and `teampool`
turns them into a pool of teams the engine can play:

```sh
python3 scripts/scrape_teams.py https://victoryroad.pro/2027-frankfurt/     # -> teams/raw/2027-frankfurt.json
cargo run --release --bin teampool -- teams/raw/2027-frankfurt.json --play 1000   # -> teams/2027-frankfurt.json
```

The scraper reads the results tables, takes each row's team-sheet link
(to vrpastes.com) and saves where the team placed, who played it and its sheet
in Showdown's export format. A paste's page is an empty frame that a script
fills in once it is open in a browser, so the scraper asks the site's data
service for the team, as that script does. It makes one request a second and
keeps what it has fetched, so
it can be stopped and run again. `teampool` reads each sheet
(`teams::read_sheet`), checks the team against the regulation, writes the
legal ones out and says which it left out and why; `--section` and `--top`
narrow it to a division or to the best-placed, and `--play` plays battles
between teams of the pool as a check that every one runs.

```rust
use vgc_engine::teams::Pool;

let pool = Pool::from_json(&std::fs::read_to_string("teams/2027-frankfurt.json")?)?;
let (ours, theirs) = (pool.teams[0].sets()?, pool.teams[7].sets()?);
let battle = Battle::with_rosters([&ours, &theirs], [&[0, 1, 2, 3], &[0, 2, 4, 5]], true, seed)?;
```

A tournament's teams are a few hundred points in a very large space, so
`teams::Sampler` hands them out varied, a little differently each battle:

```rust
use vgc_engine::teams::{Sampler, Variation};

let sampler = Sampler::new(&pool)?;
let drawn = sampler.sample(&mut rng, &Variation::default());   // drawn.team: six Pokémon, always legal
```

`Variation` says how much. By default a quarter of the Pokémon have some of
their stat points moved from one stat to another they have a use for (the
total stays the same), one team in seven has a Pokémon replaced by one from
another team of the pool, as that team had it, and one in twenty has two
replaced. A replacement that would break the regulation is drawn again.
`Variation::NONE` gives the teams as they are. `teampool --play N --vary`
plays its check with varied teams and says how many were changed.

Two things to know about what comes out:

- **The stat points are guesses.** An open team sheet gives species, item,
  ability, moves and nature, and never how the 66 stat points are spent.
  `teams::guess_spread` spends them by rule of thumb from the nature and the
  moves (32 in each of two stats, 2 in a third: attack and Speed for a Jolly
  Garchomp, HP and Special Defense for a Careful Incineroar, HP and Speed
  for a Whimsicott whose one attack does not make it an attacker). Every
  team it did this to is marked `spreads_guessed`. Real spreads are finer,
  which is one reason the sampler moves points around; a paste that does
  give stat points is taken at its word.
- **The scraper depends on one address that is not Victory Road's to keep
  stable**: the data service the paste site's script talks to
  (`BACKEND` in the script, `--backend` to override). If it moves, the scraper
  stops and says that no team came back, rather than writing empty sheets,
  and `teampool` will not write a pool with no teams in it.
  `scripts/check_scraper.py` checks both steps without a network, on a
  made-up tournament whose answers have the shape of the real ones.

## Training a bot

The pieces, from the engine up:

| | | |
|---|---|---|
| `src/env.rs` | Rust | a game as a run of decisions (Team Preview, then the battle), actions as numbers, many games stepped at once on all cores |
| `src/obs.rs` | Rust | one side's view of a game written straight into arrays |
| `src/python.rs` | Rust | the two above as a Python module that fills NumPy arrays in place |
| `python/pokemon_ml/model.py` | PyTorch | embeddings, a small transformer, policy and value heads |
| `python/pokemon_ml/ppo.py`, `train.py` | PyTorch | PPO self-play and checkpoints |
| `python/pokemon_ml/league.py` | PyTorch | snapshots of the network, games between them, ratings |
| `src/follow.rs` | Rust | a battle on Showdown, followed from what a player is sent, as the same arrays |
| `python/pokemon_ml/showdown.py` | Python | a client that plays the network on a Showdown server |

```sh
python3 -m venv .venv && source .venv/bin/activate
pip install maturin numpy torch pytest
maturin develop --release                      # builds the engine into the Python package
python -m pokemon_ml.train --pool teams/2027-frankfurt.json --run runs/first
```

`train` plays `--envs` games at once (512) with the network on both sides
and updates it every `--steps` decisions (64). It saves
`runs/first/checkpoint.pt` as it goes, when `--hours` of wall-clock time are
up and on Ctrl-C, and the same command carries on from there, so a job with a
time limit loses nothing. The network's sizes are flags (`--width 128
--layers 3 --heads 4 --ff 256`, about 920,000 parameters); `--help` lists the
rest.

### How good is it?

Every `--eval-every` updates (25) a copy of the network is put aside in
`runs/first/snapshots/` and measured two ways.

**Against three scripted players**, each a higher bar than the last:

| | |
|---|---|
| random | any legal action |
| greedy | its strongest attack by power, type and accuracy; switches only when it must |
| look-ahead | tries its choices out in the simulator one turn ahead, against the greedy player's reply, and keeps the one that leaves it best off. It sees the other side's real stats, items and moves, which no player does: a bar to clear, not a model of fair play. It beats greedy two games in three. |

**Against earlier copies of itself**, which keeps telling something after the
scripted players are beaten: is the new version better than the old ones?
Each new snapshot plays the one before it, ones further and further back, and
the first. All results, the scripted players' included, are fitted with Elo
ratings on one scale (`runs/first/ratings.json`): the untrained network is 0,
and 100 points more is a 64% chance of winning, 200 is 76%, 400 is 91%. The
log then has lines of this shape (the numbers here are made up):

```
update    25  games    68,551  ...  | rating 412; wins 98% vs random, 41% vs greedy, 22% vs look-ahead
```

`python -m pokemon_ml.league --run runs/first --pool teams/2027-frankfurt.json`
plays more games between a run's snapshots than training had time for and
prints the table. None of this says how the network does against people: that
takes playing them, which is what [the Showdown client](#playing-on-showdown)
is for.

### What a model is given

One observation is one side's view at one decision
(`python/pokemon_ml/env.py` names the parts; `src/obs.rs` documents every number):

- **The field**: weather, terrain, Trick Room and the like, each side's
  screens, Tailwind and hazards, the turn, whether the team sheets are open.
- **Twelve Pokémon**, the player's six and the opponent's six in Team
  Preview order, so that a token is one Pokémon from Team Preview to the end.
  The player's own are given in full. The opponent's are given as
  [the battle has shown them](#what-each-side-has-been-shown): species, HP as
  the bar shows it, status, and the item, ability and moves that have come to
  light or are on an open team sheet. What is not known is the id "unknown",
  not a guess. Never the opponent's stats.
- **The four positions on the field**: who stands there, stat stages, types,
  the conditions it is under, the last move it visibly attempted (including
  a Choice-item refusal).
- **Speed**, the one thing a snapshot of the battle cannot show: for each
  opposing Pokémon the range its Speed can still be in given
  [who has moved before whom](#what-the-order-of-moves-shows), whether it can
  or must be holding a Choice Scarf, and for each pair of Pokémon facing each
  other, which goes first if both use moves of the same priority: this one,
  that one, or not known.
- **The eight moves** the player's two active Pokémon can pick from.
- **What is legal**, as a mask.

Species, items, abilities and moves are ids for the model to embed, and the
module hands over a table of static data for each (`TABLES`: a species'
types and base stats; a move's type, power, accuracy, priority, target and
effects). There is no damage calculator and no usage statistics in it: the
state of the game and what things are, as in the two write-ups this follows
([Jaxcalibur](https://jaxcalibur.github.io/) for singles, and mikumiku37's
account of adapting it to this format).

Three things are done to keep hidden information hidden. The conditions an
unrevealed item or ability keeps on a Pokémon (a Choice lock) are left out.
A weather, a terrain or a screen is given by how
long it has been up, not how long it has left, since an item the opponent
may not have seen makes it last 8 turns for 5. And a Pokémon's types are
given as the player has been told them, which is not always what they are
([below](#types-and-disguises)). `tests/env.rs` rebuilds the
opponent's half of the observation from `Battle::shown` alone at every
decision of 80 games and requires it to be the same.

And one rule covers the whole of it: **nothing is in the observation that a
player on Showdown is not told**, so that the network can
[play there](#playing-on-showdown) on exactly what it trained on. Showdown
tells a player what happens, line by line, and what its own Pokémon may do.
It does not say that a Pokémon will flinch before the flinch stops a move,
how many turns of a rampage are left, or what the simulator notes for itself
in the middle of a turn, so those conditions are left out for both sides
(`HIDDEN_VOLATILES` in `src/obs.rs` lists the sixteen). The player's own
stats are the ones its team was registered with. And what a player's
Pokémon is kept from, a disabled move or being trapped, is given as
Showdown gives it: with the Pokémon's choices, when it has a choice to
make.

With closed sheets, one thing a player can work out for itself is worked
out for it. A move of its own that it is told it cannot choose, with nothing
on show to account for that, has been sealed by the other side's Imprison:
so the Pokémon with Imprison up knows that move, and its token lists the
move from then on, used or not, on the field or off it
(`Battle::sealed_moves`). The same goes for a move stopped in the act
(`cant|…|move: Imprison|Protect`). Showdown tells a player which of its
moves are sealed for all but the last of its Pokémon to choose, and the
simulator and the client keep to that; where two opposing Pokémon have
Imprison up, or the one that has may be an Illusion, nothing is concluded.
Nothing of the kind is needed for abilities that trap. Under both
regulations here the only one is Mega Gengar's Shadow Tag, and a Mega's
ability is known to everyone the moment it Mega Evolves.

### What the order of moves shows

A player never sees the other side's Speed, but sees who moves first and
knows its own Pokémon's Speed exactly. `src/speed.rs` keeps what follows from
that, for both sides, from decision to decision.

For each opposing Pokémon it holds the set of ways the Pokémon can have been
built that are still possible: each amount of stat points in Speed (0 to 32),
with each thing a nature can do to Speed, with each thing an item can (nothing,
Choice Scarf, Iron Ball). It starts as everything the rules allow, which for a
Garchomp is a Speed of 109 to 169 before items, and an open team sheet narrows
it at once by giving the nature and the item. Every time one of theirs and one
of the player's own move in the same priority bracket, the combinations that
would have moved in the other order are struck out, after allowing for what is
public: stat stages, paralysis, Tailwind, Trick Room.

```rust
let mut game = Game::new([ours, theirs], false)?;     // Milotic (Speed 101) and Sylveon lead for us
game.act([[0, 0], [0, 0]], seed)?;                    // Team Preview
game.act([[0, 0], [0, 0]], seed)?;                    // a turn: their Garchomp and Snorlax move first
let snorlax = game.speeds().belief(1, 1);
assert_eq!(snorlax.items(), [true, true, false, false]);                // a Choice Scarf: no Snorlax reaches 101 without
assert_eq!(game.speeds().first(game.battle().unwrap(), 0, 0, 1), Some(First::Theirs));
```

Because the set is about how the Pokémon was built and not about one number,
"it must be holding a Choice Scarf" falls out when nothing else explains what
was seen, and Mega Evolution needs no special case: the same points and nature
give the new forme's Speed.

Items narrow it too. A Pokémon holds one item, so any item that shows itself
(a Quick Claw going off, Leftovers, a berry eaten) means no Choice Scarf and
no Iron Ball. And a team has each item once, so when one Pokémon turns out to
have been registered with the Choice Scarf, no team-mate was: their Speed is
their own. (That is kept from Pokémon that have since been handed an item by
Trick or Thief, which may hold anything, and from a team that does not keep
the item clause.)

A comparison is passed over whenever something the player has not been shown
could be at work: an ability the Pokémon may legally have that changes Speed
or priority as things stand (Swift Swim in rain, Prankster on a status move,
Stall, Unburden once the item is gone, Klutz), a Pokémon that may be an
Illusion, a Speed Swap. Passing one over costs a little knowledge; using one
wrongly would rule out the truth. `tests/speed.rs` plays games between random
legal teams, with every ability and item the format has, and checks at every
decision that the truth is still in each set and that no "this one goes
first" is wrong. Over 2,000,000 games (28.5 million decisions) neither
happened once. In those games a side could say who goes first in 58% of the
matchups it faced, an opposing Pokémon that had been seen ended with 63% of
its range of Speed left on average, and 11,514 Choice Scarves were worked
out without being shown. (`SPEED_GAMES=2000000 cargo test --release --test
speed` repeats it; the default is 3,000 games.)

### What a model answers

At Team Preview, one of the 90 ways to pick two leads and two more. In
battle, one action for each of the side's two positions, out of 47: a move
with its target and whether to Mega Evolve first, a switch to one of its six
by name, or a pass (`src/env.rs` has the table). The second position is
chosen given the first, since the two cannot switch to the same Pokémon or
both Mega Evolve.

### The network and the learning

The observation becomes 25 tokens (the field, twelve Pokémon, four
positions, eight moves) that a transformer mixes. A Pokémon's token is the
sum of the embeddings of its species, item, ability and moves and a
projection of its numbers. The policy is read off the tokens that stand for
what it can pick: a move's logits from the move's token and its target's, a
switch's from the token of the Pokémon coming in, Team Preview from pairs of
the player's own six. The value is read off the field token.

Learning is PPO with generalized advantage estimation on games the network
plays against itself. The only reward is the result (1 for a win, -1 for a
loss, when the game ends), undiscounted, with an entropy bonus and
Jaxcalibur's term that keeps every legal action from dying out. Teams come
from the pool, [varied from game to game](#teams-from-tournaments), half the
games with open team sheets and half without.

### How fast, and what has been checked

`cargo run --release --bin envbench` measures the environment by itself: on
one core of a 2.1 GHz cloud Xeon it steps about 45,000 decisions a second
with random players, writing both sides' observations at every one (22 µs a
decision: about 12 for the battle itself, 6 for the observations and the
legal actions, 4 for keeping track of Speed), and scales with cores. A
network on a GPU will be the slower half of the loop. Training speed on a
GPU has not been measured: the machine this was written on has none, and its
two cores train the default network at a few hundred decisions a second.

What has been checked is that the machinery is right, not that the default
settings are good ones: `tests/env.rs` (actions and choices agree, the masks
are exactly the legal actions, nothing hidden is in the opponent's view, a
fixed seed replays the same games) and `python/tests` (a choice's
probability is the same when it is made and when it is learned from, the
advantages match a calculation by hand, and 25 updates of a tiny network
raise its win rate against the random player). A longer run on that machine,
a quarter-size network on 100 random legal teams for 150 updates (43,000
games, 17 minutes), went from 50% to 99% against the random player and from
about 20% to about 50% against the greedy one, and stopped and resumed from
its checkpoint. That shows the loop learns. It says nothing yet about how
strong the default network gets on real teams with a GPU.

This is a first learner, kept simple on purpose. Not in it yet: a league of
past versions to play against (self-play against only the current network
can go in circles), tokens for what happened on earlier turns, heads that
predict the opponent's hidden sets and next action (which Jaxcalibur found
worth a lot), and search at play time.

## Playing on Showdown

`python -m pokemon_ml.showdown` logs a bot in to a Pokémon Showdown server
and plays battles there with a run's network: against whoever the ladder
finds, against a player it challenges, or against anyone who challenges it.

```sh
pip install websockets
python -m pokemon_ml.showdown --pool teams/2027-frankfurt.json --run runs/first \
    --server sim3.psim.us --name MyBot --password ... --ladder --games 10
```

| | |
|---|---|
| `--ladder`, `--challenge USER`, `--accept` (`--from USER`) | how it finds battles |
| `--run RUN`, `--snapshot 000300`, `--greedy` | the network that plays: a run's latest checkpoint or one of its snapshots, drawing from its policy or always taking the likeliest action. Without `--run` it picks legal actions at random, which is for trying a connection out |
| `--pool FILE`, `--team N` | the teams it plays with: one of the pool at random each battle, or always the Nth |
| `--server HOST:PORT` | `localhost:8000` unless told otherwise |
| `--name`, `--password` | the account (the password can come from `SHOWDOWN_PASSWORD`) |
| `--closed-sheets` | turn down open team sheets when they are offered |
| `--records DIR`, `--results FILE` | keep every battle's messages as they arrived; add one line a battle to a file of results |

It prints each result and, at the end, the share of battles won.

**A server of your own** is the place to start. The copy of Showdown the
engine is checked against is a whole server:

```sh
scripts/setup-oracle.sh
(cd oracle/pokemon-showdown && node pokemon-showdown start --no-security 8000) &
python -m pokemon_ml.showdown --pool teams/2027-frankfurt.json --run runs/first --name botb --accept --games 50 &
python -m pokemon_ml.showdown --pool teams/2027-frankfurt.json --run runs/first --snapshot 000100 \
    --name bota --challenge botb --games 50
```

That plays the run's latest network against an older one of its own through
Showdown itself: slower by far than the simulator (five or six battles a
second on a two-core machine, with a small network) and with the real game
as referee. The server also prints an address to open Showdown's web client
at, where a person should be able to pick a name and challenge the bot;
that has not been tried here.

### How a battle there becomes an observation

In training the observation is written from the simulator's state. On
Showdown there is no such thing: a player is sent a log (`|move|p2a:
Garchomp|Earthquake|...`, `|-damage|p1b: Sinistcha|37/100`) and, at each
decision, a request listing its own Pokémon and what they may do.
`Follower` (`src/follow.rs`) takes in the two and keeps what they add up to:
who stands where, stat stages, every condition on every Pokémon and side and
how long it has been there, what each opposing Pokémon has shown
([`observer`](#what-each-side-has-been-shown)), and what the order of moves
says of its Speed ([`speed`](#what-the-order-of-moves-shows), fed the moves
as the log gives them). From that it builds a `Battle` that is right in
everything the observation reads, and from there on the code is the code
training uses: the same function writes the arrays, the same function lists
the legal actions, and the action the network picks is put into Showdown's
words (`move 2 1 mega, switch 3`).

### What has been checked, and what has not

**That the follower sees what the simulator sees.** Battles are played in
Showdown with random teams and random choices, half with open team sheets,
and recorded with their logs and the requests each player was sent.
`followcheck` replays each in the simulator with a follower for each side
reading only what Showdown sent that side; at every decision the two must
give the same observation, number for number, and the same legal actions,
and the follower must put the choice that was made into the words Showdown
was sent. (About one battle in a hundred is left out, all for one reason:
with closed sheets, an Illusion on a Pokémon the regulation does not give
it, which the recorder's random teams allow and which nobody watching could
suspect.)

The follower was written against 10,600 such battles until none differed.
After that, sets were recorded one at a time, each run once untouched for an
honest number before anything it turned up was fixed:

| recorded | observations | differed when first run |
|---|---|---|
| 6,000 battles | 279,547 | 32 (0.011%), in 8 battles |
| 4,000 | 181,864 | 16 (0.009%), in 2 |
| 4,000 | 183,355 | 19 (0.010%), in 2 |
| 4,000 | 184,110 | 9 (0.005%), in 1 |
| 4,000 | 182,779 | 5 (0.003%), in 2 |
| 4,000 | 182,997 | 14 (0.008%), in 2 |
| 4,000 | 178,690 | 0 |

Every battle of the seven was followed to its end. Each difference in the
first six was traced to its cause and fixed, sixteen causes in all: with
that, none of the 1,373,342 observations of the seven differs. Not every
cause was the follower's. Two were the simulator's, which gave away more
than a player is told about a Pokémon in disguise: its real types as soon
as a type was added to them (Forest's Curse, Trick-or-Treat), and the types
a Reflect Type took from it ([below](#types-and-disguises)). The seventh
set is the first with nothing in it. That is one set: the ones before it
had a battle or two in 4,000 each, and something as rare may well remain.
(The first 10,600 were not kept and have not been run again since.)

Rare things can be made common, too. Two kinds of battle were recorded to
lean on the two differences known to remain (below): 2,400 with Pressure on
half the Pokémon and Counter, Mirror Coat, Metal Burst and Comeuppance all
round; and 2,000 with Choice Scarves, Magic Room, Trick, Instruct and
Encore. They turned up several of the causes fixed above, and what is left
in them is only the two things Showdown does not tell a player: 292 of
118,862 observations in the first, 10 of 128,173 in the second.

Five more were recorded to lean on what the later sets turned up, with
closed sheets, 1,200 to 1,500 battles each:

| every team has | and these moves all round | observations | differed when first run | now |
|---|---|---|---|---|
| Zoroark | Forest's Curse, Trick-or-Treat, Soak, Magic Powder, Reflect Type | 76,544 | 232 | 0 |
| | Smack Down, Gravity, Fly, Bounce, Sky Attack, Meteor Beam, Magnet Rise | 73,017 | 72 | 0 |
| | Gravity, Magnet Rise, moves that charge | 66,580 | 0 (76 on the code before it) | 0 |
| Zoroark | Reflect Type, Roost, Ally Switch, Roar, Dragon Tail, U-turn, Speed Swap, Quash, Instruct | 86,736 | 0 | 0 |
| Eject Button or Red Card | Octolock, Bind, Wrap, Quash, After You, Encore, Round, U-turn, Volt Switch, Speed Swap | 63,302 | 3 | 0 |

What they found, beyond the causes of the table's sets, were mistakes of
the follower's with a Pokémon of its own in disguise (it took it for the
Pokémon it was passing for until the next request, and filed what happened
in between under the wrong one); a Pokémon held back by Quash, or caught by
an Encore, forgotten to be so after a switch in the middle of the turn; and
a Round taken to call forward a Pokémon that an Encore turned to Round only
afterwards.

And two sets with open sheets and Illusion on half of all Pokémon, which
no regulation allows (2,000 battles). They turned up two more mistakes of
the follower's, both fixed: what it had noted of one Pokémon's Speed went
to another that came in under its name in disguise, and of two Pokémon
under one name it took the wrong one for the copy where one of them had
transformed. In the 493 battles where neither side brought more than one
Pokémon with Illusion, whatever its species, nothing differs now. Where a
side brought two or more, 72 of 104,604 observations differ, in four
battles. One was looked at: a Pokémon in disguise as another that has
Illusion too, the two then standing side by side under one name, where the
follower stays in doubt about both and the simulator's record does not.
These were left: Zoroark and Hisuian Zoroark are one species, and a team
has one.

`tests/follow.rs` keeps 56 of the battles as a fixture, and `followcheck`
(under [Running it yourself](#running-it-yourself)) runs any number more.

**That the pieces meet.** Through a server run as above, on this machine:
two bots playing random actions finish their battles with both ends
agreeing on every result and nothing sent that Showdown refused
(`python/tests/test_showdown.py`, with `SHOWDOWN_SERVER=localhost:8000`).
And a small network keeps its strength on the way through:

| a network after 60 updates (a quarter-size one, on 100 random legal teams) | in the simulator | through the server |
|---|---|---|
| against the random player, open sheets | 98.4% of 4,000 | 98.6% of 500 |
| against the random player, closed sheets | 98.3% of 4,000 | 98.8% of 500 |
| against itself 30 updates earlier, open sheets | 72.0% of 8,000 | 71.9% of 2,500 |
| against itself 30 updates earlier, closed sheets | 71.7% of 8,000 | 71.5% of 2,500 |

(One standard error is about 0.5 points on the simulator's figures in the
last two rows and 0.9 on the server's.) In those 6,000 battles Showdown
refused a choice eight times, each time for the reason described next.

**Where it and the simulator part.** Three things, all of them something
Showdown does not tell a player, or tells it late:

- *A choice that is barred by something not yet shown*: a foe's Imprison,
  or a Shadow Tag that has not announced itself. Showdown keeps this from
  the last Pokémon of a side to choose. It lists the choice anyway, refuses
  it if it is made, and sends the request again put right. The follower
  offers what Showdown lists and takes the corrected request like any
  other. The simulator, in training, does not offer the choice in the first
  place, so this is the one situation the network meets on Showdown and not
  in training (in the recorded battles, about one decision in 200, with
  random teams that are full of such moves).
- *Which of two Pokémon standing side by side under one name is the
  disguise.* Illusion copies the last healthy Pokémon of the team, so as a
  rule the copy is on the field first and the Pokémon itself comes in
  beside it. But a Pokémon that comes in on the left with nobody left in
  reserve may be either: the last Pokémon itself, or the Illusion Pokémon
  copying its neighbour. The log reads the same. The follower takes the
  newcomer for the copy until something settles it (the disguise breaks,
  one of them transforms). Not seen in the 30,000.
- *That a Pokémon of its own came in in disguise*, for a short while. A
  player's own Pokémon with Illusion is logged under the other's name for
  its own side too. The follower knows which it is from its HP, which a
  side is shown exactly for its own, or from having sent it in. Dragged in,
  and with the very HP of the Pokémon it passes for, it is taken for that
  one until the next request, which says who stands where.

#### Hidden move information

The last-move observation records the last visibly attempted queued move.
Showdown can announce a move rejected by a Choice item before it begins;
the simulator still keeps the actual last-used move for Encore and other
mechanics, and deducts no PP for that refusal.

A failed move marked `[still]` hides its target. If that target was chosen
randomly between foes with different Pressure costs, remaining PP is `-1`
(unknown) in both training and client observations. Exact PP remains in the
simulator for mechanics. An exact move request restores the normalized PP
feature; wait and switch requests do not. Uncertainty survives being benched.

The observation dimensions have not changed, but these feature meanings have.
Existing checkpoints still load; train new models before relying on the new
representation in competitive play.

#### Types and disguises

Types are public: Showdown names the new ones whenever a Pokémon's types
change. Two things keep that from being the whole story, and the simulator
keeps, for each Pokémon, the types each side takes it to have alongside the
ones it has (`Battle::seen_types`):

- A Pokémon in disguise is shown with the types of the Pokémon it passes
  for, and a type added to it (Forest's Curse, Trick-or-Treat) is added to
  those. A change that names the new types (Soak) is taken as said.
- Reflect Type names the Pokémon copied, not the types. Each side takes
  them for the types it sees on that Pokémon, so if it is in disguise its
  own side knows what was taken and the other side sees the disguise's.
  At the end of the turn Showdown says the new types if they are not the
  ones its log last gave that Pokémon. So a player whose Pokémon copied a
  foe in disguise learns the truth then (and learns it by silence too:
  nothing said means nothing changed). After copying a partner Showdown
  says nothing, and the other side goes on seeing the disguise's types.

The follower keeps the same for its own side by the same rules. One oddity
of Showdown's is read rather than copied: with a disguise standing beside
the Pokémon it copies, the end-of-turn line names whichever of the two
stands on the left, and gives the types of the one that was copied.

**Not checked: the public server.** Logging in with a password, searching
the ladder and the pace of messages (one every 0.65 seconds, under
Showdown's limit for ordinary accounts) are written from Showdown's
protocol notes and its server's source, and have never been run: the
machine this was written on cannot reach the public server. Expect to fix
something small the first time. Whether and where a bot may play there is
for Showdown's staff to say; ask before pointing it at the ladder. The bot
answers at once and does not use the battle timer.

## What each side has been shown

The engine knows every move, item and ability on both sides. A player does
not, and neither may a policy that is going to play real games.
`Battle::shown` reports what the battle has made public about one side:

```rust
let theirs = battle.shown(1);                // side 1 as everyone watching knows it
for mon in theirs.active.iter().flatten() {  // on the field; `bench` has those seen and withdrawn
    mon.species;    // "garchomp", and "garchompmega" once it has Mega Evolved
    mon.hp;         // 73: a whole percentage, the way Showdown shows a foe's HP
    mon.status;     // "par"
    mon.moves;      // ["earthquake", "protect"]: the ones it has used
    mon.item;       // Some("lifeorb") once the orb has hurt it; None until an item is shown
    mon.ability;    // Some("roughskin") once that has gone off
}
theirs.unseen;      // how many of the Pokémon they brought have not appeared yet
```

What a player knows is then their own side in full and `shown` of the other;
`shown` of their own side is what the opponent knows about them.

"Shown" means what Pokémon Showdown's battle log says, line for line:

- **Moves**: used, or named some other way: stopped by Taunt or Disable
  (`cant`), a Focus Punch tightening its focus, read by Forewarn, drained by
  Spite, topped up by a Leppa Berry. A move borrowed with Copycat is not the
  Pokémon's own and is not counted; one picked by Sleep Talk is. Nor are the
  moves of a Pokémon that has transformed.
- **Items**: named when they act (Life Orb, Leftovers, Rocky Helmet, an Air
  Balloon on entry), when they are used up or eaten, and when they are
  knocked off, stolen, swapped or found by Frisk. An item that has gone is
  remembered as lost. A Mega Evolution names its stone.
- **Abilities**: announced on entry (Intimidate, Drizzle) or named when they
  do something. Replacements are followed (Skill Swap, Mummy, Trace, Worry
  Seed, Transform), along with the ability the Pokémon returns to when it
  leaves the field. A Mega's ability is known from the Mega.
- **HP and status** as shown to the opponent; for a Pokémon on the bench, as
  they were when it left.
- **Illusion**: what a disguised Pokémon shows is credited to the Pokémon it
  passes for, and moves over to the real one if the disguise breaks. Until
  then the record says that it cannot be trusted (see below).

Nothing is worked out. Less damage than expected does not reveal an Assault
Vest, moving first does not reveal a Choice Scarf, and a Frisk that finds
nothing does not reveal an empty hand; that kind of inference is left to
whoever uses the record. Other limits: PP is not counted, and a disguise that
is never broken is never corrected, only marked as doubtful. Stat stages, volatile conditions, weather
and side conditions are public and are in the `Battle` itself, not repeated
here, though their timers are not public (the turns a Reflect has left give
away a Light Clay); packing all of it into one view for a policy is part of
designing the observation, which is the next piece of work.

What has been shown is part of a position: `to_state` writes it down for
every Pokémon (`PokemonState::shown`), with the teams as registered
(`SideState::roster`) and whether the sheets are open, and `from_state`
takes it back, so a search that starts in the middle of a battle keeps it. A
position written by hand that says nothing about it starts with nothing
shown but who is on the field, and takes each side to have registered the
Pokémon it lists.

`observer::Observer` is the same bookkeeping done from the other end: it
reads Showdown's log, as a program playing on Showdown receives it, and
returns the same `ShownSide`. The engine is checked against it (see
[How it is checked](#how-it-is-checked)), and it is the reader a Showdown
client will need.

### Team Preview and open team sheets

Some things are known before the first turn. Team Preview shows each player
the species the other registered, and Showdown's Champions formats add open
team sheets: both players are asked in a best-of-one (the sheets open if
both agree), and in a best-of-three they are open regardless. A sheet gives
every registered Pokémon's item, ability, moves and nature. It never gives
stat points, and it does not say which four of the six are brought.

`Battle::new` takes the four Pokémon each side brought and assumes nothing
more. `Battle::with_rosters` takes the teams as registered, which of them
each side brings, and whether the sheets are open:

```rust
let battle = Battle::with_rosters([&ours, &theirs], [&[0, 2, 3, 5], &[1, 2, 4, 5]], true, seed)?;
let theirs = battle.shown(1);
theirs.roster;                 // their six, in the order registered
theirs.roster[2].species;      // Team Preview shows this much, sheets open or not
theirs.roster[2].sheet;        // open sheets: Some(item, ability, moves, nature)
let lead = theirs.active[0].as_ref().unwrap();
lead.listed;                   // Some(2): the entry of the roster it appears to be
theirs.sheet_of(lead);         // that entry's sheet
lead.item;                     // still None: it has not shown its item, whatever the sheet says
```

The battle plays the same either way; only what is known from the start
differs. The sheet and the record are kept apart on purpose. The sheet says
what a Pokémon came with, the record what the battle has shown of it since
(the Life Orb on the sheet has been knocked off; the ability has been
swapped), and an observation can use both.

### Illusion

A Zoroark comes in looking like a team-mate, so on a team that has one, a
Pokémon that appears need not be what it appears to be, and neither its
sheet nor what was noted under its name can be taken at face value.

- `ShownSide::illusion` lists the registered Pokémon that have Illusion.
  With open sheets that is what the sheets say. With closed sheets it is the
  Pokémon whose species can have Illusion under the regulation, which Team
  Preview shows; this assumes the team is a legal one.
- `ShownMon::maybe_disguise` is true for every Pokémon that may be the
  Illusion Pokémon in disguise, or under whose name the Illusion Pokémon may
  have acted earlier. It is false only when that is ruled out: the team has
  no Illusion Pokémon, or that Pokémon has fainted, or it stood on the field
  as itself beside the one in question.
- When a disguise breaks, the Pokémon is who it is from then on, and what it
  showed in disguise moves to its own record.

The rule never calls a disguised Pokémon genuine (this is checked against
the truth: see below), and it stops there. It does not reason from the
sheets, as a player would on seeing "Garchomp" use a move only Zoroark's
sheet lists. Both the sheet and what was shown are in the record for a model
to draw that conclusion itself, like every other inference.

## How it is checked

`oracle/gen_cases.js` plays random battles in Pokémon Showdown's own simulator
under `[Gen 9 Champions] VGC 2026 Reg M-C` and records, at every decision, the
choices made, the choices that were legal, and the full state afterwards
including the RNG seed. `difftest` replays each battle here from the same seed
and compares.

That only works because the engine draws random numbers in exactly the order
Showdown does. Showdown consumes its RNG in places with no in-game meaning: it
re-resolves targets each time it re-sorts the action queue, and it sorts the
handlers of every event and shuffles the ones that tie in speed. The engine
reproduces those draws too. The benefit is that any mechanical mistake
desynchronises the two RNG streams and shows up within a turn or two.

Results for the code in this repository, against Showdown commit `ad7ca5d`
(8 October 2026):

| Check | Battles | Decisions | Diverged |
|---|---|---|---|
| Everything modelled now | 51,000 | 1,210,052 | 0 |
| Fresh battles, after the last fix (Healing Wish, below) | 20,000 | 447,681 | 0 |
| Recorded before two-turn moves, pivoting, forced switches and forme changes existed | 16,900 | 364,631 | 0 |
| Recorded before Fake Out, Substitute and the volatile conditions existed | 19,300 | 385,071 | 0 |
| Recorded before weather and the other field effects existed | 5,500 | 101,656 | 0 |
| Recorded before Mega Evolution | 21,000 | 393,381 | 0 |
| Items but no abilities | 5,000 | 93,086 | 0 |
| Neither (how the engine's first version was checked) | 15,200 | 270,196 | 0 |
| Comparing every individual RNG draw as well | 5,750 | 133,075 | 0 |
| 1,000-turn limit (both sides switch whenever they can) | 32 | 30,679 | 0 |

Each row compares state, RNG seed and legal choices after every decision.
State includes every Pokémon's species, stats, moves and PP (which change
under Transform), its volatile conditions with what they remember (a
substitute's HP, the move an Encore holds it to), the per-turn bookkeeping
Showdown keeps (who hit it last and for how much, whether it has moved), the
weather, terrain, pseudo-weathers and each side's conditions with their
remaining turns. The
older rows are battles recorded at earlier stages and replayed with the
current code.

How the battles are made up:

- Teams are drawn from all 293 species that can be brought. Each battle puts
  one move, one ability and one item on a lead, cycling through all of them,
  so every one gets its share.
- Abilities are the species' own half the time and any ability at all
  otherwise, which exercises abilities on bodies and movesets their real
  owners lack. About 80% of Pokémon hold an item.
- A species with a Mega Stone holds it half the time, and Mega Evolves at its
  first chance half the time, so Megas arrive early and late. A few Pokémon
  hold a stone they cannot use.
- About 30% of battles are "themed": every Pokémon draws from the same one to
  three abilities and items, so effects meet themselves and each other
  (Intimidate into Defiant, two Lightning Rods, Unnerve against berries) far
  more often than uniform sampling would manage.
- About a third are mirror matches or single-species teams, so that speed
  ties, and with them tie-breaking draws, are constant. A few percent have no
  attacking moves and are decided by Struggle.
- Some effects only matter in combinations that random teams almost never
  produce: Aurora Veil on a side that also has Reflect up, or a Mega Stone
  being stolen. `gen_cases.js --moves`, `--species`, `--abilities` and
  `--items` build batches around such a combination; about 30,000 of the
  battles in the first three rows are of this kind.

Further checks:

- **Legal choices.** The recorder lists legal choices from the Pokémon's real
  state rather than from the request Showdown sends the player, because the
  request deliberately hides some things (a Shadow Tag trap not yet revealed).
  `gen_cases.js --check-legal` confirms that list against Showdown's own
  validation by submitting every conceivable choice: 3,000 battles, 68,337
  decisions, no disagreement. (One oddity this turned up: when a foe's
  Imprison has sealed every move of a side's last active Pokémon, Showdown
  still lists the moves, and wants the forced Struggle spelled as a use of
  the first one, target included. `legal_choices` spells it that way.)
- **Legality.** `scripts/check-teams.sh` compares the team validator with
  Showdown's in both directions. Showdown judges random teams, most of them
  legal or one step from it (a move the Pokémon cannot learn, an ability from
  another species, an item held twice, a 33rd stat point, a second forme of
  the same Pokémon, five Pokémon); the engine gave the same verdict on all
  80,000 (40,000 for each of the two regulations, about 43% of them legal).
  And the engine makes random legal teams, which Showdown must accept: all
  80,000 were. The teams are built from Showdown's data
  directly, not from the regulation file, so the file is not checked against
  itself. Fourteen faults injected into the validator (a rule skipped, a
  limit off by one) were all noticed.
- **Positions.** `difftest --by-hand` replays the recorded battles and at
  every decision writes the position down (through JSON), builds a battle
  back from it and requires the two to be the same data, field for field,
  apart from scratch space. It then carries on with the rebuilt battle, which
  must keep matching Showdown to the end, so every one of those decisions was
  also the start of a battle begun in the middle. That holds for all of
  them: about 144,000 battles, 3.06 million decisions. At each
  decision it also strips the bookkeeping from the position, rebuilds from
  that, and requires what `from_state` works out (disabled moves, trapping,
  locked moves, who is to be replaced) to be what the engine had. Seven
  faults injected into the export and rebuild code (a counter not restored, a
  source forgotten, the event masks left empty) were all noticed. For
  positions nobody would write on purpose, `tests/position.rs` throws random
  conditions onto recorded positions; of 60,000, `from_state` refused about
  one in six (a Choice lock that names no move) and every one it accepted
  could be played on without the engine tripping.
- **What has been shown.** Here the engine is checked against a second
  program rather than against Showdown's state, because Showdown keeps no
  record of what a player knows; it only sends the log. `observer` reads
  that log and keeps its own account, and `difftest --shown` requires the
  engine's account to equal it, for both sides, at every decision of battles
  recorded with their logs, their teams as registered and open team sheets
  (`gen_cases.js --log --open-sheets`): 24,250 battles, 636,029 decisions,
  no difference. Every battle is checked twice, once as recorded and once as
  if the sheets had stayed closed (`--closed-sheets`, which keeps them from
  both the engine and the reader). The two are written from opposite ends,
  the engine at the place each mechanic happens and the reader from the text
  alone, so a rule missing from either shows up. Four things keep the pair
  from being wrong together:
  - *Who a line is about.* A line naming an ability or item mentions one or
    two Pokémon and Showdown has no single rule for which of them has it.
    `gen_cases.js --holders` asked Showdown, on every such line of about
    20,000 battles, which Pokémon really had the thing; the reader's table
    was drawn up from the answers.
  - *Truth.* Everything the reader believes about a Pokémon's moves, item
    and ability is compared with the real Pokémon: in battles without
    Illusion for every record, and in battles with it for the records not
    marked as possibly a disguise's. Nothing it believed was false.
  - *Who is who.* The sheets the engine gives out are compared with the ones
    Showdown sends, and the entry a Pokémon is said to appear as with the
    Pokémon it is. On teams that brought an Illusion Pokémon, a Pokémon
    stood on the field in disguise at 7,218 decisions and was marked as a
    possible disguise at every one of them. Pokémon that were themselves
    were known to be at 21,951 and left in doubt at 28,226. (With closed
    sheets, on teams where only Pokémon that may have Illusion had it:
    2,341 in disguise, all marked; 7,280 known; 10,012 in doubt.) Most of
    those teams are ones the recorder builds to be hard, with several
    Illusion Pokémon or five Zoroark, where everything stays in doubt; on
    ordinary teams with one Zoroark about half are known.
  - *Coverage.* The engine records something in 146 places. Every one was
    reached in those battles, and 140 were at some point the first to reveal
    something, so that taking any of them out would have shown as a
    difference; the other six can only repeat what an earlier line said
    (the move a Leppa Berry tops up has been used already). Every move, item and
    ability in the game was shown somewhere, except four abilities: Battle
    Bond, Ice Face and Shields Down, which belong to formes the game lacks,
    and Stance Change, which no line names.

  `difftest --shown --by-hand` also carries the record, the rosters and the
  sheets through an exported position at every decision of the same battles,
  with no difference. Fresh
  battles were the real test, and are the reason not to read "no difference"
  as "finished". The first 50,000 played after the rules had settled turned
  up four gaps in them, each of which now has a batch of its own in
  `scripts/targeted.sh`: Big Pecks and Clear Body blocking Octolock's drops
  in silence; Symbiosis handing over an item between the two lines a
  damage-halving berry gets; Poison Touch poisoning after Wandering Spirit
  had already replaced it; and a Fling whose line names no item, because the
  Lum Berry being thrown was eaten on the way. The 40,000 after that turned
  up none, and the 20,000 after those one: an Uproar borrowed with Copycat
  and then disabled, whose `cant` line made it look like the Pokémon's own.
  That one, and a like case with Choice items that had been known and left
  alone, are now read correctly (a move just borrowed is not taken for the
  Pokémon's own by the next line that names it). The 20,000 after that,
  checked with the sheets open and closed, turned up nothing.
  Eight faults injected into the rules for team sheets and Illusion were
  all noticed, six by the recorded battles and two by `tests/shown.rs`:
  where the engine and the reader share a function, comparing them cannot
  catch a fault in it, and only the check against the truth or a test can.
- **A line of play, written out.** Random battles say that the mechanics
  agree, not that a particular plan works. `gen_cases.js --script` plays
  battles from a file of teams and choices in Showdown and records them like
  any other, so the engine can be held to a line of play.
  `scripts/perish_trap.json` has four, around the perish trap: Perish Song
  beside a Mega Gengar whose Shadow Tag keeps the other side in for three
  turns, with the singers leaving on the third and fourth; the ways out (a
  Ghost, a Shed Shell, U-turn, Soundproof); Mean Look letting go when its
  user leaves; and two against two with nobody able to leave, where all four
  faint at once and Showdown gives the battle to the side whose Pokémon went
  last. The engine matches each at every decision, legal choices included
  (confirmed against Showdown's own validation with `--check-legal`), and
  `tests/perish_trap.rs` walks through them. A batch of 600 random battles
  on the same theme agrees too (17,625 decisions): in it 739 Pokémon fainted
  to the count, 242 of them held in with somewhere to switch to, 78 of
  those on a turn when a foe left the field, and 57 battles ended with both
  sides' counts running out together.
- **Does the comparison have teeth?** `scripts/mutation_test.py` injects one
  small bug at a time (Life Orb's multiplier off by 1/4096, Intimidate
  lowering by two stages, Mold Breaker ignored, Sitrus Berry restoring a third) and
  replays recorded battles. A second mode switches off one callback at a time
  (one ability's reaction to one event, one move's script). All 415
  hand-written bugs and all 602 switched-off callbacks are caught. Another 15
  callbacks, and eight hand-written bugs that were tried, change nothing
  that can be observed in Champions (Ripen doubling the stat changes of
  berries, where no berry in the game changes stats); the script lists each
  with its reason. Random battles are not enough for this: with 3,000
  general battles, 88 of the 1,017 bugs were only caught by a batch built
  around the effect, such as Sleep Talk on a Pokémon that also knows Rest
  and Meteor Beam, or Dragon Darts into a Protect beside a Berserk Pokémon
  at just over half HP. `scripts/targeted.sh` records all such batches.

What this does **not** establish: agreement with the cartridge games where
they differ from Showdown. And coverage of rare interactions is uneven. Every
move, ability and item was brought into hundreds of battles, but an effect
that singles out one particular move or ability only shows when the two meet.
Five such cases were wrong at some point while this was being written and got
past thousands of random battles: Reckless boosting High Jump Kick, Sheer
Force boosting Electro Shot, Cud Chew ignoring a berry eaten with Bug Bite,
Magician not stealing after Fling, and a frozen Pokémon that has lost its
Fire type getting no thaw from Burn Up. They were found by reading Showdown's
source for every place that names a newly modelled move, and by batches
built around the pairing. Both are now routine for anything added, and the
mutation tests above say which pairings the recorded battles cover; but it is
the kind of error most likely to remain. A sixth turned up later, in 70,000
fresh battles played to check what each side has been shown: a Healing Wish
left waiting on a position stops working for Pokémon that switch in once
Ally Switch has moved a healthy one onto it. That one is a slip of
Showdown's (it is in the list further down) and took two battles in those
70,000 to show.

### Running it yourself

```sh
scripts/setup-oracle.sh            # clone and build the pinned Showdown commit (needs Node 22+)
scripts/fuzz.sh 2000               # 2,000 fresh battles, random seed
TRACE=1 scripts/fuzz.sh 300 42     # also compare every RNG draw
REBUILD=1 scripts/fuzz.sh 2000     # also rebuild the battle from its position at every decision
SHOWN=1 scripts/fuzz.sh 2000       # also check what each side has been shown, with open and with closed sheets
node oracle/gen_cases.js --script scripts/perish_trap.json --check-legal --out trap.jsonl   # battles written out
```

The mutation test needs recorded battles to replay: the batches built around
particular effects, and a few thousand general ones.

```sh
scripts/targeted.sh corpus                                              # 67 batches, about 21,000 battles
node oracle/gen_cases.js --n 3000 --seed 9500 --out corpus/general.jsonl
scripts/mutation_test.py corpus/*.jsonl --handlers                      # slow: a rebuild per injected bug
```

When a battle diverges, `difftest` prints the fields that differ. Re-record
that one battle with tracing to see where the RNG streams part:

```sh
node oracle/gen_cases.js --seed 42 --only 17 --trace --out one.jsonl
cargo run --release --features trace --bin difftest -- one.jsonl
```

The report lines up the engine's draws (each labelled with its purpose)
against Showdown's (each labelled with its call stack) and marks the first one
that differs, followed by Showdown's log for the turn. To hammer on particular
effects, `gen_cases.js --abilities intimidate,defiant --items whiteherb` makes
every battle a themed one over just those.

`SHOWN=1 scripts/targeted.sh corpus` records the same batches with their
logs and open team sheets (about 4 GB) and runs `difftest --shown` on each,
with the sheets open and as if they had stayed closed. When that reports a
difference it names the field, the two values and the first battle and
decision where it happened.

The [follower of Showdown's log](#playing-on-showdown) is checked on
battles recorded with what each player was sent:

```sh
node oracle/gen_cases.js --n 2000 --seed 1 --log --requests --out closed.jsonl
node oracle/gen_cases.js --n 2000 --seed 2 --log --requests --open-sheets --out open.jsonl
cargo run --release --bin followcheck -- closed.jsonl open.jsonl      # --list: the first difference of each battle
cargo run --release --bin followcheck -- open.jsonl --battle 17       # every difference of one battle
```

## How abilities, items and conditions work

The engine has a port of Showdown's event system (`src/events.rs`): `runEvent`,
`singleEvent`, `eachEvent` and `fieldEvent`, with the same handler ordering
(order, priority, speed, sub-order) and the same suppression rules (Mold
Breaker, Klutz, Cloud Nine, a status that has since changed). Effects can sit
on a Pokémon, on one of a side's positions, on a side, or on the whole field.

- **Which effect listens to which event, with what priority, is generated**
  from Showdown's data into `src/tables.rs`. This matters beyond convenience:
  a handler that does nothing still changes the RNG stream if it ties with
  another one, so the set of handlers has to be exactly Showdown's.
- **What each callback does is written by hand** in `src/abilities.rs`,
  `src/items.rs`, `src/conditions.rs` and `src/movecbs.rs`, one match arm per
  Showdown callback,
  with the callback's name and parameters in a comment above it. A callback
  listed in the tables but missing a body panics with its name, and the
  generator refuses any callback for an event the engine does not know.

When Showdown's Champions data gains an effect: run `node oracle/gen_data.js`.
It marks as unsupported any move that uses a property or event the engine has
not seen (and says which); for the rest, write the bodies of the callbacks
that now panic, and fuzz.

## What is and is not modelled

Modelled, and verified as above: everything that can happen in a battle
between two legal Champions teams.

- Champions stat formula, natures, level 50, Champions PP values
- Action order: priority, speed, speed ties, switches before moves, dynamic
  re-sorting after each action, fractional priority (Quick Claw, Stall),
  moves that reorder the queue (After You, Quash, Round, Instruct)
- Targeting in doubles: chosen targets, spread moves and their 0.75 modifier,
  retargeting when a foe has fainted, moves aimed at an ally, redirection
  (Follow Me, Rage Powder, Lightning Rod), Dragon Darts, Ally Switch
- Damage: critical hits, damage rolls, STAB, type chart and immunities, burn,
  and every modifier event abilities and items hook into
- Burn, paralysis, poison, toxic, sleep and freeze with the Champions rules
  (1-in-8 full paralysis, sleep for 2 or 3 turns, freeze for at most 3)
- **All 510 moves** in the Champions learnsets. By kind:
  - Protect and its relatives, Fake Out, Helping Hand, Wide Guard, Quick
    Guard, Feint, Endure
  - Substitute, Taunt, Encore, Disable, Torment, Imprison, Yawn, Leech Seed,
    Perish Song, Destiny Bond, Curse, the binding and trapping moves and the
    other lingering conditions
  - Two-turn moves (Fly, Solar Beam, Electro Shot), recharge moves, rampages
    (Outrage), Focus Punch, Counter, Mirror Coat, Metal Burst
  - **Pivoting** (U-turn, Volt Switch, Parting Shot, Chilly Reception, Baton
    Pass, Shed Tail) and **forced switches** (Roar, Whirlwind, Dragon Tail)
  - Moves whose power depends on the battle (Gyro Ball, Eruption, Acrobatics,
    Stored Power, Last Respects, ...)
  - Moves that tamper with items, abilities and types (Knock Off, Trick,
    Fling, Skill Swap, Entrainment, Gastro Acid, Soak, ...)
  - Moves that call other moves (Copycat, Sleep Talk), Future Sight, Healing
    Wish, Revival Blessing, Transform
- **Weather and terrain**, Trick Room, Gravity, Magic Room, Wonder Room,
  Tailwind, the screens, Safeguard, the entry hazards and the moves that
  clear them, Wish
- **All 225 abilities**, including the ones that switch a Pokémon out
  mid-turn (Emergency Exit), change its forme (Stance Change, Disguise, Zero
  to Hero, Hunger Switch), or disguise it (Illusion, Imposter)
- **Mega Evolution**: all 82 Megas
- **All 85 held items**, Eject Button and Red Card included. Items Showdown
  marks as unavailable in Champions (Choice Band, Choice Specs, Assault Vest
  among them) are left out; Choice Scarf is the only Choice item
- Switching, fainting, replacements in the middle of a turn and at its end,
  PP, Struggle, trapping, disabled moves, win and tie conditions, the
  1,000-turn limit

Not modelled:

- **Hidden information.** A position
  ([above](#starting-from-the-middle-of-a-battle)) has to say everything
  about both sides. Sampling what a player has not been shown is the next
  piece of work.
- Team preview: the engine starts from the four Pokémon each side picked, in
  the order picked.
- Anything Champions does not have: Terastallization, Z-moves, Dynamax, and
  the species, moves, abilities and items outside the format. A few abilities
  are legal to put on a Pokémon but belong to species or mechanics the game
  lacks (Ice Face, Gulp Missile, Shields Down, Battle Bond, Tera Shell, the
  four Embody Aspects); they do nothing, exactly as in Showdown.
  `oracle/coverage.json` lists these under `dormant_parts`.
- The battle log as text. The engine tracks state, not messages. What the
  messages reveal is tracked (see
  [What each side has been shown](#what-each-side-has-been-shown)): Frisk
  shows the items it finds and Forewarn the move it picks, and Anticipation
  shows itself.

### Showdown behaviour worth knowing about

The engine reproduces Showdown, not the cartridge. A few places where
Showdown's behaviour is surprising, all confirmed against its source and
reproduced here:

- A move that is vetoed while being set up (a sound move chosen before Throat
  Chop landed, a flying move under Gravity) still draws one random target
  before it fails.
- Metal Burst and Comeuppance return 1.5 times the damage taken without
  rounding, so a substitute can be left with half a hit point. The engine
  stores substitute HP in halves for this reason.
- Curse chosen by a Pokémon that is not a Ghost targets the user from the
  moment it is chosen, even if the Pokémon becomes a Ghost before moving.
- A forced switch (Roar) draws a random number to pick the replacement even
  when only one Pokémon can come in.
- With Revival Blessing and a forced replacement pending on the same side,
  Showdown counts available switches in slot order, so "revive with the left
  slot, replace the right" can be rejected where the mirror image is
  accepted. `legal_choices` applies the same rule.
- A Pokémon revived into its own active slot comes back through a switch
  queued at the end of the turn's remaining actions.
- Healing Wish waits on its position until a Pokémon it can help arrives.
  If Ally Switch moves a healthy Pokémon onto that position first, the wish
  stops working for Pokémon that switch in there, and only heals one that
  another Ally Switch brings. (Running the wish's handler for the swap
  leaves a Pokémon recorded as the wish's target, and the code that handles
  switch-ins then takes the wish for a condition of that Pokémon which it no
  longer has.)

And in what the log gives away:

- In Champions, Regenerator and Natural Cure are written into the log when
  a Pokémon is withdrawn, as lines the client does not display but every
  program reading the log receives. Both abilities count as shown the first
  time they act; a human opponent would not have noticed.
- Stance Change and Hunger Switch change the Pokémon's forme without the
  line naming the ability, unlike in Showdown's other formats.
- Big Pecks, Clear Body and White Smoke say nothing when the drop they block
  is Octolock's.
- A move borrowed with Copycat can carry on over the following turns
  (Uproar, Fly), and if something then stops it, the line that says so reads
  as if the move were the Pokémon's own: `|cant|p1a: A|Disable|Uproar`, or,
  when a Choice item is what stops it, a `move` line like that of any move
  that failed. `shown` passes over the first line that names a move just
  borrowed.
- When two allies swap abilities (Skill Swap on a partner), the log does not
  name the abilities. When the swap was a Wandering Spirit's, set off by its
  own partner hitting it, that much can still be read from the line, since
  no move called Skill Swap was used; `shown` does read it.
- An ability can act once more after it has been replaced, when it had
  already begun: Poison Touch poisons through the same hit whose contact
  swapped it for Wandering Spirit or Mummy, and the line names it as the
  attacker's. `shown` goes by the replacement.

## Layout

```
src/battle.rs      the simulator core: state changes, the queue, the turn loop
src/moves.rs       using a move, from "can it move" to damage and secondaries
src/events.rs      Showdown's event system: handler discovery, ordering, dispatch
src/abilities.rs   ability callbacks        (one arm per Showdown callback)
src/items.rs       item callbacks
src/conditions.rs  callbacks of statuses, volatiles, weather, terrain and side conditions
src/movecbs.rs     script callbacks of moves (onTry, onHit, basePowerCallback, ...)
src/choice.rs      Battle::new, legal choices, submitting choices
src/position.rs    a position as data: Battle::to_state, Battle::from_state, JSON
src/format.rs      a regulation as data: Format, the team validator, random legal teams
src/teams.rs       teams from outside: reading team sheets, guessing stat points, a pool of teams
src/env.rs         games for a model to play: Team Preview, actions as numbers, many games at once
src/obs.rs         one side's view of a game as arrays, and static data to embed ids with
src/speed.rs       what the order of moves has shown about each Pokémon's Speed
src/python.rs      the two above as a Python module (feature `python`, built by maturin)
src/shown.rs       what the battle has shown of each Pokémon: Battle::shown
src/observer.rs    the same, read from Showdown's log
src/follow.rs      a whole battle followed from what Showdown sends one player, as the model's observation
src/state.rs       fixed-size state: Battle, Side, Pokemon, the action queue
src/data.rs        data definitions; src/tables.rs is generated (do not edit)
src/rng.rs         Showdown's Gen5RNG
src/replay.rs      replays a recorded battle and reports the first difference
src/trace.rs       optional RNG/action trace (feature `trace`)
src/bin/difftest.rs, src/bin/bench.rs, src/bin/teamcheck.rs, src/bin/teampool.rs, src/bin/envbench.rs, src/bin/followcheck.rs
python/pokemon_ml/  the learner: env.py, model.py, ppo.py, train.py, league.py; showdown.py plays it on a
                    Showdown server; python/tests/ checks them (pytest)
pyproject.toml     how maturin builds the engine into that package
oracle/lib.js        what counts as modelled (the move properties and events the engine knows)
oracle/gen_data.js   Showdown data  -> src/tables.rs, pool.json, coverage.json
oracle/gen_cases.js  Showdown battles -> recorded cases (JSON lines)
oracle/gen_format.js Showdown's team validator -> formats/<id>.json
oracle/gen_teams.js  random teams with Showdown's verdict on each
formats/             what each regulation allows (generated)
scripts/             setup-oracle.sh, fuzz.sh, targeted.sh, check-teams.sh, mutation_test.py,
                     perish_trap.json (battles written out for gen_cases.js --script),
                     scrape_teams.py (a tournament's team sheets), check_scraper.py
teams/               pools of teams (teampool's output); teams/raw/ is what the scraper fetched
tests/parity.rs    fixture of recorded battles, choice-validation checks
tests/effects.rs   a few abilities and items checked directly, as API examples
tests/position.rs  positions written by hand: defaults, timers, switches in the middle of a turn
tests/format.rs    legal and illegal teams, a second regulation, fixtures of teams judged by Showdown
tests/shown.rs     what each side has been shown, as examples; a fixture of battles recorded with their logs
tests/perish_trap.rs  the perish trap, step by step, on battles played out in Showdown from a script
tests/teams.rs     team sheets read, stat points guessed, a pool saved, loaded and played
tests/env.rs       the training environment: actions, masks, what each side is given, many games at once
tests/speed.rs     Speed worked out from the order of moves: an example, and that it is never wrong
tests/follow.rs    the follower against the simulator on battles recorded in Showdown, and its use from outside
```

Everything in `src/battle.rs`, `moves.rs` and `events.rs` is a
function-by-function port of Showdown's `sim/` (comments name the function
mirrored). After updating Showdown, run `node oracle/gen_data.js` to
regenerate the tables.

## Speed

`cargo run --release --bin bench -- cases.jsonl` plays random battles to
completion using the teams in a case file. On one core of a 2.1 GHz cloud
Xeon:

| Teams | Battles/s | Decisions/s |
|---|---|---|
| No abilities or items | about 7,100 | about 127,000 |
| Random abilities and items | about 4,300 | about 82,000 |
| The same with weather, terrain and the other field effects in play | about 4,200 | about 84,000 |
| The same with Substitute, Encore and the other volatile conditions in play | about 3,600 | about 79,000 |
| The same with every move in play (two-turn moves, pivoting, forme changes) | about 3,500 | about 80,000 |

Keeping the record of what each side has been shown costs nothing that can
be measured. Nothing has been tuned beyond skipping events that nobody in the battle
listens to, and it shows: the last batch of mechanics cost 15 to 30% on the
same teams (the first row was about 10,000 before it), from a larger state
and more bookkeeping per action rather than from any one hot spot, and the
event system had already halved the speed of the first version, which ran
about 20,000 battles per second with nothing to dispatch. Caching each
Pokémon's listener set and slimming the per-move scratch state are the
likely first steps when speed starts to matter.

## What comes next

1. **A league.** Past versions of the network as opponents in training, so
   that self-play cannot go in circles. (They are already kept, and rated
   against each other: see "How good is it?".)
2. **More for the network to go on**: what damage has shown about attack and
   bulk, kept the way Speed is; and heads that predict the opponent's hidden
   sets and next action.
3. **Sampling hidden information** into a position: the opponent's
   unrevealed moves, items, abilities, spreads and bench, drawn from the
   network's own predictions and consistent with what has been shown. This
   is what search needs.
4. **Search at play time**: one turn ahead as the simultaneous game it is,
   over sampled worlds.
5. **The public server.** The [Showdown client](#playing-on-showdown) has
   played only on a server of its own: a first session on the real one,
   then results there kept beside the ratings.
6. **Searching for teams**: the regulation file gives the space of legal
   teams and steps that stay inside it; scoring a team needs a pool of
   opponents and a policy to play it.
7. **Speed.** The per-Pokémon listener cache described above, then
   profiling.
8. Keeping up with Showdown: `scripts/setup-oracle.sh` pins a commit; after
   moving the pin, `node oracle/gen_data.js` regenerates the tables, a
   missing callback body panics with its name, and `scripts/fuzz.sh` finds
   behaviour changes.
