// SHA-256 fed in chunks (FIPS 180-4), the classic console's
// `console-sha256.js`. WebCrypto digests only a whole buffer, and a
// checkpoint does not fit in memory, so a file is hashed as it streams off disk.

const K = Uint32Array.from([
  0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
  0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
  0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
  0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
  0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
  0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
  0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
  0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
]);

export class Sha256 {
  #state = Uint32Array.from([
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
  ]);
  #w = new Uint32Array(64);
  /** Bytes short of a whole block. */
  #tail = new Uint8Array(64);
  #tailLen = 0;
  #length = 0;

  /** Feed `data`. */
  update(data: Uint8Array) {
    this.#length += data.length;
    let i = 0;
    if (this.#tailLen > 0) {
      i = Math.min(64 - this.#tailLen, data.length);
      this.#tail.set(data.subarray(0, i), this.#tailLen);
      this.#tailLen += i;
      if (this.#tailLen < 64) return;
      this.#block(this.#tail, 0);
      this.#tailLen = 0;
    }
    for (; i + 64 <= data.length; i += 64) this.#block(data, i);
    this.#tail.set(data.subarray(i), 0);
    this.#tailLen = data.length - i;
  }

  /** The digest of everything fed, as 64 lowercase hex characters; feeds the padding, so call it once. */
  hex(): string {
    const length = this.#length;
    const pad = new Uint8Array((this.#tailLen < 56 ? 56 : 120) - this.#tailLen + 8);
    pad[0] = 0x80;
    const tail = new DataView(pad.buffer, pad.length - 8);
    // The length in bits, big-endian; exact below 2^50 bytes.
    tail.setUint32(0, Math.floor(length / 0x20000000));
    tail.setUint32(4, (length * 8) >>> 0);
    this.update(pad);
    return Array.from(this.#state, (v) => v.toString(16).padStart(8, "0")).join("");
  }

  #block(d: Uint8Array, off: number) {
    const w = this.#w;
    for (let t = 0; t < 16; t++, off += 4) {
      w[t] = (d[off]! << 24) | (d[off + 1]! << 16) | (d[off + 2]! << 8) | d[off + 3]!;
    }
    for (let t = 16; t < 64; t++) {
      const x = w[t - 15]!, y = w[t - 2]!;
      const s0 = ((x >>> 7) | (x << 25)) ^ ((x >>> 18) | (x << 14)) ^ (x >>> 3);
      const s1 = ((y >>> 17) | (y << 15)) ^ ((y >>> 19) | (y << 13)) ^ (y >>> 10);
      w[t] = w[t - 16]! + s0 + w[t - 7]! + s1;
    }
    const s = this.#state;
    let a = s[0]!, b = s[1]!, c = s[2]!, dd = s[3]!, e = s[4]!, f = s[5]!, g = s[6]!, h = s[7]!;
    for (let t = 0; t < 64; t++) {
      const S1 = ((e >>> 6) | (e << 26)) ^ ((e >>> 11) | (e << 21)) ^ ((e >>> 25) | (e << 7));
      const t1 = (h + S1 + ((e & f) ^ (~e & g)) + K[t]! + w[t]!) | 0;
      const S0 = ((a >>> 2) | (a << 30)) ^ ((a >>> 13) | (a << 19)) ^ ((a >>> 22) | (a << 10));
      const t2 = (S0 + ((a & b) ^ (a & c) ^ (b & c))) | 0;
      h = g; g = f; f = e; e = (dd + t1) | 0; dd = c; c = b; b = a; a = (t1 + t2) | 0;
    }
    s[0]! += a; s[1]! += b; s[2]! += c; s[3]! += dd; s[4]! += e; s[5]! += f; s[6]! += g; s[7]! += h;
  }
}

/**
 * The digest of `file`, read as a stream and never whole; `progress` hears the
 * bytes hashed so far, and `signal` stops the read.
 */
export async function sha256Of(file: Blob, progress: (done: number) => void = () => {},
                               signal?: AbortSignal): Promise<string> {
  const hash = new Sha256();
  const reader = file.stream().getReader();
  let done = 0;
  for (;;) {
    if (signal?.aborted) {
      await reader.cancel();
      signal.throwIfAborted();
    }
    const chunk = await reader.read();
    if (chunk.done) return hash.hex();
    hash.update(chunk.value);
    done += chunk.value.length;
    progress(done);
  }
}
