import { WebSocketServer, WebSocket } from "ws";

// Fake Godot game server used by the integration tests. It speaks just enough of
// the Horizon <-> Godot protocol:
//  - remembers the zones Horizon gives it (server/zone, data.zones),
//  - sends a serverinfo sample every second (with `tps`, like the real server),
//  - answers a player add_prop with two players/position packets.
// Start several on different ports (GODOT_FAKE_PORTS="8980,8981") to exercise a
// split between two servers.

export interface FakeGodotServer {
  port: number;
  zones: any[];
  serverUuid: string;
  players: string[];
  server: WebSocketServer;
}

export function startFakeGodot(port: number): FakeGodotServer {
  const fake: FakeGodotServer = {
    port,
    zones: [],
    serverUuid: "",
    players: [],
    server: new WebSocketServer({ port }),
  };

  fake.server.on("connection", (serverWs: WebSocket) => {
    console.log(`✅ Client connected to test godot server on port ${port}`);

    const metrics = setInterval(() => {
      if (fake.serverUuid === "" || serverWs.readyState !== WebSocket.OPEN) return;
      serverWs.send(JSON.stringify({
        namespace: "props",
        event: "position",
        amessagenb: 0,
        data: [{
          uuid: fake.serverUuid,
          type: "serverinfo",
          tps: 60,
          objects_number: 1,
          players_number: fake.players.length,
          scenes_number: 1,
        }],
      }));
    }, 1000);

    serverWs.on("message", (data) => {
      try {
        const message = JSON.parse(data.toString());
        console.log(`📨 [${port}] Received message:`, message);

        if (message.namespace === "server" && message.event === "zone") {
          fake.serverUuid = message.server_uuid;
          fake.zones = message.data?.zones ?? [];
          console.log(`🗺️  [${port}] zones:`, JSON.stringify(fake.zones));
          return;
        }

        if (message.namespace === "server" && message.event === "freeze_object"
            && message.data?.object_type === "player") {
          fake.players = fake.players.filter((p) => p !== message.data.object_uuid);
          return;
        }

        // Check if this is the expected "add_prop" message
        if ((message.event === "add_prop" || message.event === "initial_object") &&
            message.namespace === "server" &&
            message.data?.object_type === "player") {
          console.log(`🎯 [${port}] Matched player ${message.event}, sending response`);
          if (!fake.players.includes(message.data.object_uuid)) {
            fake.players.push(message.data.object_uuid);
          }

          // Send the specified response
          let response = {
            "amessagenb": 20,
            "data": [
              {
                "pos": { "x": message.data.object_data.position.x + 10, "y": 0, "z": 0 },
                "rot": { "x": 0, "y": 0, "z": 0 },
                "player_id": message.data.object_data.connection_id ?? message.data.object_uuid,
              }
            ],
            "event": "position",
            "namespace": "players"
          };

          serverWs.send(JSON.stringify(response));

          response.amessagenb = 21;
          response.data[0].pos.x += 10;
          serverWs.send(JSON.stringify(response));
        }
      } catch (error) {
        console.error('❌ Error parsing message:', error);
      }
    });

    serverWs.on("close", () => {
      clearInterval(metrics);
      console.log(`👋 Client disconnected from test godot server ${port}`);
    });
  });

  return fake;
}

// Start WebSocket server(s) (godot server simulation), port 8980 by default.
const ports = (process.env.GODOT_FAKE_PORTS ?? "8980").split(",").map((p) => parseInt(p.trim(), 10));
export const fakeGodotServers: FakeGodotServer[] = ports.map(startFakeGodot);
