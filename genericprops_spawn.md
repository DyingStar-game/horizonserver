# Procedure to spawn an item by the player


## Godot client

* On godot client, in file *normal_player.gd*, in function *_unhandled_input*, set an action pressed

* emit a signal:
  ```
  	emit_signal(
		"client_action_requested",
		{
			"action": "spawn",
			"entity": "box50cm",
			"spawn_position": item_spawn_position,
			"scenename": "scenes/props/testbox/box_50cm.tscn",
			"parent_id": parent.uuid,
		}
	)
    ```
    * The *entity* is the object type we will need to create into the Horizon plugin *genericprops*
    * The *spawn_position* is the relative position (`position`) where spawn the item (often before the player)
    * the *scenename* is the path and file of the scene
    * the *parent_id* is the *uuid* of the parent id, for example the planet

## Horizon

### Create the genericprops type

In the folder *ds_genericprops/props/*, create a JSON file (you can take example on *box50cm_def.json* file)









