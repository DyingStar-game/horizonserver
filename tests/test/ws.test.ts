import { WebSocket } from "ws";
import { aPlayerLoginWs } from "../builder/builders";
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

describe("WebSocket GORC Player Channel 0", function () {
  this.timeout(5000);

  let ws: WebSocket;

  beforeEach((done) => {
    ws = new WebSocket(WS_ADDRESS);

    ws.on("open", () => {
      console.log("✅ WebSocket connecté");
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

    // Step 1 : wait for player identification via channel 2
    const playerId = await waitForPlayerId(ws, expectedPlayerName);
    console.log(`✅ Player (${expectedPlayerName}) identified :`, playerId);

    // Étape 2 : wait for a message for this player on channel 0
    const message = await waitForMessage<GorcPlayerCh0WsType>(
      ws,
      (m) => m.player_id === playerId && m.channel === 0
    );

    const result = gorcPlayerCh0WsSchema.safeParse(message);
    expect(result.success).to.be.true;
    expect(result.data?.zone_data.health).to.equal(100);
  });

  it("should handle multiple simultaneous players", async () => {
    const players = [
      { login: "ddurieux", password: "pass" },
      { login: "Hugo Lizoir", password: "pass" },
      { login: "YnotnA", password: "pass" },
    ];

    const connections = await simulatePlayers<GorcPlayerCh0WsType>(players);

    // On peut attendre un message channel 0 pour chaque joueur
    const messages = await Promise.all(
      connections.map((conn) => conn.getMessage((m) => m.channel === 0))
    );

    messages.forEach((msg, i) => {
      const result = gorcPlayerCh0WsSchema.safeParse(msg);
      expect(result.success).to.be.true;
      console.log(`✅ Player ${connections[i].login} message valid`);
    });

    // Fermer toutes les connexions
    connections.forEach((conn) => conn.ws.close());
  });
});

// it("Test with first connection", function (done) {
//   ws = new WebSocket("ws://127.0.0.1:7040");
//   const messages: string[] = [];
//   let finished: boolean = false;

//   ws.on("open", () => {
//     // connected, waiting for server messages
//   });

//   ws.on("message", (data) => {
//     messages.push(data.toString());
//   });

//   ws.on("error", (err) => {
//     if (!finished) done(err);
//   });

//   ws.on("close", () => {
//     if (!finished) {
//       done(
//         new Error(
//           `Connection closed before receiving 3 messages (received ${messages.length})`
//         )
//       );
//     }
//   });

//   // timeout 0.5s for the connect
//   setTimeout(() => {
//     step = 1;
//   }, 500);

//   // must have no messages
//   if (step === 0) {
//     expect(messages).to.have.lengthOf(0);
//   }

//   ws.on("open", function open() {
//     ws.send(
//       JSON.stringify(
//         aPlayerLoginWs().withLogin("ddurieux").withPassword("pass").build()
//       )
//     );
//   });

//   // timeout 0.5s for the recept the messages
//   setTimeout(() => {
//     step = 1;
//   }, 500);

//   expect(messages).to.have.lengthOf(3);
// });

// it('Test with second connection (capture messages from both ws and ws2)', function (done) {
//     const msgs1 = [];
//     const msgs2 = [];
//     let finished = false;

//     // Ensure both sockets are open (create ws if it's missing/not open)
//     const ensureOpen = (cb) => {
//         let opened = 0;
//         const mark = () => { if (++opened === 2) cb(); };
//         const onErr = (err) => { if (!finished) { finished = true; done(err); } };

//         if (!ws || ws.readyState !== WebSocket.OPEN) {
//             ws = new WebSocket('ws://127.0.0.1:7040');
//             ws.once('open', mark);
//             ws.once('error', onErr);
//         } else {
//             process.nextTick(mark);
//         }

//         if (!ws2 || ws2.readyState !== WebSocket.OPEN) {
//             ws2 = new WebSocket('ws://127.0.0.1:7040');
//             ws2.once('open', mark);
//             ws2.once('error', onErr);
//         } else {
//             process.nextTick(mark);
//         }
//     };

//     const cleanup = () => {
//         ws.removeListener('message', handler1);
//         ws2.removeListener('message', handler2);
//         clearTimeout(timeout);
//     };

//     const finishIfReady = () => {
//         if (finished) return;
//         if (msgs1.length === 1 && msgs2.length === 3) {
//             finished = true;
//             cleanup();
//             try {
//                 expect(msgs1).to.have.lengthOf(1);
//                 expect(msgs2).to.have.lengthOf(3);
//                 done();
//             } catch (err) {
//                 done(err);
//             }
//         }
//     };

//     const handler1 = (data) => {
//         msgs1.push(data.toString());
//         // When first message on ws arrives, ensure at least one additional message appears soon (optional)
//         if (msgs1.length === 1) {
//             setTimeout(() => {
//                 if (!finished && (msgs1.length + msgs2.length) < 2) {
//                     cleanup();
//                     finished = true;
//                     done(new Error('No new message appeared after first message on ws'));
//                 }
//             }, 300);
//         }
//         finishIfReady();
//     };

//     const handler2 = (data) => {
//         msgs2.push(data.toString());
//         finishIfReady();
//     };

//     ensureOpen(() => {
//         ws.on('message', handler1);
//         ws2.on('message', handler2);

//         // Safety timeout
//         timeout = setTimeout(() => {
//             if (!finished) {
//                 finished = true;
//                 cleanup();
//                 done(new Error(`Timeout: ws received ${msgs1.length}, ws2 received ${msgs2.length}`));
//             }
//         }, 3000);
//     });
// });

// it('close first connection, the second receive player disconnect message', function (done) {
//     let finished = false;
//     const msgs2 = [];

//     const cleanup = () => {
//         if (ws2) {
//             ws2.removeAllListeners('message');
//             ws2.removeAllListeners('error');
//             ws2.removeAllListeners('close');
//         }
//     };

//     if (!ws2 || ws2.readyState !== WebSocket.OPEN) {
//         ws2 = new WebSocket('ws://127.0.0.1:7040');
//         ws2.once('open', proceed);
//         ws2.once('error', (err) => { if (!finished) done(err); });
//     } else {
//         process.nextTick(proceed);
//     }

//     function proceed() {
//         ws2.on('message', (data) => {
//             msgs2.push(data.toString());
//             // Expecting at least one message indicating player disconnect
//             if (msgs2.some(msg => msg.includes('player_disconnect')) && !finished) {
//                 finished = true;
//                 cleanup();
//                 try {
//                     expect(msgs2.some(msg => msg.includes('player_disconnect'))).to.be.true;
//                     done();
//                 } catch (err) {
//                     done(err);
//                 }
//             }
//         });

//         ws2.on('error', (err) => { if (!finished) { finished = true; cleanup(); done(err); } });

//         ws2.on('close', () => {
//             if (!finished) {
//                 finished = true;
//                 cleanup();
//                 done(new Error('ws2 closed before receiving player disconnect message'));
//             }
//         });

//         // Close the first connection to trigger disconnect message on ws2
//         if (ws) {
//             ws.close();
//         } else {
//             if (!finished) {
//                 finished = true;
//                 cleanup();
//                 done(new Error('First connection ws was not established'));
//             }
//         }

//         // Safety timeout
//         setTimeout(() => {
//             if (!finished) {
//                 finished = true;
//                 cleanup();
//                 done(new Error(`Timeout: ws2 received ${msgs2.length} messages without player disconnect`));
//             }
//         }, 3000);
//     }
// });
