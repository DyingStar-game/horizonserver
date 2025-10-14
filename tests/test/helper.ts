import { WebSocket } from "ws";
import { aPlayerLoginWs } from "../builder/builders";
import { PlayerLogingWsType } from "../builder/model/playerLogin.ws.model";

export const WS_ADDRESS = "ws://127.0.0.1:7040";

export function waitForPlayerId(
  ws: WebSocket,
  playerName: string,
  timeoutMs = 4000
): Promise<string> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error("Timeout waiting for player_id")),
      timeoutMs
    );

    ws.on("message", (raw) => {
      const msg = JSON.parse(raw.toString());
      if (msg.channel === 2 && msg.zone_data?.name === playerName) {
        clearTimeout(timer);
        resolve(msg.player_id);
      }
    });
  });
}

export function waitForMessage<T = any>(
  ws: WebSocket,
  filter: (msg: any) => boolean,
  timeoutMs = 4000
): Promise<T> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error("Timeout waiting for message")),
      timeoutMs
    );

    ws.on("message", (raw) => {
      const msg = JSON.parse(raw.toString());
      if (filter(msg)) {
        clearTimeout(timer);
        resolve(msg);
      }
    });
  });
}

type PlayerConnectionType<T = any> = {
  ws: WebSocket;
  playerId: string;
  login: string;
  getMessage: (filter: (msg: any) => boolean, timeoutMs?: number) => Promise<T>;
};

export async function simulatePlayers<T = any>(
  players: PlayerLogingWsType["data"][]
): Promise<PlayerConnectionType<T>[]> {
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

  // Waiting for player_id identification via channel 2
  const playerIds = await Promise.all(
    webs.map((ws, i) => waitForPlayerId(ws, players[i].login))
  );

  // Creation of reusable connection objects
  playerIds.forEach((playerId, i) => {
    const ws = webs[i];
    connections.push({
      ws,
      playerId,
      login: players[i].login,
      getMessage: (filter, timeoutMs = 4000) =>
        waitForMessage<T>(
          ws,
          (msg) => msg.player_id === playerId && filter(msg),
          timeoutMs
        ),
    });
  });

  return connections;
}
