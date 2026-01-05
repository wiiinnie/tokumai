use anyhow::Result;
use nym_sdk::mixnet::{self, MixnetMessageSender, Recipient, IncludedSurbs};
use scramble_shared::directory::{DirectoryRequest, DirectoryResponse};
use scramble_shared::{Request, Response};
use std::io::{self, Write};
use std::time::{SystemTime, UNIX_EPOCH, Duration};
use tokio::time::{timeout, sleep};

#[tokio::main]
async fn main() -> Result<()> {
    println!("🔐 ScrambleAI CLI - Private AI Chat");
    println!("===================================\n");
    
    // Get directory address from user
    print!("Enter Directory Nym address: ");
    io::stdout().flush()?;
    let mut directory_address = String::new();
    io::stdin().read_line(&mut directory_address)?;
    let directory_address = directory_address.trim().to_string();
    
    if directory_address.is_empty() {
        eprintln!("❌ Directory address required!");
        return Ok(());
    }
    
    println!("✅ Will use directory: {}\n", directory_address);
    
    // Ask for privacy mode
    println!("Select privacy mode:");
    println!("1. 🚀 Fast Mode (5-10s response, server sees your Nym address)");
    println!("2. 🔒 Maximum Privacy (30-60s response, server NEVER sees your address)");
    print!("Choice [1/2]: ");
    io::stdout().flush()?;
    
    let mut mode_input = String::new();
    io::stdin().read_line(&mut mode_input)?;
    let use_surbs = mode_input.trim() == "2";
    
    if use_surbs {
        println!("✅ Maximum Privacy Mode enabled\n");
    } else {
        println!("✅ Fast Mode enabled\n");
    }
    
    println!("🚀 Initializing Nym Client...");
    let start = SystemTime::now();
    
    let client = mixnet::MixnetClientBuilder::new_ephemeral()
        .build()?;
    
    println!("⏱️  Client built in {:?}", start.elapsed()?);
    
    let connect_start = SystemTime::now();
    let mut client = client.connect_to_mixnet().await?;
    
    let my_address = client.nym_address().to_string();
    
    println!("⏱️  Connected in {:?}", connect_start.elapsed()?);
    println!("✅ Connected!");
    println!("📍 Your Nym Address: {}", my_address);
    
    println!("\n🔍 Looking for available servers...");
    let servers = fetch_server_list(&mut client, &my_address, &directory_address).await?;
    
    if servers.is_empty() {
        eprintln!("❌ No servers available!");
        eprintln!("   Make sure at least one scramble-server is running.");
        return Ok(());
    }
    
    println!("✅ Found {} server(s):", servers.len());
    for (i, server) in servers.iter().enumerate() {
        println!("   {}. {}", i + 1, server.address);
    }
    
    let server_address = &servers[0].address;
    println!("\n🎯 Using server: {}", server_address);
    
    println!("\n=== Commands ===");
    println!("ping <message>   - Test connection");
    println!("<any text>       - Send chat request to AI");
    println!("exit             - Quit");
    println!("address          - Show your Nym address");
    println!("================\n");
    
    let recipient = Recipient::try_from_base58_string(server_address)?;
    
    loop {
        print!("You: ");
        io::stdout().flush()?;
        
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim();
        
        if input.is_empty() {
            continue;
        }
        
        if input == "exit" {
            println!("👋 Goodbye!");
            break;
        }
        
        if input == "address" {
            println!("📍 Your Nym Address: {}", my_address);
            continue;
        }
        
        let reply_to = if use_surbs {
            None
        } else {
            Some(my_address.clone())
        };
        
        let request = if input.starts_with("ping") {
            let message = input.strip_prefix("ping").unwrap_or("").trim();
            let message = if message.is_empty() { "hello" } else { message };
            Request::ping(message, reply_to)
        } else {
            Request::chat_groq(input, reply_to)
        };
        
        let request_bytes = serde_json::to_vec(&request)?;
        
        if use_surbs {
            println!("🔄 Sending through Mixnet with SURBs...");
        } else {
            println!("🔄 Sending through Mixnet...");
        }
        
        let send_start = SystemTime::now();
        
        let surbs = if use_surbs {
            IncludedSurbs::Amount(10)
        } else {
            IncludedSurbs::none()
        };
        
        client.send_message(recipient, request_bytes, surbs).await?;
        
        println!("⏱️  Sent in {:?}", send_start.elapsed()?);
        println!("⏳ Waiting for response...");
        
        let wait_start = SystemTime::now();
        let max_polls = if use_surbs { 120 } else { 60 };
        let mut poll_count = 0;
        
        let poll_result = timeout(Duration::from_secs(if use_surbs { 300 } else { 60 }), async {
            loop {
                poll_count += 1;
                
                if poll_count % 10 == 0 {
                    println!("   Polling attempt {}...", poll_count);
                }
                
                if let Some(messages) = client.wait_for_messages().await {
                    if !messages.is_empty() {
                        return Some(messages);
                    }
                }
                
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
                
                for received in messages.iter() {
                    match serde_json::from_slice::<Response>(&received.message) {
                        Ok(Response::Pong { original_timestamp, server_timestamp: _, message }) => {
                            let now = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_millis() as u64;
                            
                            let total_roundtrip = now.saturating_sub(original_timestamp);
                            
                            println!("\n🏓 PONG: {}", message);
                            println!("📊 Roundtrip: {} ms ({:.2}s)\n", 
                                total_roundtrip, 
                                total_roundtrip as f64 / 1000.0
                            );
                        }
                        Ok(Response::Chat(response)) => {
                            println!("\n🤖 AI: {}\n", response.content);
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
                eprintln!("❌ Timeout");
            }
        }
    }
    
    Ok(())
}

async fn fetch_server_list(
    nym_client: &mut nym_sdk::mixnet::MixnetClient,
    my_address: &str,
    directory_address: &str,
) -> Result<Vec<scramble_shared::directory::ServerEntry>> {
    println!("   Trying directory...");
    
    fetch_from_directory(directory_address, nym_client, my_address).await
}

async fn fetch_from_directory(
    dir_address: &str,
    nym_client: &mut nym_sdk::mixnet::MixnetClient,
    my_address: &str,
) -> Result<Vec<scramble_shared::directory::ServerEntry>> {
    let recipient = Recipient::try_from_base58_string(dir_address)?;
    
    let request = DirectoryRequest::ListServers {
        reply_to: Some(my_address.to_string()),
    };
    
    let request_bytes = serde_json::to_vec(&request)?;
    
    println!("   📤 Sending request...");
    nym_client.send_message(recipient, request_bytes, IncludedSurbs::Amount(5)).await?;
    
    println!("   ⏳ Waiting for response (60s timeout)...");
    
    let result = timeout(Duration::from_secs(60), async {
        let mut poll_count = 0;
        loop {
            poll_count += 1;
            
            if poll_count % 10 == 0 {
                println!("      Polling attempt {}...", poll_count);
            }
            
            if let Some(messages) = nym_client.wait_for_messages().await {
                for msg in messages {
                    if let Ok(response) = serde_json::from_slice::<DirectoryResponse>(&msg.message) {
                        if let DirectoryResponse::ServerList { servers } = response {
                            return Ok(servers);
                        }
                    }
                }
            }
            sleep(Duration::from_millis(500)).await;
        }
    }).await;
    
    match result {
        Ok(Ok(servers)) => Ok(servers),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(anyhow::anyhow!("Timeout waiting for directory response")),
    }
}
