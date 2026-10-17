"""The Rust training environment, from Python.

`Env` holds many games and the arrays their observations are written into.
Observation `2 * game + side` is that side's view of that game:

    f     float32 [2·envs, OBS_F]   features (field, Pokémon, positions, moves)
    i     int16   [2·envs, OBS_I]   ids to embed, and four numbers about the decision
    mask  uint8   [2·envs, OBS_M]   legal actions

`LAYOUT` says where each named part lies (`LAYOUT["f"]["mon_feats"]` is an
offset and a shape) and `TABLES` holds static data for the model: a row of
features for every species, move and item id. The Rust side documents what
every number means (`src/obs.rs`, `src/env.rs`).
"""
import numpy as np

from . import _engine

LAYOUT = _engine.layout()
TABLES = _engine.tables()
NAMES = _engine.names()

OBS_F, OBS_I, OBS_M = LAYOUT["obs_f"], LAYOUT["obs_i"], LAYOUT["obs_m"]
N_ACTIONS, N_PREVIEW = LAYOUT["actions"], LAYOUT["preview_actions"]
# What the observation holds and where: changes whenever a feature moves or changes meaning.
OBS_LAYOUT = LAYOUT["obs_layout"]
PASS = N_ACTIONS - 1
# info: phase, legal joint actions, turn, whether there is a choice to make
PREVIEW, MOVE, SWITCH = 0, 1, 2


def part(array, kind, name):
    """A named part of a batch of observations, shaped [batch, *shape]. Works on NumPy arrays and tensors."""
    at, shape = LAYOUT[kind][name]
    size = int(np.prod(shape))
    return array[..., at:at + size].reshape(*array.shape[:-1], *shape)


class Env:
    """`envs` games at once between teams of a pool (the JSON `teampool` writes)."""

    def __init__(self, pool, envs=256, seed=1, open_sheets=0.5, vary=True, max_turns=100, threads=0):
        with open(pool, encoding="utf-8") as file:
            self.raw = _engine.VecEnv(file.read(), envs, seed, open_sheets, vary, max_turns, threads)
        self.envs = envs
        self.f = np.zeros((2 * envs, OBS_F), np.float32)
        self.i = np.zeros((2 * envs, OBS_I), np.int16)
        self.mask = np.zeros((2 * envs, OBS_M), np.uint8)
        self.reward = np.zeros((envs, 2), np.float32)
        self.done = np.zeros(envs, np.uint8)
        self._actions = np.zeros((envs, 4), np.int32)
        self.raw.observe(self.f, self.i, self.mask)

    def step(self, actions):
        """Every side acts (`actions`: [2·envs, 2] integers). The observation arrays, `reward`
        ([envs, 2]: 1 to a winner, -1 to a loser) and `done` ([envs]) are overwritten; a game
        that ended has been replaced by a new one, at Team Preview."""
        self._actions[:] = np.asarray(actions).reshape(self.envs, 4)
        self.raw.step(self._actions, self.f, self.i, self.mask, self.reward, self.done)

    def baseline(self, kind):
        """What the "random", the "greedy" or the "lookahead" player would do on every side: [2·envs, 2]."""
        out = np.zeros((self.envs, 4), np.int32)
        self.raw.baseline(kind, out)
        return out.reshape(2 * self.envs, 2)

    def stats(self):
        return self.raw.stats()
