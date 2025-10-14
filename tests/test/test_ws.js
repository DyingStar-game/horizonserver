// const WebSocket = require('ws');
// const { expect } = require('chai');

// let ws;
// let ws2;
// let step = 0;
// let messages = [];
// let player1_gorc_id = null;
// let player1_player_id = null;

// describe('WebSocket server 127.0.0.1:7040', function () {
//     this.timeout(50000);

//     after(function (done) {
//         if (ws) {
//             ws.close();
//         }
//         if (ws2) {
//             ws2.close();
//         }
//         done();
//     });

//     it('open, wait no messages, send init, then receive 3 messages', function (done) {
//         messages = [];
//         ws = new WebSocket('ws://127.0.0.1:7040');

//         let finished = false;
//         let t1, t2;

//         const cleanup = () => {
//             clearTimeout(t1);
//             clearTimeout(t2);
//             if (ws) {
//                 ws.removeAllListeners('message');
//                 ws.removeAllListeners('error');
//                 ws.removeAllListeners('close');
//             }
//         };

//         const fail = (err) => {
//             if (finished) return;
//             finished = true;
//             cleanup();
//             done(err);
//         };

//         ws.on('error', fail);
//         ws.on('close', () => {
//             if (!finished) fail(new Error('Connection closed prematurely'));
//         });

//         ws.on('message', (data) => {
//             messages.push(data.toString());
//         });

//         ws.once('open', () => {
//             // wait 1s to ensure no messages are received before init
//             t1 = setTimeout(() => {
//                 try {
//                     expect(messages).to.have.lengthOf(0);
//                 } catch (err) {
//                     return fail(err);
//                 }

//                 // send init
//                 ws.send(JSON.stringify({
//                     namespace: "player",
//                     event: "init",
//                     data: { login: "ddurieux", password: "pass" }
//                 }));

//                 // wait 1s to collect the 3 messages
//                 t2 = setTimeout(() => {
//                     try {
//                         expect(messages).to.have.lengthOf(3);
//                         // check first message is zone enter channel 0
//                         const msg1 = JSON.parse(messages[0]);
//                         checkMessagePlayerChannel0(msg1);
//                         player1_gorc_id = msg1.object_id;
//                         player1_player_id = msg1.player_id;

//                         const msg2 = JSON.parse(messages[1]);
//                         checkMessagePlayerChannel1(msg2);

//                         const msg3 = JSON.parse(messages[2]);
//                         checkMessagePlayerChannel2(msg3, "ddurieux");

//                         finished = true;
//                         cleanup();
//                         done();
//                     } catch (err) {
//                         fail(err);
//                     }
//                 }, 1000);
//             }, 1000);
//         });
//     });

//     it('create a second player', function (done) {
//         // local collectors for each socket
//         const messages1 = [];
//         const messages2 = [];
//         let finished = false;
//         let t1, t2;

//         const cleanup = () => {
//             clearTimeout(t1);
//             clearTimeout(t2);
//             if (ws) {
//                 ws.removeListener('message', handler1);
//                 ws.removeListener('error', onError1);
//                 ws.removeListener('close', onClose1);
//             }
//             if (ws2) {
//                 ws2.removeListener('message', handler2);
//                 ws2.removeListener('error', onError2);
//                 ws2.removeListener('close', onClose2);
//             }
//         };

//         const fail = (err) => {
//             if (finished) return;
//             finished = true;
//             cleanup();
//             done(err);
//         };

//         const handler1 = (data) => messages1.push(data.toString());
//         const handler2 = (data) => messages2.push(data.toString());
//         const onError1 = (err) => fail(err);
//         const onError2 = (err) => fail(err);
//         const onClose1 = () => { if (!finished) fail(new Error('ws closed prematurely')); };
//         const onClose2 = () => { if (!finished) fail(new Error('ws2 closed prematurely')); };

//         // attach handlers to existing ws (or wait for it to open)
//         const ensureWsOpen = (cb) => {
//             if (ws && ws.readyState === WebSocket.OPEN) {
//                 ws.on('message', handler1);
//                 ws.on('error', onError1);
//                 ws.on('close', onClose1);
//                 return process.nextTick(cb);
//             }
//             // create/replace ws if missing
//             if (!ws) {
//                 ws = new WebSocket('ws://127.0.0.1:7040');
//             }
//             ws.once('open', () => {
//                 ws.on('message', handler1);
//                 ws.on('error', onError1);
//                 ws.on('close', onClose1);
//                 cb();
//             });
//             ws.once('error', onError1);
//         };

//         // create ws2 and attach handlers
//         ws2 = new WebSocket('ws://127.0.0.1:7040');
//         ws2.on('message', handler2);
//         ws2.on('error', onError2);
//         ws2.on('close', onClose2);

//         // wait until both sockets are open, then start the test steps
//         let openCount = 0;
//         const markOpen = () => { if (++openCount === 2) startSteps(); };

//         ensureWsOpen(markOpen);
//         ws2.once('open', markOpen);
//         ws2.once('error', onError2);

//         function startSteps() {
//             // step 1: wait 1s to be sure we don't receive any message before init
//             t1 = setTimeout(() => {
//                 try {
//                     expect(messages1).to.have.lengthOf(0);
//                     expect(messages2).to.have.lengthOf(0);
//                 } catch (err) {
//                     return fail(err);
//                 }

//                 // step 2: send init on ws2
//                 ws2.send(JSON.stringify({
//                     namespace: "player",
//                     event: "init",
//                     data: { login: "Hugo Lizoir", password: "pass" }
//                 }));

//                 // step 3: wait 1s to collect messages
//                 t2 = setTimeout(() => {
//                     try {
//                         console.log("Messages for ws:", messages1);
//                         console.log("Messages for ws2:", messages2);
//                         // ws1 should receive 1 message about the new player
//                         // ws2 should receive 3 messages about itself + 3 messages about the first player
//                         expect(messages1).to.have.lengthOf(1);
//                         expect(messages2).to.have.lengthOf(6);
//                         finished = true;
//                         cleanup();
//                         done();
//                     } catch (err) {
//                         fail(err);
//                     }
//                 }, 5000);
//             }, 1000);
//         }
//     });

//     it('Player 1 go very far away, second player will not receive message', function (done) {
//         const msgs1 = [];
//         const msgs2 = [];
//         let finished = false;
//         let t1, t2;

//         const cleanup = () => {
//             clearTimeout(t1);
//             clearTimeout(t2);
//             if (ws) {
//                 ws.removeListener('message', handler1);
//                 ws.removeListener('error', onError1);
//                 ws.removeListener('close', onClose1);
//             }
//             if (ws2) {
//                 ws2.removeListener('message', handler2);
//                 ws2.removeListener('error', onError2);
//                 ws2.removeListener('close', onClose2);
//             }
//         };

//         const fail = (err) => {
//             if (finished) return;
//             finished = true;
//             cleanup();
//             done(err);
//         };

//         const handler1 = (data) => msgs1.push(data.toString());
//         const handler2 = (data) => msgs2.push(data.toString());
//         const onError1 = (err) => fail(err);
//         const onError2 = (err) => fail(err);
//         const onClose1 = () => { if (!finished) fail(new Error('ws closed prematurely')); };
//         const onClose2 = () => { if (!finished) fail(new Error('ws2 closed prematurely')); };

//         // ensure ws and ws2 are open
//         const waitOpen = (socket, cb, onErr) => {
//             if (socket && socket.readyState === WebSocket.OPEN) return process.nextTick(cb);
//             if (!socket) return cb(new Error('socket missing'));
//             socket.once('open', cb);
//             socket.once('error', onErr);
//         };

//         // attach handlers
//         if (ws) {
//             ws.on('message', handler1);
//             ws.on('error', onError1);
//             ws.on('close', onClose1);
//         }
//         if (ws2) {
//             ws2.on('message', handler2);
//             ws2.on('error', onError2);
//             ws2.on('close', onClose2);
//         }

//         // wait both open, then send event
//         let openCount = 0;
//         const markOpen = (err) => {
//             if (err) return fail(err);
//             if (++openCount === 2) {
//                 // send the gorc_event on ws
//                 try {
//                     ws.send(JSON.stringify({
//                         "type": "gorc_event",
//                         "object_id": "GorcObjectId(" + player1_gorc_id + ")",
//                         "channel": 0,
//                         "event": "move",
//                         "data": {
//                             "player_id": player1_player_id,
//                             "new_position": {
//                             "x": 500000.0,
//                             "y": 1.0,
//                             "z": 1.0
//                             },
//                             "velocity":{
//                             "x": 10.0,
//                             "y": 1.0,
//                             "z": 1.0
//                             },
//                             "movement_state": 1,
//                             "client_timestamp": "2025-10-11T09:57:45Z"
//                         },
//                         "player_id": player1_player_id
//                     }));
//                 } catch (err) {
//                     return fail(err);
//                 }

//                 // wait 500ms to ensure ws received a message
//                 t1 = setTimeout(() => {
//                     try {
//                         if (msgs1.length < 1) {
//                             throw new Error(`Expected ws to receive at least 1 message, got ${msgs1.length}`);
//                         }
//                     } catch (err) {
//                         return fail(err);
//                     }

//                     // wait another 500ms to be sure ws2 receives nothing
//                     t2 = setTimeout(() => {
//                         try {
//                             if (msgs2.length !== 0) {
//                                 console.log("msgs2:", msgs2);
//                                 throw new Error(`Expected ws2 to receive 0 messages, got ${msgs2.length}`);
//                             }
//                             finished = true;
//                             cleanup();
//                             done();
//                         } catch (err) {
//                             fail(err);
//                         }
//                     }, 500);
//                 }, 500);
//             }
//         };

//         waitOpen(ws, () => markOpen(), onError1);
//         waitOpen(ws2, () => markOpen(), onError2);
//     });

//     function checkMessagePlayerChannel0(msg) {
//         // {
//         //     "channel": 0,
//         //     "object_id": "bcd4ee4e-91bd-4645-bbed-70d569be2141",
//         //     "object_type": "GorcPlayer",
//         //     "player_id": "5213cfa2-5ad4-4055-9af9-4118936c225a",
//         //     "timestamp": 1760225547,
//         //     "type": "gorc_zone_enter",
//         //     "zone_data": {
//         //         "health": 100,
//         //         "position": {
//         //             "x": 0,
//         //             "y": 0,
//         //             "z": 0
//         //         },
//         //         "velocity": {
//         //             "x": 0,
//         //             "y": 0,
//         //             "z": 0
//         //         },
//         //         "name: ""
//         //     }
//         // }
//         expect(msg).to.have.all.keys(
//             'channel',
//             'object_id',
//             'object_type',
//             'player_id',
//             'timestamp',
//             'type',
//             'zone_data'
//         );
//         expect(msg).to.have.property('channel', 0);
//         expect(msg).to.have.property('object_id');
//         expect(msg).to.have.property('object_type', 'GorcPlayer');
//         expect(msg).to.have.property('player_id');
//         expect(msg).to.have.property('timestamp');
//         expect(msg).to.have.property('type', 'gorc_zone_enter');
//         expect(msg).to.have.nested.property('zone_data').that.has.all.keys(
//             'health',
//             'position',
//             'velocity',
//         );
//         expect(msg).to.have.nested.property('zone_data.health', 100);
//         expect(msg).to.have.nested.property('zone_data.position.x', 0);
//         expect(msg).to.have.nested.property('zone_data.position.y', 0);
//         expect(msg).to.have.nested.property('zone_data.position.z', 0);
//         expect(msg).to.have.nested.property('zone_data.velocity.x', 0);
//         expect(msg).to.have.nested.property('zone_data.velocity.y', 0);
//         expect(msg).to.have.nested.property('zone_data.velocity.z', 0);
//     }

//     function checkMessagePlayerChannel1(msg) {
//         // {
//         //     "channel": 1,
//         //     "object_id": "bcd4ee4e-91bd-4645-bbed-70d569be2141",
//         //     "object_type": "GorcPlayer",
//         //     "player_id": "5213cfa2-5ad4-4055-9af9-4118936c225a",
//         //     "timestamp": 1760225547,
//         //     "type": "gorc_zone_enter",
//         //     "zone_data": {
//         //         "level": 1,
//         //         "movement_state": "idle"
//         //     }
//         // }
//         expect(msg).to.have.all.keys(
//             'channel',
//             'object_id',
//             'object_type',
//             'player_id',
//             'timestamp',
//             'type',
//             'zone_data'
//         );
//         expect(msg).to.have.property('channel', 1);
//         expect(msg).to.have.property('object_id');
//         expect(msg).to.have.property('object_type', 'GorcPlayer');
//         expect(msg).to.have.property('player_id');
//         expect(msg).to.have.property('timestamp');
//         expect(msg).to.have.property('type', 'gorc_zone_enter');
//         expect(msg).to.have.nested.property('zone_data').that.has.all.keys(
//             'level',
//             'movement_state'
//         );
//         expect(msg).to.have.nested.property('zone_data.level', 1);
//         expect(msg).to.have.nested.property('zone_data.movement_state', 'idle');
//     }

//     function checkMessagePlayerChannel2(msg, pseudo) {
//         // {
//         //     "channel": 2,
//         //     "object_id": "bcd4ee4e-91bd-4645-bbed-70d569be2141",
//         //     "object_type": "GorcPlayer",
//         //     "player_id": "5213cfa2-5ad4-4055-9af9-4118936c225a",
//         //     "timestamp": 1760225547,
//         //     "type": "gorc_zone_enter",
//         //     "zone_data": {
//         //         "chat_bubble": null,
//         //         "name": "Player_5213cfa2-5ad4-4055-9af9-4118936c225a"
//         //     }
//         // }
//         expect(msg).to.have.all.keys(
//             'channel',
//             'object_id',
//             'object_type',
//             'player_id',
//             'timestamp',
//             'type',
//             'zone_data'
//         );
//         expect(msg).to.have.property('channel', 2);
//         expect(msg).to.have.property('object_id');
//         expect(msg).to.have.property('object_type', 'GorcPlayer');
//         expect(msg).to.have.property('player_id');
//         expect(msg).to.have.property('timestamp');
//         expect(msg).to.have.property('type', 'gorc_zone_enter');
//         expect(msg).to.have.nested.property('zone_data').that.has.all.keys(
//             'chat_bubble',
//             'name'
//         );
//         expect(msg).to.have.nested.property('zone_data.chat_bubble', null);
//         expect(msg).to.have.nested.property('zone_data.name', pseudo);
//     }
// });
