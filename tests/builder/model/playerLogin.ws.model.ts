import { z } from "zod";
import { messageWsSchema } from "./message.ws.model";

export const playerLogingWsSchema = messageWsSchema.extend({
  data: z.object({
    login: z.string(),
    password: z.string(),
  }),
});

export type PlayerLogingWsType = z.infer<typeof playerLogingWsSchema>;
