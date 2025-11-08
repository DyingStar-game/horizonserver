import { z } from "zod";

export enum EventWs {
  INIT = "init",
}

export enum NamespaceWs {
  PLAYER = "player",
}

export const messageWsSchema = z.object({
  namespace: z.enum(NamespaceWs),
  event: z.enum(EventWs),
  data: z.object(),
});

export type MessageWsType = z.infer<typeof messageWsSchema>;
