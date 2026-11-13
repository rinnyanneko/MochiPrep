import { isIPv4, isIPv6 } from 'node:net';

function ipv4Public(a: number, b: number, c: number): boolean {
  if (a === 0 || a === 10 || a === 127 || a >= 224) return false; // this net, private, loopback, multicast and reserved
  if (a === 100 && (b & 0xc0) === 64) return false; // 100.64/10 carrier-grade NAT
  if (a === 169 && b === 254) return false; // link-local, cloud metadata
  if (a === 172 && (b & 0xf0) === 16) return false;
  if (a === 192 && b === 168) return false;
  if (a === 192 && b === 0 && c === 0) return false; // 192.0.0/24 protocol assignments
  if (a === 198 && (b & 0xfe) === 18) return false; // 198.18/15 benchmarking
  return true;
}

/** The eight 16-bit groups of an IPv6 address `isIPv6` accepted, with an IPv4 tail folded in. */
function ipv6Groups(text: string): number[] {
  let rest = text.split('%')[0]!;
  const tail = rest.slice(rest.lastIndexOf(':') + 1);
  if (tail.includes('.')) {
    const [a, b, c, d] = tail.split('.').map(Number) as [number, number, number, number];
    rest = `${rest.slice(0, rest.length - tail.length)}${((a << 8) | b).toString(16)}:${((c << 8) | d).toString(16)}`;
  }
  const [head, after] = rest.split('::') as [string, string | undefined];
  const parse = (part: string) => (part ? part.split(':').map((g) => parseInt(g, 16)) : []);
  const front = parse(head);
  if (after === undefined) return front;
  const back = parse(after);
  return [...front, ...new Array<number>(8 - front.length - back.length).fill(0), ...back];
}

/**
 * Whether a feed may be fetched from this address. The backend runs on the owner's machine, so
 * anything that reaches it, its LAN, a cloud metadata service or a multicast group is refused.
 * Kept in step with `is_public_address` in crates/mochi-market/src/news.rs through the golden corpus.
 */
export function isPublicAddress(address: string): boolean {
  if (isIPv4(address)) {
    const [a, b, c] = address.split('.').map(Number) as [number, number, number];
    return ipv4Public(a, b, c);
  }
  if (!isIPv6(address)) return false;
  const g = ipv6Groups(address);
  if (g.every((x) => x === 0)) return false; // ::
  if (g.slice(0, 7).every((x) => x === 0) && g[7] === 1) return false; // ::1
  if ((g[0]! & 0xfe00) === 0xfc00) return false; // fc00::/7 unique local
  if ((g[0]! & 0xffc0) === 0xfe80) return false; // fe80::/10 link-local
  if ((g[0]! & 0xffc0) === 0xfec0) return false; // fec0::/10 site-local (deprecated, still routable inside a site)
  if ((g[0]! & 0xff00) === 0xff00) return false; // ff00::/8 multicast
  // ::ffff:0:0/96 (IPv4-mapped) and 64:ff9b::/96 (NAT64) carry an IPv4 address in the last 32 bits,
  // and a connection to one reaches that IPv4 address, so it is judged by the IPv4 rules.
  const mapped = g.slice(0, 5).every((x) => x === 0) && g[5] === 0xffff;
  const nat64 = g[0] === 0x64 && g[1] === 0xff9b && g.slice(2, 6).every((x) => x === 0);
  if (mapped || nat64) return ipv4Public(g[6]! >> 8, g[6]! & 0xff, g[7]! >> 8);
  if (g[0] === 0x64 && g[1] === 0xff9b && g[2] === 1) return false; // 64:ff9b:1::/48 local-use NAT64
  // IPv4-compatible ::/96 (deprecated) holds an IPv4 address in its low 32 bits; :: and ::1 were
  // refused above.
  if (g.slice(0, 6).every((x) => x === 0)) return ipv4Public(g[6]! >> 8, g[6]! & 0xff, g[7]! >> 8);
  // 6to4 2002::/16 tunnels to the IPv4 address in bits 16-48.
  if (g[0] === 0x2002) return ipv4Public(g[1]! >> 8, g[1]! & 0xff, g[2]! >> 8);
  return true;
}
