import { z } from "zod";

export enum GorcObjectType {
  PLAYER = "GorcPlayer",
}

export enum GorcType {
  ZONE_ENTER = "gorc_zone_enter",
}

export const gorcBaseWsSchema = z.object({
  channel: z.number(),
  object_id: z.uuidv4(),
  object_type: z.enum(GorcObjectType),
  player_id: z.uuidv4(),
  timestamp: z.number(),
  type: z.enum(GorcType),
  zone_data: z.object(),
});

export type GorcBaseWsType = z.infer<typeof gorcBaseWsSchema>;

export const coordinate3dSchema = z.object({
  x: z.number(),
  y: z.number(),
  z: z.number(),
});

export type Coordinate3dType = z.infer<typeof coordinate3dSchema>;
