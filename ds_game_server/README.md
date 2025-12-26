# Godot servers management

## Information

it's ds_game_server who manage the godot servers,
the zones (meshing)

cut by universe (for objects with no parent),
then by planet / moon

inside both, split by zones

## Schemas


### Create new godot server

```
    [THREAD management]
+----- spawn a new server <-----------------------------------------------+
|                                                                         |
|   [THREAD]                                                              |
|   serveur 1                                                             |
|     -> send message                                                     |
|     -> receive message                                                  |
|                                                                         |
|     if server FPS for last 20 times is under 20, trigger new server |---+
|     I split my zone with some criteria                              |
|
+-> [THREAD]
    new server, number 2 spawned by thread management

```


