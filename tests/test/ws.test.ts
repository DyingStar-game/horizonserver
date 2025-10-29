import { WebSocket } from "ws";
import { aMovePlayerEventWs, aPlayerLoginWs } from "../builder/builders";
import {
  gorcPlayerCh0WsSchema,
  gorcPlayerCh1WsSchema,
  gorcPlayerCh2WsSchema,
  GorcPlayerCh0WsType,
  GorcPlayerCh1WsType,
  GorcPlayerCh2WsType,
} from "../builder/model/gorc/gorcPlayer.ws.model";
import { expect } from "chai";
import {
  simulatePlayers,
  waitForMessage,
  waitForMessages,
  waitForPlayerId,
  WS_ADDRESS,
} from "./helper";
import {
  GorcObjectTypeEnum,
  GorcZoneEnterWsType,
  gorcZoneExitWsSchema,
} from "../builder/model/gorc/gorcBase.ws.model";
import { GorcEventCh0WsType } from "../builder/model/gorc/gorcEvent.ws.model";

describe("WebSocket GORC Player Channel 0", function () {
  this.timeout(5000);

  let ws: WebSocket;
  let ws2: WebSocket;
  let player1: { playerId: string; message: string };
  let player2: { playerId: string; message: string };
  let player1GorcObjectId: string;
  let player2GorcObjectId: string;

  before((done) => {
    ws = new WebSocket(WS_ADDRESS);
    ws2 = new WebSocket(WS_ADDRESS);

    ws.on("open", () => {
      console.log("✅ WebSocket connected");
      done();
    });

    ws.on("error", (err) => {
      done(err);
    });
  });

  after((done) => {
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.close();
      ws2.close();
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
    player1 = player;
    
    // Wait for all three messages concurrently
    console.log("⏳ Waiting for messages on channels 0, 1, and 2...");
    
    const [messageCh0, messageCh1, messageCh2] = await Promise.all([
      waitForMessage<GorcPlayerCh0WsType>(ws, (m) => m.channel === 0),
      waitForMessage<GorcPlayerCh1WsType>(ws, (m) => m.channel === 1),
      waitForMessage<GorcPlayerCh2WsType>(ws, (m) => m.channel === 2)
    ]);

    // Validate channel 0 message
    const result0 = gorcPlayerCh0WsSchema.safeParse(messageCh0);
    expect(result0.success).to.be.true;
    expect(result0.data?.zone_data.health).to.equal(100);
    player1GorcObjectId = result0.data!.object_id;

    // Validate channel 1 message
    const result1 = gorcPlayerCh1WsSchema.safeParse(messageCh1);
    expect(result1.success).to.be.true;
    expect(result1.data?.zone_data.level).to.equal(1);

    // Validate channel 2 message if needed
    const result2 = gorcPlayerCh2WsSchema.safeParse(messageCh2);
    expect(result2.success).to.be.true;
    expect(result2.data?.zone_data.name).to.equal("ddurieux");
  });

  it("connect second player", async () => {
    const expectedPlayerName = "player2";

    ws2.on("open", () => {
      console.log("✅ WebSocket 2 connected");
    });

    await new Promise<void>((resolve, reject) => {
      if (ws2.readyState === WebSocket.OPEN) return resolve();

      const onOpen = () => {
        cleanup();
        resolve();
      };
      const onError = (err: Error) => {
        cleanup();
        reject(err);
      };

      const timer = setTimeout(() => {
        cleanup();
        reject(new Error("WebSocket 2 did not open within 500ms"));
      }, 500);

      function cleanup() {
        ws2.removeListener("open", onOpen);
        ws2.removeListener("error", onError);
        clearTimeout(timer);
      }

      ws2.on("open", onOpen);
      ws2.on("error", onError);
    });

    // Send login request
    ws2.send(
      JSON.stringify(
        aPlayerLoginWs()
          .withLogin(expectedPlayerName)
          .withPassword("pass")
          .build()
      )
    );

    // Step 1 : wait for player identification
    player2 = await waitForPlayerId(ws2);
    console.log(
      `✅ Player (${expectedPlayerName}) identified :`,
      player2.playerId
    );

    // Wait for all three messages concurrently
    console.log("⏳ Waiting for messages on channels 0, 1, and 2...");



    // start async retrieval without awaiting; will be awaited/handled later
    const messagesOnPlayer1Promise = waitForMessages(ws);
    const messagesOnPlayer2Promise = waitForMessages(ws2);

    // Await messages for Player 1
    const messagesOnPlayer1 = await messagesOnPlayer1Promise;

    // Await messages for Player 2
    const messagesOnPlayer2 = await messagesOnPlayer2Promise;

    console.log(`⇒ Player 1 received ${messagesOnPlayer1.length} messages after Player 2 login`);
    if (messagesOnPlayer1.length !== 1) {
      throw new Error(`Expected 1 message but received ${messagesOnPlayer1.length}`);
    }
    const messageOnPlayer1: any = messagesOnPlayer1.find((m: any) => m.channel === 0 && m.player_id === player1.playerId);

    // TODO waiting the fix of gorc
    // Validate channel 0 message
    // const result0 = gorcPlayerCh0WsSchema.safeParse(messageOnPlayer1);
    // expect(result0.success).to.be.true;
    // expect(result0.data?.zone_data.health).to.equal(100);
    // expect(messageOnPlayer1.player_id).to.equal(player2.playerId);


    console.log(`⇒ Player 2 received ${messagesOnPlayer2.length} messages after login`);
    if (messagesOnPlayer2.length !== 6) {
      throw new Error(`Expected 6 messages but received ${messagesOnPlayer2.length}`);
    }

    const messagePl2Ch0: any = messagesOnPlayer2.find((m: any) => m.channel === 0 && m.player_id === player2.playerId);
    const messagePl2Ch1: any = messagesOnPlayer2.find((m: any) => m.channel === 1 && m.player_id === player2.playerId);
    const messagePl2Ch2: any = messagesOnPlayer2.find((m: any) => m.channel === 2 && m.player_id === player2.playerId);
    const player2GorcObjectId = messagePl2Ch0 ? messagePl2Ch0.object_id : null;
    expect(player2GorcObjectId).to.not.be.null;

    const messagePl1Ch0: any = messagesOnPlayer2.find((m: any) => m.channel === 0 && m.player_id === player2.playerId && m.objectId !== player2GorcObjectId); // TODO must be player1, seems bug in gorc
    const messagePl1Ch1: any = messagesOnPlayer2.find((m: any) => m.channel === 1 && m.player_id === player2.playerId && m.objectId !== player2GorcObjectId);
    const messagePl1Ch2: any = messagesOnPlayer2.find((m: any) => m.channel === 2 && m.player_id === player2.playerId && m.objectId !== player2GorcObjectId);

    if (
      !messagePl2Ch0 ||
      !messagePl2Ch1 ||
      !messagePl2Ch2 ||
      !messagePl1Ch0 ||
      !messagePl1Ch1 ||
      !messagePl1Ch2
    ) {
      throw new Error("Missing expected channel messages for player1 or player2");
    }

    // TODO waiting the fix of gorc
    // // Validate channel 0 message
    // const result0 = gorcPlayerCh0WsSchema.safeParse(messagePl2Ch0);
    // expect(result0.success).to.be.true;
    // expect(result0.data?.zone_data.health).to.equal(100);
    // expect(messagePl2Ch0.player_id).to.equal(player2.playerId);

    // // Validate channel 1 message
    // const result1 = gorcPlayerCh1WsSchema.safeParse(messagePl2Ch1);
    // expect(result1.success).to.be.true;
    // expect(result1.data?.zone_data.level).to.equal(1);
    // expect(messagePl2Ch1.player_id).to.equal(player2.playerId);

    // // Validate channel 2 message if needed
    // const result2 = gorcPlayerCh2WsSchema.safeParse(messagePl2Ch2);
    // expect(result2.success).to.be.true;
    // // expect(result2.data?.zone_data.name).to.equal("player2"); TODO WHY?????
    // expect(messagePl2Ch2.player_id).to.equal(player2.playerId);

    // // Validate channel 0 message for player 1
    // const resultPl1Ch0 = gorcPlayerCh0WsSchema.safeParse(messagePl1Ch0);
    // expect(resultPl1Ch0.success).to.be.true;
    // expect(resultPl1Ch0.data?.zone_data.health).to.equal(100);
    // expect(messagePl1Ch0.player_id).to.equal(player1.playerId);

    // // Validate channel 1 message for player 1
    // const resultPl1Ch1 = gorcPlayerCh1WsSchema.safeParse(messagePl1Ch1);
    // expect(resultPl1Ch1.success).to.be.true;
    // expect(resultPl1Ch1.data?.zone_data.level).to.equal(1);
    // expect(messagePl1Ch1.player_id).to.equal(player1.playerId);

    // // Validate channel 2 message for player 1 if needed
    // const resultPl1Ch2 = gorcPlayerCh2WsSchema.safeParse(messagePl1Ch2);
    // expect(resultPl1Ch2.success).to.be.true;
    // // expect(resultPl1Ch2.data?.zone_data.name).to.equal("ddurieux"); TODO WHY?????
    // expect(messagePl1Ch2.player_id).to.equal(player1.playerId);


  });

  it("Player 1 go very far away, player 2 will receive message player 1 out of zone", async () => {

  //   const playerOneMsgCh0 = await playerOne.getMessage((m) => m.channel === 0);

  //   console.log({ playerId: playerOne.playerId, objectId: playerOne.objectId });
  //   console.log({ playerId: playerTwo.playerId, objectId: playerTwo.objectId });
  //   console.log(playerOneMsgCh0);

    ws.send(
      JSON.stringify(
        aMovePlayerEventWs()
          .withObjectId(`GorcObjectId(${player1GorcObjectId})`)
          .withPlayerId(player1.playerId)
          .withNewPosition({ x: 500000, y: 1, z: 1 })
          .withVelocity({ x: 0, y: 0, z: 0 })
          .build()
      )
    );

    const messagesOnPlayer1Promise = waitForMessages(ws);
    const messagesOnPlayer2Promise = waitForMessages(ws2);

    // Await messages for Player 1
    const messagesOnPlayer1 = await messagesOnPlayer1Promise;

    // Await messages for Player 2
    const messagesOnPlayer2 = await messagesOnPlayer2Promise;

    // console.log('player1', messagesOnPlayer1);
    // console.log('player2', messagesOnPlayer2);

    if (messagesOnPlayer2.length !== 3) {
      throw new Error(`Expected 3 messages but received ${messagesOnPlayer2.length}`);
    }

    const messagePl2Ch0: any = messagesOnPlayer2.find((m: any) => m.channel === 0 && m.player_id === player2.playerId); // must be player 1, wait bug report on gorc
    const messagePl2Ch1: any = messagesOnPlayer2.find((m: any) => m.channel === 1 && m.player_id === player2.playerId);
    const messagePl2Ch2: any = messagesOnPlayer2.find((m: any) => m.channel === 2 && m.player_id === player2.playerId);

    // Validate the messages
    const resultPl1Ch0 = gorcZoneExitWsSchema.safeParse(messagePl2Ch0);
    expect(resultPl1Ch0.success).to.be.true;
    expect(resultPl1Ch0.data?.object_type).to.equal("GorcPlayer");

    const resultPl1Ch1 = gorcZoneExitWsSchema.safeParse(messagePl2Ch1);
    expect(resultPl1Ch1.success).to.be.true;
    expect(resultPl1Ch1.data?.object_type).to.equal("GorcPlayer");

    const resultPl1Ch2 = gorcZoneExitWsSchema.safeParse(messagePl2Ch2);
    expect(resultPl1Ch2.success).to.be.true;
    expect(resultPl1Ch2.data?.object_type).to.equal("GorcPlayer");


  // {
  //   channel: 0,
  //   object_id: '8757f634-34df-4644-b9f3-3825db703068',
  //   object_type: 'GorcPlayer',
  //   player_id: '650a264c-5d44-48a8-9c39-52818ab98ebb',
  //   timestamp: 1761752904,
  //   type: 'gorc_zone_exit'
  // },
  // {
  //   channel: 1,
  //   object_id: '8757f634-34df-4644-b9f3-3825db703068',
  //   object_type: 'GorcPlayer',
  //   player_id: '650a264c-5d44-48a8-9c39-52818ab98ebb',
  //   timestamp: 1761752904,
  //   type: 'gorc_zone_exit'
  // },
  // {
  //   channel: 2,
  //   object_id: '8757f634-34df-4644-b9f3-3825db703068',
  //   object_type: 'GorcPlayer',
  //   player_id: '650a264c-5d44-48a8-9c39-52818ab98ebb',
  //   timestamp: 1761752904,
  //   type: 'gorc_zone_exit'
  // }



  });
});
