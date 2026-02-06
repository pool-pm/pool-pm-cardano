const BECH32_ALPHABET = 'qpzry9x8gf2tvdw0s3jn54khce6mua7l';
const ALPHABET_MAP = new Map([...BECH32_ALPHABET].map((c, i) => [c, i]));

export function bech32Decode(addr: string): Uint8Array | null {
	const sepIdx = addr.lastIndexOf('1');
	if (sepIdx < 1) return null;
	const data = addr.slice(sepIdx + 1).toLowerCase();

	// Convert from bech32 to 5-bit values
	const values: number[] = [];
	for (const c of data) {
		const v = ALPHABET_MAP.get(c);
		if (v === undefined) return null;
		values.push(v);
	}

	// Remove checksum (last 6 values)
	const payload = values.slice(0, -6);

	// Convert 5-bit to 8-bit
	let acc = 0;
	let bits = 0;
	const bytes: number[] = [];
	for (const v of payload) {
		acc = (acc << 5) | v;
		bits += 5;
		if (bits >= 8) {
			bits -= 8;
			bytes.push((acc >> bits) & 0xff);
		}
	}
	return new Uint8Array(bytes);
}

// Extract payment credential (28 bytes after header) as hex
export function paymentCredential(addr: string): string | null {
	const bytes = bech32Decode(addr);
	if (!bytes || bytes.length < 29) return null;
	// Header is byte 0, payment credential is bytes 1-28
	return Array.from(bytes.slice(1, 29))
		.map((b) => b.toString(16).padStart(2, '0'))
		.join('');
}

// Extract stake credential (bytes 29-56) as hex
export function stakeCredential(addr: string): string | null {
	const bytes = bech32Decode(addr);
	if (!bytes || bytes.length < 57) return null;
	return Array.from(bytes.slice(29, 57))
		.map((b) => b.toString(16).padStart(2, '0'))
		.join('');
}
