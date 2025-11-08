import { z } from "zod";
import { coordinate3dSchema, gorcEventWsSchema } from "./gorcBase.ws.model";

export const gorcEventCh0WsSchema = gorcEventWsSchema.extend({
  data: z.object({
    player_id: z.string(),
    new_position: coordinate3dSchema,
    velocity: coordinate3dSchema,
    movement_state: z.number(),
    client_timestamp: z.string(),
  }),
});

export type GorcEventCh0WsType = z.infer<typeof gorcEventCh0WsSchema>;

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
