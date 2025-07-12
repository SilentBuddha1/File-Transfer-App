use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use axum::{
    extract::{ws::{Message, WebSocket, WebSocketUpgrade}, State},
    response::IntoResponse,
    routing::get,
    Router,
};
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::time::interval;
use futures_util::{SinkExt, StreamExt};
use uuid::Uuid;
use serde_json::Value;
use bytes::Bytes;
use tower_http::cors::CorsLayer;

#[derive(Clone)]

struct AppState {
    connections: Arc<Mutex<HashMap<String, broadcast::Sender<Message>>>>,
}

#[tokio::main]
async fn main() {
    let state = AppState {
        connections: Arc::new(Mutex::new(HashMap::new()))
    };
    let app = Router::new()
        .route("/ws", get(websocket_handler))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8000").await.unwrap();
    println!("Server running on ws://0.0.0.0:8000");
    axum::serve(listener, app).await.unwrap();
}

async fn websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState){
    let conn_id = Uuid::new_v4().to_string();
    let conn_id_clone = conn_id.clone();
    println!("New connection: {}", conn_id);

    let(tx, mut rx) = broadcast::channel(100);
    {
        let mut connections = state.connections.lock().await;
        connections.insert(conn_id_clone.clone(), tx.clone());
    }

    let (mut sender, mut receiver) = socket.split();
    let (message_tx, mut message_rx) = mpsc::channel::<Message>(100);

    let sender_task = tokio::spawn(async move {
        while let Some(message) = message_rx.recv().await {
            if sender.send(message).await.is_err() {
                break;
            }
        }
    });

    let ping_tx = message_tx.clone();
    let ping_task = tokio::spawn(async move {
        let mut interval = interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            if ping_tx.send(Message::Ping(Bytes::new())).await.is_err() {
                break;
            }
        }
    });

    let forward_tx = message_tx.clone();
        let forward_task = tokio::spawn(async move {
            while let Ok(msg) = rx.recv().await {
                if forward_tx.send(msg).await.is_err() {
                    break;
                }
            }
        });

        let state_for_receive = state.clone();
        let receive_task = tokio::spawn(async move {
            let state_clone = state_for_receive.clone();
            let tx = tx.clone();
            let mut target_map: HashMap<String, String> = HashMap::new();

            while let Some(Ok(msg)) = receiver.next().await {
                match msg {
                    Message::Text(text) => {
                        println!("Received text message: {}", text);
                        if let Ok(data) = serde_json::from_str::<Value>(&text) {
                            if data["type"] == "register" {
                                if let Some(id) = data["connectionId"].as_str() {
                                    println!("Registering connection: {}", id);
                                    state_clone.connections.lock().await.insert(id.to_string(), tx.clone());
                                }
                                continue;
                            }
                            
                            // Handle receiver-ready message
                            if data["type"] == "receiver-ready" {
                                if let Some(target_sender_id) = data["target_id"].as_str() {
                                    if let Some(sender_id) = data["senderId"].as_str() {
                                        // Map this receiver to the target sender
                                        target_map.insert(conn_id.clone(), target_sender_id.to_string());
                                        // Also create reverse mapping for the sender to find this receiver
                                        target_map.insert(target_sender_id.to_string(), sender_id.to_string());
                                        println!("Receiver {} ready for sender {}", sender_id, target_sender_id);
                                        
                                        // Notify the sender that receiver is ready
                                        if let Some(sender_tx) = state_clone.connections.lock().await.get(target_sender_id) {
                                            let ready_msg = serde_json::json!({
                                                "type": "receiver-ready",
                                                "senderId": sender_id
                                            });
                                            let _ = sender_tx.send(Message::Text(ready_msg.to_string().into()));
                                            println!("Notified sender {} that receiver is ready", target_sender_id);
                                        }
                                    }
                                }
                                continue;
                            }
                            
                            // Route messages to target
                            if let Some(target_id) = data["target_id"].as_str(){
                                println!("Routing message to target: {}", target_id);
                                target_map.insert(conn_id.clone(), target_id.to_string());
                                if let Some(target_tx) =  state_clone.connections.lock().await.get(target_id) {
                                    let _ = target_tx.send(Message::Text(text));
                                    println!("Message sent to target: {}", target_id);
                                } else {
                                    println!("Target connection not found: {}", target_id);
                                }
                            }
                        }
                    }
                    Message::Binary(bin_data) => {
                        println!("Received binary data: {} bytes", bin_data.len());
                        if let Some(target_id) = target_map.get(&conn_id){
                            if let Some(target_tx) =  state_clone.connections.lock().await.get(target_id) {
                                let _ = target_tx.send(Message::Binary(bin_data));
                                println!("Binary data forwarded to: {}", target_id);
                            } else {
                                println!("No target connection found for binary data: {}", target_id);
                            }
                        } else {
                            println!("No target mapping found for connection: {}", conn_id);
                        }
                    }
                    Message::Close(_) => break,
                    _ => continue,
                }
            }
        });

        tokio::select! {
            _ = sender_task => {},
            _ = ping_task => {},
            _ = forward_task => {},
            _ = receive_task => {},
        };

        state.connections.lock().await.remove(&conn_id_clone);
        println!("Connection {} closed", conn_id_clone);

}