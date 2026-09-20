# Dynamic server meshing (ds_game_server)

Horizon splits the simulation of the universe between several Godot servers by
**zones**. This document is the contract between `ds_genericprops`,
`ds_game_server` and the Godot server (`DyingStar/server/server.gd`).

## Vocabulary

| Term | Meaning |
| --- | --- |
| **world** | Where an object lives: `space` (the root world) or one **planet** (its own `World3D` on the Godot server, planet at the origin). Derived from the object's `parent_id` chain: the nearest ancestor of type `planet`, else space. A moon is typed `planet`, so an object on a moon is in the moon's world. |
| **bounds** | An AABB `{min_x, max_x, min_y, max_y, min_z, max_z}`, in the world's coordinates (planet-local or universe). |
| **zone** | `{id, world, planet_uuid?, planet_name?, bounds}` — what a Godot server owns. `bounds: null` = the whole world. A server owns a list of zones. |

Membership of an object: `zone.world == object.world` **and** (`bounds == null`
or `local_position ∈ bounds`). Planets and stars are worlds, not content: every
server receives them, they are never frozen nor counted as managed.

Shared Rust types: `ds_common::world::{ObjectWorld, Point3}` and
`ds_common::zone::{Zone, Bounds, zones_contain}`.

## `_world`: the object world injected by ds_genericprops

`ds_genericprops` owns `parent_id`, so it resolves the chain
(`handlers/world.rs::resolve_world`) and injects it into `object_data` of every
payload `ds_game_server` consumes, next to `_global_position`:

```json
"_world": {
  "chain": [{"uuid": "<vehicle>", "object_type": "vehicle"}, {"uuid": "<planet>", "object_type": "planet"}],
  "world": "planet",
  "planet_uuid": "<planet>",
  "planet_name": "SandBox",
  "local_position": {"x": 4450602.9, "y": 2674215.6, "z": -3675740.7}
}
```

`chain` lists the parents from the direct one upward and stops at the first
planet. `local_position` is the sum of the local positions up to the world origin
(the planet centre) — for space it is the absolute position.

Injected in: `plugingameserver:new_player`, `gameserverplugin:spawn_object`,
`gameserverplugin:update_prop`, `gameserverplugin:player_out_of_zone` (`item`),
and every item of `gameserver:objects_snapshot`. Both `_world` and
`_global_position` are Horizon-internal and stripped before the Godot wire
(`ObjectWorld::strip_internal_keys`).

## Snapshot request / response

```
genericprops:get_objects_snapshot   {"request_id": "<uuid>"}
gameserver:objects_snapshot         {"request_id": "<uuid>", "items": {"<uuid>": GenericPropsRequest, ...}}
```

Every object with its flattened properties, `_global_position` and `_world`. The
ServerManager awaits the answer through a `oneshot` keyed by `request_id`
(`snapshot_timeout_secs`).

## Horizon → Godot `server/zone`

```json
{"namespace": "server", "event": "zone", "server_uuid": "…", "server_name": "word-12345",
 "data": {"zones": [
   {"id": "…", "world": "space", "bounds": null},
   {"id": "…", "world": "planet", "planet_uuid": "…", "planet_name": "SandBox",
    "bounds": {"min_x": -1e6, "max_x": 1234.5, "min_y": -9e11, "max_y": 9e11, "min_z": -9e11, "max_z": 9e11}}
 ]}}
```

`zones: []` releases the server (it goes back to the idle pool): Godot then
frees every prop and player (planets stay) — `_unload_world_objects`. Otherwise
Godot (`manage_zone`) stores the list, re-evaluates zone-frozen props
(`_apply_zones_to_existing_objects`, only props it froze itself, marked
`_zone_frozen`), pushes chunk residency for bounded planet zones (whole-planet
zones rely on the pins under players/bodies) and
uses `_in_server_zones(node, 0.4)` in `_check_out_of_zone`: the node's world is
its nearest `Planet` ancestor, its position is `global_position` (planet-local
inside a planet world).

## Horizon → Godot `server/prewarm`

```json
{"namespace": "server", "event": "prewarm",
 "data": {"planet_uuid": "…", "positions": [{"x": …, "y": …, "z": …}], "ttl_ms": 15000}}
```

Players are about to land at these planet-local positions: the server pins the
chunks there for `ttl_ms` (`_prewarm_pins`, applied by the pin sweep) so a
transferred player is not held waiting for a collision build. Sent by the
ServerManager before the players of a split/merge are moved (then it waits
`split_warmup_secs`), and by any server that sees one of its players within
`PREWARM_DISTANCE` (300 m) of a bound: it emits `gameserverplugin:prewarm
{server_uuid, player_uuid, planet_uuid, position}` and every other server whose
zone on that planet contains the point (bounds grown by 300 m) forwards it.

## Godot → Horizon `serverinfo`

```json
{"namespace": "props", "event": "position", "amessagenb": 0,
 "data": [{"uuid": "<server_uuid>", "type": "serverinfo", "tps": 58,
           "objects_number": 12345, "players_number": 3, "scenes_number": 40}]}
```

`tps` = achieved physics ticks per second over the last second (not the main-loop
FPS); `chunks_loading` = terrain collision chunks still building/queued. Both
drive the hand-over: players are moved onto a server only once it reports
`tps >= split_ready_tps` with `chunks_loading == 0` (after `split_warmup_secs`). Horizon still accepts `fps` with a warning during the transition.

Horizon forwards it to the players of that server as an `update_property` of
`object_type: "serverinfo"` with `data.godotserver.{tps, zones, ...}` and
`data.universe.{players_number, godotservers_number}` (schema
`horizon-to-client/serverinfo.update_property.schema.json`).

## Configuration (`plugins.toml`, `[ds_game_server]`)

```toml
servers_mode = "development"   # single: one server, no split/merge
game_servers = ["ws://127.0.0.1:8980", "ws://127.0.0.1:8981"]   # static pool
game_servers_dns = "godotserver:8980"  # + every address this name resolves to
split_rule = "tps:20"          # or "players:50"
merge_rule = "players:10"      # or "tps:50"
split_after_samples = 10       # consecutive 1 s samples
merge_after_samples = 30
snapshot_timeout_secs = 5
split_warmup_secs = 5          # minimum wait after props + prewarm were sent
split_warmup_max_secs = 30     # give up waiting for readiness after this
split_ready_tps = 58           # ready = serverinfo tps >= this and chunks_loading == 0
```

## Lifecycle

1. The pool is connected; each `Server` registers its handlers once. Planets
   already in Horizon are learned from a snapshot, later ones from `props:planet`.
   `game_servers_dns` (overridden by the `GAME_SERVER_HOST` env var) is resolved
   again every minute: a new address joins the pool and is connected, an address
   that left DNS is retired once its socket is gone. On kubernetes the headless
   `godotserver` service answers with one IP per ready pod, so a godotserver
   rollout while Horizon runs is picked up without a restart.
2. The first online server starts on `[space] + one zone per known planet` and
   receives the objects that already exist. A planet discovered later is appended
   to the server owning unbounded space.
3. **Split** (`split_rule` true for `split_after_samples`): an idle server is
   picked; a snapshot is taken; `mesh::plan_split`:
   - several zones → whole zones move (heaviest first, greedy balance by players,
     the parent keeps at least one);
   - one zone → its bounds are cut through the players' median on the widest
     axis (`mesh::split_bounds`), the child gets a new zone id.
   The parent gets `update_zones(keep)`, the child `start(give)`, the child
   receives every object (`initial_object`, frozen when outside its zones), the
   parent freezes what it no longer owns. A `SplitRecord` remembers the pair.
4. **Merge** (`merge_rule` true for `merge_after_samples` on a pair that no later
   split touched): the parent takes back its zones as they were before the split,
   receives the objects of the released zones it does not already manage, the
   child freezes everything and is released.
5. **Player transfer at runtime**: Godot reports `out_of_zone: <server_uuid>` in
   `players/position`; genericprops emits `player_out_of_zone` with `_world`; the
   source server freezes the player, the server whose zone contains the world
   spawns it (position clamped inside the bounds when the direct parent is the
   world itself). Whatever is parented under the player (the crate in their
   hands) is listed in `children` of `player_out_of_zone`: the source Godot
   server releases it with the player (`_release_carried_for_transfer`, no
   delete reported), the destination gets it right after the player and puts it
   back in their hands (`Player.server_adopt_carried`).
6. **Prop transfer at runtime** (a vehicle and everything riding in it): the
   Godot server zone-checks every moving prop parented directly to a world
   (planet/space) at each `props/update_object` flush; one that left its zones
   is flagged `out_of_zone: <server_uuid>` in its entry and zone-frozen on the
   spot. genericprops turns that into `gameserverplugin:object_out_of_zone
   {server_uuid, item, children}` where `children` are every descendant
   (seated players, cargo...) parents first, each with `_world`. The source
   server freezes the subtree; the server whose zone contains the prop's world
   sends `add_prop` for the prop — the Godot server, already holding a
   zone-frozen copy, *adopts* it (pose re-applied, seats/pilot/doors restored,
   unfrozen) — then the descendants; a player parented under a `Vehicle` is
   re-seated (`seat_name_of` + `server_enter(force)`). Seated players are never
   zone-checked on their own: they cross with their carrier.
7. **Server offline**: its zones are re-homed on an idle server (with a
   snapshot), its split records are dropped, and it is reconnected in the
   background; when nobody is running any more the returning server takes the
   world back.

### Transitions run off the manager loop

A split, merge, adoption (first start, re-homing) or zone grant is a `MeshOp`
run by the `MeshWorker` on its own task, one at a time; the manager loop keeps
consuming `serverinfo` meanwhile and applies the `OpResult` when it lands
(split record, released server, planets seen in the snapshot). Consequences:

- **Silence check**: a running server that sent no `serverinfo` for 60 s is
  declared dead and re-homed. The servers of the op in flight are exempt (they
  are busy instantiating what it sent) and are "touched" when it ends. Before
  the ops were moved off the loop, a 70 s split stalled the loop itself and
  every *other* server came out of it "silent" — a false death that re-homed
  the players onto a third cold server.
- **Settle window**: for 15 s after an op, the samples of the servers involved
  do not count for split/merge rules (Godot grants 5 s of grace after
  `update_zones`, then erases the players it lost a few per second, so the
  parent still reports its old `players_number` for a while). Split/merge hit
  counters are reset, and nothing is evaluated while an op is in flight.
- A split under a `players:N` rule whose plan gives away zero players is
  skipped (the count still describes players already handed over).
- Re-homing and grants queue up behind the op in flight; a split or merge is
  only decided when nothing is in flight (its rule re-fires if still true).

Logs are prefixed `[mesh]`.
