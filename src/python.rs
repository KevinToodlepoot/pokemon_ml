//! The training environment as a Python module (`pokemon_ml._engine`),
//! built with `maturin develop --release`. Everything is handed over as
//! NumPy arrays that Python owns and Rust fills in place, with the
//! interpreter lock let go while the games are stepped.

use numpy::{PyArray1, PyArrayMethods, PyReadonlyArray1, PyReadonlyArray2, PyReadwriteArray1, PyReadwriteArray2};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

use crate::data::{ABILITIES, ITEMS, MOVES, SPECIES};
use crate::env::{Baseline, Config, Game, GameState, N_ACTIONS, N_PREVIEW, OBS_M, VecEnv, preview_table};
use crate::follow::Follower;
use crate::format::{Format, ShowdownSet};
use crate::obs::{self, INFO, OBS_F, OBS_I, Phase, Table};
use crate::position::BattleState;
use crate::rng::Rng;
use crate::search::{
    self, Budget, Candidates, Cap, CellId, Follow, Leaves, Mode, Plan, SUMMARY, SimOpts, SolveOpts, Status,
};
use crate::state::{ACTIVE, Battle, Choice, Request};
use crate::teams::{Pool, Variation};

fn failed(e: crate::Error) -> PyErr {
    PyRuntimeError::new_err(format!("{e:?}"))
}

fn whole<'a, T: numpy::Element>(a: &'a mut PyReadwriteArray2<'_, T>, what: &str) -> PyResult<&'a mut [T]> {
    a.as_slice_mut().map_err(|_| PyValueError::new_err(format!("{what} must be a C-contiguous array")))
}

/// Many games at once. See `pokemon_ml.env` for the Python side of it.
#[pyclass(name = "VecEnv")]
struct PyVecEnv {
    env: VecEnv,
}

#[pymethods]
impl PyVecEnv {
    /// `pool` is the text of a team pool (`teams/<name>.json`, as `teampool` writes it).
    /// `check_teams`: refuse a pool team the regulation does not allow (the
    /// default); off, any team the engine can simulate is played as it is.
    /// `side`: the side of a split pool to play, "train" by default; a pool that
    /// says it is another side is refused. `None` plays any side.
    #[new]
    #[pyo3(signature = (
        pool, envs=256, seed=1, open_sheets=0.5, vary=true, max_turns=100, threads=0, check_teams=true,
        side=Some("train".to_string())
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        pool: &str,
        envs: usize,
        seed: u64,
        open_sheets: f64,
        vary: bool,
        max_turns: u16,
        threads: usize,
        check_teams: bool,
        side: Option<String>,
    ) -> PyResult<Self> {
        let pool = Pool::from_json(pool).map_err(failed)?;
        let variation = if vary { Variation::default() } else { Variation::NONE };
        let config = Config { envs, seed, open_sheets, variation, max_turns, threads, check_teams, side };
        Ok(PyVecEnv { env: VecEnv::new(&pool, config).map_err(failed)? })
    }

    #[getter]
    fn envs(&self) -> usize {
        self.env.len()
    }

    #[getter]
    fn threads(&self) -> usize {
        self.env.threads()
    }

    /// Fills the observations of every game as it stands: `f` (2·envs × OBS_F float32),
    /// `i` (2·envs × OBS_I int16), `mask` (2·envs × OBS_M uint8).
    fn observe(
        &self,
        py: Python<'_>,
        mut f: PyReadwriteArray2<'_, f32>,
        mut i: PyReadwriteArray2<'_, i16>,
        mut mask: PyReadwriteArray2<'_, u8>,
    ) -> PyResult<()> {
        let (f, i, mask) = (whole(&mut f, "f")?, whole(&mut i, "i")?, whole(&mut mask, "mask")?);
        self.sized(f.len(), i.len(), mask.len())?;
        let env = &self.env;
        py.detach(|| env.observe(f, i, mask));
        Ok(())
    }

    /// Both sides of every game act (`actions`: envs × 4 int32), and the observations, the
    /// results (`reward`: envs × 2 float32) and which games ended (`done`: envs uint8) are filled in.
    #[allow(clippy::too_many_arguments)]
    fn step(
        &mut self,
        py: Python<'_>,
        actions: PyReadonlyArray2<'_, i32>,
        mut f: PyReadwriteArray2<'_, f32>,
        mut i: PyReadwriteArray2<'_, i16>,
        mut mask: PyReadwriteArray2<'_, u8>,
        mut reward: PyReadwriteArray2<'_, f32>,
        mut done: PyReadwriteArray1<'_, u8>,
    ) -> PyResult<()> {
        let actions =
            actions.as_slice().map_err(|_| PyValueError::new_err("actions must be a C-contiguous int32 array"))?;
        let (f, i, mask) = (whole(&mut f, "f")?, whole(&mut i, "i")?, whole(&mut mask, "mask")?);
        let reward = whole(&mut reward, "reward")?;
        let done = done.as_slice_mut().map_err(|_| PyValueError::new_err("done must be a contiguous array"))?;
        self.sized(f.len(), i.len(), mask.len())?;
        let n = self.env.len();
        if actions.len() != 4 * n || reward.len() != 2 * n || done.len() != n {
            return Err(PyValueError::new_err(format!("actions must be {n} × 4, reward {n} × 2 and done {n}")));
        }
        let env = &mut self.env;
        py.detach(|| env.step(actions, f, i, mask, reward, done)).map_err(failed)
    }

    /// What a player that needs no model ("random", "greedy" or "lookahead") would do on each side of every game.
    fn baseline(&mut self, py: Python<'_>, kind: &str, mut actions: PyReadwriteArray2<'_, i32>) -> PyResult<()> {
        let kind = match kind {
            "random" => Baseline::Random,
            "greedy" => Baseline::Greedy,
            "lookahead" => Baseline::Lookahead,
            _ => return Err(PyValueError::new_err("the baselines are \"random\", \"greedy\" and \"lookahead\"")),
        };
        let actions = whole(&mut actions, "actions")?;
        if actions.len() != 4 * self.env.len() {
            return Err(PyValueError::new_err(format!("actions must be {} × 4", self.env.len())));
        }
        let env = &mut self.env;
        py.detach(|| env.baseline(kind, actions));
        Ok(())
    }

    /// Totals since the environment was made.
    fn stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let s = self.env.stats();
        let d = PyDict::new(py);
        d.set_item("games", s.games)?;
        d.set_item("decisions", s.decisions)?;
        d.set_item("turns", s.turns)?;
        d.set_item("ties", s.ties)?;
        d.set_item("first_wins", s.first_wins)?;
        Ok(d)
    }
}

impl PyVecEnv {
    fn sized(&self, f: usize, i: usize, mask: usize) -> PyResult<()> {
        let n = 2 * self.env.len();
        if f != n * OBS_F || i != n * OBS_I || mask != n * OBS_M {
            return Err(PyValueError::new_err(format!(
                "observation arrays must be {n} × {OBS_F} float32, {n} × {OBS_I} int16 and {n} × {OBS_M} uint8"
            )));
        }
        Ok(())
    }
}

/// A team given as JSON (a list of sets as a team pool has them) in the engine's terms.
fn team_of(json: &str) -> PyResult<Vec<crate::PokemonSet>> {
    let sets: Vec<ShowdownSet> =
        serde_json::from_str(json).map_err(|e| PyValueError::new_err(format!("unreadable team: {e}")))?;
    sets.iter().map(|s| s.to_set()).collect::<Result<_, _>>().map_err(failed)
}

/// One player's side of a battle on Pokémon Showdown, followed from the
/// messages that player is sent. See `pokemon_ml.showdown`.
#[pyclass(name = "Follower")]
struct PyFollower {
    inner: Follower,
}

#[pymethods]
impl PyFollower {
    /// `side`: 0 for `p1`, 1 for `p2`. `team`: the six Pokémon registered, as
    /// JSON (one team of a pool), in the order they were sent to Showdown.
    #[new]
    fn new(side: usize, team: &str) -> PyResult<Self> {
        Ok(PyFollower { inner: Follower::new(side, team_of(team)?).map_err(PyValueError::new_err)? })
    }

    /// Takes in lines of the battle's log, as this player is sent them.
    fn lines(&mut self, text: &str) -> PyResult<()> {
        for line in text.lines() {
            self.inner.line(line).map_err(|e| PyRuntimeError::new_err(format!("{e} in {line:?}")))?;
        }
        Ok(())
    }

    /// Takes in a request (the JSON after `|request|`). Give the log up to it first.
    fn request(&mut self, json: &str) -> PyResult<()> {
        self.inner.request(json).map_err(PyRuntimeError::new_err)
    }

    /// Writes the observation (`f`: OBS_F float32, `i`: OBS_I int16, `mask`:
    /// OBS_M uint8) and returns how many joint actions are legal.
    fn observe(
        &self,
        mut f: PyReadwriteArray1<'_, f32>,
        mut i: PyReadwriteArray1<'_, i16>,
        mut mask: PyReadwriteArray1<'_, u8>,
    ) -> PyResult<usize> {
        let flat = |_: ()| PyValueError::new_err("the observation arrays must be contiguous");
        let f = f.as_slice_mut().map_err(|_| flat(()))?;
        let i = i.as_slice_mut().map_err(|_| flat(()))?;
        let mask = mask.as_slice_mut().map_err(|_| flat(()))?;
        if f.len() != OBS_F || i.len() != OBS_I || mask.len() != OBS_M {
            return Err(PyValueError::new_err(format!(
                "observation arrays must be {OBS_F} float32, {OBS_I} int16 and {OBS_M} uint8"
            )));
        }
        self.inner.observe(f, i, mask).map_err(PyRuntimeError::new_err)
    }

    /// What to send Showdown after `/choose ` for a pair of actions.
    fn choice(&mut self, first: usize, second: usize) -> PyResult<String> {
        self.inner.choice([first, second]).map_err(PyRuntimeError::new_err)
    }

    /// This player's side of the battle as a game state (JSON that `Game.from_state` reads), for
    /// a search from what the player knows (see `Follower::game_state` in the engine). Raises
    /// `ValueError` with the reason where it cannot be had (Illusion, Transform, closed sheets,
    /// a switch in the middle of a turn, before the first request or after the end).
    fn game_state(&self) -> PyResult<String> {
        self.inner.game_state().map(|s| s.to_json()).map_err(PyValueError::new_err)
    }

    /// "preview", "move" or "switch": what the last request asks; `None` if there is none.
    #[getter]
    fn phase(&self) -> Option<&'static str> {
        self.inner.phase().map(|p| match p {
            Phase::Preview => "preview",
            Phase::Move => "move",
            Phase::Switch => "switch",
        })
    }

    #[getter]
    fn side(&self) -> usize {
        self.inner.side()
    }

    #[getter]
    fn turn(&self) -> u16 {
        self.inner.turn()
    }

    #[getter]
    fn ended(&self) -> bool {
        self.inner.ended()
    }

    /// The side that won, once the battle has ended; `None` for a tie.
    #[getter]
    fn winner(&self) -> Option<usize> {
        self.inner.winner()
    }
}

fn flat<'a, T: numpy::Element>(a: &'a mut PyReadwriteArray1<'_, T>, n: usize, what: &str) -> PyResult<&'a mut [T]> {
    let out = a.as_slice_mut().map_err(|_| PyValueError::new_err(format!("{what} must be a contiguous array")))?;
    if out.len() != n {
        return Err(PyValueError::new_err(format!("{what} must hold {n} values")));
    }
    Ok(out)
}

fn side_index(side: usize) -> PyResult<usize> {
    if side < 2 { Ok(side) } else { Err(PyValueError::new_err("side is 0 or 1")) }
}

fn side_choice(s: &str) -> PyResult<[Choice; ACTIVE]> {
    Choice::parse_side(s)
        .ok_or_else(|| PyValueError::new_err(format!("not a side's choice: {s:?} (e.g. \"move 1 2, switch 3\")")))
}

fn joint(c: [Choice; ACTIVE]) -> String {
    format!("{}, {}", c[0].to_showdown(), c[1].to_showdown())
}

fn seed_words(seed: Vec<u16>) -> PyResult<[u16; 4]> {
    <[u16; 4]>::try_from(seed).map_err(|_| PyValueError::new_err("a seed is four 16-bit words"))
}

fn request_name(r: Request) -> &'static str {
    match r {
        Request::None => "none",
        Request::Move => "move",
        Request::Switch => "switch",
    }
}

/// Pokémon Showdown's random number generator (`Gen5RNG`), for driving the
/// baseline players and drawing battle seeds from Python the way the engine does.
#[pyclass(name = "Rng", skip_from_py_object)]
#[derive(Clone)]
struct PyRng {
    inner: Rng,
}

#[pymethods]
impl PyRng {
    #[new]
    #[pyo3(signature = (seed, calls=0))]
    fn new(seed: u64, calls: u32) -> Self {
        PyRng { inner: Rng { seed, calls } }
    }

    /// From Showdown's four 16-bit seed words.
    #[staticmethod]
    fn from_words(words: Vec<u16>) -> PyResult<Self> {
        Ok(PyRng { inner: Rng::from_words(seed_words(words)?) })
    }

    #[getter]
    fn seed(&self) -> u64 {
        self.inner.seed
    }

    #[getter]
    fn calls(&self) -> u32 {
        self.inner.calls
    }

    /// The state as four 16-bit words.
    fn words(&self) -> Vec<u16> {
        self.inner.words().to_vec()
    }

    /// A number below `n`, drawn as `PRNG#random(n)` draws it.
    fn below(&mut self, n: u32) -> u32 {
        self.inner.below(n)
    }
}

/// A battle in progress: the simulator alone, without Team Preview or the
/// bookkeeping an observation needs (see `Game` for those). Cheap to copy.
/// Choices are given and returned in Showdown's notation for one side,
/// e.g. `"move 1 2, switch 3"`.
#[pyclass(name = "Battle", skip_from_py_object)]
#[derive(Clone)]
struct PyBattle {
    inner: Battle,
}

#[pymethods]
impl PyBattle {
    /// A battle from a position (`BattleState` JSON, as `to_state` writes it
    /// or as written by hand). `prepared`: `None` takes the position as it
    /// says; `False` drops its bookkeeping (after editing an exported
    /// position); `True` trusts it.
    #[staticmethod]
    #[pyo3(signature = (json, prepared=None))]
    fn from_state(json: &str, prepared: Option<bool>) -> PyResult<Self> {
        let mut st = BattleState::from_json(json).map_err(failed)?;
        match prepared {
            Some(false) => st = st.without_bookkeeping(),
            Some(true) => st.prepared = true,
            None => {}
        }
        Ok(PyBattle { inner: Battle::from_state(&st).map_err(failed)? })
    }

    /// The position as JSON, bookkeeping included: `from_state` of it continues exactly as this battle would.
    fn to_state(&self) -> String {
        self.inner.to_state().to_json()
    }

    fn copy(&self) -> Self {
        self.clone()
    }

    #[getter]
    fn turn(&self) -> u16 {
        self.inner.turn
    }

    /// "move", "switch" or "none": what the battle waits for.
    #[getter]
    fn request(&self) -> &'static str {
        request_name(self.inner.request)
    }

    #[getter]
    fn ended(&self) -> bool {
        self.inner.ended
    }

    /// The side that won, once the battle has ended; `None` for a tie.
    #[getter]
    fn winner(&self) -> Option<u8> {
        self.inner.winner
    }

    /// The random number generator's state as four 16-bit words.
    #[getter]
    fn seed(&self) -> Vec<u16> {
        self.inner.rng.words().to_vec()
    }

    #[getter]
    fn open_sheets(&self) -> bool {
        self.inner.open_team_sheets()
    }

    /// Gives the battle a fresh random number generator from four seed words.
    fn reseed(&mut self, words: Vec<u16>) -> PyResult<()> {
        self.inner.rng = Rng::from_words(seed_words(words)?);
        Ok(())
    }

    /// What one position of `side` may do now, each as `"move 1 2"`, `"switch 3"` or `"pass"`.
    fn legal_choices(&self, side: usize, pos: usize) -> PyResult<Vec<String>> {
        if pos >= ACTIVE {
            return Err(PyValueError::new_err("pos is 0 or 1"));
        }
        Ok(self.inner.legal_choices(side_index(side)?, pos).into_iter().map(|c| c.to_showdown()).collect())
    }

    /// Every pair of choices `side` may give now, as `"move 1 2, switch 3"`.
    fn joint_choices(&self, side: usize) -> PyResult<Vec<String>> {
        Ok(self.inner.joint_choices(side_index(side)?).into_iter().map(joint).collect())
    }

    /// Both sides choose (two side strings), and the battle runs to the next
    /// request or its end.
    fn choose(&mut self, py: Python<'_>, choices: Vec<String>) -> PyResult<()> {
        if choices.len() != 2 {
            return Err(PyValueError::new_err("one choice string a side"));
        }
        let c = [side_choice(&choices[0])?, side_choice(&choices[1])?];
        let b = &mut self.inner;
        py.detach(|| b.choose(c)).map_err(failed)
    }

    /// What the battle has made public about `side`, as JSON (`ShownSide`).
    fn shown(&self, side: usize) -> PyResult<String> {
        let shown = self.inner.shown(side_index(side)?);
        serde_json::to_string(&shown).map_err(|e| PyRuntimeError::new_err(e.to_string()))
    }

    /// The first difference between this position and another, or `None` if they are the same.
    fn diff(&self, other: &PyBattle) -> Option<String> {
        self.inner.position_diff(&other.inner)
    }

    /// Each of `side`'s Pokémon as (HP, max HP), in their current order (the first two on the field).
    fn hp(&self, side: usize) -> PyResult<Vec<(u16, u16)>> {
        let s = &self.inner.sides[side_index(side)?];
        Ok((0..s.n as usize).map(|p| &s.team[s.order[p] as usize]).map(|m| (m.hp, m.max_hp())).collect())
    }
}

/// One game from Team Preview on: a `Battle` with the bookkeeping an
/// observation needs (how long conditions have been up, what each side can
/// tell of the other's Speed, how far hidden-length effects have run).
/// Actions are the numbers a model answers with (`pokemon_ml.env`).
#[pyclass(name = "Game", skip_from_py_object)]
#[derive(Clone)]
struct PyGame {
    inner: Game,
}

#[pymethods]
impl PyGame {
    /// Two teams of six (JSON, as a pool has them), at Team Preview. `open`: the players have each other's sheets.
    #[new]
    fn new(rosters: Vec<String>, open: bool) -> PyResult<Self> {
        if rosters.len() != 2 {
            return Err(PyValueError::new_err("two teams"));
        }
        Ok(PyGame { inner: Game::new([team_of(&rosters[0])?, team_of(&rosters[1])?], open).map_err(failed)? })
    }

    /// A game taken up from a `GameState` JSON, as `to_state` writes it (or
    /// written by hand, with the bookkeeping left out).
    #[staticmethod]
    fn from_state(json: &str) -> PyResult<Self> {
        let st = GameState::from_json(json).map_err(failed)?;
        Ok(PyGame { inner: Game::from_state(&st).map_err(failed)? })
    }

    /// The game as `GameState` JSON: teams as registered, the position, and the bookkeeping.
    fn to_state(&self) -> String {
        self.inner.to_state().to_json()
    }

    fn copy(&self) -> Self {
        self.clone()
    }

    /// "preview", "move" or "switch".
    #[getter]
    fn phase(&self) -> &'static str {
        match self.inner.phase() {
            Phase::Preview => "preview",
            Phase::Move => "move",
            Phase::Switch => "switch",
        }
    }

    #[getter]
    fn turn(&self) -> u16 {
        self.inner.battle().map(|b| b.turn).unwrap_or(0)
    }

    #[getter]
    fn ended(&self) -> bool {
        self.inner.ended()
    }

    #[getter]
    fn winner(&self) -> Option<usize> {
        self.inner.winner()
    }

    #[getter]
    fn open_sheets(&self) -> bool {
        self.inner.open_team_sheets()
    }

    /// A copy of the battle, once Team Preview is over.
    fn battle(&self) -> Option<PyBattle> {
        self.inner.battle().map(|b| PyBattle { inner: *b })
    }

    /// Every pair of actions `side` may give now.
    fn legal(&self, side: usize) -> PyResult<Vec<(usize, usize)>> {
        Ok(self.inner.legal(side_index(side)?).into_iter().map(|a| (a[0], a[1])).collect())
    }

    /// The choice an action number stands for (Showdown's notation), whether or not it is legal now.
    fn choice_of(&self, side: usize, action: usize) -> PyResult<Option<String>> {
        Ok(self.inner.choice_of(side_index(side)?, action).map(|c| c.to_showdown()))
    }

    /// The number a choice (`"move 1 2"`, `"switch 3"`, `"pass"`) goes by.
    fn action_of(&self, side: usize, choice: &str) -> PyResult<Option<usize>> {
        let c = Choice::parse(choice).ok_or_else(|| PyValueError::new_err(format!("not a choice: {choice:?}")))?;
        Ok(self.inner.action_of(side_index(side)?, c))
    }

    /// Both sides act (`[[a, b], [a, b]]`; at Team Preview only the first
    /// number of each side counts) and `seed` (four words) seeds the battle that starts.
    fn act(&mut self, py: Python<'_>, actions: Vec<Vec<usize>>, seed: Vec<u16>) -> PyResult<()> {
        if actions.len() != 2 || actions.iter().any(|a| a.len() != 2) {
            return Err(PyValueError::new_err("actions are two numbers a side"));
        }
        let a = [[actions[0][0], actions[0][1]], [actions[1][0], actions[1][1]]];
        let seed = seed_words(seed)?;
        let g = &mut self.inner;
        py.detach(|| g.act(a, seed)).map_err(failed)
    }

    /// Writes `view`'s observation (`f`: OBS_F float32, `i`: OBS_I int16,
    /// `mask`: OBS_M uint8) and returns how many joint actions are legal.
    fn observe(
        &self,
        view: usize,
        mut f: PyReadwriteArray1<'_, f32>,
        mut i: PyReadwriteArray1<'_, i16>,
        mut mask: PyReadwriteArray1<'_, u8>,
    ) -> PyResult<usize> {
        let view = side_index(view)?;
        let (f, i, mask) = (flat(&mut f, OBS_F, "f")?, flat(&mut i, OBS_I, "i")?, flat(&mut mask, OBS_M, "mask")?);
        self.inner.observe(view, f, i, mask);
        Ok(i[OBS_I - INFO + 1] as usize)
    }

    /// What a player that needs no model ("random", "greedy" or "lookahead") would do on `side`, drawing from `rng`.
    fn baseline(&self, side: usize, kind: &str, rng: &mut PyRng) -> PyResult<(usize, usize)> {
        let kind = match kind {
            "random" => Baseline::Random,
            "greedy" => Baseline::Greedy,
            "lookahead" => Baseline::Lookahead,
            _ => return Err(PyValueError::new_err("the baselines are \"random\", \"greedy\" and \"lookahead\"")),
        };
        let a = self.inner.baseline(side_index(side)?, kind, &mut rng.inner);
        Ok((a[0], a[1]))
    }

    /// Gives the battle fresh dice from four seed words, for playing the same
    /// position out another way; `False` at Team Preview (`act` seeds the battle there).
    fn reseed(&mut self, words: Vec<u16>) -> PyResult<bool> {
        Ok(self.inner.reseed(seed_words(words)?))
    }

    /// Whether the turn has been played to its end: at a move request, at a
    /// request for replacing the fainted at the end of the turn, or once the
    /// battle is over; `False` at a switch request in the middle of a turn.
    #[getter]
    fn turn_over(&self) -> bool {
        self.inner.turn_over()
    }

    /// The amounts of Speed stat points (0 to 32) the Pokémon `side`
    /// registered `entry`th can still have, by what its opponent has seen of
    /// the order of moves, with the nature it was registered with and whatever
    /// item it holds now (a Choice Scarf or an Iron Ball may have changed hands).
    fn speed_points(&self, side: usize, entry: usize) -> PyResult<Vec<u32>> {
        let side = side_index(side)?;
        if entry >= self.inner.rosters()[side].len() {
            return Err(PyValueError::new_err("entry is a place in the registered team"));
        }
        Ok(self.inner.speed_points(side, entry).into_iter().map(u32::from).collect())
    }

    /// The lowest and highest Speed that Pokémon can have, as its opponent can tell, its item counted.
    fn speed_range(&self, side: usize, entry: usize) -> PyResult<(u32, u32)> {
        let side = side_index(side)?;
        let set = self.inner.rosters()[side]
            .get(entry)
            .ok_or_else(|| PyValueError::new_err("entry is a place in the registered team"))?;
        Ok(self.inner.speeds().belief(side, entry).range(set.species))
    }

    /// The public counts of the hidden-length effects (`Counts`), as JSON.
    fn counts(&self) -> PyResult<String> {
        serde_json::to_string(self.inner.counts()).map_err(|e| PyRuntimeError::new_err(e.to_string()))
    }
}

/// A position (`BattleState` JSON) with the simulator's bookkeeping taken out,
/// so that `Battle.from_state` works it out again: for a position edited after export.
#[pyfunction]
fn without_bookkeeping(json: &str) -> PyResult<String> {
    Ok(BattleState::from_json(json).map_err(failed)?.without_bookkeeping().to_json())
}

/// A team (JSON, as a pool has it) in Showdown's packed format, for `/utm`.
#[pyfunction]
fn packed_team(team: &str) -> PyResult<String> {
    Ok(crate::format::packed_team(&team_of(team)?))
}

/// What is wrong with a team (JSON, as a pool has it) under the regulation: nothing, if it is legal.
#[pyfunction]
fn team_problems(team: &str) -> PyResult<Vec<String>> {
    Ok(Format::current().check_team(&team_of(team)?).iter().map(|v| v.to_string()).collect())
}

/// The sizes and the parts of an observation: for each part, where it begins and its shape.
#[pyfunction]
fn layout(py: Python<'_>) -> PyResult<Bound<'_, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("obs_f", OBS_F)?;
    d.set_item("obs_i", OBS_I)?;
    d.set_item("obs_m", OBS_M)?;
    d.set_item("actions", N_ACTIONS)?;
    d.set_item("preview_actions", N_PREVIEW)?;
    d.set_item("roster", obs::ROSTER)?;
    d.set_item("format", Format::current().id.as_str())?;
    d.set_item("obs_layout", obs::OBS_LAYOUT)?;
    // The counts in a position's features and the lengths they are taken over (`obs`, obs-2).
    let timed: Vec<(&str, u8)> = obs::TIMED_VOLATILES.iter().map(|k| (k.id(), k.data().duration)).collect();
    d.set_item("timed_volatiles", timed)?;
    let counted: Vec<(&str, u8)> = obs::COUNTED_VOLATILES.iter().map(|(k, n)| (k.id(), *n)).collect();
    d.set_item("counted_volatiles", counted)?;
    let slots: Vec<(&str, u8)> = obs::TIMED_SLOTS.iter().map(|(k, n)| (k.id(), *n)).collect();
    d.set_item("timed_slots", slots)?;
    d.set_item("sleep_max", obs::SLEEP_MAX)?;
    d.set_item("freeze_max", obs::FREEZE_MAX)?;
    let [species, items, abilities, moves] = obs::vocab();
    let vocab = PyDict::new(py);
    vocab.set_item("species", species)?;
    vocab.set_item("items", items)?;
    vocab.set_item("abilities", abilities)?;
    vocab.set_item("moves", moves)?;
    d.set_item("vocab", vocab)?;
    let (f, i) = obs::layout();
    for (key, parts) in [("f", f), ("i", i)] {
        let out = PyDict::new(py);
        for (name, at, shape) in parts {
            out.set_item(name, (at, shape))?;
        }
        d.set_item(key, out)?;
    }
    Ok(d)
}

fn array<'py>(py: Python<'py>, t: Table) -> PyResult<Bound<'py, PyAny>> {
    Ok(PyArray1::from_vec(py, t.data).reshape([t.rows, t.cols])?.into_any())
}

/// Static data for a model to embed ids with: a float32 table of features
/// for species, moves and items (row = id), and the four roster entries each
/// Team Preview action brings.
#[pyfunction]
fn tables(py: Python<'_>) -> PyResult<Bound<'_, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("species", array(py, obs::species_table())?)?;
    d.set_item("moves", array(py, obs::move_table())?)?;
    d.set_item("items", array(py, obs::item_table())?)?;
    let preview: Vec<i64> = preview_table().iter().flatten().map(|&j| j as i64).collect();
    d.set_item("preview", PyArray1::from_vec(py, preview).reshape([N_PREVIEW, 4])?)?;
    Ok(d)
}

/// What each id is called (row = id; the first two are "nothing" and "unknown").
#[pyfunction]
fn names(py: Python<'_>) -> PyResult<Bound<'_, PyDict>> {
    let list = |all: Vec<&'static str>| -> Vec<&'static str> { ["", "?"].into_iter().chain(all).collect() };
    let d = PyDict::new(py);
    d.set_item("species", list(SPECIES.iter().map(|s| s.name).collect()))?;
    d.set_item("items", list(ITEMS.iter().map(|s| s.name).collect()))?;
    d.set_item("abilities", list(ABILITIES.iter().map(|s| s.name).collect()))?;
    d.set_item("moves", list(MOVES.iter().map(|s| s.name).collect()))?;
    Ok(d)
}

// Search (`vgc_engine::search`): cells, the matrix solver and the double oracle.

fn pairs(v: Vec<(usize, usize)>) -> Vec<[usize; 2]> {
    v.into_iter().map(|(a, b)| [a, b]).collect()
}

/// Plans as given from Python: a pair per action (255 for none), or no list at all.
fn plans_of(v: Option<Vec<(u8, u8)>>) -> Vec<Plan> {
    v.unwrap_or_default().into_iter().map(|(a, b)| [a, b]).collect()
}

fn plans_out(v: &[Plan]) -> Vec<(u8, u8)> {
    v.iter().map(|p| (p[0], p[1])).collect()
}

fn cells_of(cells: &PyReadonlyArray2<'_, i32>) -> PyResult<Vec<CellId>> {
    let shape = cells.as_array().shape().to_vec();
    if shape.len() != 2 || shape[1] != 3 {
        return Err(PyValueError::new_err("cells are an n × 3 int32 array: row, column, seed"));
    }
    let flat = cells.as_slice().map_err(|_| PyValueError::new_err("cells must be a C-contiguous int32 array"))?;
    flat.chunks(3)
        .map(|c| {
            if c.iter().any(|&v| v < 0) {
                return Err(PyValueError::new_err(format!("a cell index is negative: {c:?}")));
            }
            Ok(CellId { row: c[0] as u32, col: c[1] as u32, seed: c[2] as u32 })
        })
        .collect()
}

fn follow_of(s: &str) -> PyResult<Follow> {
    match s {
        "end_of_turn" => Ok(Follow::EndOfTurn),
        "to_move" => Ok(Follow::ToMove),
        "none" => Ok(Follow::None),
        _ => Err(PyValueError::new_err("follow is \"end_of_turn\", \"to_move\" or \"none\"")),
    }
}

fn mode_of(s: &str) -> PyResult<Mode> {
    match s {
        "move" => Ok(Mode::Move),
        "replacement" => Ok(Mode::Replacement),
        "preview" => Ok(Mode::Preview),
        _ => Err(PyValueError::new_err("mode is \"move\", \"replacement\" or \"preview\"")),
    }
}

fn status_name(s: Status) -> &'static str {
    match s {
        Status::NeedCells => "need_cells",
        Status::Converged { complete: true } => "converged",
        Status::Converged { complete: false } => "converged_partial",
        Status::Capped(Cap::Steps) => "capped_steps",
        Status::Capped(Cap::Time) => "capped_time",
        Status::Capped(Cap::Solver) => "capped_solver",
    }
}

type LeavesOut<'py> = (Bound<'py, PyAny>, Bound<'py, PyAny>, Bound<'py, PyAny>, Vec<PyGame>);

/// The leaves as arrays: `f` (n × OBS_F float32, or 0 × OBS_F), `i` (n × OBS_I int16), `summary`
/// (n × SUMMARY float32) and the kept states as `Game`s (empty unless asked for).
fn leaves_out(py: Python<'_>, l: Leaves) -> PyResult<LeavesOut<'_>> {
    let (nf, ni) = (l.f.len() / OBS_F, l.i.len() / OBS_I);
    let f = PyArray1::from_vec(py, l.f).reshape([nf, OBS_F])?.into_any();
    let i = PyArray1::from_vec(py, l.i).reshape([ni, OBS_I])?.into_any();
    let s = PyArray1::from_vec(py, l.summary).reshape([l.n, SUMMARY])?.into_any();
    Ok((f, i, s, l.states.into_iter().map(|g| PyGame { inner: g }).collect()))
}

/// Plays every cell (an n × 3 int32 array of row, column and seed indices into `rows`
/// and `cols`, each side's joint actions) from `game` for `view`, and returns the
/// leaves in the same order: `(f, i, summary, states)` (see `Leaves` in Rust). `follow`:
/// what to do at a switch request before the leaf ("end_of_turn", "to_move", "none").
/// `row_plans` / `col_plans`: per action, what each position brings in if it is asked to
/// switch in the middle of the turn (the switch action 40–45, or 255 for the dice's
/// choice), or `None` for no plans. `mode`: what `game` is ("move", "replacement" for a request
/// for replacements at the end of a turn, "preview"; a cell of the last two is one step). Runs
/// on `threads` worker threads (0: one per core) with the interpreter lock let go.
#[pyfunction]
#[pyo3(signature = (
    game, view, rows, cols, cells, seed, observe=true, keep_states=false, follow="end_of_turn", threads=0,
    row_plans=None, col_plans=None, mode="move"
))]
#[allow(clippy::too_many_arguments)]
fn simulate<'py>(
    py: Python<'py>,
    game: &PyGame,
    view: usize,
    rows: Vec<(usize, usize)>,
    cols: Vec<(usize, usize)>,
    cells: PyReadonlyArray2<'py, i32>,
    seed: u64,
    observe: bool,
    keep_states: bool,
    follow: &str,
    threads: usize,
    row_plans: Option<Vec<(u8, u8)>>,
    col_plans: Option<Vec<(u8, u8)>>,
    mode: &str,
) -> PyResult<LeavesOut<'py>> {
    let cells = cells_of(&cells)?;
    let (rows, cols) = (pairs(rows), pairs(cols));
    let (rp, cp) = (plans_of(row_plans), plans_of(col_plans));
    let opts = SimOpts { observe, keep_states, follow: follow_of(follow)?, mode: mode_of(mode)? };
    let pool = search::pool(threads);
    let g = &game.inner;
    let leaves = py
        .detach(|| search::simulate_planned(g, view, &rows, &cols, &rp, &cp, &cells, seed, &opts, &pool))
        .map_err(failed)?;
    leaves_out(py, leaves)
}

/// The equilibrium of a zero-sum matrix (m × n float32, the value to the row player) by
/// regret matching+: `(x, y, value, gap, iters)`. The true value is within half the gap of `value`.
#[pyfunction]
#[pyo3(signature = (matrix, max_iter=20000, tol=1e-4))]
fn solve<'py>(
    py: Python<'py>,
    matrix: PyReadonlyArray2<'py, f32>,
    max_iter: usize,
    tol: f64,
) -> PyResult<(Bound<'py, PyArray1<f64>>, Bound<'py, PyArray1<f64>>, f64, f64, usize)> {
    let shape = matrix.as_array().shape().to_vec();
    let (m, n) = (shape[0], shape[1]);
    if m == 0 || n == 0 {
        return Err(PyValueError::new_err("an empty matrix has no equilibrium"));
    }
    let a = matrix.as_slice().map_err(|_| PyValueError::new_err("the matrix must be a C-contiguous float32 array"))?;
    let s = py.detach(|| search::solve(a, m, n, SolveOpts { max_iter, tol }));
    Ok((PyArray1::from_vec(py, s.x), PyArray1::from_vec(py, s.y), s.value, s.gap, s.iters))
}

/// The hand score of each leaf summary (n × SUMMARY float32) for the viewer, in [-1, 1]: HP
/// left less the other side's, `faint_bonus` per Pokémon standing, `terminal_bonus` for a win.
#[pyfunction]
#[pyo3(signature = (summary, faint_bonus=1.0, terminal_bonus=10.0))]
fn hand_value<'py>(
    py: Python<'py>,
    summary: PyReadonlyArray2<'py, f32>,
    faint_bonus: f32,
    terminal_bonus: f32,
) -> PyResult<Bound<'py, PyArray1<f32>>> {
    if summary.as_array().shape()[1] != SUMMARY {
        return Err(PyValueError::new_err(format!("a summary has {SUMMARY} numbers")));
    }
    let s = summary.as_slice().map_err(|_| PyValueError::new_err("the summary must be a C-contiguous float32 array"))?;
    let values = s.chunks(SUMMARY).map(|row| search::hand_value(row, faint_bonus, terminal_bonus)).collect();
    Ok(PyArray1::from_vec(py, values))
}

/// A side's joint actions with the likeliest first, without a model: the engine's greedy
/// pick, then its look-ahead pick, then the rest in the engine's order.
#[pyfunction]
fn heuristic_order(game: &PyGame, side: usize, seed: u64) -> PyResult<Vec<(usize, usize)>> {
    let mut rng = Rng { seed, calls: 0 };
    Ok(search::heuristic_order(&game.inner, side_index(side)?, &mut rng).into_iter().map(|a| (a[0], a[1])).collect())
}

/// The double oracle from a position (see `vgc_engine::search::oracle`). Drive it:
/// `advance()` says "need_cells" until the cells `wanted()` names have been played
/// (`simulate`) and valued (`supply`); then it says how it ended.
#[pyclass(name = "Search", skip_from_py_object)]
struct PySearch {
    inner: search::Search,
}

#[pymethods]
impl PySearch {
    /// `rows`: the viewer's joint actions, likeliest first; `cols`: the other side's.
    /// `candidates`: `None` for every action at each oracle step, or the next so many.
    /// `row_plans` / `col_plans`: a plan per action (see `simulate`), or `None`; with plans
    /// the same action may be listed with different plans. `mode`: "move" (the default),
    /// "replacement" or "preview" (see `simulate`; one set of dice: `seeds=1`). The root must
    /// be what the mode says, and values handed in must lie in [-1, 1].
    #[new]
    #[pyo3(signature = (
        game, view, rows, cols, seed, candidates=None, seeds=16, initial=1, max_steps=None, time_cap=None, eps=1e-3,
        max_iter=20000, tol=1e-4, row_plans=None, col_plans=None, mode="move"
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        game: &PyGame,
        view: usize,
        rows: Vec<(usize, usize)>,
        cols: Vec<(usize, usize)>,
        seed: u64,
        candidates: Option<usize>,
        seeds: usize,
        initial: usize,
        max_steps: Option<usize>,
        time_cap: Option<f64>,
        eps: f64,
        max_iter: usize,
        tol: f64,
        row_plans: Option<Vec<(u8, u8)>>,
        col_plans: Option<Vec<(u8, u8)>>,
        mode: &str,
    ) -> PyResult<Self> {
        let budget = Budget {
            candidates: match candidates {
                None => Candidates::All,
                Some(m) => Candidates::Next(m),
            },
            seeds,
            initial,
            max_steps: max_steps.unwrap_or(usize::MAX),
            time_cap: time_cap.map(std::time::Duration::from_secs_f64),
            eps,
            solver: SolveOpts { max_iter, tol },
            mode: mode_of(mode)?,
        };
        let (view, rows, cols) = (side_index(view)?, pairs(rows), pairs(cols));
        let (rp, cp) = (plans_of(row_plans), plans_of(col_plans));
        let inner =
            search::Search::with_plans(game.inner.clone(), view, rows, cols, rp, cp, budget, seed).map_err(failed)?;
        Ok(PySearch { inner })
    }

    /// The cells the next step needs, as an n × 3 int32 array (row, column, seed).
    fn wanted<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let w = self.inner.wanted();
        let flat: Vec<i32> = w.iter().flat_map(|c| [c.row as i32, c.col as i32, c.seed as i32]).collect();
        Ok(PyArray1::from_vec(py, flat).reshape([w.len(), 3])?.into_any())
    }

    /// Plays `cells` from the root (the module's `simulate` with this search's lists, seed and mode).
    #[pyo3(signature = (cells, observe=true, keep_states=false, follow="end_of_turn", threads=0))]
    fn simulate<'py>(
        &self,
        py: Python<'py>,
        cells: PyReadonlyArray2<'py, i32>,
        observe: bool,
        keep_states: bool,
        follow: &str,
        threads: usize,
    ) -> PyResult<LeavesOut<'py>> {
        let cells = cells_of(&cells)?;
        let opts = SimOpts { observe, keep_states, follow: follow_of(follow)?, mode: self.inner.budget().mode };
        let pool = search::pool(threads);
        let s = &self.inner;
        let leaves = py.detach(|| s.simulate(&cells, &opts, &pool)).map_err(failed)?;
        leaves_out(py, leaves)
    }

    /// Takes in one value per cell (float32, in [-1, 1] by convention), replacing any it had.
    fn supply(&mut self, cells: PyReadonlyArray2<'_, i32>, values: PyReadonlyArray1<'_, f32>) -> PyResult<()> {
        let cells = cells_of(&cells)?;
        let values = values.as_slice().map_err(|_| PyValueError::new_err("values must be a contiguous float32 array"))?;
        self.inner.supply(&cells, values).map_err(failed)
    }

    /// Moves the search on as far as it can: "need_cells", "converged", "capped_steps",
    /// "capped_time" or "capped_solver" (a solve still above `tol` after `max_iter` rounds: no
    /// candidate is compared against it). ("converged_partial" is no longer given: a window
    /// slides on until every action has been looked at.)
    fn advance(&mut self, py: Python<'_>) -> &'static str {
        let s = &mut self.inner;
        status_name(py.detach(|| s.advance()))
    }

    /// Asks for `extra` more sets of dice for a cell; the search solves again before it goes on.
    fn deepen(&mut self, row: usize, col: usize, extra: usize) -> PyResult<()> {
        self.inner.deepen(row, col, extra).map_err(failed)
    }

    #[getter]
    fn rows(&self) -> Vec<(usize, usize)> {
        self.inner.rows().iter().map(|a| (a[0], a[1])).collect()
    }

    #[getter]
    fn cols(&self) -> Vec<(usize, usize)> {
        self.inner.cols().iter().map(|a| (a[0], a[1])).collect()
    }

    /// The plan of each row and column (empty lists when none were given).
    #[getter]
    fn row_plans(&self) -> Vec<(u8, u8)> {
        plans_out(self.inner.row_plans())
    }

    #[getter]
    fn col_plans(&self) -> Vec<(u8, u8)> {
        plans_out(self.inner.col_plans())
    }

    #[getter]
    fn view(&self) -> usize {
        self.inner.view()
    }

    #[getter]
    fn done(&self) -> bool {
        self.inner.is_done()
    }

    /// What the search found, as a dict: `rows_in`, `cols_in` (indices into the lists),
    /// `x`, `y` (the mixes over them), `value`, `gap`, `cells` (arrays `row`, `col`, `n`,
    /// `mean`, `std` over every cell with a value), `supp_mass`, `steps`, `leaves`,
    /// `elapsed` (seconds), `status` (`None` while going), `scanned`, `lists` (the length of
    /// each side's list), `iters` (rounds the last solve took) and `mode`.
    fn report<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let r = self.inner.report();
        let d = PyDict::new(py);
        d.set_item("rows_in", r.rows_in)?;
        d.set_item("cols_in", r.cols_in)?;
        d.set_item("x", PyArray1::from_vec(py, r.x))?;
        d.set_item("y", PyArray1::from_vec(py, r.y))?;
        d.set_item("value", r.value)?;
        d.set_item("gap", r.gap)?;
        let cells = PyDict::new(py);
        cells.set_item("row", PyArray1::from_vec(py, r.cells.iter().map(|c| c.row as i32).collect()))?;
        cells.set_item("col", PyArray1::from_vec(py, r.cells.iter().map(|c| c.col as i32).collect()))?;
        cells.set_item("n", PyArray1::from_vec(py, r.cells.iter().map(|c| c.n as i32).collect()))?;
        cells.set_item("mean", PyArray1::from_vec(py, r.cells.iter().map(|c| c.mean).collect()))?;
        cells.set_item("std", PyArray1::from_vec(py, r.cells.iter().map(|c| c.std).collect()))?;
        d.set_item("cells", cells)?;
        d.set_item("supp_mass", (r.supp_mass[0], r.supp_mass[1]))?;
        d.set_item("steps", r.steps)?;
        d.set_item("leaves", r.leaves)?;
        d.set_item("elapsed", r.elapsed.as_secs_f64())?;
        d.set_item("status", r.status.map(status_name))?;
        d.set_item("scanned", (r.scanned[0], r.scanned[1]))?;
        d.set_item("lists", (r.lists[0], r.lists[1]))?;
        d.set_item("iters", r.iters)?;
        d.set_item("mode", r.mode.name())?;
        Ok(d)
    }
}

/// Plays a recorded battle (one line of a `--record` file, as JSON) in a game beside a follower
/// for each side, as `followcheck` does, and returns what each side had at every decision: a
/// dict with `step`, `side`, `phase` (the follower's: "preview", "move" or "switch"), `truth`
/// (the game's state, JSON), `export` (the follower's `game_state`, or `None`), `refused` (why
/// not, or `None`), `hidden_disable` (the side is barred by something it was not shown, where
/// followcheck compares nothing) and `unsaid` (what this side has not been told and its export may
/// therefore have otherwise, D89: a list of dicts with `kind`, `side`, `species` and, for PP, `move`).
/// Raises if the battle cannot be followed, or if its recorded state is not the game's; a battle
/// with a team that has a species twice raises `ValueError("unsupported: a team with a species
/// twice")` before anything is followed, as `check_export` passes it over.
#[pyfunction]
fn follow_case_states<'py>(py: Python<'py>, case: &str) -> PyResult<Vec<Bound<'py, PyDict>>> {
    let c: crate::replay::FollowCase =
        serde_json::from_str(case).map_err(|e| PyValueError::new_err(format!("unreadable recording: {e}")))?;
    if crate::replay::species_twice(&c) {
        return Err(PyValueError::new_err(format!("unsupported: {}", crate::replay::SPECIES_TWICE)));
    }
    type Row = (usize, usize, Option<&'static str>, String, Result<String, String>, bool, Vec<crate::replay::Unsaid>);
    let mut rows: Vec<Row> = Vec::new();
    let mut unsaid = crate::replay::UnsaidTracker::default();
    let outcome = crate::replay::walk_follow(&c, &mut |game, followers, step| {
        match step {
            0 => unsaid.start(game),
            k => unsaid.step(game, &c.steps[k - 1].after.log, &c.steps[k - 1].choices),
        }
        for (side, follower) in followers.iter().enumerate() {
            let phase = follower.phase().map(|p| match p {
                Phase::Preview => "preview",
                Phase::Move => "move",
                Phase::Switch => "switch",
            });
            let export = follower.game_state().map(|s| s.to_json());
            let hidden = crate::replay::hidden_disable(game, side);
            rows.push((step, side, phase, game.to_state().to_json(), export, hidden, unsaid.for_viewer(side)));
        }
        Ok(())
    });
    match outcome {
        crate::replay::Outcome::Pass(_) => {}
        crate::replay::Outcome::Unsupported(what) => return Err(PyValueError::new_err(format!("unsupported: {what}"))),
        crate::replay::Outcome::Fail(why) => return Err(PyRuntimeError::new_err(why.join("; "))),
    }
    rows.into_iter()
        .map(|(step, side, phase, truth, export, hidden, unsaid)| {
            let d = PyDict::new(py);
            d.set_item("step", step)?;
            d.set_item("side", side)?;
            d.set_item("phase", phase)?;
            d.set_item("truth", truth)?;
            match export {
                Ok(json) => {
                    d.set_item("export", json)?;
                    d.set_item("refused", py.None())?;
                }
                Err(why) => {
                    d.set_item("export", py.None())?;
                    d.set_item("refused", why)?;
                }
            }
            d.set_item("hidden_disable", hidden)?;
            let told = PyList::empty(py);
            for u in unsaid {
                let e = PyDict::new(py);
                e.set_item("kind", u.kind())?;
                match u {
                    crate::replay::Unsaid::RampageEnded { side, species }
                    | crate::replay::Unsaid::EncoreTarget { side, species, .. } => {
                        e.set_item("side", side)?;
                        e.set_item("species", species)?;
                    }
                    crate::replay::Unsaid::PressurePp { side, species, mv } => {
                        e.set_item("side", side)?;
                        e.set_item("species", species)?;
                        e.set_item("move", mv)?;
                    }
                }
                told.append(e)?;
            }
            d.set_item("unsaid", told)?;
            Ok(d)
        })
        .collect()
}

#[pymodule]
fn _engine(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(follow_case_states, m)?)?;
    m.add_class::<PySearch>()?;
    m.add_function(wrap_pyfunction!(simulate, m)?)?;
    m.add_function(wrap_pyfunction!(solve, m)?)?;
    m.add_function(wrap_pyfunction!(hand_value, m)?)?;
    m.add_function(wrap_pyfunction!(heuristic_order, m)?)?;
    m.add_class::<PyVecEnv>()?;
    m.add_class::<PyFollower>()?;
    m.add_class::<PyBattle>()?;
    m.add_class::<PyGame>()?;
    m.add_class::<PyRng>()?;
    m.add_function(wrap_pyfunction!(without_bookkeeping, m)?)?;
    m.add_function(wrap_pyfunction!(packed_team, m)?)?;
    m.add_function(wrap_pyfunction!(team_problems, m)?)?;
    m.add_function(wrap_pyfunction!(layout, m)?)?;
    m.add_function(wrap_pyfunction!(tables, m)?)?;
    m.add_function(wrap_pyfunction!(names, m)?)?;
    Ok(())
}
