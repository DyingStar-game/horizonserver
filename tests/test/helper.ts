import { WebSocket } from "ws";
import { aPlayerLoginWs } from "../builder/builders";
import { PlayerLogingWsType } from "../builder/model/playerLogin.ws.model";
import { GorcBaseWsType } from "../builder/model/gorc/gorcBase.ws.model";

export const WS_ADDRESS = "ws://127.0.0.1:7040";

export function waitForPlayerId(
  ws: WebSocket,
  timeoutMs = 4000
): Promise<{ playerId: string; objectId: string }> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      ws.off("message", onMessage);
      reject(new Error("Timeout waiting for player_id"));
    }, timeoutMs);

    const onMessage = (raw: any) => {
      try {
        const msg = JSON.parse(raw.toString());

        clearTimeout(timer);
        ws.off("message", onMessage);
        resolve({ playerId: msg.player_id, objectId: msg.object_id });
      } catch (err) {
        console.warn("Invalid JSON message:", raw.toString());
      }
    };

    ws.on("message", onMessage);
  });
}

export function waitForMessage<
  T extends { player_id: string } = GorcBaseWsType
>(ws: WebSocket, filter: (msg: T) => boolean, timeoutMs = 4000): Promise<T> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      ws.off("message", onMessage);
      reject(new Error("Timeout waiting for message"));
    }, timeoutMs);

    const onMessage = (raw: any) => {
      try {
        const msg: T = JSON.parse(raw.toString());
        if (filter(msg)) {
          clearTimeout(timer);
          ws.off("message", onMessage);
          resolve(msg);
        }
      } catch (err) {
        console.warn("Invalid JSON message:", raw.toString());
      }
    };

    ws.on("message", onMessage);
  });
}

export function waitForMessages<
  T extends { player_id: string } = GorcBaseWsType
>(ws: WebSocket, filter?: (msg: T) => boolean, timeoutMs = 4000): Promise<T[]> {
  return new Promise((resolve) => {
    const allMsg: T[] = [];

    const timer = setTimeout(() => {
      ws.off("message", onMessage);
      resolve(allMsg);
    }, timeoutMs);

    const onMessage = (raw: any) => {
      try {
        const msg = JSON.parse(raw.toString());
        if (!filter || filter(msg)) {
          allMsg.push(msg);
        }
      } catch (err) {
        console.warn("Invalid JSON message:", raw.toString());
      }
    };

    ws.on("message", onMessage);
  });
}

type PlayerConnectionType<T = GorcBaseWsType> = {
  ws: WebSocket;
  playerId: string;
  objectId: string;
  login: string;
  getMessage: (filter: (msg: T) => boolean, timeoutMs?: number) => Promise<T>;
  getMessages: (
    filter?: (msg: T) => boolean,
    timeoutMs?: number
  ) => Promise<T[]>;
  getOtherMessages: (
    filter?: (msg: T) => boolean,
    timeoutMs?: number
  ) => Promise<T[]>;
};

export async function simulatePlayers<
  T extends { player_id: string } = GorcBaseWsType
>(players: PlayerLogingWsType["data"][]): Promise<PlayerConnectionType<T>[]> {
  const connections: PlayerConnectionType<T>[] = [];

  // Create and open WebSocket
  const webs = players.map((p) => new WebSocket(WS_ADDRESS));
  await Promise.all(
    webs.map(
      (ws) =>
        new Promise<void>((res, rej) => {
          ws.on("open", res);
          ws.on("error", rej);
        })
    )
  );

  // Send Login
  webs.forEach((ws, i) => {
    ws.send(
      JSON.stringify(
        aPlayerLoginWs()
          .withLogin(players[i].login)
          .withPassword(players[i].password)
          .build()
      )
    );
  });

  // Waiting for player_id identification
  const playerIds = await Promise.all(webs.map((ws, i) => waitForPlayerId(ws)));

  // Creation of reusable connection objects
  playerIds.forEach((player, i) => {
    const ws = webs[i];
    connections.push({
      ws,
      playerId: player.playerId,
      objectId: player.objectId,
      login: players[i].login,
      getMessage: (filter, timeoutMs = 4000) =>
        waitForMessage<T>(
          ws,
          (msg) => msg.player_id === player.playerId && filter(msg),
          timeoutMs
        ),
      getMessages: (filter, timeoutMs = 4000) =>
        waitForMessages<T>(
          ws,
          (msg) =>
            msg.player_id === player.playerId && (!filter || filter(msg)),
          timeoutMs
        ),
      getOtherMessages: (filter, timeoutMs = 4000) =>
        waitForMessages<T>(
          ws,
          (msg) =>
            msg.player_id !== player.playerId && (!filter || filter(msg)),
          timeoutMs
        ),
    });
  });

  return connections;
}

export function delay(ms: number) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
