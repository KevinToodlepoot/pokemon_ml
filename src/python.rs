//! The training environment as a Python module (`pokemon_ml._engine`),
//! built with `maturin develop --release`. Everything is handed over as
//! NumPy arrays that Python owns and Rust fills in place, with the
//! interpreter lock let go while the games are stepped.

use numpy::{PyArray1, PyArrayMethods, PyReadonlyArray2, PyReadwriteArray1, PyReadwriteArray2};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;

use crate::data::{ABILITIES, ITEMS, MOVES, SPECIES};
use crate::env::{Baseline, Config, Game, GameState, N_ACTIONS, N_PREVIEW, OBS_M, VecEnv, preview_table};
use crate::follow::Follower;
use crate::format::{Format, ShowdownSet};
use crate::obs::{self, INFO, OBS_F, OBS_I, Phase, Table};
use crate::position::BattleState;
use crate::rng::Rng;
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
    #[new]
    #[pyo3(signature = (
        pool, envs=256, seed=1, open_sheets=0.5, vary=true, max_turns=100, threads=0, check_teams=true
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
    ) -> PyResult<Self> {
        let pool = Pool::from_json(pool).map_err(failed)?;
        let variation = if vary { Variation::default() } else { Variation::NONE };
        let config = Config { envs, seed, open_sheets, variation, max_turns, threads, check_teams };
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

    /// The amounts of Speed stat points (0 to 32) the Pokémon `side`
    /// registered `entry`th can still have, by what its opponent has seen of
    /// the order of moves, with the nature and item it was registered with.
    fn speed_points(&self, side: usize, entry: usize) -> PyResult<Vec<u32>> {
        let side = side_index(side)?;
        let set = self.inner.rosters()[side]
            .get(entry)
            .ok_or_else(|| PyValueError::new_err("entry is a place in the registered team"))?;
        let belief = self.inner.speeds().belief(side, entry);
        Ok((0..=32u32).filter(|&p| belief.has(set.nature, p as u8, set.item)).collect())
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

#[pymodule]
fn _engine(m: &Bound<'_, PyModule>) -> PyResult<()> {
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
