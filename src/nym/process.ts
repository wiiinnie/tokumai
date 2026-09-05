// ---------------------------------------------------------------------------
// process.ts — locate, configure and run the nym-client binary.
//
// v1 shells out to the standalone `nym-client`. That is a deliberate stepping
// stone, not the destination: the Tauri build embeds nym-sdk in Rust instead.
// Everything platform-specific about "how does a mixnet client come to exist"
// lives in this file, so swapping it out later touches nothing else.
//
// Gateway handling, verified against clients/native/src/commands:
//   init            --id <name> [--gateway <ed25519>] [--latency-based-selection]
//   list-gateways   --id <name>
//   add-gateway     --id <name> --gateway <ed25519>
//   switch-gateway  --id <name> --gateway <ed25519>
//
// Note the last three: changing the entry gateway does NOT require re-init. You
// register an additional gateway and switch the active one. Re-init is only for
// starting over.
// ---------------------------------------------------------------------------

import { spawn, spawnSync, type ChildProcess } from "node:child_process";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join, resolve } from "node:path";

/** Where `npm run nym:install` puts the binary. Checked before PATH. */
export const LOCAL_BIN_DIR = resolve(process.cwd(), "bin");
const LOCAL_BIN = join(LOCAL_BIN_DIR, "nym-client");

export interface InitOptions {
  /** ed25519 identity of the entry gateway. Omit to let nym pick. */
  gateway?: string;
  /** Pick the lowest-latency gateway instead of a uniform random one. */
  latencyBased?: boolean;
  /** Websocket port this client will listen on in all subsequent runs. */
  port?: number;
}

/**
 * Find the nym-client binary: local ./bin first, then PATH.
 * Returns null when it is nowhere to be found.
 */
export function findBinary(): string | null {
  if (existsSync(LOCAL_BIN)) return LOCAL_BIN;
  const which = spawnSync("which", ["nym-client"], { encoding: "utf8" });
  const hit = which.stdout?.trim();
  return which.status === 0 && hit ? hit : null;
}

export function requireBinary(): string {
  const bin = findBinary();
  if (!bin) {
    throw new Error(
      "nym-client not found.\n" +
        `  Looked in: ${LOCAL_BIN} and your PATH.\n` +
        "  Install it with:  npm run nym:install",
    );
  }
  return bin;
}

/** Config dir nym-client uses for a given --id. */
export function configDir(id: string): string {
  return join(homedir(), ".nym", "clients", id);
}

export function isInitialised(id: string): boolean {
  return existsSync(join(configDir(id), "config", "config.toml"));
}

function run(bin: string, args: string[]): { ok: boolean; out: string } {
  const r = spawnSync(bin, args, { encoding: "utf8" });
  const out = `${r.stdout ?? ""}${r.stderr ?? ""}`.trim();
  return { ok: r.status === 0, out };
}

/** One-time setup for a client id. Safe to call again — it just reports. */
export function init(id: string, opts: InitOptions = {}): string {
  const bin = requireBinary();
  if (isInitialised(id)) {
    return `client "${id}" is already initialised (${configDir(id)})`;
  }

  const args = ["init", "--id", id];
  if (opts.gateway) args.push("--gateway", opts.gateway);
  else if (opts.latencyBased) args.push("--latency-based-selection");
  if (opts.port) args.push("--port", String(opts.port));

  const { ok, out } = run(bin, args);
  if (!ok) throw new Error(`nym-client init failed:\n${out}`);
  patchConfig(id);
  return out;
}

/**
 * Work around a nym-client 1.1.82 config gap: `init` writes a [storage_paths]
 * section WITHOUT `credential_requests_database`, but `run` then requires it and
 * refuses to start ("missing field credential_requests_database"). Add the key
 * (idempotent) so a fresh init just works — no manual TOML edit per deploy.
 */
function patchConfig(id: string): void {
  const cfgPath = join(configDir(id), "config", "config.toml");
  let cfg: string;
  try {
    cfg = readFileSync(cfgPath, "utf8");
  } catch {
    return; // no config to patch (shouldn't happen right after a good init)
  }
  if (/^\s*credential_requests_database\s*=/m.test(cfg)) return; // already there

  const dbPath = join(configDir(id), "data", "credential_requests_database.db");
  const line = `credential_requests_database = '${dbPath}'`;

  let patched: string | null = null;
  if (/^\s*credentials_database\s*=.*$/m.test(cfg)) {
    // Insert right after the sibling key, inside [storage_paths].
    patched = cfg.replace(/^(\s*credentials_database\s*=.*)$/m, `$1\n${line}`);
  } else if (/^\[storage_paths\]\s*$/m.test(cfg)) {
    patched = cfg.replace(/^(\[storage_paths\]\s*)$/m, `$1\n${line}`);
  }
  if (patched && patched !== cfg) writeFileSync(cfgPath, patched);
}

export function listGateways(id: string): string {
  const { ok, out } = run(requireBinary(), ["list-gateways", "--id", id]);
  if (!ok) throw new Error(`list-gateways failed:\n${out}`);
  return out;
}

/**
 * Point the client at a different entry gateway. No re-init involved — the
 * client keeps its identity and therefore its Nym address.
 *
 * Note the flag names differ across nym's own subcommands: `init` takes
 * `--gateway`, while `add-gateway` and `switch-gateway` take `--gateway-id`.
 *
 * Two cases, in the order they are cheap:
 *   already registered -> switch-gateway is purely local
 *   new gateway        -> add-gateway registers over the network, and
 *                         --set-active makes it current in the same step
 */
export function useGateway(id: string, gateway: string): string {
  const bin = requireBinary();

  const switched = run(bin, ["switch-gateway", "--id", id, "--gateway-id", gateway]);
  if (switched.ok) return switched.out || `active gateway is now ${gateway}`;

  const added = run(bin, ["add-gateway", "--id", id, "--gateway-id", gateway, "--set-active"]);
  if (added.ok) return added.out || `registered with ${gateway} and made it active`;

  throw new Error(
    `could not switch to gateway ${gateway}.\n` +
      `  switch-gateway: ${firstLine(switched.out)}\n` +
      `  add-gateway:    ${firstLine(added.out)}`,
  );
}

function firstLine(s: string): string {
  return s.split("\n").find((l) => l.trim())?.trim() ?? "(no output)";
}

export interface RunningClient {
  proc: ChildProcess;
  /**
   * Ask nym-client to shut down and wait for it.
   *
   * This matters more than it looks. nym-client keeps its reply-SURB store in
   * SQLite, and a kill that lands mid-flush leaves it inconsistent — the next
   * start then refuses with "the client hasn't finished the data flush" and
   * renames the file to *.corrupted. Those pile up and the client will not run
   * until the store is cleared. So: SIGTERM, give it a moment to flush, and
   * only SIGKILL if it is genuinely stuck.
   */
  stop(): Promise<void>;
}

/** How long nym-client gets to flush its SURB store before we insist. */
const SHUTDOWN_GRACE_MS = Number((process.env.SHUTDOWN_MS ?? process.env.SCRAI_SHUTDOWN_MS) ?? 4000);

/**
 * Start `nym-client run` and resolve once it is actually up.
 *
 * We wait for the process to report readiness on stderr rather than sleeping a
 * fixed interval, then the caller connects the websocket. If the process dies
 * during startup we surface its output — the usual causes (uninitialised id,
 * port in use, unreachable gateway) all announce themselves there.
 */
/**
 * PID of an already-running nym-client for this id, if any.
 *
 * A gateway allows one connection per client identity, so a leftover process
 * makes the next start fail with "There is already an open connection to this
 * client" — which says nothing about the actual cause. Catching it here turns a
 * confusing gateway error into an instruction.
 */
export function findRunning(id: string): number | null {
  const r = spawnSync("pgrep", ["-f", `nym-client run --id ${id}`], { encoding: "utf8" });
  if (r.status !== 0) return null;

  for (const line of (r.stdout ?? "").trim().split("\n")) {
    const pid = Number(line.trim());
    if (!pid || pid === process.pid) continue;

    // `pgrep -f` matches ANY process whose command line contains the pattern —
    // including another pgrep, a grep, an editor, or a shell that happens to
    // mention it. Taking its word for it produces false positives that block a
    // perfectly legal startup, so confirm the match really is the binary.
    const ps = spawnSync("ps", ["-o", "command=", "-p", String(pid)], { encoding: "utf8" });
    const cmd = (ps.stdout ?? "").trim();
    if (/(^|\/)nym-client\s+run\b/.test(cmd) && cmd.includes(`--id ${id}`)) return pid;
  }
  return null;
}

export function start(
  id: string,
  opts: {
    port?: number;
    verbose?: boolean;
    /**
     * Called with real startup milestones as nym-client reports them. These are
     * parsed from its own output, so a caller showing progress is reflecting
     * what actually happened rather than animating a guess.
     */
    onPhase?: (phase: string) => void;
  } = {},
): Promise<RunningClient> {
  const bin = requireBinary();
  if (!isInitialised(id)) {
    return Promise.reject(new Error(`client "${id}" is not initialised — run setup first`));
  }

  const orphan = findRunning(id);
  if (orphan) {
    return Promise.reject(
      new Error(
        `a nym-client for "${id}" is already running (pid ${orphan}).\n` +
          `  The gateway permits one connection per identity, so this one would be refused.\n` +
          `  Stop it with:  kill ${orphan}`,
      ),
    );
  }

  const args = ["run", "--id", id];
  const proc = spawn(bin, args, { stdio: ["ignore", "pipe", "pipe"] });

  return new Promise((resolvePromise, reject) => {
    let settled = false;
    let log = "";

    const ready = () => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      resolvePromise({
        proc,
        stop() {
          return new Promise<void>((done) => {
            if (proc.exitCode !== null || proc.signalCode !== null) return done();
            const hard = setTimeout(() => {
              try { proc.kill("SIGKILL"); } catch { /* already gone */ }
              done();
            }, SHUTDOWN_GRACE_MS);
            proc.once("exit", () => {
              clearTimeout(hard);
              done();
            });
            try { proc.kill("SIGTERM"); } catch { clearTimeout(hard); done(); }
          });
        },
      });
    };

    // Milestones nym-client actually prints, in the order it prints them. Each
    // match is a real event — nothing here is inferred from elapsed time.
    const MILESTONES: Array<[RegExp, string]> = [
      [/obtaining initial network topology/i, "fetching network topology"],
      [/starting topology refresher/i, "topology loaded"],
      [/connecting to gateway|gateway client/i, "connecting to gateway"],
      [/starting nym client/i, "starting mixnet client"],
      [/websocket|client startup finished|listening/i, "mixnet client ready"],
    ];
    const seen = new Set<string>();

    const watch = (buf: Buffer) => {
      const text = buf.toString();
      log += text;
      if (opts.verbose) process.stderr.write(text);

      for (const [re, phase] of MILESTONES) {
        if (!seen.has(phase) && re.test(text)) {
          seen.add(phase);
          opts.onPhase?.(phase);
        }
      }
      // nym-client announces the websocket once the gateway handshake is done
      if (/websocket|client startup finished|listening/i.test(text)) ready();
    };

    proc.stdout?.on("data", watch);
    proc.stderr?.on("data", watch);

    proc.on("exit", (code) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      reject(new Error(`nym-client exited with code ${code} during startup:\n${log.slice(-2000)}`));
    });

    // Fallback: some builds are quieter than others. Let the caller's websocket
    // connect attempt be the real readiness check.
    const timer = setTimeout(ready, 15_000);
  });
}
