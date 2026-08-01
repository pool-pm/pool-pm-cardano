/**
 * Read a transaction's metadata.
 *
 * The server sends each label's value intact — text and integers as themselves, bytes as
 * `{bytes}`, maps as `{map: [{k, v}]}` — and every decision about what any of it means
 * happens here. Same reasoning as datums: a metadata schema then costs a frontend deploy
 * rather than a server restart.
 *
 * It used to arrive pre-rendered, which threw away more than it looked like. Label 1
 * carries `{"timestamp": …, "absolute_slot": …}` on ~9,800 txs a month, and what reached
 * the client was the string `"timestamp absolute_slot"` — the key names, with the values
 * discarded on the way out.
 */
import type { MetadataEntry, MetadataValue } from './types';

/** CIP-20 transaction message standard: `{ msg: [lines] }`. */
export const CIP20_MESSAGE = 674;
/** SundaeSwap's on-chain governance tally (the number is the first digits of π). */
const SUNDAE_GOVERNANCE = 31415;
const SUNDAE_LABEL = 'SundaeSwap governance';
/** How many of a map's keys to name when nothing better is known about the label. */
const MAX_SUMMARY_KEYS = 3;
/** Longest value rendered inline beside its key; past this it's a document, not a fact. */
const MAX_VALUE_CHARS = 24;

type MapEntry = { k: MetadataValue; v: MetadataValue };

function asMap(value: MetadataValue | undefined): MapEntry[] | null {
  // The array check is load-bearing: every array has a `map` method, so a bare `'map' in
  // value` test matches a metadata *list* and hands back its method.
  if (value === null || value === undefined || typeof value !== 'object' || Array.isArray(value)) return null;
  return 'map' in value ? value.map : null;
}

function asText(value: MetadataValue | undefined): string | null {
  return typeof value === 'string' ? value : null;
}

/** The value at a text key of a metadata map. */
function get(value: MetadataValue | undefined, key: string): MetadataValue | undefined {
  return asMap(value)?.find((e) => asText(e.k) === key)?.v;
}

/** The CIP-20 message lines of a tx, which is where dApps name themselves. */
export function messageLines(metadata: MetadataEntry[] | undefined): string[] {
  const msg = get(metadata?.find((e) => e.label === CIP20_MESSAGE)?.value, 'msg');
  if (Array.isArray(msg)) return msg.filter((line): line is string => typeof line === 'string');
  return typeof msg === 'string' ? [msg] : [];
}

/** A scalar rendered for display, or null when it isn't one worth showing inline. */
function scalar(value: MetadataValue): string | null {
  if (typeof value === 'string') return value || null;
  if (typeof value === 'number') return String(value);
  return null;
}

/**
 * What a label with no known meaning is saying, as `key value` pairs.
 *
 * The keys alone say more than the label number does — `timestamp` beats `metadata 1` —
 * but the values are what the transaction was actually for, so they come too when
 * they're short enough to read on a tile.
 */
function summarise(value: MetadataValue): string[] {
  const entries = asMap(value);
  if (!entries) {
    const lone = scalar(value);
    return lone ? [lone] : [];
  }
  const lines: string[] = [];
  for (const { k, v } of entries) {
    const key = asText(k);
    if (key === null || key === '') continue;
    const rendered = scalar(v);
    lines.push(rendered !== null && rendered.length <= MAX_VALUE_CHARS ? `${key} ${rendered}` : key);
    if (lines.length === MAX_SUMMARY_KEYS) break;
  }
  return lines;
}

/**
 * Display lines for a tx's metadata, one label at a time.
 *
 * CIP-20's message is the text its author wrote, so it stands alone. Anything else is
 * summarised from its own contents, falling back to the label number only when there's
 * nothing in it that reads.
 */
export function metadataLines(metadata: MetadataEntry[] | undefined): string[] {
  const lines: string[] = [];
  for (const { label, value } of metadata ?? []) {
    if (label === CIP20_MESSAGE) {
      const msg = messageLines([{ label, value }]);
      lines.push(...(msg.length > 0 ? msg : [`metadata ${label}`]));
    } else if (label === SUNDAE_GOVERNANCE) {
      lines.push(SUNDAE_LABEL);
    } else {
      const summary = summarise(value);
      lines.push(...(summary.length > 0 ? summary : [`metadata ${label}`]));
    }
  }
  return lines;
}
