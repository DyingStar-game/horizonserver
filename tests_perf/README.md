# WebSocket Performance Test

A Rust-based performance testing tool for simulating multiple concurrent WebSocket connections to test server capacity and performance.

## Features

- **Concurrent Connections**: Supports 50-1000+ concurrent WebSocket connections using async/await
- **Configurable Parameters**: Customizable number of clients, server URL, client names, and test duration
- **Real-time Statistics**: Tracks successful/failed connections, messages sent/received
- **Unique Client Names**: Automatically generates unique login names for each client
- **Graceful Connection Management**: Properly handles connection lifecycle and cleanup
- **Performance Metrics**: Provides detailed performance statistics and success rates

## Usage

### Basic Usage

```bash
# Test with 50 clients (default)
cargo run

# Test with 100 clients
cargo run -- --clients 100

# Test with custom server and 200 clients
cargo run -- --clients 200 --url ws://127.0.0.1:8080

# Test with custom duration (30 seconds)
cargo run -- --clients 100 --duration 30
```

### Command Line Options

- `--clients` (`-c`): Number of concurrent clients (default: 50)
- `--url` (`-u`): WebSocket server URL (default: ws://127.0.0.1:7040)
- `--base-name` (`-b`): Base name for client login (default: test_client)
- `--password` (`-p`): Password for all clients (default: pass)
- `--duration` (`-d`): Duration to keep connections alive in seconds (default: 10)

### Example Commands

```bash
# Stress test with 500 clients for 60 seconds
cargo run -- --clients 500 --duration 60 --base-name stress_test

# Quick test with 10 clients
cargo run -- --clients 10 --duration 5

# Test with custom credentials
cargo run -- --clients 100 --base-name player --password secret123
```

## Message Format

Each client sends an initialization message in the following JSON format:

```json
{
    "namespace": "player",
    "event": "init", 
    "data": {
        "login": "test_client_0",
        "password": "pass"
    }
}
```

The login name is automatically generated as `{base_name}_{client_id}` to ensure uniqueness.

## Performance Characteristics

- **Memory Efficient**: Uses async/await instead of threads for better memory usage with many connections
- **Controlled Connection Rate**: Adds small delays between connection batches to avoid overwhelming the server
- **Graceful Shutdown**: Properly closes all connections at the end of the test
- **Error Handling**: Robust error handling with detailed logging

## Building

```bash
# Build in debug mode
cargo build

# Build optimized for performance testing
cargo build --release

# Run with release optimizations
cargo run --release -- --clients 1000
```

## Output Example

```
Starting WebSocket performance test
Clients: 100
Server URL: ws://127.0.0.1:7040
Base name: test_client
Duration: 10 seconds
================================

Client 0: Attempting connection to ws://127.0.0.1:7040
Client 1: Attempting connection to ws://127.0.0.1:7040
...
Client 0: Connected successfully
Client 0: Sending init message for user 'test_client_0'
Client 0: Init message sent successfully
...

=== Test Completed ===
Total test duration: 12.34 seconds

=== Connection Statistics ===
Successful connections: 98
Failed connections: 2
Messages sent: 98
Messages received: 196
Success rate: 98.0%
Average connection setup time: 0.126 seconds
```

## Dependencies

- `tokio`: Async runtime for handling concurrent connections
- `tokio-tungstenite`: WebSocket client implementation
- `serde` & `serde_json`: JSON serialization/deserialization
- `clap`: Command line argument parsing
- `futures-util`: Utilities for async programming