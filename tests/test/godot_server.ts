import { WebSocketServer } from "ws";

// Start WebSocket server (godot server simulation) on port 8980
const testServer: WebSocketServer = new WebSocketServer({ port: 8980 });

testServer.on('connection', (serverWs) => {
console.log('✅ Client connected to test godot server on port 8980');

serverWs.on('message', (data) => {
  try {
    const message = JSON.parse(data.toString());
    console.log('📨 Received message:', message);
    
    // Check if this is the expected "add_prop" message
    if (message.event === "add_prop" && 
        message.namespace === "server" && 
        message.data?.object_type === "player") {
      
      console.log('🎯 Matched add_prop message, sending response');
      
      // Send the specified response
      let response = {
        "amessagenb": 20,
        "data": [
          {
            "pos": { "x": message.data.object_data.position.x + 10, "y": 0, "z": 0 },
            "rot": { "x": 0, "y": 0, "z": 0 },
            "player_id": message.data.object_data.connection_id,
          }
        ],
        "event": "position",
        "namespace": "players"
      };
      
      serverWs.send(JSON.stringify(response));
      setTimeout(() => {
        console.log('⏰ 1 second delay completed');
      }, 1000);

      response.amessagenb = 21;
      response.data[0].pos.x += 10;
      serverWs.send(JSON.stringify(response));

    }
  } catch (error) {
    console.error('❌ Error parsing message:', error);
  }
});

serverWs.on('close', () => {
  console.log('👋 Client disconnected from test godot server');
});
});
