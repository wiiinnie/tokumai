# ScrambleAI

Privacy-first AI chat application using the Nym mixnet for anonymous communication.

## Features

- 🔐 **Anonymous Communication**: All traffic routed through Nym mixnet
- 🚀 **Two Privacy Modes**:
  - Fast Mode (5-10s): Server sees ephemeral Nym address (NOT your IP)
  - Maximum Privacy Mode (30-60s): Server never sees any address (uses SURBs)
- 🤖 **Multiple AI Providers**: Groq, OpenAI, Anthropic
- 🎯 **Zero IP Exposure**: AI providers can never track you

## Architecture
```
┌──────────┐                ┌──────────────┐                ┌─────────┐
│  Client  │───Nym Mixnet──▶│    Server    │───HTTPS───────▶│  Groq   │
│  (CLI)   │◀──Nym Mixnet───│  (Rust)      │◀──Response────│   API   │
└──────────┘                └──────────────┘                └─────────┘
```

## Project Structure
```
scramble-ai/
├── cli/           # Client CLI binary (macOS/Linux)
├── server/        # Server binary (Linux)
├── shared/        # Shared types and data structures
├── Cargo.toml     # Workspace configuration
└── .env.example   # Example environment configuration
```

## Prerequisites

- Rust 1.81+ (`rustup install stable`)
- Nym SDK (automatically fetched via Cargo)
- API keys for AI providers (Groq, OpenAI, or Anthropic)

## Building

### CLI (macOS/Linux)
```bash
cargo build --release --bin scramble-cli
# Binary: target/release/scramble-cli
```

### Server (Linux)
```bash
# Native build
cargo build --release --bin scramble-server

# Cross-compile from macOS
cargo install cross
cross build --release --target x86_64-unknown-linux-gnu --bin scramble-server
```

## Configuration

Create `.env` file in project root:
```bash
cp .env.example .env
```

Add your API keys:
```
GROQ_API_KEY=your_groq_api_key
OPENAI_API_KEY=your_openai_api_key
ANTHROPIC_API_KEY=your_anthropic_api_key
```

## Usage

### 1. Start Server
```bash
./target/release/scramble-server
```

Note the Nym address displayed (e.g., `C7abc...@gateway.nymtech.net`)

### 2. Start Client
```bash
./target/release/scramble-cli
```

Enter the server's Nym address when prompted.

Choose privacy mode:
- **Fast Mode (1)**: 5-10 second responses
- **Maximum Privacy (2)**: 30-60 second responses

### 3. Chat
```bash
You: Tell me a joke
🤖 AI (Groq - llama-3.3-70b-versatile): Why don't scientists trust atoms?
Because they make up everything!

You: ping test
🏓 PONG: pong: test
```

## Commands

- `ping <message>` - Test connection with roundtrip timing
- `<any text>` - Send AI chat request
- `address` - Show your Nym address
- `exit` - Quit

## Deployment

See [DEPLOYMENT.md](DEPLOYMENT.md) for server deployment instructions.

## Privacy

### What is Protected
- ✅ Your IP address (never visible to server or AI provider)
- ✅ Your location (mixnet provides geographic anonymity)
- ✅ Request timing correlation (cover traffic & delays)

### What Server Sees (Fast Mode)
- Your ephemeral Nym public key (rotates on restart)
- Which Nym gateway you use (shared with thousands of users)

### What Server Sees (Maximum Privacy Mode)
- Nothing! SURBs provide complete anonymity

### What AI Providers See
- Only the server's IP address
- No connection to individual users

## License

MIT

## Acknowledgments

Built with [Nym](https://nymtech.net) - the next generation of privacy infrastructure.
