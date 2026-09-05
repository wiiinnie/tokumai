// ---------------------------------------------------------------------------
// config.ts — the CLI's small persistent settings.
//
// Deliberately NOT the vault. This holds operational preferences (which server
// address, which model, which nym client id) — never conversation content,
// never anything about what was asked. Conversations in the CLI live in memory
// and die with the process; the encrypted vault is a GUI concern.
// ---------------------------------------------------------------------------

import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import type { Proof } from "../protocol.js";

export interface Config {
  /** Nym address of the scrai server, as printed by `scrai-server run`. */
  serverAddress?: string;
  /** Default model for `chat`. */
  model?: string;
  /** What that model produces — decides the SURB budget. Learned from `models`. */
  modelKind?: "text" | "image";
  /** Retail TOKU per 1M tokens for the chosen model, as the server quoted it. */
  modelRate?: { in: number; out: number };
  /**
   * Output ceiling per request. Without one there is no upper bound on what an
   * answer can cost, so there is nothing to check a balance against. Generous
   * enough that normal answers are never cut short.
   */
  maxTokens: number;
  /** nym-client --id used by this CLI. */
  clientId: string;
  /** Reply SURB budget for a text answer. Too low truncates long answers. */
  replySurbs: number;
  /**
   * Budget for an image answer. A 1024px image is ~1-2 MB of base64, which is
   * roughly a thousand Sphinx packets — each needing its own SURB. The text
   * default would truncate it before the first pixel arrives.
   */
  imageSurbs: number;
  /** Where generated images are written. */
  imageDir: string;
  /**
   * Account recovery phrase. The ONLY thing a user must keep — everything else
   * on this machine is derivable from it or replaceable.
   *
   * Stored in the clear today, which is wrong for real money: it belongs in the
   * OS keychain (the seam already noted in public/vault.js). Doing that now
   * would be premature; shipping it that way would not.
   */
  mnemonic?: string;
  /** Account id, cached so the fingerprint can be shown without re-deriving. */
  accountId?: string;
  /**
   * Fingerprint of the issuer's public keyset, pinned on first sight. A change
   * is a red flag: a server could otherwise swap in per-user keys to tag
   * withdrawals despite the blinding.
   */
  issuerKeysetId?: string;
  /**
   * Session identity. The private key is what controls the balance — the
   * server only ever sees the public half and the id derived from it.
   */
  sessionPrivateKey?: string;
  sessionPublicKey?: string;
  sessionId?: string;
  /** Which derived session is in use. Recovery scans from 0 upwards. */
  sessionIndex?: number;
  /** Monotonic request counter. Must never go backwards or the server refuses. */
  counter?: number;
  /** Last balance the SERVER reported. A mirror, never the source of truth. */
  balance?: number;
  /**
   * Unspent bearer ecash held locally between a withdrawal and its redemption,
   * grouped into PACKETS — one packet per purchase tier. Redeeming a packet at a
   * time means each session.open carries a whole tier's denominations and no
   * more, so every redemption of a given tier looks identical to every other
   * user's; a mixed set would be a fingerprint.
   *
   * Holding rather than redeeming immediately is what breaks the TIMING link:
   * the account-signed withdrawal and the anonymous redemption happen at moments
   * the user controls. The cost: these are BEARER tokens — whoever holds them
   * can spend them, and they cannot be rebuilt from the recovery phrase once
   * withdrawn. So this field is money: back up this file until it is redeemed
   * (redemption moves the value onto a phrase-derived session, which recovery
   * CAN rebuild).
   */
  ecash?: Proof[][];
  /** Cumulative spend since the last reset. */
  spent?: number;
  /** Last catalog seen from the server — lets `model <name>` validate offline. */
  catalog?: Array<{
    model: string;
    vendor: string;
    kind: "text" | "image";
    trainsOnInput: boolean;
    rate?: { in: number; out: number };
  }>;
}

const FILE = (process.env.CONFIG ?? process.env.SCRAI_CONFIG) ?? join(homedir(), ".scrai", "cli.json");

const DEFAULTS: Config = {
  clientId: "scrai-client",
  replySurbs: 200,
  imageSurbs: 3000,
  maxTokens: 4096,
  imageDir: "./images",
};

export function load(): Config {
  try {
    return { ...DEFAULTS, ...JSON.parse(readFileSync(FILE, "utf8")) };
  } catch {
    return { ...DEFAULTS };
  }
}

export function save(patch: Partial<Config>): Config {
  const next = { ...load(), ...patch };
  mkdirSync(dirname(FILE), { recursive: true });
  writeFileSync(FILE, JSON.stringify(next, null, 2) + "\n");
  return next;
}

export function configPath(): string {
  return FILE;
}
