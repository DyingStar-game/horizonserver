# Genericprops

```
{
    "namespace": "genericprops",
    "event": "object",
    "data": {
        "object_type": "planet",
        "object_uuid": "ed536e44-c2d7-4deb-bfbf-597bd335db03",
        "object_data": {
            "name": "toto",
            "position": {"x":1.0, "y": 1.0, "z":1.0},
            "rotation": {"x":0.0, "y": 0.0, "z":0.0},
            "uuid": "ed536e44-c2d7-4deb-bfbf-597bd335db03"
        }
    }
}
```

## Object definitions (`props/*_def.json`)

Each object type is described by a list of channels. A channel is both a slice of
the object's state and a replication zone:

```json
{
  "channels": [
    {
      "zone": 0,
      "distance": 200.0,
      "frequency": 30.0,
      "lod": [
        { "distance": 100.0, "frequency": 30.0 },
        { "distance": 200.0, "frequency": 10.0 }
      ],
      "properties": ["position", "rotation", "parent_id"]
    }
  ]
}
```

| Field | Meaning |
|---|---|
| `zone` | GORC channel number. Partitions the object's data and identifies the network channel. |
| `distance` | Subscription radius, in metres. A player inside it receives this channel, a player outside receives nothing. |
| `frequency` | Maximum sends per second for this channel, when no `lod` ladder is given. |
| `lod` | Optional rate ladder: subscribers within a tier's `distance` are served at most `frequency` times per second. Innermost matching tier wins; order in the file does not matter. |
| `properties` | Property names carried by this channel. |

A property belongs to exactly one zone — listing it in two channels keeps only
the last one, because the property→zone index is a plain map. To make an object
update more slowly at range, add an `lod` ladder to its channel rather than
declaring the property twice.

### How the rate is applied

`distance` is enforced by GORC itself. `frequency` and `lod` are enforced by
[`src/lod.rs`](src/lod.rs): handlers queue channel payloads instead of emitting
them, and a background loop delivers the newest queued payload to each recipient
at the rate its distance earns it.

Payloads are full snapshots of the channel, never deltas, so intermediate
versions can be dropped safely. Per-recipient bookkeeping guarantees the last
version is always delivered — an object that stops moving never leaves a client
on a stale position.

Two consequences worth keeping in mind when editing a definition:

- `frequency` is now real. A channel declaring `1.0` really is limited to one
  send per second, where it used to pass everything through.
- The ladder only shapes traffic that is already inside `distance`. A tier
  beyond `distance` is unreachable, and subscribers past the outermost tier fall
  back to that tier's rate.
