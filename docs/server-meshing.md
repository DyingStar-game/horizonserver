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

## Horizon → genericprops `servers_load`

Every 5 s (with the `[mesh] state` log line) ds_game_server publishes the load of
the running servers (`ds_common::mesh_load::PoolLoad`):

```json
{"capacity": 75, "servers": [{"uuid": "…", "name": "vitae-33386", "players": 52,
  "accepting": true, "tps": 60, "zones": [{"id": "…", "world": "planet", "planet_uuid": "…", "planet_name": "SandBox", "bounds": {…}}]}]}
```

`players` is the larger of Godot's and Horizon's counts; `accepting` = reported
within 10 s, not loading zones it was just handed, in no split / merge (queued or
running: a child warming up reports long before its players are moved), under
`placement_capacity` and at `placement_min_tps` or more; `capacity` is
`placement_capacity`, by default N of a `players:N` split rule. Besides every 5 s, the
load is published at the end of each op and on the first serverinfo of a server done
loading its new zones, so the queued players go at once.

genericprops places each new player in the villages of the **least loaded server
that accepts** (then the fullest village of that server, so villages stay compact),
counting the players it placed since the last update. Its reserve of free places is
kept in the zones of accepting servers only (when one of them owns a village at all:
an accepting server holding space and the other planets cannot take arrivals), and
new villages are requested there first, never more than 2 waiting to be spawned per
server (Godot freezes 16-20 s per village; 40+ requests in a row kept one frozen).
The reserve is `village_free_reserve` plus `village_reserve_per_server` villages for
each accepting server that owns villages, so habs are asked for before the open
villages fill up on every server at once, and never less than `prespawn_villages` (15)
villages: the reserve is also kept with nobody arriving (on every `servers_load` and every
hab registered), so the villages are spawned before the players come. At startup no new
player is placed before those villages all have their habs: arrivals wait in the queue
(`queued`, reason `habs`), never disconnected for it; returning players spawn at home. Among the accepting servers, a village whose
`village_radius` disc lies in one zone is asked for before one across a zone border.
A server refusing players may still be asked for habs when it has the tps for it
(`HABS_MIN_TPS`, 55) and is in no split or merge (`busy` in `servers_load`), one village at a
time: a spawn costs more on a loaded server, but its villages are then ready when its split
for players hands them over. A server with players is asked for at most one village every
10 s when it takes players (`HABS_LOADED_EVERY`), every 30 s when it refuses them
(`HABS_REFUSING_EVERY`); one without players (startup prespawn, fresh child) is not paced. A village's world is resolved again whenever its own position changes: Godot stands it
on its ground (radially, possibly by kilometres) before spawning its habs. A refusing server still gets players when no accepting one has room, and
without a fresh load (none yet, or older than 30 s) the placement is the historical
one: the fullest village under the cap, the next villages requested near the last.
Why: filling villages in line sent every arrival to the server owning the next
village, faster than splits could take them away (228 players on one server,
minikube load test 2026-10-02).

### The queue

When the best village is on a refusing server (no accepting one has room), the
player is **not** forced onto it: it waits in the placement queue
(`genericprops-placement` thread, first arrived first served) and no apartment is
given yet, so a village or a server freed meanwhile takes it. The players back from
an earlier session go through the same queue: persistence's `create_object` of a
player is held by genericprops until the server owning its apartment accepts, and
the player always spawns in its own apartment (`spawn_appartment_id`), never where it
was last seen. The queue is looked at again on every `servers_load` and every 2 s.

A server is given at most `queue_admit_per_server` players (10) every 2 s, queued or
not: the 49 queued players let in at once when a server came back above
`placement_min_tps` took it from 61 to 14 tps (minikube, 2026-10-20). A server that had
its share counts as refusing for the rest of the window, so the arrivals go to another
server first; the rest wait their turn.

A queued player gets `{"type":"queued","reason":"server_full"|"habs","position":N}`
as soon as its place moves (at most once a second), else every 10 s; each one restarts
the client's 45 s spawn watchdog. However long it waits, it is never forced onto a
server that refuses: spawned there after 120 s, returning NPCs held one server at 238
players and 20 tps while its splits could not keep up (preprod, 2026-10-20). Only a
split, a merge done or the tps coming back lets the queue go, so every refusal must
lead to one of them: `placement_capacity` must stay above the `players:N` threshold
of the split rule (equal to it, a full server never goes over the rule and is never
split), and `placement_min_tps` at or under its `tps:N` threshold (above it, a server
in between takes no player and is never split for it). Both are warned at startup.
When no room can come — no server takes players or is loading the zones it was just
handed, no split or merge is running or queued, and no idle server is left (the
`idle_servers` / `ops_running` of `servers_load`) — or when every village is spawned
and its buildings are full, a waiting player is disconnected after 15 s of it with the
error `All servers are full` (code 1339). Short of that it keeps waiting: a returning
player whose server is full while others take players waits for that server.

A village takes `max_players_per_village` (50) players. No village goes over it while a
place can still come: a village under it on a server taking players (one refused only by
the `queue_admit_per_server` throttle counts: it takes players again 2 s later), habs on
their way, an unspawned village its server can be asked for, a split or merge running or
due (a server refusing players while an idle server is left). The player waits in the
queue meanwhile. Only when nothing can come, or after the player waited 30 s, does the
least loaded server let its villages go over, by steps of 10, as long as their buildings
have room: the server itself bounds them, refusing players at its placement capacity
until a split gives the village more servers.

A village's habs are spawned by the Godot server owning the village's centre, but each
hab has its own coordinates: a zone border can run through a village. A prop a Godot
server creates is therefore sent on to the server owning its own position
(`_source_server` on `create_object_from_gameserver`, `_except_server` on `spawn_object`),
and a new player is admitted, throttled and capped on the server owning **its hab**, the
one it lands on: a village across a border counts as one entry per server, with the habs
of that server and the whole village's occupancy. Admitted on the centre's server, 119
players were sent to a server that never had their hab ("parent_id not found",
preprod 2026-10-20).

A player in its apartment (parented to a `spawnbuilding`) belongs with its hab: its `_world`
is the hab's (`world::resolve_player_world`), for the arrival, the split snapshot and
hand-over, the freeze and `player_out_of_zone`. Godot already moves a player on a prop with
the prop; ruled by its own position, a player whose apartment was 3.9 m across a border
from its hab's origin went to a server without the hab (preprod 2026-10-21).

## Configuration (`plugins.toml`, `[ds_game_server]`)

```toml
servers_mode = "development"   # single: one server, no split/merge
game_servers = ["ws://127.0.0.1:8980", "ws://127.0.0.1:8981"]   # static pool
game_servers_dns = "godotserver:8980"  # + every address this name resolves to
split_rule = "tps:20"          # or "players:50", or "players:50 or tps:20" (any one)
merge_rule = "players:10"      # or "tps:50", or "players:10 and tps:50" (all of them)
split_after_samples = 10       # consecutive 1 s samples
merge_after_samples = 30
snapshot_timeout_secs = 5
split_warmup_secs = 5          # minimum wait after props + prewarm were sent
split_warmup_max_secs = 30     # give up waiting for readiness after this
split_ready_tps = 58           # ready = serverinfo tps >= this and chunks_loading == 0
placement_capacity = 75        # players a server takes from the placement (default: N of players:N)
placement_min_tps = 45         # under this tps a server takes no new player
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
3. **Split** (`split_rule` true for `split_after_samples`): idle servers are
   picked — one (two with several zones), or under a `players:N` rule as many as
   it takes for each piece to fall under N (`players / N + 1` pieces: 238 players
   at players:80 = 3 pieces), at most `MAX_SPLIT_CHILDREN` (4)
   (`MeshRules::split_children_wanted`); a snapshot is taken; `mesh::plan_split_n`:
   - several zones, none holding most players → whole zones move to one child
     (heaviest first, greedy balance by players, the parent keeps at least one);
   - one zone holding most players → it is cut in one piece per child, the parent
     keeps the other zones (one child: the parent keeps a piece too);
   - one zone → it is cut in one piece per server, the parent keeping the first
     under the zone's id. A cut is made on the widest axis of the population, at
     every 1/pieces of it (`mesh::split_bounds_n`; in two: the median or a wide gap,
     `mesh::split_bounds`); each child's piece gets a new zone id. A cut running
     through a village (the box of its habs, by `poi_uuid`, grown by 300 m) moves to
     the closest village edge clear of every village that keeps the balance (the
     middle half of the population; half a piece around each cut in pieces); with
     none, it stays: a village cut in two stays correct, its habs follow their zone.
   **Split for villages** (`village_split`, default on): genericprops sends
   `gameserver:villages_wanted {server_uuid, villages}` (at most every 10 s per server)
   when its reserve stays under target and the servers owning unspawned villages cannot be
   asked for them: they already wait for 2 each, or they refuse players (full, too slow;
   habs are never asked of a refusing server) while answering and not loading. The ServerManager splits that server onto one idle server
   (keeping 2 idle for the splits for players) with `mesh::plan_village_split`: the
   villages to spawn are the population of the cut, the child gets the piece with fewer
   players, and the split is skipped if that piece holds over 25 % of the players. The
   pair is not merged back for 180 s (`VILLAGE_SPLIT_HOLD`).
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
  taken for *overloaded* (its main loop is stuck in long frames) and split onto
  an idle server, if any; silent for 300 s (420 s while it loads zones it was
  just handed, i.e. before its first `serverinfo`) it is declared dead and
  re-homed. A server that reconnects after that gets a `release` first, so a
  Godot that was only frozen drops the zones it still simulates. The servers of
  the op in flight are exempt (they are busy instantiating what it sent) and are
  "touched" when it ends. `serverinfo` is handled by the socket reader itself,
  never queued behind the positions (a burst of those delayed it for minutes).
  Writes to a Godot server go through a per-connection queue drained by its own
  thread (`handlers::Outbox`): a frozen server stops reading, its TCP buffer fills
  and a blocking write used to hold the sender mutex until every worker of the
  plugin runtime was stuck on it and the manager loop stopped. Its backlog is the
  `outbox` field of `[mesh] state`; over 1000 the server takes no new player.
  The `players:N` split rule uses the larger of Godot's `players_number` and
  Horizon's own count (Godot's lags), and `split_after_samples` counts seconds
  of overload: a sample stands for the time since the previous one, so a server
  that reports twice a minute still splits after ~10 s over the rule. Before
  the ops were moved off the loop, a 70 s split stalled the loop itself and
  every *other* server came out of it "silent" — a false death that re-homed
  the players onto a third cold server.
- **Settle window**: for 15 s after an op, the samples of the servers involved
  do not count for split/merge rules (Godot grants 5 s of grace after
  `update_zones`, then erases the players it lost a few per second, so the
  parent still reports its old `players_number` for a while). Split/merge hit
  counters are reset, and nothing is evaluated while an op is in flight.
- A split under a rule with a `players:N` condition whose plan gives away zero
  players is skipped (the count still describes players already handed over;
  moving only empty zones would not lighten a slow server either).
- Re-homing and grants queue up behind the op in flight; a split or merge is
  only decided when nothing is in flight (its rule re-fires if still true).

Logs are prefixed `[mesh]`.
