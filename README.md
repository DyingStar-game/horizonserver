# Horizon usage

## Local setup

The Horizon server itself lives in `Horizon/`, a git submodule pointing at
[DyingStar-game/Horizon](https://github.com/DyingStar-game/Horizon) — our fork,
where `main` mirrors upstream and our patches sit on `ds-develop`. Clone with
submodules:

- Install Docker
- Clone the repository `git clone --recurse-submodules https://github.com/DyingStar-game/horizonserver.git`
  (already cloned without `--recurse-submodules`? `scripts/install.sh` fixes it up)
- Run `docker compose up -d` to start the dev environment
- Enter the docker container with `docker exec -it horizon_1 bash`
- Run `scripts/install.sh` to check out the pinned Horizon commit and drop in the configs
- You can modify the `game_server_address` in `plugins.toml` to `host.docker.internal:8980` if you are not on linux
- Run `scripts/build.sh` to build the plugins
- Run `scripts/run.sh` to start the horizon server

The two runtime configs live in this repo, not in the fork: `dev_config.toml`
(bind address, GORC settings — passed to the server as `--config`) and
`plugins.toml`. `scripts/install.sh` copies both into `Horizon/`, where the
server reads them. Edit them here; anything you change directly inside
`Horizon/` is overwritten on the next `install.sh`.

To pull in upstream Horizon changes or to add a patch of our own, see
[docs/horizon-fork.md](docs/horizon-fork.md).

## Send a message to all players

To broadcast a message to all players connected in a plugin event part, use:

```rust
let login_announcement = serde_json::json!({
    "type": "user_login",
    "message": format!("User {} has joined the game", username),
    "timestamp": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()
});

// Example of how to broadcast from a sync context:
// Method 1: Using tokio::spawn for fire-and-forget broadcasting
{
    let events = events.clone();
    let announcement = login_announcement.clone();
    tokio::spawn(async move {
        if let Ok(client_count) = events.broadcast(&announcement).await {
            info!("🔧 StarsBeyondPlugin: Broadcasted login announcement to {} clients", client_count);
        } else {
            info!("🔧 StarsBeyondPlugin: Failed to broadcast login announcement");
        }
    });
}
```

## list of all messages

This is the list of messages exchanged between parts.

### From client to Horizon

| description        | namespace   | event        | data                                               |
| ------------------ | ---------   | -----        | ---------------------------------------------------|
| connect to server  | player      | init         | {"name":"ddurieux","spawnpoint":0}                 |
| move and rotation  | player      | move         | {"dir": {"x":1.0,"y":0.0,"z":0.3},"rot": {"x":1.0,"y":2.5,"z":-3.7}} |
| press key          | player      | action       | {"action":"jump"}                                  |
| press key          | player      | action       | {"action":"spawn_box50cm"}                         |


### From Horizon to game server

| description        | namespace   | event        | data                                               |
| ------------------ | ---------   | -----        | ---------------------------------------------------|
| player connected   | player      | spawn        | {"pos": {"x":1.0,"y":2.5,"z":-3.7}}                |
| move               | player      | move         | {"dir": {"x":1.0,"y":0.0,"z":0.3}}                 |
| spawn box50cm      | prop        | spawn        | {"name": "box50cm", "player_id": "566-645xxx", "pos": {"x":476.67,"y":23.45,"z":0.564}, "prop_id":"yu76-t45txxx"} |


### From game server to Horizon

| description        | namespace   | event        | data                                               |
| ------------------ | ---------   | -----        | ---------------------------------------------------|
| new player pos     | player      | position     | {"pos": {"x":456.67,"y":23.45,"z":0.564}}          |
| new prop pos       | prop        | position     | {"pos": {"x":466.67,"y":23.45,"z":0.564},"rot": {"x":1.0,"y":2.5,"z":-3.7}, "prop_id":"yu76-t45txxx"}


### From Horizon to player

| description        | namespace   | event        | data                                               |
| ------------------ | ---------   | -----        | ---------------------------------------------------|
| player first position| player    | firstpos     | {"x":1.0,"y":2.5,"z":-3.7}
| player xx position | player      | position     | {"pos": {"x":456.67,"y":23.45,"z":0.564},"rot": {"x":1.0,"y":2.5,"z":-3.7}} |
| prop first position| prop        | firstpos     | {"name": "box50cm", "pos": {"x":466.67,"y":23.45,"z":0.564},"rot": {"x":1.0,"y":2.5,"z":-3.7}, "prop_id":"yu76-t45txxx"} |
| new prop pos       | prop        | position     | {"pos": {"x":466.67,"y":23.45,"z":0.564},"rot": {"x":1.0,"y":2.5,"z":-3.7}, "prop_id":"yu76-t45txxx"} |


## Scenarii


### Player connect to server

The player connect to the server

The payload sent by the client is:

```json
{
    "namespace": "player",
    "event": "init",
    "data": {
        "name":"ddurieux",
        "spawnpoint":0
    }
}
```

The payload received by the Horizon server is:

```json
{
    "namespace": "player",
    "event": "init",
    "player_id": PlayerId,
    "data": {
        "name":"ddurieux",
        "spawnpoint":0
    }
}
```

Horizon will give to the game server the player info
Horizon will give to the client all items to load
Horizon will give to the client the player spwan position


## Plugins

### ds_game_server

Do the relation between Horizon and game server


### ds_props

Manage the props database



#################

Manage parent item / remap item
when spawn first time, we are children of the planet.
when enter in ship, the ship is children of the planet and the player the children of the ship


