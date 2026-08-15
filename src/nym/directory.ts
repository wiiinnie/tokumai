// ---------------------------------------------------------------------------
// directory.ts — which gateways exist, and where.
//
// `nym-client list-gateways` only reports gateways this client has already
// registered with, which is no help when the whole point is picking a different
// one. The network directory lives in the nym-api, so that is what we ask.
//
// Why the entry gateway is worth choosing deliberately: it is the one hop that
// sees your IP address. The mixnet hides who you are talking to and what you
// send, but the entry gateway watches you connect. Choosing its jurisdiction is
// therefore a real privacy decision, not a performance tweak.
// ---------------------------------------------------------------------------

const NYM_API = process.env.NYM_API_URL ?? "https://validator.nymtech.net/api";

export interface GatewayInfo {
  identity: string;
  /** ISO country code as the operator declared it, or "??" when unset. */
  country: string;
  host: string;
}

interface DescribedNode {
  description?: {
    declared_role?: { entry?: boolean };
    auxiliary_details?: { location?: string };
    host_information?: {
      hostname?: string;
      ip_address?: string[];
      keys?: { ed25519?: string };
    };
  };
}

/**
 * Every node currently advertising an entry role.
 *
 * Operator-declared location is taken at face value — it is a claim, not a
 * measurement, and a gateway can say whatever it likes. Treat the country as a
 * hint rather than proof.
 */
export async function listAvailableGateways(): Promise<GatewayInfo[]> {
  const res = await fetch(`${NYM_API}/v1/nym-nodes/described`, {
    signal: AbortSignal.timeout(30_000),
  });
  if (!res.ok) throw new Error(`nym-api returned ${res.status}`);

  const raw = (await res.json()) as DescribedNode[] | { data?: DescribedNode[] };
  const nodes = Array.isArray(raw) ? raw : (raw.data ?? []);

  const out: GatewayInfo[] = [];
  for (const n of nodes) {
    const d = n.description;
    if (!d?.declared_role?.entry) continue;
    const identity = d.host_information?.keys?.ed25519;
    if (!identity) continue;
    out.push({
      identity,
      country: d.auxiliary_details?.location || "??",
      host: d.host_information?.hostname || d.host_information?.ip_address?.[0] || "",
    });
  }
  return out;
}

/** Counts per country, most gateways first. */
export function byCountry(gws: GatewayInfo[]): Array<[string, number]> {
  const counts = new Map<string, number>();
  for (const g of gws) counts.set(g.country, (counts.get(g.country) ?? 0) + 1);
  return [...counts.entries()].sort((a, b) => b[1] - a[1]);
}
