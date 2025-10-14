import { z } from "zod";

export enum GorcObjectTypeEnum {
  PLAYER = "GorcPlayer",
}

export enum GorcTypeEnum {
  EVENT = "gorc_event",
  ZONE_ENTER = "gorc_zone_enter",
}

export enum GorcEventEnum {
  MOVE = "move",
}

export const gorcBaseWsSchema = z.object({
  channel: z.number(),
  type: z.enum(GorcTypeEnum),
  object_id: z.string(),
  player_id: z.uuidv4(),
  timestamp: z.number(),
});

export type GorcBaseWsType = z.infer<typeof gorcBaseWsSchema>;

export const gorcZoneEnterWsSchema = gorcBaseWsSchema.extend({
  type: z.literal(GorcTypeEnum.ZONE_ENTER),
  object_type: z.enum(GorcObjectTypeEnum),
  zone_data: z.object(),
});

export type GorcZoneEnterWsType = z.infer<typeof gorcZoneEnterWsSchema>;

export const gorcEventWsSchema = gorcBaseWsSchema.extend({
  type: z.literal(GorcTypeEnum.EVENT),
  event: z.enum(GorcEventEnum),
  data: z.object(),
});

export type GorcEventWsType = z.infer<typeof gorcEventWsSchema>;

export const coordinate3dSchema = z.object({
  x: z.number(),
  y: z.number(),
  z: z.number(),
});

export type Coordinate3dType = z.infer<typeof coordinate3dSchema>;

//  {
//   type: "gorc_event",
//   object_id: "GorcObjectId(" + player1_gorc_id + ")",
//   channel: 0,
//   event: "move",
//   data: {
//     player_id: player1_player_id,
//     new_position: {
//       x: 500000.0,
//       y: 1.0,
//       z: 1.0,
//     },
//     velocity: {
//       x: 10.0,
//       y: 1.0,
//       z: 1.0,
//     },
//     movement_state: 1,
//     client_timestamp: "2025-10-11T09:57:45Z",
//   },
//   player_id: player1_player_id,
// };

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
