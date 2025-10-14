import { WebSocket } from "ws";
import { aMovePlayerEventWs, aPlayerLoginWs } from "../builder/builders";
import {
  gorcPlayerCh0WsSchema,
  GorcPlayerCh0WsType,
} from "../builder/model/gorc/gorcPlayer.ws.model";
import { expect } from "chai";
import {
  simulatePlayers,
  waitForMessage,
  waitForPlayerId,
  WS_ADDRESS,
} from "./helper";
import {
  GorcObjectTypeEnum,
  GorcZoneEnterWsType,
} from "../builder/model/gorc/gorcBase.ws.model";
import { GorcEventCh0WsType } from "../builder/model/gorc/gorcEvent.ws.model";

describe("WebSocket GORC Player Channel 0", function () {
  this.timeout(5000);

  let ws: WebSocket;

  beforeEach((done) => {
    ws = new WebSocket(WS_ADDRESS);

    ws.on("open", () => {
      console.log("✅ WebSocket connected");
      done();
    });

    ws.on("error", (err) => {
      done(err);
    });
  });

  afterEach((done) => {
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.close();
      ws.on("close", () => done());
    } else {
      done();
    }
  });

  it("check reponse after login with one player", async () => {
    const expectedPlayerName = "ddurieux";

    // Send login request
    ws.send(
      JSON.stringify(
        aPlayerLoginWs()
          .withLogin(expectedPlayerName)
          .withPassword("pass")
          .build()
      )
    );

    // Step 1 : wait for player identification
    const player = await waitForPlayerId(ws);
    console.log(
      `✅ Player (${expectedPlayerName}) identified :`,
      player.playerId
    );

    // Étape 2 : wait for a message for this player on channel 0
    const message = await waitForMessage<GorcPlayerCh0WsType>(
      ws,
      (m) => m.player_id === player.playerId && m.channel === 0
    );

    const result = gorcPlayerCh0WsSchema.safeParse(message);
    expect(result.success).to.be.true;
    expect(result.data?.zone_data.health).to.equal(100);
  });

  it("should handle multiple simultaneous players", async () => {
    const players = [
      aPlayerLoginWs().build().data,
      aPlayerLoginWs().build().data,
      aPlayerLoginWs().build().data,
    ];

    // Simulate players
    const connections = await simulatePlayers(players);

    // Waiting message channel 0 for all players
    const messages = await Promise.all(
      connections.map((conn) => conn.getMessage((m) => m.channel === 0))
    );

    messages.forEach((msg, i) => {
      const result = gorcPlayerCh0WsSchema.safeParse(msg);
      expect(result.success).to.be.true;
      console.log(`✅ Player ${connections[i].login} message valid`);
    });

    // Close all connections
    connections.forEach((conn) => conn.ws.close());
  });

  // TODO: Change test
  it("close first connection, others players disconnect message", async () => {
    const players = [
      aPlayerLoginWs().build().data,
      aPlayerLoginWs().build().data,
      aPlayerLoginWs().build().data,
    ];

    // Simulate players
    const playerConnections = await simulatePlayers<GorcZoneEnterWsType>(
      players
    );

    // Close first player connection
    playerConnections[0].ws.close();

    // Keep other players connections
    const otherPlayerConnections = playerConnections.slice(1);

    // Filter on all messages receive by other players until 1second
    // TODO : Change test for check player_disconnect message
    const messages = await Promise.all(
      otherPlayerConnections.map((conn) =>
        conn.getMessages(
          (m) => m.object_type !== GorcObjectTypeEnum.PLAYER,
          1000
        )
      )
    );

    messages.forEach((msg, i) => {
      console.log(
        `✅ Player ${otherPlayerConnections[i].login} received disconnect:`,
        msg
      );
      // expect(msg.player_id).to.equal(disconnectedPlayer.playerId);
      // expect(msg.type).to.equal("player_disconnect");
    });

    // Close all remaining connections
    playerConnections.slice(1).forEach((conn) => conn.ws.close());
  });

  it("Player 1 go very far away, second player will not receive message", async () => {
    const players = [
      aPlayerLoginWs().build().data,
      aPlayerLoginWs().build().data,
    ];

    // Simulate players
    const playerConnections = await simulatePlayers<GorcEventCh0WsType>(
      players
    );

    const playerOne = playerConnections[0];
    const playerTwo = playerConnections[1];

    const playerOneMsgCh0 = await playerOne.getMessage((m) => m.channel === 0);

    console.log({ playerId: playerOne.playerId, objectId: playerOne.objectId });
    console.log({ playerId: playerTwo.playerId, objectId: playerTwo.objectId });
    console.log(playerOneMsgCh0);

    playerOne.ws.send(
      JSON.stringify(
        aMovePlayerEventWs()
          .withObjectId(`GorcObjectId(${playerOneMsgCh0.object_id})`)
          .withPlayerId(playerOne.playerId)
          .withNewPosition({ x: 500000, y: 1, z: 1 })
          .withVelocity({ x: 0, y: 0, z: 0 })
          .build()
      )
    );

    const messages = await Promise.all(
      playerConnections.map((conn) => conn.getOtherMessages(), 3000)
    );

    console.log(messages);

    playerConnections.forEach((conn) => conn.ws.close());
  });
});
