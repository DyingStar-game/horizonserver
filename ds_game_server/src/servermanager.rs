use horizon_event_system::{
    ServerContext, context, Vec3, utils, EventError, PlayerId,
};
use tokio_tungstenite::tungstenite::{client, handshake::server};
use std::sync::Arc;
use tracing::{info, error, debug, warn};
use crate::server::{Zone, Server, ServerState};
use tokio::{runtime, sync::mpsc};
use std::collections::HashMap;

#[derive(Clone)]
pub struct ServerInfo {
    pub uuid: String,
    pub fps: u8,
    pub objects_number: u32,
    pub players_number: u16,
    pub scenes_number: u32,
    pub players_positions: HashMap<String, Vec3>,
    pub server_name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServerSplitState {
    fps: u8,
    players: u16,
    number_times_failed: u8,
}

pub struct ServerManager {
    servers: Vec<Server>,
}

impl ServerManager {
    pub fn new() -> Self {
        ServerManager {
            servers: Vec::new(),
        }
    }

    pub fn add_server(&mut self, server: &mut Server) {
        self.servers.push(server.clone());
        info!("Server added. Total servers: {}", self.servers.len());
    }

    pub fn remove_server(&mut self, server_address: String) {
        self.servers.retain(|s| s.address != server_address);
        info!("Server removed. Total servers: {}", self.servers.len());
    }

    // pub fn server_tooheavy(&mut self, server_split: Server) {
    //     warn!("Server {} is too heavy!", server_split.address);

    //     match self.servers.iter_mut().find(|s| s.state == ServerState::Online) {
    //         Some(server) => {
    //             // info!("Found an online server: address={}", server.address);
    //             // TODO start server and give info
    //             let zone = Zone {
    //                 min_x: 0.0,
    //                 max_x: 100.0,
    //                 min_y: 0.0,
    //                 max_y: 100.0,
    //                 min_z: 0.0,
    //                 max_z: 100.0,
    //             };
    //             self.start_server(server, zone);
    //             // TODO if number of servers in state Online is less than 3, start a new pool of servers (3 for example) for the future
    //         }
    //         None => {
    //             error!("No online server found.");
    //         }
    //     }
    // }

    fn start_server(&mut self, server: &mut Server, zone: Zone, context: Arc<dyn ServerContext>, srvinfo_tx: mpsc::Sender<ServerInfo>) {
        debug!("Starting server {}", server.address);
    
        // TODO
        // get items in a zone from gorc.
        // get all properties stored in genericprops plugin (how??)
    
        // TODO add the zone + items
        server.start(zone, context.clone());
    
        // the server is connected
    
        let address = server.address.clone();
        let mut server_for_spawn = server.clone();
        server_for_spawn.receive_messages(context.clone(), srvinfo_tx);
        debug!("Server {} started.", address);
        server.state = ServerState::Running;
        self.add_server(server);
    }

    pub async fn run(
        mut self,
        context: Arc<dyn ServerContext>,
    ) {
        debug!("STEP1: ServerManager is running.");

        // load servers list in config
        let config = ds_common::config::Config::new("ds_game_server");
        debug!("STEP2: Config loaded");
        let servers: Vec<String> = config.get_vector("game_servers");
        let split_rule: String = config.get_value("split_rule")
            .and_then(|v| v.as_str())
            .unwrap_or("fps:20")
            .to_string();

        match config.get_value("servers_mode")
            .and_then(|v| v.as_str()) {
            Some("single") => self.mode_single(servers, context.clone()).await,
            Some("development") => self.mode_development(servers, context.clone(), split_rule).await,
            Some("production") => self.mode_production().await,
            _ => {
                warn!("Unknown servers_mode in configuration, defaulting to 'single'");
                self.mode_single(servers, context.clone()).await;
            }
        }
    }

    async fn mode_single(mut self, servers: Vec<String>, context: Arc<dyn ServerContext>) {
        info!("ServerManager running in single server mode.");

        let (srvinfo_tx, mut srvinfo_rx) = mpsc::channel::<ServerInfo>(100);
        
        // Spawn a task to drain the serverinfo channel in single mode
        // This prevents the channel from filling up and blocking the message processor
        let tokio_handle = context.tokio_handle();
        tokio_handle.spawn(async move {
            while let Some(serverinfo) = srvinfo_rx.recv().await {
                debug!("Single mode: Received server info: UUID={}, FPS={}, Players={}", 
                    serverinfo.uuid, serverinfo.fps, serverinfo.players_number);
            }
        });

        debug!("STEP3: Servers list loaded from configuration file: {:?}", servers);
        let firstaddress = servers.first().unwrap();
        debug!("STEP4: First server address: {}", firstaddress);

        let myzone = Zone {
            min_x: -900000000000.0,
            max_x: 900000000000.0,
            min_y: -900000000000.0,
            max_y: 900000000000.0,
            min_z: -900000000000.0,
            max_z: 900000000000.0,
        };
        let mut myserver = Server::new(
            firstaddress.to_string(),
            myzone.clone(),
        );
        loop {
            let address = myserver.clone().address.clone();
            match myserver.connect() {
                Ok(_) => {
                    debug!("[ServerManager] Websocket connect() succeeded for {}", address.clone());
                    let context_handlers = context.clone();
                    let _ = myserver.register_handlers(context_handlers).await;

                    // Connection is OK, now we start it
                    let context = context.clone();
                    self.start_server(&mut myserver, myzone.clone(), context.clone(), srvinfo_tx.clone());
                }
                Err(e) => error!("[ServerManager] Websocket connect() FAILED for {}: {:?}", address, e),
            }
            std::thread::sleep(std::time::Duration::from_secs(10));
            // We are here because server not connected or disconnected
            while myserver.state != ServerState::Offline {
                std::thread::sleep(std::time::Duration::from_secs(10));
            }
            debug!("[ServerManager] Server {} is not connected. Retrying...", firstaddress);
        }
    }

    async fn mode_development(mut self, servers: Vec<String>, context: Arc<dyn ServerContext>, split_rule: String) {
        info!("ServerManager running in development mode.");

        let mut manage_servers: Vec<Server> = Vec::new();
        let (srvinfo_tx, mut srvinfo_rx) = mpsc::channel::<ServerInfo>(1000);

        // Map to store uuid -> ServerSplitState
        let mut split_states: HashMap<String, ServerSplitState> = HashMap::new();

        // connect all servers
        debug!("we start all servers in pool...");
        let default_zone = Zone {
            min_x: -900000000000.0,
            max_x: 900000000000.0,
            min_y: -900000000000.0,
            max_y: 900000000000.0,
            min_z: -900000000000.0,
            max_z: 900000000000.0,
        };
        for server in servers.clone().iter_mut() {
            let mut myserver = Server::new(
                server.to_string(),
                default_zone.clone(),
            );
            let split_state = ServerSplitState {
                fps: 30,
                players: 0,
                number_times_failed: 0,
            };
            split_states.insert(myserver.clone().uuid.clone(), split_state);
            myserver.connect().unwrap();
            manage_servers.push(myserver);
        }
        debug!("This is the number of servers in pool: {:?}", manage_servers.len());

        // start the first server
        if let Some(first_online_server) = manage_servers.iter_mut().find(|s| s.state == ServerState::Online) {
            let context_handlers = context.clone();
            let _ = first_online_server.register_handlers(context_handlers).await;
            self.start_server(first_online_server, default_zone.clone(), context.clone(), srvinfo_tx.clone());

        } else {
            error!("No online server found in the pool.");
        }

        loop {
            // need run each second
            info!("ServerManager loop tick...");

            // get serverinfo of servers
            while let Some(serverinfo) = srvinfo_rx.recv().await {
                // info!("Received server info: UUID={}, FPS={}, Players={}", serverinfo.uuid, serverinfo.fps, serverinfo.players_number);
                // info!("Current split states: {:?}", split_states.clone());

                // send servers info to clients
                let server_zone = manage_servers.iter()
                    .find(|s| s.uuid == serverinfo.uuid)
                    .map(|s| s.zone.read().unwrap().clone());
                self.send_servers_info_to_clients(context.clone(), serverinfo.clone(), server_zone, Some(&manage_servers)).await;


                // uuid = server uuid
                // for players for example
                if let Some(split_state) = split_states.get_mut(&serverinfo.uuid) {
                    if serverinfo.players_number > 100 {
                        split_state.number_times_failed += 1;
                        // info!("Failed++");
                    } else {
                        split_state.number_times_failed = 0;
                        // info!("failed reset");
                    }
                    split_state.fps = serverinfo.fps;
                    split_state.players = serverinfo.players_number;
                    // info!("number failed: {}", split_state.number_times_failed);

                    if split_state.number_times_failed >= 20 {
                        info!("Server {} has too heavy, split to another server.", serverinfo.uuid);
                        split_state.number_times_failed = 0;

                        // Find indices first to avoid double mutable borrow
                        let server_to_split_idx = manage_servers.iter().position(|s| s.uuid == serverinfo.uuid);
                        let available_server_idx = manage_servers.iter().position(|s| s.state == ServerState::Online);

                        if server_to_split_idx == available_server_idx {
                            error!("The only available online server is the one to split. Cannot proceed with splitting.");
                        }

                        if let (Some(server_to_split_idx), Some(available_server_idx)) = (server_to_split_idx, available_server_idx) {
                            // Now borrow mutably one at a time
                            let (zone1, zone2);
                            {
                                let server_to_split = &manage_servers[server_to_split_idx];
                                // split the server zone into 2 zones
                                let (z1, z2) = ServerManager::split_zone(
                                    server_to_split.zone.read().unwrap().clone(),
                                    context.clone(),
                                );
                                zone1 = z1;
                                zone2 = z2;
                            }


                            {
                                let split_server = &mut manage_servers[server_to_split_idx];
                                split_server.update_zone(zone1.clone());
                            }                            

                            {
                                let available_server = &mut manage_servers[available_server_idx];
                                debug!("Found an available online server: {}", available_server.address);

                                let context_handlers = context.clone();
                                let _ = available_server.register_handlers(context_handlers).await;
                                *available_server.zone.write().unwrap() = zone2.clone();
                                self.start_server(available_server, zone2.clone(), context.clone(), srvinfo_tx.clone());
                            }

                            // think in progress
                            //   send event to genericprops
                            let events = context.events();
                            let available_server = &manage_servers[available_server_idx];
                            let server_to_split = &manage_servers[server_to_split_idx];
                            events.emit_plugin(
                                "genericprops",
                                "get_objects_on_zone",
                                &serde_json::json!({
                                    "server_uuid": available_server.uuid,
                                    "split_server_uuid": server_to_split.uuid,
                                    "zone": {
                                        "min_x": zone2.min_x,
                                        "max_x": zone2.max_x,
                                        "min_y": zone2.min_y,
                                        "max_y": zone2.max_y,
                                        "min_z": zone2.min_z,
                                        "max_z": zone2.max_z,
                                    }
                                }),
                            ).await.unwrap();

                            //   generic props send items to servermanager
                            //   server identified itself in messagesend items to godot server (tokio.spawn)
                            //   server say to servermanager : I'm ready, I'm online
                            //   ...




                            // use instance.get_object_positions() to get all players positions

                            // get items in the new zone (position)

                            // send them to the new server, server will listen for these items and update them

                            // when all loaded by new server, send new zone to the heavy server
                            // send to new server gogogo

                        } else {
                            if server_to_split_idx.is_none() {
                                warn!("Server with UUID {} not found in manage_servers.", serverinfo.uuid);
                            } else {
                                warn!("No available online server found for splitting.");
                            }
                        }



                    }
                }
            }
        }
    }

    async fn mode_production(mut self) {
        info!("ServerManager running in production mode.");

    }

    pub fn split_zone(zone: Zone, context: Arc<dyn ServerContext>) -> (Zone, Zone) {

        // Get GORC instances from context
        let gorc_instances = context.events().get_gorc_instances();
        info!("Initial zone to split: {:?}", zone);
        // Get GORC objects and filter players with global_position in zone
        let players_positions: Vec<Vec3> = if let Some(gorc) = &gorc_instances {
            // Use blocking approach to get object positions from GORC
            let gorc_clone = gorc.clone();
            let zone_clone = zone.clone();
            let tokio_handle = context.tokio_handle();
            tokio_handle.block_on(async {
                let mut objects_in_zone = Vec::new();
                    
                // Get all player objects from GORC
                let player_object_ids = gorc_clone.get_objects_by_type("player").await;
                info!("Total player objects in GORC: {}", player_object_ids.len());
                for object_id in player_object_ids {
                    if let Some(global_position) = gorc_clone.get_object_position(object_id).await {
                        // Check if global_position is within the zone
                        info!("Checking player object {:?} at position {:?}", object_id, global_position);
                        if global_position.x >= zone_clone.min_x && global_position.x <= zone_clone.max_x &&
                            global_position.y >= zone_clone.min_y && global_position.y <= zone_clone.max_y &&
                            global_position.z >= zone_clone.min_z && global_position.z <= zone_clone.max_z {
                            objects_in_zone.push(global_position);
                        }
                    }
                }
                
                objects_in_zone
            })
        } else {
            Vec::new()
        };
        info!("Players positions in zone: {:?}", players_positions);

        // Calculate max distance between min and max player coordinates on each axis
        let distance_x = if players_positions.is_empty() {
            0.0
        } else {
            let min_x = players_positions.iter().map(|pos| pos.x).fold(f64::INFINITY, f64::min);
            let max_x = players_positions.iter().map(|pos| pos.x).fold(f64::NEG_INFINITY, f64::max);
            (max_x - min_x).abs()
        };
        let distance_y = if players_positions.is_empty() {
            0.0
        } else {
            let min_y = players_positions.iter().map(|pos| pos.y).fold(f64::INFINITY, f64::min);
            let max_y = players_positions.iter().map(|pos| pos.y).fold(f64::NEG_INFINITY, f64::max);
            (max_y - min_y).abs()
        };
        let distance_z = if players_positions.is_empty() {
            0.0
        } else {
            let min_z = players_positions.iter().map(|pos| pos.z).fold(f64::INFINITY, f64::min);
            let max_z = players_positions.iter().map(|pos| pos.z).fold(f64::NEG_INFINITY, f64::max);
            (max_z - min_z).abs()
        };
        info!("Distance X: {}, Y: {}, Z: {}", distance_x, distance_y, distance_z);
        // Determine which axis has the greatest distance and split along that axis
        let (zone1, zone2) = if distance_x >= distance_y && distance_x >= distance_z {
            // Split along X axis
            let mid_x = if players_positions.len() <= 1 {
                ((zone.min_x + zone.max_x) / 2.0 * 1000.0).round() / 1000.0
            } else {
                let mut xs: Vec<f64> = players_positions.iter().map(|pos| pos.x).collect();
                xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let mid = xs.len() / 2;
                ((xs[mid - 1] + xs[mid]) / 2.0 * 1000.0).round() / 1000.0
            };
            (
                Zone {
                    min_x: zone.min_x,
                    max_x: mid_x,
                    min_y: zone.min_y,
                    max_y: zone.max_y,
                    min_z: zone.min_z,
                    max_z: zone.max_z,
                },
                Zone {
                    min_x: mid_x + 0.001,
                    max_x: zone.max_x,
                    min_y: zone.min_y,
                    max_y: zone.max_y,
                    min_z: zone.min_z,
                    max_z: zone.max_z,
                }
            )
        } else if distance_y >= distance_x && distance_y >= distance_z {
            // Split along Y axis
            let mid_y = if players_positions.len() <= 1 {
                ((zone.min_y + zone.max_y) / 2.0 * 1000.0).round() / 1000.0
            } else {
                let mut ys: Vec<f64> = players_positions.iter().map(|pos| pos.y).collect();
                ys.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let mid = ys.len() / 2;
                ((ys[mid - 1] + ys[mid]) / 2.0 * 1000.0).round() / 1000.0
            };
            (
                Zone {
                    min_x: zone.min_x,
                    max_x: zone.max_x,
                    min_y: zone.min_y,
                    max_y: mid_y,
                    min_z: zone.min_z,
                    max_z: zone.max_z,
                },
                Zone {
                    min_x: zone.min_x,
                    max_x: zone.max_x,
                    min_y: mid_y + 0.001,
                    max_y: zone.max_y,
                    min_z: zone.min_z,
                    max_z: zone.max_z,
                }
            )
        } else {
            // Split along Z axis
            let mid_z = if players_positions.len() <= 1 {
                ((zone.min_z + zone.max_z) / 2.0 * 1000.0).round() / 1000.0
            } else {
                let mut zs: Vec<f64> = players_positions.iter().map(|pos| pos.z).collect();
                zs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let mid = zs.len() / 2;
                ((zs[mid - 1] + zs[mid]) / 2.0 * 1000.0).round() / 1000.0
            };
            (
                Zone {
                    min_x: zone.min_x,
                    max_x: zone.max_x,
                    min_y: zone.min_y,
                    max_y: zone.max_y,
                    min_z: zone.min_z,
                    max_z: mid_z,
                },
                Zone {
                    min_x: zone.min_x,
                    max_x: zone.max_x,
                    min_y: zone.min_y,
                    max_y: zone.max_y,
                    min_z: mid_z + 0.001,
                    max_z: zone.max_z,
                }
            )
        };

        (zone1, zone2)
    }

    async fn send_servers_info_to_clients(
        &mut self,
        context: Arc<dyn ServerContext>,
        serverinfo: ServerInfo,
        server_zone: Option<Zone>,
        servers_source: Option<&Vec<Server>>,
    ) {

        // Get the client_response_sender from the context's emitters
        let events = context.events();
        let sender = events.get_client_response_sender();

        // Use provided servers_source or fall back to self.servers
        let servers_to_use = servers_source.unwrap_or(&self.servers);

        let event = serde_json::json!({
            "godotserver": {
                "uuid": serverinfo.uuid,
                "fps": serverinfo.fps,
                "objects_number": serverinfo.objects_number,
                "players_number": serverinfo.players_number,
                "scenes_number": serverinfo.scenes_number,
                "zone": {
                    "min_x": server_zone.as_ref().map_or(0.0, |z| z.min_x),
                    "max_x": server_zone.as_ref().map_or(0.0, |z| z.max_x),
                    "min_y": server_zone.as_ref().map_or(0.0, |z| z.min_y),
                    "max_y": server_zone.as_ref().map_or(0.0, |z| z.max_y),
                    "min_z": server_zone.as_ref().map_or(0.0, |z| z.min_z),
                    "max_z": server_zone.as_ref().map_or(0.0, |z| z.max_z),
                },
                "name": serverinfo.server_name,
            },
            "universe": {
                "players_number": servers_to_use.iter()
                    .map(|s| {
                        let players = s.managed_players.lock().unwrap();
                        players.len() as u32
                    })
                    .sum::<u32>(),
                "godotservers_number": servers_to_use.iter().filter(|s| s.state == ServerState::Running).count(),
            }
        });

        let mut client_event = serde_json::json!({
            "event_type": "update_property",
            "object_id": serverinfo.uuid, // server uuid
            "object_type": "serverinfo",
            "channel": 0,
            "player_id": "".to_string(),
            "data": event,
            "timestamp": utils::current_timestamp()
        });

        let handler = context.tokio_handle();
        let serverinfo = serverinfo.clone();

        // Collect player IDs before spawning the async task to avoid holding the MutexGuard across await
        let player_ids: Option<Vec<String>> = servers_to_use.iter()
            .find(|s| s.uuid == serverinfo.uuid)
            .map(|server| {
                server.managed_players.lock().unwrap().clone()
            });

        handler.spawn(async move {
            if let Some(player_ids) = player_ids {
                for player_id in player_ids.iter() {
                    client_event["player_id"] = player_id.clone().into();

                    // Serialize the event data
                    let data = serde_json::to_vec(&client_event).unwrap_or_default();

                    if let Some(sender_arc) = &sender {
                        let obj_player_id = PlayerId::from_str(
                            player_id.as_str()
                        ).unwrap_or_else(|_| PlayerId::new());
                        if let Err(e) = sender_arc.send_to_client(obj_player_id, data.clone()).await {
                            warn!("Failed to send GORC event to player {}: {}", player_id, e);
                        }
                    } else {
                        warn!("No client response sender available to send event to player {}", player_id);
                    }
                }
            }
        });

    }

}