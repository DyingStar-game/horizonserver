use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

#[derive(Parser, Debug)]
#[command(name = "websocket-perf-test")]
#[command(about = "WebSocket performance testing tool for concurrent connections")]
struct Args {
    /// Number of concurrent clients to simulate
    #[arg(short, long, default_value = "50")]
    clients: usize,
    
    /// WebSocket server URL
    #[arg(short, long, default_value = "ws://127.0.0.1:7040")]
    url: String,
    
    /// Base name for client login (will be suffixed with client number)
    #[arg(short, long, default_value = "test_client")]
    base_name: String,
    
    /// Password for all clients
    #[arg(short, long, default_value = "pass")]
    password: String,
    
    /// Duration to keep connections alive (seconds)
    #[arg(short, long, default_value = "10")]
    duration: u64,
}

#[derive(Serialize, Deserialize, Debug)]
struct InitMessage {
    namespace: String,
    event: String,
    data: InitData,
}

#[derive(Serialize, Deserialize, Debug)]
struct InitData {
    login: String,
    password: String,
}

#[derive(Debug)]
struct ConnectionStats {
    successful_connections: AtomicUsize,
    failed_connections: AtomicUsize,
    messages_sent: AtomicUsize,
    messages_received: AtomicUsize,
}

impl ConnectionStats {
    fn new() -> Self {
        Self {
            successful_connections: AtomicUsize::new(0),
            failed_connections: AtomicUsize::new(0),
            messages_sent: AtomicUsize::new(0),
            messages_received: AtomicUsize::new(0),
        }
    }

    fn print_summary(&self) {
        println!("\n=== Connection Statistics ===");
        println!("Successful connections: {}", self.successful_connections.load(Ordering::Relaxed));
        println!("Failed connections: {}", self.failed_connections.load(Ordering::Relaxed));
        println!("Messages sent: {}", self.messages_sent.load(Ordering::Relaxed));
        println!("Messages received: {}", self.messages_received.load(Ordering::Relaxed));
    }
}

async fn create_websocket_client(
    client_id: usize,
    url: String,
    base_name: String,
    password: String,
    duration: Duration,
    stats: Arc<ConnectionStats>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let login_name = format!("{}_{}", base_name, client_id);
    
    println!("Client {}: Attempting connection to {}", client_id, url);
    
    // Connect to WebSocket
    let (ws_stream, _) = match connect_async(&url).await {
        Ok(result) => {
            stats.successful_connections.fetch_add(1, Ordering::Relaxed);
            println!("Client {}: Connected successfully", client_id);
            result
        }
        Err(e) => {
            stats.failed_connections.fetch_add(1, Ordering::Relaxed);
            eprintln!("Client {}: Failed to connect: {}", client_id, e);
            return Err(Box::new(e));
        }
    };

    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    // Create the init message
    let init_message = InitMessage {
        namespace: "player".to_string(),
        event: "init".to_string(),
        data: InitData {
            login: login_name.clone(),
            password: password.clone(),
        },
    };

    let message_json = serde_json::to_string(&init_message)?;
    println!("Client {}: Sending init message for user '{}'", client_id, login_name);

    // Send the init message
    if let Err(e) = ws_sender.send(Message::Text(message_json)).await {
        eprintln!("Client {}: Failed to send message: {}", client_id, e);
        return Err(Box::new(e));
    }
    
    stats.messages_sent.fetch_add(1, Ordering::Relaxed);
    println!("Client {}: Init message sent successfully", client_id);

    // Listen for messages and keep connection alive
    let start_time = Instant::now();
    
    while start_time.elapsed() < duration {
        tokio::select! {
            // Listen for incoming messages
            msg = ws_receiver.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        stats.messages_received.fetch_add(1, Ordering::Relaxed);
                        println!("Client {}: Received message: {}", client_id, text);
                    }
                    Some(Ok(Message::Binary(data))) => {
                        stats.messages_received.fetch_add(1, Ordering::Relaxed);
                        println!("Client {}: Received binary message ({} bytes)", client_id, data.len());
                    }
                    Some(Ok(Message::Close(_))) => {
                        println!("Client {}: Connection closed by server", client_id);
                        break;
                    }
                    Some(Err(e)) => {
                        eprintln!("Client {}: Error receiving message: {}", client_id, e);
                        break;
                    }
                    None => {
                        println!("Client {}: Connection closed", client_id);
                        break;
                    }
                    _ => {
                        // Handle other message types (ping, pong, etc.)
                    }
                }
            }
            
            // Small delay to prevent busy waiting
            _ = sleep(Duration::from_millis(100)) => {
                // Continue the loop
            }
        }
    }

    println!("Client {}: Closing connection after {} seconds", client_id, duration.as_secs());
    
    // Gracefully close the connection
    if let Err(e) = ws_sender.send(Message::Close(None)).await {
        eprintln!("Client {}: Error closing connection: {}", client_id, e);
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    
    println!("Starting WebSocket performance test");
    println!("Clients: {}", args.clients);
    println!("Server URL: {}", args.url);
    println!("Base name: {}", args.base_name);
    println!("Duration: {} seconds", args.duration);
    println!("================================\n");
    
    let stats = Arc::new(ConnectionStats::new());
    let duration = Duration::from_secs(args.duration);
    
    // Record start time for performance measurement
    let test_start = Instant::now();
    
    // Create all client tasks
    let mut tasks = Vec::with_capacity(args.clients);
    
    for client_id in 0..args.clients {
        let url = args.url.clone();
        let base_name = args.base_name.clone();
        let password = args.password.clone();
        let stats_clone = Arc::clone(&stats);
        
        let task = tokio::spawn(async move {
            if let Err(e) = create_websocket_client(
                client_id,
                url,
                base_name,
                password,
                duration,
                stats_clone,
            ).await {
                eprintln!("Client {}: Task failed: {}", client_id, e);
            }
        });
        
        tasks.push(task);
        
        // Add a small delay between connection attempts to avoid overwhelming the server
        if client_id % 10 == 9 {  // Every 10 connections
            sleep(Duration::from_millis(50)).await;
        }
        sleep(Duration::from_millis(300)).await;
    }
    
    println!("All {} client tasks started, waiting for completion...", args.clients);
    
    // Wait for all tasks to complete
    for (i, task) in tasks.into_iter().enumerate() {
        if let Err(e) = task.await {
            eprintln!("Task {} panicked: {}", i, e);
        }
    }
    
    let total_time = test_start.elapsed();
    
    println!("\n=== Test Completed ===");
    println!("Total test duration: {:.2} seconds", total_time.as_secs_f64());
    
    stats.print_summary();
    
    // Calculate some performance metrics
    let successful = stats.successful_connections.load(Ordering::Relaxed);
    let failed = stats.failed_connections.load(Ordering::Relaxed);
    let total_attempted = successful + failed;
    
    if total_attempted > 0 {
        let success_rate = (successful as f64 / total_attempted as f64) * 100.0;
        println!("Success rate: {:.1}%", success_rate);
        
        if successful > 0 {
            let avg_setup_time = total_time.as_secs_f64() / successful as f64;
            println!("Average connection setup time: {:.3} seconds", avg_setup_time);
        }
    }
    
    Ok(())
}