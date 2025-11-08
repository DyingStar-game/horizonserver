import { z } from "zod";
import { coordinate3dSchema, gorcZoneEnterWsSchema } from "./gorcBase.ws.model";

export const gorcPlayerCh0WsSchema = gorcZoneEnterWsSchema.extend({
  zone_data: z.object({
    name: z.string().optional(),
    health: z.number(),
    position: coordinate3dSchema,
    velocity: coordinate3dSchema,
  }),
});

export type GorcPlayerCh0WsType = z.infer<typeof gorcPlayerCh0WsSchema>;

// {
//     "channel": 0,
//     "object_id": "bcd4ee4e-91bd-4645-bbed-70d569be2141",
//     "object_type": "GorcPlayer",
//     "player_id": "5213cfa2-5ad4-4055-9af9-4118936c225a",
//     "timestamp": 1760225547,
//     "type": "gorc_zone_enter",
//     "zone_data": {
//         "health": 100,
//         "position": {
//             "x": 0,
//             "y": 0,
//             "z": 0
//         },
//         "velocity": {
//             "x": 0,
//             "y": 0,
//             "z": 0
//         },
//         "name: ""
//     }
// }

export const gorcPlayerCh1WsSchema = gorcZoneEnterWsSchema.extend({
  zone_data: z.object({
    level: z.number(),
    movement_state: z.string(),
  }),
});

export type GorcPlayerCh1WsType = z.infer<typeof gorcPlayerCh1WsSchema>;

// {
//     "channel": 1,
//     "object_id": "bcd4ee4e-91bd-4645-bbed-70d569be2141",
//     "object_type": "GorcPlayer",
//     "player_id": "5213cfa2-5ad4-4055-9af9-4118936c225a",
//     "timestamp": 1760225547,
//     "type": "gorc_zone_enter",
//     "zone_data": {
//         "level": 1,
//         "movement_state": "idle"
//     }
// }

export const gorcPlayerCh2WsSchema = gorcZoneEnterWsSchema.extend({
  zone_data: z.object({
    chat_bubble: z.string().nullable(),
    name: z.string(),
  }),
});

export type GorcPlayerCh2WsType = z.infer<typeof gorcPlayerCh2WsSchema>;

// {
//     "channel": 2,
//     "object_id": "bcd4ee4e-91bd-4645-bbed-70d569be2141",
//     "object_type": "GorcPlayer",
//     "player_id": "5213cfa2-5ad4-4055-9af9-4118936c225a",
//     "timestamp": 1760225547,
//     "type": "gorc_zone_enter",
//     "zone_data": {
//         "chat_bubble": null,
//         "name": "Player_5213cfa2-5ad4-4055-9af9-4118936c225a"
//     }
// }
