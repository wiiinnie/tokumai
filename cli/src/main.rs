use anyhow::Result;
use nym_sdk::mixnet::{self, MixnetMessageSender, Recipient, IncludedSurbs};
use scramble_shared::{Request, Response};
use std::io::{self, Write};
use std::time::{SystemTime, UNIX_EPOCH, Duration};
use tokio::time::{timeout, sleep};

#[tokio::main]
async fn main() -> Result<()> {
    println!("🔐 ScrambleAI CLI - Private AI Chat");
    println!("===================================\n");
    
    // Get server Nym address
    print!("Enter server Nym address: ");
    io::stdout().flush()?;
    let mut server_address = String::new();
    io::stdin().read_line(&mut server_address)?;
    let server_address = server_address.trim().to_string();
    
    // Ask for privacy mode
    println!("\nSelect privacy mode:");
    println!("1. 🚀 Fast Mode (5-10s response, server sees your Nym address)");
    println!("2. 🔒 Maximum Privacy (30-60s response, server NEVER sees your address)");
    print!("Choice [1/2]: ");
    io::stdout().flush()?;
    
    let mut mode_input = String::new();
    io::stdin().read_line(&mut mode_input)?;
    let use_surbs = mode_input.trim() == "2";
    
    if use_surbs {
        println!("✅ Maximum Privacy Mode enabled (using SURBs)");
        println!("⚠️  Responses will take 30-60 seconds\n");
    } else {
        println!("✅ Fast Mode enabled");
        println!("ℹ️  Server will see your Nym address (NOT your IP!)");
        println!("ℹ️  Responses will take 5-10 seconds\n");
    }
    
    println!("🚀 Initializing Nym Client...");
    let start = SystemTime::now();
    
    // Initialize Nym client
    let client = mixnet::MixnetClientBuilder::new_ephemeral()
        .build()?;
    
    println!("⏱️  Client built in {:?}", start.elapsed()?);
    
    // Connect to mixnet
    let connect_start = SystemTime::now();
    let mut client = client.connect_to_mixnet().await?;
    
    let my_address = client.nym_address().to_string();
    
    println!("⏱️  Connected to mixnet in {:?}", connect_start.elapsed()?);
    println!("✅ Connected to Nym Mixnet!");
    println!("📍 Your Nym Address: {}", my_address);
    println!("\n=== Commands ===");
    println!("ping <message>   - Test connection");
    println!("<any text>       - Send chat request to AI");
    println!("exit             - Quit");
    println!("address          - Show your Nym address");
    println!("================\n");
    
    // Parse recipient once
    let recipient = Recipient::try_from_base58_string(&server_address)?;
    
    loop {
        // Get user input
        print!("You: ");
        io::stdout().flush()?;
        
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim();
        
        if input.is_empty() {
            continue;
        }
        
        // Handle commands
        if input == "exit" {
            println!("👋 Goodbye!");
            break;
        }
        
        if input == "address" {
            println!("📍 Your Nym Address: {}", my_address);
            continue;
        }
        
        // Prepare reply_to based on mode
        let reply_to = if use_surbs {
            None // SURBs mode - no address needed
        } else {
            Some(my_address.clone()) // Fast mode - include address
        };
        
        // Check if ping
        let request = if input.starts_with("ping") {
            let message = input.strip_prefix("ping").unwrap_or("").trim();
            let message = if message.is_empty() { "hello" } else { message };
            Request::ping(message, reply_to)
        } else {
            Request::chat_groq(input, reply_to)
        };
        
        let request_bytes = serde_json::to_vec(&request)?;
        
        if use_surbs {
            println!("🔄 Sending through Mixnet with SURBs (Maximum Privacy)...");
        } else {
            println!("🔄 Sending through Mixnet (Fast Mode)...");
        }
        
        let send_start = SystemTime::now();
        
        // Send with or without SURBs
        let surbs = if use_surbs {
            IncludedSurbs::Amount(10)
        } else {
            IncludedSurbs::none()
        };
        
        client.send_message(recipient, request_bytes, surbs).await?;
        
        println!("⏱️  Sent in {:?}", send_start.elapsed()?);
        println!("⏳ Polling for response...");
        
        let wait_start = SystemTime::now();
        let max_polls = if use_surbs { 120 } else { 60 };
        let mut poll_count = 0;
        
        // Keep polling until we get a response or timeout
        let poll_result = timeout(Duration::from_secs(if use_surbs { 300 } else { 60 }), async {
            loop {
                poll_count += 1;
                
                if poll_count % 10 == 0 {
                    println!("   Polling attempt {}...", poll_count);
                }
                
                // Wait for messages
                if let Some(messages) = client.wait_for_messages().await {
                    if !messages.is_empty() {
                        return Some(messages);
                    }
                }
                
                // Small delay between polls
                sleep(Duration::from_millis(500)).await;
                
                if poll_count >= max_polls {
                    return None;
                }
            }
        }).await;
        
        match poll_result {
            Ok(Some(messages)) => {
                let receive_time = wait_start.elapsed()?;
                println!("⏱️  Response received after {:?} ({} polls)", receive_time, poll_count);
                println!("📦 Received {} message(s)", messages.len());
                
                // Process all messages
                for (i, received) in messages.iter().enumerate() {
                    println!("📨 Processing message {}/{}", i + 1, messages.len());
                    
                    match serde_json::from_slice::<Response>(&received.message) {
                        Ok(Response::Pong { original_timestamp, server_timestamp, message }) => {
                            let now = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_millis() as u64;
                            
                            let client_to_server = server_timestamp.saturating_sub(original_timestamp);
                            let server_to_client = now.saturating_sub(server_timestamp);
                            let total_roundtrip = now.saturating_sub(original_timestamp);
                            
                            println!("\n🏓 PONG: {}", message);
                            println!("📊 Timing:");
                            println!("   Client → Server: {} ms", client_to_server);
                            println!("   Server → Client: {} ms", server_to_client);
                            println!("   Total Roundtrip: {} ms ({:.2}s)\n", 
                                total_roundtrip, 
                                total_roundtrip as f64 / 1000.0
                            );
                        }
                        Ok(Response::Chat(response)) => {
                            println!("\n🤖 AI ({:?} - {}): {}\n", 
                                response.provider, 
                                response.model, 
                                response.content
                            );
                        }
                        Err(e) => {
                            eprintln!("❌ Failed to parse response: {}", e);
                        }
                    }
                }
            }
            Ok(None) => {
                eprintln!("❌ No response after {} polls", poll_count);
            }
            Err(_) => {
                eprintln!("❌ Timeout after {} polls", poll_count);
            }
        }
    }
    
    Ok(())
}
