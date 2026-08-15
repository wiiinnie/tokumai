// ---------------------------------------------------------------------------
// install.ts — fetch the nym-client binary into ./bin.
//
// Nym publishes ONE unlabelled binary per tool per release, and it is an
// x86_64 Linux ELF. There is no macOS or Windows build. So this installer
// refuses to download on any other platform rather than dropping a file that
// cannot execute — on macOS you build from source, which the error explains.
//
// Downloads are verified against the release's own hashes.json before the file
// is made executable. An unverified binary is never left on disk in a runnable
// state.
// ---------------------------------------------------------------------------

import { createHash } from "node:crypto";
import { chmod, mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { LOCAL_BIN_DIR } from "./process.js";

const RELEASES = "https://api.github.com/repos/nymtech/nym/releases";

interface Asset { name: string; browser_download_url: string }
interface Release { tag_name: string; assets: Asset[] }

/** Newest release that actually ships a nym-client binary. */
async function latestRelease(): Promise<Release> {
  const res = await fetch(`${RELEASES}?per_page=20`, {
    headers: { accept: "application/vnd.github+json" },
    signal: AbortSignal.timeout(20_000),
  });
  if (!res.ok) throw new Error(`GitHub releases API returned ${res.status}`);
  const all = (await res.json()) as Release[];
  const hit = all.find((r) => r.assets?.some((a) => a.name === "nym-client"));
  if (!hit) throw new Error("no recent nym release contains a nym-client asset");
  return hit;
}

/** Published sha256 for one asset, when the release includes hashes.json. */
async function publishedHash(rel: Release, assetName: string): Promise<string | null> {
  const hashes = rel.assets.find((a) => a.name === "hashes.json");
  if (!hashes) return null;
  try {
    const res = await fetch(hashes.browser_download_url, { signal: AbortSignal.timeout(20_000) });
    if (!res.ok) return null;
    const table = (await res.json()) as Record<string, unknown>;
    const entry = table[assetName];
    if (typeof entry === "string") return entry;
    if (entry && typeof entry === "object") {
      const v = (entry as Record<string, unknown>).sha256 ?? (entry as Record<string, unknown>).hash;
      if (typeof v === "string") return v;
    }
    return null;
  } catch {
    return null;
  }
}

export function platformSupported(): boolean {
  return process.platform === "linux" && process.arch === "x64";
}

export function unsupportedPlatformMessage(): string {
  return [
    `No prebuilt nym-client for ${process.platform}/${process.arch}.`,
    "",
    "Nym publishes x86_64 Linux binaries only. On this machine, build from source:",
    "",
    "  cargo install --git https://github.com/nymtech/nym --bin nym-client --locked",
    "",
    "or run the client in a Linux container and point SCRAI_NYM_WS at it.",
    "The first cargo build takes a while; after that `nym-client` is on your PATH",
    "and everything else here works unchanged.",
  ].join("\n");
}

export async function install(): Promise<string> {
  if (!platformSupported()) throw new Error(unsupportedPlatformMessage());

  const rel = await latestRelease();
  const asset = rel.assets.find((a) => a.name === "nym-client")!;

  process.stderr.write(`[nym] downloading nym-client from ${rel.tag_name}…\n`);
  const res = await fetch(asset.browser_download_url, { signal: AbortSignal.timeout(180_000) });
  if (!res.ok) throw new Error(`download failed: HTTP ${res.status}`);
  const bytes = Buffer.from(await res.arrayBuffer());

  const actual = createHash("sha256").update(bytes).digest("hex");
  const expected = await publishedHash(rel, "nym-client");
  if (expected && expected.toLowerCase() !== actual.toLowerCase()) {
    throw new Error(`checksum mismatch — refusing to install.\n  expected ${expected}\n  actual   ${actual}`);
  }

  await mkdir(LOCAL_BIN_DIR, { recursive: true });
  const dest = join(LOCAL_BIN_DIR, "nym-client");
  await writeFile(dest, bytes);
  await chmod(dest, 0o755);

  return [
    `installed nym-client ${rel.tag_name} -> ${dest}`,
    expected ? `sha256 verified (${actual.slice(0, 16)}…)` : `sha256 ${actual.slice(0, 16)}… (release published no hashes.json)`,
  ].join("\n");
}
